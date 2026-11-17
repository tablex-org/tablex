//! The sidebar rows a user pinned: the `pinned_nodes` table of the app database
//! ([`crate::app_db`]).
//!
//! A pin belongs to its exact place: `(connection_id, node)`, where `node` is a text the caller
//! builds from the full path of the row (this module never parses it), so `orders` pinned in one
//! database is not pinned in another, nor on another connection. `position` is the order the
//! rows were pinned in. A pin of something that no longer exists stays in the table and is
//! ignored by the caller when it lists the row's siblings; it goes with its connection
//! ([`remove_connection`]).
//!
//! Every function takes the path of the database (the app passes [`crate::app_db::default_path`];
//! tests pass a file of their own) and opens it for the call, as [`crate::documents`] does.

use std::path::Path;

use rusqlite::params;

use crate::app_db::{self, AppDbError};

/// A statement that failed.
fn at(db: &Path) -> impl Fn(rusqlite::Error) -> AppDbError + '_ {
    move |source| AppDbError::Sqlite {
        path: db.to_owned(),
        source,
    }
}

/// Every pin of every connection, as `(connection_id, node)`, in the order they were pinned.
pub fn list_all(db: &Path) -> Result<Vec<(String, String)>, AppDbError> {
    let conn = app_db::open(db)?;
    let mut statement = conn
        .prepare("SELECT connection_id, node FROM pinned_nodes ORDER BY position, connection_id, node")
        .map_err(at(db))?;
    let rows = statement
        .query_map([], |row| Ok((row.get(0)?, row.get(1)?)))
        .map_err(at(db))?;
    rows.collect::<Result<_, _>>().map_err(at(db))
}

/// The nodes pinned on `connection_id`, in the order they were pinned.
pub fn list(db: &Path, connection_id: &str) -> Result<Vec<String>, AppDbError> {
    let conn = app_db::open(db)?;
    let mut statement = conn
        .prepare("SELECT node FROM pinned_nodes WHERE connection_id = ?1 ORDER BY position, node")
        .map_err(at(db))?;
    let rows = statement.query_map([connection_id], |row| row.get(0)).map_err(at(db))?;
    rows.collect::<Result<_, _>>().map_err(at(db))
}

/// Pins `node`, after the others. A node that is already pinned keeps its place.
pub fn pin(db: &Path, connection_id: &str, node: &str) -> Result<(), AppDbError> {
    let mut conn = app_db::open(db)?;
    let tx = app_db::write_transaction(&mut conn).map_err(at(db))?;
    tx.execute(
        "INSERT OR IGNORE INTO pinned_nodes (connection_id, node, position)
         VALUES (?1, ?2, (SELECT COALESCE(MAX(position), 0) + 1 FROM pinned_nodes))",
        params![connection_id, node],
    )
    .map_err(at(db))?;
    tx.commit().map_err(at(db))
}

/// Removes the pin of `node`; whether there was one.
pub fn unpin(db: &Path, connection_id: &str, node: &str) -> Result<bool, AppDbError> {
    let mut conn = app_db::open(db)?;
    let tx = app_db::write_transaction(&mut conn).map_err(at(db))?;
    let removed = tx
        .execute(
            "DELETE FROM pinned_nodes WHERE connection_id = ?1 AND node = ?2",
            params![connection_id, node],
        )
        .map_err(at(db))?;
    tx.commit().map_err(at(db))?;
    Ok(removed > 0)
}

/// Removes every pin of `connection_id` (the connection was deleted).
pub fn remove_connection(db: &Path, connection_id: &str) -> Result<(), AppDbError> {
    let mut conn = app_db::open(db)?;
    let tx = app_db::write_transaction(&mut conn).map_err(at(db))?;
    tx.execute("DELETE FROM pinned_nodes WHERE connection_id = ?1", [connection_id])
        .map_err(at(db))?;
    tx.commit().map_err(at(db))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_db() -> (tempfile::TempDir, std::path::PathBuf) {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join(app_db::FILE_NAME);
        (dir, path)
    }

    #[test]
    fn pins_list_in_the_order_they_were_pinned_and_unpin_removes_one() {
        let (_dir, db) = temp_db();
        assert!(list(&db, "c1").unwrap().is_empty());
        for node in ["b", "c", "a"] {
            pin(&db, "c1", node).unwrap();
        }
        assert_eq!(list(&db, "c1").unwrap(), ["b", "c", "a"]);
        // Pinning again keeps the place.
        pin(&db, "c1", "b").unwrap();
        assert_eq!(list(&db, "c1").unwrap(), ["b", "c", "a"]);
        assert!(unpin(&db, "c1", "c").unwrap());
        assert!(!unpin(&db, "c1", "c").unwrap());
        assert_eq!(list(&db, "c1").unwrap(), ["b", "a"]);
        // Pinned again, it goes last.
        pin(&db, "c1", "c").unwrap();
        assert_eq!(list(&db, "c1").unwrap(), ["b", "a", "c"]);
    }

    #[test]
    fn pins_belong_to_their_connection() {
        let (_dir, db) = temp_db();
        pin(&db, "c1", "orders").unwrap();
        pin(&db, "c2", "orders").unwrap();
        pin(&db, "c2", "users").unwrap();
        assert_eq!(list(&db, "c1").unwrap(), ["orders"]);
        assert_eq!(list(&db, "c2").unwrap(), ["orders", "users"]);
        assert!(unpin(&db, "c2", "orders").unwrap());
        assert_eq!(list(&db, "c1").unwrap(), ["orders"]);
        assert_eq!(
            list_all(&db).unwrap(),
            [("c1".to_string(), "orders".to_string()), ("c2".to_string(), "users".to_string())]
        );
    }

    #[test]
    fn deleting_a_connection_removes_its_pins_only() {
        let (_dir, db) = temp_db();
        pin(&db, "c1", "a").unwrap();
        pin(&db, "c2", "a").unwrap();
        pin(&db, "c1", "b").unwrap();
        remove_connection(&db, "c1").unwrap();
        assert!(list(&db, "c1").unwrap().is_empty());
        assert_eq!(list(&db, "c2").unwrap(), ["a"]);
    }

    #[test]
    fn pins_survive_reopening_the_database() {
        let (_dir, db) = temp_db();
        pin(&db, "c1", "x").unwrap();
        pin(&db, "c1", "y").unwrap();
        assert_eq!(list_all(&db).unwrap().len(), 2);
        assert_eq!(list(&db, "c1").unwrap(), ["x", "y"]);
    }
}
