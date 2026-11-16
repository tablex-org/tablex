//! Moving the data the app kept in separate files before `tablex.db` into it, once per store.
//!
//! # Upgrade and downgrade, for the user
//!
//! A user who installs the new version keeps everything and does nothing: the first use of each
//! store moves its old file in (connections first, before the sidebar reads them), and the old
//! files stay where they are, unchanged. Only the app moves data in; `tablex-mcp` reads the old
//! files until the app has ([`set_import_allowed`]). Going back to an older TableX works: it finds
//! its files as they were at the move; whatever was saved with the new version is not in them
//! (and what the older version saves is not brought into the database again, see below).
//! Keychain items of the older version are copied, never deleted, for the same reason.
//!
//! # How
//!
//! Each store has a name (`connections`, `history`, `aggregates`, `view_state`, `session`,
//! `update-check`, `diagrams`, `assistant`) and a row in the `imports` table once its old file
//! (or folder) was dealt with. [`import_once`] runs the import on the first use of the store:
//!
//! * Inside one `BEGIN IMMEDIATE` transaction: the marker is checked again under the lock (two
//!   app instances may start together: one waits, finds the marker and does nothing), the old
//!   data is copied, and the marker is written. All or nothing.
//! * A failure (an old file that is damaged or cannot be read) rolls back, so the database has no
//!   marker and no row of that store, and the error names the file ([`AppDbError::Import`]). The
//!   next use tries again; the old file stays the only copy until it works.
//! * No old file means nothing to copy; the marker is still written, so a fresh install does not
//!   look at the disk again.
//! * The old files are **never** modified, renamed or deleted by an import: nothing is lost if it
//!   turns out to have a bug. Old SQLite files are opened read-only (and wait up to 5 s for a
//!   writer).
//! * A row the database cannot take (a key that is there twice in an old file that had no key
//!   constraint, say) fails the import like any other error: nothing is dropped silently. The
//!   one exception is the history, whose old code deduplicated by itself (see `history`).
//! * A store with data of its own (connections, history, aggregates, view state) that fails to
//!   move its old file in does not come up empty: it works on the old file, exactly as the
//!   previous version did ([`Store`], [`fell_back`]), says so once ([`take_warnings`]) and tries
//!   again at the next start. Before each use it looks whether another process has imported
//!   meanwhile; if so it merges what it wrote to the old file into the database (a row wins over
//!   the database's when it is at least as recently updated; history rows are only added) and
//!   goes on there. The one-file-per-document stores (session, update check, diagrams,
//!   conversations) skip what cannot be read, say so and try again at the next start
//!   ([`import_tolerant`]).
//! * An old file written by an older TableX *after* it was moved in (a downgrade and a new
//!   upgrade, or both versions running) is not imported again: the database is the truth since the
//!   marker. It is said once per run ([`take_warnings`]) and stays in its file.
//! * What the user deletes goes from the database, not from the old files, except for the
//!   assistant's conversations, which can hold query results: deleting one (or all) deletes its
//!   old file too.
//!
//! Deleting `tablex.db` makes the next start import again from whatever old files remain.

use std::collections::HashSet;
use std::ops::{Deref, DerefMut};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Mutex;

use rusqlite::types::Value;
use rusqlite::{params, Connection, OpenFlags, OptionalExtension, Transaction};

use crate::app_db::{self, AppDbError};

/// What went wrong with one old file.
pub(crate) struct ImportError {
    pub file: PathBuf,
    pub message: String,
}

impl ImportError {
    pub fn new(file: &Path, message: impl std::fmt::Display) -> Self {
        Self {
            file: file.to_owned(),
            message: message.to_string(),
        }
    }
}

/// Runs `import` (given the transaction that holds the write lock) when the database has no
/// marker for `name`; `legacy` is the old file or folder, looked at only when there is no marker.
/// A failure is an error and changes nothing (the stores with data of their own then work on the
/// old file for the run, see [`warn`]).
pub(crate) fn import_once(
    conn: &mut Connection,
    db: &Path,
    name: &str,
    legacy: &Path,
    import: impl FnOnce(&Transaction<'_>) -> Result<(), ImportError>,
) -> Result<(), AppDbError> {
    import_inner(conn, db, name, legacy, false, |tx| import(tx).map(|()| Vec::new()))
}

/// Like [`import_once`] for documents that are independent of each other (one file each): what
/// can be read is moved in, and what cannot (the returned `skipped`, or an error that stops the
/// whole import) is warned about ([`warn`]) and left in its file. The marker is then **not**
/// written, so the next start tries again (what is moved in already is not overwritten, but one
/// the user deleted meanwhile comes back); this process does not try twice. It never fails.
pub(crate) fn import_tolerant(
    conn: &mut Connection,
    db: &Path,
    name: &str,
    legacy: &Path,
    import: impl FnOnce(&Transaction<'_>) -> Result<Vec<ImportError>, ImportError>,
) -> Result<(), AppDbError> {
    import_inner(conn, db, name, legacy, true, import)
}

fn import_inner(
    conn: &mut Connection,
    db: &Path,
    name: &str,
    legacy: &Path,
    tolerant: bool,
    import: impl FnOnce(&Transaction<'_>) -> Result<Vec<ImportError>, ImportError>,
) -> Result<(), AppDbError> {
    let sqlite = |source| AppDbError::Sqlite {
        path: db.to_owned(),
        source,
    };
    // Not this process's to move: it reads what is there.
    if !import_allowed() {
        return Ok(());
    }
    if let Some(at) = imported(conn, name).map_err(sqlite)? {
        if once(&CHECKED, db, name) {
            note_later_changes(name, legacy, at);
        }
        return Ok(());
    }
    // A tolerant import that left something behind already ran in this process.
    if tolerant && seen(&PARTIAL, db, name) {
        return Ok(());
    }
    let tx = app_db::write_transaction(conn).map_err(sqlite)?;
    // Under the lock: another process may have imported while this one waited.
    if imported(&tx, name).map_err(sqlite)?.is_some() {
        return Ok(());
    }
    let exists = legacy.try_exists().map_err(|error| AppDbError::Import {
        file: legacy.to_owned(),
        message: error.to_string(),
    });
    let outcome = match exists {
        Ok(true) => import(&tx).map_err(|error| AppDbError::Import {
            file: error.file,
            message: error.message,
        }),
        Ok(false) => Ok(Vec::new()),
        Err(error) => Err(error),
    };
    let skipped = match outcome {
        Ok(skipped) => skipped,
        Err(error) if tolerant => {
            // Nothing of it is kept (the transaction ends here), and nothing of it is tried again
            // in this process.
            drop(tx);
            warn(error.to_string());
            once(&PARTIAL, db, name);
            return Ok(());
        }
        Err(error) => return Err(error),
    };
    if !skipped.is_empty() {
        for error in skipped {
            warn(
                AppDbError::Import {
                    file: error.file,
                    message: error.message,
                }
                .to_string(),
            );
        }
        once(&PARTIAL, db, name);
        return tx.commit().map_err(sqlite);
    }
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |elapsed| elapsed.as_secs() as i64);
    tx.execute("INSERT INTO imports (name, at) VALUES (?1, ?2)", params![name, now])
        .map_err(sqlite)?;
    tx.commit().map_err(sqlite)
}

// ---- who may move data ------------------------------------------------------------------------

static IMPORT_ALLOWED: AtomicBool = AtomicBool::new(true);

/// Whether this process may move old files into the database. The app does (the default);
/// `tablex-mcp` calls `set_import_allowed(false)` at start: until the app has moved a store in,
/// its stores read the old file, never import and never write the marker, so an older TableX
/// that still runs next to it keeps one truth, and the first thing the new app does on an
/// upgrade is the import.
pub fn set_import_allowed(allowed: bool) {
    IMPORT_ALLOWED.store(allowed, Ordering::SeqCst);
}

/// Whether this process may move old files in (see [`set_import_allowed`]).
pub fn import_allowed() -> bool {
    IMPORT_ALLOWED.load(Ordering::SeqCst)
}

/// Whether the database at `db` has the marker of the store `name`, looked at without creating or
/// changing anything: `false` when the file is not there or cannot be read.
pub fn is_imported(db: &Path, name: &str) -> bool {
    Connection::open_with_flags(db, OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_NO_MUTEX)
        .and_then(|conn| {
            conn.busy_timeout(std::time::Duration::from_secs(5))?;
            imported(&conn, name)
        })
        .is_ok_and(|at| at.is_some())
}

/// Whether the store `name` was moved in (the marker is there).
pub(crate) fn marked(conn: &Connection, name: &str) -> rusqlite::Result<bool> {
    imported(conn, name).map(|at| at.is_some())
}

// ---- what the app is told ---------------------------------------------------------------------

static WARNINGS: Mutex<Vec<String>> = Mutex::new(Vec::new());
static PENDING: AtomicBool = AtomicBool::new(false);
static CHECKED: Mutex<Option<HashSet<(PathBuf, String)>>> = Mutex::new(None);
static PARTIAL: Mutex<Option<HashSet<(PathBuf, String)>>> = Mutex::new(None);

/// Whether `(db, name)` is new to `set` (and puts it there).
fn once(set: &Mutex<Option<HashSet<(PathBuf, String)>>>, db: &Path, name: &str) -> bool {
    set.lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
        .get_or_insert_with(HashSet::new)
        .insert((db.to_owned(), name.to_owned()))
}

fn seen(set: &Mutex<Option<HashSet<(PathBuf, String)>>>, db: &Path, name: &str) -> bool {
    set.lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
        .as_ref()
        .is_some_and(|set| set.contains(&(db.to_owned(), name.to_owned())))
}

/// Says something about the moving-in of old data that the user may want to know and that must not
/// stop anything: it goes to stderr (the log of `tablex-mcp`) and is kept for the app to show
/// ([`take_warnings`]); the same text is kept once.
pub fn warn(message: String) {
    eprintln!("[tablex] {message}");
    let mut warnings = WARNINGS.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
    if !warnings.contains(&message) {
        warnings.push(message);
        PENDING.store(true, Ordering::Release);
    }
}

/// Whether [`take_warnings`] has something (one atomic read: cheap enough for every frame).
pub fn warnings_pending() -> bool {
    PENDING.load(Ordering::Acquire)
}

/// The warnings since the last call, each once per process: an old file that could not be moved
/// in (the store then works on that file for this run and tries again at the next start), and an
/// old file that was changed after it was moved in.
pub fn take_warnings() -> Vec<String> {
    let mut warnings = WARNINGS.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
    PENDING.store(false, Ordering::Release);
    std::mem::take(&mut *warnings)
}

/// The warnings so far, without taking them (tests: other tests may take them at any time).
#[cfg(test)]
pub(crate) fn warnings_snapshot() -> Vec<String> {
    WARNINGS.lock().unwrap_or_else(|poisoned| poisoned.into_inner()).clone()
}

/// Forgets what this process has tried and checked, as a new start would (tests).
#[cfg(test)]
pub(crate) fn forget_process_state() {
    *CHECKED.lock().unwrap_or_else(|poisoned| poisoned.into_inner()) = None;
    *PARTIAL.lock().unwrap_or_else(|poisoned| poisoned.into_inner()) = None;
}

/// A store that could not move its old file in and works on that file instead.
pub(crate) fn fell_back(error: &AppDbError, legacy: &Path) {
    warn(format!("{error}; TableX uses the old file {} for this run", legacy.display()));
}

/// What a downgrade and a new upgrade leaves: the old file was written after its contents were
/// moved in (an older TableX ran in between, or still runs beside this one). Nothing is imported
/// again (the database is the truth since the marker; merging two histories of the same rows
/// cannot be done safely), nothing of the old file is touched, and this is said once per process.
fn note_later_changes(name: &str, legacy: &Path, imported_at: i64) {
    let Some(changed) = newest_mtime(legacy, 2) else {
        return;
    };
    let moved_in = std::time::UNIX_EPOCH + std::time::Duration::from_secs(imported_at.max(0) as u64);
    if changed > moved_in + std::time::Duration::from_secs(2) {
        warn(format!(
            "{} ({name}) was changed after its contents were moved into the app database, by an \
             older TableX. Those changes are not imported; they are still in that file",
            legacy.display()
        ));
    }
}

/// The latest modification time of a file, or of a folder and what is in it (`depth` levels down).
fn newest_mtime(path: &Path, depth: u8) -> Option<std::time::SystemTime> {
    let meta = std::fs::metadata(path).ok()?;
    let mut newest = meta.modified().ok();
    if meta.is_dir() && depth > 0 {
        for entry in std::fs::read_dir(path).ok()?.flatten() {
            newest = newest.max(newest_mtime(&entry.path(), depth - 1));
        }
    }
    newest
}

/// When the store was moved in (`None`: not yet).
fn imported(conn: &Connection, name: &str) -> rusqlite::Result<Option<i64>> {
    conn.query_row("SELECT at FROM imports WHERE name = ?1", [name], |row| row.get(0))
        .optional()
}

/// Opens an old SQLite file read-only (it is never written, not even a journal).
pub(crate) fn open_old(path: &Path) -> Result<Connection, ImportError> {
    let conn = Connection::open_with_flags(path, OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_NO_MUTEX)
        .map_err(|error| ImportError::new(path, error))?;
    // The previous version (or this one, working on the file) may be writing it just now.
    conn.busy_timeout(std::time::Duration::from_secs(5))
        .map_err(|error| ImportError::new(path, error))?;
    Ok(conn)
}

/// What to do with a row whose key the database has already.
#[derive(Clone, Copy)]
pub(crate) enum Copying<'a> {
    /// It is an error (the first import: nothing is there yet).
    Fresh,
    /// The row that is there stays.
    Skip,
    /// The row with the later `updated` (this one when equal) wins; `key` are the columns of the
    /// primary key.
    Newer { key: &'a [&'a str], updated: &'a str },
}

/// Copies the rows of `table` of the old database `old` (at `path`, for the error text) into the
/// same table of `tx`. `columns` are `(name, value to use when the old table has no such column)`:
/// an old file from before a column existed gets the default the current code gave such rows.
/// `order_by` is an SQL ordering of the old rows; `how` says what a row does that meets one with
/// its key (for [`Copying::Skip`] this also holds between two old rows: the first is kept). A
/// table the old file does not have is nothing to copy.
pub(crate) fn copy_table(
    old: &Connection,
    path: &Path,
    tx: &Transaction<'_>,
    table: &str,
    columns: &[(&str, &str)],
    order_by: &str,
    how: Copying<'_>,
    since: Option<&Since>,
) -> Result<(), ImportError> {
    let fail = |error: rusqlite::Error| ImportError::new(path, error);
    let present: Vec<String> = {
        let mut stmt = old
            .prepare(&format!("SELECT name FROM pragma_table_info('{table}')"))
            .map_err(fail)?;
        let rows = stmt.query_map([], |row| row.get::<_, String>(0)).map_err(fail)?;
        rows.collect::<Result<_, _>>().map_err(fail)?
    };
    if present.is_empty() {
        return Ok(());
    }
    let select = columns
        .iter()
        .map(|(name, default)| {
            if present.iter().any(|column| column == name) {
                format!("\"{name}\"")
            } else {
                (*default).to_owned()
            }
        })
        .collect::<Vec<_>>()
        .join(", ");
    let names = columns
        .iter()
        .map(|(name, _)| format!("\"{name}\""))
        .collect::<Vec<_>>()
        .join(", ");
    let marks = (1..=columns.len())
        .map(|n| format!("?{n}"))
        .collect::<Vec<_>>()
        .join(", ");
    // A merge brings in what was written after the run began on the old file: rows past the mark,
    // and rows of the table that has an `updated` column edited since.
    let filter = match since {
        None => String::new(),
        Some(since) => {
            let mark = since.marks.get(table).copied().unwrap_or(0);
            match how {
                Copying::Newer { updated, .. } if present.iter().any(|column| column == updated) => {
                    format!(" WHERE rowid > {mark} OR \"{updated}\" > {}", since.started)
                }
                _ => format!(" WHERE rowid > {mark}"),
            }
        }
    };
    let mut read = old
        .prepare(&format!("SELECT {select} FROM \"{table}\"{filter} ORDER BY {order_by}"))
        .map_err(fail)?;
    let statement = match how {
        Copying::Fresh => format!("INSERT INTO \"{table}\" ({names}) VALUES ({marks})"),
        Copying::Skip => format!("INSERT OR IGNORE INTO \"{table}\" ({names}) VALUES ({marks})"),
        Copying::Newer { key, updated } => {
            let set = columns
                .iter()
                .filter(|(name, _)| !key.contains(name))
                .map(|(name, _)| format!("\"{name}\" = excluded.\"{name}\""))
                .collect::<Vec<_>>()
                .join(", ");
            if set.is_empty() {
                format!("INSERT OR IGNORE INTO \"{table}\" ({names}) VALUES ({marks})")
            } else {
                let key = key.iter().map(|name| format!("\"{name}\"")).collect::<Vec<_>>().join(", ");
                format!(
                    "INSERT INTO \"{table}\" ({names}) VALUES ({marks}) ON CONFLICT({key}) DO UPDATE SET {set} \
                     WHERE excluded.\"{updated}\" >= \"{table}\".\"{updated}\""
                )
            }
        }
    };
    let mut write = tx.prepare(&statement).map_err(|error| ImportError::new(path, error))?;
    let mut rows = read.query([]).map_err(fail)?;
    while let Some(row) = rows.next().map_err(fail)? {
        let values: Vec<Value> = (0..columns.len())
            .map(|ix| row.get(ix))
            .collect::<Result<_, _>>()
            .map_err(fail)?;
        write
            .execute(rusqlite::params_from_iter(values))
            .map_err(|error| ImportError::new(path, error))?;
    }
    Ok(())
}

// ---- a store that may be on its old file ------------------------------------------------------

/// How a store brings its old file's rows into a transaction: `merge` is false for the import (a
/// row the database has is an error) and true for the merge of a run that worked on the old file
/// (the later row wins).
pub(crate) type ImportFn = fn(&Transaction<'_>, &Path, Option<&Since>) -> Result<(), ImportError>;

/// Where an old file stood when a store began to work on it (see [`Store`]): a merge into the
/// database brings in only what was written after, so what another process deleted from the
/// database meanwhile does not come back.
pub(crate) struct Since {
    /// The highest `rowid` of each table then.
    marks: std::collections::HashMap<String, i64>,
    /// The time (Unix seconds): a row updated after it was edited during the run.
    started: f64,
}

/// The connection of a store with data of its own. Normally the app database; for one run the old
/// file instead when the import failed (the previous version's behaviour) or when this process may
/// not import and the app has not yet ([`set_import_allowed`]). It dereferences to the connection.
pub(crate) struct Store {
    conn: Connection,
    old: Option<OnOld>,
}

impl std::fmt::Debug for Store {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Store").field("on_old_file", &self.old.is_some()).finish_non_exhaustive()
    }
}

struct OnOld {
    db: PathBuf,
    name: &'static str,
    legacy: PathBuf,
    /// Only reading it because this process may not import: nothing of it is merged.
    reader: bool,
    import: ImportFn,
    /// What the old file held when the run began on it (none for a reader).
    since: Since,
}

impl Store {
    pub(crate) fn new(conn: Connection) -> Self {
        Self { conn, old: None }
    }

    /// Whether this store is on its old file.
    #[cfg(test)]
    pub(crate) fn on_old_file(&self) -> bool {
        self.old.is_some()
    }

    /// Called before each use. A store on its old file looks whether the database has the marker
    /// now (another process imported); if so, it merges what it wrote to the old file into the
    /// database in one transaction and goes on there. A failure keeps it on the old file for
    /// now (it tries again at the next use).
    pub(crate) fn refresh(&mut self) {
        let Some(old) = &self.old else {
            return;
        };
        match old.switch() {
            Ok(Some(conn)) => {
                self.conn = conn;
                self.old = None;
            }
            Ok(None) => {}
            Err(error) => warn(format!(
                "{error}; TableX goes on with the old file {} for now",
                old.legacy.display()
            )),
        }
    }
}

impl OnOld {
    fn switch(&self) -> Result<Option<Connection>, AppDbError> {
        let sqlite = |source| AppDbError::Sqlite {
            path: self.db.clone(),
            source,
        };
        let mut conn = app_db::open(&self.db)?;
        if imported(&conn, self.name).map_err(sqlite)?.is_none() {
            return Ok(None);
        }
        if !self.reader {
            let tx = app_db::write_transaction(&mut conn).map_err(sqlite)?;
            (self.import)(&tx, &self.legacy, Some(&self.since)).map_err(|error| AppDbError::Import {
                file: error.file,
                message: error.message,
            })?;
            // The old file was written by this run until now, and is merged: what an older
            // TableX writes to it from here on is what a later start should say.
            tx.execute("UPDATE imports SET at = ?2 WHERE name = ?1", params![self.name, now_secs() as i64])
                .map_err(sqlite)?;
            tx.commit().map_err(sqlite)?;
        }
        Ok(Some(conn))
    }
}

impl Deref for Store {
    type Target = Connection;
    fn deref(&self) -> &Connection {
        &self.conn
    }
}

impl DerefMut for Store {
    fn deref_mut(&mut self) -> &mut Connection {
        &mut self.conn
    }
}

/// The store `name` for this run: the app database with the old file moved in; when that fails
/// and the old file exists, the old file ([`fell_back`]); in a process that may not import
/// ([`set_import_allowed`]), the old file until the database has the marker. `open_file` opens the
/// old file the way the previous version did.
pub(crate) fn open_run(
    db: &Path,
    name: &'static str,
    legacy: &Path,
    tables: &[&str],
    import: ImportFn,
    open_file: fn(&Path) -> rusqlite::Result<Connection>,
) -> rusqlite::Result<Store> {
    let on_old = |reader, conn: &Connection| OnOld {
        db: db.to_owned(),
        name,
        legacy: legacy.to_owned(),
        reader,
        import,
        since: Since {
            marks: tables
                .iter()
                .map(|table| {
                    let mark = conn
                        .query_row(&format!("SELECT COALESCE(MAX(rowid), 0) FROM \"{table}\""), [], |row| row.get(0))
                        .unwrap_or(0);
                    ((*table).to_owned(), mark)
                })
                .collect(),
            started: now_secs(),
        },
    };
    if !import_allowed() {
        let conn = app_db::open(db)?;
        if marked(&conn, name)? || !legacy.exists() {
            return Ok(Store::new(conn));
        }
        let conn = open_file(legacy)?;
        let old = on_old(true, &conn);
        return Ok(Store { conn, old: Some(old) });
    }
    let attempt = app_db::open(db).and_then(|mut conn| {
        import_once(&mut conn, db, name, legacy, |tx| import(tx, legacy, None))?;
        Ok(conn)
    });
    match attempt {
        Ok(conn) => Ok(Store::new(conn)),
        Err(error @ AppDbError::Import { .. }) if legacy.exists() => match open_file(legacy) {
            Ok(conn) => {
                fell_back(&error, legacy);
                let old = on_old(false, &conn);
                Ok(Store { conn, old: Some(old) })
            }
            Err(_) => Err(error.into()),
        },
        Err(error) => Err(error.into()),
    }
}

fn now_secs() -> f64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0.0, |elapsed| elapsed.as_secs_f64())
}

/// The file `<dir of the database>/<name>` the old app wrote beside `tablex.db`: only when `db`
/// is the app database itself (a database a test or a scenario puts elsewhere has no old files).
pub(crate) fn beside(db: &Path) -> Option<&Path> {
    (db.file_name()? == app_db::FILE_NAME).then(|| db.parent()).flatten()
}
