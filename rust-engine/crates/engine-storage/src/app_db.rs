//! The shared app database, `<data dir>/tablex.db` (one `TABLEX_DATA_DIR` moves it with every other
//! file). The app and `tablex-mcp` open it at the same time.
//!
//! * `WAL` journal: readers never block the writer and never see a half-written transaction. Every
//!   connection sets a 5 s `busy_timeout`, so a writer that finds another one at work waits for it
//!   instead of failing. Writers take the write lock up front (`BEGIN IMMEDIATE`, see
//!   [`write_transaction`]): a read that turns into a write halfway through would fail at once
//!   instead of waiting. Durability stays at SQLite's default (`synchronous=FULL`: a committed
//!   write is fsynced).
//! * Schema: `PRAGMA user_version` is the number of [`MIGRATIONS`] applied. Migrations only move
//!   forward, each runs once inside one write transaction (a second process opening at the same
//!   time waits, then finds nothing to do). To add a table, append a migration.
//! * Forward compatibility: the `meta` row `min_reader_version` is the lowest [`SCHEMA_VERSION`]
//!   a build must have to use the database. An additive migration (a new table, a new column
//!   with a default) leaves it alone, so an older build opens a database that a newer one
//!   migrated and keeps using the tables it knows; a migration that older builds cannot work
//!   with raises it in its own step (`UPDATE meta SET value = '<n>' WHERE key =
//!   'min_reader_version'`). A database whose `min_reader_version` is above this build's
//!   version is an [`AppDbError::Newer`], never touched; so is one with a higher `user_version`
//!   and no `meta` table to say otherwise.
//! * The file is created with mode 0600 (SQLite gives `-wal` and `-shm` the mode of the database
//!   file). A file that is not a database is an error naming it, never replaced.
//!
//! Open a connection per use (about as cheap as taking a lock): it keeps writers independent of
//! each other and of any in-process mutex, which matters to callers that wait on the OS between
//! statements.
//!
//! # What lives here
//!
//! | table(s) | store |
//! |---|---|
//! | `secrets` | `engine_connect::secret_store` (encrypted saved passwords and keys) |
//! | `saved_connections`, `projects`, `project_connection_memberships` | [`crate::connection`] |
//! | `query_history` | [`crate::history`] |
//! | `saved_aggregates` | [`crate::aggregate`] |
//! | `connection_view_state` | [`crate::view_state`] |
//! | `documents` | JSON documents of the app: workspace session, update check, diagram layouts ([`crate::documents`]) |
//! | `assistant_conversations` | the assistant's kept conversations ([`crate::documents`]) |
//! | `imports` | one row per store whose old file was moved in ([`crate::legacy`]) |
//!
//! Data the app kept in separate files before this database is moved in once per store, on its
//! first use, and the old files are left as they were (see [`crate::legacy`]).
//!
//! Files that stay files: `settings.json` (hand-editable, read by `tablex-mcp`), the driver store
//! next to the installed binaries, logs, and the read-once imports of the Swift app.

use std::fmt;
use std::path::{Path, PathBuf};
use std::time::Duration;

use rusqlite::{Connection, ErrorCode, Transaction, TransactionBehavior};

pub const FILE_NAME: &str = "tablex.db";

const BUSY_TIMEOUT: Duration = Duration::from_secs(5);

/// Schema steps; step `n` (1-based) takes the database from version `n - 1` to `n`. Append only:
/// a shipped step is never edited.
const MIGRATIONS: &[&str] = &[
    // 1: the encrypted saved secrets (see `engine_connect::secret_store`). `nonce` is 12 bytes,
    // `ct` the AES-256-GCM ciphertext with its tag; one reserved name holds the key check.
    "CREATE TABLE secrets (
        name TEXT PRIMARY KEY,
        nonce BLOB NOT NULL,
        ct BLOB NOT NULL,
        updated_at INTEGER NOT NULL
    ) WITHOUT ROWID",
    // 2: the connection store (was `connections.db`) and the import markers. A marker row says the
    // old file of that store was moved in (or that there was none); see `crate::legacy`.
    "CREATE TABLE imports (
        name TEXT PRIMARY KEY,
        at INTEGER NOT NULL
    ) WITHOUT ROWID;
    CREATE TABLE saved_connections (
        id TEXT PRIMARY KEY,
        name TEXT NOT NULL,
        uri TEXT NOT NULL,
        db_type TEXT NOT NULL,
        color_hex TEXT NOT NULL,
        save_password INTEGER NOT NULL,
        save_proxy_password INTEGER NOT NULL,
        network_settings_json TEXT NOT NULL,
        is_pinned INTEGER NOT NULL DEFAULT 0,
        last_connected REAL,
        created_at REAL NOT NULL,
        updated_at REAL NOT NULL,
        connection_type TEXT NOT NULL DEFAULT '',
        overrides_json TEXT NOT NULL DEFAULT ''
    );
    CREATE INDEX idx_saved_connections_order
        ON saved_connections(is_pinned DESC, name COLLATE NOCASE ASC);
    CREATE TABLE project_connection_memberships (
        project_id TEXT NOT NULL,
        connection_id TEXT NOT NULL,
        created_at REAL NOT NULL,
        PRIMARY KEY(project_id, connection_id)
    );
    CREATE INDEX idx_project_connection_memberships_order
        ON project_connection_memberships(project_id, created_at ASC);
    CREATE TABLE projects (
        id TEXT PRIMARY KEY,
        name TEXT NOT NULL,
        path TEXT NOT NULL,
        uses_default_location INTEGER NOT NULL,
        created_at REAL NOT NULL,
        updated_at REAL NOT NULL
    );
    CREATE INDEX idx_projects_order ON projects(name COLLATE NOCASE ASC, created_at ASC)",
    // 3: query history (was `history.db`), saved aggregates (`aggregates.db`) and the per-connection
    // view state (`view_state.db`).
    "CREATE TABLE query_history (
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
    );
    CREATE INDEX idx_history_context ON query_history(conn_uuid, database, collection);
    CREATE INDEX idx_history_timestamp ON query_history(timestamp DESC);
    CREATE UNIQUE INDEX idx_history_dedup
        ON query_history(conn_uuid, database, collection, filter, project, sort, skip, \"limit\", max_time_ms);
    CREATE TABLE saved_aggregates (
        id TEXT PRIMARY KEY,
        conn_uuid TEXT NOT NULL,
        database TEXT NOT NULL,
        collection TEXT NOT NULL DEFAULT '',
        name TEXT NOT NULL,
        kind TEXT NOT NULL DEFAULT 'aggregate',
        pipeline_json TEXT NOT NULL,
        created_at REAL NOT NULL,
        updated_at REAL NOT NULL,
        options_json TEXT
    );
    CREATE INDEX idx_saved_agg_db_lookup ON saved_aggregates(conn_uuid, database);
    CREATE TABLE connection_view_state (
        conn_uuid TEXT PRIMARY KEY,
        state_json TEXT NOT NULL,
        updated_at REAL NOT NULL
    );
    CREATE INDEX idx_view_state_updated ON connection_view_state(updated_at DESC)",
    // 4: the app's JSON documents (kind: `session`, `update-check`, `diagram`; name: the key within
    // the kind) and the assistant's conversations. `header` is the small JSON a listing reads
    // without the body; `size` is the byte length of the body.
    "CREATE TABLE documents (
        kind TEXT NOT NULL,
        name TEXT NOT NULL,
        json TEXT NOT NULL,
        updated_at INTEGER NOT NULL,
        PRIMARY KEY (kind, name)
    ) WITHOUT ROWID;
    CREATE TABLE assistant_conversations (
        connection TEXT NOT NULL,
        id TEXT NOT NULL,
        updated INTEGER NOT NULL,
        seq INTEGER NOT NULL,
        header TEXT NOT NULL,
        size INTEGER NOT NULL,
        body TEXT NOT NULL,
        PRIMARY KEY (connection, id)
    ) WITHOUT ROWID;
    CREATE INDEX idx_assistant_conversations_updated
        ON assistant_conversations(connection, updated DESC)",
    // 5: what an older build needs to know before it opens a newer database (see the module docs).
    // Every step above and this one are additive: the lowest build that can use the database is
    // the first that knows this table, this one.
    "CREATE TABLE meta (
        key TEXT PRIMARY KEY,
        value TEXT NOT NULL
    ) WITHOUT ROWID;
    INSERT INTO meta (key, value) VALUES ('min_reader_version', '5')",
];

/// The `user_version` this build writes and understands.
pub const SCHEMA_VERSION: u32 = MIGRATIONS.len() as u32;

/// `<data dir>/tablex.db`: the one place the app database is looked for (the secret store, every
/// storage module and `tablex-mcp` use this function).
pub fn default_path() -> PathBuf {
    crate::paths::data_dir("tablex").join(FILE_NAME)
}

/// A database in memory with the current schema (tests, windows that keep nothing).
pub fn open_in_memory() -> rusqlite::Result<Connection> {
    let conn = Connection::open_in_memory()?;
    conn.pragma_update(None, "foreign_keys", true)?;
    for step in MIGRATIONS {
        conn.execute_batch(step)?;
    }
    Ok(conn)
}

#[derive(Debug)]
pub enum AppDbError {
    /// The data directory or the file could not be created.
    Io {
        path: PathBuf,
        source: std::io::Error,
    },
    /// The database comes from a newer TableX (`found` is above what this build knows).
    Newer { path: PathBuf, found: u32 },
    Sqlite {
        path: PathBuf,
        source: rusqlite::Error,
    },
    /// The old file of a store could not be moved into the database: nothing was changed, the
    /// old file is as it was and the move is tried again the next time the store is used.
    Import { file: PathBuf, message: String },
    /// An old file the user asked to delete (a saved conversation) could not be deleted.
    OldFile {
        file: PathBuf,
        source: std::io::Error,
    },
}

impl fmt::Display for AppDbError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Io { path, source } => {
                write!(f, "the app database {} cannot be created: {source}", path.display())
            }
            Self::Newer { path, found } => write!(
                f,
                "the app database {} is version {found}, from a newer TableX (this one knows up to \
                 {SCHEMA_VERSION}); update TableX",
                path.display()
            ),
            Self::OldFile { file, source } => write!(
                f,
                "the old saved file {} could not be deleted: {source}",
                file.display()
            ),
            Self::Import { file, message } => write!(
                f,
                "the saved data in {} could not be moved into the app database: {message}. The \
                 file was left as it is and is tried again the next time",
                file.display()
            ),
            Self::Sqlite { path, source } => match source.sqlite_error_code() {
                Some(ErrorCode::NotADatabase | ErrorCode::DatabaseCorrupt) => write!(
                    f,
                    "the app database {} is not valid and was left untouched; restore it from a \
                     backup. The data TableX had before this version is still in its old files, \
                     and TableX starts from them again if you quit it, rename tablex.db (keep it, \
                     it may be repairable) and start it again. What was saved with this version \
                     since (connections, history, passwords, tabs) is not in those files and is \
                     lost that way",
                    path.display()
                ),
                _ => write!(f, "the app database {} cannot be used: {source}", path.display()),
            },
        }
    }
}

impl std::error::Error for AppDbError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Io { source, .. } | Self::OldFile { source, .. } => Some(source),
            Self::Sqlite { source, .. } => Some(source),
            Self::Newer { .. } | Self::Import { .. } => None,
        }
    }
}

/// For the stores whose functions return plain `rusqlite` errors: the message is kept whole.
impl From<AppDbError> for rusqlite::Error {
    fn from(error: AppDbError) -> Self {
        Self::ToSqlConversionFailure(Box::new(error))
    }
}

/// Opens (creating, migrating) the database at `path`; see the module docs.
pub fn open(path: &Path) -> Result<Connection, AppDbError> {
    let sqlite = |source| AppDbError::Sqlite {
        path: path.to_owned(),
        source,
    };
    create_private(path).map_err(|source| AppDbError::Io {
        path: path.to_owned(),
        source,
    })?;
    let mut conn = Connection::open(path).map_err(sqlite)?;
    conn.busy_timeout(BUSY_TIMEOUT).map_err(sqlite)?;
    // The first statement that reads the file: garbage fails here, and a newer database is turned
    // away before any pragma could change it.
    let version = user_version(&conn).map_err(sqlite)?;
    if version > SCHEMA_VERSION && !readable_by_this_build(&conn).map_err(sqlite)? {
        return Err(AppDbError::Newer {
            path: path.to_owned(),
            found: version,
        });
    }
    enable_wal(&conn).map_err(sqlite)?;
    conn.pragma_update(None, "foreign_keys", true).map_err(sqlite)?;
    if version < SCHEMA_VERSION {
        migrate(&mut conn, path)?;
    }
    Ok(conn)
}

/// A transaction that holds the write lock from its first statement (waiting up to the busy
/// timeout for it). Dropping it rolls back.
pub fn write_transaction(conn: &mut Connection) -> rusqlite::Result<Transaction<'_>> {
    conn.transaction_with_behavior(TransactionBehavior::Immediate)
}

/// Creates the data directory and an empty file with mode 0600 when there is none (an existing
/// file is left as it is). An empty file is an empty SQLite database.
fn create_private(path: &Path) -> std::io::Result<()> {
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)?;
    }
    let mut options = std::fs::OpenOptions::new();
    options.create(true).truncate(false).write(true);
    #[cfg(unix)]
    std::os::unix::fs::OpenOptionsExt::mode(&mut options, 0o600);
    options.open(path).map(drop)
}

/// Whether a database with a higher `user_version` than this build knows can be used by it: its
/// `meta.min_reader_version` is at most [`SCHEMA_VERSION`]. A database without that row (or table)
/// says nothing, and is not used.
fn readable_by_this_build(conn: &Connection) -> rusqlite::Result<bool> {
    let minimum = conn.query_row(
        "SELECT value FROM meta WHERE key = 'min_reader_version'",
        [],
        |row| row.get::<_, String>(0),
    );
    match minimum {
        Ok(value) => Ok(value.parse::<u32>().is_ok_and(|minimum| minimum <= SCHEMA_VERSION)),
        Err(rusqlite::Error::QueryReturnedNoRows) => Ok(false),
        // No `meta` table: a version that came before the rule.
        Err(rusqlite::Error::SqliteFailure(_, Some(message))) if message.contains("no such table") => Ok(false),
        Err(error) => Err(error),
    }
}

fn user_version(conn: &Connection) -> rusqlite::Result<u32> {
    conn.pragma_query_value(None, "user_version", |row| row.get(0))
}

/// Switches to the WAL journal (it stays on in the file). Switching needs the file to itself for
/// a moment and SQLite does not always run the busy handler for it, so it is retried here.
fn enable_wal(conn: &Connection) -> rusqlite::Result<()> {
    let mode: String = conn.pragma_query_value(None, "journal_mode", |row| row.get(0))?;
    if mode.eq_ignore_ascii_case("wal") {
        return Ok(());
    }
    let mut attempts = 0;
    loop {
        match conn.pragma_update_and_check(None, "journal_mode", "WAL", |row| row.get::<_, String>(0)) {
            Ok(_) => return Ok(()),
            Err(e) if e.sqlite_error_code() == Some(ErrorCode::DatabaseBusy) && attempts < 250 => {
                attempts += 1;
                std::thread::sleep(Duration::from_millis(20));
            }
            Err(e) => return Err(e),
        }
    }
}

fn migrate(conn: &mut Connection, path: &Path) -> Result<(), AppDbError> {
    let sqlite = |source| AppDbError::Sqlite {
        path: path.to_owned(),
        source,
    };
    let tx = write_transaction(conn).map_err(sqlite)?;
    // Read again under the lock: another process may have migrated while this one waited.
    let version = user_version(&tx).map_err(sqlite)?;
    if version > SCHEMA_VERSION {
        // Another process (a newer build) migrated while this one waited.
        if readable_by_this_build(&tx).map_err(sqlite)? {
            return Ok(());
        }
        return Err(AppDbError::Newer {
            path: path.to_owned(),
            found: version,
        });
    }
    for step in &MIGRATIONS[version as usize..] {
        tx.execute_batch(step).map_err(sqlite)?;
    }
    tx.pragma_update(None, "user_version", SCHEMA_VERSION).map_err(sqlite)?;
    tx.commit().map_err(sqlite)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_db() -> (tempfile::TempDir, PathBuf) {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("nested").join(FILE_NAME);
        (dir, path)
    }

    #[test]
    fn opening_creates_a_private_wal_database_at_the_current_version() {
        let (_dir, path) = temp_db();
        let conn = open(&path).unwrap();
        assert_eq!(user_version(&conn).unwrap(), SCHEMA_VERSION);
        let mode: String = conn.pragma_query_value(None, "journal_mode", |r| r.get(0)).unwrap();
        assert_eq!(mode, "wal");
        let keys: i64 = conn.pragma_query_value(None, "foreign_keys", |r| r.get(0)).unwrap();
        assert_eq!(keys, 1);
        let timeout: i64 = conn.pragma_query_value(None, "busy_timeout", |r| r.get(0)).unwrap();
        assert_eq!(timeout, 5000);
        conn.execute("INSERT INTO secrets VALUES ('a', x'00', x'01', 1)", []).unwrap();
        #[cfg(unix)]
        for suffix in ["", "-wal", "-shm"] {
            use std::os::unix::fs::PermissionsExt;
            let file = PathBuf::from(format!("{}{suffix}", path.display()));
            let mode = std::fs::metadata(&file).unwrap().permissions().mode();
            assert_eq!(mode & 0o777, 0o600, "{}", file.display());
        }
    }

    #[test]
    fn reopening_keeps_the_data_and_runs_no_migration_twice() {
        let (_dir, path) = temp_db();
        open(&path).unwrap().execute("INSERT INTO secrets VALUES ('a', x'00', x'01', 1)", []).unwrap();
        let conn = open(&path).unwrap();
        let rows: i64 = conn.query_row("SELECT COUNT(*) FROM secrets", [], |r| r.get(0)).unwrap();
        assert_eq!(rows, 1);
        assert_eq!(user_version(&conn).unwrap(), SCHEMA_VERSION);
    }

    #[test]
    fn an_older_version_moves_forward_and_keeps_its_tables() {
        let (_dir, path) = temp_db();
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        // A database from before the first migration: other tables, version 0.
        Connection::open(&path).unwrap().execute_batch("CREATE TABLE other (x)").unwrap();
        let conn = open(&path).unwrap();
        assert_eq!(user_version(&conn).unwrap(), SCHEMA_VERSION);
        conn.execute("INSERT INTO other VALUES (1)", []).unwrap();
        conn.execute("INSERT INTO secrets VALUES ('a', x'00', x'01', 1)", []).unwrap();
    }

    #[test]
    fn a_database_of_the_first_version_moves_forward_and_keeps_its_secrets() {
        let (_dir, path) = temp_db();
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        // How the first TableX that had this database left it: the secrets table, version 1.
        let old = Connection::open(&path).unwrap();
        old.execute_batch(MIGRATIONS[0]).unwrap();
        old.execute("INSERT INTO secrets VALUES ('a', x'00', x'01', 7)", []).unwrap();
        old.pragma_update(None, "user_version", 1).unwrap();
        drop(old);
        let conn = open(&path).unwrap();
        assert_eq!(user_version(&conn).unwrap(), SCHEMA_VERSION);
        let kept: i64 = conn.query_row("SELECT updated_at FROM secrets WHERE name = 'a'", [], |r| r.get(0)).unwrap();
        assert_eq!(kept, 7);
        for table in ["imports", "saved_connections", "projects", "project_connection_memberships", "query_history",
                      "saved_aggregates", "connection_view_state", "documents", "assistant_conversations"] {
            conn.query_row(&format!("SELECT COUNT(*) FROM {table}"), [], |r| r.get::<_, i64>(0)).unwrap();
        }
    }

    #[test]
    fn the_in_memory_database_has_the_same_schema() {
        let conn = open_in_memory().unwrap();
        let tables: i64 = conn
            .query_row("SELECT COUNT(*) FROM sqlite_master WHERE type = 'table' AND name NOT LIKE 'sqlite_%'", [], |r| r.get(0))
            .unwrap();
        let (_dir, path) = temp_db();
        let on_disk: i64 = open(&path)
            .unwrap()
            .query_row("SELECT COUNT(*) FROM sqlite_master WHERE type = 'table' AND name NOT LIKE 'sqlite_%'", [], |r| r.get(0))
            .unwrap();
        assert_eq!(tables, on_disk);
    }

    #[test]
    fn a_newer_database_whose_changes_are_additive_is_used_and_one_that_is_not_is_refused() {
        let (_dir, path) = temp_db();
        let conn = open(&path).unwrap();
        conn.execute("INSERT INTO secrets VALUES ('a', x'00', x'01', 1)", []).unwrap();
        // A newer build migrated it (a table older builds do not know), and says it is additive.
        conn.execute_batch(&format!("CREATE TABLE later (x); PRAGMA user_version = {};", SCHEMA_VERSION + 3))
            .unwrap();
        drop(conn);
        let older = open(&path).expect("an older build keeps using what it knows");
        let rows: i64 = older.query_row("SELECT COUNT(*) FROM secrets", [], |r| r.get(0)).unwrap();
        assert_eq!(rows, 1);
        assert_eq!(user_version(&older).unwrap(), SCHEMA_VERSION + 3, "and does not migrate it back");
        older.execute("INSERT INTO secrets VALUES ('b', x'00', x'01', 1)", []).unwrap();
        // A newer build raises the minimum: this one is turned away, and leaves the file as it is.
        older
            .execute(
                "UPDATE meta SET value = ?1 WHERE key = 'min_reader_version'",
                [(SCHEMA_VERSION + 1).to_string()],
            )
            .unwrap();
        drop(older);
        let before = std::fs::read(&path).unwrap();
        let error = open(&path).unwrap_err();
        assert!(matches!(&error, AppDbError::Newer { found, .. } if *found == SCHEMA_VERSION + 3), "{error:?}");
        assert_eq!(std::fs::read(&path).unwrap(), before);
        // The other direction: this build opens a database an older one made, and migrates it.
        let (_dir, old) = temp_db();
        std::fs::create_dir_all(old.parent().unwrap()).unwrap();
        let first = Connection::open(&old).unwrap();
        first.execute_batch(MIGRATIONS[0]).unwrap();
        first.pragma_update(None, "user_version", 1).unwrap();
        drop(first);
        assert_eq!(user_version(&open(&old).unwrap()).unwrap(), SCHEMA_VERSION);
    }

    #[test]
    fn a_newer_database_is_a_typed_error_and_is_not_touched() {
        let (_dir, path) = temp_db();
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        Connection::open(&path)
            .unwrap()
            .execute_batch(&format!("CREATE TABLE future (x); PRAGMA user_version = {};", SCHEMA_VERSION + 1))
            .unwrap();
        let before = std::fs::read(&path).unwrap();
        let error = open(&path).unwrap_err();
        assert!(
            matches!(&error, AppDbError::Newer { found, .. } if *found == SCHEMA_VERSION + 1),
            "{error:?}"
        );
        assert!(error.to_string().contains(&path.display().to_string()));
        assert_eq!(std::fs::read(&path).unwrap(), before);
        assert!(!PathBuf::from(format!("{}-wal", path.display())).exists());
    }

    #[test]
    fn a_file_that_is_not_a_database_is_an_error_naming_it_and_is_not_replaced() {
        let (_dir, path) = temp_db();
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        for garbage in [b"{ not json".to_vec(), vec![0xA5; 4096]] {
            std::fs::write(&path, &garbage).unwrap();
            let error = open(&path).unwrap_err();
            assert!(matches!(error, AppDbError::Sqlite { .. }), "{error:?}");
            let message = error.to_string();
            assert!(message.contains(&path.display().to_string()), "{message}");
            assert!(message.contains("left untouched"), "{message}");
            assert_eq!(std::fs::read(&path).unwrap(), garbage);
        }
    }

    #[test]
    fn a_write_succeeds_while_another_connection_holds_a_read_transaction() {
        let (_dir, path) = temp_db();
        let mut writer = open(&path).unwrap();
        let reader = open(&path).unwrap();
        reader.execute_batch("BEGIN").unwrap();
        let before: i64 = reader.query_row("SELECT COUNT(*) FROM secrets", [], |r| r.get(0)).unwrap();
        assert_eq!(before, 0);
        let tx = write_transaction(&mut writer).unwrap();
        tx.execute("INSERT INTO secrets VALUES ('a', x'00', x'01', 1)", []).unwrap();
        tx.commit().unwrap();
        // The reader keeps its snapshot until it ends the transaction.
        let during: i64 = reader.query_row("SELECT COUNT(*) FROM secrets", [], |r| r.get(0)).unwrap();
        assert_eq!(during, 0);
        reader.execute_batch("COMMIT").unwrap();
        let after: i64 = reader.query_row("SELECT COUNT(*) FROM secrets", [], |r| r.get(0)).unwrap();
        assert_eq!(after, 1);
    }

    #[test]
    fn a_second_writer_waits_for_the_first_instead_of_failing() {
        let (_dir, path) = temp_db();
        let mut first = open(&path).unwrap();
        let tx = write_transaction(&mut first).unwrap();
        tx.execute("INSERT INTO secrets VALUES ('a', x'00', x'01', 1)", []).unwrap();
        let waiter = {
            let path = path.clone();
            std::thread::spawn(move || {
                let mut second = open(&path).unwrap();
                let tx = write_transaction(&mut second).unwrap();
                tx.execute("INSERT INTO secrets VALUES ('b', x'00', x'01', 1)", []).unwrap();
                tx.commit().unwrap();
            })
        };
        std::thread::sleep(Duration::from_millis(300));
        tx.commit().unwrap();
        waiter.join().unwrap();
        let rows: i64 = first.query_row("SELECT COUNT(*) FROM secrets", [], |r| r.get(0)).unwrap();
        assert_eq!(rows, 2);
    }

    const CHILD_ENV: &str = "TABLEX_APP_DB_TEST_CHILD";

    /// Runs only as a child of `processes_opening_a_fresh_database_together_lose_nothing`.
    #[test]
    fn cross_process_child() {
        let Some(path) = std::env::var_os(CHILD_ENV) else {
            return;
        };
        let path = PathBuf::from(path);
        let me = std::process::id();
        let mut conn = open(&path).unwrap();
        for i in 0..20 {
            let tx = write_transaction(&mut conn).unwrap();
            tx.execute("INSERT INTO secrets VALUES (?1, x'00', x'01', 1)", [format!("{me}-{i}")]).unwrap();
            tx.commit().unwrap();
        }
    }

    #[test]
    fn processes_opening_a_fresh_database_together_lose_nothing() {
        let (_dir, path) = temp_db();
        let exe = std::env::current_exe().unwrap();
        let children: Vec<_> = (0..4)
            .map(|_| {
                std::process::Command::new(&exe)
                    .args(["--exact", "app_db::tests::cross_process_child", "--test-threads=1"])
                    .env(CHILD_ENV, &path)
                    .stdout(std::process::Stdio::null())
                    .stderr(std::process::Stdio::null())
                    .spawn()
                    .unwrap()
            })
            .collect();
        // The parent opens and writes at the same time too.
        let mut conn = open(&path).unwrap();
        for i in 0..20 {
            let tx = write_transaction(&mut conn).unwrap();
            tx.execute("INSERT INTO secrets VALUES (?1, x'00', x'01', 1)", [format!("parent-{i}")]).unwrap();
            tx.commit().unwrap();
        }
        for mut child in children {
            assert!(child.wait().unwrap().success(), "a child failed");
        }
        let rows: i64 = conn.query_row("SELECT COUNT(*) FROM secrets", [], |r| r.get(0)).unwrap();
        assert_eq!(rows, 100);
        assert_eq!(user_version(&conn).unwrap(), SCHEMA_VERSION);
    }
}
