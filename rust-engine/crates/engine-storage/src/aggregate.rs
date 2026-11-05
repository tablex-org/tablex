use rusqlite::{params, Connection, Result};
use std::path::{Path, PathBuf};
use std::sync::{Mutex, MutexGuard, PoisonError};
use std::time::Duration;

lazy_static::lazy_static! {
    static ref AGGREGATE_DB: Mutex<Option<Connection>> = Mutex::new(None);
}

/// How long a statement waits for another process holding the database
/// (the GPUI app and the legacy app may both be open) before failing with
/// `SQLITE_BUSY`.
const BUSY_TIMEOUT: Duration = Duration::from_secs(5);

#[derive(Debug, Clone)]
pub struct SavedAggregate {
    pub id: String,
    pub conn_uuid: String,
    pub database: String,
    pub collection: String,
    pub name: String,
    pub kind: String,
    pub pipeline_json: String,
    pub created_at: f64,
    pub updated_at: f64,
    /// Editor settings stored as JSON (sample size, `maxTimeMS`, collation,
    /// `allowDiskUse`, disabled stages). `NULL` for rows written by older
    /// versions or by callers that do not know about it.
    pub options_json: Option<String>,
}

/// Locks a mutex, recovering the guard if a previous holder panicked: the
/// connection itself is still consistent (SQLite transactions are atomic), so
/// a poisoned lock must not turn every later call into a panic.
fn locked<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(PoisonError::into_inner)
}

pub fn default_db_path() -> PathBuf {
    get_db_path()
}

fn get_db_path() -> PathBuf {
    crate::paths::data_dir("TableLeaf").join("aggregates.db")
}

fn create_schema(conn: &Connection) -> Result<()> {
    conn.execute(
        "CREATE TABLE IF NOT EXISTS saved_aggregates (
            id TEXT PRIMARY KEY,
            conn_uuid TEXT NOT NULL,
            database TEXT NOT NULL,
            collection TEXT NOT NULL DEFAULT '',
            name TEXT NOT NULL,
            kind TEXT NOT NULL DEFAULT 'aggregate',
            pipeline_json TEXT NOT NULL,
            created_at REAL NOT NULL,
            updated_at REAL NOT NULL
        )",
        [],
    )?;
    let _ = conn.execute("DROP INDEX IF EXISTS idx_saved_agg_lookup", []);
    conn.execute(
        "CREATE INDEX IF NOT EXISTS idx_saved_agg_db_lookup
            ON saved_aggregates(conn_uuid, database)",
        [],
    )?;
    ensure_column(
        conn,
        "kind",
        "ALTER TABLE saved_aggregates ADD COLUMN kind TEXT NOT NULL DEFAULT 'aggregate'",
    )?;
    ensure_column(
        conn,
        "options_json",
        "ALTER TABLE saved_aggregates ADD COLUMN options_json TEXT",
    )?;
    Ok(())
}

/// Idempotent `ALTER TABLE ... ADD COLUMN`: runs `ddl` only when the column is
/// missing, so a real failure (read-only file, corruption) is reported
/// instead of being swallowed like a "duplicate column" error.
fn ensure_column(conn: &Connection, column: &str, ddl: &str) -> Result<()> {
    let present: i64 = conn.query_row(
        "SELECT COUNT(*) FROM pragma_table_info('saved_aggregates') WHERE name = ?1",
        params![column],
        |row| row.get(0),
    )?;
    if present == 0 {
        add_column(conn, ddl)?;
    }
    Ok(())
}

/// Runs an `ALTER TABLE ... ADD COLUMN`. Another process (the previous app
/// shares this file) may add the same column between our check and our
/// statement; SQLite then reports "duplicate column name", which is exactly
/// the state we wanted, so it counts as success. Every other error is real.
fn add_column(conn: &Connection, ddl: &str) -> Result<()> {
    match conn.execute(ddl, []) {
        Ok(_) => Ok(()),
        Err(rusqlite::Error::SqliteFailure(_, Some(message)))
            if message.contains("duplicate column name") =>
        {
            Ok(())
        }
        Err(error) => Err(error),
    }
}

fn open_connection(path: &Path) -> Result<Connection> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)
            .map_err(|e| rusqlite::Error::ToSqlConversionFailure(e.into()))?;
    }
    let conn = Connection::open(path)?;
    conn.busy_timeout(BUSY_TIMEOUT)?;
    create_schema(&conn)?;
    Ok(conn)
}

/// A saved-aggregates database at an explicit location. The free functions
/// below use the shared per-process default; the GPUI app owns a store so
/// tests can point it at a temporary file or memory.
pub struct AggregateStore {
    conn: Mutex<Connection>,
}

impl AggregateStore {
    pub fn open(path: &Path) -> Result<Self> {
        Ok(Self {
            conn: Mutex::new(open_connection(path)?),
        })
    }

    pub fn open_default() -> Result<Self> {
        Self::open(&get_db_path())
    }

    pub fn open_in_memory() -> Result<Self> {
        let conn = Connection::open_in_memory()?;
        conn.busy_timeout(BUSY_TIMEOUT)?;
        create_schema(&conn)?;
        Ok(Self {
            conn: Mutex::new(conn),
        })
    }

    pub fn list(&self, conn_uuid: &str, database: &str) -> Result<Vec<SavedAggregate>> {
        list_aggregates_impl(&locked(&self.conn), conn_uuid, database)
    }

    pub fn get(&self, id: &str) -> Result<Option<SavedAggregate>> {
        get_aggregate_impl(&locked(&self.conn), id)
    }

    /// Inserts or updates a row, including `database` and `options_json`
    /// (`None` clears it).
    pub fn upsert(&self, aggregate: &SavedAggregate) -> Result<()> {
        upsert_full_impl(&locked(&self.conn), aggregate)
    }

    pub fn delete(&self, id: &str) -> Result<()> {
        delete_aggregate_impl(&locked(&self.conn), id)
    }
}

/// Runs `f` on the shared default connection, opening it on first use.
fn with_db<T>(f: impl FnOnce(&Connection) -> Result<T>) -> Result<T> {
    let mut db = locked(&AGGREGATE_DB);
    if db.is_none() {
        let path = get_db_path();
        eprintln!("[aggregate] Opening DB at: {:?}", path);
        *db = Some(open_connection(&path)?);
    }
    match db.as_ref() {
        Some(conn) => f(conn),
        None => Err(rusqlite::Error::InvalidQuery),
    }
}

pub fn list_aggregates(conn_uuid: &str, database: &str) -> Result<Vec<SavedAggregate>> {
    with_db(|conn| list_aggregates_impl(conn, conn_uuid, database))
}

pub fn get_aggregate(id: &str) -> Result<Option<SavedAggregate>> {
    with_db(|conn| get_aggregate_impl(conn, id))
}

// Keep FFI signatures stable; parameter objects would churn every Swift caller.
// Leaves `options_json` of an existing row untouched (callers of this entry
// point do not know the column); use `upsert_aggregate_with_options` or
// `AggregateStore::upsert` to write it.
#[allow(clippy::too_many_arguments)]
pub fn upsert_aggregate(
    id: &str,
    conn_uuid: &str,
    database: &str,
    collection: &str,
    name: &str,
    kind: &str,
    pipeline_json: &str,
    created_at: f64,
    updated_at: f64,
) -> Result<()> {
    with_db(|conn| {
        upsert_aggregate_impl(
            conn,
            id,
            conn_uuid,
            database,
            collection,
            name,
            kind,
            pipeline_json,
            created_at,
            updated_at,
        )
    })
}

/// Like [`upsert_aggregate`] but also writes `options_json` (`None` clears it).
pub fn upsert_aggregate_with_options(aggregate: &SavedAggregate) -> Result<()> {
    with_db(|conn| upsert_full_impl(conn, aggregate))
}

pub fn delete_aggregate(id: &str) -> Result<()> {
    with_db(|conn| delete_aggregate_impl(conn, id))
}

// MARK: - Connection-scoped helpers (testable)

fn list_aggregates_impl(
    conn: &Connection,
    conn_uuid: &str,
    database: &str,
) -> Result<Vec<SavedAggregate>> {
    let mut stmt = conn.prepare(
        "SELECT id, conn_uuid, database, collection, name, kind, pipeline_json, created_at, updated_at, options_json
         FROM saved_aggregates
         WHERE conn_uuid = ?1 AND database = ?2
         ORDER BY name COLLATE NOCASE ASC",
    )?;

    let rows = stmt.query_map(params![conn_uuid, database], row_to_aggregate)?;

    rows.collect()
}

fn get_aggregate_impl(conn: &Connection, id: &str) -> Result<Option<SavedAggregate>> {
    let mut stmt = conn.prepare(
        "SELECT id, conn_uuid, database, collection, name, kind, pipeline_json, created_at, updated_at, options_json
         FROM saved_aggregates
         WHERE id = ?1",
    )?;

    let mut rows = stmt.query(params![id])?;
    match rows.next()? {
        Some(row) => Ok(Some(row_to_aggregate(row)?)),
        None => Ok(None),
    }
}

fn row_to_aggregate(row: &rusqlite::Row<'_>) -> Result<SavedAggregate> {
    Ok(SavedAggregate {
        id: row.get(0)?,
        conn_uuid: row.get(1)?,
        database: row.get(2)?,
        collection: row.get(3)?,
        name: row.get(4)?,
        kind: row.get(5)?,
        pipeline_json: row.get(6)?,
        created_at: row.get(7)?,
        updated_at: row.get(8)?,
        options_json: row.get(9)?,
    })
}

// Keep FFI signatures stable; parameter objects would churn every Swift caller.
#[allow(clippy::too_many_arguments)]
fn upsert_aggregate_impl(
    conn: &Connection,
    id: &str,
    conn_uuid: &str,
    database: &str,
    collection: &str,
    name: &str,
    kind: &str,
    pipeline_json: &str,
    created_at: f64,
    updated_at: f64,
) -> Result<()> {
    conn.execute(
        "INSERT INTO saved_aggregates
            (id, conn_uuid, database, collection, name, kind, pipeline_json, created_at, updated_at)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9)
         ON CONFLICT(id) DO UPDATE SET
            database = excluded.database,
            collection = excluded.collection,
            name = excluded.name,
            kind = excluded.kind,
            pipeline_json = excluded.pipeline_json,
            updated_at = excluded.updated_at",
        params![
            id,
            conn_uuid,
            database,
            collection,
            name,
            kind,
            pipeline_json,
            created_at,
            updated_at
        ],
    )?;
    Ok(())
}

fn upsert_full_impl(conn: &Connection, aggregate: &SavedAggregate) -> Result<()> {
    conn.execute(
        "INSERT INTO saved_aggregates
            (id, conn_uuid, database, collection, name, kind, pipeline_json, created_at, updated_at, options_json)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10)
         ON CONFLICT(id) DO UPDATE SET
            database = excluded.database,
            collection = excluded.collection,
            name = excluded.name,
            kind = excluded.kind,
            pipeline_json = excluded.pipeline_json,
            updated_at = excluded.updated_at,
            options_json = excluded.options_json",
        params![
            aggregate.id,
            aggregate.conn_uuid,
            aggregate.database,
            aggregate.collection,
            aggregate.name,
            aggregate.kind,
            aggregate.pipeline_json,
            aggregate.created_at,
            aggregate.updated_at,
            aggregate.options_json,
        ],
    )?;
    Ok(())
}

fn delete_aggregate_impl(conn: &Connection, id: &str) -> Result<()> {
    conn.execute("DELETE FROM saved_aggregates WHERE id = ?1", params![id])?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fresh_db() -> Connection {
        let conn = Connection::open_in_memory().unwrap();
        create_schema(&conn).unwrap();
        conn
    }

    #[test]
    fn upsert_inserts_new_aggregate() {
        let conn = fresh_db();
        upsert_aggregate_impl(
            &conn,
            "agg-1",
            "conn-uuid",
            "merchant-service",
            "orders",
            "Get orders",
            "aggregate",
            "[{\"$match\": {}}]",
            1.0,
            1.0,
        )
        .unwrap();

        let got = get_aggregate_impl(&conn, "agg-1").unwrap().unwrap();
        assert_eq!(got.id, "agg-1");
        assert_eq!(got.collection, "orders");
        assert_eq!(got.name, "Get orders");
        assert_eq!(got.kind, "aggregate");
        assert_eq!(got.pipeline_json, "[{\"$match\": {}}]");
    }

    #[test]
    fn upsert_updates_target_collection_on_conflict() {
        let conn = fresh_db();
        upsert_aggregate_impl(
            &conn,
            "agg-1",
            "conn-uuid",
            "merchant-service",
            "orders",
            "Get orders",
            "aggregate",
            "[{\"$match\": {}}]",
            1.0,
            1.0,
        )
        .unwrap();

        upsert_aggregate_impl(
            &conn,
            "agg-1",
            "conn-uuid",
            "merchant-service",
            "invoices",
            "Get orders",
            "aggregate",
            "[{\"$match\": {\"status\": \"paid\"}}]",
            1.0,
            2.0,
        )
        .unwrap();

        let got = get_aggregate_impl(&conn, "agg-1").unwrap().unwrap();
        assert_eq!(
            got.collection, "invoices",
            "collection must be updated on conflict, not just pipeline"
        );
        assert_eq!(got.pipeline_json, "[{\"$match\": {\"status\": \"paid\"}}]");
        assert_eq!(got.updated_at, 2.0);
    }

    #[test]
    fn upsert_updates_name_on_conflict() {
        let conn = fresh_db();
        upsert_aggregate_impl(
            &conn,
            "agg-1",
            "conn-uuid",
            "merchant-service",
            "orders",
            "old name",
            "aggregate",
            "[]",
            1.0,
            1.0,
        )
        .unwrap();

        upsert_aggregate_impl(
            &conn,
            "agg-1",
            "conn-uuid",
            "merchant-service",
            "orders",
            "new name",
            "aggregate",
            "[]",
            1.0,
            2.0,
        )
        .unwrap();

        let got = get_aggregate_impl(&conn, "agg-1").unwrap().unwrap();
        assert_eq!(got.name, "new name");
    }

    #[test]
    fn list_filters_by_connection_and_database() {
        let conn = fresh_db();
        upsert_aggregate_impl(
            &conn,
            "a1",
            "conn-a",
            "db1",
            "c",
            "n1",
            "aggregate",
            "[]",
            1.0,
            1.0,
        )
        .unwrap();
        upsert_aggregate_impl(
            &conn,
            "a2",
            "conn-a",
            "db2",
            "c",
            "n2",
            "aggregate",
            "[]",
            1.0,
            1.0,
        )
        .unwrap();
        upsert_aggregate_impl(
            &conn,
            "a3",
            "conn-b",
            "db1",
            "c",
            "n3",
            "aggregate",
            "[]",
            1.0,
            1.0,
        )
        .unwrap();

        let items = list_aggregates_impl(&conn, "conn-a", "db1").unwrap();
        assert_eq!(items.len(), 1);
        assert_eq!(items[0].id, "a1");
    }

    #[test]
    fn delete_removes_row() {
        let conn = fresh_db();
        upsert_aggregate_impl(
            &conn,
            "a1",
            "c",
            "d",
            "coll",
            "n",
            "aggregate",
            "[]",
            1.0,
            1.0,
        )
        .unwrap();
        delete_aggregate_impl(&conn, "a1").unwrap();
        assert!(get_aggregate_impl(&conn, "a1").unwrap().is_none());
    }

    fn sample(id: &str, database: &str) -> SavedAggregate {
        SavedAggregate {
            id: id.into(),
            conn_uuid: "conn".into(),
            database: database.into(),
            collection: "orders".into(),
            name: "report".into(),
            kind: "aggregate".into(),
            pipeline_json: "[]".into(),
            created_at: 1.0,
            updated_at: 1.0,
            options_json: None,
        }
    }

    #[test]
    fn upsert_updates_database_on_conflict() {
        // Regression: moving a saved pipeline to another database used to
        // leave the row listed under the old one.
        let conn = fresh_db();
        upsert_aggregate_impl(
            &conn,
            "a1",
            "c",
            "old_db",
            "orders",
            "n",
            "aggregate",
            "[]",
            1.0,
            1.0,
        )
        .unwrap();
        upsert_aggregate_impl(
            &conn,
            "a1",
            "c",
            "new_db",
            "orders",
            "n",
            "aggregate",
            "[]",
            1.0,
            2.0,
        )
        .unwrap();
        assert_eq!(
            get_aggregate_impl(&conn, "a1").unwrap().unwrap().database,
            "new_db"
        );
        assert!(list_aggregates_impl(&conn, "c", "old_db")
            .unwrap()
            .is_empty());
        assert_eq!(list_aggregates_impl(&conn, "c", "new_db").unwrap().len(), 1);

        upsert_full_impl(&conn, &sample("a2", "db1")).unwrap();
        let mut moved = sample("a2", "db2");
        moved.updated_at = 5.0;
        upsert_full_impl(&conn, &moved).unwrap();
        assert_eq!(
            get_aggregate_impl(&conn, "a2").unwrap().unwrap().database,
            "db2"
        );
    }

    #[test]
    fn options_json_round_trips_and_legacy_upsert_preserves_it() {
        let conn = fresh_db();
        let mut item = sample("o1", "db");
        item.options_json = Some(
            r#"{"sample_size":50,"max_time_ms":20000,"allow_disk_use":true,"collation":"{ locale: 'en' }","disabled_stages":[1]}"#
                .into(),
        );
        upsert_full_impl(&conn, &item).unwrap();
        let got = get_aggregate_impl(&conn, "o1").unwrap().unwrap();
        assert_eq!(got.options_json, item.options_json);
        assert_eq!(
            list_aggregates_impl(&conn, "conn", "db").unwrap()[0].options_json,
            item.options_json
        );

        // The nine-argument entry point (old callers) must not wipe options.
        upsert_aggregate_impl(
            &conn,
            "o1",
            "conn",
            "db",
            "orders",
            "renamed",
            "aggregate",
            "[{\"$match\":{}}]",
            1.0,
            3.0,
        )
        .unwrap();
        let got = get_aggregate_impl(&conn, "o1").unwrap().unwrap();
        assert_eq!(got.name, "renamed");
        assert_eq!(got.options_json, item.options_json);

        // The full upsert can clear them.
        item.options_json = None;
        upsert_full_impl(&conn, &item).unwrap();
        assert_eq!(
            get_aggregate_impl(&conn, "o1")
                .unwrap()
                .unwrap()
                .options_json,
            None
        );
        // A brand-new row through the legacy entry point starts with NULL.
        upsert_aggregate_impl(
            &conn,
            "o2",
            "conn",
            "db",
            "c",
            "n",
            "aggregate",
            "[]",
            1.0,
            1.0,
        )
        .unwrap();
        assert_eq!(
            get_aggregate_impl(&conn, "o2")
                .unwrap()
                .unwrap()
                .options_json,
            None
        );
    }

    #[test]
    fn old_schemas_migrate_idempotently_and_keep_their_rows() {
        let conn = Connection::open_in_memory().unwrap();
        // Oldest schema: no `kind`, no `options_json`, old index.
        conn.execute(
            "CREATE TABLE saved_aggregates (
                id TEXT PRIMARY KEY, conn_uuid TEXT NOT NULL, database TEXT NOT NULL,
                collection TEXT NOT NULL DEFAULT '', name TEXT NOT NULL,
                pipeline_json TEXT NOT NULL, created_at REAL NOT NULL, updated_at REAL NOT NULL)",
            [],
        )
        .unwrap();
        conn.execute(
            "CREATE INDEX idx_saved_agg_lookup ON saved_aggregates(conn_uuid)",
            [],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO saved_aggregates VALUES ('legacy', 'c', 'd', 'coll', 'old one', '[{\"$match\":{}}]', 1.0, 2.0)",
            [],
        )
        .unwrap();
        create_schema(&conn).unwrap();
        create_schema(&conn).expect("running the migration twice is a no-op");
        let got = get_aggregate_impl(&conn, "legacy").unwrap().unwrap();
        assert_eq!(got.kind, "aggregate", "kind defaults for migrated rows");
        assert_eq!(got.options_json, None);
        assert_eq!(got.pipeline_json, "[{\"$match\":{}}]");
        // The intermediate schema (kind present, no options_json) migrates too.
        let conn = Connection::open_in_memory().unwrap();
        conn.execute(
            "CREATE TABLE saved_aggregates (
                id TEXT PRIMARY KEY, conn_uuid TEXT NOT NULL, database TEXT NOT NULL,
                collection TEXT NOT NULL DEFAULT '', name TEXT NOT NULL,
                kind TEXT NOT NULL DEFAULT 'aggregate', pipeline_json TEXT NOT NULL,
                created_at REAL NOT NULL, updated_at REAL NOT NULL)",
            [],
        )
        .unwrap();
        create_schema(&conn).unwrap();
        upsert_full_impl(&conn, &sample("n1", "db")).unwrap();
        assert!(get_aggregate_impl(&conn, "n1").unwrap().is_some());
    }

    #[test]
    fn file_store_sets_a_busy_timeout_and_serves_concurrent_writers() {
        let dir = std::env::temp_dir().join(format!("tablex-agg-{}", std::process::id()));
        let path = dir.join("nested").join("aggregates.db");
        let _ = std::fs::remove_dir_all(&dir);
        let store = AggregateStore::open(&path).unwrap();
        let timeout: i64 = locked(&store.conn)
            .query_row("PRAGMA busy_timeout", [], |row| row.get(0))
            .unwrap();
        assert_eq!(timeout, BUSY_TIMEOUT.as_millis() as i64);
        // Several connections (separate stores = separate processes' view)
        // write the same file at once; the busy timeout makes them queue
        // instead of failing with "database is locked".
        let handles: Vec<_> = (0..4)
            .map(|worker| {
                let path = path.clone();
                std::thread::spawn(move || {
                    let store = AggregateStore::open(&path).unwrap();
                    for index in 0..25 {
                        store
                            .upsert(&sample(&format!("w{worker}-{index}"), "db"))
                            .unwrap();
                    }
                })
            })
            .collect();
        for handle in handles {
            handle.join().unwrap();
        }
        assert_eq!(store.list("conn", "db").unwrap().len(), 100);
        drop(store);
        let reopened = AggregateStore::open(&path).unwrap();
        assert_eq!(reopened.list("conn", "db").unwrap().len(), 100, "persisted");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn adding_a_column_another_process_already_added_is_success_but_real_errors_are_not() {
        let conn = fresh_db();
        // `options_json` exists already: the race outcome.
        add_column(
            &conn,
            "ALTER TABLE saved_aggregates ADD COLUMN options_json TEXT",
        )
        .expect("duplicate column is tolerated");
        add_column(
            &conn,
            "ALTER TABLE saved_aggregates ADD COLUMN extra_col TEXT",
        )
        .unwrap();
        add_column(
            &conn,
            "ALTER TABLE saved_aggregates ADD COLUMN extra_col TEXT",
        )
        .unwrap();
        // A genuine failure still surfaces.
        assert!(add_column(&conn, "ALTER TABLE no_such_table ADD COLUMN x TEXT").is_err());
        assert!(add_column(&conn, "ALTER TABLE saved_aggregates ADD COLUMN").is_err());
        create_schema(&conn).expect("migration still fine");
    }

    #[test]
    fn a_poisoned_lock_does_not_panic_later_calls() {
        let store = std::sync::Arc::new(AggregateStore::open_in_memory().unwrap());
        store.upsert(&sample("p1", "db")).unwrap();
        let poisoner = std::sync::Arc::clone(&store);
        let _ = std::thread::spawn(move || {
            let _guard = poisoner.conn.lock().unwrap();
            panic!("poison the mutex on purpose");
        })
        .join();
        assert!(store.conn.is_poisoned());
        assert_eq!(store.list("conn", "db").unwrap().len(), 1);
        store.upsert(&sample("p2", "db")).unwrap();
        store.delete("p1").unwrap();
        assert!(store.get("p1").unwrap().is_none());
    }

    #[test]
    fn store_crud_round_trip() {
        let store = AggregateStore::open_in_memory().unwrap();
        let mut item = sample("s1", "db");
        item.name = "Zeta".into();
        store.upsert(&item).unwrap();
        item.id = "s2".into();
        item.name = "alpha".into();
        store.upsert(&item).unwrap();
        let names: Vec<_> = store
            .list("conn", "db")
            .unwrap()
            .into_iter()
            .map(|row| row.name)
            .collect();
        assert_eq!(names, ["alpha", "Zeta"], "case-insensitive name order");
        store.delete("s1").unwrap();
        assert_eq!(store.list("conn", "db").unwrap().len(), 1);
    }

    #[test]
    fn kind_defaults_to_aggregate() {
        let conn = fresh_db();
        upsert_aggregate_impl(
            &conn,
            "k1",
            "c",
            "d",
            "coll",
            "n",
            "mongosh",
            "db.coll.find()",
            1.0,
            1.0,
        )
        .unwrap();
        let got = get_aggregate_impl(&conn, "k1").unwrap().unwrap();
        assert_eq!(got.kind, "mongosh");
    }
}
