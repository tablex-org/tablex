//! The connections the server lists: `tablex-mcp` never moves the old `connections.db` into the
//! app database, it reads that file until the app has moved it, and the database after.

mod support;

use serde_json::json;
use support::*;

const LEGACY_ID: &str = "00000000-0000-4000-8000-0000000000aa";

fn listed_ids(body: &serde_json::Value) -> Vec<String> {
    body["connections"]
        .as_array()
        .unwrap()
        .iter()
        .map(|entry| entry["id"].as_str().unwrap().to_owned())
        .collect()
}

#[tokio::test]
async fn the_server_reads_the_old_file_until_the_app_has_moved_it_and_the_database_after() {
    // What `main` does first: this process may not import.
    engine_storage::legacy::set_import_allowed(false);
    // The data dir points at the temporary one now; nothing has opened the store yet.
    let shared = shared();
    let folder = shared.data_dir().join("tablex");
    std::fs::create_dir_all(&folder).unwrap();
    let old = folder.join("connections.db");
    rusqlite::Connection::open(&old)
        .unwrap()
        .execute_batch(&format!(
            "CREATE TABLE saved_connections (
                id TEXT PRIMARY KEY, name TEXT NOT NULL, uri TEXT NOT NULL, db_type TEXT NOT NULL,
                color_hex TEXT NOT NULL, save_password INTEGER NOT NULL, save_proxy_password INTEGER NOT NULL,
                network_settings_json TEXT NOT NULL, is_pinned INTEGER NOT NULL DEFAULT 0, last_connected REAL,
                created_at REAL NOT NULL, updated_at REAL NOT NULL);
             INSERT INTO saved_connections VALUES
                ('{LEGACY_ID}', 'from the old file', 'sqlite:///tmp/old.db',
                 'sqlite', '#34C759', 0, 0, '{{}}', 0, NULL, 1.0, 1.0);"
        ))
        .unwrap();
    let database = || rusqlite::Connection::open(folder.join("tablex.db")).unwrap();

    // No marker: the connection comes from the old file, and nothing was moved or marked.
    let harness = Harness::start(Fixture::new()).await;
    let listed = harness.call("list_connections", json!({})).await;
    assert!(listed_ids(&structured(&listed)).contains(&LEGACY_ID.to_owned()), "{listed:?}");
    let count = |table: &str| -> i64 { database().query_row(&format!("SELECT COUNT(*) FROM {table}"), [], |r| r.get(0)).unwrap() };
    assert_eq!((count("imports"), count("saved_connections")), (0, 0), "this process moved nothing");
    // (Reading it adds the two columns an old file may lack, as every previous version did.)
    let in_old: String = rusqlite::Connection::open(&old)
        .unwrap()
        .query_row("SELECT name FROM saved_connections", [], |r| r.get(0))
        .unwrap();
    assert_eq!(in_old, "from the old file", "and its data is as it was");

    // The app starts (it is the one that imports), and the database has the marker and the rows.
    engine_storage::legacy::set_import_allowed(true);
    engine_storage::import_old_files().unwrap();
    engine_storage::legacy::set_import_allowed(false);
    assert_eq!((count("imports") > 0, count("saved_connections")), (true, 1));
    // From now on the server reads the database: a change made there shows, one in the old file
    // does not.
    database().execute("UPDATE saved_connections SET name = 'from the database'", []).unwrap();
    rusqlite::Connection::open(&old).unwrap().execute("UPDATE saved_connections SET name = 'edited in the old file'", []).unwrap();
    let records = engine_storage::connection::list_connections().unwrap();
    assert_eq!(records.iter().filter(|r| r.id == LEGACY_ID).map(|r| r.name.as_str()).collect::<Vec<_>>(), ["from the database"]);
    let listed = harness.call("list_connections", json!({})).await;
    assert!(listed_ids(&structured(&listed)).contains(&LEGACY_ID.to_owned()));

    // The saved secrets: even after the app moved the connections in, this process (which may not
    // import) answers a secret that only the previous version's Keychain item holds from that
    // item, and writes nothing: no row, no master key.
    use engine_connect::credentials::{CredentialStore, SecretKind};
    let keys = std::sync::Arc::new(Keys::default());
    keys.write(SecretKind::Database, "ABC-LEGACY", "legacy-pw").unwrap();
    let secrets = engine_connect::secret_store::EncryptedStore::new(Box::new(keys.clone()), Some(folder.join("tablex.db")));
    assert_eq!(secrets.load(SecretKind::Database, "ABC-LEGACY").unwrap().as_deref(), Some("legacy-pw"));
    assert_eq!(count("secrets"), 0, "nothing was copied into the database");
    assert_eq!(keys.load(SecretKind::MasterKey, "master-key").unwrap(), None, "and no master key was made");
}
