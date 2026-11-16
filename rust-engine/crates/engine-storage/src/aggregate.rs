//! Saved aggregation pipelines: the `saved_aggregates` table of the app database
//! ([`crate::app_db`]).

use crate::app_db::{self, AppDbError};
use crate::legacy;
use rusqlite::{params, Connection, Result};
use std::path::{Path, PathBuf};
use std::sync::{Mutex, MutexGuard, PoisonError};

lazy_static::lazy_static! {
    static ref AGGREGATE_DB: Mutex<Option<legacy::Store>> = Mutex::new(None);
}

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

/// The file the saved aggregates were before `tablex.db`: `<data dir>/TableLeaf/aggregates.db`.
/// Read once to move its rows into the app database (see [`crate::legacy`]); no longer written.
pub(crate) fn legacy_path() -> PathBuf {
    crate::paths::data_dir("TableLeaf").join("aggregates.db")
}

/// The app database at `db` with the old `legacy` file moved in on first use.
pub(crate) fn open_at(db: &Path, legacy: &Path) -> std::result::Result<Connection, AppDbError> {
    let mut conn = app_db::open(db)?;
    legacy::import_once(&mut conn, db, "aggregates", legacy, |tx| import_aggregates(tx, legacy, None))?;
    Ok(conn)
}

/// `kind` and `options_json` were added after the first release: a file without them reads with
/// what such rows always had (a plain aggregate, no editor options). `merge`: into a database
/// that has rows already, the later row wins.
fn import_aggregates(
    tx: &rusqlite::Transaction<'_>,
    path: &Path,
    merge: Option<&legacy::Since>,
) -> std::result::Result<(), legacy::ImportError> {
    let old = legacy::open_old(path)?;
    legacy::copy_table(
        &old,
        path,
        tx,
        "saved_aggregates",
        &[
            ("id", "NULL"),
            ("conn_uuid", "NULL"),
            ("database", "NULL"),
            ("collection", "''"),
            ("name", "NULL"),
            ("kind", "'aggregate'"),
            ("pipeline_json", "NULL"),
            ("created_at", "NULL"),
            ("updated_at", "NULL"),
            ("options_json", "NULL"),
        ],
        "rowid",
        if merge.is_some() {
            legacy::Copying::Newer { key: &["id"], updated: "updated_at" }
        } else {
            legacy::Copying::Fresh
        },
        merge,
    )
}

/// The store for this run (see [`legacy::open_run`]): the app database with the old file moved
/// in; when that fails, the old file itself, so that the saved pipelines are never missing because
/// of an import error; in a process that may not import, the old file until the app has.
pub(crate) fn open_or_fall_back(db: &Path, legacy: &Path) -> Result<legacy::Store> {
    legacy::open_run(db, "aggregates", legacy, &["saved_aggregates"], import_aggregates, open_legacy_file)
}

/// The old `aggregates.db` as the previous version opened it. Only for a run whose import failed.
fn open_legacy_file(path: &Path) -> Result<Connection> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).map_err(|e| rusqlite::Error::ToSqlConversionFailure(e.into()))?;
    }
    let conn = Connection::open(path)?;
    conn.busy_timeout(std::time::Duration::from_secs(5))?;
    conn.execute(
        "CREATE TABLE IF NOT EXISTS saved_aggregates (
            id TEXT PRIMARY KEY, conn_uuid TEXT NOT NULL, database TEXT NOT NULL,
            collection TEXT NOT NULL DEFAULT '', name TEXT NOT NULL, kind TEXT NOT NULL DEFAULT 'aggregate',
            pipeline_json TEXT NOT NULL, created_at REAL NOT NULL, updated_at REAL NOT NULL)",
        [],
    )?;
    let _ = conn.execute("DROP INDEX IF EXISTS idx_saved_agg_lookup", []);
    conn.execute(
        "CREATE INDEX IF NOT EXISTS idx_saved_agg_db_lookup ON saved_aggregates(conn_uuid, database)",
        [],
    )?;
    for (column, ddl) in [
        ("kind", "ALTER TABLE saved_aggregates ADD COLUMN kind TEXT NOT NULL DEFAULT 'aggregate'"),
        ("options_json", "ALTER TABLE saved_aggregates ADD COLUMN options_json TEXT"),
    ] {
        let present: i64 = conn.query_row(
            "SELECT COUNT(*) FROM pragma_table_info('saved_aggregates') WHERE name = ?1",
            params![column],
            |row| row.get(0),
        )?;
        if present == 0 {
            conn.execute(ddl, [])?;
        }
    }
    Ok(conn)
}

/// Saved aggregates in the app database at an explicit location (`open`: a file of its own, as
/// tests use; `open_default`: `tablex.db`, with the old `aggregates.db` moved in). The free
/// functions below use the shared per-process default; the GPUI app owns a store so tests can
/// point it at a temporary file or memory.
pub struct AggregateStore {
    conn: Mutex<legacy::Store>,
}

impl AggregateStore {
    pub fn open(path: &Path) -> Result<Self> {
        Ok(Self {
            conn: Mutex::new(legacy::Store::new(app_db::open(path)?)),
        })
    }

    pub fn open_default() -> Result<Self> {
        Ok(Self {
            conn: Mutex::new(open_or_fall_back(&app_db::default_path(), &legacy_path())?),
        })
    }

    pub fn open_in_memory() -> Result<Self> {
        let conn = app_db::open_in_memory()?;
        Ok(Self {
            conn: Mutex::new(legacy::Store::new(conn)),
        })
    }

    /// The connection, after looking whether a store that worked on its old file can go on in the
    /// database (see [`legacy::Store::refresh`]).
    fn conn(&self) -> MutexGuard<'_, legacy::Store> {
        let mut store = locked(&self.conn);
        store.refresh();
        store
    }

    pub fn list(&self, conn_uuid: &str, database: &str) -> Result<Vec<SavedAggregate>> {
        list_aggregates_impl(&self.conn(), conn_uuid, database)
    }

    pub fn get(&self, id: &str) -> Result<Option<SavedAggregate>> {
        get_aggregate_impl(&self.conn(), id)
    }

    /// Inserts or updates a row, including `database` and `options_json`
    /// (`None` clears it).
    pub fn upsert(&self, aggregate: &SavedAggregate) -> Result<()> {
        upsert_full_impl(&self.conn(), aggregate)
    }

    pub fn delete(&self, id: &str) -> Result<()> {
        delete_aggregate_impl(&self.conn(), id)
    }
}

/// Runs `f` on the shared default connection, opening it on first use.
fn with_db<T>(f: impl FnOnce(&Connection) -> Result<T>) -> Result<T> {
    let mut db = locked(&AGGREGATE_DB);
    match db.as_mut() {
        None => *db = Some(open_or_fall_back(&app_db::default_path(), &legacy_path())?),
        Some(store) => store.refresh(),
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

pub(crate) fn get_aggregate_impl(conn: &Connection, id: &str) -> Result<Option<SavedAggregate>> {
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
        app_db::open_in_memory().unwrap()
    }

    #[test]
    fn upsert_inserts_new_aggregate() {
        let conn = fresh_db();
        upsert_aggregate_impl(
            &conn,
            "agg-1",
            "conn-uuid",
            "shop-service",
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
            "shop-service",
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
            "shop-service",
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
            "shop-service",
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
            "shop-service",
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
    fn file_store_sets_a_busy_timeout_and_serves_concurrent_writers() {
        let dir = std::env::temp_dir().join(format!("tablex-agg-{}", std::process::id()));
        let path = dir.join("nested").join("aggregates.db");
        let _ = std::fs::remove_dir_all(&dir);
        let store = AggregateStore::open(&path).unwrap();
        let timeout: i64 = locked(&store.conn)
            .query_row("PRAGMA busy_timeout", [], |row| row.get(0))
            .unwrap();
        assert_eq!(timeout, 5000);
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
