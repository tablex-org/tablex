//! Wiring of the tool layer: the one place `tablex-mcp` decides which front-end pieces it supplies.
//! It adds no policy: the saved connections come from `engine-connect`, the rules from
//! `engine-agent`.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use engine_agent::{AppShow, AuditConfig, AuditLog, ToolLayer};
use engine_connect::app_channel::AppClient;
use engine_connect::connect::{AgentConnections, Connector, RefusePrompts};
use engine_connect::drivers::{
    driver_data_dir_override, resolve_driver_data_dir, DRIVER_DATA_DIR_ENV,
};
use engine_connect::platform::audit_log_path;
use engine_driver_host::InstalledDriverStore;
use engine_storage::paths::{data_dir_override, DATA_DIR_ENV};

use crate::app_port::AppChannel;
use crate::approver::ElicitingApprover;
use crate::connection_admin::ConnectionStore;

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
/// user to connect once from the app), a write is put to the user through MCP elicitation (or in the
/// app) when the connector's own `settings.json`, read at the time of each write, allows it (a file
/// of schema 7 or later always does; an older app's `ai.mcp_writes` still counts for an older file)
/// and refused otherwise ([`ElicitingApprover`]), things can be shown in the
/// running app through its socket ([`AppChannel`]), the saved connections can be added, copied and
/// removed with the user's approval each time ([`ConnectionStore`]), and every call is audited.
pub fn build_layer(connector: Connector, audit: Arc<AuditLog>) -> ToolLayer {
    build_layer_with_channel(
        connector,
        audit,
        Arc::new(AppChannel::at_default_location()),
        AppClient::at_default_location(),
    )
}

/// [`build_layer`] with the channel to the running app (`open_table`, `execute_and_show`) given.
pub fn build_layer_with_app(
    connector: Connector,
    audit: Arc<AuditLog>,
    app: Arc<dyn AppShow>,
) -> ToolLayer {
    build_layer_with_channel(connector, audit, app, AppClient::at_default_location())
}

/// [`build_layer_with_app`] with the client that tells the app its connections changed
/// (`add_connection`, `duplicate_connection`, `remove_connection`) given too (`None`: nobody to tell).
pub fn build_layer_with_channel(
    connector: Connector,
    audit: Arc<AuditLog>,
    app: Arc<dyn AppShow>,
    notify: Option<AppClient>,
) -> ToolLayer {
    // On macOS a Keychain that will not hand a secret over is the user's one-time authorisation to
    // give: say which command does it (both the open and the connection tools print it).
    #[cfg(target_os = "macos")]
    let exe = std::env::current_exe().and_then(std::path::absolute).ok();
    #[cfg(target_os = "macos")]
    let connector = match &exe {
        Some(exe) => connector.with_keychain_command(crate::keychain::authorize_command(exe)),
        None => connector,
    };
    let settings_path = connector.settings_path().map(Path::to_path_buf);
    // The tools the user allows, read from `settings.json` at every call.
    let tools_path = settings_path.clone();
    let admin_connector = connector.clone();
    // This process holds no connection of the user's own, so a DuckDB file an agent opens can be
    // opened without file access (an instance has one setting for all its connections).
    let connections = AgentConnections::new(connector, Arc::new(RefusePrompts)).with_locked_file_access();
    #[cfg(target_os = "macos")]
    let connections = match &exe {
        Some(exe) => connections.with_credential_hint(crate::keychain::credential_hint(exe)),
        None => connections,
    };
    let connections = Arc::new(connections);
    // One approver: the questions of writes and of connection changes share its budget of open
    // questions.
    // A client that cannot ask through MCP gets its questions in the running app (`notify` is the
    // channel to it, or `None` where there is none).
    let approver = Arc::new(ElicitingApprover::new(settings_path).with_app(notify.clone()));
    let admin = ConnectionStore::new(admin_connector, approver.clone(), notify);
    ToolLayer::new(connections.clone(), connections, FRONT_END)
        // Explicit transactions (PostgreSQL, MySQL, MariaDB, SQLite): every write and the commit are put to the
        // user, and an abandoned transaction is always rolled back (see `McpServer::serve`).
        .with_transactions()
        // `open_session` with `pinned`: a connection of its own that keeps `USE` and `SET`s, only
        // for the client that opened it, closed when idle.
        .with_pinned_sessions()
        // `ai.mcp_tools` of `settings.json`: the only tools the user lets through, when it is set.
        .with_tool_allowlist(Arc::new(move || engine_connect::profile::read_mcp_tools(tools_path.as_deref())))
        // `open_table` and `execute_and_show`: shown in the running app, which applies its own
        // rules to a query it is asked to run.
        .with_app(app)
        // `add_connection`, `duplicate_connection`, `remove_connection`: each put to the user.
        .with_connection_admin(Arc::new(admin))
        .with_approver(approver)
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

/// The connector over the app's stores (also what `--authorize-keychain` reads the secrets with).
pub fn production_connector() -> Connector {
    // The app resolves the store the same way (and moves a 0.1.0 store to the new place first).
    let drivers = resolve_driver_data_dir();
    for line in drivers.moved.iter().chain(drivers.warning.iter()) {
        eprintln!("tablex-mcp: {line}");
    }
    let connector = Connector::new(Arc::new(InstalledDriverStore::new(drivers.path)));
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
