//! `--authorize-keychain`: read, once and up front, the tablex master key and the saved secrets of
//! every connection an agent may use, so macOS asks in one sitting (the user chooses Always Allow)
//! and not in the middle of an agent's session. Saved secrets are encrypted rows of the app
//! database (`tablex.db`, shared with the app: SQLite WAL, so both can run at once) whose key is a
//! single Keychain item; its access list trusts the app, and this program is a separate executable,
//! so macOS asks once for it. Secrets an older version kept as Keychain items of their own are
//! read as they are (macOS asks for each of those once more): this program never copies them into
//! the database and never creates the master key, only the app does (its "Allow Keychain access"
//! moves them all in first, in one pass, see `EncryptedStore::copy_legacy_items`). While serving,
//! the process never lets the Keychain ask the user (see `main.rs`). Nothing here prints a secret.

use std::io::Write;
use std::process::{Command, ExitCode, Stdio};

use engine_agent::AgentAccess;
use engine_connect::connect::Connector;
use engine_storage::connection::SavedConnectionRecord;

/// The hidden argument of the child that checks the access was kept.
pub const VERIFY_ARG: &str = "--verify-keychain-access";

/// What reading the saved secrets of one connection came to.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Outcome {
    Ok,
    NothingSaved,
    /// The store refused or could not be read (the error text holds no secret).
    NotAllowed(String),
}

impl Outcome {
    fn line(&self, name: &str) -> String {
        match self {
            Self::Ok => format!("{name}: ok"),
            Self::NothingSaved => format!("{name}: nothing saved"),
            Self::NotAllowed(error) => format!("{name}: not allowed ({error})"),
        }
    }
}

/// The connections an agent may use, read, and the ones it may not.
#[derive(Debug, Default)]
pub struct Report {
    pub read: Vec<(String, Outcome)>,
    /// Connections whose effective agent access is off.
    pub skipped: usize,
}

impl Report {
    pub fn failed(&self) -> impl Iterator<Item = &str> {
        self.read
            .iter()
            .filter(|(_, outcome)| matches!(outcome, Outcome::NotAllowed(_)))
            .map(|(name, _)| name.as_str())
    }
}

/// Reads the saved secrets (through `connector`, exactly the ones an open uses) of every record
/// whose effective agent access is at least schema-only (what `open_session` needs).
pub fn check(connector: &Connector, records: &[SavedConnectionRecord]) -> Result<Report, String> {
    // The one item macOS asks about: the master key of the saved secrets.
    connector
        .unlock_credentials()
        .map_err(|error| format!("cannot read the tablex master key ({error})"))?;
    // A settings file that cannot be read means the server treats every connection as off.
    let settings = connector
        .profile_settings()
        .map_err(|error| format!("cannot read the tablex settings: {error}"))?;
    let mut report = Report::default();
    for record in records {
        // A connection of a type the agent never sees is not one it can use.
        let Some(info) = settings.agent_info(record) else {
            continue;
        };
        if info.effective_access() < AgentAccess::SchemaOnly {
            report.skipped += 1;
            continue;
        }
        let outcome = match connector.read_saved_secrets(record) {
            Ok(secrets) if secrets.mask_texts().is_empty() => Outcome::NothingSaved,
            Ok(_) => Outcome::Ok,
            Err(error) => Outcome::NotAllowed(error.to_string()),
        };
        report.read.push((record.name.clone(), outcome));
    }
    Ok(report)
}

fn read_report(connector: &Connector) -> Result<Report, String> {
    let records = engine_storage::connection::list_connections()
        .map_err(|error| format!("cannot read the saved connections: {error}"))?;
    check(connector, &records)
}

/// `tablex-mcp --authorize-keychain`.
pub fn authorize(connector: &Connector) -> ExitCode {
    if !cfg!(target_os = "macos") {
        println!("The Keychain is a macOS feature: there is nothing to authorize on this platform.");
        return ExitCode::SUCCESS;
    }
    println!("macOS asks once, for the tablex master key: choose Always Allow.");
    let report = match read_report(connector) {
        Ok(report) => report,
        Err(error) => return fail(&error),
    };
    let mut out = std::io::stdout().lock();
    for (name, outcome) in &report.read {
        let _ = writeln!(out, "{}", outcome.line(name));
    }
    if report.skipped > 0 {
        let _ = writeln!(
            out,
            "{} {} off for agents and {} skipped",
            report.skipped,
            plural(report.skipped, "connection is", "connections are"),
            if report.skipped == 1 { "was" } else { "were" },
        );
    }
    if report.read.is_empty() {
        let _ = writeln!(
            out,
            "No saved connection is open to agents, so there is nothing to allow."
        );
        return ExitCode::SUCCESS;
    }
    let _ = out.flush();
    drop(out);
    // The reads above may have been allowed for this run only; a second process that cannot ask
    // shows whether the access was kept for background use.
    if report.failed().next().is_some() || !verified() {
        eprintln!(
            "tablex-mcp: access was not kept for background use. Run this command again and \
             choose Always Allow when macOS asks."
        );
        return ExitCode::FAILURE;
    }
    println!(
        "Keychain access is allowed for {} {}. Restart your MCP client if it is running.",
        report.read.len(),
        plural(report.read.len(), "connection", "connections"),
    );
    ExitCode::SUCCESS
}

fn plural<'a>(n: usize, one: &'a str, many: &'a str) -> &'a str {
    if n == 1 {
        one
    } else {
        many
    }
}

/// Runs this executable again with [`VERIFY_ARG`]; true when it exits 0.
fn verified() -> bool {
    let Ok(exe) = std::env::current_exe() else {
        return false;
    };
    Command::new(exe)
        .arg(VERIFY_ARG)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .status()
        .is_ok_and(|status| status.success())
}

/// `tablex-mcp --verify-keychain-access`: the same reads with the Keychain's prompt switched off
/// for this process, so a read that would still ask fails instead. Names that fail go to stderr.
pub fn verify(connector: &Connector) -> ExitCode {
    // Held for the whole of the reads.
    #[cfg(target_os = "macos")]
    let _no_prompt = match security_framework::os::macos::keychain::SecKeychain::disable_user_interaction()
    {
        Ok(guard) => guard,
        Err(error) => return fail(&format!("cannot turn the Keychain prompt off: {error}")),
    };
    let report = match read_report(connector) {
        Ok(report) => report,
        Err(error) => return fail(&error),
    };
    let mut failed = false;
    for name in report.failed() {
        failed = true;
        eprintln!("tablex-mcp: not allowed: {name}");
    }
    if failed {
        ExitCode::FAILURE
    } else {
        ExitCode::SUCCESS
    }
}

fn fail(message: &str) -> ExitCode {
    eprintln!("tablex-mcp: {message}");
    ExitCode::FAILURE
}

/// The command that runs the authorisation, with the path quoted for a POSIX shell (single quotes;
/// a `'` is written `'\''`): what the errors for an agent print, since a tool the Driver Manager
/// installed is not on `PATH`.
pub fn authorize_command(exe: &std::path::Path) -> String {
    let path = exe.to_string_lossy().replace('\'', "'\\''");
    format!("'{path}' --authorize-keychain")
}

/// What the model is told when the Keychain cannot be read (macOS): the command that fixes it.
pub fn credential_hint(exe: &std::path::Path) -> String {
    format!(
        "macOS Keychain did not let tablex-mcp read the tablex master key: run {} once \
         in Terminal and choose Always Allow, then restart the MCP client.",
        authorize_command(exe)
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use engine_connect::credentials::{CredentialError, CredentialStore, SecretKind};
    use engine_driver_host::InstalledDriverStore;
    use std::path::Path;
    use std::sync::Arc;

    /// Answers `load` with `answer` (a secret, nothing, or a refusal).
    struct Store(Result<Option<&'static str>, &'static str>);

    impl CredentialStore for Store {
        fn load(&self, _: SecretKind, _: &str) -> Result<Option<String>, CredentialError> {
            self.0
                .map(|text| text.map(str::to_owned))
                .map_err(|error| CredentialError::Platform(error.into()))
        }
        fn write(&self, _: SecretKind, _: &str, _: &str) -> Result<(), CredentialError> {
            Err(CredentialError::NoKeyring)
        }
        fn delete(&self, _: SecretKind, _: &str) -> Result<(), CredentialError> {
            Ok(())
        }
    }

    /// A store whose master key the user refused to hand over.
    struct Locked;

    impl CredentialStore for Locked {
        fn load(&self, _: SecretKind, _: &str) -> Result<Option<String>, CredentialError> {
            Ok(None)
        }
        fn write(&self, _: SecretKind, _: &str, _: &str) -> Result<(), CredentialError> {
            Ok(())
        }
        fn delete(&self, _: SecretKind, _: &str) -> Result<(), CredentialError> {
            Ok(())
        }
        fn unlock(&self) -> Result<(), CredentialError> {
            Err(CredentialError::Platform("user canceled".into()))
        }
    }

    fn record(name: &str, access: &str) -> SavedConnectionRecord {
        SavedConnectionRecord {
            id: format!("00000000-0000-4000-8000-{:012}", name.len()),
            name: name.into(),
            uri: "postgresql://app@127.0.0.1:1/db".into(),
            db_type: "postgresql".into(),
            color_hex: "#34C759".into(),
            save_password: true,
            save_proxy_password: false,
            network_settings_json: "{}".into(),
            is_pinned: false,
            last_connected: None,
            created_at: 1.0,
            updated_at: 1.0,
            connection_type: String::new(),
            overrides_json: serde_json::json!({ "agent.access": access }).to_string(),
        }
    }

    fn report(store: Store) -> Report {
        let dir = tempfile::tempdir().unwrap();
        let connector = Connector::new(Arc::new(InstalledDriverStore::new(dir.path())))
            .with_credentials(Arc::new(store))
            .with_settings_path(Some(dir.path().join("settings.json")));
        let records = [
            record("read", "read"),
            record("off", "off"),
            record("schema", "schema"),
        ];
        check(&connector, &records).unwrap()
    }

    #[test]
    fn only_connections_open_to_agents_are_read_and_the_others_are_counted() {
        let report = report(Store(Ok(Some("hunter2-secret"))));
        assert_eq!(
            report.read,
            vec![
                ("read".to_owned(), Outcome::Ok),
                ("schema".to_owned(), Outcome::Ok)
            ]
        );
        assert_eq!(report.skipped, 1);
        assert_eq!(report.failed().count(), 0);
        assert_eq!(self::report(Store(Ok(None))).read[0].1, Outcome::NothingSaved);
    }

    #[test]
    fn a_refused_read_is_not_allowed_and_names_the_connection_not_the_secret() {
        let report = report(Store(Err("user canceled")));
        assert_eq!(report.failed().collect::<Vec<_>>(), ["read", "schema"]);
        let line = report.read[0].1.line("read");
        assert!(line.starts_with("read: not allowed ("), "{line}");
    }

    #[test]
    fn a_master_key_that_cannot_be_read_stops_the_check_before_any_connection() {
        let dir = tempfile::tempdir().unwrap();
        let connector = Connector::new(Arc::new(InstalledDriverStore::new(dir.path())))
            .with_credentials(Arc::new(Locked))
            .with_settings_path(Some(dir.path().join("settings.json")));
        let error = check(&connector, &[record("read", "read")]).unwrap_err();
        assert_eq!(error, "cannot read the tablex master key (user canceled)");
    }

    #[test]
    fn the_hint_quotes_a_path_with_spaces_and_quotes() {
        let hint = credential_hint(Path::new("/Users/a b/it's/tablex-mcp"));
        assert!(
            hint.contains(r"run '/Users/a b/it'\''s/tablex-mcp' --authorize-keychain once"),
            "{hint}"
        );
        assert_eq!(
            authorize_command(Path::new("/Users/a b/it's/tablex-mcp")),
            r"'/Users/a b/it'\''s/tablex-mcp' --authorize-keychain"
        );
    }

    #[test]
    fn an_outcome_line_never_has_more_than_the_name_and_the_error() {
        assert_eq!(Outcome::Ok.line("a"), "a: ok");
        assert_eq!(Outcome::NothingSaved.line("a"), "a: nothing saved");
        assert_eq!(
            Outcome::NotAllowed("denied".into()).line("a"),
            "a: not allowed (denied)"
        );
    }
}
