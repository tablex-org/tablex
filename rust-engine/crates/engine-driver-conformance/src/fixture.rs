//! What a driver supplies to the conformance suite: the dialect-specific SQL
//! the scenarios cannot write portably.
//!
//! The suite creates one *scratch namespace* per scenario (a schema on
//! PostgreSQL, a database on MySQL/MariaDB) and four fixed tables in it. The
//! schema below is part of the contract between suite and fixture; every
//! fixture must create exactly these tables (types may differ per dialect):
//!
//! ```text
//! items(tenant INT NOT NULL, id INT NOT NULL, name VARCHAR(100) NULL, qty INT NULL,
//!       PRIMARY KEY (tenant, id))                       -- composite key
//! refs(id INT PRIMARY KEY, tenant INT NOT NULL, item_id INT NOT NULL,
//!      FOREIGN KEY (tenant, item_id) REFERENCES items (tenant, id))
//! notes(owner VARCHAR(20) NULL, body VARCHAR(50) NULL)  -- nullable "key" column, no PK
//! dups(k INT, v INT)                                    -- no PK: duplicate identities
//! ```
//!
//! All tables start empty. Table names contain no `_` or `%` (the navigator
//! filter test relies on that).

use engine_driver_api::sql::SqlObjectRef;
use serde_json::Value;

/// One scenario's private namespace.
#[derive(Debug, Clone)]
pub struct Scratch {
    /// Schema (PostgreSQL) or database (MySQL) name; a valid unquoted,
    /// lower-case identifier.
    pub name: String,
    /// The connection's current database (`SELECT current_database()` /
    /// `SELECT DATABASE()`).
    pub database: String,
}

/// How a mutation request addresses a table.
#[derive(Debug, Clone)]
pub struct MutationTarget {
    /// `MutationRequest.database`.
    pub database: Option<String>,
    /// `MutationRequest.object` (the bare table name).
    pub object: String,
    /// `MutationRequest.identity` (a `SqlTableIdentity`, e.g. `{"schema": ..}`).
    pub identity: Value,
}

/// Dialect-specific snippets for the SQL conformance scenarios.
///
/// Statement lists are executed in order, each through the driver's own
/// `open_query` with `payload.confirmed = true` (so setup works whether or not
/// the driver already implements confirmation).
pub trait SqlFixture: Sync {
    /// Short name for reports (`postgresql`, `mysql`, `mariadb`).
    fn label(&self) -> &str;
    /// Expected `DriverCapabilities.driver`.
    fn driver_id(&self) -> &str;
    /// Expected `SqlDialectInfo`: identifier quote, schema support.
    fn identifier_quote(&self) -> &str;
    fn supports_schemas(&self) -> bool;

    /// Whether the dialect has `CREATE DATABASE` (`database.create`,
    /// `create_database` mutations). `false` (SQLite: a database is a file) makes
    /// the `capabilities` scenario require that the token is *not* advertised and
    /// skips the `create_database` scenario.
    fn supports_create_database(&self) -> bool {
        true
    }
    /// Whether another session can interrupt a running query from SQL
    /// ([`interrupt_backend_statement`](Self::interrupt_backend_statement)).
    /// `false` (SQLite: `sqlite3_interrupt` is a C API) skips `query_interrupted`.
    fn supports_external_interrupt(&self) -> bool {
        true
    }
    /// Whether the database has transactions a user can open
    /// ([`begin`](Self::begin)). `false` (ClickHouse) skips
    /// `mutation_in_user_transaction`: there is nothing to join or roll back.
    fn supports_transactions(&self) -> bool {
        true
    }
    /// Whether the database has foreign keys, so `describe_object` of `refs` must
    /// report the composite one. `false` (ClickHouse) skips only that check.
    fn supports_foreign_keys(&self) -> bool {
        true
    }
    /// A statement that removes every row of `table`, used for scenario setup and
    /// for the check that a *confirmed* unconditional delete runs. ClickHouse has
    /// no `DELETE FROM t` without `WHERE` (it uses `TRUNCATE TABLE`).
    fn delete_all(&self, table: &str) -> String {
        format!("DELETE FROM {table}")
    }

    /// A privileged connection URI (same server) used once, before the scenarios,
    /// to run [`admin_setup`](Self::admin_setup). `None` when the test user can do
    /// everything itself. `uri` is the test user's URI.
    fn admin_uri(&self, uri: &str) -> Option<String> {
        let _ = uri;
        None
    }
    /// Privileged statements run once over [`admin_uri`](Self::admin_uri): grant the
    /// test user the right to create and drop the `cf_*`, `cfdb_*` and
    /// `cfconfirm_*` scratch databases.
    fn admin_setup(&self, uri: &str) -> Vec<String> {
        let _ = uri;
        Vec::new()
    }

    /// `SELECT` returning the current database name in one cell.
    fn current_database_query(&self) -> String;

    /// A scratch name derived from the harness's unique suffix.
    fn scratch_name(&self, unique: &str) -> String {
        format!("cf_{unique}")
    }
    fn create_scratch(&self, scratch: &Scratch) -> Vec<String>;
    /// Must be idempotent (`IF EXISTS`) and remove everything in the scratch.
    fn drop_scratch(&self, scratch: &Scratch) -> Vec<String>;
    /// Creates `items`, `refs`, `notes` and `dups` (see the module docs).
    fn create_tables(&self, scratch: &Scratch) -> Vec<String>;
    /// Qualified table name usable inside SQL text.
    fn table(&self, scratch: &Scratch, table: &str) -> String;

    /// A `SELECT n ...` returning the integers `1..=n` in ascending order, one
    /// per row, in a single column named `n`. Must work for `n` up to 100 005.
    fn numbers(&self, n: u64) -> String;
    /// A read that keeps the server busy for about `seconds` and is
    /// interruptible by the driver's cancel and by a server-side kill. It should
    /// also bound itself on the server so a driver that cannot cancel does not
    /// leave it running forever.
    fn slow_query(&self, seconds: u32) -> String;
    /// A `SELECT` returning this session's backend id (`pg_backend_pid()`,
    /// `CONNECTION_ID()`).
    fn backend_id_query(&self) -> String;
    /// A statement that interrupts the *running query* of backend `id` without
    /// killing its connection (`SELECT pg_cancel_backend(id)`, `KILL QUERY id`).
    fn interrupt_backend_statement(&self, id: &str) -> String;
    /// Starts a transaction (`BEGIN` / `START TRANSACTION`).
    fn begin(&self) -> &str {
        "BEGIN"
    }

    /// Single statements that each write exactly one new row to `items`
    /// (distinct primary keys, at least three of them). The first one is run
    /// once per scenario with offset 0, the second with offset > 0.
    fn write_statements(&self, scratch: &Scratch) -> Vec<String>;
    /// Statements that write and return rows in one statement (data-modifying
    /// CTE, `INSERT ... RETURNING`), each inserting one new row into `items`.
    /// May be empty.
    fn write_returning_statements(&self, scratch: &Scratch) -> Vec<String>;
    /// `GRANT` to a harmless target, used only to check it is *refused*.
    fn grant_statement(&self, scratch: &Scratch) -> String;
    /// A statement (or `;`-separated script) that inserts exactly three new rows
    /// into `items` with `tenant = <tenant>` (ids 1..=3) **and returns those three
    /// rows** `(tenant, id)` as its last result set, so it is a non-pageable
    /// statement that returns more rows than a small `limit`. PostgreSQL,
    /// MariaDB and SQLite use `INSERT … RETURNING`; MySQL (no `RETURNING`) uses
    /// `INSERT …; SELECT … WHERE tenant = <tenant>`. `None` skips the scenarios
    /// that need it.
    fn returning_write(&self, scratch: &Scratch, tenant: u32) -> Option<String> {
        let _ = (scratch, tenant);
        None
    }
    /// A data-modifying CTE nested in another `WITH` that deletes **every row of
    /// `items`** without a `WHERE` and returns rows, e.g. PostgreSQL
    /// `WITH x AS (WITH y AS (SELECT 1) DELETE FROM items RETURNING tenant) SELECT * FROM x`.
    /// `None` where the dialect has no data-modifying CTEs (MySQL, MariaDB, SQLite).
    fn nested_with_delete_statement(&self, scratch: &Scratch) -> Option<String> {
        let _ = scratch;
        None
    }
    /// More nested data-modifying CTEs, as `(sql, deletes_all_rows)`: the lexer-hostile
    /// forms where plain words precede the DML (PostgreSQL `SEARCH DEPTH FIRST BY n
    /// SET ord` / `CYCLE n SET c USING p`). A `deletes_all_rows` statement deletes
    /// every row of `items` without `WHERE` (needs confirmation); the others each
    /// insert one new row into `items` with a key of their own. Empty by default.
    fn nested_with_variants(&self, scratch: &Scratch) -> Vec<(String, bool)> {
        let _ = scratch;
        Vec::new()
    }
    /// Statements creating `count` extra tables (cheaply: the suite needs more
    /// than the 5 000 navigator cap) in the scratch. `None` skips the scenario.
    fn bulk_object_statements(&self, scratch: &Scratch, count: u32) -> Option<Vec<String>> {
        let _ = (scratch, count);
        None
    }
    /// Statements removing what [`bulk_object_statements`](Self::bulk_object_statements)
    /// created, in small transactions (dropping thousands of tables in one
    /// transaction exhausts PostgreSQL's lock table). Run before the scratch is
    /// dropped.
    fn bulk_object_cleanup(&self, scratch: &Scratch, count: u32) -> Vec<String> {
        let _ = (scratch, count);
        Vec::new()
    }
    /// Run [`slow_mutation_setup`](Self::slow_mutation_setup) over the privileged
    /// connection (MySQL needs SUPER to create triggers when binary logging is on).
    fn slow_mutation_setup_as_admin(&self) -> bool {
        false
    }
    /// Setup for a table `slow(id INT)` whose inserts take about 20 seconds and
    /// can be interrupted (a trigger that sleeps or loops). `None` skips
    /// `mutation_cancelled`; a setup statement that the server refuses (missing
    /// privilege) skips it too. `uri` is the test user's URI (a setup run as
    /// administrator must define the trigger *as that user*, or the server
    /// refuses the driver's `KILL QUERY` for a thread running with other rights).
    fn slow_mutation_setup(&self, scratch: &Scratch, uri: &str) -> Option<Vec<String>> {
        let _ = (scratch, uri);
        None
    }
    /// Setup for a table `swallow(id INT)` whose inserts silently affect zero
    /// rows. `None` when the dialect cannot express it.
    fn zero_row_insert_setup(&self, scratch: &Scratch) -> Option<Vec<String>> {
        let _ = scratch;
        None
    }

    /// The `NavigatorRequest` JSON listing the scratch's tables (the node
    /// payloads below it are echoed back unchanged by the suite).
    fn navigator_tables_request(&self, scratch: &Scratch) -> Value;
    /// The `SqlObjectRef` the navigator and describe use for `table`.
    fn object_ref(&self, scratch: &Scratch, table: &str) -> SqlObjectRef;
    fn mutation_target(&self, scratch: &Scratch, table: &str) -> MutationTarget;

    /// A fresh database name for `create_database`.
    fn new_database_name(&self, unique: &str) -> String {
        format!("cfdb_{unique}")
    }
    /// Idempotent removal of a database created by `create_database`.
    fn drop_database(&self, name: &str) -> Vec<String>;
    /// `CREATE DATABASE` text for the confirmation check (never expected to run).
    fn create_database_statement(&self, name: &str) -> String {
        format!("CREATE DATABASE {name}")
    }
}
