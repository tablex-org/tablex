//! `describe_object`: the Properties/DDL data of one table or view.
//!
//! A request that carries a `kind` is a `SchemaObjectRef` (function, procedure,
//! trigger, event, MariaDB sequence): see [`super::schema_objects`].
//!
//! Request: a `SqlObjectRef` (`database` optional: the connection's current
//! database; `object` required; `schema` ignored). Response: a serialized
//! `engine_driver_api::sql::TableProperties`. Lists are absent when the driver
//! cannot report them; MySQL/MariaDB report every list (empty for a view), and
//! `ConstraintKind` is snake_case (`primary_key`, `unique`, `check`).

use super::conn::describe_error;
use super::sql::{push_ident, push_qualified};
use super::{Dialect, MyConnection};
use engine_driver_kit::api::sql::{
    ColumnAttributes, ConstraintKind, SqlObjectRef, TableColumn, TableConstraint, TableForeignKey,
    TableIndex, TableProperties, TableTrigger,
};
use engine_driver_kit::BackendError;
use mysql_async::prelude::Queryable;
use mysql_async::{Conn, Row};
use serde_json::Value;
use std::fmt::Write;

pub(super) fn describe_object(
    connection: &MyConnection,
    dialect: Dialect,
    request: &Value,
) -> Result<Value, BackendError> {
    if request.get("kind").is_some() {
        return super::schema_objects::describe(connection, dialect, request);
    }
    let object: SqlObjectRef = serde_json::from_value(request.clone()).map_err(|error| {
        BackendError::with_code(
            "describe.bad_request",
            format!("request is not a SqlObjectRef: {error}"),
        )
    })?;
    if object.object.is_empty() {
        return Err(BackendError::with_code(
            "describe.bad_request",
            "missing object",
        ));
    }
    // The catalog connection: never waits behind a running user statement. Its
    // own default database is unset, so a request without one uses the user
    // connection's current database (readable without waiting).
    let database = object
        .database
        .filter(|database| !database.is_empty())
        .or_else(|| connection.current_database())
        .ok_or_else(|| BackendError::with_code("describe.bad_request", "no database selected"))?;
    let properties = connection.with_meta("describe.not_connected", async |conn| {
        load(conn, dialect, &database, &object.object).await
    })?;
    serde_json::to_value(properties).map_err(|error| BackendError::new(error.to_string()))
}

fn failed(error: mysql_async::Error) -> BackendError {
    BackendError::new(describe_error(&error))
}

async fn load(
    conn: &mut Conn,
    dialect: Dialect,
    database: &str,
    object: &str,
) -> Result<TableProperties, BackendError> {
    let params = || (database, object);
    let table_type: Option<String> = conn
        .exec_first(
            "SELECT TABLE_TYPE FROM information_schema.TABLES \
             WHERE TABLE_SCHEMA = ? AND TABLE_NAME = ?",
            params(),
        )
        .await
        .map_err(failed)?;
    let Some(table_type) = table_type else {
        return Err(BackendError::with_code(
            "describe.not_found",
            format!("`{database}`.`{object}` does not exist"),
        ));
    };
    let is_view = table_type.contains("VIEW");

    let columns = columns(conn, dialect, database, object).await?;
    let ddl = ddl(conn, database, object, is_view).await;
    if is_view {
        return Ok(TableProperties {
            columns,
            keys: Some(Vec::new()),
            foreign_keys: Some(Vec::new()),
            indexes: Some(Vec::new()),
            references: Some(Vec::new()),
            triggers: Some(Vec::new()),
            ddl,
        });
    }
    Ok(TableProperties {
        columns,
        keys: Some(keys(conn, dialect, database, object).await?),
        foreign_keys: Some(foreign_keys(conn, database, object, Direction::Outgoing).await?),
        indexes: Some(indexes(conn, dialect, database, object).await?),
        references: Some(foreign_keys(conn, database, object, Direction::Incoming).await?),
        triggers: Some(triggers(conn, database, object).await?),
        ddl,
    })
}

type ColumnRow = (
    String,
    String,
    String,
    String,
    String,
    Option<String>,
    String,
    Option<String>,
    String,
    Option<String>,
    Option<String>,
);

async fn columns(
    conn: &mut Conn,
    dialect: Dialect,
    database: &str,
    object: &str,
) -> Result<Vec<TableColumn>, BackendError> {
    let rows: Vec<ColumnRow> = conn
        .exec(
            "SELECT COLUMN_NAME, COLUMN_TYPE, DATA_TYPE, IS_NULLABLE, COLUMN_KEY, \
             COLUMN_DEFAULT, EXTRA, GENERATION_EXPRESSION, COLUMN_COMMENT, \
             CHARACTER_SET_NAME, COLLATION_NAME \
             FROM information_schema.COLUMNS \
             WHERE TABLE_SCHEMA = ? AND TABLE_NAME = ? ORDER BY ORDINAL_POSITION",
            (database, object),
        )
        .await
        .map_err(failed)?;
    Ok(rows
        .into_iter()
        .map(
            |(
                name,
                column_type,
                data_type,
                nullable,
                key,
                default,
                extra,
                generation,
                comment,
                character_set,
                collation,
            )| {
                let extra_lower = extra.to_ascii_lowercase();
                let generated =
                    extra_lower.contains("generated") && !extra_lower.contains("default_generated");
                let generation = generation.filter(|expression| !expression.is_empty());
                let default_expression = if generated {
                    generation.clone()
                } else {
                    default_expression(dialect, default, &data_type, &extra_lower)
                };
                TableColumn {
                    name,
                    data_type: column_type,
                    nullable: nullable.eq_ignore_ascii_case("YES"),
                    primary_key: key == "PRI",
                    is_generated: Some(generated),
                    default_expression,
                    auto_increment: Some(extra_lower.contains("auto_increment")),
                    comment: Some(comment).filter(|comment| !comment.is_empty()),
                    attributes: Some(ColumnAttributes {
                        character_set: character_set.filter(|set| !set.is_empty()),
                        collation: collation.filter(|collation| !collation.is_empty()),
                        extra,
                        generation_expression: generation,
                    }),
                }
            },
        )
        .collect())
}

/// `COLUMN_DEFAULT` as SQL text. The two servers differ here:
///
/// * MySQL reports the raw value for literals (`abc`, `1.50`) and the
///   expression text only for expression defaults (`DEFAULT_GENERATED` in
///   `EXTRA`), so string literals are quoted to make them SQL.
/// * MariaDB (10.2.7+) already reports SQL: literals are quoted (`'abc'`),
///   `DEFAULT NULL` is the text `NULL`, and a column without a default is SQL
///   NULL.
fn default_expression(
    dialect: Dialect,
    default: Option<String>,
    data_type: &str,
    extra_lower: &str,
) -> Option<String> {
    let default = default?;
    match dialect {
        Dialect::MariaDb => Some(default),
        Dialect::MySql => {
            let raw = extra_lower.contains("default_generated")
                || matches!(
                    data_type.to_ascii_lowercase().as_str(),
                    "tinyint"
                        | "smallint"
                        | "mediumint"
                        | "int"
                        | "bigint"
                        | "decimal"
                        | "float"
                        | "double"
                        | "bit"
                        | "year"
                );
            if raw {
                Some(default)
            } else {
                Some(quote_literal(&default))
            }
        }
    }
}

fn quote_literal(value: &str) -> String {
    let mut out = String::with_capacity(value.len() + 2);
    out.push('\'');
    for ch in value.chars() {
        match ch {
            '\'' => out.push_str("''"),
            '\\' => out.push_str("\\\\"),
            _ => out.push(ch),
        }
    }
    out.push('\'');
    out
}

fn column_list(columns: &[String]) -> String {
    let mut out = String::new();
    for (index, column) in columns.iter().enumerate() {
        if index > 0 {
            out.push_str(", ");
        }
        push_ident(&mut out, column);
    }
    out
}

async fn keys(
    conn: &mut Conn,
    dialect: Dialect,
    database: &str,
    object: &str,
) -> Result<Vec<TableConstraint>, BackendError> {
    let declared: Vec<(String, String)> = conn
        .exec(
            "SELECT CONSTRAINT_NAME, CONSTRAINT_TYPE FROM information_schema.TABLE_CONSTRAINTS \
             WHERE TABLE_SCHEMA = ? AND TABLE_NAME = ? \
             AND CONSTRAINT_TYPE IN ('PRIMARY KEY', 'UNIQUE')",
            (database, object),
        )
        .await
        .map_err(failed)?;
    let members: Vec<(String, String)> = conn
        .exec(
            "SELECT CONSTRAINT_NAME, COLUMN_NAME FROM information_schema.KEY_COLUMN_USAGE \
             WHERE TABLE_SCHEMA = ? AND TABLE_NAME = ? AND REFERENCED_TABLE_NAME IS NULL \
             ORDER BY CONSTRAINT_NAME, ORDINAL_POSITION",
            (database, object),
        )
        .await
        .map_err(failed)?;
    // MariaDB names CHECK constraints per table, MySQL per schema: only MariaDB
    // lets `CHECK_CONSTRAINTS` be joined back to the table by name.
    let table_match = match dialect {
        Dialect::MariaDb => " AND cc.TABLE_NAME = tc.TABLE_NAME",
        Dialect::MySql => "",
    };
    let checks: Vec<(String, String)> = conn
        .exec(
            format!(
                "SELECT cc.CONSTRAINT_NAME, cc.CHECK_CLAUSE \
                 FROM information_schema.TABLE_CONSTRAINTS tc \
                 JOIN information_schema.CHECK_CONSTRAINTS cc \
                   ON cc.CONSTRAINT_SCHEMA = tc.CONSTRAINT_SCHEMA \
                  AND cc.CONSTRAINT_NAME = tc.CONSTRAINT_NAME{table_match} \
                 WHERE tc.TABLE_SCHEMA = ? AND tc.TABLE_NAME = ? AND tc.CONSTRAINT_TYPE = 'CHECK' \
                 ORDER BY cc.CONSTRAINT_NAME"
            ),
            (database, object),
        )
        .await
        .map_err(failed)?;
    Ok(build_keys(declared, members, checks))
}

fn build_keys(
    declared: Vec<(String, String)>,
    members: Vec<(String, String)>,
    checks: Vec<(String, String)>,
) -> Vec<TableConstraint> {
    let mut keys: Vec<TableConstraint> = declared
        .into_iter()
        .map(|(name, kind)| {
            let columns: Vec<String> = members
                .iter()
                .filter(|(constraint, _)| *constraint == name)
                .map(|(_, column)| column.clone())
                .collect();
            let (kind, definition) = if kind == "PRIMARY KEY" {
                (
                    ConstraintKind::PrimaryKey,
                    format!("PRIMARY KEY ({})", column_list(&columns)),
                )
            } else {
                (
                    ConstraintKind::Unique,
                    format!("UNIQUE ({})", column_list(&columns)),
                )
            };
            TableConstraint {
                name,
                kind,
                columns,
                definition: Some(definition),
            }
        })
        .collect();
    keys.sort_by(|a, b| {
        (a.kind != ConstraintKind::PrimaryKey, &a.name)
            .cmp(&(b.kind != ConstraintKind::PrimaryKey, &b.name))
    });
    keys.extend(checks.into_iter().map(|(name, clause)| TableConstraint {
        name,
        kind: ConstraintKind::Check,
        columns: Vec::new(),
        definition: Some(format!("CHECK ({clause})")),
    }));
    keys
}

/// `INDEX_NAME, NON_UNIQUE, COLUMN_NAME, EXPRESSION, INDEX_TYPE`.
type IndexRow = (String, i64, Option<String>, Option<String>, String);

async fn indexes(
    conn: &mut Conn,
    dialect: Dialect,
    database: &str,
    object: &str,
) -> Result<Vec<TableIndex>, BackendError> {
    // MySQL 8 has functional key parts (`COLUMN_NAME` is NULL, `EXPRESSION` set);
    // MariaDB has no such column.
    let expression = match dialect {
        Dialect::MySql => "EXPRESSION",
        Dialect::MariaDb => "NULL",
    };
    let rows: Vec<IndexRow> = conn
        .exec(
            format!(
                "SELECT INDEX_NAME, NON_UNIQUE, COLUMN_NAME, {expression}, INDEX_TYPE \
                 FROM information_schema.STATISTICS \
                 WHERE TABLE_SCHEMA = ? AND TABLE_NAME = ? \
                 ORDER BY INDEX_NAME = 'PRIMARY' DESC, INDEX_NAME, SEQ_IN_INDEX"
            ),
            (database, object),
        )
        .await
        .map_err(failed)?;
    Ok(build_indexes(rows))
}

fn build_indexes(rows: Vec<IndexRow>) -> Vec<TableIndex> {
    let mut indexes: Vec<TableIndex> = Vec::new();
    for (name, non_unique, column, expression, method) in rows {
        let part = column.or_else(|| expression.map(|expression| format!("({expression})")));
        if let Some(index) = indexes.last_mut().filter(|index| index.name == name) {
            index.columns.extend(part);
            continue;
        }
        indexes.push(TableIndex {
            primary: name == "PRIMARY",
            unique: non_unique == 0,
            method: Some(method.to_ascii_lowercase()),
            columns: part.into_iter().collect(),
            name,
            definition: None,
        });
    }
    for index in &mut indexes {
        let mut definition = String::new();
        if index.primary {
            definition.push_str("PRIMARY KEY");
        } else {
            if index.unique {
                definition.push_str("UNIQUE ");
            }
            definition.push_str("INDEX ");
            push_ident(&mut definition, &index.name);
        }
        definition.push_str(" (");
        // Expression parts are already SQL; plain columns are quoted.
        for (position, column) in index.columns.iter().enumerate() {
            if position > 0 {
                definition.push_str(", ");
            }
            if column.starts_with('(') {
                definition.push_str(column);
            } else {
                push_ident(&mut definition, column);
            }
        }
        definition.push(')');
        if let Some(method) = &index.method {
            let _ = write!(definition, " USING {}", method.to_ascii_uppercase());
        }
        index.definition = Some(definition);
    }
    indexes
}

#[derive(Clone, Copy)]
enum Direction {
    /// Foreign keys declared by the table.
    Outgoing,
    /// Foreign keys of other tables that point at the table.
    Incoming,
}

type ForeignKeyRow = (
    String,
    String,
    String,
    String,
    String,
    String,
    String,
    String,
    String,
);

async fn foreign_keys(
    conn: &mut Conn,
    database: &str,
    object: &str,
    direction: Direction,
) -> Result<Vec<TableForeignKey>, BackendError> {
    let filter = match direction {
        Direction::Outgoing => "kcu.TABLE_SCHEMA = ? AND kcu.TABLE_NAME = ?",
        // A self-reference is already listed as an outgoing key.
        Direction::Incoming => {
            "kcu.REFERENCED_TABLE_SCHEMA = ? AND kcu.REFERENCED_TABLE_NAME = ? \
             AND NOT (kcu.TABLE_SCHEMA = kcu.REFERENCED_TABLE_SCHEMA \
                      AND kcu.TABLE_NAME = kcu.REFERENCED_TABLE_NAME)"
        }
    };
    let rows: Vec<ForeignKeyRow> = conn
        .exec(
            format!(
                "SELECT kcu.CONSTRAINT_NAME, kcu.TABLE_SCHEMA, kcu.TABLE_NAME, kcu.COLUMN_NAME, \
                        kcu.REFERENCED_TABLE_SCHEMA, kcu.REFERENCED_TABLE_NAME, \
                        kcu.REFERENCED_COLUMN_NAME, rc.UPDATE_RULE, rc.DELETE_RULE \
                 FROM information_schema.KEY_COLUMN_USAGE kcu \
                 JOIN information_schema.REFERENTIAL_CONSTRAINTS rc \
                   ON rc.CONSTRAINT_SCHEMA = kcu.CONSTRAINT_SCHEMA \
                  AND rc.CONSTRAINT_NAME = kcu.CONSTRAINT_NAME \
                  AND rc.TABLE_NAME = kcu.TABLE_NAME \
                 WHERE kcu.REFERENCED_TABLE_NAME IS NOT NULL AND {filter} \
                 ORDER BY kcu.TABLE_SCHEMA, kcu.TABLE_NAME, kcu.CONSTRAINT_NAME, kcu.ORDINAL_POSITION"
            ),
            (database, object),
        )
        .await
        .map_err(failed)?;
    Ok(build_foreign_keys(rows, database))
}

/// A table name qualified by its database only when it is another database.
fn qualify(current: &str, database: &str, table: &str) -> String {
    if database == current {
        table.to_owned()
    } else {
        format!("{database}.{table}")
    }
}

fn build_foreign_keys(rows: Vec<ForeignKeyRow>, current: &str) -> Vec<TableForeignKey> {
    let mut keys: Vec<TableForeignKey> = Vec::new();
    for (name, schema, table, column, ref_schema, ref_table, ref_column, update, delete) in rows {
        let table = qualify(current, &schema, &table);
        if let Some(key) = keys
            .last_mut()
            .filter(|key| key.name == name && key.table == table)
        {
            key.columns.push(column);
            key.referenced_columns.push(ref_column);
            continue;
        }
        keys.push(TableForeignKey {
            name,
            table,
            columns: vec![column],
            referenced_table: qualify(current, &ref_schema, &ref_table),
            referenced_columns: vec![ref_column],
            on_update: Some(update),
            on_delete: Some(delete),
            definition: None,
        });
    }
    for key in &mut keys {
        let mut definition = format!("FOREIGN KEY ({}) REFERENCES ", column_list(&key.columns));
        match key.referenced_table.split_once('.') {
            Some((schema, table)) => push_qualified(&mut definition, Some(schema), table),
            None => push_ident(&mut definition, &key.referenced_table),
        }
        let _ = write!(definition, " ({})", column_list(&key.referenced_columns));
        if let Some(rule) = &key.on_update {
            let _ = write!(definition, " ON UPDATE {rule}");
        }
        if let Some(rule) = &key.on_delete {
            let _ = write!(definition, " ON DELETE {rule}");
        }
        key.definition = Some(definition);
    }
    keys
}

async fn triggers(
    conn: &mut Conn,
    database: &str,
    object: &str,
) -> Result<Vec<TableTrigger>, BackendError> {
    let rows: Vec<(String, String, String, String)> = conn
        .exec(
            "SELECT TRIGGER_NAME, ACTION_TIMING, EVENT_MANIPULATION, ACTION_STATEMENT \
             FROM information_schema.TRIGGERS \
             WHERE EVENT_OBJECT_SCHEMA = ? AND EVENT_OBJECT_TABLE = ? ORDER BY TRIGGER_NAME",
            (database, object),
        )
        .await
        .map_err(failed)?;
    Ok(rows
        .into_iter()
        .map(|(name, timing, event, statement)| {
            let mut definition = String::from("CREATE TRIGGER ");
            push_ident(&mut definition, &name);
            let _ = write!(definition, " {timing} {event} ON ");
            push_ident(&mut definition, object);
            let _ = write!(definition, " FOR EACH ROW {statement}");
            TableTrigger {
                name,
                timing: Some(timing),
                events: vec![event],
                // MySQL and MariaDB have no way to disable a trigger.
                enabled: Some(true),
                definition: Some(definition),
            }
        })
        .collect())
}

/// `SHOW CREATE TABLE|VIEW`. `None` when the server refuses (a view the user
/// may not `SHOW CREATE`, a system view): the DDL tab then shows nothing.
async fn ddl(conn: &mut Conn, database: &str, object: &str, is_view: bool) -> Option<String> {
    let mut statement = String::from(if is_view {
        "SHOW CREATE VIEW "
    } else {
        "SHOW CREATE TABLE "
    });
    push_qualified(&mut statement, Some(database), object);
    let row: Row = conn.query_first(statement).await.ok().flatten()?;
    row.get_opt::<String, usize>(1)?.ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn defaults_are_normalised_to_sql_per_dialect() {
        let mysql = |default: Option<&str>, data_type: &str, extra: &str| {
            default_expression(Dialect::MySql, default.map(str::to_owned), data_type, extra)
        };
        assert_eq!(mysql(Some("abc"), "varchar", ""), Some("'abc'".into()));
        assert_eq!(mysql(Some("it's"), "varchar", ""), Some("'it''s'".into()));
        assert_eq!(mysql(Some("1.50"), "decimal", ""), Some("1.50".into()));
        assert_eq!(
            mysql(Some("CURRENT_TIMESTAMP"), "datetime", "default_generated"),
            Some("CURRENT_TIMESTAMP".into())
        );
        assert_eq!(mysql(None, "int", ""), None);
        let mariadb = |default: Option<&str>| {
            default_expression(Dialect::MariaDb, default.map(str::to_owned), "varchar", "")
        };
        assert_eq!(mariadb(Some("'abc'")), Some("'abc'".into()));
        assert_eq!(mariadb(Some("NULL")), Some("NULL".into()));
        assert_eq!(mariadb(None), None);
    }

    #[test]
    fn keys_list_the_primary_key_first_then_unique_then_checks() {
        let keys = build_keys(
            vec![
                ("uq_code".into(), "UNIQUE".into()),
                ("PRIMARY".into(), "PRIMARY KEY".into()),
            ],
            vec![
                ("PRIMARY".into(), "id".into()),
                ("PRIMARY".into(), "tenant".into()),
                ("uq_code".into(), "code".into()),
            ],
            vec![("chk_qty".into(), "`qty` >= 0".into())],
        );
        assert_eq!(keys[0].kind, ConstraintKind::PrimaryKey);
        assert_eq!(keys[0].columns, ["id", "tenant"]);
        assert_eq!(
            keys[0].definition.as_deref(),
            Some("PRIMARY KEY (`id`, `tenant`)")
        );
        assert_eq!(keys[1].name, "uq_code");
        assert_eq!(keys[2].kind, ConstraintKind::Check);
        assert_eq!(keys[2].definition.as_deref(), Some("CHECK (`qty` >= 0)"));
    }

    #[test]
    fn index_parts_are_grouped_by_index() {
        let indexes = build_indexes(vec![
            ("PRIMARY".into(), 0, Some("id".into()), None, "BTREE".into()),
            ("idx_ab".into(), 1, Some("a".into()), None, "BTREE".into()),
            ("idx_ab".into(), 1, Some("b".into()), None, "BTREE".into()),
            (
                "idx_fn".into(),
                1,
                None,
                Some("lower(`c`)".into()),
                "BTREE".into(),
            ),
        ]);
        assert_eq!(indexes.len(), 3);
        assert!(indexes[0].primary && indexes[0].unique);
        assert_eq!(indexes[1].columns, ["a", "b"]);
        assert_eq!(
            indexes[1].definition.as_deref(),
            Some("INDEX `idx_ab` (`a`, `b`) USING BTREE")
        );
        assert_eq!(indexes[2].columns, ["(lower(`c`))"]);
        assert_eq!(
            indexes[2].definition.as_deref(),
            Some("INDEX `idx_fn` ((lower(`c`))) USING BTREE")
        );
        assert_eq!(indexes[0].method.as_deref(), Some("btree"));
    }

    #[test]
    fn foreign_key_rows_are_grouped_and_qualified() {
        let row = |name: &str, column: &str, ref_schema: &str, ref_column: &str| -> ForeignKeyRow {
            (
                name.into(),
                "app".into(),
                "child".into(),
                column.into(),
                ref_schema.into(),
                "parent".into(),
                ref_column.into(),
                "RESTRICT".into(),
                "CASCADE".into(),
            )
        };
        let keys = build_foreign_keys(
            vec![
                row("fk_a", "a", "app", "id"),
                row("fk_a", "b", "app", "tenant"),
                row("fk_other", "c", "other", "id"),
            ],
            "app",
        );
        assert_eq!(keys.len(), 2);
        assert_eq!(keys[0].table, "child");
        assert_eq!(keys[0].columns, ["a", "b"]);
        assert_eq!(keys[0].referenced_table, "parent");
        assert_eq!(
            keys[0].definition.as_deref(),
            Some(
                "FOREIGN KEY (`a`, `b`) REFERENCES `parent` (`id`, `tenant`) \
                 ON UPDATE RESTRICT ON DELETE CASCADE"
            )
        );
        assert_eq!(keys[1].referenced_table, "other.parent");
        assert!(keys[1]
            .definition
            .as_deref()
            .unwrap()
            .contains("REFERENCES `other`.`parent`"));
    }

    #[test]
    fn reply_is_the_shared_table_properties_wire_shape() {
        let properties = TableProperties {
            columns: vec![TableColumn {
                name: "id".into(),
                data_type: "int".into(),
                nullable: false,
                primary_key: true,
                is_generated: Some(false),
                default_expression: None,
                auto_increment: Some(true),
                comment: None,
                attributes: None,
            }],
            keys: Some(vec![TableConstraint {
                name: "PRIMARY".into(),
                kind: ConstraintKind::PrimaryKey,
                columns: vec!["id".into()],
                definition: None,
            }]),
            foreign_keys: None,
            indexes: Some(Vec::new()),
            references: None,
            triggers: None,
            ddl: Some("CREATE TABLE t".into()),
        };
        let value = serde_json::to_value(&properties).unwrap();
        assert_eq!(value["keys"][0]["kind"], "primary_key");
        assert_eq!(value["columns"][0]["auto_increment"], true);
        // Lists the driver cannot report are absent, not null.
        assert!(value.get("foreign_keys").is_none());
        assert_eq!(
            serde_json::from_value::<TableProperties>(value.clone()).unwrap(),
            properties
        );
        assert_eq!(value["ddl"], "CREATE TABLE t");
    }
}
