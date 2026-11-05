use async_trait::async_trait;
use std::collections::HashMap;
use std::sync::Arc;
use tokio::sync::Mutex;
use tokio_postgres::Client;

use super::pg_transport::Transport;

use crate::db::postgres_arrow::{
    pg_row_col_to_export_string, pk_value_to_string, rows_to_record_batch,
};
use crate::db::sql_builder::{
    build_count_qualified, build_postgres_insert_from_json_keys, build_postgres_update_from_json,
    build_select_qualified, build_select_with_cursor_qualified, quote_ident, quote_qualified_table,
};
use crate::db::{CursorState, DatabaseConnection, DbType, QueryResult};

const POSTGRES_SQL_OBJECTS_SQL: &str = "SELECT c.relname, c.relkind::text
     FROM pg_class c
     JOIN pg_namespace n ON n.oid = c.relnamespace
     WHERE n.nspname = $1
       AND c.relkind IN ('r', 'p', 'v', 'm')
       AND has_table_privilege(c.oid, 'SELECT')
     ORDER BY c.relkind, c.relname";

/// Same as [`POSTGRES_SQL_OBJECTS_SQL`] plus a bound, escaped, case-insensitive
/// name filter and a bound limit ($2 = `%escaped%`, $3 = limit). Backslash is
/// PostgreSQL's default `LIKE`/`ILIKE` escape character, so no `ESCAPE` clause
/// is needed (and none is emitted) for [`pg_like_contains_pattern`].
pub(crate) const POSTGRES_SQL_OBJECTS_FILTERED_SQL: &str = "SELECT c.relname, c.relkind::text
     FROM pg_class c
     JOIN pg_namespace n ON n.oid = c.relnamespace
     WHERE n.nspname = $1
       AND c.relkind IN ('r', 'p', 'v', 'm')
       AND has_table_privilege(c.oid, 'SELECT')
       AND c.relname ILIKE $2
     ORDER BY c.relkind, c.relname
     LIMIT $3";

/// `%escaped%` pattern for a literal substring `ILIKE` (escapes `\\`, `%`, `_`
/// with PostgreSQL's default backslash escape). Local to this file because it
/// is `include!`d into `dbengine-postgres`, which has no `name_filter` module;
/// kept in sync with `dbengine::name_filter::like_contains_pattern` by a test.
pub(crate) fn pg_like_contains_pattern(filter: &str) -> String {
    let mut pattern = String::with_capacity(filter.len() + 2);
    pattern.push('%');
    for character in filter.chars() {
        if matches!(character, '\\' | '%' | '_') {
            pattern.push('\\');
        }
        pattern.push(character);
    }
    pattern.push('%');
    pattern
}

/// Clamps a caller-provided limit into a valid SQL `LIMIT` (at least 1).
pub(crate) fn pg_sql_limit(limit: usize) -> i64 {
    i64::try_from(limit.max(1)).unwrap_or(i64::MAX)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize)]
#[serde(rename_all = "snake_case")]
pub enum SqlObjectKind {
    Table,
    View,
    MaterializedView,
}

impl SqlObjectKind {
    fn from_postgres_relkind(relkind: &str) -> Option<Self> {
        match relkind {
            "r" | "p" => Some(Self::Table),
            "v" => Some(Self::View),
            "m" => Some(Self::MaterializedView),
            _ => None,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
pub struct SqlObject {
    pub name: String,
    pub kind: SqlObjectKind,
}

/// Column metadata for one object. Mirrors the shared catalog shape but
/// stays local because this file is `include!`d into multiple crates.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
pub struct SqlColumn {
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
    /// `COMMENT ON COLUMN`; filled by `table_properties` only.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub comment: Option<String>,
}

pub struct PostgresConn {
    client: Arc<Client>,
    /// Parsed once; other databases reuse it with only `dbname` replaced (a
    /// database name is never spliced into a connection string).
    config: tokio_postgres::Config,
    /// How sessions and cancel requests reach the server (plain TCP for the
    /// native engine; the v2 driver adds TLS). Every client of this connection
    /// uses it, so a per-database client has the same security as the first.
    transport: Transport,
    default_database: String,
    database_clients: Mutex<HashMap<String, Arc<Client>>>,
    mutation_lock: Mutex<()>,
    /// SQL sent on every client right after it connects, before anything else
    /// (the v2 driver's session settings; `None` for the native engine).
    connect_setup: Option<&'static str>,
}

const POSTGRES_LIST_DATABASES_SQL: &str = "SELECT datname
     FROM pg_database
     WHERE datallowconn
       AND NOT datistemplate
       AND has_database_privilege(datname, 'CONNECT')
     ORDER BY datname";

/// Fetch the full schema/table/column catalog in a single round-trip.
/// Excludes pg_catalog and information_schema themselves.
/// Capped at 20 000 rows; logs a warning if truncated.
const POSTGRES_CATALOG_SQL: &str = "SELECT n.nspname AS table_schema,
            c.relname AS table_name,
            a.attname AS column_name,
            pg_catalog.format_type(a.atttypid, a.atttypmod) AS data_type,
            NOT a.attnotnull AS nullable,
            COALESCE(pk_cols.attnum IS NOT NULL, false) AS is_primary_key,
            col_description(c.oid, a.attnum) AS column_comment,
            obj_description(c.oid, 'pg_class') AS table_comment
     FROM pg_attribute a
     JOIN pg_class c ON c.oid = a.attrelid
     JOIN pg_namespace n ON n.oid = c.relnamespace
     LEFT JOIN LATERAL (
         SELECT unnest(idx.indkey) AS attnum
         FROM pg_index idx
         WHERE idx.indrelid = c.oid AND idx.indisprimary
     ) pk_cols ON pk_cols.attnum = a.attnum
     WHERE n.nspname NOT IN ('pg_catalog', 'information_schema')
       AND n.nspname NOT LIKE 'pg_toast%'
       AND n.nspname NOT LIKE 'pg_temp_%'
       AND c.relkind IN ('r', 'p', 'v', 'm')
       AND a.attnum > 0
       AND NOT a.attisdropped
       AND has_table_privilege(c.oid, 'SELECT')
     ORDER BY n.nspname, c.relname, a.attnum
     LIMIT 20000";

const POSTGRES_CATALOG_INDEXES_SQL: &str = "SELECT n.nspname AS table_schema,
            table_class.relname AS table_name,
            index_class.relname AS index_name,
            ARRAY(
                SELECT a.attname
                FROM unnest(idx.indkey) WITH ORDINALITY AS keys(attnum, ord)
                JOIN pg_attribute a ON a.attrelid = idx.indrelid AND a.attnum = keys.attnum
                ORDER BY keys.ord
            ) AS columns,
            idx.indisunique AS is_unique,
            idx.indisprimary AS is_primary
     FROM pg_index idx
     JOIN pg_class table_class ON table_class.oid = idx.indrelid
     JOIN pg_namespace n ON n.oid = table_class.relnamespace
     JOIN pg_class index_class ON index_class.oid = idx.indexrelid
     WHERE n.nspname NOT IN ('pg_catalog', 'information_schema')
       AND n.nspname NOT LIKE 'pg_toast%'
       AND n.nspname NOT LIKE 'pg_temp_%'
       AND table_class.relkind IN ('r', 'p', 'v', 'm')
       AND has_table_privilege(table_class.oid, 'SELECT')
     ORDER BY n.nspname, table_class.relname, index_class.relname
     LIMIT 20000";

const POSTGRES_CATALOG_FOREIGN_KEYS_SQL: &str =
    "SELECT local_ns.nspname AS table_schema,
            local_table.relname AS table_name,
            con.conname,
            array_agg(att.attname ORDER BY keys.ord) AS columns,
            foreign_ns.nspname AS foreign_schema,
            foreign_table.relname AS foreign_table,
            array_agg(foreign_att.attname ORDER BY keys.ord) AS foreign_columns,
            CASE con.confupdtype
                WHEN 'a' THEN 'NO ACTION'
                WHEN 'r' THEN 'RESTRICT'
                WHEN 'c' THEN 'CASCADE'
                WHEN 'n' THEN 'SET NULL'
                WHEN 'd' THEN 'SET DEFAULT'
                ELSE con.confupdtype::text
            END AS on_update,
            CASE con.confdeltype
                WHEN 'a' THEN 'NO ACTION'
                WHEN 'r' THEN 'RESTRICT'
                WHEN 'c' THEN 'CASCADE'
                WHEN 'n' THEN 'SET NULL'
                WHEN 'd' THEN 'SET DEFAULT'
                ELSE con.confdeltype::text
            END AS on_delete,
            CASE con.confmatchtype
                WHEN 'f' THEN 'FULL'
                WHEN 'p' THEN 'PARTIAL'
                WHEN 's' THEN 'SIMPLE'
                ELSE con.confmatchtype::text
            END AS match_type,
            pg_get_constraintdef(con.oid, true) AS definition
     FROM pg_constraint con
     JOIN pg_class local_table ON local_table.oid = con.conrelid
     JOIN pg_namespace local_ns ON local_ns.oid = local_table.relnamespace
     JOIN pg_class foreign_table ON foreign_table.oid = con.confrelid
     JOIN pg_namespace foreign_ns ON foreign_ns.oid = foreign_table.relnamespace
     JOIN LATERAL unnest(con.conkey, con.confkey) WITH ORDINALITY AS keys(attnum, foreign_attnum, ord) ON true
     JOIN pg_attribute att ON att.attrelid = con.conrelid AND att.attnum = keys.attnum
     JOIN pg_attribute foreign_att ON foreign_att.attrelid = con.confrelid AND foreign_att.attnum = keys.foreign_attnum
     WHERE local_ns.nspname NOT IN ('pg_catalog', 'information_schema')
       AND local_ns.nspname NOT LIKE 'pg_toast%'
       AND local_ns.nspname NOT LIKE 'pg_temp_%'
       AND con.contype = 'f'
       AND has_table_privilege(local_table.oid, 'SELECT')
     GROUP BY con.oid, local_ns.nspname, local_table.relname, con.conname, foreign_ns.nspname, foreign_table.relname
     ORDER BY local_ns.nspname, local_table.relname, con.conname
     LIMIT 20000";

const POSTGRES_TABLE_COLUMNS_SQL: &str = "SELECT a.attname,
            pg_catalog.format_type(a.atttypid, a.atttypmod) AS data_type,
            NOT a.attnotnull AS nullable,
            pg_get_expr(ad.adbin, ad.adrelid) AS default_value,
             a.attnum,
             a.attgenerated <> '' AS is_generated,
             a.attidentity <> '' OR COALESCE(pg_get_expr(ad.adbin, ad.adrelid) LIKE 'nextval(%', false) AS auto_increment,
             EXISTS (
                 SELECT 1 FROM pg_index idx
                 WHERE idx.indrelid = c.oid AND idx.indisprimary AND a.attnum = ANY(idx.indkey)
             ) AS is_primary_key
     FROM pg_attribute a
     JOIN pg_class c ON c.oid = a.attrelid
     JOIN pg_namespace n ON n.oid = c.relnamespace
     LEFT JOIN pg_attrdef ad ON ad.adrelid = a.attrelid AND ad.adnum = a.attnum
     WHERE n.nspname = $1
       AND c.relname = $2
       AND c.relkind IN ('r', 'p', 'v', 'm')
       AND a.attnum > 0
       AND NOT a.attisdropped
      ORDER BY a.attnum";

const POSTGRES_TABLE_CONSTRAINTS_SQL: &str = "SELECT con.conname,
            CASE con.contype
                WHEN 'p' THEN 'PRIMARY KEY'
                WHEN 'u' THEN 'UNIQUE'
                WHEN 'c' THEN 'CHECK'
                WHEN 'x' THEN 'EXCLUDE'
                ELSE con.contype::text
            END AS constraint_type,
            pg_get_constraintdef(con.oid, true) AS definition
     FROM pg_constraint con
     JOIN pg_class c ON c.oid = con.conrelid
     JOIN pg_namespace n ON n.oid = c.relnamespace
     WHERE n.nspname = $1
       AND c.relname = $2
       AND con.contype <> 'f'
     ORDER BY con.conname";

const POSTGRES_TABLE_FOREIGN_KEYS_SQL: &str =
    "SELECT con.conname,
            array_agg(att.attname ORDER BY keys.ord) AS columns,
            foreign_ns.nspname AS foreign_schema,
            foreign_table.relname AS foreign_table,
            array_agg(foreign_att.attname ORDER BY keys.ord) AS foreign_columns,
            pg_get_constraintdef(con.oid, true) AS definition
     FROM pg_constraint con
     JOIN pg_class local_table ON local_table.oid = con.conrelid
     JOIN pg_namespace local_ns ON local_ns.oid = local_table.relnamespace
     JOIN pg_class foreign_table ON foreign_table.oid = con.confrelid
     JOIN pg_namespace foreign_ns ON foreign_ns.oid = foreign_table.relnamespace
     JOIN LATERAL unnest(con.conkey, con.confkey) WITH ORDINALITY AS keys(attnum, foreign_attnum, ord) ON true
     JOIN pg_attribute att ON att.attrelid = con.conrelid AND att.attnum = keys.attnum
     JOIN pg_attribute foreign_att ON foreign_att.attrelid = con.confrelid AND foreign_att.attnum = keys.foreign_attnum
     WHERE local_ns.nspname = $1
       AND local_table.relname = $2
       AND con.contype = 'f'
     GROUP BY con.oid, con.conname, foreign_ns.nspname, foreign_table.relname
     ORDER BY con.conname";

const POSTGRES_TABLE_INDEXES_SQL: &str = "SELECT index_class.relname AS index_name,
            pg_get_indexdef(index_class.oid) AS definition,
            idx.indisunique AS is_unique,
            idx.indisprimary AS is_primary
     FROM pg_index idx
     JOIN pg_class table_class ON table_class.oid = idx.indrelid
     JOIN pg_namespace n ON n.oid = table_class.relnamespace
     JOIN pg_class index_class ON index_class.oid = idx.indexrelid
     JOIN pg_indexes ix ON ix.schemaname = n.nspname
        AND ix.tablename = table_class.relname
        AND ix.indexname = index_class.relname
     WHERE n.nspname = $1
       AND table_class.relname = $2
     ORDER BY index_class.relname";

const POSTGRES_TABLE_TRIGGERS_SQL: &str =
    "SELECT trigger_class.oid,
            trigger_class.tgname,
            trigger_class.tgenabled::text AS enabled_code,
            CASE trigger_class.tgenabled
                WHEN 'O' THEN 'enabled'
                WHEN 'D' THEN 'disabled'
                WHEN 'R' THEN 'replica'
                WHEN 'A' THEN 'always'
                ELSE trigger_class.tgenabled::text
            END AS enabled,
            CASE
                WHEN (trigger_class.tgtype & 2) <> 0 THEN 'BEFORE'
                WHEN (trigger_class.tgtype & 64) <> 0 THEN 'INSTEAD'
                ELSE 'AFTER'
            END AS timing,
            ARRAY_REMOVE(ARRAY[
                CASE WHEN (trigger_class.tgtype & 4) <> 0 THEN 'INSERT' END,
                CASE WHEN (trigger_class.tgtype & 8) <> 0 THEN 'DELETE' END,
                CASE WHEN (trigger_class.tgtype & 16) <> 0 THEN 'UPDATE' END,
                CASE WHEN (trigger_class.tgtype & 32) <> 0 THEN 'TRUNCATE' END
            ], NULL)::text[] AS events,
            CASE WHEN (trigger_class.tgtype & 1) <> 0 THEN 'ROW' ELSE 'STATEMENT' END AS level,
            pg_get_expr(trigger_class.tgqual, trigger_class.tgrelid) AS when_expression,
            function_namespace.nspname AS function_schema,
            function_class.proname AS function_name,
            description.description,
            pg_get_triggerdef(trigger_class.oid, true) AS definition
     FROM pg_trigger trigger_class
     JOIN pg_class table_class ON table_class.oid = trigger_class.tgrelid
     JOIN pg_namespace n ON n.oid = table_class.relnamespace
     JOIN pg_proc function_class ON function_class.oid = trigger_class.tgfoid
     JOIN pg_namespace function_namespace ON function_namespace.oid = function_class.pronamespace
     LEFT JOIN pg_description description ON description.objoid = trigger_class.oid AND description.objsubid = 0
     WHERE n.nspname = $1
       AND table_class.relname = $2
       AND NOT trigger_class.tgisinternal
     ORDER BY trigger_class.tgname";

const POSTGRES_FUNCTIONS_SQL: &str =
    "SELECT p.oid,
            n.nspname AS schema_name,
            p.proname AS name,
            pg_catalog.pg_get_function_identity_arguments(p.oid) AS arguments,
            pg_catalog.format_type(p.prorettype, NULL) AS return_type,
            l.lanname AS language,
            p.prokind::text AS kind,
            description.description,
            pg_catalog.pg_get_functiondef(p.oid) AS definition
     FROM pg_catalog.pg_proc p
     JOIN pg_catalog.pg_namespace n ON n.oid = p.pronamespace
     JOIN pg_catalog.pg_language l ON l.oid = p.prolang
     LEFT JOIN pg_catalog.pg_description description
        ON description.objoid = p.oid
       AND description.classoid = 'pg_proc'::regclass
       AND description.objsubid = 0
     WHERE n.nspname = $1
       AND p.prokind IN ('f', 'p')
       AND ($2::bool = false OR (p.prokind = 'f' AND pg_catalog.format_type(p.prorettype, NULL) = 'trigger'))
     ORDER BY p.proname, arguments";

/// Prefix of the `table_properties` error for an object that does not exist
/// (drivers map it to a typed not-found code).
pub(crate) const TABLE_NOT_FOUND_PREFIX: &str = "Table or view not found: ";

/// `tokio_postgres::Error`'s Display prints only "db error" for server-side
/// failures; surface the server's severity/message/detail/hint instead.
fn postgres_error_detail(error: &tokio_postgres::Error) -> String {
    match error.as_db_error() {
        Some(db) => db.to_string(),
        None => error.to_string(),
    }
}

impl PostgresConn {
    pub async fn connect(uri: &str) -> Result<Self, String> {
        let (config, transport) =
            Transport::prepare(uri).map_err(|e| format!("Postgres connect failed: {e}"))?;
        let client = transport
            .connect(&config)
            .await
            .map_err(|e| format!("Postgres connect failed: {}", postgres_error_detail(&e)))?;

        let default_database = current_database_for_client(&client).await?;

        Ok(Self {
            client: Arc::new(client),
            config,
            transport,
            default_database,
            database_clients: Mutex::new(HashMap::new()),
            mutation_lock: Mutex::new(()),
            connect_setup: None,
        })
    }

    /// Runs `connect_setup` on a client that just connected. A server that does
    /// not know a setting (an error it answered) is not a failed connection; a
    /// broken connection is.
    async fn run_connect_setup(&self, client: &Client) -> Result<(), String> {
        let Some(setup) = self.connect_setup else {
            return Ok(());
        };
        match client.batch_execute(setup).await {
            Err(error) if error.as_db_error().is_none() => Err(postgres_error_detail(&error)),
            _ => Ok(()),
        }
    }

    pub fn default_database(&self) -> &str {
        &self.default_database
    }

    /// Databases this server will let us connect to.
    pub async fn list_databases(&self) -> Result<Vec<String>, String> {
        let rows = self
            .client
            .query(POSTGRES_LIST_DATABASES_SQL, &[])
            .await
            .map_err(|e| e.to_string())?;
        rows.iter().map(|row| catalog_cell(row, 0)).collect()
    }

    pub async fn create_database(&self, name: &str) -> Result<(), crate::error::DbEngineError> {
        let statement =
            create_database_sql(name).map_err(crate::error::DbEngineError::CreateRejected)?;
        // Serialize with existing write transactions, but do not open a transaction:
        // PostgreSQL rejects CREATE DATABASE inside BEGIN/COMMIT.
        let _lock = self.mutation_lock.lock().await;
        self.client
            .batch_execute(&statement)
            .await
            .map_err(|error| {
                if error.as_db_error().is_some() {
                    crate::error::DbEngineError::CreateRejected(error.to_string())
                } else {
                    crate::error::DbEngineError::Query(error.to_string())
                }
            })
    }

    async fn client_for_database(&self, database: &str) -> Result<Arc<Client>, String> {
        let database = if database.trim().is_empty() {
            self.default_database.as_str()
        } else {
            database
        };

        if database == self.default_database {
            return Ok(self.client.clone());
        }

        if let Some(client) = self.database_clients.lock().await.get(database).cloned() {
            return Ok(client);
        }

        let client = self
            .transport
            .connect(&postgres_config_with_database(&self.config, database))
            .await
            .map_err(|e| format!("Postgres connect to database '{database}' failed: {}", postgres_error_detail(&e)))?;
        self.run_connect_setup(&client).await?;

        let client = Arc::new(client);
        let mut clients = self.database_clients.lock().await;
        let entry = clients
            .entry(database.to_string())
            .or_insert_with(|| client.clone());
        Ok(entry.clone())
    }

    /// Detect primary key column + its Postgres type name (for cursor casting).
    /// Returns (column_name, pg_type_name) e.g. ("id", "int8").
    async fn detect_primary_key(
        &self,
        database: &str,
        schema: &str,
        table: &str,
    ) -> Result<(String, String), String> {
        let qualified_table = quote_qualified_table(schema, table);
        let client = self.client_for_database(database).await?;
        let rows = client
            .query(
                "SELECT a.attname, t.typname
                 FROM pg_index i
                 JOIN pg_attribute a ON a.attrelid = i.indrelid AND a.attnum = ANY(i.indkey)
                 JOIN pg_type t ON t.oid = a.atttypid
                 WHERE i.indrelid = $1::regclass AND i.indisprimary
                 ORDER BY array_position(i.indkey, a.attnum)
                 LIMIT 1",
                &[&qualified_table],
            )
            .await
            .map_err(|e| e.to_string())?;

        if let Some(row) = rows.first() {
            let col: String = row.get(0);
            let type_name: String = row.get(1);
            Ok((col, type_name))
        } else {
            Ok(("id".to_string(), "int8".to_string()))
        }
    }

    /// Fetch the complete schema/table/column catalog in one SQL round-trip.
    /// Returns a JSON string with the shape:
    /// `{"schemas":[{"name":"public","tables":[{"name":"users","columns":[{"name":"id","data_type":"integer"},...]},...]},...]}`.
    pub async fn list_catalog(&self, db: &str) -> Result<String, String> {
        let client = self.client_for_database(db).await?;
        let rows = client
            .query(POSTGRES_CATALOG_SQL, &[])
            .await
            .map_err(|e| e.to_string())?;
        let index_rows = client
            .query(POSTGRES_CATALOG_INDEXES_SQL, &[])
            .await
            .map_err(|e| e.to_string())?;
        let foreign_key_rows = client
            .query(POSTGRES_CATALOG_FOREIGN_KEYS_SQL, &[])
            .await
            .map_err(|e| e.to_string())?;

        if rows.len() >= 20000 {
            eprintln!("[postgres] catalog query hit 20000 row cap for db '{db}'; some columns may be missing");
        }

        let mut indexes_by_table: HashMap<(String, String), Vec<serde_json::Value>> =
            HashMap::new();
        for row in &index_rows {
            let schema_name: String = row.get(0);
            let table_name: String = row.get(1);
            let index_name: String = row.get(2);
            let columns: Vec<String> = row.get(3);
            let is_unique: bool = row.get(4);
            let is_primary: bool = row.get(5);
            indexes_by_table
                .entry((schema_name, table_name))
                .or_default()
                .push(serde_json::json!({
                    "name": index_name,
                    "columns": columns,
                    "is_unique": is_unique,
                    "is_primary": is_primary,
                }));
        }

        let mut foreign_keys_by_table: HashMap<(String, String), Vec<serde_json::Value>> =
            HashMap::new();
        for row in &foreign_key_rows {
            let schema_name: String = row.get(0);
            let table_name: String = row.get(1);
            let name: String = row.get(2);
            let columns: Vec<String> = row.get(3);
            let foreign_schema: String = row.get(4);
            let foreign_table: String = row.get(5);
            let foreign_columns: Vec<String> = row.get(6);
            let on_update: String = row.get(7);
            let on_delete: String = row.get(8);
            let match_type: String = row.get(9);
            let definition: String = row.get(10);
            foreign_keys_by_table
                .entry((schema_name, table_name))
                .or_default()
                .push(serde_json::json!({
                    "name": name,
                    "columns": columns,
                    "foreign_schema": foreign_schema,
                    "foreign_table": foreign_table,
                    "foreign_columns": foreign_columns,
                    "on_update": on_update,
                    "on_delete": on_delete,
                    "match_type": match_type,
                    "definition": definition,
                }));
        }

        // Group rows into schema→table→columns
        let mut schemas: Vec<serde_json::Value> = Vec::new();
        let mut cur_schema = String::new();
        let mut cur_table = String::new();
        let mut cur_table_comment: Option<String> = None;
        let mut cur_columns: Vec<serde_json::Value> = Vec::new();
        let mut cur_tables: Vec<serde_json::Value> = Vec::new();

        let flush_table = |cur_table: &str,
                           cur_schema: &str,
                           cur_table_comment: &mut Option<String>,
                           cur_columns: &mut Vec<serde_json::Value>,
                           cur_tables: &mut Vec<serde_json::Value>| {
            if !cur_table.is_empty() {
                let key = (cur_schema.to_string(), cur_table.to_string());
                cur_tables.push(serde_json::json!({
                    "name": cur_table,
                    "columns": std::mem::take(cur_columns),
                    "indexes": indexes_by_table.get(&key).cloned().unwrap_or_default(),
                    "foreign_keys": foreign_keys_by_table.get(&key).cloned().unwrap_or_default(),
                    "comment": cur_table_comment.take(),
                }));
            }
        };

        let flush_schema = |cur_schema: &str,
                            cur_tables: &mut Vec<serde_json::Value>,
                            schemas: &mut Vec<serde_json::Value>| {
            if !cur_schema.is_empty() {
                schemas.push(serde_json::json!({
                    "name": cur_schema,
                    "tables": std::mem::take(cur_tables),
                }));
            }
        };

        for row in &rows {
            let schema_name: String = row.get(0);
            let table_name: String = row.get(1);
            let column_name: String = row.get(2);
            let data_type: String = row.get(3);
            let nullable: bool = row.get(4);
            let is_primary_key: bool = row.get(5);
            let column_comment: Option<String> = row.get(6);
            let table_comment: Option<String> = row.get(7);

            if schema_name != cur_schema {
                flush_table(
                    &cur_table,
                    &cur_schema,
                    &mut cur_table_comment,
                    &mut cur_columns,
                    &mut cur_tables,
                );
                flush_schema(&cur_schema, &mut cur_tables, &mut schemas);
                cur_schema = schema_name;
                cur_table = String::new();
            }
            if table_name != cur_table {
                flush_table(
                    &cur_table,
                    &cur_schema,
                    &mut cur_table_comment,
                    &mut cur_columns,
                    &mut cur_tables,
                );
                cur_table = table_name;
                cur_table_comment = table_comment;
            }
            cur_columns.push(serde_json::json!({
                "name": column_name,
                "data_type": data_type,
                "nullable": nullable,
                "is_primary_key": is_primary_key,
                "comment": column_comment,
            }));
        }
        flush_table(
            &cur_table,
            &cur_schema,
            &mut cur_table_comment,
            &mut cur_columns,
            &mut cur_tables,
        );
        flush_schema(&cur_schema, &mut cur_tables, &mut schemas);

        serde_json::to_string(&serde_json::json!({ "schemas": schemas })).map_err(|e| e.to_string())
    }

    /// Fetch table-specific metadata for sidebar expansion.
    /// Returns compact JSON with columns, constraints, foreign_keys, indexes, and triggers.
    pub async fn table_metadata(
        &self,
        db: &str,
        schema: &str,
        table: &str,
    ) -> Result<String, String> {
        let client = self.client_for_database(db).await?;

        let object_rows = client
            .query(
                "SELECT c.oid::text, c.relkind::text
                 FROM pg_class c
                 JOIN pg_namespace n ON n.oid = c.relnamespace
                 WHERE n.nspname = $1
                   AND c.relname = $2
                 ORDER BY c.oid",
                &[&schema, &table],
            )
            .await
            .map_err(|e| e.to_string())?;
        let object_summary = object_rows
            .iter()
            .map(|row| {
                let oid: String = row.get(0);
                let relkind: String = row.get(1);
                format!("{oid}:{relkind}")
            })
            .collect::<Vec<_>>()
            .join(",");
        eprintln!(
            "[postgres] table_metadata lookup db={db} schema={schema} table={table} objects={} [{}]",
            object_rows.len(),
            object_summary
        );

        let column_rows = client
            .query(POSTGRES_TABLE_COLUMNS_SQL, &[&schema, &table])
            .await
            .map_err(|e| e.to_string())?;
        eprintln!(
            "[postgres] table_metadata columns db={db} schema={schema} table={table} rows={}",
            column_rows.len()
        );
        let constraint_rows = client
            .query(POSTGRES_TABLE_CONSTRAINTS_SQL, &[&schema, &table])
            .await
            .map_err(|e| e.to_string())?;
        let foreign_key_rows = client
            .query(POSTGRES_TABLE_FOREIGN_KEYS_SQL, &[&schema, &table])
            .await
            .map_err(|e| e.to_string())?;
        let index_rows = client
            .query(POSTGRES_TABLE_INDEXES_SQL, &[&schema, &table])
            .await
            .map_err(|e| e.to_string())?;
        let trigger_rows = client
            .query(POSTGRES_TABLE_TRIGGERS_SQL, &[&schema, &table])
            .await
            .map_err(|e| e.to_string())?;

        let columns: Vec<serde_json::Value> = column_rows
            .iter()
            .map(|row| {
                serde_json::json!({
                    "name": row.get::<_, String>(0),
                    "data_type": row.get::<_, String>(1),
                    "nullable": row.get::<_, bool>(2),
                    "default": row.get::<_, Option<String>>(3),
                    "ordinal": row.get::<_, i16>(4),
                })
            })
            .collect();

        let constraints: Vec<serde_json::Value> = constraint_rows
            .iter()
            .map(|row| {
                serde_json::json!({
                    "name": row.get::<_, String>(0),
                    "type": row.get::<_, String>(1),
                    "definition": row.get::<_, String>(2),
                })
            })
            .collect();

        let foreign_keys: Vec<serde_json::Value> = foreign_key_rows
            .iter()
            .map(|row| {
                serde_json::json!({
                    "name": row.get::<_, String>(0),
                    "columns": row.get::<_, Vec<String>>(1),
                    "foreign_schema": row.get::<_, String>(2),
                    "foreign_table": row.get::<_, String>(3),
                    "foreign_columns": row.get::<_, Vec<String>>(4),
                    "definition": row.get::<_, String>(5),
                })
            })
            .collect();

        let indexes: Vec<serde_json::Value> = index_rows
            .iter()
            .map(|row| {
                serde_json::json!({
                    "name": row.get::<_, String>(0),
                    "definition": row.get::<_, String>(1),
                    "is_unique": row.get::<_, bool>(2),
                    "is_primary": row.get::<_, bool>(3),
                })
            })
            .collect();

        let triggers: Vec<serde_json::Value> = trigger_rows
            .iter()
            .map(|row| {
                serde_json::json!({
                    "oid": row.get::<_, u32>(0),
                    "name": row.get::<_, String>(1),
                    "enabled_code": row.get::<_, String>(2),
                    "enabled": row.get::<_, String>(3),
                    "timing": row.get::<_, String>(4),
                    "events": row.get::<_, Vec<String>>(5),
                    "level": row.get::<_, String>(6),
                    "when_expression": row.get::<_, Option<String>>(7),
                    "function_schema": row.get::<_, String>(8),
                    "function_name": row.get::<_, String>(9),
                    "description": row.get::<_, Option<String>>(10),
                    "definition": row.get::<_, String>(11),
                })
            })
            .collect();

        serde_json::to_string(&serde_json::json!({
            "columns": columns,
            "constraints": constraints,
            "foreign_keys": foreign_keys,
            "indexes": indexes,
            "triggers": triggers,
        }))
        .map_err(|e| e.to_string())
    }

    pub async fn view_definition(
        &self,
        db: &str,
        schema: &str,
        name: &str,
    ) -> Result<String, String> {
        let row = self
            .client_for_database(db)
            .await?
            .query_opt(
                "SELECT c.relkind::text, pg_get_viewdef(c.oid, true) AS definition
                 FROM pg_class c
                 JOIN pg_namespace n ON n.oid = c.relnamespace
                 WHERE n.nspname = $1
                   AND c.relname = $2
                   AND c.relkind IN ('v', 'm')",
                &[&schema, &name],
            )
            .await
            .map_err(|e| e.to_string())?;

        let Some(row) = row else {
            return Err(format!("View not found: {schema}.{name}"));
        };
        let relkind: String = row.get(0);
        let definition: String = row.get(1);
        serde_json::to_string(&serde_json::json!({
            "relkind": relkind,
            "definition": definition,
        }))
        .map_err(|e| e.to_string())
    }

    pub async fn list_functions(
        &self,
        db: &str,
        schema: &str,
        trigger_only: bool,
    ) -> Result<String, String> {
        let rows = self
            .client_for_database(db)
            .await?
            .query(POSTGRES_FUNCTIONS_SQL, &[&schema, &trigger_only])
            .await
            .map_err(|e| e.to_string())?;

        let functions: Vec<serde_json::Value> = rows
            .iter()
            .map(|row| {
                serde_json::json!({
                    "oid": row.get::<_, u32>(0),
                    "schema": row.get::<_, String>(1),
                    "name": row.get::<_, String>(2),
                    "arguments": row.get::<_, String>(3),
                    "return_type": row.get::<_, String>(4),
                    "language": row.get::<_, String>(5),
                    "kind": row.get::<_, String>(6),
                    "description": row.get::<_, Option<String>>(7),
                    "definition": row.get::<_, String>(8),
                })
            })
            .collect();

        serde_json::to_string(&serde_json::json!({ "functions": functions }))
            .map_err(|e| e.to_string())
    }

    pub async fn server_info(&self) -> Result<crate::ServerInfoResponse, String> {
        let row = self
            .client
            .query_one("SELECT version()", &[])
            .await
            .map_err(|e| e.to_string())?;

        let version_full: String = row.get(0);
        let version = postgres_server_version_number(&version_full).to_string();

        Ok(crate::ServerInfoResponse {
            version,
            git_version: String::new(),
            engine: "PostgreSQL".to_string(),
            host: String::new(),
        })
    }

    pub async fn list_schemas(&self, db: &str) -> Result<Vec<String>, String> {
        let client = self.client_for_database(db).await?;
        let rows = client
            .query(
                "SELECT n.nspname
                 FROM pg_namespace n
                 WHERE n.nspname NOT IN ('pg_catalog', 'information_schema')
                   AND n.nspname NOT LIKE 'pg_toast%'
                   AND n.nspname NOT LIKE 'pg_temp_%'
                   AND has_schema_privilege(n.oid, 'USAGE')
                   AND (
                       -- a schema owned by the role, or by a role it is a member of, is listed
                       -- while it is still empty (the one New Schema just created); others need
                       -- a readable table to show
                       pg_has_role(n.nspowner, 'USAGE')
                       OR EXISTS (
                           SELECT 1
                           FROM pg_class c
                           WHERE c.relnamespace = n.oid
                             AND c.relkind IN ('r', 'p', 'v', 'm')
                             AND has_table_privilege(c.oid, 'SELECT')
                       )
                   )
                 ORDER BY n.nspname",
                &[],
            )
            .await
            .map_err(|e| e.to_string())?;

        rows.iter().map(|row| catalog_cell(row, 0)).collect()
    }

    pub async fn list_tables(&self, db: &str, schema: &str) -> Result<Vec<String>, String> {
        let rows = self
            .client_for_database(db)
            .await?
            .query(
                "SELECT c.relname
                 FROM pg_class c
                 JOIN pg_namespace n ON n.oid = c.relnamespace
                 WHERE n.nspname = $1
                   AND c.relkind IN ('r', 'p')
                   AND has_table_privilege(c.oid, 'SELECT')
                 ORDER BY c.relname",
                &[&schema],
            )
            .await
            .map_err(|e| e.to_string())?;

        Ok(rows.into_iter().map(|r| r.get::<_, String>(0)).collect())
    }

    pub async fn list_sql_objects(&self, db: &str, schema: &str) -> Result<Vec<SqlObject>, String> {
        let rows = self
            .client_for_database(db)
            .await?
            .query(POSTGRES_SQL_OBJECTS_SQL, &[&schema])
            .await
            .map_err(|e| e.to_string())?;

        Ok(rows
            .into_iter()
            .filter_map(|row| {
                let name: String = row.get(0);
                let relkind: String = row.get(1);
                SqlObjectKind::from_postgres_relkind(&relkind).map(|kind| SqlObject { name, kind })
            })
            .collect())
    }

    /// Like [`Self::list_sql_objects`] but only objects whose name contains
    /// `filter` (case-insensitive, literal), at most `limit` rows. The filter
    /// and limit are bound parameters.
    pub async fn list_sql_objects_filtered(
        &self,
        db: &str,
        schema: &str,
        filter: &str,
        limit: usize,
    ) -> Result<Vec<SqlObject>, String> {
        let pattern = pg_like_contains_pattern(filter);
        let limit = pg_sql_limit(limit);
        let rows = self
            .client_for_database(db)
            .await?
            .query(POSTGRES_SQL_OBJECTS_FILTERED_SQL, &[&schema, &pattern, &limit])
            .await
            .map_err(|e| e.to_string())?;

        Ok(rows
            .into_iter()
            .filter_map(|row| {
                let name: String = row.get(0);
                let relkind: String = row.get(1);
                SqlObjectKind::from_postgres_relkind(&relkind).map(|kind| SqlObject { name, kind })
            })
            .collect())
    }

    /// Column metadata for one object, without the full catalog round-trip.
    pub async fn list_columns(
        &self,
        db: &str,
        schema: &str,
        table: &str,
    ) -> Result<Vec<SqlColumn>, String> {
        let rows = self
            .client_for_database(db)
            .await?
            .query(POSTGRES_TABLE_COLUMNS_SQL, &[&schema, &table])
            .await
            .map_err(|e| e.to_string())?;

        rows.iter()
            .map(|row| {
                Ok(SqlColumn {
                    name: catalog_cell(row, 0)?,
                    data_type: catalog_cell(row, 1)?,
                    nullable: catalog_cell(row, 2)?,
                    primary_key: catalog_cell(row, 7)?,
                    default_expression: catalog_cell(row, 3)?,
                    is_generated: Some(catalog_cell(row, 5)?),
                    auto_increment: Some(catalog_cell(row, 6)?),
                    comment: None,
                })
            })
            .collect()
    }

    /// Requests server-side cancellation of the currently running query on
    /// this connection. The interrupted query fails with a cancellation error.
    pub async fn cancel_query(&self, database: &str) {
        let Ok(client) = self.client_for_database(database).await else {
            return;
        };
        let _ = self.transport.cancel(&client.cancel_token()).await;
    }

    pub async fn execute_sql(
        &self,
        db: &str,
        sql: &str,
        limit: i64,
    ) -> Result<QueryResult, String> {
        self.execute_sql_page(db, sql, limit, 0).await
    }

    pub async fn execute_sql_page(
        &self,
        db: &str,
        sql: &str,
        limit: i64,
        offset: i64,
    ) -> Result<QueryResult, String> {
        if !is_read_only_sql(sql) {
            return Err("Only read-only SELECT queries are supported".to_string());
        }

        let trimmed_sql = sql.trim().trim_end_matches(';').trim();
        if trimmed_sql.is_empty() {
            return Err("SQL query cannot be empty".to_string());
        }

        let effective_limit = if limit > 0 { limit } else { 200 };
        let effective_offset = offset.max(0);
        let wrapped_sql = format!(
            "SELECT * FROM ({}) AS tableleaf_raw_query LIMIT {} OFFSET {}",
            trimmed_sql, effective_limit, effective_offset
        );

        let rows = self
            .client_for_database(db)
            .await?
            .query(&wrapped_sql, &[])
            .await
            .map_err(|e| format!("Query failed: {} | SQL: {}", e, trimmed_sql))?;

        let row_count = rows.len() as i64;
        let batches = if rows.is_empty() {
            vec![]
        } else {
            vec![rows_to_record_batch(&rows)?]
        };

        Ok(QueryResult {
            batches,
            total_count: row_count,
            cursor: None,
        })
    }

    pub async fn execute_mutation(&self, db: &str, sql: &str) -> Result<u64, String> {
        self.execute_mutations(db, &[sql.to_string()]).await
    }

    pub async fn execute_mutations(&self, db: &str, sql: &[String]) -> Result<u64, String> {
        self.execute_mutations_checked(db, sql, false).await
    }

    pub async fn execute_mutations_checked(
        &self,
        db: &str,
        sql: &[String],
        require_affected: bool,
    ) -> Result<u64, String> {
        let _lock = self.mutation_lock.lock().await;
        if require_affected {
            let database = if db.trim().is_empty() {
                self.default_database.as_str()
            } else {
                db
            };
            let mut client = self
                .transport
                .connect(&postgres_config_with_database(&self.config, database))
                .await
                .map_err(|error| format!("Postgres mutation connection failed: {}", postgres_error_detail(&error)))?;
            let transaction = client
                .transaction()
                .await
                .map_err(|error| error.to_string())?;
            let mut affected = 0;
            for statement in sql {
                match transaction.execute(statement, &[]).await {
                    Ok(0) => {
                        transaction.rollback().await.map_err(|error| {
                            format!("SQL mutation affected no rows and rollback failed: {error}")
                        })?;
                        return Err("SQL mutation affected no rows; transaction rolled back".into());
                    }
                    Ok(count) => affected += count,
                    Err(error) => {
                        transaction.rollback().await.map_err(|rollback| {
                            format!("SQL mutation failed: {error}; rollback failed: {rollback}")
                        })?;
                        return Err(format!("SQL mutation failed and rolled back: {error}"));
                    }
                }
            }
            transaction
                .commit()
                .await
                .map_err(|error| format!("SQL mutation commit failed: {error}"))?;
            return Ok(affected);
        }
        let client = self.client_for_database(db).await?;
        client
            .batch_execute("BEGIN")
            .await
            .map_err(|error| format!("Mutation transaction failed to begin: {error}"))?;
        let mut affected = 0;
        for statement in sql {
            match client.execute(statement, &[]).await {
                Ok(count) if require_affected && count == 0 => {
                    let _ = client.batch_execute("ROLLBACK").await;
                    return Err("SQL mutation affected no rows; transaction rolled back".into());
                }
                Ok(count) => affected += count,
                Err(error) => {
                    let _ = client.batch_execute("ROLLBACK").await;
                    return Err(format!("Mutation failed: {error}"));
                }
            }
        }
        client
            .batch_execute("COMMIT")
            .await
            .map_err(|error| format!("Mutation transaction failed to commit: {error}"))?;
        Ok(affected)
    }

    pub async fn execute_query_in_schema(
        &self,
        db: &str,
        schema: &str,
        table: &str,
        user_where: Option<&str>,
        user_order_by: Option<&str>,
        cursor: Option<&CursorState>,
        limit: i64,
    ) -> Result<QueryResult, String> {
        let (sql, params) = if let Some(c) = cursor {
            build_select_with_cursor_qualified(schema, table, user_where, user_order_by, c, limit)?
        } else {
            let s = build_select_qualified(schema, table, user_where, user_order_by, limit)?;
            (s, Vec::new())
        };

        let count_sql = build_count_qualified(schema, table, user_where)?;

        let client = self.client_for_database(db).await?;
        let sql_clone = sql.clone();
        let params_refs: Vec<&(dyn tokio_postgres::types::ToSql + Sync)> = params
            .iter()
            .map(|p| p as &(dyn tokio_postgres::types::ToSql + Sync))
            .collect();

        let (data_result, count_result) = tokio::join!(
            client.query(&sql, &params_refs),
            client.query_one(&count_sql, &[])
        );

        let rows = data_result.map_err(|e| format!("Query failed: {} | SQL: {}", e, sql_clone))?;
        let total_count = count_result.map(|r| r.get::<_, i64>(0)).unwrap_or(0);

        let next_cursor = if rows.len() as i64 >= limit && !rows.is_empty() {
            let last_row = rows.last().expect("rows is not empty");
            let (pk_col, pk_type_name) = if let Some(c) = cursor {
                (c.order_column.clone(), c.pk_type.clone())
            } else {
                self.detect_primary_key(db, schema, table)
                    .await
                    .unwrap_or_else(|_| ("id".to_string(), "int8".to_string()))
            };

            let pk_col_idx = last_row.columns().iter().position(|c| c.name() == pk_col);
            if let Some(col_idx) = pk_col_idx {
                let pg_type = last_row.columns()[col_idx].type_().clone();
                let last_value = pk_value_to_string(last_row, col_idx, &pg_type);

                last_value.map(|v| CursorState {
                    order_column: pk_col,
                    last_value: v,
                    pk_type: pk_type_name,
                })
            } else {
                None
            }
        } else {
            None
        };

        let batches = if rows.is_empty() {
            vec![]
        } else {
            vec![rows_to_record_batch(&rows)?]
        };

        Ok(QueryResult {
            batches,
            total_count,
            cursor: next_cursor,
        })
    }

    pub async fn export_query_page(
        &self,
        db: &str,
        schema: &str,
        table: &str,
        user_where: Option<&str>,
        user_order_by: Option<&str>,
        limit: i64,
        offset: i64,
    ) -> Result<(Vec<String>, Vec<Vec<String>>), String> {
        let mut sql = build_select_qualified(schema, table, user_where, user_order_by, limit)?;
        sql.push_str(&format!(" OFFSET {}", offset.max(0)));
        let rows = self
            .client_for_database(db)
            .await?
            .query(&sql, &[])
            .await
            .map_err(|e| format!("Query failed: {} | SQL: {}", e, sql))?;
        let columns = rows
            .first()
            .map(|row| {
                row.columns()
                    .iter()
                    .map(|column| column.name().to_string())
                    .collect()
            })
            .unwrap_or_default();
        let values = rows
            .iter()
            .map(|row| {
                row.columns()
                    .iter()
                    .enumerate()
                    .map(|(index, column)| {
                        pg_row_col_to_export_string(row, index, column.type_())
                            .unwrap_or_else(|| "null".to_string())
                    })
                    .collect::<Vec<_>>()
            })
            .collect::<Vec<_>>();
        Ok((columns, values))
    }

    pub async fn insert_row_from_json(
        &self,
        db: &str,
        schema: &str,
        table: &str,
        row_json: &str,
    ) -> Result<u64, String> {
        let row_value = parse_postgres_json_param(row_json)?;
        let mut columns = row_value
            .as_object()
            .map(|object| object.keys().cloned().collect::<Vec<_>>())
            .unwrap_or_default();
        columns.sort();
        let sql = build_postgres_insert_from_json_keys(schema, table, &columns)?;
        self.client_for_database(db)
            .await?
            .execute(&sql, &[&row_value])
            .await
            .map_err(|e| format_postgres_execute_error("Insert failed", &e, &sql))
    }

    pub async fn batch_update_from_json(
        &self,
        db: &str,
        schema: &str,
        table: &str,
        key_column: &str,
        updates_json: &str,
    ) -> Result<u64, String> {
        let updates: Vec<PostgresUpdatePatch> = serde_json::from_str(updates_json)
            .map_err(|e| format!("Invalid update JSON: {}", e))?;
        let mut changed = 0;

        for update in updates {
            let serde_json::Value::Object(changes) = update.changes else {
                return Err("Update changes must be a JSON object".to_string());
            };
            if changes.is_empty() {
                continue;
            }

            let mut columns = changes.keys().cloned().collect::<Vec<_>>();
            columns.sort();
            let patch_value = serde_json::Value::Object(changes);
            let sql = build_postgres_update_from_json(schema, table, key_column, &columns)?;
            let params: [&(dyn tokio_postgres::types::ToSql + Sync); 2] =
                [&patch_value, &update.key];
            changed += self
                .client_for_database(db)
                .await?
                .execute(&sql, &params)
                .await
                .map_err(|e| format_postgres_execute_error("Update failed", &e, &sql))?;
        }

        Ok(changed)
    }

    pub async fn enable_trigger(
        &self,
        db: &str,
        schema: &str,
        table: &str,
        trigger: &str,
    ) -> Result<u64, String> {
        self.set_trigger_enabled(db, schema, table, trigger, true)
            .await
    }

    pub async fn disable_trigger(
        &self,
        db: &str,
        schema: &str,
        table: &str,
        trigger: &str,
    ) -> Result<u64, String> {
        self.set_trigger_enabled(db, schema, table, trigger, false)
            .await
    }

    async fn set_trigger_enabled(
        &self,
        db: &str,
        schema: &str,
        table: &str,
        trigger: &str,
        enabled: bool,
    ) -> Result<u64, String> {
        validate_identifier(trigger, "trigger")?;
        let action = if enabled { "ENABLE" } else { "DISABLE" };
        let sql = format!(
            "ALTER TABLE {} {} TRIGGER {}",
            quote_qualified_table(schema, table),
            action,
            quote_ident(trigger)
        );
        self.client_for_database(db)
            .await?
            .execute(&sql, &[])
            .await
            .map_err(|e| format_postgres_execute_error("Trigger state update failed", &e, &sql))
    }

    pub async fn drop_trigger(
        &self,
        db: &str,
        schema: &str,
        table: &str,
        trigger: &str,
    ) -> Result<u64, String> {
        validate_identifier(trigger, "trigger")?;
        let sql = format!(
            "DROP TRIGGER {} ON {}",
            quote_ident(trigger),
            quote_qualified_table(schema, table)
        );
        self.client_for_database(db)
            .await?
            .execute(&sql, &[])
            .await
            .map_err(|e| format_postgres_execute_error("Drop trigger failed", &e, &sql))
    }

    pub async fn rename_trigger(
        &self,
        db: &str,
        schema: &str,
        table: &str,
        old_name: &str,
        new_name: &str,
    ) -> Result<u64, String> {
        validate_identifier(old_name, "old trigger")?;
        validate_identifier(new_name, "new trigger")?;
        let sql = format!(
            "ALTER TRIGGER {} ON {} RENAME TO {}",
            quote_ident(old_name),
            quote_qualified_table(schema, table),
            quote_ident(new_name)
        );
        self.client_for_database(db)
            .await?
            .execute(&sql, &[])
            .await
            .map_err(|e| format_postgres_execute_error("Rename trigger failed", &e, &sql))
    }

    pub async fn replace_trigger(
        &self,
        db: &str,
        schema: &str,
        table: &str,
        old_name: &str,
        ddl: &str,
    ) -> Result<(), String> {
        validate_identifier(old_name, "trigger")?;
        if ddl.trim().is_empty() {
            return Err("Trigger DDL cannot be empty".to_string());
        }
        let drop_sql = format!(
            "DROP TRIGGER IF EXISTS {} ON {}",
            quote_ident(old_name),
            quote_qualified_table(schema, table)
        );
        let sql = format!(
            "BEGIN;\n{};\n{};\nCOMMIT;",
            drop_sql,
            ddl.trim().trim_end_matches(';')
        );
        self.client_for_database(db)
            .await?
            .batch_execute(&sql)
            .await
            .map_err(|e| format_postgres_execute_error("Replace trigger failed", &e, &sql))
    }

    pub async fn create_trigger(&self, db: &str, ddl: &str) -> Result<(), String> {
        if ddl.trim().is_empty() {
            return Err("Trigger DDL cannot be empty".to_string());
        }
        self.client_for_database(db)
            .await?
            .batch_execute(ddl)
            .await
            .map_err(|e| format_postgres_execute_error("Create trigger failed", &e, ddl))
    }

    pub async fn create_or_replace_function(&self, db: &str, ddl: &str) -> Result<(), String> {
        if ddl.trim().is_empty() {
            return Err("Function DDL cannot be empty".to_string());
        }
        self.client_for_database(db)
            .await?
            .batch_execute(ddl)
            .await
            .map_err(|e| format_postgres_execute_error("Save function failed", &e, ddl))
    }

    pub async fn execute_ddl(&self, db: &str, ddl: &str) -> Result<(), String> {
        if ddl.trim().is_empty() {
            return Err("DDL cannot be empty".to_string());
        }
        self.client_for_database(db)
            .await?
            .batch_execute(ddl)
            .await
            .map_err(|e| format_postgres_execute_error("DDL execution failed", &e, ddl))
    }

    pub async fn drop_function(
        &self,
        db: &str,
        schema: &str,
        name: &str,
        arguments: &str,
    ) -> Result<u64, String> {
        validate_identifier(name, "function")?;
        if arguments.contains(';') {
            return Err("Function arguments cannot contain semicolon".to_string());
        }
        let sql = format!(
            "DROP FUNCTION {}.{}({})",
            quote_ident(schema),
            quote_ident(name),
            arguments
        );
        self.client_for_database(db)
            .await?
            .execute(&sql, &[])
            .await
            .map_err(|e| format_postgres_execute_error("Drop function failed", &e, &sql))
    }
}

fn create_database_sql(name: &str) -> Result<String, String> {
    if name.is_empty() || name.trim() != name || name.as_bytes().len() > 63 || name.contains('\0') {
        return Err("Invalid PostgreSQL database name".into());
    }
    Ok(format!("CREATE DATABASE \"{}\"", name.replace('"', "\"\"")))
}

#[derive(serde::Deserialize)]
struct PostgresUpdatePatch {
    key: String,
    changes: serde_json::Value,
}

fn parse_postgres_json_param(json: &str) -> Result<serde_json::Value, String> {
    let value: serde_json::Value =
        serde_json::from_str(json).map_err(|e| format!("Invalid row JSON: {}", e))?;
    if !value.is_object() {
        return Err("Row JSON must be a JSON object".to_string());
    }
    Ok(value)
}

fn validate_identifier(name: &str, label: &str) -> Result<(), String> {
    if name.trim().is_empty() {
        return Err(format!("{label} name cannot be empty"));
    }
    if name.contains('\0') {
        return Err(format!("{label} name cannot contain NUL"));
    }
    Ok(())
}

fn postgres_server_version_number(version: &str) -> &str {
    version.split_whitespace().nth(1).unwrap_or("unknown")
}

#[cfg(test)]
fn empty_table_metadata_json() -> serde_json::Value {
    serde_json::json!({
        "columns": [],
        "constraints": [],
        "foreign_keys": [],
        "indexes": [],
        "triggers": [],
    })
}

async fn current_database_for_client(client: &Client) -> Result<String, String> {
    let row = client
        .query_one("SELECT current_database()::text", &[])
        .await
        .map_err(|e| e.to_string())?;
    catalog_cell(&row, 0)
}

/// One cell of a catalog row. A value that cannot be read (text that is not valid
/// UTF-8, an unexpected NULL) is an error that names the column, never a panic:
/// with the client in UTF-8 the server itself refuses to send non-UTF-8 text, so
/// this is the safety net, and no object is silently dropped from a listing.
fn catalog_cell<'a, T: tokio_postgres::types::FromSql<'a>>(
    row: &'a tokio_postgres::Row,
    index: usize,
) -> Result<T, String> {
    row.try_get(index)
        .map_err(|error| format!("cannot read catalog column {index}: {error}"))
}

/// `config` with only the database replaced. The name is set on the parsed
/// configuration, never pasted into a URI: a name such as
/// `x?hostaddr=203.0.113.5` must stay a database name and not redirect the
/// connection (with the user's credentials) to another host.
fn postgres_config_with_database(
    config: &tokio_postgres::Config,
    database: &str,
) -> tokio_postgres::Config {
    let mut config = config.clone();
    config.dbname(database);
    config
}

fn format_postgres_execute_error(prefix: &str, error: &tokio_postgres::Error, sql: &str) -> String {
    if let Some(db_error) = error.as_db_error() {
        let mut parts = vec![db_error.message().to_string()];
        if let Some(detail) = db_error.detail() {
            parts.push(format!("detail: {detail}"));
        }
        if let Some(hint) = db_error.hint() {
            parts.push(format!("hint: {hint}"));
        }
        if let Some(column) = db_error.column() {
            parts.push(format!("column: {column}"));
        }
        if let Some(constraint) = db_error.constraint() {
            parts.push(format!("constraint: {constraint}"));
        }
        parts.push(format!("code: {}", db_error.code().code()));
        return format!("{prefix}: {} | SQL: {sql}", parts.join("; "));
    }

    format!("{prefix}: {error} | SQL: {sql}")
}

fn is_read_only_sql(sql: &str) -> bool {
    let trimmed = sql.trim_start();
    let lower = trimmed.to_ascii_lowercase();
    lower.starts_with("select") || lower.starts_with("with")
}

#[async_trait]
impl DatabaseConnection for PostgresConn {
    fn db_type(&self) -> DbType {
        DbType::Postgres
    }

    async fn test(&self) -> Result<(), String> {
        self.client
            .simple_query("SELECT 1")
            .await
            .map(|_| ())
            .map_err(|e| e.to_string())
    }

    async fn list_databases(&self) -> Result<Vec<String>, String> {
        let rows = self
            .client
            .query(POSTGRES_LIST_DATABASES_SQL, &[])
            .await
            .map_err(|e| e.to_string())?;
        rows.iter().map(|row| catalog_cell(row, 0)).collect()
    }

    async fn list_collections(&self, _db: &str) -> Result<Vec<String>, String> {
        self.list_tables(_db, "public").await
    }

    async fn execute_query(
        &self,
        _db: &str,
        table: &str,
        user_where: Option<&str>,
        user_order_by: Option<&str>,
        cursor: Option<&CursorState>,
        limit: i64,
    ) -> Result<QueryResult, String> {
        self.execute_query_in_schema(
            _db,
            "public",
            table,
            user_where,
            user_order_by,
            cursor,
            limit,
        )
        .await
    }

    async fn cancel(&self) {
        // tokio-postgres doesn't support mid-query cancel without a separate cancel token;
        // the query will complete or fail naturally.
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn read_only_sql_allows_select_and_with() {
        assert!(is_read_only_sql("SELECT * FROM users"));
        assert!(is_read_only_sql(
            "  \nwith recent AS (SELECT 1) SELECT * FROM recent"
        ));
    }

    #[test]
    fn read_only_sql_rejects_mutating_statements() {
        assert!(!is_read_only_sql("DELETE FROM users"));
        assert!(!is_read_only_sql("UPDATE users SET name = 'x'"));
        assert!(!is_read_only_sql("INSERT INTO users VALUES (1)"));
        assert!(!is_read_only_sql("DROP TABLE users"));
    }

    #[test]
    fn create_database_identifier_is_quoted_without_truncation_or_injection() {
        assert_eq!(
            create_database_sql("a\"b").unwrap(),
            "CREATE DATABASE \"a\"\"b\""
        );
        for bad in ["", " ", "x\0y", &"x".repeat(64)] {
            assert!(create_database_sql(bad).is_err());
        }
        assert!(create_database_sql(&"é".repeat(32)).is_err());
    }

    #[test]
    fn parse_postgres_json_param_rejects_non_object_json() {
        let err = parse_postgres_json_param("[]").expect_err("arrays are not row objects");
        assert!(err.contains("JSON object"));
    }

    fn config_for(uri: &str, database: &str) -> tokio_postgres::Config {
        postgres_config_with_database(&uri.parse().expect("test URI parses"), database)
    }

    #[test]
    fn config_with_database_adds_missing_path() {
        let config = config_for("postgresql://devuser:devpass@localhost:5432", "postgres");
        assert_eq!(config.get_dbname(), Some("postgres"));
        assert_eq!(config.get_user(), Some("devuser"));
    }

    #[test]
    fn config_with_database_replaces_the_database_and_keeps_the_rest() {
        let config = config_for(
            "postgresql://devuser:devpass@localhost:5432/devdb?sslmode=disable&connect_timeout=7",
            "postgres",
        );
        assert_eq!(config.get_dbname(), Some("postgres"));
        assert_eq!(config.get_ports(), &[5432]);
        assert_eq!(config.get_ssl_mode(), tokio_postgres::config::SslMode::Disable);
        assert_eq!(
            config.get_connect_timeout(),
            Some(&std::time::Duration::from_secs(7))
        );
    }

    #[test]
    fn hostile_database_names_stay_database_names() {
        let uri = "postgresql://devuser:devpass@db.example:5432/devdb?sslmode=disable";
        let original: tokio_postgres::Config = uri.parse().unwrap();
        for name in [
            "x?hostaddr=203.0.113.5",
            "hostaddr=203.0.113.5",
            "a%20b",
            "h#frag",
            "sl/ash",
            "sp ace",
            "x&host=evil.example",
            "x@evil.example",
            "x:5433",
            "\"quoted\"",
        ] {
            let config = config_for(uri, name);
            assert_eq!(config.get_dbname(), Some(name), "{name}");
            assert_eq!(config.get_hosts(), original.get_hosts(), "{name}");
            assert!(config.get_hostaddrs().is_empty(), "{name}");
            assert_eq!(config.get_ports(), original.get_ports(), "{name}");
            assert_eq!(config.get_user(), Some("devuser"), "{name}");
            assert_eq!(config.get_ssl_mode(), original.get_ssl_mode(), "{name}");
        }
    }

    #[test]
    fn list_databases_sql_returns_connectable_non_template_databases() {
        assert!(POSTGRES_LIST_DATABASES_SQL.contains("pg_database"));
        assert!(POSTGRES_LIST_DATABASES_SQL.contains("datallowconn"));
        assert!(POSTGRES_LIST_DATABASES_SQL.contains("NOT datistemplate"));
        assert!(POSTGRES_LIST_DATABASES_SQL.contains("has_database_privilege"));
    }

    #[test]
    fn extracts_postgres_server_version_number() {
        assert_eq!(
            postgres_server_version_number("PostgreSQL 16.2 on aarch64-apple-darwin"),
            "16.2"
        );
        assert_eq!(postgres_server_version_number("PostgreSQL"), "unknown");
    }

    #[test]
    fn sql_objects_sql_covers_tables_views_and_materialized_views() {
        assert!(POSTGRES_SQL_OBJECTS_SQL.contains("pg_class"));
        assert!(POSTGRES_SQL_OBJECTS_SQL.contains("pg_namespace"));
        assert!(POSTGRES_SQL_OBJECTS_SQL.contains("'r', 'p', 'v', 'm'"));
        assert_eq!(
            SqlObjectKind::from_postgres_relkind("r"),
            Some(SqlObjectKind::Table)
        );
        assert_eq!(
            SqlObjectKind::from_postgres_relkind("p"),
            Some(SqlObjectKind::Table)
        );
        assert_eq!(
            SqlObjectKind::from_postgres_relkind("v"),
            Some(SqlObjectKind::View)
        );
        assert_eq!(
            SqlObjectKind::from_postgres_relkind("m"),
            Some(SqlObjectKind::MaterializedView)
        );
    }

    #[test]
    fn catalog_sql_covers_schema_table_column() {
        assert!(POSTGRES_CATALOG_SQL.contains("pg_attribute"));
        assert!(POSTGRES_CATALOG_SQL.contains("table_schema"));
        assert!(POSTGRES_CATALOG_SQL.contains("table_name"));
        assert!(POSTGRES_CATALOG_SQL.contains("column_name"));
        assert!(POSTGRES_CATALOG_SQL.contains("data_type"));
        assert!(POSTGRES_CATALOG_SQL.contains("pg_catalog"));
        assert!(POSTGRES_CATALOG_SQL.contains("information_schema"));
        assert!(POSTGRES_CATALOG_SQL.contains("a.attnum"));
    }

    #[test]
    fn table_metadata_sql_covers_columns_and_relations() {
        assert!(POSTGRES_TABLE_COLUMNS_SQL.contains("pg_attribute"));
        assert!(POSTGRES_TABLE_COLUMNS_SQL.contains("format_type"));
        assert!(POSTGRES_TABLE_COLUMNS_SQL.contains("attgenerated"));
        assert!(POSTGRES_TABLE_COLUMNS_SQL.contains("attidentity"));
        assert!(POSTGRES_TABLE_COLUMNS_SQL.contains("pg_index"));
        assert!(POSTGRES_TABLE_COLUMNS_SQL.contains("'r', 'p', 'v', 'm'"));
        assert!(POSTGRES_TABLE_CONSTRAINTS_SQL.contains("pg_constraint"));
        assert!(POSTGRES_TABLE_FOREIGN_KEYS_SQL.contains("confrelid"));
        assert!(POSTGRES_TABLE_INDEXES_SQL.contains("pg_indexes"));
        assert!(POSTGRES_TABLE_TRIGGERS_SQL.contains("pg_trigger"));
    }

    #[test]
    fn table_metadata_empty_json_has_expected_sections() {
        let json = empty_table_metadata_json();
        assert!(json["columns"].is_array());
        assert!(json["constraints"].is_array());
        assert!(json["foreign_keys"].is_array());
        assert!(json["indexes"].is_array());
        assert!(json["triggers"].is_array());
    }
}

// ---------------------------------------------------------------------------
// Table properties (Properties presentation). Local types only: this file is
// `include!`d into the PostgreSQL driver crate, which has no `crate::catalog`;
// `engine.rs` maps these into the shared catalog types.
// ---------------------------------------------------------------------------

/// One row: relkind, table comment, view definition (views/matviews only).
const POSTGRES_PROPS_OBJECT_SQL: &str = "SELECT c.relkind::text,
            pg_catalog.obj_description(c.oid, 'pg_class') AS comment,
            CASE WHEN c.relkind IN ('v', 'm') THEN pg_get_viewdef(c.oid, true) END AS view_definition
     FROM pg_class c
     JOIN pg_namespace n ON n.oid = c.relnamespace
     WHERE n.nspname = $1
       AND c.relname = $2
       AND c.relkind IN ('r', 'p', 'v', 'm')";

/// Primary/unique/check/exclusion constraints with their column lists.
/// (`contype` 'n' NOT NULL rows on PostgreSQL 18+ and 't' are excluded.)
const POSTGRES_PROPS_CONSTRAINTS_SQL: &str = "SELECT con.conname::text,
            con.contype::text,
            ARRAY(SELECT att.attname::text
                  FROM unnest(con.conkey) WITH ORDINALITY AS k(attnum, ord)
                  JOIN pg_attribute att ON att.attrelid = con.conrelid AND att.attnum = k.attnum
                  ORDER BY k.ord) AS columns,
            pg_get_constraintdef(con.oid, true) AS definition
     FROM pg_constraint con
     JOIN pg_class c ON c.oid = con.conrelid
     JOIN pg_namespace n ON n.oid = c.relnamespace
     WHERE n.nspname = $1
       AND c.relname = $2
       AND con.contype IN ('p', 'u', 'c', 'x')
     ORDER BY con.contype <> 'p', con.conname";

macro_rules! postgres_props_foreign_keys_sql {
    ($filter:literal, $tail:literal) => {
        concat!(
            "SELECT con.conname::text,
            local_ns.nspname::text AS local_schema,
            local_table.relname::text AS local_table,
            ARRAY(SELECT att.attname::text
                  FROM unnest(con.conkey) WITH ORDINALITY AS k(attnum, ord)
                  JOIN pg_attribute att ON att.attrelid = con.conrelid AND att.attnum = k.attnum
                  ORDER BY k.ord) AS columns,
            foreign_ns.nspname::text AS foreign_schema,
            foreign_table.relname::text AS foreign_table,
            ARRAY(SELECT att.attname::text
                  FROM unnest(con.confkey) WITH ORDINALITY AS k(attnum, ord)
                  JOIN pg_attribute att ON att.attrelid = con.confrelid AND att.attnum = k.attnum
                  ORDER BY k.ord) AS foreign_columns,
            con.confupdtype::text AS on_update,
            con.confdeltype::text AS on_delete,
            pg_get_constraintdef(con.oid, true) AS definition
     FROM pg_constraint con
     JOIN pg_class local_table ON local_table.oid = con.conrelid
     JOIN pg_namespace local_ns ON local_ns.oid = local_table.relnamespace
     JOIN pg_class foreign_table ON foreign_table.oid = con.confrelid
     JOIN pg_namespace foreign_ns ON foreign_ns.oid = foreign_table.relnamespace
     WHERE con.contype = 'f'
       AND ",
            $filter,
            "\n     ",
            $tail
        )
    };
}

/// Foreign keys owned by the table ($1 = schema, $2 = table).
const POSTGRES_PROPS_FOREIGN_KEYS_SQL: &str = postgres_props_foreign_keys_sql!(
    "local_ns.nspname = $1 AND local_table.relname = $2",
    "ORDER BY con.conname"
);

/// Foreign keys in other tables whose `confrelid` is the table ($1 = schema,
/// $2 = table). `conparentid = 0` skips per-partition clones of a
/// partitioned table's foreign key so each logical FK is listed once.
const POSTGRES_PROPS_REFERENCES_SQL: &str = postgres_props_foreign_keys_sql!(
    "con.confrelid = (SELECT t.oid FROM pg_class t JOIN pg_namespace tn ON tn.oid = t.relnamespace WHERE tn.nspname = $1 AND t.relname = $2)
       AND con.conparentid = 0",
    "ORDER BY local_ns.nspname, local_table.relname, con.conname LIMIT 1000"
);

/// Indexes with access method (pg_am), key columns/expressions and whether the
/// index backs a PRIMARY KEY/UNIQUE/EXCLUDE constraint.
const POSTGRES_PROPS_INDEXES_SQL: &str = "SELECT index_class.relname::text AS index_name,
            pg_get_indexdef(index_class.oid) AS definition,
            idx.indisunique AS is_unique,
            idx.indisprimary AS is_primary,
            am.amname::text AS method,
            ARRAY(SELECT pg_get_indexdef(idx.indexrelid, k, true)
                  FROM generate_series(1, idx.indnkeyatts::int) AS k
                  ORDER BY k) AS columns,
            EXISTS (SELECT 1 FROM pg_constraint pc
                    WHERE pc.conindid = idx.indexrelid AND pc.contype IN ('p', 'u', 'x')) AS backs_constraint
     FROM pg_index idx
     JOIN pg_class table_class ON table_class.oid = idx.indrelid
     JOIN pg_namespace n ON n.oid = table_class.relnamespace
     JOIN pg_class index_class ON index_class.oid = idx.indexrelid
     JOIN pg_am am ON am.oid = index_class.relam
     WHERE n.nspname = $1
       AND table_class.relname = $2
     ORDER BY index_class.relname";

/// Column comments (only commented columns).
const POSTGRES_PROPS_COLUMN_COMMENTS_SQL: &str = "SELECT a.attname::text,
            pg_catalog.col_description(a.attrelid, a.attnum) AS comment
     FROM pg_attribute a
     JOIN pg_class c ON c.oid = a.attrelid
     JOIN pg_namespace n ON n.oid = c.relnamespace
     WHERE n.nspname = $1
       AND c.relname = $2
       AND a.attnum > 0
       AND NOT a.attisdropped
       AND pg_catalog.col_description(a.attrelid, a.attnum) IS NOT NULL
     ORDER BY a.attnum";

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SqlConstraint {
    pub name: String,
    /// `pg_constraint.contype`: `p`, `u`, `c` or `x`.
    pub kind_code: String,
    pub columns: Vec<String>,
    pub definition: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SqlForeignKey {
    pub name: String,
    /// Schema-qualified owning table (`schema.table`).
    pub table: String,
    pub columns: Vec<String>,
    /// Schema-qualified referenced table (`schema.table`).
    pub referenced_table: String,
    pub referenced_columns: Vec<String>,
    pub on_update: String,
    pub on_delete: String,
    pub definition: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SqlIndex {
    pub name: String,
    pub columns: Vec<String>,
    pub unique: bool,
    pub primary: bool,
    pub method: String,
    pub definition: String,
    pub backs_constraint: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SqlTrigger {
    pub name: String,
    /// `BEFORE`, `AFTER` or `INSTEAD OF`.
    pub timing: String,
    pub events: Vec<String>,
    pub enabled: bool,
    pub definition: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SqlTableProperties {
    pub columns: Vec<SqlColumn>,
    pub constraints: Vec<SqlConstraint>,
    pub foreign_keys: Vec<SqlForeignKey>,
    pub indexes: Vec<SqlIndex>,
    pub references: Vec<SqlForeignKey>,
    pub triggers: Vec<SqlTrigger>,
    pub ddl: String,
}

impl PostgresConn {
    /// Typed properties for one table/view/materialized view. All queries are
    /// parameterized on (schema, name) and pipelined on one connection.
    pub async fn table_properties(
        &self,
        db: &str,
        schema: &str,
        table: &str,
    ) -> Result<SqlTableProperties, String> {
        let client = self.client_for_database(db).await?;
        let params: &[&(dyn tokio_postgres::types::ToSql + Sync)] = &[&schema, &table];
        let fail = |error: tokio_postgres::Error| postgres_error_detail(&error);

        let (
            object_row,
            column_rows,
            constraint_rows,
            foreign_key_rows,
            reference_rows,
            index_rows,
            trigger_rows,
            comment_rows,
        ) = tokio::try_join!(
            client.query_opt(POSTGRES_PROPS_OBJECT_SQL, params),
            client.query(POSTGRES_TABLE_COLUMNS_SQL, params),
            client.query(POSTGRES_PROPS_CONSTRAINTS_SQL, params),
            client.query(POSTGRES_PROPS_FOREIGN_KEYS_SQL, params),
            client.query(POSTGRES_PROPS_REFERENCES_SQL, params),
            client.query(POSTGRES_PROPS_INDEXES_SQL, params),
            client.query(POSTGRES_TABLE_TRIGGERS_SQL, params),
            client.query(POSTGRES_PROPS_COLUMN_COMMENTS_SQL, params),
        )
        .map_err(fail)?;

        let Some(object_row) = object_row else {
            return Err(format!("{TABLE_NOT_FOUND_PREFIX}{schema}.{table}"));
        };
        let relkind: String = catalog_cell(&object_row, 0)?;
        let table_comment: Option<String> = catalog_cell(&object_row, 1)?;
        let view_definition: Option<String> = catalog_cell(&object_row, 2)?;

        let column_comments: Vec<(String, String)> = comment_rows
            .iter()
            .map(|row| Ok((catalog_cell(row, 0)?, catalog_cell(row, 1)?)))
            .collect::<Result<_, String>>()?;
        let columns: Vec<SqlColumn> = column_rows
            .iter()
            .map(|row| {
                let name: String = catalog_cell(row, 0)?;
                let comment = column_comments
                    .iter()
                    .find(|(column, _)| *column == name)
                    .map(|(_, comment)| comment.clone());
                Ok(SqlColumn {
                    name,
                    data_type: catalog_cell(row, 1)?,
                    nullable: catalog_cell(row, 2)?,
                    primary_key: catalog_cell(row, 7)?,
                    default_expression: catalog_cell(row, 3)?,
                    is_generated: Some(catalog_cell(row, 5)?),
                    auto_increment: Some(catalog_cell(row, 6)?),
                    comment,
                })
            })
            .collect::<Result<_, String>>()?;
        let constraints: Vec<SqlConstraint> = constraint_rows
            .iter()
            .map(|row| {
                Ok(SqlConstraint {
                    name: catalog_cell(row, 0)?,
                    kind_code: catalog_cell(row, 1)?,
                    columns: catalog_cell(row, 2)?,
                    definition: catalog_cell(row, 3)?,
                })
            })
            .collect::<Result<_, String>>()?;
        let foreign_keys: Vec<SqlForeignKey> = foreign_key_rows
            .iter()
            .map(postgres_foreign_key_from_row)
            .collect::<Result<_, String>>()?;
        let references: Vec<SqlForeignKey> = reference_rows
            .iter()
            .map(postgres_foreign_key_from_row)
            .collect::<Result<_, String>>()?;
        let indexes: Vec<SqlIndex> = index_rows
            .iter()
            .map(|row| {
                Ok(SqlIndex {
                    name: catalog_cell(row, 0)?,
                    definition: catalog_cell(row, 1)?,
                    unique: catalog_cell(row, 2)?,
                    primary: catalog_cell(row, 3)?,
                    method: catalog_cell(row, 4)?,
                    columns: catalog_cell(row, 5)?,
                    backs_constraint: catalog_cell(row, 6)?,
                })
            })
            .collect::<Result<_, String>>()?;
        let triggers: Vec<SqlTrigger> = trigger_rows
            .iter()
            .map(|row| {
                let enabled: String = catalog_cell(row, 2)?;
                let timing: String = catalog_cell(row, 4)?;
                Ok(SqlTrigger {
                    name: catalog_cell(row, 1)?,
                    enabled: enabled != "D",
                    timing: match timing.as_str() {
                        "INSTEAD" => "INSTEAD OF".to_string(),
                        other => other.to_string(),
                    },
                    events: catalog_cell(row, 5)?,
                    definition: catalog_cell(row, 11)?,
                })
            })
            .collect::<Result<_, String>>()?;

        let ddl = match relkind.as_str() {
            "v" | "m" => build_postgres_view_ddl(
                schema,
                table,
                relkind == "m",
                view_definition.as_deref().unwrap_or_default(),
                &indexes,
                table_comment.as_deref(),
            ),
            _ => build_postgres_table_ddl(
                schema,
                table,
                &columns,
                &constraints,
                &foreign_keys,
                &indexes,
                &triggers,
                table_comment.as_deref(),
                &column_comments,
            ),
        };

        Ok(SqlTableProperties {
            columns,
            constraints,
            foreign_keys,
            indexes,
            references,
            triggers,
            ddl,
        })
    }
}

fn postgres_foreign_key_from_row(row: &tokio_postgres::Row) -> Result<SqlForeignKey, String> {
    let local_schema: String = catalog_cell(row, 1)?;
    let local_table: String = catalog_cell(row, 2)?;
    let foreign_schema: String = catalog_cell(row, 4)?;
    let foreign_table: String = catalog_cell(row, 5)?;
    let on_update: String = catalog_cell(row, 7)?;
    let on_delete: String = catalog_cell(row, 8)?;
    Ok(SqlForeignKey {
        name: catalog_cell(row, 0)?,
        table: format!("{local_schema}.{local_table}"),
        columns: catalog_cell(row, 3)?,
        referenced_table: format!("{foreign_schema}.{foreign_table}"),
        referenced_columns: catalog_cell(row, 6)?,
        on_update: postgres_fk_action(&on_update).to_string(),
        on_delete: postgres_fk_action(&on_delete).to_string(),
        definition: catalog_cell(row, 9)?,
    })
}

/// `confupdtype` / `confdeltype` code to its SQL action name.
fn postgres_fk_action(code: &str) -> &'static str {
    match code {
        "r" => "RESTRICT",
        "c" => "CASCADE",
        "n" => "SET NULL",
        "d" => "SET DEFAULT",
        _ => "NO ACTION", // 'a'
    }
}

fn postgres_quote_literal(value: &str) -> String {
    format!("'{}'", value.replace('\'', "''"))
}

fn postgres_comment_statements(
    target: &str,
    table_comment: Option<&str>,
    column_comments: &[(String, String)],
    qualified_table: &str,
) -> Vec<String> {
    let mut statements = Vec::new();
    if let Some(comment) = table_comment {
        statements.push(format!(
            "COMMENT ON {target} {qualified_table} IS {};",
            postgres_quote_literal(comment)
        ));
    }
    for (column, comment) in column_comments {
        statements.push(format!(
            "COMMENT ON COLUMN {qualified_table}.{} IS {};",
            quote_ident(column),
            postgres_quote_literal(comment)
        ));
    }
    statements
}

fn build_postgres_view_ddl(
    schema: &str,
    name: &str,
    materialized: bool,
    definition: &str,
    indexes: &[SqlIndex],
    comment: Option<&str>,
) -> String {
    let qualified = quote_qualified_table(schema, name);
    let keyword = if materialized {
        "MATERIALIZED VIEW"
    } else {
        "VIEW"
    };
    let mut ddl = format!(
        "CREATE {keyword} {qualified} AS\n{};",
        definition.trim().trim_end_matches(';').trim_end()
    );
    for index in indexes {
        ddl.push_str(&format!("\n\n{};", index.definition.trim_end_matches(';')));
    }
    for statement in postgres_comment_statements(keyword, comment, &[], &qualified) {
        ddl.push_str(&format!("\n\n{statement}"));
    }
    ddl
}

#[allow(clippy::too_many_arguments)]
fn build_postgres_table_ddl(
    schema: &str,
    table: &str,
    columns: &[SqlColumn],
    constraints: &[SqlConstraint],
    foreign_keys: &[SqlForeignKey],
    indexes: &[SqlIndex],
    triggers: &[SqlTrigger],
    table_comment: Option<&str>,
    column_comments: &[(String, String)],
) -> String {
    let qualified = quote_qualified_table(schema, table);
    let mut lines = Vec::with_capacity(columns.len() + constraints.len() + foreign_keys.len());
    for column in columns {
        let mut line = format!("  {} {}", quote_ident(&column.name), column.data_type);
        if column.is_generated == Some(true) {
            if let Some(expression) = column.default_expression.as_deref() {
                line.push_str(&format!(" GENERATED ALWAYS AS ({expression}) STORED"));
            }
        } else if let Some(default) = column.default_expression.as_deref() {
            line.push_str(&format!(" DEFAULT {default}"));
        } else if column.auto_increment == Some(true) {
            // Identity column (a serial column would carry a nextval() default).
            // The catalog does not distinguish ALWAYS from BY DEFAULT.
            line.push_str(" GENERATED BY DEFAULT AS IDENTITY");
        }
        if !column.nullable {
            line.push_str(" NOT NULL");
        }
        lines.push(line);
    }
    for constraint in constraints {
        lines.push(format!(
            "  CONSTRAINT {} {}",
            quote_ident(&constraint.name),
            constraint.definition
        ));
    }
    for foreign_key in foreign_keys {
        lines.push(format!(
            "  CONSTRAINT {} {}",
            quote_ident(&foreign_key.name),
            foreign_key.definition
        ));
    }

    let mut ddl = format!("CREATE TABLE {qualified} (\n{}\n);", lines.join(",\n"));
    for index in indexes.iter().filter(|index| !index.backs_constraint) {
        ddl.push_str(&format!("\n\n{};", index.definition.trim_end_matches(';')));
    }
    for trigger in triggers {
        ddl.push_str(&format!("\n\n{};", trigger.definition.trim_end_matches(';')));
    }
    for statement in postgres_comment_statements("TABLE", table_comment, column_comments, &qualified)
    {
        ddl.push_str(&format!("\n\n{statement}"));
    }
    ddl
}

#[cfg(test)]
mod table_properties_tests {
    use super::*;

    fn column(name: &str, data_type: &str, nullable: bool, default: Option<&str>) -> SqlColumn {
        SqlColumn {
            name: name.to_string(),
            data_type: data_type.to_string(),
            nullable,
            primary_key: false,
            is_generated: Some(false),
            default_expression: default.map(str::to_string),
            auto_increment: Some(false),
            comment: None,
        }
    }

    #[test]
    fn properties_sql_is_parameterized_and_covers_required_catalogs() {
        for sql in [
            POSTGRES_PROPS_OBJECT_SQL,
            POSTGRES_PROPS_CONSTRAINTS_SQL,
            POSTGRES_PROPS_FOREIGN_KEYS_SQL,
            POSTGRES_PROPS_REFERENCES_SQL,
            POSTGRES_PROPS_INDEXES_SQL,
            POSTGRES_PROPS_COLUMN_COMMENTS_SQL,
        ] {
            assert!(sql.contains("$1") && sql.contains("$2"), "{sql}");
        }
        assert!(POSTGRES_PROPS_FOREIGN_KEYS_SQL.contains("local_ns.nspname = $1 AND local_table.relname = $2"));
        assert!(POSTGRES_PROPS_FOREIGN_KEYS_SQL.contains("confupdtype"));
        assert!(POSTGRES_PROPS_FOREIGN_KEYS_SQL.contains("confdeltype"));
        assert!(POSTGRES_PROPS_REFERENCES_SQL.contains("con.contype = 'f'"));
        assert!(POSTGRES_PROPS_REFERENCES_SQL.contains("con.confrelid = (SELECT t.oid"));
        assert!(POSTGRES_PROPS_REFERENCES_SQL.contains("conparentid = 0"));
        assert!(POSTGRES_PROPS_INDEXES_SQL.contains("pg_am"));
        assert!(POSTGRES_PROPS_INDEXES_SQL.contains("amname"));
        assert!(POSTGRES_PROPS_CONSTRAINTS_SQL.contains("con.contype IN ('p', 'u', 'c', 'x')"));
        assert!(POSTGRES_PROPS_OBJECT_SQL.contains("pg_get_viewdef"));
        // No caller-supplied text is ever spliced into the statements.
        assert!(!POSTGRES_PROPS_REFERENCES_SQL.contains("{}"));
    }

    #[test]
    fn foreign_key_actions_map_from_catalog_codes() {
        assert_eq!(postgres_fk_action("a"), "NO ACTION");
        assert_eq!(postgres_fk_action("r"), "RESTRICT");
        assert_eq!(postgres_fk_action("c"), "CASCADE");
        assert_eq!(postgres_fk_action("n"), "SET NULL");
        assert_eq!(postgres_fk_action("d"), "SET DEFAULT");
    }

    #[test]
    fn table_ddl_lists_columns_constraints_non_constraint_indexes_and_comments() {
        let mut id = column("id", "integer", false, Some("nextval('orders_id_seq'::regclass)"));
        id.primary_key = true;
        let mut total = column("total", "numeric(10,2)", true, None);
        total.is_generated = Some(true);
        total.default_expression = Some("qty * price".into());
        let mut ident = column("seq", "bigint", false, None);
        ident.auto_increment = Some(true);
        let columns = [id, column("we\"ird", "text", true, None), total, ident];
        let constraints = [SqlConstraint {
            name: "orders_pkey".into(),
            kind_code: "p".into(),
            columns: vec!["id".into()],
            definition: "PRIMARY KEY (id)".into(),
        }];
        let foreign_keys = [SqlForeignKey {
            name: "orders_customer_fk".into(),
            table: "public.orders".into(),
            columns: vec!["customer_id".into()],
            referenced_table: "public.customers".into(),
            referenced_columns: vec!["id".into()],
            on_update: "NO ACTION".into(),
            on_delete: "CASCADE".into(),
            definition: "FOREIGN KEY (customer_id) REFERENCES customers(id) ON DELETE CASCADE".into(),
        }];
        let indexes = [
            SqlIndex {
                name: "orders_pkey".into(),
                columns: vec!["id".into()],
                unique: true,
                primary: true,
                method: "btree".into(),
                definition: "CREATE UNIQUE INDEX orders_pkey ON public.orders USING btree (id)".into(),
                backs_constraint: true,
            },
            SqlIndex {
                name: "orders_note_idx".into(),
                columns: vec!["note".into()],
                unique: false,
                primary: false,
                method: "gin".into(),
                definition: "CREATE INDEX orders_note_idx ON public.orders USING gin (note)".into(),
                backs_constraint: false,
            },
        ];
        let triggers = [SqlTrigger {
            name: "audit".into(),
            timing: "AFTER".into(),
            events: vec!["INSERT".into()],
            enabled: true,
            definition: "CREATE TRIGGER audit AFTER INSERT ON public.orders FOR EACH ROW EXECUTE FUNCTION log()".into(),
        }];
        let ddl = build_postgres_table_ddl(
            "public",
            "orders",
            &columns,
            &constraints,
            &foreign_keys,
            &indexes,
            &triggers,
            Some("it's an order"),
            &[("id".into(), "surrogate key".into())],
        );

        assert!(ddl.starts_with("CREATE TABLE \"public\".\"orders\" (\n"), "{ddl}");
        assert!(ddl.contains("  \"id\" integer DEFAULT nextval('orders_id_seq'::regclass) NOT NULL,\n"), "{ddl}");
        assert!(ddl.contains("  \"we\"\"ird\" text,\n"), "{ddl}");
        assert!(ddl.contains("  \"total\" numeric(10,2) GENERATED ALWAYS AS (qty * price) STORED,\n"), "{ddl}");
        assert!(ddl.contains("  \"seq\" bigint GENERATED BY DEFAULT AS IDENTITY NOT NULL,\n"), "{ddl}");
        assert!(ddl.contains("  CONSTRAINT \"orders_pkey\" PRIMARY KEY (id),\n"), "{ddl}");
        assert!(ddl.contains("  CONSTRAINT \"orders_customer_fk\" FOREIGN KEY (customer_id) REFERENCES customers(id) ON DELETE CASCADE\n);"), "{ddl}");
        assert!(ddl.contains("CREATE INDEX orders_note_idx ON public.orders USING gin (note);"), "{ddl}");
        assert!(!ddl.contains("CREATE UNIQUE INDEX orders_pkey"), "constraint-backed index is implied: {ddl}");
        assert!(ddl.contains("CREATE TRIGGER audit AFTER INSERT"), "{ddl}");
        assert!(ddl.contains("COMMENT ON TABLE \"public\".\"orders\" IS 'it''s an order';"), "{ddl}");
        assert!(ddl.contains("COMMENT ON COLUMN \"public\".\"orders\".\"id\" IS 'surrogate key';"), "{ddl}");
    }

    #[test]
    fn view_ddl_wraps_definition_as_create_view() {
        let view = build_postgres_view_ddl(
            "public",
            "v",
            false,
            " SELECT 1 AS one;\n",
            &[],
            None,
        );
        assert_eq!(view, "CREATE VIEW \"public\".\"v\" AS\nSELECT 1 AS one;");

        let matview = build_postgres_view_ddl(
            "s",
            "m",
            true,
            "SELECT 1",
            &[SqlIndex {
                name: "m_idx".into(),
                columns: vec!["one".into()],
                unique: false,
                primary: false,
                method: "btree".into(),
                definition: "CREATE INDEX m_idx ON s.m USING btree (one)".into(),
                backs_constraint: false,
            }],
            Some("cached"),
        );
        assert!(matview.starts_with("CREATE MATERIALIZED VIEW \"s\".\"m\" AS\nSELECT 1;"), "{matview}");
        assert!(matview.contains("CREATE INDEX m_idx ON s.m USING btree (one);"));
        assert!(matview.contains("COMMENT ON MATERIALIZED VIEW \"s\".\"m\" IS 'cached';"));
    }
}
