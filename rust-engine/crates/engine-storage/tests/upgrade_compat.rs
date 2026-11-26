//! Upgrade compatibility of the app database file itself: the `tablex.db` of every earlier release
//! (`fixtures/compat/<version>/tablex/tablex.db`, made-up data, see `fixtures/compat/README.md`)
//! is opened by this build and must come out with every row it had, a schema that only grew, and
//! a `min_reader_version` that leaves the release that wrote it able to read it again (a user who
//! goes back). What the rows mean to the stores and the app is checked where those live
//! (`engine-connect/tests/upgrade_compat.rs`, `tablex-gpui`'s `upgrade_compat`).

use engine_storage::app_db::{self, AppDbError};
use rusqlite::types::Value;
use rusqlite::{Connection, OpenFlags};
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

/// The releases whose fixture sets are checked. Append the version when its fixtures are added.
const VERSIONS: &[&str] = &["v0.1.10", "v0.1.11"];

fn fixture_db(version: &str) -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../../../fixtures/compat")
        .join(version)
        .join("tablex")
        .join(app_db::FILE_NAME)
}

/// A private copy of the fixture (opening migrates and writes the log files beside it).
fn copy_of(version: &str) -> (tempfile::TempDir, PathBuf) {
    let source = fixture_db(version);
    assert!(source.is_file(), "{version}: missing fixture {}", source.display());
    let dir = tempfile::tempdir().unwrap();
    let copy = dir.path().join(app_db::FILE_NAME);
    std::fs::copy(&source, &copy).unwrap();
    (dir, copy)
}

type Rows = Vec<Vec<String>>;

/// The column names of every table, in order.
fn columns(path: &Path) -> BTreeMap<String, Vec<String>> {
    let conn = Connection::open_with_flags(path, OpenFlags::SQLITE_OPEN_READ_ONLY).unwrap();
    let names: Vec<String> = conn
        .prepare("SELECT name FROM sqlite_master WHERE type = 'table' AND name NOT LIKE 'sqlite_%' ORDER BY name")
        .unwrap()
        .query_map([], |row| row.get(0))
        .unwrap()
        .collect::<Result<_, _>>()
        .unwrap();
    names
        .into_iter()
        .map(|name| {
            let found = conn
                .prepare(&format!("SELECT name FROM pragma_table_info('{name}') ORDER BY cid"))
                .unwrap()
                .query_map([], |row| row.get(0))
                .unwrap()
                .collect::<Result<_, _>>()
                .unwrap();
            (name, found)
        })
        .collect()
}

/// Every row of every table, as text, in primary-key order of the table's own scan.
fn dump(path: &Path) -> BTreeMap<String, Rows> {
    dump_columns(path, None)
}

/// [`dump`], reading only the columns `only` lists for a table (the columns it had before a
/// migration added some).
fn dump_columns(path: &Path, only: Option<&BTreeMap<String, Vec<String>>>) -> BTreeMap<String, Rows> {
    let conn = Connection::open_with_flags(path, OpenFlags::SQLITE_OPEN_READ_ONLY).unwrap();
    let names: Vec<String> = conn
        .prepare("SELECT name FROM sqlite_master WHERE type = 'table' AND name NOT LIKE 'sqlite_%' ORDER BY name")
        .unwrap()
        .query_map([], |row| row.get(0))
        .unwrap()
        .collect::<Result<_, _>>()
        .unwrap();
    names
        .into_iter()
        .map(|name| {
            let selected = only
                .and_then(|only| only.get(&name))
                .map_or("*".to_owned(), |columns| columns.iter().map(|column| format!("\"{column}\"")).collect::<Vec<_>>().join(", "));
            let mut statement = conn.prepare(&format!("SELECT {selected} FROM \"{name}\"")).unwrap();
            let columns = statement.column_count();
            let mut rows: Rows = statement
                .query_map([], |row| {
                    (0..columns)
                        .map(|index| row.get::<_, Value>(index).map(|value| format!("{value:?}")))
                        .collect()
                })
                .unwrap()
                .collect::<Result<_, _>>()
                .unwrap();
            rows.sort();
            (name, rows)
        })
        .collect()
}

/// `(type, name) -> sql` of the schema objects a migration created.
fn schema(path: &Path) -> BTreeMap<(String, String), String> {
    let conn = Connection::open_with_flags(path, OpenFlags::SQLITE_OPEN_READ_ONLY).unwrap();
    let mut statement = conn
        .prepare("SELECT type, name, sql FROM sqlite_master WHERE sql IS NOT NULL AND name NOT LIKE 'sqlite_%'")
        .unwrap();
    let objects = statement
        .query_map([], |row| Ok(((row.get(0)?, row.get(1)?), row.get(2)?)))
        .unwrap()
        .collect::<Result<_, _>>()
        .unwrap();
    objects
}

fn user_version(path: &Path) -> u32 {
    let conn = Connection::open_with_flags(path, OpenFlags::SQLITE_OPEN_READ_ONLY).unwrap();
    conn.pragma_query_value(None, "user_version", |row| row.get(0)).unwrap()
}

fn min_reader_version(path: &Path) -> u32 {
    let conn = Connection::open_with_flags(path, OpenFlags::SQLITE_OPEN_READ_ONLY).unwrap();
    conn.query_row("SELECT value FROM meta WHERE key = 'min_reader_version'", [], |row| row.get::<_, String>(0))
        .unwrap()
        .parse()
        .unwrap()
}

#[test]
fn the_fixtures_hold_every_kind_of_row_they_are_meant_to() {
    for version in VERSIONS {
        let (_dir, path) = copy_of(version);
        let rows = dump(&path);
        for table in [
            "secrets",
            "saved_connections",
            "projects",
            "project_connection_memberships",
            "query_history",
            "saved_aggregates",
            "connection_view_state",
            "pinned_nodes",
            "documents",
            "assistant_conversations",
            "imports",
            "meta",
        ] {
            let count = rows.get(table).map_or(0, Vec::len);
            assert!(count > 0, "{version}: the fixture has no row in `{table}`");
        }
    }
}

#[test]
fn opening_an_earlier_database_keeps_every_row_and_only_adds_schema() {
    for version in VERSIONS {
        let (_dir, path) = copy_of(version);
        let before = dump(&path);
        let columns_before = columns(&path);
        let schema_before = schema(&path);
        let written_by = user_version(&path);
        assert!(written_by <= app_db::SCHEMA_VERSION, "{version}: written by a newer schema ({written_by})");

        drop(app_db::open(&path).unwrap_or_else(|error| panic!("{version}: {error}")));

        assert_eq!(user_version(&path), app_db::SCHEMA_VERSION, "{version}: migrated to the current schema");
        // A migration may add (nullable) columns to a table; the columns it had keep every value.
        let after = dump_columns(&path, Some(&columns_before));
        for (table, rows) in &before {
            assert_eq!(after.get(table), Some(rows), "{version}: the rows of `{table}` changed on open");
        }
        // Where columns came in, they are empty: nothing was made up for a row of an older release
        // (but the kind, which is read from the statement).
        let grown = columns(&path);
        for (table, was) in &columns_before {
            assert!(grown[table].starts_with(was), "{version}: `{table}` lost or moved a column");
        }
        // A shipped migration is never edited: what the earlier release created is still created
        // the same way by a fresh database of this build.
        let fresh_dir = tempfile::tempdir().unwrap();
        let fresh = fresh_dir.path().join(app_db::FILE_NAME);
        drop(app_db::open(&fresh).unwrap());
        let fresh_schema = schema(&fresh);
        for (object, sql) in &schema_before {
            let fresh = fresh_schema.get(object).unwrap_or_else(|| panic!("{version}: {object:?} is gone"));
            if object.0 == "table" && columns_before.contains_key(&object.1) && columns(&fresh_dir.path().join(app_db::FILE_NAME))[&object.1].len() > columns_before[&object.1].len() {
                // A table a later migration added columns to: its text is the old one, grown at the end.
                let old_body = sql.trim_end().trim_end_matches(')');
                assert!(fresh.starts_with(old_body), "{version}: {object:?} is not created as it was");
            } else {
                assert_eq!(fresh, sql, "{version}: {object:?} is not created as it was");
            }
        }
        // Opened again nothing moves.
        let settled = dump(&path);
        drop(app_db::open(&path).unwrap());
        assert_eq!(dump(&path), settled, "{version}: a second open changed rows");
        let conn = Connection::open_with_flags(&path, OpenFlags::SQLITE_OPEN_READ_ONLY).unwrap();
        let check: String = conn.query_row("PRAGMA integrity_check", [], |row| row.get(0)).unwrap();
        assert_eq!(check, "ok", "{version}");
    }
}

/// The way back: a user who goes back to the release that wrote the file must still be able to
/// open it after this build has been at it. The release reads a database whose `user_version` is
/// above its own when `meta.min_reader_version` is at most its own schema version. A migration
/// that raises it above the previous release's version fails here: raise it on purpose only (and
/// say so in the release notes).
#[test]
fn the_release_that_wrote_the_database_can_still_read_it_after_this_build_opened_it() {
    for version in VERSIONS {
        let (_dir, path) = copy_of(version);
        let written_by = user_version(&path);
        let minimum_before = min_reader_version(&path);
        drop(app_db::open(&path).unwrap());
        let minimum_after = min_reader_version(&path);
        assert_eq!(minimum_after, minimum_before, "{version}: opening raised min_reader_version");
        assert!(minimum_after <= written_by, "{version}: {version} could not read the database any more ({minimum_after} > {written_by})");
    }
}

#[test]
fn a_database_from_a_newer_release_is_refused_untouched_and_a_damaged_one_is_not_replaced() {
    let (_dir, path) = copy_of(VERSIONS[0]);
    // A newer release that the older code cannot work with.
    {
        let conn = Connection::open(&path).unwrap();
        conn.execute("UPDATE meta SET value = ?1 WHERE key = 'min_reader_version'", [(app_db::SCHEMA_VERSION + 1).to_string()]).unwrap();
        conn.pragma_update(None, "user_version", app_db::SCHEMA_VERSION + 1).unwrap();
    }
    let before = dump(&path);
    assert!(matches!(app_db::open(&path), Err(AppDbError::Newer { found, .. }) if found == app_db::SCHEMA_VERSION + 1));
    assert_eq!(dump(&path), before);

    let damaged = path.with_file_name("damaged.db");
    std::fs::write(&damaged, vec![0xA5u8; 8192]).unwrap();
    assert!(app_db::open(&damaged).is_err());
    assert_eq!(std::fs::read(&damaged).unwrap(), vec![0xA5u8; 8192], "a file that is not a database is left as it is");
}

/// The query history of an earlier release after the migration that gave it a status, an error, a
/// duration and a kind: every old row loads, as a run that worked with nothing made up, and the
/// manager's searches find it.
#[test]
fn the_query_history_of_an_earlier_release_loads_and_is_searched_after_the_migration() {
    use engine_storage::history::{self, HistoryFilter, StatusFilter};
    for version in VERSIONS {
        let (_dir, path) = copy_of(version);
        let before = Connection::open_with_flags(&path, OpenFlags::SQLITE_OPEN_READ_ONLY).unwrap();
        let total: i64 = before.query_row("SELECT COUNT(*) FROM query_history", [], |row| row.get(0)).unwrap();
        assert!(total > 0, "{version}");
        drop(before);

        let conn = app_db::open(&path).unwrap();
        let all = history::search_in(&conn, &HistoryFilter::default(), None, 1000).unwrap();
        assert_eq!(all.len() as i64, total, "{version}: every row loads");
        for entry in &all {
            assert_eq!(entry.status, None, "{version}: an old row has no status made up");
            assert_eq!((entry.error.as_deref(), entry.duration_ms), (None, None), "{version}");
            assert!(!entry.failed(), "{version}");
            assert!(entry.kind.is_some(), "{version}: the kind is read from the statement");
        }
        // An old row counts as a run that worked.
        let ok = HistoryFilter { status: StatusFilter::Ok, ..Default::default() };
        assert_eq!(history::search_in(&conn, &ok, None, 1000).unwrap().len() as i64, total, "{version}");
        let failed = HistoryFilter { status: StatusFilter::Failed, ..Default::default() };
        assert!(history::search_in(&conn, &failed, None, 1000).unwrap().is_empty(), "{version}");
        // The text search works on the old statements, and a new run lands beside them.
        let word = all[0].filter.split_whitespace().next().unwrap_or("").to_owned();
        let found = history::search_in(&conn, &HistoryFilter { text: word, ..Default::default() }, None, 1000).unwrap();
        assert!(found.iter().any(|entry| entry.id == all[0].id), "{version}");
        history::record_in(
            &conn,
            &history::Record {
                conn_uuid: "c",
                database: "d",
                collection: "",
                filter: "SELECT 1",
                project: "",
                sort: "",
                skip: 0,
                limit: 10,
                max_time_ms: 0,
                result_count: 1,
                error: Some("boom"),
                duration_ms: Some(3),
                kind: history::KIND_QUERY,
            },
            4_000_000_000.0,
        )
        .unwrap();
        let newest = history::search_in(&conn, &HistoryFilter::default(), None, 1).unwrap();
        assert!(newest[0].failed() && newest[0].error.as_deref() == Some("boom"), "{version}");
        assert_eq!(history::search_in(&conn, &HistoryFilter::default(), None, 1000).unwrap().len() as i64, total + 1, "{version}");
    }
}
