//! Query history: the `query_history` table of the app database ([`crate::app_db`]), one cached
//! connection per process.

use crate::app_db::{self, AppDbError};
use crate::legacy;
use rusqlite::{params, Connection};

/// What the history's functions return (a `rusqlite` result, so that a caller need not depend on it).
pub type Result<T> = rusqlite::Result<T>;
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
    /// `ok` or `failed`; `None` in a row an older build wrote (it recorded only runs that worked).
    pub status: Option<String>,
    /// Why a failed run failed.
    pub error: Option<String>,
    pub duration_ms: Option<i64>,
    /// [`KIND_QUERY`], [`KIND_DATA_CHANGE`], [`KIND_SCHEMA_CHANGE`] or [`KIND_REDIS_COMMAND`].
    pub kind: Option<String>,
}

impl HistoryEntry {
    /// Whether the run ended in an error.
    pub fn failed(&self) -> bool {
        self.status.as_deref() == Some(STATUS_FAILED)
    }
}

/// A statement of the Mongo shell: what its calls do (`db.users.insertOne(...)` changes data,
/// `db.users.drop()` the schema, `find` and the rest read).
fn classify_shell(script: &str) -> &'static str {
    let lower = script.to_ascii_lowercase();
    const SCHEMA: [&str; 8] = [
        ".drop(", ".dropdatabase(", ".createcollection(", ".createindex(", ".dropindex", ".renamecollection(", ".createindexes(", ".createview(",
    ];
    const DATA: [&str; 12] = [
        ".insert", ".update", ".delete", ".remove(", ".save(", ".replaceone(", ".bulkwrite(", ".findoneandupdate(",
        ".findoneanddelete(", ".findoneandreplace(", ".findandmodify(", "$out",
    ];
    if SCHEMA.iter().any(|call| lower.contains(call)) {
        KIND_SCHEMA_CHANGE
    } else if DATA.iter().any(|call| lower.contains(call)) || lower.contains("$merge") {
        KIND_DATA_CHANGE
    } else {
        KIND_QUERY
    }
}

pub const STATUS_OK: &str = "ok";
pub const STATUS_FAILED: &str = "failed";
pub const KIND_QUERY: &str = "query";
pub const KIND_DATA_CHANGE: &str = "data_change";
pub const KIND_SCHEMA_CHANGE: &str = "schema_change";
pub const KIND_REDIS_COMMAND: &str = "redis_command";

/// What a SQL text does: a schema change when any of its statements creates, alters, drops or
/// renames, else a data change when any inserts, updates, deletes, merges, replaces or truncates,
/// else a query. Comments are skipped; `;` in a string literal is not told apart from a separator
/// (a wrong guess only files a statement under another kind).
pub fn classify_sql(sql: &str) -> &'static str {
    if sql.trim_start().get(..3).is_some_and(|start| start.eq_ignore_ascii_case("db.")) {
        return classify_shell(sql);
    }
    let mut text = String::with_capacity(sql.len());
    let mut chars = sql.chars().peekable();
    while let Some(ch) = chars.next() {
        match ch {
            '-' if chars.peek() == Some(&'-') => {
                for c in chars.by_ref() {
                    if c == '\n' {
                        break;
                    }
                }
                text.push(' ');
            }
            '#' => {
                for c in chars.by_ref() {
                    if c == '\n' {
                        break;
                    }
                }
                text.push(' ');
            }
            '/' if chars.peek() == Some(&'*') => {
                chars.next();
                let mut prev = ' ';
                for c in chars.by_ref() {
                    if prev == '*' && c == '/' {
                        break;
                    }
                    prev = c;
                }
                text.push(' ');
            }
            c => text.push(c),
        }
    }
    let mut kind = KIND_QUERY;
    for statement in text.split(';') {
        let word: String = statement
            .trim_start()
            .chars()
            .take_while(char::is_ascii_alphabetic)
            .collect::<String>()
            .to_ascii_lowercase();
        match word.as_str() {
            "create" | "alter" | "drop" | "rename" => return KIND_SCHEMA_CHANGE,
            "insert" | "update" | "delete" | "merge" | "replace" | "truncate" => kind = KIND_DATA_CHANGE,
            _ => {}
        }
    }
    kind
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
    // The columns of schema step 7, so that the same queries work on this file.
    for (column, ty) in [("status", "TEXT"), ("error", "TEXT"), ("duration_ms", "INTEGER"), ("kind", "TEXT")] {
        let has = conn.query_row(
            "SELECT COUNT(*) FROM pragma_table_info('query_history') WHERE name = ?1",
            [column],
            |row| row.get::<_, i64>(0),
        )? > 0;
        if !has {
            conn.execute(&format!("ALTER TABLE query_history ADD COLUMN {column} {ty}"), [])?;
        }
    }
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
    record(&Record {
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
        error: None,
        duration_ms: None,
        kind: classify_sql(filter),
    })
}

/// One run to put in the history. `filter` is the statement (or a Mongo find's filter).
pub struct Record<'a> {
    pub conn_uuid: &'a str,
    pub database: &'a str,
    pub collection: &'a str,
    pub filter: &'a str,
    pub project: &'a str,
    pub sort: &'a str,
    pub skip: i64,
    pub limit: i64,
    pub max_time_ms: i64,
    pub result_count: i64,
    /// `Some`: the run failed with this error (clipped to [`MAX_ERROR_CHARS`]).
    pub error: Option<&'a str>,
    pub duration_ms: Option<i64>,
    /// One of the `KIND_*` names.
    pub kind: &'a str,
}

/// The longest error kept for a failed run, in characters.
pub const MAX_ERROR_CHARS: usize = 500;

/// How many runs one connection, database and table keep.
const KEEP_PER_CONTEXT: i64 = 500;

fn now_secs() -> f64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        // A clock before 1970 must not panic while a storage lock is held.
        .unwrap_or_default()
        .as_secs_f64()
}

/// Puts a run in the history. The same statement of the same context is one row: running it again
/// moves it to the top and takes the new outcome (rows, duration, error).
pub fn record(run: &Record<'_>) -> Result<()> {
    ensure_db()?;
    let db = HISTORY_DB.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
    let conn = db.as_ref().unwrap();
    record_in(conn, run, now_secs())
}

/// [`record`] on `conn` at `now` (epoch seconds).
pub fn record_in(conn: &Connection, run: &Record<'_>, now: f64) -> Result<()> {
    let error = run.error.map(|error| match error.char_indices().nth(MAX_ERROR_CHARS) {
        Some((end, _)) => &error[..end],
        None => error,
    });
    let status = if error.is_some() { STATUS_FAILED } else { STATUS_OK };
    conn.execute(
        "INSERT INTO query_history
         (conn_uuid, database, collection, filter, project, sort,
          skip, \"limit\", max_time_ms, result_count, timestamp, status, error, duration_ms, kind)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14, ?15)
         ON CONFLICT (conn_uuid, database, collection, filter, project, sort, skip, \"limit\", max_time_ms)
         DO UPDATE SET result_count = excluded.result_count, timestamp = excluded.timestamp,
                       status = excluded.status, error = excluded.error,
                       duration_ms = excluded.duration_ms, kind = excluded.kind",
        params![
            run.conn_uuid,
            run.database,
            run.collection,
            run.filter,
            run.project,
            run.sort,
            run.skip,
            run.limit,
            run.max_time_ms,
            run.result_count,
            now,
            status,
            error,
            run.duration_ms,
            run.kind
        ],
    )?;
    conn.execute(
        "DELETE FROM query_history
         WHERE conn_uuid = ?1 AND database = ?2 AND collection = ?3
         AND id NOT IN (
             SELECT id FROM query_history
             WHERE conn_uuid = ?1 AND database = ?2 AND collection = ?3
             ORDER BY timestamp DESC LIMIT ?4
         )",
        params![run.conn_uuid, run.database, run.collection, KEEP_PER_CONTEXT],
    )?;
    Ok(())
}

const COLUMNS: &str = "id, conn_uuid, database, collection, filter, project, sort,
                skip, \"limit\", max_time_ms, result_count, timestamp, status, error, duration_ms, kind";

fn entry_of(row: &rusqlite::Row<'_>) -> Result<HistoryEntry> {
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
        status: row.get(12)?,
        error: row.get(13)?,
        duration_ms: row.get(14)?,
        kind: row.get(15)?,
    })
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

    let mut stmt = conn.prepare(&format!(
        "SELECT {COLUMNS}
         FROM query_history
         WHERE conn_uuid = ?1 AND database = ?2 AND collection = ?3
         ORDER BY timestamp DESC
         LIMIT ?4"
    ))?;
    let entries = stmt.query_map(params![conn_uuid, database, collection, limit], entry_of)?;
    entries.collect()
}

/// What the Query History manager narrows the runs to; every part is optional.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct HistoryFilter {
    /// Words that must all be in the statement, the table or the error (any case).
    pub text: String,
    /// Only runs of these connections (and, where `databases` is not empty, of those databases);
    /// empty: every connection.
    pub scopes: Vec<Scope>,
    /// Only runs of this kind (one of the `KIND_*` names).
    pub kind: Option<String>,
    /// Only runs at or after this time (epoch seconds).
    pub since: Option<f64>,
    /// Only runs before this time (epoch seconds).
    pub until: Option<f64>,
    pub status: StatusFilter,
}

/// One connection of a filter, narrowed to some of its databases when `databases` is not empty.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Scope {
    pub conn_uuid: String,
    pub databases: Vec<String>,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum StatusFilter {
    #[default]
    Any,
    Ok,
    Failed,
}

/// Where the next page of a search starts: the last row of the page before.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Cursor {
    pub timestamp: f64,
    pub id: i64,
}

impl HistoryFilter {
    /// The `WHERE` clause (with its parameters) of this filter and, past `after`, of the next page.
    fn clause(&self, after: Option<Cursor>) -> (String, Vec<rusqlite::types::Value>) {
        use rusqlite::types::Value;
        let mut parts: Vec<String> = Vec::new();
        let mut values: Vec<Value> = Vec::new();
        let mut bind = |value: Value| {
            values.push(value);
            values.len()
        };
        if !self.scopes.is_empty() {
            let mut any = Vec::new();
            for scope in &self.scopes {
                let c = bind(Value::Text(scope.conn_uuid.clone()));
                if scope.databases.is_empty() {
                    any.push(format!("conn_uuid = ?{c}"));
                } else {
                    let marks: Vec<String> = scope
                        .databases
                        .iter()
                        .map(|database| format!("?{}", bind(Value::Text(database.clone()))))
                        .collect();
                    any.push(format!("(conn_uuid = ?{c} AND database IN ({}))", marks.join(", ")));
                }
            }
            parts.push(format!("({})", any.join(" OR ")));
        }
        if let Some(kind) = &self.kind {
            let n = bind(Value::Text(kind.clone()));
            parts.push(format!("kind = ?{n}"));
        }
        if let Some(since) = self.since {
            let n = bind(Value::Real(since));
            parts.push(format!("timestamp >= ?{n}"));
        }
        if let Some(until) = self.until {
            let n = bind(Value::Real(until));
            parts.push(format!("timestamp < ?{n}"));
        }
        match self.status {
            StatusFilter::Any => {}
            StatusFilter::Ok => parts.push(format!("(status IS NULL OR status <> '{STATUS_FAILED}')")),
            StatusFilter::Failed => parts.push(format!("status = '{STATUS_FAILED}'")),
        }
        for word in self.text.split_whitespace() {
            let mut pattern = String::with_capacity(word.len() + 2);
            pattern.push('%');
            for ch in word.chars() {
                if matches!(ch, '%' | '_' | '\\') {
                    pattern.push('\\');
                }
                pattern.push(ch);
            }
            pattern.push('%');
            let n = bind(Value::Text(pattern));
            parts.push(format!(
                "(filter LIKE ?{n} ESCAPE '\\' OR collection LIKE ?{n} ESCAPE '\\' OR error LIKE ?{n} ESCAPE '\\')"
            ));
        }
        if let Some(after) = after {
            let t = bind(Value::Real(after.timestamp));
            let i = bind(Value::Integer(after.id));
            parts.push(format!("(timestamp < ?{t} OR (timestamp = ?{t} AND id < ?{i}))"));
        }
        let clause = if parts.is_empty() { String::new() } else { format!(" WHERE {}", parts.join(" AND ")) };
        (clause, values)
    }
}

/// The runs that pass `filter`, newest first, `limit` of them from past `after`.
pub fn search(filter: &HistoryFilter, after: Option<Cursor>, limit: u32) -> Result<Vec<HistoryEntry>> {
    ensure_db()?;
    let db = HISTORY_DB.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
    search_in(db.as_ref().unwrap(), filter, after, limit)
}

/// [`search`] on `conn`.
pub fn search_in(
    conn: &Connection,
    filter: &HistoryFilter,
    after: Option<Cursor>,
    limit: u32,
) -> Result<Vec<HistoryEntry>> {
    let (clause, values) = filter.clause(after);
    let mut stmt = conn.prepare_cached(&format!(
        "SELECT {COLUMNS} FROM query_history{clause} ORDER BY timestamp DESC, id DESC LIMIT {limit}"
    ))?;
    let rows = stmt.query_map(rusqlite::params_from_iter(values), entry_of)?;
    rows.collect()
}

/// How many runs pass `filter`.
pub fn count(filter: &HistoryFilter) -> Result<i64> {
    ensure_db()?;
    let db = HISTORY_DB.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
    let (clause, values) = filter.clause(None);
    db.as_ref()
        .unwrap()
        .query_row(
            &format!("SELECT COUNT(*) FROM query_history{clause}"),
            rusqlite::params_from_iter(values),
            |row| row.get(0),
        )
}

/// Deletes every run that passes `filter`; returns how many.
pub fn delete_matching(filter: &HistoryFilter) -> Result<usize> {
    ensure_db()?;
    let db = HISTORY_DB.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
    let (clause, values) = filter.clause(None);
    db.as_ref().unwrap().execute(
        &format!("DELETE FROM query_history{clause}"),
        rusqlite::params_from_iter(values),
    )
}

/// The connections runs were made in, each with its databases (those that have runs), for the
/// scope picker.
pub fn scope_options() -> Result<Vec<(String, Vec<String>)>> {
    ensure_db()?;
    let db = HISTORY_DB.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
    let conn = db.as_ref().unwrap();
    let mut stmt = conn.prepare(
        "SELECT DISTINCT conn_uuid, database FROM query_history ORDER BY conn_uuid, database COLLATE NOCASE",
    )?;
    let mut out: Vec<(String, Vec<String>)> = Vec::new();
    for row in stmt.query_map([], |row| Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?)))? {
        let (conn_uuid, database) = row?;
        if out.last().map(|(last, _)| last != &conn_uuid).unwrap_or(true) {
            out.push((conn_uuid, Vec::new()));
        }
        if !database.is_empty() {
            out.last_mut().unwrap().1.push(database);
        }
    }
    Ok(out)
}

/// Deletes the runs older than `cutoff` (epoch seconds), `batch` rows per statement and the lock
/// let go between statements, so that the app's own writes never wait for a long delete. Returns
/// how many went.
pub fn prune_older_than(cutoff: f64, batch: u32) -> Result<usize> {
    let mut total = 0;
    loop {
        ensure_db()?;
        let deleted = {
            let db = HISTORY_DB.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
            db.as_ref().unwrap().execute(
                "DELETE FROM query_history WHERE id IN
                 (SELECT id FROM query_history WHERE timestamp < ?1 LIMIT ?2)",
                params![cutoff, batch],
            )?
        };
        total += deleted;
        if deleted < batch as usize {
            return Ok(total);
        }
        std::thread::yield_now();
    }
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

    fn run<'a>(sql: &'a str) -> Record<'a> {
        Record {
            conn_uuid: "c1",
            database: "app",
            collection: "",
            filter: sql,
            project: "",
            sort: "",
            skip: 0,
            limit: 200,
            max_time_ms: 0,
            result_count: 3,
            error: None,
            duration_ms: Some(12),
            kind: classify_sql(sql),
        }
    }

    #[test]
    fn classify_sql_reads_the_statements_not_the_comments() {
        assert_eq!(classify_sql("SELECT 1"), KIND_QUERY);
        assert_eq!(classify_sql("  with t as (select 1) select * from t"), KIND_QUERY);
        assert_eq!(classify_sql("-- drop table x\nSELECT 1"), KIND_QUERY);
        assert_eq!(classify_sql("/* create */ UPDATE t SET a = 1"), KIND_DATA_CHANGE);
        assert_eq!(classify_sql("SELECT 1; DELETE FROM t"), KIND_DATA_CHANGE);
        assert_eq!(classify_sql("INSERT INTO t VALUES (1); alter table t add b int"), KIND_SCHEMA_CHANGE);
        assert_eq!(classify_sql("truncate t"), KIND_DATA_CHANGE);
        assert_eq!(classify_sql("{ \"a\": 1 }"), KIND_QUERY);
        assert_eq!(classify_sql(""), KIND_QUERY);
        assert_eq!(classify_sql("db.users.find({a: 1}).sort({b: 1})"), KIND_QUERY);
        assert_eq!(classify_sql("DB.users.insertOne({a: 1})"), KIND_DATA_CHANGE);
        assert_eq!(classify_sql("db.users.drop()"), KIND_SCHEMA_CHANGE);
        assert_eq!(classify_sql("db.users.aggregate([{$match: {}}, {$out: 'x'}])"), KIND_DATA_CHANGE);
    }

    #[test]
    fn a_failed_run_is_kept_with_its_clipped_error_and_duration() {
        let _guard = TEST_LOCK.lock().unwrap();
        use_temp_home("failed");
        let error = "é".repeat(MAX_ERROR_CHARS + 50);
        record(&Record { error: Some(&error), result_count: 0, duration_ms: Some(7), ..run("SELECT * FROM nope") }).unwrap();
        let all = search(&HistoryFilter::default(), None, 10).unwrap();
        assert_eq!(all.len(), 1);
        assert!(all[0].failed());
        assert_eq!(all[0].error.as_deref().unwrap().chars().count(), MAX_ERROR_CHARS);
        assert_eq!(all[0].duration_ms, Some(7));
        assert_eq!(all[0].kind.as_deref(), Some(KIND_QUERY));
        let failed = HistoryFilter { status: StatusFilter::Failed, ..Default::default() };
        assert_eq!(search(&failed, None, 10).unwrap().len(), 1);
        let ok = HistoryFilter { status: StatusFilter::Ok, ..Default::default() };
        assert!(search(&ok, None, 10).unwrap().is_empty());
        assert_eq!(count(&failed).unwrap(), 1);
    }

    #[test]
    fn running_a_statement_again_moves_it_to_the_top_with_its_new_outcome() {
        let _guard = TEST_LOCK.lock().unwrap();
        use_temp_home("again");
        let db = HISTORY_DB.lock().unwrap().is_none();
        assert!(db);
        ensure_db().unwrap();
        let guard = HISTORY_DB.lock().unwrap();
        let conn = guard.as_ref().unwrap();
        record_in(conn, &Record { error: Some("boom"), ..run("SELECT 1") }, 100.0).unwrap();
        record_in(conn, &run("SELECT 2"), 200.0).unwrap();
        record_in(conn, &run("SELECT 1"), 300.0).unwrap();
        let rows = search_in(conn, &HistoryFilter::default(), None, 10).unwrap();
        let order: Vec<_> = rows.iter().map(|r| r.filter.as_str()).collect();
        assert_eq!(order, ["SELECT 1", "SELECT 2"], "one row per statement, the latest run first");
        assert!(!rows[0].failed() && rows[0].error.is_none(), "the new outcome replaced the failure");
        assert_eq!(rows[0].timestamp, 300.0);
    }

    #[test]
    fn search_narrows_by_words_scope_kind_status_and_time_and_pages_by_cursor() {
        let _guard = TEST_LOCK.lock().unwrap();
        use_temp_home("search");
        ensure_db().unwrap();
        let guard = HISTORY_DB.lock().unwrap();
        let conn = guard.as_ref().unwrap();
        let other = |sql, database| Record { conn_uuid: "c2", database, ..run(sql) };
        record_in(conn, &run("SELECT name FROM users"), 100.0).unwrap();
        record_in(conn, &run("UPDATE users SET name = 'a_b%'"), 200.0).unwrap();
        record_in(conn, &Record { error: Some("no such table: orders"), ..run("SELECT * FROM orders") }, 300.0).unwrap();
        record_in(conn, &other("DROP TABLE users", "ops"), 400.0).unwrap();
        record_in(conn, &other("SELECT 1", "main"), 500.0).unwrap();
        let find = |filter: HistoryFilter| -> Vec<String> {
            search_in(conn, &filter, None, 50).unwrap().into_iter().map(|r| r.filter).collect()
        };
        let text = |t: &str| HistoryFilter { text: t.into(), ..Default::default() };
        assert_eq!(find(text("USERS name")), ["UPDATE users SET name = 'a_b%'", "SELECT name FROM users"]);
        assert_eq!(find(text("a_b%")).len(), 1, "% and _ are not wildcards");
        assert_eq!(find(text("a%b")).len(), 0);
        assert_eq!(find(text("no such table")), ["SELECT * FROM orders"], "the error is searched too");
        let scope = |c: &str, d: &[&str]| Scope { conn_uuid: c.into(), databases: d.iter().map(|d| d.to_string()).collect() };
        assert_eq!(find(HistoryFilter { scopes: vec![scope("c2", &[])], ..Default::default() }).len(), 2);
        assert_eq!(find(HistoryFilter { scopes: vec![scope("c2", &["ops"])], ..Default::default() }), ["DROP TABLE users"]);
        assert_eq!(find(HistoryFilter { scopes: vec![scope("c2", &["ops"]), scope("c1", &[])], ..Default::default() }).len(), 4);
        let kind = |k: &str| HistoryFilter { kind: Some(k.into()), ..Default::default() };
        assert_eq!(find(kind(KIND_DATA_CHANGE)), ["UPDATE users SET name = 'a_b%'"]);
        assert_eq!(find(kind(KIND_SCHEMA_CHANGE)), ["DROP TABLE users"]);
        assert_eq!(find(kind(KIND_QUERY)).len(), 3);
        assert_eq!(find(HistoryFilter { since: Some(200.0), until: Some(400.0), ..Default::default() }).len(), 2, "since is inclusive, until is not");
        // Pages of two, newest first, no row twice and none missed.
        let mut seen = Vec::new();
        let mut after = None;
        loop {
            let page = search_in(conn, &HistoryFilter::default(), after, 2).unwrap();
            let Some(last) = page.last() else { break };
            after = Some(Cursor { timestamp: last.timestamp, id: last.id });
            seen.extend(page.into_iter().map(|r| r.timestamp));
        }
        assert_eq!(seen, [500.0, 400.0, 300.0, 200.0, 100.0]);
    }

    #[test]
    fn pruning_deletes_what_is_older_in_batches_and_keeps_the_rest() {
        let _guard = TEST_LOCK.lock().unwrap();
        use_temp_home("prune");
        ensure_db().unwrap();
        {
            let guard = HISTORY_DB.lock().unwrap();
            let conn = guard.as_ref().unwrap();
            for i in 0..25 {
                record_in(conn, &run(&format!("SELECT {i}")), 1000.0 + i as f64).unwrap();
            }
        }
        assert_eq!(prune_older_than(1020.0, 7).unwrap(), 20, "four batches of 7, 7, 6 and none left");
        let left = search(&HistoryFilter::default(), None, 100).unwrap();
        assert_eq!(left.len(), 5);
        assert!(left.iter().all(|r| r.timestamp >= 1020.0));
        assert_eq!(prune_older_than(0.0, 7).unwrap(), 0);
    }

    #[test]
    fn delete_matching_removes_exactly_what_the_filter_shows() {
        let _guard = TEST_LOCK.lock().unwrap();
        use_temp_home("delete-matching");
        ensure_db().unwrap();
        {
            let guard = HISTORY_DB.lock().unwrap();
            let conn = guard.as_ref().unwrap();
            record_in(conn, &run("SELECT 1"), 10.0).unwrap();
            record_in(conn, &Record { error: Some("x"), ..run("SELECT 2") }, 20.0).unwrap();
            record_in(conn, &run("DELETE FROM t"), 30.0).unwrap();
        }
        let failed = HistoryFilter { status: StatusFilter::Failed, ..Default::default() };
        assert_eq!(delete_matching(&failed).unwrap(), 1);
        assert_eq!(count(&HistoryFilter::default()).unwrap(), 2);
        assert_eq!(scope_options().unwrap(), vec![("c1".to_string(), vec!["app".to_string()])]);
    }

    /// 100 000 rows in a file database: how long the search takes, per kind of search. Run with
    /// `cargo test -p engine-storage --release -- --ignored --nocapture history_search_bench`.
    #[test]
    #[ignore = "a measurement, not a check"]
    fn history_search_bench() {
        use std::time::Instant;
        let dir = tempfile::tempdir().unwrap();
        let mut conn = app_db::open(&dir.path().join("tablex.db")).unwrap();
        let tx = conn.transaction().unwrap();
        {
            let mut insert = tx
                .prepare(
                    "INSERT INTO query_history (conn_uuid, database, collection, filter, timestamp, status, error, duration_ms, kind, result_count)
                     VALUES (?1, ?2, '', ?3, ?4, ?5, ?6, ?7, ?8, ?9)",
                )
                .unwrap();
            for i in 0..100_000i64 {
                let sql = match i % 4 {
                    0 => format!("SELECT id, name, created_at FROM customers WHERE region = 'r{}' AND total > {} ORDER BY created_at DESC LIMIT 100", i % 97, i),
                    1 => format!("UPDATE orders SET status = 'shipped' WHERE id = {i}"),
                    2 => format!("SELECT o.id, c.name FROM orders o JOIN customers c ON c.id = o.customer_id WHERE o.total > {i}"),
                    _ => format!("INSERT INTO audit_log (event, ref) VALUES ('e{}', {i})", i % 13),
                };
                let failed = i % 10 == 0;
                insert
                    .execute(params![
                        format!("conn-{}", i % 5),
                        format!("db{}", i % 3),
                        sql,
                        1_700_000_000.0 + i as f64 * 30.0,
                        if failed { STATUS_FAILED } else { STATUS_OK },
                        if failed { Some("relation does not exist") } else { None },
                        i % 900,
                        classify_sql(&sql),
                        i % 200
                    ])
                    .unwrap();
            }
        }
        tx.commit().unwrap();
        let time = |label: &str, filter: HistoryFilter| {
            let started = Instant::now();
            let rows = search_in(&conn, &filter, None, 200).unwrap();
            let first = started.elapsed();
            let started = Instant::now();
            let n = rows.len();
            let last = rows.last().map(|r| Cursor { timestamp: r.timestamp, id: r.id });
            let more = search_in(&conn, &filter, last, 200).unwrap().len();
            eprintln!("[bench] {label:<34} first page ({n}) {first:>9.2?}   next page ({more}) {:>9.2?}", started.elapsed());
        };
        time("no filter", HistoryFilter::default());
        time("text common word (shipped)", HistoryFilter { text: "shipped".into(), ..Default::default() });
        time("text rare word, one match", HistoryFilter { text: "e12 99999".into(), ..Default::default() });
        time("text no match (full scan)", HistoryFilter { text: "zzz_nothing".into(), ..Default::default() });
        time("text no match, 3 words", HistoryFilter { text: "zzz yyy xxx".into(), ..Default::default() });
        time("status failed", HistoryFilter { status: StatusFilter::Failed, ..Default::default() });
        time("kind data_change", HistoryFilter { kind: Some(KIND_DATA_CHANGE.into()), ..Default::default() });
        time(
            "scope conn-2 / db1",
            HistoryFilter { scopes: vec![Scope { conn_uuid: "conn-2".into(), databases: vec!["db1".into()] }], ..Default::default() },
        );
        time("since 7 days of the newest", HistoryFilter { since: Some(1_700_000_000.0 + 99_000.0 * 30.0), ..Default::default() });
        let started = Instant::now();
        let n: i64 = conn.query_row("SELECT COUNT(*) FROM query_history WHERE filter LIKE '%zzz_nothing%' ESCAPE '\\'", [], |r| r.get(0)).unwrap();
        eprintln!("[bench] raw LIKE scan, no match ({n})            {:.2?}", started.elapsed());
        let has_fts5: bool = conn.query_row("SELECT sqlite_compileoption_used('ENABLE_FTS5')", [], |r| r.get(0)).unwrap();
        eprintln!("[bench] bundled SQLite {} FTS5: {has_fts5}", rusqlite::version());
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
