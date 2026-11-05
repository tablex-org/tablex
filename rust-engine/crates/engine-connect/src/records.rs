//! Saved-connection loading compatible with the Swift app's storage formats:
//! records live in `~/Library/Application Support/tablex/connections.db`
//! (via `engine-storage`), passwords live in the OS credential store (see
//! `credentials`) under the exact service/account identifiers the Swift
//! `ConnectionCredentialStore` writes. This module never persists passwords
//! outside it.

use engine_storage::connection::{self, SavedConnectionRecord};
use uuid::Uuid;

/// Persisted `db_type` values the GPUI shell can drive.
pub const DB_TYPE_POSTGRESQL: &str = "postgresql";
pub const DB_TYPE_SQLITE: &str = "sqlite";
pub const DB_TYPE_MONGODB: &str = "mongodb";
pub const DB_TYPE_MYSQL: &str = "mysql";
pub const DB_TYPE_MARIADB: &str = "mariadb";
pub const DB_TYPE_REDIS: &str = "redis";
pub const DB_TYPE_CLICKHOUSE: &str = "clickhouse";

pub fn missing_password_error() -> String {
    format!(
        "Saved password was not found in {}",
        crate::platform::CREDENTIAL_STORE_LABEL
    )
}

pub fn is_missing_password_error(message: &str) -> bool {
    message.contains(&missing_password_error())
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ConnectionKind {
    Postgres,
    Sqlite,
    Mongodb,
    Mysql,
    Mariadb,
    Redis,
    Clickhouse,
}

impl ConnectionKind {
    pub fn from_persisted(db_type: &str) -> Option<Self> {
        match db_type {
            DB_TYPE_POSTGRESQL => Some(Self::Postgres),
            DB_TYPE_SQLITE => Some(Self::Sqlite),
            DB_TYPE_MONGODB => Some(Self::Mongodb),
            DB_TYPE_MYSQL => Some(Self::Mysql),
            DB_TYPE_MARIADB => Some(Self::Mariadb),
            DB_TYPE_REDIS => Some(Self::Redis),
            DB_TYPE_CLICKHOUSE => Some(Self::Clickhouse),
            _ => None,
        }
    }

    /// The scheme prefix the engine accepts for this kind.
    pub fn expected_scheme(&self) -> &'static str {
        match self {
            Self::Postgres => "postgres",
            Self::Sqlite => "sqlite",
            Self::Mongodb => "mongodb",
            Self::Mysql => "mysql",
            Self::Mariadb => "mariadb",
            Self::Redis => "redis",
            Self::Clickhouse => "clickhouse",
        }
    }

    /// The id of the driver that serves this kind (the installed-driver id and
    /// the `DriverCapabilities::driver` it reports). Differs from
    /// [`Self::expected_scheme`] for PostgreSQL: `postgresql` vs `postgres`.
    pub fn driver_id(&self) -> &'static str {
        match self {
            Self::Postgres => DB_TYPE_POSTGRESQL,
            Self::Sqlite => DB_TYPE_SQLITE,
            Self::Mongodb => DB_TYPE_MONGODB,
            Self::Mysql => DB_TYPE_MYSQL,
            Self::Mariadb => DB_TYPE_MARIADB,
            Self::Redis => DB_TYPE_REDIS,
            Self::Clickhouse => DB_TYPE_CLICKHOUSE,
        }
    }

    pub fn is_supported(&self) -> bool {
        matches!(
            self,
            Self::Postgres
                | Self::Sqlite
                | Self::Mongodb
                | Self::Mysql
                | Self::Mariadb
                | Self::Redis
                | Self::Clickhouse
        )
    }

    /// `false` when a staged batch can be left half-applied after a failure
    /// (ClickHouse has no transactions), which the Apply dialog must say.
    pub fn applies_batches_atomically(&self) -> bool {
        !matches!(self, Self::Clickhouse)
    }
}

/// A saved connection the shell can open, paired with its persisted type.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SavedConnection {
    pub id: Uuid,
    pub name: String,
    pub uri: String,
    pub kind: ConnectionKind,
    pub save_password: bool,
    pub is_pinned: bool,
    /// `true` when the persisted record exists but has an unsupported type
    /// (redis/…). Rendered disabled; never deleted.
    pub disabled: bool,
    /// The connection goes through an SSH tunnel and / or a proxy (read with the record). A
    /// connection flagged like this is never connected without one; settings that cannot be read
    /// count as flagged.
    pub ssh_tunnel: bool,
}

impl SavedConnection {
    pub fn from_record(record: &SavedConnectionRecord) -> Self {
        Self {
            id: Uuid::parse_str(&record.id).unwrap_or_else(|_| Uuid::nil()),
            name: record.name.clone(),
            uri: record.uri.clone(),
            kind: ConnectionKind::from_persisted(&record.db_type).unwrap_or(ConnectionKind::Sqlite),
            save_password: record.save_password,
            is_pinned: record.is_pinned,
            disabled: ConnectionKind::from_persisted(&record.db_type)
                .is_none_or(|kind| !kind.is_supported()),
            // A switch that cannot be read is not "off": the connect then fails on it.
            ssh_tunnel: crate::tunnel::ssh::SshSettings::parse(&record.network_settings_json)
                .map_or(true, |settings| settings.routed()),
        }
    }

    pub fn matches_id(&self, value: &str) -> bool {
        Uuid::parse_str(value)
            .map(|id| id == self.id)
            .unwrap_or(false)
    }
}

/// Every persisted record, in the store's pinned-then-name order.
pub fn load_saved_records() -> Vec<SavedConnectionRecord> {
    connection::list_connections().unwrap_or_default()
}

/// The id as the agent tools and logs show it: a UUID in the canonical lowercase form
/// `SavedConnection::id.to_string()` prints, else the stored text.
pub fn display_connection_id(value: &str) -> String {
    Uuid::parse_str(value)
        .map(|id| id.to_string())
        .unwrap_or_else(|_| value.to_string())
}

pub fn storage_connection_id(value: &str) -> String {
    Uuid::parse_str(value)
        .map(|id| id.to_string().to_ascii_uppercase())
        .unwrap_or_else(|_| value.to_string())
}

pub fn normalize_uri_scheme(uri: &str) -> String {
    let Some((scheme, rest)) = uri.split_once("://") else {
        return uri.to_string();
    };
    let normalized = scheme.to_ascii_lowercase();
    if normalized == scheme {
        uri.to_string()
    } else {
        format!("{normalized}://{rest}")
    }
}

/// A sanitized copy of the URI with the password component removed, matching
/// Swift `ConnectionCredentialResolver.sanitizedURIForStorage`.
pub fn sanitized_uri(uri: &str) -> Result<String, String> {
    let uri = normalize_uri_scheme(uri);
    if uri
        .get(..9)
        .is_some_and(|scheme| scheme.eq_ignore_ascii_case("sqlite://"))
    {
        return Ok(format!("sqlite://{}", &uri[9..]));
    }
    if is_mongodb_uri(&uri) {
        let (authority_start, authority_end) = mongodb_authority_bounds(&uri)?;
        let authority = &uri[authority_start..authority_end];
        let Some(at) = authority.rfind('@') else {
            return Ok(uri.to_string());
        };
        let username = authority[..at].split(':').next().unwrap_or_default();
        let mut sanitized = String::with_capacity(uri.len());
        sanitized.push_str(&uri[..authority_start]);
        sanitized.push_str(username);
        sanitized.push('@');
        sanitized.push_str(&authority[at + 1..]);
        sanitized.push_str(&uri[authority_end..]);
        return Ok(sanitized);
    }
    let mut url = url::Url::parse(&uri).map_err(|_| "Invalid connection URI".to_string())?;
    if url.password().is_some() {
        url.set_password(None)
            .map_err(|_| "Invalid connection URI".to_string())?;
    }
    Ok(url.to_string())
}

/// A shareable URI: no userinfo password, secret-valued query option, or fragment.
pub fn public_connection_uri(uri: &str) -> Result<String, String> {
    let sanitized = sanitized_uri(uri)?;
    if sanitized.starts_with("sqlite://") {
        // SQLite's engine treats `#` as a filename byte, not a URL fragment.
        return Ok(sanitized
            .split_once('?')
            .map_or(sanitized.as_str(), |(path, _)| path)
            .to_string());
    }
    let without_fragment = sanitized.split('#').next().unwrap_or_default();
    let Some((base, query)) = without_fragment.split_once('?') else {
        return Ok(without_fragment.into());
    };
    let mut safe = url::form_urlencoded::Serializer::new(String::new());
    for (key, value) in url::form_urlencoded::parse(query.as_bytes()) {
        let key_lower = key.to_ascii_lowercase();
        // Unknown driver options may carry credentials; only known routing options are shareable.
        if !matches!(
            key_lower.as_str(),
            "authsource"
                | "authmechanism"
                | "replicaset"
                | "tls"
                | "ssl"
                | "sslmode"
                | "ssl-mode"
                | "retrywrites"
                | "readpreference"
                | "connecttimeoutms"
                | "serverselectiontimeoutms"
                | "directconnection"
                | "appname"
                | "w"
                | "wtimeoutms"
                | "secure"
                | "ca_cert"
        ) {
            continue;
        }
        safe.append_pair(&key, &value);
    }
    let query = safe.finish();
    if query.is_empty() {
        Ok(base.into())
    } else {
        Ok(format!("{base}?{query}"))
    }
}

pub fn duplicate_record(
    source: &SavedConnectionRecord,
    id: &str,
    timestamp: f64,
) -> Result<SavedConnectionRecord, String> {
    Uuid::parse_str(id).map_err(|_| "Invalid new connection id")?;
    let settings = redacted_network_settings(&source.network_settings_json, true)?;
    let mut copy = source.clone();
    copy.id = id.to_ascii_uppercase();
    copy.name = format!("{} copy", source.name);
    copy.uri = public_connection_uri(&source.uri)?;
    copy.save_password = false;
    copy.save_proxy_password = false;
    copy.network_settings_json = settings.to_string();
    copy.is_pinned = false;
    // The copy may point at another database: what agents may do there is a decision of its own. It
    // keeps its type, so it gets what a NEW connection of that type gets (the type's preset, never
    // for Production), not necessarily `off`.
    copy.overrides_json = crate::profile::without_agent_access(&source.overrides_json);
    copy.last_connected = None;
    copy.created_at = timestamp;
    copy.updated_at = timestamp;
    Ok(copy)
}

/// The network settings without anything that is a secret or that an untrusted file could use to
/// reach out: only the route's coordinates and choices survive (the bastion, its jump hosts, the
/// proxy). `keep_ssh_enabled` is for a local duplicate (the user's own tunnel and proxy keep
/// working; like the database password, their secrets are not copied and the copy asks for them);
/// an exported or imported file always comes back with the tunnel **and** the proxy switched off.
fn redacted_network_settings(
    json: &str,
    keep_ssh_enabled: bool,
) -> Result<serde_json::Value, String> {
    let settings: serde_json::Value =
        serde_json::from_str(json).map_err(|_| "Invalid saved connection network settings")?;
    if !settings.is_object() {
        return Err("Invalid saved connection network settings".into());
    }
    let mut redacted = serde_json::Map::new();
    for (section, keys, flags) in [
        (
            "ssh",
            &[
                "host",
                "port",
                "user",
                "authMethod",
                "privateKeyPath",
                "configHost",
                "remoteHost",
                "remotePort",
            ][..],
            &["allowLegacyAlgorithms"][..],
        ),
        ("proxy", &["host", "port", "kind", "user"][..], &[][..]),
    ] {
        let Some(value) = settings.get(section) else {
            continue;
        };
        let object = value
            .as_object()
            .ok_or("Invalid saved connection network settings")?;
        let mut safe = serde_json::Map::new();
        for key in keys {
            match object.get(*key) {
                Some(value) if value.is_string() => {
                    safe.insert((*key).into(), value.clone());
                }
                // Ports written as numbers by other tools.
                Some(serde_json::Value::Number(number)) if key.ends_with("ort") => {
                    safe.insert((*key).into(), serde_json::Value::String(number.to_string()));
                }
                _ => {}
            }
        }
        for key in flags {
            // Legacy algorithms weaken the SSH handshake: only the user's own duplicate keeps
            // the choice; an exported or imported file always starts from the safe default.
            if !keep_ssh_enabled {
                continue;
            }
            if let Some(value) = object.get(*key).filter(|value| value.is_boolean()) {
                safe.insert((*key).into(), value.clone());
            }
        }
        let enabled = keep_ssh_enabled
            && object.get("isEnabled").and_then(serde_json::Value::as_bool) == Some(true);
        safe.insert("isEnabled".into(), serde_json::Value::Bool(enabled));
        // Mirrors `save_password = false`: the copy does not inherit saved secrets.
        safe.insert("saveSecrets".into(), serde_json::Value::Bool(false));
        if section == "ssh" {
            if let Some(jumps) = object.get("jumps") {
                safe.insert(
                    "jumps".into(),
                    redacted_jumps(jumps).map_err(|error| error.to_string())?,
                );
            }
        }
        redacted.insert(section.into(), serde_json::Value::Object(safe));
    }
    Ok(serde_json::Value::Object(redacted))
}

/// The jump hosts of an `ssh` object with only their coordinates: a smuggled key stays out, so the
/// import's comparison against the redacted form rejects it.
fn redacted_jumps(jumps: &serde_json::Value) -> Result<serde_json::Value, InvalidNetworkSettings> {
    let jumps = jumps.as_array().ok_or(InvalidNetworkSettings)?;
    if jumps.len() > engine_net::MAX_JUMPS {
        return Err(InvalidNetworkSettings);
    }
    jumps
        .iter()
        .map(|jump| {
            let object = jump.as_object().ok_or(InvalidNetworkSettings)?;
            let mut safe = serde_json::Map::new();
            for key in ["host", "port", "user", "authMethod", "privateKeyPath"] {
                match object.get(key) {
                    Some(value) if value.is_string() => {
                        safe.insert(key.into(), value.clone());
                    }
                    Some(serde_json::Value::Number(number)) if key == "port" => {
                        safe.insert(key.into(), serde_json::Value::String(number.to_string()));
                    }
                    _ => {}
                }
            }
            Ok(serde_json::Value::Object(safe))
        })
        .collect::<Result<Vec<_>, InvalidNetworkSettings>>()
        .map(serde_json::Value::Array)
}

/// The jump list of a record or a file is not a list of at most `MAX_JUMPS` objects.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct InvalidNetworkSettings;

impl std::fmt::Display for InvalidNetworkSettings {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("Invalid saved connection network settings")
    }
}

#[derive(serde::Serialize, serde::Deserialize)]
struct ConnectionConfigBundle {
    version: u8,
    connections: Vec<ConnectionConfig>,
}

#[derive(serde::Serialize, serde::Deserialize)]
struct ConnectionConfig {
    name: String,
    db_type: String,
    uri: String,
    color_hex: String,
    network_settings: serde_json::Value,
}

pub fn export_connection_configs(records: &[SavedConnectionRecord]) -> Result<Vec<u8>, String> {
    let connections = records
        .iter()
        .map(|record| {
            Ok(ConnectionConfig {
                name: record.name.clone(),
                db_type: record.db_type.clone(),
                uri: public_connection_uri(&record.uri)?,
                color_hex: record.color_hex.clone(),
                network_settings: redacted_network_settings(&record.network_settings_json, false)?,
            })
        })
        .collect::<Result<Vec<_>, String>>()?;
    serde_json::to_vec_pretty(&ConnectionConfigBundle {
        version: 1,
        connections,
    })
    .map_err(|_| "Could not encode connection configuration".into())
}

pub fn stage_connection_configs(
    json: &[u8],
    timestamp: f64,
) -> Result<Vec<SavedConnectionRecord>, String> {
    if json.len() > 1_048_576 {
        return Err("Connection configuration is too large".into());
    }
    let bundle: ConnectionConfigBundle =
        serde_json::from_slice(json).map_err(|_| "Invalid connection configuration JSON")?;
    if bundle.version != 1 || bundle.connections.is_empty() || bundle.connections.len() > 100 {
        return Err("Unsupported connection configuration version or count".into());
    }
    bundle
        .connections
        .into_iter()
        .map(|config| {
            let kind = ConnectionKind::from_persisted(&config.db_type)
                .filter(ConnectionKind::is_supported)
                .ok_or("Unsupported connection type")?;
            if config.name.trim().is_empty() || config.name.len() > 200 {
                return Err("Invalid connection name".into());
            }
            if config.uri != public_connection_uri(&config.uri)? {
                return Err(
                    "Connection configuration contains credentials; remove them before importing"
                        .into(),
                );
            }
            validate_uri(kind, &config.uri)?;
            // An empty color is the "no color" swatch.
            if !config.color_hex.is_empty()
                && (config.color_hex.len() != 7
                    || !config.color_hex.starts_with('#')
                    || !config.color_hex.as_bytes()[1..]
                        .iter()
                        .all(u8::is_ascii_hexdigit))
            {
                return Err("Invalid connection color".into());
            }
            if !config.network_settings.is_object() {
                return Err("Invalid connection network settings".into());
            }
            // Import resets `allowLegacyAlgorithms`: the staged settings never carry it.
            let mut incoming = config.network_settings.clone();
            if let Some(ssh) = incoming
                .get_mut("ssh")
                .and_then(serde_json::Value::as_object_mut)
            {
                ssh.remove("allowLegacyAlgorithms");
            }
            // Files exported before the proxy had a switch for saved secrets have none.
            if let Some(proxy) = incoming
                .get_mut("proxy")
                .and_then(serde_json::Value::as_object_mut)
            {
                proxy
                    .entry("saveSecrets")
                    .or_insert(serde_json::Value::Bool(false));
            }
            let settings = incoming.to_string();
            if incoming != redacted_network_settings(&settings, false)? {
                return Err("Connection configuration contains network credentials".into());
            }
            Ok(SavedConnectionRecord {
                id: Uuid::new_v4().to_string().to_ascii_uppercase(),
                name: config.name.trim().into(),
                uri: config.uri,
                db_type: config.db_type,
                color_hex: config.color_hex,
                network_settings_json: settings,
                save_password: false,
                save_proxy_password: false,
                is_pinned: false,
                last_connected: None,
                created_at: timestamp,
                updated_at: timestamp,
                connection_type: String::new(),
                overrides_json: String::new(),
            })
        })
        .collect()
}

/// Compares persisted URIs after removing secrets and normalizing aliases.
pub fn persisted_uris_match(kind: ConnectionKind, left: &str, right: &str) -> bool {
    fn canonical(kind: ConnectionKind, uri: &str) -> Option<String> {
        let uri = sanitized_uri(uri).ok()?;
        if kind == ConnectionKind::Mongodb {
            let Ok(mut parsed) = url::Url::parse(&uri) else {
                return Some(canonicalize_mongodb_query(&uri));
            };
            let mut query = parsed
                .query_pairs()
                .map(|(key, value)| (key.into_owned(), value.into_owned()))
                .collect::<Vec<_>>();
            query.sort_unstable();
            parsed.set_query(None);
            if !query.is_empty() {
                let mut pairs = parsed.query_pairs_mut();
                for (key, value) in query {
                    pairs.append_pair(&key, &value);
                }
            }
            return Some(parsed.to_string());
        }
        if kind != ConnectionKind::Postgres {
            return Some(uri);
        }
        let mut parsed = url::Url::parse(&uri).ok()?;
        if parsed.scheme() == "postgres" {
            parsed.set_scheme("postgresql").ok()?;
            return Some(parsed.to_string());
        }
        Some(uri)
    }

    match (canonical(kind, left), canonical(kind, right)) {
        (Some(left), Some(right)) => left == right,
        _ => false,
    }
}

fn canonicalize_mongodb_query(uri: &str) -> String {
    let Some(query_start) = uri.find('?') else {
        return uri.to_string();
    };
    let query_end = uri[query_start + 1..]
        .find('#')
        .map(|offset| query_start + 1 + offset)
        .unwrap_or(uri.len());
    let mut options = uri[query_start + 1..query_end]
        .split('&')
        .collect::<Vec<_>>();
    options.sort_unstable();
    let mut canonical = String::with_capacity(uri.len());
    canonical.push_str(&uri[..query_start + 1]);
    canonical.push_str(&options.join("&"));
    canonical.push_str(&uri[query_end..]);
    canonical
}

/// Rebuilds the runtime URI with the Keychain password placed into the
/// userinfo section. The `url` crate percent-encodes userinfo automatically,
/// matching Swift `ConnectionCredentialResolver.runtimeURI` behavior.
pub fn runtime_uri(uri: &str, password: &str) -> Result<String, String> {
    let uri = normalize_uri_scheme(uri);
    if password.is_empty() {
        return Ok(uri);
    }
    if is_mongodb_uri(&uri) {
        let (authority_start, authority_end) = mongodb_authority_bounds(&uri)?;
        let authority = &uri[authority_start..authority_end];
        let encoded_password = encode_mongodb_password(password)?;
        let mut runtime = String::with_capacity(uri.len() + encoded_password.len() + 1);
        runtime.push_str(&uri[..authority_start]);
        if let Some(at) = authority.rfind('@') {
            let username = authority[..at].split(':').next().unwrap_or_default();
            runtime.push_str(username);
            runtime.push(':');
            runtime.push_str(&encoded_password);
            runtime.push('@');
            runtime.push_str(&authority[at + 1..]);
        } else {
            runtime.push(':');
            runtime.push_str(&encoded_password);
            runtime.push('@');
            runtime.push_str(authority);
        }
        runtime.push_str(&uri[authority_end..]);
        return Ok(runtime);
    }
    let mut url = url::Url::parse(&uri).map_err(|_| "Invalid connection URI".to_string())?;
    url.set_password(Some(password))
        .map_err(|_| "Invalid connection URI".to_string())?;
    Ok(url.to_string())
}

/// Validates that a URI matches the persisted connection kind's scheme.
pub fn validate_uri(kind: ConnectionKind, uri: &str) -> Result<(), String> {
    if kind == ConnectionKind::Sqlite
        && uri
            .get(..9)
            .is_some_and(|scheme| scheme.eq_ignore_ascii_case("sqlite://"))
    {
        return Ok(());
    }
    if kind == ConnectionKind::Mongodb && is_mongodb_uri(uri) {
        mongodb_authority_bounds(uri).map(|_| ())?;
        return Ok(());
    }
    let url = url::Url::parse(uri).map_err(|_| "Invalid connection URI".to_string())?;
    let scheme = url.scheme();
    let expected = kind.expected_scheme();
    let scheme_ok = match kind {
        ConnectionKind::Postgres => scheme == "postgres" || scheme == "postgresql",
        ConnectionKind::Sqlite => scheme == "sqlite",
        ConnectionKind::Mongodb => scheme == "mongodb" || scheme == "mongodb+srv",
        ConnectionKind::Mysql => scheme == "mysql",
        ConnectionKind::Mariadb => scheme == "mariadb",
        ConnectionKind::Redis => matches!(scheme, "redis" | "rediss"),
        // The driver also accepts the plain HTTP(S) interface URLs.
        ConnectionKind::Clickhouse => matches!(scheme, "clickhouse" | "http" | "https"),
    };
    if !scheme_ok {
        return Err(format!(
            "URI scheme '{scheme}' does not match connection type (expected '{expected}')"
        ));
    }
    Ok(())
}

fn is_mongodb_uri(uri: &str) -> bool {
    uri.get(..10)
        .is_some_and(|scheme| scheme.eq_ignore_ascii_case("mongodb://"))
        || uri
            .get(..14)
            .is_some_and(|scheme| scheme.eq_ignore_ascii_case("mongodb+srv://"))
}

fn mongodb_authority_bounds(uri: &str) -> Result<(usize, usize), String> {
    let authority_start = uri
        .find("://")
        .map(|index| index + 3)
        .ok_or_else(|| "Invalid connection URI".to_string())?;
    let authority_end = uri[authority_start..]
        .find(['/', '?', '#'])
        .map(|offset| authority_start + offset)
        .unwrap_or(uri.len());
    let authority = &uri[authority_start..authority_end];
    let hosts = authority
        .rsplit_once('@')
        .map(|(_, hosts)| hosts)
        .unwrap_or(authority);
    if hosts.is_empty() {
        return Err("Invalid connection URI".to_string());
    }
    Ok((authority_start, authority_end))
}

fn encode_mongodb_password(password: &str) -> Result<String, String> {
    let mut url = url::Url::parse("mongodb://tablex@host.invalid")
        .map_err(|_| "Invalid connection URI".to_string())?;
    url.set_password(Some(password))
        .map_err(|_| "Invalid connection URI".to_string())?;
    let authority = url
        .as_str()
        .strip_prefix("mongodb://")
        .and_then(|value| value.split_once('@'))
        .map(|(userinfo, _)| userinfo)
        .ok_or_else(|| "Invalid connection URI".to_string())?;
    authority
        .split_once(':')
        .map(|(_, password)| password.to_string())
        .ok_or_else(|| "Invalid connection URI".to_string())
}

/// The forms `password` takes inside a runtime URI, taken from [`runtime_uri`] itself (the code that
/// builds the URI the driver gets: the `url` crate's user-info encoding, and the MongoDB path), not
/// re-implemented: they differ from a plain percent-encoding (`! $ & ' ( ) * + ,` stay as they are).
/// Without the raw password itself; empty for a password that needs no encoding.
pub fn encoded_password_forms(password: &str) -> Vec<String> {
    let mut forms = Vec::new();
    for template in ["postgresql://u@h/d", "mongodb://u@h/d"] {
        let Ok(uri) = runtime_uri(template, password) else {
            continue;
        };
        let form = uri
            .split_once("://")
            .and_then(|(_, rest)| rest.split_once('@'))
            .and_then(|(userinfo, _)| userinfo.split_once(':'))
            .map(|(_, encoded)| encoded.to_owned());
        if let Some(form) = form.filter(|form| form != password && !forms.contains(form)) {
            forms.push(form);
        }
    }
    forms
}

pub use crate::credentials::SecretKind;
use crate::credentials::{CredentialStore, SystemStore};

/// A connection URI that may carry a password: wiped when dropped, `Debug` prints nothing of it,
/// and its text can only be read inside this crate (to hand it to a driver, the tunnel plan or
/// [`RuntimeUri::tool_endpoint`], which gives a native tool's connection as parts with the password as a
/// [`engine_net::Secret`]). The app keeps the one of an open session in this type to open the session's siblings.
///
/// ```compile_fail,E0624
/// fn read(uri: engine_connect::records::RuntimeUri) {
///     let _ = uri.expose(); // crate-private
/// }
/// ```
#[derive(Clone)]
pub struct RuntimeUri(zeroize::Zeroizing<String>);

impl RuntimeUri {
    pub fn new(uri: impl Into<String>) -> Self {
        Self(zeroize::Zeroizing::new(uri.into()))
    }

    pub(crate) fn expose(&self) -> &str {
        &self.0
    }

    /// The text, for tests only.
    #[cfg(any(test, feature = "testing"))]
    pub fn expose_for_test(&self) -> &str {
        &self.0
    }
}

impl From<String> for RuntimeUri {
    fn from(uri: String) -> Self {
        Self::new(uri)
    }
}

impl From<&str> for RuntimeUri {
    fn from(uri: &str) -> Self {
        Self::new(uri)
    }
}

impl std::fmt::Debug for RuntimeUri {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("RuntimeUri(<hidden>)")
    }
}

/// Why the connectable URI of a saved connection could not be made. The messages are the ones the
/// front ends show (and [`is_missing_password_error`] reads).
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub(crate) enum RuntimeUriError {
    /// The connection saves its password and the store has none.
    #[error("{}", missing_password_error())]
    MissingPassword,
    /// The persisted URI cannot carry the password (see [`runtime_uri`]).
    #[error("{0}")]
    Invalid(String),
}

/// Resolves the connectable URI for a saved connection: the persisted URI
/// plus its saved password when the user opted into saving it.
pub fn resolve_runtime_uri(conn: &SavedConnection) -> Result<RuntimeUri, String> {
    resolve_runtime_uri_with(conn, &SystemStore::new())
        .map(RuntimeUri::new)
        .map_err(|error| error.to_string())
}

/// [`resolve_runtime_uri`] over any credential store (the Connector's).
pub(crate) fn resolve_runtime_uri_with(
    conn: &SavedConnection,
    store: &dyn CredentialStore,
) -> Result<String, RuntimeUriError> {
    let saved = conn.save_password.then(|| {
        let account = conn.id.to_string();
        let load = |account: &str| store.load(SecretKind::Database, account).ok().flatten();
        load(&account.to_uppercase()).or_else(|| load(&account))
    });
    resolve_runtime_uri_from(conn, saved.flatten().as_deref())
}

/// The connectable URI of a saved connection from a password that was already read (`None`: the
/// store has none). Only a connection that saves its password needs one.
pub(crate) fn resolve_runtime_uri_from(
    conn: &SavedConnection,
    password: Option<&str>,
) -> Result<String, RuntimeUriError> {
    let password = if conn.save_password {
        password.ok_or(RuntimeUriError::MissingPassword)?
    } else {
        ""
    };
    runtime_uri(&conn.uri, password)
        .map(with_default_postgres_database)
        .map_err(RuntimeUriError::Invalid)
}

/// A PostgreSQL URI without a database makes libpq/tokio-postgres connect to a
/// database named after the user, which usually does not exist. The form
/// treats Database as optional, so default to the `postgres` maintenance DB
/// unless the URI names one via path or `dbname=`.
pub fn with_default_postgres_database(uri: String) -> String {
    let lower = uri.get(..13).unwrap_or(&uri).to_ascii_lowercase();
    if !(lower.starts_with("postgres://") || lower.starts_with("postgresql://")) {
        return uri;
    }
    let Ok(mut url) = url::Url::parse(&uri) else {
        return uri;
    };
    let has_path_db = !url.path().trim_matches('/').is_empty();
    let has_query_db = url.query_pairs().any(|(key, _)| key == "dbname");
    if has_path_db || has_query_db {
        return uri;
    }
    url.set_path("/postgres");
    url.to_string()
}

/// Removes userinfo credentials from driver error text before it reaches the
/// UI: drivers echo connection strings in some failure modes.
pub fn redact_secrets(message: &str) -> String {
    let mut redacted = message.to_string();
    for marker in [
        "postgres://",
        "postgresql://",
        "sqlite://",
        "mongodb://",
        "mongodb+srv://",
        "mysql://",
        "mariadb://",
        "redis://",
        "rediss://",
        "clickhouse://",
        "http://",
        "https://",
    ] {
        while let Some(start) = redacted.find(marker) {
            let after = &redacted[start + marker.len()..];
            // The URL ends at whitespace or a quote, or at the end of the text.
            let token = after
                .find(|c: char| c.is_whitespace() || c == '"' || c == '\'')
                .unwrap_or(after.len());
            let end = start + marker.len() + token;
            redacted.replace_range(start..end, "<redacted-uri>");
        }
    }
    redacted
}

/// Adds the global connect timeout (Settings › Query & Connections) to a
/// PostgreSQL / MongoDB URI that carries none of its own; `0` and other
/// connection kinds leave the URI as it is.
pub fn with_default_connect_timeout(uri: String, kind: ConnectionKind, seconds: u32) -> String {
    let (key, value) = match kind {
        ConnectionKind::Postgres => ("connect_timeout", u64::from(seconds)),
        ConnectionKind::Mongodb => ("connectTimeoutMS", u64::from(seconds) * 1000),
        _ => return uri,
    };
    if seconds == 0
        || uri
            .to_ascii_lowercase()
            .contains(&format!("{}=", key.to_ascii_lowercase()))
    {
        return uri;
    }
    let mut uri = uri;
    let after_scheme = uri.find("://").map_or(0, |at| at + 3);
    let has_path_or_query = uri[after_scheme..].contains(['/', '?']);
    if !has_path_or_query && kind == ConnectionKind::Mongodb {
        uri.push('/');
    }
    uri.push(if uri.contains('?') { '&' } else { '?' });
    uri.push_str(&format!("{key}={value}"));
    uri
}

/// A plain saved record for tests in other modules (and the app's, with the `testing` feature).
#[cfg(any(test, feature = "testing"))]
pub fn tests_record_for(id: &str) -> SavedConnectionRecord {
    SavedConnectionRecord {
        id: id.to_string(),
        name: "fixture".into(),
        uri: "postgresql://localhost/app".into(),
        db_type: DB_TYPE_POSTGRESQL.to_string(),
        color_hex: "#34C759".to_string(),
        save_password: false,
        save_proxy_password: false,
        network_settings_json: "{}".to_string(),
        is_pinned: false,
        last_connected: None,
        created_at: 1.0,
        updated_at: 1.0,
        connection_type: String::new(),
        overrides_json: String::new(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use engine_storage::connection::SavedConnectionRecord;

    fn record(id: &str, name: &str, db_type: &str, uri: &str) -> SavedConnectionRecord {
        SavedConnectionRecord {
            id: id.to_string(),
            name: name.to_string(),
            uri: uri.to_string(),
            db_type: db_type.to_string(),
            color_hex: "#34C759".to_string(),
            save_password: false,
            save_proxy_password: false,
            network_settings_json: r#"{"ssh":{"isEnabled":false},"proxy":{"isEnabled":false}}"#
                .to_string(),
            is_pinned: false,
            last_connected: None,
            created_at: 1_700_000_000.0,
            updated_at: 1_700_000_000.0,
            connection_type: String::new(),
            overrides_json: String::new(),
        }
    }

    #[test]
    fn global_connect_timeout_fills_in_only_when_the_uri_has_none() {
        use super::with_default_connect_timeout as with;
        assert_eq!(
            with("postgres://u@h/db".into(), ConnectionKind::Postgres, 7),
            "postgres://u@h/db?connect_timeout=7"
        );
        assert_eq!(
            with(
                "postgres://u@h/db?sslmode=require".into(),
                ConnectionKind::Postgres,
                7
            ),
            "postgres://u@h/db?sslmode=require&connect_timeout=7"
        );
        assert_eq!(
            with("mongodb://h".into(), ConnectionKind::Mongodb, 5),
            "mongodb://h/?connectTimeoutMS=5000"
        );
        // The connection's own timeout, zero, and other kinds win.
        let own = "postgres://u@h/db?connect_timeout=3";
        assert_eq!(with(own.into(), ConnectionKind::Postgres, 7), own);
        assert_eq!(
            with("postgres://h/db".into(), ConnectionKind::Postgres, 0),
            "postgres://h/db"
        );
        assert_eq!(
            with("sqlite:///x.db".into(), ConnectionKind::Sqlite, 9),
            "sqlite:///x.db"
        );
    }

    #[test]
    fn redaction_covers_every_driver_scheme() {
        for uri in [
            "clickhouse://default:hunter2@ch.example.com:8123/db",
            "https://default:hunter2@ch.example.com:8443/db",
            "http://default:hunter2@ch.example.com/db",
            "rediss://default:hunter2@cache.example.com:6380/0",
            "redis://default:hunter2@cache.example.com/0",
        ] {
            let redacted = redact_secrets(&format!("connect failed for {uri} (refused)"));
            assert!(!redacted.contains("hunter2"), "{redacted}");
            assert!(
                redacted.contains("<redacted-uri>") && redacted.ends_with("(refused)"),
                "{redacted}"
            );
        }
    }

    #[test]
    fn redaction_keeps_the_rest_of_a_message_that_ends_with_a_url() {
        for (message, expected) in [
            (
                "could not connect to postgres://u:hunter2@db.example.com:5432/app",
                "could not connect to <redacted-uri>",
            ),
            (
                "failed: redis://default:hunter2@cache.example.com/0",
                "failed: <redacted-uri>",
            ),
            (
                "see https://user:hunter2@host/x then retry",
                "see <redacted-uri> then retry",
            ),
            (
                "clickhouse://default:hunter2@ch.example.com",
                "<redacted-uri>",
            ),
            (
                "a user@example.com wrote postgres://u:hunter2@h/d",
                "a user@example.com wrote <redacted-uri>",
            ),
            (
                "no url here, user@example.com",
                "no url here, user@example.com",
            ),
        ] {
            assert_eq!(redact_secrets(message), expected, "{message}");
        }
    }

    #[test]
    fn redaction_handles_multiple_uris_and_trailing_text() {
        let redacted = redact_secrets(
            "dial postgres://alice:hunter2@db1:5432/x then postgres://bob:p@db2:5432/y failed",
        );
        assert!(!redacted.contains("hunter2"));
        assert!(!redacted.contains("alice:"));
        assert!(redacted.contains("<redacted-uri>"));
    }

    #[test]
    fn the_encoded_password_forms_are_the_ones_the_runtime_uri_carries() {
        let password = "pa!ss'w@rd%1 x(y)*+,";
        let forms = encoded_password_forms(password);
        assert!(!forms.is_empty());
        for uri in [
            runtime_uri("postgresql://u@h:5432/d", password).unwrap(),
            runtime_uri("mongodb://u@h1,h2/d", password).unwrap(),
        ] {
            assert!(
                forms.iter().any(|form| uri.contains(form.as_str())),
                "{uri} vs {forms:?}"
            );
        }
        // Not a plain percent-encoding: the characters the user-info set keeps are kept.
        assert!(
            forms
                .iter()
                .any(|form| form.contains('!') && form.contains('\'')),
            "{forms:?}"
        );
        assert!(forms.iter().all(|form| form != password));
        assert!(encoded_password_forms("plainpassword").is_empty());
    }

    #[test]
    fn maps_persisted_types_to_kinds() {
        assert_eq!(
            ConnectionKind::from_persisted(DB_TYPE_POSTGRESQL),
            Some(ConnectionKind::Postgres)
        );
        assert_eq!(
            ConnectionKind::from_persisted(DB_TYPE_SQLITE),
            Some(ConnectionKind::Sqlite)
        );
        assert_eq!(
            ConnectionKind::from_persisted(DB_TYPE_MONGODB),
            Some(ConnectionKind::Mongodb)
        );
        assert_eq!(
            ConnectionKind::from_persisted(DB_TYPE_MYSQL),
            Some(ConnectionKind::Mysql)
        );
        assert_eq!(
            ConnectionKind::from_persisted(DB_TYPE_MARIADB),
            Some(ConnectionKind::Mariadb)
        );
        assert_eq!(
            ConnectionKind::from_persisted(DB_TYPE_REDIS),
            Some(ConnectionKind::Redis)
        );
    }

    #[test]
    fn redis_type_loads_enabled() {
        let conn =
            SavedConnection::from_record(&record("c1", "My Redis", "redis", "redis://localhost"));
        assert!(!conn.disabled);
        assert_eq!(conn.kind, ConnectionKind::Redis);
        assert_eq!(conn.name, "My Redis");
    }

    #[test]
    fn supported_types_load_enabled() {
        let conn = SavedConnection::from_record(&record(
            "c2",
            "Local PG",
            DB_TYPE_POSTGRESQL,
            "postgresql://localhost/app",
        ));
        assert!(!conn.disabled);
        assert_eq!(conn.kind, ConnectionKind::Postgres);
    }

    #[test]
    fn sql_family_loads_enabled_and_maps_to_its_driver_id() {
        for (db_type, kind, driver_id) in [
            (DB_TYPE_POSTGRESQL, ConnectionKind::Postgres, "postgresql"),
            (DB_TYPE_SQLITE, ConnectionKind::Sqlite, "sqlite"),
            (DB_TYPE_MYSQL, ConnectionKind::Mysql, "mysql"),
            (DB_TYPE_MARIADB, ConnectionKind::Mariadb, "mariadb"),
        ] {
            let conn = SavedConnection::from_record(&record("c", "n", db_type, "x://h/d"));
            assert!(!conn.disabled, "{db_type} loads enabled");
            assert_eq!(conn.kind, kind);
            assert_eq!(
                kind.driver_id(),
                driver_id,
                "installed driver id of {db_type}"
            );
        }
        // PostgreSQL's URI scheme is not its driver id.
        assert_eq!(ConnectionKind::Postgres.expected_scheme(), "postgres");
    }

    #[test]
    fn mongodb_type_loads_enabled() {
        let conn = SavedConnection::from_record(&record(
            "c3",
            "My Mongo",
            "mongodb",
            "mongodb://localhost",
        ));
        assert!(!conn.disabled);
        assert_eq!(conn.kind, ConnectionKind::Mongodb);
    }

    #[test]
    fn validates_mongodb_scheme() {
        assert!(validate_uri(ConnectionKind::Mongodb, "mongodb://localhost:27017").is_ok());
        assert!(validate_uri(ConnectionKind::Mongodb, "mongodb+srv://cluster.example.com").is_ok());
        assert!(validate_uri(
            ConnectionKind::Mongodb,
            "mongodb://root@192.0.2.11:27017,192.0.2.12:27017,192.0.2.13:27017/"
        )
        .is_ok());
        assert!(validate_uri(ConnectionKind::Mongodb, "postgresql://localhost").is_err());
    }

    #[test]
    fn matches_swift_uuid_text_case_insensitively_by_parsing() {
        let conn = SavedConnection::from_record(&record(
            "11111111-1111-1111-1111-111111111111",
            "Local PG",
            DB_TYPE_POSTGRESQL,
            "postgresql://localhost/app",
        ));
        assert!(conn.matches_id("11111111-1111-1111-1111-111111111111"));
        assert!(conn.matches_id(
            "11111111-1111-1111-1111-111111111111"
                .to_uppercase()
                .as_str()
        ));
        assert!(!conn.matches_id("not-a-uuid"));
    }

    #[test]
    fn a_saved_password_that_is_not_in_the_store_is_a_typed_missing_password() {
        let mut saved = SavedConnection::from_record(&SavedConnectionRecord {
            id: "ABCDEFAB-1234-4ABC-8DEF-0123456789AB".into(),
            name: "n".into(),
            uri: "postgresql://alice@db.example.com:5432/app".into(),
            db_type: DB_TYPE_POSTGRESQL.into(),
            color_hex: String::new(),
            save_password: true,
            save_proxy_password: false,
            network_settings_json: "{}".into(),
            is_pinned: false,
            last_connected: None,
            created_at: 0.0,
            updated_at: 0.0,
            connection_type: String::new(),
            overrides_json: String::new(),
        });
        let missing = resolve_runtime_uri_from(&saved, None).unwrap_err();
        assert_eq!(missing, RuntimeUriError::MissingPassword);
        assert!(is_missing_password_error(&missing.to_string()));
        let uri = resolve_runtime_uri_from(&saved, Some("pw")).unwrap();
        assert!(
            uri.starts_with("postgresql://alice:pw@db.example.com"),
            "{uri}"
        );
        // A connection that does not save its password needs none.
        saved.save_password = false;
        assert!(resolve_runtime_uri_from(&saved, None).is_ok());
        // A URI that cannot carry a password is the other typed case, with the old text.
        saved.save_password = true;
        saved.uri = "not a uri".into();
        let invalid = resolve_runtime_uri_from(&saved, Some("pw")).unwrap_err();
        assert_eq!(
            invalid,
            RuntimeUriError::Invalid("Invalid connection URI".into())
        );
        assert_eq!(invalid.to_string(), "Invalid connection URI");
    }

    #[test]
    fn storage_connection_id_matches_persisted_uuid_case() {
        assert_eq!(
            storage_connection_id("abcdefab-1234-4abc-8def-0123456789ab"),
            "ABCDEFAB-1234-4ABC-8DEF-0123456789AB"
        );
        assert_eq!(storage_connection_id("not-a-uuid"), "not-a-uuid");
    }

    #[test]
    fn sanitizes_and_rehydrates_passwords_with_special_characters() {
        let original = "postgresql://user:p@ss:word@host:5432/app";

        let sanitized = sanitized_uri(original).expect("sanitize");
        assert_eq!(sanitized, "postgresql://user@host:5432/app");
        assert!(!sanitized.contains("p@ss"), "password removed for storage");

        let runtime = runtime_uri(&sanitized, "p@ss:word").expect("runtime uri");
        assert!(
            runtime.contains("user:p%40ss%3Aword@host:5432/app"),
            "password must be percent-encoded into userinfo: {runtime}"
        );
        assert!(
            !runtime.contains("p@ss:word@"),
            "raw special characters must not appear unencoded: {runtime}"
        );
    }

    #[test]
    fn postgres_scheme_aliases_match_for_saved_password_reuse() {
        assert!(persisted_uris_match(
            ConnectionKind::Postgres,
            "postgres://alice@localhost/app",
            "postgresql://alice@localhost/app",
        ));
    }

    #[test]
    fn mongodb_query_order_does_not_break_saved_password_reuse() {
        assert!(persisted_uris_match(
            ConnectionKind::Mongodb,
            "mongodb://alice@localhost/app?authSource=admin&retryWrites=true",
            "mongodb://alice@localhost/app?retryWrites=true&authSource=admin",
        ));
        assert!(persisted_uris_match(
            ConnectionKind::Mongodb,
            "mongodb://alice@cluster-a:27017,cluster-b:27017/app?authSource=admin&retryWrites=true",
            "mongodb://alice@cluster-a:27017,cluster-b:27017/app?retryWrites=true&authSource=admin",
        ));
        assert!(!persisted_uris_match(
            ConnectionKind::Mongodb,
            "mongodb://cluster-a:27017,cluster-b:27017/app",
            "mongodb://cluster-c:27017,cluster-d:27017/app",
        ));
        assert!(!persisted_uris_match(
            ConnectionKind::Postgres,
            "not-a-uri",
            "not-a-uri",
        ));
    }

    #[test]
    fn empty_password_runtime_uri_is_the_persisted_uri() {
        let stored = "sqlite:///Users/dev/app.db";
        assert_eq!(runtime_uri(stored, "").unwrap(), stored);
    }

    #[test]
    fn runtime_uri_normalizes_sqlite_scheme_case() {
        assert_eq!(
            runtime_uri("SQLITE:///Users/dev/app.db", "").unwrap(),
            "sqlite:///Users/dev/app.db"
        );
    }

    #[test]
    fn runtime_uri_normalizes_mongodb_scheme_case() {
        assert_eq!(
            runtime_uri("MONGODB://mongo.example.test/app", "").unwrap(),
            "mongodb://mongo.example.test/app"
        );
    }

    #[test]
    fn rehydrates_password_in_mongodb_seed_list() {
        let stored = "mongodb://root@192.0.2.11:27017,192.0.2.12:27017,192.0.2.13:27017/";

        let runtime = runtime_uri(stored, "p@ss:word").expect("runtime uri");

        assert_eq!(
            runtime,
            "mongodb://root:p%40ss%3Aword@192.0.2.11:27017,192.0.2.12:27017,192.0.2.13:27017/"
        );
    }

    #[test]
    fn sanitizes_password_in_mongodb_seed_list() {
        let original = "mongodb://root:p%40ss%3Aword@192.0.2.11:27017,192.0.2.12:27017/";

        assert_eq!(
            sanitized_uri(original).expect("sanitized uri"),
            "mongodb://root@192.0.2.11:27017,192.0.2.12:27017/"
        );
    }

    #[test]
    fn validates_scheme_against_connection_kind() {
        assert!(validate_uri(ConnectionKind::Postgres, "postgres://h/db").is_ok());
        assert!(validate_uri(ConnectionKind::Postgres, "postgresql://h/db").is_ok());
        assert!(validate_uri(ConnectionKind::Postgres, "sqlite://x.db").is_err());
        assert!(validate_uri(ConnectionKind::Sqlite, "sqlite://x.db").is_ok());
        assert!(validate_uri(ConnectionKind::Sqlite, "mysql://h/db").is_err());
        assert!(validate_uri(ConnectionKind::Sqlite, "not-a-uri").is_err());
        assert!(validate_uri(ConnectionKind::Mysql, "mysql://h/db").is_ok());
        assert!(validate_uri(ConnectionKind::Mariadb, "mariadb://h/db").is_ok());
        assert!(validate_uri(ConnectionKind::Redis, "redis://h/0").is_ok());
        assert!(validate_uri(ConnectionKind::Redis, "rediss://h:6380/2").is_ok());
        assert!(validate_uri(ConnectionKind::Redis, "mysql://h/db").is_err());
    }

    #[test]
    fn sanitizes_driver_passwords_for_all_sql_and_redis_schemes() {
        for uri in [
            "mysql://user:secret@host/db",
            "mariadb://user:secret@host/db",
            "redis://user:secret@host/0",
        ] {
            let sanitized = sanitized_uri(uri).expect("sanitize URI");
            assert!(
                !sanitized.contains("secret"),
                "password leaked in {sanitized}"
            );
        }
    }

    #[test]
    fn sanitize_rejects_invalid_uris_without_leaking_them() {
        let error = sanitized_uri("::not a uri::").expect_err("invalid");
        assert_eq!(error, "Invalid connection URI");
        assert!(!error.contains("not a uri"));
    }

    #[test]
    fn public_uri_drops_secret_query_options_but_keeps_routing_options() {
        let uri = "mongodb://alice:secret@localhost/app?authSource=admin&accessToken=topsecret&retryWrites=true#private";
        assert_eq!(
            public_connection_uri(uri).unwrap(),
            "mongodb://alice@localhost/app?authSource=admin&retryWrites=true"
        );
        assert_eq!(
            public_connection_uri("mongodb://host/app?custom=unknown-value&tls=true").unwrap(),
            "mongodb://host/app?tls=true"
        );
    }

    #[test]
    fn a_copied_or_exported_mysql_uri_keeps_its_ssl_mode_but_not_its_certificate_paths() {
        // Dropping `ssl-mode` would silently weaken VERIFY_IDENTITY to the PREFERRED default.
        assert_eq!(
            public_connection_uri(
                "mysql://u:secret@h/db?ssl-mode=VERIFY_IDENTITY&ssl-ca=%2Fca.pem&ssl-key=%2Fkey.pem"
            )
            .unwrap(),
            "mysql://u@h/db?ssl-mode=VERIFY_IDENTITY"
        );
    }

    #[test]
    fn sqlite_hash_filename_survives_copy_duplicate_export_and_import_while_query_is_removed() {
        let uri =
            "sqlite:///tmp/accounts#archive.db?sqlite_extension=%2Ftmp%2Fsecret.dylib&token=secret";
        let expected = "sqlite:///tmp/accounts#archive.db";
        assert_eq!(public_connection_uri(uri).unwrap(), expected);
        let source = record(
            "11111111-1111-1111-1111-111111111111",
            "Local",
            DB_TYPE_SQLITE,
            uri,
        );
        let duplicate =
            duplicate_record(&source, "22222222-2222-2222-2222-222222222222", 2.0).unwrap();
        assert_eq!(duplicate.uri, expected);
        let export = export_connection_configs(&[source]).unwrap();
        let text = String::from_utf8(export.clone()).unwrap();
        assert!(text.contains("accounts#archive.db"));
        assert!(!text.contains("secret"));
        let imported = stage_connection_configs(&export, 3.0).unwrap();
        assert_eq!(imported[0].uri, expected);
        assert_eq!(runtime_uri(&imported[0].uri, "").unwrap(), expected);
    }

    #[test]
    fn duplicate_record_never_copies_keychain_flags_or_embedded_secrets() {
        let mut source = record(
            "11111111-1111-1111-1111-111111111111",
            "Production",
            DB_TYPE_POSTGRESQL,
            "postgresql://alice:secret@host/app?accessToken=topsecret",
        );
        source.save_password = true;
        source.save_proxy_password = true;
        source.network_settings_json =
            r#"{"proxy":{"host":"proxy","password":"proxy-secret","opaque":"unknown-secret"}}"#
                .into();
        let copied =
            duplicate_record(&source, "22222222-2222-2222-2222-222222222222", 42.0).unwrap();
        assert_eq!(copied.name, "Production copy");
        assert!(!copied.save_password && !copied.save_proxy_password);
        assert_eq!(copied.uri, "postgresql://alice@host/app");
        assert!(!copied.network_settings_json.contains("proxy-secret"));
        assert!(!copied.network_settings_json.contains("unknown-secret"));
        assert_eq!(copied.last_connected, None);
        assert!(!copied.is_pinned);
        assert_eq!(source.name, "Production");
    }

    #[test]
    fn duplicating_a_connection_resets_agent_access_to_off() {
        let mut source = record(
            "11111111-1111-1111-1111-111111111111",
            "Warehouse",
            DB_TYPE_POSTGRESQL,
            "postgresql://alice@host/app",
        );
        source.connection_type = "production".into();
        source.overrides_json =
            r#"{"agent.access":"read_write","results.page_size":50,"query.query_timeout_secs":30}"#
                .into();
        let copied =
            duplicate_record(&source, "22222222-2222-2222-2222-222222222222", 42.0).unwrap();
        let profile = crate::profile::ConnectionProfile::from_record(&copied);
        assert_eq!(
            profile.agent_level(),
            None,
            "the copy has no level of its own"
        );
        assert!(
            !copied.overrides_json.contains("agent"),
            "{}",
            copied.overrides_json
        );
        let settings = crate::profile::ProfileSettings::default();
        assert_eq!(
            settings.effective(&profile).agent_access,
            crate::profile::AgentAccess::Off,
            "so agents are off on the copy"
        );
        // Every other override and the type survive; the source is untouched.
        assert_eq!(
            profile.overrides.get("results.page_size"),
            Some(&crate::profile::OverrideValue::Number(50.0))
        );
        assert_eq!(
            profile.overrides.get("query.query_timeout_secs"),
            Some(&crate::profile::OverrideValue::Number(30.0))
        );
        assert_eq!(copied.connection_type, "production");
        assert!(source.overrides_json.contains("agent.access"));
        // Nothing but the level in the overrides: nothing is stored at all.
        source.overrides_json = r#"{"agent.access":"read"}"#.into();
        let bare = duplicate_record(&source, "33333333-3333-3333-3333-333333333333", 1.0).unwrap();
        assert_eq!(bare.overrides_json, "");
    }

    #[test]
    fn export_and_import_staging_never_carry_agent_access_or_any_override() {
        let mut source = record(
            "11111111-1111-1111-1111-111111111111",
            "Warehouse",
            DB_TYPE_POSTGRESQL,
            "postgresql://alice@host/app",
        );
        source.connection_type = "production".into();
        source.overrides_json = r#"{"agent.access":"read_write"}"#.into();
        let exported = export_connection_configs(&[source]).unwrap();
        assert!(!String::from_utf8_lossy(&exported).contains("agent"));
        let staged = stage_connection_configs(&exported, 1.0).unwrap();
        assert_eq!(
            (
                staged[0].connection_type.as_str(),
                staged[0].overrides_json.as_str()
            ),
            ("", "")
        );
        // A file that smuggles the level in as a field is not read for it.
        let smuggled = String::from_utf8(exported.clone()).unwrap().replacen(
            "\"name\"",
            "\"overrides_json\":\"{\\\"agent.access\\\":\\\"read_write\\\"}\",\"name\"",
            1,
        );
        if let Ok(staged) = stage_connection_configs(smuggled.as_bytes(), 1.0) {
            assert_eq!(staged[0].overrides_json, "");
        }
    }

    #[test]
    fn config_export_and_staging_redact_secrets_and_reject_bad_input_before_storage() {
        let mut source = record(
            "11111111-1111-1111-1111-111111111111",
            "Production",
            DB_TYPE_POSTGRESQL,
            "postgresql://alice:secret@host/app?password=secret",
        );
        source.save_password = true;
        source.network_settings_json =
            r#"{"proxy":{"host":"proxy","password":"proxy-secret","opaque":"unknown-secret"}}"#
                .into();
        let export = export_connection_configs(&[source]).unwrap();
        let json = String::from_utf8(export.clone()).unwrap();
        assert!(!json.contains("secret"));
        assert!(!json.contains("save_password"));
        let staged = stage_connection_configs(&export, 1_700_000_001.0).unwrap();
        assert_eq!(staged.len(), 1);
        assert_eq!(staged[0].uri, "postgresql://alice@host/app");
        assert!(!staged[0].save_password);
        assert_ne!(staged[0].id, "11111111-1111-1111-1111-111111111111");
        let with_secret = json.replace("alice@host", "alice:secret@host");
        assert!(stage_connection_configs(with_secret.as_bytes(), 1.0).is_err());
        assert!(stage_connection_configs(br#"{"version":2,"connections":[]}"#, 1.0).is_err());
        assert!(stage_connection_configs(&vec![b' '; 1_100_000], 1.0).is_err());
    }

    const SSH_NETWORK_JSON: &str = r#"{"ssh":{"isEnabled":true,"host":"bastion.example.com","port":2200,"user":"deploy","authMethod":"key","privateKeyPath":"/keys/id_ed25519","configHost":"bastion","remoteHost":"10.0.0.9","remotePort":"5432","allowLegacyAlgorithms":true,"saveSecrets":true,"password":"ssh-secret","passphrase":"key-secret"},"proxy":{"host":"proxy.example.com","isEnabled":true}}"#;

    #[test]
    fn duplicating_keeps_the_tunnel_but_none_of_its_secrets() {
        let mut source = record(
            "11111111-1111-1111-1111-111111111111",
            "Behind a bastion",
            DB_TYPE_POSTGRESQL,
            "postgresql://alice@db.example.com/app",
        );
        source.network_settings_json = SSH_NETWORK_JSON.into();
        let copied =
            duplicate_record(&source, "22222222-2222-2222-2222-222222222222", 1.0).unwrap();
        let json = &copied.network_settings_json;
        assert!(
            !json.contains("ssh-secret") && !json.contains("key-secret"),
            "{json}"
        );
        let ssh = crate::tunnel::ssh::SshSettings::from_network_json(json);
        assert!(ssh.enabled, "a local duplicate keeps working");
        assert_eq!(
            (
                ssh.host.as_str(),
                ssh.port.as_str(),
                ssh.user.as_str(),
                ssh.key_path.as_str()
            ),
            ("bastion.example.com", "2200", "deploy", "/keys/id_ed25519")
        );
        assert_eq!(ssh.auth, crate::tunnel::ssh::SshAuthMethod::KeyFile);
        assert_eq!(
            (ssh.remote_host.as_str(), ssh.config_host.as_str()),
            ("10.0.0.9", "bastion")
        );
        assert!(ssh.allow_legacy_algorithms);
        assert!(
            !ssh.save_secrets,
            "the copy does not claim saved secrets, like save_password"
        );
        // Like the tunnel, the proxy of a local duplicate keeps working but not its secrets.
        assert!(
            json.contains(r#""proxy":{"host":"proxy.example.com","isEnabled":true,"saveSecrets":false}"#),
            "{json}"
        );
    }

    #[test]
    fn an_exported_or_imported_tunnel_is_off_and_carries_no_secret() {
        let mut source = record(
            "11111111-1111-1111-1111-111111111111",
            "Behind a bastion",
            DB_TYPE_POSTGRESQL,
            "postgresql://alice@db.example.com/app",
        );
        source.network_settings_json = SSH_NETWORK_JSON.into();
        let export = export_connection_configs(&[source]).unwrap();
        let text = String::from_utf8(export.clone()).unwrap();
        assert!(
            !text.contains("ssh-secret") && !text.contains("key-secret"),
            "{text}"
        );
        let staged = stage_connection_configs(&export, 5.0).unwrap();
        let ssh =
            crate::tunnel::ssh::SshSettings::from_network_json(&staged[0].network_settings_json);
        assert!(!ssh.enabled, "a file never switches a tunnel on by itself");
        assert_eq!(ssh.host, "bastion.example.com");
        assert_eq!(ssh.user, "deploy");
        // A smuggled secret or key in the file is rejected before anything is stored.
        let tampered = text.replace(
            r#""authMethod": "key""#,
            r#""authMethod": "key", "password": "x""#,
        );
        assert_ne!(tampered, text);
        assert!(stage_connection_configs(tampered.as_bytes(), 1.0).is_err());
    }

    const ROUTE_NETWORK_JSON: &str = r#"{"ssh":{"isEnabled":true,"host":"bastion.example.com","port":"22","user":"deploy","authMethod":"agent","jumps":[{"host":"jump1.example.com","port":"2201","user":"hop1","authMethod":"password","privateKeyPath":"","password":"jump-secret"},{"host":"jump2.example.com","port":2202,"user":"hop2","authMethod":"key","privateKeyPath":"/keys/j2","passphrase":"jump-phrase"}]},"proxy":{"isEnabled":true,"kind":"socks5","host":"proxy.example.com","port":"1080","user":"px","password":"proxy-secret"}}"#;

    #[test]
    fn jumps_and_the_proxy_travel_in_a_file_but_switched_off_and_without_secrets() {
        let mut source = record(
            "22222222-2222-2222-2222-222222222222",
            "Behind a chain",
            DB_TYPE_POSTGRESQL,
            "postgresql://alice@db.example.com/app",
        );
        source.network_settings_json = ROUTE_NETWORK_JSON.into();
        let export = export_connection_configs(&[source.clone()]).unwrap();
        let text = String::from_utf8(export.clone()).unwrap();
        for secret in ["jump-secret", "jump-phrase", "proxy-secret"] {
            assert!(!text.contains(secret), "{text}");
        }
        let staged = stage_connection_configs(&export, 5.0).unwrap();
        let route = crate::tunnel::ssh::SshSettings::parse(&staged[0].network_settings_json).unwrap();
        assert!(!route.enabled && !route.proxy.enabled && !route.routed());
        assert_eq!(route.jumps.len(), 2, "the jumps are kept");
        assert_eq!((route.jumps[0].host.as_str(), route.jumps[0].port.as_str()), ("jump1.example.com", "2201"));
        assert_eq!((route.jumps[1].port.as_str(), route.jumps[1].key_path.as_str()), ("2202", "/keys/j2"));
        assert_eq!((route.proxy.host.as_str(), route.proxy.user.as_str()), ("proxy.example.com", "px"));
        assert!(!route.save_secrets && !route.proxy.save_secrets);
        assert!(!SavedConnection::from_record(&staged[0]).ssh_tunnel);
        // A smuggled secret in a jump or in the proxy is rejected before anything is stored.
        for (from, to) in [
            (r#""host": "jump2.example.com""#, r#""host": "jump2.example.com", "password": "x""#),
            (r#""kind": "socks5""#, r#""kind": "socks5", "password": "x""#),
        ] {
            let tampered = text.replace(from, to);
            assert_ne!(tampered, text, "{from}");
            assert!(stage_connection_configs(tampered.as_bytes(), 1.0).is_err(), "{from}");
        }
        // A file cannot switch the proxy on either.
        let on = text.replace(r#""isEnabled": false"#, r#""isEnabled": true"#);
        assert!(stage_connection_configs(on.as_bytes(), 1.0).is_err());
        // A local duplicate keeps the whole route working, without any secret.
        let copy = duplicate_record(&source, "33333333-3333-3333-3333-333333333333", 9.0).unwrap();
        let kept = crate::tunnel::ssh::SshSettings::parse(&copy.network_settings_json).unwrap();
        assert!(kept.enabled && kept.proxy.enabled && kept.jumps.len() == 2);
        assert!(!copy.network_settings_json.contains("secret") && !copy.network_settings_json.contains("jump-phrase"));
        assert!(SavedConnection::from_record(&copy).ssh_tunnel);
    }

    #[test]
    fn a_proxy_alone_flags_the_connection_as_routed() {
        let mut source = record("3", "n", DB_TYPE_POSTGRESQL, "postgresql://h/d");
        for (json, routed) in [
            (r#"{"proxy":{"isEnabled":true}}"#, true),
            (r#"{"proxy":{"isEnabled":false}}"#, false),
            (r#"{"proxy":{"isEnabled":true,"kind":"ftp"}}"#, true),
            (r#"{"proxy":7}"#, true),
        ] {
            source.network_settings_json = json.into();
            assert_eq!(SavedConnection::from_record(&source).ssh_tunnel, routed, "{json}");
        }
    }

    #[test]
    fn two_records_with_non_uuid_ids_do_not_share_a_tunnel_flag() {
        let mut tunnelled = record("not-a-uuid-1", "a", DB_TYPE_POSTGRESQL, "postgresql://h/d");
        tunnelled.network_settings_json = r#"{"ssh":{"isEnabled":true}}"#.into();
        let plain = record("not-a-uuid-2", "b", DB_TYPE_POSTGRESQL, "postgresql://h2/d");
        let (a, b) = (
            SavedConnection::from_record(&tunnelled),
            SavedConnection::from_record(&plain),
        );
        // The flag belongs to the record it was read from, whatever the ids parse to.
        assert!(a.ssh_tunnel && !b.ssh_tunnel);
        let swapped = [
            SavedConnection::from_record(&plain),
            SavedConnection::from_record(&tunnelled),
        ];
        assert!(!swapped[0].ssh_tunnel && swapped[1].ssh_tunnel);
    }

    #[test]
    fn a_tunnel_switch_that_cannot_be_read_counts_as_a_tunnel() {
        let mut source = record("1", "n", DB_TYPE_POSTGRESQL, "postgresql://h/d");
        for (json, expected) in [
            ("{}", false),
            (r#"{"ssh":{"isEnabled":false}}"#, false),
            (r#"{"ssh":{"isEnabled":true}}"#, true),
            (r#"{"ssh":{"isEnabled":"yes"}}"#, true),
            (r#"{"ssh":5}"#, true),
            ("not json", true),
        ] {
            source.network_settings_json = json.into();
            assert_eq!(
                SavedConnection::from_record(&source).ssh_tunnel,
                expected,
                "{json}"
            );
        }
    }

    #[test]
    fn import_resets_allow_legacy_algorithms() {
        let mut source = record(
            "11111111-1111-1111-1111-111111111111",
            "Old appliance",
            DB_TYPE_POSTGRESQL,
            "postgresql://alice@db.example.com/app",
        );
        source.network_settings_json = SSH_NETWORK_JSON.into();
        // A duplicate is the user's own: it keeps the choice. An export never writes it.
        let copy = duplicate_record(&source, "22222222-2222-2222-2222-222222222222", 1.0).unwrap();
        assert!(
            crate::tunnel::ssh::SshSettings::from_network_json(&copy.network_settings_json)
                .allow_legacy_algorithms
        );
        let export = export_connection_configs(&[source]).unwrap();
        assert!(!String::from_utf8(export.clone())
            .unwrap()
            .contains("allowLegacyAlgorithms"));
        // A file that sets it anyway (hand-made, or from another tool) is staged without it.
        let mut bundle: serde_json::Value = serde_json::from_slice(&export).unwrap();
        bundle["connections"][0]["network_settings"]["ssh"]["allowLegacyAlgorithms"] = true.into();
        let staged = stage_connection_configs(&serde_json::to_vec(&bundle).unwrap(), 2.0).unwrap();
        let ssh =
            crate::tunnel::ssh::SshSettings::from_network_json(&staged[0].network_settings_json);
        assert!(
            !ssh.allow_legacy_algorithms,
            "{}",
            staged[0].network_settings_json
        );
        assert!(!ssh.enabled);
    }

    #[test]
    fn a_remote_endpoint_survives_export_and_import_with_the_tunnel_off() {
        let mut source = record(
            "11111111-1111-1111-1111-111111111111",
            "Behind a bastion",
            DB_TYPE_POSTGRESQL,
            "postgresql://alice@db.example.com/app",
        );
        source.network_settings_json = SSH_NETWORK_JSON.into();
        let staged =
            stage_connection_configs(&export_connection_configs(&[source]).unwrap(), 3.0).unwrap();
        let ssh =
            crate::tunnel::ssh::SshSettings::from_network_json(&staged[0].network_settings_json);
        assert_eq!(
            (ssh.remote_host.as_str(), ssh.remote_port.as_str()),
            ("10.0.0.9", "5432")
        );
        assert!(!ssh.enabled);
    }
}
