//! Structure changes as data, and the DDL they turn into (no GPUI).
//!
//! A [`Change`] is one edit of a database's structure (`CreateTable`,
//! `AddColumn`, `AlterColumn`, ...). A [`draft::Draft`] collects them while the
//! user edits a diagram; [`plan`] turns a change-set into ordered statements for
//! one SQL dialect through the [`DdlDialect`] trait (PostgreSQL, MySQL, MariaDB,
//! SQLite, ClickHouse), or into a typed [`DdlError`] naming what that dialect
//! cannot do. [`apply::apply`] runs the statements, in a transaction where the
//! dialect has transactional DDL, and reports exactly what ran.
//!
//! Adding a database kind is one more `DdlDialect` impl and one arm in
//! [`dialect_for`]; nothing else here branches on the product.
//!
//! Every identifier is quoted with the dialect's own quoting
//! ([`SqlDialect::quote`], the helper `join_sql` uses) and every string literal
//! with [`sql_string_literal`]. The two free-text fields, a column type and a
//! default expression, are spliced into the statement as typed, after the same
//! check the table tab applies to its WHERE fragment (no `;`, no comments,
//! balanced quotes and parentheses).
//!
//! Statement order ([`Phase`]): renames of tables first (so every other change
//! uses the table's new name), then the drops (foreign keys, indexes, columns,
//! tables), then creations (tables, renamed / added / altered columns, primary
//! keys, indexes, foreign keys). Drop foreign key therefore always runs before
//! drop column, and create table before any foreign key that points at it.

use serde::{Deserialize, Serialize};

use crate::connections::ConnectionKind;
use crate::sql_account::{AccountChange, AccountEngine, AccountPhase, Reveal};
use crate::sql_filter::sql_string_literal;
use crate::sql_table::SqlDialect;

pub mod apply;
mod classify;
pub use classify::Flavor;
mod clickhouse;
pub mod draft;
mod mysql;
mod objects;
pub use objects::checked_name;
mod postgres;
pub mod schema;
mod sqlite;
mod text;
#[cfg(test)]
pub(crate) mod live;
#[cfg(test)]
mod tests;

/// A table: `schema` is the namespace the dialect qualifies with (PostgreSQL
/// schema, SQLite attached database, the database of MySQL / MariaDB /
/// ClickHouse), `None` for the connection's default.
#[derive(Debug, Clone, Default, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(default)]
pub struct TableId {
    pub schema: Option<String>,
    pub name: String,
}

#[cfg(test)]
impl TableId {
    pub fn new(schema: Option<&str>, name: &str) -> Self {
        Self { schema: schema.map(str::to_owned), name: name.to_owned() }
    }
}

impl std::fmt::Display for TableId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self.schema.as_deref().filter(|schema| !schema.is_empty()) {
            Some(schema) => write!(f, "{schema}.{}", self.name),
            None => f.write_str(&self.name),
        }
    }
}

/// One column as a statement needs it.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct ColumnSpec {
    pub name: String,
    /// The type as the dialect writes it (`varchar(255)`, `timestamp with time zone`).
    pub data_type: String,
    pub nullable: bool,
    /// The default as typed in the editor (see [`default_sql`]); `None`: no default.
    pub default: Option<String>,
    pub comment: Option<String>,
    /// Keeps `AUTO_INCREMENT` when MySQL restates the column.
    pub auto_increment: bool,
    /// A generated column (MySQL / MariaDB: restated with its expression;
    /// ClickHouse `MATERIALIZED` / `ALIAS`: its type and default are not editable).
    pub generated: bool,
    /// MySQL / MariaDB: everything a `MODIFY COLUMN` has to restate besides
    /// the fields above. `None` from a driver that does not report it: the
    /// column is then refused rather than restated from a partial picture.
    pub attributes: Option<ColumnAttributes>,
}

/// See `engine_driver_api::sql::ColumnAttributes`.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct ColumnAttributes {
    pub character_set: Option<String>,
    pub collation: Option<String>,
    /// The raw `EXTRA` text of `information_schema.COLUMNS`.
    pub extra: String,
    pub generation_expression: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum FkAction {
    NoAction,
    Restrict,
    Cascade,
    SetNull,
    SetDefault,
}

impl FkAction {
    pub const ALL: [Self; 5] = [Self::NoAction, Self::Restrict, Self::Cascade, Self::SetNull, Self::SetDefault];

    pub fn sql(self) -> &'static str {
        match self {
            Self::NoAction => "NO ACTION",
            Self::Restrict => "RESTRICT",
            Self::Cascade => "CASCADE",
            Self::SetNull => "SET NULL",
            Self::SetDefault => "SET DEFAULT",
        }
    }
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct ForeignKeySpec {
    pub name: String,
    pub columns: Vec<String>,
    pub ref_table: TableId,
    pub ref_columns: Vec<String>,
    pub on_delete: Option<FkAction>,
    pub on_update: Option<FkAction>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct IndexSpec {
    pub name: String,
    pub columns: Vec<String>,
    pub unique: bool,
}

/// The primary key a table has now (dropped when it is replaced).
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct PrimaryKeyRef {
    /// The constraint's name when the catalog reported one (PostgreSQL needs it to drop it).
    pub name: Option<String>,
    pub columns: Vec<String>,
}

/// One edit of the structure. A change names its table by the name the table
/// has *after* every rename of the change-set (see [`Phase`]).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "op", rename_all = "snake_case")]
pub enum Change {
    CreateTable {
        table: TableId,
        columns: Vec<ColumnSpec>,
        primary_key: Vec<String>,
        /// The table's own comment (`COMMENT ON TABLE`); none where the dialect has no such thing.
        #[serde(default)]
        comment: Option<String>,
    },
    /// `cascade`: also drop what depends on the table (PostgreSQL), never set by the diagram.
    DropTable {
        table: TableId,
        #[serde(default)]
        cascade: bool,
    },
    /// `table` is the name before the rename.
    RenameTable { table: TableId, to: String },
    /// A view or materialized view (the navigator's object actions; the diagram has none).
    DropView {
        table: TableId,
        materialized: bool,
        #[serde(default)]
        cascade: bool,
    },
    /// `table` is the name before the rename.
    RenameView { table: TableId, to: String, materialized: bool },
    /// Removes every row. `restart_identity` / `cascade` are PostgreSQL's options of the same name.
    TruncateTable { table: TableId, restart_identity: bool, cascade: bool },
    /// A schema (PostgreSQL) or a database (MySQL, MariaDB, ClickHouse); it belongs to no table.
    CreateNamespace { kind: NamespaceKind, name: String },
    AddColumn { table: TableId, column: ColumnSpec },
    DropColumn { table: TableId, column: String },
    RenameColumn { table: TableId, from: String, to: String },
    /// Both specs carry the column's current name; a rename is a separate change.
    AlterColumn { table: TableId, from: ColumnSpec, to: ColumnSpec },
    /// Replaces the primary key (`columns` empty: drops it).
    SetPrimaryKey { table: TableId, columns: Vec<String>, previous: Option<PrimaryKeyRef> },
    CreateIndex { table: TableId, index: IndexSpec },
    DropIndex { table: TableId, name: String },
    AddForeignKey { table: TableId, foreign_key: ForeignKeySpec },
    DropForeignKey { table: TableId, name: String },
    /// A role, user or privilege of the server (PostgreSQL, MySQL, MariaDB): see [`crate::sql_account`]. It belongs to no table.
    Account(AccountChange),
}

/// What a [`Change::CreateNamespace`] creates.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum NamespaceKind {
    Schema,
    Database,
}

impl NamespaceKind {
    pub fn noun(self) -> &'static str {
        match self {
            Self::Schema => "schema",
            Self::Database => "database",
        }
    }
}

/// Where a change runs in a change-set (stable inside a phase).
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum Phase {
    CreateNamespace,
    RenameTable,
    TruncateTable,
    DropForeignKey,
    DropIndex,
    DropColumn,
    DropTable,
    CreateTable,
    RenameColumn,
    AddColumn,
    AlterColumn,
    SetPrimaryKey,
    CreateIndex,
    AddForeignKey,
    /// The changes of users and privileges run after every structure change, in [`AccountPhase`] order.
    Account(AccountPhase),
}

impl Change {
    pub fn phase(&self) -> Phase {
        match self {
            Self::CreateNamespace { .. } => Phase::CreateNamespace,
            Self::RenameTable { .. } | Self::RenameView { .. } => Phase::RenameTable,
            Self::TruncateTable { .. } => Phase::TruncateTable,
            Self::DropForeignKey { .. } => Phase::DropForeignKey,
            Self::DropIndex { .. } => Phase::DropIndex,
            Self::DropColumn { .. } => Phase::DropColumn,
            Self::DropTable { .. } | Self::DropView { .. } => Phase::DropTable,
            Self::CreateTable { .. } => Phase::CreateTable,
            Self::RenameColumn { .. } => Phase::RenameColumn,
            Self::AddColumn { .. } => Phase::AddColumn,
            Self::AlterColumn { .. } => Phase::AlterColumn,
            Self::SetPrimaryKey { .. } => Phase::SetPrimaryKey,
            Self::CreateIndex { .. } => Phase::CreateIndex,
            Self::AddForeignKey { .. } => Phase::AddForeignKey,
            Self::Account(account) => Phase::Account(account.phase()),
        }
    }

    /// The table (or view) the change is about; `None` for a namespace.
    pub fn table(&self) -> Option<&TableId> {
        match self {
            Self::CreateTable { table, .. }
            | Self::DropTable { table, .. }
            | Self::RenameTable { table, .. }
            | Self::DropView { table, .. }
            | Self::RenameView { table, .. }
            | Self::TruncateTable { table, .. }
            | Self::AddColumn { table, .. }
            | Self::DropColumn { table, .. }
            | Self::RenameColumn { table, .. }
            | Self::AlterColumn { table, .. }
            | Self::SetPrimaryKey { table, .. }
            | Self::CreateIndex { table, .. }
            | Self::DropIndex { table, .. }
            | Self::AddForeignKey { table, .. }
            | Self::DropForeignKey { table, .. } => Some(table),
            Self::CreateNamespace { .. } | Self::Account(_) => None,
        }
    }

    pub(crate) fn table_mut(&mut self) -> Option<&mut TableId> {
        match self {
            Self::CreateTable { table, .. }
            | Self::DropTable { table, .. }
            | Self::RenameTable { table, .. }
            | Self::DropView { table, .. }
            | Self::RenameView { table, .. }
            | Self::TruncateTable { table, .. }
            | Self::AddColumn { table, .. }
            | Self::DropColumn { table, .. }
            | Self::RenameColumn { table, .. }
            | Self::AlterColumn { table, .. }
            | Self::SetPrimaryKey { table, .. }
            | Self::CreateIndex { table, .. }
            | Self::DropIndex { table, .. }
            | Self::AddForeignKey { table, .. }
            | Self::DropForeignKey { table, .. } => Some(table),
            Self::CreateNamespace { .. } | Self::Account(_) => None,
        }
    }

    /// The change creates, drops or renames a table as a whole (or truncates it), whatever its columns are.
    pub(crate) fn is_whole_table(&self) -> bool {
        matches!(
            self,
            Self::CreateTable { .. }
                | Self::DropTable { .. }
                | Self::RenameTable { .. }
                | Self::DropView { .. }
                | Self::RenameView { .. }
                | Self::TruncateTable { .. }
        )
    }

    /// One line for a list of changes ("Add column orders.note").
    pub fn title(&self) -> String {
        match self {
            Self::CreateTable { table, .. } => format!("Create table {table}"),
            Self::DropTable { table, .. } => format!("Drop table {table}"),
            Self::RenameTable { table, to } => format!("Rename table {table} to {to}"),
            Self::DropView { table, materialized, .. } => format!("Drop {} {table}", view_noun(*materialized)),
            Self::RenameView { table, to, materialized } => format!("Rename {} {table} to {to}", view_noun(*materialized)),
            Self::TruncateTable { table, .. } => format!("Truncate table {table}"),
            Self::CreateNamespace { kind, name } => format!("Create {} {name}", kind.noun()),
            Self::AddColumn { table, column } => format!("Add column {table}.{}", column.name),
            Self::DropColumn { table, column } => format!("Drop column {table}.{column}"),
            Self::RenameColumn { table, from, to } => format!("Rename column {table}.{from} to {to}"),
            Self::AlterColumn { table, from, .. } => format!("Alter column {table}.{}", from.name),
            Self::SetPrimaryKey { table, columns, .. } if columns.is_empty() => {
                format!("Drop the primary key of {table}")
            }
            Self::SetPrimaryKey { table, columns, .. } => {
                format!("Set the primary key of {table} to ({})", columns.join(", "))
            }
            Self::CreateIndex { table, index } => format!("Create index {} on {table}", index.name),
            Self::DropIndex { table, name } => format!("Drop index {name} on {table}"),
            Self::AddForeignKey { table, foreign_key } => {
                format!("Add foreign key {} on {table}", foreign_key.name)
            }
            Self::DropForeignKey { table, name } => format!("Drop foreign key {name} on {table}"),
            Self::Account(account) => account.title(),
        }
    }
}

fn view_noun(materialized: bool) -> &'static str {
    if materialized {
        "materialized view"
    } else {
        "view"
    }
}

/// Why a change could not become SQL.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DdlError {
    /// The dialect cannot do it; the message says what to do instead.
    Unsupported(String),
    /// The change is not well formed (a blank name, a type with `;` in it).
    Invalid(String),
    /// `CREATE DATABASE` cannot run in a transaction block, so a change-set with one would lose the
    /// transaction (and the session setup inside it) for every other change: it goes alone.
    CreateDatabaseNotAlone,
}

impl std::fmt::Display for DdlError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Unsupported(message) | Self::Invalid(message) => f.write_str(message),
            Self::CreateDatabaseNotAlone => f.write_str(
                "CREATE DATABASE cannot run inside a transaction, so it cannot be applied together with other changes: apply it on its own, then the rest.",
            ),
        }
    }
}

impl std::error::Error for DdlError {}

/// How a server compares the names of a table's columns, indexes and foreign keys when it decides
/// that two are the same (checked live: MySQL and MariaDB refuse `fk1` next to `Fk1`, an index
/// `ix` next to `Ix` and a column `Note` next to `note`; PostgreSQL, which quotes every name,
/// keeps them apart). Case is folded for ASCII letters only, as the servers' collations do for the
/// names the checks compare.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct NameRules {
    pub ignore_case: bool,
}

impl NameRules {
    pub const EXACT: Self = Self { ignore_case: false };
    pub const IGNORE_CASE: Self = Self { ignore_case: true };

    /// Whether `a` and `b` name the same column, index or foreign key.
    pub fn same(self, a: &str, b: &str) -> bool {
        if self.ignore_case {
            a.eq_ignore_ascii_case(b)
        } else {
            a == b
        }
    }
}

/// What a dialect lets the editor offer (the inspector disables the rest).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DdlCapabilities {
    pub indexes: bool,
    pub foreign_keys: bool,
    pub primary_key_edit: bool,
    pub column_comments: bool,
    pub alter_column: bool,
}

/// What a dialect lets the navigator's object actions (drop, rename, truncate, create namespace) do.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ObjectCapabilities {
    /// `DROP ... CASCADE` (PostgreSQL). Elsewhere it is ignored or a syntax error, so it is refused.
    pub drop_cascade: bool,
    /// `TRUNCATE TABLE`; without it (SQLite) the rows go with `DELETE FROM`.
    pub truncate_statement: bool,
    /// `RESTART IDENTITY` / `CASCADE` on a truncate (PostgreSQL).
    pub truncate_options: bool,
    /// A view can be renamed.
    pub rename_view: bool,
    pub materialized_views: bool,
    /// The kinds of namespace a connection of this dialect can create (`CREATE SCHEMA` /
    /// `CREATE DATABASE`); none for SQLite.
    pub create_namespace: &'static [NamespaceKind],
}

/// One SQL dialect's DDL. `statements` is the only generator: everything a
/// dialect cannot do is a [`DdlError::Unsupported`] naming the limitation.
pub trait DdlDialect: Send + Sync {
    /// Short name used in messages ("PostgreSQL").
    fn label(&self) -> &'static str;
    fn quoting(&self) -> SqlDialect;
    /// A name as an identifier, quoted the way this server reads it. Only a server that also takes
    /// a backslash inside a quoted name as an escape (ClickHouse) says more than [`SqlDialect::quote`].
    fn quote(&self, name: &str) -> String {
        self.quoting().quote(name)
    }
    /// DDL can be rolled back (PostgreSQL, SQLite).
    fn transactional(&self) -> bool;
    /// The table's namespace is the database (MySQL, MariaDB, ClickHouse).
    fn schema_is_database(&self) -> bool;
    /// Longest identifier the server accepts, measured by [`Self::identifier_length`].
    fn max_identifier_length(&self) -> Option<usize>;
    /// How the server measures an identifier (characters; PostgreSQL: bytes).
    fn identifier_length(&self, name: &str) -> usize {
        name.chars().count()
    }
    fn length_unit(&self) -> &'static str {
        "characters"
    }
    fn capabilities(&self) -> DdlCapabilities;
    fn object_capabilities(&self) -> ObjectCapabilities;
    /// How the server compares column, index and foreign key names (exact unless a dialect says so).
    fn name_rules(&self) -> NameRules {
        NameRules::EXACT
    }
    /// Whose reading of a type name the classifier applies.
    fn flavor(&self) -> Flavor;
    /// Suggestions for the free-text type field.
    fn common_types(&self) -> &'static [&'static str];
    /// Foreign keys of a table created in the same change-set go inside its
    /// `CREATE TABLE` (SQLite cannot add one afterwards).
    fn inline_foreign_keys(&self) -> bool {
        false
    }
    /// SQL for one change. `inline_foreign_keys` is the foreign keys of the
    /// change-set that belong to a `CreateTable` (empty for any other change
    /// and when [`Self::inline_foreign_keys`] is false).
    fn statements(&self, change: &Change, inline_foreign_keys: &[ForeignKeySpec]) -> Result<Vec<String>, DdlError>;
    /// SQL for a change of users and privileges. [`Reveal::Redacted`] is what [`Self::statements`] gives (the review,
    /// Copy SQL, the logs); [`Reveal::Exec`] has the real password in it and is for the driver only.
    fn account_statements(&self, _change: &AccountChange, _reveal: Reveal) -> Result<Vec<String>, DdlError> {
        Err(DdlError::Unsupported(format!("{} has no users and privileges to edit here.", self.label())))
    }
    /// The server's users and privileges, when the editor handles them (PostgreSQL, MySQL, MariaDB).
    fn account_engine(&self) -> Option<AccountEngine> {
        None
    }
    /// What the session that applies a change-set is set up with so it neither
    /// waits for a lock for ever nor is cut short by a statement timeout.
    fn session_setup(&self, _lock_timeout_secs: u32) -> SessionSetup {
        SessionSetup::default()
    }
    /// Statements run inside the apply's transaction, right after its session
    /// setup, that stop other sessions from changing `tables` while the batch
    /// re-reads and checks them (PostgreSQL: `SHARE UPDATE EXCLUSIVE` conflicts
    /// with every DDL lock and with itself, and not with reads or writes).
    fn check_locks(&self, _tables: &[TableId]) -> Result<Vec<String>, DdlError> {
        Ok(Vec::new())
    }
    /// Making a nullable column NOT NULL rewrites its NULLs (to the type's
    /// implicit default) instead of failing: MySQL, MariaDB, ClickHouse.
    fn null_to_not_null_rewrites(&self) -> bool {
        false
    }
}

/// Statements run on the apply session before the batch.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct SessionSetup {
    /// Before `BEGIN` (session settings).
    pub before: Vec<String>,
    /// Right after `BEGIN` (`SET LOCAL`: they end with the transaction).
    pub in_transaction: Vec<String>,
}

/// The dialect of a connection kind; `None` for kinds without SQL DDL.
pub fn dialect_for(kind: ConnectionKind) -> Option<&'static dyn DdlDialect> {
    match kind {
        ConnectionKind::Postgres => Some(&postgres::Postgres),
        ConnectionKind::Mysql => Some(&mysql::MySql { mariadb: false, no_backslash_escapes: false, ansi_quotes: false, bracket_identifiers: false }),
        ConnectionKind::Mariadb => Some(&mysql::MySql { mariadb: true, no_backslash_escapes: false, ansi_quotes: false, bracket_identifiers: false }),
        ConnectionKind::Sqlite => Some(&sqlite::Sqlite),
        ConnectionKind::Clickhouse => Some(&clickhouse::ClickHouse),
        ConnectionKind::Mongodb | ConnectionKind::Redis => None,
    }
}

/// The dialect of a server whose `sql_mode` is known: with
/// `NO_BACKSLASH_ESCAPES` a backslash in a MySQL / MariaDB string is an
/// ordinary character, with `ANSI_QUOTES` (`ANSI`) `"…"` is a name, and with
/// MariaDB's `MSSQL` so is `[…]`; each changes how typed text is lexed and quoted.
/// The mode string is the one source: Apply reads it from the server it runs on.
pub fn dialect_in_mode(kind: ConnectionKind, sql_mode: &str) -> Option<Box<dyn DdlDialect>> {
    let has = |wanted: &str| sql_mode.split(',').any(|mode| mode.trim().eq_ignore_ascii_case(wanted));
    let no_backslash_escapes = has("NO_BACKSLASH_ESCAPES");
    let bracket_identifiers = has("MSSQL");
    let ansi_quotes = has("ANSI_QUOTES") || has("ANSI") || bracket_identifiers;
    match kind {
        ConnectionKind::Mysql => Some(Box::new(mysql::MySql { mariadb: false, no_backslash_escapes, ansi_quotes, bracket_identifiers: false })),
        ConnectionKind::Mariadb => Some(Box::new(mysql::MySql { mariadb: true, no_backslash_escapes, ansi_quotes, bracket_identifiers })),
        other => dialect_for(other).map(|_| -> Box<dyn DdlDialect> { Box::new(Delegate(other)) }),
    }
}

/// A kind whose dialect does not depend on `sql_mode`.
struct Delegate(ConnectionKind);

impl Delegate {
    fn inner(&self) -> &'static dyn DdlDialect {
        dialect_for(self.0).expect("checked when created")
    }
}

impl DdlDialect for Delegate {
    fn label(&self) -> &'static str {
        self.inner().label()
    }
    fn quoting(&self) -> SqlDialect {
        self.inner().quoting()
    }
    fn quote(&self, name: &str) -> String {
        self.inner().quote(name)
    }
    fn transactional(&self) -> bool {
        self.inner().transactional()
    }
    fn schema_is_database(&self) -> bool {
        self.inner().schema_is_database()
    }
    fn max_identifier_length(&self) -> Option<usize> {
        self.inner().max_identifier_length()
    }
    fn identifier_length(&self, name: &str) -> usize {
        self.inner().identifier_length(name)
    }
    fn length_unit(&self) -> &'static str {
        self.inner().length_unit()
    }
    fn capabilities(&self) -> DdlCapabilities {
        self.inner().capabilities()
    }
    fn object_capabilities(&self) -> ObjectCapabilities {
        self.inner().object_capabilities()
    }
    fn name_rules(&self) -> NameRules {
        self.inner().name_rules()
    }
    fn flavor(&self) -> Flavor {
        self.inner().flavor()
    }
    fn common_types(&self) -> &'static [&'static str] {
        self.inner().common_types()
    }
    fn inline_foreign_keys(&self) -> bool {
        self.inner().inline_foreign_keys()
    }
    fn statements(&self, change: &Change, inline: &[ForeignKeySpec]) -> Result<Vec<String>, DdlError> {
        self.inner().statements(change, inline)
    }
    fn account_statements(&self, change: &AccountChange, reveal: Reveal) -> Result<Vec<String>, DdlError> {
        self.inner().account_statements(change, reveal)
    }
    fn account_engine(&self) -> Option<AccountEngine> {
        self.inner().account_engine()
    }
    fn session_setup(&self, lock_timeout_secs: u32) -> SessionSetup {
        self.inner().session_setup(lock_timeout_secs)
    }
    fn null_to_not_null_rewrites(&self) -> bool {
        self.inner().null_to_not_null_rewrites()
    }
    fn check_locks(&self, tables: &[TableId]) -> Result<Vec<String>, DdlError> {
        self.inner().check_locks(tables)
    }
}

// ---- shared generation helpers ------------------------------------------------------------

/// A quoted identifier, after the checks every dialect shares.
pub(crate) fn ident(d: &dyn DdlDialect, what: &str, name: &str) -> Result<String, DdlError> {
    if name.trim().is_empty() {
        return Err(DdlError::Invalid(format!("The {what} name is empty.")));
    }
    if name.contains('\0') {
        return Err(DdlError::Invalid(format!("The {what} name \"{}\" contains a NUL character.", name.escape_debug())));
    }
    if let Some(limit) = d.max_identifier_length() {
        if d.identifier_length(name) > limit {
            return Err(DdlError::Invalid(format!(
                "The {what} name \"{name}\" is longer than the {limit} {} {} allows.",
                d.length_unit(),
                d.label()
            )));
        }
    }
    Ok(d.quote(name))
}

pub(crate) fn table_sql(d: &dyn DdlDialect, table: &TableId) -> Result<String, DdlError> {
    ident(d, "table", &table.name)?;
    if let Some(schema) = table.schema.as_deref().filter(|schema| !schema.is_empty()) {
        ident(d, "schema", schema)?;
    }
    Ok(match table.schema.as_deref().filter(|schema| !schema.is_empty()) {
        Some(schema) => format!("{}.{}", d.quote(schema), d.quote(&table.name)),
        None => d.quote(&table.name),
    })
}

pub(crate) fn column_list(d: &dyn DdlDialect, what: &str, columns: &[String]) -> Result<String, DdlError> {
    if columns.is_empty() {
        return Err(DdlError::Invalid(format!("At least one column is needed for the {what}.")));
    }
    let quoted: Result<Vec<String>, DdlError> = columns.iter().map(|column| ident(d, "column", column)).collect();
    Ok(quoted?.join(", "))
}

/// The type text, checked: non-empty and a single expression.
pub(crate) fn type_sql(d: &dyn DdlDialect, data_type: &str) -> Result<String, DdlError> {
    let text = data_type.trim();
    if text.is_empty() {
        return Err(DdlError::Invalid("A column needs a type.".into()));
    }
    text::check_type(d.quoting(), text).map_err(DdlError::Invalid)?;
    Ok(text.to_owned())
}

/// The type operand of an alteration: text equal to the column's current
/// (catalog) type is SQL the database printed and is used as it is; only text
/// the user typed is checked.
pub(crate) fn type_operand(d: &dyn DdlDialect, from: &str, to: &str) -> Result<String, DdlError> {
    if from.trim() == to.trim() && !to.trim().is_empty() {
        return Ok(to.trim().to_owned());
    }
    type_sql(d, to)
}

/// The `DEFAULT` operand of an alteration. A default equal to the catalog's is
/// raw SQL the database printed (`NULL::character varying`, `CURRENT_USER`,
/// `nextval('s'::regclass)`) and is never classified; only text the user typed
/// goes through [`default_sql`]. `None`: no default.
pub(crate) fn default_operand(d: &dyn DdlDialect, from: Option<&str>, to: Option<&str>) -> Result<Option<String>, DdlError> {
    let clean = |text: Option<&str>| text.map(str::trim).filter(|text| !text.is_empty()).map(str::to_owned);
    let (from, to) = (clean(from), clean(to));
    match to {
        None => Ok(None),
        Some(text) if from.as_deref() == Some(text.as_str()) => Ok(Some(text)),
        Some(text) => default_sql(d, &text).map(Some),
    }
}

/// `DEFAULT` operand for the text typed in the editor. Numbers, `NULL`,
/// `TRUE` / `FALSE`, the `CURRENT_*` keywords, quoted text and anything with a
/// parenthesis (a function call or an expression) are SQL as typed; any other
/// text is a string literal. The catalog's own expressions (`'x'::text`,
/// `nextval('s'::regclass)`, `now()`) therefore round-trip unchanged.
pub fn default_sql(d: &dyn DdlDialect, text: &str) -> Result<String, DdlError> {
    let text = text.trim();
    if text.is_empty() {
        return Err(DdlError::Invalid("The default is empty.".into()));
    }
    const KEYWORDS: [&str; 8] =
        ["NULL", "TRUE", "FALSE", "CURRENT_TIMESTAMP", "CURRENT_DATE", "CURRENT_TIME", "LOCALTIME", "LOCALTIMESTAMP"];
    let raw = is_number(text)
        || KEYWORDS.iter().any(|keyword| text.eq_ignore_ascii_case(keyword))
        || text.starts_with(['\'', '('])
        || text.contains('(');
    if raw {
        text::check_default(d.quoting(), text).map_err(DdlError::Invalid)?;
        Ok(text.to_owned())
    } else {
        Ok(sql_string_literal(d.quoting(), text))
    }
}

fn is_number(text: &str) -> bool {
    let body = text.strip_prefix(['+', '-']).unwrap_or(text);
    let (mantissa, exponent) = match body.split_once(['e', 'E']) {
        Some((mantissa, exponent)) => (mantissa, Some(exponent)),
        None => (body, None),
    };
    let digits = |s: &str| !s.is_empty() && s.bytes().all(|b| b.is_ascii_digit());
    let mantissa_ok = match mantissa.split_once('.') {
        Some((whole, fraction)) => {
            (whole.is_empty() || digits(whole)) && (fraction.is_empty() || digits(fraction)) && !(whole.is_empty() && fraction.is_empty())
        }
        None => digits(mantissa),
    };
    mantissa_ok && exponent.is_none_or(|e| digits(e.strip_prefix(['+', '-']).unwrap_or(e)))
}

pub(crate) fn literal(d: &dyn DdlDialect, text: &str) -> String {
    sql_string_literal(d.quoting(), text)
}

pub(crate) fn fk_clause(
    d: &dyn DdlDialect,
    foreign_key: &ForeignKeySpec,
    allow_set_default: bool,
) -> Result<String, DdlError> {
    if foreign_key.columns.len() != foreign_key.ref_columns.len() {
        return Err(DdlError::Invalid(format!(
            "Foreign key \"{}\" has {} column(s) but references {}.",
            foreign_key.name,
            foreign_key.columns.len(),
            foreign_key.ref_columns.len()
        )));
    }
    let mut sql = format!(
        "FOREIGN KEY ({}) REFERENCES {} ({})",
        column_list(d, "foreign key", &foreign_key.columns)?,
        table_sql(d, &foreign_key.ref_table)?,
        column_list(d, "referenced columns", &foreign_key.ref_columns)?,
    );
    for (keyword, action) in [("ON DELETE", foreign_key.on_delete), ("ON UPDATE", foreign_key.on_update)] {
        let Some(action) = action else { continue };
        if action == FkAction::SetDefault && !allow_set_default {
            return Err(DdlError::Unsupported(format!("{} does not support {keyword} SET DEFAULT.", d.label())));
        }
        sql.push_str(&format!(" {keyword} {}", action.sql()));
    }
    Ok(sql)
}

/// `CONSTRAINT "name" FOREIGN KEY ...` (named, the form `ALTER TABLE ADD` and
/// `CREATE TABLE` share).
pub(crate) fn named_fk(d: &dyn DdlDialect, foreign_key: &ForeignKeySpec, allow_set_default: bool) -> Result<String, DdlError> {
    Ok(format!(
        "CONSTRAINT {} {}",
        ident(d, "foreign key", &foreign_key.name)?,
        fk_clause(d, foreign_key, allow_set_default)?
    ))
}

pub(crate) fn index_columns(d: &dyn DdlDialect, index: &IndexSpec) -> Result<String, DdlError> {
    column_list(d, "index", &index.columns)
}

// ---- planning ------------------------------------------------------------------------------

/// One statement to run and the change it belongs to.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Step {
    /// Index into the change-set.
    pub change: usize,
    pub sql: String,
}

/// What one change became.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Entry {
    /// Index into the change-set.
    pub change: usize,
    pub statements: Result<Vec<String>, DdlError>,
    /// The change is part of another change's statement (a foreign key inside
    /// the `CREATE TABLE` of a table of the same change-set); the index of that change.
    pub folded_into: Option<usize>,
}

/// A change-set in execution order.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Planned {
    pub entries: Vec<Entry>,
    pub transactional: bool,
    /// Run on the apply session around the batch.
    pub setup: SessionSetup,
}

impl Planned {
    /// Every change became SQL.
    pub fn executable(&self) -> bool {
        self.entries.iter().all(|entry| entry.statements.is_ok())
    }

    pub fn first_error(&self) -> Option<(usize, &DdlError)> {
        self.entries.iter().find_map(|entry| entry.statements.as_ref().err().map(|error| (entry.change, error)))
    }

    /// The statements in execution order (changes that failed to generate contribute none).
    pub fn steps(&self) -> Vec<Step> {
        self.entries
            .iter()
            .flat_map(|entry| {
                entry
                    .statements
                    .iter()
                    .flatten()
                    .map(|sql| Step { change: entry.change, sql: sql.clone() })
            })
            .collect()
    }

    /// The change the statement number `step` of [`Self::steps`] belongs to (without copying any statement).
    pub fn change_of_step(&self, step: usize) -> Option<usize> {
        self.entries.iter().flat_map(|entry| entry.statements.iter().flatten().map(move |_| entry.change)).nth(step)
    }

    /// The entry of change `change`.
    #[cfg(test)]
    pub fn entry(&self, change: usize) -> Option<&Entry> {
        self.entries.iter().find(|entry| entry.change == change)
    }

    /// The statements as a script (one `;` after each), for Copy SQL.
    pub fn script(&self) -> String {
        self.steps().iter().map(|step| format!("{};", step.sql)).collect::<Vec<_>>().join("\n")
    }
}

/// Orders `changes` and generates their statements for `dialect`. A change that takes a column,
/// index or foreign key name another change of the set already took is refused
/// ([`DdlError::Invalid`]).
#[cfg(test)]
pub fn plan(dialect: &dyn DdlDialect, changes: &[Change]) -> Planned {
    plan_with_lock_timeout(dialect, changes, DEFAULT_LOCK_TIMEOUT_SECS)
}

/// [`plan`] for tables whose catalog is known: a change that takes a name the table already has
/// is refused as well.
pub fn plan_against(dialect: &dyn DdlDialect, changes: &[Change], live: &schema::LiveSchema) -> Planned {
    plan_against_with(dialect, changes, &schema::name_clashes(changes, live, dialect.name_rules()))
}

/// [`plan_against`] for a caller that has the catalog's [`schema::name_clashes`] already.
pub fn plan_against_with(dialect: &dyn DdlDialect, changes: &[Change], clashes: &[(usize, String)]) -> Planned {
    #[cfg(test)]
    crate::perf_counters::bump(&crate::perf_counters::PLAN_AGAINST);
    plan_clashing(dialect, changes, clashes, DEFAULT_LOCK_TIMEOUT_SECS)
}

/// Seconds an apply waits for a lock before it fails (and rolls back, where it can).
pub const DEFAULT_LOCK_TIMEOUT_SECS: u32 = 10;

/// [`plan`] with another lock timeout for the apply session.
pub fn plan_with_lock_timeout(dialect: &dyn DdlDialect, changes: &[Change], lock_timeout_secs: u32) -> Planned {
    plan_in(dialect, changes, &schema::LiveSchema::new(false), lock_timeout_secs)
}

fn plan_in(dialect: &dyn DdlDialect, changes: &[Change], live: &schema::LiveSchema, lock_timeout_secs: u32) -> Planned {
    plan_clashing(dialect, changes, &schema::name_clashes(changes, live, dialect.name_rules()), lock_timeout_secs)
}

fn creates_database_in(change: &Change) -> bool {
    matches!(change, Change::CreateNamespace { kind: NamespaceKind::Database, .. })
}

fn plan_clashing(dialect: &dyn DdlDialect, changes: &[Change], clashes: &[(usize, String)], lock_timeout_secs: u32) -> Planned {
    let created: Vec<(usize, &TableId)> = changes
        .iter()
        .enumerate()
        .filter_map(|(index, change)| match change {
            Change::CreateTable { table, .. } => Some((index, table)),
            _ => None,
        })
        .collect();
    let host_of = |change: &Change| match change {
        Change::AddForeignKey { table, .. } if dialect.inline_foreign_keys() => {
            created.iter().find(|(_, created)| *created == table).map(|(index, _)| *index)
        }
        _ => None,
    };
    let mut order: Vec<usize> = (0..changes.len()).collect();
    order.sort_by_key(|&index| changes[index].phase());
    let entries = order
        .into_iter()
        .map(|index| {
            let change = &changes[index];
            if let Some(host) = host_of(change) {
                return Entry { change: index, statements: Ok(Vec::new()), folded_into: Some(host) };
            }
            let inline: Vec<ForeignKeySpec> = match change {
                Change::CreateTable { table, .. } if dialect.inline_foreign_keys() => changes
                    .iter()
                    .filter_map(|other| match other {
                        Change::AddForeignKey { table: owner, foreign_key } if owner == table => Some(foreign_key.clone()),
                        _ => None,
                    })
                    .collect(),
                _ => Vec::new(),
            };
            let statements = match clashes.iter().find(|(clash, _)| *clash == index) {
                Some((_, message)) => Err(DdlError::Invalid(message.clone())),
                None => dialect.statements(change, &inline).and_then(|statements| {
                    if creates_database_in(change) && changes.len() > 1 {
                        Err(DdlError::CreateDatabaseNotAlone)
                    } else {
                        Ok(statements)
                    }
                }),
            };
            Entry { change: index, statements, folded_into: None }
        })
        .collect();
    // `CREATE DATABASE` cannot run inside a transaction block (PostgreSQL), so a change-set that is
    // one runs statement by statement, like a dialect without transactional DDL. (A change-set that
    // has another change next to it is refused above: it would run without its transaction and
    // session setup.)
    let creates_database = changes.iter().any(creates_database_in);
    Planned { entries, transactional: dialect.transactional() && !creates_database, setup: dialect.session_setup(lock_timeout_secs) }
}

// ---- warnings ------------------------------------------------------------------------------

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum Severity {
    /// Worth reading (a rename breaks queries that use the old name).
    Caution,
    /// Data is lost or may be (drop column / table, narrowing a type); needs an explicit confirmation.
    Destructive,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Warning {
    /// Index into the change-set.
    pub change: usize,
    pub severity: Severity,
    pub message: String,
}

/// Warnings about what `changes` do to existing data. A type change is
/// destructive unless [`classify::widens`] proves it a pure widening.
pub fn warnings(dialect: &dyn DdlDialect, changes: &[Change]) -> Vec<Warning> {
    let mut found = Vec::new();
    let mut warn = |change: usize, severity: Severity, message: String| found.push(Warning { change, severity, message });
    for (index, change) in changes.iter().enumerate() {
        match change {
            Change::DropTable { table, cascade } => {
                warn(index, Severity::Destructive, format!("Drops table {table} and every row in it."));
                if *cascade {
                    warn(
                        index,
                        Severity::Destructive,
                        format!("CASCADE also drops everything that depends on {table}: views, foreign keys of other tables and the like."),
                    );
                }
            }
            Change::DropView { table, materialized, cascade } => {
                let what = view_noun(*materialized);
                warn(
                    index,
                    Severity::Destructive,
                    format!(
                        "Drops the {what} {table}{}; queries, views and code that use it break.",
                        if *materialized { " and the rows it stores" } else { "" }
                    ),
                );
                if *cascade {
                    warn(index, Severity::Destructive, format!("CASCADE also drops everything that depends on {table}."));
                }
            }
            Change::RenameView { table, to, materialized } => {
                warn(
                    index,
                    Severity::Caution,
                    format!("Queries, views and code that use the name {table} break; the {} is called {to} afterwards.", view_noun(*materialized)),
                );
            }
            Change::TruncateTable { table, restart_identity, cascade } => {
                warn(index, Severity::Destructive, format!("Deletes every row of {table}; this cannot be undone."));
                let caps = dialect.object_capabilities();
                if !caps.truncate_statement {
                    warn(
                        index,
                        Severity::Caution,
                        format!(
                            "{} has no TRUNCATE: the rows are deleted with DELETE FROM. That is slower on a big table, fires the DELETE triggers of {table}, and deletes the child rows of ON DELETE CASCADE foreign keys when foreign keys are enforced.",
                            dialect.label()
                        ),
                    );
                } else if !dialect.transactional() {
                    warn(index, Severity::Caution, format!("{} cannot roll TRUNCATE back.", dialect.label()));
                }
                if *restart_identity {
                    warn(index, Severity::Caution, format!("RESTART IDENTITY resets the sequences owned by the columns of {table}."));
                }
                if *cascade {
                    warn(
                        index,
                        Severity::Destructive,
                        format!("CASCADE also deletes every row of the tables that have a foreign key to {table}, and of the tables that reference those."),
                    );
                }
            }
            Change::DropColumn { table, column } => {
                warn(index, Severity::Destructive, format!("Drops column {column} of {table}; the data in it is lost."));
            }
            Change::AlterColumn { table, from, to } => {
                if from.data_type.trim() != to.data_type.trim() {
                    if classify::widens(dialect.flavor(), &from.data_type, &to.data_type) {
                        warn(
                            index,
                            Severity::Caution,
                            format!("Converts the values of {table}.{} from {} to {}.", from.name, from.data_type.trim(), to.data_type.trim()),
                        );
                    } else {
                        warn(
                            index,
                            Severity::Destructive,
                            format!(
                                "Changes {table}.{} from {} to {}, which is not a pure widening: values may be truncated, rounded, converted or rejected.",
                                from.name,
                                from.data_type.trim(),
                                to.data_type.trim()
                            ),
                        );
                    }
                }
                if from.nullable && !to.nullable {
                    if dialect.null_to_not_null_rewrites() {
                        warn(
                            index,
                            Severity::Destructive,
                            format!("{} turns the NULL values of {table}.{} into a default value instead of failing.", dialect.label(), from.name),
                        );
                    } else {
                        warn(index, Severity::Caution, format!("Fails if {table}.{} holds NULL values.", from.name));
                    }
                }
            }
            Change::AddColumn { table, column } if !column.nullable && column.default.as_deref().is_none_or(|d| d.trim().is_empty()) => {
                warn(index, Severity::Caution, format!("{table}.{} is NOT NULL without a default: this fails when {table} has rows.", column.name));
            }
            Change::RenameTable { table, to } => {
                warn(index, Severity::Caution, format!("Queries, views and code that use the name {table} break; the new name is {to}."));
            }
            Change::RenameColumn { table, from, to } => {
                warn(index, Severity::Caution, format!("Queries and code that use {table}.{from} break; the new name is {to}."));
            }
            Change::SetPrimaryKey { table, .. } => {
                warn(index, Severity::Caution, format!("Replaces the primary key of {table}; rows that repeat the new key make it fail."));
            }
            Change::DropIndex { table, name } => {
                warn(index, Severity::Caution, format!("Drops index {name} of {table}; queries that relied on it get slower."));
            }
            Change::DropForeignKey { table, name } => {
                warn(index, Severity::Caution, format!("Drops foreign key {name} of {table}; the database stops checking it."));
            }
            Change::Account(account) => {
                for (severity, message) in account.warnings(dialect.account_engine().is_some_and(AccountEngine::is_mysql)) {
                    warn(index, severity, message);
                }
            }
            _ => {}
        }
    }
    found
}

#[cfg(test)]
/// Whether a type change loses nothing for certain (see `classify`).
pub fn widens(flavor: Flavor, from: &str, to: &str) -> bool {
    classify::widens(flavor, from, to)
}
