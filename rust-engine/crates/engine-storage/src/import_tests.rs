//! Moving the old per-store files into `tablex.db` (see `crate::legacy`): a fresh install, an
//! import of every schema the old code accepted, no second import, old files untouched, a failing
//! import that changes nothing, and several openers at once.

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};

use rusqlite::Connection;

use crate::app_db::{self, AppDbError};
use crate::{aggregate, connection, documents, history, legacy, view_state};

struct Fixture {
    dir: tempfile::TempDir,
}

impl Fixture {
    fn new() -> Self {
        Self {
            dir: tempfile::tempdir().unwrap(),
        }
    }

    /// `<dir>/tablex/tablex.db`, the app database.
    fn db(&self) -> PathBuf {
        self.dir.path().join("tablex").join(app_db::FILE_NAME)
    }

    /// An old SQLite file `name` made by running `sql`.
    fn old(&self, name: &str, sql: &str) -> PathBuf {
        let path = self.dir.path().join(name);
        Connection::open(&path).unwrap().execute_batch(sql).unwrap();
        path
    }

    /// Everything under the fixture directory, with its bytes: what "unchanged" compares.
    fn snapshot(&self) -> Vec<(PathBuf, Vec<u8>)> {
        fn walk(dir: &Path, out: &mut Vec<(PathBuf, Vec<u8>)>) {
            let mut entries: Vec<_> = std::fs::read_dir(dir).unwrap().map(|e| e.unwrap().path()).collect();
            entries.sort();
            for path in entries {
                if path.is_dir() {
                    walk(&path, out);
                } else {
                    out.push((path.clone(), std::fs::read(&path).unwrap()));
                }
            }
        }
        let mut out = Vec::new();
        walk(self.dir.path(), &mut out);
        out
    }

    /// The old files (not the app database) as they are now.
    fn old_files(&self) -> Vec<(PathBuf, Vec<u8>)> {
        let db = self.db();
        let wal = PathBuf::from(format!("{}-wal", db.display()));
        let shm = PathBuf::from(format!("{}-shm", db.display()));
        self.snapshot()
            .into_iter()
            .filter(|(path, _)| *path != db && *path != wal && *path != shm)
            .collect()
    }
}

fn markers(conn: &Connection) -> Vec<String> {
    let mut stmt = conn.prepare("SELECT name FROM imports ORDER BY name").unwrap();
    stmt.query_map([], |row| row.get(0)).unwrap().map(Result::unwrap).collect()
}

fn count(conn: &Connection, table: &str) -> i64 {
    conn.query_row(&format!("SELECT COUNT(*) FROM {table}"), [], |row| row.get(0))
        .unwrap()
}

fn garbage(fixture: &Fixture, name: &str) -> PathBuf {
    let path = fixture.dir.path().join(name);
    std::fs::write(&path, vec![0xA5u8; 8192]).unwrap();
    path
}

/// The shared checks of a failed import: it names the file, nothing was written, the old file is
/// as it was, and the next attempt works once the file is fixed.
fn assert_failed_import(error: &AppDbError, file: &Path) {
    assert!(matches!(error, AppDbError::Import { file: named, .. } if named == file), "{error:?}");
    let message = error.to_string();
    assert!(message.contains(&file.display().to_string()), "{message}");
}

// ---- connections ----------------------------------------------------------------------------

const OLD_CONNECTIONS: &str = "
    CREATE TABLE saved_connections (
        id TEXT PRIMARY KEY, name TEXT NOT NULL, uri TEXT NOT NULL, db_type TEXT NOT NULL,
        color_hex TEXT NOT NULL, save_password INTEGER NOT NULL, save_proxy_password INTEGER NOT NULL,
        network_settings_json TEXT NOT NULL, is_pinned INTEGER NOT NULL DEFAULT 0, last_connected REAL,
        created_at REAL NOT NULL, updated_at REAL NOT NULL);
    INSERT INTO saved_connections VALUES
        ('old', 'Old', 'sqlite:///tmp/a.db', 'sqlite', '#34C759', 0, 0, '{}', 1, 5.5, 1.0, 2.0),
        ('two', 'Two', 'postgres://h/db', 'postgres', '#FF0000', 1, 0, '{\"ssh\":1}', 0, NULL, 3.0, 4.0);
    CREATE TABLE project_connection_memberships (
        project_id TEXT NOT NULL, connection_id TEXT NOT NULL, created_at REAL NOT NULL,
        PRIMARY KEY(project_id, connection_id));
    INSERT INTO project_connection_memberships VALUES ('Work', 'old', 9.0), ('Work', 'two', 10.0);
    CREATE TABLE projects (
        id TEXT PRIMARY KEY, name TEXT NOT NULL, path TEXT NOT NULL,
        uses_default_location INTEGER NOT NULL, created_at REAL NOT NULL, updated_at REAL NOT NULL);
    INSERT INTO projects VALUES ('Work', 'Work', '/tmp/work', 1, 7.0, 8.0);";

#[test]
fn connections_import_every_row_of_a_file_from_before_connection_types() {
    let fx = Fixture::new();
    let legacy = fx.old("connections.db", OLD_CONNECTIONS);
    let before = fx.old_files();
    let conn = connection::open_at(&fx.db(), &legacy).unwrap();
    let records = connection::list_connections_impl(&conn).unwrap();
    assert_eq!(records.iter().map(|r| r.id.as_str()).collect::<Vec<_>>(), ["old", "two"]);
    let old = &records[0];
    assert_eq!((old.name.as_str(), old.is_pinned, old.last_connected), ("Old", true, Some(5.5)));
    assert_eq!((old.created_at, old.updated_at), (1.0, 2.0));
    assert_eq!((old.connection_type.as_str(), old.overrides_json.as_str()), ("", ""));
    assert_eq!(records[1].network_settings_json, r#"{"ssh":1}"#);
    assert_eq!(count(&conn, "project_connection_memberships"), 2);
    assert_eq!(count(&conn, "projects"), 1);
    assert_eq!(markers(&conn), ["connections"]);
    assert_eq!(fx.old_files(), before, "the old file and its folder are byte for byte as they were");
}

#[test]
fn connections_import_a_current_file_with_its_type_and_overrides() {
    let fx = Fixture::new();
    // How the old code migrated its own file.
    let legacy = fx.old(
        "connections.db",
        &format!(
            "{OLD_CONNECTIONS}
             ALTER TABLE saved_connections ADD COLUMN connection_type TEXT NOT NULL DEFAULT '';
             ALTER TABLE saved_connections ADD COLUMN overrides_json TEXT NOT NULL DEFAULT '';
             UPDATE saved_connections SET connection_type = 'production', overrides_json = '{{\"a\":1}}' WHERE id = 'old';"
        ),
    );
    let conn = connection::open_at(&fx.db(), &legacy).unwrap();
    let records = connection::list_connections_impl(&conn).unwrap();
    assert_eq!(records.len(), 2);
    assert_eq!(records[0].connection_type, "production");
    assert_eq!(records[0].overrides_json, r#"{"a":1}"#);
}

#[test]
fn a_fresh_install_has_nothing_to_import_and_does_not_look_again() {
    let fx = Fixture::new();
    let legacy = fx.dir.path().join("connections.db");
    let conn = connection::open_at(&fx.db(), &legacy).unwrap();
    assert_eq!(count(&conn, "saved_connections"), 0);
    assert_eq!(markers(&conn), ["connections"]);
    drop(conn);
    // An old file that shows up later is not imported: the decision was made once.
    fx.old("connections.db", OLD_CONNECTIONS);
    let conn = connection::open_at(&fx.db(), &legacy).unwrap();
    assert_eq!(count(&conn, "saved_connections"), 0);
}

#[test]
fn the_second_start_does_not_import_again_and_later_changes_stay() {
    let fx = Fixture::new();
    let legacy = fx.old("connections.db", OLD_CONNECTIONS);
    let conn = connection::open_at(&fx.db(), &legacy).unwrap();
    conn.execute("DELETE FROM saved_connections WHERE id = 'two'", []).unwrap();
    conn.execute("UPDATE saved_connections SET name = 'Renamed' WHERE id = 'old'", []).unwrap();
    drop(conn);
    let conn = connection::open_at(&fx.db(), &legacy).unwrap();
    let records = connection::list_connections_impl(&conn).unwrap();
    assert_eq!(records.len(), 1);
    assert_eq!(records[0].name, "Renamed");
}

#[test]
fn a_corrupt_old_connection_file_fails_by_name_and_changes_nothing_then_works_once_fixed() {
    let fx = Fixture::new();
    let legacy = garbage(&fx, "connections.db");
    let before = fx.old_files();
    let error = connection::open_at(&fx.db(), &legacy).unwrap_err();
    assert_failed_import(&error, &legacy);
    assert_eq!(fx.old_files(), before);
    let conn = app_db::open(&fx.db()).unwrap();
    assert!(markers(&conn).is_empty(), "no marker after a failed import");
    assert_eq!(count(&conn, "saved_connections"), 0);
    drop(conn);
    // The old file stays authoritative: once it reads, the next start imports it.
    std::fs::remove_file(&legacy).unwrap();
    fx.old("connections.db", OLD_CONNECTIONS);
    let conn = connection::open_at(&fx.db(), &legacy).unwrap();
    assert_eq!(count(&conn, "saved_connections"), 2);
}

#[test]
fn a_half_failed_import_rolls_every_row_back() {
    let fx = Fixture::new();
    // The last row cannot be inserted: this old file had no key, so an id is there twice.
    let legacy = fx.old(
        "connections.db",
        "CREATE TABLE saved_connections (id, name, uri, db_type, color_hex, save_password,
            save_proxy_password, network_settings_json, is_pinned, last_connected, created_at, updated_at);
         INSERT INTO saved_connections VALUES ('ok','n','u','t','c',0,0,'{}',0,NULL,1,1);
         INSERT INTO saved_connections VALUES ('twice','n','u','t','c',0,0,'{}',0,NULL,1,1);
         INSERT INTO saved_connections VALUES ('twice','m','u','t','c',0,0,'{}',0,NULL,1,1);",
    );
    let error = connection::open_at(&fx.db(), &legacy).unwrap_err();
    assert_failed_import(&error, &legacy);
    let conn = app_db::open(&fx.db()).unwrap();
    assert_eq!(count(&conn, "saved_connections"), 0, "the good row did not stay");
    assert!(markers(&conn).is_empty());
}

#[test]
fn openers_at_the_same_moment_import_exactly_once() {
    let fx = Fixture::new();
    let legacy = fx.old("connections.db", OLD_CONNECTIONS);
    let db = fx.db();
    app_db::open(&db).unwrap();
    let runs = AtomicUsize::new(0);
    std::thread::scope(|scope| {
        for _ in 0..8 {
            scope.spawn(|| {
                let mut conn = app_db::open(&db).unwrap();
                legacy::import_once(&mut conn, &db, "connections", &legacy, |tx| {
                    runs.fetch_add(1, Ordering::SeqCst);
                    std::thread::sleep(std::time::Duration::from_millis(20));
                    tx.execute("INSERT INTO saved_connections VALUES ('x','n','u','t','c',0,0,'{}',0,NULL,1,1,'','')", [])
                        .map(drop)
                        .map_err(|error| legacy::ImportError::new(&legacy, error))
                })
                .unwrap();
            });
        }
    });
    assert_eq!(runs.load(Ordering::SeqCst), 1);
    assert_eq!(count(&app_db::open(&db).unwrap(), "saved_connections"), 1);
}

const CHILD_ENV: &str = "TABLEX_IMPORT_TEST_CHILD";

/// Runs only as a child of `processes_starting_together_import_once`: opens the connection store
/// (importing, or finding it imported) and then deletes every row, as a user might.
#[test]
fn import_child() {
    let Some(dir) = std::env::var_os(CHILD_ENV) else {
        return;
    };
    let dir = PathBuf::from(dir);
    let conn = connection::open_at(&dir.join("tablex").join(app_db::FILE_NAME), &dir.join("connections.db")).unwrap();
    conn.execute("DELETE FROM saved_connections", []).unwrap();
}

#[test]
fn processes_starting_together_import_once() {
    let fx = Fixture::new();
    fx.old("connections.db", OLD_CONNECTIONS);
    let exe = std::env::current_exe().unwrap();
    let children: Vec<_> = (0..4)
        .map(|_| {
            std::process::Command::new(&exe)
                .args(["--exact", "import_tests::import_child", "--test-threads=1"])
                .env(CHILD_ENV, fx.dir.path())
                .stdout(std::process::Stdio::null())
                .stderr(std::process::Stdio::null())
                .spawn()
                .unwrap()
        })
        .collect();
    let legacy = fx.dir.path().join("connections.db");
    let conn = connection::open_at(&fx.db(), &legacy).unwrap();
    conn.execute("DELETE FROM saved_connections", []).unwrap();
    for mut child in children {
        assert!(child.wait().unwrap().success());
    }
    // Every opener deleted what it found; a second import after any of those deletes would have
    // brought the two rows back.
    assert_eq!(count(&conn, "saved_connections"), 0);
    assert_eq!(markers(&conn), ["connections"]);
}

// ---- every store: a fresh install, and a second start ------------------------------------------

#[test]
fn history_aggregates_and_view_state_start_empty_on_a_fresh_install_and_import_once() {
    // (store name, old file, schema + two rows, table, how to open it)
    type Open = fn(&Path, &Path) -> Result<Connection, AppDbError>;
    let cases: [(&str, &str, &str, &str, Open); 3] = [
        (
            "history",
            "history.db",
            "CREATE TABLE query_history (id INTEGER PRIMARY KEY AUTOINCREMENT, conn_uuid TEXT NOT NULL,
                database TEXT NOT NULL, collection TEXT NOT NULL, filter TEXT DEFAULT '', project TEXT DEFAULT '',
                sort TEXT DEFAULT '', skip INTEGER DEFAULT 0, \"limit\" INTEGER DEFAULT 0,
                max_time_ms INTEGER DEFAULT 0, result_count INTEGER DEFAULT 0, timestamp REAL NOT NULL);
             INSERT INTO query_history (conn_uuid, database, collection, filter, timestamp) VALUES ('c','d','x','a',1.0), ('c','d','x','b',2.0);",
            "query_history",
            history::open_at,
        ),
        (
            "aggregates",
            "aggregates.db",
            "CREATE TABLE saved_aggregates (id TEXT PRIMARY KEY, conn_uuid TEXT NOT NULL, database TEXT NOT NULL,
                collection TEXT NOT NULL DEFAULT '', name TEXT NOT NULL, kind TEXT NOT NULL DEFAULT 'aggregate',
                pipeline_json TEXT NOT NULL, created_at REAL NOT NULL, updated_at REAL NOT NULL, options_json TEXT);
             INSERT INTO saved_aggregates VALUES ('a','c','d','x','A','aggregate','[]',1.0,1.0,NULL), ('b','c','d','x','B','aggregate','[]',1.0,1.0,NULL);",
            "saved_aggregates",
            aggregate::open_at,
        ),
        (
            "view_state",
            "view_state.db",
            "CREATE TABLE connection_view_state (conn_uuid TEXT PRIMARY KEY, state_json TEXT NOT NULL, updated_at REAL NOT NULL);
             INSERT INTO connection_view_state VALUES ('a','{}',1.0), ('b','{}',2.0);",
            "connection_view_state",
            view_state::open_at,
        ),
    ];
    for (name, file, schema, table, open) in cases {
        // Fresh install: nothing to read, a marker, and an old file that appears later is not read.
        let fresh = Fixture::new();
        let legacy = fresh.dir.path().join(file);
        let conn = open(&fresh.db(), &legacy).unwrap();
        assert_eq!((count(&conn, table), markers(&conn)), (0, vec![name.to_owned()]), "{name}");
        drop(conn);
        fresh.old(file, schema);
        assert_eq!(count(&open(&fresh.db(), &legacy).unwrap(), table), 0, "{name}: decided once");

        // An old file: both rows come over; a row deleted afterwards does not come back.
        let fx = Fixture::new();
        let legacy = fx.old(file, schema);
        let conn = open(&fx.db(), &legacy).unwrap();
        assert_eq!(count(&conn, table), 2, "{name}");
        conn.execute(&format!("DELETE FROM {table}"), []).unwrap();
        drop(conn);
        assert_eq!(count(&open(&fx.db(), &legacy).unwrap(), table), 0, "{name}: not imported twice");
    }
}

// ---- history --------------------------------------------------------------------------------

#[test]
fn history_imports_with_ids_and_keeps_the_newer_of_two_duplicates() {
    let fx = Fixture::new();
    // An old file from before the unique index: two rows are the same query.
    let legacy = fx.old(
        "history.db",
        "CREATE TABLE query_history (
            id INTEGER PRIMARY KEY AUTOINCREMENT, conn_uuid TEXT NOT NULL, database TEXT NOT NULL,
            collection TEXT NOT NULL, filter TEXT DEFAULT '', project TEXT DEFAULT '', sort TEXT DEFAULT '',
            skip INTEGER DEFAULT 0, \"limit\" INTEGER DEFAULT 0, max_time_ms INTEGER DEFAULT 0,
            result_count INTEGER DEFAULT 0, timestamp REAL NOT NULL);
         INSERT INTO query_history VALUES (1,'c','d','coll','{}','','',0,10,0,5,100.0);
         INSERT INTO query_history VALUES (2,'c','d','coll','{\"a\":1}','','',0,10,0,6,200.0);
         INSERT INTO query_history VALUES (3,'c','d','coll','{}','','',0,10,0,7,300.0);",
    );
    let before = fx.old_files();
    let conn = history::open_at(&fx.db(), &legacy).unwrap();
    let rows: Vec<(i64, i64, f64)> = conn
        .prepare("SELECT id, result_count, timestamp FROM query_history ORDER BY id")
        .unwrap()
        .query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))
        .unwrap()
        .map(Result::unwrap)
        .collect();
    assert_eq!(rows, [(2, 6, 200.0), (3, 7, 300.0)]);
    assert_eq!(markers(&conn), ["history"]);
    assert_eq!(fx.old_files(), before);
    // New entries continue after the highest imported id.
    conn.execute(
        "INSERT INTO query_history (conn_uuid, database, collection, timestamp) VALUES ('c','d','x',1.0)",
        [],
    )
    .unwrap();
    assert_eq!(conn.last_insert_rowid(), 4);
}

#[test]
fn a_history_file_from_before_per_connection_history_has_nothing_usable_and_is_left_alone() {
    let fx = Fixture::new();
    let legacy = fx.old(
        "history.db",
        "CREATE TABLE query_history (id INTEGER PRIMARY KEY, database TEXT, collection TEXT, timestamp REAL);
         INSERT INTO query_history VALUES (1, 'd', 'c', 1.0);",
    );
    let before = fx.old_files();
    let conn = history::open_at(&fx.db(), &legacy).unwrap();
    assert_eq!(count(&conn, "query_history"), 0);
    assert_eq!(markers(&conn), ["history"]);
    assert_eq!(fx.old_files(), before);
}

#[test]
fn a_corrupt_history_file_fails_by_name() {
    let fx = Fixture::new();
    let legacy = garbage(&fx, "history.db");
    let error = history::open_at(&fx.db(), &legacy).unwrap_err();
    assert_failed_import(&error, &legacy);
    assert!(markers(&app_db::open(&fx.db()).unwrap()).is_empty());
}

// ---- aggregates -----------------------------------------------------------------------------

#[test]
fn aggregates_import_from_every_schema_the_old_code_accepted() {
    let oldest = "CREATE TABLE saved_aggregates (
            id TEXT PRIMARY KEY, conn_uuid TEXT NOT NULL, database TEXT NOT NULL,
            collection TEXT NOT NULL DEFAULT '', name TEXT NOT NULL, pipeline_json TEXT NOT NULL,
            created_at REAL NOT NULL, updated_at REAL NOT NULL);
         CREATE INDEX idx_saved_agg_lookup ON saved_aggregates(conn_uuid);
         INSERT INTO saved_aggregates VALUES ('a1','c','d','coll','One','[1]',1.0,2.0);";
    let middle = "CREATE TABLE saved_aggregates (
            id TEXT PRIMARY KEY, conn_uuid TEXT NOT NULL, database TEXT NOT NULL,
            collection TEXT NOT NULL DEFAULT '', name TEXT NOT NULL, kind TEXT NOT NULL DEFAULT 'aggregate',
            pipeline_json TEXT NOT NULL, created_at REAL NOT NULL, updated_at REAL NOT NULL);
         INSERT INTO saved_aggregates VALUES ('a1','c','d','coll','One','view','[1]',1.0,2.0);";
    let current = "CREATE TABLE saved_aggregates (
            id TEXT PRIMARY KEY, conn_uuid TEXT NOT NULL, database TEXT NOT NULL,
            collection TEXT NOT NULL DEFAULT '', name TEXT NOT NULL, kind TEXT NOT NULL DEFAULT 'aggregate',
            pipeline_json TEXT NOT NULL, created_at REAL NOT NULL, updated_at REAL NOT NULL, options_json TEXT);
         INSERT INTO saved_aggregates VALUES ('a1','c','d','coll','One','view','[1]',1.0,2.0,'{\"o\":1}');";
    for (schema, kind, options) in [
        (oldest, "aggregate", None),
        (middle, "view", None),
        (current, "view", Some(r#"{"o":1}"#)),
    ] {
        let fx = Fixture::new();
        let legacy = fx.old("aggregates.db", &format!("{schema}INSERT INTO saved_aggregates (id, conn_uuid, database, collection, name, pipeline_json, created_at, updated_at) VALUES ('a2','c','d2','','Two','[2]',3.0,4.0);"));
        let before = fx.old_files();
        let conn = aggregate::open_at(&fx.db(), &legacy).unwrap();
        let one = aggregate::get_aggregate_impl(&conn, "a1").unwrap().unwrap();
        assert_eq!(
            (one.kind.as_str(), one.options_json.as_deref(), one.pipeline_json.as_str()),
            (kind, options, "[1]")
        );
        assert_eq!((one.created_at, one.updated_at), (1.0, 2.0));
        let two = aggregate::get_aggregate_impl(&conn, "a2").unwrap().unwrap();
        assert_eq!((two.database.as_str(), two.collection.as_str(), two.name.as_str()), ("d2", "", "Two"));
        assert_eq!(markers(&conn), ["aggregates"]);
        assert_eq!(fx.old_files(), before);
    }
}

#[test]
fn a_corrupt_aggregates_file_fails_by_name() {
    let fx = Fixture::new();
    let legacy = garbage(&fx, "aggregates.db");
    let error = aggregate::open_at(&fx.db(), &legacy).unwrap_err();
    assert_failed_import(&error, &legacy);
    assert!(markers(&app_db::open(&fx.db()).unwrap()).is_empty());
}

// ---- view state -----------------------------------------------------------------------------

#[test]
fn view_state_imports_and_a_corrupt_file_fails_by_name() {
    let fx = Fixture::new();
    let legacy = fx.old(
        "view_state.db",
        "CREATE TABLE connection_view_state (conn_uuid TEXT PRIMARY KEY, state_json TEXT NOT NULL, updated_at REAL NOT NULL);
         CREATE INDEX idx_view_state_updated ON connection_view_state(updated_at DESC);
         INSERT INTO connection_view_state VALUES ('c1', '{\"selected\":\"users\"}', 5.0), ('c2', '{}', 6.0);",
    );
    let before = fx.old_files();
    let conn = view_state::open_at(&fx.db(), &legacy).unwrap();
    let json: String = conn
        .query_row("SELECT state_json FROM connection_view_state WHERE conn_uuid = 'c1'", [], |r| r.get(0))
        .unwrap();
    assert_eq!(json, r#"{"selected":"users"}"#);
    assert_eq!(count(&conn, "connection_view_state"), 2);
    assert_eq!(markers(&conn), ["view_state"]);
    assert_eq!(fx.old_files(), before);

    let other = Fixture::new();
    let bad = garbage(&other, "view_state.db");
    let error = view_state::open_at(&other.db(), &bad).unwrap_err();
    assert_failed_import(&error, &bad);
    assert!(markers(&app_db::open(&other.db()).unwrap()).is_empty());
}

// ---- documents ------------------------------------------------------------------------------

fn write(fx: &Fixture, relative: &str, text: &str) -> PathBuf {
    let path = fx.dir.path().join("tablex").join(relative);
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    std::fs::write(&path, text).unwrap();
    path
}

#[test]
fn the_session_and_the_update_check_are_imported_as_they_are_and_never_again() {
    let fx = Fixture::new();
    // A damaged session is copied too: the app reports it where it reads it, as it did the file.
    write(&fx, "gpui-workspace-session.json", r#"{"version":1,"tabs":[{"kind":"query"}]}"#);
    write(&fx, "update-check.json", r#"{"last_check":42}"#);
    let before = fx.old_files();
    let db = fx.db();
    assert_eq!(
        documents::get(&db, documents::SESSION, documents::SESSION_NAME).unwrap().as_deref(),
        Some(r#"{"version":1,"tabs":[{"kind":"query"}]}"#)
    );
    assert_eq!(
        documents::get(&db, documents::UPDATE_CHECK, documents::UPDATE_CHECK_NAME).unwrap().as_deref(),
        Some(r#"{"last_check":42}"#)
    );
    assert_eq!(fx.old_files(), before);
    // Deleted in the database: not brought back from the old file.
    assert!(documents::delete(&db, documents::SESSION, documents::SESSION_NAME).unwrap());
    assert_eq!(documents::get(&db, documents::SESSION, documents::SESSION_NAME).unwrap(), None);
    // New content wins over, and survives, the old file.
    documents::put(&db, documents::UPDATE_CHECK, documents::UPDATE_CHECK_NAME, r#"{"last_check":99}"#).unwrap();
    assert_eq!(
        documents::get(&db, documents::UPDATE_CHECK, documents::UPDATE_CHECK_NAME).unwrap().as_deref(),
        Some(r#"{"last_check":99}"#)
    );
}

#[test]
fn a_fresh_install_has_no_session_and_a_database_of_another_name_has_no_old_files() {
    let fx = Fixture::new();
    assert_eq!(documents::get(&fx.db(), documents::SESSION, documents::SESSION_NAME).unwrap(), None);
    // A database a test puts elsewhere ignores old files beside it.
    let other = fx.dir.path().join("elsewhere").join("session.sqlite");
    std::fs::create_dir_all(other.parent().unwrap()).unwrap();
    std::fs::write(other.parent().unwrap().join("gpui-workspace-session.json"), "{}").unwrap();
    assert_eq!(documents::get(&other, documents::SESSION, documents::SESSION_NAME).unwrap(), None);
}

#[test]
fn diagrams_import_by_file_name_and_skip_what_is_not_a_diagram() {
    let fx = Fixture::new();
    write(&fx, "diagrams/0123456789abcdef01234567.json", r#"{"key":{"connection":"c"}}"#);
    write(&fx, "diagrams/aaaaaaaaaaaaaaaaaaaaaaaa.json", "{ not json");
    write(&fx, "diagrams/.diagram-1234.tmp", "left by a crash");
    write(&fx, "diagrams/notes.txt", "not a diagram");
    write(&fx, "diagrams/67541154-C0B6-4750-9E19-2C62133C8A51__devdb__current-table.json", "{\"swift\":true}");
    let before = fx.old_files();
    let db = fx.db();
    assert_eq!(documents::count(&db, documents::DIAGRAM).unwrap(), 2);
    assert_eq!(
        documents::get(&db, documents::DIAGRAM, "0123456789abcdef01234567").unwrap().as_deref(),
        Some(r#"{"key":{"connection":"c"}}"#)
    );
    assert_eq!(
        documents::get(&db, documents::DIAGRAM, "aaaaaaaaaaaaaaaaaaaaaaaa").unwrap().as_deref(),
        Some("{ not json"),
        "a damaged one is copied as it is"
    );
    assert_eq!(fx.old_files(), before);
    documents::delete(&db, documents::DIAGRAM, "aaaaaaaaaaaaaaaaaaaaaaaa").unwrap();
    assert_eq!(documents::count(&db, documents::DIAGRAM).unwrap(), 1, "not imported twice");
}

#[cfg(unix)]
#[test]
fn a_diagram_file_that_cannot_be_read_is_skipped_by_name_and_tried_again_at_the_next_start() {
    use std::os::unix::fs::PermissionsExt;
    let fx = Fixture::new();
    write(&fx, "diagrams/0123456789abcdef01234567.json", "{}");
    let locked = write(&fx, "diagrams/ffffffffffffffffffffffff.json", "{\"later\":true}");
    std::fs::set_permissions(&locked, std::fs::Permissions::from_mode(0o000)).unwrap();
    // Running as a user that can read it anyway (root) cannot show the failure.
    if std::fs::read(&locked).is_ok() {
        return;
    }
    let db = fx.db();
    // The readable one is there; the other is named in a warning and is not an error.
    assert_eq!(documents::count(&db, documents::DIAGRAM).unwrap(), 1);
    let warned = legacy::warnings_snapshot();
    assert!(warned.iter().any(|w| w.contains(&locked.display().to_string())), "{warned:?}");
    let conn = app_db::open(&db).unwrap();
    assert!(markers(&conn).is_empty(), "no marker: the next start tries again");
    // This process does not try again; the next one does, and gets it.
    std::fs::set_permissions(&locked, std::fs::Permissions::from_mode(0o600)).unwrap();
    assert_eq!(documents::count(&db, documents::DIAGRAM).unwrap(), 1);
    legacy::forget_process_state();
    assert_eq!(documents::count(&db, documents::DIAGRAM).unwrap(), 2);
    assert_eq!(markers(&conn), ["diagrams"]);
}

fn conversation_file(id: &str, connection: &str, updated: u64, seq: u64) -> String {
    format!(
        "{{\"format\":1,\"id\":\"{id}\",\"title\":\"t {id}\",\"created\":1,\"updated\":{updated},\
         \"provider\":\"p\",\"model\":\"m\",\"access\":\"read\",\"seq\":{seq},\n\
         \"conversation\":{{\"scope\":\"{connection}\"}}}}"
    )
}

#[test]
fn conversations_import_by_connection_and_id_and_skip_junk() {
    let fx = Fixture::new();
    let a = conversation_file("one", "c1", 100, 2);
    write(&fx, "assistant/conversations/c1/one.json", &a);
    write(&fx, "assistant/conversations/C1/two.json", &conversation_file("two", "c1", 200, 4));
    write(&fx, "assistant/conversations/c2/three.json", &conversation_file("three", "c2", 50, 0));
    write(&fx, "assistant/conversations/c1/.one.1-2.tmp", "left over");
    write(&fx, "assistant/conversations/c1/broken.json", "not a conversation");
    write(&fx, "assistant/conversations/c1/other.json", &conversation_file("different", "c1", 1, 1));
    write(&fx, "assistant/conversations/c1/not an id.json", &conversation_file("x", "c1", 1, 1));
    write(&fx, "assistant/conversations/readme.txt", "junk beside the folders");
    let before = fx.old_files();
    let db = fx.db();
    let rows = documents::list_conversations(&db, "c1", 50).unwrap();
    assert_eq!(rows.iter().map(|r| r.id.as_str()).collect::<Vec<_>>(), ["two", "one"], "newest first");
    assert_eq!(rows[1].size, a.len() as u64);
    assert!(rows[1].header.starts_with("{\"format\":1,\"id\":\"one\""));
    assert_eq!(documents::list_conversations(&db, "c2", 50).unwrap().len(), 1);
    let body = documents::load_conversation(&db, "c1", "one", usize::MAX).unwrap().unwrap().unwrap();
    assert_eq!(body, a, "the body is the file");
    assert_eq!(fx.old_files(), before);
    // Written afterwards: replaced, and the old file does not come back over it.
    let doc = documents::ConversationDoc { connection: "c1", id: "one", updated: 300, seq: 6, header: "{}", body: "{}" };
    assert_eq!(documents::put_conversation(&db, &doc, 50).unwrap(), documents::PutConversation::Written { pruned: 0 });
    assert_eq!(documents::list_conversations(&db, "c1", 50).unwrap()[0].id, "one");
}

#[test]
fn a_conversation_file_over_the_limit_is_not_imported() {
    let fx = Fixture::new();
    let mut big = conversation_file("big", "c1", 1, 1);
    big.push_str(&" ".repeat(documents::MAX_CONVERSATION_BYTES));
    write(&fx, "assistant/conversations/c1/big.json", &big);
    write(&fx, "assistant/conversations/c1/small.json", &conversation_file("small", "c1", 1, 1));
    let rows = documents::list_conversations(&fx.db(), "c1", 50).unwrap();
    assert_eq!(rows.iter().map(|r| r.id.as_str()).collect::<Vec<_>>(), ["small"]);
}

// ---- a failed import never leaves a store empty ----------------------------------------------

/// A table the database cannot insert into: every import into it fails (a stand-in for a full
/// disk or a lock that does not go away).
fn refuse_rows(db: &Path, table: &str) {
    app_db::open(db)
        .unwrap()
        .execute_batch(&format!(
            "CREATE TRIGGER no_rows BEFORE INSERT ON {table} BEGIN SELECT RAISE(ABORT, 'disk full'); END"
        ))
        .unwrap();
}

#[test]
fn every_store_whose_import_fails_works_on_its_old_file_and_imports_at_the_next_start() {
    type Fallback = fn(&Path, &Path) -> rusqlite::Result<legacy::Store>;
    let cases: [(&str, &str, &str, &str, Fallback); 4] = [
        (
            "history.db",
            "query_history",
            "CREATE TABLE query_history (id INTEGER PRIMARY KEY AUTOINCREMENT, conn_uuid TEXT NOT NULL,
                database TEXT NOT NULL, collection TEXT NOT NULL, filter TEXT DEFAULT '', project TEXT DEFAULT '',
                sort TEXT DEFAULT '', skip INTEGER DEFAULT 0, \"limit\" INTEGER DEFAULT 0,
                max_time_ms INTEGER DEFAULT 0, result_count INTEGER DEFAULT 0, timestamp REAL NOT NULL);
             INSERT INTO query_history (conn_uuid, database, collection, filter, timestamp) VALUES ('c','d','x','a',1.0), ('c','d','x','b',2.0);",
            "INSERT INTO query_history (conn_uuid, database, collection, filter, timestamp) VALUES ('c','d','x','new',3.0)",
            history::open_or_fall_back,
        ),
        (
            "aggregates.db",
            "saved_aggregates",
            "CREATE TABLE saved_aggregates (id TEXT PRIMARY KEY, conn_uuid TEXT NOT NULL, database TEXT NOT NULL,
                collection TEXT NOT NULL DEFAULT '', name TEXT NOT NULL, kind TEXT NOT NULL DEFAULT 'aggregate',
                pipeline_json TEXT NOT NULL, created_at REAL NOT NULL, updated_at REAL NOT NULL, options_json TEXT);
             INSERT INTO saved_aggregates VALUES ('a','c','d','x','A','aggregate','[]',1.0,1.0,NULL), ('b','c','d','x','B','aggregate','[]',1.0,1.0,NULL);",
            "INSERT INTO saved_aggregates (id, conn_uuid, database, name, pipeline_json, created_at, updated_at) VALUES ('new','c','d','N','[]',1.0,1.0)",
            aggregate::open_or_fall_back,
        ),
        (
            "view_state.db",
            "connection_view_state",
            "CREATE TABLE connection_view_state (conn_uuid TEXT PRIMARY KEY, state_json TEXT NOT NULL, updated_at REAL NOT NULL);
             INSERT INTO connection_view_state VALUES ('a','{}',1.0), ('b','{}',2.0);",
            "INSERT INTO connection_view_state VALUES ('new','{}',3.0)",
            view_state::open_or_fall_back,
        ),
        (
            "connections.db",
            "saved_connections",
            // From before connection types: the fallback adds the columns, as the old code did.
            OLD_CONNECTIONS,
            "INSERT INTO saved_connections (id, name, uri, db_type, color_hex, save_password, save_proxy_password, network_settings_json, created_at, updated_at) VALUES ('new','N','u','sqlite','#fff',0,0,'{}',1.0,1.0)",
            connection::open_or_fall_back,
        ),
    ];
    for (file, table, schema, write_sql, fall_back) in &cases {
        let fx = Fixture::new();
        let legacy = fx.old(file, schema);
        let db = fx.db();
        refuse_rows(&db, table);
        let old_count = count(&Connection::open(&legacy).unwrap(), table);

        // The store comes up on the old file: everything is in it, and it can be written.
        let conn = fall_back(&db, &legacy).unwrap_or_else(|e| panic!("{file}: {e}"));
        assert_eq!(count(&conn, table), old_count, "{file}");
        assert!(conn.on_old_file(), "{file}");
        conn.execute(write_sql, []).unwrap_or_else(|e| panic!("{file}: {e}"));
        drop(conn);
        let warned = legacy::warnings_snapshot();
        assert!(
            warned.iter().any(|w| w.contains(&legacy.display().to_string()) && w.contains("disk full")),
            "{file}: a warning naming the file: {warned:?}"
        );
        let app = app_db::open(&db).unwrap();
        assert_eq!((count(&app, table), markers(&app).len()), (0, 0), "{file}: nothing half moved in");

        // The next start: the fault is gone, and what was written meanwhile comes with the rest.
        app.execute_batch("DROP TRIGGER no_rows").unwrap();
        drop(app);
        let conn = fall_back(&db, &legacy).unwrap();
        assert_eq!(count(&conn, table), old_count + 1, "{file}");
        assert_eq!(markers(&conn).len(), 1, "{file}");
    }
}

#[test]
fn a_failed_import_of_a_file_that_is_not_there_or_not_a_database_is_still_an_error() {
    // Nothing to fall back on: the old file is garbage (the previous version could not use it
    // either), so the error says so and nothing is created or changed.
    let fx = Fixture::new();
    let bad = garbage(&fx, "connections.db");
    let error = connection::open_or_fall_back(&fx.db(), &bad).unwrap_err();
    assert!(error.to_string().contains(&bad.display().to_string()), "{error}");
    assert_eq!(std::fs::read(&bad).unwrap(), vec![0xA5u8; 8192]);
    // No old file: an error of the database itself is not covered by an old file that is not there.
    let missing = fx.dir.path().join("history.db");
    refuse_rows(&fx.db(), "imports");
    assert!(history::open_or_fall_back(&fx.db(), &missing).is_err());
    assert!(!missing.exists(), "no old file is made up");
}

// ---- the old file written after it was moved in -----------------------------------------------

#[test]
fn an_old_file_written_after_the_import_is_not_imported_again_and_is_said_once() {
    let fx = Fixture::new();
    let legacy = fx.old("connections.db", OLD_CONNECTIONS);
    let db = fx.db();
    connection::open_at(&db, &legacy).unwrap();
    // The old file is as it was when moved in: nothing to say.
    connection::open_at(&db, &legacy).unwrap();
    let path = legacy.display().to_string();
    let said = |text: &str| legacy::warnings_snapshot().iter().filter(|w| w.contains(&path) && w.contains(text)).count();
    assert_eq!(said("changed after"), 0);

    // An older TableX runs (a downgrade, and a new upgrade after it) and adds a connection.
    legacy::forget_process_state();
    Connection::open(&legacy)
        .unwrap()
        .execute(
            "INSERT INTO saved_connections VALUES ('later','Later','u','sqlite','#fff',0,0,'{}',0,NULL,1.0,1.0)",
            [],
        )
        .unwrap();
    let file = std::fs::File::options().write(true).open(&legacy).unwrap();
    file.set_modified(std::time::SystemTime::now() + std::time::Duration::from_secs(3600)).unwrap();
    drop(file);
    let before = std::fs::read(&legacy).unwrap();
    let conn = connection::open_at(&db, &legacy).unwrap();
    connection::open_at(&db, &legacy).unwrap();
    // Said, once; the database is the truth: the connection written there is not brought in.
    assert_eq!(said("changed after"), 1);
    assert_eq!(count(&conn, "saved_connections"), 2);
    assert_eq!(std::fs::read(&legacy).unwrap(), before, "and the old file is left alone");
}

// ---- a check of real data ---------------------------------------------------------------------

/// Moves a COPY of real data into a database of a temporary directory and prints, per store, what
/// the old file held and what the database has. Nothing at the source is written (old SQLite files
/// are opened read-only, JSON files are only read) and the test fails if any file or folder there
/// differs afterwards.
///
/// ```text
/// TABLEX_IMPORT_CHECK_DIR=<copy of the data dir> \
///   cargo test -j 4 -p engine-storage import_check -- --ignored --nocapture
/// ```
///
/// `<dir>` holds `tablex/` (`connections.db`, `gpui-workspace-session.json`, `update-check.json`,
/// `diagrams/` or `Diagrams/`, `assistant/conversations/`) and `TableLeaf/` (`history.db`,
/// `aggregates.db`, `view_state.db`): what `~/Library/Application Support` holds. A folder that
/// has the files of `tablex/` itself is taken as that folder.
#[test]
#[ignore = "run by hand on a copy of real data: set TABLEX_IMPORT_CHECK_DIR"]
fn import_check_of_a_copy_of_real_data() {
    let source = PathBuf::from(
        std::env::var_os("TABLEX_IMPORT_CHECK_DIR").expect("set TABLEX_IMPORT_CHECK_DIR to a copy of the data dir"),
    );
    let app = if source.join("tablex").is_dir() { source.join("tablex") } else { source.clone() };
    let lib = if source.join("TableLeaf").is_dir() { source.join("TableLeaf") } else { source.clone() };
    let before = tree(&source);

    let work = tempfile::tempdir().unwrap();
    let db = work.path().join("tablex").join(app_db::FILE_NAME);
    let old_rows = |file: &Path, table: &str| -> String {
        if !file.exists() {
            return "no file".into();
        }
        match legacy::open_old(file) {
            Ok(old) => old
                .query_row(&format!("SELECT COUNT(*) FROM \"{table}\""), [], |row| row.get::<_, i64>(0))
                .map_or_else(|_| "no table".into(), |n| n.to_string()),
            Err(error) => format!("UNREADABLE ({})", error.message),
        }
    };
    let mut report: Vec<(String, String, String)> = Vec::new();
    let mut failures: Vec<String> = Vec::new();
    let mut check = |name: &str, old: String, new: Result<i64, String>, exact: bool| {
        let new_text = new.as_ref().map_or_else(|e| format!("FAILED: {e}"), |n| n.to_string());
        let same = match (&new, old.parse::<i64>()) {
            (Ok(new), Ok(old)) => if exact { *new == old } else { *new <= old },
            (Ok(new), Err(_)) => *new == 0 && matches!(old.as_str(), "no file" | "no table" | "no folder"),
            (Err(_), _) => false,
        };
        if !same {
            failures.push(format!("{name}: old {old}, new {new_text}"));
        }
        report.push((name.to_owned(), old, new_text));
    };

    let new_count = |table: &str| -> i64 {
        app_db::open(&db).unwrap().query_row(&format!("SELECT COUNT(*) FROM {table}"), [], |r| r.get(0)).unwrap()
    };
    let stores: [(&str, PathBuf, Vec<(&str, bool)>, Box<dyn Fn() -> Result<(), String>>); 4] = [
        (
            "connections",
            app.join("connections.db"),
            vec![("saved_connections", true), ("projects", true), ("project_connection_memberships", true)],
            Box::new({
                let (db, legacy) = (db.clone(), app.join("connections.db"));
                move || connection::open_at(&db, &legacy).map(drop).map_err(|e| e.to_string())
            }),
        ),
        (
            "history",
            lib.join("history.db"),
            // The old code deduplicated identical queries on every open: never more rows than it had.
            vec![("query_history", false)],
            Box::new({
                let (db, legacy) = (db.clone(), lib.join("history.db"));
                move || history::open_at(&db, &legacy).map(drop).map_err(|e| e.to_string())
            }),
        ),
        (
            "aggregates",
            lib.join("aggregates.db"),
            vec![("saved_aggregates", true)],
            Box::new({
                let (db, legacy) = (db.clone(), lib.join("aggregates.db"));
                move || aggregate::open_at(&db, &legacy).map(drop).map_err(|e| e.to_string())
            }),
        ),
        (
            "view_state",
            lib.join("view_state.db"),
            vec![("connection_view_state", true)],
            Box::new({
                let (db, legacy) = (db.clone(), lib.join("view_state.db"));
                move || view_state::open_at(&db, &legacy).map(drop).map_err(|e| e.to_string())
            }),
        ),
    ];
    for (store, file, tables, open) in &stores {
        let outcome = open();
        for (table, exact) in tables {
            let new = match &outcome {
                Ok(()) => Ok(new_count(table)),
                Err(error) => Err(error.clone()),
            };
            check(&format!("{store}.{table}"), old_rows(file, table), new, *exact);
        }
    }

    // The documents: counted from the files the old code would have read.
    let documents_outcome = documents::import_all_from(&db, &app).map_err(|e| e.to_string());
    let file_count = |dir: &Path, keep: &dyn Fn(&str) -> bool| -> String {
        std::fs::read_dir(dir).map_or_else(
            |_| "no folder".into(),
            |entries| entries.flatten().filter(|e| keep(&e.file_name().to_string_lossy())).count().to_string(),
        )
    };
    let single = |name: &str| if app.join(name).is_file() { "1".to_owned() } else { "no file".to_owned() };
    let diagrams = ["diagrams", "Diagrams"].iter().map(|n| app.join(n)).find(|p| p.is_dir()).unwrap_or_else(|| app.join("diagrams"));
    let hex = |name: &str| name.strip_suffix(".json").is_some_and(|stem| stem.len() == 24 && stem.bytes().all(|b| b.is_ascii_hexdigit() && !b.is_ascii_uppercase()));
    let in_documents = |kind: &str| -> Result<i64, String> {
        documents_outcome.clone()?;
        Ok(app_db::open(&db).unwrap().query_row("SELECT COUNT(*) FROM documents WHERE kind = ?1", [kind], |r| r.get(0)).unwrap())
    };
    check("session", single("gpui-workspace-session.json"), in_documents(documents::SESSION), true);
    check("update-check", single("update-check.json"), in_documents(documents::UPDATE_CHECK), true);
    check("diagrams", file_count(&diagrams, &hex), in_documents(documents::DIAGRAM), true);
    let conversations = app.join("assistant").join("conversations");
    let old_conversations: usize = std::fs::read_dir(&conversations)
        .map(|folders| {
            folders
                .flatten()
                .filter_map(|folder| std::fs::read_dir(folder.path()).ok())
                .map(|files| files.flatten().filter(|f| f.file_name().to_string_lossy().ends_with(".json")).count())
                .sum()
        })
        .unwrap_or(0);
    let old_conversations = if conversations.is_dir() { old_conversations.to_string() } else { "no folder".into() };
    // Files that are not conversations of this build (a header that does not read) are not moved in.
    check(
        "assistant conversations",
        old_conversations,
        documents_outcome.clone().map(|()| new_count("assistant_conversations")),
        false,
    );

    println!("\n{:<34} {:>12} {:>12}", "store", "old file", "new database");
    for (name, old, new) in &report {
        println!("{name:<34} {old:>12} {new:>12}");
    }
    println!("imports recorded: {:?}", markers(&app_db::open(&db).unwrap()));
    println!("warnings: {:?}", legacy::warnings_snapshot());
    assert_eq!(tree(&source), before, "the source was changed");
    assert!(failures.is_empty(), "differences: {failures:?}");
}

/// Every file and folder under `dir` with the bytes of the files.
fn tree(dir: &Path) -> Vec<(PathBuf, Option<Vec<u8>>)> {
    let mut out = Vec::new();
    let mut entries: Vec<_> = std::fs::read_dir(dir).unwrap().map(|e| e.unwrap().path()).collect();
    entries.sort();
    for path in entries {
        if path.is_dir() {
            out.push((path.clone(), None));
            out.extend(tree(&path));
        } else {
            out.push((path.clone(), Some(std::fs::read(&path).unwrap())));
        }
    }
    out
}

// ---- a store on its old file while another process imports -----------------------------------

#[test]
fn a_store_on_its_old_file_goes_on_in_the_database_once_another_process_imported_and_loses_nothing() {
    let insert = |id: &str, name: &str, updated: f64| {
        format!(
            "INSERT INTO saved_connections (id, name, uri, db_type, color_hex, save_password, save_proxy_password, \
             network_settings_json, created_at, updated_at) VALUES ('{id}','{name}','u','sqlite','#fff',0,0,'{{}}',1.0,{updated})"
        )
    };
    // Times are seconds since the epoch, as the app writes them: the run began on the old file now.
    let now = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_secs_f64();
    let fx = Fixture::new();
    let legacy = fx.old("connections.db", OLD_CONNECTIONS);
    let db = fx.db();
    refuse_rows(&db, "saved_connections");
    let mut store = connection::open_or_fall_back(&db, &legacy).unwrap();
    assert!(store.on_old_file());
    // This run saves one connection: it goes to the old file.
    store.execute(&insert("during", "During", now + 10.0), []).unwrap();

    // Another process gets the database right and moves the old file in (it holds `during` too).
    app_db::open(&db).unwrap().execute_batch("DROP TRIGGER no_rows").unwrap();
    let other = connection::open_at(&db, &legacy).unwrap();
    assert_eq!(count(&other, "saved_connections"), 3);
    // It then changes a row (later than the old file has it); this run saves another one, still to
    // the old file, and renames `two` there, earlier than the database's own copy of it.
    other.execute(&format!("UPDATE saved_connections SET name = 'Changed there', updated_at = {} WHERE id = 'old'", now + 500.0), []).unwrap();
    other.execute(&format!("UPDATE saved_connections SET name = 'Changed here', updated_at = {} WHERE id = 'two'", now + 600.0), []).unwrap();
    store.execute(&format!("UPDATE saved_connections SET name = 'Stale rename', updated_at = {} WHERE id = 'two'", now + 100.0), []).unwrap();
    store.execute(&format!("UPDATE saved_connections SET name = 'Renamed here', updated_at = {} WHERE id = 'old'", now + 700.0), []).unwrap();
    store.execute(&insert("after", "After", now + 20.0), []).unwrap();

    // The next use: the store sees the marker, merges the old file in and goes on in the database.
    store.refresh();
    assert!(!store.on_old_file());
    let names: Vec<(String, String)> = store
        .prepare("SELECT id, name FROM saved_connections ORDER BY id")
        .unwrap()
        .query_map([], |r| Ok((r.get(0)?, r.get(1)?)))
        .unwrap()
        .map(Result::unwrap)
        .collect();
    assert_eq!(
        names,
        [
            ("after".into(), "After".into()),
            ("during".into(), "During".into()),
            ("old".into(), "Renamed here".into()),
            ("two".into(), "Changed here".into()),
        ],
        "nothing lost; of a row both changed, the later change wins"
    );
    // And a write now lands in the database, not in the old file.
    store.execute(&insert("later", "Later", now + 30.0), []).unwrap();
    assert_eq!(count(&app_db::open(&db).unwrap(), "saved_connections"), 5);
    let in_old: i64 = Connection::open(&legacy)
        .unwrap()
        .query_row("SELECT COUNT(*) FROM saved_connections WHERE id = 'later'", [], |r| r.get(0))
        .unwrap();
    assert_eq!(in_old, 0);
}

#[test]
fn the_history_of_a_store_on_its_old_file_is_added_to_the_database_when_another_process_imported() {
    let fx = Fixture::new();
    let schema = "CREATE TABLE query_history (id INTEGER PRIMARY KEY AUTOINCREMENT, conn_uuid TEXT NOT NULL,
            database TEXT NOT NULL, collection TEXT NOT NULL, filter TEXT DEFAULT '', project TEXT DEFAULT '',
            sort TEXT DEFAULT '', skip INTEGER DEFAULT 0, \"limit\" INTEGER DEFAULT 0,
            max_time_ms INTEGER DEFAULT 0, result_count INTEGER DEFAULT 0, timestamp REAL NOT NULL);
         INSERT INTO query_history (conn_uuid, database, collection, filter, timestamp) VALUES ('c','d','x','a',1.0);";
    let legacy = fx.old("history.db", schema);
    let db = fx.db();
    refuse_rows(&db, "query_history");
    let mut store = history::open_or_fall_back(&db, &legacy).unwrap();
    let add = "INSERT INTO query_history (conn_uuid, database, collection, filter, timestamp) VALUES ('c','d','x','b',2.0)";
    store.execute(add, []).unwrap();
    app_db::open(&db).unwrap().execute_batch("DROP TRIGGER no_rows").unwrap();
    // Another process imports, and adds a row of its own that takes the id this run gave `b`.
    let other = history::open_at(&db, &legacy).unwrap();
    other
        .execute("INSERT INTO query_history (conn_uuid, database, collection, filter, timestamp) VALUES ('c','d','x','theirs',3.0)", [])
        .unwrap();
    store.execute("INSERT INTO query_history (conn_uuid, database, collection, filter, timestamp) VALUES ('c','d','x','c',4.0)", []).unwrap();
    store.refresh();
    assert!(!store.on_old_file());
    let filters: Vec<String> = store
        .prepare("SELECT filter FROM query_history ORDER BY timestamp")
        .unwrap()
        .query_map([], |r| r.get(0))
        .unwrap()
        .map(Result::unwrap)
        .collect();
    assert_eq!(filters, ["a", "b", "theirs", "c"], "every entry, none twice");
}

#[test]
fn an_old_file_that_a_writer_holds_for_a_moment_is_waited_for() {
    let fx = Fixture::new();
    let legacy = fx.old("connections.db", OLD_CONNECTIONS);
    // The previous version (or this one on the old file) is in the middle of a write.
    let holder = Connection::open(&legacy).unwrap();
    holder.execute_batch("BEGIN EXCLUSIVE").unwrap();
    let release = std::thread::spawn(move || {
        std::thread::sleep(std::time::Duration::from_millis(400));
        holder.execute_batch("COMMIT").unwrap();
    });
    let started = std::time::Instant::now();
    let conn = connection::open_at(&fx.db(), &legacy).expect("the import waits for the writer");
    assert!(started.elapsed() >= std::time::Duration::from_millis(300), "it did wait");
    assert_eq!(count(&conn, "saved_connections"), 2);
    release.join().unwrap();
}

// ---- conversations: what the user deletes is gone, old files included -------------------------

#[test]
fn deleting_conversations_deletes_their_old_files_and_a_cleared_store_is_never_filled_again() {
    let fx = Fixture::new();
    let one = write(&fx, "assistant/conversations/c1/one.json", &conversation_file("one", "c1", 100, 2));
    let two = write(&fx, "assistant/conversations/c1/two.json", &conversation_file("two", "c1", 200, 2));
    let other = write(&fx, "assistant/conversations/c2/three.json", &conversation_file("three", "c2", 50, 2));
    let damaged = write(&fx, "assistant/conversations/c1/broken.json", "not a conversation");
    let db = fx.db();
    assert_eq!(documents::list_conversations(&db, "c1", 50).unwrap().len(), 2);
    // The one that was not moved in is something the user sees, with the reason.
    let problems = documents::old_conversation_problems(&db, "c1").unwrap();
    assert_eq!(problems.len(), 1);
    assert_eq!((problems[0].id.as_str(), problems[0].why.as_str()), ("broken", "its header does not read"));
    assert!(documents::old_conversation_problems(&db, "c2").unwrap().is_empty());

    // One conversation: its row and its old file.
    assert!(documents::delete_conversation(&db, "c1", "one").unwrap().0);
    assert!(!one.exists() && two.exists());
    // The damaged one has no row: deleting it removes the file and says there was one.
    assert!(documents::delete_conversation(&db, "c1", "broken").unwrap().0);
    assert!(!damaged.exists());
    assert!(!documents::delete_conversation(&db, "c1", "broken").unwrap().0, "nothing left");
    // One connection's: the other connection's old files stay.
    assert_eq!(documents::delete_conversations(&db, Some("c1")).unwrap().0, 1);
    assert!(!two.exists() && other.exists());
    // Everything: nothing of it is left on disk, and nothing comes back at the next start.
    assert_eq!(documents::delete_conversations(&db, None).unwrap().0, 1);
    assert!(!other.exists());
    legacy::forget_process_state();
    assert!(documents::list_conversations(&db, "c1", 50).unwrap().is_empty());
    assert!(documents::list_conversations(&db, "c2", 50).unwrap().is_empty());
}

#[cfg(unix)]
#[test]
fn an_old_conversation_that_cannot_be_read_is_a_problem_to_see_and_delete_not_a_reason_to_try_again() {
    use std::os::unix::fs::PermissionsExt;
    let fx = Fixture::new();
    write(&fx, "assistant/conversations/c1/ok.json", &conversation_file("ok", "c1", 100, 2));
    let locked = write(&fx, "assistant/conversations/c1/locked.json", &conversation_file("locked", "c1", 200, 2));
    std::fs::set_permissions(&locked, std::fs::Permissions::from_mode(0o000)).unwrap();
    if std::fs::read(&locked).is_ok() {
        return; // root can read it anyway
    }
    let db = fx.db();
    assert_eq!(documents::list_conversations(&db, "c1", 50).unwrap().len(), 1);
    // The marker is written all the same: nothing is tried again at the next start.
    assert_eq!(markers(&app_db::open(&db).unwrap()), ["assistant"]);
    let problems = documents::old_conversation_problems(&db, "c1").unwrap();
    assert_eq!(problems.iter().map(|p| p.id.as_str()).collect::<Vec<_>>(), ["locked"]);
    assert!(problems[0].why.contains("cannot be read"), "{}", problems[0].why);
    // The user deletes it: the file is gone (a file's own mode does not stop its removal).
    assert!(documents::delete_conversation(&db, "c1", "locked").unwrap().0);
    assert!(!locked.exists());
}

#[test]
fn a_saved_session_that_is_not_valid_text_is_set_aside_as_damaged_not_repaired() {
    let fx = Fixture::new();
    let old = fx.dir.path().join("tablex").join("gpui-workspace-session.json");
    std::fs::create_dir_all(old.parent().unwrap()).unwrap();
    std::fs::write(&old, [b'{', 0xff, 0xfe, b'}']).unwrap();
    let db = fx.db();
    // No session to restore, and the damaged text is kept apart (as the old quarantine did).
    assert_eq!(documents::get(&db, documents::SESSION, documents::SESSION_NAME).unwrap(), None);
    assert!(documents::get(&db, documents::SESSION, documents::SESSION_CORRUPT_NAME).unwrap().is_some());
    assert_eq!(markers(&app_db::open(&db).unwrap()), ["session"]);
    assert_eq!(std::fs::read(&old).unwrap(), [b'{', 0xff, 0xfe, b'}'], "the file is not touched");
}

#[test]
fn what_another_process_deleted_from_the_database_does_not_come_back_when_a_store_switches() {
    let now = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_secs_f64();
    let insert = |id: &str, updated: f64| {
        format!(
            "INSERT INTO saved_connections (id, name, uri, db_type, color_hex, save_password, save_proxy_password, \
             network_settings_json, created_at, updated_at) VALUES ('{id}','{id}','u','sqlite','#fff',0,0,'{{}}',1.0,{updated})"
        )
    };
    let fx = Fixture::new();
    let legacy = fx.old("connections.db", OLD_CONNECTIONS);
    let db = fx.db();
    refuse_rows(&db, "saved_connections");
    let mut store = connection::open_or_fall_back(&db, &legacy).unwrap();
    // The run adds one connection and edits one that was there; it leaves `two` alone.
    store.execute(&insert("added", now + 5.0), []).unwrap();
    store.execute(&format!("UPDATE saved_connections SET name = 'Edited here', updated_at = {} WHERE id = 'old'", now + 6.0), []).unwrap();
    // Another process imports, then the user deletes `two` there (and `added`, which it got too).
    app_db::open(&db).unwrap().execute_batch("DROP TRIGGER no_rows").unwrap();
    let other = connection::open_at(&db, &legacy).unwrap();
    other.execute("DELETE FROM saved_connections WHERE id IN ('two', 'added')", []).unwrap();
    let imported_at = |conn: &Connection| -> i64 {
        conn.query_row("SELECT at FROM imports WHERE name = 'connections'", [], |r| r.get(0)).unwrap()
    };
    let first_marker = imported_at(&other);
    std::thread::sleep(std::time::Duration::from_millis(1100));
    store.refresh();
    assert!(!store.on_old_file());
    assert!(imported_at(&store) > first_marker, "the marker's time is the switch's");
    let ids: Vec<String> = store
        .prepare("SELECT id FROM saved_connections ORDER BY id")
        .unwrap()
        .query_map([], |r| r.get(0))
        .unwrap()
        .map(Result::unwrap)
        .collect();
    // `two` stays deleted (it was there when the run began and was not touched). `added` was
    // written during the run: it arrives again, as the run's own write. `old` was edited.
    assert_eq!(ids, ["added", "old"]);
    let name: String = store.query_row("SELECT name FROM saved_connections WHERE id = 'old'", [], |r| r.get(0)).unwrap();
    assert_eq!(name, "Edited here");
    // The marker's time moved to the switch: the old file this run wrote is not "changed by an
    // older TableX" at the next start.
    legacy::forget_process_state();
    let path = legacy.display().to_string();
    let before = legacy::warnings_snapshot().iter().filter(|w| w.contains(&path) && w.contains("changed after")).count();
    connection::open_at(&db, &legacy).unwrap();
    let after = legacy::warnings_snapshot().iter().filter(|w| w.contains(&path) && w.contains("changed after")).count();
    assert_eq!(before, after, "no warning for the run's own writes");
}
