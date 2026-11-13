//! [`Connector`]: a saved connection (or a form that is not saved yet) to a live driver session and
//! the SSH tunnel it goes through. The GPUI app and `tablex-mcp` open connections only through it.
//!
//! The rules that used to be conventions of the app are code here:
//!
//! * **Fail closed.** A connection flagged as tunnelled is never connected without a live tunnel:
//!   tunnel settings that cannot be read, a record that has vanished or a switch that disagrees with
//!   the saved flag are errors, never "no tunnel" ([`TunnelInput::resolve`]), and [`guard_direct`] is
//!   the single check before a driver URI is used.
//! * **The rewritten URI is rebuilt on every open** from the saved URI and the new local port
//!   (`tunnel::ssh::open_for_session`); no rewritten URI is ever stored or reused.
//! * **One tunnel per driver session.** A session owns its [`LiveTunnel`]; only a bastion that asked
//!   a question (a one-time code) is shared with the connection's other sessions, so they do not ask
//!   again.
//! * **No trust without an answer.** A host key is recorded only after
//!   [`TunnelPrompts::trust_host`] said [`HostKeyAnswer::Trust`] for that exact key; a changed or
//!   revoked key is never offered.
//! * **Secrets stay inside.** They are read from the credential store, held as zeroizing
//!   [`Secret`]s and handed to `engine-net` and to the driver's connect request; `Debug` and error
//!   texts never contain them.
//!
//! Every interaction is a callback ([`TunnelPrompts`]). A front end that cannot ask (`tablex-mcp`)
//! passes [`RefusePrompts`]; the app answers "not now" and shows its own dialogs from the typed
//! [`ConnectFailure`] it gets back.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

use engine_agent::{
    AgentError, ConnectionCatalog, ConnectionInfo, DriverSessionAdapter, OpenGeneration,
    SecretText, Session, SessionProvider,
};
use engine_driver_host::{DriverHandle, DriverSession, InstalledDriverStore, LoaderError};
use engine_net::{KeyboardInteractiveRequest, Prompts, Secret};
use engine_storage::connection::SavedConnectionRecord;

use crate::credentials::{CredentialError, CredentialStore, SecretKind, SystemStore};
use crate::drivers::{load_driver, DriverLoadError};
use crate::profile::{self, ConnectionProfile, ProfileSettings};
use crate::records::{
    display_connection_id, resolve_runtime_uri_from, resolve_runtime_uri_with,
    storage_connection_id, with_default_connect_timeout, ConnectionKind, RuntimeUri,
    SavedConnection,
};
use crate::tunnel::ssh::{
    self, ConnectFailure, EngineOpener, HostKeyRequest, SecretRequest, SessionTunnel,
    SshSecrets, SshSettings, TunnelAccess, TunnelOpener,
};

/// Rounds of "ask, then try again" for one tunnel (host key, password, passphrase): enough for a
/// first connection that needs a host key and a secret, small enough that a misbehaving bastion
/// cannot keep the user answering.
const MAX_PROMPT_ROUNDS: usize = 4;
/// Every further hop of the route (a jump host) may ask for its host key and its secret too.
const PROMPT_ROUNDS_PER_HOP: usize = 3;

// ---------------------------------------------------------------------------------------------
// Callbacks
// ---------------------------------------------------------------------------------------------

/// The answer to "do you trust this SSH host key?".
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum HostKeyAnswer {
    /// Record the key in the request and connect again, **if** `sha256_fingerprint` is the one the
    /// connector asked about; an answer for another key counts as a refusal. Take it from the
    /// request ([`HostKeyAnswer::trust`]).
    Trust { sha256_fingerprint: String },
    /// Do not connect. The caller gets [`ConnectFailure::HostKeyUnknown`] (the app then parks the
    /// attempt and shows its dialog; a headless front end reports it).
    Refuse,
}

impl HostKeyAnswer {
    /// "Yes, trust the key of this request."
    pub fn trust(request: &HostKeyRequest) -> Self {
        Self::Trust {
            sha256_fingerprint: request.fingerprint.clone(),
        }
    }
}

/// Something worth telling the user that is not a question.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TunnelNotice {
    /// The key the user confirmed was recorded.
    HostKeyTrusted {
        host: String,
        port: u16,
        fingerprint: String,
    },
    /// A tunnel is up (`user@host:port`).
    TunnelOpened { via: String },
}

/// Every question a connect can ask. Called on the thread that connects (never the UI thread):
/// they may block until a person answers.
pub trait TunnelPrompts: Send + Sync {
    /// The bastion presented a key nobody recorded. See [`HostKeyAnswer`].
    fn trust_host(&self, request: &HostKeyRequest) -> HostKeyAnswer;

    /// The tunnel needs a password or key passphrase that is neither typed nor saved. `None`: the
    /// caller gets [`ConnectFailure::SecretNeeded`].
    fn secret(&self, request: &SecretRequest) -> Option<Secret>;

    /// A keyboard-interactive round of the bastion for `user`. `None` cancels authentication.
    fn keyboard_interactive(
        &self,
        user: &str,
        request: &KeyboardInteractiveRequest,
    ) -> Option<Vec<Secret>>;

    fn notice(&self, _notice: &TunnelNotice) {}
}

/// Answers nothing: every question is a refusal. What a front end without a window passes.
#[derive(Debug, Default, Clone, Copy)]
pub struct RefusePrompts;

impl TunnelPrompts for RefusePrompts {
    fn trust_host(&self, _request: &HostKeyRequest) -> HostKeyAnswer {
        HostKeyAnswer::Refuse
    }

    fn secret(&self, _request: &SecretRequest) -> Option<Secret> {
        None
    }

    fn keyboard_interactive(
        &self,
        _user: &str,
        _request: &KeyboardInteractiveRequest,
    ) -> Option<Vec<Secret>> {
        None
    }
}

/// `engine-net`'s prompt seam over [`TunnelPrompts`]: remembers that the bastion asked a question.
struct PromptBridge {
    prompts: Arc<dyn TunnelPrompts>,
    settings: SshSettings,
    asked: Arc<AtomicBool>,
}

impl Prompts for PromptBridge {
    fn keyboard_interactive(&self, request: &KeyboardInteractiveRequest) -> Option<Vec<Secret>> {
        self.asked.store(true, Ordering::Release);
        // The login of the hop that asks (a jump host has its own).
        let user = self.settings.user_at(&request.host, request.port);
        self.prompts.keyboard_interactive(&user, request)
    }
}

// ---------------------------------------------------------------------------------------------
// Inputs and results
// ---------------------------------------------------------------------------------------------

/// The distinct spellings under which a connection id may be stored, in the order they are
/// tried: the upper-case UUID `connections.db` keeps (the Swift app's), then the lower-case one
/// the display id and older secrets use. An id that is not a UUID has one spelling. However the
/// caller spells the id, the same accounts are probed in the same order.
pub fn accounts_of(connection: &str) -> Vec<String> {
    let mut accounts = vec![
        storage_connection_id(connection),
        display_connection_id(connection),
    ];
    accounts.dedup();
    accounts
}

/// How a connect finds out whether and how it goes through a tunnel. Built only by [`OpenRequest`]
/// from the record or the form being opened, so the requirement (`expected`) can never be supplied
/// by a caller.
#[derive(Debug, Clone)]
pub(crate) struct TunnelInput {
    /// Settings that win over the stored record (a form being tested, UI scenarios).
    pub(crate) stored: Option<SshSettings>,
    /// The saved connection says it goes through a tunnel (or a form with the tab on).
    pub(crate) expected: bool,
    /// Secrets typed for this connection, memory only; the credential store fills in the rest.
    pub(crate) secrets: SshSecrets,
    /// Where the saved record and the saved secrets are looked up (see [`accounts_of`]); empty
    /// for a connection that was never saved.
    pub(crate) accounts: Vec<String>,
    /// `secrets` already holds what the credential store has for this tunnel (see
    /// [`SavedSecrets`]), so the store is not asked again.
    pub(crate) preread: bool,
}

impl TunnelInput {
    /// The tunnel of a saved connection: its settings are read from the record.
    fn saved(connection: &str, expected: bool, secrets: SshSecrets) -> Self {
        Self {
            stored: None,
            expected,
            secrets,
            accounts: accounts_of(connection),
            preread: false,
        }
    }

    /// The tunnel tab of the dialog's form: what is typed wins, and a tab that is on is a tunnel
    /// that is required.
    fn form(settings: Option<SshSettings>, secrets: SshSecrets, accounts: Vec<String>) -> Self {
        let settings = settings.unwrap_or_default();
        Self {
            expected: settings.routed(),
            stored: Some(settings),
            secrets,
            accounts,
            preread: false,
        }
    }

    /// A direct connection with nothing expected (tests of the guard).
    #[cfg(test)]
    pub(crate) fn direct() -> Self {
        Self::form(None, SshSecrets::default(), Vec::new())
    }
}

/// Where the URI of an open comes from.
#[derive(Debug, Clone)]
pub(crate) enum UriSource {
    /// Exactly this URI (a form's runtime URI, an override).
    Given(RuntimeUri),
    /// The saved connection's own: its persisted URI plus the saved password, read from the
    /// credential store.
    Saved(SavedConnection),
}

/// Everything one open needs, derived from the record or form being opened: the tunnel
/// requirement is computed here, never passed in.
///
/// A caller cannot build a request that says "no tunnel" for a tunnelled record: the tunnel input
/// type is not exported and the fields are private.
///
/// ```compile_fail,E0603
/// use engine_connect::connect::TunnelInput; // not exported
/// ```
///
/// ```compile_fail,E0616
/// fn peek(request: engine_connect::connect::OpenRequest) {
///     let _ = request.tunnel; // private
/// }
/// ```
#[derive(Debug, Clone)]
pub struct OpenRequest {
    pub(crate) kind: ConnectionKind,
    pub(crate) uri: UriSource,
    /// The global connect timeout; `0` leaves the URI alone.
    pub(crate) connect_timeout_secs: u32,
    pub(crate) tunnel: TunnelInput,
}

impl OpenRequest {
    /// A saved connection: its URI (or `uri_override`, a URI the user just typed for it), its saved
    /// password, its tunnel as the record says. If the record says tunnelled, nothing else can turn
    /// the tunnel off.
    pub fn saved(
        saved: SavedConnection,
        uri_override: Option<RuntimeUri>,
        connect_timeout_secs: u32,
        secrets: SshSecrets,
    ) -> Self {
        let tunnel = TunnelInput::saved(&saved.id.to_string(), saved.ssh_tunnel, secrets);
        Self {
            kind: saved.kind,
            uri: match uri_override {
                Some(uri) => UriSource::Given(uri),
                None => UriSource::Saved(saved),
            },
            connect_timeout_secs,
            tunnel,
        }
    }

    /// Tunnel settings that win over the stored ones (UI scenarios and tests have no storage). A
    /// record saved as tunnelled still requires a tunnel.
    #[must_use]
    pub fn with_stored_ssh(mut self, settings: Option<SshSettings>) -> Self {
        self.tunnel.stored = settings;
        self
    }

    /// A connection the dialog is testing, maybe not saved yet: the URI as typed and the tunnel tab as
    /// typed (`None` or switched off: direct; switched on: a tunnel is required).
    pub fn form(
        kind: ConnectionKind,
        uri: RuntimeUri,
        ssh: Option<SshSettings>,
        secrets: SshSecrets,
        accounts: Vec<String>,
    ) -> Self {
        Self {
            kind,
            uri: UriSource::Given(uri),
            connect_timeout_secs: 0,
            tunnel: TunnelInput::form(ssh, secrets, accounts),
        }
    }
}

/// The tunnel of one driver session and what opened it (the access is what the connection's other
/// sessions open their own tunnel with).
pub struct LiveTunnel {
    pub tunnel: SessionTunnel,
    pub access: TunnelAccess,
}

/// The saved secrets one open of a saved connection uses, read from the credential store once
/// ([`Connector::read_saved_secrets`]) and handed to [`Connector::open_with_secrets`]. `Debug`
/// prints nothing of them.
#[derive(Debug, Clone, Default)]
pub struct SavedSecrets {
    database: Option<Secret>,
    ssh: SshSecrets,
}

impl SavedSecrets {
    /// The exact texts to scrub from error messages and audit lines: the database password in the
    /// forms its runtime URI carries, then every SSH password and key passphrase and the proxy
    /// password.
    pub fn mask_texts(&self) -> Vec<SecretText> {
        let database = self.database.iter().flat_map(|secret| {
            let text = secret.expose();
            let forms = if text.is_empty() {
                Vec::new()
            } else {
                crate::records::encoded_password_forms(text)
            };
            forms
                .into_iter()
                .chain((!text.is_empty()).then(|| text.to_owned()))
        });
        let ssh = self
            .ssh
            .iter()
            .map(|secret| secret.expose().to_owned());
        database.chain(ssh).map(SecretText::new).collect()
    }
}

/// A driver session that is open, with the SSH tunnel it connected through.
///
/// Only [`Connector::open`], [`Connector::open_request`] and [`Connector::open_session`] produce
/// one, and each derives the tunnel requirement from the record or form it is given: a connection
/// flagged as tunnelled only comes back with `tunnel: Some(..)`. The fields are private; take the
/// parts with [`Connection::into_parts`]. The session is declared before the tunnel, so dropping a
/// `Connection` closes the session first, then the tunnel.
///
/// ```compile_fail,E0616
/// fn handle_of(connection: engine_connect::connect::Connection) {
///     let _ = connection.handle; // private: a caller cannot reach the driver handle from here
/// }
/// ```
pub struct Connection {
    session: DriverSession,
    tunnel: Option<LiveTunnel>,
    handle: Arc<DriverHandle>,
    connection_id: u64,
    /// The URI as the user wrote it (the tunnel rewrote a copy for the driver).
    uri: RuntimeUri,
}

/// What an open [`Connection`] is made of, for the front end that keeps them in its own maps.
pub struct OpenedConnection {
    pub session: DriverSession,
    pub tunnel: Option<LiveTunnel>,
    /// The driver the session belongs to: for requests on that session. A new session of a
    /// connection is [`Connector::open_session`], which applies the connection's tunnel rule.
    pub handle: Arc<DriverHandle>,
    pub connection_id: u64,
    /// The session's URI, unrewritten, to open its siblings with.
    pub uri: RuntimeUri,
}

impl Connection {
    pub fn connection_id(&self) -> u64 {
        self.connection_id
    }

    pub fn into_parts(self) -> OpenedConnection {
        OpenedConnection {
            session: self.session,
            tunnel: self.tunnel,
            handle: self.handle,
            connection_id: self.connection_id,
            uri: self.uri,
        }
    }

    /// Disconnects the session, then drops the tunnel (it carried the goodbye).
    pub fn close(mut self) -> Result<(), LoaderError> {
        let result = self.session.disconnect();
        drop(self.tunnel.take());
        result
    }
}

impl std::fmt::Debug for Connection {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Connection")
            .field("connection_id", &self.connection_id)
            .field(
                "tunnel",
                &self.tunnel.as_ref().map(|live| live.tunnel.via()),
            )
            .finish()
    }
}

// ---------------------------------------------------------------------------------------------
// Tunnel resolution (fail closed)
// ---------------------------------------------------------------------------------------------

/// The stored tunnel settings of a connection. `Ok(None)`: there is no record under any spelling of
/// the id. A store error or unreadable settings are errors, not "no tunnel".
fn read_stored_settings(accounts: &[String]) -> Result<Option<SshSettings>, ssh::SettingsError> {
    for id in accounts {
        let record = engine_storage::connection::get_connection(id)
            .map_err(|error| ssh::SettingsError(format!("the connection store failed: {error}")))?;
        if let Some(record) = record {
            return SshSettings::parse(&record.network_settings_json).map(Some);
        }
    }
    Ok(None)
}

/// The single check before a driver URI is used: a connection that is flagged as tunnelled never
/// connects without a live tunnel.
pub fn guard_direct(expected: bool, live: &Option<LiveTunnel>) -> Result<(), ConnectFailure> {
    if expected && live.is_none() {
        return Err(ConnectFailure::Message(
            "Refusing to connect directly: this connection is configured to go through an SSH tunnel or a proxy"
                .into(),
        ));
    }
    Ok(())
}

/// A driver that refused to connect through a tunnel: when the tunnel saw why (the bastion could
/// not reach the database port), its code and reason come with the driver's own text.
pub fn driver_connect_failure(error: String, live: Option<&LiveTunnel>) -> ConnectFailure {
    let tunnel_error = live.and_then(|live| {
        live.tunnel
            .targets
            .iter()
            .find_map(|target| live.tunnel.handle.last_forward_error(target))
    });
    match tunnel_error {
        Some(tunnel_error) => ConnectFailure::Tunnel {
            code: tunnel_error.code(),
            message: format!("{error} (SSH tunnel: {tunnel_error})"),
        },
        None => ConnectFailure::Message(error),
    }
}

// ---------------------------------------------------------------------------------------------
// The connector
// ---------------------------------------------------------------------------------------------

/// Opens saved connections. Cheap to clone (everything inside is shared).
#[derive(Clone)]
pub struct Connector {
    store: Arc<InstalledDriverStore>,
    credentials: Arc<dyn CredentialStore + Send + Sync>,
    opener: Arc<dyn TunnelOpener>,
    /// `None` when the app data directory is unknown: tunnels then fail with a typed error.
    known_hosts: Option<PathBuf>,
    /// `None` when the platform has no config directory: the profile settings are the defaults.
    settings_path: Option<PathBuf>,
}

impl Connector {
    /// The real thing: the OS credential store, `engine-net` tunnels, the app-owned `known_hosts`
    /// and the app's `settings.json`.
    pub fn new(store: Arc<InstalledDriverStore>) -> Self {
        Self {
            store,
            credentials: Arc::new(SystemStore::new()),
            opener: Arc::new(EngineOpener),
            known_hosts: crate::platform::ssh_known_hosts_path(),
            settings_path: profile::default_settings_path(),
        }
    }

    #[must_use]
    pub fn with_tunnels(
        mut self,
        opener: Arc<dyn TunnelOpener>,
        known_hosts: Option<PathBuf>,
    ) -> Self {
        self.opener = opener;
        self.known_hosts = known_hosts;
        self
    }

    #[must_use]
    pub fn with_credentials(mut self, credentials: Arc<dyn CredentialStore + Send + Sync>) -> Self {
        self.credentials = credentials;
        self
    }

    #[must_use]
    pub fn with_settings_path(mut self, path: Option<PathBuf>) -> Self {
        self.settings_path = path;
        self
    }

    /// The `settings.json` this connector reads, `None` when the platform has no config directory.
    /// `tablex-mcp` reads the app's MCP switch from the same file.
    pub fn settings_path(&self) -> Option<&Path> {
        self.settings_path.as_deref()
    }

    pub fn driver_store(&self) -> &Arc<InstalledDriverStore> {
        &self.store
    }

    pub fn opener(&self) -> &Arc<dyn TunnelOpener> {
        &self.opener
    }

    pub fn known_hosts(&self) -> Option<&Path> {
        self.known_hosts.as_deref()
    }

    /// The connection-scoped settings, read from `settings.json` now. With no settings path (the
    /// platform has no config directory) the built-in types are used: a connection whose type id is
    /// a custom one is then unknown, which the agent treats restrictively (read-only, no preset).
    pub fn profile_settings(&self) -> Result<ProfileSettings, profile::ProfileError> {
        match &self.settings_path {
            Some(path) => ProfileSettings::load(path),
            None => Ok(ProfileSettings::default()),
        }
    }

    // --- the entry points ----------------------------------------------------------------------

    /// Opens the saved connection `connection_id` exactly as the app's connect does: its URI with
    /// the saved password, the global connect timeout, the installed driver, the connection's SSH
    /// tunnel (when it has one) and a fresh driver session. Blocks.
    pub fn open(
        &self,
        connection_id: &str,
        prompts: &Arc<dyn TunnelPrompts>,
    ) -> Result<Connection, ConnectFailure> {
        let record = Self::record(connection_id)?;
        self.open_saved(&record, prompts, None)
    }

    /// [`Self::open`] for a record the caller read already ([`Self::record`] is not read again)
    /// with the saved secrets it read already ([`Self::read_saved_secrets`]): the credential
    /// store is not asked again, so the secrets a caller masked are the ones used.
    pub fn open_with_secrets(
        &self,
        record: &SavedConnectionRecord,
        prompts: &Arc<dyn TunnelPrompts>,
        secrets: SavedSecrets,
    ) -> Result<Connection, ConnectFailure> {
        self.open_saved(record, prompts, Some(secrets))
    }

    fn open_saved(
        &self,
        record: &SavedConnectionRecord,
        prompts: &Arc<dyn TunnelPrompts>,
        preread: Option<SavedSecrets>,
    ) -> Result<Connection, ConnectFailure> {
        let saved = SavedConnection::from_record(record);
        if saved.disabled {
            return Err(ConnectFailure::Message(
                "This connection's database type is not supported".into(),
            ));
        }
        let connect_timeout_secs = self
            .profile_settings()
            .map(|settings| {
                settings
                    .effective(&ConnectionProfile::from_record(record))
                    .connect_timeout_secs
            })
            .unwrap_or(0);
        let Some(preread) = preread else {
            return self.open_request(
                OpenRequest::saved(saved, None, connect_timeout_secs, SshSecrets::default()),
                prompts,
            );
        };
        let password = saved
            .save_password
            .then(|| preread.database.as_ref().map(Secret::expose))
            .flatten();
        let uri = RuntimeUri::new(resolve_runtime_uri_from(&saved, password)?);
        let mut request = OpenRequest::saved(saved, Some(uri), connect_timeout_secs, preread.ssh);
        request.tunnel.preread = true;
        self.open_request(request, prompts)
    }

    /// The saved secrets that opening `record` uses, and no others: the database password of a
    /// connection that saves it, the SSH passwords or key passphrases (bastion and jump hosts) of
    /// an enabled tunnel that saves them, the password of an enabled proxy that saves it. A connection that saves nothing reads
    /// nothing (a locked store does not matter to it). Nothing saved is `None`, as is a platform without a store; a store that cannot be
    /// read is an error.
    pub fn read_saved_secrets(
        &self,
        record: &SavedConnectionRecord,
    ) -> Result<SavedSecrets, CredentialError> {
        let accounts = accounts_of(&record.id);
        let settings = SshSettings::parse(&record.network_settings_json).ok();
        let mut ssh = SshSecrets::default();
        for (hop, need) in settings.iter().flat_map(SshSettings::saved_slots) {
            let hop_accounts: Vec<String> =
                accounts.iter().map(|account| hop.account(account)).collect();
            if let Some(secret) = self
                .load_saved(true, hop.kind(need), &hop_accounts)?
                .filter(|secret| !secret.is_empty())
            {
                ssh.set(hop, need, secret);
            }
        }
        Ok(SavedSecrets {
            database: self.load_saved(record.save_password, SecretKind::Database, &accounts)?,
            ssh,
        })
    }

    /// The first spelling of the id the store has `kind` under; asks nothing when not `wanted`.
    fn load_saved(
        &self,
        wanted: bool,
        kind: SecretKind,
        accounts: &[String],
    ) -> Result<Option<Secret>, CredentialError> {
        if !wanted {
            return Ok(None);
        }
        for account in accounts {
            match self.credentials.load(kind, account) {
                Ok(Some(text)) => return Ok(Some(Secret::new(text))),
                Ok(None) | Err(CredentialError::NoKeyring) => {}
                Err(error) => return Err(error),
            }
        }
        Ok(None)
    }

    /// [`Self::open`] for inputs the caller already has (the app's tabs, the dialog's Test button).
    /// The tunnel requirement comes from the record or form inside the request. Order: URI, driver,
    /// tunnel, the direct-connection guard, driver connect.
    pub fn open_request(
        &self,
        request: OpenRequest,
        prompts: &Arc<dyn TunnelPrompts>,
    ) -> Result<Connection, ConnectFailure> {
        let OpenRequest {
            kind,
            uri,
            connect_timeout_secs,
            tunnel,
        } = request;
        // However the URI was given, a record saved as tunnelled requires a tunnel.
        let expected =
            tunnel.expected || matches!(&uri, UriSource::Saved(saved) if saved.ssh_tunnel);
        let uri = match uri {
            UriSource::Given(uri) => uri,
            UriSource::Saved(saved) => {
                RuntimeUri::new(resolve_runtime_uri_with(&saved, &*self.credentials)?)
            }
        };
        let uri = RuntimeUri::new(with_default_connect_timeout(
            uri.expose().to_owned(),
            kind,
            connect_timeout_secs,
        ));
        let handle = load_driver(&self.store, kind.driver_id(), uri.expose())?;
        let access = self.resolve_tunnel(TunnelInput { expected, ..tunnel }, prompts)?;
        let (session, live) =
            self.connect_session(&handle, kind, uri.expose(), access, expected, prompts)?;
        let connection_id = session.connection_id();
        Ok(Connection {
            session,
            tunnel: live,
            handle,
            connection_id,
            uri,
        })
    }

    /// Another session of a connection that is already open (the diagram's apply session): its own
    /// tunnel from `access` (the main session's, see [`LiveTunnel::access`]), never the main
    /// session's. `saved` is the connection being opened: if it says tunnelled and `access` is
    /// `None` the open is refused.
    pub fn open_session(
        &self,
        handle: &Arc<DriverHandle>,
        saved: &SavedConnection,
        uri: &RuntimeUri,
        connect_timeout_secs: u32,
        access: Option<TunnelAccess>,
        prompts: &Arc<dyn TunnelPrompts>,
    ) -> Result<(DriverSession, Option<LiveTunnel>), ConnectFailure> {
        let uri =
            with_default_connect_timeout(uri.expose().to_owned(), saved.kind, connect_timeout_secs);
        let uri = RuntimeUri::new(uri);
        self.connect_session(
            handle,
            saved.kind,
            uri.expose(),
            access,
            saved.ssh_tunnel,
            prompts,
        )
    }

    /// "Test tunnel": opens and probes every target of the form's tunnel, then drops it.
    pub fn test_tunnel(
        &self,
        kind: ConnectionKind,
        uri: &RuntimeUri,
        ssh: SshSettings,
        secrets: SshSecrets,
        accounts: Vec<String>,
        prompts: &Arc<dyn TunnelPrompts>,
    ) -> Result<ssh::TestedTunnel, ConnectFailure> {
        let input = TunnelInput::form(Some(ssh), secrets, accounts);
        let Some(mut access) = self.resolve_tunnel(input, prompts)? else {
            return Err(ConnectFailure::Message(
                "Turn the SSH tunnel or the proxy on first".into(),
            ));
        };
        self.with_answers(&mut access, prompts, |access| {
            ssh::test_tunnel(access, kind, uri.expose())
        })
    }

    // --- internals -----------------------------------------------------------------------------

    fn record(connection_id: &str) -> Result<SavedConnectionRecord, ConnectFailure> {
        for id in accounts_of(connection_id) {
            let found = engine_storage::connection::get_connection(&id).map_err(|error| {
                ConnectFailure::Message(format!("the connection store failed: {error}"))
            })?;
            if let Some(record) = found {
                return Ok(record);
            }
        }
        Err(ConnectFailure::Message(
            "the saved connection was not found".into(),
        ))
    }

    /// `Ok(None)`: no tunnel. It **fails closed**: settings that cannot be read, a record that has
    /// vanished or a switch that disagrees with the saved flag never read as "no tunnel".
    pub(crate) fn resolve_tunnel(
        &self,
        input: TunnelInput,
        prompts: &Arc<dyn TunnelPrompts>,
    ) -> Result<Option<TunnelAccess>, ConnectFailure> {
        let settings = match input.stored {
            Some(settings) => settings,
            None => match read_stored_settings(&input.accounts) {
                Ok(Some(settings)) => settings,
                Ok(None) if input.expected => {
                    return Err(ConnectFailure::Message(
                        ssh::SettingsError("the saved connection was not found".into()).to_string(),
                    ))
                }
                Ok(None) => return Ok(None),
                Err(error) => return Err(ConnectFailure::Message(error.to_string())),
            },
        };
        if !settings.routed() {
            return if input.expected {
                Err(ConnectFailure::Message(
                    ssh::SettingsError(
                        "the connection is saved as tunnelled but its settings say the tunnel is off; reopen the connection".into(),
                    )
                    .to_string(),
                ))
            } else {
                Ok(None)
            };
        }
        let saved = if input.preread {
            SshSecrets::default()
        } else {
            SshSecrets::load(&*self.credentials, &input.accounts, &settings)
        };
        Ok(Some(self.tunnel_access(
            settings,
            input.secrets.or(saved),
            prompts,
        )))
    }

    /// The access a session opens its tunnel with (see [`LiveTunnel::access`]); the connect
    /// functions build theirs from the settings they resolve.
    pub fn tunnel_access(
        &self,
        settings: SshSettings,
        secrets: SshSecrets,
        prompts: &Arc<dyn TunnelPrompts>,
    ) -> TunnelAccess {
        let asked = Arc::new(AtomicBool::new(false));
        TunnelAccess {
            opener: Arc::clone(&self.opener),
            known_hosts: self.known_hosts.clone(),
            // The dialog's fixed first line names the login this connection uses.
            prompts: Arc::new(PromptBridge {
                prompts: Arc::clone(prompts),
                settings: settings.clone(),
                asked: Arc::clone(&asked),
            }),
            asked,
            settings,
            secrets,
            shared: None,
        }
    }

    /// The tunnel (when there is one), the guard and the driver connect of one session.
    fn connect_session(
        &self,
        handle: &Arc<DriverHandle>,
        kind: ConnectionKind,
        uri: &str,
        access: Option<TunnelAccess>,
        expected: bool,
        prompts: &Arc<dyn TunnelPrompts>,
    ) -> Result<(DriverSession, Option<LiveTunnel>), ConnectFailure> {
        let (connect_uri, live) = match access {
            Some(mut access) => {
                let (rewritten, tunnel) = self.with_answers(&mut access, prompts, |access| {
                    ssh::open_for_session(access, kind, uri)
                })?;
                prompts.notice(&TunnelNotice::TunnelOpened { via: tunnel.via() });
                (rewritten, Some(LiveTunnel { tunnel, access }))
            }
            None => (uri.to_owned(), None),
        };
        // The one place the driver URI is produced: a connection that is configured for a tunnel
        // never reaches the driver without one.
        guard_direct(expected, &live)?;
        let request_json = serde_json::json!({ "uri": connect_uri }).to_string();
        let session = handle
            .connect(&request_json)
            .map_err(|error| driver_connect_failure(error.to_string(), live.as_ref()))?;
        Ok((session, live))
    }

    /// Runs `attempt`; a question the user can answer is asked through `prompts` and the attempt is
    /// repeated, at most [`MAX_PROMPT_ROUNDS`] times. A host key is recorded only for the key the
    /// callback trusted.
    fn with_answers<T>(
        &self,
        access: &mut TunnelAccess,
        prompts: &Arc<dyn TunnelPrompts>,
        mut attempt: impl FnMut(&TunnelAccess) -> Result<T, ConnectFailure>,
    ) -> Result<T, ConnectFailure> {
        let rounds = MAX_PROMPT_ROUNDS + PROMPT_ROUNDS_PER_HOP * access.settings.jumps.len();
        for _ in 1..rounds {
            match attempt(access) {
                Err(ConnectFailure::HostKeyUnknown(request)) => {
                    // Only an explicit answer for this exact key records it.
                    match prompts.trust_host(&request) {
                        HostKeyAnswer::Trust { sha256_fingerprint }
                            if sha256_fingerprint == request.fingerprint => {}
                        _ => return Err(ConnectFailure::HostKeyUnknown(request)),
                    }
                    let known_hosts = ssh::known_hosts_of(access)?;
                    // Exactly the host and port the user saw: a jump host is not the bastion.
                    access
                        .opener
                        .trust_host_key(
                            &ssh::trust_spec(&request, known_hosts),
                            &request.host,
                            request.port,
                            &request.public_key,
                        )
                        .map_err(|error| ssh::classify(error, &access.settings))?;
                    prompts.notice(&TunnelNotice::HostKeyTrusted {
                        host: request.host,
                        port: request.port,
                        fingerprint: request.fingerprint,
                    });
                }
                Err(ConnectFailure::SecretNeeded(request)) => {
                    let Some(secret) = prompts.secret(&request) else {
                        return Err(ConnectFailure::SecretNeeded(request));
                    };
                    // Only the hop that asked gets the answer.
                    access.secrets.set(request.hop, request.need, secret);
                }
                other => return other,
            }
        }
        attempt(access)
    }
}

impl std::fmt::Debug for Connector {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Connector")
            .field("known_hosts", &self.known_hosts)
            .finish_non_exhaustive()
    }
}

// ---------------------------------------------------------------------------------------------
// The agent's view
// ---------------------------------------------------------------------------------------------

/// `engine-agent`'s [`ConnectionCatalog`] and [`SessionProvider`] over a [`Connector`].
///
/// **The agent always gets its own driver session** (never one of the user's tabs), opened through
/// [`Connector::open`] with the prompts the caller supplies; its tunnel lives here, under the
/// connection id and the open's [`OpenGeneration`], until [`SessionProvider::close_generation`]
/// for that generation (a close names its own open, so it never drops the tunnel of a later open
/// of the same connection). The catalog is read on every call, so a changed setting applies at
/// once, and **fails closed**: when `settings.json` cannot be read every connection is `off`.
pub struct AgentConnections {
    connector: Connector,
    prompts: Arc<dyn TunnelPrompts>,
    tunnels: Mutex<HashMap<(String, OpenGeneration), LiveTunnel>>,
    /// The highest generation an open has started with: what [`Self::revoke`] reaches.
    newest_open: AtomicU64,
    /// What [`SessionProvider::secrets`] read for a connection, until `open` takes it: the record
    /// and the saved secrets are read once per open, and an error survives until `open` can fail
    /// on it.
    prefetched: Mutex<HashMap<String, Result<Prefetched, ReadFailure>>>,
    /// What to do about a credential store that cannot be read, appended to the error the model
    /// sees (the standalone MCP server names its one-time authorisation command here).
    credential_hint: Option<String>,
}

/// The record and the saved secrets of one open: the texts to mask and the values the open
/// connects with (the same read, so the masked secret is the used one).
struct Prefetched {
    record: SavedConnectionRecord,
    masks: Vec<SecretText>,
    saved: SavedSecrets,
}

/// Why [`AgentConnections::mask_secrets`] has no secrets to hand over. The text never holds one.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MaskSecretsError(String);

impl std::fmt::Display for MaskSecretsError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

impl std::error::Error for MaskSecretsError {}

/// Why [`AgentConnections::read_secrets`] has nothing to hand to the open.
enum ReadFailure {
    /// The record is gone or the connection store failed: reported as `open` would.
    Record(ConnectFailure),
    /// The credential store could not be read.
    Credentials(CredentialError),
}

impl AgentConnections {
    pub fn new(connector: Connector, prompts: Arc<dyn TunnelPrompts>) -> Self {
        Self {
            connector,
            prompts,
            tunnels: Mutex::new(HashMap::new()),
            newest_open: AtomicU64::new(0),
            prefetched: Mutex::new(HashMap::new()),
            credential_hint: None,
        }
    }

    /// Appends `hint` to the `credentials.unavailable` error of an open (what the user can do
    /// about a store that cannot be read).
    pub fn with_credential_hint(mut self, hint: String) -> Self {
        self.credential_hint = Some(hint);
        self
    }

    /// The error of an open whose credential store could not be read.
    fn credentials_unavailable(&self, error: &CredentialError) -> AgentError {
        let mut text = format!(
            "the credential store could not be read ({error}), so the session was not opened"
        );
        if let Some(hint) = &self.credential_hint {
            text.push_str(". ");
            text.push_str(hint);
        }
        AgentError::driver("credentials.unavailable", text, false)
    }

    /// Drops the tunnels of the connection's agent sessions **opened before this call**. Front ends
    /// call it (after `ToolLayer::revoke`, which also disconnects the session and ends up in
    /// [`SessionProvider::close_generation`]) when a connection is edited, deleted or its access
    /// changes; with no agent session open it does nothing. Idempotent. An open that starts after
    /// the call keeps its tunnel (the call cannot drop what a later open built); one that was
    /// connecting when it ran is not reached, and the layer closes its session when it finds it
    /// opened under an older revision of the connection.
    pub fn revoke(&self, connection_id: &str) {
        let newest = self.newest_open.load(Ordering::SeqCst);
        // Dropped after the lock is released: closing a tunnel can take a moment.
        drop(self.take_opens_up_to(connection_id, newest));
    }

    /// The tunnels of the connection's opens numbered `newest` or lower.
    fn take_opens_up_to(&self, connection_id: &str, newest: u64) -> Vec<LiveTunnel> {
        let id = display_connection_id(connection_id);
        self.take_tunnels(|(held, generation)| *held == id && generation.get() <= newest)
    }

    /// An open numbered `generation` starts: a revoke from now on reaches its tunnel.
    fn begin_open(&self, generation: OpenGeneration) {
        self.newest_open
            .fetch_max(generation.get(), Ordering::SeqCst);
    }

    /// Keeps the tunnel of one open of the connection (a reopen of an untagged open replaces the
    /// previous one).
    fn hold(&self, connection_id: &str, generation: OpenGeneration, tunnel: Option<LiveTunnel>) {
        let key = (display_connection_id(connection_id), generation);
        let replaced = match tunnel {
            Some(live) => self.tunnels().insert(key, live),
            None => self.tunnels().remove(&key),
        };
        drop(replaced);
    }

    fn take_tunnels(
        &self,
        mut select: impl FnMut(&(String, OpenGeneration)) -> bool,
    ) -> Vec<LiveTunnel> {
        let mut tunnels = self.tunnels();
        let keys: Vec<_> = tunnels.keys().filter(|key| select(key)).cloned().collect();
        keys.iter().filter_map(|key| tunnels.remove(key)).collect()
    }

    /// The names that are not secrets, from the saved URI: its user name and its database (the
    /// path, or `dbname` of a query). A saved password equal to one of them is not masked by value.
    fn names_in_saved_uri(
        connection: &ConnectionInfo,
        record: &SavedConnectionRecord,
    ) -> Vec<String> {
        let Ok(url) = url::Url::parse(&record.uri) else {
            return Vec::new();
        };
        let decode = |text: &str| {
            percent_encoding::percent_decode_str(text)
                .decode_utf8_lossy()
                .into_owned()
        };
        let mut names = vec![decode(url.username())];
        if connection.kind != engine_agent::ConnectionKind::Sqlite {
            names.push(decode(url.path().trim_matches('/')));
            names.extend(
                url.query_pairs()
                    .filter(|(key, _)| key == "dbname" || key == "user")
                    .map(|(_, value)| value.into_owned()),
            );
        }
        names.retain(|name| !name.is_empty());
        names
    }

    /// The saved secrets the connection's record uses (database password with the forms it takes
    /// in the runtime URI, SSH password, key passphrase) and nothing else. A store that cannot be
    /// read is an error, not "no secrets"; a platform without any store has none saved, and a
    /// connection that saves nothing never asks the store.
    fn read_secrets(&self, connection_id: &str) -> Result<Prefetched, ReadFailure> {
        let record = Connector::record(connection_id).map_err(ReadFailure::Record)?;
        let saved = self
            .connector
            .read_saved_secrets(&record)
            .map_err(ReadFailure::Credentials)?;
        Ok(Prefetched {
            masks: saved.mask_texts(),
            record,
            saved,
        })
    }

    /// The exact secrets of the saved connection `connection_id`, to scrub a text of them (an error
    /// a front end is about to show a model, say): the database password in the forms its runtime
    /// URI carries, the SSH password and the key passphrase. Unlike [`SessionProvider::secrets`]
    /// it is **not** the prefetch for the next open: it reads the record and the credential store,
    /// hands back the texts and keeps nothing, so nothing stays in this object. A record or a
    /// store that cannot be read is an error, never "no secrets": the caller must withhold the text.
    pub fn mask_secrets(&self, connection_id: &str) -> Result<Vec<SecretText>, MaskSecretsError> {
        self.read_secrets(connection_id)
            .map(|read| read.masks)
            .map_err(|failure| match failure {
                ReadFailure::Record(_) => {
                    MaskSecretsError("the saved connection could not be read".to_owned())
                }
                ReadFailure::Credentials(error) => {
                    MaskSecretsError(format!("the credential store could not be read ({error})"))
                }
            })
    }

    /// Secrets held for the next open of any connection (tests).
    #[cfg(any(test, feature = "testing"))]
    pub fn held_secrets(&self) -> usize {
        self.prefetched
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .len()
    }

    /// What `secrets` read for this open, or a fresh read when nothing was prefetched.
    fn take_secrets(&self, connection: &ConnectionInfo) -> Result<Prefetched, ReadFailure> {
        let prefetched = self
            .prefetched
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .remove(&connection.id);
        prefetched.unwrap_or_else(|| self.read_secrets(&connection.id))
    }

    fn tunnels(&self) -> std::sync::MutexGuard<'_, HashMap<(String, OpenGeneration), LiveTunnel>> {
        self.tunnels
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }
}

impl ConnectionCatalog for AgentConnections {
    fn list(&self) -> Vec<ConnectionInfo> {
        let Ok(records) = engine_storage::connection::list_connections() else {
            return Vec::new();
        };
        match self.connector.profile_settings() {
            Ok(settings) => records
                .iter()
                .filter_map(|record| settings.agent_info(record))
                .collect(),
            // Without the types there is no way to know which connections are read-only or
            // production: nothing may be used.
            Err(_) => records
                .iter()
                .filter_map(|record| ProfileSettings::default().agent_info(record))
                .map(|info| ConnectionInfo {
                    agent_access: Some(engine_agent::AgentAccess::Off),
                    ..info
                })
                .collect(),
        }
    }

    fn canonical_id(&self, id: &str) -> String {
        display_connection_id(id)
    }
}

impl ConnectFailure {
    /// What the model is told about a connect failure of `connection`. **No address detail**:
    /// `list_connections` promises the model no host, port, login or path, so none reaches it here -
    /// not the bastion's `host:port`, the SSH login or key path, the SQLite file path or the driver
    /// store path, which the interactive messages (and the driver's own error text) carry. The
    /// message names the connection, gives the typed code and says what to do in TableX, where the
    /// full message is shown. The detailed text is dropped: it is not logged anywhere.
    fn agent_error(&self, connection: &ConnectionInfo) -> AgentError {
        let name = &connection.name;
        let (code, text) = match self {
            ConnectFailure::HostKeyUnknown(_) => (
                "tunnel.host_key_unknown",
                format!(
                    "The SSH host key of the tunnel of `{name}` is not trusted yet. Connect to this \
                     database once from TableX and trust the host there, then try again."
                ),
            ),
            ConnectFailure::HostKeyRejected(..) => (
                "tunnel.host_key_mismatch",
                format!(
                    "The SSH host key of the tunnel of `{name}` was refused (it changed or does not \
                     match). Check the connection's SSH settings in TableX."
                ),
            ),
            ConnectFailure::SecretNeeded(_) => (
                "tunnel.secret_needed",
                format!(
                    "The route of `{name}` (SSH tunnel, jump hosts or proxy) needs a password or key \
                     passphrase. Save it in the connection's Tunnel / Proxy settings in TableX."
                ),
            ),
            ConnectFailure::Tunnel { code, .. } => (
                *code,
                format!(
                    "The SSH tunnel of `{name}` could not be opened. Open the connection's SSH \
                     settings in TableX and use Test tunnel."
                ),
            ),
            ConnectFailure::Driver(DriverLoadError::NotInstalled { driver_id }) => (
                "connect.driver",
                format!(
                    "The `{driver_id}` driver needed by `{name}` is not installed. Install it in \
                     TableX (Driver Manager), then try again."
                ),
            ),
            ConnectFailure::Driver(DriverLoadError::Disabled { driver_id }) => (
                "connect.driver",
                format!(
                    "The `{driver_id}` driver needed by `{name}` is disabled. Enable it in TableX \
                     (Driver Manager), then try again."
                ),
            ),
            ConnectFailure::Driver(_) => (
                "connect.driver",
                format!(
                    "The database driver needed by `{name}` could not be loaded. Check it in TableX \
                     (Driver Manager), then try again."
                ),
            ),
            ConnectFailure::Message(_) => (
                "connect.failed",
                format!(
                    "Could not connect to `{name}`. Open the connection in TableX, check its \
                     settings and use Test connection there, then try again."
                ),
            ),
        };
        AgentError::driver(code, text, false)
    }
}

impl SessionProvider for AgentConnections {
    fn open(&self, connection: &ConnectionInfo) -> Result<Arc<dyn Session>, AgentError> {
        self.open_generation(connection, OpenGeneration::UNTAGGED)
    }

    fn open_generation(
        &self,
        connection: &ConnectionInfo,
        generation: OpenGeneration,
    ) -> Result<Arc<dyn Session>, AgentError> {
        self.begin_open(generation);
        // Before anything connects: without the exact secrets a connect error that echoes one could
        // not be masked, so a store that cannot be read fails the open (closed).
        let Prefetched {
            record,
            masks,
            saved,
        } = self
            .take_secrets(connection)
            .map_err(|failure| match failure {
                ReadFailure::Record(failure) => failure.agent_error(connection),
                ReadFailure::Credentials(error) => self.credentials_unavailable(&error),
            })?;
        let OpenedConnection {
            session,
            tunnel,
            handle,
            ..
        } = self
            .connector
            .open_with_secrets(&record, &self.prompts, saved)
            .map_err(|failure| failure.agent_error(connection))?
            .into_parts();
        let adapter = DriverSessionAdapter::new(handle, session)?.with_secrets(masks);
        // One tunnel per driver session, kept under the number of this open: closing another open
        // of the connection never reaches it.
        self.hold(&connection.id, generation, tunnel);
        Ok(Arc::new(adapter))
    }

    fn secrets(&self, connection: &ConnectionInfo) -> Vec<SecretText> {
        let read = self.read_secrets(&connection.id);
        let known = read
            .as_ref()
            .map(|prefetched| prefetched.masks.clone())
            .unwrap_or_default();
        self.prefetched
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .insert(connection.id.clone(), read);
        known
    }

    fn plain_words(&self, connection: &ConnectionInfo) -> Vec<String> {
        // After `secrets` (the layer's order): the record that read took is used, not read again.
        let prefetched = self
            .prefetched
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        match prefetched.get(&connection.id) {
            Some(Ok(prefetched)) => Self::names_in_saved_uri(connection, &prefetched.record),
            Some(Err(_)) => Vec::new(),
            None => {
                drop(prefetched);
                Connector::record(&connection.id)
                    .map(|record| Self::names_in_saved_uri(connection, &record))
                    .unwrap_or_default()
            }
        }
    }

    fn close(&self, connection: &ConnectionInfo) {
        // No generation: every tunnel of the connection (the old behaviour; the layer names one).
        let id = display_connection_id(&connection.id);
        drop(self.take_tunnels(|(held, _)| *held == id));
    }

    fn close_generation(&self, connection: &ConnectionInfo, generation: OpenGeneration) {
        // The session said goodbye through the tunnel; now that tunnel goes, and no other.
        let key = (display_connection_id(&connection.id), generation);
        let removed = self.tunnels().remove(&key);
        drop(removed);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tunnel::ssh::fake::{
        changed_host, unknown_host, unknown_host_other_key, FakeOpener,
    };
    use crate::tunnel::ssh::{SecretHop, SecretNeed, SshAuthMethod};

    /// Records every question and answers from a script.
    #[derive(Default)]
    struct Scripted {
        trust: bool,
        /// Answers `Trust` for this fingerprint instead of the one asked about.
        trust_other: Option<String>,
        password: Option<&'static str>,
        asked_keys: Mutex<Vec<String>>,
        asked_secrets: Mutex<Vec<SecretNeed>>,
        asked_hops: Mutex<Vec<(SecretHop, String)>>,
        users: Mutex<Vec<String>>,
        notices: Mutex<Vec<TunnelNotice>>,
    }

    impl TunnelPrompts for Scripted {
        fn trust_host(&self, request: &HostKeyRequest) -> HostKeyAnswer {
            self.asked_keys
                .lock()
                .unwrap()
                .push(request.public_key.clone());
            match (&self.trust_other, self.trust) {
                (Some(other), true) => HostKeyAnswer::Trust {
                    sha256_fingerprint: other.clone(),
                },
                (None, true) => HostKeyAnswer::trust(request),
                (_, false) => HostKeyAnswer::Refuse,
            }
        }
        fn secret(&self, request: &SecretRequest) -> Option<Secret> {
            self.asked_secrets.lock().unwrap().push(request.need);
            self.asked_hops
                .lock()
                .unwrap()
                .push((request.hop, request.who.clone()));
            self.password.map(Secret::new)
        }
        fn keyboard_interactive(
            &self,
            user: &str,
            request: &KeyboardInteractiveRequest,
        ) -> Option<Vec<Secret>> {
            self.users.lock().unwrap().push(user.to_owned());
            Some(
                request
                    .prompts
                    .iter()
                    .map(|_| Secret::new("123456"))
                    .collect(),
            )
        }
        fn notice(&self, notice: &TunnelNotice) {
            self.notices.lock().unwrap().push(notice.clone());
        }
    }

    fn settings(auth: SshAuthMethod) -> SshSettings {
        SshSettings {
            enabled: true,
            host: "bastion.example.com".into(),
            user: "deploy".into(),
            auth,
            ..SshSettings::default()
        }
    }

    struct Rig {
        connector: Connector,
        opener: Arc<FakeOpener>,
        prompts: Arc<Scripted>,
        _dir: tempfile::TempDir,
    }

    fn rig(prompts: Scripted) -> Rig {
        let dir = tempfile::tempdir().unwrap();
        let opener = FakeOpener::new();
        let connector = Connector::new(Arc::new(InstalledDriverStore::new(dir.path())))
            .with_tunnels(opener.clone(), Some(dir.path().join("known_hosts")));
        Rig {
            connector,
            opener,
            prompts: Arc::new(prompts),
            _dir: dir,
        }
    }

    impl Rig {
        fn dyn_prompts(&self) -> Arc<dyn TunnelPrompts> {
            self.prompts.clone()
        }

        fn open(
            &self,
            settings: SshSettings,
            secrets: SshSecrets,
        ) -> Result<(String, SessionTunnel), ConnectFailure> {
            let prompts = self.dyn_prompts();
            let mut access = self.connector.tunnel_access(settings, secrets, &prompts);
            let kind = ConnectionKind::Mysql;
            let uri = "mysql://u@db.example.com:3306/d";
            self.connector
                .with_answers(&mut access, &prompts, |access| {
                    ssh::open_for_session(access, kind, uri)
                })
        }
    }

    #[test]
    fn every_spelling_of_a_connection_id_probes_the_same_distinct_accounts_in_the_same_order() {
        let upper = "ABCDEFAB-1234-4ABC-8DEF-0123456789AB";
        let lower = upper.to_ascii_lowercase();
        // The stored (upper-case) id and the display (lower-case) id: both, upper first, once each.
        for spelling in [upper, lower.as_str()] {
            assert_eq!(accounts_of(spelling), [upper.to_owned(), lower.clone()]);
        }
        // Not a UUID: one spelling, not the same account twice.
        assert_eq!(accounts_of("open-1"), ["open-1"]);
    }

    #[test]
    fn a_host_key_nobody_trusted_is_never_recorded_and_comes_back_typed() {
        let rig = rig(Scripted::default());
        rig.opener.then(Err(unknown_host("bastion.example.com")));
        let failure = rig
            .open(settings(SshAuthMethod::Agent), SshSecrets::default())
            .unwrap_err();
        assert!(
            matches!(failure, ConnectFailure::HostKeyUnknown(_)),
            "{failure:?}"
        );
        assert!(
            rig.opener.trusted.lock().unwrap().is_empty(),
            "no trust without an answer"
        );
        assert_eq!(rig.opener.opened_count(), 1, "not tried again");
        assert!(rig.prompts.notices.lock().unwrap().is_empty());
    }

    #[test]
    fn trusting_records_exactly_the_key_that_was_asked_about_and_connects_again() {
        let rig = rig(Scripted {
            trust: true,
            ..Scripted::default()
        });
        rig.opener.then(Err(unknown_host("bastion.example.com")));
        let (uri, tunnel) = rig
            .open(settings(SshAuthMethod::Agent), SshSecrets::default())
            .unwrap();
        assert!(uri.starts_with("mysql://u@127.0.0.1:"), "{uri}");
        assert!(tunnel.is_alive());
        let asked = rig.prompts.asked_keys.lock().unwrap().clone();
        let trusted = rig.opener.trusted.lock().unwrap().clone();
        assert_eq!(trusted.len(), 1);
        assert_eq!(
            asked,
            [trusted[0].2.clone()],
            "the trusted key is the one the callback saw"
        );
        assert_eq!(
            (trusted[0].0.as_str(), trusted[0].1),
            ("bastion.example.com", 22)
        );
        assert!(matches!(
            rig.prompts.notices.lock().unwrap().as_slice(),
            [TunnelNotice::HostKeyTrusted { host, .. }] if host == "bastion.example.com"
        ));
    }

    #[test]
    fn a_second_unknown_key_is_asked_about_again_and_never_trusted_by_the_first_answer() {
        let rig = rig(Scripted {
            trust: true,
            ..Scripted::default()
        });
        rig.opener
            .then(Err(unknown_host("bastion.example.com")))
            .then(Err(unknown_host_other_key("bastion.example.com")));
        rig.open(settings(SshAuthMethod::Agent), SshSecrets::default())
            .unwrap();
        let asked = rig.prompts.asked_keys.lock().unwrap().clone();
        let trusted: Vec<_> = rig
            .opener
            .trusted
            .lock()
            .unwrap()
            .iter()
            .map(|t| t.2.clone())
            .collect();
        assert_eq!(asked.len(), 2);
        assert_ne!(asked[0], asked[1]);
        assert_eq!(trusted, asked, "each recorded key was the one just shown");
    }

    #[test]
    fn a_trust_answer_for_another_key_is_refused() {
        let rig = rig(Scripted {
            trust: true,
            trust_other: Some("SHA256:someOtherKeyThatWasNeverShown".into()),
            ..Scripted::default()
        });
        rig.opener.then(Err(unknown_host("bastion.example.com")));
        let failure = rig
            .open(settings(SshAuthMethod::Agent), SshSecrets::default())
            .unwrap_err();
        assert!(
            matches!(failure, ConnectFailure::HostKeyUnknown(_)),
            "{failure:?}"
        );
        assert!(
            rig.opener.trusted.lock().unwrap().is_empty(),
            "nothing was recorded"
        );
        assert_eq!(
            rig.opener.opened_count(),
            1,
            "and no second attempt was made"
        );
    }

    #[test]
    fn a_changed_or_revoked_key_is_never_offered_for_trust() {
        let rig = rig(Scripted {
            trust: true,
            ..Scripted::default()
        });
        rig.opener.then(Err(changed_host("bastion.example.com")));
        let failure = rig
            .open(settings(SshAuthMethod::Agent), SshSecrets::default())
            .unwrap_err();
        assert!(
            matches!(failure, ConnectFailure::HostKeyRejected(..)),
            "{failure:?}"
        );
        assert!(rig.prompts.asked_keys.lock().unwrap().is_empty());
        assert!(rig.opener.trusted.lock().unwrap().is_empty());
    }

    #[test]
    fn a_bastion_that_keeps_changing_its_mind_is_asked_a_bounded_number_of_times() {
        let rig = rig(Scripted {
            trust: true,
            ..Scripted::default()
        });
        for _ in 0..20 {
            rig.opener.then(Err(unknown_host("bastion.example.com")));
        }
        let failure = rig
            .open(settings(SshAuthMethod::Agent), SshSecrets::default())
            .unwrap_err();
        assert!(matches!(failure, ConnectFailure::HostKeyUnknown(_)));
        assert_eq!(rig.opener.opened_count(), MAX_PROMPT_ROUNDS);
        assert_eq!(
            rig.opener.trusted.lock().unwrap().len(),
            MAX_PROMPT_ROUNDS - 1
        );
    }

    #[test]
    fn a_missing_ssh_password_is_asked_for_and_used_or_reported_typed() {
        let asks = rig(Scripted {
            password: Some("pw-needle"),
            ..Scripted::default()
        });
        asks.open(settings(SshAuthMethod::Password), SshSecrets::default())
            .unwrap();
        assert_eq!(
            *asks.prompts.asked_secrets.lock().unwrap(),
            [SecretNeed::Password]
        );
        let spec = asks.opener.opened.lock().unwrap()[0].clone();
        assert!(
            matches!(spec.auth, engine_net::Auth::Password(ref secret) if secret.expose() == "pw-needle")
        );

        let refuses = rig(Scripted::default());
        let failure = refuses
            .open(settings(SshAuthMethod::Password), SshSecrets::default())
            .unwrap_err();
        assert!(
            matches!(failure, ConnectFailure::SecretNeeded(_)),
            "{failure:?}"
        );
        assert_eq!(
            refuses.opener.opened_count(),
            0,
            "nothing is sent without the secret"
        );
    }

    fn chain() -> SshSettings {
        use crate::tunnel::ssh::{JumpSettings, ProxySettings};
        SshSettings {
            jumps: vec![
                JumpSettings {
                    host: "jump1.example.com".into(),
                    port: "2201".into(),
                    user: "hop1".into(),
                    auth: SshAuthMethod::Password,
                    key_path: String::new(),
                },
                JumpSettings {
                    host: "jump2.example.com".into(),
                    port: "2202".into(),
                    user: "hop2".into(),
                    auth: SshAuthMethod::Password,
                    key_path: String::new(),
                },
            ],
            proxy: ProxySettings {
                enabled: true,
                host: "proxy.example.com".into(),
                user: "px".into(),
                ..ProxySettings::default()
            },
            ..settings(SshAuthMethod::Password)
        }
    }

    #[test]
    fn each_hops_missing_secret_is_asked_by_name_and_goes_to_that_hop_only() {
        let rig = rig(Scripted {
            password: Some("typed"),
            ..Scripted::default()
        });
        rig.open(chain(), SshSecrets::default()).unwrap();
        let asked = rig.prompts.asked_hops.lock().unwrap().clone();
        assert_eq!(
            asked.iter().map(|(hop, _)| *hop).collect::<Vec<_>>(),
            [
                SecretHop::Proxy,
                SecretHop::Jump(0),
                SecretHop::Jump(1),
                SecretHop::Bastion
            ]
        );
        assert_eq!(asked[1].1, "jump 1 of 2 hop1@jump1.example.com:2201");
        assert_eq!(asked[0].1, "proxy proxy.example.com:1080");
        let spec = rig.opener.opened.lock().unwrap()[0].clone();
        assert_eq!(spec.jumps.len(), 2);
        assert!(spec.proxy.as_ref().unwrap().credentials.is_some());

        // A secret supplied for one hop is not used for another.
        let partial = self::rig(Scripted::default());
        let mut secrets = SshSecrets::default();
        secrets.set(SecretHop::Jump(1), SecretNeed::Password, Secret::new("only-jump-2"));
        let failure = partial.open(chain(), secrets).unwrap_err();
        match failure {
            ConnectFailure::SecretNeeded(request) => assert_eq!(request.hop, SecretHop::Proxy),
            other => panic!("{other:?}"),
        }
        assert_eq!(partial.opener.opened_count(), 0);
    }

    #[test]
    fn a_jump_hosts_key_is_trusted_for_that_jump_hosts_host_and_port() {
        use crate::tunnel::ssh::fake::unknown_host_at;
        let rig = rig(Scripted {
            trust: true,
            ..Scripted::default()
        });
        rig.opener.then(Err(unknown_host_at("jump2.example.com", 2202)));
        let mut secrets = SshSecrets::default();
        for hop in [SecretHop::Proxy, SecretHop::Jump(0), SecretHop::Jump(1), SecretHop::Bastion] {
            secrets.set(hop, SecretNeed::Password, Secret::new("x"));
        }
        rig.open(chain(), secrets).unwrap();
        let trusted = rig.opener.trusted.lock().unwrap().clone();
        assert_eq!(trusted.len(), 1);
        assert_eq!(
            (trusted[0].0.as_str(), trusted[0].1),
            ("jump2.example.com", 2202),
            "exactly the host:port the dialog showed, not the bastion's"
        );
        assert!(matches!(
            rig.prompts.notices.lock().unwrap().first(),
            Some(TunnelNotice::HostKeyTrusted { host, port: 2202, .. }) if host == "jump2.example.com"
        ));
    }

    #[test]
    fn a_proxy_only_connection_opens_the_proxy_route_through_the_connector() {
        use crate::tunnel::ssh::ProxySettings;
        let rig = rig(Scripted {
            password: Some("px-secret"),
            ..Scripted::default()
        });
        let only = SshSettings {
            proxy: ProxySettings {
                enabled: true,
                host: "proxy.example.com".into(),
                user: "px".into(),
                ..ProxySettings::default()
            },
            ..SshSettings::default()
        };
        let (uri, tunnel) = rig.open(only, SshSecrets::default()).unwrap();
        assert!(uri.starts_with("mysql://u@127.0.0.1:"), "{uri}");
        assert_eq!(tunnel.via(), "SOCKS5 proxy proxy.example.com:1080");
        assert!(rig.opener.opened.lock().unwrap().is_empty());
        assert_eq!(rig.opener.opened_proxy.lock().unwrap().len(), 1);
        assert_eq!(
            rig.prompts.asked_hops.lock().unwrap().iter().map(|(hop, _)| *hop).collect::<Vec<_>>(),
            [SecretHop::Proxy]
        );
    }

    #[test]
    fn the_masks_cover_every_hops_secret() {
        let mut ssh = SshSecrets::default();
        ssh.set(SecretHop::Jump(1), SecretNeed::Password, Secret::new("jump-needle"));
        ssh.set(SecretHop::Proxy, SecretNeed::Password, Secret::new("proxy-needle"));
        let saved = SavedSecrets { database: None, ssh };
        let masks = saved.mask_texts();
        assert_eq!(masks.len(), 2);
    }

    #[test]
    fn a_bastion_that_asked_a_question_is_shared_and_the_callback_gets_the_login() {
        let rig = rig(Scripted::default());
        rig.opener.ask.lock().unwrap().push(("Code:".into(), false));
        let (_, tunnel) = rig
            .open(settings(SshAuthMethod::Agent), SshSecrets::default())
            .unwrap();
        assert!(
            tunnel.interactive,
            "a second session reuses this tunnel instead of asking again"
        );
        assert_eq!(*rig.prompts.users.lock().unwrap(), ["deploy"]);
    }

    #[test]
    fn a_front_end_that_refuses_everything_is_never_trusted_for_a_host_key() {
        let rig = rig(Scripted {
            trust: true,
            ..Scripted::default()
        });
        let prompts: Arc<dyn TunnelPrompts> = Arc::new(RefusePrompts);
        let mut access = rig.connector.tunnel_access(
            settings(SshAuthMethod::Agent),
            SshSecrets::default(),
            &prompts,
        );
        rig.opener.then(Err(unknown_host("bastion.example.com")));
        let kind = ConnectionKind::Mysql;
        let failure = rig
            .connector
            .with_answers(&mut access, &prompts, |access| {
                ssh::open_for_session(access, kind, "mysql://u@db.example.com/d")
            })
            .unwrap_err();
        assert!(matches!(failure, ConnectFailure::HostKeyUnknown(_)));
        assert!(rig.opener.trusted.lock().unwrap().is_empty());
    }

    #[test]
    fn the_guard_refuses_a_flagged_connection_without_a_tunnel_and_allows_the_rest() {
        assert!(guard_direct(false, &None).is_ok());
        let refusal = guard_direct(true, &None).unwrap_err();
        assert!(refusal
            .message()
            .starts_with("Refusing to connect directly"));
    }

    #[test]
    fn a_driver_failure_the_tunnel_explains_carries_the_tunnels_code() {
        let plain = driver_connect_failure("refused".into(), None);
        assert_eq!(plain, ConnectFailure::Message("refused".into()));
        assert_eq!(plain.code(), None);
    }

    #[test]
    fn a_direct_input_is_no_tunnel_unless_something_expected_one() {
        let rig = rig(Scripted::default());
        let prompts = rig.dyn_prompts();
        let direct = rig
            .connector
            .resolve_tunnel(TunnelInput::direct(), &prompts);
        assert!(direct.unwrap().is_none());
        let expected = TunnelInput {
            expected: true,
            ..TunnelInput::direct()
        };
        let failure = rig
            .connector
            .resolve_tunnel(expected, &prompts)
            .unwrap_err();
        assert!(
            failure.message().contains("tunnel is off"),
            "{}",
            failure.message()
        );
    }

    #[test]
    fn revoking_drops_the_agent_sessions_tunnel_after_the_session_and_is_idempotent() {
        use crate::tunnel::ssh::fake::FakeTunnel;
        let rig = rig(Scripted::default());
        let agent = AgentConnections::new(rig.connector.clone(), rig.dyn_prompts());
        let drops = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let live = |id: &str| {
            let handle: Arc<dyn crate::tunnel::ssh::TunnelHandle> = Arc::new(FakeTunnel {
                via: "deploy@bastion:22".into(),
                addrs: Vec::new(),
                alive: Arc::new(AtomicBool::new(true)),
                probe_error: None,
                drops: Some(Arc::clone(&drops)),
                drop_delay_ms: None,
            });
            let access = rig.connector.tunnel_access(
                settings(SshAuthMethod::Agent),
                SshSecrets::default(),
                &rig.dyn_prompts(),
            );
            agent.hold(
                id,
                OpenGeneration::UNTAGGED,
                Some(LiveTunnel {
                    tunnel: SessionTunnel {
                        handle,
                        targets: Vec::new(),
                        interactive: false,
                    },
                    access,
                }),
            );
        };
        let id = "0f8fad5b-d9cb-469f-a165-70867728950e";
        live(id);
        // Either spelling of the id reaches it; a second revoke finds nothing.
        agent.revoke(&id.to_uppercase());
        assert_eq!(drops.load(Ordering::SeqCst), 1, "the tunnel closed");
        agent.revoke(id);
        assert_eq!(drops.load(Ordering::SeqCst), 1);
        // `close` (what the layer calls after it disconnected the session) does the same.
        live(id);
        let info = ConnectionInfo {
            id: id.into(),
            name: "n".into(),
            kind: engine_agent::ConnectionKind::PostgreSql,
            connection_type: Default::default(),
            agent_access: None,
            statement_timeout_secs: None,
            revision: Default::default(),
        };
        agent.close(&info);
        assert_eq!(drops.load(Ordering::SeqCst), 2);
    }

    fn info_of(id: &str) -> ConnectionInfo {
        ConnectionInfo {
            id: id.into(),
            name: "n".into(),
            kind: engine_agent::ConnectionKind::PostgreSql,
            connection_type: Default::default(),
            agent_access: None,
            statement_timeout_secs: None,
            revision: Default::default(),
        }
    }

    /// A tunnel the scripted opener produced (its drops are counted in `rig.opener.drops`).
    fn scripted_tunnel(rig: &Rig) -> LiveTunnel {
        let prompts = rig.dyn_prompts();
        let (_, tunnel) = rig
            .open(settings(SshAuthMethod::Agent), SshSecrets::default())
            .expect("the scripted opener opens");
        LiveTunnel {
            tunnel,
            access: rig.connector.tunnel_access(
                settings(SshAuthMethod::Agent),
                SshSecrets::default(),
                &prompts,
            ),
        }
    }

    #[test]
    fn closing_one_open_never_drops_the_tunnel_of_a_later_open_of_the_connection() {
        let rig = rig(Scripted::default());
        let agent = AgentConnections::new(rig.connector.clone(), rig.dyn_prompts());
        let id = "0f8fad5b-d9cb-469f-a165-70867728950e";
        let drops = || rig.opener.drops.load(Ordering::SeqCst);
        // The first open's session is being closed (hung call); the connection is opened again
        // meanwhile: two tunnels, one per open.
        agent.hold(id, OpenGeneration::new(1), Some(scripted_tunnel(&rig)));
        agent.hold(id, OpenGeneration::new(2), Some(scripted_tunnel(&rig)));
        assert_eq!(rig.opener.opened_count(), 2);
        // The first close names its own open: only that tunnel goes ...
        agent.close_generation(&info_of(id), OpenGeneration::new(1));
        assert_eq!(drops(), 1);
        assert_eq!(agent.tunnels().len(), 1);
        assert!(agent
            .tunnels()
            .contains_key(&(id.to_owned(), OpenGeneration::new(2))));
        // ... a close for an open that is already gone is a no-op for the newer one ...
        agent.close_generation(&info_of(id), OpenGeneration::new(1));
        assert_eq!(drops(), 1);
        // ... and closing the newer one drops it.
        agent.close_generation(&info_of(id), OpenGeneration::new(2));
        assert_eq!(drops(), 2);
        assert!(agent.tunnels().is_empty());
    }

    /// The two steps of a successful open that the tunnels see: it starts (the watermark a revoke
    /// reaches), then it holds its tunnel.
    fn open_and_hold(agent: &AgentConnections, rig: &Rig, id: &str, number: u64) {
        let generation = OpenGeneration::new(number);
        agent.begin_open(generation);
        agent.hold(id, generation, Some(scripted_tunnel(rig)));
    }

    #[test]
    fn revoking_reaches_only_the_opens_that_started_before_it() {
        let rig = rig(Scripted::default());
        let agent = AgentConnections::new(rig.connector.clone(), rig.dyn_prompts());
        let id = "0f8fad5b-d9cb-469f-a165-70867728950e";
        let other = "1f8fad5b-d9cb-469f-a165-70867728950e";
        let drops = || rig.opener.drops.load(Ordering::SeqCst);
        // Opens 1 and 2 of the connection, one of another connection.
        open_and_hold(&agent, &rig, id, 1);
        open_and_hold(&agent, &rig, id, 2);
        open_and_hold(&agent, &rig, other, 3);
        assert_eq!(agent.newest_open.load(Ordering::SeqCst), 3);
        // A revoke that looked at the watermark when open 2 was the newest ...
        let watermark = 2;
        // ... and open 4 of the connection that starts and holds its tunnel before the revoke gets
        // to dropping tunnels: it is not reached.
        open_and_hold(&agent, &rig, id, 4);
        drop(agent.take_opens_up_to(id, watermark));
        assert_eq!(
            drops(),
            2,
            "both older tunnels of the connection; not open 4's, not the other connection's"
        );
        assert_eq!(agent.tunnels().len(), 2);
        // A revoke that starts now reaches everything that started: open 4 goes, the other
        // connection's tunnel stays. Idempotent for what is gone.
        agent.revoke(&id.to_uppercase());
        agent.revoke(id);
        assert_eq!(drops(), 3);
        assert_eq!(
            agent.tunnels().len(),
            1,
            "the other connection's tunnel stays"
        );
    }

    #[test]
    fn a_failed_agent_open_names_the_connection_and_no_address_detail() {
        use crate::drivers::DriverLoadError;
        let info = ConnectionInfo {
            name: "Billing replica".into(),
            ..info_of("0f8fad5b-d9cb-469f-a165-70867728950e")
        };
        let host_key = HostKeyRequest {
            host: "bastion.fixture-host.example".into(),
            port: 2222,
            algorithm: "ssh-ed25519".into(),
            fingerprint: "SHA256:fixtureFingerprint".into(),
            public_key: "ssh-ed25519 AAAAfixture".into(),
            user: "deploy-fixture".into(),
        };
        let secret_request = |need| SecretRequest {
            need,
            hop: SecretHop::Bastion,
            who: "deploy-fixture@bastion.fixture-host.example".into(),
            key_path: "/home/fixture-user/.ssh/id_fixture".into(),
            retry: false,
        };
        let failures = [
            // The SQLite file path in the driver's own text.
            (
                ConnectFailure::Message(
                    "unable to open database file: /Users/fixture-user/data/app.sqlite".into(),
                ),
                "connect.failed",
            ),
            // The bastion address.
            (
                ConnectFailure::HostKeyUnknown(host_key.clone()),
                "tunnel.host_key_unknown",
            ),
            (
                ConnectFailure::Tunnel {
                    code: "tunnel.connect",
                    message: "deploy-fixture@bastion.fixture-host.example:2222 refused".into(),
                },
                "tunnel.connect",
            ),
            // The SSH login and the key path.
            (
                ConnectFailure::SecretNeeded(secret_request(SecretNeed::Password)),
                "tunnel.secret_needed",
            ),
            (
                ConnectFailure::SecretNeeded(secret_request(SecretNeed::Passphrase)),
                "tunnel.secret_needed",
            ),
            // The driver store path.
            (
                ConnectFailure::Driver(DriverLoadError::ArtifactMissing {
                    driver_id: "sqlite".into(),
                    path: "/Users/fixture-user/Library/drivers/libsqlite.dylib".into(),
                }),
                "connect.driver",
            ),
            (
                ConnectFailure::Driver(DriverLoadError::Load(
                    "dlopen(/Users/fixture-user/Library/drivers/libx.dylib) failed".into(),
                )),
                "connect.driver",
            ),
        ];
        for (failure, expected_code) in failures {
            let error = failure.agent_error(&info);
            let text = error.to_string();
            assert_eq!(error.code(), expected_code, "{text}");
            assert!(
                text.contains("Billing replica"),
                "names the connection: {text}"
            );
            assert!(text.contains("TableX"), "says what to do: {text}");
            for detail in [
                "fixture-host",
                "bastion",
                "2222",
                "deploy",
                "fixture-user",
                "id_fixture",
                "/Users",
                "/home",
                ".sqlite",
                ".dylib",
                "app.",
            ] {
                assert!(!text.contains(detail), "{detail} in {text}");
            }
        }
        // What the model can act on stays: a driver that is simply not installed.
        let error = ConnectFailure::Driver(DriverLoadError::NotInstalled {
            driver_id: "sqlite".into(),
        })
        .agent_error(&info);
        assert!(error.to_string().contains("`sqlite` driver"), "{error}");
    }

    #[test]
    fn the_credential_hint_is_appended_to_the_unavailable_error_only_when_set() {
        let rig = rig(Scripted::default());
        let error = CredentialError::Platform("keychain locked".into());
        let plain = AgentConnections::new(rig.connector.clone(), rig.dyn_prompts());
        let without = plain.credentials_unavailable(&error).to_string();
        assert!(without.contains("the credential store could not be read"), "{without}");
        assert!(without.ends_with("so the session was not opened"), "{without}");
        let hinted = AgentConnections::new(rig.connector.clone(), rig.dyn_prompts())
            .with_credential_hint("run the authorize command once.".into());
        let with = hinted.credentials_unavailable(&error).to_string();
        assert!(with.starts_with(&without), "{with}");
        assert!(with.ends_with(". run the authorize command once."), "{with}");
    }

    #[test]
    fn no_secret_appears_in_debug_output() {
        let rig = rig(Scripted::default());
        let prompts = rig.dyn_prompts();
        let access = rig.connector.tunnel_access(
            settings(SshAuthMethod::Password),
            SshSecrets::from_secrets(Secret::new("needle-pw"), Secret::new("needle-pass")),
            &prompts,
        );
        let shown = format!("{access:?} {:?} {:?}", rig.connector, access.secrets);
        assert!(!shown.contains("needle"), "{shown}");
    }
}
