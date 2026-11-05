//! The parts of a connection's network settings beyond the bastion: jump hosts
//! (`network_settings_json["ssh"].jumps`) and the proxy (`["proxy"]`), plus the typed reasons a
//! route cannot be used ([`SettingsInvalid`]).
//!
//! Neither holds a secret: a jump's password / passphrase and the proxy password live in the
//! credential store (see `SshSecrets` in [`super::ssh`]).

use engine_net::ProxyKind;
use serde_json::{json, Map, Value};

use super::ssh::{flag_of, parse_port, text_of, SettingsError, SshAuthMethod};
use crate::credentials::{jump_account, SecretKind};

// ---------------------------------------------------------------------------------------------
// Jump hosts
// ---------------------------------------------------------------------------------------------

/// One SSH server on the way to the bastion. The first in the list is dialled first.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct JumpSettings {
    pub host: String,
    pub port: String,
    pub user: String,
    pub auth: SshAuthMethod,
    pub key_path: String,
}

impl Default for JumpSettings {
    fn default() -> Self {
        Self {
            host: String::new(),
            port: "22".into(),
            user: String::new(),
            auth: SshAuthMethod::Password,
            key_path: String::new(),
        }
    }
}

impl JumpSettings {
    pub fn port_number(&self) -> Option<u16> {
        parse_port(&self.port)
    }

    /// Same server and login: a secret saved for one is never offered to the other.
    pub fn same_hop(&self, other: &Self) -> bool {
        self.host.trim() == other.host.trim()
            && self.port_number() == other.port_number()
            && self.user.trim() == other.user.trim()
    }

    pub(super) fn parse(object: &Map<String, Value>, index: usize) -> Result<Self, SettingsError> {
        let mut jump = Self {
            host: text_of(object, "host").unwrap_or_default().trim().to_owned(),
            user: text_of(object, "user").unwrap_or_default().trim().to_owned(),
            key_path: text_of(object, "privateKeyPath").unwrap_or_default(),
            ..Self::default()
        };
        if let Some(port) = text_of(object, "port").filter(|port| !port.trim().is_empty()) {
            jump.port = port.trim().to_owned();
        }
        jump.auth = match text_of(object, "authMethod") {
            None => {
                if jump.key_path.is_empty() {
                    SshAuthMethod::Password
                } else {
                    SshAuthMethod::KeyFile
                }
            }
            Some(method) => SshAuthMethod::from_persisted(&method).ok_or_else(|| {
                SettingsError(format!("jump host {}: unknown authMethod", index + 1))
            })?,
        };
        Ok(jump)
    }

    pub(super) fn to_json(&self) -> Value {
        json!({
            "host": self.host.trim(),
            "port": self.port.trim(),
            "user": self.user.trim(),
            "authMethod": self.auth.persisted(),
            "privateKeyPath": self.key_path.trim(),
        })
    }
}

/// The `jumps` array of the `ssh` object. Absent is "no jump hosts"; anything that cannot be read
/// is an error (a hop that is silently dropped would send the connection around a firewall's
/// intended path, or straight to a host that cannot be reached).
pub(super) fn parse_jumps(ssh: &Map<String, Value>) -> Result<Vec<JumpSettings>, SettingsError> {
    let Some(jumps) = ssh.get("jumps") else {
        return Ok(Vec::new());
    };
    let Some(jumps) = jumps.as_array() else {
        return Err(SettingsError("the jumps are not a list".into()));
    };
    if jumps.len() > engine_net::MAX_JUMPS {
        return Err(SettingsError(format!(
            "there are more than {} jump hosts",
            engine_net::MAX_JUMPS
        )));
    }
    jumps
        .iter()
        .enumerate()
        .map(|(index, jump)| {
            let object = jump.as_object().ok_or_else(|| {
                SettingsError(format!("jump host {} is not an object", index + 1))
            })?;
            JumpSettings::parse(object, index)
        })
        .collect()
}

// ---------------------------------------------------------------------------------------------
// Proxy
// ---------------------------------------------------------------------------------------------

/// The protocol of the proxy.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum ProxyType {
    #[default]
    Socks5,
    /// `CONNECT host:port` (RFC 9110).
    Http,
}

impl ProxyType {
    pub const ALL: [Self; 2] = [Self::Socks5, Self::Http];

    pub fn label(self) -> &'static str {
        match self {
            Self::Socks5 => "SOCKS5",
            Self::Http => "HTTP CONNECT",
        }
    }

    pub(super) fn persisted(self) -> &'static str {
        match self {
            Self::Socks5 => "socks5",
            Self::Http => "http",
        }
    }

    /// The names TableX writes (`socks5`, `http`) and the ones the first versions wrote
    /// (`SOCKS5`, `HTTP CONNECT`).
    fn from_persisted(text: &str) -> Option<Self> {
        match text.trim().to_ascii_lowercase().as_str() {
            "socks5" => Some(Self::Socks5),
            "http" | "http connect" => Some(Self::Http),
            _ => None,
        }
    }

    pub fn default_port(self) -> &'static str {
        match self {
            Self::Socks5 => "1080",
            Self::Http => "8080",
        }
    }

    pub fn net_kind(self) -> ProxyKind {
        match self {
            Self::Socks5 => ProxyKind::Socks5,
            Self::Http => ProxyKind::HttpConnect,
        }
    }
}

/// The `proxy` object of a connection's network settings: a SOCKS5 or HTTP CONNECT proxy that
/// carries the connection to the first SSH hop, or (without SSH) to the database itself.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProxySettings {
    pub enabled: bool,
    pub kind: ProxyType,
    pub host: String,
    pub port: String,
    pub user: String,
    /// Keep the proxy password in the credential store.
    pub save_secrets: bool,
}

impl Default for ProxySettings {
    fn default() -> Self {
        Self {
            enabled: false,
            kind: ProxyType::Socks5,
            host: String::new(),
            port: ProxyType::Socks5.default_port().into(),
            user: String::new(),
            save_secrets: true,
        }
    }
}

impl ProxySettings {
    pub fn port_number(&self) -> Option<u16> {
        parse_port(&self.port)
    }

    /// `SOCKS5 proxy host:port` / `HTTP proxy host:port`, as `engine-net` names it.
    pub fn describe(&self) -> String {
        let label = match self.kind {
            ProxyType::Socks5 => "SOCKS5 proxy",
            ProxyType::Http => "HTTP proxy",
        };
        format!("{label} {}:{}", self.host.trim(), self.port.trim())
    }

    /// The proxy has a login (and so a password).
    pub fn has_login(&self) -> bool {
        self.enabled && !self.user.trim().is_empty()
    }

    /// Same proxy and login: a password saved for one is never offered to the other.
    pub fn same_proxy(&self, other: &Self) -> bool {
        self.kind == other.kind
            && self.host.trim() == other.host.trim()
            && self.port_number() == other.port_number()
            && self.user.trim() == other.user.trim()
    }

    /// Reads the `proxy` object. A switch or a kind that cannot be read is an error.
    pub(super) fn parse(object: &Map<String, Value>) -> Result<Self, SettingsError> {
        let mut proxy = Self {
            enabled: match object.get("isEnabled") {
                None => false,
                Some(Value::Bool(enabled)) => *enabled,
                Some(_) => {
                    return Err(SettingsError("the proxy isEnabled is not true or false".into()))
                }
            },
            ..Self::default()
        };
        proxy.kind = match text_of(object, "kind") {
            None => ProxyType::default(),
            Some(kind) => ProxyType::from_persisted(&kind)
                .ok_or_else(|| SettingsError("the proxy kind is not socks5 or http".into()))?,
        };
        proxy.port = proxy.kind.default_port().to_owned();
        if let Some(port) = text_of(object, "port").filter(|port| !port.trim().is_empty()) {
            proxy.port = port.trim().to_owned();
        }
        proxy.host = text_of(object, "host").unwrap_or_default().trim().to_owned();
        // `username` is what the first versions wrote.
        proxy.user = text_of(object, "user")
            .or_else(|| text_of(object, "username"))
            .unwrap_or_default()
            .trim()
            .to_owned();
        proxy.save_secrets = flag_of(object, "saveSecrets").unwrap_or(true);
        Ok(proxy)
    }

    /// Merges this proxy into the `proxy` object of `root`; keys this version does not know stay.
    pub(super) fn write_into(&self, root: &mut Map<String, Value>) {
        let mut proxy = root
            .get("proxy")
            .and_then(Value::as_object)
            .cloned()
            .unwrap_or_default();
        proxy.insert("isEnabled".into(), json!(self.enabled));
        proxy.insert("kind".into(), json!(self.kind.persisted()));
        proxy.insert("host".into(), json!(self.host.trim()));
        proxy.insert("port".into(), json!(self.port.trim()));
        proxy.insert("user".into(), json!(self.user.trim()));
        proxy.insert("saveSecrets".into(), json!(self.save_secrets));
        // The first versions kept the proxy login in the record; it is not written back.
        proxy.remove("username");
        proxy.remove("password");
        root.insert("proxy".into(), Value::Object(proxy));
    }
}

// ---------------------------------------------------------------------------------------------
// Which secret belongs to which hop
// ---------------------------------------------------------------------------------------------

/// The place in the route a secret is for. A route dials the proxy, then each jump host in order,
/// then the bastion.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SecretHop {
    Bastion,
    /// 0 = dialled first.
    Jump(usize),
    Proxy,
}

/// What kind of secret.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SecretNeed {
    Password,
    Passphrase,
}

impl SecretHop {
    /// The credential store entry (service) of this hop's secret.
    pub fn kind(self, need: SecretNeed) -> SecretKind {
        match (self, need) {
            (Self::Proxy, _) => SecretKind::Proxy,
            (_, SecretNeed::Password) => SecretKind::SshPassword,
            (_, SecretNeed::Passphrase) => SecretKind::SshPassphrase,
        }
    }

    /// The credential store account for the connection account `base`.
    pub fn account(self, base: &str) -> String {
        match self {
            Self::Jump(index) => jump_account(base, index),
            Self::Bastion | Self::Proxy => base.to_owned(),
        }
    }
}

// ---------------------------------------------------------------------------------------------
// Validation
// ---------------------------------------------------------------------------------------------

/// What is wrong with a field of a jump host.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum JumpField {
    Host,
    Port,
    User,
    KeyFile,
}

impl std::fmt::Display for JumpField {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            Self::Host => "host is required",
            Self::Port => "port must be a number from 1 to 65535",
            Self::User => "user is required",
            Self::KeyFile => "choose the private key file",
        })
    }
}

/// What must be fixed before a route can be opened.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum SettingsInvalid {
    #[error("SSH host is required (Tunnel / Proxy tab)")]
    SshHost,
    #[error("SSH port must be a number from 1 to 65535")]
    SshPort,
    #[error("SSH user is required (Tunnel / Proxy tab)")]
    SshUser,
    #[error("Choose the private key file (Tunnel / Proxy tab)")]
    SshKeyFile,
    #[error("SSH remote port must be a number from 1 to 65535")]
    RemotePort,
    /// 1-based, as the tab numbers its rows.
    #[error("Jump host {number}: {field}")]
    Jump { number: usize, field: JumpField },
    #[error("At most {max} jump hosts can be chained")]
    TooManyJumps { max: usize },
    #[error("{hop} appears twice in the route: every SSH server can only be listed once")]
    RepeatedHop { hop: String },
    #[error("Proxy host is required (Tunnel / Proxy tab)")]
    ProxyHost,
    #[error("Proxy port must be a number from 1 to 65535")]
    ProxyPort,
}
