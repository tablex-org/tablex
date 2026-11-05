//! Starting a script: which executable may run, how its server commands reach the tab's session,
//! and how a run is stopped.
//!
//! # Which executable
//! `MongoShell::new(path)` runs whatever it is given and the child has no OS sandbox, so the
//! path is never chosen by anyone but the installer: it is [`ToolStore::executable_path`] of the
//! plugin, and only while [`ToolStore::installed`] agrees that the file there is the one the
//! installer wrote (same size and SHA-256 as `installed.json`, which holds the hash of the
//! artifact the signed manifest named). That check runs before EVERY spawn, on the worker thread
//! (the digest of an unchanged file is remembered, so a run does not re-read 3.6 MB). A file
//! that was swapped, truncated or replaced by a link is refused, never executed. It is not a
//! defence against someone who can already write the tools folder and its record (the folder is
//! owner-only, which the store enforces).
//!
//! # Stop
//! `ShellRun::wait` takes the run, so the worker thread owns it; Stop goes through the host's
//! [`ShellCancel`] handle (`ShellRun::cancel_handle`), taken before the wait: it cancels the
//! driver operation the run's commands carry (`killOp` for a command in flight, the next one
//! starts cancelled) and kills the child through the host's own `Child` handle. It is idempotent
//! and does nothing once the run ended, so a late Stop can never touch another process.

use crate::bson_page::{documents_page, BsonPageError};
use crate::mongo_shell::{check_reply, CheckedReply};
use engine_driver_api::mongo_shell::ShellRequest;
use engine_driver_api::PageColumn;
use engine_driver_host::PageLease;
use engine_driver_host::{
    MongoShell, SharedDriverSession, ShellHostError, ToolError, ToolStore, MONGO_SHELL_TOOL_ID,
};
use std::fmt;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};

/// Ends a started run (idempotent).
pub(in crate::app) type Stop = Box<dyn Fn() + Send + Sync>;

/// What an engine needs to start one run.
pub(in crate::app) struct ShellLaunch {
    /// The verified plugin executable.
    pub exe: PathBuf,
    pub request: ShellRequest,
    /// The tab's driver session, which the script's commands go through (none for an engine
    /// that needs no session).
    pub session: Option<Arc<SharedDriverSession>>,
}

/// A run in flight.
pub(in crate::app) struct RunningShell {
    pub stop: Stop,
    /// Blocks until the run ends: call it off the UI thread.
    pub wait: Box<dyn FnOnce() -> Result<engine_driver_api::mongo_shell::ShellReply, ShellHostError> + Send>,
}

/// Starts plugin runs. The app uses [`HostEngine`]; tests replace it to script replies.
pub(in crate::app) trait ShellEngine: Send + Sync {
    fn start(&self, launch: ShellLaunch) -> Result<RunningShell, ShellHostError>;

    /// Whether a run needs the tab's live driver session for its commands (the host does).
    fn needs_session(&self) -> bool {
        true
    }
}

/// The real engine: the plugin as a child process through `engine_driver_host`.
pub(in crate::app) struct HostEngine;

impl ShellEngine for HostEngine {
    fn start(&self, launch: ShellLaunch) -> Result<RunningShell, ShellHostError> {
        let session = launch.session.ok_or(ShellHostError::Unsupported)?;
        // Only to start: the run builds its own runners and keeps no hold on the session lock.
        let run = {
            let session = session
                .lock()
                .map_err(|_| ShellHostError::Io(std::io::Error::other("the driver session lock is poisoned")))?;
            MongoShell::new(launch.exe).run(&session, launch.request)?
        };
        let cancel = run.cancel_handle();
        Ok(RunningShell {
            stop: Box::new(move || cancel.cancel()),
            wait: Box::new(move || run.wait()),
        })
    }
}

/// Reaches [`Stop`] from the UI thread before, while and after the run starts on its worker.
#[derive(Default)]
pub(in crate::app) struct StopSlot {
    state: Mutex<StopState>,
}

#[derive(Default)]
enum StopState {
    #[default]
    Idle,
    Armed(Stop),
    Stopped,
}

impl StopSlot {
    /// The run started: `stop` ends it, at once when a stop was asked for in the meantime.
    pub(in crate::app) fn arm(&self, stop: Stop) {
        let mut state = self.state.lock().unwrap_or_else(|e| e.into_inner());
        if matches!(*state, StopState::Stopped) {
            drop(state);
            stop();
        } else {
            *state = StopState::Armed(stop);
        }
    }

    /// Stops the run now, or as soon as it starts.
    pub(in crate::app) fn stop(&self) {
        let previous = {
            let mut state = self.state.lock().unwrap_or_else(|e| e.into_inner());
            std::mem::replace(&mut *state, StopState::Stopped)
        };
        if let StopState::Armed(stop) = previous {
            stop();
        }
    }

    pub(in crate::app) fn is_stopped(&self) -> bool {
        matches!(
            *self.state.lock().unwrap_or_else(|e| e.into_inner()),
            StopState::Stopped
        )
    }
}

/// Why a run did not produce a reply.
#[derive(Debug)]
pub(in crate::app) enum ShellRunError {
    /// The plugin is not installed, or the file is not the one that was installed.
    Plugin(PluginError),
    Host(ShellHostError),
    /// The reply's documents could not be made into a page the grid reads.
    Page(BsonPageError),
}

impl fmt::Display for ShellRunError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Plugin(error) => error.fmt(f),
            Self::Host(error) => error.fmt(f),
            Self::Page(error) => error.fmt(f),
        }
    }
}

impl From<BsonPageError> for ShellRunError {
    fn from(error: BsonPageError) -> Self {
        Self::Page(error)
    }
}

/// A reply the UI can show as it is: checked, cleaned and bounded, with its documents already
/// parsed and laid out as a page. Made on the worker thread, never on the UI's.
pub(in crate::app) struct ReadyReply {
    /// Without its documents (they are in `page`).
    pub checked: CheckedReply,
    pub rows: usize,
    /// `None` when there are no documents.
    pub page: Option<(Vec<PageColumn>, PageLease)>,
}

/// Checks `reply` of a run that started in `database` and builds the page of its documents.
pub(in crate::app) fn prepare_reply(
    reply: engine_driver_api::mongo_shell::ShellReply,
    database: &str,
    generation: u64,
) -> Result<ReadyReply, BsonPageError> {
    let mut checked = check_reply(reply, database);
    let documents = std::mem::take(&mut checked.documents);
    let rows = documents.len();
    let page = if documents.is_empty() { None } else { Some(documents_page(&documents, generation)?) };
    Ok(ReadyReply { checked, rows, page })
}

impl From<ShellHostError> for ShellRunError {
    fn from(error: ShellHostError) -> Self {
        Self::Host(error)
    }
}

impl ShellRunError {
    /// The user stopped the run (the host said so, or the slot was stopped before it started).
    pub(in crate::app) fn is_cancelled(&self) -> bool {
        matches!(self, Self::Host(ShellHostError::Cancelled))
    }
}

#[derive(Debug)]
pub(in crate::app) enum PluginError {
    NoDataDirectory,
    NotInstalled,
    /// The record or the file could not be trusted (swapped, truncated, a link, unreadable).
    Unusable(ToolError),
}

impl fmt::Display for PluginError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::NoDataDirectory => f.write_str("TableX has no data directory, so no MongoDB Shell plugin can be installed"),
            Self::NotInstalled => f.write_str("The MongoDB Shell plugin is not installed (or its file changed since it was installed). Install it from the Driver Manager"),
            Self::Unusable(error) => write!(f, "The MongoDB Shell plugin cannot be used: {error}. Reinstall it from the Driver Manager"),
        }
    }
}

/// The plugin executable, when the file in the tool store is the one the installer wrote.
/// The only way a path reaches `MongoShell::new`.
pub(in crate::app) fn verified_plugin(store: Option<&ToolStore>) -> Result<PathBuf, PluginError> {
    let store = store.ok_or(PluginError::NoDataDirectory)?;
    match store.installed(MONGO_SHELL_TOOL_ID) {
        Ok(Some(_)) => store
            .executable_path(MONGO_SHELL_TOOL_ID)
            .map_err(PluginError::Unusable),
        Ok(None) => Err(PluginError::NotInstalled),
        Err(error) => Err(PluginError::Unusable(error)),
    }
}

/// One run, started and waited for on a blocking thread.
pub(in crate::app) struct ShellJob {
    pub engine: Arc<dyn ShellEngine>,
    pub tools: Option<Arc<ToolStore>>,
    pub request: ShellRequest,
    pub session: Option<Arc<SharedDriverSession>>,
    pub slot: Arc<StopSlot>,
    /// Labels the result page (any number unique to the run).
    pub generation: u64,
}

impl ShellJob {
    pub(in crate::app) fn run(self) -> Result<ReadyReply, ShellRunError> {
        let (database, generation) = (self.request.database.clone(), self.generation);
        let reply = self.run_plugin()?;
        Ok(prepare_reply(reply, &database, generation)?)
    }

    fn run_plugin(self) -> Result<engine_driver_api::mongo_shell::ShellReply, ShellRunError> {
        let exe = verified_plugin(self.tools.as_deref()).map_err(ShellRunError::Plugin)?;
        if self.slot.is_stopped() {
            return Err(ShellHostError::Cancelled.into());
        }
        let running = self.engine.start(ShellLaunch {
            exe,
            request: self.request,
            session: self.session,
        })?;
        self.slot.arm(running.stop);
        Ok((running.wait)()?)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};

    #[test]
    fn a_stop_before_the_run_starts_stops_it_the_moment_it_does() {
        let stops = Arc::new(AtomicUsize::new(0));
        let slot = StopSlot::default();
        slot.stop();
        assert!(slot.is_stopped());
        let counted = Arc::clone(&stops);
        slot.arm(Box::new(move || {
            counted.fetch_add(1, Ordering::SeqCst);
        }));
        assert_eq!(stops.load(Ordering::SeqCst), 1);
    }

    #[test]
    fn a_stop_after_the_run_started_stops_it_once() {
        let stops = Arc::new(AtomicUsize::new(0));
        let slot = StopSlot::default();
        let counted = Arc::clone(&stops);
        slot.arm(Box::new(move || {
            counted.fetch_add(1, Ordering::SeqCst);
        }));
        assert!(!slot.is_stopped());
        slot.stop();
        slot.stop();
        assert_eq!(stops.load(Ordering::SeqCst), 1);
    }

    #[test]
    fn only_an_installed_unchanged_plugin_has_a_path() {
        let dir = std::env::temp_dir().join(format!("tablex-shell-plugin-{}", uuid::Uuid::new_v4()));
        let store = ToolStore::new(dir.join("tools"));
        assert!(matches!(verified_plugin(None), Err(PluginError::NoDataDirectory)));
        assert!(matches!(verified_plugin(Some(&store)), Err(PluginError::NotInstalled)));
        let _ = std::fs::remove_dir_all(dir);
    }
}
