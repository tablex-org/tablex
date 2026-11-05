//! Shared helpers of the integration tests: building and loading a real
//! driver, and a `SessionProvider` over it.
#![allow(dead_code)]

use engine_agent::testing::connection;
use engine_agent::{
    AgentError, ConnectionInfo, ConnectionKind, DriverSessionAdapter, Session, SessionProvider,
};
use engine_driver_host::{DriverHandle, DriverLoader};
use serde_json::{json, Value};
use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::{Arc, Mutex, OnceLock};

/// Builds `package` (once per process) into `rust-engine/target/test-drivers`
/// (a target dir of its own, so the nested cargo does not wait for the lock
/// of the test build; never the OS temp dir) and returns its cdylib. Not
/// stripped: a stripped cdylib can be rejected by dyld.
pub fn built_driver(package: &'static str, features: &'static str) -> PathBuf {
    static BUILT: OnceLock<Mutex<HashMap<&'static str, PathBuf>>> = OnceLock::new();
    let mut built = BUILT
        .get_or_init(Default::default)
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    built
        .entry(package)
        .or_insert_with(|| {
            let manifest = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../Cargo.toml");
            let target =
                PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../target/test-drivers");
            let mut command = std::process::Command::new(env!("CARGO"));
            command
                .args(["build", "--manifest-path"])
                .arg(&manifest)
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
        })
        .clone()
}

pub fn load_driver(package: &'static str, features: &'static str) -> Arc<DriverHandle> {
    DriverLoader
        .load(&built_driver(package, features))
        .expect("the driver loads")
}

/// Opens driver sessions for connection ids mapped to URIs.
pub struct DriverProvider {
    pub handle: Arc<DriverHandle>,
    pub uris: Mutex<HashMap<String, String>>,
    pub secrets: Vec<engine_agent::SecretText>,
}

impl DriverProvider {
    pub fn new(handle: Arc<DriverHandle>, uris: &[(&str, &str)]) -> Arc<Self> {
        Arc::new(Self {
            handle,
            uris: Mutex::new(
                uris.iter()
                    .map(|(id, uri)| (id.to_string(), uri.to_string()))
                    .collect(),
            ),
            secrets: Vec::new(),
        })
    }

    /// A session outside the tool layer (to seed data).
    pub fn raw_session(&self, id: &str) -> Arc<dyn Session> {
        let connection = connection(id, ConnectionKind::Sqlite, None);
        self.open(&connection).expect("raw session")
    }
}

impl SessionProvider for DriverProvider {
    fn open(&self, connection: &ConnectionInfo) -> Result<Arc<dyn Session>, AgentError> {
        let uri = self
            .uris
            .lock()
            .unwrap()
            .get(&connection.id)
            .cloned()
            .ok_or_else(|| AgentError::invalid("unknown connection"))?;
        let request: Value = json!({ "uri": uri });
        let session = self
            .handle
            .connect(&request.to_string())
            .map_err(engine_agent::driver_session::map_error)?;
        Ok(Arc::new(
            DriverSessionAdapter::new(Arc::clone(&self.handle), session)?
                .with_secrets(self.secrets.clone()),
        ))
    }
}
