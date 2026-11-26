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

/// The app's old switch "Let MCP clients run changes" in `settings.json`: `{"ai": {"mcp_writes": true}}`.
/// **Deprecated**: the app (schema 7 and later) no longer shows it and keeps writing `true` so that
/// an older `tablex-mcp` paired with it still allows writes on a connection set to Read and write.
/// `tablex-mcp` honours it only for a file written by an older app (see [`read_mcp_writes`]).
pub const MCP_WRITES_KEY: &str = "ai.mcp_writes";

/// The first `schema_version` of `settings.json` that has `ai.mcp_writes` (the app's `SCHEMA_VERSION`
/// when the switch was added). `true` in a file with a lower or no version is not honoured.
pub const MCP_WRITES_SCHEMA_VERSION: u64 = 4;

/// The first `schema_version` of `settings.json` for which a missing or `true` `ai.mcp_writes` is
/// ignored: from there on every write is put to the user per statement and no switch gates it. An
/// explicit `false` still blocks (see [`read_mcp_writes`]).
pub const MCP_WRITES_IGNORED_FROM_SCHEMA: u64 = 7;

/// Whether `tablex-mcp` may put changes to the user, as far as `settings.json` says.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum McpWrites {
    /// Each change is asked about (a file of schema 7 or later, or an older one with the switch on).
    Allowed,
    /// A file written by an older app (schema below 7) with the switch "Let MCP clients run changes" off.
    SwitchOff,
    /// No file, or one that cannot be read or understood: nothing is asked (fails closed).
    Unavailable,
}

/// Reads [`McpWrites`] from the file the app writes. A missing file is [`McpWrites::Unavailable`]; a file
/// that cannot be read, is not a JSON object, or holds anything but a boolean in `ai.mcp_writes` is
/// an error. With a numeric `schema_version` of at least [`MCP_WRITES_IGNORED_FROM_SCHEMA`] a missing
/// or `true` switch is ignored (writes are allowed) but an explicit `false` is honoured; below it (or
/// with no or a non-numeric version) it is honoured as before: only a `true` with a `schema_version`
/// of at least [`MCP_WRITES_SCHEMA_VERSION`] allows writes. (Walked by
/// hand rather than deserialized: serde would also read a struct out of a JSON array, and only an
/// object may allow anything.)
pub fn read_mcp_writes(path: &Path) -> Result<McpWrites, ProfileError> {
    let shown = path.display().to_string();
    let text = match std::fs::read_to_string(path) {
        Ok(text) => text,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            return Ok(McpWrites::Unavailable)
        }
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
    let version = root
        .get("schema_version")
        .and_then(serde_json::Value::as_u64);
    let switch = match root.get("ai") {
        None => None,
        Some(ai) => {
            let ai = ai
                .as_object()
                .ok_or_else(|| invalid("`ai` is not an object".into()))?;
            match ai.get("mcp_writes") {
                None => None,
                Some(serde_json::Value::Bool(on)) => Some(*on),
                Some(_) => return Err(invalid("`ai.mcp_writes` is not a boolean".into())),
            }
        }
    };
    let on = switch == Some(true);
    // From schema 7 the app keeps the switch `true` and a missing key means nothing; an explicit
    // `false` is the old switch turned off (an older app opened the file after a newer one wrote
    // it): the user's "no" still stands, whatever the version says.
    let ignored = version.is_some_and(|version| version >= MCP_WRITES_IGNORED_FROM_SCHEMA)
        && switch != Some(false);
    // An older app's migration forces the switch off when it adds it; a `true` in a file that has
    // not been through it (an older `schema_version`, or none) is not something the app wrote.
    let honoured = on && version.is_some_and(|version| version >= MCP_WRITES_SCHEMA_VERSION);
    Ok(if ignored || honoured {
        McpWrites::Allowed
    } else {
        McpWrites::SwitchOff
    })
}

/// [`McpWrites`] right now, for `tablex-mcp`, which reads it at the time of each write. **Fails
/// closed**: no path, a missing or unreadable file, a value that is not a boolean, all read as
/// [`McpWrites::Unavailable`].
pub fn mcp_writes_state(path: Option<&Path>) -> McpWrites {
    path.and_then(|path| read_mcp_writes(path).ok())
        .unwrap_or(McpWrites::Unavailable)
}

/// Whether MCP clients may be asked to run changes right now ([`mcp_writes_state`] is `Allowed`).
pub fn mcp_writes_enabled(path: Option<&Path>) -> bool {
    mcp_writes_state(path) == McpWrites::Allowed
}

/// The tools the user lets MCP clients use, in `settings.json`: `{"ai": {"mcp_tools": ["list_tables",
/// "execute_query"]}}`. Absent: every tool.
pub const MCP_TOOLS_KEY: &str = "ai.mcp_tools";

/// The tools [`MCP_TOOLS_KEY`] allows, read from the file at the time of each call: `None` allows
/// every tool (no file, no key, or `null`). **Fails closed** for what is there but unusable: a
/// file that cannot be read or parsed, or a value that is not a list of strings, allows no tool at
/// all, so a typo narrows the agent rather than widening it.
pub fn read_mcp_tools(path: Option<&Path>) -> Option<Vec<String>> {
    let path = path?;
    let text = match std::fs::read_to_string(path) {
        Ok(text) => text,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return None,
        Err(_) => return Some(Vec::new()),
    };
    let Ok(serde_json::Value::Object(root)) = serde_json::from_str::<serde_json::Value>(&text) else {
        return Some(Vec::new());
    };
    match root.get("ai")? {
        serde_json::Value::Object(ai) => match ai.get("mcp_tools")? {
            serde_json::Value::Null => None,
            serde_json::Value::Array(tools) => Some(
                tools
                    .iter()
                    .filter_map(|tool| tool.as_str().map(str::to_owned))
                    .collect(),
            )
            .filter(|names: &Vec<String>| names.len() == tools.len())
            .or(Some(Vec::new())),
            _ => Some(Vec::new()),
        },
        _ => Some(Vec::new()),
    }
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
/// The databases an AI agent may use on the connection: names separated by commas (`app,reports`).
/// Absent or empty: every database. Present but not text: none (it fails closed, like
/// [`AGENT_ACCESS`]).
pub const AGENT_DATABASES: &str = "agent.databases";
/// The user's note on the connection, shown to an AI agent in the connection list.
pub const CONNECTION_NOTES: &str = "connection.notes";

/// The most characters of a note an agent is shown.
const NOTES_MAX_CHARS: usize = 1000;

/// `host:port/database` of a saved URI, for the engines that have a host, with no user, password,
/// query or path: what an agent may be told about where a connection goes. `None` for SQLite,
/// a URI that cannot be read, and anything with no host.
pub fn endpoint_of(uri: &str) -> Option<String> {
    let (scheme, rest) = uri.split_once("://")?;
    if scheme.eq_ignore_ascii_case("sqlite") || scheme.eq_ignore_ascii_case("file") {
        return None;
    }
    let rest = rest.split(['?', '#']).next().unwrap_or(rest);
    let (authority, path) = rest.split_once('/').unwrap_or((rest, ""));
    // The user and password come before the last `@`.
    let hosts = authority.rsplit_once('@').map_or(authority, |(_, hosts)| hosts);
    if hosts.is_empty() || hosts.chars().any(|ch| ch.is_whitespace() || ch.is_control()) {
        return None;
    }
    let database = path.split('/').next().unwrap_or_default();
    let mut endpoint = hosts.to_owned();
    if !database.is_empty() {
        endpoint.push('/');
        endpoint.push_str(database);
    }
    Some(endpoint.chars().take(200).collect())
}

impl Overrides {
    /// The databases agents may use ([`AGENT_DATABASES`]); `None` is all of them.
    pub fn agent_databases(&self) -> Option<Vec<String>> {
        match self.get(AGENT_DATABASES)? {
            OverrideValue::Text(text) => {
                let names: Vec<String> = text
                    .split(',')
                    .map(str::trim)
                    .filter(|name| !name.is_empty())
                    .map(str::to_owned)
                    .collect();
                (!names.is_empty()).then_some(names)
            }
            _ => Some(Vec::new()),
        }
    }

    /// The user's note on the connection ([`CONNECTION_NOTES`]).
    pub fn notes(&self) -> Option<String> {
        match self.get(CONNECTION_NOTES)? {
            OverrideValue::Text(text) => {
                let text = text.trim();
                (!text.is_empty()).then(|| text.chars().take(NOTES_MAX_CHARS).collect())
            }
            _ => None,
        }
    }
}

/// Stored beside the overrides, but not a setting (it is not in the registry, nothing resolves it
/// against a global value): `true` = the connection lists only the database its address names.
/// Absent (every connection saved before it existed) = every database, so nothing disappears.
pub const ONLY_CONNECTION_DATABASE: &str = "navigator.only_connection_database";

/// Kinds whose navigator lists databases of one server, so that listing can be narrowed to one.
pub const fn supports_database_scope(kind: ConnectionKind) -> bool {
    matches!(
        kind,
        ConnectionKind::Postgres
            | ConnectionKind::Mysql
            | ConnectionKind::Mariadb
            | ConnectionKind::Clickhouse
            | ConnectionKind::Mssql
    )
}

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
            Self::Schema => "Agents see table, column and routine definitions, never rows.",
            Self::Read => "Agents may also run statements that only read data.",
            Self::ReadWrite => "Agents may propose writes; each one is put to you for approval first.",
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
    /// What an AI agent may do on connections of this type that set nothing themselves. A production
    /// type never grants it (the connection has to say so itself); a read-only type caps it at
    /// `read` ([`AgentAccess::effective`]).
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
    ///
    /// `global` is the default level of Settings -> AI: a type that is not production gets
    /// `max(its preset, global)`, and so does a connection without a type. A production type
    /// keeps its own (the policy gives it no preset without a setting).
    pub fn agent_info_in(types: &[Self], type_id: &str, global: AgentLevel) -> ConnectionTypeInfo {
        match find_type(types, type_id) {
            Some(ty) => {
                let mut info = ty.agent_info();
                if !info.production {
                    let preset = ty.agent_access.access().max(global.access());
                    info.default_agent_access = Some(preset);
                }
                info
            }
            None if type_id.is_empty() => ConnectionTypeInfo {
                default_agent_access: Some(global.access()),
                ..ConnectionTypeInfo::default()
            },
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

    /// The connection lists only the database its address names (see [`ONLY_CONNECTION_DATABASE`]).
    pub fn only_connection_database(&self) -> bool {
        matches!(self.get(ONLY_CONNECTION_DATABASE), Some(OverrideValue::Bool(true)))
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
/// the default: the preset of its connection type, which is `off` unless the type says otherwise);
/// every other override stays.
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
    pub fn of(types: &[ConnectionType], global: AgentLevel, profile: &ConnectionProfile) -> Self {
        let connection_type = find_type(types, &profile.type_id);
        let info = ConnectionType::agent_info_in(types, &profile.type_id, global);
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
            None if effective > AgentAccess::Off
                && connection_type.is_none_or(|ty| effective > ty.agent_access.access()) =>
            {
                format!(
                    "{} \u{2014} the default for all connections (Settings \u{2192} AI)",
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
    /// Settings -> AI "Default agent access".
    pub agent_access: AgentLevel,
}

impl Default for Globals {
    fn default() -> Self {
        Self {
            query_timeout_secs: 0,
            connect_timeout_secs: 0,
            keep_alive_secs: DEFAULT_KEEP_ALIVE_SECS,
            page_size: DEFAULT_PAGE_SIZE,
            confirm_dangerous: true,
            agent_access: AgentLevel::Off,
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
        let type_info = ConnectionType::agent_info_in(types, &profile.type_id, globals.agent_access);
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
    #[serde(deserialize_with = "lenient_ai")]
    ai: AiValues,
}

#[derive(Debug, Clone, Default, PartialEq)]
struct AiValues {
    agent_access: AgentLevel,
    /// `ai.mcp_databases`: connection id or name (lower case) to the databases agents may use.
    databases: Vec<(String, Vec<String>)>,
}

impl AiValues {
    /// The databases the settings file limits the connection to (`None`: it says nothing).
    fn databases_of(&self, id: &str, name: &str) -> Option<Vec<String>> {
        let (id, name) = (id.to_ascii_lowercase(), name.to_lowercase());
        self.databases
            .iter()
            .find(|(key, _)| *key == id || *key == name)
            .map(|(_, databases)| databases.clone())
    }
}

/// The `ai` section as far as this reader needs it. Lenient: a value that is not a level id (or an
/// `ai` that is not an object) is `off`; a bad value never fails the whole file.
fn lenient_ai<'de, D: serde::Deserializer<'de>>(deserializer: D) -> Result<AiValues, D::Error> {
    let value = serde_json::Value::deserialize(deserializer)?;
    let agent_access = value
        .get("agent_access")
        .and_then(serde_json::Value::as_str)
        .and_then(AgentLevel::from_id)
        .unwrap_or(AgentLevel::Off);
    // `{"mcp_databases": {"<connection id or name>": ["shop", "reports"]}}`. An entry that is not
    // a list of names allows no database at all (it fails closed, like the tool list).
    let databases = match value.get("mcp_databases") {
        Some(serde_json::Value::Object(entries)) => entries
            .iter()
            .map(|(key, names)| {
                let names = match names {
                    serde_json::Value::Array(names) => names
                        .iter()
                        .map(|name| name.as_str().map(str::to_owned))
                        .collect::<Option<Vec<String>>>()
                        .unwrap_or_default(),
                    _ => Vec::new(),
                };
                (key.to_lowercase(), names)
            })
            .collect(),
        _ => Vec::new(),
    };
    Ok(AiValues { agent_access, databases })
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
            ai: AiValues::default(),
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
            agent_access: self.ai.agent_access,
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
                self.ai.agent_access,
            ),
            agent_access: profile.agent_level().map(AgentLevel::access),
            statement_timeout_secs: effective.query_timeout.map(|timeout| timeout.as_secs()),
            // The connection's own setting wins; else the list the settings file keeps for it.
            allowed_databases: profile.overrides.agent_databases().or_else(|| {
                self.ai
                    .databases_of(&crate::records::display_connection_id(&record.id), &record.name)
            }),
            endpoint: endpoint_of(&record.uri),
            // The folders come from the sidebar's own file, which the catalog reads.
            group: Vec::new(),
            notes: profile.overrides.notes(),
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
    fn listing_one_database_is_an_override_only_when_it_is_true_and_only_for_kinds_with_databases() {
        // Saved before the option existed, or with it off in any form: every database.
        for json in ["", "{}", r#"{"results.page_size":50}"#, r#"{"navigator.only_connection_database":false}"#, r#"{"navigator.only_connection_database":"true"}"#] {
            assert!(!Overrides::from_json(json).only_connection_database(), "{json}");
        }
        let mut overrides = Overrides::from_json(r#"{"results.page_size":50}"#);
        overrides.set(ONLY_CONNECTION_DATABASE, OverrideValue::Bool(true));
        assert!(overrides.only_connection_database());
        // It is stored with the others and read back, and a copy of the record keeps it.
        let json = overrides.to_json();
        assert_eq!(json, r#"{"navigator.only_connection_database":true,"results.page_size":50.0}"#);
        assert!(Overrides::from_json(&json).only_connection_database());
        assert!(Overrides::from_json(&without_agent_access(&json)).only_connection_database());
        // It is not a setting a connection overrides.
        assert!(!OVERRIDABLE_IDS.contains(&ONLY_CONNECTION_DATABASE));
        let kinds = [
            (ConnectionKind::Postgres, true),
            (ConnectionKind::Mysql, true),
            (ConnectionKind::Mariadb, true),
            (ConnectionKind::Clickhouse, true),
            (ConnectionKind::Mssql, true),
            (ConnectionKind::Sqlite, false),
            (ConnectionKind::Mongodb, false),
            (ConnectionKind::Redis, false),
        ];
        for (kind, supported) in kinds {
            assert_eq!(supports_database_scope(kind), supported, "{kind:?}");
        }
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
    fn a_type_preset_applies_to_unset_connections_up_to_read_write_and_never_on_production() {
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
        // A type may preset Read and write; production still has no preset.
        types[0].agent_access = AgentLevel::ReadWrite;
        types[2].agent_access = AgentLevel::ReadWrite;
        assert_eq!(effective(&types, DEVELOPMENT, None), AgentAccess::ReadWrite);
        assert_eq!(effective(&types, PRODUCTION, None), AgentAccess::Off);
        // A read-only type still caps it at read.
        types[0].read_only = true;
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
        let note = AgentResolution::of(&types, AgentLevel::Off, &gone(Some("read_write"))).note;
        assert_eq!(
            note,
            "Read and write \u{2014} limited to Read: connection type not found"
        );
        assert!(AgentResolution::of(&types, AgentLevel::Off, &gone(None))
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
    fn an_endpoint_is_host_port_and_database_and_never_the_login_the_query_or_a_path() {
        for (uri, endpoint) in [
            ("postgresql://app:s3cret@db.internal:5432/shop?sslmode=require", Some("db.internal:5432/shop")),
            ("postgresql://app@db.internal/shop", Some("db.internal/shop")),
            ("mysql://db.internal:3306", Some("db.internal:3306")),
            ("mongodb://u:p@h1:27017,h2:27017/app?replicaSet=rs0&authSource=admin", Some("h1:27017,h2:27017/app")),
            ("mongodb+srv://u@cluster.example.net/app", Some("cluster.example.net/app")),
            ("redis://:pw@cache.internal:6379/2", Some("cache.internal:6379/2")),
            ("clickhouse://default@ch.internal:8123/analytics?secure=true", Some("ch.internal:8123/analytics")),
            ("sqlite:///var/data/app.db", None),
            ("SQLITE:///x.db", None),
            ("not a uri", None),
            ("postgresql:///shop", None),
        ] {
            assert_eq!(endpoint_of(uri).as_deref(), endpoint, "{uri}");
        }
        let shown = endpoint_of("postgresql://app:s3cret@db.internal:5432/shop").unwrap();
        assert!(!shown.contains("s3cret") && !shown.contains("app@"));
    }

    #[test]
    fn the_databases_agents_may_use_and_the_note_come_from_the_overrides_of_the_connection() {
        let overrides = |json: &str| Overrides::from_json(json);
        assert_eq!(overrides("").agent_databases(), None);
        assert_eq!(overrides(r#"{"results.page_size":5}"#).agent_databases(), None);
        assert_eq!(
            overrides(r#"{"agent.databases":" shop , reports,,"}"#).agent_databases(),
            Some(vec!["shop".to_owned(), "reports".to_owned()])
        );
        // Empty text is "no limit" (the box was cleared); anything that is not text is none at all.
        assert_eq!(overrides(r#"{"agent.databases":"  "}"#).agent_databases(), None);
        assert_eq!(overrides(r#"{"agent.databases":true}"#).agent_databases(), Some(Vec::new()));
        assert_eq!(overrides(r#"{"agent.databases":5}"#).agent_databases(), Some(Vec::new()));
        assert_eq!(overrides(r#"{"connection.notes":"  the orders replica "}"#).notes().as_deref(), Some("the orders replica"));
        assert_eq!(overrides(r#"{"connection.notes":""}"#).notes(), None);
        assert_eq!(overrides(r#"{"connection.notes":7}"#).notes(), None);
        let long = format!(r#"{{"connection.notes":"{}"}}"#, "n".repeat(5000));
        assert_eq!(overrides(&long).notes().unwrap().chars().count(), NOTES_MAX_CHARS);
    }

    #[test]
    fn the_agent_view_of_a_connection_has_its_endpoint_note_and_allowed_databases() {
        let record = SavedConnectionRecord {
            uri: "postgresql://app:pw@db.internal:5432/shop?sslmode=require".into(),
            db_type: "postgresql".into(),
            overrides_json: r#"{"agent.access":"read","agent.databases":"shop,reports","connection.notes":"replica"}"#.into(),
            ..crate::records::tests_record_for("X")
        };
        let info = ProfileSettings::default().agent_info(&record).expect("a connection");
        assert_eq!(info.endpoint.as_deref(), Some("db.internal:5432/shop"));
        assert_eq!(info.allowed_databases, Some(vec!["shop".to_owned(), "reports".to_owned()]));
        assert_eq!(info.notes.as_deref(), Some("replica"));
        let shown = format!("{info:?}");
        assert!(!shown.contains("pw@") && !shown.contains("sslmode"), "{shown}");
        // A connection without any of them has none.
        let plain = SavedConnectionRecord { uri: "sqlite:///x".into(), db_type: "sqlite".into(), overrides_json: String::new(), ..record };
        let info = ProfileSettings::default().agent_info(&plain).expect("a connection");
        assert_eq!((info.endpoint, info.allowed_databases, info.notes), (None, None, None));
    }

    #[test]
    fn the_settings_file_can_limit_the_databases_of_a_connection_by_id_or_name() {
        let settings: ProfileSettings = serde_json::from_str(
            r#"{"ai": {"mcp_databases": {"Orders DB": ["shop", "reports"], "7B1D3F0E-3A55-4D8E-9A54-0C6F2F4E9A11": ["a"], "broken": "shop", "mixed": ["a", 1]}}}"#,
        )
        .unwrap();
        let record = |id: &str, name: &str, overrides: &str| SavedConnectionRecord {
            id: id.into(),
            name: name.into(),
            uri: "postgresql://h/db".into(),
            db_type: "postgresql".into(),
            overrides_json: overrides.into(),
            ..crate::records::tests_record_for("X")
        };
        let databases = |record: &SavedConnectionRecord| settings.agent_info(record).unwrap().allowed_databases;
        assert_eq!(databases(&record("1", "orders db", "")), Some(vec!["shop".to_owned(), "reports".to_owned()]), "by name, any case");
        assert_eq!(databases(&record("7b1d3f0e-3a55-4d8e-9a54-0c6f2f4e9a11", "x", "")), Some(vec!["a".to_owned()]), "by id, any case");
        assert_eq!(databases(&record("2", "broken", "")), Some(Vec::new()), "not a list: none");
        assert_eq!(databases(&record("3", "mixed", "")), Some(Vec::new()), "not only names: none");
        assert_eq!(databases(&record("4", "other", "")), None);
        // The connection's own override wins over the file.
        assert_eq!(databases(&record("5", "orders db", r#"{"agent.databases":"only"}"#)), Some(vec!["only".to_owned()]));
        // No `ai.mcp_databases`, or one that is not an object: nothing is limited.
        let plain: ProfileSettings = serde_json::from_str(r#"{"ai": {"mcp_databases": []}}"#).unwrap();
        assert_eq!(plain.agent_info(&record("6", "orders db", "")).unwrap().allowed_databases, None);
    }

    #[test]
    fn the_tool_allowlist_is_read_from_settings_and_fails_closed() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("settings.json");
        let read = |text: Option<&str>| {
            match text {
                Some(text) => std::fs::write(&path, text).unwrap(),
                None => drop(std::fs::remove_file(&path)),
            }
            read_mcp_tools(Some(&path))
        };
        assert_eq!(read_mcp_tools(None), None);
        assert_eq!(read(None), None, "no file: every tool");
        assert_eq!(read(Some("{}")), None);
        assert_eq!(read(Some(r#"{"ai":{}}"#)), None);
        assert_eq!(read(Some(r#"{"ai":{"mcp_tools":null}}"#)), None);
        assert_eq!(
            read(Some(r#"{"schema_version":4,"ai":{"mcp_tools":["list_tables","execute_query"],"agent_access":"read"}}"#)),
            Some(vec!["list_tables".to_owned(), "execute_query".to_owned()])
        );
        assert_eq!(read(Some(r#"{"ai":{"mcp_tools":[]}}"#)), Some(Vec::new()), "an empty list allows none");
        // What is there but unusable allows nothing.
        for bad in [r#"{"ai":{"mcp_tools":"list_tables"}}"#, r#"{"ai":{"mcp_tools":["a", 1]}}"#, r#"{"ai":{"mcp_tools":{}}}"#, r#"{"ai":"x"}"#, "not json", "[]"] {
            assert_eq!(read(Some(bad)), Some(Vec::new()), "{bad}");
        }
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
    fn the_mcp_writes_switch_counts_only_for_a_file_of_an_older_app_and_fails_closed() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("settings.json");
        let read = |text: &str| {
            std::fs::write(&path, text).unwrap();
            mcp_writes_enabled(Some(&path))
        };
        // No path, no file: off.
        assert!(!mcp_writes_enabled(None));
        assert!(!mcp_writes_enabled(Some(&path)));
        assert_eq!(mcp_writes_state(Some(&path)), McpWrites::Unavailable);
        assert_eq!(mcp_writes_state(None), McpWrites::Unavailable);
        assert!(read(
            r#"{"schema_version": 4, "ai": {"mcp_writes": true}, "future": [1]}"#
        ));
        assert!(read(r#"{"schema_version": 7, "ai": {"mcp_writes": true}}"#));
        // A file written by an older app (below schema 7): `false` still blocks.
        assert!(!read(
            r#"{"schema_version": 4, "ai": {"mcp_writes": false}}"#
        ));
        assert!(!read(
            r#"{"schema_version": 6, "ai": {"mcp_writes": false}}"#
        ));
        assert_eq!(mcp_writes_state(Some(&path)), McpWrites::SwitchOff);
        // Schema 7 and later: the switch is ignored (the app keeps it `true`, and no key at all
        // changes nothing)...
        for ignored in [
            r#"{"schema_version": 7, "ai": {"mcp_writes": true}}"#,
            r#"{"schema_version": 7, "ai": {}}"#,
            r#"{"schema_version": 7}"#,
            r#"{"schema_version": 8, "ai": {"agent_access": "read_write"}}"#,
        ] {
            assert!(read(ignored), "{ignored}");
            assert_eq!(read_mcp_writes(&path).unwrap(), McpWrites::Allowed);
        }
        // ...but an explicit `false` is the user's old switch turned off (a downgrade and back):
        // it blocks at any schema.
        for off in [
            r#"{"schema_version": 7, "ai": {"mcp_writes": false}}"#,
            r#"{"schema_version": 8, "ai": {"mcp_writes": false, "agent_access": "read_write"}}"#,
            r#"{"schema_version": 99, "ai": {"mcp_writes": false}}"#,
        ] {
            assert!(!read(off), "{off}");
            assert_eq!(read_mcp_writes(&path).unwrap(), McpWrites::SwitchOff, "{off}");
        }
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
            assert!(matches!(read_mcp_writes(&path), Ok(McpWrites::SwitchOff)), "{stray}");
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
            AgentResolution::of(types, AgentLevel::Off, &profile)
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
    fn the_global_agent_default_fills_in_what_nothing_else_sets() {
        let mut types = ConnectionType::builtins();
        let with_global = |types: &[ConnectionType],
                           global: AgentLevel,
                           type_id: &str,
                           level: Option<&str>| {
            let mut profile = ConnectionProfile {
                type_id: type_id.into(),
                ..Default::default()
            };
            if let Some(level) = level {
                profile.overrides.set(AGENT_ACCESS, text(level));
            }
            let globals = Globals {
                agent_access: global,
                ..Globals::default()
            };
            let effective = Effective::resolve(globals, types, &profile).agent_access;
            let resolution = AgentResolution::of(types, global, &profile);
            assert_eq!(resolution.effective, effective, "the dialog agrees");
            (effective, resolution.note)
        };
        // No type: the global is the level.
        assert_eq!(
            with_global(&types, AgentLevel::Read, "", None),
            (
                AgentAccess::Read,
                "Read \u{2014} the default for all connections (Settings \u{2192} AI)".into()
            )
        );
        assert_eq!(with_global(&types, AgentLevel::Off, "", None).0, AgentAccess::Off);
        // A type whose preset is lower is raised; a higher preset stays.
        assert_eq!(
            with_global(&types, AgentLevel::Read, DEVELOPMENT, None),
            (
                AgentAccess::Read,
                "Read \u{2014} the default for all connections (Settings \u{2192} AI)".into()
            )
        );
        assert_eq!(
            with_global(&types, AgentLevel::Schema, DEVELOPMENT, None).0,
            AgentAccess::SchemaOnly
        );
        for ty in &mut types {
            ty.agent_access = AgentLevel::Read;
        }
        assert_eq!(
            with_global(&types, AgentLevel::Off, DEVELOPMENT, None),
            (
                AgentAccess::Read,
                "Read \u{2014} the default of connection type Development".into()
            )
        );
        let (level, note) = with_global(&types, AgentLevel::Schema, DEVELOPMENT, None);
        assert_eq!(level, AgentAccess::Read);
        assert!(note.contains("default of connection type"), "{note}");
        // Production keeps no default; an explicit level (even off) wins; a dangling type stays closed.
        assert_eq!(
            with_global(&types, AgentLevel::Read, PRODUCTION, None).0,
            AgentAccess::Off
        );
        assert_eq!(
            with_global(&types, AgentLevel::Read, "", Some("off")).0,
            AgentAccess::Off
        );
        assert_eq!(
            with_global(&types, AgentLevel::Read, DEVELOPMENT, Some("schema")).0,
            AgentAccess::SchemaOnly
        );
        assert_eq!(
            with_global(&types, AgentLevel::Read, "custom-gone", None).0,
            AgentAccess::Off
        );
    }

    #[test]
    fn a_global_read_write_reaches_every_connection_that_is_not_production() {
        let mut types = ConnectionType::builtins();
        let rw = |types: &[ConnectionType], type_id: &str, level: Option<&str>| {
            let mut profile = ConnectionProfile {
                type_id: type_id.into(),
                ..Default::default()
            };
            if let Some(level) = level {
                profile.overrides.set(AGENT_ACCESS, text(level));
            }
            let globals = Globals {
                agent_access: AgentLevel::ReadWrite,
                ..Globals::default()
            };
            Effective::resolve(globals, types, &profile).agent_access
        };
        // No type, and the built-in and a custom non-production type, with nothing of their own.
        types.push(ConnectionType {
            id: "custom-1".into(),
            name: "Staging".into(),
            ..ConnectionType::default()
        });
        for type_id in ["", DEVELOPMENT, TEST, "custom-1"] {
            assert_eq!(rw(&types, type_id, None), AgentAccess::ReadWrite, "{type_id:?}");
        }
        // Production is as before: no default, whatever the global says; its own level still works.
        assert_eq!(rw(&types, PRODUCTION, None), AgentAccess::Off);
        assert_eq!(rw(&types, PRODUCTION, Some("read")), AgentAccess::Read);
        assert_eq!(rw(&types, PRODUCTION, Some("read_write")), AgentAccess::ReadWrite);
        // A type id nobody defines stays restrictive: no default, and a read-only cap.
        assert_eq!(rw(&types, "custom-gone", None), AgentAccess::Off);
        assert_eq!(rw(&types, "custom-gone", Some("read_write")), AgentAccess::Read);
        // The connection's own level still wins; a read-only type caps the default at read.
        assert_eq!(rw(&types, DEVELOPMENT, Some("read")), AgentAccess::Read);
        assert_eq!(rw(&types, DEVELOPMENT, Some("off")), AgentAccess::Off);
        types[0].read_only = true;
        assert_eq!(rw(&types, DEVELOPMENT, None), AgentAccess::Read);
        // The agent's view (what the MCP server and the assistant use) agrees.
        let info = ConnectionType::agent_info_in(&types, TEST, AgentLevel::ReadWrite);
        assert_eq!(info.default_agent_access, Some(AgentAccess::ReadWrite));
        let info = ConnectionType::agent_info_in(&types, "", AgentLevel::ReadWrite);
        assert_eq!(info.default_agent_access, Some(AgentAccess::ReadWrite));
        let info = ConnectionType::agent_info_in(&types, PRODUCTION, AgentLevel::ReadWrite);
        assert_eq!(AgentAccess::effective(None, &info), AgentAccess::Off);
        let info = ConnectionType::agent_info_in(&types, "custom-gone", AgentLevel::ReadWrite);
        assert_eq!(AgentAccess::effective(None, &info), AgentAccess::Off);
        // A type preset of its own may be Read and write too, with a lower global.
        types[1].agent_access = AgentLevel::ReadWrite;
        let info = ConnectionType::agent_info_in(&types, TEST, AgentLevel::Read);
        assert_eq!(info.default_agent_access, Some(AgentAccess::ReadWrite));
    }

    #[test]
    fn the_settings_file_gives_the_global_agent_level_and_never_fails_on_it() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("settings.json");
        let level = |text: &str| {
            std::fs::write(&path, text).unwrap();
            ProfileSettings::load(&path).unwrap().globals().agent_access
        };
        assert_eq!(level("{}"), AgentLevel::Off);
        assert_eq!(level(r#"{"ai": {"agent_access": "schema"}}"#), AgentLevel::Schema);
        assert_eq!(level(r#"{"ai": {"agent_access": "read"}}"#), AgentLevel::Read);
        assert_eq!(level(r#"{"ai": {"agent_access": "read_write"}}"#), AgentLevel::ReadWrite);
        for bad in [
            r#"{"ai": {"agent_access": "everything"}}"#,
            r#"{"ai": {"agent_access": 3}}"#,
            r#"{"ai": {"agent_access": null}}"#,
            r#"{"ai": "on"}"#,
            r#"{"ai": null}"#,
        ] {
            assert_eq!(level(bad), AgentLevel::Off, "{bad}");
        }
        // The rest of the file still loads next to a bad value.
        std::fs::write(
            &path,
            r#"{"ai": {"agent_access": "bogus"}, "results": {"page_size": 75}}"#,
        )
        .unwrap();
        let settings = ProfileSettings::load(&path).unwrap();
        assert_eq!(settings.globals().page_size, 75);
        // The agent's view of a connection carries the global.
        std::fs::write(&path, r#"{"ai": {"agent_access": "read"}}"#).unwrap();
        let settings = ProfileSettings::load(&path).unwrap();
        let record = crate::records::tests_record_for("0F8FAD5B-D9CB-469F-A165-70867728950E");
        let info = settings.agent_info(&record).expect("a connection");
        assert_eq!(info.effective_access(), AgentAccess::Read);
        assert_eq!(info.agent_access, None);
    }

    #[test]
    fn the_agent_sees_a_connection_with_its_endpoint_and_without_its_login_or_secrets() {
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
        // Where it goes (host and database) is told; the login and the password never are.
        assert_eq!(info.endpoint.as_deref(), Some("db.internal/app"));
        assert!(
            !shown.contains("hunter2") && !shown.contains("alice"),
            "{shown}"
        );
        record.db_type = "oracle".into();
        assert_eq!(settings.agent_info(&record), None, "no driver serves it");
    }
}
