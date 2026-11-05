//! The tool layer of the app and the places a run gets it from. Its approver is the app's own
//! ([`AppApprover`]): a write waits for the user's click in the chat.
//!
//! Building it opens the audit log (a file), so it is never done on the UI thread: a run asks for
//! it on its own worker thread ([`LayerSource::resolve`]), the first one builds it, the others
//! share it. `revoke` and quit only use a layer that exists.

use super::approval::AppApprover;
use super::FRONT_END;
use engine_agent::{AuditConfig, AuditLog, ToolLayer};
use engine_ai::ConfigError;
use engine_connect::connect::AgentConnections;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};

/// Why the tool layer could not be built.
#[derive(Debug)]
pub(super) enum LayerError {
    /// The app has no agent connections (it always has one outside tests).
    NoAgent,
    Audit(engine_agent::audit::AuditError),
    /// Settings hold an address the provider refuses.
    Config(ConfigError),
}

impl std::fmt::Display for LayerError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::NoAgent => f.write_str("The assistant has no connections to work with."),
            Self::Audit(error) => write!(
                f,
                "The assistant needs its audit log, which could not be opened: {error}."
            ),
            Self::Config(error) => write!(
                f,
                "The AI settings cannot be used: {error}. Check Settings \u{2192} AI."
            ),
        }
    }
}

struct Built {
    layer: Arc<ToolLayer>,
    audit: Option<Arc<AuditLog>>,
}

impl std::error::Error for LayerError {}

/// The one `ToolLayer` of the app over its agent connections, with the audit log (front end
/// `assistant`, the default limits, writes asked about), built on first use.
pub struct LayerCell {
    agent: Option<Arc<AgentConnections>>,
    /// Where the audit log is (`None` in tests and scenarios: no file is written).
    audit_path: Option<PathBuf>,
    built: Mutex<Option<Built>>,
}

impl LayerCell {
    pub(super) fn new(agent: Option<Arc<AgentConnections>>, audit_path: Option<PathBuf>) -> Self {
        Self {
            agent,
            audit_path,
            built: Mutex::new(None),
        }
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, Option<Built>> {
        self.built
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    /// The layer, built by the first caller. Blocking (it opens the audit file): not for the UI
    /// thread.
    pub(super) fn get_or_build(&self) -> Result<Arc<ToolLayer>, LayerError> {
        let mut built = self.lock();
        if let Some(built) = built.as_ref() {
            return Ok(Arc::clone(&built.layer));
        }
        let agent = self.agent.clone().ok_or(LayerError::NoAgent)?;
        let mut layer =
            ToolLayer::new(agent.clone(), agent, FRONT_END).with_approver(AppApprover::new());
        let mut audit = None;
        if let Some(path) = &self.audit_path {
            let log =
                Arc::new(AuditLog::open(path, AuditConfig::default()).map_err(LayerError::Audit)?);
            layer = layer.with_audit(Arc::clone(&log));
            audit = Some(log);
        }
        let layer = Arc::new(layer);
        *built = Some(Built {
            layer: Arc::clone(&layer),
            audit,
        });
        Ok(layer)
    }

    /// Whether a layer exists or is being built right now (without waiting for a build: this is
    /// asked on the UI thread, and a build holds the lock while it opens the audit file).
    pub(super) fn exists(&self) -> bool {
        match self.built.try_lock() {
            Ok(built) => built.is_some(),
            Err(std::sync::TryLockError::WouldBlock) => true,
            Err(std::sync::TryLockError::Poisoned(poisoned)) => poisoned.into_inner().is_some(),
        }
    }

    /// The layer if it exists already.
    pub(super) fn built(&self) -> Option<Arc<ToolLayer>> {
        self.lock().as_ref().map(|built| Arc::clone(&built.layer))
    }

    /// Closes the sessions and flushes the audit log, if the layer was ever built.
    pub(super) fn shut_down(&self) {
        let Some((layer, audit)) = self
            .lock()
            .as_ref()
            .map(|built| (Arc::clone(&built.layer), built.audit.clone()))
        else {
            return;
        };
        layer.close_all();
        if let Some(audit) = audit {
            audit.flush();
        }
    }

    /// A layer over fakes instead of the real one (tests, scenarios).
    #[cfg(any(test, feature = "ui-scenarios"))]
    pub(super) fn preset(&self, layer: Arc<ToolLayer>) {
        *self.lock() = Some(Built { layer, audit: None });
    }
}

/// Where a run gets its tool layer.
#[derive(Clone)]
pub enum LayerSource {
    Ready(Arc<ToolLayer>),
    Cell(Arc<LayerCell>),
}

impl From<Arc<ToolLayer>> for LayerSource {
    fn from(layer: Arc<ToolLayer>) -> Self {
        Self::Ready(layer)
    }
}

impl LayerSource {
    /// On the run's worker thread.
    pub(super) fn resolve(&self) -> Result<Arc<ToolLayer>, LayerError> {
        match self {
            Self::Ready(layer) => Ok(Arc::clone(layer)),
            Self::Cell(cell) => cell.get_or_build(),
        }
    }
}
