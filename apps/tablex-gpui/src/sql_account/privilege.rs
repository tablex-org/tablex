//! Privileges, what they are granted on, and the statements that take an account from the privileges it holds to the ones
//! it should hold.
//!
//! A [`Privilege`] is one of the names the editor offers (the fixed lists below); a name the catalog reports that is not in
//! them (a dynamic privilege of MySQL 8, `MAINTAIN` of PostgreSQL 17) is shown as text and never touched. [`PrivSet`] maps
//! each privilege held to whether it was granted `WITH GRANT OPTION`: PostgreSQL keeps that per privilege; MySQL and MariaDB
//! have `GRANT OPTION` as a privilege of its own, listed in the set like the others (its value is always `false`).

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

/// A privilege the editor can grant and revoke.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize)]
pub struct Privilege(&'static str);

impl Privilege {
    pub const GRANT_OPTION: Self = Self("GRANT OPTION");

    pub fn name(self) -> &'static str {
        self.0
    }

    /// The offered privilege called `name` (any case) among `offered`.
    pub fn find(offered: &[Self], name: &str) -> Option<Self> {
        offered.iter().copied().find(|privilege| privilege.0.eq_ignore_ascii_case(name.trim()))
    }
}

impl std::fmt::Display for Privilege {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.0)
    }
}

impl<'de> Deserialize<'de> for Privilege {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let name = String::deserialize(deserializer)?;
        PG_DATABASE
            .iter()
            .chain(PG_SCHEMA)
            .chain(PG_TABLE)
            .chain(MYSQL_GLOBAL)
            .chain(MYSQL_DATABASE)
            .chain(MYSQL_TABLE)
            .copied()
            .find(|privilege| privilege.0 == name)
            .ok_or_else(|| serde::de::Error::custom(format!("unknown privilege {name}")))
    }
}

macro_rules! privileges {
    ($($name:literal),+ $(,)?) => { &[$(Privilege($name)),+] };
}

pub const PG_DATABASE: &[Privilege] = privileges!["CONNECT", "CREATE", "TEMPORARY"];
pub const PG_SCHEMA: &[Privilege] = privileges!["USAGE", "CREATE"];
pub const PG_TABLE: &[Privilege] = privileges!["SELECT", "INSERT", "UPDATE", "DELETE", "TRUNCATE", "REFERENCES", "TRIGGER"];

pub const MYSQL_GLOBAL: &[Privilege] = privileges![
    "SELECT", "INSERT", "UPDATE", "DELETE", "CREATE", "DROP", "ALTER", "INDEX", "REFERENCES", "TRIGGER", "CREATE VIEW", "SHOW VIEW", "CREATE ROUTINE",
    "ALTER ROUTINE", "EXECUTE", "EVENT", "CREATE TEMPORARY TABLES", "LOCK TABLES", "SHOW DATABASES", "PROCESS", "FILE", "RELOAD", "SHUTDOWN", "SUPER",
    "REPLICATION CLIENT", "REPLICATION SLAVE", "CREATE USER", "CREATE TABLESPACE", "GRANT OPTION"
];
pub const MYSQL_DATABASE: &[Privilege] = privileges![
    "SELECT", "INSERT", "UPDATE", "DELETE", "CREATE", "DROP", "ALTER", "INDEX", "REFERENCES", "TRIGGER", "CREATE VIEW", "SHOW VIEW", "CREATE ROUTINE",
    "ALTER ROUTINE", "EXECUTE", "EVENT", "CREATE TEMPORARY TABLES", "LOCK TABLES", "GRANT OPTION"
];
pub const MYSQL_TABLE: &[Privilege] =
    privileges!["SELECT", "INSERT", "UPDATE", "DELETE", "CREATE", "DROP", "ALTER", "INDEX", "REFERENCES", "TRIGGER", "CREATE VIEW", "SHOW VIEW", "GRANT OPTION"];

/// What privileges are granted on.
#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(tag = "on", rename_all = "snake_case")]
pub enum GrantTarget {
    /// Every database of the server (`*.*`; MySQL, MariaDB).
    Global,
    /// A PostgreSQL database, or a MySQL database. A MySQL name is a pattern as the server keeps it: `_` and `%` match
    /// unless a backslash escapes them ([`escape_pattern`]).
    Database { name: String },
    /// A PostgreSQL schema.
    Schema { name: String },
    /// A table, view or materialized view. `schema` is the MySQL database.
    Table { schema: String, name: String },
}

impl GrantTarget {
    /// The privileges the editor offers on this kind of thing for a server of `mysql` (or PostgreSQL).
    pub fn offered(&self, mysql: bool) -> &'static [Privilege] {
        match (self, mysql) {
            (Self::Global, _) => MYSQL_GLOBAL,
            (Self::Database { .. }, false) => PG_DATABASE,
            (Self::Database { .. }, true) => MYSQL_DATABASE,
            (Self::Schema { .. }, _) => PG_SCHEMA,
            (Self::Table { .. }, false) => PG_TABLE,
            (Self::Table { .. }, true) => MYSQL_TABLE,
        }
    }

    /// What the target is, for a heading.
    pub fn noun(&self, mysql: bool) -> &'static str {
        match (self, mysql) {
            (Self::Global, _) => "server",
            (Self::Database { .. }, _) => "database",
            (Self::Schema { .. }, _) => "schema",
            (Self::Table { .. }, _) => "table",
        }
    }

    /// The name as a person reads it (a MySQL database pattern without its escapes).
    pub fn label(&self, mysql: bool) -> String {
        match self {
            Self::Global => "All databases (*.*)".to_owned(),
            // A database name with an unescaped `_` or `%` is a pattern that stands for several databases: told apart from the database
            // whose name is the same text (listed escaped).
            Self::Database { name } if mysql && has_wildcard(name) => format!("{} (pattern)", unescape_pattern(name)),
            Self::Database { name } if mysql => unescape_pattern(name),
            Self::Database { name } | Self::Schema { name } => name.clone(),
            Self::Table { schema, name } if mysql => format!("{}.{name}", unescape_pattern(schema)),
            Self::Table { schema, name } => format!("{schema}.{name}"),
        }
    }
}

/// A MySQL database name as `GRANT ... ON db.*` reads it when `_` and `%` must match themselves.
pub fn escape_pattern(name: &str) -> String {
    let mut out = String::with_capacity(name.len() + 2);
    for c in name.chars() {
        if matches!(c, '_' | '%' | '\\') {
            out.push('\\');
        }
        out.push(c);
    }
    out
}

/// Whether `pattern` has a `_` or `%` that no backslash escapes (it matches more than one name).
pub fn has_wildcard(pattern: &str) -> bool {
    let mut chars = pattern.chars();
    while let Some(c) = chars.next() {
        match c {
            '\\' => {
                chars.next();
            }
            '_' | '%' => return true,
            _ => {}
        }
    }
    false
}

/// [`escape_pattern`] undone, for display.
pub fn unescape_pattern(pattern: &str) -> String {
    let mut out = String::with_capacity(pattern.len());
    let mut chars = pattern.chars();
    while let Some(c) = chars.next() {
        match (c, chars.clone().next()) {
            ('\\', Some(next @ ('_' | '%' | '\\'))) => {
                out.push(next);
                chars.next();
            }
            _ => out.push(c),
        }
    }
    out
}

/// The privileges held on one target; the value is `WITH GRANT OPTION` (PostgreSQL), always `false` for MySQL, where
/// [`Privilege::GRANT_OPTION`] is a member.
pub type PrivSet = BTreeMap<Privilege, bool>;

/// What to run to take `held` to `wanted` on one target of one grantee, for a server of `mysql` (or PostgreSQL).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct GrantDelta {
    /// Grants without the grant option.
    pub grant: Vec<Privilege>,
    /// Grants `WITH GRANT OPTION` (PostgreSQL; also lifts a privilege already held to it).
    pub grant_with_option: Vec<Privilege>,
    /// Revokes the privileges.
    pub revoke: Vec<Privilege>,
    /// `REVOKE GRANT OPTION FOR` the privileges (PostgreSQL), keeping them.
    pub revoke_option: Vec<Privilege>,
}

#[cfg(test)]
impl GrantDelta {
    pub fn is_empty(&self) -> bool {
        self.grant.is_empty() && self.grant_with_option.is_empty() && self.revoke.is_empty() && self.revoke_option.is_empty()
    }
}

/// The difference between `held` and `wanted`. Only the privileges in `offered` are compared: one the editor does not
/// offer is never granted or revoked.
pub fn diff(offered: &[Privilege], held: &PrivSet, wanted: &PrivSet) -> GrantDelta {
    let mut delta = GrantDelta::default();
    for privilege in offered {
        match (held.get(privilege).copied(), wanted.get(privilege).copied()) {
            (None, None) => {}
            (None, Some(false)) => delta.grant.push(*privilege),
            (None, Some(true)) | (Some(false), Some(true)) => delta.grant_with_option.push(*privilege),
            (Some(true), Some(false)) => delta.revoke_option.push(*privilege),
            (Some(_), Some(_)) => {}
            (Some(_), None) => delta.revoke.push(*privilege),
        }
    }
    delta
}
