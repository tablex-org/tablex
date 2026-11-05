//! Navigator and `describe_object` for SQLite, per `engine_driver_api::sql`.
//!
//! Everything here runs on a [`Connection`] chosen by the caller (the driver's
//! separate read-only metadata connection when the database is a file, see
//! `SqliteConnection::with_metadata`). Object names are always bound as
//! parameters (`pragma_*` table-valued functions), never interpolated.

use engine_driver_kit::api::sql::{
    codes, ConstraintKind, SchemaObjectKind, SchemaObjectRef, SchemaObjectSource, SqlColumnNode,
    SqlObjectRef, TableColumn, TableConstraint, TableForeignKey, TableIndex, TableProperties,
    TableTrigger,
};
use engine_driver_kit::api::{NavigatorNode, NavigatorPage, NavigatorRequest};
use engine_driver_kit::BackendError;
use rusqlite::{params, Connection};

pub(crate) const MAIN: &str = "main";
const DEFAULT_LIMIT: u64 = 500;
const MAX_LIMIT: u64 = 5_000;

fn sqlite_error(error: rusqlite::Error) -> BackendError {
    BackendError::new(error.to_string())
}

pub(crate) fn database_node() -> NavigatorNode {
    NavigatorNode {
        id: format!("database:{MAIN}"),
        kind: "database".into(),
        label: MAIN.into(),
        subtitle: None,
        icon: Some("database".into()),
        has_children: true,
        payload: serde_json::json!({ "database": MAIN }),
        actions: vec!["refresh".into()],
    }
}

/// `root` -> `main` database; `database:main` -> tables and views (`payload.filter`,
/// `limit`, `cursor`); a table-like payload (`object`) -> its columns. `payload.kind`
/// lists one `SchemaObjectKind` instead: SQLite has triggers only, every other kind is
/// an empty page and an unknown one `navigator.bad_request`.
pub(crate) fn list(
    conn: &Connection,
    request: &NavigatorRequest,
) -> Result<NavigatorPage, BackendError> {
    let payload = request.payload.as_ref();
    if let Some(object) = payload
        .and_then(|payload| payload.get("object"))
        .and_then(serde_json::Value::as_str)
    {
        return columns(conn, object);
    }
    let parent = request.parent.as_deref().unwrap_or_default();
    let database = request
        .database
        .as_deref()
        .filter(|database| !database.is_empty());
    if parent.is_empty() && database.is_none() {
        return Ok(NavigatorPage {
            nodes: vec![database_node()],
            next_cursor: None,
        });
    }
    if let Some(database) = database {
        require_main(database)
            .map_err(|message| BackendError::with_code("navigator.bad_request", message))?;
    }
    if let Some(table) = ["table:", "view:"]
        .iter()
        .find_map(|prefix| parent.strip_prefix(prefix))
        .and_then(|rest| rest.split_once(':').map(|(_, name)| name))
    {
        return columns(conn, table);
    }
    let filter = payload
        .and_then(|payload| payload.get("filter"))
        .and_then(serde_json::Value::as_str)
        .filter(|filter| !filter.is_empty());
    let kind = payload
        .and_then(|payload| payload.get("kind"))
        .and_then(serde_json::Value::as_str)
        .filter(|kind| !kind.is_empty());
    match kind.map(|name| (name, SchemaObjectKind::parse(name))) {
        None => tables(conn, filter, request.cursor.as_deref(), request.limit),
        Some((_, Some(SchemaObjectKind::Trigger))) => {
            trigger_page(conn, filter, request.cursor.as_deref(), request.limit)
        }
        Some((_, Some(_))) => Ok(NavigatorPage { nodes: Vec::new(), next_cursor: None }),
        Some((name, None)) => Err(BackendError::with_code(
            "navigator.bad_request",
            format!("unknown object kind `{name}`"),
        )),
    }
}

/// One page of the database's triggers, ordered by name (a trigger name is unique in
/// the database), with the same keyset cursor and literal filter as [`tables`].
fn trigger_page(
    conn: &Connection,
    filter: Option<&str>,
    cursor: Option<&str>,
    limit: u64,
) -> Result<NavigatorPage, BackendError> {
    let limit = match limit {
        0 => DEFAULT_LIMIT,
        limit => limit.min(MAX_LIMIT),
    };
    let mut statement = conn
        .prepare_cached(
            "SELECT name, tbl_name FROM sqlite_schema WHERE type = 'trigger' \
               AND (?1 IS NULL OR instr(lower(name), lower(?1)) > 0) \
               AND (?2 IS NULL OR name > ?2) \
             ORDER BY name LIMIT ?3",
        )
        .map_err(sqlite_error)?;
    let rows = statement
        .query_map(
            params![filter, cursor, i64::try_from(limit + 1).unwrap_or(i64::MAX)],
            |row| Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?)),
        )
        .map_err(sqlite_error)?
        .collect::<Result<Vec<_>, _>>()
        .map_err(sqlite_error)?;
    let more = rows.len() as u64 > limit;
    let nodes: Vec<NavigatorNode> = rows
        .into_iter()
        .take(limit as usize)
        .map(|(name, table)| trigger_node(name, table))
        .collect();
    let next_cursor = more
        .then(|| nodes.last().and_then(|node| node.payload["name"].as_str().map(str::to_owned)))
        .flatten();
    Ok(NavigatorPage { nodes, next_cursor })
}

fn trigger_node(name: String, table: String) -> NavigatorNode {
    let label = format!("{name} on {table}");
    let reference = SchemaObjectRef {
        database: Some(MAIN.into()),
        schema: None,
        kind: SchemaObjectKind::Trigger,
        name,
        arguments: None,
        table: Some(table),
        identity: None,
    };
    NavigatorNode {
        id: format!("trigger:{MAIN}:{label}"),
        kind: "trigger".into(),
        label,
        subtitle: None,
        icon: Some("trigger".into()),
        has_children: false,
        payload: serde_json::to_value(&reference).unwrap_or_default(),
        actions: vec!["view_source".into()],
    }
}

fn require_main(database: &str) -> Result<(), String> {
    if database.eq_ignore_ascii_case(MAIN) {
        Ok(())
    } else {
        Err(format!(
            "SQLite exposes only the `{MAIN}` database, not `{database}`"
        ))
    }
}

fn tables(
    conn: &Connection,
    filter: Option<&str>,
    cursor: Option<&str>,
    limit: u64,
) -> Result<NavigatorPage, BackendError> {
    let limit = match limit {
        0 => DEFAULT_LIMIT,
        limit => limit.min(MAX_LIMIT),
    };
    // Keyset cursor on the (binary-collated) name; one lookahead row makes
    // `next_cursor` exact. The filter is a literal, ASCII-case-insensitive
    // substring (`instr`, not `LIKE`: `_` and `%` are ordinary characters).
    let mut statement = conn
        .prepare_cached(
            "SELECT name, type FROM sqlite_schema \
             WHERE type IN ('table', 'view') AND name NOT LIKE 'sqlite\\_%' ESCAPE '\\' \
               AND (?1 IS NULL OR instr(lower(name), lower(?1)) > 0) \
               AND (?2 IS NULL OR name > ?2) \
             ORDER BY name LIMIT ?3",
        )
        .map_err(sqlite_error)?;
    let rows = statement
        .query_map(
            params![filter, cursor, i64::try_from(limit + 1).unwrap_or(i64::MAX)],
            |row| Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?)),
        )
        .map_err(sqlite_error)?
        .collect::<Result<Vec<_>, _>>()
        .map_err(sqlite_error)?;
    let more = rows.len() as u64 > limit;
    let nodes: Vec<NavigatorNode> = rows
        .into_iter()
        .take(limit as usize)
        .map(|(name, kind)| table_node(name, &kind))
        .collect();
    let next_cursor = more
        .then(|| nodes.last().map(|node| node.label.clone()))
        .flatten();
    Ok(NavigatorPage { nodes, next_cursor })
}

fn table_node(name: String, kind: &str) -> NavigatorNode {
    let kind = if kind == "view" { "view" } else { "table" };
    let object = object_ref(&name);
    NavigatorNode {
        id: format!("{kind}:{MAIN}:{name}"),
        kind: kind.into(),
        label: name,
        subtitle: None,
        icon: Some(kind.into()),
        has_children: true,
        payload: serde_json::to_value(&object).unwrap_or_default(),
        actions: vec!["open_query".into(), "describe".into()],
    }
}

fn object_ref(name: &str) -> SqlObjectRef {
    SqlObjectRef {
        database: Some(MAIN.into()),
        schema: None,
        object: name.to_owned(),
    }
}

struct ColumnInfo {
    column: TableColumn,
    /// Position in the primary key (1-based), 0 when not part of it.
    pk_index: i64,
}

/// Columns of a table or view (`table_xinfo`: generated columns included,
/// hidden virtual-table columns excluded).
fn column_infos(conn: &Connection, object: &str) -> Result<Vec<ColumnInfo>, BackendError> {
    let without_rowid = table_sql(conn, object)?
        .is_some_and(|sql| sql.to_ascii_lowercase().contains("without rowid"));
    let mut statement = conn
        .prepare_cached(
            "SELECT name, type, \"notnull\", dflt_value, pk, hidden \
             FROM pragma_table_xinfo(?1) WHERE hidden != 1 ORDER BY cid",
        )
        .map_err(sqlite_error)?;
    let raw = statement
        .query_map([object], |row| {
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, String>(1)?,
                row.get::<_, i64>(2)?,
                row.get::<_, Option<String>>(3)?,
                row.get::<_, i64>(4)?,
                row.get::<_, i64>(5)?,
            ))
        })
        .map_err(sqlite_error)?
        .collect::<Result<Vec<_>, _>>()
        .map_err(sqlite_error)?;
    let pk_columns = raw.iter().filter(|column| column.4 > 0).count();
    Ok(raw
        .into_iter()
        .map(|(name, data_type, not_null, default, pk, hidden)| {
            // `INTEGER PRIMARY KEY` aliases the rowid: never NULL, auto-assigned.
            let rowid_alias = !without_rowid
                && pk_columns == 1
                && pk > 0
                && data_type.eq_ignore_ascii_case("integer");
            let implicitly_not_null = pk > 0 && (rowid_alias || without_rowid);
            ColumnInfo {
                pk_index: pk,
                column: TableColumn {
                    name,
                    data_type,
                    nullable: not_null == 0 && !implicitly_not_null,
                    primary_key: pk > 0,
                    is_generated: (hidden >= 2).then_some(true),
                    default_expression: default,
                    auto_increment: rowid_alias.then_some(true),
                    // SQLite has no column comments.
                    comment: None,
                    attributes: None,
                },
            }
        })
        .collect())
}

fn columns(conn: &Connection, object: &str) -> Result<NavigatorPage, BackendError> {
    let infos = column_infos(conn, object)?;
    if infos.is_empty() {
        return Err(BackendError::with_code(
            codes::DESCRIBE_NOT_FOUND,
            format!("no table or view named `{object}`"),
        ));
    }
    let nodes = infos
        .into_iter()
        .map(|info| {
            let payload = SqlColumnNode {
                object: object_ref(object),
                column: info.column.name.clone(),
                details: info.column.clone(),
            };
            NavigatorNode {
                id: format!("column:{MAIN}:{object}:{}", info.column.name),
                kind: "column".into(),
                label: info.column.name,
                subtitle: Some(info.column.data_type),
                icon: Some("column".into()),
                has_children: false,
                payload: serde_json::to_value(&payload).unwrap_or_default(),
                actions: Vec::new(),
            }
        })
        .collect();
    Ok(NavigatorPage {
        nodes,
        next_cursor: None,
    })
}

/// `sql` of the table or view `name` in `sqlite_schema` (case-insensitive name).
fn table_sql(conn: &Connection, name: &str) -> Result<Option<String>, BackendError> {
    let mut statement = conn
        .prepare_cached(
            "SELECT sql FROM sqlite_schema WHERE type IN ('table', 'view') AND name = ?1 COLLATE NOCASE",
        )
        .map_err(sqlite_error)?;
    let mut rows = statement.query([name]).map_err(sqlite_error)?;
    match rows.next().map_err(sqlite_error)? {
        Some(row) => Ok(Some(
            row.get::<_, Option<String>>(0)
                .map_err(sqlite_error)?
                .unwrap_or_default(),
        )),
        None => Ok(None),
    }
}

/// The catalog's own spelling of an object name, if it exists.
fn canonical_name(conn: &Connection, name: &str) -> Result<Option<String>, BackendError> {
    let mut statement = conn
        .prepare_cached(
            "SELECT name FROM sqlite_schema WHERE type IN ('table', 'view') AND name = ?1 COLLATE NOCASE",
        )
        .map_err(sqlite_error)?;
    let mut rows = statement.query([name]).map_err(sqlite_error)?;
    match rows.next().map_err(sqlite_error)? {
        Some(row) => Ok(Some(row.get(0).map_err(sqlite_error)?)),
        None => Ok(None),
    }
}

/// `describe_object`: columns, primary/unique keys, foreign keys (and the ones
/// pointing here), indexes, triggers and DDL, all from the catalog. `CHECK`
/// constraints are only part of the DDL text (SQLite does not expose them).
pub(crate) fn describe(
    conn: &Connection,
    request: &serde_json::Value,
) -> Result<serde_json::Value, BackendError> {
    if request.get("kind").is_some() {
        return describe_schema_object(conn, request);
    }
    let object: SqlObjectRef = serde_json::from_value(request.clone()).map_err(|error| {
        BackendError::with_code(
            "describe.bad_request",
            format!("describe_object takes a SqlObjectRef: {error}"),
        )
    })?;
    let not_found = || {
        BackendError::with_code(
            codes::DESCRIBE_NOT_FOUND,
            format!("no table or view named `{}`", object.object),
        )
    };
    if let Some(database) = object.database.as_deref().filter(|d| !d.is_empty()) {
        require_main(database).map_err(|_| not_found())?;
    }
    let name = canonical_name(conn, &object.object)?.ok_or_else(not_found)?;
    let properties = properties(conn, &name)?;
    serde_json::to_value(&properties)
        .map_err(|error| BackendError::new(format!("cannot encode table properties: {error}")))
}

/// The source of a trigger (`describe_object` with a `SchemaObjectRef`): the statement
/// SQLite stored, plus the `;` it leaves off. The name and table are parameters.
fn describe_schema_object(
    conn: &Connection,
    request: &serde_json::Value,
) -> Result<serde_json::Value, BackendError> {
    let reference: SchemaObjectRef = serde_json::from_value(request.clone()).map_err(|error| {
        BackendError::with_code(
            "describe.bad_request",
            format!("describe_object takes a SqlObjectRef or a SchemaObjectRef: {error}"),
        )
    })?;
    if reference.kind != SchemaObjectKind::Trigger {
        return Err(BackendError::with_code(
            "describe.bad_request",
            format!("SQLite has no {} source", reference.kind.as_str()),
        ));
    }
    let not_found = || {
        BackendError::with_code(
            codes::DESCRIBE_NOT_FOUND,
            format!("no trigger named `{}`", reference.name),
        )
    };
    if let Some(database) = reference.database.as_deref().filter(|d| !d.is_empty()) {
        require_main(database).map_err(|_| not_found())?;
    }
    let sql: Option<Option<String>> = conn
        .query_row(
            "SELECT sql FROM sqlite_schema WHERE type = 'trigger' AND name = ?1 \
               AND (?2 IS NULL OR tbl_name = ?2)",
            params![reference.name, reference.table],
            |row| row.get(0),
        )
        .map(Some)
        .or_else(|error| match error {
            rusqlite::Error::QueryReturnedNoRows => Ok(None),
            other => Err(other),
        })
        .map_err(sqlite_error)?;
    let mut source = sql.flatten().ok_or_else(not_found)?;
    let trimmed = source.trim_end().len();
    source.truncate(trimmed);
    if !source.ends_with(';') {
        source.push(';');
    }
    serde_json::to_value(SchemaObjectSource { source })
        .map_err(|error| BackendError::new(format!("cannot encode the source: {error}")))
}

fn properties(conn: &Connection, table: &str) -> Result<TableProperties, BackendError> {
    let infos = column_infos(conn, table)?;
    let mut primary: Vec<(i64, String)> = infos
        .iter()
        .filter(|info| info.pk_index > 0)
        .map(|info| (info.pk_index, info.column.name.clone()))
        .collect();
    primary.sort();
    let index_rows = index_list(conn, table)?;

    let mut keys = Vec::new();
    if !primary.is_empty() {
        keys.push(TableConstraint {
            name: "PRIMARY KEY".into(),
            kind: ConstraintKind::PrimaryKey,
            columns: primary.into_iter().map(|(_, name)| name).collect(),
            definition: None,
        });
    }
    let mut indexes = Vec::new();
    let mut index_ddl = Vec::new();
    for index in &index_rows {
        let index_columns = index_columns(conn, &index.name)?;
        if index.origin == "u" {
            keys.push(TableConstraint {
                name: index.name.clone(),
                kind: ConstraintKind::Unique,
                columns: index_columns.clone(),
                definition: None,
            });
        }
        if let Some(sql) = &index.sql {
            index_ddl.push(sql.clone());
        }
        indexes.push(TableIndex {
            name: index.name.clone(),
            columns: index_columns,
            unique: index.unique,
            primary: index.origin == "pk",
            method: Some("btree".into()),
            definition: index.sql.clone(),
        });
    }

    let foreign_keys = foreign_keys(conn, table)?;
    let references = references(conn, table)?;
    let triggers = triggers(conn, table)?;

    let mut ddl = table_sql(conn, table)?.unwrap_or_default();
    for sql in index_ddl
        .iter()
        .chain(triggers.iter().filter_map(|t| t.definition.as_ref()))
    {
        ddl.push_str(";\n");
        ddl.push_str(sql);
    }
    if !ddl.is_empty() {
        ddl.push(';');
    }

    Ok(TableProperties {
        columns: infos.into_iter().map(|info| info.column).collect(),
        keys: Some(keys),
        foreign_keys: Some(foreign_keys),
        indexes: Some(indexes),
        references: Some(references),
        triggers: Some(triggers),
        ddl: Some(ddl),
    })
}

struct IndexRow {
    name: String,
    unique: bool,
    origin: String,
    sql: Option<String>,
}

fn index_list(conn: &Connection, table: &str) -> Result<Vec<IndexRow>, BackendError> {
    let mut statement = conn
        .prepare_cached(
            "SELECT l.name, l.\"unique\", l.origin, \
                    (SELECT sql FROM sqlite_schema WHERE type = 'index' AND name = l.name) \
             FROM pragma_index_list(?1) AS l ORDER BY l.seq",
        )
        .map_err(sqlite_error)?;
    let rows = statement
        .query_map([table], |row| {
            Ok(IndexRow {
                name: row.get(0)?,
                unique: row.get::<_, i64>(1)? != 0,
                origin: row.get(2)?,
                sql: row.get(3)?,
            })
        })
        .map_err(sqlite_error)?
        .collect::<Result<Vec<_>, _>>()
        .map_err(sqlite_error)?;
    Ok(rows)
}

/// Column names of an index; an expression column shows as `<expression>`.
fn index_columns(conn: &Connection, index: &str) -> Result<Vec<String>, BackendError> {
    let mut statement = conn
        .prepare_cached("SELECT name FROM pragma_index_info(?1) ORDER BY seqno")
        .map_err(sqlite_error)?;
    let columns = statement
        .query_map([index], |row| {
            Ok(row
                .get::<_, Option<String>>(0)?
                .unwrap_or_else(|| "<expression>".into()))
        })
        .map_err(sqlite_error)?
        .collect::<Result<Vec<_>, _>>()
        .map_err(sqlite_error)?;
    Ok(columns)
}

/// Primary key column names of `table` in key order.
fn primary_key_columns(conn: &Connection, table: &str) -> Result<Vec<String>, BackendError> {
    let mut statement = conn
        .prepare_cached("SELECT name FROM pragma_table_info(?1) WHERE pk > 0 ORDER BY pk")
        .map_err(sqlite_error)?;
    let columns = statement
        .query_map([table], |row| row.get::<_, String>(0))
        .map_err(sqlite_error)?
        .collect::<Result<Vec<_>, _>>()
        .map_err(sqlite_error)?;
    Ok(columns)
}

/// Foreign keys declared by `table`. SQLite does not name them; the name is
/// `fk_<table>_<id>`.
fn foreign_keys(conn: &Connection, table: &str) -> Result<Vec<TableForeignKey>, BackendError> {
    let mut statement = conn
        .prepare_cached(
            "SELECT id, seq, \"table\", \"from\", \"to\", on_update, on_delete \
             FROM pragma_foreign_key_list(?1) ORDER BY id, seq",
        )
        .map_err(sqlite_error)?;
    let rows = statement
        .query_map([table], |row| {
            Ok((
                row.get::<_, i64>(0)?,
                row.get::<_, String>(2)?,
                row.get::<_, String>(3)?,
                row.get::<_, Option<String>>(4)?,
                row.get::<_, String>(5)?,
                row.get::<_, String>(6)?,
            ))
        })
        .map_err(sqlite_error)?
        .collect::<Result<Vec<_>, _>>()
        .map_err(sqlite_error)?;
    let mut keys: Vec<(i64, TableForeignKey)> = Vec::new();
    for (id, referenced_table, from, to, on_update, on_delete) in rows {
        match keys.iter_mut().find(|(key_id, _)| *key_id == id) {
            Some((_, key)) => {
                key.columns.push(from);
                if let Some(to) = to {
                    key.referenced_columns.push(to);
                }
            }
            None => keys.push((
                id,
                TableForeignKey {
                    name: format!("fk_{table}_{id}"),
                    table: table.to_owned(),
                    columns: vec![from],
                    referenced_table,
                    referenced_columns: to.into_iter().collect(),
                    on_update: Some(on_update),
                    on_delete: Some(on_delete),
                    definition: None,
                },
            )),
        }
    }
    let mut keys: Vec<TableForeignKey> = keys.into_iter().map(|(_, key)| key).collect();
    for key in &mut keys {
        // `REFERENCES parent` without columns means the parent's primary key.
        if key.referenced_columns.is_empty() {
            key.referenced_columns = primary_key_columns(conn, &key.referenced_table)?;
        }
    }
    Ok(keys)
}

/// Foreign keys of other tables that point at `table`.
fn references(conn: &Connection, table: &str) -> Result<Vec<TableForeignKey>, BackendError> {
    let mut statement = conn
        .prepare_cached("SELECT name FROM sqlite_schema WHERE type = 'table' ORDER BY name")
        .map_err(sqlite_error)?;
    let tables = statement
        .query_map([], |row| row.get::<_, String>(0))
        .map_err(sqlite_error)?
        .collect::<Result<Vec<_>, _>>()
        .map_err(sqlite_error)?;
    let mut references = Vec::new();
    for other in tables {
        for key in foreign_keys(conn, &other)? {
            if key.referenced_table.eq_ignore_ascii_case(table) {
                references.push(key);
            }
        }
    }
    Ok(references)
}

fn triggers(conn: &Connection, table: &str) -> Result<Vec<TableTrigger>, BackendError> {
    let mut statement = conn
        .prepare_cached(
            "SELECT name, sql FROM sqlite_schema \
             WHERE type = 'trigger' AND tbl_name = ?1 COLLATE NOCASE ORDER BY name",
        )
        .map_err(sqlite_error)?;
    let rows = statement
        .query_map([table], |row| {
            Ok((row.get::<_, String>(0)?, row.get::<_, Option<String>>(1)?))
        })
        .map_err(sqlite_error)?
        .collect::<Result<Vec<_>, _>>()
        .map_err(sqlite_error)?;
    Ok(rows
        .into_iter()
        .map(|(name, sql)| {
            let (timing, events) = sql.as_deref().map(parse_trigger).unwrap_or_default();
            TableTrigger {
                name,
                timing,
                events,
                enabled: Some(true),
                definition: sql,
            }
        })
        .collect())
}

/// Timing (`BEFORE`, `AFTER`, `INSTEAD OF`) and events (`INSERT`, `UPDATE`,
/// `DELETE`) of a `CREATE TRIGGER` statement, read from its head (up to `ON`).
fn parse_trigger(sql: &str) -> (Option<String>, Vec<String>) {
    let words: Vec<String> = sql
        .split(|c: char| c.is_whitespace() || c == ',')
        .filter(|word| !word.is_empty())
        .map(str::to_ascii_uppercase)
        .collect();
    let mut timing = None;
    let mut events: Vec<String> = Vec::new();
    let mut index = 0;
    while index < words.len() {
        match words[index].as_str() {
            "BEFORE" | "AFTER" if timing.is_none() => timing = Some(words[index].clone()),
            "INSTEAD"
                if timing.is_none() && words.get(index + 1).map(String::as_str) == Some("OF") =>
            {
                timing = Some("INSTEAD OF".into());
                index += 1;
            }
            "INSERT" | "DELETE" | "UPDATE"
                if timing.is_some() && !events.contains(&words[index]) =>
            {
                events.push(words[index].clone());
            }
            "ON" if timing.is_some() => break,
            _ => {}
        }
        index += 1;
    }
    (timing, events)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn db() -> Connection {
        let conn = Connection::open_in_memory().unwrap();
        conn.execute_batch(
            "CREATE TABLE items (tenant INT NOT NULL, id INT NOT NULL, name VARCHAR(100), qty INT, \
                                 PRIMARY KEY (tenant, id));
             CREATE TABLE refs (id INTEGER PRIMARY KEY, tenant INT NOT NULL, item_id INT NOT NULL, \
                                note TEXT DEFAULT 'x', \
                                FOREIGN KEY (tenant, item_id) REFERENCES items (tenant, id) ON DELETE CASCADE);
             CREATE TABLE implicit (a INT, b INT, FOREIGN KEY (a, b) REFERENCES items);
             CREATE TABLE uniq (k TEXT UNIQUE, v INT, g INT GENERATED ALWAYS AS (v * 2) VIRTUAL);
             CREATE UNIQUE INDEX uniq_v ON uniq (v);
             CREATE TABLE wr (a TEXT PRIMARY KEY, b INT) WITHOUT ROWID;
             CREATE VIEW v_items AS SELECT tenant, id FROM items;
             CREATE TABLE \"Odd Name\" (x INT);
             CREATE TRIGGER items_audit BEFORE UPDATE OF qty ON items BEGIN SELECT 1; END;
             CREATE TRIGGER items_gone AFTER DELETE ON items BEGIN SELECT 1; END;",
        )
        .unwrap();
        conn
    }

    fn request(
        parent: Option<&str>,
        payload: serde_json::Value,
        cursor: Option<&str>,
        limit: u64,
    ) -> NavigatorRequest {
        NavigatorRequest {
            parent: parent.map(str::to_owned),
            database: Some("main".into()),
            payload: Some(payload),
            cursor: cursor.map(str::to_owned),
            limit,
        }
    }

    fn labels(page: &NavigatorPage) -> Vec<&str> {
        page.nodes.iter().map(|n| n.label.as_str()).collect()
    }

    #[test]
    fn root_lists_main_and_database_lists_tables_and_views() {
        let conn = db();
        let root = list(
            &conn,
            &NavigatorRequest {
                parent: None,
                database: None,
                payload: None,
                cursor: None,
                limit: 0,
            },
        )
        .unwrap();
        assert_eq!(root.nodes[0].id, "database:main");
        assert_eq!(
            root.nodes[0].payload,
            serde_json::json!({"database": "main"})
        );
        let page = list(
            &conn,
            &request(
                Some("database:main"),
                serde_json::json!({"database": "main"}),
                None,
                0,
            ),
        )
        .unwrap();
        assert_eq!(
            labels(&page),
            ["Odd Name", "implicit", "items", "refs", "uniq", "v_items", "wr"]
        );
        assert!(page.next_cursor.is_none());
        let items = page.nodes.iter().find(|n| n.label == "items").unwrap();
        assert_eq!(items.kind, "table");
        assert_eq!(
            items.payload,
            serde_json::json!({"database": "main", "object": "items"})
        );
        assert!(items.actions.contains(&"describe".to_string()));
        assert!(items.has_children);
        let view = page.nodes.iter().find(|n| n.label == "v_items").unwrap();
        assert_eq!(view.kind, "view");
    }

    #[test]
    fn tables_page_with_exact_cursors_and_literal_filter() {
        let conn = db();
        let base = |cursor: Option<&str>, limit| {
            request(
                Some("database:main"),
                serde_json::json!({"database": "main"}),
                cursor,
                limit,
            )
        };
        let first = list(&conn, &base(None, 3)).unwrap();
        assert_eq!(labels(&first), ["Odd Name", "implicit", "items"]);
        assert_eq!(first.next_cursor.as_deref(), Some("items"));
        let second = list(&conn, &base(first.next_cursor.as_deref(), 3)).unwrap();
        assert_eq!(labels(&second), ["refs", "uniq", "v_items"]);
        let third = list(&conn, &base(second.next_cursor.as_deref(), 3)).unwrap();
        assert_eq!(labels(&third), ["wr"]);
        assert!(
            third.next_cursor.is_none(),
            "the lookahead row makes the last cursor exact"
        );
        let all = list(&conn, &base(None, u64::MAX)).unwrap();
        assert_eq!(all.nodes.len(), 7);
        let filtered = |filter: &str| {
            let page = list(
                &conn,
                &request(
                    Some("database:main"),
                    serde_json::json!({"database": "main", "filter": filter}),
                    None,
                    0,
                ),
            )
            .unwrap();
            page.nodes.into_iter().map(|n| n.label).collect::<Vec<_>>()
        };
        assert_eq!(filtered("ITEM"), ["items", "v_items"]);
        assert_eq!(filtered("od"), ["Odd Name"]);
        assert!(
            filtered("_").len() == 1,
            "`_` is literal: only v_items contains it"
        );
        assert!(filtered("%").is_empty());
        assert!(filtered("i_ems").is_empty());
    }

    #[test]
    fn internal_tables_are_hidden_and_other_databases_refused() {
        let conn = db();
        conn.execute_batch("CREATE TABLE seqt (id INTEGER PRIMARY KEY AUTOINCREMENT)")
            .unwrap();
        conn.execute("INSERT INTO seqt DEFAULT VALUES", []).unwrap();
        let page = list(
            &conn,
            &request(
                Some("database:main"),
                serde_json::json!({"database": "main"}),
                None,
                0,
            ),
        )
        .unwrap();
        assert!(!labels(&page).iter().any(|l| l.starts_with("sqlite_")));
        let mut other = request(
            Some("database:other"),
            serde_json::json!({"database": "other"}),
            None,
            0,
        );
        other.database = Some("other".into());
        let error = list(&conn, &other).unwrap_err();
        assert_eq!(error.code.as_deref(), Some("navigator.bad_request"));
    }

    #[test]
    fn column_nodes_carry_sql_column_payloads() {
        let conn = db();
        let page = list(
            &conn,
            &request(
                Some("table:main:items"),
                serde_json::json!({"database": "main", "object": "items"}),
                None,
                0,
            ),
        )
        .unwrap();
        assert_eq!(labels(&page), ["tenant", "id", "name", "qty"]);
        for node in &page.nodes {
            assert_eq!(node.kind, "column");
            let column: SqlColumnNode = serde_json::from_value(node.payload.clone()).unwrap();
            assert_eq!(column.object, object_ref("items"));
            assert_eq!(column.column, node.label);
            assert_eq!(
                column.details.primary_key,
                matches!(node.label.as_str(), "tenant" | "id")
            );
            assert_eq!(
                column.details.nullable,
                matches!(node.label.as_str(), "name" | "qty")
            );
        }
        // By parent id alone (no payload object).
        let by_id = list(
            &conn,
            &request(
                Some("table:main:refs"),
                serde_json::json!({"database": "main"}),
                None,
                0,
            ),
        )
        .unwrap();
        assert_eq!(labels(&by_id), ["id", "tenant", "item_id", "note"]);
        let id = &by_id.nodes[0];
        let id: SqlColumnNode = serde_json::from_value(id.payload.clone()).unwrap();
        assert!(
            !id.details.nullable && id.details.primary_key,
            "INTEGER PRIMARY KEY aliases the rowid"
        );
        assert_eq!(id.details.auto_increment, Some(true));
        let note: SqlColumnNode = serde_json::from_value(by_id.nodes[3].payload.clone()).unwrap();
        assert_eq!(note.details.default_expression.as_deref(), Some("'x'"));
        let missing = list(
            &conn,
            &request(
                None,
                serde_json::json!({"database": "main", "object": "nope"}),
                None,
                0,
            ),
        )
        .unwrap_err();
        assert_eq!(missing.code.as_deref(), Some(codes::DESCRIBE_NOT_FOUND));
    }

    fn describe_table(conn: &Connection, table: &str) -> TableProperties {
        let value = describe(
            conn,
            &serde_json::json!({"database": "main", "object": table}),
        )
        .unwrap();
        serde_json::from_value(value).unwrap()
    }

    #[test]
    fn describe_reports_keys_columns_and_ddl() {
        let conn = db();
        let items = describe_table(&conn, "items");
        let names: Vec<_> = items.columns.iter().map(|c| c.name.as_str()).collect();
        assert_eq!(names, ["tenant", "id", "name", "qty"]);
        let keys = items.keys.unwrap();
        assert_eq!(keys[0].kind, ConstraintKind::PrimaryKey);
        assert_eq!(keys[0].columns, ["tenant", "id"]);
        assert!(items.ddl.unwrap().starts_with("CREATE TABLE items"));
        let references = items.references.unwrap();
        assert!(references
            .iter()
            .any(|fk| fk.table == "refs" && fk.referenced_columns == ["tenant", "id"]));
        assert!(references.iter().any(|fk| fk.table == "implicit"));
        // Case-insensitive lookup returns the catalog's spelling.
        assert_eq!(describe_table(&conn, "ITEMS").columns.len(), 4);
    }

    #[test]
    fn describe_reports_foreign_keys_with_actions_and_implicit_targets() {
        let conn = db();
        let refs = describe_table(&conn, "refs");
        let fk = &refs.foreign_keys.unwrap()[0];
        assert_eq!(
            (fk.table.as_str(), fk.referenced_table.as_str()),
            ("refs", "items")
        );
        assert_eq!(fk.columns, ["tenant", "item_id"]);
        assert_eq!(fk.referenced_columns, ["tenant", "id"]);
        assert_eq!(fk.on_delete.as_deref(), Some("CASCADE"));
        let implicit = describe_table(&conn, "implicit");
        let fk = &implicit.foreign_keys.unwrap()[0];
        assert_eq!(
            fk.referenced_columns,
            ["tenant", "id"],
            "REFERENCES items means its primary key"
        );
    }

    #[test]
    fn describe_reports_unique_constraints_indexes_generated_columns_and_triggers() {
        let conn = db();
        let uniq = describe_table(&conn, "uniq");
        let keys = uniq.keys.unwrap();
        assert!(keys
            .iter()
            .any(|k| k.kind == ConstraintKind::Unique && k.columns == ["k"]));
        let indexes = uniq.indexes.unwrap();
        let by_v = indexes.iter().find(|i| i.name == "uniq_v").unwrap();
        assert!(by_v.unique && !by_v.primary);
        assert_eq!(by_v.columns, ["v"]);
        assert_eq!(
            by_v.definition.as_deref(),
            Some("CREATE UNIQUE INDEX uniq_v ON uniq (v)")
        );
        let generated = uniq.columns.iter().find(|c| c.name == "g").unwrap();
        assert_eq!(generated.is_generated, Some(true));
        assert!(uniq.ddl.unwrap().contains("CREATE UNIQUE INDEX uniq_v"));

        let items = describe_table(&conn, "items");
        let triggers = items.triggers.unwrap();
        let audit = triggers.iter().find(|t| t.name == "items_audit").unwrap();
        assert_eq!(audit.timing.as_deref(), Some("BEFORE"));
        assert_eq!(audit.events, ["UPDATE"]);
        let gone = triggers.iter().find(|t| t.name == "items_gone").unwrap();
        assert_eq!(
            (gone.timing.as_deref(), gone.events.as_slice()),
            (Some("AFTER"), ["DELETE".to_string()].as_slice())
        );
        assert!(items.ddl.unwrap().contains("CREATE TRIGGER items_audit"));

        let wr = describe_table(&conn, "wr");
        assert!(wr.columns.iter().all(|c| !c.nullable || c.name == "b"));
        assert!(
            !wr.columns[0].nullable,
            "WITHOUT ROWID primary keys are NOT NULL"
        );
        assert_eq!(describe_table(&conn, "Odd Name").columns[0].name, "x");
        assert_eq!(describe_table(&conn, "v_items").columns.len(), 2);
    }

    #[test]
    fn describe_errors_are_typed() {
        let conn = db();
        let not_found = |request: serde_json::Value| {
            describe(&conn, &request)
                .unwrap_err()
                .code
                .unwrap()
                .into_owned()
        };
        assert_eq!(
            not_found(serde_json::json!({"object": "no_such_table_zz"})),
            codes::DESCRIBE_NOT_FOUND
        );
        assert_eq!(
            not_found(serde_json::json!({"database": "other", "object": "items"})),
            codes::DESCRIBE_NOT_FOUND
        );
        assert_eq!(
            not_found(serde_json::json!({"nope": 1})),
            "describe.bad_request"
        );
        // Names are parameters: an injection attempt is just an unknown name.
        assert_eq!(
            not_found(serde_json::json!({"object": "items'); DROP TABLE items; --"})),
            codes::DESCRIBE_NOT_FOUND
        );
        assert_eq!(describe_table(&conn, "items").columns.len(), 4);
    }

    #[test]
    fn triggers_list_by_kind_page_and_show_their_source() {
        let conn = db();
        let triggers = |payload: serde_json::Value, cursor: Option<&str>, limit: u64| {
            list(&conn, &request(Some("database:main"), payload, cursor, limit)).unwrap()
        };
        let all = triggers(serde_json::json!({"database": "main", "kind": "trigger"}), None, 0);
        assert_eq!(labels(&all), ["items_audit on items", "items_gone on items"]);
        assert!(all.nodes.iter().all(|node| node.kind == "trigger" && !node.has_children));
        // The payload is the reference `describe_object` takes back.
        let source = describe(&conn, &all.nodes[0].payload).unwrap();
        let text = source["source"].as_str().unwrap();
        assert!(text.starts_with("CREATE TRIGGER items_audit BEFORE UPDATE OF qty ON items"), "{text}");
        assert!(text.ends_with("END;"), "{text}");

        // Keyset paging and the literal filter, like the tables.
        let first = triggers(serde_json::json!({"database": "main", "kind": "trigger"}), None, 1);
        assert_eq!(labels(&first), ["items_audit on items"]);
        assert_eq!(first.next_cursor.as_deref(), Some("items_audit"));
        let second = triggers(serde_json::json!({"database": "main", "kind": "trigger"}), first.next_cursor.as_deref(), 1);
        assert_eq!(labels(&second), ["items_gone on items"]);
        assert!(second.next_cursor.is_none());
        let filtered = triggers(serde_json::json!({"database": "main", "kind": "trigger", "filter": "GONE"}), None, 0);
        assert_eq!(labels(&filtered), ["items_gone on items"]);

        // Tables and views are untouched, and kinds SQLite has none of list nothing.
        let plain = triggers(serde_json::json!({"database": "main"}), None, 0);
        assert!(plain.nodes.iter().all(|node| node.kind != "trigger"));
        for kind in ["function", "procedure", "sequence", "type", "event", "materialized_view"] {
            let page = triggers(serde_json::json!({"database": "main", "kind": kind}), None, 0);
            assert!(page.nodes.is_empty(), "{kind}");
        }
        let unknown = list(&conn, &request(None, serde_json::json!({"database": "main", "kind": "package"}), None, 0));
        assert_eq!(unknown.unwrap_err().code.as_deref(), Some("navigator.bad_request"));
    }

    #[test]
    fn trigger_source_errors_are_typed_and_names_are_parameters() {
        let conn = db();
        let code = |request: serde_json::Value| {
            describe(&conn, &request).unwrap_err().code.unwrap().into_owned()
        };
        let reference = |name: &str, kind: &str, table: Option<&str>| {
            serde_json::json!({"database": "main", "kind": kind, "name": name, "table": table})
        };
        assert_eq!(code(reference("no_such_trigger", "trigger", None)), codes::DESCRIBE_NOT_FOUND);
        // The trigger exists, but on another table.
        assert_eq!(code(reference("items_audit", "trigger", Some("refs"))), codes::DESCRIBE_NOT_FOUND);
        assert_eq!(
            code(reference("x'; DROP TABLE items; --", "trigger", Some("items"))),
            codes::DESCRIBE_NOT_FOUND
        );
        assert_eq!(code(reference("f", "function", None)), "describe.bad_request");
        assert_eq!(code(serde_json::json!({"kind": "trigger"})), "describe.bad_request");
        assert_eq!(describe_table(&conn, "items").columns.len(), 4);
    }

    #[test]
    fn trigger_heads_are_parsed() {
        assert_eq!(
            parse_trigger("CREATE TRIGGER t INSTEAD OF DELETE ON v BEGIN SELECT 1; END"),
            (Some("INSTEAD OF".into()), vec!["DELETE".into()])
        );
        assert_eq!(
            parse_trigger("CREATE TEMP TRIGGER IF NOT EXISTS t AFTER UPDATE OF a, b ON x BEGIN DELETE FROM y; END"),
            (Some("AFTER".into()), vec!["UPDATE".into()])
        );
        assert_eq!(parse_trigger("garbage"), (None, vec![]));
    }
}
