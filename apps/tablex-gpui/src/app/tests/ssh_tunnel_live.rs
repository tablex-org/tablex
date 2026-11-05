// SSH tunnels against the dedicated `tablex-test-sshd` container and the `tablex-test-*` databases
// behind it, through the real engine-net tunnel, the real host key dialog and the real drivers.
// All `#[ignore]`d. Run:
//
//   rust-engine/scripts/test-sshd.sh start
//   rust-engine/scripts/test-tls-servers.sh start          # only for the live_tls_* tests
//   source "$(rust-engine/scripts/test-sshd.sh env)"; source "$(rust-engine/scripts/test-tls-servers.sh env)"
//   TABLEX_TEST_PG_URI=postgres://tablex:tablex@127.0.0.1:55432/tablex \
//   TABLEX_TEST_MYSQL_URI=mysql://tablex:tablex@127.0.0.1:53306/tablex \
//   TABLEX_TEST_MARIADB_URI=mariadb://tablex:tablex@127.0.0.1:53307/tablex \
//   TABLEX_TEST_REDIS_URI=redis://127.0.0.1:56379 \
//   TABLEX_TEST_CLICKHOUSE_URI=clickhouse://tablex:tablex@127.0.0.1:58123/tablex \
//     cargo test -p tablex-gpui live_tunnel -- --ignored --test-threads=1
//
// The databases are reached from the bastion as `host.docker.internal:<port>`. Nothing reads
// ~/.ssh: every run has its own known_hosts file in a temp directory and the host key goes through
// the same "Trust this host?" dialog a user sees.

use crate::ssh_tunnel::EngineOpener;

struct Bastion {
    host: String,
    port: u16,
    user: String,
    password: String,
    passphrase: String,
    key: String,
    key_encrypted: String,
    container: String,
}

impl Bastion {
    fn from_env() -> Option<Self> {
        let var = |name: &str| std::env::var(name).ok();
        let bastion = Self {
            host: var("TABLEX_TEST_SSHD_HOST")?,
            port: var("TABLEX_TEST_SSHD_PORT")?.parse().ok()?,
            user: var("TABLEX_TEST_SSHD_USER")?,
            password: var("TABLEX_TEST_SSHD_PASSWORD")?,
            passphrase: var("TABLEX_TEST_SSHD_PASSPHRASE")?,
            key: var("TABLEX_TEST_SSHD_KEY_ED25519")?,
            key_encrypted: var("TABLEX_TEST_SSHD_KEY_ED25519_ENC")?,
            container: var("TABLEX_TEST_SSHD_CONTAINER")?,
        };
        assert!(bastion.container.starts_with("tablex-test-"), "refusing to touch {}", bastion.container);
        Some(bastion)
    }

    fn settings(&self, auth: crate::ssh_tunnel::SshAuthMethod) -> SshSettings {
        SshSettings {
            enabled: true,
            host: self.host.clone(),
            port: self.port.to_string(),
            user: self.user.clone(),
            auth,
            key_path: match auth {
                SshAuthMethod::KeyFile => self.key.clone(),
                _ => String::new(),
            },
            save_secrets: false,
            ..SshSettings::default()
        }
    }

    /// SSH connections of the test user on the bastion (one per tunnel).
    fn sessions(&self) -> usize {
        let output = std::process::Command::new("docker")
            .args(["exec", &self.container, "sh", "-c", "ps -o args | grep -c 'tablex \\[priv\\]'"])
            .output()
            .expect("docker exec");
        String::from_utf8_lossy(&output.stdout).trim().parse().unwrap_or(0)
    }

    /// Ends every SSH connection of the test user, like a bastion that drops them.
    fn kill_sessions(&self) {
        let status = std::process::Command::new("docker")
            .args(["exec", &self.container, "sh", "-c", "pkill -f 'tablex [[]priv[]]'; pkill -f 'tablex[@]'; true"])
            .status()
            .expect("docker exec");
        assert!(status.success());
    }
}

struct LiveKind {
    kind: ConnectionKind,
    driver: (&'static str, &'static str),
    schemes: &'static [&'static str],
    env: &'static str,
}

const PG: LiveKind = LiveKind { kind: ConnectionKind::Postgres, driver: ("dbengine-postgres", "abi-v2"), schemes: &["postgres", "postgresql"], env: "TABLEX_TEST_PG_URI" };
const MYSQL: LiveKind = LiveKind { kind: ConnectionKind::Mysql, driver: ("dbengine-mysql", "abi-v2"), schemes: &["mysql"], env: "TABLEX_TEST_MYSQL_URI" };
const MARIADB: LiveKind = LiveKind { kind: ConnectionKind::Mariadb, driver: ("dbengine-mariadb", "abi-v2"), schemes: &["mariadb", "mysql"], env: "TABLEX_TEST_MARIADB_URI" };
const REDIS: LiveKind = LiveKind { kind: ConnectionKind::Redis, driver: ("dbengine-redis", "abi-v2"), schemes: &["redis", "rediss"], env: "TABLEX_TEST_REDIS_URI" };
const MONGODB: LiveKind = LiveKind { kind: ConnectionKind::Mongodb, driver: ("dbengine-mongo", "abi-v2"), schemes: &["mongodb", "mongodb+srv"], env: "TABLEX_TEST_MONGO_URI" };
const CLICKHOUSE: LiveKind = LiveKind { kind: ConnectionKind::Clickhouse, driver: ("dbengine-clickhouse", ""), schemes: &["clickhouse", "http", "https"], env: "TABLEX_TEST_CLICKHOUSE_URI" };

/// The URI as the user writes it for a database only the bastion reaches: the container's
/// published port under the name the bastion's container knows the host by.
fn behind_the_bastion(uri: &str) -> String {
    let mut url = url::Url::parse(uri).expect("a database URI");
    url.set_host(Some("host.docker.internal")).expect("a host");
    url.to_string()
}

/// A tunnelled connection of one kind on the real driver, with the real engine-net opener.
struct LiveRig<'a> {
    app: gpui::Entity<AppRoot>,
    visual: &'a mut gpui::VisualTestContext,
    key: String,
    kind: ConnectionKind,
    uri: String,
    _store_dir: std::path::PathBuf,
    known_hosts: std::path::PathBuf,
}

impl<'a> LiveRig<'a> {
    fn new(cx: &'a mut TestAppContext, live: &LiveKind, uri: String, settings: SshSettings) -> Self {
        let dylib = built_driver(live.driver.0, live.driver.1);
        cx.dispatcher.allow_parking();
        cx.update(|cx| cx.set_reduce_motion(true));
        let (app, visual) = sidebar_test_app(cx);
        visual.simulate_resize(gpui::size(gpui::px(1100.0), gpui::px(850.0)));
        settle(visual);
        let store_dir = std::env::temp_dir().join(format!("tablex-live-tunnel-{}", uuid::Uuid::new_v4()));
        let store = engine_driver_host::InstalledDriverStore::new(&store_dir);
        let driver_id = live.kind.driver_id();
        let sha256 = driver_sha256(&dylib);
        store
            .save(engine_driver_host::InstalledDriverRecord {
                id: driver_id.into(),
                version: "0.0.0-test".into(),
                path: dylib,
                sha256,
                team_id: "TEST".into(),
                installed_at: "2026-10-11T00:00:00Z".into(),
                app_abi_version: 2,
                uri_schemes: live.schemes.iter().map(|scheme| scheme.to_string()).collect(),
                license: "MIT".into(),
                source_repo: String::new(),
                disabled: false,
                ..Default::default()
            })
            .unwrap();
        let id = uuid::Uuid::new_v4();
        let key = id.to_string();
        let known_hosts = store_dir.join("known_hosts");
        let (kind, uri_for_app, key_for_app, hosts) = (live.kind, uri.clone(), key.clone(), known_hosts.clone());
        app.update(visual, |this, cx| {
            this.scenario_ui.service = None;
            this.driver_ui.store = std::sync::Arc::new(store.clone());
            this.driver_ui.installed = drivers::DriverManagerUi::installed_ids(&store);
            this.tunnel_ui.opener = std::sync::Arc::new(EngineOpener);
            this.tunnel_ui.known_hosts = Some(hosts);
            this.tunnel_ui.settings_override.insert(key_for_app, settings);
            this.saved_connections.push(SavedConnection {
                id,
                name: "live tunnel".into(),
                uri: uri_for_app,
                kind,
                save_password: false,
                is_pinned: false,
                disabled: false,
                ssh_tunnel: false,
            });
            this.navigator.set_connections(this.saved_connections.clone());
            this.sync_sidebar_connections(cx);
        });
        Self { app, visual, key, kind: live.kind, uri, _store_dir: store_dir, known_hosts }
    }

    fn secrets(&mut self, password: &str, passphrase: &str) {
        let (key, secrets) = (self.key.clone(), crate::ssh_tunnel::SshSecrets::from_text(password, passphrase));
        self.app.update(self.visual, |this, _| {
            this.tunnel_ui.secrets.insert(key, secrets);
        });
    }

    fn status(&self) -> ConnectionStatus {
        self.app.read_with(self.visual, |this, _| this.sessions.status(&self.key))
    }

    /// Connects, answering the first-use "Trust this host?" dialog like a user would.
    fn connect(&mut self) -> ConnectionStatus {
        let (key, uri) = (self.key.clone(), self.uri.clone());
        self.app.update(self.visual, |this, cx| this.ensure_connected_with_uri(key, Some(uri.into()), None, cx));
        let start = std::time::Instant::now();
        loop {
            settle(self.visual);
            if self.visual.debug_bounds("ssh-trust-accept").is_some() {
                click_debug_button(self.visual, "ssh-trust-accept");
            }
            match self.status() {
                ConnectionStatus::Connecting => {}
                done => return done,
            }
            assert!(start.elapsed() < std::time::Duration::from_secs(60), "the connect did not finish");
            std::thread::sleep(std::time::Duration::from_millis(20));
        }
    }

    fn disconnect(&mut self) {
        let key = self.key.clone();
        self.app.update(self.visual, |this, cx| this.disconnect_connection(&key, cx));
        settle(self.visual);
    }

    fn tunnel_port(&self) -> Option<u16> {
        self.app.read_with(self.visual, |this, _| {
            let live = this.tunnel_ui.live.get(&self.key)?;
            let target = live.tunnel.targets.first()?;
            live.tunnel.handle.local_addr(target).map(|addr| addr.port())
        })
    }

    fn session(&self) -> std::sync::Arc<engine_driver_host::SharedDriverSession> {
        self.app.read_with(self.visual, |this, _| this.driver_runtime.sessions.get(&self.key).cloned().expect("a live session"))
    }

    /// The first cell of a trivial query, run on the driver session of the connection.
    fn query_first_cell(&self) -> String {
        let session = self.session();
        let (kind, payload) = match self.kind {
            ConnectionKind::Redis => ("redis.command", serde_json::json!({"command": "PING"})),
            // The grid helper prints ClickHouse's UInt8 column as its type name.
            ConnectionKind::Clickhouse => ("sql", serde_json::json!({"sql": "SELECT toString(1) AS one"})),
            _ => ("sql", serde_json::json!({"sql": "SELECT 1 AS one"})),
        };
        let runner = session.query_runner().expect("a runner");
        let request = serde_json::to_string(&engine_driver_api::QueryRequest {
            kind: kind.into(),
            operation_id: runner.operation_id(),
            database: None,
            object: None,
            offset: 0,
            limit: 0,
            payload,
        })
        .unwrap();
        let page = runner.open_query(&request).expect("the query runs through the tunnel");
        let mut guard = session.lock().unwrap();
        guard.adopt_page(&runner, page.page_id).unwrap();
        let lease = guard.acquire_arrow_page(page.page_id, 0, page.row_count).unwrap();
        // SAFETY: the lease holds the Arrow C Data pair the driver exported.
        let imported = unsafe { lease.into_record_batch() }.unwrap();
        let text = crate::grid_arrow::array_value_to_string(imported.batch.column(0).as_ref(), 0);
        drop(imported);
        drop(guard);
        runner.release_page(page.page_id);
        text
    }

    fn log_codes(&self) -> Vec<Option<String>> {
        self.app.read_with(self.visual, |this, _| {
            (0..this.error_log.log.len())
                .filter_map(|index| this.error_log.log.get(index))
                .map(|entry| entry.code.clone())
                .collect()
        })
    }
}

fn live_env(live: &LiveKind) -> Option<(Bastion, String)> {
    let Some(bastion) = Bastion::from_env() else {
        eprintln!("skipped: source the env file of rust-engine/scripts/test-sshd.sh");
        return None;
    };
    let Ok(uri) = std::env::var(live.env) else {
        eprintln!("skipped: set {}", live.env);
        return None;
    };
    Some((bastion, behind_the_bastion(&uri)))
}

/// What the trivial query returns; MongoDB's connect already talks to the server (no query here).
fn expected_cell(kind: ConnectionKind) -> Option<&'static str> {
    match kind {
        ConnectionKind::Redis => Some("PONG"),
        ConnectionKind::Mongodb => None,
        _ => Some("1"),
    }
}

/// Connect and run a query through a tunnel with a password, with an unencrypted key and with an
/// encrypted key + passphrase; one tunnel per session; the host key went through the dialog.
fn plain_kind_through_the_tunnel(cx: &mut TestAppContext, live: LiveKind) {
    let Some((bastion, uri)) = live_env(&live) else { return };
    let mut rig = LiveRig::new(cx, &live, uri, bastion.settings(SshAuthMethod::Password));
    let before = bastion.sessions();
    for (label, settings, password, passphrase) in [
        ("password", bastion.settings(SshAuthMethod::Password), bastion.password.as_str(), ""),
        ("key", bastion.settings(SshAuthMethod::KeyFile), "", ""),
        (
            "encrypted key",
            SshSettings { key_path: bastion.key_encrypted.clone(), ..bastion.settings(SshAuthMethod::KeyFile) },
            "",
            bastion.passphrase.as_str(),
        ),
    ] {
        let key = rig.key.clone();
        rig.app.update(rig.visual, |this, _| {
            this.tunnel_ui.settings_override.insert(key, settings);
        });
        rig.secrets(password, passphrase);
        let status = rig.connect();
        assert_eq!(status, ConnectionStatus::Connected, "{label}: {status:?}");
        if let Some(cell) = expected_cell(live.kind) {
            assert_eq!(rig.query_first_cell(), cell, "{label}");
        }
        assert!(rig.tunnel_port().is_some(), "{label}: the session has a tunnel");
        assert_eq!(bastion.sessions(), before + 1, "{label}: one SSH connection for the one session");
        let via = rig.app.read_with(rig.visual, |this, _| this.via_ssh(&rig.key));
        assert_eq!(via.as_deref(), Some(format!("via SSH {}@{}:{}", bastion.user, bastion.host, bastion.port).as_str()));
        // The session URI is still the one the user wrote; the driver talked to 127.0.0.1.
        rig.app.read_with(rig.visual, |this, _| {
            assert!(this.driver_runtime.session_uris.get(&rig.key).unwrap().expose_for_test().contains("host.docker.internal"));
        });
        rig.disconnect();
        let mut gone = false;
        for _ in 0..100 {
            if bastion.sessions() == before {
                gone = true;
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(50));
        }
        assert!(gone, "{label}: disconnecting closed the tunnel's SSH connection");
    }
    // The host key was trusted once, in the app's own file, through the dialog.
    let recorded = std::fs::read_to_string(&rig.known_hosts).expect("the app known_hosts file");
    assert_eq!(recorded.lines().count(), 1, "{recorded}");
    assert!(!recorded.contains(&bastion.host), "entries are hashed");
    assert!(rig.log_codes().is_empty(), "{:?}", rig.log_codes());
}

#[gpui::test]
#[ignore = "needs tablex-test-sshd and TABLEX_TEST_PG_URI"]
fn live_tunnel_postgresql(cx: &mut TestAppContext) {
    plain_kind_through_the_tunnel(cx, PG);
}

#[gpui::test]
#[ignore = "needs tablex-test-sshd and TABLEX_TEST_MYSQL_URI"]
fn live_tunnel_mysql(cx: &mut TestAppContext) {
    plain_kind_through_the_tunnel(cx, MYSQL);
}

#[gpui::test]
#[ignore = "needs tablex-test-sshd and TABLEX_TEST_MARIADB_URI"]
fn live_tunnel_mariadb(cx: &mut TestAppContext) {
    plain_kind_through_the_tunnel(cx, MARIADB);
}

#[gpui::test]
#[ignore = "needs tablex-test-sshd and TABLEX_TEST_REDIS_URI"]
fn live_tunnel_redis(cx: &mut TestAppContext) {
    plain_kind_through_the_tunnel(cx, REDIS);
}

/// MongoDB through the tunnel (`directConnection=true`) against a private `mongod` on port 27999
/// (never 27017) that the bastion's container reaches as `host.docker.internal:27999`:
///   mongod --port 27999 --bind_ip 127.0.0.1 --dbpath <empty temp dir>
///   TABLEX_TEST_MONGO_URI=mongodb://127.0.0.1:27999
#[gpui::test]
#[ignore = "needs tablex-test-sshd and a private mongod on 27999 (TABLEX_TEST_MONGO_URI)"]
fn live_tunnel_mongodb(cx: &mut TestAppContext) {
    if let Ok(uri) = std::env::var("TABLEX_TEST_MONGO_URI") {
        assert!(uri.contains(":27999"), "only the private mongod on 27999 is ever used: {uri}");
    }
    plain_kind_through_the_tunnel(cx, MONGODB);
}

#[gpui::test]
#[ignore = "needs tablex-test-sshd and TABLEX_TEST_CLICKHOUSE_URI"]
fn live_tunnel_clickhouse(cx: &mut TestAppContext) {
    plain_kind_through_the_tunnel(cx, CLICKHOUSE);
}

#[gpui::test]
#[ignore = "needs tablex-test-sshd and TABLEX_TEST_PG_URI"]
fn live_tunnel_one_tunnel_per_database_session_and_the_apply_session_has_its_own(cx: &mut TestAppContext) {
    use crate::sql_ddl::apply::StatementRunner as _;
    let Some((bastion, uri)) = live_env(&PG) else { return };
    let mut rig = LiveRig::new(cx, &PG, uri, bastion.settings(SshAuthMethod::Password));
    rig.secrets(&bastion.password, "");
    let before = bastion.sessions();
    assert_eq!(rig.connect(), ConnectionStatus::Connected);
    assert_eq!(bastion.sessions(), before + 1, "the main session");

    // The diagram's dedicated apply session: its own driver session, its own tunnel.
    let (handle, access, original) = rig.app.read_with(rig.visual, |this, _| {
        (
            this.driver_runtime.session_handles.get(&rig.key).cloned().unwrap(),
            this.secondary_tunnel_access(&rig.key).expect("a tunnel to copy the settings of"),
            this.driver_runtime.session_uris.get(&rig.key).unwrap().expose_for_test().to_owned(),
        )
    });
    assert!(access.shared.is_none(), "a password bastion asks nothing: no sharing");
    let control = std::sync::Arc::new(crate::app::structure_edit::apply::ApplyControl::default());
    let ports = std::thread::spawn(move || {
        let mut apply = crate::app::structure_edit::apply::DriverApplySession::connect_for_test(handle, &original, None, ConnectionKind::Postgres, control, Some(access))
            .expect("the apply session connects through a tunnel of its own");
        apply.run("SELECT 1").expect("it runs a statement");
        apply
    });
    let apply = ports.join().unwrap();
    assert_eq!(bastion.sessions(), before + 2, "main + apply: one tunnel per database session");
    // The main session keeps working while the apply session lives.
    assert_eq!(rig.query_first_cell(), "1");
    drop(apply);
    let mut one_left = false;
    for _ in 0..100 {
        if bastion.sessions() == before + 1 {
            one_left = true;
            break;
        }
        std::thread::sleep(std::time::Duration::from_millis(50));
    }
    assert!(one_left, "dropping the apply session dropped its tunnel and only its tunnel");
    assert_eq!(rig.query_first_cell(), "1");
    rig.disconnect();
}

#[gpui::test]
#[ignore = "needs tablex-test-sshd and TABLEX_TEST_PG_URI"]
fn live_tunnel_a_killed_bastion_session_shows_the_banner_and_reconnect_opens_a_fresh_tunnel(cx: &mut TestAppContext) {
    let Some((bastion, uri)) = live_env(&PG) else { return };
    let mut rig = LiveRig::new(cx, &PG, uri, bastion.settings(SshAuthMethod::Password));
    rig.secrets(&bastion.password, "");
    assert_eq!(rig.connect(), ConnectionStatus::Connected);
    assert_eq!(rig.query_first_cell(), "1");
    let first_port = rig.tunnel_port().expect("a tunnel");

    // A tab on the connection, so the failing request goes through the real UI path.
    let key = rig.key.clone();
    let tab = rig.app.update(rig.visual, |this, cx| {
        let tab = this.workspace.open_query_tab_with_metadata("SQL", TabMetadata { connection_id: key.clone(), ..Default::default() });
        this.reset_active_view();
        this.workspace.tab_mut(tab).unwrap().sql = "SELECT 1".into();
        cx.notify();
        tab
    });
    let _ = tab;
    bastion.kill_sessions();
    let (app, key) = (rig.app.clone(), rig.key.clone());
    wait_until(rig.visual, "the tunnel to notice", |visual| {
        app.read_with(visual, |this, _| this.tunnel_failure(&key).is_some())
    });

    // The next request fails; the connection flips to the reconnect state with the tunnel's code.
    rig.app.update(rig.visual, |this, cx| this.run_active_tab_query(cx));
    let (app, key) = (rig.app.clone(), rig.key.clone());
    wait_until(rig.visual, "the lost connection", |visual| {
        app.read_with(visual, |this, _| matches!(this.sessions.status(&key), ConnectionStatus::Failed(_)))
    });
    let ConnectionStatus::Failed(reason) = rig.status() else { unreachable!() };
    assert!(reason.contains("SSH tunnel") && reason.contains("tunnel."), "{reason}");
    settle(rig.visual);
    assert!(rig.visual.debug_bounds("connection-lost-banner").is_some() || rig.visual.debug_bounds("connection-lost-placeholder").is_some());
    assert!(rig.log_codes().iter().flatten().any(|code| code.starts_with("tunnel.")), "{:?}", rig.log_codes());

    // Reconnect: a fresh tunnel on another local port, and the query works again.
    assert_eq!(rig.connect(), ConnectionStatus::Connected);
    let second_port = rig.tunnel_port().expect("a new tunnel");
    assert_ne!(first_port, second_port, "never the dead tunnel's local port");
    assert_eq!(rig.query_first_cell(), "1");
    rig.disconnect();
}

#[gpui::test]
#[ignore = "needs tablex-test-sshd and TABLEX_TEST_PG_URI"]
fn live_tunnel_keyboard_interactive_asks_the_real_servers_prompt_and_shares_one_tunnel(cx: &mut TestAppContext) {
    let Some((bastion, uri)) = live_env(&PG) else { return };
    let mut rig = LiveRig::new(cx, &PG, uri, bastion.settings(SshAuthMethod::KeyboardInteractive));
    let (app, key) = (rig.app.clone(), rig.key.clone());
    app.update(rig.visual, |this, cx| {
        let uri = this.saved_connections.iter().find(|s| s.matches_id(&key)).unwrap().uri.clone();
        this.ensure_connected_with_uri(key.clone(), Some(uri.into()), None, cx);
    });
    // The first thing to appear is the trust question, then the server's own password prompt.
    let start = std::time::Instant::now();
    loop {
        settle(rig.visual);
        if rig.visual.debug_bounds("ssh-trust-accept").is_some() {
            click_debug_button(rig.visual, "ssh-trust-accept");
        }
        if rig.visual.debug_bounds("ssh-ki-input-0").is_some() {
            break;
        }
        assert!(start.elapsed() < std::time::Duration::from_secs(60), "no keyboard-interactive dialog");
        std::thread::sleep(std::time::Duration::from_millis(20));
    }
    click_debug_button(rig.visual, "ssh-ki-input-0");
    rig.visual.simulate_input(&bastion.password);
    click_debug_button(rig.visual, "ssh-ki-ok");
    let (app, key) = (rig.app.clone(), rig.key.clone());
    wait_until(rig.visual, "the connection", |visual| {
        app.read_with(visual, |this, _| !matches!(this.sessions.status(&key), ConnectionStatus::Connecting))
    });
    assert_eq!(rig.status(), ConnectionStatus::Connected);
    assert_eq!(rig.query_first_cell(), "1");
    rig.app.read_with(rig.visual, |this, _| {
        assert!(this.secondary_tunnel_access(&rig.key).unwrap().shared.is_some(), "one tunnel for the connection");
    });
    rig.disconnect();
}

/// What the TLS servers of `rust-engine/scripts/test-tls-servers.sh` are called from the bastion.
fn tls_uri(scheme: &str, user: &str, password: &str, port: &str, database: &str, query: &str, name: &str) -> String {
    format!("{scheme}://{user}:{password}@{name}:{port}/{database}?{query}")
}

/// TLS through the tunnel keeps verifying the real server name `db.tablex.test`; a wrong name fails.
fn tls_through_the_tunnel(cx: &mut TestAppContext, live: LiveKind, scheme: &str, prefix: &str, query: impl Fn(&str) -> String) {
    let (Some(bastion), Ok(ca), Ok(name)) = (Bastion::from_env(), std::env::var("TABLEX_TEST_TLS_CA"), std::env::var("TABLEX_TEST_TLS_SERVER_NAME")) else {
        eprintln!("skipped: source the env files of test-sshd.sh and test-tls-servers.sh");
        return;
    };
    let var = |suffix: &str| std::env::var(format!("TABLEX_TEST_{prefix}_TLS_{suffix}")).expect(suffix);
    let (user, password, port, database) = (var("USER"), var("PASSWORD"), var("PORT"), var("DB"));
    let mut settings = bastion.settings(SshAuthMethod::Password);
    // The bastion knows the TLS server as host.docker.internal; the certificate says db.tablex.test.
    settings.remote_host = "host.docker.internal".into();
    for (label, server_name, should_connect) in [("right name", name.clone(), true), ("wrong name", "wrong.tablex.test".to_string(), false)] {
        let uri = tls_uri(scheme, &user, &password, &port, &database, &query(&ca), &server_name);
        let mut rig = LiveRig::new(cx, &live, uri, settings.clone());
        rig.secrets(&bastion.password, "");
        let status = rig.connect();
        if should_connect {
            assert_eq!(status, ConnectionStatus::Connected, "{label}: {status:?}");
            assert_eq!(rig.query_first_cell(), "1", "{label}");
            rig.disconnect();
        } else {
            let ConnectionStatus::Failed(message) = status else { panic!("{label}: a wrong name must not connect: {status:?}") };
            assert!(
                message.to_lowercase().contains("name") || message.contains("tls_certificate"),
                "{label}: the certificate check named the problem: {message}"
            );
            assert!(!message.contains(&password) && !message.contains(&bastion.password));
        }
    }
}

#[gpui::test]
#[ignore = "needs tablex-test-sshd and tablex-test-pg-tls"]
fn live_tunnel_postgresql_verify_full_keeps_verifying_the_real_name(cx: &mut TestAppContext) {
    tls_through_the_tunnel(cx, PG, "postgres", "PG", |ca| format!("sslmode=verify-full&sslrootcert={ca}"));
}

#[gpui::test]
#[ignore = "needs tablex-test-sshd and tablex-test-mysql-tls"]
fn live_tunnel_mysql_verify_identity_keeps_verifying_the_real_name(cx: &mut TestAppContext) {
    tls_through_the_tunnel(cx, MYSQL, "mysql", "MYSQL", |ca| format!("ssl-mode=VERIFY_IDENTITY&ssl-ca={ca}"));
}

#[gpui::test]
#[ignore = "needs tablex-test-sshd"]
fn live_tunnel_tls_for_mongodb_redis_and_clickhouse_is_refused_before_any_ssh_connection(cx: &mut TestAppContext) {
    let Some(bastion) = Bastion::from_env() else {
        eprintln!("skipped: source the env file of rust-engine/scripts/test-sshd.sh");
        return;
    };
    let mongo = LiveKind { kind: ConnectionKind::Mongodb, driver: ("dbengine-mongo", "abi-v2"), schemes: &["mongodb", "mongodb+srv"], env: "" };
    let cases = [
        (REDIS, "rediss://host.docker.internal:6380/0", "TLS to Redis through an SSH tunnel is not supported yet"),
        (CLICKHOUSE, "clickhouse://host.docker.internal:8443/db?secure=true", "TLS to ClickHouse through an SSH tunnel is not supported yet"),
        (mongo, "mongodb://host.docker.internal:27017/db?tls=true", "TLS to MongoDB through an SSH tunnel is not supported yet"),
    ];
    for (live, uri, expected) in cases {
        let before = bastion.sessions();
        let mut rig = LiveRig::new(cx, &live, uri.to_string(), bastion.settings(SshAuthMethod::Password));
        rig.secrets(&bastion.password, "");
        let status = rig.connect();
        let ConnectionStatus::Failed(message) = status else { panic!("{uri}: {status:?}") };
        assert!(message.starts_with(expected), "{message}");
        assert_eq!(bastion.sessions(), before, "{uri}: no SSH connection was made");
        assert!(rig.tunnel_port().is_none());
    }
}

// ---- jump hosts and a proxy ------------------------------------------------------------------
//
// A two-hop chain through the one test container (jump = 127.0.0.1:52222, bastion = 127.0.0.1:22 as
// the first hop sees it), and an in-process SOCKS5 proxy (below), alone and in front of the
// bastion. Only the dedicated PostgreSQL (127.0.0.1:55432, `host.docker.internal:55432` from
// inside the container) is used.
//
//   cargo test -p tablex-gpui live_tunnel_chain live_tunnel_socks5 -- --ignored --test-threads=1

/// A SOCKS5 proxy on 127.0.0.1 for tests: threads and `std::net` only, optional username /
/// password login, names answered through `resolve` (so a test can prove the *proxy* resolved the
/// name), every request recorded.
struct TestSocks5 {
    addr: std::net::SocketAddr,
    requests: std::sync::Arc<std::sync::Mutex<Vec<String>>>,
    tunnels: std::sync::Arc<std::sync::atomic::AtomicUsize>,
    stop: std::sync::Arc<std::sync::atomic::AtomicBool>,
}

impl TestSocks5 {
    fn start(login: Option<(&str, &str)>, resolve: Vec<((String, u16), std::net::SocketAddr)>) -> Self {
        use std::sync::atomic::Ordering;
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        let requests = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
        let tunnels = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let stop = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
        let login = login.map(|(user, password)| (user.to_owned(), password.to_owned()));
        let resolve = std::sync::Arc::new(resolve);
        let (accept_requests, accept_tunnels, accept_stop) = (requests.clone(), tunnels.clone(), stop.clone());
        std::thread::spawn(move || {
            for stream in listener.incoming().flatten() {
                if accept_stop.load(Ordering::SeqCst) {
                    return;
                }
                let (requests, tunnels, login, resolve) = (accept_requests.clone(), accept_tunnels.clone(), login.clone(), resolve.clone());
                std::thread::spawn(move || {
                    let _ = Self::serve(stream, login.as_ref(), &resolve, &requests, &tunnels);
                });
            }
        });
        Self { addr, requests, tunnels, stop }
    }

    fn serve(
        mut client: std::net::TcpStream,
        login: Option<&(String, String)>,
        resolve: &[((String, u16), std::net::SocketAddr)],
        requests: &std::sync::Mutex<Vec<String>>,
        tunnels: &std::sync::atomic::AtomicUsize,
    ) -> std::io::Result<()> {
        use std::io::{Read as _, Write as _};
        let mut head = [0u8; 2];
        client.read_exact(&mut head)?;
        let mut methods = vec![0u8; head[1] as usize];
        client.read_exact(&mut methods)?;
        match login {
            Some((user, password)) => {
                if !methods.contains(&2) {
                    return client.write_all(&[5, 0xff]);
                }
                client.write_all(&[5, 2])?;
                let mut version = [0u8; 2];
                client.read_exact(&mut version)?;
                let mut got_user = vec![0u8; version[1] as usize];
                client.read_exact(&mut got_user)?;
                let mut length = [0u8; 1];
                client.read_exact(&mut length)?;
                let mut got_password = vec![0u8; length[0] as usize];
                client.read_exact(&mut got_password)?;
                if got_user != user.as_bytes() || got_password != password.as_bytes() {
                    return client.write_all(&[1, 1]);
                }
                client.write_all(&[1, 0])?;
            }
            None => client.write_all(&[5, 0])?,
        }
        let mut request = [0u8; 4];
        client.read_exact(&mut request)?;
        let host = match request[3] {
            1 => {
                let mut octets = [0u8; 4];
                client.read_exact(&mut octets)?;
                std::net::Ipv4Addr::from(octets).to_string()
            }
            3 => {
                let mut length = [0u8; 1];
                client.read_exact(&mut length)?;
                let mut name = vec![0u8; length[0] as usize];
                client.read_exact(&mut name)?;
                String::from_utf8_lossy(&name).into_owned()
            }
            _ => return client.write_all(&[5, 8, 0, 1, 0, 0, 0, 0, 0, 0]),
        };
        let mut port = [0u8; 2];
        client.read_exact(&mut port)?;
        let port = u16::from_be_bytes(port);
        requests.lock().unwrap().push(format!("{host}:{port}"));
        let target = match resolve.iter().find(|((name, wanted), _)| *name == host && *wanted == port) {
            Some((_, addr)) => std::net::TcpStream::connect(addr),
            None => std::net::TcpStream::connect((host.as_str(), port)),
        };
        let Ok(target) = target else {
            return client.write_all(&[5, 5, 0, 1, 0, 0, 0, 0, 0, 0]);
        };
        client.write_all(&[5, 0, 0, 1, 0, 0, 0, 0, 0, 0])?;
        tunnels.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        let (mut up_from, mut up_to) = (client.try_clone()?, target.try_clone()?);
        let uplink = std::thread::spawn(move || {
            let _ = std::io::copy(&mut up_from, &mut up_to);
            let _ = up_to.shutdown(std::net::Shutdown::Write);
        });
        let (mut down_from, mut down_to) = (target, client);
        let _ = std::io::copy(&mut down_from, &mut down_to);
        let _ = down_to.shutdown(std::net::Shutdown::Write);
        let _ = uplink.join();
        Ok(())
    }

    fn requests(&self) -> Vec<String> {
        self.requests.lock().unwrap().clone()
    }

    fn tunnels(&self) -> usize {
        self.tunnels.load(std::sync::atomic::Ordering::SeqCst)
    }
}

impl Drop for TestSocks5 {
    fn drop(&mut self) {
        self.stop.store(true, std::sync::atomic::Ordering::SeqCst);
        let _ = std::net::TcpStream::connect(self.addr);
    }
}

impl LiveRig<'_> {
    /// Types the secret of one hop of the route, as the dialog would.
    fn hop_secret(&mut self, hop: crate::ssh_tunnel::SecretHop, need: crate::ssh_tunnel::SecretNeed, text: &str) {
        let (key, secret) = (self.key.clone(), engine_net::Secret::new(text));
        self.app.update(self.visual, |this, _| {
            let entry = this.tunnel_ui.secrets.remove(&key).unwrap_or_default();
            let mut entry = entry;
            entry.set(hop, need, secret);
            this.tunnel_ui.secrets.insert(key, entry);
        });
    }
}

#[gpui::test]
#[ignore = "needs tablex-test-sshd and TABLEX_TEST_PG_URI"]
fn live_tunnel_chain_postgresql_through_two_hops(cx: &mut TestAppContext) {
    use crate::ssh_tunnel::{JumpSettings, SecretHop, SecretNeed};
    let Some((bastion, uri)) = live_env(&PG) else { return };
    // Hop 1 is the container as the host sees it; hop 2 is the same container as hop 1 sees itself.
    let settings = SshSettings {
        enabled: true,
        host: "127.0.0.1".into(),
        port: "22".into(),
        user: bastion.user.clone(),
        auth: SshAuthMethod::Password,
        save_secrets: false,
        jumps: vec![JumpSettings {
            host: bastion.host.clone(),
            port: bastion.port.to_string(),
            user: bastion.user.clone(),
            auth: SshAuthMethod::Password,
            key_path: String::new(),
        }],
        ..SshSettings::default()
    };
    let mut rig = LiveRig::new(cx, &PG, uri, settings);
    rig.hop_secret(SecretHop::Jump(0), SecretNeed::Password, &bastion.password);
    rig.hop_secret(SecretHop::Bastion, SecretNeed::Password, &bastion.password);
    let before = bastion.sessions();
    let status = rig.connect();
    assert_eq!(status, ConnectionStatus::Connected, "{status:?}");
    assert_eq!(rig.query_first_cell(), "1", "a query went through both hops");
    assert_eq!(bastion.sessions(), before + 2, "one SSH connection per hop");
    let via = rig.app.read_with(rig.visual, |this, _| this.via_ssh(&rig.key)).unwrap();
    assert_eq!(via, format!("via SSH {}@127.0.0.1 (1 jump)", bastion.user));
    // Both hosts went through the "Trust this host?" dialog and were recorded under their own host:port.
    let recorded = std::fs::read_to_string(&rig.known_hosts).expect("the app known_hosts file");
    assert_eq!(recorded.lines().count(), 2, "{recorded}");
    assert!(rig.log_codes().is_empty(), "{:?}", rig.log_codes());
    rig.disconnect();
    let mut gone = false;
    for _ in 0..100 {
        if bastion.sessions() == before {
            gone = true;
            break;
        }
        std::thread::sleep(std::time::Duration::from_millis(50));
    }
    assert!(gone, "disconnecting closed both hops");
}

#[gpui::test]
#[ignore = "needs TABLEX_TEST_PG_URI (the dedicated PostgreSQL on 127.0.0.1:55432)"]
fn live_tunnel_socks5_proxy_alone_reaches_postgresql_by_a_name_only_the_proxy_knows(cx: &mut TestAppContext) {
    use crate::ssh_tunnel::{ProxySettings, ProxyType, SecretHop, SecretNeed};
    let Ok(raw) = std::env::var(PG.env) else {
        eprintln!("skipped: set {}", PG.env);
        return;
    };
    let url = url::Url::parse(&raw).unwrap();
    let real = std::net::SocketAddr::from(([127, 0, 0, 1], url.port().expect("a port")));
    assert_eq!(real.port(), 55432, "only the dedicated PostgreSQL is ever used");
    // The URI names a host nothing resolves; the proxy maps it to the database. Reaching it at all
    // proves the connection went through the proxy.
    let mut named = url.clone();
    named.set_host(Some("db.proxy.test")).unwrap();
    let proxy = TestSocks5::start(Some(("svc", "proxy-pass")), vec![(("db.proxy.test".into(), real.port()), real)]);
    let settings = SshSettings {
        proxy: ProxySettings {
            enabled: true,
            kind: ProxyType::Socks5,
            host: "127.0.0.1".into(),
            port: proxy.addr.port().to_string(),
            user: "svc".into(),
            save_secrets: false,
        },
        ..SshSettings::default()
    };
    let mut rig = LiveRig::new(cx, &PG, named.to_string(), settings);
    rig.hop_secret(SecretHop::Proxy, SecretNeed::Password, "proxy-pass");
    let status = rig.connect();
    assert_eq!(status, ConnectionStatus::Connected, "{status:?}");
    assert_eq!(rig.query_first_cell(), "1");
    assert!(proxy.requests().iter().any(|request| request == &format!("db.proxy.test:{}", real.port())), "{:?}", proxy.requests());
    assert!(proxy.tunnels() >= 1);
    let via = rig.app.read_with(rig.visual, |this, _| this.via_ssh(&rig.key)).unwrap();
    assert_eq!(via, format!("via SOCKS5 proxy 127.0.0.1:{}", proxy.addr.port()));
    rig.disconnect();

    // A wrong proxy password fails with the proxy's own code, naming the proxy, and never connects.
    rig.hop_secret(SecretHop::Proxy, SecretNeed::Password, "wrong");
    let status = rig.connect();
    let ConnectionStatus::Failed(message) = status else { panic!("{status:?}") };
    assert!(message.contains("proxy"), "{message}");
    assert!(rig.log_codes().iter().flatten().any(|code| code == "tunnel.proxy"), "{:?}", rig.log_codes());
}

#[gpui::test]
#[ignore = "needs tablex-test-sshd and TABLEX_TEST_PG_URI"]
fn live_tunnel_socks5_proxy_in_front_of_the_bastion_carries_the_ssh_connection(cx: &mut TestAppContext) {
    use crate::ssh_tunnel::{ProxySettings, ProxyType, SecretHop, SecretNeed};
    let Some((bastion, uri)) = live_env(&PG) else { return };
    // The bastion is only reachable under a name the proxy resolves.
    let proxy = TestSocks5::start(None, vec![(("bastion.proxy.test".into(), 2222), std::net::SocketAddr::from(([127, 0, 0, 1], bastion.port)))]);
    let settings = SshSettings {
        enabled: true,
        host: "bastion.proxy.test".into(),
        port: "2222".into(),
        user: bastion.user.clone(),
        auth: SshAuthMethod::Password,
        save_secrets: false,
        proxy: ProxySettings {
            enabled: true,
            kind: ProxyType::Socks5,
            host: "127.0.0.1".into(),
            port: proxy.addr.port().to_string(),
            user: String::new(),
            save_secrets: false,
        },
        ..SshSettings::default()
    };
    let mut rig = LiveRig::new(cx, &PG, uri, settings);
    rig.hop_secret(SecretHop::Bastion, SecretNeed::Password, &bastion.password);
    let status = rig.connect();
    assert_eq!(status, ConnectionStatus::Connected, "{status:?}");
    assert_eq!(rig.query_first_cell(), "1");
    // One dial for the first-use host key question, one after trusting it: nothing else (the database
    // leg is inside the SSH connection, not a connection of its own).
    let requests = proxy.requests();
    assert!(!requests.is_empty() && requests.iter().all(|request| request == "bastion.proxy.test:2222"), "{requests:?}");
    let via = rig.app.read_with(rig.visual, |this, _| this.via_ssh(&rig.key)).unwrap();
    assert_eq!(via, format!("via SSH {}@bastion.proxy.test:2222 through SOCKS5 proxy 127.0.0.1:{}", bastion.user, proxy.addr.port()));
    rig.disconnect();
}
