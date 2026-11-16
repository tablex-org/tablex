//! Saved connections and projects: the `saved_connections`, `projects` and
//! `project_connection_memberships` tables of the app database ([`crate::app_db`]). The functions
//! keep one connection per process; `tablex-mcp` reads the same rows.

use crate::app_db::{self, AppDbError};
use crate::legacy;
use rusqlite::{params, Connection, OptionalExtension, Result};
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};
use std::sync::Mutex;

lazy_static::lazy_static! {
    static ref CONNECTION_DB: Mutex<Option<legacy::Store>> = Mutex::new(None);
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SavedConnectionRecord {
    pub id: String,
    pub name: String,
    pub uri: String,
    pub db_type: String,
    pub color_hex: String,
    pub save_password: bool,
    pub save_proxy_password: bool,
    pub network_settings_json: String,
    pub is_pinned: bool,
    pub last_connected: Option<f64>,
    pub created_at: f64,
    pub updated_at: f64,
    /// Id of the connection type (Development / Test / Production / a custom
    /// one defined in the app settings). Empty = no type: the connection
    /// follows the global settings. Added after the first release: records
    /// written before it read back as empty.
    #[serde(default)]
    pub connection_type: String,
    /// JSON object of per-connection setting overrides (`{"query.query_timeout_secs": 30}`).
    /// Empty = none. Opaque to storage; the app parses and validates it.
    #[serde(default)]
    pub overrides_json: String,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ProjectRecord {
    pub id: String,
    pub name: String,
    pub path: String,
    pub uses_default_location: bool,
    pub created_at: f64,
    pub updated_at: f64,
}

/// The file the connection store was before `tablex.db`: `<data dir>/tablex/connections.db`. It is
/// read once to move its rows into the app database (see [`crate::legacy`]) and is no longer
/// written: an older TableX still sees the connections as they were at that moment.
pub(crate) fn legacy_path() -> PathBuf {
    crate::paths::data_dir("tablex").join("connections.db")
}

/// The ids of the connections in the old `connections.db` beside the app database at `db` (none
/// when there is no such file). For a front end that must find what belongs to a connection
/// before the connections were moved in (the saved secrets of an earlier version, say); the error
/// names the file when it cannot be read.
pub fn old_connection_ids(db: &Path) -> std::result::Result<Vec<String>, String> {
    let Some(file) = legacy::beside(db).map(|dir| dir.join("connections.db")).filter(|file| file.exists()) else {
        return Ok(Vec::new());
    };
    let say = |error: &dyn std::fmt::Display| format!("{}: {error}", file.display());
    let old = legacy::open_old(&file).map_err(|error| say(&error.message))?;
    let has_table = old
        .query_row("SELECT COUNT(*) FROM sqlite_master WHERE type = 'table' AND name = 'saved_connections'", [], |row| row.get::<_, i64>(0))
        .map_err(|error| say(&error))?
        > 0;
    if !has_table {
        return Ok(Vec::new());
    }
    let mut stmt = old.prepare("SELECT id FROM saved_connections").map_err(|error| say(&error))?;
    let ids = stmt
        .query_map([], |row| row.get::<_, String>(0))
        .and_then(|rows| rows.collect::<std::result::Result<Vec<_>, _>>())
        .map_err(|error| say(&error));
    ids
}

/// The app database at `db` with the old `legacy` file moved in on first use.
pub(crate) fn open_at(db: &Path, legacy: &Path) -> std::result::Result<Connection, AppDbError> {
    let mut conn = app_db::open(db)?;
    legacy::import_once(&mut conn, db, "connections", legacy, |tx| import_connections(tx, legacy, None))?;
    Ok(conn)
}

/// Copies the rows of an old `connections.db` (`merge`: into a database that has some already, the later row
/// wins and only what was written since a run began on the old file comes). A file from before `connection_type` and `overrides_json` (they were
/// added after the first release) reads with the defaults those rows always had: no type, no
/// overrides. The three tables are read in one transaction of the old file, so they agree.
fn import_connections(
    tx: &rusqlite::Transaction<'_>,
    path: &Path,
    merge: Option<&legacy::Since>,
) -> std::result::Result<(), legacy::ImportError> {
    use legacy::Copying;
    let old = legacy::open_old(path)?;
    let _snapshot = old
        .unchecked_transaction()
        .map_err(|error| legacy::ImportError::new(path, error))?;
    let pick = |key: &'static [&'static str], updated: &'static str| {
        if merge.is_some() {
            Copying::Newer { key, updated }
        } else {
            Copying::Fresh
        }
    };
    legacy::copy_table(
        &old,
        path,
        tx,
        "saved_connections",
        &[
            ("id", "NULL"),
            ("name", "NULL"),
            ("uri", "NULL"),
            ("db_type", "NULL"),
            ("color_hex", "NULL"),
            ("save_password", "NULL"),
            ("save_proxy_password", "NULL"),
            ("network_settings_json", "NULL"),
            ("is_pinned", "0"),
            ("last_connected", "NULL"),
            ("created_at", "NULL"),
            ("updated_at", "NULL"),
            ("connection_type", "''"),
            ("overrides_json", "''"),
        ],
        "rowid",
        pick(&["id"], "updated_at"),
        merge,
    )?;
    legacy::copy_table(
        &old,
        path,
        tx,
        "project_connection_memberships",
        &[("project_id", "NULL"), ("connection_id", "NULL"), ("created_at", "NULL")],
        "rowid",
        if merge.is_some() { Copying::Skip } else { Copying::Fresh },
        merge,
    )?;
    legacy::copy_table(
        &old,
        path,
        tx,
        "projects",
        &[
            ("id", "NULL"),
            ("name", "NULL"),
            ("path", "NULL"),
            ("uses_default_location", "NULL"),
            ("created_at", "NULL"),
            ("updated_at", "NULL"),
        ],
        "rowid",
        pick(&["id"], "updated_at"),
        merge,
    )
}

fn ensure_db() -> Result<()> {
    let mut db = CONNECTION_DB.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
    match db.as_mut() {
        None => *db = Some(open_or_fall_back(&app_db::default_path(), &legacy_path())?),
        Some(store) => store.refresh(),
    }
    Ok(())
}

/// The store for this run (see [`legacy::open_run`]): the app database with the old file moved
/// in; when that fails, the old file itself, so that the list of connections is never empty
/// because of an import error; in a process that may not import, the old file until the app has.
pub(crate) fn open_or_fall_back(db: &Path, legacy: &Path) -> Result<legacy::Store> {
    legacy::open_run(
        db,
        "connections",
        legacy,
        &["saved_connections", "project_connection_memberships", "projects"],
        import_connections,
        open_legacy_file,
    )
}

/// The old `connections.db` as the previous version opened it: its own tables, created or
/// extended as that version did. Only for a run whose import failed.
fn open_legacy_file(path: &Path) -> Result<Connection> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).map_err(|e| rusqlite::Error::ToSqlConversionFailure(e.into()))?;
    }
    let conn = Connection::open(path)?;
    conn.busy_timeout(std::time::Duration::from_secs(5))?;
    conn.execute(
        "CREATE TABLE IF NOT EXISTS saved_connections (
            id TEXT PRIMARY KEY, name TEXT NOT NULL, uri TEXT NOT NULL, db_type TEXT NOT NULL,
            color_hex TEXT NOT NULL, save_password INTEGER NOT NULL, save_proxy_password INTEGER NOT NULL,
            network_settings_json TEXT NOT NULL, is_pinned INTEGER NOT NULL DEFAULT 0, last_connected REAL,
            created_at REAL NOT NULL, updated_at REAL NOT NULL)",
        [],
    )?;
    conn.execute(
        "CREATE INDEX IF NOT EXISTS idx_saved_connections_order
            ON saved_connections(is_pinned DESC, name COLLATE NOCASE ASC)",
        [],
    )?;
    conn.execute(
        "CREATE TABLE IF NOT EXISTS project_connection_memberships (
            project_id TEXT NOT NULL, connection_id TEXT NOT NULL, created_at REAL NOT NULL,
            PRIMARY KEY(project_id, connection_id))",
        [],
    )?;
    conn.execute(
        "CREATE INDEX IF NOT EXISTS idx_project_connection_memberships_order
            ON project_connection_memberships(project_id, created_at ASC)",
        [],
    )?;
    conn.execute(
        "CREATE TABLE IF NOT EXISTS projects (
            id TEXT PRIMARY KEY, name TEXT NOT NULL, path TEXT NOT NULL,
            uses_default_location INTEGER NOT NULL, created_at REAL NOT NULL, updated_at REAL NOT NULL)",
        [],
    )?;
    conn.execute(
        "CREATE INDEX IF NOT EXISTS idx_projects_order
            ON projects(name COLLATE NOCASE ASC, created_at ASC)",
        [],
    )?;
    let existing: Vec<String> = conn
        .prepare("PRAGMA table_info(saved_connections)")?
        .query_map([], |row| row.get::<_, String>(1))?
        .collect::<Result<_>>()?;
    for (name, definition) in [
        ("connection_type", "TEXT NOT NULL DEFAULT ''"),
        ("overrides_json", "TEXT NOT NULL DEFAULT ''"),
    ] {
        if !existing.iter().any(|column| column == name) {
            conn.execute(&format!("ALTER TABLE saved_connections ADD COLUMN {name} {definition}"), [])?;
        }
    }
    Ok(conn)
}

pub fn list_connections() -> Result<Vec<SavedConnectionRecord>> {
    ensure_db()?;
    let db = CONNECTION_DB.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
    let conn = db.as_ref().expect("connection db initialized");
    list_connections_impl(conn)
}

pub fn get_connection(id: &str) -> Result<Option<SavedConnectionRecord>> {
    ensure_db()?;
    let db = CONNECTION_DB.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
    let conn = db.as_ref().expect("connection db initialized");
    get_connection_impl(conn, id)
}

pub fn upsert_connection(record: &SavedConnectionRecord) -> Result<()> {
    ensure_db()?;
    let db = CONNECTION_DB.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
    let conn = db.as_ref().expect("connection db initialized");
    upsert_connection_impl(conn, record)
}

pub fn upsert_connections_batch(records: &[SavedConnectionRecord]) -> Result<()> {
    ensure_db()?;
    let mut db = CONNECTION_DB.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
    let conn = db.as_mut().expect("connection db initialized");
    upsert_connections_batch_impl(conn, records)
}

pub fn delete_connection(id: &str) -> Result<()> {
    ensure_db()?;
    let mut db = CONNECTION_DB.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
    let conn = db.as_mut().expect("connection db initialized");
    let tx = conn.transaction()?;
    tx.execute(
        "DELETE FROM project_connection_memberships WHERE connection_id = ?1",
        params![id],
    )?;
    tx.execute("DELETE FROM saved_connections WHERE id = ?1", params![id])?;
    tx.commit()
}

pub fn delete_connections_batch(ids: &[String]) -> Result<()> {
    ensure_db()?;
    let mut db = CONNECTION_DB.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
    let conn = db.as_mut().expect("connection db initialized");
    delete_connections_batch_impl(conn, ids)
}

pub fn list_project_connection_ids(project_id: &str) -> Result<Vec<String>> {
    ensure_db()?;
    let db = CONNECTION_DB.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
    let conn = db.as_ref().expect("connection db initialized");
    let mut stmt = conn.prepare(
        "SELECT connection_id
         FROM project_connection_memberships
         WHERE project_id = ?1
         ORDER BY created_at ASC",
    )?;
    let rows = stmt.query_map(params![project_id], |row| row.get(0))?;
    rows.collect()
}

pub fn add_project_connection(project_id: &str, connection_id: &str) -> Result<()> {
    ensure_db()?;
    let db = CONNECTION_DB.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
    let conn = db.as_ref().expect("connection db initialized");
    conn.execute(
        "INSERT OR IGNORE INTO project_connection_memberships
         (project_id, connection_id, created_at)
         VALUES (?1, ?2, ?3)",
        params![project_id, connection_id, now_secs()],
    )?;
    Ok(())
}

pub fn remove_project_connection(project_id: &str, connection_id: &str) -> Result<()> {
    ensure_db()?;
    let db = CONNECTION_DB.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
    let conn = db.as_ref().expect("connection db initialized");
    conn.execute(
        "DELETE FROM project_connection_memberships WHERE project_id = ?1 AND connection_id = ?2",
        params![project_id, connection_id],
    )?;
    Ok(())
}

pub fn list_projects() -> Result<Vec<ProjectRecord>> {
    ensure_db()?;
    let db = CONNECTION_DB.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
    let conn = db.as_ref().expect("connection db initialized");
    let mut stmt = conn.prepare(
        "SELECT id, name, path, uses_default_location, created_at, updated_at
         FROM projects
         ORDER BY name COLLATE NOCASE ASC, created_at ASC",
    )?;
    let rows = stmt.query_map([], row_to_project_record)?;
    rows.collect()
}

pub fn upsert_project(record: &ProjectRecord) -> Result<()> {
    ensure_db()?;
    let db = CONNECTION_DB.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
    let conn = db.as_ref().expect("connection db initialized");
    conn.execute(
        "INSERT INTO projects (id, name, path, uses_default_location, created_at, updated_at)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6)
         ON CONFLICT(id) DO UPDATE SET
            name = excluded.name,
            path = excluded.path,
            uses_default_location = excluded.uses_default_location,
            updated_at = excluded.updated_at",
        params![
            record.id,
            record.name,
            record.path,
            record.uses_default_location,
            record.created_at,
            record.updated_at,
        ],
    )?;
    Ok(())
}

pub fn delete_project(id: &str) -> Result<()> {
    ensure_db()?;
    let mut db = CONNECTION_DB.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
    let conn = db.as_mut().expect("connection db initialized");
    let tx = conn.transaction()?;
    tx.execute(
        "DELETE FROM project_connection_memberships WHERE project_id = ?1",
        params![id],
    )?;
    tx.execute("DELETE FROM projects WHERE id = ?1", params![id])?;
    tx.commit()
}

pub fn rename_project(old_id: &str, record: &ProjectRecord) -> Result<()> {
    ensure_db()?;
    let mut db = CONNECTION_DB.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
    let conn = db.as_mut().expect("connection db initialized");
    let tx = conn.transaction()?;
    tx.execute(
        "INSERT INTO projects (id, name, path, uses_default_location, created_at, updated_at)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
        params![
            record.id,
            record.name,
            record.path,
            record.uses_default_location,
            record.created_at,
            record.updated_at
        ],
    )?;
    tx.execute(
        "UPDATE project_connection_memberships SET project_id = ?2 WHERE project_id = ?1",
        params![old_id, record.id],
    )?;
    tx.execute("DELETE FROM projects WHERE id = ?1", params![old_id])?;
    tx.commit()
}

pub fn touch_connection(id: &str, timestamp: f64) -> Result<()> {
    ensure_db()?;
    let db = CONNECTION_DB.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
    let conn = db.as_ref().expect("connection db initialized");
    conn.execute(
        "UPDATE saved_connections SET last_connected = ?2, updated_at = ?2 WHERE id = ?1",
        params![id, timestamp],
    )?;
    Ok(())
}

pub fn set_connection_pinned(id: &str, is_pinned: bool) -> Result<()> {
    ensure_db()?;
    let db = CONNECTION_DB.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
    let conn = db.as_ref().expect("connection db initialized");
    let now = now_secs();
    conn.execute(
        "UPDATE saved_connections SET is_pinned = ?2, updated_at = ?3 WHERE id = ?1",
        params![id, is_pinned, now],
    )?;
    Ok(())
}

pub(crate) fn list_connections_impl(conn: &Connection) -> Result<Vec<SavedConnectionRecord>> {
    let mut stmt = conn.prepare(
        "SELECT id, name, uri, db_type, color_hex, save_password, save_proxy_password,
                network_settings_json, is_pinned, last_connected, created_at, updated_at,
                connection_type, overrides_json
         FROM saved_connections
         ORDER BY is_pinned DESC, name COLLATE NOCASE ASC, created_at ASC",
    )?;
    let rows = stmt.query_map([], row_to_record)?;
    rows.collect()
}

fn get_connection_impl(conn: &Connection, id: &str) -> Result<Option<SavedConnectionRecord>> {
    conn.query_row(
        "SELECT id, name, uri, db_type, color_hex, save_password, save_proxy_password,
                network_settings_json, is_pinned, last_connected, created_at, updated_at,
                connection_type, overrides_json
         FROM saved_connections
         WHERE id = ?1",
        params![id],
        row_to_record,
    )
    .optional()
}

fn upsert_connection_impl(conn: &Connection, record: &SavedConnectionRecord) -> Result<()> {
    conn.execute(
        "INSERT INTO saved_connections
         (id, name, uri, db_type, color_hex, save_password, save_proxy_password,
          network_settings_json, is_pinned, last_connected, created_at, updated_at,
          connection_type, overrides_json)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14)
         ON CONFLICT(id) DO UPDATE SET
            name = excluded.name,
            uri = excluded.uri,
            db_type = excluded.db_type,
            color_hex = excluded.color_hex,
            save_password = excluded.save_password,
            save_proxy_password = excluded.save_proxy_password,
            network_settings_json = excluded.network_settings_json,
            is_pinned = excluded.is_pinned,
            last_connected = excluded.last_connected,
            updated_at = excluded.updated_at,
            connection_type = excluded.connection_type,
            overrides_json = excluded.overrides_json",
        params![
            record.id,
            record.name,
            record.uri,
            record.db_type,
            record.color_hex,
            record.save_password,
            record.save_proxy_password,
            record.network_settings_json,
            record.is_pinned,
            record.last_connected,
            record.created_at,
            record.updated_at,
            record.connection_type,
            record.overrides_json,
        ],
    )?;
    Ok(())
}

fn upsert_connections_batch_impl(
    conn: &mut Connection,
    records: &[SavedConnectionRecord],
) -> Result<()> {
    let transaction = conn.transaction()?;
    for record in records {
        upsert_connection_impl(&transaction, record)?;
    }
    transaction.commit()
}

fn delete_connections_batch_impl(conn: &mut Connection, ids: &[String]) -> Result<()> {
    if ids.is_empty() {
        return Err(rusqlite::Error::InvalidQuery);
    }
    let transaction = conn.transaction()?;
    for id in ids {
        transaction.execute(
            "DELETE FROM project_connection_memberships WHERE connection_id = ?1",
            params![id],
        )?;
        if transaction.execute("DELETE FROM saved_connections WHERE id = ?1", params![id])? != 1 {
            return Err(rusqlite::Error::QueryReturnedNoRows);
        }
    }
    transaction.commit()
}

fn row_to_record(row: &rusqlite::Row<'_>) -> Result<SavedConnectionRecord> {
    Ok(SavedConnectionRecord {
        id: row.get(0)?,
        name: row.get(1)?,
        uri: row.get(2)?,
        db_type: row.get(3)?,
        color_hex: row.get(4)?,
        save_password: row.get(5)?,
        save_proxy_password: row.get(6)?,
        network_settings_json: row.get(7)?,
        is_pinned: row.get(8)?,
        last_connected: row.get(9)?,
        created_at: row.get(10)?,
        updated_at: row.get(11)?,
        connection_type: row.get(12)?,
        overrides_json: row.get(13)?,
    })
}

fn row_to_project_record(row: &rusqlite::Row<'_>) -> Result<ProjectRecord> {
    Ok(ProjectRecord {
        id: row.get(0)?,
        name: row.get(1)?,
        path: row.get(2)?,
        uses_default_location: row.get(3)?,
        created_at: row.get(4)?,
        updated_at: row.get(5)?,
    })
}

fn now_secs() -> f64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|duration| duration.as_secs_f64())
        .unwrap_or(0.0)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::paths::TEST_ENV_LOCK as TEST_LOCK;

    fn reset_db_for_test() {
        let mut db = CONNECTION_DB.lock().expect("lock connection db");
        *db = None;
    }

    fn use_temp_home(name: &str) {
        let dir = std::env::temp_dir().join(format!(
            "tablex-connection-store-test-{}-{}",
            name,
            std::process::id()
        ));
        if dir.exists() {
            std::fs::remove_dir_all(&dir).expect("remove temp connection home");
        }
        std::fs::create_dir_all(&dir).expect("create temp connection home");
        std::env::set_var(crate::paths::DATA_DIR_ENV, dir);
        reset_db_for_test();
    }

    fn sample_connection(id: &str, name: &str) -> SavedConnectionRecord {
        SavedConnectionRecord {
            id: id.to_string(),
            name: name.to_string(),
            uri: "mongodb://localhost:27017".to_string(),
            db_type: "mongodb".to_string(),
            color_hex: "#34C759".to_string(),
            save_password: false,
            save_proxy_password: false,
            network_settings_json: r#"{"ssh":{"isEnabled":false},"proxy":{"isEnabled":false}}"#
                .to_string(),
            is_pinned: false,
            last_connected: None,
            created_at: 1_700_000_000.0,
            updated_at: 1_700_000_000.0,
            connection_type: String::new(),
            overrides_json: String::new(),
        }
    }

    fn sample_project(
        id: &str,
        name: &str,
        path: &str,
        uses_default_location: bool,
    ) -> ProjectRecord {
        ProjectRecord {
            id: id.to_string(),
            name: name.to_string(),
            path: path.to_string(),
            uses_default_location,
            created_at: 1_700_000_000.0,
            updated_at: 1_700_000_000.0,
        }
    }

    #[test]
    fn upsert_list_get_and_delete_connection_round_trips() {
        let _guard = TEST_LOCK.lock().expect("lock test");
        use_temp_home("roundtrip");

        let mut first = sample_connection("conn-1", "Beta");
        first.is_pinned = true;
        upsert_connection(&first).expect("upsert first");
        upsert_connection(&sample_connection("conn-2", "Alpha")).expect("upsert second");

        let listed = list_connections().expect("list connections");
        assert_eq!(
            listed.iter().map(|c| c.id.as_str()).collect::<Vec<_>>(),
            vec!["conn-1", "conn-2"]
        );
        assert!(listed[0].is_pinned);

        let loaded = get_connection("conn-1")
            .expect("get connection")
            .expect("connection exists");
        assert_eq!(loaded.name, "Beta");
        assert_eq!(loaded.last_connected, None);

        delete_connection("conn-1").expect("delete connection");
        assert!(get_connection("conn-1").expect("get deleted").is_none());
    }

    #[test]
    fn connection_type_and_overrides_round_trip() {
        let mut db = app_db::open_in_memory().unwrap();
        let mut record = sample_connection("typed", "Typed");
        record.connection_type = "production".into();
        record.overrides_json = r#"{"query.query_timeout_secs":30}"#.into();
        upsert_connection_impl(&db, &record).unwrap();
        assert_eq!(get_connection_impl(&db, "typed").unwrap().unwrap(), record);
        // Saving again with the type cleared overwrites it.
        record.connection_type.clear();
        record.overrides_json.clear();
        upsert_connection_impl(&db, &record).unwrap();
        assert_eq!(get_connection_impl(&db, "typed").unwrap().unwrap(), record);
        upsert_connections_batch_impl(&mut db, &[record]).unwrap();
    }

    #[test]
    fn a_record_json_without_the_new_fields_still_deserializes() {
        let json = r##"{"id":"a","name":"n","uri":"u","db_type":"sqlite","color_hex":"#fff",
            "save_password":false,"save_proxy_password":false,"network_settings_json":"{}",
            "is_pinned":false,"last_connected":null,"created_at":1.0,"updated_at":1.0}"##;
        let record: SavedConnectionRecord = serde_json::from_str(json).unwrap();
        assert!(record.connection_type.is_empty() && record.overrides_json.is_empty());
    }

    #[test]
    fn import_batch_rolls_back_every_record_if_later_insert_fails() {
        let mut db = app_db::open_in_memory().unwrap();
        db.execute_batch("CREATE TRIGGER fail_import BEFORE INSERT ON saved_connections WHEN NEW.id = 'bad' BEGIN SELECT RAISE(ABORT, 'rejected'); END;").unwrap();
        let records = [
            sample_connection("good", "Good"),
            sample_connection("bad", "Bad"),
        ];
        assert!(upsert_connections_batch_impl(&mut db, &records).is_err());
        assert!(list_connections_impl(&db).unwrap().is_empty());
        upsert_connections_batch_impl(&mut db, &records[..1]).unwrap();
        assert_eq!(list_connections_impl(&db).unwrap().len(), 1);
    }

    #[test]
    fn delete_batch_rejects_missing_and_rolls_back_on_later_failure() {
        let mut db = app_db::open_in_memory().unwrap();
        upsert_connection_impl(&db, &sample_connection("one", "One")).unwrap();
        upsert_connection_impl(&db, &sample_connection("two", "Two")).unwrap();
        db.execute_batch("CREATE TRIGGER fail_second BEFORE DELETE ON saved_connections WHEN OLD.id = 'two' BEGIN SELECT RAISE(ABORT, 'blocked'); END;").unwrap();
        assert!(delete_connections_batch_impl(&mut db, &["one".into(), "two".into()]).is_err());
        assert_eq!(list_connections_impl(&db).unwrap().len(), 2);
        assert!(delete_connections_batch_impl(&mut db, &["one".into(), "missing".into()]).is_err());
        assert_eq!(list_connections_impl(&db).unwrap().len(), 2);
        delete_connections_batch_impl(&mut db, &["one".into()]).unwrap();
        assert!(get_connection_impl(&db, "one").unwrap().is_none());
        assert!(get_connection_impl(&db, "two").unwrap().is_some());
    }

    #[test]
    fn touch_and_pin_update_existing_connection() {
        let _guard = TEST_LOCK.lock().expect("lock test");
        use_temp_home("touch-pin");

        upsert_connection(&sample_connection("conn-1", "Local")).expect("upsert");
        set_connection_pinned("conn-1", true).expect("pin");
        touch_connection("conn-1", 1_800_000_000.0).expect("touch");

        let loaded = get_connection("conn-1")
            .expect("get")
            .expect("connection exists");
        assert!(loaded.is_pinned);
        assert_eq!(loaded.last_connected, Some(1_800_000_000.0));
    }

    #[test]
    fn project_connection_memberships_add_list_and_remove() {
        let _guard = TEST_LOCK.lock().expect("lock test");
        use_temp_home("project-memberships");

        add_project_connection("Analytics", "conn-1").expect("add first");
        add_project_connection("Analytics", "conn-1").expect("add duplicate");
        add_project_connection("Analytics", "conn-2").expect("add second");

        assert_eq!(
            list_project_connection_ids("Analytics").expect("list memberships"),
            vec!["conn-1".to_string(), "conn-2".to_string()]
        );

        remove_project_connection("Analytics", "conn-1").expect("remove first");

        assert_eq!(
            list_project_connection_ids("Analytics").expect("list after remove"),
            vec!["conn-2".to_string()]
        );
    }

    #[test]
    fn deleting_connection_removes_project_memberships() {
        let _guard = TEST_LOCK.lock().expect("lock test");
        use_temp_home("membership-cleanup");

        upsert_connection(&sample_connection("conn-1", "Local")).expect("upsert");
        add_project_connection("Analytics", "conn-1").expect("add membership");

        delete_connection("conn-1").expect("delete connection");

        assert_eq!(
            list_project_connection_ids("Analytics").expect("list after delete"),
            Vec::<String>::new()
        );
    }

    #[test]
    fn projects_upsert_list_and_delete() {
        let _guard = TEST_LOCK.lock().expect("lock test");
        use_temp_home("projects-roundtrip");

        upsert_project(&sample_project(
            "Warehouse",
            "Warehouse",
            "/tmp/warehouse",
            false,
        ))
        .expect("upsert custom project");
        upsert_project(&sample_project(
            "Analytics",
            "Analytics",
            "/tmp/analytics",
            true,
        ))
        .expect("upsert default project");

        let listed = list_projects().expect("list projects");
        assert_eq!(
            listed.iter().map(|p| p.id.as_str()).collect::<Vec<_>>(),
            vec!["Analytics", "Warehouse"]
        );
        assert_eq!(listed[1].path, "/tmp/warehouse");
        assert!(!listed[1].uses_default_location);

        let renamed = sample_project("Warehouse", "Warehouse 2", "/tmp/warehouse", false);
        upsert_project(&renamed).expect("rename custom project");
        assert_eq!(
            list_projects().expect("list renamed")[1].name,
            "Warehouse 2"
        );

        delete_project("Warehouse").expect("delete project");
        assert_eq!(
            list_projects()
                .expect("list after delete")
                .iter()
                .map(|p| p.id.as_str())
                .collect::<Vec<_>>(),
            vec!["Analytics"]
        );
    }

    #[test]
    fn deleting_project_removes_memberships() {
        let _guard = TEST_LOCK.lock().expect("lock test");
        use_temp_home("project-membership-cleanup");

        upsert_project(&sample_project(
            "Analytics",
            "Analytics",
            "/tmp/analytics",
            true,
        ))
        .expect("upsert project");
        add_project_connection("Analytics", "conn-1").expect("add membership");

        delete_project("Analytics").expect("delete project");

        assert_eq!(
            list_project_connection_ids("Analytics").expect("list after project delete"),
            Vec::<String>::new()
        );
    }

    #[test]
    fn renaming_project_moves_memberships_atomically() {
        let _guard = TEST_LOCK.lock().expect("lock test");
        use_temp_home("project-rename");

        upsert_project(&sample_project(
            "Analytics",
            "Analytics",
            "/tmp/analytics",
            false,
        ))
        .expect("upsert project");
        add_project_connection("Analytics", "conn-1").expect("add membership");

        rename_project(
            "Analytics",
            &sample_project("Warehouse", "Warehouse", "/tmp/analytics", false),
        )
        .expect("rename project");

        assert_eq!(list_projects().expect("list renamed")[0].id, "Warehouse");
        assert!(list_project_connection_ids("Analytics")
            .expect("old memberships")
            .is_empty());
        assert_eq!(
            list_project_connection_ids("Warehouse").expect("new memberships"),
            vec!["conn-1".to_string()]
        );
    }

    #[test]
    fn a_failed_import_keeps_the_old_file_in_use_for_the_run_and_the_next_start_brings_everything_in() {
        let _guard = TEST_LOCK.lock().expect("lock test");
        use_temp_home("fallback");
        let old = legacy_path();
        // The old file, as the previous version left it.
        let conn = open_legacy_file(&old).unwrap();
        upsert_connection_impl(&conn, &sample_connection("conn-1", "Alpha")).unwrap();
        upsert_connection_impl(&conn, &sample_connection("conn-2", "Beta")).unwrap();
        drop(conn);
        // The database cannot take rows now (a full disk, a lock, a bug): the import fails.
        let db = app_db::default_path();
        app_db::open(&db)
            .unwrap()
            .execute_batch("CREATE TRIGGER no_rows BEFORE INSERT ON saved_connections BEGIN SELECT RAISE(ABORT, 'disk full'); END")
            .unwrap();
        reset_db_for_test();

        // The list is not empty: the run works on the old file, as the previous version did.
        let listed = list_connections().expect("the old file is used");
        assert_eq!(listed.iter().map(|c| c.id.as_str()).collect::<Vec<_>>(), ["conn-1", "conn-2"]);
        upsert_connection(&sample_connection("conn-3", "Gamma")).expect("written to the old file");
        let warned = crate::legacy::warnings_snapshot();
        assert!(
            warned.iter().any(|w| w.contains(&old.display().to_string()) && w.contains("disk full")),
            "one warning naming the file: {warned:?}"
        );
        let in_old: i64 = crate::legacy::open_old(&old)
            .ok()
            .unwrap()
            .query_row("SELECT COUNT(*) FROM saved_connections", [], |r| r.get(0))
            .unwrap();
        assert_eq!(in_old, 3, "the new connection is in the old file");
        let in_new: i64 = app_db::open(&db).unwrap().query_row("SELECT COUNT(*) FROM saved_connections", [], |r| r.get(0)).unwrap();
        assert_eq!(in_new, 0, "nothing half imported");

        // The next start, the fault gone: all three are imported, the one written meanwhile too.
        app_db::open(&db).unwrap().execute_batch("DROP TRIGGER no_rows").unwrap();
        reset_db_for_test();
        let listed = list_connections().unwrap();
        assert_eq!(listed.len(), 3);
        let in_new: i64 = app_db::open(&db).unwrap().query_row("SELECT COUNT(*) FROM saved_connections", [], |r| r.get(0)).unwrap();
        assert_eq!(in_new, 3);
    }
}
