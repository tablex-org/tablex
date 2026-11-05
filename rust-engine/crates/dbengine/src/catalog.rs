//! Database-neutral catalog types shared by the GPUI shell and engine drivers.

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CatalogObjectKind {
    Table,
    View,
    MaterializedView,
}

impl CatalogObjectKind {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Table => "table",
            Self::View => "view",
            Self::MaterializedView => "materialized_view",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CatalogObject {
    pub name: String,
    pub kind: CatalogObjectKind,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct CatalogColumn {
    pub name: String,
    pub data_type: String,
    pub nullable: bool,
    pub primary_key: bool,
    pub is_generated: Option<bool>,
    pub default_expression: Option<String>,
    pub auto_increment: Option<bool>,
    /// The column's comment (PostgreSQL, MySQL/MariaDB, ClickHouse); `None`
    /// when unset or when the engine has no column comments (SQLite).
    pub comment: Option<String>,
    /// MySQL / MariaDB: what restating the column needs (see the driver API's
    /// `ColumnAttributes`); `None` when the driver does not report it.
    pub attributes: Option<CatalogColumnAttributes>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct CatalogColumnAttributes {
    pub character_set: Option<String>,
    pub collation: Option<String>,
    /// The raw `EXTRA` text.
    pub extra: String,
    pub generation_expression: Option<String>,
}

/// Table/collection metadata for the Properties presentation. Every list is
/// optional per driver: an empty Vec means "none", `None` means "this driver
/// cannot report it" so the UI can hide the sub-tab instead of showing zero.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct TableProperties {
    pub columns: Vec<CatalogColumn>,
    /// Primary/unique/check/exclusion constraints.
    pub keys: Option<Vec<CatalogConstraint>>,
    pub foreign_keys: Option<Vec<CatalogForeignKey>>,
    pub indexes: Option<Vec<CatalogIndex>>,
    /// Foreign keys in OTHER tables that reference this table.
    pub references: Option<Vec<CatalogForeignKey>>,
    pub triggers: Option<Vec<CatalogTrigger>>,
    /// CREATE statement (table DDL, or view definition wrapped as CREATE VIEW).
    pub ddl: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CatalogConstraintKind {
    PrimaryKey,
    Unique,
    Check,
    Exclusion,
    Other,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CatalogConstraint {
    pub name: String,
    pub kind: CatalogConstraintKind,
    pub columns: Vec<String>,
    /// Driver-rendered definition, e.g. `PRIMARY KEY (id)` / `CHECK (qty > 0)`.
    pub definition: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CatalogForeignKey {
    pub name: String,
    /// Owning table (schema-qualified when the driver is schema-aware).
    pub table: String,
    pub columns: Vec<String>,
    pub referenced_table: String,
    pub referenced_columns: Vec<String>,
    pub on_update: Option<String>,
    pub on_delete: Option<String>,
    pub definition: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CatalogIndex {
    pub name: String,
    pub columns: Vec<String>,
    pub unique: bool,
    pub primary: bool,
    /// Access method / index type, e.g. `btree`, `gin`, `hashed`, `2dsphere`.
    pub method: Option<String>,
    pub definition: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CatalogTrigger {
    pub name: String,
    /// `BEFORE` / `AFTER` / `INSTEAD OF`.
    pub timing: Option<String>,
    /// `INSERT`, `UPDATE`, `DELETE`, `TRUNCATE`.
    pub events: Vec<String>,
    pub enabled: Option<bool>,
    pub definition: Option<String>,
}
