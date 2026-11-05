use arrow::datatypes::{DataType, Field, Schema};
use arrow_array::{ArrayRef, Float64Array, Int64Array, RecordBatch, StringArray};
use rusqlite::types::ValueRef;
use rusqlite::LoadExtensionGuard;
use serde::Serialize;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::Mutex;
use std::time::Duration;

use crate::catalog::{CatalogColumn, CatalogObject, CatalogObjectKind};

pub struct SQLiteConn {
    conn: Arc<Mutex<rusqlite::Connection>>,
    interrupt: rusqlite::InterruptHandle,
}

impl std::fmt::Debug for SQLiteConn {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SQLiteConn").finish_non_exhaustive()
    }
}

#[derive(Debug, Serialize)]
pub struct SQLiteCatalog {
    pub schemas: Vec<SQLiteSchema>,
}

#[derive(Debug, Serialize)]
pub struct SQLiteSchema {
    pub name: String,
    pub tables: Vec<SQLiteObject>,
}

#[derive(Debug, Serialize)]
pub struct SQLiteObject {
    pub name: String,
    pub kind: String,
    pub columns: Vec<SQLiteColumn>,
    pub indexes: Vec<SQLiteIndex>,
    pub foreign_keys: Vec<SQLiteForeignKey>,
    pub triggers: Vec<SQLiteTrigger>,
}

#[derive(Debug, Serialize)]
pub struct SQLiteColumn {
    pub name: String,
    pub data_type: String,
    pub nullable: bool,
    pub primary_key: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub is_generated: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub default_expression: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub auto_increment: Option<bool>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SQLiteExtensionSpec {
    pub path: String,
    pub entry_point: Option<String>,
}

#[derive(Debug, Serialize)]
pub struct SQLiteIndex {
    pub name: String,
    pub columns: Vec<String>,
    pub unique: bool,
    pub primary: bool,
}

#[derive(Debug, Serialize)]
pub struct SQLiteForeignKey {
    pub name: String,
    pub column: String,
    pub ref_table: String,
    pub ref_column: String,
    pub on_update: String,
    pub on_delete: String,
}

#[derive(Debug, Serialize)]
pub struct SQLiteTrigger {
    pub name: String,
    pub event: String,
    pub timing: String,
}

impl SQLiteConn {
    pub fn connect(uri: &str) -> Result<Self, String> {
        let path = sqlite_uri_path(uri)?;
        let extensions = sqlite_extension_specs_from_uri(uri);
        let conn = if path == PathBuf::from(":memory:") {
            rusqlite::Connection::open_in_memory().map_err(|e| e.to_string())?
        } else {
            rusqlite::Connection::open(&path).map_err(|e| e.to_string())?
        };
        conn.busy_timeout(Duration::from_secs(10))
            .map_err(|e| e.to_string())?;
        load_sqlite_extensions(&conn, &extensions)?;
        conn.pragma_query(None, "database_list", |_| Ok(()))
            .map_err(|e| e.to_string())?;
        let interrupt = conn.get_interrupt_handle();
        Ok(Self {
            conn: Arc::new(Mutex::new(conn)),
            interrupt,
        })
    }

    /// Interrupts any in-flight statement without waiting for the connection
    /// mutex, so a stuck query can be cancelled from another thread.
    pub fn interrupt(&self) {
        self.interrupt.interrupt();
    }

    /// Attached database names, e.g. ["main", "sales"].
    pub async fn list_databases(&self) -> Result<Vec<String>, String> {
        let conn = self.conn.clone();
        tokio::task::spawn_blocking(move || {
            let mut conn = conn.lock().map_err(|e| e.to_string())?;
            database_names(&mut conn)
        })
        .await
        .map_err(|e| e.to_string())?
    }

    /// Tables and views in a schema, without loading the full catalog.
    pub async fn list_objects(&self, schema: &str) -> Result<Vec<CatalogObject>, String> {
        let conn = self.conn.clone();
        let schema = schema.to_string();
        tokio::task::spawn_blocking(move || {
            let mut conn = conn.lock().map_err(|e| e.to_string())?;
            tables_blocking(&mut conn, &schema).map(|tables| {
                tables
                    .into_iter()
                    .map(|table| CatalogObject {
                        kind: if table.kind == "view" {
                            CatalogObjectKind::View
                        } else {
                            CatalogObjectKind::Table
                        },
                        name: table.name,
                    })
                    .collect()
            })
        })
        .await
        .map_err(|e| e.to_string())?
    }

    /// Tables and views whose name contains `filter` (case-insensitive for
    /// ASCII, literal), at most `limit`. Filter and limit are bound parameters.
    pub async fn list_objects_filtered(
        &self,
        schema: &str,
        filter: &str,
        limit: usize,
    ) -> Result<Vec<CatalogObject>, String> {
        let conn = self.conn.clone();
        let schema = schema.to_string();
        let pattern = crate::name_filter::like_contains_pattern(filter);
        let limit = crate::name_filter::sql_limit(limit);
        tokio::task::spawn_blocking(move || {
            let mut conn = conn.lock().map_err(|e| e.to_string())?;
            tables_filtered_blocking(&mut conn, &schema, &pattern, limit).map(|tables| {
                tables
                    .into_iter()
                    .map(|table| CatalogObject {
                        kind: if table.kind == "view" {
                            CatalogObjectKind::View
                        } else {
                            CatalogObjectKind::Table
                        },
                        name: table.name,
                    })
                    .collect()
            })
        })
        .await
        .map_err(|e| e.to_string())?
    }

    /// Column metadata for one object, without loading the full catalog.
    pub async fn list_columns(
        &self,
        schema: &str,
        table: &str,
    ) -> Result<Vec<CatalogColumn>, String> {
        let conn = self.conn.clone();
        let schema = schema.to_string();
        let table = table.to_string();
        tokio::task::spawn_blocking(move || {
            let mut conn = conn.lock().map_err(|e| e.to_string())?;
            columns_blocking(&mut conn, &schema, &table).map(|columns| {
                columns
                    .into_iter()
                    .map(|column| CatalogColumn {
                        name: column.name,
                        data_type: column.data_type,
                        nullable: column.nullable,
                        primary_key: column.primary_key,
                        is_generated: column.is_generated,
                        default_expression: column.default_expression,
                        auto_increment: column.auto_increment,
                        comment: None,
                        attributes: None,
                    })
                    .collect()
            })
        })
        .await
        .map_err(|e| e.to_string())?
    }

    fn with_connection<T>(
        &self,
        f: impl FnOnce(&mut rusqlite::Connection) -> Result<T, String>,
    ) -> Result<T, String> {
        let mut conn = self.conn.lock().map_err(|e| e.to_string())?;
        f(&mut conn)
    }

    pub async fn catalog(&self) -> Result<SQLiteCatalog, String> {
        let conn = self.conn.clone();
        tokio::task::spawn_blocking(move || {
            let mut conn = conn.lock().map_err(|e| e.to_string())?;
            catalog_blocking(&mut conn)
        })
        .await
        .map_err(|e| e.to_string())?
    }

    pub async fn execute_sql(&self, sql: &str, limit: i64) -> Result<RecordBatch, String> {
        self.execute_sql_page(sql, limit, 0).await
    }

    pub async fn execute_sql_page(
        &self,
        sql: &str,
        limit: i64,
        offset: i64,
    ) -> Result<RecordBatch, String> {
        let sql = sql.to_string();
        let conn = self.conn.clone();
        tokio::task::spawn_blocking(move || {
            let mut conn = conn.lock().map_err(|e| e.to_string())?;
            query_blocking(&mut conn, &sql, limit, offset)
        })
        .await
        .map_err(|e| e.to_string())?
    }

    pub async fn execute_mutation(&self, sql: &str) -> Result<u64, String> {
        self.execute_mutations(&[sql.to_string()]).await
    }

    pub async fn execute_mutations(&self, sql: &[String]) -> Result<u64, String> {
        self.execute_mutations_checked(sql, false).await
    }

    pub async fn execute_mutations_checked(
        &self,
        sql: &[String],
        require_affected: bool,
    ) -> Result<u64, String> {
        let sql = sql.to_vec();
        let conn = self.conn.clone();
        tokio::task::spawn_blocking(move || {
            let mut conn = conn.lock().map_err(|e| e.to_string())?;
            let transaction = conn.transaction().map_err(|e| e.to_string())?;
            let mut affected = 0;
            for statement in sql {
                let count = transaction
                    .execute(&statement, [])
                    .map_err(|e| e.to_string())? as u64;
                if require_affected && count == 0 {
                    return Err("SQL mutation affected no rows; transaction rolled back".into());
                }
                affected += count;
            }
            transaction.commit().map_err(|e| e.to_string())?;
            Ok(affected)
        })
        .await
        .map_err(|e| e.to_string())?
    }
}

fn sqlite_uri_path(uri: &str) -> Result<PathBuf, String> {
    let prefix = "sqlite://";
    let Some(uri_prefix) = uri.get(..prefix.len()) else {
        return Err("SQLite URI must start with sqlite://".to_string());
    };
    if !uri_prefix.eq_ignore_ascii_case(prefix) {
        return Err("SQLite URI must start with sqlite://".to_string());
    }
    let rest = &uri[prefix.len()..];
    if rest.is_empty() || rest == "/" {
        return Err("SQLite URI path is empty".to_string());
    }
    let path = rest.split_once('?').map_or(rest, |(path, _)| path);
    Ok(PathBuf::from(percent_decode(path)?))
}

fn sqlite_extension_specs_from_uri(uri: &str) -> Vec<SQLiteExtensionSpec> {
    let Some((_, query)) = uri.split_once('?') else {
        return Vec::new();
    };
    query
        .split('&')
        .filter_map(|part| {
            let (raw_key, raw_value) = part.split_once('=').unwrap_or((part, ""));
            let key = percent_decode_query(raw_key).ok()?;
            if key != "sqlite_extension" && key != "sqlite_extensions" {
                return None;
            }
            Some(percent_decode_query(raw_value).ok()?)
        })
        .flat_map(|value| {
            value
                .lines()
                .filter_map(parse_sqlite_extension_spec)
                .collect::<Vec<_>>()
        })
        .collect()
}

fn parse_sqlite_extension_spec(value: &str) -> Option<SQLiteExtensionSpec> {
    let value = value.trim();
    if value.is_empty() {
        return None;
    }
    let (path, entry_point) = match value.rsplit_once('|') {
        Some((path, entry_point)) if !path.trim().is_empty() && !entry_point.trim().is_empty() => {
            (path.trim(), Some(entry_point.trim().to_string()))
        }
        _ => (value, None),
    };
    Some(SQLiteExtensionSpec {
        path: path.to_string(),
        entry_point,
    })
}

fn load_sqlite_extensions(
    conn: &rusqlite::Connection,
    extensions: &[SQLiteExtensionSpec],
) -> Result<(), String> {
    if extensions.is_empty() {
        return Ok(());
    }
    let _guard = unsafe { LoadExtensionGuard::new(conn) }
        .map_err(|e| format!("SQLite extension loading failed: {e}"))?;
    for extension in extensions {
        unsafe { conn.load_extension(&extension.path, extension.entry_point.as_deref()) }
            .map_err(|e| format!("SQLite extension load failed ({}): {e}", extension.path))?;
    }
    Ok(())
}

fn catalog_blocking(conn: &mut rusqlite::Connection) -> Result<SQLiteCatalog, String> {
    let schemas = database_names(conn)?;
    let mut catalog_schemas = Vec::with_capacity(schemas.len());
    for schema in schemas {
        let mut tables = tables_blocking(conn, &schema)?;
        for table in &mut tables {
            table.columns = columns_blocking(conn, &schema, &table.name)?;
            table.indexes = indexes_blocking(conn, &schema, &table.name)?;
            table.foreign_keys = foreign_keys_blocking(conn, &schema, &table.name)?;
            table.triggers = triggers_blocking(conn, &schema, &table.name)?;
        }
        catalog_schemas.push(SQLiteSchema {
            name: schema,
            tables,
        });
    }
    Ok(SQLiteCatalog {
        schemas: catalog_schemas,
    })
}

fn database_names(conn: &mut rusqlite::Connection) -> Result<Vec<String>, String> {
    let mut stmt = conn
        .prepare("PRAGMA database_list")
        .map_err(|e| e.to_string())?;
    let rows = stmt
        .query_map([], |row| row.get::<_, String>("name"))
        .map_err(|e| e.to_string())?;
    rows.collect::<Result<Vec<_>, _>>()
        .map_err(|e| e.to_string())
}

fn tables_blocking(
    conn: &mut rusqlite::Connection,
    schema: &str,
) -> Result<Vec<SQLiteObject>, String> {
    let sql = format!(
        "SELECT name, type FROM {}.sqlite_schema WHERE type IN ('table', 'view') AND name NOT LIKE 'sqlite_%' ORDER BY type, name",
        quote_identifier(schema)
    );
    let mut stmt = conn.prepare(&sql).map_err(|e| e.to_string())?;
    let rows = stmt
        .query_map([], |row| {
            Ok(SQLiteObject {
                name: row.get(0)?,
                kind: row.get(1)?,
                columns: Vec::new(),
                indexes: Vec::new(),
                foreign_keys: Vec::new(),
                triggers: Vec::new(),
            })
        })
        .map_err(|e| e.to_string())?;
    rows.collect::<Result<Vec<_>, _>>()
        .map_err(|e| e.to_string())
}

fn tables_filtered_blocking(
    conn: &mut rusqlite::Connection,
    schema: &str,
    pattern: &str,
    limit: i64,
) -> Result<Vec<SQLiteObject>, String> {
    let sql = format!(
        "SELECT name, type FROM {}.sqlite_schema WHERE type IN ('table', 'view') AND name NOT LIKE 'sqlite_%' AND name LIKE ?1 ESCAPE '\\' ORDER BY type, name LIMIT ?2",
        quote_identifier(schema)
    );
    let mut stmt = conn.prepare(&sql).map_err(|e| e.to_string())?;
    let rows = stmt
        .query_map(rusqlite::params![pattern, limit], |row| {
            Ok(SQLiteObject {
                name: row.get(0)?,
                kind: row.get(1)?,
                columns: Vec::new(),
                indexes: Vec::new(),
                foreign_keys: Vec::new(),
                triggers: Vec::new(),
            })
        })
        .map_err(|e| e.to_string())?;
    rows.collect::<Result<Vec<_>, _>>()
        .map_err(|e| e.to_string())
}

fn columns_blocking(
    conn: &mut rusqlite::Connection,
    schema: &str,
    table: &str,
) -> Result<Vec<SQLiteColumn>, String> {
    let sql = format!(
        "PRAGMA {}.table_xinfo({})",
        quote_identifier(schema),
        quote_string(table)
    );
    let mut stmt = conn.prepare(&sql).map_err(|e| e.to_string())?;
    let rows = stmt
        .query_map([], |row| {
            Ok(SQLiteColumn {
                name: row.get("name")?,
                data_type: row.get("type")?,
                nullable: row.get::<_, i32>("notnull")? == 0,
                primary_key: row.get::<_, i32>("pk")? > 0,
                is_generated: Some(row.get::<_, i32>("hidden")? != 0),
                default_expression: row.get("dflt_value")?,
                auto_increment: None,
            })
        })
        .map_err(|e| e.to_string())?;
    let mut columns = rows
        .collect::<Result<Vec<_>, _>>()
        .map_err(|e| e.to_string())?;
    drop(stmt);
    let pk_count = columns.iter().filter(|column| column.primary_key).count();
    let table_list_sql = format!(
        "PRAGMA {}.table_list({})",
        quote_identifier(schema),
        quote_string(table)
    );
    let without_rowid = conn
        .query_row(&table_list_sql, [], |row| row.get::<_, i32>("wr"))
        .ok()
        .map(|flag| flag != 0);
    for column in &mut columns {
        column.auto_increment = if column.primary_key
            && pk_count == 1
            && column.data_type.eq_ignore_ascii_case("INTEGER")
        {
            without_rowid.map(|without| !without)
        } else {
            Some(false)
        };
    }
    Ok(columns)
}

fn indexes_blocking(
    conn: &mut rusqlite::Connection,
    schema: &str,
    table: &str,
) -> Result<Vec<SQLiteIndex>, String> {
    let sql = format!(
        "PRAGMA {}.index_list({})",
        quote_identifier(schema),
        quote_string(table)
    );
    let mut stmt = conn.prepare(&sql).map_err(|e| e.to_string())?;
    let index_rows = stmt
        .query_map([], |row| {
            Ok((
                row.get::<_, String>("name")?,
                row.get::<_, i32>("unique")? != 0,
                row.get::<_, String>("origin")?,
            ))
        })
        .map_err(|e| e.to_string())?
        .collect::<Result<Vec<_>, _>>()
        .map_err(|e| e.to_string())?;
    drop(stmt);

    let mut indexes = Vec::with_capacity(index_rows.len());
    for (name, unique, origin) in index_rows {
        let column_sql = format!(
            "PRAGMA {}.index_info({})",
            quote_identifier(schema),
            quote_string(&name)
        );
        let mut column_stmt = conn.prepare(&column_sql).map_err(|e| e.to_string())?;
        let columns = column_stmt
            .query_map([], |row| row.get::<_, String>("name"))
            .map_err(|e| e.to_string())?
            .collect::<Result<Vec<_>, _>>()
            .map_err(|e| e.to_string())?;
        indexes.push(SQLiteIndex {
            name,
            columns,
            unique,
            primary: origin == "pk",
        });
    }
    Ok(indexes)
}

fn foreign_keys_blocking(
    conn: &mut rusqlite::Connection,
    schema: &str,
    table: &str,
) -> Result<Vec<SQLiteForeignKey>, String> {
    let sql = format!(
        "PRAGMA {}.foreign_key_list({})",
        quote_identifier(schema),
        quote_string(table)
    );
    let mut stmt = conn.prepare(&sql).map_err(|e| e.to_string())?;
    let rows = stmt
        .query_map([], |row| {
            Ok(SQLiteForeignKey {
                name: format!("fk_{}", row.get::<_, i32>("id")?),
                column: row.get("from")?,
                ref_table: row.get("table")?,
                ref_column: row.get("to")?,
                on_update: row.get("on_update")?,
                on_delete: row.get("on_delete")?,
            })
        })
        .map_err(|e| e.to_string())?;
    rows.collect::<Result<Vec<_>, _>>()
        .map_err(|e| e.to_string())
}

fn triggers_blocking(
    conn: &mut rusqlite::Connection,
    schema: &str,
    table: &str,
) -> Result<Vec<SQLiteTrigger>, String> {
    let sql = format!(
        "SELECT name, sql FROM {}.sqlite_schema WHERE type = 'trigger' AND tbl_name = ? ORDER BY name",
        quote_identifier(schema)
    );
    let mut stmt = conn.prepare(&sql).map_err(|e| e.to_string())?;
    let rows = stmt
        .query_map([table], |row| {
            let sql_text: Option<String> = row.get("sql")?;
            let upper = sql_text.unwrap_or_default().to_ascii_uppercase();
            let timing = if upper.contains("BEFORE") {
                "BEFORE"
            } else if upper.contains("AFTER") {
                "AFTER"
            } else {
                "INSTEAD OF"
            };
            let event = if upper.contains("INSERT") {
                "INSERT"
            } else if upper.contains("UPDATE") {
                "UPDATE"
            } else {
                "DELETE"
            };
            Ok(SQLiteTrigger {
                name: row.get("name")?,
                event: event.to_string(),
                timing: timing.to_string(),
            })
        })
        .map_err(|e| e.to_string())?;
    rows.collect::<Result<Vec<_>, _>>()
        .map_err(|e| e.to_string())
}

fn quote_identifier(value: &str) -> String {
    format!("\"{}\"", value.replace('"', "\"\""))
}

fn quote_string(value: &str) -> String {
    format!("'{}'", value.replace('\'', "''"))
}

fn query_blocking(
    conn: &mut rusqlite::Connection,
    sql: &str,
    limit: i64,
    offset: i64,
) -> Result<RecordBatch, String> {
    let normalized = normalize_sqlite_sql(sql.trim());
    if !starts_with_read_keyword(&normalized) {
        conn.execute_batch(&normalized).map_err(|e| e.to_string())?;
        return empty_record_batch();
    }
    let mut stmt = conn.prepare(&normalized).map_err(|e| e.to_string())?;
    let names: Vec<String> = stmt
        .column_names()
        .iter()
        .map(|name| name.to_string())
        .collect();
    let col_count = names.len();
    // Declared column types (`VARCHAR(20)`, `DATETIME`, ...); expression
    // columns have none. TODO(mysql): attach the same metadata in
    // mysql_arrow once MySQL/MariaDB connections are supported.
    let decl_types: Vec<Option<String>> = stmt
        .columns()
        .iter()
        .map(|column| column.decl_type().map(str::to_string))
        .collect();
    let mut columns: Vec<Vec<SQLiteCell>> = (0..col_count).map(|_| Vec::new()).collect();
    let mut rows = stmt.query([]).map_err(|e| e.to_string())?;
    let max_rows = limit.max(1) as usize;
    let skip_rows = offset.max(0) as usize;
    let mut skipped = 0;
    while let Some(row) = rows.next().map_err(|e| e.to_string())? {
        if skipped < skip_rows {
            skipped += 1;
            continue;
        }
        if columns.first().map_or(0, Vec::len) >= max_rows {
            break;
        }
        for index in 0..col_count {
            let cell = match row.get_ref(index).map_err(|e| e.to_string())? {
                ValueRef::Null => SQLiteCell::Null,
                ValueRef::Integer(value) => SQLiteCell::Integer(value),
                ValueRef::Real(value) => SQLiteCell::Real(value),
                ValueRef::Text(value) => {
                    SQLiteCell::Text(String::from_utf8_lossy(value).into_owned())
                }
                ValueRef::Blob(value) => SQLiteCell::Text(format!("<{} bytes>", value.len())),
            };
            columns[index].push(cell);
        }
    }

    let mut fields = Vec::with_capacity(col_count);
    let mut arrays: Vec<ArrayRef> = Vec::with_capacity(col_count);
    for (name, cells) in names.into_iter().zip(columns.into_iter()) {
        let kind = infer_kind(&cells);
        let mut field = Field::new(&name, kind.data_type(), true);
        if let Some(Some(decl_type)) = decl_types.get(fields.len()) {
            field = field.with_metadata(std::collections::HashMap::from([(
                crate::arrow::NATIVE_TYPE_METADATA_KEY.to_string(),
                decl_type.clone(),
            )]));
        }
        fields.push(field);
        arrays.push(kind.build_array(cells));
    }
    let schema = Arc::new(Schema::new(fields));
    RecordBatch::try_new(schema, arrays).map_err(|e| e.to_string())
}

fn empty_record_batch() -> Result<RecordBatch, String> {
    let schema = Arc::new(Schema::new(vec![Field::new(
        "result",
        DataType::Utf8,
        true,
    )]));
    RecordBatch::try_new(
        schema,
        vec![Arc::new(StringArray::from(Vec::<Option<String>>::new()))],
    )
    .map_err(|e| e.to_string())
}

fn starts_with_read_keyword(sql: &str) -> bool {
    let trimmed = strip_leading_sql_comments(sql);
    let first = trimmed
        .split(|ch: char| ch.is_whitespace() || ch == '(')
        .next()
        .unwrap_or("")
        .trim_end_matches(';');
    matches!(
        first.to_ascii_uppercase().as_str(),
        "SELECT" | "WITH" | "PRAGMA" | "EXPLAIN"
    )
}

fn strip_leading_sql_comments(mut sql: &str) -> &str {
    loop {
        sql = sql.trim_start();
        if let Some(rest) = sql.strip_prefix("--") {
            if let Some(newline) = rest.find('\n') {
                sql = &rest[newline + 1..];
                continue;
            }
            return "";
        }
        if let Some(rest) = sql.strip_prefix("/*") {
            if let Some(end) = rest.find("*/") {
                sql = &rest[end + 2..];
                continue;
            }
            return "";
        }
        return sql;
    }
}

const SQLITE_FUNCTION_ALIASES: &[(&str, &str)] = &[("if", "IIF"), ("substring", "substr")];

fn normalize_sqlite_sql(sql: &str) -> String {
    let mut result = String::with_capacity(sql.len());
    let chars: Vec<char> = sql.chars().collect();
    let len = chars.len();
    let mut i = 0;

    while i < len {
        if i + 1 < len && chars[i] == '-' && chars[i + 1] == '-' {
            while i < len && chars[i] != '\n' {
                result.push(chars[i]);
                i += 1;
            }
            continue;
        }

        if i + 1 < len && chars[i] == '/' && chars[i + 1] == '*' {
            while i + 1 < len && !(chars[i] == '*' && chars[i + 1] == '/') {
                result.push(chars[i]);
                i += 1;
            }
            if i + 1 < len {
                result.push(chars[i]);
                result.push(chars[i + 1]);
                i += 2;
            }
            continue;
        }

        if chars[i] == '\'' {
            result.push(chars[i]);
            i += 1;
            while i < len {
                result.push(chars[i]);
                if chars[i] == '\'' {
                    i += 1;
                    if i < len && chars[i] == '\'' {
                        result.push(chars[i]);
                        i += 1;
                    } else {
                        break;
                    }
                } else {
                    i += 1;
                }
            }
            continue;
        }

        let prev = if i == 0 { '\0' } else { chars[i - 1] };
        if !prev.is_alphanumeric() && prev != '_' && prev != '.' {
            let remaining: String = chars[i..].iter().collect();
            let remaining_lower = remaining.to_lowercase();
            let mut matched = false;
            for (source, replacement) in SQLITE_FUNCTION_ALIASES {
                if remaining_lower.starts_with(*source) && chars.get(i + source.len()) != Some(&'_')
                {
                    let mut j = i + source.len();
                    while j < len && chars[j].is_whitespace() {
                        j += 1;
                    }
                    if j < len && chars[j] == '(' {
                        let whitespace: String = chars[i + source.len()..j].iter().collect();
                        result.push_str(replacement);
                        result.push_str(&whitespace);
                        i = j;
                        matched = true;
                        break;
                    }
                }
            }
            if matched {
                continue;
            }
        }

        result.push(chars[i]);
        i += 1;
    }

    result
}

fn percent_decode(value: &str) -> Result<String, String> {
    let bytes = value.as_bytes();
    let mut decoded = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%' {
            if i + 2 >= bytes.len() {
                return Err("Invalid percent-encoded SQLite URI path".to_string());
            }
            let hi = hex_value(bytes[i + 1])?;
            let lo = hex_value(bytes[i + 2])?;
            decoded.push((hi << 4) | lo);
            i += 3;
        } else {
            decoded.push(bytes[i]);
            i += 1;
        }
    }
    String::from_utf8(decoded).map_err(|e| e.to_string())
}

fn percent_decode_query(value: &str) -> Result<String, String> {
    percent_decode(&value.replace('+', " "))
}

fn hex_value(byte: u8) -> Result<u8, String> {
    match byte {
        b'0'..=b'9' => Ok(byte - b'0'),
        b'a'..=b'f' => Ok(byte - b'a' + 10),
        b'A'..=b'F' => Ok(byte - b'A' + 10),
        _ => Err("Invalid percent-encoded SQLite URI path".to_string()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn extension_specs_parse_repeated_and_multiline_uri_params() {
        let specs = sqlite_extension_specs_from_uri(
            "sqlite:///tmp/app.db?cache=shared&sqlite_extension=%2Fopt%2Fregexp.dylib&sqlite_extensions=%2Fopt%2Ftext.dylib%7Csqlite3_text_init%0A%2Fopt%2Fcrypto.dylib",
        );

        assert_eq!(
            specs,
            vec![
                SQLiteExtensionSpec {
                    path: "/opt/regexp.dylib".to_string(),
                    entry_point: None
                },
                SQLiteExtensionSpec {
                    path: "/opt/text.dylib".to_string(),
                    entry_point: Some("sqlite3_text_init".to_string()),
                },
                SQLiteExtensionSpec {
                    path: "/opt/crypto.dylib".to_string(),
                    entry_point: None
                },
            ],
        );
    }

    #[test]
    fn extension_specs_ignore_empty_values() {
        assert!(sqlite_extension_specs_from_uri(
            "sqlite:///tmp/app.db?sqlite_extension=&sqlite_extensions=%0A"
        )
        .is_empty());
    }

    #[test]
    fn sqlite_uri_path_accepts_case_insensitive_scheme() {
        assert_eq!(
            sqlite_uri_path("SQLITE:///tmp/app%20file.db").unwrap(),
            PathBuf::from("/tmp/app file.db")
        );
    }
}

enum SQLiteCell {
    Null,
    Integer(i64),
    Real(f64),
    Text(String),
}

#[derive(Clone, Copy)]
enum SQLiteColumnKind {
    Integer,
    Real,
    Text,
}

fn infer_kind(cells: &[SQLiteCell]) -> SQLiteColumnKind {
    let mut kind = SQLiteColumnKind::Integer;
    for cell in cells {
        match cell {
            SQLiteCell::Null | SQLiteCell::Integer(_) => {}
            SQLiteCell::Real(_) => kind = SQLiteColumnKind::Real,
            SQLiteCell::Text(_) => return SQLiteColumnKind::Text,
        }
    }
    kind
}

impl SQLiteColumnKind {
    fn data_type(self) -> DataType {
        match self {
            Self::Integer => DataType::Int64,
            Self::Real => DataType::Float64,
            Self::Text => DataType::Utf8,
        }
    }

    fn build_array(self, cells: Vec<SQLiteCell>) -> ArrayRef {
        match self {
            Self::Integer => Arc::new(Int64Array::from(
                cells
                    .into_iter()
                    .map(|cell| match cell {
                        SQLiteCell::Integer(value) => Some(value),
                        _ => None,
                    })
                    .collect::<Vec<_>>(),
            )),
            Self::Real => Arc::new(Float64Array::from(
                cells
                    .into_iter()
                    .map(|cell| match cell {
                        SQLiteCell::Integer(value) => Some(value as f64),
                        SQLiteCell::Real(value) => Some(value),
                        _ => None,
                    })
                    .collect::<Vec<_>>(),
            )),
            Self::Text => Arc::new(StringArray::from(
                cells
                    .into_iter()
                    .map(|cell| match cell {
                        SQLiteCell::Integer(value) => Some(value.to_string()),
                        SQLiteCell::Real(value) => Some(value.to_string()),
                        SQLiteCell::Text(value) => Some(value),
                        SQLiteCell::Null => None,
                    })
                    .collect::<Vec<_>>(),
            )),
        }
    }
}

// ---------------------------------------------------------------------------
// Table properties (Properties presentation): keys, foreign keys, indexes,
// referencing FKs, triggers and DDL for one table/view.
// ---------------------------------------------------------------------------

/// Upper bound on tables scanned when looking for foreign keys that point at
/// the inspected table, so a huge schema cannot stall the properties load.
const SQLITE_REFERENCES_TABLE_SCAN_LIMIT: i64 = 500;

impl SQLiteConn {
    /// Full properties of one table/view. Every list is `Some` because SQLite
    /// can report all of them; CHECK constraints and trigger timing/events are
    /// parsed best-effort from the stored `CREATE` SQL (SQLite has no catalog
    /// for them).
    pub async fn table_properties(
        &self,
        schema: &str,
        table: &str,
    ) -> Result<crate::catalog::TableProperties, String> {
        let conn = self.conn.clone();
        let schema = schema.to_string();
        let table = table.to_string();
        tokio::task::spawn_blocking(move || {
            let mut conn = conn.lock().map_err(|e| e.to_string())?;
            table_properties_blocking(&mut conn, &schema, &table)
        })
        .await
        .map_err(|e| e.to_string())?
    }
}

fn table_properties_blocking(
    conn: &mut rusqlite::Connection,
    schema: &str,
    table: &str,
) -> Result<crate::catalog::TableProperties, String> {
    use crate::catalog::{CatalogColumn, CatalogIndex, TableProperties};

    let schema = if schema.trim().is_empty() {
        "main"
    } else {
        schema
    };
    let master = format!("{}.sqlite_schema", quote_identifier(schema));

    // Table/view row + index rows in one pass; tables/views sort first.
    let mut stmt = conn
        .prepare(&format!(
            "SELECT type, name, sql FROM {master} \
             WHERE tbl_name = ?1 COLLATE NOCASE AND type IN ('table', 'view', 'index') \
             ORDER BY type = 'index', name"
        ))
        .map_err(|e| e.to_string())?;
    let master_rows = stmt
        .query_map([table], |row| {
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, String>(1)?,
                row.get::<_, Option<String>>(2)?,
            ))
        })
        .map_err(|e| e.to_string())?
        .collect::<Result<Vec<_>, _>>()
        .map_err(|e| e.to_string())?;
    drop(stmt);
    let Some((object_type, object_name, object_sql)) = master_rows
        .iter()
        .find(|(kind, _, _)| kind != "index")
        .cloned()
    else {
        return Err(format!("Table or view not found: {schema}.{table}"));
    };
    let is_table = object_type == "table";
    let index_sql = |name: &str| -> Option<String> {
        master_rows
            .iter()
            .find(|(kind, index_name, _)| kind == "index" && index_name == name)
            .and_then(|(_, _, sql)| sql.clone())
    };

    let columns: Vec<CatalogColumn> = columns_blocking(conn, schema, &object_name)?
        .into_iter()
        .map(|column| CatalogColumn {
            name: column.name,
            data_type: column.data_type,
            nullable: column.nullable,
            primary_key: column.primary_key,
            is_generated: column.is_generated,
            default_expression: column.default_expression,
            auto_increment: column.auto_increment,
            comment: None,
            attributes: None,
        })
        .collect();

    // Indexes (includes the implicit sqlite_autoindex_* ones, origin u/pk).
    let index_rows = {
        let mut stmt = conn
            .prepare("SELECT name, \"unique\", origin FROM pragma_index_list(?1, ?2) ORDER BY seq")
            .map_err(|e| e.to_string())?;
        let rows = stmt
            .query_map([&object_name, schema], |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, i64>(1)? != 0,
                    row.get::<_, String>(2)?,
                ))
            })
            .map_err(|e| e.to_string())?
            .collect::<Result<Vec<_>, _>>()
            .map_err(|e| e.to_string())?;
        rows
    };
    let mut indexes = Vec::with_capacity(index_rows.len());
    let mut unique_keys = Vec::new();
    let mut primary_index_name = None;
    for (name, unique, origin) in index_rows {
        let mut stmt = conn
            .prepare("SELECT cid, name FROM pragma_index_info(?1, ?2) ORDER BY seqno")
            .map_err(|e| e.to_string())?;
        let index_columns = stmt
            .query_map([&name, schema], |row| {
                let cid = row.get::<_, i64>(0)?;
                let column = row.get::<_, Option<String>>(1)?;
                Ok(column.unwrap_or_else(|| {
                    if cid == -1 {
                        "rowid".to_string()
                    } else {
                        "(expression)".to_string()
                    }
                }))
            })
            .map_err(|e| e.to_string())?
            .collect::<Result<Vec<_>, _>>()
            .map_err(|e| e.to_string())?;
        drop(stmt);
        match origin.as_str() {
            "pk" => primary_index_name = Some(name.clone()),
            "u" => unique_keys.push(crate::catalog::CatalogConstraint {
                name: name.clone(),
                kind: crate::catalog::CatalogConstraintKind::Unique,
                definition: Some(format!("UNIQUE ({})", quoted_column_list(&index_columns))),
                columns: index_columns.clone(),
            }),
            _ => {}
        }
        indexes.push(CatalogIndex {
            definition: index_sql(&name),
            name,
            columns: index_columns,
            unique,
            primary: origin == "pk",
            method: None,
        });
    }

    // Keys: PRIMARY KEY (column order from table_info.pk), UNIQUE, CHECK.
    let mut keys = Vec::new();
    let primary_columns = {
        let mut stmt = conn
            .prepare("SELECT name FROM pragma_table_info(?1, ?2) WHERE pk > 0 ORDER BY pk")
            .map_err(|e| e.to_string())?;
        let rows = stmt
            .query_map([&object_name, schema], |row| row.get::<_, String>(0))
            .map_err(|e| e.to_string())?
            .collect::<Result<Vec<_>, _>>()
            .map_err(|e| e.to_string())?;
        rows
    };
    if !primary_columns.is_empty() {
        keys.push(crate::catalog::CatalogConstraint {
            name: primary_index_name.unwrap_or_else(|| "PRIMARY KEY".to_string()),
            kind: crate::catalog::CatalogConstraintKind::PrimaryKey,
            definition: Some(format!(
                "PRIMARY KEY ({})",
                quoted_column_list(&primary_columns)
            )),
            columns: primary_columns.clone(),
        });
    }
    keys.append(&mut unique_keys);
    if is_table {
        if let Some(sql) = object_sql.as_deref() {
            for (position, (name, expression)) in sqlite_check_constraints(sql).into_iter().enumerate()
            {
                keys.push(crate::catalog::CatalogConstraint {
                    name: name.unwrap_or_else(|| format!("check_{}", position + 1)),
                    kind: crate::catalog::CatalogConstraintKind::Check,
                    columns: Vec::new(),
                    definition: Some(format!("CHECK ({expression})")),
                });
            }
        }
    }

    let foreign_keys = sqlite_foreign_keys(conn, schema, &object_name, &primary_columns)?;

    // References: FKs in other tables (bounded scan) pointing at this table.
    let references = {
        let mut stmt = conn
            .prepare(&format!(
                "SELECT m.name, f.id, f.seq, f.\"table\", f.\"from\", f.\"to\", f.on_update, f.on_delete \
                 FROM (SELECT name FROM {master} \
                       WHERE type = 'table' AND name NOT LIKE 'sqlite_%' \
                       ORDER BY name LIMIT ?3) AS m, \
                      pragma_foreign_key_list(m.name, ?2) AS f \
                 WHERE f.\"table\" = ?1 COLLATE NOCASE \
                 ORDER BY m.name, f.id, f.seq"
            ))
            .map_err(|e| e.to_string())?;
        let rows = stmt
            .query_map(
                rusqlite::params![object_name, schema, SQLITE_REFERENCES_TABLE_SCAN_LIMIT],
                sqlite_foreign_key_row,
            )
            .map_err(|e| e.to_string())?
            .collect::<Result<Vec<_>, _>>()
            .map_err(|e| e.to_string())?;
        group_sqlite_foreign_keys(rows, &object_name, &primary_columns)
    };

    // Triggers.
    let triggers = {
        let mut stmt = conn
            .prepare(&format!(
                "SELECT name, sql FROM {master} \
                 WHERE type = 'trigger' AND tbl_name = ?1 COLLATE NOCASE ORDER BY name"
            ))
            .map_err(|e| e.to_string())?;
        let rows = stmt
            .query_map([&object_name], |row| {
                Ok((row.get::<_, String>(0)?, row.get::<_, Option<String>>(1)?))
            })
            .map_err(|e| e.to_string())?
            .collect::<Result<Vec<_>, _>>()
            .map_err(|e| e.to_string())?;
        rows.into_iter()
            .map(|(name, sql)| {
                let (timing, events) = sql
                    .as_deref()
                    .map(sqlite_trigger_timing_and_events)
                    .unwrap_or((None, Vec::new()));
                crate::catalog::CatalogTrigger {
                    name,
                    timing,
                    events,
                    // SQLite cannot disable a trigger; it exists or it does not.
                    enabled: Some(true),
                    definition: sql,
                }
            })
            .collect::<Vec<_>>()
    };

    // DDL: the table/view statement followed by its (explicit) indexes.
    let mut ddl = String::new();
    for sql in object_sql
        .iter()
        .chain(indexes.iter().filter_map(|index| index.definition.as_ref()))
    {
        if !ddl.is_empty() {
            ddl.push_str("\n\n");
        }
        ddl.push_str(sql.trim_end().trim_end_matches(';'));
        ddl.push(';');
    }

    Ok(TableProperties {
        columns,
        keys: Some(keys),
        foreign_keys: Some(foreign_keys),
        indexes: Some(indexes),
        references: Some(references),
        triggers: Some(triggers),
        ddl: (!ddl.is_empty()).then_some(ddl),
    })
}

type SqliteForeignKeyRow = (String, i64, i64, String, String, Option<String>, String, String);

/// Row shape shared by the own-FK and referencing-FK queries:
/// (owner table, id, seq, referenced table, from, to, on_update, on_delete).
fn sqlite_foreign_key_row(row: &rusqlite::Row<'_>) -> rusqlite::Result<SqliteForeignKeyRow> {
    Ok((
        row.get(0)?,
        row.get(1)?,
        row.get(2)?,
        row.get(3)?,
        row.get(4)?,
        row.get(5)?,
        row.get(6)?,
        row.get(7)?,
    ))
}

fn sqlite_foreign_keys(
    conn: &mut rusqlite::Connection,
    schema: &str,
    table: &str,
    own_primary_columns: &[String],
) -> Result<Vec<crate::catalog::CatalogForeignKey>, String> {
    let mut stmt = conn
        .prepare(
            "SELECT ?1, id, seq, \"table\", \"from\", \"to\", on_update, on_delete \
             FROM pragma_foreign_key_list(?1, ?2) ORDER BY id, seq",
        )
        .map_err(|e| e.to_string())?;
    let rows = stmt
        .query_map([table, schema], sqlite_foreign_key_row)
        .map_err(|e| e.to_string())?
        .collect::<Result<Vec<_>, _>>()
        .map_err(|e| e.to_string())?;
    drop(stmt);
    // An FK without an explicit parent column targets the parent's primary key.
    let mut foreign_keys = group_sqlite_foreign_keys(rows, table, own_primary_columns);
    for foreign_key in &mut foreign_keys {
        if foreign_key.referenced_columns.is_empty() {
            let mut stmt = conn
                .prepare("SELECT name FROM pragma_table_info(?1, ?2) WHERE pk > 0 ORDER BY pk")
                .map_err(|e| e.to_string())?;
            foreign_key.referenced_columns = stmt
                .query_map([&foreign_key.referenced_table, schema], |row| {
                    row.get::<_, String>(0)
                })
                .map_err(|e| e.to_string())?
                .collect::<Result<Vec<_>, _>>()
                .map_err(|e| e.to_string())?;
            drop(stmt);
            foreign_key.definition = Some(sqlite_foreign_key_definition(
                &foreign_key.columns,
                &foreign_key.referenced_table,
                &foreign_key.referenced_columns,
                foreign_key.on_update.as_deref(),
                foreign_key.on_delete.as_deref(),
            ));
        }
    }
    Ok(foreign_keys)
}

/// Groups `foreign_key_list` rows (one per column) into one FK per `id`.
/// `inspected_primary_columns` fills the parent column list when an FK omits
/// it and its parent is the inspected table itself.
fn group_sqlite_foreign_keys(
    rows: Vec<SqliteForeignKeyRow>,
    inspected_table: &str,
    inspected_primary_columns: &[String],
) -> Vec<crate::catalog::CatalogForeignKey> {
    let mut foreign_keys: Vec<(String, i64, crate::catalog::CatalogForeignKey)> = Vec::new();
    for (owner, id, _seq, parent, from, to, on_update, on_delete) in rows {
        if let Some((_, _, foreign_key)) = foreign_keys
            .iter_mut()
            .find(|(existing_owner, existing_id, _)| *existing_owner == owner && *existing_id == id)
        {
            foreign_key.columns.push(from);
            if let Some(to) = to {
                foreign_key.referenced_columns.push(to);
            }
            continue;
        }
        foreign_keys.push((
            owner.clone(),
            id,
            crate::catalog::CatalogForeignKey {
                name: format!("fk_{id}"),
                table: owner,
                columns: vec![from],
                referenced_table: parent,
                referenced_columns: to.into_iter().collect(),
                on_update: Some(on_update),
                on_delete: Some(on_delete),
                definition: None,
            },
        ));
    }
    foreign_keys
        .into_iter()
        .map(|(_, _, mut foreign_key)| {
            if foreign_key.referenced_columns.is_empty()
                && foreign_key
                    .referenced_table
                    .eq_ignore_ascii_case(inspected_table)
            {
                foreign_key.referenced_columns = inspected_primary_columns.to_vec();
            }
            foreign_key.definition = Some(sqlite_foreign_key_definition(
                &foreign_key.columns,
                &foreign_key.referenced_table,
                &foreign_key.referenced_columns,
                foreign_key.on_update.as_deref(),
                foreign_key.on_delete.as_deref(),
            ));
            foreign_key
        })
        .collect()
}

fn sqlite_foreign_key_definition(
    columns: &[String],
    referenced_table: &str,
    referenced_columns: &[String],
    on_update: Option<&str>,
    on_delete: Option<&str>,
) -> String {
    let mut definition = format!(
        "FOREIGN KEY ({}) REFERENCES {}",
        quoted_column_list(columns),
        quote_identifier(referenced_table)
    );
    if !referenced_columns.is_empty() {
        definition.push_str(&format!(" ({})", quoted_column_list(referenced_columns)));
    }
    if let Some(action) = on_update.filter(|action| !action.eq_ignore_ascii_case("NO ACTION")) {
        definition.push_str(&format!(" ON UPDATE {action}"));
    }
    if let Some(action) = on_delete.filter(|action| !action.eq_ignore_ascii_case("NO ACTION")) {
        definition.push_str(&format!(" ON DELETE {action}"));
    }
    definition
}

fn quoted_column_list(columns: &[String]) -> String {
    columns
        .iter()
        .map(|column| quote_identifier(column))
        .collect::<Vec<_>>()
        .join(", ")
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum SqliteTokenKind {
    Word,
    /// `"x"`, `` `x` `` or `[x]` identifier (text is unquoted).
    Quoted,
    /// `'text'` literal.
    String,
    Punct,
}

#[derive(Debug, Clone)]
struct SqliteToken {
    kind: SqliteTokenKind,
    text: String,
    start: usize,
    end: usize,
}

impl SqliteToken {
    fn is_word(&self, word: &str) -> bool {
        self.kind == SqliteTokenKind::Word && self.text.eq_ignore_ascii_case(word)
    }

    fn is_punct(&self, punct: char) -> bool {
        self.kind == SqliteTokenKind::Punct && self.text.starts_with(punct)
    }
}

/// Minimal SQL tokenizer for stored `CREATE` statements: skips whitespace and
/// comments, keeps quoted identifiers/strings as single tokens so keyword
/// searches never match inside them.
fn sqlite_tokenize(sql: &str) -> Vec<SqliteToken> {
    let bytes = sql.as_bytes();
    let mut tokens = Vec::new();
    let mut i = 0;
    while i < bytes.len() {
        let byte = bytes[i];
        if byte.is_ascii_whitespace() {
            i += 1;
        } else if byte == b'-' && bytes.get(i + 1) == Some(&b'-') {
            while i < bytes.len() && bytes[i] != b'\n' {
                i += 1;
            }
        } else if byte == b'/' && bytes.get(i + 1) == Some(&b'*') {
            i += 2;
            while i < bytes.len() && !(bytes[i] == b'*' && bytes.get(i + 1) == Some(&b'/')) {
                i += 1;
            }
            i = (i + 2).min(bytes.len());
        } else if matches!(byte, b'\'' | b'"' | b'`' | b'[') {
            let close = if byte == b'[' { b']' } else { byte };
            let start = i;
            i += 1;
            let mut text = Vec::new();
            while i < bytes.len() {
                if bytes[i] == close {
                    if close != b']' && bytes.get(i + 1) == Some(&close) {
                        text.push(close);
                        i += 2;
                        continue;
                    }
                    i += 1;
                    break;
                }
                text.push(bytes[i]);
                i += 1;
            }
            tokens.push(SqliteToken {
                kind: if byte == b'\'' {
                    SqliteTokenKind::String
                } else {
                    SqliteTokenKind::Quoted
                },
                text: String::from_utf8_lossy(&text).into_owned(),
                start,
                end: i,
            });
        } else if byte.is_ascii_alphanumeric() || byte == b'_' || byte == b'$' || byte >= 0x80 {
            let start = i;
            while i < bytes.len()
                && (bytes[i].is_ascii_alphanumeric()
                    || bytes[i] == b'_'
                    || bytes[i] == b'$'
                    || bytes[i] >= 0x80)
            {
                i += 1;
            }
            tokens.push(SqliteToken {
                kind: SqliteTokenKind::Word,
                text: sql[start..i].to_string(),
                start,
                end: i,
            });
        } else {
            tokens.push(SqliteToken {
                kind: SqliteTokenKind::Punct,
                text: (byte as char).to_string(),
                start: i,
                end: i + 1,
            });
            i += 1;
        }
    }
    tokens
}

/// `CHECK (...)` constraints (optionally named via `CONSTRAINT name`) from a
/// stored `CREATE TABLE` statement, as `(name, expression)`.
fn sqlite_check_constraints(sql: &str) -> Vec<(Option<String>, String)> {
    let tokens = sqlite_tokenize(sql);
    let mut checks = Vec::new();
    let mut i = 0;
    while i < tokens.len() {
        if tokens[i].is_word("CHECK") && tokens.get(i + 1).is_some_and(|t| t.is_punct('(')) {
            let mut depth = 0usize;
            let mut close = None;
            for (offset, token) in tokens[i + 1..].iter().enumerate() {
                if token.is_punct('(') {
                    depth += 1;
                } else if token.is_punct(')') {
                    depth -= 1;
                    if depth == 0 {
                        close = Some(i + 1 + offset);
                        break;
                    }
                }
            }
            let Some(close) = close else { break };
            let name = (i >= 2 && tokens[i - 2].is_word("CONSTRAINT"))
                .then(|| tokens[i - 1].text.clone());
            let expression = sql[tokens[i + 1].end..tokens[close].start].trim().to_string();
            checks.push((name, expression));
            i = close + 1;
        } else {
            i += 1;
        }
    }
    checks
}

/// `(timing, events)` from a stored `CREATE TRIGGER` statement. SQLite treats a
/// missing timing as BEFORE; a trigger has exactly one event (UPDATE may carry
/// an `OF` column list, which is ignored here).
fn sqlite_trigger_timing_and_events(sql: &str) -> (Option<String>, Vec<String>) {
    let tokens = sqlite_tokenize(sql);
    let Some(trigger) = tokens.iter().position(|token| token.is_word("TRIGGER")) else {
        return (None, Vec::new());
    };
    let mut i = trigger + 1;
    if tokens.get(i).is_some_and(|t| t.is_word("IF")) {
        i += 3; // IF NOT EXISTS
    }
    // Trigger name, optionally schema-qualified (`schema . name`).
    i += 1;
    if tokens.get(i).is_some_and(|t| t.is_punct('.')) {
        i += 2;
    }
    let mut timing = "BEFORE";
    if let Some(token) = tokens.get(i) {
        if token.is_word("BEFORE") {
            i += 1;
        } else if token.is_word("AFTER") {
            timing = "AFTER";
            i += 1;
        } else if token.is_word("INSTEAD") {
            timing = "INSTEAD OF";
            i += 2;
        }
    }
    let events = tokens
        .get(i)
        .and_then(|token| {
            ["INSERT", "UPDATE", "DELETE"]
                .into_iter()
                .find(|event| token.is_word(event))
        })
        .map(|event| vec![event.to_string()])
        .unwrap_or_default();
    (Some(timing.to_string()), events)
}

#[cfg(test)]
mod table_properties_tests {
    use super::*;
    use crate::catalog::CatalogConstraintKind;

    fn fixture() -> rusqlite::Connection {
        let conn = rusqlite::Connection::open_in_memory().expect("in-memory sqlite");
        conn.execute_batch(
            "CREATE TABLE customers (id INTEGER PRIMARY KEY, name TEXT NOT NULL);
             CREATE TABLE orders (
                 id INTEGER PRIMARY KEY,
                 customer_id INTEGER NOT NULL,
                 sku TEXT NOT NULL,
                 qty INTEGER NOT NULL DEFAULT 1 CHECK (qty > 0),
                 note TEXT,
                 CONSTRAINT sku_len CHECK (length(sku) < 32 AND sku <> 'a)b'),
                 UNIQUE (customer_id, sku),
                 FOREIGN KEY (customer_id) REFERENCES customers(id) ON DELETE CASCADE ON UPDATE SET NULL
             );
             CREATE INDEX idx_orders_note ON orders (note);
             CREATE UNIQUE INDEX idx_orders_sku ON orders (sku);
             CREATE TABLE refunds (
                 id INTEGER PRIMARY KEY,
                 order_id INTEGER REFERENCES orders
             );
             CREATE TRIGGER orders_after_insert AFTER INSERT ON orders
             BEGIN
                 UPDATE customers SET name = name WHERE id = NEW.customer_id;
             END;
             CREATE TRIGGER \"orders before update\" BEFORE UPDATE OF qty ON orders
             BEGIN SELECT 1; END;
             CREATE VIEW order_skus AS SELECT sku FROM orders;",
        )
        .expect("fixture schema");
        conn
    }

    #[test]
    fn orders_table_reports_every_properties_field() {
        let mut conn = fixture();
        let props = table_properties_blocking(&mut conn, "main", "orders").expect("properties");

        let names: Vec<_> = props.columns.iter().map(|c| c.name.as_str()).collect();
        assert_eq!(names, ["id", "customer_id", "sku", "qty", "note"]);
        assert!(props.columns[0].primary_key);
        assert_eq!(props.columns[3].default_expression.as_deref(), Some("1"));

        let keys = props.keys.expect("keys");
        let primary = keys
            .iter()
            .find(|key| key.kind == CatalogConstraintKind::PrimaryKey)
            .expect("primary key");
        assert_eq!(primary.columns, ["id"]);
        let unique = keys
            .iter()
            .find(|key| key.kind == CatalogConstraintKind::Unique)
            .expect("unique key");
        assert_eq!(unique.columns, ["customer_id", "sku"]);
        assert_eq!(
            unique.definition.as_deref(),
            Some("UNIQUE (\"customer_id\", \"sku\")")
        );
        let checks: Vec<_> = keys
            .iter()
            .filter(|key| key.kind == CatalogConstraintKind::Check)
            .collect();
        assert_eq!(checks.len(), 2);
        assert_eq!(checks[0].name, "check_1");
        assert_eq!(checks[0].definition.as_deref(), Some("CHECK (qty > 0)"));
        assert_eq!(checks[1].name, "sku_len");
        assert_eq!(
            checks[1].definition.as_deref(),
            Some("CHECK (length(sku) < 32 AND sku <> 'a)b')")
        );

        let foreign_keys = props.foreign_keys.expect("foreign keys");
        assert_eq!(foreign_keys.len(), 1);
        let fk = &foreign_keys[0];
        assert_eq!(fk.table, "orders");
        assert_eq!(fk.columns, ["customer_id"]);
        assert_eq!(fk.referenced_table, "customers");
        assert_eq!(fk.referenced_columns, ["id"]);
        assert_eq!(fk.on_delete.as_deref(), Some("CASCADE"));
        assert_eq!(fk.on_update.as_deref(), Some("SET NULL"));
        assert_eq!(
            fk.definition.as_deref(),
            Some("FOREIGN KEY (\"customer_id\") REFERENCES \"customers\" (\"id\") ON UPDATE SET NULL ON DELETE CASCADE")
        );

        let indexes = props.indexes.expect("indexes");
        let note = indexes.iter().find(|i| i.name == "idx_orders_note").expect("note index");
        assert_eq!(note.columns, ["note"]);
        assert!(!note.unique && !note.primary);
        assert_eq!(note.method, None);
        let sku = indexes.iter().find(|i| i.name == "idx_orders_sku").expect("sku index");
        assert!(sku.unique && !sku.primary);
        assert_eq!(
            sku.definition.as_deref(),
            Some("CREATE UNIQUE INDEX idx_orders_sku ON orders (sku)")
        );
        // The UNIQUE constraint's implicit index is reported too.
        assert!(indexes.iter().any(|i| i.name.starts_with("sqlite_autoindex_orders")));

        let references = props.references.expect("references");
        assert_eq!(references.len(), 1, "{references:?}");
        assert_eq!(references[0].table, "refunds");
        assert_eq!(references[0].columns, ["order_id"]);
        assert_eq!(references[0].referenced_columns, ["id"]);

        let triggers = props.triggers.expect("triggers");
        assert_eq!(triggers.len(), 2);
        let after = triggers.iter().find(|t| t.name == "orders_after_insert").expect("trigger");
        assert_eq!(after.timing.as_deref(), Some("AFTER"));
        assert_eq!(after.events, ["INSERT"]);
        assert_eq!(after.enabled, Some(true));
        assert!(after.definition.as_deref().unwrap().contains("UPDATE customers"));
        let before = triggers
            .iter()
            .find(|t| t.name == "orders before update")
            .expect("quoted trigger name");
        assert_eq!(before.timing.as_deref(), Some("BEFORE"));
        assert_eq!(before.events, ["UPDATE"]);

        let ddl = props.ddl.expect("ddl");
        assert!(ddl.starts_with("CREATE TABLE orders"), "{ddl}");
        assert!(ddl.contains("CREATE INDEX idx_orders_note"), "{ddl}");
        assert!(ddl.contains("CREATE UNIQUE INDEX idx_orders_sku"), "{ddl}");
        assert!(ddl.trim_end().ends_with(';'));
    }

    #[test]
    fn referenced_table_lists_incoming_foreign_keys() {
        let mut conn = fixture();

        let customers = table_properties_blocking(&mut conn, "main", "customers").expect("customers");
        let references = customers.references.expect("references");
        assert_eq!(references.len(), 1);
        assert_eq!(references[0].table, "orders");
        assert_eq!(references[0].columns, ["customer_id"]);
        assert_eq!(references[0].referenced_table, "customers");
        assert_eq!(references[0].referenced_columns, ["id"]);
        assert_eq!(references[0].on_delete.as_deref(), Some("CASCADE"));
        assert_eq!(customers.foreign_keys.expect("fks").len(), 0);

        // `REFERENCES orders` without a column list resolves to orders' PK.
        let orders = table_properties_blocking(&mut conn, "main", "orders").expect("orders");
        assert_eq!(orders.references.expect("references").len(), 1);
        let refunds = table_properties_blocking(&mut conn, "main", "refunds").expect("refunds");
        let fk = &refunds.foreign_keys.expect("fks")[0];
        assert_eq!(fk.referenced_table, "orders");
        assert_eq!(fk.referenced_columns, ["id"]);
    }

    #[test]
    fn view_reports_columns_and_definition_without_keys() {
        let mut conn = fixture();
        let props = table_properties_blocking(&mut conn, "main", "order_skus").expect("view");
        assert_eq!(props.columns.len(), 1);
        assert!(props.keys.expect("keys").is_empty());
        assert!(props.indexes.expect("indexes").is_empty());
        assert!(props.ddl.expect("ddl").starts_with("CREATE VIEW order_skus"));
    }

    #[test]
    fn missing_table_is_an_error_and_identifiers_are_not_injectable() {
        let mut conn = fixture();
        assert!(table_properties_blocking(&mut conn, "main", "nope").is_err());
        assert!(table_properties_blocking(&mut conn, "main", "orders'); DROP TABLE orders;--").is_err());
        assert!(table_properties_blocking(&mut conn, "main\"; DROP TABLE orders;--", "orders").is_err());
        assert!(table_properties_blocking(&mut conn, "main", "orders").is_ok());
    }

    #[test]
    fn check_constraint_scanner_ignores_strings_comments_and_identifiers() {
        let checks = sqlite_check_constraints(
            "CREATE TABLE t (\"check (x)\" TEXT DEFAULT 'CHECK (1)', -- CHECK (2)\n a INT CHECK (a > (1 + 1)))",
        );
        assert_eq!(checks, vec![(None, "a > (1 + 1)".to_string())]);
    }

    #[test]
    fn trigger_header_parser_handles_timing_variants() {
        let parse = sqlite_trigger_timing_and_events;
        assert_eq!(
            parse("CREATE TRIGGER t INSTEAD OF DELETE ON v BEGIN SELECT 1; END"),
            (Some("INSTEAD OF".into()), vec!["DELETE".into()])
        );
        assert_eq!(
            parse("CREATE TEMP TRIGGER IF NOT EXISTS main.t UPDATE ON x BEGIN SELECT 1; END"),
            (Some("BEFORE".into()), vec!["UPDATE".into()])
        );
        assert_eq!(parse("SELECT 1"), (None, Vec::new()));
    }
}

#[cfg(test)]
mod native_type_metadata_tests {
    use super::*;
    use crate::arrow::NATIVE_TYPE_METADATA_KEY;

    fn native(batch: &RecordBatch, column: usize) -> Option<String> {
        batch
            .schema()
            .field(column)
            .metadata()
            .get(NATIVE_TYPE_METADATA_KEY)
            .cloned()
    }

    fn fixture() -> rusqlite::Connection {
        let conn = rusqlite::Connection::open_in_memory().expect("in-memory sqlite");
        conn.execute_batch(
            "CREATE TABLE t (id INTEGER PRIMARY KEY, name VARCHAR(20), at DATETIME, score REAL);
             INSERT INTO t VALUES (1, 'a', '2024-01-01 00:00:00', 1.5);
             INSERT INTO t VALUES (2, 'b', '2024-01-02 00:00:00', 2.5);",
        )
        .expect("fixture schema");
        conn
    }

    #[test]
    fn query_attaches_declared_column_types_as_field_metadata() {
        let mut conn = fixture();
        let batch = query_blocking(&mut conn, "SELECT * FROM t", 100, 0).expect("query");
        assert_eq!(native(&batch, 0).as_deref(), Some("INTEGER"));
        assert_eq!(native(&batch, 1).as_deref(), Some("VARCHAR(20)"));
        assert_eq!(native(&batch, 2).as_deref(), Some("DATETIME"));
        assert_eq!(native(&batch, 3).as_deref(), Some("REAL"));
    }

    #[test]
    fn declared_types_survive_paging_and_empty_pages_and_skip_expressions() {
        let mut conn = fixture();
        let page = query_blocking(&mut conn, "SELECT id, name, 1 + 1 AS two FROM t", 1, 1)
            .expect("second page");
        assert_eq!(page.num_rows(), 1);
        assert_eq!(native(&page, 0).as_deref(), Some("INTEGER"));
        assert_eq!(native(&page, 1).as_deref(), Some("VARCHAR(20)"));
        // Expressions have no declared type: the UI falls back to Arrow.
        assert_eq!(native(&page, 2), None);

        let empty = query_blocking(&mut conn, "SELECT id FROM t", 10, 50).expect("empty page");
        assert_eq!(empty.num_rows(), 0);
        assert_eq!(native(&empty, 0).as_deref(), Some("INTEGER"));
    }
}
