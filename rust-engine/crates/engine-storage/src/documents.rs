//! The app's JSON documents and the assistant's conversations, as rows of `tablex.db`.
//!
//! Every function takes the path of the database (the app passes [`crate::app_db::default_path`];
//! tests and scenarios pass a file of their own) and opens it for the call, so nothing is held
//! between calls and several processes can use it together.
//!
//! # `documents`: one row per document
//!
//! `(kind, name) -> json`, the JSON text exactly as the caller produced it (this module never
//! parses it).
//!
//! | kind | name | what | old file (no longer written) |
//! |---|---|---|---|
//! | [`SESSION`] | `workspace` | the tabs the window had open | `gpui-workspace-session.json` |
//! | [`UPDATE_CHECK`] | `state` | when the last update check ran, the version skipped | `update-check.json` |
//! | [`DIAGRAM`] | 24 hex digits, a hash of the diagram's key | positions and annotations of one diagram | `diagrams/<name>.json` |
//!
//! # `assistant_conversations`: one row per conversation
//!
//! `(connection, id)`; `body` is the whole document the assistant wrote as a file before (header
//! line, then the conversation), `header` its header as JSON, so a listing never reads a body;
//! `updated` and `seq` repeat two of its fields for ordering and for refusing an older write.
//! The old files are `assistant/conversations/<connection>/<id>.json`.
//!
//! # Moving the old files in
//!
//! The first use of each kind finds no marker in `imports` and copies what the old files held
//! (see [`crate::legacy`]) when the database is the app's own (`tablex.db`, the old files are
//! beside it). The old files stay where they are, unchanged: nothing writes them any more. A file
//! that cannot be read is skipped with a warning ([`crate::legacy::take_warnings`]) and tried again
//! at the next start; the others are moved in.

use std::path::{Path, PathBuf};

use rusqlite::{params, Connection, OptionalExtension};

use crate::app_db::{self, AppDbError};
use crate::legacy::{self, ImportError};

/// The workspace session (one document, named [`SESSION_NAME`]).
pub const SESSION: &str = "session";
pub const SESSION_NAME: &str = "workspace";
/// Where a session that could not be used is kept (set aside, not destroyed).
pub const SESSION_CORRUPT_NAME: &str = "workspace.corrupt";
/// Update-check state (one document, named [`UPDATE_CHECK_NAME`]).
pub const UPDATE_CHECK: &str = "update-check";
pub const UPDATE_CHECK_NAME: &str = "state";
/// One document per diagram.
pub const DIAGRAM: &str = "diagram";

/// The largest conversation body kept (and the largest old file imported).
pub const MAX_CONVERSATION_BYTES: usize = 8 * 1024 * 1024;

const SESSION_FILE: &str = "gpui-workspace-session.json";
const UPDATE_CHECK_FILE: &str = "update-check.json";
const DIAGRAMS_DIR: &str = "diagrams";
const CONVERSATIONS_DIR: [&str; 2] = ["assistant", "conversations"];

fn now() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |elapsed| elapsed.as_secs() as i64)
}

/// A statement that failed.
fn at(db: &Path) -> impl Fn(rusqlite::Error) -> AppDbError + '_ {
    move |source| AppDbError::Sqlite {
        path: db.to_owned(),
        source,
    }
}

/// Opens the database and, when `kind` has an old file beside it that was not moved in yet,
/// moves it in.
fn open_kind(db: &Path, kind: &str) -> Result<Connection, AppDbError> {
    open_kind_from(db, kind, legacy::beside(db))
}

/// [`open_kind`] with the folder the old files are in given (`None`: there are none).
fn open_kind_from(db: &Path, kind: &str, dir: Option<&Path>) -> Result<Connection, AppDbError> {
    let mut conn = app_db::open(db)?;
    let Some(dir) = dir else {
        return Ok(conn);
    };
    match kind {
        SESSION => {
            let file = dir.join(SESSION_FILE);
            legacy::import_tolerant(&mut conn, db, "session", &file, |tx| {
                import_text(tx, &file, SESSION, SESSION_NAME, true)
            })?;
        }
        UPDATE_CHECK => {
            let file = dir.join(UPDATE_CHECK_FILE);
            legacy::import_tolerant(&mut conn, db, "update-check", &file, |tx| {
                import_text(tx, &file, UPDATE_CHECK, UPDATE_CHECK_NAME, false)
            })?;
        }
        DIAGRAM => {
            // `diagrams`; a data dir that was made by an older build names it `Diagrams` (the same
            // folder on a case-insensitive file system).
            let folder = [DIAGRAMS_DIR, "Diagrams"]
                .iter()
                .map(|name| dir.join(name))
                .find(|path| path.is_dir())
                .unwrap_or_else(|| dir.join(DIAGRAMS_DIR));
            legacy::import_tolerant(&mut conn, db, "diagrams", &folder, |tx| import_diagrams(tx, &folder))?;
        }
        _ => {}
    }
    Ok(conn)
}

/// Moves every old file of the app in, as the first use of each kind would (for a check of real
/// data: `dir` is the folder the old files are in, which is not touched; `db` is where they go).
pub fn import_all_from(db: &Path, dir: &Path) -> Result<(), AppDbError> {
    for kind in [SESSION, UPDATE_CHECK, DIAGRAM] {
        open_kind_from(db, kind, Some(dir))?;
    }
    open_conversations_from(db, Some(dir))?;
    Ok(())
}

/// One old file as one document; a file that cannot be read is skipped (and reported). With
/// `strict`, text that is not UTF-8 is not a document of this kind (the previous version could
/// not read such a file either): it is kept aside as [`SESSION_CORRUPT_NAME`] and said once.
fn import_text(
    tx: &rusqlite::Transaction<'_>,
    file: &Path,
    kind: &str,
    name: &str,
    strict: bool,
) -> Result<Vec<ImportError>, ImportError> {
    let bytes = match std::fs::read(file) {
        Ok(bytes) => bytes,
        Err(error) => return Ok(vec![ImportError::new(file, error)]),
    };
    let (text, name) = match String::from_utf8(bytes) {
        Ok(text) => (text, name),
        Err(invalid) if strict => {
            legacy::warn(format!(
                "{} is not valid text and was set aside as a damaged saved workspace",
                file.display()
            ));
            (String::from_utf8_lossy(invalid.as_bytes()).into_owned(), SESSION_CORRUPT_NAME)
        }
        Err(invalid) => (String::from_utf8_lossy(invalid.as_bytes()).into_owned(), name),
    };
    tx.execute(
        "INSERT OR IGNORE INTO documents (kind, name, json, updated_at) VALUES (?1, ?2, ?3, ?4)",
        params![kind, name, text, now()],
    )
    .map_err(|error| ImportError::new(file, error))?;
    Ok(Vec::new())
}

fn import_diagrams(tx: &rusqlite::Transaction<'_>, folder: &Path) -> Result<Vec<ImportError>, ImportError> {
    let entries = std::fs::read_dir(folder).map_err(|error| ImportError::new(folder, error))?;
    let mut skipped = Vec::new();
    for entry in entries {
        let entry = entry.map_err(|error| ImportError::new(folder, error))?;
        let path = entry.path();
        let name = entry.file_name().to_string_lossy().into_owned();
        // Only the files the app names by the hash of a diagram's key (24 hex digits):
        // temporary files of an interrupted write, files of the Swift app (named by
        // connection) and anything else are not diagrams of this build.
        let Some(stem) = name.strip_suffix(".json").filter(|stem| is_diagram_name(stem)) else {
            continue;
        };
        if !std::fs::metadata(&path).is_ok_and(|meta| meta.is_file()) {
            continue;
        }
        match read_text(&path) {
            Ok(text) => {
                tx.execute(
                    "INSERT OR IGNORE INTO documents (kind, name, json, updated_at) VALUES (?1, ?2, ?3, ?4)",
                    params![DIAGRAM, stem, text, now()],
                )
                .map_err(|error| ImportError::new(&path, error))?;
            }
            Err(error) => skipped.push(error),
        }
    }
    Ok(skipped)
}

fn is_diagram_name(stem: &str) -> bool {
    stem.len() == 24 && stem.bytes().all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
}

/// The text of an old file. Bytes that are not UTF-8 are replaced (the document then fails to
/// parse where it is used, as the file did); an unreadable file is an import error.
fn read_text(path: &Path) -> Result<String, ImportError> {
    std::fs::read(path)
        .map(|bytes| String::from_utf8_lossy(&bytes).into_owned())
        .map_err(|error| ImportError::new(path, error))
}

/// The document `kind` / `name`.
pub fn get(db: &Path, kind: &str, name: &str) -> Result<Option<String>, AppDbError> {
    open_kind(db, kind)?
        .query_row(
            "SELECT json FROM documents WHERE kind = ?1 AND name = ?2",
            [kind, name],
            |row| row.get(0),
        )
        .optional()
        .map_err(at(db))
}

/// Stores `json` as the document `kind` / `name`, replacing the one there.
pub fn put(db: &Path, kind: &str, name: &str, json: &str) -> Result<(), AppDbError> {
    let mut conn = open_kind(db, kind)?;
    let tx = app_db::write_transaction(&mut conn).map_err(at(db))?;
    tx.execute(
        "INSERT INTO documents (kind, name, json, updated_at) VALUES (?1, ?2, ?3, ?4)
         ON CONFLICT(kind, name) DO UPDATE SET json = excluded.json, updated_at = excluded.updated_at",
        params![kind, name, json, now()],
    )
    .map_err(at(db))?;
    tx.commit().map_err(at(db))
}

/// Removes the document; whether there was one.
pub fn delete(db: &Path, kind: &str, name: &str) -> Result<bool, AppDbError> {
    let mut conn = open_kind(db, kind)?;
    let tx = app_db::write_transaction(&mut conn).map_err(at(db))?;
    let removed = tx
        .execute("DELETE FROM documents WHERE kind = ?1 AND name = ?2", [kind, name])
        .map_err(at(db))?;
    tx.commit().map_err(at(db))?;
    Ok(removed > 0)
}

/// Gives the document another name (a damaged one is moved aside, not destroyed), replacing any
/// document of that name; whether there was one to move.
pub fn rename(db: &Path, kind: &str, from: &str, to: &str) -> Result<bool, AppDbError> {
    let mut conn = open_kind(db, kind)?;
    let tx = app_db::write_transaction(&mut conn).map_err(at(db))?;
    tx.execute("DELETE FROM documents WHERE kind = ?1 AND name = ?2", [kind, to])
        .map_err(at(db))?;
    let moved = tx
        .execute(
            "UPDATE documents SET name = ?3 WHERE kind = ?1 AND name = ?2",
            [kind, from, to],
        )
        .map_err(at(db))?;
    tx.commit().map_err(at(db))?;
    Ok(moved > 0)
}

/// How many documents of `kind` there are.
pub fn count(db: &Path, kind: &str) -> Result<usize, AppDbError> {
    open_kind(db, kind)?
        .query_row("SELECT COUNT(*) FROM documents WHERE kind = ?1", [kind], |row| row.get::<_, i64>(0))
        .map(|count| count as usize)
        .map_err(at(db))
}

// ---- conversations --------------------------------------------------------------------------

/// A row of a conversation listing (the body is not read).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ConversationRow {
    pub id: String,
    /// The header, JSON.
    pub header: String,
    /// The byte length of the body.
    pub size: u64,
}

/// What [`put_conversation`] did.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PutConversation {
    /// Written; `pruned` older conversations of the connection went to stay within the limit.
    Written { pruned: usize },
    /// The row holds more messages than this write (a newer write came first): left alone.
    Stale,
}

/// A conversation as it is stored.
pub struct ConversationDoc<'a> {
    pub connection: &'a str,
    pub id: &'a str,
    pub updated: u64,
    pub seq: u64,
    pub header: &'a str,
    pub body: &'a str,
}

fn open_conversations(db: &Path) -> Result<Connection, AppDbError> {
    open_conversations_from(db, legacy::beside(db))
}

fn open_conversations_from(db: &Path, dir: Option<&Path>) -> Result<Connection, AppDbError> {
    let mut conn = app_db::open(db)?;
    let Some(dir) = dir else {
        return Ok(conn);
    };
    let root = CONVERSATIONS_DIR.iter().fold(dir.to_path_buf(), |path, part| path.join(part));
    legacy::import_tolerant(&mut conn, db, "assistant", &root, |tx| import_conversations(tx, &root))?;
    Ok(conn)
}

/// `<root>/<connection>/<id>.json` files: the ones that read as a conversation are copied (the
/// rest, and files over [`MAX_CONVERSATION_BYTES`], are not conversations of this build and stay
/// where they are); a file that cannot be read at all fails the import.
fn import_conversations(tx: &rusqlite::Transaction<'_>, root: &Path) -> Result<Vec<ImportError>, ImportError> {
    let skipped = Vec::new();
    let folders = std::fs::read_dir(root).map_err(|error| ImportError::new(root, error))?;
    for folder in folders {
        let folder = folder.map_err(|error| ImportError::new(root, error))?;
        let connection = folder.file_name().to_string_lossy().to_ascii_lowercase();
        if !name_ok(&connection) || !folder.file_type().is_ok_and(|kind| kind.is_dir()) {
            continue;
        }
        let folder = folder.path();
        let files = match std::fs::read_dir(&folder) {
            Ok(files) => files,
            // A folder that cannot be listed has nothing the user can open: it is shown as a
            // problem by `old_conversation_problems`, like a file that cannot be read.
            Err(_) => continue,
        };
        for file in files {
            let file = file.map_err(|error| ImportError::new(&folder, error))?;
            let name = file.file_name().to_string_lossy().into_owned();
            let Some(id) = name.strip_suffix(".json").filter(|id| name_ok(id)) else {
                continue;
            };
            let path = file.path();
            let usable = std::fs::symlink_metadata(&path)
                .is_ok_and(|meta| meta.is_file() && meta.len() <= MAX_CONVERSATION_BYTES as u64);
            if !usable {
                continue;
            }
            let Ok(bytes) = std::fs::read(&path) else {
                // Not moved in, and not tried again: it is a problem the user sees and can delete
                // (`old_conversation_problems`); a store the user has cleared must stay cleared.
                continue;
            };
            let Ok(body) = String::from_utf8(bytes) else {
                continue;
            };
            let Some((header, updated, seq)) = header_of(&body, id) else {
                continue;
            };
            tx.execute(
                "INSERT OR IGNORE INTO assistant_conversations
                 (connection, id, updated, seq, header, size, body) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
                params![connection, id, updated as i64, seq as i64, header, body.len() as i64, body],
            )
            .map_err(|error| ImportError::new(&path, error))?;
        }
    }
    Ok(skipped)
}

/// A name that is safe as an id (ASCII letters, digits, `-` and `_`, at most 64).
fn name_ok(name: &str) -> bool {
    !name.is_empty()
        && name.len() <= 64
        && name.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_')
}

/// The header of a conversation file as JSON, with its `updated` and `seq`: the first line of the
/// file is `{...,` and the object goes on after it. `None` when the file is not one for `id`.
fn header_of(body: &str, id: &str) -> Option<(String, u64, u64)> {
    let line = body.split_once('\n')?.0.strip_suffix(',')?;
    let header = format!("{line}}}");
    let value: serde_json::Value = serde_json::from_str(&header).ok()?;
    (value.get("id")?.as_str()? == id).then_some(())?;
    Some((
        header,
        value.get("updated")?.as_u64()?,
        value.get("seq").and_then(serde_json::Value::as_u64).unwrap_or(0),
    ))
}

/// Writes a conversation, unless the stored one has more messages ([`PutConversation::Stale`]),
/// and then keeps the `keep` most recently updated conversations of the connection.
pub fn put_conversation(db: &Path, doc: &ConversationDoc<'_>, keep: usize) -> Result<PutConversation, AppDbError> {
    let mut conn = open_conversations(db)?;
    let tx = app_db::write_transaction(&mut conn).map_err(at(db))?;
    let stored: Option<i64> = tx
        .query_row(
            "SELECT seq FROM assistant_conversations WHERE connection = ?1 AND id = ?2",
            [doc.connection, doc.id],
            |row| row.get(0),
        )
        .optional()
        .map_err(at(db))?;
    if stored.is_some_and(|stored| stored as u64 > doc.seq) {
        return Ok(PutConversation::Stale);
    }
    tx.execute(
        "INSERT OR REPLACE INTO assistant_conversations
         (connection, id, updated, seq, header, size, body) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
        params![
            doc.connection,
            doc.id,
            doc.updated as i64,
            doc.seq as i64,
            doc.header,
            doc.body.len() as i64,
            doc.body
        ],
    )
    .map_err(at(db))?;
    let pruned = tx
        .execute(
            "DELETE FROM assistant_conversations WHERE connection = ?1 AND id NOT IN (
                 SELECT id FROM assistant_conversations WHERE connection = ?1
                 ORDER BY updated DESC, id DESC LIMIT ?2)",
            params![doc.connection, keep as i64],
        )
        .map_err(at(db))?;
    tx.commit().map_err(at(db))?;
    Ok(PutConversation::Written { pruned })
}

/// The newest `limit` conversations of `connection` (newest update first), without their bodies.
pub fn list_conversations(db: &Path, connection: &str, limit: usize) -> Result<Vec<ConversationRow>, AppDbError> {
    let conn = open_conversations(db)?;
    let mut stmt = conn
        .prepare(
            // Only names that are ids (letters, digits, `-`, `_`; at most 64): a row of any other
            // name is not a conversation of the app and must not take a place in the limit.
            "SELECT id, header, size FROM assistant_conversations
             WHERE connection = ?1 AND id NOT GLOB '*[^A-Za-z0-9_-]*' AND length(id) BETWEEN 1 AND 64
             ORDER BY updated DESC, id ASC LIMIT ?2",
        )
        .map_err(at(db))?;
    let rows = stmt
        .query_map(params![connection, limit as i64], |row| {
            Ok(ConversationRow {
                id: row.get(0)?,
                header: row.get(1)?,
                size: row.get::<_, i64>(2)? as u64,
            })
        })
        .map_err(at(db))?;
    rows.collect::<Result<_, _>>().map_err(at(db))
}

/// The stored body of a conversation (`None`: there is none, or it is over `max_bytes`, which is
/// `Err(size)` so the caller can say so).
pub fn load_conversation(db: &Path, connection: &str, id: &str, max_bytes: usize) -> Result<Result<Option<String>, u64>, AppDbError> {
    let conn = open_conversations(db)?;
    let size: Option<i64> = conn
        .query_row(
            "SELECT size FROM assistant_conversations WHERE connection = ?1 AND id = ?2",
            [connection, id],
            |row| row.get(0),
        )
        .optional()
        .map_err(at(db))?;
    match size {
        None => Ok(Ok(None)),
        Some(size) if size as u64 > max_bytes as u64 => Ok(Err(size as u64)),
        Some(_) => conn
            .query_row(
                "SELECT body FROM assistant_conversations WHERE connection = ?1 AND id = ?2",
                [connection, id],
                |row| row.get(0),
            )
            .optional()
            .map(Ok)
            .map_err(at(db)),
    }
}

/// The folder the previous version kept conversations in, beside the app's own database.
fn old_conversations_root(db: &Path) -> Option<PathBuf> {
    let dir = legacy::beside(db)?;
    Some(CONVERSATIONS_DIR.iter().fold(dir.to_path_buf(), |path, part| path.join(part)))
}

/// The old conversation files `<root>/<connection folder>/<id>.json` that match: `connection` and
/// `id` of the one asked for (`None`: any), folders compared without regard to case, only names
/// this app wrote, only regular files.
fn old_conversation_files(db: &Path, connection: Option<&str>, id: Option<&str>) -> Vec<(String, String, PathBuf)> {
    let Some(root) = old_conversations_root(db) else {
        return Vec::new();
    };
    let mut found = Vec::new();
    let Ok(folders) = std::fs::read_dir(&root) else {
        return found;
    };
    for folder in folders.flatten() {
        let folder_name = folder.file_name().to_string_lossy().to_ascii_lowercase();
        if !name_ok(&folder_name)
            || connection.is_some_and(|wanted| wanted != folder_name)
            || !folder.file_type().is_ok_and(|kind| kind.is_dir())
        {
            continue;
        }
        let Ok(files) = std::fs::read_dir(folder.path()) else {
            continue;
        };
        for file in files.flatten() {
            let name = file.file_name().to_string_lossy().into_owned();
            let Some(stem) = name.strip_suffix(".json").filter(|stem| name_ok(stem)) else {
                continue;
            };
            if id.is_some_and(|wanted| wanted != stem) || !file.file_type().is_ok_and(|kind| kind.is_file()) {
                continue;
            }
            found.push((folder_name.clone(), stem.to_owned(), file.path()));
        }
    }
    found
}

/// Deletes the old files that match, which can hold query results: the user asked for the
/// conversation to be gone, and the old copy is the one they cannot see. Returns the ones that had
/// no row (not moved in: a damaged file, say) and the first file that could not be deleted.
fn delete_old_conversations(
    db: &Path,
    tx: &rusqlite::Transaction<'_>,
    connection: Option<&str>,
    id: Option<&str>,
) -> Result<(usize, Option<AppDbError>), AppDbError> {
    let mut orphans = 0;
    let mut failed = None;
    for (folder, stem, path) in old_conversation_files(db, connection, id) {
        let has_row = tx
            .query_row(
                "SELECT 1 FROM assistant_conversations WHERE connection = ?1 AND id = ?2",
                [&folder, &stem],
                |_| Ok(()),
            )
            .optional()
            .map_err(at(db))?
            .is_some();
        match std::fs::remove_file(&path) {
            Ok(()) => orphans += usize::from(!has_row),
            Err(source) => {
                failed.get_or_insert(AppDbError::OldFile { file: path, source });
            }
        }
    }
    Ok((orphans, failed))
}

/// Deletes one conversation and its old file; whether there was either, and the old file that
/// could not be deleted, if one.
pub fn delete_conversation(db: &Path, connection: &str, id: &str) -> Result<(bool, Option<AppDbError>), AppDbError> {
    let mut conn = open_conversations(db)?;
    let tx = app_db::write_transaction(&mut conn).map_err(at(db))?;
    let (orphans, failed) = delete_old_conversations(db, &tx, Some(connection), Some(id))?;
    let removed = tx
        .execute(
            "DELETE FROM assistant_conversations WHERE connection = ?1 AND id = ?2",
            [connection, id],
        )
        .map_err(at(db))?;
    tx.commit().map_err(at(db))?;
    Ok((removed + orphans > 0, failed))
}

/// Deletes the conversations of one connection (`Some`) or of all (`None`), and their old files;
/// how many went (an old file that was moved in counts once) and the first old file that could not
/// be deleted, if one.
pub fn delete_conversations(db: &Path, connection: Option<&str>) -> Result<(usize, Option<AppDbError>), AppDbError> {
    let mut conn = open_conversations(db)?;
    let tx = app_db::write_transaction(&mut conn).map_err(at(db))?;
    let (orphans, failed) = delete_old_conversations(db, &tx, connection, None)?;
    let removed = match connection {
        Some(connection) => tx.execute("DELETE FROM assistant_conversations WHERE connection = ?1", [connection]),
        None => tx.execute("DELETE FROM assistant_conversations", []),
    }
    .map_err(at(db))?;
    tx.commit().map_err(at(db))?;
    Ok((removed + orphans, failed))
}

/// An old conversation file that was not moved in, and why.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OldConversation {
    pub id: String,
    pub bytes: u64,
    pub why: String,
}

/// The old conversation files of `connection` that are not in the database although the folder
/// was moved in: files that could not be read, are over [`MAX_CONVERSATION_BYTES`] or are not a
/// conversation of this build. They are the user's to see and to delete ([`delete_conversation`],
/// [`delete_conversations`] remove them); nothing retries them.
pub fn old_conversation_problems(db: &Path, connection: &str) -> Result<Vec<OldConversation>, AppDbError> {
    let conn = open_conversations(db)?;
    let moved_in = legacy::marked(&conn, "assistant").map_err(at(db))?;
    if !moved_in {
        return Ok(Vec::new());
    }
    let mut problems = Vec::new();
    for (_, id, path) in old_conversation_files(db, Some(connection), None) {
        let has_row = conn
            .query_row(
                "SELECT 1 FROM assistant_conversations WHERE connection = ?1 AND id = ?2",
                [connection, id.as_str()],
                |_| Ok(()),
            )
            .optional()
            .map_err(at(db))?
            .is_some();
        if has_row {
            continue;
        }
        let bytes = std::fs::metadata(&path).map_or(0, |meta| meta.len());
        let why = if bytes > MAX_CONVERSATION_BYTES as u64 {
            "it is larger than the limit".to_owned()
        } else {
            match std::fs::read(&path) {
                Err(error) => format!("it cannot be read: {error}"),
                Ok(bytes) => match String::from_utf8(bytes) {
                    Err(_) => "it is not valid text".to_owned(),
                    Ok(text) if header_of(&text, &id).is_none() => "its header does not read".to_owned(),
                    Ok(_) => continue,
                },
            }
        };
        problems.push(OldConversation { id, bytes, why });
    }
    Ok(problems)
}

/// Opens the database for a call that has nothing to read or write but must surface a problem
/// (a damaged or newer database, an old file that cannot be moved in): the conversations'.
pub fn check_conversations(db: &Path) -> Result<(), AppDbError> {
    open_conversations(db).map(drop)
}
