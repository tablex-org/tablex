//! Upgrade compatibility of the app database (`tablex.db`) and of what is in it: the data dir of an
//! earlier release (`fixtures/compat/<version>/tablex/`, see `fixtures/compat/README.md`) must load
//! with the current code, keep every row and value through a round trip, and still open the saved
//! secrets. The fixtures hold made-up data only. Never point these tests at a real data dir, and
//! never at the OS credential store: the master key is an in-memory item here.
//!
//! `generate_fixture_set` (ignored) writes the data dir of a release, see the README; the app's
//! documents (session, update check, diagram layouts, conversations) are added to it by the generator of
//! tablex-gpui, which owns their formats.

use engine_connect::credentials::{jump_account, CredentialError, CredentialStore, SecretKind};
use engine_connect::records::{self, ConnectionKind, SavedConnection};
use engine_connect::secret_store::EncryptedStore;
use engine_storage::aggregate::{self, SavedAggregate};
use engine_storage::connection::{self, ProjectRecord, SavedConnectionRecord};
use engine_storage::{app_db, history, pinned_nodes, view_state};
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Mutex, MutexGuard};

/// The releases whose fixture sets are checked. Append the version when its fixtures are added.
/// The data of a release only grows: change no value below once a fixture set holds it, add new
/// rows (the checks look each expected row up, so a later set may hold more).
const VERSIONS: &[&str] = &["v0.1.10"];

/// The master key of the fixtures' secrets: made up, used only here (base64 of 32 bytes `0x24`).
const FIXTURE_KEY: &str = "JCQkJCQkJCQkJCQkJCQkJCQkJCQkJCQkJCQkJCQkJCQ=";

const SHOP: &str = "AAAAAAAA-0000-4000-8000-000000000001";
const LOCAL: &str = "AAAAAAAA-0000-4000-8000-000000000002";
const ANALYTICS: &str = "AAAAAAAA-0000-4000-8000-000000000003";
const ORDERS: &str = "AAAAAAAA-0000-4000-8000-000000000004";
const CACHE: &str = "AAAAAAAA-0000-4000-8000-000000000005";
const EVENTS: &str = "AAAAAAAA-0000-4000-8000-000000000006";
const LEGACY: &str = "AAAAAAAA-0000-4000-8000-000000000007";
const UNKNOWN_TYPE: &str = "AAAAAAAA-0000-4000-8000-000000000008";

/// The SSH tunnel of the shop connection as the app writes it (no secrets: those are in the
/// secrets table).
const SSH_JSON: &str = r#"{"ssh":{"isEnabled":true,"host":"bastion.example.com","port":2200,"user":"deploy","authMethod":"key","privateKeyPath":"/keys/id_example","remoteHost":"10.0.0.9","remotePort":"5432","saveSecrets":true},"proxy":{"isEnabled":false}}"#;

/// Everything below is the data of the fixture sets; the generator writes it, the tests read it
/// back. Values are made up (example.com hosts, fake names).
fn record(
    id: &str,
    name: &str,
    uri: &str,
    db_type: &str,
    color: &str,
    pinned: bool,
    connection_type: &str,
    overrides: &str,
    network: &str,
) -> SavedConnectionRecord {
    SavedConnectionRecord {
        id: id.into(),
        name: name.into(),
        uri: uri.into(),
        db_type: db_type.into(),
        color_hex: color.into(),
        save_password: !connection_type.is_empty() && id != LOCAL,
        save_proxy_password: false,
        network_settings_json: network.into(),
        is_pinned: pinned,
        last_connected: Some(1_700_000_000.5),
        created_at: 1_690_000_000.25,
        updated_at: 1_700_000_100.75,
        connection_type: connection_type.into(),
        overrides_json: overrides.into(),
    }
}

fn connections() -> Vec<SavedConnectionRecord> {
    vec![
        record(SHOP, "Shop (production)", "postgresql://shop_app@pg.example.com:5432/shop", "postgresql", "#E11D48", true, "production", r#"{"query.query_timeout_secs":30}"#, SSH_JSON),
        record(LOCAL, "Local file", "sqlite:///Users/example/data/demo.db", "sqlite", "", false, "", "", "{}"),
        record(ANALYTICS, "Analytics", "mysql://reader@mysql.example.com:3306/analytics", "mysql", "#0EA5E9", false, "test", "", r#"{"ssh":{"isEnabled":false},"proxy":{"isEnabled":false}}"#),
        record(ORDERS, "Orders store", "mongodb://mongo.example.com:27017/orders?replicaSet=rs0", "mongodb", "#22C55E", false, "development", "", "{}"),
        record(CACHE, "Cache", "redis://:@redis.example.com:6379", "redis", "#F59E0B", false, "", "", "{}"),
        record(EVENTS, "Events", "clickhouse://default@ch.example.com:9000/events", "clickhouse", "", false, "", "", "{}"),
        record(LEGACY, "Old reports", "mariadb://root@maria.example.com:3306/legacy", "mariadb", "#A855F7", false, "", "", "{}"),
        // A type this build does not drive: kept, shown disabled, never deleted.
        record(UNKNOWN_TYPE, "Unsupported server", "oracle://app@ora.example.com:1521/xe", "oracle", "#64748B", false, "", "", "{}"),
    ]
}

fn projects() -> Vec<ProjectRecord> {
    vec![
        ProjectRecord { id: "General".into(), name: "General".into(), path: String::new(), uses_default_location: true, created_at: 1_690_000_000.0, updated_at: 1_690_000_000.0 },
        ProjectRecord { id: "reports".into(), name: "Reports".into(), path: "/Users/example/reports".into(), uses_default_location: false, created_at: 1_691_000_000.0, updated_at: 1_692_000_000.0 },
    ]
}

fn aggregates() -> Vec<SavedAggregate> {
    vec![
        SavedAggregate {
            id: "agg-1".into(),
            conn_uuid: ORDERS.into(),
            database: "orders".into(),
            collection: "carts".into(),
            name: "Open carts by user".into(),
            kind: "aggregate".into(),
            pipeline_json: r#"[{"$match":{"open":true}},{"$group":{"_id":"$user","n":{"$sum":1}}}]"#.into(),
            created_at: 1_695_000_000.0,
            updated_at: 1_695_000_500.0,
            options_json: Some(r#"{"allowDiskUse":true,"maxTimeMs":5000}"#.into()),
        },
        SavedAggregate {
            id: "agg-2".into(),
            conn_uuid: ORDERS.into(),
            database: "orders".into(),
            collection: String::new(),
            name: "Database stats".into(),
            kind: "aggregate".into(),
            pipeline_json: "[]".into(),
            created_at: 1_695_100_000.0,
            updated_at: 1_695_100_000.0,
            options_json: None,
        },
    ]
}

/// `(connection, database, collection, filter, project, sort, skip, limit, max_time_ms, result_count)`.
type HistoryRow = (&'static str, &'static str, &'static str, &'static str, &'static str, &'static str, i64, i64, i64, i64);

const HISTORY: &[HistoryRow] = &[
    (ORDERS, "orders", "carts", r#"{"open":true}"#, r#"{"user":1}"#, r#"{"_id":-1}"#, 0, 50, 1000, 12),
    (ORDERS, "orders", "carts", r#"{"open":false}"#, "", "", 10, 20, 0, 3),
    (ORDERS, "orders", "users", r#"{"name":"Ada Example"}"#, "", "", 0, 0, 0, 1),
];

const VIEW_STATE: &[(&str, &str)] = &[
    (SHOP, r#"{"expanded":["shop","shop.public"],"selected":"shop.public.orders"}"#),
    (ORDERS, r#"{"expanded":["orders"]}"#),
];

/// `(connection, node)` as `NavigatorNodeId::pin_key` writes them.
const PINS: &[(&str, &str)] = &[
    (SHOP, r#"["database","shop"]"#),
    (SHOP, r#"["object","shop","public","orders"]"#),
    (ORDERS, r#"["node","orders.carts"]"#),
];

/// The secrets of the fixtures: `(kind, account, value)`. Fake values.
fn secrets() -> Vec<(SecretKind, String, &'static str)> {
    vec![
        (SecretKind::Database, SHOP.into(), "example-db-password-1"),
        (SecretKind::Database, ANALYTICS.into(), "example-db-password-3"),
        (SecretKind::Proxy, SHOP.into(), "example-proxy-password"),
        (SecretKind::SshPassword, SHOP.into(), "example-ssh-password"),
        (SecretKind::SshPassphrase, SHOP.into(), "example-ssh-passphrase"),
        (SecretKind::SshPassword, jump_account(SHOP, 0), "example-jump-password"),
        (SecretKind::AiApiKey, "anthropic".into(), "example-not-a-real-key"),
    ]
}

/// The OS credential store, in memory: the master key only.
#[derive(Default)]
struct Os {
    items: Mutex<HashMap<(String, String), String>>,
}

fn service(kind: SecretKind) -> String {
    format!("{kind:?}")
}

impl CredentialStore for Os {
    fn load(&self, kind: SecretKind, account: &str) -> Result<Option<String>, CredentialError> {
        Ok(self.items.lock().unwrap().get(&(service(kind), account.to_owned())).cloned())
    }
    fn write(&self, kind: SecretKind, account: &str, password: &str) -> Result<(), CredentialError> {
        self.items.lock().unwrap().insert((service(kind), account.to_owned()), password.to_owned());
        Ok(())
    }
    fn delete(&self, kind: SecretKind, account: &str) -> Result<(), CredentialError> {
        self.items.lock().unwrap().remove(&(service(kind), account.to_owned()));
        Ok(())
    }
}

fn secret_store(db: &Path) -> EncryptedStore {
    let os = Os::default();
    os.write(SecretKind::MasterKey, "master-key", FIXTURE_KEY).unwrap();
    EncryptedStore::new(Box::new(os), Some(db.to_path_buf()))
}

/// The tests of this binary share one data dir (the stores are process-wide) and take turns.
fn serial() -> MutexGuard<'static, ()> {
    static LOCK: Mutex<()> = Mutex::new(());
    LOCK.lock().unwrap_or_else(|poisoned| poisoned.into_inner())
}

fn fixture_root(version: &str) -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../../../fixtures/compat").join(version)
}

fn copy_dir(from: &Path, to: &Path) {
    std::fs::create_dir_all(to).unwrap();
    for entry in std::fs::read_dir(from).unwrap() {
        let entry = entry.unwrap();
        let target = to.join(entry.file_name());
        if entry.file_type().unwrap().is_dir() {
            copy_dir(&entry.path(), &target);
        } else {
            std::fs::copy(entry.path(), target).unwrap();
        }
    }
}

/// A copy of a release's fixture data dir, used as `TABLEX_DATA_DIR` of this process. The stores
/// keep one database per process, so each release is checked in a process of its own (see
/// `every_released_version_...`).
fn use_fixture(version: &str) -> (tempfile::TempDir, PathBuf) {
    let dir = tempfile::tempdir().unwrap();
    copy_dir(&fixture_root(version).join("tablex"), &dir.path().join("tablex"));
    std::env::set_var(engine_storage::paths::DATA_DIR_ENV, dir.path());
    let db = dir.path().join("tablex").join(app_db::FILE_NAME);
    (dir, db)
}

/// Writes the data dir of a release into `$TABLEX_COMPAT_OUT/tablex/tablex.db`, with the
/// connections, projects, aggregates, history, view state, pins, sealed
/// secrets above. Run at the release commit:
/// `TABLEX_COMPAT_OUT=<repo>/fixtures/compat/vX.Y.Z cargo test -p engine-connect --test upgrade_compat generate_fixture_set -- --ignored`
#[test]
#[ignore = "writes the fixture set of a release (see fixtures/compat/README.md)"]
fn generate_fixture_set() {
    let _turn = serial();
    let out = PathBuf::from(std::env::var_os("TABLEX_COMPAT_OUT").expect("TABLEX_COMPAT_OUT names the output dir"));
    assert!(out.is_absolute(), "TABLEX_COMPAT_OUT must be absolute");
    assert!(!out.join("tablex").join(app_db::FILE_NAME).exists(), "the fixture set exists already");
    std::env::set_var(engine_storage::paths::DATA_DIR_ENV, &out);
    let db = app_db::default_path();

    connection::upsert_connections_batch(&connections()).unwrap();
    for project in projects() {
        connection::upsert_project(&project).unwrap();
    }
    connection::add_project_connection("reports", SHOP).unwrap();
    connection::add_project_connection("reports", ANALYTICS).unwrap();
    for aggregate in aggregates() {
        aggregate::upsert_aggregate_with_options(&aggregate).unwrap();
    }
    for (conn, database, collection, filter, project, sort, skip, limit, max_time, count) in HISTORY {
        history::save_entry(conn, database, collection, filter, project, sort, *skip, *limit, *max_time, *count).unwrap();
    }
    for (conn, state) in VIEW_STATE {
        view_state::save_state(conn, state).unwrap();
    }
    for (conn, node) in PINS {
        pinned_nodes::pin(&db, conn, node).unwrap();
    }
    let store = secret_store(&db);
    for (kind, account, value) in secrets() {
        store.write(kind, &account, value).unwrap();
    }
    // One file: fold the write-ahead log in so no `-wal` / `-shm` is left beside it.
    let conn = app_db::open(&db).unwrap();
    conn.query_row("PRAGMA wal_checkpoint(TRUNCATE)", [], |_| Ok(())).unwrap();
}

fn assert_connections(version: &str) {
    let loaded = connection::list_connections().unwrap();
    assert!(loaded.len() >= connections().len(), "{version}: no saved connection is missing");
    for expected in connections() {
        let found = loaded.iter().find(|record| record.id == expected.id);
        assert_eq!(found, Some(&expected), "{version}: saved connection {}", expected.name);
    }
    // The store's order: pinned first, then by name.
    let order: Vec<_> = loaded.iter().map(|record| (!record.is_pinned, record.name.to_lowercase())).collect();
    assert!(order.windows(2).all(|pair| pair[0] <= pair[1]), "{version}: pinned first, then by name");
    // The reading the app does of every record.
    let by_id: HashMap<_, _> = loaded.iter().map(|record| (record.id.as_str(), SavedConnection::from_record(record))).collect();
    let shop = &by_id[SHOP];
    assert_eq!((shop.kind, shop.save_password, shop.is_pinned, shop.disabled, shop.ssh_tunnel), (ConnectionKind::Postgres, true, true, false, true));
    assert_eq!(by_id[LOCAL].kind, ConnectionKind::Sqlite);
    assert_eq!(by_id[ORDERS].kind, ConnectionKind::Mongodb);
    assert_eq!(by_id[CACHE].kind, ConnectionKind::Redis);
    assert_eq!(by_id[EVENTS].kind, ConnectionKind::Clickhouse);
    assert_eq!(by_id[LEGACY].kind, ConnectionKind::Mariadb);
    assert!(by_id[UNKNOWN_TYPE].disabled, "an unknown type is listed disabled, not dropped");
    assert!(!by_id[ANALYTICS].ssh_tunnel);
    assert_eq!(records::try_load_saved_records().unwrap(), loaded);

    let loaded_projects = connection::list_projects().unwrap();
    for expected in projects() {
        assert_eq!(loaded_projects.iter().find(|project| project.id == expected.id), Some(&expected), "{version}: project {}", expected.id);
    }
    let members = connection::list_project_connection_ids("reports").unwrap();
    for id in [SHOP, ANALYTICS] {
        assert!(members.iter().any(|member| member == id), "{version}: project membership of {id}");
    }
}

fn assert_other_stores(version: &str, db: &Path) {
    for expected in aggregates() {
        let found = aggregate::get_aggregate(&expected.id).unwrap();
        // `SavedAggregate` has no `PartialEq`: its `Debug` shows every field.
        assert_eq!(format!("{found:?}"), format!("Some({expected:?})"), "{version}: aggregate {}", expected.id);
    }
    assert!(aggregate::list_aggregates(ORDERS, "orders").unwrap().len() >= 2);

    let carts = history::fetch_entries(ORDERS, "orders", "carts", 100).unwrap();
    assert!(carts.len() >= 2, "{version}: history");
    for (conn, database, collection, filter, project, sort, skip, limit, max_time, count) in HISTORY {
        let found = history::fetch_entries(conn, database, collection, 100)
            .unwrap()
            .into_iter()
            .find(|entry| entry.filter == *filter)
            .unwrap_or_else(|| panic!("{version}: history entry {filter}"));
        assert_eq!(
            (found.project.as_str(), found.sort.as_str(), found.skip, found.limit, found.max_time_ms, found.result_count),
            (*project, *sort, *skip, *limit, *max_time, *count)
        );
        assert!(found.timestamp > 0.0);
    }

    for (conn, state) in VIEW_STATE {
        assert_eq!(view_state::load_state(conn).unwrap().as_deref(), Some(*state), "{version}: view state");
    }
    let pins = pinned_nodes::list_all(db).unwrap();
    let expected_pins: Vec<_> = PINS.iter().map(|(c, n)| ((*c).to_owned(), (*n).to_owned())).collect();
    assert_eq!(&pins[..expected_pins.len()], &expected_pins[..], "{version}: pins in the order they were made");

    // The session, the update check, the diagram layouts and the conversations are documents of
    // the app: its own tests (`upgrade_compat` of tablex-gpui) read them with its own code.
}

fn assert_secrets(version: &str, db: &Path) {
    let store = secret_store(db);
    for (kind, account, value) in secrets() {
        assert_eq!(store.load(kind, &account).unwrap().as_deref(), Some(value), "{version}: secret {kind:?} {account}");
    }
    assert_eq!(store.load(SecretKind::Database, LOCAL).unwrap(), None, "no password was saved for the file connection");
}

/// Each release's data dir, checked in a process of its own (the stores keep one database per
/// process): this test starts itself once per version with `TABLEX_COMPAT_VERSION` set.
#[test]
fn every_released_version_loads_and_survives_a_round_trip() {
    const SELF: &str = "every_released_version_loads_and_survives_a_round_trip";
    if let Ok(version) = std::env::var("TABLEX_COMPAT_VERSION") {
        return check_version(&version);
    }
    for version in VERSIONS {
        let output = std::process::Command::new(std::env::current_exe().unwrap())
            .args([SELF, "--exact", "--test-threads=1"])
            .env("TABLEX_COMPAT_VERSION", version)
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "{version}\n{}\n{}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
        assert!(String::from_utf8_lossy(&output.stdout).contains("1 passed"), "{version} did not run");
    }
}

fn check_version(version: &str) {
    let _turn = serial();
    let (_dir, db) = use_fixture(version);

    // Loading.
    assert_connections(version);
    assert_other_stores(version, &db);
    assert_secrets(version, &db);

    // The round trip: write through the stores (as the app does on any change), read again.
    let all = connection::list_connections().unwrap();
    connection::upsert_connections_batch(&all).unwrap();
    let mut renamed = all.iter().find(|record| record.id == SHOP).unwrap().clone();
    renamed.name.push_str(" (renamed)");
    connection::upsert_connection(&renamed).unwrap();
    connection::set_connection_pinned(UNKNOWN_TYPE, true).unwrap();
    history::save_entry(ORDERS, "orders", "carts", r#"{"new":1}"#, "", "", 0, 0, 0, 0).unwrap();
    pinned_nodes::pin(&db, ORDERS, r#"["database","orders"]"#).unwrap();
    secret_store(&db).write(SecretKind::Database, LOCAL, "example-new-password").unwrap();

    // Everything that was there is still there, unchanged, next to the new rows.
    let after = connection::list_connections().unwrap();
    assert_eq!(after.len(), all.len());
    assert_eq!(after.iter().find(|r| r.id == SHOP).unwrap().name, renamed.name);
    for before in all.iter().filter(|r| r.id != SHOP && r.id != UNKNOWN_TYPE) {
        assert_eq!(after.iter().find(|r| r.id == before.id), Some(before), "{version}: {} unchanged", before.name);
    }
    assert!(after.iter().find(|r| r.id == UNKNOWN_TYPE).unwrap().is_pinned);
    let pinned_before = pinned_nodes::list_all(&db).unwrap();
    assert!(pinned_before.iter().any(|(c, n)| c == ORDERS && n == r#"["database","orders"]"#));
    assert_eq!(pinned_nodes::list(&db, SHOP).unwrap(), [PINS[0].1.to_owned(), PINS[1].1.to_owned()]);
    for (kind, account, value) in secrets() {
        assert_eq!(secret_store(&db).load(kind, &account).unwrap().as_deref(), Some(value));
    }
    assert_eq!(secret_store(&db).load(SecretKind::Database, LOCAL).unwrap().as_deref(), Some("example-new-password"));
    // Nothing needs the generator's constants any more: the old entries are still found.
    assert!(history::fetch_entries(ORDERS, "orders", "carts", 100).unwrap().len() >= 3);
    assert!(aggregate::list_aggregates(ORDERS, "orders").unwrap().len() >= 2);
}
