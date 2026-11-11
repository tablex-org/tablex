//! Connection types (Development / Test / Production / custom), the
//! per-connection overrides of the settings, and the effective values a
//! connection ends up with: the model the app's Settings and `tablex-mcp` share. The app keeps
//! the settings registry (UI descriptors) and the whole `Settings` struct; this module reads the
//! connection-scoped part of `settings.json` itself ([`ProfileSettings`]), exactly as the app does.
//!
//! Precedence, strongest first: the connection's own override, the preset of
//! its connection type, the global setting. Only the settings flagged
//! `overridable` in the registry can be overridden; the safety presets (confirm
//! execution, confirm data changes, read-only, auto-commit, auto-close idle
//! transactions) belong to the type and have no global counterpart, so a
//! connection without a type has none of them on.
//!
//! A saved connection without a type (every connection saved before this
//! existed) behaves exactly as before: global settings only, no badge.

pub use engine_agent::AgentAccess;
use engine_agent::{ConnectionInfo, ConnectionTypeInfo};
use engine_storage::connection::SavedConnectionRecord;
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::path::Path;
use std::time::Duration;

use crate::records::ConnectionKind;

/// The file name of the app's settings (`<config dir>/tablex/settings.json`).
pub const SETTINGS_FILE: &str = "settings.json";

/// `<config dir>/tablex/settings.json` (see [`crate::platform::config_dir`]).
pub fn default_settings_path() -> Option<std::path::PathBuf> {
    crate::platform::config_dir().map(|dir| dir.join(SETTINGS_FILE))
}

/// The app's switch "Let MCP clients run changes" in `settings.json`: `{"ai": {"mcp_writes": true}}`.
pub const MCP_WRITES_KEY: &str = "ai.mcp_writes";

/// The first `schema_version` of `settings.json` that has `ai.mcp_writes` (the app's `SCHEMA_VERSION`
/// when the switch was added). `true` in a file with a lower or no version is not honoured.
pub const MCP_WRITES_SCHEMA_VERSION: u64 = 4;

/// Reads the switch from the file the app writes. A missing file or key is `false` (the default);
/// a file that cannot be read, is not a JSON object, or holds anything but a boolean there is an
/// error; a `true` needs a numeric `schema_version` of at least [`MCP_WRITES_SCHEMA_VERSION`], else
/// it reads as `false`. (Walked by hand rather than deserialized: serde would also read a struct out of a JSON
/// array, and only an object may switch anything on.)
pub fn read_mcp_writes(path: &Path) -> Result<bool, ProfileError> {
    let shown = path.display().to_string();
    let text = match std::fs::read_to_string(path) {
        Ok(text) => text,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(false),
        Err(source) => {
            return Err(ProfileError::Unreadable {
                path: shown,
                source,
            })
        }
    };
    let invalid = |message: String| ProfileError::Invalid {
        path: shown.clone(),
        message,
    };
    let value: serde_json::Value =
        serde_json::from_str(&text).map_err(|error| invalid(error.to_string()))?;
    let root = value
        .as_object()
        .ok_or_else(|| invalid("the file is not a JSON object".into()))?;
    let Some(ai) = root.get("ai") else {
        return Ok(false);
    };
    let ai = ai
        .as_object()
        .ok_or_else(|| invalid("`ai` is not an object".into()))?;
    let on = match ai.get("mcp_writes") {
        None => return Ok(false),
        Some(serde_json::Value::Bool(on)) => *on,
        Some(_) => return Err(invalid("`ai.mcp_writes` is not a boolean".into())),
    };
    // The app's migration to this schema forces the switch off; a `true` in a file that has not
    // been through it (an older `schema_version`, or none) is not something the app wrote.
    let version = root
        .get("schema_version")
        .and_then(serde_json::Value::as_u64);
    Ok(on && version.is_some_and(|version| version >= MCP_WRITES_SCHEMA_VERSION))
}

/// Whether MCP clients may be asked to run changes right now, for `tablex-mcp`, which reads it at
/// the time of each write. **Fails closed**: no path, a missing or unreadable file, a missing key,
/// a value that is not a boolean, all read as `false`. Only `true` in the file turns it on.
pub fn mcp_writes_enabled(path: Option<&Path>) -> bool {
    path.is_some_and(|path| matches!(read_mcp_writes(path), Ok(true)))
}

/// Rows per page when nothing says otherwise (the app's `DEFAULT_OUTPUT_LIMIT`).
pub const DEFAULT_PAGE_SIZE: u32 = 200;

/// Limits of the overridable numbers: `(min, max, step)`. The app's settings registry uses the same
/// constants for its descriptors, so the page, the dialog and [`Effective::resolve`] agree.
pub const QUERY_TIMEOUT_SECS_LIMITS: (f64, f64, f64) = (0.0, 86_400.0, 1.0);
pub const CONNECT_TIMEOUT_SECS_LIMITS: (f64, f64, f64) = (0.0, 600.0, 1.0);
pub const PAGE_SIZE_LIMITS: (f64, f64, f64) = (1.0, 100_000.0, 1.0);
pub const KEEP_ALIVE_SECS_LIMITS: (f64, f64, f64) = (0.0, 86_400.0, 1.0);

/// Registry ids of the overridable settings (`settings.json` / `overrides_json` keys).
pub const QUERY_TIMEOUT_SECS: &str = "query.query_timeout_secs";
pub const CONNECT_TIMEOUT_SECS: &str = "query.connect_timeout_secs";
pub const PAGE_SIZE: &str = "results.page_size";
pub const KEEP_ALIVE_SECS: &str = "query.keep_alive_secs";
pub const CONFIRM_DANGEROUS: &str = "editor.confirm_dangerous";
/// How far an AI agent may go on the connection (see [`AgentLevel`]).
pub const AGENT_ACCESS: &str = "agent.access";

/// Seconds an open connection may sit idle before the app checks it is alive (0 = never).
pub const DEFAULT_KEEP_ALIVE_SECS: u32 = 60;

/// Every setting a connection can override, by id.
pub const OVERRIDABLE_IDS: [&str; 6] = [
    QUERY_TIMEOUT_SECS,
    CONNECT_TIMEOUT_SECS,
    KEEP_ALIVE_SECS,
    PAGE_SIZE,
    CONFIRM_DANGEROUS,
    AGENT_ACCESS,
];

/// How far an AI agent (the in-app assistant, `tablex-mcp`) may go on a connection. The stored ids are
/// `off` (default), `schema`, `read` and `read_write`; the policy itself is
/// [`engine_agent::AgentAccess`], and [`AgentAccess::effective`] is the one place a level is computed.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AgentLevel {
    #[default]
    Off,
    Schema,
    Read,
    ReadWrite,
}

impl AgentLevel {
    pub const ALL: [Self; 4] = [Self::Off, Self::Schema, Self::Read, Self::ReadWrite];
    /// What a connection type can preset: never more than `read`.
    pub const TYPE_PRESETS: [Self; 3] = [Self::Off, Self::Schema, Self::Read];

    pub const fn id(self) -> &'static str {
        match self {
            Self::Off => "off",
            Self::Schema => "schema",
            Self::Read => "read",
            Self::ReadWrite => "read_write",
        }
    }

    pub fn from_id(id: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|level| level.id() == id)
    }

    pub const fn label(self) -> &'static str {
        match self {
            Self::Off => "Off",
            Self::Schema => "Schema only",
            Self::Read => "Read",
            Self::ReadWrite => "Read and write",
        }
    }

    /// One line for the settings controls.
    pub const fn description(self) -> &'static str {
        match self {
            Self::Off => "No AI agent can use this connection.",
            Self::Schema => "Agents see table and column definitions, never rows.",
            Self::Read => "Agents may also run statements that only read data.",
            Self::ReadWrite => "Agents may propose writes; each one is approved first.",
        }
    }

    /// The level a connection type can preset: `read_write` shows (and acts) as `read`.
    pub const fn type_preset(self) -> Self {
        match self {
            Self::ReadWrite => Self::Read,
            other => other,
        }
    }

    pub const fn from_access(access: AgentAccess) -> Self {
        match access {
            AgentAccess::Off => Self::Off,
            AgentAccess::SchemaOnly => Self::Schema,
            AgentAccess::Read => Self::Read,
            AgentAccess::ReadWrite => Self::ReadWrite,
        }
    }

    pub const fn access(self) -> AgentAccess {
        match self {
            Self::Off => AgentAccess::Off,
            Self::Schema => AgentAccess::SchemaOnly,
            Self::Read => AgentAccess::Read,
            Self::ReadWrite => AgentAccess::ReadWrite,
        }
    }
}

pub const DEVELOPMENT: &str = "development";
pub const TEST: &str = "test";
pub const PRODUCTION: &str = "production";

/// Longest connection-type name the editor accepts.
pub const NAME_MAX_LEN: usize = 32;

/// A named set of safety presets with a color.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct ConnectionType {
    /// Stable id stored in the connection record.
    pub id: String,
    pub name: String,
    /// `#rrggbb`; drawn on the sidebar row, the tab group chip and the tab.
    pub color: String,
    /// Every statement run from the SQL editor asks first.
    pub confirm_execute: bool,
    /// Applying staged grid edits (and committing Mongo changes) asks first,
    /// with a summary.
    pub confirm_data_change: bool,
    /// Writes are refused: DML / DDL statements and grid edits.
    pub read_only: bool,
    /// SQL query tabs of this type start in auto-commit; off starts them in manual commit.
    pub auto_commit: bool,
    /// Smart commit: in auto-commit, a statement that modifies switches the tab to
    /// manual commit first, so its change waits in a transaction.
    pub smart_commit: bool,
    /// With [`Self::smart_commit`]: a tab it switched returns to auto-commit after Commit / Roll Back.
    pub smart_commit_recover: bool,
    /// A manual-commit tab whose transaction has changes and stays idle for
    /// [`Self::idle_transaction_secs`] is rolled back.
    pub auto_close_idle_transactions: bool,
    /// How long a transaction may stay idle before it is rolled back. Unset: the per-type
    /// defaults (see [`Self::idle_secs`]), so a type saved before this existed gets them too.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub idle_transaction_secs: Option<u32>,
    /// What an AI agent may do on connections of this type that set nothing themselves. Capped at
    /// `read` by [`AgentAccess::effective`], and a production type never grants it (the
    /// connection has to say so itself).
    pub agent_access: AgentLevel,
}

impl Default for ConnectionType {
    fn default() -> Self {
        Self {
            id: String::new(),
            name: String::new(),
            color: "#3b82f6".into(),
            confirm_execute: false,
            confirm_data_change: false,
            read_only: false,
            auto_commit: true,
            smart_commit: false,
            smart_commit_recover: false,
            auto_close_idle_transactions: false,
            idle_transaction_secs: None,
            agent_access: AgentLevel::Off,
        }
    }
}

impl ConnectionType {
    /// The built-in types, in display order.
    pub fn builtins() -> Vec<Self> {
        let base = Self::default();
        vec![
            Self {
                id: DEVELOPMENT.into(),
                name: "Development".into(),
                color: "#22c55e".into(),
                ..base.clone()
            },
            Self {
                id: TEST.into(),
                name: "Test".into(),
                color: "#3b82f6".into(),
                ..base.clone()
            },
            Self {
                id: PRODUCTION.into(),
                name: "Production".into(),
                color: "#ef4444".into(),
                confirm_execute: true,
                confirm_data_change: true,
                ..base
            },
        ]
    }

    /// Seconds a transaction may stay idle: the type's own value, else Development 30 min, Test
    /// 15 min, Production 10 min, else 30 min.
    pub fn idle_secs(&self) -> u32 {
        self.idle_transaction_secs.unwrap_or(match self.id.as_str() {
            TEST => 900,
            PRODUCTION => 600,
            _ => 1800,
        })
    }

    pub fn is_builtin(&self) -> bool {
        is_builtin_id(&self.id)
    }

    /// Production-grade: a warning color and a status-bar note.
    pub fn is_production(&self) -> bool {
        self.id == PRODUCTION
    }

    /// Badge text: DEV / TEST / PROD, or the first letters of a custom name.
    pub fn badge(&self) -> String {
        match self.id.as_str() {
            DEVELOPMENT => "DEV".into(),
            TEST => "TEST".into(),
            PRODUCTION => "PROD".into(),
            _ => self
                .name
                .trim()
                .chars()
                .take(4)
                .collect::<String>()
                .to_uppercase(),
        }
    }

    /// A name for a new custom type that no type in `existing` has.
    pub fn new_custom_name(existing: &[Self]) -> String {
        let taken = |name: &str| {
            existing
                .iter()
                .any(|ty| ty.name.trim().eq_ignore_ascii_case(name))
        };
        std::iter::once("New type".to_owned())
            .chain((2..).map(|n| format!("New type {n}")))
            .find(|name| !taken(name))
            .expect("an unused name exists")
    }

    /// A fresh id for a custom type, unique among `existing`.
    pub fn new_custom_id(existing: &[Self]) -> String {
        (1..)
            .map(|n| format!("custom-{n}"))
            .find(|id| existing.iter().all(|other| other.id != *id))
            .expect("an unused id exists")
    }

    /// The parts of the type the agent policy looks at.
    pub fn agent_info(&self) -> ConnectionTypeInfo {
        ConnectionTypeInfo {
            name: self.name.clone(),
            read_only: self.read_only,
            production: self.is_production(),
            default_agent_access: Some(self.agent_access.access()),
        }
    }

    /// What the agent policy sees of a connection whose type id is `type_id`. **A non-empty id that
    /// no type of `types` defines is restrictive** (`read_only` and `production`: no preset, capped
    /// at `read`): the type may have been a read-only one that was deleted or lost with a damaged
    /// settings file. An empty id is "no type", as before. Interactive (non-agent) handling is not
    /// affected.
    pub fn agent_info_in(types: &[Self], type_id: &str) -> ConnectionTypeInfo {
        match find_type(types, type_id) {
            Some(ty) => ty.agent_info(),
            None if type_id.is_empty() => ConnectionTypeInfo::default(),
            None => ConnectionTypeInfo {
                name: "(unknown type)".into(),
                read_only: true,
                production: true,
                default_agent_access: None,
            },
        }
    }
}

/// The type with this id (`None` for an empty id or one nobody defines).
pub fn find_type<'a>(types: &'a [ConnectionType], id: &str) -> Option<&'a ConnectionType> {
    if id.is_empty() {
        return None;
    }
    types.iter().find(|ty| ty.id == id)
}

/// Makes sure the built-in types exist (a hand-edited file may have lost
/// one) and that no id repeats.
pub fn normalize_connection_types(types: &mut Vec<ConnectionType>) {
    let mut seen = std::collections::HashSet::new();
    types.retain(|ty| !ty.id.is_empty() && seen.insert(ty.id.clone()));
    for (position, builtin) in ConnectionType::builtins().into_iter().enumerate() {
        if !seen.contains(&builtin.id) {
            types.insert(position.min(types.len()), builtin);
        }
    }
}

/// The switches of a connection type's safety presets.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PresetFlag {
    ConfirmExecute,
    ConfirmDataChange,
    ReadOnly,
    AutoCommit,
    SmartCommit,
    SmartCommitRecover,
    AutoCloseIdleTransactions,
}

impl PresetFlag {
    pub const ALL: [Self; 7] = [
        Self::ConfirmExecute,
        Self::ConfirmDataChange,
        Self::ReadOnly,
        Self::AutoCommit,
        Self::SmartCommit,
        Self::SmartCommitRecover,
        Self::AutoCloseIdleTransactions,
    ];

    /// Stable key (test selectors).
    pub fn key(self) -> &'static str {
        match self {
            Self::ConfirmExecute => "confirm-execute",
            Self::ConfirmDataChange => "confirm-data-change",
            Self::ReadOnly => "read-only",
            Self::AutoCommit => "auto-commit",
            Self::SmartCommit => "smart-commit",
            Self::SmartCommitRecover => "smart-commit-recover",
            Self::AutoCloseIdleTransactions => "auto-close-idle",
        }
    }

    pub fn title(self) -> &'static str {
        match self {
            Self::ConfirmExecute => "Confirm SQL execution",
            Self::ConfirmDataChange => "Confirm data changes",
            Self::ReadOnly => "Read-only",
            Self::AutoCommit => "Auto-commit by default",
            Self::SmartCommit => "Smart commit",
            Self::SmartCommitRecover => "Return to auto-commit after Commit / Roll Back",
            Self::AutoCloseIdleTransactions => "Auto-close idle transactions",
        }
    }

    pub fn description(self) -> &'static str {
        match self {
            Self::ConfirmExecute => "Every statement run from the SQL editor asks before it runs.",
            Self::ConfirmDataChange => "Applying staged grid edits (or committing Mongo changes) asks first and lists what will change.",
            Self::ReadOnly => "Block writes: INSERT / UPDATE / DELETE / DDL statements and grid edits are refused. SELECT keeps working.",
            Self::AutoCommit => "SQL query tabs start in auto-commit. Off starts them in manual commit: statements wait for Commit or Rollback on a session of the tab.",
            Self::SmartCommit => "In auto-commit, a statement that modifies data switches the tab to manual commit first, so the change waits for Commit or Roll Back. Not on connections without transactions or read-only ones.",
            Self::SmartCommitRecover => "Needs Smart commit. A tab it switched goes back to auto-commit once its transaction is committed or rolled back.",
            Self::AutoCloseIdleTransactions => "Roll back a manual-commit transaction with changes that stays idle (Development 30 min, Test 15 min, Production 10 min).",
        }
    }

    pub fn get(self, ty: &ConnectionType) -> bool {
        match self {
            Self::ConfirmExecute => ty.confirm_execute,
            Self::ConfirmDataChange => ty.confirm_data_change,
            Self::ReadOnly => ty.read_only,
            Self::AutoCommit => ty.auto_commit,
            Self::SmartCommit => ty.smart_commit,
            Self::SmartCommitRecover => ty.smart_commit_recover,
            Self::AutoCloseIdleTransactions => ty.auto_close_idle_transactions,
        }
    }

    pub fn set(self, ty: &mut ConnectionType, value: bool) {
        match self {
            Self::ConfirmExecute => ty.confirm_execute = value,
            Self::ConfirmDataChange => ty.confirm_data_change = value,
            Self::ReadOnly => ty.read_only = value,
            Self::AutoCommit => ty.auto_commit = value,
            Self::SmartCommit => ty.smart_commit = value,
            Self::SmartCommitRecover => ty.smart_commit_recover = value,
            Self::AutoCloseIdleTransactions => ty.auto_close_idle_transactions = value,
        }
    }
}

pub fn is_builtin_id(id: &str) -> bool {
    matches!(id, DEVELOPMENT | TEST | PRODUCTION)
}

/// A problem with a connection type the editor reports inline.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TypeError {
    EmptyName,
    NameTooLong,
    DuplicateName,
    BadColor,
}

impl std::fmt::Display for TypeError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::EmptyName => f.write_str("Enter a name"),
            Self::NameTooLong => write!(f, "At most {NAME_MAX_LEN} characters"),
            Self::DuplicateName => f.write_str("Another connection type has this name"),
            Self::BadColor => f.write_str("Enter a color like #ef4444"),
        }
    }
}

impl std::error::Error for TypeError {}

pub fn is_hex_color(text: &str) -> bool {
    text.len() == 7
        && text.starts_with('#')
        && text.as_bytes()[1..].iter().all(u8::is_ascii_hexdigit)
}

/// Checks the types of a draft; `(type id, error)` per problem.
pub fn type_errors(types: &[ConnectionType]) -> Vec<(String, TypeError)> {
    let mut errors = Vec::new();
    for (index, ty) in types.iter().enumerate() {
        let name = ty.name.trim();
        let error = if name.is_empty() {
            Some(TypeError::EmptyName)
        } else if name.chars().count() > NAME_MAX_LEN {
            Some(TypeError::NameTooLong)
        } else if types[..index]
            .iter()
            .any(|other| other.name.trim().eq_ignore_ascii_case(name))
        {
            Some(TypeError::DuplicateName)
        } else if !is_hex_color(&ty.color) {
            Some(TypeError::BadColor)
        } else {
            None
        };
        errors.extend(error.map(|error| (ty.id.clone(), error)));
    }
    errors
}

/// A per-connection value of an overridable setting.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(untagged)]
pub enum OverrideValue {
    Bool(bool),
    Number(f64),
    Text(String),
}

/// The overridden settings of one connection, by registry id.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(transparent)]
pub struct Overrides(BTreeMap<String, OverrideValue>);

impl Overrides {
    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }

    pub fn get(&self, id: &str) -> Option<&OverrideValue> {
        self.0.get(id)
    }

    pub fn set(&mut self, id: &str, value: OverrideValue) {
        self.0.insert(id.to_owned(), value);
    }

    /// "Use global" again.
    pub fn clear(&mut self, id: &str) {
        self.0.remove(id);
    }

    pub fn iter(&self) -> impl Iterator<Item = (&str, &OverrideValue)> {
        self.0.iter().map(|(id, value)| (id.as_str(), value))
    }

    /// The saved form, read entry by entry: every readable scalar override is kept and an unreadable
    /// one (`null`, an array, an object) is skipped, so a bad record never blocks the connection.
    ///
    /// **The agent level is never lost to bad text.** `agent.access` present but not a readable string
    /// is explicit `off`; text that is not a JSON object at all (garbage, truncated, an array) has
    /// no other overrides but the level reads as explicit `off` (it is inserted into the map, so the
    /// dialog shows it and a save writes it back). Empty text is "no overrides".
    pub fn from_json(json: &str) -> Self {
        if json.trim().is_empty() {
            return Self::default();
        }
        let mut overrides = Self::default();
        match serde_json::from_str::<serde_json::Value>(json) {
            Ok(serde_json::Value::Object(map)) => {
                for (key, value) in map {
                    let value = match value {
                        serde_json::Value::Bool(on) => Some(OverrideValue::Bool(on)),
                        serde_json::Value::Number(number) => {
                            number.as_f64().map(OverrideValue::Number)
                        }
                        serde_json::Value::String(text) => Some(OverrideValue::Text(text)),
                        _ => None,
                    };
                    match value {
                        Some(value) => overrides.set(&key, value),
                        None if key == AGENT_ACCESS => {
                            overrides.set(&key, OverrideValue::Text(AgentLevel::Off.id().into()))
                        }
                        None => {}
                    }
                }
            }
            _ => overrides.set(
                AGENT_ACCESS,
                OverrideValue::Text(AgentLevel::Off.id().into()),
            ),
        }
        overrides
    }

    /// Empty when there is nothing to save.
    pub fn to_json(&self) -> String {
        if self.is_empty() {
            String::new()
        } else {
            serde_json::to_string(self).unwrap_or_default()
        }
    }
}

/// What a saved connection carries besides its address: the type and the
/// overrides.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct ConnectionProfile {
    /// Id of the connection type; empty = none.
    pub type_id: String,
    pub overrides: Overrides,
}

impl ConnectionProfile {
    pub fn from_record(record: &SavedConnectionRecord) -> Self {
        Self {
            type_id: record.connection_type.clone(),
            overrides: Overrides::from_json(&record.overrides_json),
        }
    }
}

/// `overrides_json` without the agent access level (a record made from another record starts from
/// the default: the preset of its connection type, which is `off` unless the type says otherwise,
/// and never more than `read`); every other override stays.
pub fn without_agent_access(overrides_json: &str) -> String {
    let mut overrides = Overrides::from_json(overrides_json);
    overrides.clear(AGENT_ACCESS);
    overrides.to_json()
}

/// A hash of everything in the record that decides where and how the connection goes: the saved URI,
/// the kind, the tunnel settings (secrets are not in them), the type, the overrides and whether a
/// password is saved. The agent layer closes a session opened under another value, so an edit takes
/// effect on the next call. It is a SHA-256 prefix: it cannot be turned back into the address.
fn revision_of(record: &SavedConnectionRecord) -> engine_agent::Revision {
    use sha2::{Digest, Sha256};
    let mut hash = Sha256::new();
    for part in [
        record.uri.as_str(),
        record.db_type.as_str(),
        record.network_settings_json.as_str(),
        record.connection_type.as_str(),
        record.overrides_json.as_str(),
    ] {
        hash.update(part.len().to_le_bytes());
        hash.update(part.as_bytes());
    }
    hash.update([u8::from(record.save_password)]);
    let digest = hash.finalize();
    let mut prefix = [0u8; 8];
    prefix.copy_from_slice(&digest[..8]);
    engine_agent::Revision::new(u64::from_le_bytes(prefix))
}

/// The values a connection runs with.
#[derive(Debug, Clone, PartialEq)]
pub struct Effective {
    pub query_timeout: Option<Duration>,
    pub connect_timeout_secs: u32,
    /// Idle seconds before the liveness check of an open connection; 0 = off.
    pub keep_alive_secs: u32,
    pub page_size: u32,
    pub confirm_dangerous: bool,
    pub confirm_execute: bool,
    pub confirm_data_change: bool,
    pub read_only: bool,
    pub auto_commit: bool,
    /// Smart commit is on, and whether a tab it switched returns to auto-commit (never without it).
    pub smart_commit: bool,
    pub smart_commit_recover: bool,
    pub auto_close_idle_transactions: bool,
    pub idle_transaction_secs: u32,
    /// The connection's type, when it has a known one.
    pub connection_type: Option<ConnectionType>,
    /// What an AI agent may do here: the connection's own setting, else its type's preset,
    /// capped by the type (see [`AgentAccess::effective`]).
    pub agent_access: AgentAccess,
}

/// The level a connection ends up with and, in words, where it comes from (the dialog shows it under
/// the "Agent access" control). The level itself is [`AgentAccess::effective`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AgentResolution {
    pub effective: AgentAccess,
    /// E.g. `Read and write — limited to Read by connection type Production (read-only)`.
    pub note: String,
}

impl AgentResolution {
    pub fn of(types: &[ConnectionType], profile: &ConnectionProfile) -> Self {
        let connection_type = find_type(types, &profile.type_id);
        let info = ConnectionType::agent_info_in(types, &profile.type_id);
        let explicit = profile.agent_level();
        let effective = AgentAccess::effective(explicit.map(AgentLevel::access), &info);
        let label = |access: AgentAccess| AgentLevel::from_access(access).label();
        let type_name = connection_type.map_or("", |ty| ty.name.as_str());
        let dangling = connection_type.is_none() && !profile.type_id.is_empty();
        let note = match explicit {
            Some(level) if dangling && effective < level.access() => format!(
                "{} \u{2014} limited to {}: connection type not found",
                level.label(),
                label(effective)
            ),
            None if dangling => format!(
                "{} \u{2014} connection type not found, so no default applies",
                label(effective)
            ),
            Some(level) if effective < level.access() => format!(
                "{} \u{2014} limited to {} by connection type {type_name} (read-only)",
                level.label(),
                label(effective)
            ),
            Some(_) => label(effective).to_owned(),
            None if info.production
                && info
                    .default_agent_access
                    .is_some_and(|p| p > AgentAccess::Off) =>
            {
                format!(
                    "{} \u{2014} the {type_name} type does not grant access by default; choose a level for this connection",
                    label(effective)
                )
            }
            None => match connection_type {
                Some(ty) if effective > AgentAccess::Off => format!(
                    "{} \u{2014} the default of connection type {}",
                    label(effective),
                    ty.name
                ),
                _ => label(effective).to_owned(),
            },
        };
        Self { effective, note }
    }
}

/// The global values the overridable settings fall back to.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Globals {
    pub query_timeout_secs: u32,
    pub connect_timeout_secs: u32,
    pub keep_alive_secs: u32,
    pub page_size: u32,
    pub confirm_dangerous: bool,
}

impl Default for Globals {
    fn default() -> Self {
        Self {
            query_timeout_secs: 0,
            connect_timeout_secs: 0,
            keep_alive_secs: DEFAULT_KEEP_ALIVE_SECS,
            page_size: DEFAULT_PAGE_SIZE,
            confirm_dangerous: true,
        }
    }
}

/// A whole number inside `limits`, or `None` (an out-of-range or hand-edited value falls back to
/// the global one).
fn whole_in(value: &OverrideValue, (min, max, _): (f64, f64, f64)) -> Option<u32> {
    match value {
        OverrideValue::Number(number)
            if !number.is_nan() && (min..=max).contains(number) && number.fract() == 0.0 =>
        {
            Some(*number as u32)
        }
        _ => None,
    }
}

impl Globals {
    /// Applies one override; a stale, unknown or invalid one changes nothing.
    fn apply(&mut self, id: &str, value: &OverrideValue) {
        match (id, value) {
            (QUERY_TIMEOUT_SECS, value) => {
                self.query_timeout_secs =
                    whole_in(value, QUERY_TIMEOUT_SECS_LIMITS).unwrap_or(self.query_timeout_secs)
            }
            (CONNECT_TIMEOUT_SECS, value) => {
                self.connect_timeout_secs = whole_in(value, CONNECT_TIMEOUT_SECS_LIMITS)
                    .unwrap_or(self.connect_timeout_secs)
            }
            (KEEP_ALIVE_SECS, value) => {
                self.keep_alive_secs =
                    whole_in(value, KEEP_ALIVE_SECS_LIMITS).unwrap_or(self.keep_alive_secs)
            }
            (PAGE_SIZE, value) => {
                self.page_size = whole_in(value, PAGE_SIZE_LIMITS).unwrap_or(self.page_size)
            }
            (CONFIRM_DANGEROUS, OverrideValue::Bool(on)) => self.confirm_dangerous = *on,
            _ => {}
        }
    }
}

impl ConnectionProfile {
    /// The connection's own agent setting. **Fails closed:** a value that is not one of the four
    /// ids is `off`, never "unset" (which would let the type's preset apply).
    pub fn agent_level(&self) -> Option<AgentLevel> {
        self.overrides.get(AGENT_ACCESS).map(|value| match value {
            OverrideValue::Text(id) => AgentLevel::from_id(id).unwrap_or(AgentLevel::Off),
            _ => AgentLevel::Off,
        })
    }
}

impl Effective {
    /// The effective values of a connection: override > type preset > global.
    pub fn resolve(
        globals: Globals,
        types: &[ConnectionType],
        profile: &ConnectionProfile,
    ) -> Self {
        let mut values = globals;
        for (id, value) in profile.overrides.iter() {
            values.apply(id, value);
        }
        let connection_type = find_type(types, &profile.type_id);
        let preset = connection_type.cloned().unwrap_or_else(|| ConnectionType {
            auto_commit: true,
            ..ConnectionType::default()
        });
        let type_info = ConnectionType::agent_info_in(types, &profile.type_id);
        Self {
            query_timeout: (values.query_timeout_secs > 0)
                .then(|| Duration::from_secs(u64::from(values.query_timeout_secs))),
            connect_timeout_secs: values.connect_timeout_secs,
            keep_alive_secs: values.keep_alive_secs,
            page_size: values.page_size,
            confirm_dangerous: values.confirm_dangerous,
            confirm_execute: preset.confirm_execute,
            confirm_data_change: preset.confirm_data_change,
            read_only: preset.read_only,
            auto_commit: preset.auto_commit,
            smart_commit: preset.smart_commit,
            smart_commit_recover: preset.smart_commit && preset.smart_commit_recover,
            auto_close_idle_transactions: preset.auto_close_idle_transactions,
            idle_transaction_secs: preset.idle_secs(),
            agent_access: AgentAccess::effective(
                profile.agent_level().map(AgentLevel::access),
                &type_info,
            ),
            connection_type: connection_type.cloned(),
        }
    }
}

/// The connection-scoped part of `settings.json`: what [`Effective`] needs besides a connection's own
/// profile. Every other setting of the file is ignored.
#[derive(Debug, Clone, PartialEq, Deserialize)]
#[serde(default)]
pub struct ProfileSettings {
    pub connection_types: Vec<ConnectionType>,
    query: QueryValues,
    results: ResultValues,
    editor: EditorValues,
}

#[derive(Debug, Clone, PartialEq, Deserialize)]
#[serde(default)]
struct QueryValues {
    query_timeout_secs: u32,
    connect_timeout_secs: u32,
    keep_alive_secs: u32,
}

impl Default for QueryValues {
    fn default() -> Self {
        Self {
            query_timeout_secs: 0,
            connect_timeout_secs: 0,
            keep_alive_secs: DEFAULT_KEEP_ALIVE_SECS,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Deserialize)]
#[serde(default)]
struct ResultValues {
    page_size: u32,
}

impl Default for ResultValues {
    fn default() -> Self {
        Self {
            page_size: DEFAULT_PAGE_SIZE,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Deserialize)]
#[serde(default)]
struct EditorValues {
    confirm_dangerous: bool,
}

impl Default for EditorValues {
    fn default() -> Self {
        Self {
            confirm_dangerous: true,
        }
    }
}

impl Default for ProfileSettings {
    fn default() -> Self {
        Self {
            connection_types: ConnectionType::builtins(),
            query: QueryValues::default(),
            results: ResultValues::default(),
            editor: EditorValues::default(),
        }
    }
}

/// Why `settings.json` could not be used. The agent catalog fails closed on it.
#[derive(Debug, thiserror::Error)]
pub enum ProfileError {
    #[error("cannot read the settings file {path}: {source}")]
    Unreadable {
        path: String,
        source: std::io::Error,
    },
    #[error("the settings file {path} is not valid: {message}")]
    Invalid { path: String, message: String },
}

impl ProfileSettings {
    /// Reads the file the app writes. A missing file is the defaults (a fresh install); a file
    /// that exists and cannot be read or parsed is an error, never silently "no read-only types".
    pub fn load(path: &Path) -> Result<Self, ProfileError> {
        let shown = path.display().to_string();
        let text = match std::fs::read_to_string(path) {
            Ok(text) => text,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                return Ok(Self::default())
            }
            Err(source) => {
                return Err(ProfileError::Unreadable {
                    path: shown,
                    source,
                })
            }
        };
        let mut settings: Self =
            serde_json::from_str(&text).map_err(|error| ProfileError::Invalid {
                path: shown,
                message: error.to_string(),
            })?;
        // A hand-edited file may have lost a built-in type.
        normalize_connection_types(&mut settings.connection_types);
        Ok(settings)
    }

    pub fn globals(&self) -> Globals {
        Globals {
            query_timeout_secs: self.query.query_timeout_secs,
            connect_timeout_secs: self.query.connect_timeout_secs,
            keep_alive_secs: self.query.keep_alive_secs,
            page_size: self.results.page_size,
            confirm_dangerous: self.editor.confirm_dangerous,
        }
    }

    pub fn effective(&self, profile: &ConnectionProfile) -> Effective {
        Effective::resolve(self.globals(), &self.connection_types, profile)
    }

    /// The tool layer's view of a saved connection, or `None` for a record whose kind no driver
    /// serves. There is no URI, password or key in it.
    pub fn agent_info(&self, record: &SavedConnectionRecord) -> Option<ConnectionInfo> {
        let kind = ConnectionKind::from_persisted(&record.db_type)
            .and_then(|kind| engine_agent::ConnectionKind::from_driver_id(kind.driver_id()))?;
        let profile = ConnectionProfile::from_record(record);
        let effective = self.effective(&profile);
        Some(ConnectionInfo {
            id: crate::records::display_connection_id(&record.id),
            name: record.name.clone(),
            kind,
            connection_type: ConnectionType::agent_info_in(
                &self.connection_types,
                &profile.type_id,
            ),
            agent_access: profile.agent_level().map(AgentLevel::access),
            statement_timeout_secs: effective.query_timeout.map(|timeout| timeout.as_secs()),
            revision: revision_of(record),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn built_in_types_follow_the_spec() {
        let types = ConnectionType::builtins();
        let ids: Vec<_> = types.iter().map(|ty| ty.id.as_str()).collect();
        assert_eq!(ids, [DEVELOPMENT, TEST, PRODUCTION]);
        let dev = &types[0];
        assert!(!dev.confirm_execute && !dev.confirm_data_change && !dev.read_only);
        let production = &types[2];
        assert!(production.confirm_execute && production.confirm_data_change);
        assert!(!production.read_only, "read-only is the user's choice");
        assert!(production.is_production() && production.is_builtin());
        assert_eq!(production.badge(), "PROD");
        assert_eq!(
            ConnectionType {
                name: " Staging ".into(),
                ..Default::default()
            }
            .badge(),
            "STAG"
        );
    }

    #[test]
    fn overrides_round_trip_through_json_and_bad_json_reads_as_none() {
        let mut overrides = Overrides::default();
        assert_eq!(overrides.to_json(), "", "nothing to save");
        overrides.set("query.query_timeout_secs", OverrideValue::Number(30.0));
        overrides.set("editor.confirm_dangerous", OverrideValue::Bool(false));
        let json = overrides.to_json();
        assert_eq!(Overrides::from_json(&json), overrides);
        // Unreadable text has no other overrides, but the agent level is explicit `off`.
        let only_off = |overrides: Overrides| {
            overrides
                .iter()
                .map(|(id, value)| (id.to_owned(), value.clone()))
                .collect::<Vec<_>>()
        };
        for bad in [
            "not json",
            "[1,2]",
            "{\"results.page_size\": 5",
            "null",
            "42",
        ] {
            assert_eq!(
                only_off(Overrides::from_json(bad)),
                [(AGENT_ACCESS.to_owned(), OverrideValue::Text("off".into()))],
                "{bad}"
            );
        }
        assert!(Overrides::from_json("").is_empty());
        assert!(Overrides::from_json("  ").is_empty());
    }

    #[test]
    fn the_profile_comes_from_the_saved_record() {
        let mut record = SavedConnectionRecord {
            id: "A".into(),
            name: "n".into(),
            uri: "sqlite:///x".into(),
            db_type: "sqlite".into(),
            color_hex: String::new(),
            save_password: false,
            save_proxy_password: false,
            network_settings_json: "{}".into(),
            is_pinned: false,
            last_connected: None,
            created_at: 0.0,
            updated_at: 0.0,
            connection_type: PRODUCTION.into(),
            overrides_json: r#"{"results.page_size":50}"#.into(),
        };
        let profile = ConnectionProfile::from_record(&record);
        assert_eq!(profile.type_id, PRODUCTION);
        assert_eq!(
            profile.overrides.get("results.page_size"),
            Some(&OverrideValue::Number(50.0))
        );
        record.connection_type.clear();
        record.overrides_json.clear();
        assert_eq!(
            ConnectionProfile::from_record(&record),
            ConnectionProfile::default()
        );
    }

    #[test]
    fn a_type_saved_before_the_idle_limit_existed_gets_its_builtin_default() {
        let old: Vec<ConnectionType> = serde_json::from_str(
            r##"[{"id":"development","name":"Development","color":"#22c55e","auto_close_idle_transactions":true},
                 {"id":"test","name":"Test","color":"#3b82f6"},
                 {"id":"production","name":"Production","color":"#ef4444","confirm_execute":true},
                 {"id":"custom-1","name":"Staging","color":"#a855f7"},
                 {"id":"custom-2","name":"Own","color":"#a855f7","idle_transaction_secs":120}]"##,
        )
        .unwrap();
        let secs: Vec<u32> = old.iter().map(ConnectionType::idle_secs).collect();
        assert_eq!(secs, [1800, 900, 600, 1800, 120]);
        // A builtin that is re-saved untouched stays unset, so the default keeps following the id.
        assert!(!serde_json::to_string(&ConnectionType::builtins()[1]).unwrap().contains("idle_transaction_secs"));
    }

    #[test]
    fn smart_commit_is_off_for_a_type_saved_without_it_and_recovery_needs_it() {
        let old: Vec<ConnectionType> =
            serde_json::from_str(r##"[{"id":"production","name":"Production","color":"#ef4444","confirm_execute":true}]"##).unwrap();
        let effective = Effective::resolve(
            Globals::default(),
            &old,
            &ConnectionProfile { type_id: "production".into(), ..Default::default() },
        );
        assert!(!effective.smart_commit && !effective.smart_commit_recover);
        let mut recover_only = old[0].clone();
        recover_only.smart_commit_recover = true;
        let effective = Effective::resolve(
            Globals::default(),
            &[recover_only.clone()],
            &ConnectionProfile { type_id: "production".into(), ..Default::default() },
        );
        assert!(!effective.smart_commit_recover, "recovery without smart commit does nothing");
        recover_only.smart_commit = true;
        let effective = Effective::resolve(
            Globals::default(),
            &[recover_only],
            &ConnectionProfile { type_id: "production".into(), ..Default::default() },
        );
        assert!(effective.smart_commit && effective.smart_commit_recover);
    }

    #[test]
    fn preset_flags_edit_their_own_field() {
        for flag in PresetFlag::ALL {
            let mut ty = ConnectionType::default();
            let before = flag.get(&ty);
            flag.set(&mut ty, !before);
            assert_eq!(flag.get(&ty), !before, "{flag:?}");
            let others = PresetFlag::ALL
                .into_iter()
                .filter(|other| *other != flag)
                .all(|other| other.get(&ty) == other.get(&ConnectionType::default()));
            assert!(others, "{flag:?} touched another flag");
        }
    }

    #[test]
    fn custom_names_and_ids_are_unique() {
        let mut types = ConnectionType::builtins();
        assert_eq!(ConnectionType::new_custom_name(&types), "New type");
        types.push(ConnectionType {
            id: "custom-1".into(),
            name: "New type".into(),
            ..Default::default()
        });
        assert_eq!(ConnectionType::new_custom_name(&types), "New type 2");
        assert_eq!(ConnectionType::new_custom_id(&types), "custom-2");
    }

    #[test]
    fn type_editor_validation() {
        let mut types = ConnectionType::builtins();
        assert!(type_errors(&types).is_empty());
        types.push(ConnectionType {
            id: "c".into(),
            name: "  ".into(),
            ..Default::default()
        });
        assert_eq!(
            type_errors(&types),
            vec![("c".to_owned(), TypeError::EmptyName)]
        );
        types[3].name = "production".into();
        assert_eq!(
            type_errors(&types),
            vec![("c".to_owned(), TypeError::DuplicateName)]
        );
        types[3].name = "Staging".into();
        types[3].color = "red".into();
        assert_eq!(
            type_errors(&types),
            vec![("c".to_owned(), TypeError::BadColor)]
        );
        types[3].color = "#a1B2c3".into();
        assert!(type_errors(&types).is_empty());
    }

    fn override_of(id: &str, value: OverrideValue) -> ConnectionProfile {
        let mut profile = ConnectionProfile::default();
        profile.overrides.set(id, value);
        profile
    }

    fn text(level: &str) -> OverrideValue {
        OverrideValue::Text(level.into())
    }

    fn effective(types: &[ConnectionType], type_id: &str, level: Option<&str>) -> AgentAccess {
        let mut profile = ConnectionProfile {
            type_id: type_id.into(),
            ..Default::default()
        };
        if let Some(level) = level {
            profile.overrides.set(AGENT_ACCESS, text(level));
        }
        Effective::resolve(Globals::default(), types, &profile).agent_access
    }

    #[test]
    fn agent_levels_use_the_four_stored_ids() {
        let ids: Vec<_> = AgentLevel::ALL.iter().map(|level| level.id()).collect();
        assert_eq!(ids, ["off", "schema", "read", "read_write"]);
        for level in AgentLevel::ALL {
            assert_eq!(AgentLevel::from_id(level.id()), Some(level));
            assert_eq!(
                serde_json::to_value(level).unwrap(),
                serde_json::json!(level.id())
            );
            assert!(!level.description().is_empty() && !level.label().is_empty());
        }
        assert_eq!(AgentLevel::default(), AgentLevel::Off);
        assert_eq!(AgentLevel::from_id("everything"), None);
        assert_eq!(AgentLevel::Schema.access(), AgentAccess::SchemaOnly);
        assert!(
            !AgentLevel::TYPE_PRESETS.contains(&AgentLevel::ReadWrite),
            "a preset never writes"
        );
    }

    #[test]
    fn agent_access_is_off_until_somebody_sets_it() {
        let types = ConnectionType::builtins();
        for type_id in ["", DEVELOPMENT, TEST, PRODUCTION, "gone"] {
            assert_eq!(
                effective(&types, type_id, None),
                AgentAccess::Off,
                "{type_id:?}"
            );
        }
        assert_eq!(ConnectionType::default().agent_access, AgentLevel::Off);
    }

    #[test]
    fn a_connections_own_level_wins_and_a_read_only_type_caps_it_at_read() {
        let mut types = ConnectionType::builtins();
        assert_eq!(
            effective(&types, DEVELOPMENT, Some("schema")),
            AgentAccess::SchemaOnly
        );
        assert_eq!(
            effective(&types, DEVELOPMENT, Some("read")),
            AgentAccess::Read
        );
        assert_eq!(
            effective(&types, DEVELOPMENT, Some("read_write")),
            AgentAccess::ReadWrite
        );
        assert_eq!(
            effective(&types, "", Some("read_write")),
            AgentAccess::ReadWrite
        );
        // Production is explicit-only, and a read-only type caps everything at read.
        assert_eq!(
            effective(&types, PRODUCTION, Some("read_write")),
            AgentAccess::ReadWrite
        );
        types
            .iter_mut()
            .find(|ty| ty.id == PRODUCTION)
            .unwrap()
            .read_only = true;
        assert_eq!(
            effective(&types, PRODUCTION, Some("read_write")),
            AgentAccess::Read
        );
        assert_eq!(
            effective(&types, PRODUCTION, Some("schema")),
            AgentAccess::SchemaOnly
        );
        assert_eq!(effective(&types, PRODUCTION, Some("off")), AgentAccess::Off);
    }

    #[test]
    fn a_type_preset_applies_to_unset_connections_capped_at_read_and_never_on_production() {
        let mut types = ConnectionType::builtins();
        for ty in &mut types {
            ty.agent_access = AgentLevel::Read;
        }
        assert_eq!(effective(&types, DEVELOPMENT, None), AgentAccess::Read);
        assert_eq!(effective(&types, TEST, None), AgentAccess::Read);
        assert_eq!(
            effective(&types, PRODUCTION, None),
            AgentAccess::Off,
            "production needs its own setting"
        );
        assert_eq!(
            effective(&types, PRODUCTION, Some("read")),
            AgentAccess::Read
        );
        // The connection's own `off` beats the preset.
        assert_eq!(
            effective(&types, DEVELOPMENT, Some("off")),
            AgentAccess::Off
        );
        // A hand-edited `read_write` preset is capped at read.
        types[0].agent_access = AgentLevel::ReadWrite;
        assert_eq!(effective(&types, DEVELOPMENT, None), AgentAccess::Read);
        // An unknown type id has no preset.
        assert_eq!(effective(&types, "gone", None), AgentAccess::Off);
    }

    #[test]
    fn an_unreadable_agent_setting_means_off_never_the_presets() {
        let mut types = ConnectionType::builtins();
        types[0].agent_access = AgentLevel::Read;
        for bad in [
            text("root"),
            text(""),
            OverrideValue::Bool(true),
            OverrideValue::Number(3.0),
        ] {
            let mut profile = ConnectionProfile {
                type_id: DEVELOPMENT.into(),
                ..Default::default()
            };
            profile.overrides.set(AGENT_ACCESS, bad.clone());
            assert_eq!(profile.agent_level(), Some(AgentLevel::Off), "{bad:?}");
            assert_eq!(
                Effective::resolve(Globals::default(), &types, &profile).agent_access,
                AgentAccess::Off
            );
        }
    }

    fn dev_read_types() -> Vec<ConnectionType> {
        let mut types = ConnectionType::builtins();
        types[0].agent_access = AgentLevel::Read;
        types
    }

    fn from_text(json: &str) -> ConnectionProfile {
        ConnectionProfile {
            type_id: DEVELOPMENT.into(),
            overrides: Overrides::from_json(json),
        }
    }

    #[test]
    fn unreadable_overrides_text_never_drops_an_explicit_agent_level_to_the_type_preset() {
        let types = dev_read_types();
        let level = |json: &str| {
            let profile = from_text(json);
            (
                profile.agent_level(),
                Effective::resolve(Globals::default(), &types, &profile).agent_access,
            )
        };
        let off = (Some(AgentLevel::Off), AgentAccess::Off);
        // Text that is not an object: the level is explicit off, not "unset" (preset read).
        for bad in [
            "not json",
            "{\"agent.access\": \"off\"",
            "[\"agent.access\"]",
            "null",
            "7",
            "\"off\"",
        ] {
            assert_eq!(level(bad), off, "{bad}");
        }
        // The level itself unreadable: off.
        for bad in [
            r#"{"agent.access": null}"#,
            r#"{"agent.access": ["read"]}"#,
            r#"{"agent.access": {"level": "read"}}"#,
            r#"{"agent.access": 3}"#,
            r#"{"agent.access": true}"#,
            r#"{"agent.access": "everything"}"#,
        ] {
            assert_eq!(level(bad), off, "{bad}");
        }
        // A non-scalar value for ANOTHER key does not cost the level, nor the other readable overrides.
        let json = r#"{"results.page_size": [1], "agent.access": "schema", "query.query_timeout_secs": 30, "x": null}"#;
        assert_eq!(
            level(json),
            (Some(AgentLevel::Schema), AgentAccess::SchemaOnly)
        );
        let profile = from_text(json);
        assert_eq!(
            profile.overrides.get(QUERY_TIMEOUT_SECS),
            Some(&OverrideValue::Number(30.0))
        );
        assert_eq!(profile.overrides.get(PAGE_SIZE), None);
        // No level at all still follows the preset.
        assert_eq!(
            level(r#"{"results.page_size": [1]}"#),
            (None, AgentAccess::Read)
        );
        // What the dialog saves back keeps the level.
        let saved = from_text("{ truncated").overrides.to_json();
        assert_eq!(saved, r#"{"agent.access":"off"}"#);
        assert_eq!(without_agent_access("{ truncated"), "");
    }

    #[test]
    fn a_type_id_nobody_defines_is_restrictive_for_the_agent_only() {
        let types = dev_read_types();
        let gone = |level: Option<&str>| {
            let mut profile = ConnectionProfile {
                type_id: "custom-gone".into(),
                ..Default::default()
            };
            if let Some(level) = level {
                profile.overrides.set(AGENT_ACCESS, text(level));
            }
            profile
        };
        let resolve =
            |profile: &ConnectionProfile| Effective::resolve(Globals::default(), &types, profile);
        // Explicit read_write is capped at read; no level means off (never a preset).
        assert_eq!(
            resolve(&gone(Some("read_write"))).agent_access,
            AgentAccess::Read
        );
        assert_eq!(
            resolve(&gone(Some("schema"))).agent_access,
            AgentAccess::SchemaOnly
        );
        assert_eq!(resolve(&gone(None)).agent_access, AgentAccess::Off);
        // No type at all keeps today's behaviour; interactive values do not change either.
        let plain = ConnectionProfile::default();
        assert_eq!(resolve(&plain).agent_access, AgentAccess::Off);
        let mut explicit = plain.clone();
        explicit.overrides.set(AGENT_ACCESS, text("read_write"));
        assert_eq!(resolve(&explicit).agent_access, AgentAccess::ReadWrite);
        assert!(
            !resolve(&gone(None)).read_only,
            "the interactive side is unchanged"
        );
        // The agent's info says so too, and the dialog's note says why.
        let settings = ProfileSettings {
            connection_types: types.clone(),
            ..Default::default()
        };
        let mut record = crate::records::tests_record_for("0F8FAD5B-D9CB-469F-A165-70867728950E");
        record.connection_type = "custom-gone".into();
        record.overrides_json = r#"{"agent.access":"read_write"}"#.into();
        let info = settings.agent_info(&record).unwrap();
        assert_eq!(info.effective_access(), AgentAccess::Read);
        assert!(info.connection_type.read_only && info.connection_type.production);
        let note = AgentResolution::of(&types, &gone(Some("read_write"))).note;
        assert_eq!(
            note,
            "Read and write \u{2014} limited to Read: connection type not found"
        );
        assert!(AgentResolution::of(&types, &gone(None))
            .note
            .contains("connection type not found"));
    }

    #[test]
    fn the_agent_setting_is_saved_like_every_other_override() {
        let mut profile = override_of(AGENT_ACCESS, text("schema"));
        profile
            .overrides
            .set(PAGE_SIZE, OverrideValue::Number(50.0));
        let json = profile.overrides.to_json();
        assert_eq!(
            json,
            r#"{"agent.access":"schema","results.page_size":50.0}"#
        );
        assert_eq!(Overrides::from_json(&json), profile.overrides);
        let record = SavedConnectionRecord {
            overrides_json: json,
            ..crate::records::tests_record_for("X")
        };
        assert_eq!(
            ConnectionProfile::from_record(&record).agent_level(),
            Some(AgentLevel::Schema)
        );
    }

    #[test]
    fn overrides_apply_inside_their_limits_and_fall_back_outside() {
        let globals = Globals {
            query_timeout_secs: 10,
            page_size: 500,
            ..Globals::default()
        };
        let profile = |id: &str, value| override_of(id, value);
        let of = |p: &ConnectionProfile| Effective::resolve(globals, &[], p);
        assert_eq!(
            of(&profile(QUERY_TIMEOUT_SECS, OverrideValue::Number(90.0))).query_timeout,
            Some(Duration::from_secs(90))
        );
        assert_eq!(
            of(&profile(QUERY_TIMEOUT_SECS, OverrideValue::Number(0.0))).query_timeout,
            None,
            "0 = no limit"
        );
        assert_eq!(
            of(&profile(PAGE_SIZE, OverrideValue::Number(25.0))).page_size,
            25
        );
        assert_eq!(
            of(&profile(PAGE_SIZE, OverrideValue::Number(0.0))).page_size,
            500,
            "below the limit"
        );
        assert_eq!(
            of(&profile(PAGE_SIZE, OverrideValue::Number(2.5))).page_size,
            500,
            "not whole"
        );
        assert_eq!(
            of(&profile(PAGE_SIZE, OverrideValue::Number(f64::NAN))).page_size,
            500
        );
        assert_eq!(
            of(&profile(QUERY_TIMEOUT_SECS, OverrideValue::Bool(true))).query_timeout,
            Some(Duration::from_secs(10))
        );
        assert_eq!(
            of(&profile(CONNECT_TIMEOUT_SECS, OverrideValue::Number(7.0))).connect_timeout_secs,
            7
        );
        assert_eq!(of(&ConnectionProfile::default()).keep_alive_secs, 60, "on by default");
        assert_eq!(
            of(&profile(KEEP_ALIVE_SECS, OverrideValue::Number(0.0))).keep_alive_secs,
            0,
            "0 = off"
        );
        assert_eq!(
            of(&profile(KEEP_ALIVE_SECS, OverrideValue::Number(15.0))).keep_alive_secs,
            15
        );
        assert_eq!(
            of(&profile(KEEP_ALIVE_SECS, OverrideValue::Number(-1.0))).keep_alive_secs,
            60,
            "below the limit"
        );
        assert!(!of(&profile(CONFIRM_DANGEROUS, OverrideValue::Bool(false))).confirm_dangerous);
        assert!(of(&profile("no.such_setting", OverrideValue::Bool(false))).confirm_dangerous);
    }

    #[test]
    fn the_settings_file_is_read_like_the_app_reads_it() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("settings.json");
        // A fresh install has no file: the built-in types and the defaults.
        assert_eq!(
            ProfileSettings::load(&path).unwrap(),
            ProfileSettings::default()
        );
        std::fs::write(
            &path,
            r##"{"schema_version":2,"appearance":{"theme":"light"},"future":[1],
                "query":{"query_timeout_secs":30,"connect_timeout_secs":4},
                "results":{"page_size":75},"editor":{"confirm_dangerous":false},
                "connection_types":[
                    {"id":"production","name":"Production","read_only":true},
                    {"id":"custom-1","name":"Staging","agent_access":"read","color":"#112233"}]}"##,
        )
        .unwrap();
        let settings = ProfileSettings::load(&path).unwrap();
        let ids: Vec<_> = settings
            .connection_types
            .iter()
            .map(|ty| ty.id.as_str())
            .collect();
        assert_eq!(
            ids,
            [DEVELOPMENT, TEST, PRODUCTION, "custom-1"],
            "lost built-ins come back"
        );
        let globals = settings.globals();
        assert_eq!(
            (
                globals.query_timeout_secs,
                globals.connect_timeout_secs,
                globals.page_size
            ),
            (30, 4, 75)
        );
        assert!(!globals.confirm_dangerous);
        let staging = ConnectionProfile {
            type_id: "custom-1".into(),
            ..Default::default()
        };
        assert_eq!(settings.effective(&staging).agent_access, AgentAccess::Read);
        let production = ConnectionProfile {
            type_id: PRODUCTION.into(),
            ..Default::default()
        };
        assert!(settings.effective(&production).read_only);
    }

    #[test]
    fn a_settings_file_that_cannot_be_used_is_an_error_not_the_defaults() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("settings.json");
        std::fs::write(&path, "{ not json").unwrap();
        assert!(matches!(
            ProfileSettings::load(&path),
            Err(ProfileError::Invalid { .. })
        ));
        let error = ProfileSettings::load(dir.path()).unwrap_err();
        assert!(matches!(error, ProfileError::Unreadable { .. }), "{error}");
    }

    #[test]
    fn the_mcp_writes_switch_is_on_only_for_a_true_boolean_and_fails_closed() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("settings.json");
        let read = |text: &str| {
            std::fs::write(&path, text).unwrap();
            mcp_writes_enabled(Some(&path))
        };
        // No path, no file: off.
        assert!(!mcp_writes_enabled(None));
        assert!(!mcp_writes_enabled(Some(&path)));
        assert!(matches!(read_mcp_writes(&path), Ok(false)));
        assert!(read(
            r#"{"schema_version": 4, "ai": {"mcp_writes": true}, "future": [1]}"#
        ));
        assert!(read(r#"{"schema_version": 7, "ai": {"mcp_writes": true}}"#));
        assert!(!read(
            r#"{"schema_version": 4, "ai": {"mcp_writes": false}}"#
        ));
        // A `true` the app did not write: an older schema, none, or a version that is no number.
        for stray in [
            r#"{"schema_version": 3, "ai": {"mcp_writes": true}}"#,
            r#"{"schema_version": 0, "ai": {"mcp_writes": true}}"#,
            r#"{"ai": {"mcp_writes": true}}"#,
            r#"{"schema_version": "4", "ai": {"mcp_writes": true}}"#,
            r#"{"schema_version": 4.5e-1, "ai": {"mcp_writes": true}}"#,
            r#"{"schema_version": -4, "ai": {"mcp_writes": true}}"#,
            r#"{"schema_version": null, "ai": {"mcp_writes": true}}"#,
        ] {
            assert!(!read(stray), "{stray}");
            assert!(matches!(read_mcp_writes(&path), Ok(false)), "{stray}");
        }
        // A missing section or key is off.
        for off in ["{}", r#"{"ai": {}}"#, r#"{"editor": {"soft_wrap": true}}"#] {
            assert!(!read(off), "{off}");
        }
        // Garbage, the wrong type, a null, a directory, an empty file: off, and an error to say why.
        for bad in [
            "{ not json",
            "",
            "[]",
            "true",
            r#"[{"mcp_writes": true}]"#,
            r#"[{"ai": {"mcp_writes": true}}]"#,
            r#"{"ai": {"mcp_writes": "true"}}"#,
            r#"{"ai": {"mcp_writes": 1}}"#,
            r#"{"ai": {"mcp_writes": null}}"#,
            r#"{"ai": "on"}"#,
            r#"{"ai": null}"#,
        ] {
            assert!(!read(bad), "{bad}");
            assert!(
                matches!(read_mcp_writes(&path), Err(ProfileError::Invalid { .. })),
                "{bad}"
            );
        }
        assert!(!mcp_writes_enabled(Some(dir.path())));
        assert!(matches!(
            read_mcp_writes(dir.path()),
            Err(ProfileError::Unreadable { .. })
        ));
        // Flipping the file takes effect at the next read.
        assert!(read(r#"{"schema_version": 4, "ai": {"mcp_writes": true}}"#));
        assert!(!read(
            r#"{"schema_version": 4, "ai": {"mcp_writes": false}}"#
        ));
        assert!(read(r#"{"schema_version": 4, "ai": {"mcp_writes": true}}"#));
    }

    #[test]
    fn the_resulting_level_says_where_it_comes_from() {
        let mut types = ConnectionType::builtins();
        let of = |types: &[ConnectionType], type_id: &str, level: Option<&str>| {
            let mut profile = ConnectionProfile {
                type_id: type_id.into(),
                ..Default::default()
            };
            if let Some(level) = level {
                profile.overrides.set(AGENT_ACCESS, text(level));
            }
            AgentResolution::of(types, &profile)
        };
        assert_eq!(of(&types, "", None).note, "Off");
        assert_eq!(
            of(&types, DEVELOPMENT, Some("read_write")).note,
            "Read and write"
        );
        types
            .iter_mut()
            .find(|ty| ty.id == PRODUCTION)
            .unwrap()
            .read_only = true;
        let capped = of(&types, PRODUCTION, Some("read_write"));
        assert_eq!(capped.effective, AgentAccess::Read);
        assert_eq!(
            capped.note,
            "Read and write \u{2014} limited to Read by connection type Production (read-only)"
        );
        assert_eq!(of(&types, PRODUCTION, Some("read")).note, "Read");
        // A production preset is not inherited; a development preset is, and says so.
        for ty in &mut types {
            ty.agent_access = AgentLevel::Read;
        }
        let production = of(&types, PRODUCTION, None);
        assert_eq!(production.effective, AgentAccess::Off);
        assert!(
            production.note.contains("does not grant access by default"),
            "{}",
            production.note
        );
        let development = of(&types, DEVELOPMENT, None);
        assert_eq!(development.effective, AgentAccess::Read);
        assert_eq!(
            development.note,
            "Read \u{2014} the default of connection type Development"
        );
        assert_eq!(
            AgentLevel::from_access(AgentAccess::SchemaOnly),
            AgentLevel::Schema
        );
    }

    #[test]
    fn the_agent_sees_a_connection_without_its_address_or_secrets() {
        let mut settings = ProfileSettings::default();
        settings
            .connection_types
            .iter_mut()
            .find(|ty| ty.id == PRODUCTION)
            .unwrap()
            .read_only = true;
        let mut record = crate::records::tests_record_for("0F8FAD5B-D9CB-469F-A165-70867728950E");
        record.uri = "postgresql://alice:hunter2@db.internal/app".into();
        record.connection_type = PRODUCTION.into();
        record.overrides_json =
            r#"{"agent.access":"read_write","query.query_timeout_secs":20}"#.into();
        let info = settings
            .agent_info(&record)
            .expect("a PostgreSQL connection");
        assert_eq!(info.id, "0f8fad5b-d9cb-469f-a165-70867728950e");
        assert_eq!(info.kind, engine_agent::ConnectionKind::PostgreSql);
        assert_eq!(info.agent_access, Some(AgentAccess::ReadWrite));
        assert_eq!(
            info.effective_access(),
            AgentAccess::Read,
            "production + read-only caps at read"
        );
        assert!(info.connection_type.production && info.connection_type.read_only);
        assert_eq!(info.statement_timeout_secs, Some(20));
        let shown = format!("{info:?}");
        assert!(
            !shown.contains("hunter2") && !shown.contains("db.internal"),
            "{shown}"
        );
        record.db_type = "oracle".into();
        assert_eq!(settings.agent_info(&record), None, "no driver serves it");
    }
}
