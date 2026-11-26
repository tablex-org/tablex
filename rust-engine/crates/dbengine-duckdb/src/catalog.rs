//! Navigator and `describe_object` for DuckDB, per `engine_driver_api::sql`:
//! databases (the attached catalogs) -> schemas -> tables and views -> columns.
//!
//! The level is chosen from the parent node's `payload` (the host echoes it back in
//! `NavigatorRequest.payload`; `NavigatorRequest.database` mirrors `payload.database`):
//!
//! | request                              | lists                              |
//! |--------------------------------------|------------------------------------|
//! | no `database`                        | databases                          |
//! | `database`                           | schemas                            |
//! | `database` + `schema`                | tables and views (`payload.filter`) |
//! | `database` + `schema` + `kind`       | nothing: DuckDB has no triggers, `materialized_view`s, ... here |
//! | `database` + `schema` + `object`     | columns of that table or view      |
//! | `database` + `schema` + `columns: true` | the columns of every table and view of the schema |
//!
//! `columns: true` (capability `metadata.schema_columns`) is one `duckdb_columns()` statement per
//! page, keyset-paged by `engine_driver_api::sql::schema_columns_cursor` (binary table name, then
//! `column_index`), with the same per-column details as the per-table request.
//!
//! Only the connection's current database is connectable: a statement names a table as
//! `schema.table`, which DuckDB resolves in the current database, so the other attached
//! databases are listed with `"can_connect": false` (the host shows them dimmed), and `USE name`
//! makes one current. Paging: `limit` (0 = 500, at most 5000) and a cursor that is the next offset.
//!
//! Everything runs on the driver's metadata connection and names are bound as parameters.

use duckdb::types::Value;
use duckdb::{params, Connection};
use engine_driver_kit::api::sql::{
    codes, parse_schema_columns_cursor, schema_columns_cursor, ConstraintKind, SchemaObjectKind, SqlColumnNode, SqlObjectRef, TableColumn,
    TableConstraint, TableForeignKey, TableIndex, TableProperties,
};
use engine_driver_kit::api::{NavigatorNode, NavigatorPage, NavigatorRequest};
use engine_driver_kit::BackendError;
use serde_json::{json, Value as Json};

const DEFAULT_LIMIT: u64 = 500;
const MAX_LIMIT: u64 = 5_000;

fn duck_error(error: duckdb::Error) -> BackendError {
    BackendError::new(crate::duck_failure(&error))
}

fn bad_request(message: impl Into<String>) -> BackendError {
    BackendError::with_code("navigator.bad_request", message)
}

fn payload_str<'a>(request: &'a NavigatorRequest, key: &str) -> Option<&'a str> {
    request
        .payload
        .as_ref()?
        .get(key)?
        .as_str()
        .filter(|value| !value.is_empty())
}

pub(crate) fn list(
    conn: &Connection,
    request: &NavigatorRequest,
) -> Result<NavigatorPage, BackendError> {
    let database = request
        .database
        .as_deref()
        .filter(|database| !database.is_empty())
        .or_else(|| payload_str(request, "database"))
        .or_else(|| {
            request
                .parent
                .as_deref()
                .and_then(|parent| parent.strip_prefix("database:"))
        });
    let schema = payload_str(request, "schema");
    let object = payload_str(request, "object");
    if let (Some(database), Some(schema), None) = (database, schema, object) {
        if request.payload.as_ref().and_then(|payload| payload.get("columns")) == Some(&Json::Bool(true)) {
            return schema_columns(conn, database, schema, request);
        }
    }
    let (offset, limit) = window(request)?;
    match (database, schema, object) {
        (None, None, None) => databases(conn, offset, limit),
        (Some(database), None, _) => schemas(conn, database, offset, limit),
        (Some(database), Some(schema), None) => {
            let kind = match payload_str(request, "kind") {
                None => None,
                Some(name) => Some(SchemaObjectKind::parse(name).ok_or_else(|| {
                    bad_request(format!("unknown object kind `{name}`"))
                })?),
            };
            if kind.is_some() {
                return Ok(NavigatorPage {
                    nodes: Vec::new(),
                    next_cursor: None,
                });
            }
            objects(
                conn,
                database,
                schema,
                payload_str(request, "filter"),
                offset,
                limit,
            )
        }
        (Some(database), Some(schema), Some(object)) => {
            columns(conn, database, schema, object, offset, limit)
        }
        _ => Err(bad_request("a schema or object parent needs its database")),
    }
}

/// `(offset, limit)` of the requested page.
fn window(request: &NavigatorRequest) -> Result<(usize, usize), BackendError> {
    let offset = match request.cursor.as_deref().filter(|cursor| !cursor.is_empty()) {
        None => 0,
        Some(cursor) => cursor
            .parse::<usize>()
            .map_err(|_| bad_request("invalid navigator cursor"))?,
    };
    let limit = match request.limit {
        0 => DEFAULT_LIMIT,
        limit => limit.min(MAX_LIMIT),
    };
    Ok((offset, limit as usize))
}

/// Slices a fully loaded list into one page.
fn page_of<T>(items: Vec<T>, offset: usize, limit: usize) -> (Vec<T>, Option<String>) {
    let total = items.len();
    let page: Vec<T> = items.into_iter().skip(offset).take(limit).collect();
    let next = (offset + page.len() < total).then(|| (offset + page.len()).to_string());
    (page, next)
}

fn databases(
    conn: &Connection,
    offset: usize,
    limit: usize,
) -> Result<NavigatorPage, BackendError> {
    let current: String = conn
        .query_row("SELECT current_database()", [], |row| row.get(0))
        .map_err(duck_error)?;
    let names = conn
        .prepare("SELECT database_name FROM duckdb_databases() WHERE NOT internal ORDER BY database_name")
        .and_then(|mut statement| {
            statement
                .query_map([], |row| row.get::<_, String>(0))?
                .collect::<Result<Vec<_>, _>>()
        })
        .map_err(duck_error)?;
    let (names, next_cursor) = page_of(names, offset, limit);
    Ok(NavigatorPage {
        nodes: names
            .iter()
            .map(|name| database_node(name, *name == current))
            .collect(),
        next_cursor,
    })
}

fn schemas(
    conn: &Connection,
    database: &str,
    offset: usize,
    limit: usize,
) -> Result<NavigatorPage, BackendError> {
    // `main` is flagged internal by DuckDB although it holds the user's tables.
    let names = conn
        .prepare(
            "SELECT schema_name FROM duckdb_schemas() \
             WHERE database_name = ? AND (NOT internal OR schema_name = 'main') ORDER BY schema_name",
        )
        .and_then(|mut statement| {
            statement
                .query_map([database], |row| row.get::<_, String>(0))?
                .collect::<Result<Vec<_>, _>>()
        })
        .map_err(duck_error)?;
    let (names, next_cursor) = page_of(names, offset, limit);
    Ok(NavigatorPage {
        nodes: names.iter().map(|name| schema_node(database, name)).collect(),
        next_cursor,
    })
}

fn objects(
    conn: &Connection,
    database: &str,
    schema: &str,
    filter: Option<&str>,
    offset: usize,
    limit: usize,
) -> Result<NavigatorPage, BackendError> {
    // The filter is a literal, case-insensitive substring (`contains`, not `LIKE`: `_` and `%`
    // are ordinary characters). One extra row tells whether another page exists.
    let found = conn
        .prepare_cached(
            "SELECT name, kind FROM ( \
               SELECT table_name AS name, 'table' AS kind FROM duckdb_tables() \
                 WHERE database_name = $1 AND schema_name = $2 AND NOT internal \
               UNION ALL \
               SELECT view_name, 'view' FROM duckdb_views() \
                 WHERE database_name = $1 AND schema_name = $2 AND NOT internal) \
             WHERE $3::VARCHAR IS NULL OR contains(lower(name), lower($3::VARCHAR)) \
             ORDER BY name, kind LIMIT $4 OFFSET $5",
        )
        .and_then(|mut statement| {
            statement
                .query_map(
                    params![database, schema, filter, (limit + 1) as i64, offset as i64],
                    |row| Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?)),
                )?
                .collect::<Result<Vec<_>, _>>()
        })
        .map_err(duck_error)?;
    let next_cursor = (found.len() > limit).then(|| (offset + limit).to_string());
    Ok(NavigatorPage {
        nodes: found
            .into_iter()
            .take(limit)
            .map(|(name, kind)| object_node(database, schema, &kind, &name))
            .collect(),
        next_cursor,
    })
}

/// `can_connect` is in the payload only when it is `false`.
fn database_node(name: &str, current: bool) -> NavigatorNode {
    let mut payload = json!({ "database": name });
    if !current {
        payload["can_connect"] = json!(false);
    }
    NavigatorNode {
        id: format!("database:{name}"),
        kind: "database".into(),
        label: name.into(),
        subtitle: None,
        icon: Some("database".into()),
        has_children: true,
        payload,
        actions: vec!["refresh".into()],
    }
}

fn schema_node(database: &str, schema: &str) -> NavigatorNode {
    NavigatorNode {
        id: format!("schema:{database}:{schema}"),
        kind: "schema".into(),
        label: schema.into(),
        subtitle: None,
        icon: Some("schema".into()),
        has_children: true,
        payload: json!({ "database": database, "schema": schema }),
        actions: vec!["refresh".into()],
    }
}

fn object_ref(database: &str, schema: &str, object: &str) -> SqlObjectRef {
    SqlObjectRef {
        database: Some(database.to_owned()),
        schema: Some(schema.to_owned()),
        object: object.to_owned(),
    }
}

fn object_node(database: &str, schema: &str, kind: &str, name: &str) -> NavigatorNode {
    let kind = if kind == "view" { "view" } else { "table" };
    NavigatorNode {
        id: format!("{kind}:{database}:{schema}:{name}"),
        kind: kind.into(),
        label: name.into(),
        subtitle: None,
        icon: Some(kind.into()),
        has_children: true,
        payload: serde_json::to_value(object_ref(database, schema, name)).unwrap_or(Json::Null),
        actions: vec!["open_query".into(), "describe".into(), "refresh".into()],
    }
}

// --------------------------------------------------------------------------------------- columns

/// The columns of a table or view, in order; empty when there is no such object.
fn column_details(
    conn: &Connection,
    database: &str,
    schema: &str,
    object: &str,
) -> Result<Vec<TableColumn>, BackendError> {
    let primary_key = primary_key_columns(conn, database, schema, object)?;
    conn.prepare_cached(
        "SELECT column_name, data_type, is_nullable, column_default, comment, is_generated \
         FROM duckdb_columns() \
         WHERE database_name = ? AND schema_name = ? AND table_name = ? ORDER BY column_index",
    )
    .and_then(|mut statement| {
        statement
            .query_map(params![database, schema, object], |row| {
                let name: String = row.get(0)?;
                Ok(TableColumn {
                    primary_key: primary_key.contains(&name),
                    name,
                    data_type: row.get(1)?,
                    nullable: row.get(2)?,
                    is_generated: row.get::<_, bool>(5)?.then_some(true),
                    default_expression: row.get(3)?,
                    // A DuckDB default `nextval(..)` is a plain default.
                    auto_increment: None,
                    comment: row.get(4)?,
                    attributes: None,
                })
            })?
            .collect::<Result<Vec<_>, _>>()
    })
    .map_err(duck_error)
}

fn columns(
    conn: &Connection,
    database: &str,
    schema: &str,
    object: &str,
    offset: usize,
    limit: usize,
) -> Result<NavigatorPage, BackendError> {
    let details = column_details(conn, database, schema, object)?;
    if details.is_empty() {
        return Err(not_found(database, schema, object));
    }
    let (details, next_cursor) = page_of(details, offset, limit);
    let nodes = details
        .into_iter()
        .map(|details| column_node(database, schema, object, details))
        .collect();
    Ok(NavigatorPage { nodes, next_cursor })
}

fn column_node(database: &str, schema: &str, object: &str, details: TableColumn) -> NavigatorNode {
    let payload = SqlColumnNode {
        object: object_ref(database, schema, object),
        column: details.name.clone(),
        details: details.clone(),
    };
    NavigatorNode {
        id: format!("column:{database}:{schema}:{object}:{}", details.name),
        kind: "column".into(),
        icon: Some(if details.primary_key { "key" } else { "column" }.into()),
        label: details.name,
        subtitle: Some(details.data_type),
        has_children: false,
        payload: serde_json::to_value(&payload).unwrap_or(Json::Null),
        actions: Vec::new(),
    }
}

/// One page of the schema's columns (`payload.columns == true`): at most `limit` columns of the
/// tables and views, by table name then `column_index`, after the column the cursor names. A
/// column is a primary key column when `duckdb_constraints()` lists it, as in [`column_details`].
fn schema_columns(
    conn: &Connection,
    database: &str,
    schema: &str,
    request: &NavigatorRequest,
) -> Result<NavigatorPage, BackendError> {
    let limit = match request.limit {
        0 => DEFAULT_LIMIT,
        limit => limit.min(MAX_LIMIT),
    } as usize;
    let (after_table, after_index) = match request.cursor.as_deref().filter(|cursor| !cursor.is_empty()) {
        None => (None, 0),
        Some(cursor) => match parse_schema_columns_cursor(cursor) {
            Some((table, index)) => (Some(table), index),
            None => return Err(bad_request("invalid navigator cursor")),
        },
    };
    let mut rows = conn
        .prepare_cached(
            "SELECT c.table_name, c.column_index, c.column_name, c.data_type, c.is_nullable, \
                    c.column_default, c.comment, c.is_generated, \
                    EXISTS (SELECT 1 FROM duckdb_constraints() k, unnest(k.constraint_column_names) AS pk(name) \
                            WHERE k.database_name = c.database_name AND k.schema_name = c.schema_name \
                              AND k.table_name = c.table_name AND k.constraint_type = 'PRIMARY KEY' \
                              AND pk.name = c.column_name) \
             FROM duckdb_columns() c \
             WHERE c.database_name = $1 AND c.schema_name = $2 AND NOT c.internal \
               AND ($3::VARCHAR IS NULL OR c.table_name > $3::VARCHAR \
                    OR (c.table_name = $3::VARCHAR AND c.column_index > $4::BIGINT)) \
             ORDER BY c.table_name, c.column_index LIMIT $5",
        )
        .and_then(|mut statement| {
            statement
                .query_map(
                    params![database, schema, after_table, after_index, (limit + 1) as i64],
                    |row| {
                        let name: String = row.get(2)?;
                        Ok((
                            row.get::<_, String>(0)?,
                            i64::from(row.get::<_, i32>(1)?),
                            TableColumn {
                                name,
                                data_type: row.get(3)?,
                                nullable: row.get(4)?,
                                primary_key: row.get(8)?,
                                is_generated: row.get::<_, bool>(7)?.then_some(true),
                                default_expression: row.get(5)?,
                                auto_increment: None,
                                comment: row.get(6)?,
                                attributes: None,
                            },
                        ))
                    },
                )?
                .collect::<Result<Vec<_>, _>>()
        })
        .map_err(duck_error)?;
    let next_cursor = (rows.len() > limit).then(|| {
        rows.truncate(limit);
        let (table, index, _) = &rows[limit - 1];
        schema_columns_cursor(table, *index)
    });
    Ok(NavigatorPage {
        nodes: rows
            .into_iter()
            .map(|(table, _, details)| column_node(database, schema, &table, details))
            .collect(),
        next_cursor,
    })
}

fn not_found(database: &str, schema: &str, object: &str) -> BackendError {
    BackendError::with_code(
        codes::DESCRIBE_NOT_FOUND,
        format!("no table or view named `{database}.{schema}.{object}`"),
    )
}

/// The text of a `VARCHAR[]` cell.
fn text_list(value: Value) -> Vec<String> {
    match value {
        Value::List(items) | Value::Array(items) => items
            .into_iter()
            .filter_map(|item| match item {
                Value::Text(text) => Some(text),
                _ => None,
            })
            .collect(),
        _ => Vec::new(),
    }
}

fn primary_key_columns(
    conn: &Connection,
    database: &str,
    schema: &str,
    object: &str,
) -> Result<Vec<String>, BackendError> {
    conn.prepare_cached(
        "SELECT constraint_column_names FROM duckdb_constraints() \
         WHERE database_name = ? AND schema_name = ? AND table_name = ? \
           AND constraint_type = 'PRIMARY KEY'",
    )
    .and_then(|mut statement| {
        let rows = statement
            .query_map(params![database, schema, object], |row| {
                row.get::<_, Value>(0)
            })?
            .collect::<Result<Vec<_>, _>>()?;
        Ok(rows.into_iter().flat_map(text_list).collect())
    })
    .map_err(duck_error)
}

// -------------------------------------------------------------------------------------- describe

/// `describe_object`: a [`SqlObjectRef`] answers a [`TableProperties`]. A `SchemaObjectRef`
/// (a request with a `kind`) is for objects DuckDB's navigator does not list.
pub(crate) fn describe(conn: &Connection, request: &Json) -> Result<Json, BackendError> {
    if request.get("kind").is_some() {
        return Err(BackendError::with_code(
            "describe.bad_request",
            "DuckDB lists only tables and views",
        ));
    }
    let object: SqlObjectRef = serde_json::from_value(request.clone()).map_err(|error| {
        BackendError::with_code(
            "describe.bad_request",
            format!("request is not a SqlObjectRef: {error}"),
        )
    })?;
    let (current_database, current_schema): (String, String) = conn
        .query_row("SELECT current_database(), current_schema()", [], |row| {
            Ok((row.get(0)?, row.get(1)?))
        })
        .map_err(duck_error)?;
    let database = object.database.unwrap_or(current_database);
    let schema = object.schema.unwrap_or(current_schema);
    let name = object.object;
    let columns = column_details(conn, &database, &schema, &name)?;
    if columns.is_empty() {
        return Err(not_found(&database, &schema, &name));
    }
    let (keys, foreign_keys) = constraints(conn, &database, &schema, &name)?;
    let properties = TableProperties {
        columns,
        keys: Some(keys),
        foreign_keys: Some(foreign_keys),
        indexes: Some(indexes(conn, &database, &schema, &name)?),
        references: Some(references(conn, &database, &schema, &name)?),
        triggers: None,
        ddl: ddl(conn, &database, &schema, &name)?,
    };
    serde_json::to_value(&properties).map_err(|error| BackendError::new(error.to_string()))
}

fn constraints(
    conn: &Connection,
    database: &str,
    schema: &str,
    table: &str,
) -> Result<(Vec<TableConstraint>, Vec<TableForeignKey>), BackendError> {
    let rows = conn
        .prepare_cached(
            "SELECT constraint_type, constraint_name, constraint_column_names, constraint_text, \
                    referenced_table, referenced_column_names \
             FROM duckdb_constraints() \
             WHERE database_name = ? AND schema_name = ? AND table_name = ? \
               AND constraint_type IN ('PRIMARY KEY', 'UNIQUE', 'CHECK', 'FOREIGN KEY') \
             ORDER BY constraint_index",
        )
        .and_then(|mut statement| {
            statement
                .query_map(params![database, schema, table], |row| {
                    Ok((
                        row.get::<_, String>(0)?,
                        row.get::<_, String>(1)?,
                        row.get::<_, Value>(2)?,
                        row.get::<_, String>(3)?,
                        row.get::<_, Option<String>>(4)?,
                        row.get::<_, Value>(5)?,
                    ))
                })?
                .collect::<Result<Vec<_>, _>>()
        })
        .map_err(duck_error)?;
    let mut keys = Vec::new();
    let mut foreign_keys = Vec::new();
    for (kind, name, columns, text, referenced_table, referenced_columns) in rows {
        let columns = text_list(columns);
        match (kind.as_str(), referenced_table) {
            ("FOREIGN KEY", Some(referenced_table)) => foreign_keys.push(TableForeignKey {
                name,
                table: table.to_owned(),
                columns,
                referenced_table,
                referenced_columns: text_list(referenced_columns),
                on_update: None,
                on_delete: None,
                definition: Some(text),
            }),
            (kind, _) => keys.push(TableConstraint {
                name,
                kind: match kind {
                    "PRIMARY KEY" => ConstraintKind::PrimaryKey,
                    "UNIQUE" => ConstraintKind::Unique,
                    "CHECK" => ConstraintKind::Check,
                    _ => ConstraintKind::Other,
                },
                columns,
                definition: Some(text),
            }),
        }
    }
    Ok((keys, foreign_keys))
}

/// Foreign keys of other tables of the schema that point at `table`.
fn references(
    conn: &Connection,
    database: &str,
    schema: &str,
    table: &str,
) -> Result<Vec<TableForeignKey>, BackendError> {
    conn.prepare_cached(
        "SELECT constraint_name, table_name, constraint_column_names, referenced_column_names, \
                constraint_text \
         FROM duckdb_constraints() \
         WHERE database_name = ? AND schema_name = ? AND referenced_table = ? \
           AND constraint_type = 'FOREIGN KEY' ORDER BY table_name, constraint_index",
    )
    .and_then(|mut statement| {
        statement
            .query_map(params![database, schema, table], |row| {
                Ok(TableForeignKey {
                    name: row.get(0)?,
                    table: row.get(1)?,
                    columns: text_list(row.get(2)?),
                    referenced_table: table.to_owned(),
                    referenced_columns: text_list(row.get(3)?),
                    on_update: None,
                    on_delete: None,
                    definition: Some(row.get(4)?),
                })
            })?
            .collect::<Result<Vec<_>, _>>()
    })
    .map_err(duck_error)
}

fn indexes(
    conn: &Connection,
    database: &str,
    schema: &str,
    table: &str,
) -> Result<Vec<TableIndex>, BackendError> {
    conn.prepare_cached(
        "SELECT index_name, is_unique, is_primary, expressions, sql FROM duckdb_indexes() \
         WHERE database_name = ? AND schema_name = ? AND table_name = ? ORDER BY index_name",
    )
    .and_then(|mut statement| {
        statement
            .query_map(params![database, schema, table], |row| {
                Ok(TableIndex {
                    name: row.get(0)?,
                    unique: row.get(1)?,
                    primary: row.get(2)?,
                    columns: expression_names(&row.get::<_, Option<String>>(3)?.unwrap_or_default()),
                    method: Some("art".into()),
                    definition: row.get(4)?,
                })
            })?
            .collect::<Result<Vec<_>, _>>()
    })
    .map_err(duck_error)
}

/// The names in the text DuckDB prints for an index's expressions, `['"a"', '"b"']`: each one
/// without the list's quotes and, when it is a plain column, without its double quotes.
fn expression_names(text: &str) -> Vec<String> {
    let inner = text.trim().trim_start_matches('[').trim_end_matches(']');
    if inner.is_empty() {
        return Vec::new();
    }
    inner
        .split("', '")
        .map(|part| {
            let part = part.trim_matches('\'');
            part.strip_prefix('"')
                .and_then(|name| name.strip_suffix('"'))
                .filter(|name| !name.contains('"'))
                .unwrap_or(part)
                .to_owned()
        })
        .collect()
}

/// The statement that creates the table or view, as DuckDB prints it.
fn ddl(
    conn: &Connection,
    database: &str,
    schema: &str,
    object: &str,
) -> Result<Option<String>, BackendError> {
    conn.prepare_cached(
        "SELECT sql FROM duckdb_tables() \
           WHERE database_name = $1 AND schema_name = $2 AND table_name = $3 \
         UNION ALL \
         SELECT sql FROM duckdb_views() \
           WHERE database_name = $1 AND schema_name = $2 AND view_name = $3",
    )
    .and_then(|mut statement| {
        statement
            .query_map(params![database, schema, object], |row| {
                row.get::<_, Option<String>>(0)
            })?
            .next()
            .transpose()
            .map(Option::flatten)
    })
    .map_err(duck_error)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn index_expressions_read_as_column_names() {
        assert_eq!(expression_names("['\"name\"']"), ["name"]);
        assert_eq!(expression_names("['\"a\"', '\"b\"']"), ["a", "b"]);
        assert_eq!(expression_names("['lower(\"a\")']"), ["lower(\"a\")"]);
        assert!(expression_names("[]").is_empty());
    }

    fn request(payload: Json, cursor: Option<&str>, limit: u64) -> NavigatorRequest {
        NavigatorRequest {
            parent: None,
            database: payload["database"].as_str().map(str::to_owned),
            payload: Some(payload),
            cursor: cursor.map(str::to_owned),
            limit,
        }
    }

    /// The schema-wide listing gives each table what the per-table listing gives it (keys,
    /// defaults, comments, generated columns, views), at page sizes that cut a table in two.
    #[test]
    fn schema_columns_equal_the_per_table_columns_at_every_page_size() {
        let conn = Connection::open_in_memory().unwrap();
        conn.execute_batch(
            "CREATE TABLE items (tenant INTEGER NOT NULL, id INTEGER NOT NULL, name VARCHAR DEFAULT 'x', \
                                 qty INTEGER, PRIMARY KEY (tenant, id));
             CREATE TABLE gen (a INTEGER PRIMARY KEY, b INTEGER GENERATED ALWAYS AS (a * 2) VIRTUAL);
             CREATE TABLE \"Odd:Name\" (x INTEGER);
             CREATE VIEW v_items AS SELECT tenant, id FROM items;
             CREATE SCHEMA other;
             CREATE TABLE other.elsewhere (z INTEGER);
             COMMENT ON COLUMN items.qty IS 'how many';",
        )
        .unwrap();
        let database: String = conn.query_row("SELECT current_database()", [], |row| row.get(0)).unwrap();
        let mut expected = Vec::new();
        for table in ["Odd:Name", "gen", "items", "v_items"] {
            let page = columns(&conn, &database, "main", table, 0, 100).unwrap();
            expected.extend(page.nodes.into_iter().map(|node| (table.to_owned(), node.payload)));
        }
        assert_eq!(expected.len(), 1 + 2 + 4 + 2);
        for limit in [1, 2, 3, 0] {
            let mut got = Vec::new();
            let mut cursor: Option<String> = None;
            loop {
                let page = list(
                    &conn,
                    &request(json!({"database": database, "schema": "main", "columns": true}), cursor.as_deref(), limit),
                )
                .unwrap();
                got.extend(page.nodes.into_iter().map(|node| {
                    let column: SqlColumnNode = serde_json::from_value(node.payload.clone()).unwrap();
                    (column.object.object, node.payload)
                }));
                cursor = page.next_cursor;
                if cursor.is_none() {
                    break;
                }
            }
            assert_eq!(got, expected, "limit {limit}");
        }
        let bad = list(
            &conn,
            &request(json!({"database": database, "schema": "main", "columns": true}), Some("x"), 0),
        )
        .unwrap_err();
        assert_eq!(bad.code.as_deref(), Some("navigator.bad_request"));
    }
}
