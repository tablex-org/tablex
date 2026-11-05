//! The fixture of the end-to-end tests: a temporary connection store and driver store, the REAL
//! SQLite driver (built by cargo into `rust-engine/target/test-drivers`, installed into the
//! temporary store and loaded through the SHA-256-verified path), an in-memory credential store,
//! and an in-process MCP client talking to the real server over an in-memory pipe. Nothing here
//! touches the network, the user's keychain, connection store or driver directory.
#![allow(dead_code)]

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, OnceLock};

use engine_agent::ports::RunRequest;
use engine_agent::{
    AuditConfig, AuditLog, CancelToken, DriverSessionAdapter, Language, Limits, Session, ToolLayer,
};
use engine_connect::connect::{Connector, RefusePrompts, TunnelPrompts};
use engine_connect::credentials::{CredentialError, CredentialStore, SecretKind};
use engine_driver_host::{InstalledDriverRecord, InstalledDriverStore};
use engine_storage::connection::{upsert_connection, SavedConnectionRecord};
use rmcp::model::{
    CallToolRequestParams, CallToolResult, CancelledNotificationParam, ClientCapabilities,
    ClientInfo, ClientRequest, ElicitRequestParams, ElicitResult, ElicitationAction,
    ErrorData as McpError, Implementation, Request, ServerResult,
};
use rmcp::service::{
    NotificationContext, PeerRequestOptions, RequestContext, RequestHandle, RunningService,
};
use rmcp::{ClientHandler, Peer, RoleClient, ServiceExt};
use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use tablex_mcp::server::McpServer;
use tablex_mcp::transport::ServeError;

/// The saved password of the PostgreSQL fixture connection (see [`Fixture::secret_connection`]):
/// long enough for exact-secret masking, with characters that change when percent-encoded. It must
/// never appear in any response, raw or encoded.
pub const FIXTURE_PASSWORD: &str = "p@ss/w0rd:Zq81-Lm";
/// A directory name inside every database path: the URI is not to be echoed either.
pub const URI_MARKER: &str = "uri-marker-9b2e41";

pub struct Shared {
    pub dir: tempfile::TempDir,
    pub drivers: Arc<InstalledDriverStore>,
}

impl Shared {
    pub fn data_dir(&self) -> PathBuf {
        self.dir.path().join("data")
    }
    pub fn driver_dir(&self) -> PathBuf {
        self.dir.path().join("drivers")
    }
}

/// One environment per test binary: the connection store is a process-wide singleton, so every
/// test shares it and uses connection ids of its own.
pub fn shared() -> &'static Shared {
    static SHARED: OnceLock<Shared> = OnceLock::new();
    SHARED.get_or_init(|| {
        let dir = tempfile::tempdir().unwrap();
        std::env::set_var(engine_storage::paths::DATA_DIR_ENV, dir.path().join("data"));
        let drivers = Arc::new(InstalledDriverStore::new(dir.path().join("drivers")));
        let driver = built_driver("dbengine-sqlite");
        let sha256 = Sha256::digest(std::fs::read(&driver).unwrap())
            .iter()
            .map(|byte| format!("{byte:02x}"))
            .collect();
        drivers
            .save(InstalledDriverRecord {
                id: "sqlite".into(),
                path: driver,
                sha256,
                ..Default::default()
            })
            .unwrap();
        Shared { dir, drivers }
    })
}

/// Builds `package` into `rust-engine/target/test-drivers` (a target dir of its own, so the nested
/// cargo does not wait for the lock of the test build) and returns its cdylib. Not stripped: a
/// stripped cdylib can be rejected by dyld. `TABLEX_TEST_SKIP_DRIVER_BUILD=1` reuses the last one.
fn built_driver(package: &str) -> PathBuf {
    let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../rust-engine");
    let target = root.join("target/test-drivers");
    let artifact = target.join("debug").join(format!(
        "{}{}{}",
        std::env::consts::DLL_PREFIX,
        package.replace('-', "_"),
        std::env::consts::DLL_SUFFIX
    ));
    if std::env::var_os("TABLEX_TEST_SKIP_DRIVER_BUILD").is_some() && artifact.exists() {
        return artifact;
    }
    let output = std::process::Command::new(env!("CARGO"))
        .args(["build", "--manifest-path"])
        .arg(root.join("Cargo.toml"))
        .args(["-p", package, "--target-dir"])
        .arg(&target)
        .env("CARGO_PROFILE_DEV_STRIP", "none")
        .output()
        .expect("cargo build driver");
    assert!(
        output.status.success(),
        "{package} build failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    artifact
}

/// A thread-safe in-memory credential store (the crate's own `testing` store is single-threaded)
/// that remembers which accounts were asked for, so a test can prove a secret really was read.
#[derive(Default)]
pub struct Keys {
    items: Mutex<HashMap<(String, String), String>>,
    loads: Mutex<Vec<String>>,
}

impl Keys {
    /// Every account `load` was called for, in order.
    pub fn loaded_accounts(&self) -> Vec<String> {
        self.loads.lock().unwrap().clone()
    }
}

impl CredentialStore for Keys {
    fn load(&self, kind: SecretKind, account: &str) -> Result<Option<String>, CredentialError> {
        self.loads.lock().unwrap().push(account.to_owned());
        Ok(self
            .items
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
        self.items.lock().unwrap().insert(
            (format!("{kind:?}"), account.to_owned()),
            password.to_owned(),
        );
        Ok(())
    }
    fn delete(&self, kind: SecretKind, account: &str) -> Result<(), CredentialError> {
        self.items
            .lock()
            .unwrap()
            .remove(&(format!("{kind:?}"), account.to_owned()));
        Ok(())
    }
}

/// A saved SQLite connection with a seeded database.
pub struct Conn {
    pub id: String,
    pub db_path: PathBuf,
}

/// A tool layer built exactly as `tablex-mcp` builds it (`setup::build_layer`) over the temporary
/// stores, with an audit log of its own.
pub struct Fixture {
    pub layer: Arc<ToolLayer>,
    pub audit: Arc<AuditLog>,
    pub audit_path: PathBuf,
    pub keys: Arc<Keys>,
    connector: Connector,
    dir: PathBuf,
}

static NEXT: AtomicUsize = AtomicUsize::new(1);

impl Fixture {
    pub fn new() -> Self {
        Self::build(|connector, audit, _| tablex_mcp::setup::build_layer(connector, audit))
    }

    /// A fixture whose layer is made by `make` (fakes instead of the connector) and audited into
    /// this fixture's audit log.
    pub fn with_layer(make: impl FnOnce(Arc<AuditLog>) -> ToolLayer) -> Self {
        Self::build(|_, audit, _| make(audit))
    }

    /// [`with_layer`](Self::with_layer), and `make` also gets this fixture's `settings.json` path.
    pub fn with_layer_in(make: impl FnOnce(Arc<AuditLog>, PathBuf) -> ToolLayer) -> Self {
        Self::build(|_, audit, settings| make(audit, settings))
    }

    /// The app's `settings.json` of this fixture (the file the MCP switch is read from).
    pub fn settings_path(&self) -> PathBuf {
        self.dir.join("settings.json")
    }

    /// Writes the switch "Let MCP clients run changes" the way the app does.
    pub fn set_mcp_writes(&self, on: bool) {
        self.write_settings(&json!({"schema_version": 4, "ai": {"mcp_writes": on}}).to_string());
    }

    pub fn write_settings(&self, text: &str) {
        std::fs::write(self.settings_path(), text).unwrap();
    }

    fn build(make: impl FnOnce(Connector, Arc<AuditLog>, PathBuf) -> ToolLayer) -> Self {
        let shared = shared();
        let n = NEXT.fetch_add(1, Ordering::Relaxed);
        let dir = shared.dir.path().join(format!("fixture-{n}"));
        std::fs::create_dir_all(&dir).unwrap();
        let keys = Arc::new(Keys::default());
        let connector = Connector::new(Arc::clone(&shared.drivers))
            .with_credentials(keys.clone())
            .with_settings_path(Some(dir.join("settings.json")));
        let audit_path = dir.join("audit.jsonl");
        let audit = Arc::new(AuditLog::open(&audit_path, AuditConfig::default()).unwrap());
        let layer = Arc::new(make(
            connector.clone(),
            Arc::clone(&audit),
            dir.join("settings.json"),
        ));
        Self {
            layer,
            audit,
            audit_path,
            keys,
            connector,
            dir,
        }
    }

    /// Saves a connection whose `agent.access` override is `access` (`None`: not set, so `off`)
    /// and seeds its database with a table `items` of three rows.
    pub fn connection(&self, access: Option<&str>) -> Conn {
        self.typed_connection(access, "")
    }

    /// [`connection`](Self::connection) of the connection type `connection_type` (`""`: none,
    /// `"production"`, ...).
    pub fn typed_connection(&self, access: Option<&str>, connection_type: &str) -> Conn {
        let n = NEXT.fetch_add(1, Ordering::Relaxed);
        let id = format!("00000000-0000-4000-8000-{n:012}");
        let db_dir = self.dir.join(URI_MARKER);
        std::fs::create_dir_all(&db_dir).unwrap();
        let db_path = db_dir.join(format!("{n}.db"));
        let overrides = access
            .map(|level| json!({"agent.access": level}).to_string())
            .unwrap_or_default();
        upsert_connection(&SavedConnectionRecord {
            id: id.clone(),
            name: format!("fixture {n}"),
            uri: format!("sqlite://{}", db_path.display()),
            db_type: "sqlite".into(),
            color_hex: "#34C759".into(),
            save_password: false,
            save_proxy_password: false,
            network_settings_json: "{}".into(),
            is_pinned: false,
            last_connected: None,
            created_at: 1.0,
            updated_at: 1.0,
            connection_type: connection_type.into(),
            overrides_json: overrides,
        })
        .unwrap();
        self.seed(
            &id,
            "CREATE TABLE items(id INTEGER PRIMARY KEY, label TEXT NOT NULL); \
             INSERT INTO items(label) VALUES ('alpha'), ('beta'), ('gamma');",
        );
        Conn { id, db_path }
    }

    /// A PostgreSQL connection with a saved password (stored under the connection's own id in
    /// the credential store), agent access `read`, and NO PostgreSQL driver in the driver store:
    /// `open_session` loads the password, builds the runtime URI with it, and fails at driver
    /// loading, so the password and the address are in play on the failing path.
    pub fn secret_connection(&self) -> String {
        let n = NEXT.fetch_add(1, Ordering::Relaxed);
        let id = format!("00000000-0000-4000-8000-{n:012}");
        self.keys
            .write(SecretKind::Database, &id.to_uppercase(), FIXTURE_PASSWORD)
            .unwrap();
        upsert_connection(&SavedConnectionRecord {
            id: id.clone(),
            name: format!("postgres fixture {n}"),
            uri: "postgresql://app@127.0.0.1:1/db".into(),
            db_type: "postgresql".into(),
            color_hex: "#34C759".into(),
            save_password: true,
            save_proxy_password: false,
            network_settings_json: "{}".into(),
            is_pinned: false,
            last_connected: None,
            created_at: 1.0,
            updated_at: 1.0,
            connection_type: String::new(),
            overrides_json: json!({"agent.access": "read"}).to_string(),
        })
        .unwrap();
        id
    }

    /// A SQLite connection (access `read`) whose database directory does not exist.
    pub fn unreachable_sqlite_connection(&self) -> String {
        let n = NEXT.fetch_add(1, Ordering::Relaxed);
        let id = format!("00000000-0000-4000-8000-{n:012}");
        let missing = self.dir.join(URI_MARKER).join("no-such-dir").join("x.db");
        upsert_connection(&SavedConnectionRecord {
            id: id.clone(),
            name: format!("missing {n}"),
            uri: format!("sqlite://{}", missing.display()),
            db_type: "sqlite".into(),
            color_hex: "#34C759".into(),
            save_password: false,
            save_proxy_password: false,
            network_settings_json: "{}".into(),
            is_pinned: false,
            last_connected: None,
            created_at: 1.0,
            updated_at: 1.0,
            connection_type: String::new(),
            overrides_json: json!({"agent.access": "read"}).to_string(),
        })
        .unwrap();
        id
    }

    /// Runs `sql` through a session of the connector itself, outside the agent policy.
    fn seed(&self, id: &str, sql: &str) {
        let prompts: Arc<dyn TunnelPrompts> = Arc::new(RefusePrompts);
        let engine_connect::connect::OpenedConnection {
            session, handle, ..
        } = self
            .connector
            .open(id, &prompts)
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

    /// The audit entries written so far (flushed first), oldest first.
    pub fn audit_entries(&self) -> Vec<engine_agent::AuditEntry> {
        self.audit.flush();
        let mut entries = engine_agent::read_entries(&self.audit_path, 10_000)
            .unwrap()
            .entries;
        entries.reverse();
        entries
    }
}

/// The real server and a client connected by an in-memory pipe.
pub struct Harness<H: ClientHandler = ()> {
    pub fixture: Fixture,
    pub client: RunningService<RoleClient, H>,
    pub server: tokio::task::JoinHandle<Result<(), ServeError>>,
}

impl Harness<()> {
    pub async fn start(fixture: Fixture) -> Self {
        Self::start_with(fixture, ()).await
    }
}

impl<H: ClientHandler> Harness<H> {
    pub async fn start_with(fixture: Fixture, handler: H) -> Self {
        let (client_end, server_end) = tokio::io::duplex(1 << 20);
        let (reader, writer) = tokio::io::split(server_end);
        let server = McpServer::new(Arc::clone(&fixture.layer)).unwrap();
        let server = tokio::spawn(server.serve(reader, writer, never));
        let client = handler.serve(client_end).await.expect("initialize");
        Self {
            fixture,
            client,
            server,
        }
    }

    pub fn peer(&self) -> Peer<RoleClient> {
        self.client.peer().clone()
    }

    pub async fn call(&self, tool: &str, arguments: Value) -> CallToolResult {
        self.try_call(tool, arguments).await.expect("tools/call")
    }

    pub async fn try_call(
        &self,
        tool: &str,
        arguments: Value,
    ) -> Result<CallToolResult, rmcp::ServiceError> {
        self.peer().call_tool(params(tool, arguments)).await
    }

    /// Sends a `tools/call` and returns its handle, to cancel it or await it.
    pub async fn start_call(&self, tool: &str, arguments: Value) -> RequestHandle<RoleClient> {
        let request = ClientRequest::CallToolRequest(Request::new(params(tool, arguments)));
        self.peer()
            .send_cancellable_request(request, PeerRequestOptions::no_options())
            .await
            .expect("send")
    }
}

/// A `stop` that never fires.
pub async fn never() {
    std::future::pending::<()>().await;
}

pub fn params(tool: &str, arguments: Value) -> CallToolRequestParams {
    let mut params = CallToolRequestParams::new(tool.to_owned());
    if let Value::Object(map) = arguments {
        params.arguments = Some(map);
    }
    params
}

/// The text of the first content block.
pub fn text(result: &CallToolResult) -> String {
    result
        .content
        .first()
        .and_then(|block| block.as_text())
        .map(|text| text.text.clone())
        .expect("a text block")
}

/// The typed error code in an `isError` result, `ok` otherwise.
pub fn code(result: &CallToolResult) -> String {
    if result.is_error != Some(true) {
        return "ok".into();
    }
    let body: Value = serde_json::from_str(&text(result)).expect("the error is JSON");
    body["error"]["code"].as_str().expect("a code").to_owned()
}

/// A successful result's structured content.
pub fn structured(result: &CallToolResult) -> Value {
    assert_ne!(result.is_error, Some(true), "{}", text(result));
    result
        .structured_content
        .clone()
        .expect("structuredContent")
}

pub fn unwrap_call(response: ServerResult) -> CallToolResult {
    match response {
        ServerResult::CallToolResult(result) => result,
        other => panic!("not a tool result: {other:?}"),
    }
}

// ---- fakes: a tool layer over in-memory sessions (no driver, no store) ---------------------------

use engine_agent::ports::{NodeLevel, NodePage, NodeQuery, SessionInfo, SessionProvider, TableRef};
use engine_agent::testing::{connection, Behavior, FakeCatalog, FakeSession};
use engine_agent::{AgentAccess, AgentError, ConnectionInfo, ConnectionKind};

/// Hands out the sessions it was given, by connection id.
pub struct MapProvider(pub HashMap<String, Arc<dyn Session>>);

impl SessionProvider for MapProvider {
    fn open(&self, connection: &ConnectionInfo) -> Result<Arc<dyn Session>, AgentError> {
        self.0
            .get(&connection.id)
            .cloned()
            .ok_or_else(|| AgentError::invalid("no session for this connection"))
    }
}

/// A flag a test opens to release blocked threads.
#[derive(Default)]
pub struct Gate(Mutex<bool>, std::sync::Condvar);

impl Gate {
    pub fn open(&self) {
        *self.0.lock().unwrap() = true;
        self.1.notify_all();
    }
    pub fn wait(&self) {
        let mut open = self.0.lock().unwrap();
        while !*open {
            open = self.1.wait(open).unwrap();
        }
    }
}

/// A session whose `disconnect` hangs (until the gate opens), like a driver stuck on a dead server.
pub struct HangingDisconnect {
    pub inner: Arc<FakeSession>,
    pub entered: Mutex<std::sync::mpsc::Sender<()>>,
    pub release: Arc<Gate>,
}

impl Session for HangingDisconnect {
    fn info(&self) -> SessionInfo {
        self.inner.info()
    }
    fn list_nodes(&self, level: NodeLevel, query: &NodeQuery) -> Result<NodePage, AgentError> {
        self.inner.list_nodes(level, query)
    }
    fn describe_table(
        &self,
        table: &TableRef,
    ) -> Result<engine_agent::table::TableDescription, AgentError> {
        self.inner.describe_table(table)
    }
    fn run(
        &self,
        request: &RunRequest<'_>,
        cancel: &CancelToken,
    ) -> Result<engine_agent::result::QueryResult, AgentError> {
        self.inner.run(request, cancel)
    }
    fn disconnect(&self) {
        let _ = self.entered.lock().unwrap().send(());
        self.release.wait();
        self.inner.disconnect();
    }
}

/// The connection ids `fake-0`, `fake-1`, ... with access `read`, each over its own session.
pub fn fake_ids(count: usize) -> Vec<String> {
    (0..count).map(|n| format!("fake-{n}")).collect()
}

/// A fixture over fake sessions (`session(id)` makes the session of each connection).
pub fn fake_fixture(ids: &[String], mut session: impl FnMut(&str) -> Arc<dyn Session>) -> Fixture {
    let sessions: HashMap<String, Arc<dyn Session>> =
        ids.iter().map(|id| (id.clone(), session(id))).collect();
    let catalog = FakeCatalog::new(
        ids.iter()
            .map(|id| connection(id, ConnectionKind::Sqlite, Some(AgentAccess::Read)))
            .collect(),
    );
    Fixture::with_layer(|audit| {
        ToolLayer::new(catalog, Arc::new(MapProvider(sessions)), "mcp").with_audit(audit)
    })
}

/// A fake session that blocks every statement until it is cancelled.
pub fn blocking_session() -> Arc<FakeSession> {
    let session = FakeSession::new(ConnectionKind::Sqlite).arc();
    session.set_behavior(Behavior::BlockUntilCancelled);
    session
}

/// An in-memory pipe: the client end, and the halves the server serves.
pub fn pipe() -> (
    tokio::io::DuplexStream,
    tokio::io::ReadHalf<tokio::io::DuplexStream>,
    tokio::io::WriteHalf<tokio::io::DuplexStream>,
) {
    let (client_end, server_end) = tokio::io::duplex(1 << 20);
    let (reader, writer) = tokio::io::split(server_end);
    (client_end, reader, writer)
}

// ---- a client that can be asked --------------------------------------------------------------------

/// One `elicitation/create` the server sent, held until the test answers it.
pub struct Question {
    pub message: String,
    /// The requested schema as the server sent it.
    pub schema: Value,
    /// Cancelled when the server sends `notifications/cancelled` for this question.
    pub cancelled: tokio_util::sync::CancellationToken,
    reply: tokio::sync::oneshot::Sender<Result<ElicitResult, McpError>>,
}

impl Question {
    pub fn answer(self, result: Result<ElicitResult, McpError>) {
        let _ = self.reply.send(result);
    }

    pub fn accept(self, run: bool) {
        self.answer(Ok(accepted(run)));
    }
}

pub fn accepted(run: bool) -> ElicitResult {
    ElicitResult::new(ElicitationAction::Accept).with_content(json!({"run": run}))
}

/// Everything the client saw of the server's questions.
#[derive(Default)]
pub struct Seen {
    /// Message and schema of every question, in order.
    pub asked: Mutex<Vec<(String, Value)>>,
    /// `notifications/cancelled` received.
    pub cancels: AtomicUsize,
}

impl Seen {
    pub fn asked(&self) -> Vec<(String, Value)> {
        self.asked.lock().unwrap().clone()
    }
    pub fn cancels(&self) -> usize {
        self.cancels.load(Ordering::SeqCst)
    }
}

enum Mode {
    /// Answers every question at once with this.
    Auto(Result<ElicitResult, McpError>),
    /// Hands every question to the test.
    Manual(tokio::sync::mpsc::UnboundedSender<Question>),
}

/// An MCP client that answers `elicitation/create`. Without the capability it is a plain client:
/// it never declares elicitation (and would answer if asked).
pub struct Asker {
    capability: bool,
    mode: Mode,
    pub seen: Arc<Seen>,
}

impl Asker {
    pub fn answering(capability: bool, answer: Result<ElicitResult, McpError>) -> Self {
        Self {
            capability,
            mode: Mode::Auto(answer),
            seen: Arc::default(),
        }
    }

    /// Questions arrive on the returned receiver; the test answers them.
    pub fn manual(capability: bool) -> (Self, tokio::sync::mpsc::UnboundedReceiver<Question>) {
        let (questions, desk) = tokio::sync::mpsc::unbounded_channel();
        (
            Self {
                capability,
                mode: Mode::Manual(questions),
                seen: Arc::default(),
            },
            desk,
        )
    }
}

impl ClientHandler for Asker {
    fn get_info(&self) -> ClientInfo {
        let capabilities = if self.capability {
            ClientCapabilities::builder().enable_elicitation().build()
        } else {
            ClientCapabilities::default()
        };
        ClientInfo::new(capabilities, Implementation::new("test-client", "0"))
    }

    async fn create_elicitation(
        &self,
        request: ElicitRequestParams,
        context: RequestContext<RoleClient>,
    ) -> Result<ElicitResult, McpError> {
        let ElicitRequestParams::FormElicitationParams {
            message,
            requested_schema,
            ..
        } = request
        else {
            return Err(McpError::invalid_params("only form questions", None));
        };
        let schema = serde_json::to_value(requested_schema).unwrap();
        self.seen
            .asked
            .lock()
            .unwrap()
            .push((message.clone(), schema.clone()));
        match &self.mode {
            Mode::Auto(answer) => answer.clone(),
            Mode::Manual(questions) => {
                let (reply, answered) = tokio::sync::oneshot::channel();
                let _ = questions.send(Question {
                    message,
                    schema,
                    cancelled: context.ct.clone(),
                    reply,
                });
                tokio::select! {
                    answer = answered => {
                        answer.unwrap_or_else(|_| Err(McpError::internal_error("no answer", None)))
                    }
                    () = context.ct.cancelled() => Ok(ElicitResult::new(ElicitationAction::Cancel)),
                }
            }
        }
    }

    async fn on_cancelled(
        &self,
        _params: CancelledNotificationParam,
        _context: NotificationContext<RoleClient>,
    ) {
        self.seen.cancels.fetch_add(1, Ordering::SeqCst);
    }
}

/// Polls `probe` every few milliseconds until it returns something, for at most a minute.
pub async fn poll<T>(what: &str, mut probe: impl AsyncFnMut() -> Option<T>) -> T {
    let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(60);
    loop {
        if let Some(found) = probe().await {
            return found;
        }
        assert!(tokio::time::Instant::now() < deadline, "timed out: {what}");
        tokio::time::sleep(std::time::Duration::from_millis(5)).await;
    }
}

/// Awaits `future` for at most a minute, then fails the test: a regression that stops something
/// from happening ends the test instead of hanging it.
pub async fn within<T>(what: &str, future: impl std::future::Future<Output = T>) -> T {
    tokio::time::timeout(std::time::Duration::from_secs(60), future)
        .await
        .unwrap_or_else(|_| panic!("timed out waiting for {what}"))
}
