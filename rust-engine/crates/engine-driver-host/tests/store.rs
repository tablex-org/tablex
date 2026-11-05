use engine_driver_host::{InstalledDriverRecord, InstalledDriverStore, StoreError};
use std::fs::{self, OpenOptions};
use std::sync::{Arc, Barrier};
use std::thread;
use std::time::Duration;
use tempfile::TempDir;

fn record(id: &str) -> InstalledDriverRecord {
    InstalledDriverRecord {
        id: id.into(),
        version: "2.0.0".into(),
        path: format!("/tmp/{id}/libdbengine_{id}.dylib").into(),
        sha256: "abc123".into(),
        team_id: "ABCDE12345".into(),
        installed_at: "2026-08-31T00:00:00Z".into(),
        app_abi_version: 2,
        uri_schemes: vec![id.into()],
        license: "Apache-2.0".into(),
        source_repo: "https://github.com/tablex-org/tablex".into(),
        disabled: false,
        ..Default::default()
    }
}

#[test]
fn old_records_default_to_enabled_and_disable_state_persists() {
    let temp = TempDir::new().expect("temp directory");
    let store = InstalledDriverStore::new(temp.path());
    fs::create_dir_all(store.store_path().parent().expect("store parent"))
        .expect("create store parent");
    fs::write(
        store.store_path(),
        r#"[{"id":"mongodb","version":"2.0.0","path":"/tmp/mongodb.dylib","sha256":"abc","teamId":"TEAM","installedAt":"now","appAbiVersion":2,"uriSchemes":["mongodb"],"license":"MIT","sourceRepo":"https://example.com"}]"#,
    )
    .expect("write legacy record");

    assert!(
        !store
            .record("mongodb")
            .expect("load legacy record")
            .expect("mongodb")
            .disabled
    );

    store.set_disabled("mongodb", true).expect("disable driver");
    assert!(
        store
            .record("mongodb")
            .expect("reload record")
            .expect("mongodb")
            .disabled
    );
}

#[test]
fn remove_record_returns_the_removed_record() {
    let temp = TempDir::new().expect("temp directory");
    let store = InstalledDriverStore::new(temp.path());
    store.save(record("mongodb")).expect("save record");

    let removed = store.remove_record("mongodb").expect("remove record");
    assert_eq!(removed.expect("removed").id, "mongodb");
    assert!(store.record("mongodb").expect("load record").is_none());
}

#[test]
fn store_is_namespaced_under_abi_v2() {
    let temp = TempDir::new().expect("temp directory");
    let store = InstalledDriverStore::new(temp.path());

    assert_eq!(
        store.store_path(),
        temp.path()
            .join("Drivers")
            .join("v2")
            .join("installed-drivers.json")
    );
    assert_eq!(
        store.lock_path(),
        temp.path()
            .join("Drivers")
            .join("v2")
            .join("installed-drivers.lock")
    );
}

#[test]
fn decodes_the_existing_camel_case_record_shape() {
    let temp = TempDir::new().expect("temp directory");
    let store = InstalledDriverStore::new(temp.path());
    fs::create_dir_all(store.store_path().parent().expect("store parent"))
        .expect("create store parent");
    fs::write(
        store.store_path(),
        r#"[{
          "id":"mongodb",
          "version":"2.0.0",
          "path":"/tmp/mongodb/libdbengine_mongodb.dylib",
          "sha256":"abc123",
          "teamId":"ABCDE12345",
          "installedAt":"2026-08-31T00:00:00Z",
          "appAbiVersion":2,
          "uriSchemes":["mongodb","mongodb+srv"],
          "license":"Apache-2.0",
          "sourceRepo":"https://github.com/tablex-org/tablex"
        }]"#,
    )
    .expect("write fixture");

    let records = store.load_all().expect("load records");
    let mongodb = records.get("mongodb").expect("mongodb record");

    assert_eq!(mongodb.team_id, "ABCDE12345");
    assert_eq!(mongodb.app_abi_version, 2);
    assert_eq!(mongodb.uri_schemes, ["mongodb", "mongodb+srv"]);
}

#[test]
fn writes_records_in_stable_id_order() {
    let temp = TempDir::new().expect("temp directory");
    let store = InstalledDriverStore::new(temp.path());

    store.save(record("sqlite")).expect("save sqlite");
    store.save(record("mongodb")).expect("save mongodb");

    let contents = fs::read_to_string(store.store_path()).expect("read store");
    let mongodb_position = contents.find("mongodb").expect("mongodb in store");
    let sqlite_position = contents.find("sqlite").expect("sqlite in store");
    assert!(mongodb_position < sqlite_position);
}

#[test]
fn concurrent_writers_preserve_both_records() {
    let temp = TempDir::new().expect("temp directory");
    let store = Arc::new(InstalledDriverStore::new(temp.path()));
    let barrier = Arc::new(Barrier::new(3));

    let handles = ["mongodb", "postgresql"].map(|id| {
        let store = Arc::clone(&store);
        let barrier = Arc::clone(&barrier);
        thread::spawn(move || {
            barrier.wait();
            store.save(record(id)).expect("save record");
        })
    });

    barrier.wait();
    for handle in handles {
        handle.join().expect("writer joins");
    }

    let records = store.load_all().expect("load records");
    assert_eq!(records.len(), 2);
    assert!(records.contains_key("mongodb"));
    assert!(records.contains_key("postgresql"));
}

#[test]
fn reports_lock_timeout_instead_of_blocking_forever() {
    let temp = TempDir::new().expect("temp directory");
    let store = InstalledDriverStore::with_lock_timeout(temp.path(), Duration::from_millis(25));
    fs::create_dir_all(store.lock_path().parent().expect("lock parent"))
        .expect("create lock parent");
    let lock_file = OpenOptions::new()
        .create(true)
        .read(true)
        .write(true)
        .truncate(false)
        .open(store.lock_path())
        .expect("open lock file");

    lock_file.lock().expect("hold the store lock");
    let error = store.load_all().expect_err("second lock must time out");
    lock_file.unlock().expect("release the store lock");

    assert!(matches!(error, StoreError::LockTimeout { .. }));
}
