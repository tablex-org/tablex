//! Wiring of the tool layer: the one place `tablex-mcp` decides which front-end pieces it supplies.
//! It adds no policy: the saved connections come from `engine-connect`, the rules from
//! `engine-agent`.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use engine_agent::{AuditConfig, AuditLog, ToolLayer};
use engine_connect::connect::{AgentConnections, Connector, RefusePrompts};
use engine_connect::drivers::{driver_data_dir, driver_data_dir_override, DRIVER_DATA_DIR_ENV};
use engine_connect::platform::audit_log_path;
use engine_driver_host::InstalledDriverStore;
use engine_storage::paths::{data_dir_override, DATA_DIR_ENV};

use crate::approver::ElicitingApprover;

// A shipped build must never carry the test credential switch.
#[cfg(all(feature = "test-credentials", not(debug_assertions)))]
compile_error!("the `test-credentials` feature is for test builds only (debug profile)");

/// The `front_end` of every audit line this program writes.
pub const FRONT_END: &str = "mcp";

/// Why the server could not be set up (it then exits 1 without serving).
#[derive(Debug, thiserror::Error)]
pub enum SetupError {
    #[error("this platform has no data directory, so there is nowhere to keep the audit log")]
    NoDataDir,
    #[error("cannot open the audit log: {0}")]
    Audit(#[from] engine_agent::audit::AuditError),
}

/// The tool layer over `connector`'s saved connections: the connector is both catalog and session
/// provider, tunnel questions are refused (no UI: an unknown bastion key is an error telling the
/// user to connect once from the app), a write is put to the user through MCP elicitation when the
/// app's switch (`ai.mcp_writes` in the connector's own `settings.json`, read at the time of each
/// write) allows it and refused otherwise ([`ElicitingApprover`]), and every call is audited.
pub fn build_layer(connector: Connector, audit: Arc<AuditLog>) -> ToolLayer {
    let settings_path = connector.settings_path().map(Path::to_path_buf);
    let connections = Arc::new(AgentConnections::new(connector, Arc::new(RefusePrompts)));
    ToolLayer::new(connections.clone(), connections, FRONT_END)
        .with_approver(Arc::new(ElicitingApprover::new(settings_path)))
        .with_audit(audit)
}

/// The real thing: the app's connection store, `settings.json`, driver install directory and OS
/// credential store, and the audit log in the app data directory. The audit log is returned too,
/// so the caller can flush it before the process exits.
pub fn production_layer() -> Result<(ToolLayer, Arc<AuditLog>), SetupError> {
    let connector = production_connector();
    let path: PathBuf = audit_log_path().ok_or(SetupError::NoDataDir)?;
    let audit = Arc::new(AuditLog::open(path, AuditConfig::default())?);
    Ok((build_layer(connector, Arc::clone(&audit)), audit))
}

fn production_connector() -> Connector {
    let connector = Connector::new(Arc::new(InstalledDriverStore::new(driver_data_dir())));
    #[cfg(feature = "test-credentials")]
    let connector = connector.with_credentials(Arc::new(test_credentials::Nothing));
    connector
}

/// The stderr lines a start prints about `TABLEX_DATA_DIR` and `TABLEX_DRIVER_DATA_DIR` when they
/// are set (and the test build marker).
pub fn startup_notices() -> Vec<String> {
    let mut notices = Vec::new();
    match (std::env::var_os(DATA_DIR_ENV), data_dir_override()) {
        (_, Some(dir)) => notices.push(format!(
            "using the data directory {} from {DATA_DIR_ENV}",
            dir.display()
        )),
        (Some(value), None) if !value.is_empty() => notices.push(format!(
            "ignoring {DATA_DIR_ENV}: it is not an absolute path"
        )),
        _ => {}
    }
    match (
        std::env::var_os(DRIVER_DATA_DIR_ENV),
        driver_data_dir_override(),
    ) {
        (_, Some(dir)) => notices.push(format!(
            "using the driver directory {} from {DRIVER_DATA_DIR_ENV}",
            dir.display()
        )),
        (Some(value), None) if !value.is_empty() => notices.push(format!(
            "ignoring {DRIVER_DATA_DIR_ENV}: it is not an absolute path"
        )),
        _ => {}
    }
    #[cfg(feature = "test-credentials")]
    notices.push("TEST BUILD (test-credentials): the OS credential store is not used".to_owned());
    notices
}

#[cfg(feature = "test-credentials")]
mod test_credentials {
    use engine_connect::credentials::{CredentialError, CredentialStore, SecretKind};

    /// A credential store with nothing in it that stores nothing.
    pub struct Nothing;

    impl CredentialStore for Nothing {
        fn load(&self, _: SecretKind, _: &str) -> Result<Option<String>, CredentialError> {
            Ok(None)
        }
        fn write(&self, _: SecretKind, _: &str, _: &str) -> Result<(), CredentialError> {
            Err(CredentialError::NoKeyring)
        }
        fn delete(&self, _: SecretKind, _: &str) -> Result<(), CredentialError> {
            Ok(())
        }
    }
}
