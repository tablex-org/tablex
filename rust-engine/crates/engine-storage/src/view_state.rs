//! Per-connection view state: the `connection_view_state` table of the app database
//! ([`crate::app_db`]), one cached connection per process.

use crate::app_db::{self, AppDbError};
use crate::legacy;
use rusqlite::{params, Connection, Result};
use std::path::{Path, PathBuf};
use std::sync::Mutex;

lazy_static::lazy_static! {
    static ref VIEW_STATE_DB: Mutex<Option<legacy::Store>> = Mutex::new(None);
}

/// The file the view state was before `tablex.db`: `<data dir>/TableLeaf/view_state.db`. Read once
/// to move its rows into the app database (see [`crate::legacy`]); no longer written.
pub(crate) fn legacy_path() -> PathBuf {
    crate::paths::data_dir("TableLeaf").join("view_state.db")
}

/// The app database at `db` with the old `legacy` file moved in on first use.
pub(crate) fn open_at(db: &Path, legacy: &Path) -> std::result::Result<Connection, AppDbError> {
    let mut conn = app_db::open(db)?;
    legacy::import_once(&mut conn, db, "view_state", legacy, |tx| import_view_state(tx, legacy, None))?;
    Ok(conn)
}

fn import_view_state(
    tx: &rusqlite::Transaction<'_>,
    path: &Path,
    merge: Option<&legacy::Since>,
) -> std::result::Result<(), legacy::ImportError> {
    let old = legacy::open_old(path)?;
    legacy::copy_table(
        &old,
        path,
        tx,
        "connection_view_state",
        &[("conn_uuid", "NULL"), ("state_json", "NULL"), ("updated_at", "NULL")],
        "rowid",
        if merge.is_some() {
            legacy::Copying::Newer { key: &["conn_uuid"], updated: "updated_at" }
        } else {
            legacy::Copying::Fresh
        },
        merge,
    )
}

fn ensure_db() -> Result<()> {
    let mut db = VIEW_STATE_DB.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
    match db.as_mut() {
        None => *db = Some(open_or_fall_back(&app_db::default_path(), &legacy_path())?),
        Some(store) => store.refresh(),
    }
    Ok(())
}

/// The store for this run (see [`legacy::open_run`]).
pub(crate) fn open_or_fall_back(db: &Path, legacy: &Path) -> Result<legacy::Store> {
    legacy::open_run(db, "view_state", legacy, &["connection_view_state"], import_view_state, open_legacy_file)
}

/// The old `view_state.db` as the previous version opened it. Only for a run whose import failed.
fn open_legacy_file(path: &Path) -> Result<Connection> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).map_err(|e| rusqlite::Error::ToSqlConversionFailure(e.into()))?;
    }
    let conn = Connection::open(path)?;
    conn.busy_timeout(std::time::Duration::from_secs(5))?;
    conn.execute(
        "CREATE TABLE IF NOT EXISTS connection_view_state (
            conn_uuid TEXT PRIMARY KEY, state_json TEXT NOT NULL, updated_at REAL NOT NULL)",
        [],
    )?;
    conn.execute(
        "CREATE INDEX IF NOT EXISTS idx_view_state_updated ON connection_view_state(updated_at DESC)",
        [],
    )?;
    Ok(conn)
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
    use crate::paths::TEST_ENV_LOCK as TEST_LOCK;

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
