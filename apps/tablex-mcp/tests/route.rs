//! The MCP server honours the jump hosts and the proxy of a saved connection, the way the app does,
//! and (it has no window to ask in) fails with a typed, address-free error when a secret is missing.
//!
//! The layer is `setup::build_layer` over a REAL PostgreSQL driver and the REAL tunnel opener; the
//! credential store is in memory and the proxy / bastion addresses are closed local ports, so no
//! database, SSH server or keychain is touched.

use std::collections::HashMap;
use std::net::TcpListener;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};

use engine_agent::{AuditConfig, AuditLog, ToolLayer, ToolOutcome};
use engine_connect::connect::Connector;
use engine_connect::credentials::{CredentialError, CredentialStore, SecretKind};
use engine_driver_host::{InstalledDriverRecord, InstalledDriverStore};
use engine_storage::connection::{upsert_connection, SavedConnectionRecord};
use serde_json::json;
use sha2::{Digest, Sha256};

#[derive(Default)]
struct Keys(Mutex<HashMap<(String, String), String>>);

impl CredentialStore for Keys {
    fn load(&self, kind: SecretKind, account: &str) -> Result<Option<String>, CredentialError> {
        Ok(self.0.lock().unwrap().get(&(format!("{kind:?}"), account.to_owned())).cloned())
    }
    fn write(&self, kind: SecretKind, account: &str, password: &str) -> Result<(), CredentialError> {
        self.0.lock().unwrap().insert((format!("{kind:?}"), account.to_owned()), password.to_owned());
        Ok(())
    }
    fn delete(&self, kind: SecretKind, account: &str) -> Result<(), CredentialError> {
        self.0.lock().unwrap().remove(&(format!("{kind:?}"), account.to_owned()));
        Ok(())
    }
}

struct Env {
    dir: tempfile::TempDir,
    drivers: Arc<InstalledDriverStore>,
}

fn env() -> &'static Env {
    static ENV: std::sync::OnceLock<Env> = std::sync::OnceLock::new();
    ENV.get_or_init(|| {
        let dir = tempfile::tempdir().unwrap();
        std::env::set_var(engine_storage::paths::DATA_DIR_ENV, dir.path().join("data"));
        let drivers = Arc::new(InstalledDriverStore::new(dir.path().join("drivers")));
        let root = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../rust-engine");
        let target = root.join("target/test-drivers");
        let output = std::process::Command::new(env!("CARGO"))
            .args(["build", "--manifest-path"])
            .arg(root.join("Cargo.toml"))
            .args(["-p", "dbengine-postgres", "--features", "abi-v2", "--target-dir"])
            .arg(&target)
            .env("CARGO_PROFILE_DEV_STRIP", "none")
            .output()
            .expect("cargo build driver");
        assert!(output.status.success(), "{}", String::from_utf8_lossy(&output.stderr));
        let driver = target.join("debug").join(format!(
            "{}dbengine_postgres{}",
            std::env::consts::DLL_PREFIX,
            std::env::consts::DLL_SUFFIX
        ));
        let sha256 = Sha256::digest(std::fs::read(&driver).unwrap())
            .iter()
            .map(|byte| format!("{byte:02x}"))
            .collect();
        drivers
            .save(InstalledDriverRecord { id: "postgresql".into(), path: driver, sha256, ..Default::default() })
            .unwrap();
        Env { dir, drivers }
    })
}

fn closed_port() -> u16 {
    TcpListener::bind("127.0.0.1:0").unwrap().local_addr().unwrap().port()
}

fn layer(keys: Arc<Keys>) -> ToolLayer {
    let env = env();
    let settings = env.dir.path().join("settings.json");
    std::fs::write(&settings, "{}").unwrap();
    let connector = Connector::new(Arc::clone(&env.drivers))
        .with_credentials(keys)
        .with_tunnels(Arc::new(engine_connect::tunnel::ssh::EngineOpener), Some(env.dir.path().join("known_hosts")))
        .with_settings_path(Some(settings));
    let audit = Arc::new(AuditLog::open(env.dir.path().join("audit.jsonl"), AuditConfig::default()).unwrap());
    tablex_mcp::setup::build_layer(connector, audit)
}

/// Writes a record into the store, after `env()` pointed the data dir at the temporary one (the store
/// opens on first use, once per process: written first, it would be the user's real store).
fn save(id: &str, network: &str) {
    env();
    upsert_connection(&SavedConnectionRecord {
        id: id.into(),
        name: format!("route {id}"),
        uri: "postgresql://app:dbpw@db.internal.example:5432/db".into(),
        db_type: "postgresql".into(),
        color_hex: "#34C759".into(),
        save_password: false,
        save_proxy_password: false,
        network_settings_json: network.into(),
        is_pinned: false,
        last_connected: None,
        created_at: 1.0,
        updated_at: 1.0,
        connection_type: String::new(),
        overrides_json: json!({"agent.access": "read"}).to_string(),
    })
    .unwrap();
}

#[test]
fn a_proxy_only_connection_goes_through_the_proxy_never_straight_to_the_database() {
    let id = "00000000-0000-4000-8000-0000000000a1";
    let proxy_port = closed_port();
    save(
        id,
        &json!({"proxy": {"isEnabled": true, "kind": "socks5", "host": "127.0.0.1", "port": proxy_port.to_string()}}).to_string(),
    );
    let layer = layer(Arc::new(Keys::default()));
    let ToolOutcome::Failure(error) = layer.call("open_session", json!({"connection_id": id})) else {
        panic!("the proxy is closed: the open must fail");
    };
    // The driver dialled the local proxy-route port and the proxy refused: the tunnel's own code,
    // not a direct (and DNS-failing) connection to db.internal.example.
    assert_eq!(error.code, "tunnel.proxy", "{}", error.message);
    for address in ["db.internal.example", "127.0.0.1", &proxy_port.to_string(), "dbpw"] {
        assert!(!error.message.contains(address), "{}", error.message);
    }
}

#[test]
fn a_chain_with_a_missing_secret_fails_typed_without_dialling_or_naming_a_hop() {
    let id = "00000000-0000-4000-8000-0000000000a2";
    save(
        id,
        &json!({
            "ssh": {"isEnabled": true, "host": "bastion.internal.example", "user": "deploy", "authMethod": "agent",
                    "jumps": [{"host": "jump.internal.example", "port": "2201", "user": "hop1", "authMethod": "password"}]}
        })
        .to_string(),
    );
    let keys = Arc::new(Keys::default());
    let layer = self::layer(keys.clone());
    let ToolOutcome::Failure(error) = layer.call("open_session", json!({"connection_id": id})) else {
        panic!("the jump host's password is not saved");
    };
    assert_eq!(error.code, "tunnel.secret_needed", "{}", error.message);
    assert!(error.message.contains("Tunnel / Proxy"), "{}", error.message);
    for address in ["jump.internal.example", "bastion.internal.example", "hop1", "2201"] {
        assert!(!error.message.contains(address), "{}", error.message);
    }

    // With the password saved under the jump's own account the open goes on to dial it (and fails
    // there: nothing listens), so the secret was found by position.
    keys.write(SecretKind::SshPassword, &format!("{}#jump0", id.to_uppercase()), "jump-pw").unwrap();
    let ToolOutcome::Failure(error) = layer.call("open_session", json!({"connection_id": id})) else {
        panic!("nothing listens");
    };
    assert_ne!(error.code, "tunnel.secret_needed", "{}", error.message);
}
