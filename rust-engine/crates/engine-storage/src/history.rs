use rusqlite::{params, Connection, Result};
use std::path::PathBuf;
use std::sync::Mutex;

lazy_static::lazy_static! {
    static ref HISTORY_DB: Mutex<Option<Connection>> = Mutex::new(None);
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

fn get_db_path() -> PathBuf {
    crate::paths::data_dir("TableLeaf").join("history.db")
}

fn ensure_db() -> Result<()> {
    let mut db = HISTORY_DB.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
    if db.is_none() {
        let path = get_db_path();
        eprintln!("[history] Opening DB at: {:?}", path);
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)
                .map_err(|e| rusqlite::Error::ToSqlConversionFailure(e.into()))?;
        }
        let conn = Connection::open(&path)?;
        eprintln!("[history] DB opened, creating tables...");

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
                id INTEGER PRIMARY KEY AUTOINCREMENT,
                conn_uuid TEXT NOT NULL,
                database TEXT NOT NULL,
                collection TEXT NOT NULL,
                filter TEXT DEFAULT '',
                project TEXT DEFAULT '',
                sort TEXT DEFAULT '',
                skip INTEGER DEFAULT 0,
                \"limit\" INTEGER DEFAULT 0,
                max_time_ms INTEGER DEFAULT 0,
                result_count INTEGER DEFAULT 0,
                timestamp REAL NOT NULL
            )",
            [],
        )?;
        eprintln!("[history] Table created");
        conn.execute(
            "CREATE INDEX IF NOT EXISTS idx_history_context
                ON query_history(conn_uuid, database, collection)",
            [],
        )?;
        conn.execute(
            "CREATE INDEX IF NOT EXISTS idx_history_timestamp
                ON query_history(timestamp DESC)",
            [],
        )?;

        conn.execute(
            "DELETE FROM query_history
             WHERE id NOT IN (
                 SELECT MAX(id) FROM query_history
                 GROUP BY conn_uuid, database, collection, filter, project, sort, skip, \"limit\", max_time_ms
             )",
            [],
        )?;

        conn.execute("DROP INDEX IF EXISTS idx_history_dedup", [])?;
        conn.execute(
            "CREATE UNIQUE INDEX idx_history_dedup
                ON query_history(conn_uuid, database, collection, filter, project, sort, skip, \"limit\", max_time_ms)",
            [],
        )?;
        eprintln!("[history] Indexes created");
        *db = Some(conn);
    }
    Ok(())
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
    use std::sync::Mutex;

    static TEST_LOCK: Mutex<()> = Mutex::new(());

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
}
