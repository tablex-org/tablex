//! SSH tunnel settings of a saved connection and the glue between the app and `engine-net`.
//!
//! * [`SshSettings`] is the typed network settings of a connection: the `ssh` object (with its jump
//!   hosts) and the `proxy` object. It never holds a secret: the SSH passwords and key passphrases
//!   (of the bastion and of each jump host) and the proxy password live in the credential store
//!   ([`SshSecrets`] is their in-memory form, `Secret`s that print nothing and wipe themselves).
//! * [`TunnelOpener`] / [`TunnelHandle`] are the seam over `engine_net::Tunnel` (a fake in tests).
//! * [`open_for_session`] turns a driver URI into a tunnelled one: **one tunnel per database
//!   session**, because a forward whose reader stalls slows every other forward of its tunnel
//!   (plan, "Known limits"). Only a keyboard-interactive / one-time-code bastion shares one tunnel
//!   per connection, so the second session does not ask for the code again.
//!
//! Nothing here touches the UI thread's time budget: the callers run it on a background thread.

use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

use engine_net::{
    Auth, ConfigError, Jump, Prompts, ProxyCredentials, ProxyRoute, ProxySpec, Secret, Target,
    Tunnel, TunnelError, TunnelSpec,
};
use serde_json::{json, Map, Value};

pub use super::route::{
    JumpField, JumpSettings, ProxySettings, ProxyType, SecretHop, SecretNeed, SettingsInvalid,
};
use super::plan::{Endpoint, PlanError, Route, TunnelPlan};
use super::route::parse_jumps;
use crate::credentials::{CredentialStore, SecretKind};
use crate::drivers::DriverLoadError;
use crate::records::ConnectionKind;

// ---------------------------------------------------------------------------------------------
// Settings
// ---------------------------------------------------------------------------------------------

/// How the bastion is authenticated.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum SshAuthMethod {
    #[default]
    Password,
    KeyFile,
    Agent,
    /// Every challenge (a one-time code, a second factor) is answered by the user in a dialog.
    KeyboardInteractive,
}

impl SshAuthMethod {
    pub const ALL: [Self; 4] = [
        Self::Password,
        Self::KeyFile,
        Self::Agent,
        Self::KeyboardInteractive,
    ];

    pub fn label(self) -> &'static str {
        match self {
            Self::Password => "Password",
            Self::KeyFile => "Private key",
            Self::Agent => "SSH agent",
            Self::KeyboardInteractive => "Keyboard-interactive",
        }
    }

    pub(super) fn persisted(self) -> &'static str {
        match self {
            Self::Password => "password",
            Self::KeyFile => "key",
            Self::Agent => "agent",
            Self::KeyboardInteractive => "keyboard-interactive",
        }
    }

    pub(super) fn from_persisted(text: &str) -> Option<Self> {
        Self::ALL
            .into_iter()
            .find(|method| method.persisted() == text)
    }
}

/// A connection's network settings: the `ssh` object (the bastion and its jump hosts) and the
/// `proxy` object. `enabled` is the SSH switch only; [`Self::routed`] says whether the connection
/// goes through anything at all.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SshSettings {
    pub enabled: bool,
    pub host: String,
    pub port: String,
    pub user: String,
    pub auth: SshAuthMethod,
    pub key_path: String,
    /// The `~/.ssh/config` alias the fields were filled from (display only).
    pub config_host: String,
    /// Where the bastion reaches the database, when not the URI's own host.
    pub remote_host: String,
    pub remote_port: String,
    pub allow_legacy_algorithms: bool,
    /// Keep the passwords / passphrases (of the bastion and the jump hosts) in the credential store.
    pub save_secrets: bool,
    /// SSH servers dialled before the bastion, first dialled first (at most `engine_net::MAX_JUMPS`).
    pub jumps: Vec<JumpSettings>,
    pub proxy: ProxySettings,
}

impl Default for SshSettings {
    fn default() -> Self {
        Self {
            enabled: false,
            host: String::new(),
            port: "22".into(),
            user: String::new(),
            auth: SshAuthMethod::Password,
            key_path: String::new(),
            config_host: String::new(),
            remote_host: String::new(),
            remote_port: String::new(),
            allow_legacy_algorithms: false,
            save_secrets: true,
            jumps: Vec::new(),
            proxy: ProxySettings::default(),
        }
    }
}

pub(super) fn text_of(object: &Map<String, Value>, key: &str) -> Option<String> {
    match object.get(key)? {
        Value::String(text) => Some(text.clone()),
        Value::Number(number) => Some(number.to_string()),
        _ => None,
    }
}

pub(super) fn flag_of(object: &Map<String, Value>, key: &str) -> Option<bool> {
    object.get(key)?.as_bool()
}

/// The saved network settings cannot be understood: the connect must stop, never fall back to a
/// direct connection.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SettingsError(pub String);

impl std::fmt::Display for SettingsError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "cannot read this connection's tunnel settings: {}",
            self.0
        )
    }
}

impl std::error::Error for SettingsError {}

impl SshSettings {
    /// For display (the dialog, status): anything unreadable shows as the default. Never use it
    /// to decide whether to tunnel: that is [`Self::parse`].
    pub fn from_network_json(json: &str) -> Self {
        Self::parse(json).unwrap_or_default()
    }

    /// Reads `network_settings_json`. An empty text or no `ssh` section is "no tunnel"; text that is
    /// not a JSON object, an `ssh` that is not an object or an `isEnabled` that is not a boolean is
    /// an error (a switch that cannot be read must not read as "off").
    pub fn parse(json: &str) -> Result<Self, SettingsError> {
        let mut settings = Self::default();
        if json.trim().is_empty() {
            return Ok(settings);
        }
        let root: Value = serde_json::from_str(json)
            .map_err(|_| SettingsError("the network settings are not valid JSON".into()))?;
        let Some(root) = root.as_object() else {
            return Err(SettingsError(
                "the network settings are not an object".into(),
            ));
        };
        if let Some(proxy) = root.get("proxy") {
            let Some(proxy) = proxy.as_object() else {
                return Err(SettingsError("the proxy section is not an object".into()));
            };
            settings.proxy = ProxySettings::parse(proxy)?;
        }
        let Some(ssh) = root.get("ssh") else {
            return Ok(settings);
        };
        let Some(ssh) = ssh.as_object() else {
            return Err(SettingsError("the ssh section is not an object".into()));
        };
        settings.enabled = match ssh.get("isEnabled") {
            None => false,
            Some(Value::Bool(enabled)) => *enabled,
            Some(_) => return Err(SettingsError("isEnabled is not true or false".into())),
        };
        let ssh = ssh.clone();
        settings.host = text_of(&ssh, "host").unwrap_or_default().trim().to_owned();
        if let Some(port) = text_of(&ssh, "port").filter(|port| !port.trim().is_empty()) {
            settings.port = port.trim().to_owned();
        }
        settings.user = text_of(&ssh, "user").unwrap_or_default().trim().to_owned();
        settings.key_path = text_of(&ssh, "privateKeyPath").unwrap_or_default();
        settings.auth = text_of(&ssh, "authMethod")
            .and_then(|method| SshAuthMethod::from_persisted(&method))
            // Settings written before the method existed (importers): a key path means a key.
            .unwrap_or(if settings.key_path.is_empty() {
                SshAuthMethod::Password
            } else {
                SshAuthMethod::KeyFile
            });
        settings.config_host = text_of(&ssh, "configHost").unwrap_or_default();
        settings.remote_host = text_of(&ssh, "remoteHost")
            .unwrap_or_default()
            .trim()
            .to_owned();
        settings.remote_port = text_of(&ssh, "remotePort")
            .unwrap_or_default()
            .trim()
            .to_owned();
        settings.allow_legacy_algorithms = flag_of(&ssh, "allowLegacyAlgorithms").unwrap_or(false);
        settings.save_secrets = flag_of(&ssh, "saveSecrets").unwrap_or(true);
        settings.jumps = parse_jumps(&ssh)?;
        Ok(settings)
    }

    /// `network_settings_json` with the `ssh` and `proxy` objects merged in: every other key (keys
    /// this version does not know, in both objects) is kept as it was. The jump list is rewritten
    /// as a whole.
    pub fn write_into(&self, network_json: &str) -> String {
        let mut root = serde_json::from_str::<Value>(network_json)
            .ok()
            .and_then(|value| value.as_object().cloned())
            .unwrap_or_default();
        let mut ssh = root
            .get("ssh")
            .and_then(Value::as_object)
            .cloned()
            .unwrap_or_default();
        ssh.insert("isEnabled".into(), json!(self.enabled));
        for (key, value) in [
            ("host", &self.host),
            ("port", &self.port),
            ("user", &self.user),
            ("privateKeyPath", &self.key_path),
            ("configHost", &self.config_host),
            ("remoteHost", &self.remote_host),
            ("remotePort", &self.remote_port),
        ] {
            ssh.insert(key.into(), json!(value.trim()));
        }
        ssh.insert("authMethod".into(), json!(self.auth.persisted()));
        ssh.insert(
            "allowLegacyAlgorithms".into(),
            json!(self.allow_legacy_algorithms),
        );
        ssh.insert("saveSecrets".into(), json!(self.save_secrets));
        if !self.jumps.is_empty() || ssh.contains_key("jumps") {
            ssh.insert(
                "jumps".into(),
                Value::Array(self.jumps.iter().map(JumpSettings::to_json).collect()),
            );
        }
        root.insert("ssh".into(), Value::Object(ssh));
        if self.proxy != ProxySettings::default() || root.contains_key("proxy") {
            self.proxy.write_into(&mut root);
        }
        Value::Object(root).to_string()
    }

    /// How a refusal of the URI is worded: an SSH tunnel, or a proxy alone.
    pub fn plan_route(&self) -> Route {
        if self.enabled {
            Route::Ssh
        } else {
            Route::Proxy
        }
    }

    /// The bastion or a jump host authenticates with keyboard-interactive: it asks questions, so
    /// the connection's sessions share one tunnel.
    pub fn asks_questions(&self) -> bool {
        self.enabled
            && (self.auth == SshAuthMethod::KeyboardInteractive
                || self
                    .jumps
                    .iter()
                    .any(|jump| jump.auth == SshAuthMethod::KeyboardInteractive))
    }

    /// The connection goes through something: the SSH tunnel and / or the proxy. A connection
    /// that is `routed` never connects directly.
    pub fn routed(&self) -> bool {
        self.enabled || self.proxy.enabled
    }

    /// What must be filled in before the route can be opened. Each section is only checked when
    /// it is enabled (the jump hosts belong to the SSH section).
    pub fn validate(&self) -> Result<(), SettingsInvalid> {
        if self.enabled {
            self.validate_ssh()?;
        }
        if self.proxy.enabled {
            if self.proxy.host.trim().is_empty() {
                return Err(SettingsInvalid::ProxyHost);
            }
            if self.proxy.port_number().is_none() {
                return Err(SettingsInvalid::ProxyPort);
            }
        }
        Ok(())
    }

    fn validate_ssh(&self) -> Result<(), SettingsInvalid> {
        if self.host.trim().is_empty() {
            return Err(SettingsInvalid::SshHost);
        }
        if self.port_number().is_none() {
            return Err(SettingsInvalid::SshPort);
        }
        if self.user.trim().is_empty() {
            return Err(SettingsInvalid::SshUser);
        }
        if self.auth == SshAuthMethod::KeyFile && self.key_path.trim().is_empty() {
            return Err(SettingsInvalid::SshKeyFile);
        }
        if !self.remote_port.trim().is_empty() && parse_port(&self.remote_port).is_none() {
            return Err(SettingsInvalid::RemotePort);
        }
        if self.jumps.len() > engine_net::MAX_JUMPS {
            return Err(SettingsInvalid::TooManyJumps {
                max: engine_net::MAX_JUMPS,
            });
        }
        for (index, jump) in self.jumps.iter().enumerate() {
            let number = index + 1;
            let wrong = |field| Err(SettingsInvalid::Jump { number, field });
            if jump.host.trim().is_empty() {
                return wrong(JumpField::Host);
            }
            if jump.port_number().is_none() {
                return wrong(JumpField::Port);
            }
            if jump.user.trim().is_empty() {
                return wrong(JumpField::User);
            }
            if jump.auth == SshAuthMethod::KeyFile && jump.key_path.trim().is_empty() {
                return wrong(JumpField::KeyFile);
            }
        }
        // `engine-net` refuses a server that appears twice (a loop); say so before connecting.
        let mut seen: Vec<(String, Option<u16>)> = Vec::new();
        let hops = self
            .jumps
            .iter()
            .map(|jump| (jump.host.trim(), jump.port_number()))
            .chain([(self.host.trim(), self.port_number())]);
        for (host, port) in hops {
            let key = (host.to_ascii_lowercase(), port);
            if seen.contains(&key) {
                return Err(SettingsInvalid::RepeatedHop {
                    hop: format!("{host}:{}", port.unwrap_or_default()),
                });
            }
            seen.push(key);
        }
        Ok(())
    }

    pub fn port_number(&self) -> Option<u16> {
        parse_port(&self.port)
    }

    /// `user@host`, for status text before a tunnel exists.
    pub fn via(&self) -> String {
        format!("{}@{}", self.user.trim(), self.host.trim())
    }

    /// The login of the SSH server at `host:port` (a jump host or the bastion), for the dialog
    /// that shows the questions that server asks.
    pub fn user_at(&self, host: &str, port: u16) -> String {
        self.jumps
            .iter()
            .find(|jump| jump.host.trim() == host && jump.port_number() == Some(port))
            .map_or_else(|| self.user.trim(), |jump| jump.user.trim())
            .to_owned()
    }

    /// How a prompt names a hop: `user@host` for a lone bastion, `jump 1 of 2 user@host:port`,
    /// `bastion user@host:port` (when there are jumps) and `proxy host:port`.
    pub fn hop_label(&self, hop: SecretHop) -> String {
        match hop {
            SecretHop::Bastion if self.jumps.is_empty() => self.via(),
            SecretHop::Bastion => format!(
                "bastion {}@{}:{}",
                self.user.trim(),
                self.host.trim(),
                self.port.trim()
            ),
            SecretHop::Jump(index) => {
                let jump = self.jumps.get(index).cloned().unwrap_or_default();
                format!(
                    "jump {} of {} {}@{}:{}",
                    index + 1,
                    self.jumps.len(),
                    jump.user.trim(),
                    jump.host.trim(),
                    jump.port.trim()
                )
            }
            SecretHop::Proxy => format!(
                "proxy {}:{}",
                self.proxy.host.trim(),
                self.proxy.port.trim()
            ),
        }
    }

    /// How the hop authenticates, `None` when the hop is not part of the route.
    fn hop_auth(&self, hop: SecretHop) -> Option<SshAuthMethod> {
        match hop {
            SecretHop::Bastion => self.enabled.then_some(self.auth),
            SecretHop::Jump(index) => self
                .enabled
                .then(|| self.jumps.get(index).map(|jump| jump.auth))
                .flatten(),
            SecretHop::Proxy => None,
        }
    }

    /// Every secret the route can use, in the order the route is dialled: the proxy password (when
    /// the proxy has a login), then each jump host's password or key passphrase, then the
    /// bastion's. A key passphrase is only needed when the key is encrypted.
    pub fn secret_slots(&self) -> Vec<(SecretHop, SecretNeed)> {
        let need_of = |auth: SshAuthMethod| match auth {
            SshAuthMethod::Password => Some(SecretNeed::Password),
            SshAuthMethod::KeyFile => Some(SecretNeed::Passphrase),
            SshAuthMethod::Agent | SshAuthMethod::KeyboardInteractive => None,
        };
        let mut slots = Vec::new();
        if self.proxy.has_login() {
            slots.push((SecretHop::Proxy, SecretNeed::Password));
        }
        let ssh_hops = (0..self.jumps.len())
            .map(SecretHop::Jump)
            .chain([SecretHop::Bastion]);
        for hop in ssh_hops {
            if let Some(need) = self.hop_auth(hop).and_then(need_of) {
                slots.push((hop, need));
            }
        }
        slots
    }

    /// Whether the hop's secrets are kept in the credential store (its section's "save" choice).
    pub fn saves_secrets(&self, hop: SecretHop) -> bool {
        match hop {
            SecretHop::Proxy => self.proxy.save_secrets,
            SecretHop::Bastion | SecretHop::Jump(_) => self.save_secrets,
        }
    }

    /// The slots whose secrets a connect reads from the credential store.
    pub fn saved_slots(&self) -> Vec<(SecretHop, SecretNeed)> {
        self.secret_slots()
            .into_iter()
            .filter(|(hop, _)| self.saves_secrets(*hop))
            .collect()
    }

    /// A route that dials a server a saved secret was not entered for is a different route: the
    /// bastion, a jump host (by position) or the proxy changed host, port, user or kind.
    pub fn hop_changed(old: &Self, new: &Self, hop: SecretHop) -> bool {
        match hop {
            SecretHop::Bastion => bastion_changed(old, new),
            SecretHop::Jump(index) => {
                // A new row has nothing saved to protect; a removed or replaced one does (also
                // when the route was off: an import stores secrets for a route that comes in off).
                old.jumps.get(index).is_some_and(|before| {
                    !before.host.trim().is_empty()
                        && new
                            .jumps
                            .get(index)
                            .is_none_or(|after| !before.same_hop(after))
                })
            }
            SecretHop::Proxy => {
                !old.proxy.host.trim().is_empty() && !old.proxy.same_proxy(&new.proxy)
            }
        }
    }

    /// [`Self::hop_changed`] for any hop of the route.
    pub fn route_changed(old: &Self, new: &Self) -> bool {
        bastion_changed(old, new)
            || (0..old.jumps.len().max(new.jumps.len()))
                .any(|index| Self::hop_changed(old, new, SecretHop::Jump(index)))
            || Self::hop_changed(old, new, SecretHop::Proxy)
    }

    /// The endpoint the bastion forwards to instead of the URI's own, when overridden.
    fn remote_override(&self, from_uri: &Endpoint) -> Option<Endpoint> {
        let host = self.remote_host.trim();
        let port = parse_port(&self.remote_port);
        if host.is_empty() && port.is_none() {
            return None;
        }
        Some(Endpoint::new(
            if host.is_empty() {
                from_uri.host.as_str()
            } else {
                host
            },
            port.unwrap_or(from_uri.port),
        ))
    }

    fn needs_password(&self) -> bool {
        self.auth == SshAuthMethod::Password
    }
}

pub(super) fn parse_port(text: &str) -> Option<u16> {
    text.trim().parse::<u16>().ok().filter(|port| *port != 0)
}

// ---------------------------------------------------------------------------------------------
// Secrets
// ---------------------------------------------------------------------------------------------

/// The password and key passphrase of one jump host.
#[derive(Debug, Clone, Default)]
pub struct HopSecrets {
    pub password: Option<Secret>,
    pub passphrase: Option<Secret>,
}

impl HopSecrets {
    fn or(self, fallback: Self) -> Self {
        Self {
            password: self.password.or(fallback.password),
            passphrase: self.passphrase.or(fallback.passphrase),
        }
    }
}

/// The secrets of a route, in memory only: the bastion's SSH password and key passphrase, those of
/// each jump host (by position) and the proxy password.
#[derive(Debug, Clone, Default)]
pub struct SshSecrets {
    pub password: Option<Secret>,
    pub passphrase: Option<Secret>,
    pub jumps: Vec<HopSecrets>,
    pub proxy: Option<Secret>,
}

impl SshSecrets {
    pub fn is_empty(&self) -> bool {
        self.iter().next().is_none()
    }

    /// Every secret held.
    pub fn iter(&self) -> impl Iterator<Item = &Secret> {
        [&self.password, &self.passphrase, &self.proxy]
            .into_iter()
            .chain(
                self.jumps
                    .iter()
                    .flat_map(|jump| [&jump.password, &jump.passphrase]),
            )
            .flatten()
    }

    /// `self` wins; whatever it lacks comes from `fallback`.
    pub fn or(self, fallback: Self) -> Self {
        let mut fallback_jumps = fallback.jumps.into_iter();
        let mut jumps: Vec<HopSecrets> = self
            .jumps
            .into_iter()
            .map(|jump| jump.or(fallback_jumps.next().unwrap_or_default()))
            .collect();
        jumps.extend(fallback_jumps);
        Self {
            password: self.password.or(fallback.password),
            passphrase: self.passphrase.or(fallback.passphrase),
            jumps,
            proxy: self.proxy.or(fallback.proxy),
        }
    }

    pub fn get(&self, hop: SecretHop, need: SecretNeed) -> Option<&Secret> {
        match (hop, need) {
            (SecretHop::Proxy, _) => self.proxy.as_ref(),
            (SecretHop::Bastion, SecretNeed::Password) => self.password.as_ref(),
            (SecretHop::Bastion, SecretNeed::Passphrase) => self.passphrase.as_ref(),
            (SecretHop::Jump(index), need) => {
                let jump = self.jumps.get(index)?;
                match need {
                    SecretNeed::Password => jump.password.as_ref(),
                    SecretNeed::Passphrase => jump.passphrase.as_ref(),
                }
            }
        }
    }

    pub fn set(&mut self, hop: SecretHop, need: SecretNeed, secret: Secret) {
        match (hop, need) {
            (SecretHop::Proxy, _) => self.proxy = Some(secret),
            (SecretHop::Bastion, SecretNeed::Password) => self.password = Some(secret),
            (SecretHop::Bastion, SecretNeed::Passphrase) => self.passphrase = Some(secret),
            (SecretHop::Jump(index), need) => {
                if self.jumps.len() <= index {
                    self.jumps.resize_with(index + 1, HopSecrets::default);
                }
                match need {
                    SecretNeed::Password => self.jumps[index].password = Some(secret),
                    SecretNeed::Passphrase => self.jumps[index].passphrase = Some(secret),
                }
            }
        }
    }

    /// Just this one secret (an answer to a prompt); an empty one is "nothing typed".
    pub fn only(hop: SecretHop, need: SecretNeed, secret: Secret) -> Self {
        let mut secrets = Self::default();
        if !secret.is_empty() {
            secrets.set(hop, need, secret);
        }
        secrets
    }

    /// Typed secrets (an empty one is "nothing typed").
    pub fn from_secrets(password: Secret, passphrase: Secret) -> Self {
        let keep = |secret: Secret| (!secret.is_empty()).then_some(secret);
        Self {
            password: keep(password),
            passphrase: keep(passphrase),
            ..Self::default()
        }
    }

    /// Test helper: typed text becomes secrets.
    #[cfg(any(test, feature = "testing"))]
    pub fn from_text(password: &str, passphrase: &str) -> Self {
        Self::from_secrets(Secret::new(password), Secret::new(passphrase))
    }

    /// The saved secrets the route uses (the store is only asked for the sections that save
    /// them). A store that fails reads as "nothing saved": the connect then asks.
    pub fn load(
        store: &(impl CredentialStore + ?Sized),
        accounts: &[String],
        settings: &SshSettings,
    ) -> Self {
        let mut secrets = Self::default();
        for (hop, need) in settings.saved_slots() {
            let saved = accounts
                .iter()
                .find_map(|account| {
                    store
                        .load(hop.kind(need), &hop.account(account))
                        .ok()
                        .flatten()
                })
                .filter(|text| !text.is_empty());
            if let Some(text) = saved {
                secrets.set(hop, need, Secret::new(text));
            }
        }
        secrets
    }
}

/// What the dialog typed, as zeroizing `Secret`s. `Debug` prints nothing of it. (The text widgets'
/// own buffers cannot be wiped: they are emptied when the dialog closes, and russh keeps copies it
/// does not zeroize, see the plan.)
#[derive(Clone, Default)]
pub struct SshSecretInput {
    pub password: Secret,
    pub passphrase: Secret,
    /// One per jump row: its password or its key passphrase, whichever the row's method uses.
    pub jumps: Vec<Secret>,
    pub proxy: Secret,
}

impl std::fmt::Debug for SshSecretInput {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("SshSecretInput(<hidden>)")
    }
}

impl PartialEq for SshSecretInput {
    fn eq(&self, other: &Self) -> bool {
        self.password.expose() == other.password.expose()
            && self.passphrase.expose() == other.passphrase.expose()
            && self.proxy.expose() == other.proxy.expose()
            && self.jumps.len() == other.jumps.len()
            && self
                .jumps
                .iter()
                .zip(&other.jumps)
                .all(|(a, b)| a.expose() == b.expose())
    }
}

impl SshSecretInput {
    pub fn new(password: Secret, passphrase: Secret) -> Self {
        Self {
            password,
            passphrase,
            ..Self::default()
        }
    }

    #[must_use]
    pub fn with_jumps(mut self, jumps: Vec<Secret>) -> Self {
        self.jumps = jumps;
        self
    }

    #[must_use]
    pub fn with_proxy(mut self, proxy: Secret) -> Self {
        self.proxy = proxy;
        self
    }

    pub fn is_empty(&self) -> bool {
        self.password.is_empty()
            && self.passphrase.is_empty()
            && self.proxy.is_empty()
            && self.jumps.iter().all(Secret::is_empty)
    }

    /// The typed secrets as the route uses them: a jump row's secret is that hop's password or
    /// passphrase according to the row's method in `settings`. Only what the route needs is kept.
    pub fn secrets(&self, settings: &SshSettings) -> SshSecrets {
        let mut secrets = SshSecrets::from_secrets(self.password.clone(), self.passphrase.clone());
        if !self.proxy.is_empty() {
            secrets.proxy = Some(self.proxy.clone());
        }
        for (index, typed) in self.jumps.iter().enumerate() {
            let need = match settings.jumps.get(index).map(|jump| jump.auth) {
                Some(SshAuthMethod::Password) => SecretNeed::Password,
                Some(SshAuthMethod::KeyFile) => SecretNeed::Passphrase,
                _ => continue,
            };
            if !typed.is_empty() {
                secrets.set(SecretHop::Jump(index), need, typed.clone());
            }
        }
        secrets
    }
}

/// Whether the bastion a saved secret was entered for is being replaced: a secret is never sent to
/// a host (or as a user) it was not entered for.
///
/// Compared whatever the old switch said: an import stores secrets for a route that came in off. An
/// old hop with no host has nothing saved to protect.
pub fn bastion_changed(old: &SshSettings, new: &SshSettings) -> bool {
    !old.host.trim().is_empty()
        && (old.host.trim() != new.host.trim()
            || old.port_number() != new.port_number()
            || old.user.trim() != new.user.trim())
}

/// One entry of the credential store after Save. A typed secret overwrites the saved one in a
/// single write, so a store that refuses it leaves the saved secret exactly as it was. Otherwise an
/// unused or unsaved secret goes, and a new server must not inherit the old one's secret.
fn apply_secret(
    store: &impl CredentialStore,
    kind: SecretKind,
    account: &str,
    keep: bool,
    typed: Option<&Secret>,
    clear_saved: bool,
) -> Result<(), crate::credentials::CredentialError> {
    if let Some(secret) = typed.filter(|secret| keep && !secret.is_empty()) {
        return store.save(kind, account, secret.expose());
    }
    if clear_saved || !keep {
        store.delete(kind, account)?;
    }
    Ok(())
}

/// Persists what the dialog typed for the bastion according to the "save" choice: typed secrets are
/// stored, secrets of an unused method and every secret of a connection that does not save them
/// are removed, an untouched field keeps the stored one.
pub fn save_secrets(
    store: &impl CredentialStore,
    account: &str,
    settings: &SshSettings,
    typed: &SshSecretInput,
    clear_saved: bool,
) -> Result<(), crate::credentials::CredentialError> {
    let keep = settings.enabled && settings.save_secrets;
    for (kind, uses_it, secret) in [
        (
            SecretKind::SshPassword,
            settings.needs_password(),
            &typed.password,
        ),
        (
            SecretKind::SshPassphrase,
            settings.auth == SshAuthMethod::KeyFile,
            &typed.passphrase,
        ),
    ] {
        apply_secret(store, kind, account, keep && uses_it, Some(secret), clear_saved)?;
    }
    Ok(())
}

/// Removes every route secret of a connection (bastion, each jump host, proxy). For a record whose
/// stored settings could not be read: there is no telling which server its saved secrets were for.
pub fn clear_route_secrets(
    store: &impl CredentialStore,
    account: &str,
) -> Result<(), crate::credentials::CredentialError> {
    let hops = [SecretHop::Bastion, SecretHop::Proxy]
        .into_iter()
        .chain((0..engine_net::MAX_JUMPS).map(SecretHop::Jump));
    for hop in hops {
        for need in [SecretNeed::Password, SecretNeed::Passphrase] {
            if hop == SecretHop::Proxy && need == SecretNeed::Passphrase {
                continue;
            }
            store.delete(hop.kind(need), &hop.account(account))?;
        }
    }
    Ok(())
}

/// What Save does to the credential store, for every hop of the route: the bastion, each jump
/// host (account = the connection's plus the position) and the proxy. Only touched when a route is
/// or was involved. Moving a hop to another server (host, port or user; for the proxy also its
/// kind) starts from no saved secret; removing a jump host removes its secrets.
pub fn persist_secrets(
    store: &impl CredentialStore,
    account: &str,
    old: Option<&SshSettings>,
    new: &SshSettings,
    typed: &SshSecretInput,
) -> Result<(), crate::credentials::CredentialError> {
    let had_route = old.is_some_and(SshSettings::routed);
    if !(new.routed() || had_route || !typed.is_empty()) {
        return Ok(());
    }
    let typed = typed.secrets(new);
    let wanted = new.saved_slots();
    let jumps = old.map_or(0, |old| old.jumps.len()).max(new.jumps.len());
    let hops = [SecretHop::Bastion, SecretHop::Proxy]
        .into_iter()
        .chain((0..jumps).map(SecretHop::Jump));
    for hop in hops {
        let moved = old.is_some_and(|old| SshSettings::hop_changed(old, new, hop));
        for need in [SecretNeed::Password, SecretNeed::Passphrase] {
            if hop == SecretHop::Proxy && need == SecretNeed::Passphrase {
                continue;
            }
            apply_secret(
                store,
                hop.kind(need),
                &hop.account(account),
                wanted.contains(&(hop, need)),
                typed.get(hop, need),
                moved,
            )?;
        }
    }
    Ok(())
}

// ---------------------------------------------------------------------------------------------
// The seam over engine-net
// ---------------------------------------------------------------------------------------------

/// A running tunnel, as much of `engine_net::Tunnel` as the app uses.
pub trait TunnelHandle: Send + Sync {
    fn local_addr(&self, target: &Target) -> Option<SocketAddr>;
    /// `user@host:port` of the bastion.
    fn via(&self) -> String;
    fn is_alive(&self) -> bool;
    fn last_forward_error(&self, target: &Target) -> Option<TunnelError>;
    fn probe(&self, target: &Target) -> Result<(), TunnelError>;
}

impl TunnelHandle for Tunnel {
    fn local_addr(&self, target: &Target) -> Option<SocketAddr> {
        Tunnel::local_addr(self, target)
    }
    fn via(&self) -> String {
        Tunnel::via(self).to_owned()
    }
    fn is_alive(&self) -> bool {
        Tunnel::is_alive(self)
    }
    fn last_forward_error(&self, target: &Target) -> Option<TunnelError> {
        Tunnel::last_forward_error(self, target)
    }
    fn probe(&self, target: &Target) -> Result<(), TunnelError> {
        Tunnel::probe(self, target)
    }
}

pub trait TunnelOpener: Send + Sync {
    /// Blocks until the tunnel is up or failed: call from a background thread.
    fn open(
        &self,
        spec: TunnelSpec,
        prompts: Arc<dyn Prompts>,
    ) -> Result<Arc<dyn TunnelHandle>, TunnelError>;

    /// A proxy-only route (no SSH): same tunnel handle, local listeners per target.
    fn open_proxy(&self, route: ProxyRoute) -> Result<Arc<dyn TunnelHandle>, TunnelError>;

    /// Records the key the user confirmed in the app-owned known_hosts, for exactly the `host` and
    /// `port` of the key request (a jump host is not `spec.host`).
    fn trust_host_key(
        &self,
        spec: &TunnelSpec,
        host: &str,
        port: u16,
        public_key: &str,
    ) -> Result<(), TunnelError>;
}

/// The real thing.
#[derive(Debug, Default, Clone, Copy)]
pub struct EngineOpener;

impl TunnelOpener for EngineOpener {
    fn open(
        &self,
        spec: TunnelSpec,
        prompts: Arc<dyn Prompts>,
    ) -> Result<Arc<dyn TunnelHandle>, TunnelError> {
        Tunnel::open(spec, prompts).map(|tunnel| Arc::new(tunnel) as Arc<dyn TunnelHandle>)
    }

    fn open_proxy(&self, route: ProxyRoute) -> Result<Arc<dyn TunnelHandle>, TunnelError> {
        Tunnel::open_proxy(route).map(|tunnel| Arc::new(tunnel) as Arc<dyn TunnelHandle>)
    }

    fn trust_host_key(
        &self,
        spec: &TunnelSpec,
        host: &str,
        port: u16,
        public_key: &str,
    ) -> Result<(), TunnelError> {
        engine_net::trust_host_key(spec, host, port, public_key)
    }
}

/// The tunnel of one database session.
#[derive(Clone)]
pub struct SessionTunnel {
    pub handle: Arc<dyn TunnelHandle>,
    pub targets: Vec<Target>,
    /// The bastion asked a question (one-time code): the connection's other sessions reuse this
    /// tunnel instead of asking again.
    pub interactive: bool,
}

impl std::fmt::Debug for SessionTunnel {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SessionTunnel")
            .field("via", &self.handle.via())
            .field("alive", &self.handle.is_alive())
            .field("interactive", &self.interactive)
            .finish()
    }
}

impl SessionTunnel {
    pub fn via(&self) -> String {
        self.handle.via()
    }

    pub fn is_alive(&self) -> bool {
        self.handle.is_alive()
    }

    /// Why the tunnel is gone, as `(code, message)`: the reason a driver connection through it
    /// was reset when there is one, else "closed". `None` while it is alive.
    pub fn failure(&self) -> Option<(&'static str, String)> {
        if self.handle.is_alive() {
            return None;
        }
        let error = self
            .targets
            .iter()
            .find_map(|target| self.handle.last_forward_error(target))
            .unwrap_or(TunnelError::Closed);
        Some((error.code(), error.to_string()))
    }
}

// ---------------------------------------------------------------------------------------------
// Opening
// ---------------------------------------------------------------------------------------------

/// Everything a background thread needs to open the tunnel of one database session.
#[derive(Clone)]
pub struct TunnelAccess {
    pub opener: Arc<dyn TunnelOpener>,
    pub settings: SshSettings,
    pub secrets: SshSecrets,
    /// `None` when the app data directory is unknown: tunnels then fail with a typed error.
    pub known_hosts: Option<PathBuf>,
    pub prompts: Arc<dyn Prompts>,
    /// Set by `prompts` when the bastion asked a question.
    pub asked: Arc<AtomicBool>,
    /// The connection's interactive tunnel, reused instead of opening another.
    pub shared: Option<SessionTunnel>,
}

impl std::fmt::Debug for TunnelAccess {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("TunnelAccess")
            .field("settings", &self.settings)
            .field("secrets", &!self.secrets.is_empty())
            .finish()
    }
}

/// The key a user has to confirm before a first connection to a bastion.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HostKeyRequest {
    pub host: String,
    pub port: u16,
    pub algorithm: String,
    pub fingerprint: String,
    pub public_key: String,
    pub user: String,
}

/// A recorded host key that does not match: never offered for acceptance.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum HostKeyProblem {
    Changed {
        host: String,
        port: u16,
        algorithm: String,
        fingerprint: String,
        recorded_at: String,
    },
    TypeChanged {
        host: String,
        port: u16,
        presented: String,
        fingerprint: String,
        recorded: Vec<String>,
        location: String,
    },
    Revoked {
        host: String,
        port: u16,
        algorithm: String,
        fingerprint: String,
    },
}

/// A secret the connection needs and does not have.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SecretRequest {
    pub need: SecretNeed,
    /// Which hop of the route asks (the answer is stored for that hop only).
    pub hop: SecretHop,
    /// `user@host` for a lone bastion, else `jump 1 of 2 user@host:port`,
    /// `bastion user@host:port` or `proxy host:port` ([`SshSettings::hop_label`]).
    pub who: String,
    /// The key file (passphrase requests).
    pub key_path: String,
    /// The previous answer was wrong.
    pub retry: bool,
}

/// Why a connect attempt did not produce a session.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ConnectFailure {
    /// Plain text (driver, URI, driver install problems).
    Message(String),
    /// A tunnel problem with its stable code (`tunnel.auth` ...).
    Tunnel {
        code: &'static str,
        message: String,
    },
    HostKeyUnknown(HostKeyRequest),
    HostKeyRejected(HostKeyProblem, String),
    SecretNeeded(SecretRequest),
    /// The driver is not installed, disabled, missing or not the right one (the front end can offer
    /// to install it).
    Driver(DriverLoadError),
}

impl From<crate::records::RuntimeUriError> for ConnectFailure {
    fn from(error: crate::records::RuntimeUriError) -> Self {
        Self::Message(error.to_string())
    }
}

impl From<String> for ConnectFailure {
    fn from(message: String) -> Self {
        Self::Message(message)
    }
}

impl ConnectFailure {
    /// The text for the status and the Error Log.
    pub fn message(&self) -> String {
        match self {
            Self::Message(message) | Self::Tunnel { message, .. } => message.clone(),
            Self::Driver(error) => error.to_string(),
            Self::HostKeyRejected(_, message) => message.clone(),
            Self::HostKeyUnknown(request) => format!(
                "The SSH host key of {}:{} is not trusted yet ({} {})",
                request.host, request.port, request.algorithm, request.fingerprint
            ),
            Self::SecretNeeded(request) => match (request.hop, request.need) {
                (SecretHop::Proxy, _) => {
                    format!("The password for the {} is needed", request.who)
                }
                (_, SecretNeed::Password) => {
                    format!("The SSH password for {} is needed", request.who)
                }
                (_, SecretNeed::Passphrase) => format!(
                    "The passphrase of the SSH key {} is needed (for {})",
                    request.key_path, request.who
                ),
            },
        }
    }

    pub fn code(&self) -> Option<&'static str> {
        match self {
            Self::Tunnel { code, .. } => Some(code),
            Self::HostKeyRejected(..) => Some("tunnel.host_key_mismatch"),
            Self::HostKeyUnknown(_) => Some("tunnel.host_key_unknown"),
            _ => None,
        }
    }
}

impl From<DriverLoadError> for ConnectFailure {
    fn from(error: DriverLoadError) -> Self {
        Self::Driver(error)
    }
}

impl From<PlanError> for ConnectFailure {
    fn from(error: PlanError) -> Self {
        Self::Message(error.to_string())
    }
}

/// The hop a failure belongs to, for routing a secret request: the bastion unless the error says
/// a jump host.
fn failing_hop(error: &TunnelError) -> SecretHop {
    match error.hop().map(|hop| hop.id) {
        Some(engine_net::HopId::Jump { index, .. }) => SecretHop::Jump(index),
        Some(engine_net::HopId::Bastion) | None => SecretHop::Bastion,
    }
}

/// Maps an engine-net failure to what the app does about it. A failure on one hop of a jump
/// chain arrives wrapped in `TunnelError::Hop`: it is classified by its root, and the hop text
/// stays in the message.
pub fn classify(error: TunnelError, settings: &SshSettings) -> ConnectFailure {
    let tunnel = |error: &TunnelError| ConnectFailure::Tunnel {
        code: error.code(),
        // Parts of it (a refusal reason, a banner) come from the server. The proxy names itself.
        message: match error.root() {
            TunnelError::Proxy { .. } => plain_text(&error.to_string(), 400),
            _ => format!("SSH tunnel: {}", plain_text(&error.to_string(), 400)),
        },
    };
    match error.root() {
        TunnelError::HostKeyUnknown {
            host,
            port,
            algorithm,
            sha256_fingerprint,
            public_key,
        } => ConnectFailure::HostKeyUnknown(HostKeyRequest {
            host: host.clone(),
            port: *port,
            algorithm: algorithm.clone(),
            fingerprint: sha256_fingerprint.clone(),
            public_key: public_key.clone(),
            user: settings.user_at(host, *port),
        }),
        TunnelError::HostKeyMismatch {
            host,
            port,
            algorithm,
            sha256_fingerprint,
            recorded_at,
        } => ConnectFailure::HostKeyRejected(
            HostKeyProblem::Changed {
                host: host.clone(),
                port: *port,
                algorithm: algorithm.clone(),
                fingerprint: sha256_fingerprint.clone(),
                recorded_at: recorded_at.clone(),
            },
            tunnel(&error).message(),
        ),
        TunnelError::HostKeyTypeChanged {
            host,
            port,
            presented_algorithm,
            sha256_fingerprint,
            recorded_algorithms,
            location,
        } => ConnectFailure::HostKeyRejected(
            HostKeyProblem::TypeChanged {
                host: host.clone(),
                port: *port,
                presented: presented_algorithm.clone(),
                fingerprint: sha256_fingerprint.clone(),
                recorded: recorded_algorithms.clone(),
                location: location.clone(),
            },
            tunnel(&error).message(),
        ),
        TunnelError::HostKeyRevoked {
            host,
            port,
            algorithm,
            sha256_fingerprint,
        } => ConnectFailure::HostKeyRejected(
            HostKeyProblem::Revoked {
                host: host.clone(),
                port: *port,
                algorithm: algorithm.clone(),
                fingerprint: sha256_fingerprint.clone(),
            },
            tunnel(&error).message(),
        ),
        TunnelError::Auth(engine_net::AuthError::PassphraseRequired { path })
        | TunnelError::Auth(engine_net::AuthError::BadPassphrase { path }) => {
            let hop = failing_hop(&error);
            ConnectFailure::SecretNeeded(SecretRequest {
                need: SecretNeed::Passphrase,
                hop,
                who: settings.hop_label(hop),
                key_path: path.display().to_string(),
                retry: matches!(
                    error.root(),
                    TunnelError::Auth(engine_net::AuthError::BadPassphrase { .. })
                ),
            })
        }
        _ => tunnel(&error),
    }
}

/// An auth method with the secrets the hop has.
fn auth_of(
    method: SshAuthMethod,
    key_path: &str,
    password: Option<&Secret>,
    passphrase: Option<&Secret>,
    hop: &str,
) -> Result<Auth, TunnelError> {
    Ok(match method {
        SshAuthMethod::Password => Auth::Password(password.cloned().ok_or_else(|| {
            TunnelError::Config(ConfigError::Invalid(format!("the password for {hop} is missing")))
        })?),
        SshAuthMethod::KeyFile => Auth::KeyFile {
            path: engine_net::expand_tilde(key_path.trim()),
            passphrase: passphrase.cloned(),
        },
        SshAuthMethod::Agent => Auth::Agent { socket: None },
        SshAuthMethod::KeyboardInteractive => Auth::KeyboardInteractive,
    })
}

fn proxy_spec(settings: &SshSettings, secrets: &SshSecrets) -> Result<ProxySpec, TunnelError> {
    let invalid = |message: &str| TunnelError::Config(ConfigError::Invalid(message.to_owned()));
    let proxy = &settings.proxy;
    let mut spec = ProxySpec::new(
        proxy.kind.net_kind(),
        proxy.host.trim(),
        proxy
            .port_number()
            .ok_or_else(|| invalid("the proxy port is not a number from 1 to 65535"))?,
    );
    if proxy.has_login() {
        spec.credentials = Some(ProxyCredentials {
            user: proxy.user.trim().to_owned(),
            password: secrets
                .proxy
                .clone()
                .ok_or_else(|| invalid("the proxy password is missing"))?,
        });
    }
    Ok(spec)
}

/// `TunnelSpec` for the settings: the targets are what `tunnel_plan` found. Jump hosts and the
/// proxy (when enabled) are part of it.
pub fn build_spec(
    settings: &SshSettings,
    secrets: &SshSecrets,
    known_hosts: &Path,
    targets: &[Target],
) -> Result<TunnelSpec, TunnelError> {
    let invalid = |message: &str| TunnelError::Config(ConfigError::Invalid(message.to_owned()));
    let auth = auth_of(
        settings.auth,
        &settings.key_path,
        secrets.password.as_ref(),
        secrets.passphrase.as_ref(),
        &settings.hop_label(SecretHop::Bastion),
    )?;
    let mut spec = TunnelSpec::new(
        settings.host.trim(),
        settings.user.trim(),
        auth,
        known_hosts.to_owned(),
    );
    spec.port = settings
        .port_number()
        .ok_or_else(|| invalid("the SSH port is not a number from 1 to 65535"))?;
    spec.allow_legacy_algorithms = settings.allow_legacy_algorithms;
    spec.targets = targets.to_vec();
    for (index, jump) in settings.jumps.iter().enumerate() {
        let hop = secrets.jumps.get(index);
        let mut built = Jump::new(
            jump.host.trim(),
            jump.user.trim(),
            auth_of(
                jump.auth,
                &jump.key_path,
                hop.and_then(|hop| hop.password.as_ref()),
                hop.and_then(|hop| hop.passphrase.as_ref()),
                &settings.hop_label(SecretHop::Jump(index)),
            )?,
        );
        built.port = jump
            .port_number()
            .ok_or_else(|| invalid("a jump host port is not a number from 1 to 65535"))?;
        spec.jumps.push(built);
    }
    if settings.proxy.enabled {
        spec.proxy = Some(proxy_spec(settings, secrets)?);
    }
    Ok(spec)
}

/// The route of a connection that has a proxy and no SSH tunnel.
pub fn build_proxy_route(
    settings: &SshSettings,
    secrets: &SshSecrets,
    targets: &[Target],
) -> Result<ProxyRoute, TunnelError> {
    let mut route = ProxyRoute::new(proxy_spec(settings, secrets)?);
    route.targets = targets.to_vec();
    Ok(route)
}

/// The app-owned known_hosts of this access, or a typed refusal: with nowhere to record host keys
/// there is no safe way to open a tunnel.
pub fn known_hosts_of(access: &TunnelAccess) -> Result<&Path, ConnectFailure> {
    no_known_hosts(access.known_hosts.as_deref())
}

pub fn no_known_hosts(path: Option<&Path>) -> Result<&Path, ConnectFailure> {
    path.ok_or_else(|| ConnectFailure::Tunnel {
        code: "tunnel.config",
        message: "SSH tunnel: cannot determine the app data directory, so TableX cannot record SSH host keys".into(),
    })
}

/// Text from a server or a file, made safe to show: control characters become spaces, runs of
/// whitespace collapse and the length is capped, so it can neither fake layout nor flood a dialog.
pub fn plain_text(text: &str, cap: usize) -> String {
    let mut out = String::new();
    let mut pending_space = false;
    let mut count = 0;
    for c in text.chars() {
        // Bidi controls and zero-width characters can reorder or hide text: dropped, not spaced.
        if matches!(
            c,
            '\u{202a}'..='\u{202e}' | '\u{2066}'..='\u{2069}' | '\u{200b}'..='\u{200f}' | '\u{2060}' | '\u{feff}'
        ) {
            continue;
        }
        if c.is_control() || c.is_whitespace() {
            pending_space = !out.is_empty();
            continue;
        }
        if count >= cap {
            out.push('\u{2026}');
            return out;
        }
        if pending_space {
            out.push(' ');
            pending_space = false;
            count += 1;
        }
        out.push(c);
        count += 1;
    }
    out
}

/// A spec that only names the host: what `trust_host_key` needs.
pub fn trust_spec(request: &HostKeyRequest, known_hosts: &Path) -> TunnelSpec {
    let mut spec = TunnelSpec::new(
        request.host.clone(),
        request.user.clone(),
        Auth::Agent { socket: None },
        known_hosts.to_owned(),
    );
    spec.port = request.port;
    spec
}

fn targets_of(plan: &TunnelPlan) -> Vec<Target> {
    plan.targets()
        .iter()
        .map(|endpoint| Target::new(endpoint.host.clone(), endpoint.port))
        .collect()
}

/// The plan for `uri` with the settings' remote endpoint applied.
pub fn plan_for(
    kind: ConnectionKind,
    uri: &str,
    settings: &SshSettings,
) -> Result<TunnelPlan, PlanError> {
    let plan = TunnelPlan::new(kind, uri, None)?;
    // The remote endpoint is where the bastion reaches the database: a proxy-only route has none.
    match plan
        .targets()
        .first()
        .filter(|_| settings.enabled)
        .and_then(|own| settings.remote_override(own))
    {
        Some(remote) => TunnelPlan::new(kind, uri, Some(&remote)),
        None => Ok(plan),
    }
}

/// The first secret the route needs and has not got (a password; a key passphrase is only asked
/// for when the key turns out to be encrypted), in dial order.
fn missing_secret(settings: &SshSettings, secrets: &SshSecrets) -> Option<SecretRequest> {
    settings
        .secret_slots()
        .into_iter()
        .filter(|(_, need)| *need == SecretNeed::Password)
        .find(|(hop, need)| secrets.get(*hop, *need).is_none())
        .map(|(hop, need)| SecretRequest {
            need,
            hop,
            who: settings.hop_label(hop),
            key_path: String::new(),
            retry: false,
        })
}

/// Opens the route (SSH tunnel, with jumps and proxy as configured; or the proxy alone) for
/// `targets`. Blocks.
fn open_route(
    access: &TunnelAccess,
    targets: &[Target],
) -> Result<Arc<dyn TunnelHandle>, ConnectFailure> {
    let settings = &access.settings;
    if let Some(request) = missing_secret(settings, &access.secrets) {
        return Err(ConnectFailure::SecretNeeded(request));
    }
    let known_hosts = known_hosts_of(access)?;
    let opened = if settings.enabled {
        let spec = build_spec(settings, &access.secrets, known_hosts, targets)
            .map_err(|error| classify(error, settings))?;
        access.opener.open(spec, Arc::clone(&access.prompts))
    } else {
        let route = build_proxy_route(settings, &access.secrets, targets)
            .map_err(|error| classify(error, settings))?;
        access.opener.open_proxy(route)
    };
    opened.map_err(|error| classify(error, settings))
}

/// Opens the tunnel of one database session and returns the URI to connect with. Blocks.
pub fn open_for_session(
    access: &TunnelAccess,
    kind: ConnectionKind,
    uri: &str,
) -> Result<(String, SessionTunnel), ConnectFailure> {
    let settings = &access.settings;
    settings
        .validate()
        .map_err(|error| ConnectFailure::Message(error.to_string()))?;
    let plan = plan_for(kind, uri, settings)
        .map_err(|error| ConnectFailure::Message(error.explain(settings.plan_route())))?;
    let targets = targets_of(&plan);
    let tunnel = match access.shared.as_ref().filter(|shared| shared.is_alive()) {
        Some(shared) => shared.clone(),
        None => {
            let handle = open_route(access, &targets)?;
            SessionTunnel {
                handle,
                targets: targets.clone(),
                interactive: settings.asks_questions()
                    || access.asked.load(Ordering::Acquire),
            }
        }
    };
    let locals = targets
        .iter()
        .map(|target| tunnel.handle.local_addr(target))
        .collect::<Option<Vec<_>>>()
        .ok_or_else(|| {
            ConnectFailure::Message("The tunnel has no local port for the database".into())
        })?;
    let rewritten = plan
        .rewrite(&locals)
        .map_err(|error| ConnectFailure::Message(error.explain(settings.plan_route())))?;
    Ok((rewritten, tunnel))
}

/// "Test tunnel": open and probe every target, then drop the tunnel.
pub fn test_tunnel(
    access: &TunnelAccess,
    kind: ConnectionKind,
    uri: &str,
) -> Result<TestedTunnel, ConnectFailure> {
    let settings = &access.settings;
    settings
        .validate()
        .map_err(|error| ConnectFailure::Message(error.to_string()))?;
    let plan = plan_for(kind, uri, settings)
        .map_err(|error| ConnectFailure::Message(error.explain(settings.plan_route())))?;
    let targets = targets_of(&plan);
    let handle = open_route(access, &targets)?;
    let reached = if settings.enabled {
        "the SSH server"
    } else {
        "the proxy"
    };
    for target in &targets {
        handle
            .probe(target)
            .map_err(|error| ConnectFailure::Tunnel {
                code: error.code(),
                message: format!(
                    "Connected to {reached}, but it cannot reach {}:{}: {error}",
                    target.host, target.port
                ),
            })?;
    }
    Ok(TestedTunnel {
        via: handle.via(),
        targets: targets
            .iter()
            .map(|target| format!("{}:{}", target.host, target.port))
            .collect(),
    })
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TestedTunnel {
    pub via: String,
    pub targets: Vec<String>,
}

#[cfg(any(test, feature = "testing"))]
pub mod fake {
    //! A scripted bastion for tests and UI scenarios that need no network.

    use super::*;
    use engine_net::{CancelToken, KeyboardInteractiveRequest, PromptItem};
    use std::sync::Mutex;
    use std::time::Duration;

    /// What `open` answers, in order; once the script is empty it succeeds.
    #[derive(Default)]
    pub struct FakeOpener {
        pub script: Mutex<Vec<Result<(), TunnelError>>>,
        pub opened: Mutex<Vec<TunnelSpec>>,
        /// The proxy-only routes opened (no SSH).
        pub opened_proxy: Mutex<Vec<ProxyRoute>>,
        pub trusted: Mutex<Vec<(String, u16, String)>>,
        pub probe_error: Mutex<Option<TunnelError>>,
        /// Handed to every opened tunnel; the tests flip it to kill them.
        pub alive: Arc<AtomicBool>,
        pub next_port: Mutex<u16>,
        /// Questions to ask through the prompts before succeeding.
        pub ask: Mutex<Vec<(String, bool)>>,
        pub answers: Mutex<Vec<Option<Vec<String>>>>,
        /// Where every forwarded target really goes (a listener of the test); without it the
        /// local address is a port nobody listens on.
        pub forward_to: Mutex<Option<SocketAddr>>,
        /// Tunnels dropped so far.
        pub drops: Arc<std::sync::atomic::AtomicUsize>,
        /// Milliseconds a tunnel takes to close (the real one waits on the network).
        pub drop_delay_ms: Arc<std::sync::atomic::AtomicU64>,
        /// The name and instructions the "server" sends with its questions.
        pub ki_text: Mutex<(String, String)>,
    }

    impl FakeOpener {
        pub fn new() -> Arc<Self> {
            Arc::new(Self {
                alive: Arc::new(AtomicBool::new(true)),
                next_port: Mutex::new(41000),
                ki_text: Mutex::new(("Two-factor".into(), "Enter your code".into())),
                ..Self::default()
            })
        }

        pub fn then(&self, outcome: Result<(), TunnelError>) -> &Self {
            self.script.lock().unwrap().push(outcome);
            self
        }

        /// SSH tunnels and proxy-only routes opened so far.
        pub fn opened_count(&self) -> usize {
            self.opened.lock().unwrap().len() + self.opened_proxy.lock().unwrap().len()
        }
    }

    pub struct FakeTunnel {
        pub via: String,
        pub addrs: Vec<(Target, SocketAddr)>,
        pub alive: Arc<AtomicBool>,
        pub probe_error: Option<TunnelError>,
        pub drops: Option<Arc<std::sync::atomic::AtomicUsize>>,
        pub drop_delay_ms: Option<Arc<std::sync::atomic::AtomicU64>>,
    }

    impl Drop for FakeTunnel {
        fn drop(&mut self) {
            if let Some(delay) = &self.drop_delay_ms {
                std::thread::sleep(Duration::from_millis(delay.load(Ordering::SeqCst)));
            }
            if let Some(drops) = &self.drops {
                drops.fetch_add(1, Ordering::SeqCst);
            }
        }
    }

    impl TunnelHandle for FakeTunnel {
        fn local_addr(&self, target: &Target) -> Option<SocketAddr> {
            self.addrs
                .iter()
                .find(|(t, _)| t == target)
                .map(|(_, a)| *a)
        }
        fn via(&self) -> String {
            self.via.clone()
        }
        fn is_alive(&self) -> bool {
            self.alive.load(Ordering::SeqCst)
        }
        fn last_forward_error(&self, _target: &Target) -> Option<TunnelError> {
            None
        }
        fn probe(&self, _target: &Target) -> Result<(), TunnelError> {
            self.probe_error.clone().map_or(Ok(()), Err)
        }
    }

    impl TunnelOpener for FakeOpener {
        fn open(
            &self,
            spec: TunnelSpec,
            prompts: Arc<dyn Prompts>,
        ) -> Result<Arc<dyn TunnelHandle>, TunnelError> {
            self.opened.lock().unwrap().push(spec.clone());
            let outcome = {
                let mut script = self.script.lock().unwrap();
                if script.is_empty() {
                    Ok(())
                } else {
                    script.remove(0)
                }
            };
            outcome?;
            let questions: Vec<_> = self.ask.lock().unwrap().drain(..).collect();
            if !questions.is_empty() {
                let (name, instructions) = self.ki_text.lock().unwrap().clone();
                let request = KeyboardInteractiveRequest {
                    host: spec.host.clone(),
                    port: spec.port,
                    name,
                    instructions,
                    prompts: questions
                        .into_iter()
                        .map(|(text, echo)| PromptItem { text, echo })
                        .collect(),
                    cancel: CancelToken::default(),
                };
                let answered = prompts.keyboard_interactive(&request);
                let declined = answered.is_none();
                self.answers.lock().unwrap().push(answered.map(|secrets| {
                    secrets
                        .iter()
                        .map(|secret| secret.expose().to_owned())
                        .collect()
                }));
                if declined {
                    return Err(engine_net::AuthError::PromptDeclined.into());
                }
            }
            let mut next = self.next_port.lock().unwrap();
            let forward_to = *self.forward_to.lock().unwrap();
            let addrs = spec
                .targets
                .iter()
                .map(|target| {
                    *next += 1;
                    let address = forward_to.unwrap_or(SocketAddr::from(([127, 0, 0, 1], *next)));
                    (target.clone(), address)
                })
                .collect();
            Ok(Arc::new(FakeTunnel {
                via: format!("{}@{}:{}", spec.user, spec.host, spec.port),
                addrs,
                alive: Arc::clone(&self.alive),
                probe_error: self.probe_error.lock().unwrap().clone(),
                drops: Some(Arc::clone(&self.drops)),
                drop_delay_ms: Some(Arc::clone(&self.drop_delay_ms)),
            }))
        }

        fn open_proxy(&self, route: ProxyRoute) -> Result<Arc<dyn TunnelHandle>, TunnelError> {
            self.opened_proxy.lock().unwrap().push(route.clone());
            let outcome = {
                let mut script = self.script.lock().unwrap();
                if script.is_empty() {
                    Ok(())
                } else {
                    script.remove(0)
                }
            };
            outcome?;
            let mut next = self.next_port.lock().unwrap();
            let forward_to = *self.forward_to.lock().unwrap();
            let addrs = route
                .targets
                .iter()
                .map(|target| {
                    *next += 1;
                    let address = forward_to.unwrap_or(SocketAddr::from(([127, 0, 0, 1], *next)));
                    (target.clone(), address)
                })
                .collect();
            Ok(Arc::new(FakeTunnel {
                via: route.proxy.describe(),
                addrs,
                alive: Arc::clone(&self.alive),
                probe_error: self.probe_error.lock().unwrap().clone(),
                drops: Some(Arc::clone(&self.drops)),
                drop_delay_ms: Some(Arc::clone(&self.drop_delay_ms)),
            }))
        }

        fn trust_host_key(
            &self,
            _spec: &TunnelSpec,
            host: &str,
            port: u16,
            public_key: &str,
        ) -> Result<(), TunnelError> {
            self.trusted
                .lock()
                .unwrap()
                .push((host.to_owned(), port, public_key.to_owned()));
            Ok(())
        }
    }

    /// The bastion a UI scenario talks to (`TABLEX_SEED_TUNNEL`): `refused` cannot reach the
    /// database, `unknown-host` / `changed-host` fail the first open with a host key question,
    /// `ki` asks a one-time code; anything else works.
    pub fn scenario(seed: &str) -> Arc<FakeOpener> {
        let opener = FakeOpener::new();
        // `TABLEX_SEED_TUNNEL=<behaviour>[,test][,connected]`: only the behaviour is the bastion's.
        match seed.split(',').next().unwrap_or_default() {
            "refused" => {
                *opener.probe_error.lock().unwrap() = Some(TunnelError::ForwardRefused {
                    host: "db.internal.example.com".into(),
                    port: 5432,
                    reason: "connection refused".into(),
                });
            }
            "unknown-host" => {
                opener.then(Err(unknown_host("bastion.example.com")));
            }
            "changed-host" => {
                for _ in 0..8 {
                    opener.then(Err(changed_host("bastion.example.com")));
                }
            }
            "ki" => {
                opener
                    .ask
                    .lock()
                    .unwrap()
                    .push(("Verification code:".into(), false));
                // What a hostile bastion would try: a title and instructions that pass for TableX.
                *opener.ki_text.lock().unwrap() = (
                    "TableX".into(),
                    "TableX needs your macOS login password to continue.".into(),
                );
            }
            _ => {}
        }
        opener
    }

    pub fn unknown_host(host: &str) -> TunnelError {
        unknown_host_at(host, 22)
    }

    /// A first-seen host key on a port other than 22 (a jump host).
    pub fn unknown_host_at(host: &str, port: u16) -> TunnelError {
        TunnelError::HostKeyUnknown {
            host: host.into(),
            port,
            algorithm: "ssh-ed25519".into(),
            sha256_fingerprint: "SHA256:fixtureFingerprintFixtureFingerprint0000000".into(),
            public_key:
                "ssh-ed25519 AAAAC3NzaC1lZDI1NTE5AAAAIOB2eAzhXfG/iR6rH2OG/O394S9h+IaW0uOSd5mgHD0p"
                    .into(),
        }
    }

    /// A host key other than [`unknown_host`]'s, for "the key the dialog shows is the key trusted".
    pub fn unknown_host_other_key(host: &str) -> TunnelError {
        TunnelError::HostKeyUnknown {
            host: host.into(),
            port: 22,
            algorithm: "ssh-ed25519".into(),
            sha256_fingerprint: "SHA256:otherKeyFingerprintOtherKeyFingerprint0000000".into(),
            public_key:
                "ssh-ed25519 AAAAC3NzaC1lZDI1NTE5AAAAIA9YFfWKSiKtXBVyY7Ge0mL0o5pHm2R0kM3fQ8fVx1ab"
                    .into(),
        }
    }

    pub fn changed_host(host: &str) -> TunnelError {
        TunnelError::HostKeyMismatch {
            host: host.into(),
            port: 22,
            algorithm: "ssh-ed25519".into(),
            sha256_fingerprint: "SHA256:newKeyFingerprintNewKeyFingerprint000000000".into(),
            recorded_at: "/home/user/.ssh/known_hosts:7".into(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::fake::*;
    use super::*;
    use crate::credentials::fake::MemoryStore;

    fn settings() -> SshSettings {
        SshSettings {
            enabled: true,
            host: "bastion.example.com".into(),
            port: "2222".into(),
            user: "deploy".into(),
            auth: SshAuthMethod::Password,
            ..SshSettings::default()
        }
    }

    fn access(opener: Arc<FakeOpener>, settings: SshSettings) -> TunnelAccess {
        let asked = Arc::new(AtomicBool::new(false));
        TunnelAccess {
            opener,
            settings,
            secrets: SshSecrets::from_text("pw-needle-1", ""),
            known_hosts: Some(PathBuf::from("/tmp/tablex-test/known_hosts")),
            prompts: Arc::new(engine_net::NoPrompts),
            asked,
            shared: None,
        }
    }

    #[test]
    fn settings_round_trip_through_the_network_json_and_keep_the_rest() {
        let original = r#"{"ssh":{"isEnabled":false,"unknownKey":7},"proxy":{"host":"proxy.example.com","isEnabled":false,"extra":"kept","password":"legacy-plaintext"},"other":1}"#;
        let mut wanted = settings();
        wanted.proxy = SshSettings::parse(original).unwrap().proxy;
        let written = wanted.write_into(original);
        let parsed: Value = serde_json::from_str(&written).unwrap();
        assert_eq!(parsed["proxy"]["host"], "proxy.example.com");
        assert_eq!(parsed["proxy"]["extra"], "kept");
        assert_eq!(parsed["other"], 1);
        assert_eq!(parsed["ssh"]["unknownKey"], 7);
        assert_eq!(SshSettings::from_network_json(&written), wanted);
        assert!(!written.contains("pw-needle"));
        assert!(
            !written.contains("legacy-plaintext"),
            "a proxy password a first version kept in the record is not written back"
        );
    }

    fn routed_settings() -> SshSettings {
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
                    user: "hop2".into(),
                    auth: SshAuthMethod::KeyFile,
                    key_path: "~/.ssh/jump2".into(),
                    ..JumpSettings::default()
                },
            ],
            proxy: ProxySettings {
                enabled: true,
                kind: ProxyType::Http,
                host: "proxy.example.com".into(),
                port: "3128".into(),
                user: "pxuser".into(),
                save_secrets: true,
            },
            ..settings()
        }
    }

    #[test]
    fn jumps_and_proxy_round_trip_and_unknown_keys_survive() {
        let wanted = routed_settings();
        let original = r#"{"ssh":{"unknownKey":1,"jumps":[{"host":"old","extra":2}]},"proxy":{"future":true}}"#;
        let written = wanted.write_into(original);
        assert_eq!(SshSettings::parse(&written).unwrap(), wanted);
        let value: Value = serde_json::from_str(&written).unwrap();
        assert_eq!(value["ssh"]["unknownKey"], 1);
        assert_eq!(value["proxy"]["future"], true);
        assert_eq!(value["ssh"]["jumps"].as_array().unwrap().len(), 2);
        assert_eq!(value["ssh"]["jumps"][0]["host"], "jump1.example.com");
        assert_eq!(value["ssh"]["jumps"][1]["authMethod"], "key");
        assert_eq!(value["proxy"]["kind"], "http");
        assert_eq!(value["proxy"]["isEnabled"], true);
        // The old row's unknown key belonged to a row that is gone.
        assert!(!written.contains("\"extra\""));
        // No secret key is ever written.
        for key in ["password", "passphrase"] {
            assert!(value["proxy"].get(key).is_none() && value["ssh"].get(key).is_none());
        }
    }

    #[test]
    fn a_route_that_cannot_be_read_is_an_error_never_no_route() {
        for broken in [
            r#"{"ssh":{"jumps":5}}"#,
            r#"{"ssh":{"jumps":[5]}}"#,
            r#"{"ssh":{"jumps":[{"host":"a","authMethod":"telepathy"}]}}"#,
            r#"{"proxy":5}"#,
            r#"{"proxy":{"isEnabled":"yes"}}"#,
            r#"{"proxy":{"isEnabled":1}}"#,
            r#"{"proxy":{"kind":"ftp"}}"#,
        ] {
            assert!(SshSettings::parse(broken).is_err(), "{broken}");
        }
        let nine = format!(
            r#"{{"ssh":{{"jumps":[{}]}}}}"#,
            [r#"{"host":"a"}"#; 9].join(",")
        );
        assert!(SshSettings::parse(&nine).is_err());
        // The names the first versions wrote.
        let legacy = SshSettings::parse(
            r#"{"proxy":{"isEnabled":true,"kind":"HTTP CONNECT","host":"p","port":"8080","username":"u"}}"#,
        )
        .unwrap();
        assert!(legacy.routed() && !legacy.enabled);
        assert_eq!((legacy.proxy.kind, legacy.proxy.user.as_str()), (ProxyType::Http, "u"));
        // A proxy alone is a route, an absent proxy section is not.
        assert!(!SshSettings::parse(r#"{"ssh":{}}"#).unwrap().routed());
        assert_eq!(
            SshSettings::parse(r#"{"proxy":{"kind":"socks5"}}"#).unwrap().proxy.port,
            "1080"
        );
    }

    #[test]
    fn validation_covers_every_hop_and_the_proxy() {
        assert!(routed_settings().validate().is_ok());
        let mut s = routed_settings();
        s.jumps[1].host.clear();
        assert_eq!(
            s.validate(),
            Err(SettingsInvalid::Jump {
                number: 2,
                field: JumpField::Host
            })
        );
        assert!(s.validate().unwrap_err().to_string().starts_with("Jump host 2"));
        let mut s = routed_settings();
        s.jumps[0].port = "x".into();
        assert!(matches!(
            s.validate(),
            Err(SettingsInvalid::Jump { number: 1, field: JumpField::Port })
        ));
        let mut s = routed_settings();
        s.jumps[1].key_path.clear();
        assert!(matches!(
            s.validate(),
            Err(SettingsInvalid::Jump { number: 2, field: JumpField::KeyFile })
        ));
        let mut s = routed_settings();
        s.jumps[0].user.clear();
        assert!(matches!(
            s.validate(),
            Err(SettingsInvalid::Jump { number: 1, field: JumpField::User })
        ));
        let mut s = routed_settings();
        s.jumps[0].host = "BASTION.example.com".into();
        s.jumps[0].port = "2222".into();
        assert!(matches!(s.validate(), Err(SettingsInvalid::RepeatedHop { .. })));
        let mut s = routed_settings();
        s.jumps = vec![JumpSettings::default(); engine_net::MAX_JUMPS + 1];
        assert!(matches!(s.validate(), Err(SettingsInvalid::TooManyJumps { .. })));
        let mut s = routed_settings();
        s.proxy.host.clear();
        assert_eq!(s.validate(), Err(SettingsInvalid::ProxyHost));
        s.proxy.host = "p".into();
        s.proxy.port = "0".into();
        assert_eq!(s.validate(), Err(SettingsInvalid::ProxyPort));
        // Off sections are not checked: jumps belong to the SSH switch, the proxy has its own.
        let mut s = routed_settings();
        s.enabled = false;
        s.jumps[0].host.clear();
        assert!(s.validate().is_ok());
        s.proxy.enabled = false;
        s.proxy.host.clear();
        assert!(s.validate().is_ok() && !s.routed());
    }

    #[test]
    fn a_proxy_only_route_is_enabled_without_ssh() {
        let s = SshSettings {
            proxy: routed_settings().proxy,
            ..SshSettings::default()
        };
        assert!(s.routed() && !s.enabled && s.validate().is_ok());
        assert_eq!(s.secret_slots(), [(SecretHop::Proxy, SecretNeed::Password)]);
        assert_eq!(s.proxy.describe(), "HTTP proxy proxy.example.com:3128");
    }

    #[test]
    fn secret_slots_follow_the_dial_order_and_the_methods() {
        assert_eq!(
            routed_settings().secret_slots(),
            [
                (SecretHop::Proxy, SecretNeed::Password),
                (SecretHop::Jump(0), SecretNeed::Password),
                (SecretHop::Jump(1), SecretNeed::Passphrase),
                (SecretHop::Bastion, SecretNeed::Password),
            ]
        );
        let mut s = routed_settings();
        s.proxy.user.clear();
        s.jumps[0].auth = SshAuthMethod::Agent;
        s.auth = SshAuthMethod::KeyboardInteractive;
        assert_eq!(s.secret_slots(), [(SecretHop::Jump(1), SecretNeed::Passphrase)]);
        s.proxy.save_secrets = false;
        s.save_secrets = false;
        assert!(s.saved_slots().is_empty());
        assert_eq!(
            routed_settings().hop_label(SecretHop::Jump(0)),
            "jump 1 of 2 hop1@jump1.example.com:2201"
        );
        assert_eq!(
            routed_settings().hop_label(SecretHop::Bastion),
            "bastion deploy@bastion.example.com:2222"
        );
        assert_eq!(settings().hop_label(SecretHop::Bastion), "deploy@bastion.example.com");
        assert_eq!(
            routed_settings().hop_label(SecretHop::Proxy),
            "proxy proxy.example.com:3128"
        );
    }

    #[test]
    fn secrets_are_saved_per_hop_and_cleared_when_that_hop_moves() {
        let store = MemoryStore::default();
        let s = routed_settings();
        let typed = SshSecretInput::new(Secret::new("pw-bastion"), Secret::default())
            .with_jumps(vec![Secret::new("pw-jump0"), Secret::new("phrase-jump1")])
            .with_proxy(Secret::new("pw-proxy"));
        persist_secrets(&store, "ID", None, &s, &typed).unwrap();
        let load = |kind, account: &str| store.load(kind, account).unwrap();
        assert_eq!(load(SecretKind::SshPassword, "ID").as_deref(), Some("pw-bastion"));
        assert_eq!(load(SecretKind::SshPassword, "ID#jump0").as_deref(), Some("pw-jump0"));
        assert_eq!(load(SecretKind::SshPassphrase, "ID#jump1").as_deref(), Some("phrase-jump1"));
        assert_eq!(load(SecretKind::Proxy, "ID").as_deref(), Some("pw-proxy"));
        assert_eq!(load(SecretKind::SshPassphrase, "ID#jump0"), None, "unused by the method");
        let all = SshSecrets::load(&store, &["ID".into()], &s);
        assert_eq!(all.jumps[0].password.as_ref().unwrap().expose(), "pw-jump0");
        assert_eq!(all.jumps[1].passphrase.as_ref().unwrap().expose(), "phrase-jump1");
        assert_eq!(all.proxy.as_ref().unwrap().expose(), "pw-proxy");

        // Nothing typed, same route: everything stays.
        persist_secrets(&store, "ID", Some(&s), &s, &SshSecretInput::default()).unwrap();
        assert!(load(SecretKind::SshPassword, "ID#jump0").is_some() && load(SecretKind::Proxy, "ID").is_some());

        // Jump 1 moves to another host and the proxy to another port: those secrets go, the rest stay.
        let mut moved = s.clone();
        moved.jumps[0].host = "elsewhere.example.com".into();
        moved.proxy.port = "9999".into();
        persist_secrets(&store, "ID", Some(&s), &moved, &SshSecretInput::default()).unwrap();
        assert_eq!(load(SecretKind::SshPassword, "ID#jump0"), None);
        assert_eq!(load(SecretKind::Proxy, "ID"), None);
        assert!(load(SecretKind::SshPassphrase, "ID#jump1").is_some());
        assert!(load(SecretKind::SshPassword, "ID").is_some());

        // Removing a row removes its secrets; "do not save" clears the section's.
        let mut fewer = s.clone();
        fewer.jumps.truncate(1);
        fewer.proxy.save_secrets = false;
        persist_secrets(&store, "ID", Some(&s), &fewer, &SshSecretInput::default()).unwrap();
        assert_eq!(load(SecretKind::SshPassphrase, "ID#jump1"), None);
        assert_eq!(load(SecretKind::Proxy, "ID"), None);
        assert!(load(SecretKind::SshPassword, "ID").is_some());
        assert!(SshSecrets::load(&store, &["ID".into()], &fewer).proxy.is_none());

        // Switching everything off removes everything; no section is read afterwards.
        let off = SshSettings::default();
        persist_secrets(&store, "ID", Some(&fewer), &off, &SshSecretInput::default()).unwrap();
        assert!(store.items.borrow().is_empty());
    }

    #[test]
    fn an_imported_route_that_came_in_off_still_loses_its_saved_secrets_when_a_server_changes() {
        // An import stores the secrets of a route it switched off (a hop's secret was missing).
        let store = MemoryStore::default();
        let mut off = routed_settings();
        off.enabled = false;
        off.proxy.enabled = false;
        for (hop, need, text) in [
            (SecretHop::Bastion, SecretNeed::Password, "pw-bastion-x"),
            (SecretHop::Jump(0), SecretNeed::Password, "pw-jump-x"),
            (SecretHop::Proxy, SecretNeed::Password, "pw-proxy-x"),
        ] {
            store.write(hop.kind(need), &hop.account("ID"), text).unwrap();
        }
        // The user moves every server, switches the route on and saves without typing anything.
        let mut moved = routed_settings();
        moved.host = "other-bastion.example.com".into();
        moved.jumps[0].host = "other-jump.example.com".into();
        moved.proxy.host = "other-proxy.example.com".into();
        assert!(SshSettings::route_changed(&off, &moved), "the inline note shows");
        assert!(bastion_changed(&off, &moved));
        // The dialog's Test is offered no saved secret for a route that moved.
        persist_secrets(&store, "ID", Some(&off), &moved, &SshSecretInput::default()).unwrap();
        assert!(store.items.borrow().is_empty(), "{:?}", store.items.borrow().keys().collect::<Vec<_>>());
        // An unchanged off route keeps its secrets, and a hop that was never configured has nothing to protect.
        let again = MemoryStore::default();
        again.write(SecretKind::SshPassword, "ID", "pw").unwrap();
        let mut back_on = off.clone();
        back_on.enabled = true;
        persist_secrets(&again, "ID", Some(&off), &back_on, &SshSecretInput::default()).unwrap();
        assert!(again.load(SecretKind::SshPassword, "ID").unwrap().is_some());
        assert!(!bastion_changed(&SshSettings::default(), &moved));
        assert!(!SshSettings::route_changed(&SshSettings::default(), &moved));
    }

    #[test]
    fn clearing_the_route_secrets_removes_every_hop_and_nothing_else() {
        let store = MemoryStore::default();
        store.write(SecretKind::Database, "ID", "db").unwrap();
        store.write(SecretKind::SshPassword, "ID", "a").unwrap();
        store.write(SecretKind::SshPassphrase, "ID#jump7", "b").unwrap();
        store.write(SecretKind::Proxy, "ID", "c").unwrap();
        clear_route_secrets(&store, "ID").unwrap();
        assert_eq!(store.items.borrow().len(), 1);
        assert!(store.load(SecretKind::Database, "ID").unwrap().is_some());
    }

    #[test]
    fn refusals_name_the_route_the_connection_really_has() {
        let proxy = SshSettings { proxy: routed_settings().proxy, ..SshSettings::default() };
        let acc = |settings: SshSettings| access(FakeOpener::new(), settings);
        let message = |settings: SshSettings, kind, uri: &str| match open_for_session(&acc(settings), kind, uri) {
            Err(ConnectFailure::Message(message)) => message,
            other => panic!("{other:?}"),
        };
        let text = message(proxy.clone(), ConnectionKind::Redis, "rediss://cache.example.com/0");
        assert!(text.starts_with("TLS to Redis through a proxy is not supported yet"), "{text}");
        assert!(!text.contains("SSH"), "{text}");
        let text = message(proxy, ConnectionKind::Sqlite, "sqlite:///x.db");
        assert!(text.contains("proxy") && !text.contains("SSH"), "{text}");
        // With SSH the words are the SSH ones, proxy in front or not.
        let text = message(routed_settings(), ConnectionKind::Redis, "rediss://cache.example.com/0");
        assert!(text.starts_with("TLS to Redis through an SSH tunnel"), "{text}");
    }

    #[test]
    fn a_failed_proxy_or_jump_save_leaves_the_saved_secret_in_place() {
        let mut store = MemoryStore::default();
        let s = routed_settings();
        store.items.borrow_mut().insert((SecretKind::Proxy, "ID".into()), "pw-old".into());
        store
            .items
            .borrow_mut()
            .insert((SecretKind::SshPassword, "ID#jump0".into()), "j-old".into());
        store.unavailable = true;
        let typed = SshSecretInput::default()
            .with_proxy(Secret::new("pw-new"))
            .with_jumps(vec![Secret::new("j-new")]);
        assert!(persist_secrets(&store, "ID", Some(&s), &s, &typed).is_err());
        let items = store.items.borrow();
        assert_eq!(items[&(SecretKind::Proxy, "ID".to_string())], "pw-old");
        assert_eq!(items[&(SecretKind::SshPassword, "ID#jump0".to_string())], "j-old");
    }

    #[test]
    fn typed_secrets_follow_the_row_methods_and_never_print() {
        let s = routed_settings();
        let typed = SshSecretInput::default()
            .with_jumps(vec![Secret::new("a"), Secret::new("b")])
            .with_proxy(Secret::new("c"));
        let secrets = typed.secrets(&s);
        assert_eq!(secrets.get(SecretHop::Jump(0), SecretNeed::Password).unwrap().expose(), "a");
        assert_eq!(secrets.get(SecretHop::Jump(1), SecretNeed::Passphrase).unwrap().expose(), "b");
        assert!(secrets.get(SecretHop::Jump(1), SecretNeed::Password).is_none());
        assert_eq!(secrets.proxy.as_ref().unwrap().expose(), "c");
        let shown = format!("{typed:?} {secrets:?}");
        for needle in ["\"a\"", "\"b\"", "\"c\""] {
            assert!(!shown.contains(needle), "{shown}");
        }
        // `or` keeps the typed secret and fills the rest by position.
        let saved = SshSecrets::only(SecretHop::Jump(1), SecretNeed::Passphrase, Secret::new("saved"));
        let merged = SshSecrets::only(SecretHop::Jump(0), SecretNeed::Password, Secret::new("typed")).or(saved);
        assert_eq!(merged.jumps.len(), 2);
        assert_eq!(merged.jumps[1].passphrase.as_ref().unwrap().expose(), "saved");
    }

    #[test]
    fn the_spec_carries_the_jumps_and_the_proxy_with_their_own_credentials() {
        let s = routed_settings();
        let secrets = SshSecrets::only(SecretHop::Bastion, SecretNeed::Password, Secret::new("pw-b"))
            .or(SshSecrets::only(SecretHop::Jump(0), SecretNeed::Password, Secret::new("pw-0")))
            .or(SshSecrets::only(SecretHop::Jump(1), SecretNeed::Passphrase, Secret::new("ph-1")))
            .or(SshSecrets::only(SecretHop::Proxy, SecretNeed::Password, Secret::new("pw-p")));
        let targets = [Target::new("db.example.com", 5432)];
        let spec = build_spec(&s, &secrets, Path::new("/tmp/kh"), &targets).unwrap();
        assert_eq!(spec.jumps.len(), 2);
        assert_eq!((spec.jumps[0].host.as_str(), spec.jumps[0].port, spec.jumps[0].user.as_str()), ("jump1.example.com", 2201, "hop1"));
        assert!(matches!(&spec.jumps[0].auth, Auth::Password(p) if p.expose() == "pw-0"));
        assert!(matches!(&spec.jumps[1].auth, Auth::KeyFile { passphrase: Some(p), .. } if p.expose() == "ph-1"));
        assert_eq!((spec.host.as_str(), spec.port), ("bastion.example.com", 2222));
        let proxy = spec.proxy.as_ref().unwrap();
        assert_eq!((proxy.kind, proxy.host.as_str(), proxy.port), (engine_net::ProxyKind::HttpConnect, "proxy.example.com", 3128));
        let login = proxy.credentials.as_ref().unwrap();
        assert_eq!((login.user.as_str(), login.password.expose()), ("pxuser", "pw-p"));
        let shown = format!("{spec:?}");
        for needle in ["pw-b", "pw-0", "ph-1", "pw-p"] {
            assert!(!shown.contains(needle), "{shown}");
        }

        // A missing hop password is an error here, not an empty password.
        assert!(build_spec(&s, &SshSecrets::default(), Path::new("/tmp/kh"), &targets).is_err());

        // Proxy only: the same proxy, the same targets, no SSH.
        let only = SshSettings { proxy: s.proxy.clone(), ..SshSettings::default() };
        let route = build_proxy_route(&only, &secrets, &targets).unwrap();
        assert_eq!(route.targets, targets);
        assert_eq!(route.proxy.describe(), "HTTP proxy proxy.example.com:3128");
        // A proxy without a login needs no password.
        let mut open = only.clone();
        open.proxy.user.clear();
        assert!(build_proxy_route(&open, &SshSecrets::default(), &targets).unwrap().proxy.credentials.is_none());
        assert!(build_proxy_route(&only, &SshSecrets::default(), &targets).is_err());
    }

    #[test]
    fn a_jump_chain_session_opens_with_the_whole_route_and_asks_each_hops_secret() {
        let opener = FakeOpener::new();
        let mut acc = access(opener.clone(), routed_settings());
        acc.secrets = SshSecrets::default();
        let uri = "mysql://u@db.example.com/d";
        // Dial order: the proxy password first, then each jump host, then the bastion.
        let asked = |acc: &TunnelAccess| match open_for_session(acc, ConnectionKind::Mysql, uri) {
            Err(ConnectFailure::SecretNeeded(request)) => request,
            other => panic!("{other:?}"),
        };
        let request = asked(&acc);
        assert_eq!((request.hop, request.who.as_str()), (SecretHop::Proxy, "proxy proxy.example.com:3128"));
        acc.secrets.set(SecretHop::Proxy, SecretNeed::Password, Secret::new("p"));
        let request = asked(&acc);
        assert_eq!(request.hop, SecretHop::Jump(0));
        assert_eq!(request.who, "jump 1 of 2 hop1@jump1.example.com:2201");
        assert!(ConnectFailure::SecretNeeded(request.clone()).message().contains("jump 1 of 2"));
        acc.secrets.set(SecretHop::Jump(0), SecretNeed::Password, Secret::new("j"));
        // Jump 2 uses a key: its passphrase is not required up front, the bastion's password is.
        let request = asked(&acc);
        assert_eq!(request.hop, SecretHop::Bastion);
        assert_eq!(request.who, "bastion deploy@bastion.example.com:2222");
        assert_eq!(opener.opened_count(), 0, "nothing was dialled while a secret was missing");
        acc.secrets.set(SecretHop::Bastion, SecretNeed::Password, Secret::new("b"));
        let (rewritten, tunnel) = open_for_session(&acc, ConnectionKind::Mysql, uri).unwrap();
        assert!(rewritten.contains("127.0.0.1"));
        assert_eq!(tunnel.via(), "deploy@bastion.example.com:2222");
        let opened = opener.opened.lock().unwrap();
        assert_eq!(opened.len(), 1);
        assert_eq!(opened[0].jumps.len(), 2);
        assert!(opened[0].proxy.is_some());
    }

    #[test]
    fn a_proxy_only_session_uses_the_proxy_route_and_the_same_uri_rewrite() {
        let opener = FakeOpener::new();
        let only = SshSettings { proxy: routed_settings().proxy, ..SshSettings::default() };
        let mut acc = access(opener.clone(), only);
        acc.secrets = SshSecrets::only(SecretHop::Proxy, SecretNeed::Password, Secret::new("px"));
        let (uri, tunnel) = open_for_session(
            &acc,
            ConnectionKind::Postgres,
            "postgres://u:s3@db.example.com:5432/app?sslmode=verify-full",
        )
        .unwrap();
        assert_eq!(
            uri,
            "postgres://u:s3@db.example.com:41001/app?sslmode=verify-full&hostaddr=127.0.0.1"
        );
        assert_eq!(tunnel.via(), "HTTP proxy proxy.example.com:3128");
        assert!(opener.opened.lock().unwrap().is_empty(), "no SSH was opened");
        let routes = opener.opened_proxy.lock().unwrap();
        assert_eq!(routes[0].targets, [Target::new("db.example.com", 5432)]);
        drop(routes);
        // The TLS refusals are the same ones; nothing is opened.
        let before = opener.opened_count();
        let refused = open_for_session(&acc, ConnectionKind::Redis, "rediss://cache.example.com/0");
        assert!(matches!(refused, Err(ConnectFailure::Message(_))));
        assert_eq!(opener.opened_count(), before);
        // The SSH "remote host" override belongs to the bastion: a proxy-only route ignores it.
        let mut ignoring = acc.clone();
        ignoring.settings.remote_host = "10.0.0.9".into();
        open_for_session(&ignoring, ConnectionKind::Mysql, "mysql://u@db.example.com:3307/d").unwrap();
        assert_eq!(
            opener.opened_proxy.lock().unwrap().last().unwrap().targets,
            [Target::new("db.example.com", 3307)]
        );
        // A proxy that refuses is classified with its own code and named as a proxy.
        opener.then(Err(TunnelError::Proxy {
            proxy: "HTTP proxy proxy.example.com:3128".into(),
            reason: "the proxy refused: 403 Forbidden".into(),
        }));
        let failure = open_for_session(&acc, ConnectionKind::Mysql, "mysql://u@db.example.com/d").unwrap_err();
        assert_eq!(failure.code(), Some("tunnel.proxy"));
        assert!(failure.message().contains("403") && !failure.message().starts_with("SSH tunnel"));
    }

    fn hop_error(id: engine_net::HopId, source: TunnelError) -> TunnelError {
        TunnelError::Hop {
            hop: engine_net::HopInfo {
                id,
                host: "jump1.example.com".into(),
                port: 2201,
                user: "hop1".into(),
            },
            source: Box::new(source),
        }
    }

    #[test]
    fn classify_looks_through_the_hop_wrapper_and_routes_to_the_hop() {
        let s = routed_settings();
        let jump0 = engine_net::HopId::Jump { index: 0, of: 2 };
        let jump1 = engine_net::HopId::Jump { index: 1, of: 2 };
        // An auth failure keeps its code, and the hop text stays in the message.
        let failure = classify(
            hop_error(jump0, engine_net::AuthError::Rejected { method: "password", remaining: "publickey".into() }.into()),
            &s,
        );
        assert_eq!(failure.code(), Some("tunnel.auth"));
        assert!(failure.message().contains("jump 1 of 2 hop1@jump1.example.com:2201"), "{}", failure.message());
        // A timeout likewise.
        let failure = classify(
            hop_error(jump1, TunnelError::Timeout { operation: "connect", seconds: 10 }),
            &s,
        );
        assert_eq!(failure.code(), Some("tunnel.timeout"));
        // A passphrase request names the jump and goes to it, not to the bastion.
        match classify(
            hop_error(jump1, engine_net::AuthError::PassphraseRequired { path: "/k/j2".into() }.into()),
            &s,
        ) {
            ConnectFailure::SecretNeeded(request) => {
                assert_eq!((request.hop, request.need), (SecretHop::Jump(1), SecretNeed::Passphrase));
                assert_eq!(request.who, "jump 2 of 2 hop2@jump2.example.com:22");
                assert!(!request.retry);
            }
            other => panic!("{other:?}"),
        }
        match classify(
            hop_error(engine_net::HopId::Bastion, engine_net::AuthError::BadPassphrase { path: "/k/b".into() }.into()),
            &s,
        ) {
            ConnectFailure::SecretNeeded(request) => {
                assert_eq!(request.hop, SecretHop::Bastion);
                assert!(request.retry);
            }
            other => panic!("{other:?}"),
        }
        // Without jumps there is no wrapper: the bastion.
        match classify(engine_net::AuthError::PassphraseRequired { path: "/k/b".into() }.into(), &settings()) {
            ConnectFailure::SecretNeeded(request) => assert_eq!(request.hop, SecretHop::Bastion),
            other => panic!("{other:?}"),
        }
        // A host key question carries the hop's own host:port, and that hop's login.
        match classify(unknown_host_at("jump1.example.com", 2201), &s) {
            ConnectFailure::HostKeyUnknown(request) => {
                assert_eq!((request.host.as_str(), request.port, request.user.as_str()), ("jump1.example.com", 2201, "hop1"));
            }
            other => panic!("{other:?}"),
        }
        match classify(unknown_host_at("bastion.example.com", 2222), &s) {
            ConnectFailure::HostKeyUnknown(request) => assert_eq!(request.user, "deploy"),
            other => panic!("{other:?}"),
        }
        assert_eq!(
            classify(changed_host("jump1.example.com"), &s).code(),
            Some("tunnel.host_key_mismatch")
        );
    }

    #[test]
    fn settings_read_importer_json_and_unreadable_json_is_no_tunnel() {
        let imported = SshSettings::from_network_json(
            r#"{"ssh":{"isEnabled":false,"host":"bastion.example.com","port":2222,"user":"deploy","privateKeyPath":"/keys/id_ed25519"}}"#,
        );
        assert!(!imported.enabled);
        assert_eq!(imported.port, "2222");
        assert_eq!(
            imported.auth,
            SshAuthMethod::KeyFile,
            "a key path means a key"
        );
        for broken in ["", "{", "[]", r#"{"ssh":5}"#, "{}"] {
            assert_eq!(
                SshSettings::from_network_json(broken),
                SshSettings::default(),
                "{broken}"
            );
        }
    }

    #[test]
    fn validation_names_the_missing_field_only_when_enabled() {
        assert!(SshSettings::default().validate().is_ok());
        let mut s = settings();
        s.host.clear();
        assert!(s.validate().unwrap_err().to_string().contains("host"));
        let mut s = settings();
        s.port = "0".into();
        assert!(s.validate().unwrap_err().to_string().contains("port"));
        let mut s = settings();
        s.user = " ".into();
        assert!(s.validate().unwrap_err().to_string().contains("user"));
        let mut s = settings();
        s.auth = SshAuthMethod::KeyFile;
        assert!(s.validate().unwrap_err().to_string().contains("private key"));
        let mut s = settings();
        s.remote_port = "x".into();
        assert!(s.validate().unwrap_err().to_string().contains("remote port"));
    }

    #[test]
    fn saved_secrets_follow_the_method_and_the_save_choice() {
        let store = MemoryStore::default();
        let typed = SshSecretInput::new(Secret::new("pw"), Secret::new("phrase"));
        let mut s = settings();
        save_secrets(&store, "ID", &s, &typed, false).unwrap();
        assert_eq!(
            store
                .load(SecretKind::SshPassword, "ID")
                .unwrap()
                .as_deref(),
            Some("pw")
        );
        assert_eq!(
            store.load(SecretKind::SshPassphrase, "ID").unwrap(),
            None,
            "unused by the method"
        );
        let loaded = SshSecrets::load(&store, &["ID".into()], &s);
        assert_eq!(loaded.password.unwrap().expose(), "pw");
        // An untouched field keeps the stored secret.
        save_secrets(&store, "ID", &s, &SshSecretInput::default(), false).unwrap();
        assert!(store.load(SecretKind::SshPassword, "ID").unwrap().is_some());
        // Switching the method clears the password; "do not save" clears everything.
        s.auth = SshAuthMethod::KeyFile;
        save_secrets(&store, "ID", &s, &typed, false).unwrap();
        assert_eq!(store.load(SecretKind::SshPassword, "ID").unwrap(), None);
        assert_eq!(
            store
                .load(SecretKind::SshPassphrase, "ID")
                .unwrap()
                .as_deref(),
            Some("phrase")
        );
        s.save_secrets = false;
        save_secrets(&store, "ID", &s, &typed, false).unwrap();
        assert_eq!(store.load(SecretKind::SshPassphrase, "ID").unwrap(), None);
        assert!(
            SshSecrets::load(&store, &["ID".into()], &s).is_empty(),
            "never read when not saved"
        );
        assert!(!format!("{typed:?}").contains("phrase"));
    }

    #[test]
    fn a_session_tunnel_rewrites_the_uri_and_opens_with_the_settings() {
        let opener = FakeOpener::new();
        let (uri, tunnel) = open_for_session(
            &access(opener.clone(), settings()),
            ConnectionKind::Postgres,
            "postgres://u:s3@db.example.com:5432/app?sslmode=verify-full",
        )
        .unwrap();
        assert_eq!(
            uri,
            "postgres://u:s3@db.example.com:41001/app?sslmode=verify-full&hostaddr=127.0.0.1"
        );
        assert_eq!(tunnel.via(), "deploy@bastion.example.com:2222");
        let opened = opener.opened.lock().unwrap();
        assert_eq!(opened.len(), 1);
        assert_eq!(opened[0].port, 2222);
        assert_eq!(opened[0].targets, [Target::new("db.example.com", 5432)]);
        assert!(!opened[0].allow_legacy_algorithms);
        assert!(!format!("{:?}", opened[0]).contains("pw-needle"));
    }

    #[test]
    fn every_session_gets_its_own_tunnel_unless_the_bastion_asks_questions() {
        let opener = FakeOpener::new();
        let first = access(opener.clone(), settings());
        let (uri_a, tunnel_a) =
            open_for_session(&first, ConnectionKind::Mysql, "mysql://u@db.example.com/d").unwrap();
        let (uri_b, _tunnel_b) =
            open_for_session(&first, ConnectionKind::Mysql, "mysql://u@db.example.com/d").unwrap();
        assert_eq!(opener.opened_count(), 2, "one tunnel per database session");
        assert_ne!(uri_a, uri_b, "different local ports");
        assert!(!tunnel_a.interactive);

        // A one-time-code bastion: the second session reuses the first tunnel.
        let interactive = SessionTunnel {
            interactive: true,
            ..tunnel_a
        };
        let second = TunnelAccess {
            shared: Some(interactive),
            ..first
        };
        let (uri_c, _) =
            open_for_session(&second, ConnectionKind::Mysql, "mysql://u@db.example.com/d").unwrap();
        assert_eq!(opener.opened_count(), 2, "shared, not reopened");
        assert_eq!(uri_c, uri_a);
    }

    #[test]
    fn a_dead_shared_tunnel_is_never_reused() {
        let opener = FakeOpener::new();
        let first = access(opener.clone(), settings());
        let dead = Arc::new(FakeTunnel {
            via: "x@y:22".into(),
            addrs: Vec::new(),
            alive: Arc::new(AtomicBool::new(false)),
            probe_error: None,
            drops: None,
            drop_delay_ms: None,
        });
        let reuse = TunnelAccess {
            shared: Some(SessionTunnel {
                handle: dead,
                targets: Vec::new(),
                interactive: true,
            }),
            ..first
        };
        open_for_session(&reuse, ConnectionKind::Mysql, "mysql://u@db.example.com/d").unwrap();
        assert_eq!(
            opener.opened_count(),
            1,
            "a fresh tunnel replaced the dead one"
        );
    }

    #[test]
    fn the_remote_endpoint_overrides_the_forward_target_only() {
        let opener = FakeOpener::new();
        let mut s = settings();
        s.remote_host = "10.0.0.9".into();
        let (uri, _) = open_for_session(
            &access(opener.clone(), s),
            ConnectionKind::Mysql,
            "mysql://u@db.example.com:3307/d",
        )
        .unwrap();
        assert_eq!(
            opener.opened.lock().unwrap()[0].targets,
            [Target::new("10.0.0.9", 3307)]
        );
        assert!(uri.contains("tls-server-name=db.example.com"), "{uri}");
    }

    #[test]
    fn refusals_happen_before_any_connection_is_made() {
        let opener = FakeOpener::new();
        let acc = access(opener.clone(), settings());
        for (kind, uri, needle) in [
            (
                ConnectionKind::Redis,
                "rediss://cache.example.com/0",
                "TLS to Redis through an SSH tunnel is not supported yet",
            ),
            (
                ConnectionKind::Clickhouse,
                "clickhouse://ch.example.com/?secure=true",
                "TLS to ClickHouse through an SSH tunnel is not supported yet",
            ),
            (
                ConnectionKind::Mongodb,
                "mongodb://m.example.com/?tls=true",
                "TLS to MongoDB through an SSH tunnel is not supported yet",
            ),
            (
                ConnectionKind::Mongodb,
                "mongodb+srv://m.example.com/",
                "SRV",
            ),
            (
                ConnectionKind::Mongodb,
                "mongodb://a:1,b:2/",
                "several hosts",
            ),
            (
                ConnectionKind::Sqlite,
                "sqlite:///x.db",
                "no network address",
            ),
        ] {
            match open_for_session(&acc, kind, uri) {
                Err(ConnectFailure::Message(message)) => {
                    assert!(message.contains(needle), "{message}")
                }
                other => panic!("{uri}: {other:?}"),
            }
        }
        assert_eq!(opener.opened_count(), 0);
    }

    #[test]
    fn a_missing_password_is_asked_for_not_sent_empty() {
        let opener = FakeOpener::new();
        let mut acc = access(opener.clone(), settings());
        acc.secrets = SshSecrets::default();
        match open_for_session(&acc, ConnectionKind::Mysql, "mysql://u@db.example.com/d") {
            Err(ConnectFailure::SecretNeeded(request)) => {
                assert_eq!(request.need, SecretNeed::Password);
                assert_eq!(request.who, "deploy@bastion.example.com");
            }
            other => panic!("{other:?}"),
        }
        assert_eq!(opener.opened_count(), 0);
    }

    #[test]
    fn host_key_failures_are_classified_by_what_the_user_may_do() {
        let opener = FakeOpener::new();
        opener
            .then(Err(unknown_host("bastion.example.com")))
            .then(Err(changed_host("bastion.example.com")));
        let acc = access(opener, settings());
        match open_for_session(&acc, ConnectionKind::Mysql, "mysql://u@db.example.com/d") {
            Err(ConnectFailure::HostKeyUnknown(request)) => {
                assert_eq!(
                    (
                        request.host.as_str(),
                        request.port,
                        request.algorithm.as_str()
                    ),
                    ("bastion.example.com", 22, "ssh-ed25519")
                );
                assert!(request.fingerprint.starts_with("SHA256:"));
                assert_eq!(request.user, "deploy");
            }
            other => panic!("{other:?}"),
        }
        match open_for_session(&acc, ConnectionKind::Mysql, "mysql://u@db.example.com/d") {
            Err(failure @ ConnectFailure::HostKeyRejected(..)) => {
                assert_eq!(failure.code(), Some("tunnel.host_key_mismatch"));
                assert!(
                    failure.message().contains("CHANGED"),
                    "{}",
                    failure.message()
                );
            }
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn auth_and_connect_failures_carry_their_stable_code() {
        let opener = FakeOpener::new();
        opener
            .then(Err(engine_net::AuthError::Rejected {
                method: "password",
                remaining: "publickey".into(),
            }
            .into()))
            .then(Err(TunnelError::Connect {
                host: "bastion.example.com".into(),
                port: 2222,
                reason: "refused".into(),
            }))
            .then(Err(engine_net::AuthError::PassphraseRequired {
                path: "/keys/id".into(),
            }
            .into()));
        let acc = access(opener, settings());
        let uri = "mysql://u@db.example.com/d";
        match open_for_session(&acc, ConnectionKind::Mysql, uri) {
            Err(failure) => {
                assert_eq!(failure.code(), Some("tunnel.auth"));
                assert!(!failure.message().contains("pw-needle"));
            }
            other => panic!("{other:?}"),
        }
        assert_eq!(
            open_for_session(&acc, ConnectionKind::Mysql, uri)
                .unwrap_err()
                .code(),
            Some("tunnel.connect")
        );
        match open_for_session(&acc, ConnectionKind::Mysql, uri) {
            Err(ConnectFailure::SecretNeeded(request)) => {
                assert_eq!(request.need, SecretNeed::Passphrase);
                assert!(!request.retry);
            }
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn test_tunnel_probes_every_target_and_reports_what_failed() {
        let opener = FakeOpener::new();
        let acc = access(opener.clone(), settings());
        let tested = test_tunnel(
            &acc,
            ConnectionKind::Postgres,
            "postgres://u@db.example.com/d",
        )
        .unwrap();
        assert_eq!(tested.targets, ["db.example.com:5432"]);
        assert_eq!(tested.via, "deploy@bastion.example.com:2222");
        *opener.probe_error.lock().unwrap() = Some(TunnelError::ForwardRefused {
            host: "db.example.com".into(),
            port: 5432,
            reason: "connection refused".into(),
        });
        match test_tunnel(
            &acc,
            ConnectionKind::Postgres,
            "postgres://u@db.example.com/d",
        ) {
            Err(ConnectFailure::Tunnel { code, message }) => {
                assert_eq!(code, "tunnel.forward_refused");
                assert!(
                    message.contains("cannot reach db.example.com:5432"),
                    "{message}"
                );
            }
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn a_dead_tunnel_reports_its_code() {
        let opener = FakeOpener::new();
        let (_, tunnel) = open_for_session(
            &access(opener.clone(), settings()),
            ConnectionKind::Mysql,
            "mysql://u@db.example.com/d",
        )
        .unwrap();
        assert_eq!(tunnel.failure(), None);
        opener.alive.store(false, Ordering::SeqCst);
        let (code, message) = tunnel.failure().unwrap();
        assert_eq!(code, "tunnel.closed");
        assert!(message.contains("closed"));
    }

    #[test]
    fn unreadable_settings_are_an_error_never_no_tunnel() {
        assert_eq!(SshSettings::parse("").unwrap(), SshSettings::default());
        assert_eq!(SshSettings::parse("{}").unwrap(), SshSettings::default());
        assert!(SshSettings::parse(r#"{"proxy":{}}"#).is_ok());
        for broken in [
            "{",
            "[]",
            "5",
            r#"{"ssh":5}"#,
            r#"{"ssh":{"isEnabled":"yes"}}"#,
            r#"{"ssh":{"isEnabled":1}}"#,
            r#"{"ssh":{"isEnabled":null}}"#,
        ] {
            let error = SshSettings::parse(broken).unwrap_err();
            assert!(
                error
                    .to_string()
                    .starts_with("cannot read this connection's tunnel settings"),
                "{broken}: {error}"
            );
        }
        assert!(
            SshSettings::parse(r#"{"ssh":{"isEnabled":true}}"#)
                .unwrap()
                .enabled
        );
    }

    #[test]
    fn server_text_is_stripped_collapsed_and_capped() {
        assert_eq!(plain_text("a\nb\t c\u{7}d\u{1b}[31m", 50), "a b c d [31m");
        let long = "x".repeat(500);
        let shown = plain_text(&long, 200);
        assert_eq!(shown.chars().count(), 201);
        assert!(shown.ends_with('\u{2026}'));
        assert_eq!(plain_text("  \n\r ", 10), "");
    }

    #[test]
    fn plain_text_strips_bidi_and_zero_width_characters() {
        let tricky =
            "pay\u{202e}drowssap\u{202c} to\u{200b}ken\u{2066}x\u{2069}\u{200f}\u{2060}\u{feff}!";
        assert_eq!(plain_text(tricky, 100), "paydrowssap tokenx!");
        for c in [
            '\u{202a}', '\u{202e}', '\u{2066}', '\u{2069}', '\u{200b}', '\u{200f}', '\u{2060}',
            '\u{feff}',
        ] {
            assert_eq!(plain_text(&format!("a{c}b"), 10), "ab", "{:04x}", c as u32);
        }
        // Real spaces still collapse; ordinary non-ASCII text is kept.
        assert_eq!(
            plain_text("  caf\u{e9}   \u{6771}\u{4eac} ", 10),
            "caf\u{e9} \u{6771}\u{4eac}"
        );
    }

    #[test]
    fn a_failed_save_leaves_the_saved_ssh_secret_in_place() {
        let mut store = MemoryStore::default();
        store
            .items
            .borrow_mut()
            .insert((SecretKind::SshPassword, "ID".into()), "pw-old".into());
        let old = SshSettings {
            save_secrets: true,
            ..settings()
        };
        let moved = SshSettings {
            host: "other.example.com".into(),
            ..old.clone()
        };
        // The store refuses the new password: the saved one is untouched (not deleted first).
        store.unavailable = true;
        let typed = SshSecretInput::new(Secret::new("pw-new"), Secret::default());
        assert!(persist_secrets(&store, "ID", Some(&old), &moved, &typed).is_err());
        assert_eq!(
            store
                .items
                .borrow()
                .get(&(SecretKind::SshPassword, "ID".to_string()))
                .map(String::as_str),
            Some("pw-old")
        );
        // Nothing typed and the same bastion: also untouched.
        persist_secrets(&store, "ID", Some(&old), &old, &SshSecretInput::default()).unwrap();
        assert!(store
            .items
            .borrow()
            .contains_key(&(SecretKind::SshPassword, "ID".to_string())));
    }

    #[test]
    fn a_changed_bastion_does_not_inherit_the_saved_secret() {
        let old = settings();
        let mut moved = settings();
        moved.host = "evil.example.com".into();
        assert!(bastion_changed(&old, &moved));
        let mut other_user = settings();
        other_user.user = "root".into();
        assert!(bastion_changed(&old, &other_user));
        let mut other_port = settings();
        other_port.port = "22".into();
        assert!(bastion_changed(&old, &other_port));
        assert!(!bastion_changed(&old, &old.clone()));
        assert!(
            !bastion_changed(&SshSettings::default(), &moved),
            "a tunnel that was off had no saved secret to protect"
        );

        let store = MemoryStore::default();
        store
            .write(SecretKind::SshPassword, "ID", "pw-old")
            .unwrap();
        // Untouched field + changed bastion: the stored password goes, nothing replaces it.
        save_secrets(
            &store,
            "ID",
            &moved,
            &SshSecretInput::default(),
            bastion_changed(&old, &moved),
        )
        .unwrap();
        assert_eq!(store.load(SecretKind::SshPassword, "ID").unwrap(), None);
        // A password typed for the new bastion is kept.
        let typed = SshSecretInput::new(Secret::new("pw-new"), Secret::default());
        save_secrets(&store, "ID", &moved, &typed, true).unwrap();
        assert_eq!(
            store
                .load(SecretKind::SshPassword, "ID")
                .unwrap()
                .as_deref(),
            Some("pw-new")
        );
    }

    #[test]
    fn without_an_app_data_dir_there_is_no_tunnel() {
        let opener = FakeOpener::new();
        let mut acc = access(opener.clone(), settings());
        acc.known_hosts = None;
        match open_for_session(&acc, ConnectionKind::Mysql, "mysql://u@db.example.com/d") {
            Err(failure @ ConnectFailure::Tunnel { .. }) => {
                assert_eq!(failure.code(), Some("tunnel.config"));
                assert!(failure.message().contains("app data directory"));
            }
            other => panic!("{other:?}"),
        }
        assert_eq!(opener.opened_count(), 0);
    }

    #[test]
    fn server_supplied_error_text_is_collapsed() {
        let error = TunnelError::Connect {
            host: "h.example.com".into(),
            port: 22,
            reason: format!("line one\nENTER YOUR PASSWORD {}", "z".repeat(900)),
        };
        let failure = classify(error, &settings());
        assert!(!failure.message().contains('\n'));
        assert!(failure.message().chars().count() < 600);
    }
}
