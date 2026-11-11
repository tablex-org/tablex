//! Connection importers: Navicat NCX, DBeaver, DataGrip and a DBX config
//! export. Each source is a pure parser from file bytes to
//! [`ImportedConnection`]s; nothing here touches the saved-connection store or
//! the credential store (see [`store`]), so the parsers are tested against
//! small fixtures and never see a real user file.
//!
//! Passwords travel as [`Secret`] (redacted `Debug`) from the decrypted file
//! to the credential store and never reach a record, a log or an error: every
//! [`ImportError`] message is fixed text.

mod crypto;
pub mod datagrip;
pub mod dbeaver;
pub mod dbx;
mod jdbc;
pub mod navicat;
pub mod store;

use crate::connection_form::ConnectionFormState;
use crate::connections::ConnectionKind;
use crate::credentials::SecretKind;
use crate::ssh_tunnel::{
    plan_for, JumpSettings, ProxySettings, ProxyType, SshAuthMethod, SshSettings,
};
use engine_storage::connection::SavedConnectionRecord;
use std::collections::HashSet;
use std::io::Read;
use std::path::{Path, PathBuf};

/// Largest input file read (a Navicat export of hundreds of connections is
/// well under 1 MiB).
const MAX_INPUT_BYTES: u64 = 16 * 1024 * 1024;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ImportSource {
    Dbx,
    Navicat,
    Dbeaver,
    DataGrip,
}

impl ImportSource {
    pub fn title(self) -> &'static str {
        match self {
            Self::Dbx => "DBX Config",
            Self::Navicat => "Navicat NCX",
            Self::Dbeaver => "DBeaver",
            Self::DataGrip => "DataGrip",
        }
    }

    /// Button label of the OS file picker.
    pub fn picker_prompt(self) -> &'static str {
        match self {
            Self::Dbx => "Import DBX Config",
            Self::Navicat => "Import Navicat NCX",
            Self::Dbeaver => "Import DBeaver",
            Self::DataGrip => "Import DataGrip",
        }
    }

    /// DBeaver and DataGrip keep several files together, so their pickers
    /// also take a folder and several files.
    pub fn picks_folders(self) -> bool {
        matches!(self, Self::Dbeaver | Self::DataGrip)
    }

    /// What to choose, shown while the picker is open.
    pub fn picker_hint(self) -> String {
        let location = |dir: Option<PathBuf>| {
            dir.map(|dir| format!(" (usually {})", dir.display()))
                .unwrap_or_default()
        };
        match self {
            Self::Dbx => "Choose a DBX connections export (.json)".into(),
            Self::Navicat => "Choose a Navicat connections export (.ncx)".into(),
            Self::Dbeaver => format!(
                "Choose the DBeaver workspace folder or data-sources.json{}",
                location(crate::platform::dbeaver_workspace_dir())
            ),
            Self::DataGrip => datagrip::PICK_HINT.to_string(),
        }
    }

    /// Said once the connections are saved, for what this source cannot give.
    fn password_note(self) -> Option<&'static str> {
        match self {
            Self::DataGrip => Some("DataGrip keeps passwords in its own keychain entries, which are not read"),
            _ => None,
        }
    }
}

/// A secret read from a foreign file. `Debug` never prints it.
#[derive(Clone, PartialEq, Eq)]
pub struct Secret(String);

impl Secret {
    pub fn new(value: impl Into<String>) -> Option<Self> {
        let value = value.into();
        (!value.is_empty()).then_some(Self(value))
    }

    pub fn expose(&self) -> &str {
        &self.0
    }
}

impl std::fmt::Debug for Secret {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("Secret(<redacted>)")
    }
}

/// Whether TableX can open the imported connection.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ImportKind {
    Supported(ConnectionKind),
    /// The foreign product, by name (e.g. `Oracle`).
    Unsupported(String),
}

impl ImportKind {
    fn of(product: &Product) -> Self {
        match product.kind {
            Some(kind) => Self::from_kind(kind),
            None => Self::Unsupported(product.label.to_string()),
        }
    }

    /// A kind the shell cannot drive is listed but not importable.
    fn from_kind(kind: ConnectionKind) -> Self {
        if kind.is_supported() {
            Self::Supported(kind)
        } else {
            Self::Unsupported(kind_label(kind).to_string())
        }
    }

    pub fn supported(&self) -> Option<ConnectionKind> {
        match self {
            Self::Supported(kind) => Some(*kind),
            Self::Unsupported(_) => None,
        }
    }

    /// Product name as the user knows it.
    pub fn label(&self) -> String {
        match self {
            Self::Supported(kind) => kind_label(*kind).to_string(),
            Self::Unsupported(label) => label.clone(),
        }
    }
}

pub fn kind_label(kind: ConnectionKind) -> &'static str {
    match kind {
        ConnectionKind::Postgres => "PostgreSQL",
        ConnectionKind::Sqlite => "SQLite",
        ConnectionKind::Mongodb => "MongoDB",
        ConnectionKind::Mysql => "MySQL",
        ConnectionKind::Mariadb => "MariaDB",
        ConnectionKind::Redis => "Redis",
        ConnectionKind::Clickhouse => "ClickHouse",
    }
}

/// A jump host of an imported tunnel (dialled before the bastion, first dialled first), with the
/// same rules as the bastion: the secrets go to the credential store, the hop is "usable" when the
/// file brought what its method needs.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct ImportedJump {
    pub host: String,
    pub port: String,
    pub user: String,
    pub auth: SshAuthMethod,
    pub key_path: String,
    pub password: Option<Secret>,
    pub passphrase: Option<Secret>,
}

impl ImportedJump {
    pub fn usable(&self) -> bool {
        match self.auth {
            SshAuthMethod::Password => self.password.is_some(),
            SshAuthMethod::KeyFile => !self.key_path.is_empty(),
            SshAuthMethod::Agent | SshAuthMethod::KeyboardInteractive => true,
        }
    }

    fn settings(&self) -> JumpSettings {
        JumpSettings {
            host: self.host.clone(),
            port: self.port.clone(),
            user: self.user.clone(),
            auth: self.auth,
            key_path: self.key_path.clone(),
        }
    }

    /// The secret to store: only what the method uses.
    fn secret(&self) -> Option<(SecretKind, Secret)> {
        match self.auth {
            SshAuthMethod::Password => self.password.clone().map(|s| (SecretKind::SshPassword, s)),
            SshAuthMethod::KeyFile => {
                self.passphrase.clone().map(|s| (SecretKind::SshPassphrase, s))
            }
            _ => None,
        }
    }
}

/// A SOCKS5 / HTTP CONNECT proxy of an imported connection. It is switched on only when its
/// login (if it has one) came with its password.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ImportedProxy {
    pub kind: ProxyType,
    pub host: String,
    pub port: String,
    pub user: String,
    pub password: Option<Secret>,
}

impl ImportedProxy {
    pub fn usable(&self) -> bool {
        !self.host.is_empty() && (self.user.is_empty() || self.password.is_some())
    }

    fn settings(&self, enabled: bool) -> ProxySettings {
        ProxySettings {
            enabled,
            kind: self.kind,
            host: self.host.clone(),
            port: self.port.clone(),
            user: self.user.clone(),
            save_secrets: true,
        }
    }

    /// `SOCKS5 proxy host:port`.
    pub fn describe(&self) -> String {
        self.settings(true).describe()
    }
}

/// SSH tunnel details of an imported connection, mapped onto TableX's own tunnel settings.
/// The tunnel is switched on only when everything it needs is there: a password for password
/// authentication (the foreign tool kept it somewhere TableX can read), a key file for a key.
/// The secrets go to the credential store like the database password, never into the record.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct ImportedSsh {
    pub host: String,
    pub port: String,
    pub user: String,
    pub auth: SshAuthMethod,
    pub key_path: String,
    pub password: Option<Secret>,
    pub passphrase: Option<Secret>,
    /// Dialled before the bastion, in order.
    pub jumps: Vec<ImportedJump>,
}

impl ImportedSsh {
    /// Whether the tunnel can be opened without asking the user for anything the file lacked:
    /// the bastion and every jump host.
    pub fn usable(&self) -> bool {
        let bastion = match self.auth {
            SshAuthMethod::Password => self.password.is_some(),
            SshAuthMethod::KeyFile => !self.key_path.is_empty(),
            SshAuthMethod::Agent | SshAuthMethod::KeyboardInteractive => true,
        };
        bastion && self.jumps.iter().all(ImportedJump::usable)
    }

    /// The `(jump index, kind, secret)` triples to store for the jump hosts.
    pub fn jump_secrets(&self) -> Vec<(usize, SecretKind, Secret)> {
        self.jumps
            .iter()
            .enumerate()
            .filter_map(|(index, jump)| jump.secret().map(|(kind, secret)| (index, kind, secret)))
            .collect()
    }

    /// The `(kind, secret)` pairs to store: only what the chosen method uses.
    pub fn secrets(&self) -> Vec<(SecretKind, Secret)> {
        match self.auth {
            SshAuthMethod::Password => self.password.clone().map(|s| (SecretKind::SshPassword, s)).into_iter().collect(),
            SshAuthMethod::KeyFile => self.passphrase.clone().map(|s| (SecretKind::SshPassphrase, s)).into_iter().collect(),
            _ => Vec::new(),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ImportedConnection {
    pub name: String,
    pub kind: ImportKind,
    /// SQLite: the database file path.
    pub host: String,
    pub port: Option<u16>,
    pub database: String,
    pub username: String,
    pub password: Option<Secret>,
    /// The connection asks for TLS/SSL.
    pub tls: bool,
    /// A multi-host / SRV MongoDB URI (no password) that the host/port
    /// fields cannot express.
    pub uri: Option<String>,
    pub ssh: Option<ImportedSsh>,
    pub proxy: Option<ImportedProxy>,
    /// Why part of the file's route could not be imported (a hop TableX cannot express or that
    /// was cut off). A route with a note is never switched on, and the selection says so.
    pub route_note: Option<&'static str>,
}

impl ImportedConnection {
    pub fn new(name: String, kind: ImportKind) -> Self {
        Self {
            name,
            kind,
            host: String::new(),
            port: None,
            database: String::new(),
            username: String::new(),
            password: None,
            tls: false,
            uri: None,
            ssh: None,
            proxy: None,
            route_note: None,
        }
    }

    /// The file named a route (or part of one): the selection lists it.
    pub fn has_route(&self) -> bool {
        self.ssh.is_some() || self.proxy.is_some() || self.route_note.is_some()
    }

    /// Why the route stays off whatever the secrets: the file's own note, or what TableX's tunnel
    /// settings would refuse (a blank SSH user, no key file...).
    fn route_off_reason(&self) -> Option<String> {
        match self.route_note {
            Some(note) => Some(note.to_owned()),
            None => self
                .route_settings(true)?
                .validate()
                .err()
                .map(|invalid| invalid.to_string()),
        }
    }

    /// The whole route (every hop, the proxy) is there: it is switched on only then, so a half
    /// route never connects some other way than the file's tool did.
    pub fn route_usable(&self) -> bool {
        self.route_off_reason().is_none()
            && self.ssh.as_ref().is_none_or(ImportedSsh::usable)
            && self.proxy.as_ref().is_none_or(ImportedProxy::usable)
    }

    /// TableX's tunnel settings for the route, `None` without one.
    fn route_settings(&self, on: bool) -> Option<SshSettings> {
        if self.ssh.is_none() && self.proxy.is_none() {
            return None;
        }
        let mut settings = SshSettings::default();
        if let Some(ssh) = &self.ssh {
            settings = SshSettings {
                enabled: on,
                host: ssh.host.clone(),
                port: ssh.port.clone(),
                user: ssh.user.clone(),
                auth: ssh.auth,
                key_path: ssh.key_path.clone(),
                save_secrets: true,
                jumps: ssh.jumps.iter().map(ImportedJump::settings).collect(),
                ..SshSettings::default()
            };
        }
        if let Some(proxy) = &self.proxy {
            settings.proxy = proxy.settings(on);
        }
        Some(settings)
    }

    /// The tunnel cannot carry this connection (SQLite, an SRV or multi-host MongoDB URI...).
    fn route_blocked_by_plan(&self, kind: ConnectionKind, record: &SavedConnectionRecord) -> bool {
        self.route_settings(true)
            .is_some_and(|settings| plan_for(kind, &record.uri, &settings).is_err())
    }

    /// Notes a route the connection's address cannot use, so the selection says it is off.
    pub fn check_route(&mut self) {
        if self.route_note.is_some() {
            return;
        }
        let Some(kind) = self.kind.supported() else { return };
        if let Ok(record) = self.record_without_route(0.0) {
            if self.route_blocked_by_plan(kind, &record) {
                self.route_note = Some("a tunnel cannot carry this connection's address");
            }
        }
    }

    /// `via SSH user@bastion[:port] (2 jumps) through SOCKS5 proxy host:port` when the connection
    /// comes with a route, and whether it will be switched on, for the selection list: a
    /// third-party file never brings one in unseen.
    pub fn tunnel_label(&self) -> Option<String> {
        if !self.has_route() {
            return None;
        }
        let mut label = match &self.ssh {
            Some(ssh) => {
                let port = if ssh.port == "22" { String::new() } else { format!(":{}", ssh.port) };
                let mut label = format!("via SSH {}@{}{port}", ssh.user, ssh.host);
                match ssh.jumps.len() {
                    0 => {}
                    1 => label.push_str(" (1 jump)"),
                    count => label.push_str(&format!(" ({count} jumps)")),
                }
                if let Some(proxy) = &self.proxy {
                    label.push_str(&format!(" through {}", proxy.describe()));
                }
                label
            }
            None => match &self.proxy {
                Some(proxy) => format!("via {}", proxy.describe()),
                None => "no tunnel or proxy imported".to_owned(),
            },
        };
        match self.route_off_reason() {
            Some(reason) => label.push_str(&format!(" (off: {reason})")),
            None if !self.route_usable() => label.push_str(" (off: a secret is missing)"),
            None => {}
        }
        Some(label)
    }

    /// `host:port/database` (or the file path) for the selection list.
    pub fn summary(&self) -> String {
        if self.kind.supported() == Some(ConnectionKind::Sqlite) {
            return self.host.clone();
        }
        let mut text = self.host.clone();
        if let Some(port) = self.port {
            text.push_str(&format!(":{port}"));
        }
        if !self.database.is_empty() {
            text.push_str(&format!("/{}", self.database));
        }
        text
    }

    /// The record to save. The password is not part of it: the caller stores
    /// [`Self::password`] under the record's id in the credential store.
    pub fn to_record(&self, now: f64) -> Result<SavedConnectionRecord, ImportError> {
        let mut record = self.record_without_route(now)?;
        let Some(kind) = self.kind.supported() else {
            return Ok(record);
        };
        let on = self.route_usable() && !self.route_blocked_by_plan(kind, &record);
        if let Some(settings) = self.route_settings(on) {
            record.network_settings_json = settings.write_into(&record.network_settings_json);
        }
        Ok(record)
    }

    fn record_without_route(&self, now: f64) -> Result<SavedConnectionRecord, ImportError> {
        let ImportKind::Supported(kind) = &self.kind else {
            return Err(ImportError::UnsupportedKind);
        };
        let mut form = ConnectionFormState::new(*kind);
        form.name = self.name.clone();
        form.password = self
            .password
            .as_ref()
            .map(|secret| secret.expose().to_string())
            .unwrap_or_default();
        match (&self.uri, kind) {
            (_, ConnectionKind::Sqlite) => form.file_path = self.host.clone(),
            (Some(uri), _) => {
                form.use_uri = true;
                form.uri = uri.clone();
            }
            _ => {
                // An IPv6 literal is bracketed in a URL.
                form.host = if self.host.contains(':') && !self.host.starts_with('[') {
                    format!("[{}]", self.host)
                } else {
                    self.host.clone()
                };
                form.port = self.port.map(|port| port.to_string()).unwrap_or_default();
                form.username = self.username.clone();
                form.database = self.database.clone();
                if self.tls {
                    match kind {
                        ConnectionKind::Mongodb => form.uri_options = "tls=true".into(),
                        ConnectionKind::Postgres => form.uri_options = "sslmode=require".into(),
                        ConnectionKind::Mysql | ConnectionKind::Mariadb => {
                            form.uri_options = "ssl-mode=REQUIRED".into()
                        }
                        ConnectionKind::Clickhouse | ConnectionKind::Redis => form.secure = true,
                        ConnectionKind::Sqlite => {}
                    }
                }
            }
        }
        form.to_record(now, None)
            .map_err(ImportError::InvalidConnection)
    }
}

/// Fixed-text failures; none carries content of the imported file.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ImportError {
    NotReadable,
    TooLarge,
    MissingFile(&'static str),
    InvalidXml,
    InvalidJson,
    NotThisFormat(&'static str),
    /// A DBX export protected by a passphrase.
    Encrypted,
    WrongPassphrase,
    UnsupportedKind,
    InvalidConnection(String),
}

impl std::fmt::Display for ImportError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::NotReadable => f.write_str("Could not read the selected file"),
            Self::TooLarge => f.write_str("The selected file is too large"),
            Self::MissingFile(name) => write!(f, "Could not find {name}"),
            Self::InvalidXml => f.write_str("The file is not valid XML"),
            Self::InvalidJson => f.write_str("The file is not valid JSON"),
            Self::NotThisFormat(what) => write!(f, "The file is not {what}"),
            Self::Encrypted => f.write_str("This DBX export is protected by a passphrase"),
            Self::WrongPassphrase => f.write_str("Wrong passphrase, or the file is damaged"),
            Self::UnsupportedKind => f.write_str("This database type is not supported"),
            Self::InvalidConnection(reason) => write!(f, "Invalid connection: {reason}"),
        }
    }
}

impl std::error::Error for ImportError {}

/// What a picked selection holds, before the user chooses.
#[derive(Debug, Clone, PartialEq)]
pub struct ImportPreview {
    pub source: ImportSource,
    pub connections: Vec<ImportedConnection>,
}

impl ImportPreview {
    /// Distinct products with no TableX equivalent, in file order.
    pub fn unsupported_labels(&self) -> Vec<String> {
        unsupported_labels(self.connections.iter())
    }

    /// Text shown in the selection dialog about what is not imported.
    pub fn note(&self) -> Option<&'static str> {
        self.source.password_note()
    }
}

pub fn unsupported_labels<'a>(
    connections: impl Iterator<Item = &'a ImportedConnection>,
) -> Vec<String> {
    let mut labels = Vec::new();
    for connection in connections {
        if let ImportKind::Unsupported(label) = &connection.kind {
            if !labels.contains(label) {
                labels.push(label.clone());
            }
        }
    }
    labels
}

/// Result of reading the picked paths.
#[derive(Debug)]
pub enum Loaded {
    Ready(ImportPreview),
    /// A DBX export that needs its passphrase (the raw file).
    NeedsPassphrase(Vec<u8>),
}

/// Reads and parses what the file picker returned. Blocking: run it off the
/// UI thread.
pub fn load(source: ImportSource, paths: &[PathBuf]) -> Result<Loaded, ImportError> {
    let mut connections = match source {
        ImportSource::Dbx => {
            let bytes = read_limited(paths.first().ok_or(ImportError::NotReadable)?)?;
            if dbx::is_encrypted(&bytes) {
                return Ok(Loaded::NeedsPassphrase(bytes));
            }
            dbx::parse(&bytes, None)?
        }
        ImportSource::Navicat => {
            navicat::parse(&read_limited(paths.first().ok_or(ImportError::NotReadable)?)?)?
        }
        ImportSource::Dbeaver => merge_sources(dbeaver::locate(paths)?.iter().map(|files| {
            let credentials = files
                .credentials
                .as_deref()
                .map(read_limited)
                .transpose()
                .unwrap_or(None);
            dbeaver::parse(&read_limited(&files.data_sources)?, credentials.as_deref())
        }))?,
        ImportSource::DataGrip => merge_sources(datagrip::locate(paths)?.iter().map(|files| {
            let local = files
                .local
                .as_deref()
                .map(read_limited)
                .transpose()
                .unwrap_or(None);
            let ssh_configs: Vec<Vec<u8>> = files
                .ssh_configs
                .iter()
                .filter_map(|path| read_limited(path).ok())
                .collect();
            let ssh_configs: Vec<&[u8]> = ssh_configs.iter().map(Vec::as_slice).collect();
            datagrip::parse(
                &read_limited(&files.data_sources)?,
                local.as_deref(),
                &ssh_configs,
                files.project_dir.as_deref(),
            )
        }))?,
    };
    for connection in &mut connections {
        connection.check_route();
    }
    Ok(Loaded::Ready(ImportPreview {
        source,
        connections,
    }))
}

/// The connections of every file read, one list: a file that fails is skipped while another
/// brought something, and the first failure is the answer when none did.
fn merge_sources(
    results: impl Iterator<Item = Result<Vec<ImportedConnection>, ImportError>>,
) -> Result<Vec<ImportedConnection>, ImportError> {
    let mut merged = Vec::new();
    let mut first_error = None;
    let mut any_read = false;
    for result in results {
        match result {
            Ok(connections) => {
                any_read = true;
                merged.extend(connections);
            }
            Err(error) => first_error = first_error.or(Some(error)),
        }
    }
    match first_error {
        Some(error) if !any_read => Err(error),
        _ => Ok(dedup(merged)),
    }
}

/// The same connection listed twice (a file read twice, a name in two projects) is kept once.
fn dedup(connections: Vec<ImportedConnection>) -> Vec<ImportedConnection> {
    let mut seen = HashSet::new();
    connections
        .into_iter()
        .filter(|c| {
            seen.insert((
                c.name.clone(),
                c.kind.label(),
                c.host.clone(),
                c.port,
                c.database.clone(),
                c.username.clone(),
                c.uri.clone(),
            ))
        })
        .collect()
}

/// A UTF-8 byte order mark some editors put in front of JSON.
fn strip_bom(bytes: &[u8]) -> &[u8] {
    bytes.strip_prefix(&[0xEF, 0xBB, 0xBF]).unwrap_or(bytes)
}

fn read_limited(path: &Path) -> Result<Vec<u8>, ImportError> {
    let mut data = Vec::new();
    std::fs::File::open(path)
        .and_then(|file| file.take(MAX_INPUT_BYTES + 1).read_to_end(&mut data))
        .map_err(|_| ImportError::NotReadable)?;
    if data.len() as u64 > MAX_INPUT_BYTES {
        return Err(ImportError::TooLarge);
    }
    Ok(data)
}

/// Pieces of the final "Imported N …" message.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ImportSummary {
    pub source: ImportSource,
    pub imported: usize,
    pub skipped_unsupported: Vec<String>,
    /// Selected connections whose fields could not form a valid record.
    pub invalid: usize,
    pub passwords_saved: usize,
    pub passwords_failed: usize,
    /// Selected connections already saved (same name, type, host, port, database and user):
    /// skipped, not duplicated.
    pub already_present: usize,
}

impl ImportSummary {
    pub fn message(&self) -> String {
        let mut text = format!(
            "Imported {} connection{} from {}",
            self.imported,
            if self.imported == 1 { "" } else { "s" },
            self.source.title()
        );
        if !self.skipped_unsupported.is_empty() {
            text.push_str(&format!(
                ", skipped unsupported: {}",
                self.skipped_unsupported.join(", ")
            ));
        }
        if self.invalid > 0 {
            text.push_str(&format!("; {} could not be converted", self.invalid));
        }
        if self.already_present > 0 {
            text.push_str(&format!("; {} already saved, skipped", self.already_present));
        }
        if self.passwords_saved > 0 {
            text.push_str(&format!(
                "; {} password{} saved to {}",
                self.passwords_saved,
                if self.passwords_saved == 1 { "" } else { "s" },
                crate::platform::CREDENTIAL_STORE_LABEL
            ));
        }
        if self.passwords_failed > 0 {
            text.push_str(&format!(
                "; {} password{} could not be saved, re-enter before connecting",
                self.passwords_failed,
                if self.passwords_failed == 1 { "" } else { "s" },
            ));
        }
        if self.passwords_saved == 0 && self.passwords_failed == 0 {
            text.push_str("; no passwords were imported, enter them before connecting");
        }
        if let Some(note) = self.source.password_note() {
            text.push_str(&format!(" ({note})"));
        }
        text
    }
}

// --- product classification shared by the JDBC-style sources ---------------

struct Product {
    /// Lowercase alphanumeric text the product is recognised by.
    needle: &'static str,
    /// `needle` must equal the whole key (short ids that would otherwise hit
    /// inside unrelated words).
    exact: bool,
    label: &'static str,
    kind: Option<ConnectionKind>,
}

const fn product(
    needle: &'static str,
    label: &'static str,
    kind: Option<ConnectionKind>,
) -> Product {
    Product {
        needle,
        exact: false,
        label,
        kind,
    }
}

const fn exact(needle: &'static str, label: &'static str, kind: Option<ConnectionKind>) -> Product {
    Product {
        needle,
        exact: true,
        label,
        kind,
    }
}

/// Ordered: forks and look-alikes before the product they speak the protocol of.
const PRODUCTS: &[Product] = &[
    product("cockroach", "CockroachDB", Some(ConnectionKind::Postgres)),
    product("cloudberry", "Apache Cloudberry", Some(ConnectionKind::Postgres)),
    product("opentenbase", "OpenTenBase", Some(ConnectionKind::Postgres)),
    product("redshift", "Redshift", None),
    product("opengauss", "openGauss", None),
    product("gaussdb", "GaussDB", None),
    product("kingbase", "KingbaseES", None),
    product("oceanbase", "OceanBase", None),
    product("starrocks", "StarRocks", None),
    product("doris", "Doris", None),
    product("clickhouse", "ClickHouse", Some(ConnectionKind::Clickhouse)),
    product("mysql", "MySQL", Some(ConnectionKind::Mysql)),
    product("mariadb", "MariaDB", Some(ConnectionKind::Mariadb)),
    product("postgresql", "PostgreSQL", Some(ConnectionKind::Postgres)),
    product("postgres", "PostgreSQL", Some(ConnectionKind::Postgres)),
    product("sqlite", "SQLite", Some(ConnectionKind::Sqlite)),
    product("mongodb", "MongoDB", Some(ConnectionKind::Mongodb)),
    exact("mongo", "MongoDB", Some(ConnectionKind::Mongodb)),
    product("redis", "Redis", Some(ConnectionKind::Redis)),
    product("sqlserver", "SQL Server", None),
    product("mssql", "SQL Server", None),
    product("jtds", "SQL Server", None),
    product("oracle", "Oracle", None),
    product("duckdb", "DuckDB", None),
    product("snowflake", "Snowflake", None),
    product("cassandra", "Cassandra", None),
    product("bigquery", "BigQuery", None),
    product("elasticsearch", "Elasticsearch", None),
    product("easysearch", "Easysearch", None),
    product("questdb", "QuestDB", None),
    product("influxdb", "InfluxDB", None),
    exact("h2", "H2", None),
    exact("dm", "Dameng", None),
    exact("dameng", "Dameng", None),
];

/// Lowercase letters and digits only: `Postgres-JDBC` becomes `postgresjdbc`.
fn normalize(value: &str) -> String {
    value
        .chars()
        .filter(char::is_ascii_alphanumeric)
        .map(|c| c.to_ascii_lowercase())
        .collect()
}

/// The first product named in `text` (already normalized).
fn find_product(text: &str) -> Option<&'static Product> {
    PRODUCTS.iter().find(|product| {
        if product.exact {
            text == product.needle
        } else {
            text.contains(product.needle)
        }
    })
}

/// A port from text, `None` when absent or out of range.
fn parse_port(value: &str) -> Option<u16> {
    value.trim().parse::<u16>().ok().filter(|port| *port > 0)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn classification_prefers_forks_and_marks_unsupported_products() {
        let kind = |text: &str| find_product(&normalize(text)).map(ImportKind::of);
        assert_eq!(
            kind("cockroachdb postgresql"),
            Some(ImportKind::Supported(ConnectionKind::Postgres))
        );
        assert_eq!(
            kind("Postgres-JDBC"),
            Some(ImportKind::Supported(ConnectionKind::Postgres))
        );
        assert_eq!(
            kind("redshift postgresql"),
            Some(ImportKind::Unsupported("Redshift".into()))
        );
        assert_eq!(
            kind("oracle.thin"),
            Some(ImportKind::Unsupported("Oracle".into()))
        );
        // Redis is a kind the shell drives (the key browser).
        assert_eq!(
            kind("redis"),
            Some(ImportKind::Supported(ConnectionKind::Redis))
        );
        // Short ids only match whole, not inside a word.
        assert_eq!(kind("admin"), None);
        assert_eq!(kind("dm"), Some(ImportKind::Unsupported("Dameng".into())));
    }

    #[test]
    fn secret_never_prints() {
        let mut connection = ImportedConnection::new(
            "db".into(),
            ImportKind::Supported(ConnectionKind::Postgres),
        );
        connection.password = Secret::new("hunter2");
        assert!(!format!("{connection:?}").contains("hunter2"));
        assert!(Secret::new("").is_none());
    }

    #[test]
    fn to_record_builds_the_uri_and_keeps_the_password_out_of_the_record() {
        let mut connection = ImportedConnection::new(
            "prod".into(),
            ImportKind::Supported(ConnectionKind::Postgres),
        );
        connection.host = "db.example.com".into();
        connection.port = Some(5433);
        connection.database = "shop".into();
        connection.username = "app".into();
        connection.password = Secret::new("p@ss/word");
        connection.tls = true;
        let record = connection.to_record(10.0).unwrap();
        assert_eq!(
            record.uri,
            "postgresql://app@db.example.com:5433/shop?sslmode=require"
        );
        assert_eq!(record.db_type, "postgresql");
        assert!(record.save_password);
        assert!(!serde_json::to_string(&record).unwrap().contains("p@ss"));
        assert_eq!(record.id, record.id.to_ascii_uppercase());
    }

    #[test]
    fn to_record_for_sqlite_and_uri_and_unsupported() {
        let mut sqlite = ImportedConnection::new(
            "file".into(),
            ImportKind::Supported(ConnectionKind::Sqlite),
        );
        sqlite.host = "/data/app.db".into();
        assert_eq!(sqlite.to_record(1.0).unwrap().uri, "sqlite:///data/app.db");
        assert!(!sqlite.to_record(1.0).unwrap().save_password);

        let mut mongo = ImportedConnection::new(
            "rs".into(),
            ImportKind::Supported(ConnectionKind::Mongodb),
        );
        mongo.uri = Some("mongodb://u@a:1,b:2/db?replicaSet=rs0".into());
        assert_eq!(
            mongo.to_record(1.0).unwrap().uri,
            "mongodb://u@a:1,b:2/db?replicaSet=rs0"
        );

        let oracle = ImportedConnection::new("o".into(), ImportKind::Unsupported("Oracle".into()));
        assert_eq!(oracle.to_record(1.0), Err(ImportError::UnsupportedKind));
    }

    fn supported(kind: ConnectionKind, host: &str) -> ImportedConnection {
        let mut connection = ImportedConnection::new("c".into(), ImportKind::Supported(kind));
        connection.host = host.into();
        connection.port = Some(5432);
        connection
    }

    #[test]
    fn an_ipv6_host_is_bracketed_so_the_record_can_be_saved() {
        for host in ["::1", "fe80::1", "[::1]"] {
            let record = supported(ConnectionKind::Postgres, host).to_record(1.0).unwrap();
            let bracketed = format!("[{}]", host.trim_matches(['[', ']']));
            assert!(record.uri.contains(&bracketed), "{host}: {}", record.uri);
        }
        // Each importer hands the host over unbracketed or bracketed; all of them save.
        let navicat = navicat::parse(br#"<Connections><Connection ConnectionName="v6" ConnType="POSTGRESQL" Host="fe80::1" Port="5432" UserName="u" /></Connections>"#).unwrap();
        assert!(navicat[0].to_record(1.0).unwrap().uri.contains("[fe80::1]:5432"));
        let dbx = dbx::parse(br#"[{"name":"v6","db_type":"mysql","host":"::1","port":3306}]"#, None).unwrap();
        assert!(dbx[0].to_record(1.0).unwrap().uri.contains("[::1]:3306"));
        let jdbc = datagrip::parse(br#"<project><component><data-source name="v6" uuid="a"><driver-ref>postgresql</driver-ref><jdbc-url>jdbc:postgresql://[::1]:5432/db</jdbc-url></data-source></component></project>"#, None, &[], None).unwrap();
        assert!(jdbc[0].to_record(1.0).unwrap().uri.contains("[::1]:5432/db"));
    }

    #[test]
    fn tls_maps_onto_each_kinds_own_setting() {
        let uri = |kind| {
            let mut connection = supported(kind, "db.example.com");
            connection.tls = true;
            connection.to_record(1.0).unwrap().uri
        };
        assert!(uri(ConnectionKind::Mysql).contains("ssl-mode=REQUIRED"), "{}", uri(ConnectionKind::Mysql));
        assert!(uri(ConnectionKind::Mariadb).contains("ssl-mode=REQUIRED"));
        assert!(uri(ConnectionKind::Redis).starts_with("rediss://"), "{}", uri(ConnectionKind::Redis));
        assert!(uri(ConnectionKind::Postgres).contains("sslmode=require"));
        assert!(uri(ConnectionKind::Clickhouse).contains("secure=true"));
    }

    fn agent_ssh(user: &str) -> ImportedSsh {
        ImportedSsh {
            host: "bastion.example.com".into(),
            port: "22".into(),
            user: user.into(),
            auth: SshAuthMethod::Agent,
            ..ImportedSsh::default()
        }
    }

    #[test]
    fn an_imported_route_the_tunnel_settings_refuse_is_saved_off_with_the_reason() {
        let mut blank_user = supported(ConnectionKind::Postgres, "db.example.com");
        blank_user.ssh = Some(agent_ssh(""));
        assert!(!blank_user.route_usable());
        let label = blank_user.tunnel_label().unwrap();
        assert!(label.contains("(off: SSH user is required"), "{label}");
        let route = SshSettings::from_network_json(&blank_user.to_record(1.0).unwrap().network_settings_json);
        assert!(!route.enabled, "validate refused it");
        assert_eq!(route.host, "bastion.example.com", "the hop is kept for the user to complete");

        let mut fine = supported(ConnectionKind::Postgres, "db.example.com");
        fine.ssh = Some(agent_ssh("ops"));
        assert!(SshSettings::from_network_json(&fine.to_record(1.0).unwrap().network_settings_json).enabled);
    }

    #[test]
    fn a_route_the_connections_address_cannot_use_is_off_and_says_so() {
        // A SQLite file and an SRV seed list have no host the bastion could forward to.
        let mut sqlite = supported(ConnectionKind::Sqlite, "/data/app.db");
        sqlite.ssh = Some(agent_ssh("ops"));
        let mut srv = supported(ConnectionKind::Mongodb, "cluster0.example.com");
        srv.uri = Some("mongodb+srv://cluster0.example.com/db".into());
        srv.ssh = Some(agent_ssh("ops"));
        for mut connection in [sqlite, srv] {
            let route = |c: &ImportedConnection| SshSettings::from_network_json(&c.to_record(1.0).unwrap().network_settings_json);
            assert!(!route(&connection).enabled, "never a tunnel that cannot open");
            assert_eq!(connection.route_note, None);
            connection.check_route();
            assert!(connection.route_note.is_some());
            assert!(connection.tunnel_label().unwrap().contains("(off: a tunnel cannot carry"));
            assert!(!route(&connection).enabled);
        }
    }

    #[test]
    fn importing_a_selection_twice_is_recognised_as_present() {
        let mut batch = store::Batch::build(&[supported(ConnectionKind::Postgres, "db.example.com")], 1.0);
        let saved: Vec<_> = batch.records.iter().map(crate::connections::SavedConnection::from_record).collect();
        let mut again = store::Batch::build(
            &[supported(ConnectionKind::Postgres, "db.example.com"), supported(ConnectionKind::Postgres, "other.example.com")],
            2.0,
        );
        assert_eq!(again.skip_existing(&saved), 1);
        assert_eq!(again.records.len(), 1);
        assert!(again.records[0].uri.contains("other.example.com"));
        assert_eq!(batch.skip_existing(&[]), 0);
    }

    #[test]
    fn several_files_merge_and_a_bad_one_only_matters_when_nothing_was_read() {
        let one = || Ok(vec![supported(ConnectionKind::Postgres, "a.example.com")]);
        let merged = merge_sources([one(), Err(ImportError::InvalidJson), one()].into_iter()).unwrap();
        assert_eq!(merged.len(), 1, "the same connection from two files is kept once");
        assert_eq!(
            merge_sources([Err(ImportError::InvalidJson), Err(ImportError::InvalidXml)].into_iter()),
            Err(ImportError::InvalidJson)
        );
    }

    #[test]
    fn summary_message_counts_skips_and_passwords() {
        let summary = ImportSummary {
            source: ImportSource::Navicat,
            imported: 2,
            skipped_unsupported: vec!["Oracle".into(), "SQL Server".into()],
            invalid: 0,
            passwords_saved: 1,
            passwords_failed: 0,
            already_present: 0,
        };
        assert_eq!(
            summary.message(),
            format!(
                "Imported 2 connections from Navicat NCX, skipped unsupported: Oracle, SQL Server; 1 password saved to {}",
                crate::platform::CREDENTIAL_STORE_LABEL
            )
        );
        let repeated = ImportSummary { already_present: 3, ..summary };
        assert!(repeated.message().contains("; 3 already saved, skipped"), "{}", repeated.message());
        let datagrip = ImportSummary {
            source: ImportSource::DataGrip,
            imported: 1,
            skipped_unsupported: Vec::new(),
            invalid: 0,
            passwords_saved: 0,
            passwords_failed: 0,
            already_present: 0,
        };
        assert!(datagrip.message().contains("keychain entries"));
        assert!(datagrip.message().contains("no passwords were imported"));
    }
}
