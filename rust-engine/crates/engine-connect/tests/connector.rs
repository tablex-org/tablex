//! `Connector` end to end: a temporary connection store, a temporary `settings.json`, the REAL
//! SQLite driver (built by cargo, installed into a temporary driver store and loaded through the
//! same SHA-256-verified path the app uses), and the `engine-agent` tools on top of
//! `AgentConnections`.

use engine_agent::ports::RunRequest;
use engine_agent::{
    AgentAccess, CancelToken, ConnectionCatalog, DriverSessionAdapter, Language, Limits, Session,
    ToolLayer, ToolOutcome,
};
use engine_connect::connect::{
    AgentConnections, Connector, OpenRequest, RefusePrompts, TunnelPrompts,
};
use engine_connect::credentials::{CredentialError, CredentialStore, SecretKind};
use engine_connect::drivers::DriverLoadError;
use engine_connect::records::{ConnectionKind, RuntimeUri, SavedConnection};
use engine_connect::tunnel::ssh::{ConnectFailure, SshSettings};
use engine_driver_host::{InstalledDriverRecord, InstalledDriverStore};
use engine_storage::connection::{upsert_connection as store_upsert, SavedConnectionRecord};
use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::{Arc, Mutex, OnceLock};

const ID: &str = "7D1C0E55-2C79-4D8B-9B19-3A1F0B6A1111";
const SHOWN: &str = "7d1c0e55-2c79-4d8b-9b19-3a1f0b6a1111";

/// The one environment of this test binary: the connection store is a process-wide singleton, so
/// every test shares the temporary data dir and uses its own connection ids.
struct Env {
    dir: tempfile::TempDir,
    drivers: Arc<InstalledDriverStore>,
}

fn env() -> &'static Env {
    static ENV: OnceLock<Env> = OnceLock::new();
    ENV.get_or_init(|| {
        let dir = tempfile::tempdir().unwrap();
        std::env::set_var(engine_storage::paths::DATA_DIR_ENV, dir.path().join("data"));
        let drivers = Arc::new(InstalledDriverStore::new(dir.path().join("drivers")));
        for (id, package, features) in [
            ("sqlite", "dbengine-sqlite", ""),
            ("postgresql", "dbengine-postgres", "abi-v2"),
        ] {
            let driver = built_driver(package, features);
            let sha256 = Sha256::digest(std::fs::read(&driver).unwrap())
                .iter()
                .map(|byte| format!("{byte:02x}"))
                .collect();
            drivers
                .save(InstalledDriverRecord {
                    id: id.into(),
                    path: driver,
                    sha256,
                    ..Default::default()
                })
                .unwrap();
        }
        Env { dir, drivers }
    })
}

/// Every write to the connection store goes through here. The store opens its database on first
/// use, once per process: a test that wrote before `env()` set the temporary data dir opened the
/// user's real store, and every test after it wrote there too.
fn upsert_connection(record: &SavedConnectionRecord) -> Result<(), impl std::fmt::Debug> {
    env();
    store_upsert(record)
}

/// Builds `package` into `rust-engine/target/test-drivers` (a target dir of its own, so the nested
/// cargo does not wait for the lock of the test build) and returns its cdylib. Not stripped: a
/// stripped cdylib can be rejected by dyld.
fn built_driver(package: &str, features: &str) -> PathBuf {
    let root = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../..");
    let target = root.join("target/test-drivers");
    let mut command = std::process::Command::new(env!("CARGO"));
    command
        .args(["build", "--manifest-path"])
        .arg(root.join("Cargo.toml"))
        .args(["-p", package, "--target-dir"])
        .arg(&target)
        .env("CARGO_PROFILE_DEV_STRIP", "none");
    if !features.is_empty() {
        command.args(["--features", features]);
    }
    let output = command.output().expect("cargo build driver");
    assert!(
        output.status.success(),
        "{package} build failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    target.join("debug").join(format!(
        "{}{}{}",
        std::env::consts::DLL_PREFIX,
        package.replace('-', "_"),
        std::env::consts::DLL_SUFFIX
    ))
}

/// A thread-safe in-memory credential store.
#[derive(Default)]
struct Keys(
    Mutex<HashMap<(String, String), String>>,
    std::sync::atomic::AtomicUsize,
);

impl CredentialStore for Keys {
    fn load(&self, kind: SecretKind, account: &str) -> Result<Option<String>, CredentialError> {
        self.1.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        Ok(self
            .0
            .lock()
            .unwrap()
            .get(&(format!("{kind:?}"), account.to_owned()))
            .cloned())
    }
    fn write(
        &self,
        kind: SecretKind,
        account: &str,
        password: &str,
    ) -> Result<(), CredentialError> {
        self.0.lock().unwrap().insert(
            (format!("{kind:?}"), account.to_owned()),
            password.to_owned(),
        );
        Ok(())
    }
    fn delete(&self, kind: SecretKind, account: &str) -> Result<(), CredentialError> {
        self.0
            .lock()
            .unwrap()
            .remove(&(format!("{kind:?}"), account.to_owned()));
        Ok(())
    }
}

fn connector() -> Connector {
    connector_with("settings.json")
}

/// A connector reading its own settings file (the tests run in parallel).
fn connector_with(settings: &str) -> Connector {
    let env = env();
    Connector::new(Arc::clone(&env.drivers))
        .with_credentials(Arc::new(Keys::default()))
        .with_settings_path(Some(env.dir.path().join(settings)))
}

fn prompts() -> Arc<dyn TunnelPrompts> {
    Arc::new(RefusePrompts)
}

fn record(
    id: &str,
    uri: &str,
    network: &str,
    overrides: &str,
    connection_type: &str,
) -> SavedConnectionRecord {
    SavedConnectionRecord {
        id: id.into(),
        name: format!("conn {id}"),
        uri: uri.into(),
        db_type: "sqlite".into(),
        color_hex: "#34C759".into(),
        save_password: false,
        save_proxy_password: false,
        network_settings_json: network.into(),
        is_pinned: false,
        last_connected: None,
        created_at: 1.0,
        updated_at: 1.0,
        connection_type: connection_type.into(),
        overrides_json: overrides.into(),
    }
}

fn sqlite_uri(name: &str) -> String {
    format!("sqlite://{}", env().dir.path().join(name).display())
}

fn write_settings(file: &str, text: &str) {
    std::fs::write(env().dir.path().join(file), text).unwrap();
}

/// Seeds the database through a session of the connector itself, outside the agent policy.
fn seed(connector: &Connector, id: &str, sql: &str) {
    let engine_connect::connect::OpenedConnection {
        session, handle, ..
    } = connector
        .open(id, &prompts())
        .expect("seed session")
        .into_parts();
    let adapter = DriverSessionAdapter::new(handle, session).expect("adapter");
    let limits = Limits::default();
    adapter
        .run(
            &RunRequest {
                language: Language::Sql,
                text: sql,
                database: None,
                limits: &limits,
                row_limit: 10,
                confirmed: true,
                read_only: false,
            },
            &CancelToken::new(),
        )
        .expect("seed the database");
    adapter.disconnect();
}

fn success(outcome: ToolOutcome) -> Value {
    match outcome {
        ToolOutcome::Success(value) => value,
        ToolOutcome::Failure(error) => panic!("tool failed: {} {}", error.code, error.message),
    }
}

fn code(outcome: &ToolOutcome) -> String {
    outcome.code().unwrap_or("ok").to_owned()
}

#[test]
fn a_saved_connection_opens_through_the_installed_driver_and_closes() {
    let db = sqlite_uri("open.db");
    upsert_connection(&record("open-1", &db, "{}", "", "")).unwrap();
    let connection = connector()
        .open("open-1", &prompts())
        .expect("a direct SQLite connection opens");
    let opened = connection.into_parts();
    assert!(opened.tunnel.is_none());
    assert_eq!(opened.uri.expose_for_test(), db, "the URI as saved");
    assert!(
        !format!("{opened:?}", opened = opened.uri).contains(&db),
        "Debug hides the URI"
    );
    drop(opened);
}

#[test]
fn a_missing_driver_is_a_typed_error_for_the_front_end_to_act_on() {
    let _ = env();
    let empty = Arc::new(InstalledDriverStore::new(
        env().dir.path().join("no-drivers"),
    ));
    let connector = Connector::new(empty).with_credentials(Arc::new(Keys::default()));
    upsert_connection(&record("missing-driver", &sqlite_uri("m.db"), "{}", "", "")).unwrap();
    let failure = connector
        .open("missing-driver", &prompts())
        .expect_err("no driver installed");
    assert_eq!(
        failure,
        ConnectFailure::Driver(DriverLoadError::NotInstalled {
            driver_id: "sqlite".into()
        })
    );
    assert!(failure
        .message()
        .contains("open Driver Manager to install it"));
}

#[test]
fn an_unknown_connection_is_an_error() {
    let failure = connector()
        .open("nobody", &prompts())
        .expect_err("no such record");
    assert!(
        failure.message().contains("not found"),
        "{}",
        failure.message()
    );
}

#[test]
fn a_tunnelled_connection_whose_settings_cannot_be_read_never_connects_directly() {
    // The flag is read with the record: unreadable settings count as tunnelled.
    upsert_connection(&record(
        "broken-net",
        &sqlite_uri("b.db"),
        "{ not json",
        "",
        "",
    ))
    .unwrap();
    let failure = connector()
        .open("broken-net", &prompts())
        .expect_err("fails closed");
    assert!(matches!(failure, ConnectFailure::Message(_)), "{failure:?}");
    assert!(!failure.message().is_empty());
}

fn saved_of(record: &SavedConnectionRecord) -> SavedConnection {
    SavedConnection::from_record(record)
}

const BASTION_ON: &str =
    r#"{"ssh":{"isEnabled":true,"host":"bastion","user":"u","authMethod":"agent"}}"#;

#[test]
fn a_tunnel_switch_that_disagrees_with_the_saved_flag_fails_instead_of_going_direct() {
    let connector = connector();
    let flagged = record("flag-on", &sqlite_uri("f.db"), BASTION_ON, "", "");
    upsert_connection(&flagged).unwrap();
    // The record says the tunnel is on, but the settings given say off: never direct.
    let request = OpenRequest::saved(saved_of(&flagged), None, 0, Default::default())
        .with_stored_ssh(Some(Default::default()));
    let failure = connector
        .open_request(request, &prompts())
        .expect_err("expected a tunnel, got none");
    assert!(
        failure.message().contains("tunnel is off"),
        "{}",
        failure.message()
    );
    // A vanished record fails too when the record says tunnelled, and is "no tunnel" when it does not.
    let gone = record("vanished", &sqlite_uri("v.db"), BASTION_ON, "", "");
    let failure = connector
        .open_request(
            OpenRequest::saved(saved_of(&gone), None, 0, Default::default()),
            &prompts(),
        )
        .expect_err("the record the flag came from is not in the store");
    assert!(
        failure.message().contains("not found"),
        "{}",
        failure.message()
    );
    let direct = record("vanished-direct", &sqlite_uri("vd.db"), "{}", "", "");
    connector
        .open_request(
            OpenRequest::saved(saved_of(&direct), None, 0, Default::default()),
            &prompts(),
        )
        .expect("a connection that is not tunnelled opens directly")
        .close()
        .unwrap();
    // A tunnel on a kind that has no network endpoint is refused, never opened directly.
    let failure = connector
        .open_request(
            OpenRequest::saved(saved_of(&flagged), None, 0, Default::default()),
            &prompts(),
        )
        .expect_err("SQLite cannot be tunnelled");
    assert!(
        failure.message().to_lowercase().contains("tunnel")
            || failure.message().contains("network"),
        "{}",
        failure.message()
    );
}

#[test]
fn open_request_with_a_saved_tunnelled_connection_and_a_direct_input_is_refused() {
    let connector = connector();
    let flagged = record("flag-direct", &sqlite_uri("fd.db"), BASTION_ON, "", "");
    let saved = saved_of(&flagged);
    assert!(saved.ssh_tunnel);
    let off = || Some(SshSettings::default());
    // The saved URI, a URI given for it, and settings that say off: all refused.
    for override_uri in [None, Some(RuntimeUri::new(sqlite_uri("fd.db")))] {
        let request = OpenRequest::saved(saved.clone(), override_uri, 0, Default::default())
            .with_stored_ssh(off());
        let failure = connector
            .open_request(request, &prompts())
            .expect_err("never direct");
        assert!(matches!(failure, ConnectFailure::Message(_)), "{failure:?}");
        assert!(
            failure.message().contains("saved as tunnelled"),
            "{}",
            failure.message()
        );
    }
    // Two records whose ids are not UUIDs read as the same id: the one flagged still requires its
    // tunnel, whatever the other one says.
    let plain = record("flag-plain", &sqlite_uri("fp.db"), "{}", "", "");
    upsert_connection(&plain).unwrap();
    let request = OpenRequest::saved(saved.clone(), None, 0, Default::default());
    assert!(connector.open_request(request, &prompts()).is_err());
}

#[test]
fn a_session_cannot_be_opened_directly_for_a_tunnelled_connection() {
    let handle =
        engine_connect::drivers::load_driver(&env().drivers, "sqlite", &sqlite_uri("g.db"))
            .expect("the driver loads");
    let uri = RuntimeUri::new(sqlite_uri("g.db"));
    let tunnelled = saved_of(&record("g-on", &sqlite_uri("g.db"), BASTION_ON, "", ""));
    let refusal = connector()
        .open_session(&handle, &tunnelled, &uri, 0, None, &prompts())
        .err()
        .expect("the connection requires a tunnel, none given");
    assert!(
        refusal
            .message()
            .starts_with("Refusing to connect directly"),
        "{}",
        refusal.message()
    );
    let direct = saved_of(&record("g-off", &sqlite_uri("g.db"), "{}", "", ""));
    let (session, tunnel) = connector()
        .open_session(&handle, &direct, &uri, 0, None, &prompts())
        .expect("a direct session when none is required");
    assert!(tunnel.is_none());
    drop(session);
}

#[test]
fn a_form_that_is_not_saved_opens_with_the_uri_it_is_given() {
    let uri = sqlite_uri("form.db");
    let form = |uri: String, ssh| {
        OpenRequest::form(
            ConnectionKind::Sqlite,
            RuntimeUri::new(uri),
            ssh,
            Default::default(),
            Vec::new(),
        )
    };
    let connection = connector()
        .open_request(form(uri.clone(), None), &prompts())
        .expect("opens");
    let opened = connection.into_parts();
    assert_eq!(opened.uri.expose_for_test(), uri);
    drop(opened);
    // A different scheme than the driver serves is refused before connecting.
    let wrong = connector()
        .open_request(form("postgres://h/d".into(), None), &prompts())
        .expect_err("scheme mismatch");
    assert!(
        matches!(wrong, ConnectFailure::Driver(DriverLoadError::Mismatch(_))),
        "{wrong:?}"
    );
    // A tunnel tab that is on is a tunnel that is required: SQLite has none to give.
    let on = SshSettings {
        enabled: true,
        host: "bastion".into(),
        user: "u".into(),
        ..SshSettings::default()
    };
    let refused = connector()
        .open_request(form(uri, Some(on)), &prompts())
        .expect_err("the form asks for a tunnel");
    assert!(matches!(refused, ConnectFailure::Message(_)), "{refused:?}");
}

#[test]
fn the_only_ways_to_get_a_driver_session_are_open_open_request_and_open_session() {
    // The sources of this crate: `handle.connect(` appears once, inside `connect_session`, which
    // `open_request` and `open_session` reach after deriving the tunnel requirement themselves.
    let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("src");
    let mut hits = Vec::new();
    for file in [
        "connect.rs",
        "drivers.rs",
        "records.rs",
        "profile.rs",
        "credentials.rs",
    ] {
        let text = std::fs::read_to_string(root.join(file)).unwrap();
        let production = text.split("\n#[cfg(test)]\nmod tests").next().unwrap();
        hits.extend(production.match_indices(".connect(").map(|_| file));
    }
    assert_eq!(hits, ["connect.rs"], "one call site: {hits:?}");
    let text = std::fs::read_to_string(root.join("connect.rs")).unwrap();
    let production = text.split("\n#[cfg(test)]\nmod tests").next().unwrap();
    let call = production
        .find(".connect(&request_json)")
        .expect("the call");
    let function = production[..call]
        .rfind("fn connect_session")
        .expect("inside connect_session");
    assert_eq!(
        production[function..call]
            .matches("guard_direct(expected")
            .count(),
        1,
        "the guard precedes the call"
    );
}

#[test]
fn the_agent_tools_run_end_to_end_over_connections_opened_by_the_connector() {
    let db = sqlite_uri("agent.db");
    write_settings(
        "agent.json",
        r#"{"connection_types":[{"id":"test","name":"Test","agent_access":"read"}]}"#,
    );
    upsert_connection(&record(ID, &db, "{}", "", "")).unwrap();
    let connector = connector_with("agent.json");
    let agent = Arc::new(AgentConnections::new(connector.clone(), prompts()));
    let layer = ToolLayer::new(agent.clone(), agent.clone(), "test");

    // 1. Seed the database through a connection of the connector itself.
    seed(
        &connector,
        ID,
        "CREATE TABLE users(id INTEGER PRIMARY KEY, email TEXT NOT NULL); \
         INSERT INTO users(email) VALUES ('a@x.io'), ('b@x.io');",
    );

    // 2. The default is `off`: listed by name, never openable.
    let listed = success(layer.call("list_connections", json!({})));
    let entry = listed["connections"]
        .as_array()
        .unwrap()
        .iter()
        .find(|c| c["id"] == SHOWN)
        .cloned()
        .unwrap();
    assert_eq!(entry["access"], "off");
    assert!(entry.get("uri").is_none() && !listed.to_string().contains("agent.db"));
    let denied = layer.call("open_session", json!({"connection_id": SHOWN}));
    assert_eq!(code(&denied), "agent.access_denied");

    // 3. The connection's own setting applies at once (the catalog is read on every call).
    upsert_connection(&record(ID, &db, "{}", r#"{"agent.access":"schema"}"#, "")).unwrap();
    success(layer.call("open_session", json!({"connection_id": SHOWN})));
    let tables = success(layer.call("list_tables", json!({"connection_id": SHOWN})));
    assert!(tables.to_string().contains("users"), "{tables}");
    let described = layer.call(
        "execute_query",
        json!({"connection_id": SHOWN, "statement": "SELECT 1"}),
    );
    assert_eq!(
        code(&described),
        "agent.access_denied",
        "schema_only never returns rows"
    );
    success(layer.call("close_session", json!({"connection_id": SHOWN})));

    // 4. `read` runs reads and refuses writes.
    upsert_connection(&record(ID, &db, "{}", r#"{"agent.access":"read"}"#, "")).unwrap();
    success(layer.call("open_session", json!({"connection_id": SHOWN})));
    let rows = success(layer.call(
        "execute_query",
        json!({"connection_id": SHOWN, "statement": "SELECT 41 + 1 AS answer"}),
    ));
    assert!(rows.to_string().contains("42"), "{rows}");
    let write = layer.call(
        "execute_query",
        json!({"connection_id": SHOWN, "statement": "CREATE TABLE t(x)"}),
    );
    assert_eq!(code(&write), "agent.write_refused");
    success(layer.call("close_session", json!({"connection_id": SHOWN})));
    assert!(!layer.is_open(SHOWN));

    // 5. The type's preset applies to connections that set nothing, capped by the type.
    upsert_connection(&record(ID, &db, "{}", "", "test")).unwrap();
    let info = agent.get(SHOWN).unwrap();
    assert_eq!(info.effective_access(), AgentAccess::Read);
    upsert_connection(&record(ID, &db, "{}", "", "production")).unwrap();
    assert_eq!(
        agent.get(SHOWN).unwrap().effective_access(),
        AgentAccess::Off,
        "production is explicit-only"
    );
}

#[test]
fn an_unreadable_settings_file_turns_every_connection_off() {
    let db = sqlite_uri("closed.db");
    upsert_connection(&record(
        "0A0A0A0A-0000-4000-8000-000000000001",
        &db,
        "{}",
        r#"{"agent.access":"read_write"}"#,
        "",
    ))
    .unwrap();
    write_settings("closed.json", "{ broken");
    let agent = AgentConnections::new(connector_with("closed.json"), prompts());
    let listed = agent.list();
    assert!(!listed.is_empty());
    assert!(
        listed
            .iter()
            .all(|info| info.effective_access() == AgentAccess::Off),
        "{listed:?}"
    );
    write_settings("closed.json", "{}");
    assert!(agent
        .list()
        .iter()
        .any(|info| info.effective_access() == AgentAccess::ReadWrite));
}

const PG_ID: &str = "5B1D2C3E-1111-4222-8333-444455556666";

#[test]
fn an_agent_open_on_an_unknown_host_key_returns_a_typed_error_and_records_nothing() {
    use engine_connect::tunnel::ssh::fake::{unknown_host, FakeOpener};
    let password = "needle-db-password-91";
    let keys = Arc::new(Keys::default());
    keys.write(SecretKind::Database, PG_ID, password).unwrap();
    write_settings("hostkey.json", "{}");
    let mut tunnelled = record(
        PG_ID,
        "postgresql://alice@db.example.com:5432/app",
        r#"{"ssh":{"isEnabled":true,"host":"bastion.example.com","user":"deploy","authMethod":"agent"}}"#,
        r#"{"agent.access":"read"}"#,
        "",
    );
    tunnelled.db_type = "postgresql".into();
    tunnelled.save_password = true;
    upsert_connection(&tunnelled).unwrap();

    let opener = FakeOpener::new();
    opener.then(Err(unknown_host("bastion.example.com")));
    let known_hosts = env().dir.path().join("known_hosts-agent");
    let connector = Connector::new(Arc::clone(&env().drivers))
        .with_credentials(keys)
        .with_tunnels(opener.clone(), Some(known_hosts.clone()))
        .with_settings_path(Some(env().dir.path().join("hostkey.json")));
    let agent = Arc::new(AgentConnections::new(connector, Arc::new(RefusePrompts)));
    let layer = ToolLayer::new(agent.clone(), agent.clone(), "test");

    let outcome = layer.call(
        "open_session",
        json!({"connection_id": PG_ID.to_lowercase()}),
    );
    let ToolOutcome::Failure(error) = outcome else {
        panic!("an unknown bastion key must not open a session");
    };
    assert_eq!(error.code, "tunnel.host_key_unknown", "{}", error.message);
    assert!(
        error.message.contains("Connect to this database once"),
        "{}",
        error.message
    );
    for secret in [password, "alice:"] {
        assert!(!error.message.contains(secret), "{}", error.message);
    }
    // The model is not told where the bastion is, or who logs in there.
    for address in [
        "bastion.example.com",
        "deploy",
        ":22",
        "ssh-ed25519",
        "SHA256",
    ] {
        assert!(!error.message.contains(address), "{}", error.message);
    }
    assert!(
        opener.trusted.lock().unwrap().is_empty(),
        "no key was recorded"
    );
    assert!(!known_hosts.exists(), "and no known_hosts file was written");
    assert_eq!(
        opener.opened_count(),
        1,
        "one attempt, no retry with a guessed answer"
    );
    assert!(!layer.is_open(&PG_ID.to_lowercase()));
}

#[test]
fn an_open_agent_session_ends_when_access_is_lowered_or_the_connection_is_revoked() {
    let db = sqlite_uri("revoke.db");
    const REV: &str = "6C2E3D4F-1111-4222-8333-444455556667";
    let shown = REV.to_lowercase();
    write_settings("revoke.json", "{}");
    upsert_connection(&record(REV, &db, "{}", r#"{"agent.access":"read"}"#, "")).unwrap();
    let agent = Arc::new(AgentConnections::new(
        connector_with("revoke.json"),
        prompts(),
    ));
    let layer = ToolLayer::new(agent.clone(), agent.clone(), "test");

    success(layer.call("open_session", json!({"connection_id": shown})));
    assert!(layer.is_open(&shown));
    // The user lowers the level in the app: the next call finds `off` and closes the session.
    upsert_connection(&record(REV, &db, "{}", r#"{"agent.access":"off"}"#, "")).unwrap();
    let outcome = layer.call("list_tables", json!({"connection_id": shown}));
    assert_eq!(code(&outcome), "agent.access_denied");
    assert!(
        !layer.is_open(&shown),
        "the session did not outlive the permission"
    );

    // An explicit revoke (a connection edited or deleted) is the same, and harmless when nothing is open.
    upsert_connection(&record(REV, &db, "{}", r#"{"agent.access":"read"}"#, "")).unwrap();
    success(layer.call("open_session", json!({"connection_id": shown})));
    layer.revoke(&shown);
    agent.revoke(&shown);
    agent.revoke(&shown);
    assert!(!layer.is_open(&shown));
    // The store keeps the upper-case spelling and the layer shows the lower-case one; a front end
    // that holds the stored id (the in-app assistant) reaches the same session with it.
    success(layer.call("open_session", json!({"connection_id": shown})));
    assert!(layer.is_open(REV), "is_open takes the stored spelling");
    layer.revoke(REV);
    assert!(!layer.is_open(&shown), "so does revoke");
    success(layer.call("open_session", json!({"connection_id": REV})));
    assert!(layer.is_open(&shown), "and so does a call");
    layer.revoke(&shown);
    // Deleted: the id is unknown at the next call.
    success(layer.call("open_session", json!({"connection_id": shown})));
    engine_storage::connection::delete_connection(REV).unwrap();
    assert_eq!(
        code(&layer.call("list_tables", json!({"connection_id": shown}))),
        "agent.invalid_arguments"
    );
    assert!(!layer.is_open(&shown));
}

#[test]
fn the_catalog_reports_the_explicit_level_even_when_the_overrides_text_is_damaged() {
    const DAM: &str = "7D2F0E55-2C79-4D8B-9B19-3A1F0B6A3333";
    write_settings(
        "damaged.json",
        r#"{"connection_types":[{"id":"development","name":"Development","agent_access":"read"}]}"#,
    );
    let agent = AgentConnections::new(connector_with("damaged.json"), prompts());
    let shown = DAM.to_lowercase();
    // A non-scalar value next to the level, a null level, truncated text: never the type's `read`.
    for (overrides, expected) in [
        (
            r#"{"results.page_size":[1,2],"agent.access":"off"}"#,
            AgentAccess::Off,
        ),
        (
            r#"{"x":{"a":1},"agent.access":"schema"}"#,
            AgentAccess::SchemaOnly,
        ),
        (r#"{"agent.access":null}"#, AgentAccess::Off),
        (r#"{"agent.access":"off""#, AgentAccess::Off),
        (r#"{"results.page_size":50}"#, AgentAccess::Read),
    ] {
        upsert_connection(&record(
            DAM,
            &sqlite_uri("dam.db"),
            "{}",
            overrides,
            "development",
        ))
        .unwrap();
        let info = agent.get(&shown).expect("listed");
        assert_eq!(info.effective_access(), expected, "{overrides}");
    }
}

#[test]
fn a_connection_whose_type_disappeared_is_capped_at_read_for_the_agent() {
    const GONE: &str = "7D2F0E55-2C79-4D8B-9B19-3A1F0B6A4444";
    write_settings("gone-type.json", "{}");
    let agent = AgentConnections::new(connector_with("gone-type.json"), prompts());
    upsert_connection(&record(
        GONE,
        &sqlite_uri("gone.db"),
        "{}",
        r#"{"agent.access":"read_write"}"#,
        "custom-that-was-read-only",
    ))
    .unwrap();
    let info = agent.get(&GONE.to_lowercase()).unwrap();
    assert_eq!(info.effective_access(), AgentAccess::Read);
    upsert_connection(&record(
        GONE,
        &sqlite_uri("gone.db"),
        "{}",
        r#"{"agent.access":"read_write"}"#,
        "",
    ))
    .unwrap();
    assert_eq!(
        agent.get(&GONE.to_lowercase()).unwrap().effective_access(),
        AgentAccess::ReadWrite,
        "no type: unchanged"
    );
}

#[test]
fn editing_a_connections_address_between_two_calls_closes_the_session_to_the_old_target() {
    const EDIT: &str = "6C2E3D4F-1111-4222-8333-444455556668";
    let shown = EDIT.to_lowercase();
    write_settings("edit.json", "{}");
    upsert_connection(&record(
        EDIT,
        &sqlite_uri("edit-a.db"),
        "{}",
        r#"{"agent.access":"read"}"#,
        "",
    ))
    .unwrap();
    let agent = Arc::new(AgentConnections::new(
        connector_with("edit.json"),
        prompts(),
    ));
    let layer = ToolLayer::new(agent.clone(), agent.clone(), "test");
    success(layer.call("open_session", json!({"connection_id": shown})));
    success(layer.call("list_namespaces", json!({"connection_id": shown})));
    // Same record, saved again unchanged: the session stays.
    upsert_connection(&record(
        EDIT,
        &sqlite_uri("edit-a.db"),
        "{}",
        r#"{"agent.access":"read"}"#,
        "",
    ))
    .unwrap();
    success(layer.call("list_namespaces", json!({"connection_id": shown})));
    // The address is edited (access still `read`): the next call finds the session closed.
    upsert_connection(&record(
        EDIT,
        &sqlite_uri("edit-b.db"),
        "{}",
        r#"{"agent.access":"read"}"#,
        "",
    ))
    .unwrap();
    let outcome = layer.call("list_namespaces", json!({"connection_id": shown}));
    assert_eq!(code(&outcome), "agent.not_connected");
    assert!(
        !layer.is_open(&shown),
        "the session to the old target is gone"
    );
    // open_session connects to the new address.
    let reopened = success(layer.call("open_session", json!({"connection_id": shown})));
    assert_eq!(reopened["already_open"], false);
    success(layer.call("list_namespaces", json!({"connection_id": shown})));
}

#[test]
fn a_connection_that_saves_nothing_reads_nothing_per_agent_open() {
    const ONCE: &str = "6C2E3D4F-1111-4222-8333-444455556669";
    write_settings("once.json", "{}");
    upsert_connection(&record(
        ONCE,
        &sqlite_uri("once.db"),
        "{}",
        r#"{"agent.access":"read"}"#,
        "",
    ))
    .unwrap();
    let keys = Arc::new(Keys::default());
    let connector = connector_with("once.json").with_credentials(keys.clone());
    let agent = Arc::new(AgentConnections::new(connector, prompts()));
    let layer = ToolLayer::new(agent.clone(), agent.clone(), "test");
    success(layer.call(
        "open_session",
        json!({"connection_id": ONCE.to_lowercase()}),
    ));
    // Only what the record uses is read: no saved password, no tunnel, no read at all.
    assert_eq!(keys.1.load(std::sync::atomic::Ordering::SeqCst), 0);
}

#[test]
fn an_agent_open_that_fails_tells_the_model_no_address_detail() {
    const BAD: &str = "6C2E3D4F-1111-4222-8333-44445555666A";
    const GONE: &str = "6C2E3D4F-1111-4222-8333-44445555666B";
    write_settings("noaddr.json", "{}");
    // 1. A SQLite file whose directory does not exist: the driver's error names the path.
    let missing_dir = "no-such-dir-fixture-9f3a";
    upsert_connection(&record(
        BAD,
        &sqlite_uri(&format!("{missing_dir}/app-fixture.db")),
        "{}",
        r#"{"agent.access":"read"}"#,
        "",
    ))
    .unwrap();
    let agent = Arc::new(AgentConnections::new(
        connector_with("noaddr.json"),
        prompts(),
    ));
    let layer = ToolLayer::new(agent.clone(), agent.clone(), "test");
    let outcome = layer.call("open_session", json!({"connection_id": BAD.to_lowercase()}));
    let ToolOutcome::Failure(error) = outcome else {
        panic!("a database file in a missing directory must not open");
    };
    assert_eq!(error.code, "connect.failed", "{}", error.message);
    assert!(error.message.contains("conn 6C2E"), "{}", error.message);
    let data_dir = env().dir.path().display().to_string();
    for detail in [
        missing_dir,
        "app-fixture",
        data_dir.as_str(),
        "unable to open",
    ] {
        assert!(
            !error.message.contains(detail),
            "{detail} in {}",
            error.message
        );
    }
    // 2. A driver whose artifact vanished from the store: the store path stays out.
    let store = Arc::new(InstalledDriverStore::new(
        env().dir.path().join("vanished-drivers"),
    ));
    store
        .save(InstalledDriverRecord {
            id: "sqlite".into(),
            path: env()
                .dir
                .path()
                .join("vanished-drivers/libsqlite-fixture.dylib"),
            sha256: "0".repeat(64),
            ..Default::default()
        })
        .unwrap();
    upsert_connection(&record(
        GONE,
        &sqlite_uri("gone.db"),
        "{}",
        r#"{"agent.access":"read"}"#,
        "",
    ))
    .unwrap();
    let connector = Connector::new(store)
        .with_credentials(Arc::new(Keys::default()))
        .with_settings_path(Some(env().dir.path().join("noaddr.json")));
    let agent = Arc::new(AgentConnections::new(connector, prompts()));
    let layer = ToolLayer::new(agent.clone(), agent.clone(), "test");
    let ToolOutcome::Failure(error) = layer.call(
        "open_session",
        json!({"connection_id": GONE.to_lowercase()}),
    ) else {
        panic!("a driver without its artifact must not open");
    };
    assert_eq!(error.code, "connect.driver", "{}", error.message);
    for detail in ["vanished-drivers", "libsqlite-fixture", data_dir.as_str()] {
        assert!(
            !error.message.contains(detail),
            "{detail} in {}",
            error.message
        );
    }
}

#[test]
fn the_agent_never_masks_a_password_that_is_the_user_or_the_database_name() {
    const NAMED: &str = "6C2E3D4F-1111-4222-8333-44445555666C";
    write_settings("named.json", "{}");
    let mut named = record(
        NAMED,
        "postgresql://app%2Duser@db.example.com:5432/shop?dbname=analytics",
        "{}",
        r#"{"agent.access":"read"}"#,
        "",
    );
    named.db_type = "postgresql".into();
    upsert_connection(&named).unwrap();
    let agent = AgentConnections::new(connector_with("named.json"), prompts());
    let info = ConnectionCatalog::get(&agent, &NAMED.to_lowercase()).expect("listed");
    let mut words = engine_agent::SessionProvider::plain_words(&agent, &info);
    words.sort();
    assert_eq!(words, ["analytics", "app-user", "shop"]);
}

// ---- the credential store is read for what the connection saves, once ---------------------------

/// A credential store that fails every read, and counts them.
#[derive(Default)]
struct Locked(std::sync::atomic::AtomicUsize);

impl Locked {
    fn reads(&self) -> usize {
        self.0.load(std::sync::atomic::Ordering::SeqCst)
    }
}

impl CredentialStore for Locked {
    fn load(&self, _: SecretKind, _: &str) -> Result<Option<String>, CredentialError> {
        self.0.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        Err(CredentialError::Platform("keychain locked".into()))
    }
    fn write(&self, _: SecretKind, _: &str, _: &str) -> Result<(), CredentialError> {
        Ok(())
    }
    fn delete(&self, _: SecretKind, _: &str) -> Result<(), CredentialError> {
        Ok(())
    }
}

fn agent_over(
    credentials: Arc<dyn CredentialStore + Send + Sync>,
    drivers: Arc<InstalledDriverStore>,
    settings: &str,
) -> (Arc<AgentConnections>, ToolLayer) {
    write_settings(settings, "{}");
    let connector = Connector::new(drivers)
        .with_credentials(credentials)
        .with_settings_path(Some(env().dir.path().join(settings)));
    let agent = Arc::new(AgentConnections::new(connector, prompts()));
    let layer = ToolLayer::new(agent.clone(), agent.clone(), "test");
    (agent, layer)
}

fn postgres_record(id: &str, network: &str) -> SavedConnectionRecord {
    let mut record = record(
        id,
        "postgresql://alice@db.example.com:5432/app",
        network,
        r#"{"agent.access":"read"}"#,
        "",
    );
    record.db_type = "postgresql".into();
    record.save_password = true;
    record
}

#[test]
fn a_connection_that_saves_nothing_opens_even_when_the_credential_store_cannot_be_read() {
    const LOCKED_SQLITE: &str = "7A1B2C3D-0000-4000-8000-000000000011";
    upsert_connection(&record(
        LOCKED_SQLITE,
        &sqlite_uri("locked.db"),
        "{}",
        r#"{"agent.access":"read"}"#,
        "",
    ))
    .unwrap();
    let store = Arc::new(Locked::default());
    let (_, layer) = agent_over(store.clone(), Arc::clone(&env().drivers), "locked.json");
    let shown = LOCKED_SQLITE.to_lowercase();
    success(layer.call("open_session", json!({"connection_id": shown})));
    let rows = success(layer.call(
        "execute_query",
        json!({"connection_id": shown, "statement": "SELECT 41 + 1 AS answer"}),
    ));
    assert!(rows.to_string().contains("42"), "{rows}");
    assert_eq!(
        store.reads(),
        0,
        "nothing saved, nothing asked of the store"
    );
    success(layer.call("close_session", json!({"connection_id": shown})));
}

#[test]
fn a_saved_password_in_a_store_that_cannot_be_read_still_fails_the_agent_open_closed() {
    const LOCKED_PG: &str = "7A1B2C3D-0000-4000-8000-000000000012";
    upsert_connection(&postgres_record(LOCKED_PG, "{}")).unwrap();
    let store = Arc::new(Locked::default());
    let (_, layer) = agent_over(store.clone(), Arc::clone(&env().drivers), "locked-pg.json");
    let outcome = layer.call(
        "open_session",
        json!({"connection_id": LOCKED_PG.to_lowercase()}),
    );
    let ToolOutcome::Failure(error) = outcome else {
        panic!("a store that cannot be read must not open a session");
    };
    assert_eq!(error.code, "credentials.unavailable", "{}", error.message);
    assert!(
        error.message.contains("keychain locked"),
        "{}",
        error.message
    );
    assert_eq!(store.reads(), 1, "one failed read, no retries");
}

#[test]
fn every_saved_item_is_read_once_per_open_and_the_masks_come_from_that_read() {
    use engine_connect::tunnel::ssh::fake::{unknown_host, FakeOpener};
    const ONCE: &str = "7A1B2C3D-0000-4000-8000-000000000013";
    let keys = Arc::new(Keys::default());
    keys.write(SecretKind::Database, ONCE, "needle-db-password-93")
        .unwrap();
    keys.write(SecretKind::SshPassword, ONCE, "needle-ssh-password-93")
        .unwrap();
    // A password-authenticated tunnel with both saved: the open reaches the tunnel, which asks
    // for the SSH password, after the URI and the driver are settled.
    upsert_connection(&postgres_record(
        ONCE,
        r#"{"ssh":{"isEnabled":true,"host":"bastion.example.com","user":"deploy","authMethod":"password"}}"#,
    ))
    .unwrap();
    write_settings("once-ssh.json", "{}");
    let opener = FakeOpener::new();
    opener.then(Err(unknown_host("bastion.example.com")));
    let connector = Connector::new(Arc::clone(&env().drivers))
        .with_credentials(keys.clone())
        .with_tunnels(
            opener.clone(),
            Some(env().dir.path().join("known_hosts-once")),
        )
        .with_settings_path(Some(env().dir.path().join("once-ssh.json")));
    let agent = Arc::new(AgentConnections::new(connector.clone(), prompts()));
    let layer = ToolLayer::new(agent.clone(), agent.clone(), "test");

    let outcome = layer.call(
        "open_session",
        json!({"connection_id": ONCE.to_lowercase()}),
    );
    assert_eq!(code(&outcome), "tunnel.host_key_unknown", "{outcome:?}");
    assert_eq!(
        opener.opened_count(),
        1,
        "the open got as far as the tunnel"
    );
    // The database password and the SSH password, one read each; the passphrase is not used by
    // this method and is not asked for.
    assert_eq!(keys.1.load(std::sync::atomic::Ordering::SeqCst), 2);

    // The secrets handed to the connect path are the ones the agent masks (this read is a third).
    let record = engine_storage::connection::get_connection(ONCE)
        .unwrap()
        .unwrap();
    let saved = connector.read_saved_secrets(&record).unwrap();
    let masked = engine_agent::redact::redact(
        "a needle-db-password-93 b needle-ssh-password-93 c",
        &saved.mask_texts(),
    );
    assert_eq!(masked, "a *** b *** c");
}

#[test]
fn the_agent_masks_every_form_of_a_saved_password_a_driver_error_may_echo() {
    const MASKED: &str = "7A1B2C3D-0000-4000-8000-000000000014";
    let password = "pa!ss'w@rd%1 x(y)";
    let keys = Arc::new(Keys::default());
    keys.write(SecretKind::Database, MASKED, password).unwrap();
    upsert_connection(&postgres_record(MASKED, "{}")).unwrap();
    let (agent, _) = agent_over(keys.clone(), Arc::clone(&env().drivers), "masked.json");
    let info = agent.get(&MASKED.to_lowercase()).expect("listed");
    let secrets = engine_agent::SessionProvider::secrets(&*agent, &info);
    assert!(secrets.len() >= 2, "the raw password and its URI forms");
    let runtime =
        engine_connect::records::runtime_uri("postgresql://app@db.internal/x", password).unwrap();
    let forms = engine_connect::records::encoded_password_forms(password);
    for echoed in forms
        .iter()
        .map(String::as_str)
        .chain([password, runtime.as_str()])
    {
        let text = format!("FATAL: rejected [{echoed}] for app");
        let masked = engine_agent::redact::redact(&text, &secrets);
        for fragment in ["pa!ss", "w@rd", "w%40rd", "%251", "x(y)", "x%28y%29"] {
            assert!(!masked.contains(fragment), "{fragment} in {masked}");
        }
    }
}

#[test]
fn mask_secrets_hands_over_the_saved_secrets_and_keeps_nothing_for_the_next_open() {
    const FOR_TEXT: &str = "7A1B2C3D-0000-4000-8000-000000000031";
    let keys = Arc::new(Keys::default());
    keys.write(SecretKind::Database, FOR_TEXT, "needle-mask-pw-31")
        .unwrap();
    upsert_connection(&postgres_record(FOR_TEXT, "{}")).unwrap();
    let (agent, _) = agent_over(keys, Arc::clone(&env().drivers), "mask-only.json");

    let secrets = agent.mask_secrets(&FOR_TEXT.to_lowercase()).expect("read");
    let masked = engine_agent::redact::redact("FATAL: needle-mask-pw-31 rejected", &secrets);
    assert_eq!(masked, "FATAL: *** rejected");
    assert_eq!(
        agent.held_secrets(),
        0,
        "the record and the passwords are not left in the agent's prefetch"
    );
}

#[test]
fn mask_secrets_fails_when_the_store_or_the_record_cannot_be_read() {
    const LOCKED_FOR_TEXT: &str = "7A1B2C3D-0000-4000-8000-000000000032";
    upsert_connection(&postgres_record(LOCKED_FOR_TEXT, "{}")).unwrap();
    let locked = Arc::new(Locked(std::sync::atomic::AtomicUsize::new(0)));
    let (agent, _) = agent_over(locked, Arc::clone(&env().drivers), "mask-locked.json");
    let error = agent
        .mask_secrets(&LOCKED_FOR_TEXT.to_lowercase())
        .expect_err("a store that cannot be read is not 'no secrets'");
    assert!(error.to_string().contains("credential store"), "{error}");
    assert_eq!(agent.held_secrets(), 0);

    let gone = agent
        .mask_secrets("7a1b2c3d-0000-4000-8000-0000000000ff")
        .expect_err("a connection that is not saved");
    assert!(gone.to_string().contains("could not be read"), "{gone}");
}

#[test]
fn a_secret_saved_under_the_lower_case_account_is_found_for_an_upper_case_record() {
    const LEGACY: &str = "7A1B2C3D-0000-4000-8000-000000000021";
    let keys = Arc::new(Keys::default());
    keys.write(
        SecretKind::Database,
        &LEGACY.to_lowercase(),
        "legacy-secret-21",
    )
    .unwrap();
    let connector = connector_with("legacy.json").with_credentials(keys.clone());
    let saved = connector
        .read_saved_secrets(&postgres_record(LEGACY, "{}"))
        .unwrap();
    let masked = engine_agent::redact::redact("a legacy-secret-21 b", &saved.mask_texts());
    assert_eq!(masked, "a *** b", "the lower-case account is read");
    // Upper-case first (a miss), then lower-case (the hit).
    assert_eq!(keys.1.load(std::sync::atomic::Ordering::SeqCst), 2);

    // Nothing saved anywhere: the two distinct spellings are asked once each, not one twice.
    let keys = Arc::new(Keys::default());
    let connector = connector_with("legacy.json").with_credentials(keys.clone());
    let saved = connector
        .read_saved_secrets(&postgres_record(LEGACY, "{}"))
        .unwrap();
    assert!(saved.mask_texts().is_empty());
    assert_eq!(keys.1.load(std::sync::atomic::Ordering::SeqCst), 2);
}

#[test]
fn the_connection_record_is_read_once_per_agent_open() {
    use engine_agent::SessionProvider;
    const ONCE: &str = "7A1B2C3D-0000-4000-8000-000000000022";
    write_settings("record-once.json", "{}");
    upsert_connection(&record(
        ONCE,
        &sqlite_uri("record-once.db"),
        "{}",
        r#"{"agent.access":"read"}"#,
        "",
    ))
    .unwrap();
    let (agent, _) = agent_over(
        Arc::new(Keys::default()),
        Arc::clone(&env().drivers),
        "record-once.json",
    );
    let info = agent.get(&ONCE.to_lowercase()).expect("listed");
    // The layer's order: the secrets (which read the record), the plain words, then the open.
    let _ = agent.secrets(&info);
    let _ = agent.plain_words(&info);
    // The record is gone from the store; an open that read it again would fail.
    engine_storage::connection::delete_connection(ONCE).unwrap();
    let session = agent
        .open(&info)
        .expect("the open uses the record read for the secrets");
    session.disconnect();
}

const CHAIN_ID: &str = "6C2E3D4F-2222-4333-8444-555566667777";
const CHAIN_NETWORK: &str = r#"{"ssh":{"isEnabled":true,"host":"bastion.example.com","port":"22","user":"deploy","authMethod":"password","jumps":[{"host":"jump1.example.com","port":"2201","user":"hop1","authMethod":"password","privateKeyPath":""},{"host":"jump2.example.com","port":"2202","user":"hop2","authMethod":"key","privateKeyPath":"/keys/j2"}]},"proxy":{"isEnabled":true,"kind":"http","host":"proxy.example.com","port":"3128","user":"px"}}"#;

fn chain_agent(
    keys: Arc<Keys>,
    opener: Arc<engine_connect::tunnel::ssh::fake::FakeOpener>,
) -> Arc<AgentConnections> {
    write_settings("chain.json", "{}");
    upsert_connection(&postgres_record(CHAIN_ID, CHAIN_NETWORK)).unwrap();
    let connector = Connector::new(Arc::clone(&env().drivers))
        .with_credentials(keys)
        .with_tunnels(opener, Some(env().dir.path().join("known_hosts-chain")))
        .with_settings_path(Some(env().dir.path().join("chain.json")));
    Arc::new(AgentConnections::new(connector, Arc::new(RefusePrompts)))
}

fn chain_keys() -> Arc<Keys> {
    let keys = Arc::new(Keys::default());
    for (kind, account, secret) in [
        (SecretKind::Database, CHAIN_ID.to_owned(), "db-pw"),
        (SecretKind::SshPassword, CHAIN_ID.to_owned(), "bastion-pw"),
        (SecretKind::SshPassword, format!("{CHAIN_ID}#jump0"), "jump0-pw"),
        (SecretKind::SshPassphrase, format!("{CHAIN_ID}#jump1"), "jump1-phrase"),
        (SecretKind::Proxy, CHAIN_ID.to_owned(), "proxy-pw"),
    ] {
        keys.write(kind, &account, secret).unwrap();
    }
    keys
}

#[test]
fn an_agent_open_honours_the_jump_hosts_and_the_proxy_of_a_saved_connection() {
    use engine_connect::tunnel::ssh::fake::FakeOpener;
    let opener = FakeOpener::new();
    let agent = chain_agent(chain_keys(), opener.clone());
    let layer = ToolLayer::new(agent.clone(), agent.clone(), "test");
    // The database behind the fake tunnel is not there; what matters is the route that was built.
    let _ = layer.call("open_session", json!({"connection_id": CHAIN_ID.to_lowercase()}));
    let opened = opener.opened.lock().unwrap();
    assert_eq!(opened.len(), 1, "one tunnel for the one session");
    let spec = &opened[0];
    assert_eq!((spec.host.as_str(), spec.port), ("bastion.example.com", 22));
    assert_eq!(spec.jumps.len(), 2);
    assert_eq!(
        (spec.jumps[0].host.as_str(), spec.jumps[0].port, spec.jumps[0].user.as_str()),
        ("jump1.example.com", 2201, "hop1")
    );
    assert!(matches!(&spec.jumps[0].auth, engine_net::Auth::Password(p) if p.expose() == "jump0-pw"));
    assert!(matches!(
        &spec.jumps[1].auth,
        engine_net::Auth::KeyFile { passphrase: Some(p), .. } if p.expose() == "jump1-phrase"
    ));
    assert!(matches!(&spec.auth, engine_net::Auth::Password(p) if p.expose() == "bastion-pw"));
    let proxy = spec.proxy.as_ref().expect("the proxy carries the first hop");
    assert_eq!((proxy.host.as_str(), proxy.port), ("proxy.example.com", 3128));
    assert_eq!(proxy.credentials.as_ref().unwrap().password.expose(), "proxy-pw");
}

#[test]
fn an_agent_open_with_a_missing_hop_secret_or_an_unknown_jump_key_fails_typed_and_dials_nothing() {
    use engine_connect::tunnel::ssh::fake::{unknown_host_at, FakeOpener};
    // The jump host's password is not saved: the agent cannot ask, so it says what to do.
    let keys = chain_keys();
    keys.delete(SecretKind::SshPassword, &format!("{CHAIN_ID}#jump0")).unwrap();
    let opener = FakeOpener::new();
    let agent = chain_agent(keys, opener.clone());
    let layer = ToolLayer::new(agent.clone(), agent.clone(), "test");
    let ToolOutcome::Failure(error) =
        layer.call("open_session", json!({"connection_id": CHAIN_ID.to_lowercase()}))
    else {
        panic!("a route with a missing secret must not open");
    };
    assert_eq!(error.code, "tunnel.secret_needed", "{}", error.message);
    for address in ["jump1.example.com", "hop1", "proxy.example.com", "bastion"] {
        assert!(!error.message.contains(address), "{}", error.message);
    }
    assert_eq!(opener.opened_count(), 0, "nothing was dialled");

    // An unknown host key on a jump host: refused, nothing recorded, nothing about the address.
    let opener = FakeOpener::new();
    opener.then(Err(unknown_host_at("jump2.example.com", 2202)));
    let agent = chain_agent(chain_keys(), opener.clone());
    let layer = ToolLayer::new(agent.clone(), agent.clone(), "test");
    let ToolOutcome::Failure(error) =
        layer.call("open_session", json!({"connection_id": CHAIN_ID.to_lowercase()}))
    else {
        panic!("an unknown jump host key must not open a session");
    };
    assert_eq!(error.code, "tunnel.host_key_unknown", "{}", error.message);
    assert!(error.message.contains("Connect to this database once"), "{}", error.message);
    for address in ["jump2.example.com", "2202", "SHA256"] {
        assert!(!error.message.contains(address), "{}", error.message);
    }
    assert!(opener.trusted.lock().unwrap().is_empty());
    assert_eq!(opener.opened_count(), 1, "one attempt, no guessed answer");
}
