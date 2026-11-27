//! `add_connection`, `duplicate_connection` and `remove_connection`: the MCP server's
//! [`ConnectionAdmin`] over the app's saved connections (`tablex.db`) and credential store.
//!
//! **Every change is put to the user** through MCP elicitation, or in the tablex app when the client
//! cannot ask ([`ElicitingApprover::ask_connection_change`]), with a question that says what will be added,
//! copied or removed: name, kind, address (the stored URI, which has no password), connection type
//! (and that it is a production one), whether a password is saved or copied. The password is never
//! in a question, the audit line, an error or the stored record: it goes to the credential store
//! only, under the connection's id, as the New Connection dialog saves it. A question that is
//! declined, dismissed or not answered, or a client that can be asked in neither way, changes nothing.
//!
//! Records are built by [`engine_connect::new_connection`] (add) and
//! [`engine_connect::records::duplicate_record`] (duplicate, as the sidebar's Duplicate does:
//! options outside the routing set, the access setting and the tunnel's secrets are not copied), so
//! this adds no format of its own. A new connection has no access setting of its own: it follows
//! the user's settings for its type. After a change the running app is told to reload
//! ([`Request::ConnectionsChanged`]); an app that is not running is not an error.

use std::sync::{Arc, Mutex};

use engine_agent::sanitize::{is_right_to_left, is_shapeless, spell_out};
use engine_agent::{
    AgentError, ApprovalDecision, CancelToken, ConnectionAdmin, ConnectionChange,
    ConnectionChanged, ConnectionInfo,
};
use engine_connect::app_channel::{AppClient, ApprovalAsk, Request};
use engine_connect::connect::{accounts_of, Connector};
use engine_connect::credentials::{CredentialError, SecretKind};
use engine_connect::new_connection::{self, NewConnection, Tls};
use engine_connect::profile::{self, ConnectionType, DEVELOPMENT};
use engine_connect::records::{self, ConnectionKind};
use engine_storage::connection::{self, SavedConnectionRecord};

use crate::approver::{code_span, ConnectionQuestion, ElicitingApprover, CONNECTION_TITLE};

/// What a change comes to, found before the user is asked and again when it is made.
enum Plan {
    Add {
        record: SavedConnectionRecord,
        password: Option<String>,
        type_name: String,
        production: bool,
    },
    Duplicate {
        source: Box<SavedConnectionRecord>,
        copy: Box<SavedConnectionRecord>,
        source_name: String,
        type_name: String,
        production: bool,
    },
    Remove {
        record: SavedConnectionRecord,
        type_name: String,
        production: bool,
    },
}

/// [`ConnectionAdmin`] over the app's connection store. Changes are made one at a time, so two
/// calls cannot both take a name.
pub struct ConnectionStore {
    connector: Connector,
    approver: Arc<ElicitingApprover>,
    /// `None`: this platform has no data directory, so there is no socket to tell the app on.
    app: Option<AppClient>,
    changing: Mutex<()>,
}

fn store_error(what: &str, error: impl std::fmt::Display) -> AgentError {
    AgentError::driver("connections.store_failed", format!("{what}: {error}"), false)
}

fn credentials_error(connector: &Connector, what: &str, error: CredentialError) -> AgentError {
    // The Keychain would have to ask the user: say how to allow it, as an open does.
    if error == CredentialError::InteractionNotAllowed {
        return AgentError::driver("credentials.keychain_access_needed", connector.keychain_access_needed(), false);
    }
    AgentError::driver("credentials.unavailable", format!("{what}: {error}"), false)
}

/// What to say of a removed connection whose secrets could not all be cleared. An old Keychain
/// item that macOS would have to ask about stays (nothing here may ask). Nothing reads it again;
/// the user may delete it in Keychain Access.
fn removal_warning(error: &CredentialError) -> String {
    if *error == CredentialError::InteractionNotAllowed {
        return "the connection was removed, but an old Keychain item of its password stays (macOS would \
                have to ask before deleting it). Nothing reads it any more; delete it in Keychain Access if \
                you like"
            .to_owned();
    }
    format!("the connection was removed, but its saved secrets could not be cleared: {error}")
}

/// Said when "Don't save passwords" is on and a password was given or would be copied: the
/// connection is made without one, as the app's own dialog does.
const NOT_SAVED: &str = "passwords are not being saved (Don't save passwords is on), so the connection has no saved password; tablex asks for it when connecting";

fn now() -> f64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|duration| duration.as_secs_f64())
        .unwrap_or(0.0)
}

impl ConnectionStore {
    pub fn new(connector: Connector, approver: Arc<ElicitingApprover>, app: Option<AppClient>) -> Self {
        Self {
            connector,
            approver,
            app,
            changing: Mutex::new(()),
        }
    }

    fn settings(&self) -> Result<profile::ProfileSettings, AgentError> {
        self.connector.profile_settings().map_err(|error| {
            AgentError::invalid(format!(
                "the connection types could not be read from the tablex settings, so nothing was changed: {error}"
            ))
        })
    }

    /// The connection type named `wanted` (an id or a name; default `development`).
    fn resolve_type(
        &self,
        wanted: Option<&str>,
    ) -> Result<ConnectionType, AgentError> {
        let types = self.settings()?.connection_types;
        let wanted = wanted.map(str::trim).filter(|wanted| !wanted.is_empty()).unwrap_or(DEVELOPMENT);
        types
            .iter()
            .find(|ty| ty.id == wanted)
            .or_else(|| types.iter().find(|ty| ty.name.trim().eq_ignore_ascii_case(wanted)))
            .cloned()
            .ok_or_else(|| {
                let known: Vec<&str> = types.iter().map(|ty| ty.id.as_str()).collect();
                AgentError::invalid(format!(
                    "unknown connection type `{}`; the user's types are: {}",
                    wanted.chars().take(64).collect::<String>(),
                    known.join(", ")
                ))
            })
    }

    /// The name, valid and not used (case-insensitive) by another saved connection.
    fn free_name(name: &str) -> Result<String, AgentError> {
        let name = new_connection::check_name(name).map_err(AgentError::invalid)?;
        if name.chars().any(is_shapeless) {
            return Err(AgentError::invalid(
                "the connection name has a character with no shape of its own (a direction change or zero-width character)",
            ));
        }
        let records = connection::list_connections().map_err(|error| store_error("the saved connections could not be read", error))?;
        if records.iter().any(|record| record.name.trim().to_lowercase() == name.to_lowercase()) {
            return Err(AgentError::invalid(format!(
                "a connection named `{name}` already exists (names are compared without regard to case); choose another name"
            )));
        }
        Ok(name)
    }

    fn saved(&self, info: &ConnectionInfo) -> Result<SavedConnectionRecord, AgentError> {
        // The id is stored in the spelling the app that saved it used (upper or lower case).
        for id in accounts_of(&info.id) {
            let found = connection::get_connection(&id)
                .map_err(|error| store_error("the saved connection could not be read", error))?;
            if let Some(record) = found {
                return Ok(record);
            }
        }
        Err(AgentError::NotConnected(format!("connection `{}` no longer exists", info.name)))
    }

    /// The TLS choice of an `add_connection`: `options.tls`, and/or the `ssl` flag (`true` is
    /// `require`, or `verify_full` where `require` is not offered; `false` is `disable`). Both
    /// must say the same.
    fn tls_choice(request: &engine_agent::tools::AddConnectionRequest, kind: ConnectionKind) -> Result<Option<Tls>, AgentError> {
        let from_id = match request.options.as_ref().and_then(|options| options.tls.as_deref()) {
            Some(id) => Some(Tls::from_id(id).ok_or_else(|| {
                AgentError::invalid(format!("`options.tls` is one of {}", Tls::IDS.join(", ")))
            })?),
            None => None,
        };
        let flags = [request.ssl, request.options.as_ref().and_then(|options| options.ssl)];
        let ssl = match (flags[0], flags[1]) {
            (Some(a), Some(b)) if a != b => {
                return Err(AgentError::invalid("`ssl` and `options.ssl` disagree"));
            }
            (a, b) => a.or(b),
        };
        let from_flag = ssl.map(|on| match (on, kind) {
            (false, _) => Tls::Disable,
            (true, ConnectionKind::Mongodb) => Tls::VerifyFull,
            (true, _) => Tls::Require,
        });
        match (from_id, from_flag) {
            (Some(id), Some(flag)) => {
                let agree = id == flag || (flag == Tls::Require && id == Tls::VerifyFull);
                if !agree {
                    return Err(AgentError::invalid(
                        "`ssl` and `options.tls` disagree: `ssl` true means `require` or `verify_full`, false means `disable`",
                    ));
                }
                Ok(Some(id))
            }
            (id, flag) => Ok(id.or(flag)),
        }
    }

    fn plan(&self, change: &ConnectionChange<'_>) -> Result<Plan, AgentError> {
        match change {
            ConnectionChange::Add(request) => {
                let kind = ConnectionKind::from_persisted(request.kind.driver_id())
                    .ok_or_else(|| AgentError::invalid("this kind of connection is not supported"))?;
                let tls = Self::tls_choice(request, kind)?;
                if let Some(password) = &request.password {
                    new_connection::check_password(password).map_err(AgentError::invalid)?;
                }
                let ty = self.resolve_type(request.connection_type.as_deref())?;
                let name = Self::free_name(&request.name)?;
                let new = NewConnection {
                    name,
                    kind,
                    uri: request.uri.clone(),
                    host: request.host.clone(),
                    port: request.port,
                    database: request.database.clone(),
                    user: request.user.clone(),
                    path: request.path.clone(),
                    tls,
                    connection_type: ty.id.clone(),
                };
                let mut record = new_connection::build_record(&new, &new_connection::new_id(), now())
                    .map_err(AgentError::invalid)?;
                record.save_password = request.password.is_some();
                Ok(Plan::Add {
                    record,
                    password: request.password.clone(),
                    type_name: ty.name.clone(),
                    production: ty.is_production(),
                })
            }
            ConnectionChange::Duplicate { source, name } => {
                let name = Self::free_name(name)?;
                let record = self.saved(source)?;
                let mut copy = records::duplicate_record(&record, &new_connection::new_id(), now())
                    .map_err(|error| AgentError::invalid(format!("the connection cannot be copied: {error}")))?;
                copy.name = name;
                Ok(Plan::Duplicate {
                    source: Box::new(record),
                    copy: Box::new(copy),
                    source_name: source.name.clone(),
                    type_name: source.connection_type.name.clone(),
                    production: source.connection_type.production,
                })
            }
            ConnectionChange::Remove { connection } => Ok(Plan::Remove {
                record: self.saved(connection)?,
                type_name: connection.connection_type.name.clone(),
                production: connection.connection_type.production,
            }),
        }
    }

    /// The one-line description for the audit log: no password, and the address is the stored URI,
    /// which has none.
    fn describe(plan: &Plan) -> String {
        match plan {
            Plan::Add { record, password, type_name, .. } => format!(
                "add_connection name={:?} kind={} type={:?} address={} password={}",
                record.name,
                record.db_type,
                type_name,
                record.uri,
                if password.is_some() { "saved" } else { "none" }
            ),
            Plan::Duplicate { source, copy, .. } => format!(
                "duplicate_connection of={} name={:?} kind={} address={} password={}",
                records::display_connection_id(&source.id),
                copy.name,
                copy.db_type,
                copy.uri,
                if source.save_password { "copied" } else { "none" }
            ),
            Plan::Remove { record, production, .. } => format!(
                "remove_connection id={} name={:?} kind={} address={}{}",
                records::display_connection_id(&record.id),
                record.name,
                record.db_type,
                record.uri,
                if *production { " production" } else { "" }
            ),
        }
    }

    /// The question. Every name and address is a code span; characters without a shape of their
    /// own are spelled out, and right-to-left letters are announced.
    fn message(plan: &Plan) -> String {
        let label = |text: &str| code_span(&spell_out(text, false).0);
        let kind_type = |record: &SavedConnectionRecord, type_name: &str, production: bool| {
            let ty = if type_name.trim().is_empty() { "No type".to_owned() } else { label(type_name) };
            let mut text = format!(
                "Kind: {}\n\nAddress (the password is never part of it): {}\n\nConnection type: {ty}\n\n",
                label(&record.db_type),
                label(&record.uri),
            );
            if production {
                text.push_str("WARNING: this is a PRODUCTION connection.\n\n");
            }
            text
        };
        let (mut message, shown): (String, Vec<&str>) = match plan {
            Plan::Add { record, type_name, production, .. } => {
                let mut message = format!(
                    "An MCP client asks to ADD a connection to tablex. Nothing is added unless you accept. To add it, choose \"{CONNECTION_TITLE}\" and accept.\n\nName: {}\n\n{}",
                    label(&record.name),
                    kind_type(record, type_name, *production),
                );
                message.push_str(&format!("{}\n\n{}", Self::password_note(plan), Self::footer(plan)));
                (message, vec![&record.name, &record.uri, type_name])
            }
            Plan::Duplicate { copy, source_name, type_name, production, .. } => {
                let mut message = format!(
                    "An MCP client asks to COPY the connection {} as a new connection. Nothing is copied unless you accept. To copy it, choose \"{CONNECTION_TITLE}\" and accept.\n\nNew name: {}\n\n{}",
                    label(source_name),
                    label(&copy.name),
                    kind_type(copy, type_name, *production),
                );
                message.push_str(&format!("{}\n\n{}", Self::password_note(plan), Self::footer(plan)));
                (message, vec![&copy.name, source_name, &copy.uri, type_name])
            }
            Plan::Remove { record, type_name, production } => {
                let mut message = format!(
                    "An MCP client asks to REMOVE a connection from tablex. This cannot be undone. Nothing is removed unless you accept. To remove it, choose \"{CONNECTION_TITLE}\" and accept.\n\nName: {}\n\n{}",
                    label(&record.name),
                    kind_type(record, type_name, *production),
                );
                message.push_str(&Self::footer(plan));
                (message, vec![&record.name, &record.uri, type_name])
            }
        };
        if shown.iter().any(|text| text.chars().any(is_right_to_left)) {
            message.push_str(
                "\n\nWarning: a name above has right-to-left letters, which can change how the text around it is displayed. Read it with care.",
            );
        }
        message
    }

    /// The "Password: ..." line of a question (none for a removal).
    fn password_note(plan: &Plan) -> String {
        let saves = engine_connect::key_home::passwords_saved();
        match plan {
            Plan::Add { password: Some(_), .. } if !saves => "Password: one is given, but passwords are not being saved (Don't save passwords is on); the connection is added without one and tablex asks for it when connecting.",
            Plan::Add { password: Some(_), .. } => "Password: one is given; it is saved in tablex's credential store and not shown here.",
            Plan::Add { .. } => "Password: none is saved; tablex asks for it when connecting.",
            Plan::Duplicate { source, .. } if source.save_password && !saves => "Password: passwords are not being saved (Don't save passwords is on), so the copy has none and tablex asks for it when connecting.",
            Plan::Duplicate { source, .. } if source.save_password => "Password: the saved password of the original is copied; it is not shown here.",
            Plan::Duplicate { .. } => "Password: none (the original has none saved).",
            Plan::Remove { .. } => "",
        }
        .to_owned()
    }

    /// The closing sentence of a question.
    fn footer(plan: &Plan) -> String {
        match plan {
            Plan::Add { .. } => "The connection has no agent access setting of its own: it follows your settings for its connection type.",
            Plan::Duplicate { .. } => "The copy has no agent access setting of its own: it follows your settings for its connection type.",
            Plan::Remove { .. } => "Its saved passwords and pinned objects are removed with it.",
        }
        .to_owned()
    }

    /// What the user can do in the app instead of asking the agent, for a refusal.
    fn instead(change: &ConnectionChange<'_>) -> &'static str {
        match change {
            ConnectionChange::Add(_) => "add the connection in the tablex app (New Connection)",
            ConnectionChange::Duplicate { .. } => "copy the connection in the tablex app",
            ConnectionChange::Remove { .. } => "remove the connection in the tablex app",
        }
    }

    /// The same facts as [`message`](Self::message) for the app's dialog, as plain text (names are
    /// spelled, not marked up): the sentence that says what is asked, the production warning as the
    /// danger line, and the details in the block the dialog shows in monospace.
    fn app_question(plan: &Plan) -> ApprovalAsk {
        let name = |text: &str| spell_out(text, false).0;
        let (what, record, type_name, production, first) = match plan {
            Plan::Add { record, type_name, production, .. } => (
                "An MCP client asks to ADD a connection to tablex. Nothing is added unless you approve.",
                record,
                type_name,
                *production,
                ("Name", name(&record.name)),
            ),
            Plan::Duplicate { copy, type_name, production, source_name, .. } => (
                "An MCP client asks to COPY a connection as a new connection. Nothing is copied unless you approve.",
                &**copy,
                type_name,
                *production,
                ("Copy of", name(source_name)),
            ),
            Plan::Remove { record, type_name, production } => (
                "An MCP client asks to REMOVE a connection from tablex. This cannot be undone. Nothing is removed unless you approve.",
                record,
                type_name,
                *production,
                ("Name", name(&record.name)),
            ),
        };
        let mut lines = vec![format!("{}: {}", first.0, first.1)];
        if matches!(plan, Plan::Duplicate { .. }) {
            lines.push(format!("New name: {}", name(&record.name)));
        }
        lines.push(format!("Kind: {}", name(&record.db_type)));
        lines.push(format!("Address (the password is never part of it): {}", name(&record.uri)));
        lines.push(format!(
            "Connection type: {}",
            if type_name.trim().is_empty() { "No type".to_owned() } else { name(type_name) }
        ));
        let password = Self::password_note(plan);
        if !password.is_empty() {
            lines.push(password);
        }
        let mut notes = vec![what.to_owned(), Self::footer(plan)];
        let shown = [&record.name, &record.uri, type_name];
        if shown.iter().any(|text| text.chars().any(is_right_to_left)) {
            notes.push("Warning: a name above has right-to-left letters, which can change how the text around it is displayed. Read it with care.".to_owned());
        }
        ApprovalAsk {
            title: CONNECTION_TITLE.to_owned(),
            danger: production.then(|| "this is a PRODUCTION connection".to_owned()),
            // Removing cannot be undone: approved by a mouse click only.
            click_only: matches!(plan, Plan::Remove { .. }),
            notes,
            statements: vec![lines.join("\n")],
            ..ApprovalAsk::default()
        }
    }

    /// Both forms of the question about `plan`.
    fn question(plan: &Plan, change: &ConnectionChange<'_>) -> ConnectionQuestion {
        ConnectionQuestion {
            message: Self::message(plan),
            app: Self::app_question(plan),
            instead: Self::instead(change),
        }
    }

    /// Tells the running app; whatever comes back, the change was made.
    fn notify(&self) -> bool {
        self.app
            .as_ref()
            .is_some_and(|app| app.send(&Request::ConnectionsChanged {}).is_ok())
    }

    fn driver_missing(&self, db_type: &str) -> Option<String> {
        matches!(self.connector.driver_store().record(db_type), Ok(None)).then(|| {
            format!(
                "the {db_type} driver is not installed yet: install it from tablex's Driver Manager before connecting"
            )
        })
    }

    /// Stores `record`, and `password` under its id first (a secret without a record is cleaned up
    /// when the record cannot be saved; a record never claims a password that was not stored).
    fn store(&self, mut record: SavedConnectionRecord, password: Option<&str>) -> Result<SavedConnectionRecord, AgentError> {
        let credentials = self.connector.credentials();
        record.save_password = password.is_some();
        if let Some(password) = password {
            credentials
                .write(SecretKind::Database, &record.id, password)
                .map_err(|error| credentials_error(&self.connector, "the password could not be saved, so the connection was not added", error))?;
        }
        if let Err(error) = connection::upsert_connection(&record) {
            if password.is_some() {
                let _ = credentials.delete(SecretKind::Database, &record.id);
            }
            return Err(store_error("the connection could not be saved", error));
        }
        Ok(record)
    }
}

impl ConnectionAdmin for ConnectionStore {
    fn check(&self, change: &ConnectionChange<'_>) -> Result<String, AgentError> {
        self.plan(change).map(|plan| Self::describe(&plan))
    }

    fn approve(&self, change: &ConnectionChange<'_>, cancel: &CancelToken) -> ApprovalDecision {
        match self.plan(change) {
            Ok(plan) => self.approver.ask_connection_change(Self::question(&plan, change), cancel),
            Err(error) => ApprovalDecision::Refuse { reason: error.to_string() },
        }
    }

    fn apply(&self, change: &ConnectionChange<'_>) -> Result<ConnectionChanged, AgentError> {
        let _one_at_a_time = self.changing.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
        // The question may have waited a long time: the app's switch may be off by now.
        self.approver
            .connection_change_still_allowed(Self::instead(change))
            .map_err(|reason| AgentError::WriteRefused(format!("the change was approved, but it can no longer be made: {reason}")))?;
        // Looked at again: the name may be taken, the connection gone.
        let plan = self.plan(change)?;
        let mut changed = match plan {
            Plan::Add { record, password, .. } => {
                let saves = engine_connect::key_home::passwords_saved();
                let dropped = !saves && password.is_some();
                let record = self.store(record, password.filter(|_| saves).as_deref())?;
                ConnectionChanged {
                    connection_id: records::display_connection_id(&record.id),
                    password_saved: record.save_password,
                    app_notified: false,
                    warning: dropped
                        .then(|| NOT_SAVED.to_owned())
                        .or_else(|| self.driver_missing(&record.db_type)),
                }
            }
            Plan::Duplicate { source, copy, .. } => {
                let saves = engine_connect::key_home::passwords_saved();
                let mut password = None;
                if source.save_password && saves {
                    // Under the spelling of the id the secret was saved with.
                    for account in accounts_of(&source.id) {
                        password = self
                            .connector
                            .credentials()
                            .load(SecretKind::Database, &account)
                            .map_err(|error| credentials_error(&self.connector, "the saved password of the original could not be read, so nothing was copied", error))?;
                        if password.is_some() {
                            break;
                        }
                    }
                }
                let missing = source.save_password && saves && password.is_none();
                let dropped = source.save_password && !saves;
                let record = self.store(*copy, password.as_deref())?;
                ConnectionChanged {
                    connection_id: records::display_connection_id(&record.id),
                    password_saved: record.save_password,
                    app_notified: false,
                    warning: missing
                        .then(|| "the original's saved password was not found, so the copy has none".to_owned())
                        .or_else(|| dropped.then(|| NOT_SAVED.to_owned()))
                        .or_else(|| self.driver_missing(&record.db_type)),
                }
            }
            Plan::Remove { record, .. } => {
                // The record goes first, with its memberships and pinned objects; then the secrets.
                connection::delete_connection(&record.id).map_err(|error| store_error("the connection could not be removed", error))?;
                // Every spelling of the id is cleared, whatever happens to the others.
                let mut warning = None;
                for account in accounts_of(&record.id) {
                    if let Err(error) = self.connector.credentials().delete_all(&account) {
                        warning.get_or_insert(removal_warning(&error));
                    }
                }
                ConnectionChanged {
                    connection_id: records::display_connection_id(&record.id),
                    password_saved: false,
                    app_notified: false,
                    warning,
                }
            }
        };
        changed.app_notified = self.notify();
        Ok(changed)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn connector(command: Option<&str>) -> Connector {
        let dir = std::env::temp_dir();
        let connector = Connector::new(std::sync::Arc::new(engine_driver_host::InstalledDriverStore::new(&dir)));
        match command {
            Some(command) => connector.with_keychain_command(command.into()),
            None => connector,
        }
    }

    #[test]
    fn a_keychain_that_would_ask_the_user_is_told_with_the_fix_and_other_errors_stay_unavailable() {
        let command = "'/Users/a b/tablex-mcp' --authorize-keychain";
        let blocked = credentials_error(
            &connector(Some(command)),
            "the password could not be saved",
            CredentialError::InteractionNotAllowed,
        );
        assert_eq!(blocked.code(), "credentials.keychain_access_needed");
        let text = blocked.to_string();
        assert!(text.contains("Allow Keychain access") && text.contains(command), "{text}");
        assert!(!text.contains("could not be saved"), "{text}");
        let other = credentials_error(
            &connector(None),
            "the password could not be saved",
            CredentialError::Platform("locked".into()),
        );
        assert_eq!(other.code(), "credentials.unavailable");
    }

    #[test]
    fn a_removed_connection_whose_old_item_needs_the_user_says_it_stays_not_that_it_cannot_be_read() {
        let text = removal_warning(&CredentialError::InteractionNotAllowed);
        assert!(text.contains("stays") && text.contains("Keychain Access") && !text.contains("cannot read"), "{text}");
        assert!(!text.contains("cannot read"), "{text}");
        let other = removal_warning(&CredentialError::Platform("keychain locked".into()));
        assert!(other.ends_with("could not be cleared: keychain locked"), "{other}");
    }

    #[test]
    fn removing_a_connection_is_click_only_in_the_app_and_adding_is_not() {
        let new = NewConnection {
            name: "shop".into(),
            kind: ConnectionKind::Sqlite,
            uri: None,
            host: None,
            port: None,
            database: None,
            user: None,
            path: Some(std::env::temp_dir().join("shop.db").to_string_lossy().into_owned()),
            tls: None,
            connection_type: "development".into(),
        };
        let record = new_connection::build_record(&new, "id-1", 0.0).unwrap();
        let remove = Plan::Remove { record: record.clone(), type_name: "Development".into(), production: false };
        assert!(ConnectionStore::app_question(&remove).click_only);
        let add = Plan::Add { record, password: None, type_name: "Development".into(), production: false };
        assert!(!ConnectionStore::app_question(&add).click_only);
    }
}
