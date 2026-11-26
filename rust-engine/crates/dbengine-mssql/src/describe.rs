//! `dbengine_describe_object` (capability `metadata.table_properties`).
//!
//! Request: a `SqlObjectRef` (`{"database", "schema", "object"}`; `schema` defaults to `dbo`,
//! `database` to the connection's). A request that carries a `kind` is a `SchemaObjectRef`
//! instead (function, procedure, trigger, sequence): the response is a `SchemaObjectSource`.
//!
//! Response: a serialized `TableProperties`: columns, keys (primary key, unique and check
//! constraints), foreign keys, indexes, the foreign keys of other tables that point here,
//! triggers, and a `ddl` composed from the catalog (SQL Server cannot print a table's
//! `CREATE`): the columns with `IDENTITY` and computed definitions, the constraints inline, then
//! one `CREATE INDEX` per index that backs no constraint. A view's `ddl` is its definition.
//! Errors: `describe.bad_request` (no `object`), `describe.not_found`, `describe.failed`. Runs on
//! the metadata connection, so it never waits behind a running user query.

use crate::catalog::{self, flag, int, object_name, opt_text, query, resolve_database, rows, sys, text};
use crate::conn::{Area, Client, MsConnection};
use crate::sql::{push_ident, push_qualified};
use engine_driver_kit::api::sql::{
    codes, ConstraintKind, SchemaObjectKind, SchemaObjectRef, SchemaObjectSource, SqlObjectRef,
    TableConstraint, TableForeignKey, TableIndex, TableProperties, TableTrigger,
};
use engine_driver_kit::BackendError;
use serde_json::Value;
use std::fmt::Write;

fn bad_request(message: impl Into<String>) -> BackendError {
    BackendError::with_code("describe.bad_request", message)
}

pub(crate) fn describe_object(connection: &MsConnection, request: &Value) -> Result<Value, BackendError> {
    let configured = connection.configured_database().map(str::to_owned);
    if request.get("kind").is_some() {
        let reference: SchemaObjectRef = serde_json::from_value(request.clone())
            .map_err(|error| bad_request(format!("describe needs a schema object reference ({error})")))?;
        if reference.name.is_empty() {
            return Err(bad_request("describe needs the object's `name`"));
        }
        let source = connection.with_meta(Area::Describe, async |client| {
            source(client, configured.as_deref(), &reference).await
        })?;
        let Some(source) = source else {
            return Err(BackendError::with_code(
                codes::DESCRIBE_NOT_FOUND,
                format!("{} `{}` does not exist", reference.kind.as_str(), reference.name),
            ));
        };
        return serde_json::to_value(SchemaObjectSource { source })
            .map_err(|error| BackendError::new(error.to_string()));
    }
    let object: SqlObjectRef = serde_json::from_value(request.clone())
        .map_err(|error| bad_request(format!("describe needs an object reference ({error})")))?;
    if object.object.is_empty() {
        return Err(bad_request("describe needs `object` (table or view)"));
    }
    let properties = connection.with_meta(Area::Describe, async |client| {
        table_properties(client, configured.as_deref(), &object).await
    })?;
    let Some(properties) = properties else {
        return Err(BackendError::with_code(
            codes::DESCRIBE_NOT_FOUND,
            format!("`{}` does not exist", object.object),
        ));
    };
    serde_json::to_value(properties).map_err(|error| BackendError::new(error.to_string()))
}

/// The source text of a function, procedure, trigger or sequence; `None` when it is gone.
async fn source(
    client: &mut Client,
    configured: Option<&str>,
    reference: &SchemaObjectRef,
) -> Result<Option<String>, BackendError> {
    let database = resolve_database(client, Area::Describe, reference.database.as_deref(), configured).await?;
    let schema = reference.schema.as_deref().filter(|schema| !schema.is_empty()).unwrap_or("dbo");
    match reference.kind {
        SchemaObjectKind::Function | SchemaObjectKind::Procedure | SchemaObjectKind::Trigger => {
            let sql = format!(
                "SELECT m.definition FROM {m} m JOIN {o} o ON o.object_id = m.object_id \
                 JOIN {s} s ON s.schema_id = o.schema_id WHERE s.name = @P1 AND o.name = @P2",
                m = sys(&database, "sql_modules"),
                o = sys(&database, "objects"),
                s = sys(&database, "schemas"),
            );
            let found = rows(client, Area::Describe, query(sql, &[schema, &reference.name])).await?;
            Ok(found.first().and_then(|row| opt_text(row, 0)))
        }
        SchemaObjectKind::Sequence => {
            let sql = format!(
                "SELECT TYPE_NAME(q.user_type_id), CAST(q.start_value AS nvarchar(60)), CAST(q.increment AS nvarchar(60)), \
                        CAST(q.minimum_value AS nvarchar(60)), CAST(q.maximum_value AS nvarchar(60)), \
                        CAST(q.is_cycling AS int), CAST(q.is_cached AS int), CAST(q.cache_size AS int) \
                 FROM {q} q JOIN {s} s ON s.schema_id = q.schema_id WHERE s.name = @P1 AND q.name = @P2",
                q = sys(&database, "sequences"),
                s = sys(&database, "schemas"),
            );
            let found = rows(client, Area::Describe, query(sql, &[schema, &reference.name])).await?;
            Ok(found.first().map(|row| {
                let mut out = String::from("CREATE SEQUENCE ");
                push_qualified(&mut out, None, Some(schema), &reference.name);
                let _ = write!(
                    out,
                    "\n    AS {}\n    START WITH {}\n    INCREMENT BY {}\n    MINVALUE {}\n    MAXVALUE {}\n    {}",
                    text(row, 0),
                    text(row, 1),
                    text(row, 2),
                    text(row, 3),
                    text(row, 4),
                    if flag(row, 5) { "CYCLE" } else { "NO CYCLE" }
                );
                if flag(row, 6) {
                    let _ = write!(out, "\n    CACHE {}", int(row, 7));
                } else {
                    out.push_str("\n    NO CACHE");
                }
                out
            }))
        }
        other => Err(bad_request(format!("SQL Server has no `{}` objects", other.as_str()))),
    }
}

async fn table_properties(
    client: &mut Client,
    configured: Option<&str>,
    object: &SqlObjectRef,
) -> Result<Option<TableProperties>, BackendError> {
    let area = Area::Describe;
    let database = resolve_database(client, area, object.database.as_deref(), configured).await?;
    let schema = object.schema.as_deref().filter(|schema| !schema.is_empty()).unwrap_or("dbo");
    let table = object.object.as_str();
    let kind_sql = format!(
        "SELECT o.type FROM {o} o JOIN {s} s ON s.schema_id = o.schema_id \
         WHERE s.name = @P1 AND o.name = @P2 AND o.type IN ('U', 'V')",
        o = sys(&database, "objects"),
        s = sys(&database, "schemas"),
    );
    let found = rows(client, area, query(kind_sql, &[schema, table])).await?;
    let Some(is_view) = found.first().map(|row| text(row, 0).trim() == "V") else {
        return Ok(None);
    };
    let name = object_name(&database, schema, table);
    let columns = catalog::columns(client, area, &database, schema, table).await?;

    // Constraints: primary key, unique, check.
    let mut keys: Vec<TableConstraint> = Vec::new();
    let sql = format!(
        "SELECT kc.name, kc.type, c.name \
         FROM {kc} kc JOIN {ic} ic ON ic.object_id = kc.parent_object_id AND ic.index_id = kc.unique_index_id \
         JOIN {c} c ON c.object_id = ic.object_id AND c.column_id = ic.column_id \
         WHERE kc.parent_object_id = OBJECT_ID(@P1) AND ic.is_included_column = 0 \
         ORDER BY kc.name, ic.key_ordinal",
        kc = sys(&database, "key_constraints"),
        ic = sys(&database, "index_columns"),
        c = sys(&database, "columns"),
    );
    for row in rows(client, area, query(sql, &[&name])).await? {
        let (key_name, column) = (text(&row, 0), text(&row, 2));
        match keys.last_mut() {
            Some(last) if last.name == key_name => last.columns.push(column),
            _ => keys.push(TableConstraint {
                name: key_name,
                kind: if text(&row, 1).trim() == "PK" { ConstraintKind::PrimaryKey } else { ConstraintKind::Unique },
                columns: vec![column],
                definition: None,
            }),
        }
    }
    let sql = format!(
        "SELECT name, definition FROM {} WHERE parent_object_id = OBJECT_ID(@P1) ORDER BY name",
        sys(&database, "check_constraints")
    );
    for row in rows(client, area, query(sql, &[&name])).await? {
        keys.push(TableConstraint {
            name: text(&row, 0),
            kind: ConstraintKind::Check,
            columns: Vec::new(),
            definition: opt_text(&row, 1),
        });
    }

    let foreign_keys = foreign_keys(client, &database, schema, table, &name, false).await?;
    let references = foreign_keys_to(client, &database, schema, table, &name).await?;

    // Indexes (and which of them back a constraint).
    let sql = format!(
        "SELECT i.name, i.type_desc, CAST(i.is_unique AS int), CAST(i.is_primary_key AS int), \
                CAST(i.is_unique_constraint AS int), ic.is_included_column, c.name, ic.is_descending_key, i.filter_definition \
         FROM {i} i JOIN {ic} ic ON ic.object_id = i.object_id AND ic.index_id = i.index_id \
         JOIN {c} c ON c.object_id = ic.object_id AND c.column_id = ic.column_id \
         WHERE i.object_id = OBJECT_ID(@P1) AND i.index_id > 0 \
         ORDER BY i.index_id, ic.is_included_column, ic.key_ordinal, ic.index_column_id",
        i = sys(&database, "indexes"),
        ic = sys(&database, "index_columns"),
        c = sys(&database, "columns"),
    );
    struct Index {
        index: TableIndex,
        constraint: bool,
        include: Vec<String>,
        keys: Vec<String>,
        filter: Option<String>,
    }
    let mut indexes: Vec<Index> = Vec::new();
    for row in rows(client, area, query(sql, &[&name])).await? {
        let index_name = text(&row, 0);
        let included = flag(&row, 5);
        let mut column = text(&row, 6);
        if !included && flag(&row, 7) {
            column.push_str(" DESC");
        }
        if indexes.last().is_none_or(|last| last.index.name != index_name) {
            indexes.push(Index {
                index: TableIndex {
                    name: index_name,
                    columns: Vec::new(),
                    unique: flag(&row, 2),
                    primary: flag(&row, 3),
                    method: Some(text(&row, 1)),
                    definition: None,
                },
                constraint: flag(&row, 3) || flag(&row, 4),
                include: Vec::new(),
                keys: Vec::new(),
                filter: opt_text(&row, 8),
            });
        }
        let Some(last) = indexes.last_mut() else { continue };
        if included {
            last.include.push(column);
        } else {
            last.index.columns.push(text(&row, 6));
            last.keys.push(column);
        }
    }
    for entry in &mut indexes {
        let mut definition = String::from("CREATE ");
        if entry.index.unique {
            definition.push_str("UNIQUE ");
        }
        let method = entry.index.method.as_deref().unwrap_or("");
        if method.eq_ignore_ascii_case("CLUSTERED") || method.eq_ignore_ascii_case("NONCLUSTERED") {
            definition.push_str(method);
            definition.push(' ');
        }
        definition.push_str("INDEX ");
        push_ident(&mut definition, &entry.index.name);
        definition.push_str(" ON ");
        push_qualified(&mut definition, None, Some(schema), table);
        let _ = write!(definition, " ({})", entry.keys.iter().map(|column| quote_with_order(column)).collect::<Vec<_>>().join(", "));
        if !entry.include.is_empty() {
            let _ = write!(definition, " INCLUDE ({})", entry.include.iter().map(|column| quote_with_order(column)).collect::<Vec<_>>().join(", "));
        }
        if let Some(filter) = &entry.filter {
            let _ = write!(definition, " WHERE {filter}");
        }
        entry.index.definition = Some(definition);
    }

    // Triggers.
    let sql = format!(
        "SELECT tr.name, CAST(tr.is_instead_of_trigger AS int), CAST(tr.is_disabled AS int), m.definition \
         FROM {tr} tr LEFT JOIN {m} m ON m.object_id = tr.object_id \
         WHERE tr.parent_id = OBJECT_ID(@P1) ORDER BY tr.name",
        tr = sys(&database, "triggers"),
        m = sys(&database, "sql_modules"),
    );
    let mut triggers: Vec<TableTrigger> = rows(client, area, query(sql, &[&name]))
        .await?
        .iter()
        .map(|row| TableTrigger {
            name: text(row, 0),
            timing: Some(if flag(row, 1) { "INSTEAD OF" } else { "AFTER" }.to_owned()),
            events: Vec::new(),
            enabled: Some(!flag(row, 2)),
            definition: opt_text(row, 3),
        })
        .collect();
    let sql = format!(
        "SELECT tr.name, te.type_desc FROM {tr} tr JOIN {te} te ON te.object_id = tr.object_id \
         WHERE tr.parent_id = OBJECT_ID(@P1) ORDER BY tr.name, te.type",
        tr = sys(&database, "triggers"),
        te = sys(&database, "trigger_events"),
    );
    for row in rows(client, area, query(sql, &[&name])).await? {
        let trigger = text(&row, 0);
        if let Some(found) = triggers.iter_mut().find(|candidate| candidate.name == trigger) {
            found.events.push(text(&row, 1));
        }
    }

    let ddl = if is_view {
        let sql = format!(
            "SELECT m.definition FROM {m} m WHERE m.object_id = OBJECT_ID(@P1)",
            m = sys(&database, "sql_modules")
        );
        rows(client, area, query(sql, &[&name])).await?.first().and_then(|row| opt_text(row, 0))
    } else {
        Some(compose_ddl(client, &database, schema, table, &name, &columns, &keys, &foreign_keys, &indexes.iter().filter(|entry| !entry.constraint).map(|entry| entry.index.definition.clone().unwrap_or_default()).collect::<Vec<_>>()).await?)
    };
    Ok(Some(TableProperties {
        columns: columns.iter().map(catalog::ColumnInfo::detail).collect(),
        keys: Some(keys),
        foreign_keys: Some(foreign_keys),
        indexes: Some(indexes.into_iter().map(|entry| entry.index).collect()),
        references: Some(references),
        triggers: Some(triggers),
        ddl,
    }))
}

/// `col DESC` -> `[col] DESC`.
fn quote_with_order(column: &str) -> String {
    let (name, order) = column.strip_suffix(" DESC").map_or((column, ""), |name| (name, " DESC"));
    let mut out = String::new();
    push_ident(&mut out, name);
    out.push_str(order);
    out
}

/// Foreign keys of `table` (`incoming == false`).
async fn foreign_keys(
    client: &mut Client,
    database: &str,
    schema: &str,
    table: &str,
    name: &str,
    _incoming: bool,
) -> Result<Vec<TableForeignKey>, BackendError> {
    let sql = format!(
        "SELECT fk.name, fk.update_referential_action_desc, fk.delete_referential_action_desc, \
                rs.name, rt.name, pc.name, rc.name \
         FROM {fk} fk JOIN {fkc} fkc ON fkc.constraint_object_id = fk.object_id \
         JOIN {c} pc ON pc.object_id = fkc.parent_object_id AND pc.column_id = fkc.parent_column_id \
         JOIN {t} rt ON rt.object_id = fkc.referenced_object_id \
         JOIN {s} rs ON rs.schema_id = rt.schema_id \
         JOIN {c} rc ON rc.object_id = fkc.referenced_object_id AND rc.column_id = fkc.referenced_column_id \
         WHERE fk.parent_object_id = OBJECT_ID(@P1) \
         ORDER BY fk.name, fkc.constraint_column_id",
        fk = sys(database, "foreign_keys"),
        fkc = sys(database, "foreign_key_columns"),
        c = sys(database, "columns"),
        t = sys(database, "tables"),
        s = sys(database, "schemas"),
    );
    let mut keys: Vec<TableForeignKey> = Vec::new();
    for row in rows(client, Area::Describe, query(sql, &[name])).await? {
        let key_name = text(&row, 0);
        match keys.last_mut() {
            Some(last) if last.name == key_name => {
                last.columns.push(text(&row, 5));
                last.referenced_columns.push(text(&row, 6));
            }
            _ => keys.push(TableForeignKey {
                name: key_name,
                table: table.to_owned(),
                columns: vec![text(&row, 5)],
                referenced_table: qualify(schema, &text(&row, 3), &text(&row, 4)),
                referenced_columns: vec![text(&row, 6)],
                on_update: Some(action(&text(&row, 1))),
                on_delete: Some(action(&text(&row, 2))),
                definition: None,
            }),
        }
    }
    for key in &mut keys {
        key.definition = Some(fk_definition(schema, key));
    }
    Ok(keys)
}

/// Foreign keys of other tables that point at `table`.
async fn foreign_keys_to(
    client: &mut Client,
    database: &str,
    schema: &str,
    table: &str,
    name: &str,
) -> Result<Vec<TableForeignKey>, BackendError> {
    let sql = format!(
        "SELECT fk.name, fk.update_referential_action_desc, fk.delete_referential_action_desc, \
                ps.name, pt.name, pc.name, rc.name \
         FROM {fk} fk JOIN {fkc} fkc ON fkc.constraint_object_id = fk.object_id \
         JOIN {c} pc ON pc.object_id = fkc.parent_object_id AND pc.column_id = fkc.parent_column_id \
         JOIN {t} pt ON pt.object_id = fkc.parent_object_id \
         JOIN {s} ps ON ps.schema_id = pt.schema_id \
         JOIN {c} rc ON rc.object_id = fkc.referenced_object_id AND rc.column_id = fkc.referenced_column_id \
         WHERE fk.referenced_object_id = OBJECT_ID(@P1) \
         ORDER BY ps.name, pt.name, fk.name, fkc.constraint_column_id",
        fk = sys(database, "foreign_keys"),
        fkc = sys(database, "foreign_key_columns"),
        c = sys(database, "columns"),
        t = sys(database, "tables"),
        s = sys(database, "schemas"),
    );
    let mut keys: Vec<TableForeignKey> = Vec::new();
    for row in rows(client, Area::Describe, query(sql, &[name])).await? {
        let key_name = text(&row, 0);
        let owner = qualify(schema, &text(&row, 3), &text(&row, 4));
        match keys.last_mut() {
            Some(last) if last.name == key_name && last.table == owner => {
                last.columns.push(text(&row, 5));
                last.referenced_columns.push(text(&row, 6));
            }
            _ => keys.push(TableForeignKey {
                name: key_name,
                table: owner,
                columns: vec![text(&row, 5)],
                referenced_table: table.to_owned(),
                referenced_columns: vec![text(&row, 6)],
                on_update: Some(action(&text(&row, 1))),
                on_delete: Some(action(&text(&row, 2))),
                definition: None,
            }),
        }
    }
    Ok(keys)
}

/// `NO_ACTION` -> `NO ACTION`.
fn action(desc: &str) -> String {
    desc.replace('_', " ")
}

/// A table name, with its schema when that is not `current`.
fn qualify(current: &str, schema: &str, table: &str) -> String {
    if schema.eq_ignore_ascii_case(current) {
        table.to_owned()
    } else {
        format!("{schema}.{table}")
    }
}

fn push_reference(out: &mut String, current: &str, table: &str) {
    match table.split_once('.') {
        Some((schema, table)) => push_qualified(out, None, Some(schema), table),
        None => push_qualified(out, None, Some(current), table),
    }
}

fn fk_definition(schema: &str, key: &TableForeignKey) -> String {
    let list = |columns: &[String]| {
        columns
            .iter()
            .map(|column| {
                let mut out = String::new();
                push_ident(&mut out, column);
                out
            })
            .collect::<Vec<_>>()
            .join(", ")
    };
    let mut out = String::from("FOREIGN KEY (");
    out.push_str(&list(&key.columns));
    out.push_str(") REFERENCES ");
    push_reference(&mut out, schema, &key.referenced_table);
    let _ = write!(out, " ({})", list(&key.referenced_columns));
    if let Some(on_delete) = key.on_delete.as_deref().filter(|action| *action != "NO ACTION") {
        let _ = write!(out, " ON DELETE {on_delete}");
    }
    if let Some(on_update) = key.on_update.as_deref().filter(|action| *action != "NO ACTION") {
        let _ = write!(out, " ON UPDATE {on_update}");
    }
    out
}

#[allow(clippy::too_many_arguments)]
async fn compose_ddl(
    client: &mut Client,
    database: &str,
    schema: &str,
    table: &str,
    name: &str,
    columns: &[catalog::ColumnInfo],
    keys: &[TableConstraint],
    foreign_keys: &[TableForeignKey],
    index_statements: &[String],
) -> Result<String, BackendError> {
    // IDENTITY seeds and computed definitions.
    let sql = format!(
        "SELECT c.name, CAST(ic.seed_value AS nvarchar(40)), CAST(ic.increment_value AS nvarchar(40)) \
         FROM {ic} ic JOIN {c} c ON c.object_id = ic.object_id AND c.column_id = ic.column_id \
         WHERE ic.object_id = OBJECT_ID(@P1)",
        ic = sys(database, "identity_columns"),
        c = sys(database, "columns"),
    );
    let identities = rows(client, Area::Describe, query(sql, &[name])).await?;
    let sql = format!(
        "SELECT c.name, cc.definition, CAST(cc.is_persisted AS int) \
         FROM {cc} cc JOIN {c} c ON c.object_id = cc.object_id AND c.column_id = cc.column_id \
         WHERE cc.object_id = OBJECT_ID(@P1)",
        cc = sys(database, "computed_columns"),
        c = sys(database, "columns"),
    );
    let computed = rows(client, Area::Describe, query(sql, &[name])).await?;
    let mut lines: Vec<String> = Vec::new();
    for column in columns {
        let mut line = String::from("    ");
        push_ident(&mut line, &column.name);
        if let Some(row) = computed.iter().find(|row| text(row, 0) == column.name) {
            let _ = write!(line, " AS {}", text(row, 1));
            if flag(row, 2) {
                line.push_str(" PERSISTED");
            }
            lines.push(line);
            continue;
        }
        let _ = write!(line, " {}", column.data_type);
        if let Some(row) = identities.iter().find(|row| text(row, 0) == column.name) {
            let _ = write!(line, " IDENTITY({},{})", text(row, 1), text(row, 2));
        }
        line.push_str(if column.nullable { " NULL" } else { " NOT NULL" });
        if let Some(default) = &column.default {
            let _ = write!(line, " DEFAULT {default}");
        }
        lines.push(line);
    }
    for key in keys {
        let mut line = String::from("    CONSTRAINT ");
        push_ident(&mut line, &key.name);
        match key.kind {
            ConstraintKind::Check => {
                let _ = write!(line, " CHECK {}", key.definition.as_deref().unwrap_or("(1 = 1)"));
            }
            kind => {
                line.push_str(if kind == ConstraintKind::PrimaryKey { " PRIMARY KEY (" } else { " UNIQUE (" });
                let columns: Vec<String> = key
                    .columns
                    .iter()
                    .map(|column| {
                        let mut out = String::new();
                        push_ident(&mut out, column);
                        out
                    })
                    .collect();
                line.push_str(&columns.join(", "));
                line.push(')');
            }
        }
        lines.push(line);
    }
    for key in foreign_keys {
        let mut line = String::from("    CONSTRAINT ");
        push_ident(&mut line, &key.name);
        let _ = write!(line, " {}", key.definition.as_deref().unwrap_or(""));
        lines.push(line);
    }
    let mut ddl = String::from("CREATE TABLE ");
    push_qualified(&mut ddl, None, Some(schema), table);
    ddl.push_str(" (\n");
    ddl.push_str(&lines.join(",\n"));
    ddl.push_str("\n);");
    for statement in index_statements {
        let _ = write!(ddl, "\n\n{statement};");
    }
    Ok(ddl)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn foreign_key_definitions_name_the_other_schema_only_when_it_differs() {
        let key = TableForeignKey {
            name: "fk".into(),
            table: "refs".into(),
            columns: vec!["tenant".into(), "item_id".into()],
            referenced_table: "items".into(),
            referenced_columns: vec!["tenant".into(), "id".into()],
            on_update: Some("NO ACTION".into()),
            on_delete: Some("CASCADE".into()),
            definition: None,
        };
        assert_eq!(
            fk_definition("dbo", &key),
            "FOREIGN KEY ([tenant], [item_id]) REFERENCES [dbo].[items] ([tenant], [id]) ON DELETE CASCADE"
        );
        let other = TableForeignKey {
            referenced_table: "sales.items".into(),
            ..key
        };
        assert!(fk_definition("dbo", &other).contains("REFERENCES [sales].[items]"));
        assert_eq!(qualify("dbo", "DBO", "t"), "t");
        assert_eq!(qualify("dbo", "sales", "t"), "sales.t");
        assert_eq!(action("SET_NULL"), "SET NULL");
    }

    #[test]
    fn index_columns_keep_their_order() {
        assert_eq!(quote_with_order("a DESC"), "[a] DESC");
        assert_eq!(quote_with_order("b"), "[b]");
    }
}
