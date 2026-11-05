//! Wire contract shared by every SQL driver (PostgreSQL, MySQL/MariaDB,
//! SQLite, ...), so the host application handles all of them with one code
//! path instead of branching on the driver id.
//!
//! * Navigator: table-like nodes (`table`, `view`, `materialized_view`) carry a
//!   [`SqlObjectRef`] as their payload; `column` nodes carry a [`SqlColumnNode`].
//!   A payload is echoed back unchanged as the next `NavigatorRequest.payload`.
//! * Mutations: `MutationRequest.database`/`object` name the table,
//!   `identity` is a [`SqlTableIdentity`] and `payload` a [`SqlRowChange`]
//!   (kinds `insert`/`update`/`delete`) or a [`SqlBatch`] (kind `batch`,
//!   applied in one transaction, all-or-nothing).
//! * `describe_object` (capability `metadata.table_properties`): request is a
//!   [`SqlObjectRef`], response a [`TableProperties`]. A request that carries a
//!   `kind` is a [`SchemaObjectRef`] instead and the response a
//!   [`SchemaObjectSource`] (see below).
//! * Schema objects: a navigator request on a schema (a database for drivers
//!   without schemas) with `payload.kind` set to a [`SchemaObjectKind`] lists only
//!   the objects of that kind. `materialized_view` answers table-like nodes
//!   (a [`SqlObjectRef`] payload, as for `view`); every other kind answers nodes
//!   of that kind with a [`SchemaObjectRef`] payload, no children, ordered by
//!   name. A driver that has no such objects answers an empty page. `describe_object`
//!   with a [`SchemaObjectRef`] returns the object's source text as a
//!   [`SchemaObjectSource`] (`describe.not_found` when it no longer exists,
//!   `describe.bad_request` for a kind the driver has not).
//!
//! # Conformance rules (every SQL driver)
//!
//! Queries (`QueryRequest.kind == "sql"`, SQL in `payload.sql`):
//! * Only a single pure `SELECT`/`WITH … SELECT`/`VALUES`/`SHOW`/`EXPLAIN`
//!   (no data-modifying CTE) is pageable. Anything else must run once:
//!   `offset > 0` is refused with [`codes::QUERY_NOT_PAGEABLE`], and its page
//!   reports `truncated = false` — also when it returns more rows than
//!   `limit` (e.g. `INSERT … RETURNING`): the extra rows are dropped, never
//!   offered as a next page.
//! * A driver must never re-run a statement to serve a page: if a pageable
//!   read cannot be wrapped for paging, it runs once at `offset == 0` and is
//!   treated as non-pageable; `offset > 0` is refused, never re-executed.
//! * A multi-statement script runs every statement once, in order, and
//!   returns the rows of its LAST statement that produces a result set (or a
//!   zero-column page when none does).
//! * The classifier is a lexer, not a parser. Known blind spots (document,
//!   don't rely on): statements hidden in procedural code (`DO $$ … $$`,
//!   `PREPARE s FROM '…'`, function bodies), side-effecting functions called
//!   from a pageable `SELECT` (`nextval`, user functions — PG serves
//!   `offset > 0` pages READ ONLY where possible), session settings changed
//!   inside the same script (`standard_conforming_strings`, `sql_mode`).
//! * `payload.read_only == true` (capability `query.read_only`) runs the text in a READ
//!   ONLY scope (MySQL/MariaDB `START TRANSACTION READ ONLY`, PostgreSQL `BEGIN READ
//!   ONLY` or a read-only savepoint, SQLite `PRAGMA query_only`, ClickHouse
//!   `readonly = 2`); a data write fails with [`codes::QUERY_READ_ONLY_VIOLATION`].
//!   The scope guarantees data writes only: statements that need no transaction
//!   (MySQL `SET GLOBAL read_only = 0`, `KILL`, `FLUSH`, `SELECT ... INTO OUTFILE`;
//!   SQLite `VACUUM INTO`, `ATTACH`, `PRAGMA query_only = OFF`) and `CALL` (a
//!   procedure may `COMMIT`) are not stopped by it, so the driver refuses them. The
//!   scope must not be escapable by the text itself, so before anything runs the
//!   driver classifies the text with the lexer options of the SESSION (MySQL/MariaDB
//!   `sql_mode`: `ANSI_QUOTES`, `NO_BACKSLASH_ESCAPES`, `MSSQL`, and the server
//!   version, which decides whether a versioned comment `/*!50000 ... */` is code;
//!   PostgreSQL `standard_conforming_strings`, set `on` for the run) and refuses it
//!   with the same code, nothing run, when
//!   (1) any statement controls the transaction (`BEGIN`, `START`, `COMMIT`,
//!   `ROLLBACK [TO]`, `END`, `ABORT`, `SAVEPOINT`, `RELEASE`, `SET TRANSACTION`,
//!   `SET autocommit`, `XA`, `PREPARE TRANSACTION`, `LOCK`/`UNLOCK`), (2) there
//!   are several statements and any of them is not a plain read (several plain
//!   `SELECT`s are accepted), (3) a single statement is neither a plain read nor
//!   `EXPLAIN`/`DESCRIBE`/`ANALYZE` of one (`EXPLAIN ANALYZE <read>` is the shape
//!   the app's Explain sends; the scope refuses a write it would run), or (4) the
//!   text has a versioned comment that the server version cannot decide and whose
//!   code and comment readings differ. The rule is
//!   `engine_driver_kit::sql::Classification::read_only_refusal`. Not covered
//!   (blind spots): side effects hidden in a function the single statement calls
//!   (`nextval`, a user function: the scope stops its data writes, not the rest).
//!   The connect reply may carry `server_version` (MySQL numeric form: MySQL 8.4.6 =
//!   `80406`, MariaDB 11.8.9 = `110809`) for hosts that classify too, and a
//!   `sql_dialect` ([`SqlDialectInfo`](crate::SqlDialectInfo)) with the session's own
//!   `string_backslash_escapes`, `double_quote_identifiers` and `bracket_identifiers`
//!   (MySQL/MariaDB `sql_mode`), which a host lexes the text by.
//! * `payload.run_to_end == true` (capability `query.run_to_end`) runs the single statement exactly as
//!   written and to its end, for a host that runs a script and wants the statement's effects, not its rows:
//!   a read is not wrapped in a paging subquery, no `LIMIT` is appended, and every row the statement
//!   produces is read, so a function called per row runs for every row. At most `limit` rows are kept (the
//!   rest are read and discarded, never buffered) and the page reports `truncated = false`. `offset` must be 0.
//!   A driver that does not know the field pages the read as usual: check the capability first.
//! * `limit == 0` means the driver default page size (200); drivers cap pages
//!   at 100 000 rows and never buffer more than one page (+1 lookahead row).
//! * `truncated` is exact (lookahead row), `total_count` is `None` unless the
//!   driver knows the exact total (then `total_is_exact = true`).
//! * Dangerous statements (DROP, TRUNCATE, ALTER, DELETE/UPDATE without WHERE,
//!   GRANT/REVOKE, CREATE/DROP DATABASE) are refused with
//!   [`codes::QUERY_CONFIRMATION_REQUIRED`] (message = reason) unless
//!   `payload.confirmed == true`.
//! * A user-requested cancel fails with `<area>.cancelled`; an interrupt nobody
//!   requested fails with retryable `<area>.interrupted`.
//!
//! Navigator: `limit == 0` → 500, capped at 5 000; `payload.filter` is a
//! case-insensitive substring on object names; table-like nodes advertise the
//! `describe` action when `metadata.table_properties` is advertised.
//! Catalog calls (navigator, describe) must not wait behind a running user
//! query (use a separate metadata connection) and are bounded by a timeout.
//!
//! Mutations: unknown kind → [`codes::MUTATION_UNSUPPORTED`]; a `null` key value
//! matches SQL NULL (null-safe equality); any change affecting zero rows
//! (insert included) → [`codes::MUTATION_NO_ROWS_AFFECTED`]; more than one →
//! [`codes::MUTATION_IDENTITY_NOT_UNIQUE`]. If the session is inside a
//! user-opened transaction, the mutation joins it through a SAVEPOINT (released
//! on success, rolled back to on failure) and never commits or rolls back the
//! user's transaction. If a ROLLBACK fails, the connection is marked broken
//! and later calls fail with `<area>.not_connected`.
//! `create_database`: `payload` must be absent/empty, `affected_rows == 0`,
//! server refusal → [`codes::CREATE_DATABASE_REJECTED`].
//!
//! Describe: unknown object → [`codes::DESCRIBE_NOT_FOUND`].
//!
//! Values are JSON: `null` → SQL NULL, booleans, numbers and strings map to the
//! natural SQL literal/parameter; arrays and objects are sent as JSON text.
//! Drivers must bind values as parameters (never interpolate).
//!
//! Binary values (MySQL/MariaDB `BINARY`/`VARBINARY`/`*BLOB`, SQLite BLOB cells and
//! columns declared BLOB) travel as the string `0x` + an even number of hex digits
//! (either case; the empty value is `0x`) in a cell, insert value or row key; the
//! driver binds the real bytes and refuses any other string with
//! [`codes::MUTATION_INVALID_VALUE`]. MySQL `BIT` takes an unsigned integer, or
//! `b'0101'`. MySQL `GEOMETRY` columns refuse any non-null value with
//! [`codes::MUTATION_VALUE_NOT_EDITABLE`].

use serde::{Deserialize, Serialize};

/// Error codes with a contract meaning shared by every SQL driver.
pub mod codes {
    pub const QUERY_NOT_PAGEABLE: &str = "query.not_pageable";
    pub const QUERY_CONFIRMATION_REQUIRED: &str = "query.confirmation_required";
    /// A `payload.read_only` request wrote (the server's READ ONLY scope refused
    /// it), or its text is refused up front because it could leave that scope: it
    /// touches transaction state, or it is a script with a statement that is not a
    /// plain read. Nothing ran.
    pub const QUERY_READ_ONLY_VIOLATION: &str = "query.read_only_violation";
    pub const QUERY_CANCELLED: &str = "query.cancelled";
    pub const QUERY_INTERRUPTED: &str = "query.interrupted";
    pub const MUTATION_CANCELLED: &str = "mutation.cancelled";
    pub const MUTATION_UNSUPPORTED: &str = "mutation.unsupported";
    pub const MUTATION_BAD_REQUEST: &str = "mutation.bad_request";
    pub const MUTATION_FAILED: &str = "mutation.failed";
    /// The COMMIT did not answer with a success but may have been applied.
    pub const MUTATION_COMMIT_UNKNOWN: &str = "mutation.commit_unknown";
    /// A value cannot be written to its column in the form it was given (a
    /// binary column takes `0x` + hex digits, a BIT column an unsigned integer
    /// or `b'0101'`, a SQLite BLOB cell `0x` + hex). Nothing was written.
    pub const MUTATION_INVALID_VALUE: &str = "mutation.invalid_value";
    /// The column cannot be written through text at all (MySQL GEOMETRY and
    /// its subtypes). Nothing was written.
    pub const MUTATION_VALUE_NOT_EDITABLE: &str = "mutation.value_not_editable";
    /// A batch on a table without transactions (MyISAM, Aria, MEMORY ...) failed
    /// part way: the changes before the failing one ARE applied and were NOT
    /// rolled back. The message lists `applied`, `failed` and `not attempted`
    /// change indexes. Open pages are stale: reload them.
    pub const MUTATION_PARTIALLY_APPLIED: &str = "mutation.partially_applied";
    pub const MUTATION_NO_ROWS_AFFECTED: &str = "mutation.no_rows_affected";
    pub const MUTATION_IDENTITY_NOT_UNIQUE: &str = "mutation.identity_not_unique";
    pub const CREATE_DATABASE_REJECTED: &str = "mutation.create_database.rejected";
    pub const DESCRIBE_NOT_FOUND: &str = "describe.not_found";
}
use serde_json::{Map, Value};

/// A table-like object. `schema` is absent for drivers without a schema level
/// (`SqlDialectInfo::supports_schemas == false`); `database` is absent when the
/// connection's current database is meant.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct SqlObjectRef {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub database: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub schema: Option<String>,
    pub object: String,
}

/// A schema-level object that has source text rather than columns. Its wire
/// name (`snake_case`) is both the navigator `payload.kind` that lists it and
/// the `kind` of the nodes listed.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SchemaObjectKind {
    MaterializedView,
    Function,
    Procedure,
    Sequence,
    Trigger,
    /// PostgreSQL enum, composite and domain types.
    Type,
    Event,
}

impl SchemaObjectKind {
    /// The wire name (`"materialized_view"`, `"function"`, ...).
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::MaterializedView => "materialized_view",
            Self::Function => "function",
            Self::Procedure => "procedure",
            Self::Sequence => "sequence",
            Self::Trigger => "trigger",
            Self::Type => "type",
            Self::Event => "event",
        }
    }

    /// The kind named by a navigator `payload.kind`, `None` for anything else.
    pub fn parse(value: &str) -> Option<Self> {
        match value {
            "materialized_view" => Some(Self::MaterializedView),
            "function" => Some(Self::Function),
            "procedure" => Some(Self::Procedure),
            "sequence" => Some(Self::Sequence),
            "trigger" => Some(Self::Trigger),
            "type" => Some(Self::Type),
            "event" => Some(Self::Event),
            _ => None,
        }
    }
}

/// Payload of a schema-object navigator node, and the `describe_object`
/// request for its source. `schema` is absent for drivers without a schema
/// level. `name` is the object's own name; the rest identifies it among
/// objects of that name:
/// * `arguments`: a PostgreSQL function / procedure's input argument types
///   (`"integer, text"`), which tell overloads apart on screen;
/// * `table`: the table a trigger belongs to;
/// * `identity`: a driver-defined key that finds the object again when it is not
///   enough to match by the text above (PostgreSQL: the argument type OIDs, which
///   do not change with the connection's `search_path` the way the printed type
///   names do). Opaque to the host, which only echoes it back.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct SchemaObjectRef {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub database: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub schema: Option<String>,
    pub kind: SchemaObjectKind,
    pub name: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub arguments: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub table: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub identity: Option<String>,
}

/// `describe_object` response for a [`SchemaObjectRef`]: the statement that
/// creates the object, as the server reports it (or, for objects the server
/// cannot print, as the driver composes it from the catalog).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SchemaObjectSource {
    pub source: String,
}

/// Payload of a navigator `column` node.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SqlColumnNode {
    #[serde(flatten)]
    pub object: SqlObjectRef,
    pub column: String,
    #[serde(flatten)]
    pub details: TableColumn,
}

/// `MutationRequest.identity` for SQL mutations: which schema the table lives
/// in (the table itself is `MutationRequest.object`).
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct SqlTableIdentity {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub schema: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SqlRowOp {
    Insert,
    Update,
    Delete,
}

impl SqlRowOp {
    /// The single-operation `MutationRequest.kind` (and `mutation.<kind>`
    /// capability) for this operation.
    pub const fn kind(self) -> &'static str {
        match self {
            Self::Insert => "insert",
            Self::Update => "update",
            Self::Delete => "delete",
        }
    }
}

/// One row change.
///
/// * `insert`: `values` holds the new row (empty → `DEFAULT VALUES`); `key` unused.
/// * `update`: `key` identifies exactly one row (composite keys allowed),
///   `values` holds only the changed columns.
/// * `delete`: `key` identifies exactly one row; `values` unused.
///
/// A key that matches zero or more than one row fails the whole request and
/// rolls back (`mutation.no_rows_affected` / `mutation.identity_not_unique`).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SqlRowChange {
    pub op: SqlRowOp,
    #[serde(default, skip_serializing_if = "Map::is_empty")]
    pub key: Map<String, Value>,
    #[serde(default, skip_serializing_if = "Map::is_empty")]
    pub values: Map<String, Value>,
}

/// Payload of a `batch` mutation: applied in order inside one transaction.
/// Every change's `op` must also be advertised (`mutation.<op>`).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SqlBatch {
    pub changes: Vec<SqlRowChange>,
}

/// `describe_object` response; mirrors the application's catalog model.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct TableProperties {
    pub columns: Vec<TableColumn>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub keys: Option<Vec<TableConstraint>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub foreign_keys: Option<Vec<TableForeignKey>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub indexes: Option<Vec<TableIndex>>,
    /// Foreign keys of other tables that point at this one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub references: Option<Vec<TableForeignKey>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub triggers: Option<Vec<TableTrigger>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ddl: Option<String>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct TableColumn {
    pub name: String,
    pub data_type: String,
    pub nullable: bool,
    pub primary_key: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub is_generated: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub default_expression: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub auto_increment: Option<bool>,
    /// The column's comment (`COMMENT ON COLUMN`, MySQL `COLUMN_COMMENT`,
    /// ClickHouse `system.columns.comment`). Absent when none is set or the
    /// driver has no column comments (SQLite). Additive: older hosts ignore it
    /// and older drivers never send it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub comment: Option<String>,
    /// What a faithful `ALTER ... MODIFY COLUMN` has to restate beyond the
    /// type, nullability, default and comment (MySQL / MariaDB). Additive:
    /// absent from drivers that do not report it, which tells the host the
    /// column cannot be restated safely.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub attributes: Option<ColumnAttributes>,
}

/// Column attributes only a restatement needs (MySQL / MariaDB
/// `information_schema.COLUMNS`). `extra` is the raw `EXTRA` text
/// (`auto_increment`, `on update CURRENT_TIMESTAMP(6)`, `INVISIBLE`, `STORED
/// GENERATED`, ...), `generation_expression` the `GENERATION_EXPRESSION`.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ColumnAttributes {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub character_set: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub collation: Option<String>,
    #[serde(default)]
    pub extra: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub generation_expression: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ConstraintKind {
    PrimaryKey,
    Unique,
    Check,
    Exclusion,
    #[serde(other)]
    Other,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TableConstraint {
    pub name: String,
    pub kind: ConstraintKind,
    pub columns: Vec<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub definition: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TableForeignKey {
    pub name: String,
    pub table: String,
    pub columns: Vec<String>,
    pub referenced_table: String,
    pub referenced_columns: Vec<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub on_update: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub on_delete: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub definition: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TableIndex {
    pub name: String,
    pub columns: Vec<String>,
    pub unique: bool,
    pub primary: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub method: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub definition: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TableTrigger {
    pub name: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub timing: Option<String>,
    pub events: Vec<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub enabled: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub definition: Option<String>,
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn object_ref_omits_absent_levels() {
        let mysql = SqlObjectRef {
            database: Some("shop".into()),
            schema: None,
            object: "orders".into(),
        };
        assert_eq!(
            serde_json::to_value(&mysql).unwrap(),
            json!({"database": "shop", "object": "orders"})
        );
        let back: SqlObjectRef =
            serde_json::from_value(json!({"schema": "public", "object": "t"})).unwrap();
        assert_eq!(back.database, None);
        assert_eq!(back.schema.as_deref(), Some("public"));
    }

    #[test]
    fn column_node_flattens_object_and_details() {
        let node = SqlColumnNode {
            object: SqlObjectRef {
                database: Some("db".into()),
                schema: Some("public".into()),
                object: "orders".into(),
            },
            column: "id".into(),
            details: TableColumn {
                name: "id".into(),
                data_type: "bigint".into(),
                nullable: false,
                primary_key: true,
                ..Default::default()
            },
        };
        let value = serde_json::to_value(&node).unwrap();
        assert_eq!(value["object"], "orders");
        assert_eq!(value["column"], "id");
        assert_eq!(value["primary_key"], true);
        assert_eq!(
            serde_json::from_value::<SqlColumnNode>(value).unwrap(),
            node
        );
    }

    #[test]
    fn schema_object_refs_use_snake_case_kinds_and_omit_absent_parts() {
        let function = SchemaObjectRef {
            database: Some("app".into()),
            schema: Some("public".into()),
            kind: SchemaObjectKind::Function,
            name: "add".into(),
            arguments: Some("integer, integer".into()),
            table: None,
            identity: Some("23 23".into()),
        };
        let wire = json!({"database": "app", "schema": "public", "kind": "function",
                          "name": "add", "arguments": "integer, integer", "identity": "23 23"});
        assert_eq!(serde_json::to_value(&function).unwrap(), wire);
        assert_eq!(serde_json::from_value::<SchemaObjectRef>(wire).unwrap(), function);
        // A table reference has no `kind`: the two requests never mix up.
        assert!(serde_json::from_value::<SchemaObjectRef>(json!({"object": "t"})).is_err());
        for kind in [
            SchemaObjectKind::MaterializedView,
            SchemaObjectKind::Function,
            SchemaObjectKind::Procedure,
            SchemaObjectKind::Sequence,
            SchemaObjectKind::Trigger,
            SchemaObjectKind::Type,
            SchemaObjectKind::Event,
        ] {
            assert_eq!(serde_json::to_value(kind).unwrap(), json!(kind.as_str()));
            assert_eq!(SchemaObjectKind::parse(kind.as_str()), Some(kind));
        }
        assert_eq!(SchemaObjectKind::parse("table"), None);
    }

    #[test]
    fn batch_round_trips_with_composite_keys() {
        let wire = json!({"changes": [
            {"op": "insert", "values": {"name": "a"}},
            {"op": "update", "key": {"tenant": 1, "id": 7}, "values": {"name": "b"}},
            {"op": "delete", "key": {"tenant": 1, "id": 8}}
        ]});
        let batch: SqlBatch = serde_json::from_value(wire.clone()).unwrap();
        assert_eq!(batch.changes.len(), 3);
        assert_eq!(batch.changes[1].key.len(), 2);
        assert_eq!(batch.changes[2].op.kind(), "delete");
        assert_eq!(serde_json::to_value(&batch).unwrap(), wire);
    }

    #[test]
    fn constraint_kinds_are_snake_case_and_unknown_maps_to_other() {
        assert_eq!(
            serde_json::to_value(ConstraintKind::PrimaryKey).unwrap(),
            json!("primary_key")
        );
        let kind: ConstraintKind = serde_json::from_value(json!("foreign_key")).unwrap();
        assert_eq!(kind, ConstraintKind::Other);
    }

    #[test]
    fn table_properties_minimal_json_parses() {
        let props: TableProperties = serde_json::from_value(json!({
            "columns": [{"name": "id", "data_type": "int", "nullable": false, "primary_key": true}]
        }))
        .unwrap();
        assert_eq!(props.columns.len(), 1);
        assert!(props.keys.is_none() && props.ddl.is_none());
    }
}
