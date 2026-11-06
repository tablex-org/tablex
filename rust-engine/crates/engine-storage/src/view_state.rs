use rusqlite::{params, Connection, Result};
use std::path::PathBuf;
use std::sync::Mutex;

lazy_static::lazy_static! {
    static ref VIEW_STATE_DB: Mutex<Option<Connection>> = Mutex::new(None);
}

fn get_db_path() -> PathBuf {
    crate::paths::data_dir("TableLeaf").join("view_state.db")
}

fn ensure_db() -> Result<()> {
    let mut db = VIEW_STATE_DB.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
    if db.is_none() {
        let path = get_db_path();
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)
                .map_err(|e| rusqlite::Error::ToSqlConversionFailure(e.into()))?;
        }
        let conn = Connection::open(&path)?;
        conn.execute(
            "CREATE TABLE IF NOT EXISTS connection_view_state (
                conn_uuid TEXT PRIMARY KEY,
                state_json TEXT NOT NULL,
                updated_at REAL NOT NULL
            )",
            [],
        )?;
        conn.execute(
            "CREATE INDEX IF NOT EXISTS idx_view_state_updated
                ON connection_view_state(updated_at DESC)",
            [],
        )?;
        *db = Some(conn);
    }
    Ok(())
}

pub fn save_state(conn_uuid: &str, state_json: &str) -> Result<()> {
    ensure_db()?;
    let db = VIEW_STATE_DB.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
    let conn = db.as_ref().unwrap();

    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        // A clock before 1970 must not panic while a storage lock is held.
        .unwrap_or_default()
        .as_secs_f64();

    conn.execute(
        "INSERT OR REPLACE INTO connection_view_state
         (conn_uuid, state_json, updated_at)
         VALUES (?1, ?2, ?3)",
        params![conn_uuid, state_json, now],
    )?;

    Ok(())
}

pub fn load_state(conn_uuid: &str) -> Result<Option<String>> {
    ensure_db()?;
    let db = VIEW_STATE_DB.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
    let conn = db.as_ref().unwrap();

    let mut stmt =
        conn.prepare("SELECT state_json FROM connection_view_state WHERE conn_uuid = ?1")?;

    let result = stmt.query_row(params![conn_uuid], |row| row.get::<_, String>(0));

    match result {
        Ok(json) => Ok(Some(json)),
        Err(rusqlite::Error::QueryReturnedNoRows) => Ok(None),
        Err(e) => Err(e),
    }
}

pub fn delete_state(conn_uuid: &str) -> Result<()> {
    ensure_db()?;
    let db = VIEW_STATE_DB.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
    let conn = db.as_ref().unwrap();

    conn.execute(
        "DELETE FROM connection_view_state WHERE conn_uuid = ?1",
        params![conn_uuid],
    )?;

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex;

    static TEST_LOCK: Mutex<()> = Mutex::new(());

    fn reset_db_for_test() {
        let mut db = VIEW_STATE_DB.lock().unwrap();
        *db = None;
    }

    fn use_temp_home(name: &str) {
        let dir = std::env::temp_dir().join(format!(
            "tableleaf-view-state-test-{}-{}",
            name,
            std::process::id()
        ));
        if dir.exists() {
            std::fs::remove_dir_all(&dir).expect("remove temp view-state home");
        }
        std::fs::create_dir_all(&dir).expect("create temp view-state home");
        std::env::set_var(crate::paths::DATA_DIR_ENV, dir);
        reset_db_for_test();
    }

    #[test]
    fn save_load_delete_state_round_trips() {
        let _guard = TEST_LOCK.lock().unwrap();
        use_temp_home("roundtrip");

        save_state("conn-1", r#"{"selected":"users"}"#).expect("save view state");
        let loaded = load_state("conn-1").expect("load view state");
        assert_eq!(loaded.as_deref(), Some(r#"{"selected":"users"}"#));

        delete_state("conn-1").expect("delete view state");
        let loaded = load_state("conn-1").expect("load deleted view state");
        assert!(loaded.is_none());
    }
}
