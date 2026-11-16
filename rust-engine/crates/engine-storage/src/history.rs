//! Query history: the `query_history` table of the app database ([`crate::app_db`]), one cached
//! connection per process.

use crate::app_db::{self, AppDbError};
use crate::legacy;
use rusqlite::{params, Connection, Result};
use std::path::{Path, PathBuf};
use std::sync::Mutex;

lazy_static::lazy_static! {
    static ref HISTORY_DB: Mutex<Option<legacy::Store>> = Mutex::new(None);
}

#[derive(Debug, Clone)]
pub struct HistoryEntry {
    pub id: i64,
    pub conn_uuid: String,
    pub database: String,
    pub collection: String,
    pub filter: String,
    pub project: String,
    pub sort: String,
    pub skip: i64,
    pub limit: i64,
    pub max_time_ms: i64,
    pub result_count: i64,
    pub timestamp: f64,
}

/// The file the history was before `tablex.db`: `<data dir>/TableLeaf/history.db`. Read once to
/// move its rows into the app database (see [`crate::legacy`]); no longer written.
pub(crate) fn legacy_path() -> PathBuf {
    crate::paths::data_dir("TableLeaf").join("history.db")
}

/// The app database at `db` with the old `legacy` file moved in on first use.
pub(crate) fn open_at(db: &Path, legacy: &Path) -> std::result::Result<Connection, AppDbError> {
    let mut conn = app_db::open(db)?;
    legacy::import_once(&mut conn, db, "history", legacy, |tx| import_history(tx, legacy, None))?;
    Ok(conn)
}

/// Copies the rows of an old `history.db`, newest id first so that of two rows the old code
/// would have deduplicated (an unique index it created on every open) the newer one stays. An
/// old table without `conn_uuid` is from before the history was per connection: the old code
/// dropped it on open, so it is nothing to copy. `merge`: into a database that has rows of its
/// own already (see [`legacy::Store`]): ids are given anew (the old ones may be taken) and a row
/// the database has stays.
fn import_history(
    tx: &rusqlite::Transaction<'_>,
    path: &Path,
    merge: Option<&legacy::Since>,
) -> std::result::Result<(), legacy::ImportError> {
    let old = legacy::open_old(path)?;
    let has_connection = old
        .query_row(
            "SELECT COUNT(*) FROM pragma_table_info('query_history') WHERE name = 'conn_uuid'",
            [],
            |row| row.get::<_, i64>(0),
        )
        .map_err(|error| legacy::ImportError::new(path, error))?
        > 0;
    if !has_connection {
        return Ok(());
    }
    legacy::copy_table(
        &old,
        path,
        tx,
        "query_history",
        if merge.is_some() { &HISTORY_COLUMNS[1..] } else { &HISTORY_COLUMNS },
        "id DESC",
        legacy::Copying::Skip,
        merge,
    )
}

/// The columns of `query_history`, with what a row of an old file that lacks one gets.
const HISTORY_COLUMNS: [(&str, &str); 12] = [
    ("id", "NULL"),
    ("conn_uuid", "NULL"),
    ("database", "NULL"),
    ("collection", "NULL"),
    ("filter", "''"),
    ("project", "''"),
    ("sort", "''"),
    ("skip", "0"),
    ("limit", "0"),
    ("max_time_ms", "0"),
    ("result_count", "0"),
    ("timestamp", "NULL"),
];

fn ensure_db() -> Result<()> {
    let mut db = HISTORY_DB.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
    match db.as_mut() {
        None => *db = Some(open_or_fall_back(&app_db::default_path(), &legacy_path())?),
        Some(store) => store.refresh(),
    }
    Ok(())
}

/// The store for this run (see [`legacy::open_run`]): the app database with the old file moved
/// in; when that fails, the old file itself, so that the history is never empty because of an
/// import error; in a process that may not import, the old file until the app has.
pub(crate) fn open_or_fall_back(db: &Path, legacy: &Path) -> Result<legacy::Store> {
    legacy::open_run(db, "history", legacy, &["query_history"], import_history, open_legacy_file)
}

/// The old `history.db` as the previous version opened it. Only for a run whose import failed.
fn open_legacy_file(path: &Path) -> Result<Connection> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).map_err(|e| rusqlite::Error::ToSqlConversionFailure(e.into()))?;
    }
    let conn = Connection::open(path)?;
    conn.busy_timeout(std::time::Duration::from_secs(5))?;
    let has_conn_uuid = conn
        .query_row(
            "SELECT COUNT(*) FROM pragma_table_info('query_history') WHERE name='conn_uuid'",
            [],
            |row| row.get::<_, i64>(0),
        )
        .unwrap_or(0)
        > 0;
    if !has_conn_uuid {
        conn.execute("DROP TABLE IF EXISTS query_history", [])?;
    }
    conn.execute(
        "CREATE TABLE IF NOT EXISTS query_history (
            id INTEGER PRIMARY KEY AUTOINCREMENT, conn_uuid TEXT NOT NULL, database TEXT NOT NULL,
            collection TEXT NOT NULL, filter TEXT DEFAULT '', project TEXT DEFAULT '', sort TEXT DEFAULT '',
            skip INTEGER DEFAULT 0, \"limit\" INTEGER DEFAULT 0, max_time_ms INTEGER DEFAULT 0,
            result_count INTEGER DEFAULT 0, timestamp REAL NOT NULL)",
        [],
    )?;
    conn.execute(
        "CREATE INDEX IF NOT EXISTS idx_history_context ON query_history(conn_uuid, database, collection)",
        [],
    )?;
    conn.execute(
        "CREATE INDEX IF NOT EXISTS idx_history_timestamp ON query_history(timestamp DESC)",
        [],
    )?;
    conn.execute(
        "DELETE FROM query_history WHERE id NOT IN (
             SELECT MAX(id) FROM query_history
             GROUP BY conn_uuid, database, collection, filter, project, sort, skip, \"limit\", max_time_ms)",
        [],
    )?;
    conn.execute("DROP INDEX IF EXISTS idx_history_dedup", [])?;
    conn.execute(
        "CREATE UNIQUE INDEX idx_history_dedup ON query_history(
             conn_uuid, database, collection, filter, project, sort, skip, \"limit\", max_time_ms)",
        [],
    )?;
    Ok(conn)
}

// Keep FFI signatures stable; parameter objects would churn every Swift caller.
#[allow(clippy::too_many_arguments)]
pub fn save_entry(
    conn_uuid: &str,
    database: &str,
    collection: &str,
    filter: &str,
    project: &str,
    sort: &str,
    skip: i64,
    limit: i64,
    max_time_ms: i64,
    result_count: i64,
) -> Result<()> {
    eprintln!(
        "[history] save_entry called: conn_uuid={} {}.{} filter={}",
        conn_uuid, database, collection, filter
    );
    ensure_db()?;
    let db = HISTORY_DB.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
    let conn = db.as_ref().unwrap();

    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        // A clock before 1970 must not panic while a storage lock is held.
        .unwrap_or_default()
        .as_secs_f64();

    let rows = conn.execute(
        "INSERT OR IGNORE INTO query_history
         (conn_uuid, database, collection, filter, project, sort,
          skip, \"limit\", max_time_ms, result_count, timestamp)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11)",
        params![
            conn_uuid,
            database,
            collection,
            filter,
            project,
            sort,
            skip,
            limit,
            max_time_ms,
            result_count,
            now
        ],
    )?;
    if rows == 0 {
        eprintln!("[history] Skipped duplicate entry");
    } else {
        eprintln!("[history] Inserted {} rows", rows);
    }

    conn.execute(
        "DELETE FROM query_history
         WHERE id NOT IN (
             SELECT id FROM query_history
             WHERE conn_uuid = ?1 AND database = ?2 AND collection = ?3
             ORDER BY timestamp DESC LIMIT 500
         )
         AND conn_uuid = ?1 AND database = ?2 AND collection = ?3",
        params![conn_uuid, database, collection],
    )?;

    Ok(())
}

pub fn fetch_entries(
    conn_uuid: &str,
    database: &str,
    collection: &str,
    limit: u32,
) -> Result<Vec<HistoryEntry>> {
    ensure_db()?;
    let db = HISTORY_DB.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
    let conn = db.as_ref().unwrap();

    let mut stmt = conn.prepare(
        "SELECT id, conn_uuid, database, collection, filter, project, sort,
                skip, \"limit\", max_time_ms, result_count, timestamp
         FROM query_history
         WHERE conn_uuid = ?1 AND database = ?2 AND collection = ?3
         ORDER BY timestamp DESC
         LIMIT ?4",
    )?;

    let entries = stmt.query_map(params![conn_uuid, database, collection, limit], |row| {
        Ok(HistoryEntry {
            id: row.get(0)?,
            conn_uuid: row.get(1)?,
            database: row.get(2)?,
            collection: row.get(3)?,
            filter: row.get(4)?,
            project: row.get(5)?,
            sort: row.get(6)?,
            skip: row.get(7)?,
            limit: row.get(8)?,
            max_time_ms: row.get(9)?,
            result_count: row.get(10)?,
            timestamp: row.get(11)?,
        })
    })?;

    entries.collect()
}

pub fn delete_entry(id: i64) -> Result<()> {
    ensure_db()?;
    let db = HISTORY_DB.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
    let conn = db.as_ref().unwrap();
    conn.execute("DELETE FROM query_history WHERE id = ?1", params![id])?;
    Ok(())
}

pub fn clear_entries(conn_uuid: &str, database: &str, collection: &str) -> Result<()> {
    ensure_db()?;
    let db = HISTORY_DB.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
    let conn = db.as_ref().unwrap();
    conn.execute(
        "DELETE FROM query_history WHERE conn_uuid = ?1 AND database = ?2 AND collection = ?3",
        params![conn_uuid, database, collection],
    )?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::paths::TEST_ENV_LOCK as TEST_LOCK;

    fn reset_db_for_test() {
        let mut db = HISTORY_DB.lock().unwrap();
        *db = None;
    }

    fn use_temp_home(name: &str) {
        let dir = std::env::temp_dir().join(format!(
            "tableleaf-history-test-{}-{}",
            name,
            std::process::id()
        ));
        if dir.exists() {
            std::fs::remove_dir_all(&dir).expect("remove temp history home");
        }
        std::fs::create_dir_all(&dir).expect("create temp history home");
        std::env::set_var(crate::paths::DATA_DIR_ENV, dir);
        reset_db_for_test();
    }

    #[test]
    fn save_and_fetch_history_entry_round_trips() {
        let _guard = TEST_LOCK.lock().unwrap();
        use_temp_home("roundtrip");

        save_entry(
            "conn-1",
            "app",
            "users",
            "{}",
            "",
            "{name: 1}",
            0,
            100,
            5000,
            42,
        )
        .expect("save history entry");

        let entries = fetch_entries("conn-1", "app", "users", 10).expect("fetch history");
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].conn_uuid, "conn-1");
        assert_eq!(entries[0].database, "app");
        assert_eq!(entries[0].collection, "users");
        assert_eq!(entries[0].sort, "{name: 1}");
        assert_eq!(entries[0].result_count, 42);
    }

    #[test]
    fn clear_history_removes_matching_context() {
        let _guard = TEST_LOCK.lock().unwrap();
        use_temp_home("clear");

        save_entry("conn-1", "app", "users", "{}", "", "", 0, 10, 0, 1)
            .expect("save history entry");
        clear_entries("conn-1", "app", "users").expect("clear history");

        let entries = fetch_entries("conn-1", "app", "users", 10).expect("fetch history");
        assert!(entries.is_empty());
    }

    #[test]
    fn a_failed_import_keeps_the_old_history_in_use_for_the_run_and_the_next_start_brings_everything_in() {
        let _guard = TEST_LOCK.lock().unwrap();
        use_temp_home("fallback");
        let old = legacy_path();
        let conn = open_legacy_file(&old).unwrap();
        conn.execute_batch(
            "INSERT INTO query_history (conn_uuid, database, collection, filter, timestamp) VALUES ('c', 'd', 'x', 'a', 1.0), ('c', 'd', 'x', 'b', 2.0)",
        )
        .unwrap();
        drop(conn);
        let db = app_db::default_path();
        app_db::open(&db)
            .unwrap()
            .execute_batch("CREATE TRIGGER no_rows BEFORE INSERT ON query_history BEGIN SELECT RAISE(ABORT, 'disk full'); END")
            .unwrap();
        reset_db_for_test();

        // The history is there, and a new entry goes to the old file.
        assert_eq!(fetch_entries("c", "d", "x", 10).expect("the old file is used").len(), 2);
        save_entry("c", "d", "x", "c", "", "", 0, 0, 0, 1).expect("written to the old file");
        assert_eq!(fetch_entries("c", "d", "x", 10).unwrap().len(), 3);
        let warned = crate::legacy::warnings_snapshot();
        assert!(
            warned.iter().any(|w| w.contains(&old.display().to_string()) && w.contains("disk full")),
            "one warning naming the file: {warned:?}"
        );
        let in_new: i64 = app_db::open(&db).unwrap().query_row("SELECT COUNT(*) FROM query_history", [], |r| r.get(0)).unwrap();
        assert_eq!(in_new, 0);

        app_db::open(&db).unwrap().execute_batch("DROP TRIGGER no_rows").unwrap();
        reset_db_for_test();
        assert_eq!(fetch_entries("c", "d", "x", 10).unwrap().len(), 3, "the next start has all three");
        let in_new: i64 = app_db::open(&db).unwrap().query_row("SELECT COUNT(*) FROM query_history", [], |r| r.get(0)).unwrap();
        assert_eq!(in_new, 3);
    }
}
