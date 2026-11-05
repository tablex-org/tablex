//! Live tests of the generated DDL against real servers (and a SQLite file),
//! through the real driver dylibs. They are `#[ignore]`d: each needs its
//! dedicated test server and builds the driver in release mode (into
//! `rust-engine/target/release`, delete it afterwards).
//!
//! ```text
//! TABLEX_TEST_PG_URI=postgres://tablex:tablex@127.0.0.1:55432/tablex \
//! TABLEX_TEST_MYSQL_URI=mysql://tablex:tablex@127.0.0.1:53306/tablex \
//! TABLEX_TEST_MARIADB_URI=mariadb://tablex:tablex@127.0.0.1:53307/tablex \
//! TABLEX_TEST_CLICKHOUSE_URI=clickhouse://tablex:tablex@127.0.0.1:58123/tablex \
//!   cargo test -p tablex-gpui --bin tablex-gpui sql_ddl::live -- --ignored --nocapture --test-threads=1
//! ```
//!
//! Every test works in the harness' uniquely named scratch namespace (a schema
//! or database it drops afterwards; for SQLite a temporary file) and creates
//! uniquely named tables only.

use std::path::{Path, PathBuf};
use std::sync::OnceLock;

use engine_driver_api::sql::{ConstraintKind, TableProperties};
use engine_driver_conformance::fixtures::{ClickHouseFixture, MysqlFixture, PostgresFixture, SqliteFixture};
use engine_driver_conformance::harness::{Conn, Ctx, Failure};
use engine_driver_conformance::{build_release_dylib, SqlFixture};
use serde_json::json;

use super::apply::{apply, StatementRunner};
use super::draft::Draft;
use super::*;
use crate::connections::ConnectionKind;

fn rust_engine(crate_dir: &str) -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../../rust-engine/crates").join(crate_dir).join("Cargo.toml")
}

pub(crate) fn dylib(crate_dir: &'static str, lib: &'static str, features: &'static [&'static str]) -> PathBuf {
    static BUILT: OnceLock<std::sync::Mutex<std::collections::HashMap<&'static str, PathBuf>>> = OnceLock::new();
    let built = BUILT.get_or_init(Default::default);
    let mut built = built.lock().unwrap();
    built
        .entry(lib)
        .or_insert_with(|| build_release_dylib(&rust_engine(crate_dir), lib, features).expect("build the driver"))
        .clone()
}

pub(crate) struct Statements<'a, 'b>(pub(crate) &'a mut Conn<'b>);

impl StatementRunner for Statements<'_, '_> {
    fn run(&mut self, sql: &str) -> Result<(), String> {
        self.0.exec(sql).map_err(|failure| match failure {
            Failure::Fail(message) | Failure::Skip(message) => message,
        })
    }
}

/// One dialect's names for the same scenario.
struct Types {
    id: &'static str,
    text: &'static str,
    short: &'static str,
}

struct Env {
    kind: ConnectionKind,
    /// PostgreSQL schema / MySQL, MariaDB, ClickHouse database; `None` for SQLite.
    namespace: Option<String>,
    /// The `database` of a describe request.
    database: Option<String>,
    types: Types,
}

impl Env {
    fn dialect(&self) -> &'static dyn DdlDialect {
        dialect_for(self.kind).expect("a SQL kind")
    }

    fn table(&self, name: &str) -> TableId {
        TableId { schema: self.namespace.clone(), name: name.into() }
    }

    fn describe(&self, conn: &mut Conn<'_>, table: &str) -> Result<TableProperties, String> {
        let schema = (self.kind == ConnectionKind::Postgres).then(|| self.namespace.clone()).flatten();
        let database = match self.kind {
            ConnectionKind::Postgres => self.database.clone(),
            ConnectionKind::Sqlite => None,
            _ => self.namespace.clone(),
        };
        let value = conn
            .describe(&json!({"database": database, "schema": schema, "object": table}))
            .map_err(|error| engine_driver_conformance::harness::describe(&error))?;
        serde_json::from_value(value).map_err(|error| error.to_string())
    }

    fn column(&self, name: &str, data_type: &str, nullable: bool) -> ColumnSpec {
        ColumnSpec { name: name.into(), data_type: data_type.into(), nullable, ..ColumnSpec::default() }
    }

    fn apply(&self, conn: &mut Conn<'_>, draft: &Draft) -> super::apply::Outcome {
        let planned = plan(self.dialect(), &draft.changes);
        for entry in &planned.entries {
            if let Err(error) = &entry.statements {
                panic!("{}: {} -> {error}", self.dialect().label(), draft.changes[entry.change].title());
            }
        }
        eprintln!("--- {} ---\n{}", self.dialect().label(), planned.script());
        let outcome = apply(&planned, &mut Statements(conn));
        assert!(outcome.succeeded(), "{}", outcome.report(&planned.steps()));
        outcome
    }
}

fn columns_of(properties: &TableProperties) -> Vec<&str> {
    properties.columns.iter().map(|c| c.name.as_str()).collect()
}

fn column<'a>(properties: &'a TableProperties, name: &str) -> &'a engine_driver_api::sql::TableColumn {
    properties.columns.iter().find(|c| c.name == name).unwrap_or_else(|| panic!("column {name} in {:?}", columns_of(properties)))
}

fn not_found(env: &Env, conn: &mut Conn<'_>, table: &str) -> bool {
    env.describe(conn, table).is_err()
}

/// The shared scenario: create, alter, drop, and a failing change-set.
fn scenario(env: &Env, conn: &mut Conn<'_>, unique: &str) {
    let d = env.dialect();
    let caps = d.capabilities();
    let comments = caps.column_comments;
    let (customers, orders, purchases) = (format!("customers_{unique}"), format!("orders_{unique}"), format!("purchases_{unique}"));
    let (fk_name, idx_name) = (format!("fk_{unique}"), format!("idx_note_{unique}"));

    // 1. create two tables, a foreign key and an index
    let mut draft = Draft::default();
    draft.push(Change::CreateTable {
        table: env.table(&customers),
        columns: vec![
            env.column("id", env.types.id, false),
            ColumnSpec { comment: comments.then(|| "the customer's name".to_string()), ..env.column("name", env.types.short, true) },
        ],
        primary_key: vec!["id".into()],
        comment: None,
    });
    draft.push(Change::CreateTable {
        table: env.table(&orders),
        columns: vec![
            env.column("id", env.types.id, false),
            env.column("customer_id", env.types.id, true),
            env.column("note", env.types.short, true),
            env.column("spare", env.types.text, true),
        ],
        primary_key: vec!["id".into()],
        comment: None,
    });
    if caps.foreign_keys || d.inline_foreign_keys() {
        draft.push(Change::AddForeignKey {
            table: env.table(&orders),
            foreign_key: ForeignKeySpec {
                name: fk_name.clone(),
                columns: vec!["customer_id".into()],
                ref_table: env.table(&customers),
                ref_columns: vec!["id".into()],
                on_delete: Some(FkAction::Cascade),
                on_update: None,
            },
        });
    }
    if caps.indexes {
        draft.push(Change::CreateIndex {
            table: env.table(&orders),
            index: IndexSpec { name: idx_name.clone(), columns: vec!["note".into()], unique: false },
        });
    }
    env.apply(conn, &draft);

    let found = env.describe(conn, &orders).expect("orders exists");
    assert_eq!(columns_of(&found), ["id", "customer_id", "note", "spare"], "{found:?}");
    assert!(column(&found, "id").primary_key && !column(&found, "id").nullable);
    assert!(column(&found, "customer_id").nullable);
    if comments {
        let named = env.describe(conn, &customers).expect("customers exists");
        assert_eq!(column(&named, "name").comment.as_deref(), Some("the customer's name"), "{named:?}");
    }
    if caps.foreign_keys || d.inline_foreign_keys() {
        let fks = found.foreign_keys.as_deref().unwrap_or_default();
        assert_eq!(fks.len(), 1, "{found:?}");
        assert_eq!(fks[0].columns, ["customer_id"]);
        assert!(fks[0].referenced_table.ends_with(&customers), "{:?}", fks[0]);
        if caps.foreign_keys {
            // SQLite reports a name of its own for a foreign key.
            assert_eq!(fks[0].name, fk_name);
        }
    }
    if caps.indexes {
        let indexes = found.indexes.as_deref().unwrap_or_default();
        assert!(indexes.iter().any(|i| i.name == idx_name && i.columns == ["note"]), "{indexes:?}");
    }

    // 2. alter: add / alter / rename / drop columns, indexes, foreign key, primary key, rename the table
    let mut draft = Draft::default();
    let t = env.table(&orders);
    draft.push(Change::AddColumn {
        table: t.clone(),
        column: ColumnSpec {
            default: Some("new".into()),
            comment: comments.then(|| "order state".to_string()),
            ..env.column("status", env.types.short, false)
        },
    });
    draft.push(Change::DropColumn { table: t.clone(), column: "spare".into() });
    if caps.alter_column {
        let before = found.columns.iter().find(|c| c.name == "note").unwrap();
        // What the draft starts from, with the attributes the driver reports (MODIFY restates them).
        let attributes = before.attributes.as_ref().map(|a| ColumnAttributes {
            character_set: a.character_set.clone(),
            collation: a.collation.clone(),
            extra: a.extra.clone(),
            generation_expression: a.generation_expression.clone(),
        });
        let from = ColumnSpec {
            default: None,
            comment: None,
            attributes: attributes.clone(),
            ..env.column("note", &before.data_type, before.nullable)
        };
        // ClickHouse needs a default to turn a Nullable column into a plain one.
        let default = (env.kind == ConnectionKind::Clickhouse).then(|| "none".to_string());
        let to = ColumnSpec {
            default,
            comment: comments.then(|| "free text".to_string()),
            attributes,
            ..env.column("note", &before.data_type, false)
        };
        draft.push(Change::AlterColumn { table: t.clone(), from, to });
    }
    draft.push(Change::RenameColumn { table: t.clone(), from: "note".into(), to: "memo".into() });
    if caps.indexes {
        draft.push(Change::DropIndex { table: t.clone(), name: idx_name.clone() });
        draft.push(Change::CreateIndex {
            table: t.clone(),
            index: IndexSpec { name: format!("idx_memo_{unique}"), columns: vec!["memo".into()], unique: true },
        });
    }
    if caps.foreign_keys {
        draft.push(Change::DropForeignKey { table: t.clone(), name: fk_name.clone() });
        draft.push(Change::AddForeignKey {
            table: t.clone(),
            foreign_key: ForeignKeySpec {
                name: format!("fk2_{unique}"),
                columns: vec!["customer_id".into()],
                ref_table: env.table(&customers),
                ref_columns: vec!["id".into()],
                on_delete: Some(FkAction::SetNull),
                on_update: None,
            },
        });
    }
    if caps.primary_key_edit {
        let previous = found
            .keys
            .as_deref()
            .unwrap_or_default()
            .iter()
            .find(|key| key.kind == ConstraintKind::PrimaryKey)
            .map(|key| PrimaryKeyRef { name: Some(key.name.clone()), columns: key.columns.clone() });
        draft.push(Change::SetPrimaryKey { table: t.clone(), columns: vec!["id".into(), "status".into()], previous });
    }
    draft.push(Change::RenameTable { table: t.clone(), to: purchases.clone() });
    // The draft keeps every later change on the table under its new name.
    assert!(draft.changes.iter().all(|c| matches!(c, Change::RenameTable { .. }) || c.table().is_some_and(|t| t.name == purchases)));
    env.apply(conn, &draft);

    assert!(not_found(env, conn, &orders), "the old name is gone");
    let moved = env.describe(conn, &purchases).expect("purchases exists");
    assert_eq!(columns_of(&moved).len(), 4, "{moved:?}");
    for expected in ["id", "customer_id", "memo", "status"] {
        assert!(columns_of(&moved).contains(&expected), "{expected} in {moved:?}");
    }
    let status = column(&moved, "status");
    assert!(!status.nullable);
    assert!(status.default_expression.as_deref().is_some_and(|d| d.contains("new")), "{status:?}");
    if comments {
        assert_eq!(status.comment.as_deref(), Some("order state"));
    }
    if caps.alter_column {
        assert!(!column(&moved, "memo").nullable, "{moved:?}");
        if comments {
            assert_eq!(column(&moved, "memo").comment.as_deref(), Some("free text"));
        }
    }
    if caps.indexes {
        let indexes = moved.indexes.as_deref().unwrap_or_default();
        assert!(indexes.iter().any(|i| i.name == format!("idx_memo_{unique}") && i.unique), "{indexes:?}");
        assert!(!indexes.iter().any(|i| i.name == idx_name), "{indexes:?}");
    }
    if caps.foreign_keys {
        let fks = moved.foreign_keys.as_deref().unwrap_or_default();
        assert_eq!(fks.len(), 1, "{moved:?}");
        assert_eq!(fks[0].name, format!("fk2_{unique}"));
        assert!(fks[0].on_delete.as_deref().is_some_and(|a| a.eq_ignore_ascii_case("set null")), "{:?}", fks[0]);
    }
    if caps.primary_key_edit {
        assert!(column(&moved, "id").primary_key && column(&moved, "status").primary_key, "{moved:?}");
    }

    // 3. a change-set that fails half-way reports exactly what ran
    let extra = |name: &str| Change::AddColumn { table: env.table(&purchases), column: env.column(name, env.types.short, true) };
    let missing = Change::AddColumn { table: env.table(&format!("missing_{unique}")), column: env.column("x", env.types.short, true) };
    let changes = vec![extra("extra1"), missing, extra("extra2")];
    let planned = plan(d, &changes);
    assert!(planned.executable());
    let outcome = apply(&planned, &mut Statements(conn));
    let report = outcome.report(&planned.steps());
    eprintln!("{report}");
    assert_eq!(outcome.failed.as_ref().map(|f| f.step), Some(1), "{report}");
    let after = env.describe(conn, &purchases).expect("still there");
    if d.transactional() {
        assert!(outcome.rolled_back && outcome.applied.is_empty(), "{report}");
        assert!(!columns_of(&after).contains(&"extra1"), "rolled back: {after:?}");
        assert!(outcome.completed_changes(&planned).is_empty());
        assert!(report.contains("nothing was applied"), "{report}");
    } else {
        assert_eq!(outcome.applied, [0], "{report}");
        assert!(columns_of(&after).contains(&"extra1") && !columns_of(&after).contains(&"extra2"), "{after:?}");
        assert_eq!(outcome.completed_changes(&planned), [0]);
        assert!(report.contains("These 1 statement(s) ran"), "{report}");
    }

    // 4. drop the foreign key's table last, the key first
    let mut draft = Draft::default();
    draft.push(Change::DropTable { table: env.table(&customers), cascade: false });
    draft.push(Change::DropTable { table: env.table(&purchases), cascade: false });
    if caps.foreign_keys {
        draft.push(Change::DropForeignKey { table: env.table(&purchases), name: format!("fk2_{unique}") });
    }
    // Dropping the parent while the foreign key exists would fail; the plan puts the key first.
    env.apply(conn, &draft);
    assert!(not_found(env, conn, &customers) && not_found(env, conn, &purchases));
}

/// Names that need every bit of the quoting.
fn hostile(env: &Env, conn: &mut Conn<'_>, unique: &str) {
    let quote = env.dialect().quoting().identifier_quote;
    // A backslash right before a quote: ClickHouse reads it as an escape inside backticks.
    let table = format!("we{quote}ird\\{quote}; tab -- ü✓ {unique}");
    let column_name = format!("co{quote}l; DROP TABLE x; -- ✓");
    let mut draft = Draft::default();
    draft.push(Change::CreateTable {
        table: env.table(&table),
        columns: vec![env.column("id", env.types.id, false), env.column(&column_name, env.types.short, true)],
        primary_key: vec!["id".into()],
        comment: None,
    });
    env.apply(conn, &draft);
    let found = env.describe(conn, &table).expect("created under its exact name");
    assert_eq!(columns_of(&found), ["id", column_name.as_str()]);
    let mut draft = Draft::default();
    draft.push(Change::RenameColumn { table: env.table(&table), from: column_name.clone(), to: format!("{column_name} 2") });
    draft.push(Change::AddColumn {
        table: env.table(&table),
        column: ColumnSpec {
            default: Some("it's; -- not a comment".into()),
            comment: env.dialect().capabilities().column_comments.then(|| "it's \\ a 'comment'; -- x".to_string()),
            ..env.column("note", env.types.short, true)
        },
    });
    env.apply(conn, &draft);
    let found = env.describe(conn, &table).expect("still there");
    assert!(columns_of(&found).contains(&format!("{column_name} 2").as_str()), "{found:?}");
    let note = column(&found, "note");
    assert!(note.default_expression.as_deref().is_some_and(|d| d.contains("it's") || d.contains("it''s") || d.contains("it\\'s")), "{note:?}");
    if env.dialect().capabilities().column_comments {
        assert_eq!(note.comment.as_deref(), Some("it's \\ a 'comment'; -- x"));
    }
    let mut draft = Draft::default();
    draft.push(Change::DropTable { table: env.table(&table), cascade: false });
    env.apply(conn, &draft);
    assert!(not_found(env, conn, &table));
}

fn uri(var: &str) -> Option<String> {
    let value = std::env::var(var).ok();
    if value.is_none() {
        eprintln!("skipped: {var} is not set");
    }
    value
}

fn run(fixture: &dyn SqlFixture, uri: &str, dylib: PathBuf, kind: ConnectionKind, types: Types, scratch: bool) {
    let ctx = Ctx::load(&dylib, uri, fixture).expect("load the driver");
    let guard = scratch.then(|| ctx.scratch(false).expect("scratch namespace"));
    let (namespace, database) = match &guard {
        Some(guard) => (Some(guard.scratch.name.clone()), Some(guard.scratch.database.clone())),
        None => (None, None),
    };
    let env = Env { kind, namespace, database, types };
    let unique = ctx.unique();
    let mut conn = ctx.connect().expect("connect");
    scenario(&env, &mut conn, &unique);
    hostile(&env, &mut conn, &unique);
}

#[test]
#[ignore = "needs TABLEX_TEST_PG_URI"]
fn postgres_applies_a_change_set_and_rolls_back_a_failing_one() {
    let Some(uri) = uri("TABLEX_TEST_PG_URI") else { return };
    run(
        &PostgresFixture,
        &uri,
        dylib("dbengine-postgres", "dbengine_postgres", &["abi-v2"]),
        ConnectionKind::Postgres,
        Types { id: "bigint", text: "text", short: "varchar(50)" },
        true,
    );
}

#[test]
#[ignore = "needs TABLEX_TEST_MYSQL_URI"]
fn mysql_applies_a_change_set_and_stops_at_a_failing_statement() {
    let Some(uri) = uri("TABLEX_TEST_MYSQL_URI") else { return };
    run(
        &MysqlFixture::MYSQL,
        &uri,
        dylib("dbengine-mysql", "dbengine_mysql", &["abi-v2"]),
        ConnectionKind::Mysql,
        Types { id: "bigint", text: "text", short: "varchar(50)" },
        true,
    );
}

#[test]
#[ignore = "needs TABLEX_TEST_MARIADB_URI"]
fn mariadb_applies_a_change_set_and_stops_at_a_failing_statement() {
    let Some(uri) = uri("TABLEX_TEST_MARIADB_URI") else { return };
    run(
        &MysqlFixture::MARIADB,
        &uri,
        dylib("dbengine-mariadb", "dbengine_mariadb", &["abi-v2"]),
        ConnectionKind::Mariadb,
        Types { id: "bigint", text: "text", short: "varchar(50)" },
        true,
    );
}

#[test]
#[ignore = "needs TABLEX_TEST_CLICKHOUSE_URI"]
fn clickhouse_applies_a_change_set_and_stops_at_a_failing_statement() {
    let Some(uri) = uri("TABLEX_TEST_CLICKHOUSE_URI") else { return };
    run(
        &ClickHouseFixture,
        &uri,
        dylib("dbengine-clickhouse", "dbengine_clickhouse", &[]),
        ConnectionKind::Clickhouse,
        Types { id: "Int64", text: "String", short: "String" },
        true,
    );
}

#[test]
#[ignore = "builds the SQLite driver"]
fn sqlite_applies_what_it_can_and_rolls_back_a_failing_change_set() {
    let dir = tempfile::TempDir::new().unwrap();
    let uri = format!("sqlite://{}", dir.path().join("ddl.db").display());
    run(
        &SqliteFixture,
        &uri,
        dylib("dbengine-sqlite", "dbengine_sqlite", &[]),
        ConnectionKind::Sqlite,
        Types { id: "INTEGER", text: "TEXT", short: "TEXT" },
        false,
    );
}
