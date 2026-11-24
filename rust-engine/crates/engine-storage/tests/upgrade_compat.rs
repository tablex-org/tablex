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
const VERSIONS: &[&str] = &["v0.1.10"];

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

/// Every row of every table, as text, in primary-key order of the table's own scan.
fn dump(path: &Path) -> BTreeMap<String, Rows> {
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
            let mut statement = conn.prepare(&format!("SELECT * FROM \"{name}\"")).unwrap();
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
        let schema_before = schema(&path);
        let written_by = user_version(&path);
        assert!(written_by <= app_db::SCHEMA_VERSION, "{version}: written by a newer schema ({written_by})");

        drop(app_db::open(&path).unwrap_or_else(|error| panic!("{version}: {error}")));

        assert_eq!(user_version(&path), app_db::SCHEMA_VERSION, "{version}: migrated to the current schema");
        let after = dump(&path);
        for (table, rows) in &before {
            assert_eq!(after.get(table), Some(rows), "{version}: the rows of `{table}` changed on open");
        }
        // A shipped migration is never edited: what the earlier release created is still created
        // the same way by a fresh database of this build.
        let fresh_dir = tempfile::tempdir().unwrap();
        let fresh = fresh_dir.path().join(app_db::FILE_NAME);
        drop(app_db::open(&fresh).unwrap());
        let fresh_schema = schema(&fresh);
        for (object, sql) in &schema_before {
            assert_eq!(fresh_schema.get(object), Some(sql), "{version}: {object:?} is not created as it was");
        }
        // Opened again nothing moves.
        drop(app_db::open(&path).unwrap());
        assert_eq!(dump(&path), after, "{version}: a second open changed rows");
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
