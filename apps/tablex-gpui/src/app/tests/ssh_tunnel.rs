// SSH tunnels through the app, driven through the real UI: the connection dialog's Tunnel tab
// (enable, fill, authentication method, `~/.ssh/config` picker, save and reopen, "Test tunnel"),
// the "Trust this host?" / changed-key / password / one-time-code dialogs, and the connect flow
// on the REAL Redis driver dylib. The bastion is the scripted `FakeOpener` (no network); a tiny
// RESP server stands in for the database behind it. The `live_*` tests at the end are `#[ignore]`d
// and use the dedicated `tablex-test-sshd` container and `tablex-test-*` databases.

use crate::ssh_tunnel::fake::{changed_host, unknown_host, FakeOpener};
use crate::ssh_tunnel::{SshAuthMethod, SshSettings};
use std::io::Read as _;

const BASTION: &str = "bastion.example.com";

/// A database that answers just enough RESP for the Redis driver to connect.
struct MiniRedis {
    addr: std::net::SocketAddr,
    accepted: std::sync::Arc<std::sync::atomic::AtomicUsize>,
}

impl MiniRedis {
    fn start() -> Self {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        let accepted = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let counter = accepted.clone();
        std::thread::spawn(move || {
            for stream in listener.incoming().flatten() {
                counter.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                std::thread::spawn(move || MiniRedis::serve(stream));
            }
        });
        Self { addr, accepted }
    }

    fn connections(&self) -> usize {
        self.accepted.load(std::sync::atomic::Ordering::SeqCst)
    }

    /// One RESP command (`*N` of bulk strings) from `buf`, with the bytes it took.
    fn command(buf: &[u8]) -> Option<(String, usize)> {
        fn line(buf: &[u8], at: &mut usize) -> Option<usize> {
            let end = buf[*at..].windows(2).position(|pair| pair == b"\r\n")?;
            let number = std::str::from_utf8(&buf[*at + 1..*at + end]).ok()?.parse().ok()?;
            *at += end + 2;
            Some(number)
        }
        let mut at = 0;
        let count = line(buf, &mut at)?;
        let mut name = String::new();
        for index in 0..count {
            let length = line(buf, &mut at)?;
            if buf.len() < at + length + 2 {
                return None;
            }
            if index == 0 {
                name = String::from_utf8_lossy(&buf[at..at + length]).to_ascii_uppercase();
            }
            at += length + 2;
        }
        Some((name, at))
    }

    fn serve(mut stream: std::net::TcpStream) {
        let (mut buf, mut chunk) = (Vec::new(), [0u8; 4096]);
        loop {
            match stream.read(&mut chunk) {
                Ok(0) | Err(_) => return,
                Ok(read) => buf.extend_from_slice(&chunk[..read]),
            }
            while let Some((name, used)) = Self::command(&buf) {
                buf.drain(..used);
                let reply = match name.as_str() {
                    "PING" => "+PONG\r\n".to_owned(),
                    "INFO" => {
                        let body = "# Server\r\nredis_version:7.2.0\r\n";
                        format!("${}\r\n{body}\r\n", body.len())
                    }
                    _ => "+OK\r\n".to_owned(),
                };
                if stream.write_all(reply.as_bytes()).is_err() {
                    return;
                }
            }
        }
    }
}

fn bastion_settings(auth: SshAuthMethod) -> SshSettings {
    SshSettings {
        enabled: true,
        host: BASTION.into(),
        port: "2222".into(),
        user: "deploy".into(),
        auth,
        save_secrets: false,
        ..SshSettings::default()
    }
}

/// An app with the real Redis driver installed and one Redis connection behind a bastion.
struct TunnelRig<'a> {
    app: gpui::Entity<AppRoot>,
    visual: &'a mut gpui::VisualTestContext,
    key: String,
    opener: std::sync::Arc<FakeOpener>,
    redis: MiniRedis,
    store_dir: std::path::PathBuf,
}

impl<'a> TunnelRig<'a> {
    fn new(cx: &'a mut TestAppContext, uri: &str, settings: SshSettings) -> Self {
        let dylib = built_driver("dbengine-redis", "abi-v2");
        cx.dispatcher.allow_parking();
        cx.update(|cx| cx.set_reduce_motion(true));
        let (app, visual) = sidebar_test_app(cx);
        visual.simulate_resize(gpui::size(gpui::px(1100.0), gpui::px(850.0)));
        settle(visual);
        let store_dir =
            std::env::temp_dir().join(format!("tablex-tunnel-store-{}", uuid::Uuid::new_v4()));
        let store = engine_driver_host::InstalledDriverStore::new(&store_dir);
        let sha256 = driver_sha256(&dylib);
        store
            .save(engine_driver_host::InstalledDriverRecord {
                id: "redis".into(),
                version: "0.0.0-test".into(),
                path: dylib,
                sha256,
                team_id: "TEST".into(),
                installed_at: "2026-10-11T00:00:00Z".into(),
                app_abi_version: 2,
                uri_schemes: vec!["redis".into(), "rediss".into()],
                license: "MIT".into(),
                source_repo: String::new(),
                disabled: false,
                ..Default::default()
            })
            .unwrap();
        let opener = FakeOpener::new();
        let redis = MiniRedis::start();
        *opener.forward_to.lock().unwrap() = Some(redis.addr);
        let id = uuid::Uuid::new_v4();
        let key = id.to_string();
        let (opener_for_app, uri, settings_key) = (opener.clone(), uri.to_owned(), key.clone());
        app.update(visual, |this, cx| {
            this.scenario_ui.service = None;
            this.driver_ui.store = std::sync::Arc::new(store.clone());
            this.driver_ui.installed = drivers::DriverManagerUi::installed_ids(&store);
            this.tunnel_ui.opener = opener_for_app;
            this.tunnel_ui.settings_override.insert(settings_key, settings);
            this.saved_connections.push(SavedConnection {
                id,
                name: "behind a bastion".into(),
                uri: uri.clone(),
                kind: ConnectionKind::Redis,
                save_password: false,
                is_pinned: false,
                disabled: false,
                ssh_tunnel: false,
            });
            this.navigator.set_connections(this.saved_connections.clone());
            this.sync_sidebar_connections(cx);
        });
        Self { app, visual, key, opener, redis, store_dir }
    }

    /// Marks the saved connection as one that goes through a tunnel (read from the record in the
    /// app) and forgets the in-memory settings, so the connect has to find them in storage.
    fn flag_as_tunnelled_without_settings(&mut self) {
        let key = self.key.clone();
        self.app.update(self.visual, |this, _| {
            this.tunnel_ui.settings_override.remove(&key);
            if let Some(saved) = this.saved_connections.iter_mut().find(|s| s.matches_id(&key)) {
                saved.ssh_tunnel = true;
            }
        });
    }

    fn uri(&self) -> String {
        self.app.read_with(self.visual, |this, _| {
            this.saved_connections.iter().find(|saved| saved.matches_id(&self.key)).unwrap().uri.clone()
        })
    }

    fn seed_secret(&mut self, password: &str) {
        let key = self.key.clone();
        let secrets = crate::ssh_tunnel::SshSecrets::from_text(password, "");
        self.app.update(self.visual, |this, _| {
            this.tunnel_ui.secrets.insert(key, secrets);
        });
    }

    fn connect(&mut self) {
        let (key, uri) = (self.key.clone(), self.uri());
        self.app.update(self.visual, |this, cx| {
            this.ensure_connected_with_uri(key, Some(uri.into()), None, cx);
        });
        settle(self.visual);
    }

    fn status(&self) -> ConnectionStatus {
        self.app.read_with(self.visual, |this, _| this.sessions.status(&self.key))
    }

    fn wait_status(&mut self, label: &'static str, ready: impl Fn(&ConnectionStatus) -> bool) {
        let (app, key) = (self.app.clone(), self.key.clone());
        wait_until(self.visual, label, |visual| {
            app.read_with(visual, |this, _| ready(&this.sessions.status(&key)))
        });
        settle(self.visual);
    }

    fn wait_connected(&mut self) {
        self.wait_status("the connection", |status| matches!(status, ConnectionStatus::Connected));
    }

    fn wait_failed(&mut self) -> String {
        self.wait_status("the connect to fail", |status| matches!(status, ConnectionStatus::Failed(_)));
        let ConnectionStatus::Failed(message) = self.status() else { unreachable!() };
        message
    }

    fn wait_dialog(&mut self, selector: &'static str) {
        let mut found = false;
        for _ in 0..400 {
            settle(self.visual);
            if self.visual.debug_bounds(selector).is_some() {
                found = true;
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(5));
        }
        if !found {
            let present: Vec<&str> = ["ssh-ki-host-line", "ssh-ki-quoted-0", "ssh-ki-prompt-0", "ssh-ki-input-0", "ssh-ki-ok", "ssh-ki-cancel", "ssh-trust-accept", "ssh-secret-input"]
                .into_iter()
                .filter(|s| self.visual.debug_bounds(s).is_some())
                .collect();
            panic!("{selector} is rendered (present: {present:?}, dialogs: {:?}, status: {:?})", self.dialogs(), self.status());
        }
    }

    fn rendered(&mut self, selector: &'static str) -> bool {
        settle(self.visual);
        self.visual.debug_bounds(selector).is_some()
    }

    fn click(&mut self, selector: &'static str) {
        settle(self.visual);
        click_debug_button(self.visual, selector);
        settle(self.visual);
    }

    fn dialogs(&self) -> Vec<&'static str> {
        self.app.read_with(self.visual, |this, _| this.tunnel_ui.opened_dialogs.clone())
    }

    /// `(code, retryable)` of every Error Log entry.
    fn log_codes(&self) -> Vec<Option<String>> {
        self.app.read_with(self.visual, |this, _| {
            (0..this.error_log.log.len())
                .filter_map(|index| this.error_log.log.get(index))
                .map(|entry| entry.code.clone())
                .collect()
        })
    }

    fn log_text(&self) -> String {
        self.app.read_with(self.visual, |this, _| this.error_log.log.export_text())
    }
}

impl Drop for TunnelRig<'_> {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.store_dir);
    }
}

// ---- the dialog ------------------------------------------------------------------------------

fn fill_row(visual: &mut gpui::VisualTestContext, row: &'static str, text: &str) {
    click(visual, row.into());
    visual.simulate_keystrokes("secondary-a");
    visual.simulate_input(text);
    settle(visual);
}

fn ssh_record(app: &gpui::Entity<AppRoot>, visual: &mut gpui::VisualTestContext) -> serde_json::Value {
    app.read_with(visual, |this, _| {
        let record = this.scenario_ui.service.as_ref().unwrap().last_saved_record().expect("a saved record");
        serde_json::from_str(&record.network_settings_json).unwrap()
    })
}

#[gpui::test]
fn ssh_tunnel_the_tunnel_tab_fills_saves_and_reopens_without_putting_a_secret_in_the_record(cx: &mut TestAppContext) {
    cx.dispatcher.allow_parking();
    let (app, visual) = dialog_app(cx);
    let pg = crate::ui_fixture::POSTGRES_ID;
    let inputs = open_fixture_dialog(&app, visual, pg);
    click(visual, "connection-config-tab-tunnel".into());
    for selector in [
        "connection-config-tunnel-notice",
        "connection-config-ssh-enable",
        "connection-config-ssh-host",
        "connection-config-ssh-user",
        "connection-config-ssh-auth",
        "connection-config-ssh-test",
        "connection-config-ssh-legacy",
        "connection-config-proxy",
    ] {
        assert!(rendered(visual, selector.into()), "{selector}");
    }
    assert!(!inputs.ssh.enabled.get(), "a new tunnel starts off");

    click(visual, "ssh-enable-switch".into());
    assert!(inputs.ssh.enabled.get(), "the switch turned the tunnel on");
    fill_row(visual, "connection-config-ssh-host", BASTION);
    fill_row(visual, "connection-config-ssh-port", "2222");
    fill_row(visual, "connection-config-ssh-user", "deploy");

    // Password is the default method: its field is there, the key fields are not.
    assert!(rendered(visual, "connection-config-ssh-password".into()));
    assert!(!rendered(visual, "connection-config-ssh-key".into()));
    fill_row(visual, "connection-config-ssh-password", "ssh-password-needle");

    // Switch to a private key through the real select.
    click(visual, "connection-config-ssh-auth-select".into());
    visual.simulate_keystrokes("down enter");
    settle(visual);
    assert_eq!(inputs.ssh.auth.get(), SshAuthMethod::KeyFile, "the select updated the tab");
    assert!(rendered(visual, "connection-config-ssh-key".into()));
    assert!(rendered(visual, "connection-config-ssh-passphrase".into()));
    assert!(!rendered(visual, "connection-config-ssh-password".into()), "the password field went away");
    fill_row(visual, "connection-config-ssh-key", "/keys/id_ed25519");
    fill_row(visual, "connection-config-ssh-passphrase", "key-passphrase-needle");

    // The agent and keyboard-interactive methods explain themselves and need no secret.
    click(visual, "connection-config-ssh-auth-select".into());
    visual.simulate_keystrokes("down enter");
    settle(visual);
    assert_eq!(inputs.ssh.auth.get(), SshAuthMethod::Agent);
    assert!(rendered(visual, "connection-config-ssh-agent-note".into()));
    assert!(!rendered(visual, "connection-config-ssh-key".into()));
    click(visual, "connection-config-ssh-auth-select".into());
    visual.simulate_keystrokes("down enter");
    settle(visual);
    assert_eq!(inputs.ssh.auth.get(), SshAuthMethod::KeyboardInteractive);
    assert!(rendered(visual, "connection-config-ssh-ki-note".into()));
    // Back to the key.
    click(visual, "connection-config-ssh-auth-select".into());
    visual.simulate_keystrokes("up enter");
    settle(visual);
    click(visual, "connection-config-ssh-auth-select".into());
    visual.simulate_keystrokes("up enter");
    settle(visual);
    assert_eq!(inputs.ssh.auth.get(), SshAuthMethod::KeyFile);

    let read = visual.update(|_, cx| inputs.ssh.read(cx));
    assert_eq!(
        (read.host.as_str(), read.port.as_str(), read.user.as_str(), read.key_path.as_str()),
        (BASTION, "2222", "deploy", "/keys/id_ed25519")
    );
    let typed = visual.update(|_, cx| inputs.ssh.read_secrets(cx));
    assert_eq!(typed.passphrase.expose(), "key-passphrase-needle");

    // Save through the real button: the record has the settings and no secret.
    click(visual, "connection-save".into());
    let json = ssh_record(&app, visual);
    assert_eq!(json["ssh"]["isEnabled"], true);
    assert_eq!(json["ssh"]["host"], BASTION);
    assert_eq!(json["ssh"]["authMethod"], "key");
    assert_eq!(json["ssh"]["privateKeyPath"], "/keys/id_ed25519");
    let text = json.to_string();
    for secret in ["ssh-password-needle", "key-passphrase-needle"] {
        assert!(!text.contains(secret), "{secret} reached the record: {text}");
    }
    // Closing the dialog emptied the secret widgets: the text does not outlive it.
    let left = visual.update(|_, cx| inputs.ssh.read_secrets(cx));
    assert!(left.is_empty(), "the typed secrets were cleared with the dialog");

    // Reopening shows what was saved.
    let again = open_fixture_dialog(&app, visual, pg);
    click(visual, "connection-config-tab-tunnel".into());
    let reopened = visual.update(|_, cx| again.ssh.read(cx));
    assert_eq!(
        (reopened.enabled, reopened.host.as_str(), reopened.port.as_str(), reopened.user.as_str(), reopened.auth),
        (true, BASTION, "2222", "deploy", SshAuthMethod::KeyFile)
    );
    assert_eq!(reopened.key_path, "/keys/id_ed25519");
    assert!(rendered(visual, "connection-config-ssh-key".into()), "the key fields show for a key");
    let retyped = visual.update(|_, cx| again.ssh.read_secrets(cx));
    assert_eq!(retyped.passphrase.expose(), "", "secrets are never shown again");
}

#[test]
fn ssh_tunnel_saving_with_the_tunnel_on_refuses_what_cannot_go_through_one() {
    // A TLS Redis URI cannot be tunnelled yet: the form says so.
    let mut form = ConnectionFormState::new(ConnectionKind::Redis);
    form.name = "tls redis".into();
    form.use_uri = true;
    form.uri = "rediss://cache.example.com:6380/0".into();
    form.ssh = Some(bastion_settings(SshAuthMethod::Agent));
    let error = form.validate().expect_err("TLS Redis through a tunnel is refused");
    assert!(error.starts_with("TLS to Redis through an SSH tunnel is not supported yet"), "{error}");
    form.ssh.as_mut().unwrap().enabled = false;
    form.validate().expect("without the tunnel it is fine");
    form.ssh = Some(SshSettings { host: String::new(), ..bastion_settings(SshAuthMethod::Agent) });
    assert!(form.validate().unwrap_err().contains("SSH host is required"));
}

#[gpui::test]
fn ssh_tunnel_the_ssh_config_picker_fills_the_fields_and_refuses_hosts_the_tunnel_cannot_use(cx: &mut TestAppContext) {
    cx.dispatcher.allow_parking();
    let (app, visual) = dialog_app(cx);
    let pg = crate::ui_fixture::POSTGRES_ID;
    let inputs = open_fixture_dialog(&app, visual, pg);
    let config = engine_net::SshConfig::parse(
        "Host bastion\n  HostName bastion.example.com\n  Port 2200\n  User deploy\n  IdentityFile /keys/id_work\n  IdentityAgent /run/agent.sock\n\
         Host jumpy\n  HostName jumpy.example.com\n  ProxyJump hop.example.com\n\
         Host cmd\n  HostName cmd.example.com\n  ProxyCommand nc %h %p\n",
    )
    .unwrap();
    visual.update(|window, cx| inputs.ssh.set_config_result(Ok(config), window, cx));
    click(visual, "connection-config-tab-tunnel".into());
    click(visual, "ssh-enable-switch".into());

    click(visual, "connection-config-ssh-config-select".into());
    visual.simulate_keystrokes("down enter");
    settle(visual);
    let filled = visual.update(|_, cx| inputs.ssh.read(cx));
    assert_eq!(
        (filled.host.as_str(), filled.port.as_str(), filled.user.as_str(), filled.key_path.as_str(), filled.auth),
        ("bastion.example.com", "2200", "deploy", "/keys/id_work", SshAuthMethod::KeyFile)
    );
    assert_eq!(filled.config_host, "bastion");
    assert!(
        rendered(visual, "connection-config-ssh-config-ignored".into()),
        "IdentityAgent is reported as not applied: {:?}",
        inputs.ssh.config_note.borrow()
    );

    // A ProxyJump host fills the jump rows from its chain.
    click(visual, "connection-config-ssh-config-select".into());
    visual.simulate_keystrokes("down enter");
    settle(visual);
    assert!(!rendered(visual, "connection-config-ssh-config-refused".into()));
    let after = visual.update(|_, cx| inputs.ssh.read(cx));
    assert_eq!(after.host, "jumpy.example.com");
    assert_eq!(after.jumps.len(), 1, "the ProxyJump chain became a jump row");
    assert_eq!((after.jumps[0].host.as_str(), after.jumps[0].port.as_str()), ("hop.example.com", "22"));
    assert_eq!(after.jumps[0].auth, SshAuthMethod::Agent, "no IdentityFile: the agent");
    assert!(rendered(visual, "ssh-jump-0-row".into()), "and the row is on the tab");
    // ProxyCommand hosts are refused with engine-net's words and fill nothing.
    click(visual, "connection-config-ssh-config-select".into());
    visual.simulate_keystrokes("down enter");
    settle(visual);
    let note = inputs.ssh.config_note.borrow().clone();
    assert!(
        matches!(&note, crate::connection_dialog::ConfigNote::Refused(text) if text.contains("ProxyCommand")),
        "{note:?}"
    );
    let after = visual.update(|_, cx| inputs.ssh.read(cx));
    assert_eq!(after.host, "jumpy.example.com", "the refused host filled nothing");
    assert_eq!(after.jumps.len(), 1, "and left the rows alone");
}

#[gpui::test]
fn ssh_tunnel_test_tunnel_reports_success_failure_and_asks_before_trusting_a_new_host(cx: &mut TestAppContext) {
    use crate::connection_dialog::TestStatus;
    cx.dispatcher.allow_parking();
    let (app, visual) = dialog_app(cx);
    let pg = crate::ui_fixture::POSTGRES_ID;
    let opener = FakeOpener::new();
    let opener_for_app = opener.clone();
    app.update(visual, |this, _| this.tunnel_ui.opener = opener_for_app);
    let inputs = open_fixture_dialog(&app, visual, pg);
    click(visual, "connection-config-tab-tunnel".into());

    // Off: the button does nothing yet.
    click(visual, "ssh-enable-switch".into());
    fill_row(visual, "connection-config-ssh-host", BASTION);
    fill_row(visual, "connection-config-ssh-user", "deploy");
    fill_row(visual, "connection-config-ssh-password", "ssh-password-needle");

    let status = || inputs.ssh.test_status.borrow().clone();
    let wait_done = |visual: &mut gpui::VisualTestContext| {
        let status = inputs.ssh.test_status.clone();
        wait_until(visual, "the tunnel test", |_| *status.borrow() != TestStatus::Running);
        settle(visual);
    };

    // Success: inline, no connection is made.
    click(visual, "ssh-test-tunnel".into());
    wait_done(visual);
    assert!(matches!(status(), TestStatus::Success { .. }), "{:?}", status());
    assert!(rendered(visual, "connection-ssh-test-status".into()));
    assert_eq!(opener.opened_count(), 1);
    {
        let opened = opener.opened.lock().unwrap();
        assert_eq!(opened[0].host, BASTION);
        assert_eq!(opened[0].user, "deploy");
        // The form's typed password reached the tunnel and nothing else.
        assert!(matches!(&opened[0].auth, engine_net::Auth::Password(secret) if secret.expose() == "ssh-password-needle"));
    }

    // The bastion cannot reach the database: the failure says what.
    *opener.probe_error.lock().unwrap() = Some(engine_net::TunnelError::ForwardRefused {
        host: "db.local".into(),
        port: 5432,
        reason: "connection refused".into(),
    });
    click(visual, "ssh-test-tunnel".into());
    wait_done(visual);
    let TestStatus::Failure(message) = status() else { panic!("a failure") };
    assert!(message.contains("cannot reach") && message.contains("connection refused"), "{message}");
    assert!(!message.contains("ssh-password-needle"));
    *opener.probe_error.lock().unwrap() = None;

    // An unknown host: nothing is trusted until the dialog says so.
    opener.then(Err(unknown_host(BASTION)));
    click(visual, "ssh-test-tunnel".into());
    let mut shown = false;
    for _ in 0..400 {
        settle(visual);
        if visual.debug_bounds("ssh-trust-accept").is_some() {
            shown = true;
            break;
        }
        std::thread::sleep(std::time::Duration::from_millis(5));
    }
    assert!(
        shown,
        "the Trust this host? dialog is shown (opened {}, status {:?}, dialogs {:?})",
        opener.opened_count(),
        inputs.ssh.test_status.borrow(),
        app.read_with(visual, |this, _| this.tunnel_ui.opened_dialogs.clone())
    );
    for selector in ["ssh-trust-host", "ssh-trust-port", "ssh-trust-algorithm", "ssh-trust-fingerprint", "ssh-trust-cancel"] {
        assert!(rendered(visual, selector.into()), "{selector}");
    }
    assert!(opener.trusted.lock().unwrap().is_empty(), "asking does not trust");
    click(visual, "ssh-trust-accept".into());
    wait_done(visual);
    assert!(matches!(status(), TestStatus::Success { .. }), "{:?}", status());
    let trusted = opener.trusted.lock().unwrap().clone();
    assert_eq!(trusted.len(), 1);
    assert_eq!((trusted[0].0.as_str(), trusted[0].1), (BASTION, 22));
    assert!(trusted[0].2.starts_with("ssh-ed25519 "));
    assert!(visual.debug_bounds("ssh-trust-accept").is_none(), "the dialog closed");

    // Declining leaves the key untrusted.
    opener.then(Err(unknown_host(BASTION)));
    click(visual, "ssh-test-tunnel".into());
    for _ in 0..400 {
        settle(visual);
        if visual.debug_bounds("ssh-trust-cancel").is_some() {
            break;
        }
        std::thread::sleep(std::time::Duration::from_millis(5));
    }
    click(visual, "ssh-trust-cancel".into());
    settle(visual);
    assert_eq!(opener.trusted.lock().unwrap().len(), 1, "cancel trusted nothing");
    assert!(matches!(status(), TestStatus::Failure(message) if message.contains("not trusted yet")));
}

#[gpui::test]
fn ssh_tunnel_a_changed_host_key_is_a_blocking_error_with_no_way_to_accept_it(cx: &mut TestAppContext) {
    use crate::connection_dialog::TestStatus;
    cx.dispatcher.allow_parking();
    let (app, visual) = dialog_app(cx);
    let pg = crate::ui_fixture::POSTGRES_ID;
    let opener = FakeOpener::new();
    opener.then(Err(changed_host(BASTION)));
    let opener_for_app = opener.clone();
    app.update(visual, |this, _| this.tunnel_ui.opener = opener_for_app);
    let inputs = open_fixture_dialog(&app, visual, pg);
    click(visual, "connection-config-tab-tunnel".into());
    click(visual, "ssh-enable-switch".into());
    fill_row(visual, "connection-config-ssh-host", BASTION);
    fill_row(visual, "connection-config-ssh-user", "deploy");
    fill_row(visual, "connection-config-ssh-password", "pw");
    click(visual, "ssh-test-tunnel".into());
    let mut shown = false;
    for _ in 0..400 {
        settle(visual);
        if visual.debug_bounds("ssh-hostkey-close").is_some() {
            shown = true;
            break;
        }
        std::thread::sleep(std::time::Duration::from_millis(5));
    }
    assert!(shown, "the key-changed dialog is shown");
    for selector in ["ssh-problem-algorithm", "ssh-problem-fingerprint", "ssh-problem-recorded"] {
        assert!(rendered(visual, selector.into()), "{selector}");
    }
    // The only button closes it: there is no accept path, now or after a refresh.
    assert!(visual.debug_bounds("ssh-trust-accept").is_none());
    let status = inputs.ssh.test_status.borrow().clone();
    assert!(matches!(&status, TestStatus::Failure(message) if message.contains("CHANGED")), "{status:?}");
    click(visual, "ssh-hostkey-close".into());
    assert!(visual.debug_bounds("ssh-hostkey-close").is_none());
    assert!(opener.trusted.lock().unwrap().is_empty());
    assert_eq!(opener.opened_count(), 1, "the failed test is not retried by itself");
}

// ---- the connect flow ------------------------------------------------------------------------

#[gpui::test]
fn ssh_tunnel_a_connection_behind_a_bastion_connects_to_the_tunnel_and_says_so(cx: &mut TestAppContext) {
    let mut rig = TunnelRig::new(cx, "redis://cache.example.com:6379/0", bastion_settings(SshAuthMethod::Password));
    rig.seed_secret("pw-memory-needle");
    rig.connect();
    rig.wait_connected();

    // The bastion was asked for the database endpoint, with the connection's credentials.
    assert_eq!(rig.opener.opened_count(), 1, "one session, one tunnel");
    {
        let opened = rig.opener.opened.lock().unwrap();
        assert_eq!(opened[0].targets, [engine_net::Target::new("cache.example.com", 6379)]);
        assert_eq!((opened[0].host.as_str(), opened[0].port, opened[0].user.as_str()), (BASTION, 2222, "deploy"));
        assert!(matches!(&opened[0].auth, engine_net::Auth::Password(secret) if secret.expose() == "pw-memory-needle"));
    }
    // The driver really connected to the tunnel's local address, not to the database host.
    assert!(rig.redis.connections() >= 1);
    rig.app.read_with(rig.visual, |this, _| {
        let session_uri = this.driver_runtime.session_uris.get(&rig.key).expect("the live session's URI").expose_for_test().to_owned();
        assert_eq!(session_uri, "redis://cache.example.com:6379/0", "the URI is kept as the user wrote it");
        assert_eq!(this.via_ssh(&rig.key).as_deref(), Some("via SSH deploy@bastion.example.com:2222"));
        let access = this.secondary_tunnel_access(&rig.key).expect("a second session can open its own tunnel");
        assert!(access.shared.is_none(), "one tunnel per database session");
    });

    // Status: the connection tab, the status bar and the sidebar row.
    let key = rig.key.clone();
    rig.app.update(rig.visual, |this, cx| {
        this.workspace.open_query_tab_with_metadata("Redis", TabMetadata { connection_id: key.clone(), ..Default::default() });
        this.reset_active_view();
        cx.notify();
    });
    settle(rig.visual);
    assert!(rig.rendered("status-ssh"), "the status bar shows the tunnel");
    let any_badge = (0..40).any(|index| rig.visual.debug_bounds(Box::leak(format!("sidebar-ssh-badge-{index}").into_boxed_str())).is_some());
    assert!(any_badge, "the sidebar row carries the SSH badge");

    // Disconnect closes the tunnel after the session.
    let key = rig.key.clone();
    rig.app.update(rig.visual, |this, cx| this.disconnect_connection(&key, cx));
    let drops = rig.opener.drops.clone();
    wait_until(rig.visual, "the tunnel to be dropped", |_| drops.load(std::sync::atomic::Ordering::SeqCst) == 1);
    rig.app.read_with(rig.visual, |this, _| {
        assert!(this.tunnel_ui.live.is_empty());
        assert!(this.tunnel_ui.secrets.is_empty(), "a deliberate disconnect forgets the typed secrets");
    });
}

#[gpui::test]
fn ssh_tunnel_a_dead_tunnel_shows_in_the_banner_with_its_code_and_a_reconnect_opens_a_fresh_one(cx: &mut TestAppContext) {
    let mut rig = TunnelRig::new(cx, "redis://cache.example.com:6379/0", bastion_settings(SshAuthMethod::Password));
    rig.seed_secret("pw");
    rig.connect();
    rig.wait_connected();
    let first_via = rig.app.read_with(rig.visual, |this, _| this.via_ssh(&rig.key));

    // The bastion drops the SSH session. Nothing polls: the next failing request notices.
    rig.opener.alive.store(false, std::sync::atomic::Ordering::SeqCst);
    let key = rig.key.clone();
    rig.app.update(rig.visual, |this, _| {
        this.note_navigator_connection_lost(&key, "driver rejected the request: navigator.io: connection reset");
    });
    let ConnectionStatus::Failed(reason) = rig.status() else { panic!("the banner state") };
    assert!(reason.contains("SSH tunnel") && reason.contains("tunnel.closed"), "{reason}");
    assert!(rig.log_codes().contains(&Some("tunnel.closed".to_string())), "{:?}", rig.log_codes());

    // Reconnect: a new tunnel, never the old local port, and no password prompt.
    rig.opener.alive.store(true, std::sync::atomic::Ordering::SeqCst);
    let key = rig.key.clone();
    rig.app.update(rig.visual, |this, cx| this.ensure_connected(key, None, cx));
    rig.wait_connected();
    assert_eq!(rig.opener.opened_count(), 2, "reconnecting always opens a fresh tunnel");
    assert!(rig.dialogs().is_empty(), "the typed secret was kept: {:?}", rig.dialogs());
    assert_eq!(rig.app.read_with(rig.visual, |this, _| this.via_ssh(&rig.key)), first_via);
    let drops = rig.opener.drops.clone();
    wait_until(rig.visual, "the dead tunnel to be dropped", |_| drops.load(std::sync::atomic::Ordering::SeqCst) == 1);
}

#[gpui::test]
fn ssh_tunnel_an_unknown_bastion_asks_first_and_connects_after_the_user_trusts_it(cx: &mut TestAppContext) {
    let mut rig = TunnelRig::new(cx, "redis://cache.example.com:6379/0", bastion_settings(SshAuthMethod::Agent));
    rig.opener.then(Err(unknown_host(BASTION)));
    rig.connect();
    rig.wait_dialog("ssh-trust-accept");
    assert!(matches!(rig.status(), ConnectionStatus::Connecting), "the connect waits for the answer");
    assert!(rig.opener.trusted.lock().unwrap().is_empty());
    rig.click("ssh-trust-accept");
    rig.wait_connected();
    let trusted = rig.opener.trusted.lock().unwrap().clone();
    assert_eq!(trusted.len(), 1, "the key the user saw was recorded");
    assert_eq!(rig.opener.opened_count(), 2, "then the tunnel was opened again");
    assert!(rig.log_codes().is_empty(), "{:?}", rig.log_codes());
}

#[gpui::test]
fn ssh_tunnel_declining_an_unknown_bastion_fails_the_connect_and_trusts_nothing(cx: &mut TestAppContext) {
    let mut rig = TunnelRig::new(cx, "redis://cache.example.com:6379/0", bastion_settings(SshAuthMethod::Agent));
    rig.opener.then(Err(unknown_host(BASTION)));
    rig.connect();
    rig.wait_dialog("ssh-trust-cancel");
    rig.click("ssh-trust-cancel");
    let message = rig.wait_failed();
    assert!(message.contains("not trusted"), "{message}");
    assert!(rig.opener.trusted.lock().unwrap().is_empty());
    assert_eq!(rig.opener.opened_count(), 1);
}

#[gpui::test]
fn ssh_tunnel_a_changed_host_key_fails_the_connect_with_its_code_and_offers_no_accept(cx: &mut TestAppContext) {
    let mut rig = TunnelRig::new(cx, "redis://cache.example.com:6379/0", bastion_settings(SshAuthMethod::Agent));
    rig.opener.then(Err(changed_host(BASTION)));
    rig.connect();
    let message = rig.wait_failed();
    assert!(message.contains("CHANGED") && message.contains("known_hosts"), "{message}");
    rig.wait_dialog("ssh-hostkey-close");
    assert!(!rig.rendered("ssh-trust-accept"), "no accept path");
    assert!(rig.log_codes().contains(&Some("tunnel.host_key_mismatch".to_string())), "{:?}", rig.log_codes());
    assert!(rig.opener.trusted.lock().unwrap().is_empty());
    rig.click("ssh-hostkey-close");
}

#[gpui::test]
fn ssh_tunnel_an_unsaved_ssh_password_is_asked_for_once_and_kept_only_while_connected(cx: &mut TestAppContext) {
    let mut rig = TunnelRig::new(cx, "redis://cache.example.com:6379/0", bastion_settings(SshAuthMethod::Password));
    rig.connect();
    rig.wait_dialog("ssh-secret-input");
    assert_eq!(rig.opener.opened_count(), 0, "nothing is sent before the password is known");
    click_debug_button(rig.visual, "ssh-secret-input");
    rig.visual.simulate_input("pw-typed-needle");
    rig.click("ssh-secret-ok");
    rig.wait_connected();
    {
        let opened = rig.opener.opened.lock().unwrap();
        assert!(matches!(&opened[0].auth, engine_net::Auth::Password(secret) if secret.expose() == "pw-typed-needle"));
    }
    assert!(!rig.log_text().contains("pw-typed-needle"));
    rig.app.read_with(rig.visual, |this, _| {
        assert!(this.tunnel_ui.secrets.contains_key(&rig.key), "kept for the connection's lifetime");
        let access = this.secondary_tunnel_access(&rig.key).unwrap();
        assert_eq!(access.secrets.password.as_ref().map(|secret| secret.expose().to_owned()).as_deref(), Some("pw-typed-needle"));
    });

    // Disconnected on purpose: it is forgotten and asked for again.
    let key = rig.key.clone();
    rig.app.update(rig.visual, |this, cx| this.disconnect_connection(&key, cx));
    settle(rig.visual);
    rig.connect();
    rig.wait_dialog("ssh-secret-cancel");
    rig.click("ssh-secret-cancel");
    let message = rig.wait_failed();
    assert!(message.contains("cancelled"), "{message}");
}

#[gpui::test]
fn ssh_tunnel_keyboard_interactive_questions_open_a_dialog_with_the_servers_prompts_and_can_be_cancelled(cx: &mut TestAppContext) {
    let mut rig = TunnelRig::new(cx, "redis://cache.example.com:6379/0", bastion_settings(SshAuthMethod::KeyboardInteractive));
    rig.opener.ask.lock().unwrap().extend([("Verification code:".to_string(), false), ("Device name:".to_string(), true)]);
    rig.connect();
    rig.wait_dialog("ssh-ki-input-1");
    assert!(rig.rendered("ssh-ki-input-0"));
    assert!(rig.rendered("ssh-ki-ok") && rig.rendered("ssh-ki-cancel"));
    click_debug_button(rig.visual, "ssh-ki-input-0");
    rig.visual.simulate_input("123456");
    click_debug_button(rig.visual, "ssh-ki-input-1");
    rig.visual.simulate_input("laptop");
    rig.click("ssh-ki-ok");
    rig.wait_connected();
    assert_eq!(
        rig.opener.answers.lock().unwrap().clone(),
        vec![Some(vec!["123456".to_string(), "laptop".to_string()])]
    );
    // A bastion that asks questions shares its one tunnel with the connection's other sessions.
    rig.app.read_with(rig.visual, |this, _| {
        let access = this.secondary_tunnel_access(&rig.key).unwrap();
        assert!(access.shared.is_some());
        assert!(this.via_ssh(&rig.key).unwrap().contains("shared"));
    });

    // Cancelling the question fails the sign-in.
    let key = rig.key.clone();
    rig.app.update(rig.visual, |this, cx| this.disconnect_connection(&key, cx));
    settle(rig.visual);
    rig.opener.ask.lock().unwrap().push(("Verification code:".to_string(), false));
    rig.connect();
    rig.wait_dialog("ssh-ki-cancel");
    rig.click("ssh-ki-cancel");
    let message = rig.wait_failed();
    assert!(message.contains("SSH tunnel") && message.contains("keyboard-interactive"), "{message}");
    assert!(rig.log_codes().contains(&Some("tunnel.auth".to_string())), "{:?}", rig.log_codes());
    assert_eq!(rig.opener.answers.lock().unwrap().last().cloned(), Some(None));
}

#[gpui::test]
fn ssh_tunnel_what_cannot_go_through_a_tunnel_is_refused_before_any_ssh_connection(cx: &mut TestAppContext) {
    let mut rig = TunnelRig::new(cx, "rediss://cache.example.com:6380/0", bastion_settings(SshAuthMethod::Agent));
    rig.connect();
    let message = rig.wait_failed();
    assert!(message.starts_with("TLS to Redis through an SSH tunnel is not supported yet"), "{message}");
    assert_eq!(rig.opener.opened_count(), 0);
    assert_eq!(rig.redis.connections(), 0);
}

#[gpui::test]
fn ssh_tunnel_the_dialogs_test_button_connects_through_the_forms_tunnel(cx: &mut TestAppContext) {
    use crate::connection_dialog::TestStatus;
    let rig = TunnelRig::new(cx, "redis://cache.example.com:6379/0", bastion_settings(SshAuthMethod::Password));
    let mut form = ConnectionFormState::new(ConnectionKind::Redis);
    form.name = "via ssh".into();
    form.host = "cache.example.com".into();
    form.port = "6379".into();
    form.ssh = Some(bastion_settings(SshAuthMethod::Password));
    form.ssh_secrets = crate::ssh_tunnel::SshSecretInput::new(engine_net::Secret::new("pw-form-needle"), engine_net::Secret::default());
    let status = std::rc::Rc::new(std::cell::RefCell::new(TestStatus::Idle));
    let (app, status_for_call) = (rig.app.clone(), status.clone());
    rig.visual.update(|window, cx| {
        app.update(cx, |this, cx| {
            this.start_connection_test(form.clone(), None, window.window_handle(), status_for_call.clone(), cx);
        });
    });
    let waiting = status.clone();
    wait_until(rig.visual, "the connection test", |_| !matches!(*waiting.borrow(), TestStatus::Running | TestStatus::Idle));
    assert!(matches!(&*status.borrow(), TestStatus::Success { .. }), "{:?}", status.borrow());
    assert_eq!(rig.opener.opened_count(), 1);
    assert!(rig.redis.connections() >= 1, "the test session went through the tunnel");
    let drops = rig.opener.drops.clone();
    wait_until(rig.visual, "the test tunnel to be dropped", |_| drops.load(std::sync::atomic::Ordering::SeqCst) == 1);
}


// ---- security review fixes -------------------------------------------------------------------

#[gpui::test]
fn ssh_tunnel_keyboard_interactive_dialog_always_names_the_server_and_never_uses_its_title(cx: &mut TestAppContext) {
    use engine_net::{CancelToken, PromptItem};
    // The text a hostile bastion would send.
    let hostile = format!(
        "Enter your macOS login password to continue.\nIgnore the line above.\u{7} {}",
        "A".repeat(400)
    );
    let (reply, _answers) = std::sync::mpsc::channel();
    let prompt = crate::ssh_tunnel::KiPrompt {
        host: "bastion.example.com".into(),
        port: 2222,
        name: "TableX".into(),
        instructions: hostile.clone(),
        prompts: vec![PromptItem { text: "Your macOS password:\n".into(), echo: false }],
        cancel: CancelToken::default(),
        reply,
        user: "deploy".into(),
    };
    let texts = super::tunnel_ui::ki_texts(&prompt);
    assert_eq!(texts.title, "SSH server asks");
    assert_ne!(texts.title, "TableX");
    assert_eq!(texts.host_line, "deploy@bastion.example.com:2222 asks:");
    assert_eq!(texts.server_text[0], "\u{201c}TableX\u{201d}", "the server's name is quoted text");
    let instructions = &texts.server_text[1];
    assert!(instructions.starts_with("\u{201c}Enter your macOS login password to continue. Ignore the line above. AAAA"), "{instructions}");
    assert!(!instructions.contains('\n') && !instructions.contains('\u{7}'));
    assert!(instructions.chars().count() <= 200 + 3, "capped: {}", instructions.chars().count());
    assert!(instructions.ends_with("\u{2026}\u{201d}"));
    assert_eq!(texts.prompts, ["\u{201c}Your macOS password:\u{201d}"]);

    // And the dialog really built from it.
    let mut rig = TunnelRig::new(cx, "redis://cache.example.com:6379/0", bastion_settings(SshAuthMethod::KeyboardInteractive));
    *rig.opener.ki_text.lock().unwrap() = ("TableX".into(), hostile);
    rig.opener.ask.lock().unwrap().push(("Your macOS password:".to_string(), false));
    rig.connect();
    rig.wait_dialog("ssh-ki-input-0");
    assert!(rig.rendered("ssh-ki-host-line"), "the server and login are always named");
    assert!(rig.rendered("ssh-ki-quoted-0") && rig.rendered("ssh-ki-quoted-1"), "the server's words sit in quoted boxes");
    assert!(rig.rendered("ssh-ki-prompt-0"));
    rig.click("ssh-ki-cancel");
    rig.wait_failed();
}

#[gpui::test]
fn ssh_tunnel_a_tunnel_connection_whose_settings_cannot_be_read_fails_instead_of_connecting_directly(cx: &mut TestAppContext) {
    // The database is reachable directly (the stub): a fail-open connect would simply work.
    let direct = MiniRedis::start();
    let mut rig = TunnelRig::new(cx, &format!("redis://127.0.0.1:{}/0", direct.addr.port()), bastion_settings(SshAuthMethod::Agent));
    // The record says "tunnel", the settings cannot be found in storage.
    rig.flag_as_tunnelled_without_settings();
    rig.connect();
    let message = rig.wait_failed();
    assert!(message.starts_with("cannot read this connection's tunnel settings"), "{message}");
    assert_eq!(direct.connections(), 0, "the database was never dialled directly");
    assert_eq!(rig.redis.connections(), 0);
    assert_eq!(rig.opener.opened_count(), 0);

    // Settings that now say "off" for a connection saved as tunnelled disagree: fail too.
    let key = rig.key.clone();
    rig.app.update(rig.visual, |this, _| {
        this.tunnel_ui.settings_override.insert(key, SshSettings { enabled: false, ..bastion_settings(SshAuthMethod::Agent) });
    });
    let (key, uri) = (rig.key.clone(), rig.uri());
    rig.app.update(rig.visual, |this, cx| this.ensure_connected_with_uri(key, Some(uri.into()), None, cx));
    let message = rig.wait_failed();
    assert!(message.contains("saved as tunnelled"), "{message}");
    assert_eq!(direct.connections(), 0);

    // A connection that never had a tunnel and has no record still connects, directly.
    let key = rig.key.clone();
    rig.app.update(rig.visual, |this, _| {
        this.tunnel_ui.settings_override.remove(&key);
        this.saved_connections.iter_mut().find(|s| s.matches_id(&key)).unwrap().ssh_tunnel = false;
    });
    let (key, uri) = (rig.key.clone(), rig.uri());
    rig.app.update(rig.visual, |this, cx| this.ensure_connected_with_uri(key, Some(uri.into()), None, cx));
    rig.wait_connected();
    assert!(direct.connections() >= 1);
}

#[gpui::test]
fn ssh_tunnel_the_apply_session_of_a_tunnel_connection_never_opens_directly(cx: &mut TestAppContext) {
    let mut rig = TunnelRig::new(cx, "redis://cache.example.com:6379/0", bastion_settings(SshAuthMethod::Agent));
    rig.connect();
    rig.wait_connected();
    let key = rig.key.clone();
    // Connected through a tunnel: the apply session gets its own access.
    rig.app.update(rig.visual, |this, _| {
        this.saved_connections.iter_mut().find(|s| s.matches_id(&key)).unwrap().ssh_tunnel = true;
        assert!(this.secondary_tunnel_access(&key).is_some());
    });
    // The tunnel state is gone while the record still says "tunnel": refuse, do not go direct.
    rig.app.update(rig.visual, |this, _| {
        this.tunnel_ui.live.remove(&key);
        let saved = this.saved_connections.iter().find(|s| s.matches_id(&key)).unwrap().clone();
        let refused = this.structure_executor(&saved, &key).err().expect("no direct apply session");
        assert!(refused.contains("Refusing to open a direct session"), "{refused}");
    });
}

#[gpui::test]
fn ssh_tunnel_enter_in_the_trust_dialog_does_not_trust_and_does_not_strand_the_attempt(cx: &mut TestAppContext) {
    let mut rig = TunnelRig::new(cx, "redis://cache.example.com:6379/0", bastion_settings(SshAuthMethod::Agent));
    rig.opener.then(Err(unknown_host(BASTION)));
    rig.connect();
    rig.wait_dialog("ssh-trust-accept");
    rig.visual.simulate_keystrokes("enter");
    settle(rig.visual);
    let message = rig.wait_failed();
    assert!(message.contains("not trusted"), "{message}");
    assert!(rig.opener.trusted.lock().unwrap().is_empty(), "Enter never trusts a key");
    assert!(!rig.rendered("ssh-trust-accept"), "the dialog closed");
    assert_eq!(rig.opener.opened_count(), 1);

    // Escape is Cancel too.
    rig.opener.then(Err(unknown_host(BASTION)));
    rig.connect();
    rig.wait_dialog("ssh-trust-accept");
    rig.visual.simulate_keystrokes("escape");
    settle(rig.visual);
    let message = rig.wait_failed();
    assert!(message.contains("not trusted"), "{message}");
    assert!(rig.opener.trusted.lock().unwrap().is_empty());
}

#[gpui::test]
fn ssh_tunnel_enter_submits_the_ssh_secret_prompt(cx: &mut TestAppContext) {
    let mut rig = TunnelRig::new(cx, "redis://cache.example.com:6379/0", bastion_settings(SshAuthMethod::Password));
    rig.connect();
    rig.wait_dialog("ssh-secret-input");
    click_debug_button(rig.visual, "ssh-secret-input");
    rig.visual.simulate_input("pw-enter-needle");
    rig.visual.simulate_keystrokes("enter");
    rig.wait_connected();
    let opened = rig.opener.opened.lock().unwrap();
    assert!(matches!(&opened[0].auth, engine_net::Auth::Password(secret) if secret.expose() == "pw-enter-needle"));
    drop(opened);

    // The same for a one-time code.
    let key = rig.key.clone();
    rig.app.update(rig.visual, |this, cx| this.disconnect_connection(&key, cx));
    settle(rig.visual);
    rig.app.update(rig.visual, |this, _| {
        this.tunnel_ui.settings_override.insert(key.clone(), bastion_settings(SshAuthMethod::KeyboardInteractive));
    });
    rig.opener.ask.lock().unwrap().push(("Code:".to_string(), false));
    rig.connect();
    rig.wait_dialog("ssh-ki-input-0");
    click_debug_button(rig.visual, "ssh-ki-input-0");
    rig.visual.simulate_input("424242");
    rig.visual.simulate_keystrokes("enter");
    rig.wait_connected();
    assert_eq!(rig.opener.answers.lock().unwrap().last().cloned(), Some(Some(vec!["424242".to_string()])));
}

#[gpui::test]
fn ssh_tunnel_enter_submits_the_keyboard_interactive_dialog(cx: &mut TestAppContext) {
    let mut rig = TunnelRig::new(cx, "redis://cache.example.com:6379/0", bastion_settings(SshAuthMethod::KeyboardInteractive));
    rig.opener.ask.lock().unwrap().extend([("Code:".to_string(), false), ("Device:".to_string(), true)]);
    rig.connect();
    rig.wait_dialog("ssh-ki-input-1");
    click_debug_button(rig.visual, "ssh-ki-input-0");
    rig.visual.simulate_input("424242");
    click_debug_button(rig.visual, "ssh-ki-input-1");
    rig.visual.simulate_input("laptop");
    rig.visual.simulate_keystrokes("enter");
    rig.wait_connected();
    assert_eq!(
        rig.opener.answers.lock().unwrap().clone(),
        vec![Some(vec!["424242".to_string(), "laptop".to_string()])]
    );
    assert!(!rig.rendered("ssh-ki-ok"), "the dialog closed");
}

#[gpui::test]
fn ssh_tunnel_quit_waits_a_bounded_time_for_the_tunnels_to_close(cx: &mut TestAppContext) {
    let mut fast = TunnelRig::new(cx, "redis://cache.example.com:6379/0", bastion_settings(SshAuthMethod::Agent));
    fast.connect();
    fast.wait_connected();
    let drops = fast.opener.drops.clone();
    fast.visual.cx.update(|cx| cx.shutdown());
    assert_eq!(drops.load(std::sync::atomic::Ordering::SeqCst), 1, "a quick tunnel is closed before the quit finishes");
}

#[gpui::test]
fn ssh_tunnel_quit_never_waits_longer_than_the_bound_for_a_slow_tunnel(cx: &mut TestAppContext) {
    let mut slow = TunnelRig::new(cx, "redis://cache.example.com:6379/0", bastion_settings(SshAuthMethod::Agent));
    slow.opener.drop_delay_ms.store(3000, std::sync::atomic::Ordering::SeqCst);
    slow.connect();
    slow.wait_connected();
    let started = std::time::Instant::now();
    slow.visual.cx.update(|cx| cx.shutdown());
    assert!(started.elapsed() < std::time::Duration::from_millis(1800), "quit waited {:?}", started.elapsed());
}

#[gpui::test]
fn ssh_tunnel_saving_a_connection_with_unreadable_network_settings_repairs_them(cx: &mut TestAppContext) {
    // Saving rewrites settings that cannot be parsed, whatever they were.
    for broken in ["{", r#"{"ssh":5}"#, r#"{"ssh":{"isEnabled":"yes","host":"bastion.example.com"}}"#] {
        assert!(SshSettings::parse(broken).is_err(), "{broken}");
        let mut original = crate::connections::tests_record_for("22222222-2222-2222-2222-222222222222");
        original.network_settings_json = broken.into();
        let mut form = ConnectionFormState::from_saved(&SavedConnection::from_record(&original));
        form.name = "repaired".into();
        form.ssh = Some(SshSettings::default());
        let saved = form.to_record(2.0, Some(&original)).expect("the form saves");
        assert!(SshSettings::parse(&saved.network_settings_json).is_ok(), "{}", saved.network_settings_json);
        assert!(!SshSettings::parse(&saved.network_settings_json).unwrap().enabled);
    }
    // The record is flagged as tunnelled until it is repaired (so a connect fails, never goes direct).
    let mut original = crate::connections::tests_record_for("22222222-2222-2222-2222-222222222222");
    original.network_settings_json = "{".into();
    assert!(SavedConnection::from_record(&original).ssh_tunnel);

    // And the dialog says it: the tab shows the reset note.
    cx.dispatcher.allow_parking();
    let (app, visual) = dialog_app(cx);
    let pg = crate::ui_fixture::POSTGRES_ID;
    let inputs = open_fixture_dialog(&app, visual, pg);
    click(visual, "connection-config-tab-tunnel".into());
    assert!(!rendered(visual, "connection-config-ssh-unreadable".into()));
    inputs.ssh.note_unreadable_settings();
    visual.update(|_, cx| cx.refresh_windows());
    settle(visual);
    assert!(rendered(visual, "connection-config-ssh-unreadable".into()));
}

#[gpui::test]
fn ssh_tunnel_a_stale_trust_dialog_trusts_only_the_key_it_shows(cx: &mut TestAppContext) {
    use crate::ssh_tunnel::fake::unknown_host_other_key;
    let mut rig = TunnelRig::new(cx, "redis://cache.example.com:6379/0", bastion_settings(SshAuthMethod::Agent));
    let shown_key = match unknown_host(BASTION) {
        engine_net::TunnelError::HostKeyUnknown { public_key, .. } => public_key,
        _ => unreachable!(),
    };
    let other_key = match unknown_host_other_key(BASTION) {
        engine_net::TunnelError::HostKeyUnknown { public_key, .. } => public_key,
        _ => unreachable!(),
    };
    rig.opener.then(Err(unknown_host(BASTION))).then(Err(unknown_host_other_key(BASTION)));
    rig.connect();
    rig.wait_dialog("ssh-trust-accept");
    // The attempt is abandoned while the dialog is up (the user disconnects).
    let key = rig.key.clone();
    rig.app.update(rig.visual, |this, cx| this.disconnect_connection(&key, cx));
    settle(rig.visual);
    rig.click("ssh-trust-accept");
    settle(rig.visual);
    let trusted = rig.opener.trusted.lock().unwrap().clone();
    assert_eq!(trusted.iter().map(|t| t.2.clone()).collect::<Vec<_>>(), std::slice::from_ref(&shown_key), "the key the dialog showed, nothing else");
    assert_eq!(rig.opener.opened_count(), 1, "a stale answer does not connect");

    // The next attempt meets a different key: it asks again and only trusts what it shows.
    rig.connect();
    rig.wait_dialog("ssh-trust-accept");
    assert_eq!(rig.opener.trusted.lock().unwrap().len(), 1, "the earlier trust did not cover the new key");
    rig.click("ssh-trust-accept");
    rig.wait_status("the connect", |s| !matches!(s, ConnectionStatus::Connecting));
    let trusted: Vec<String> = rig.opener.trusted.lock().unwrap().iter().map(|t| t.2.clone()).collect();
    assert_eq!(trusted, [shown_key, other_key]);
}

#[gpui::test]
fn ssh_tunnel_without_an_app_data_dir_a_tunnel_fails_with_a_typed_error(cx: &mut TestAppContext) {
    let mut rig = TunnelRig::new(cx, "redis://cache.example.com:6379/0", bastion_settings(SshAuthMethod::Agent));
    rig.app.update(rig.visual, |this, _| this.tunnel_ui.known_hosts = None);
    rig.connect();
    let message = rig.wait_failed();
    assert!(message.contains("app data directory"), "{message}");
    assert!(rig.log_codes().contains(&Some("tunnel.config".to_string())), "{:?}", rig.log_codes());
    assert_eq!(rig.opener.opened_count(), 0);
    assert_eq!(rig.redis.connections(), 0);
}

#[gpui::test]
fn ssh_tunnel_closing_a_tunnel_never_blocks_the_ui_thread(cx: &mut TestAppContext) {
    let mut rig = TunnelRig::new(cx, "redis://cache.example.com:6379/0", bastion_settings(SshAuthMethod::Agent));
    rig.opener.drop_delay_ms.store(800, std::sync::atomic::Ordering::SeqCst);
    rig.connect();
    rig.wait_connected();
    let key = rig.key.clone();
    let started = std::time::Instant::now();
    rig.app.update(rig.visual, |this, cx| this.disconnect_connection(&key, cx));
    assert!(started.elapsed() < std::time::Duration::from_millis(300), "disconnect waited {:?} for the tunnel", started.elapsed());
    let drops = rig.opener.drops.clone();
    wait_until(rig.visual, "the slow tunnel to close in the background", |_| drops.load(std::sync::atomic::Ordering::SeqCst) == 1);
}

#[gpui::test]
fn ssh_tunnel_disconnect_delete_and_quit_clear_typed_ssh_secrets(cx: &mut TestAppContext) {
    let mut rig = TunnelRig::new(cx, "redis://cache.example.com:6379/0", bastion_settings(SshAuthMethod::Password));
    let secrets_held = |rig: &TunnelRig| rig.app.read_with(rig.visual, |this, _| (this.tunnel_ui.secrets.len(), this.tunnel_ui.live.len()));
    // Disconnect.
    rig.seed_secret("pw-1");
    rig.connect();
    rig.wait_connected();
    assert_eq!(secrets_held(&rig), (1, 1));
    let key = rig.key.clone();
    rig.app.update(rig.visual, |this, cx| this.disconnect_connection(&key, cx));
    assert_eq!(secrets_held(&rig), (0, 0));
    // Delete: the saved connection goes, the connection is disconnected with it.
    rig.seed_secret("pw-2");
    rig.connect();
    rig.wait_connected();
    assert_eq!(secrets_held(&rig), (1, 1));
    rig.app.update(rig.visual, |this, cx| {
        let others: Vec<_> = this.saved_connections.iter().filter(|s| !s.matches_id(&key)).cloned().collect();
        this.apply_saved_connections(others, cx);
    });
    assert_eq!(secrets_held(&rig), (0, 0));
    // Quit.
    let key = rig.key.clone();
    rig.app.update(rig.visual, |this, _| {
        this.tunnel_ui.secrets.insert(key, crate::ssh_tunnel::SshSecrets::from_text("pw-3", ""));
    });
    assert_eq!(secrets_held(&rig).0, 1);
    rig.visual.cx.update(|cx| cx.shutdown());
    assert_eq!(rig.app.read_with(rig.visual, |this, _| this.tunnel_ui.secrets.len()), 0, "quit wiped the typed secrets");
}

#[gpui::test]
fn ssh_tunnel_remote_override_is_shown_and_survives_export_import_off(cx: &mut TestAppContext) {
    // Shown: in the status line of the live tunnel ...
    let mut settings = bastion_settings(SshAuthMethod::Agent);
    settings.remote_host = "10.0.0.9".into();
    let mut rig = TunnelRig::new(cx, "redis://cache.example.com:6379/0", settings.clone());
    rig.connect();
    rig.wait_connected();
    let via = rig.app.read_with(rig.visual, |this, _| this.via_ssh(&rig.key)).unwrap();
    assert!(via.contains("forwards to 10.0.0.9:6379"), "{via}");
    // ... and in the Tunnel tab of the dialog.
    let key = rig.key.clone();
    let inputs = rig.app.update_in(rig.visual, |this, window, cx| {
        let saved = this.saved_connections.iter().find(|s| s.matches_id(&key)).cloned();
        this.open_connection_dialog(saved, window, cx);
        this.connection_ui.last_inputs.clone().unwrap()
    });
    settle(rig.visual);
    assert_eq!(rig.visual.update(|_, cx| inputs.ssh.read(cx)).remote_host, "10.0.0.9");
    // Survives export and import, with the tunnel off.
    let mut record = engine_storage::connection::SavedConnectionRecord {
        id: "11111111-1111-1111-1111-111111111111".into(),
        name: "remote".into(),
        uri: "redis://cache.example.com:6379/0".into(),
        db_type: crate::connections::DB_TYPE_REDIS.into(),
        color_hex: "#34C759".into(),
        save_password: false,
        save_proxy_password: false,
        network_settings_json: String::new(),
        is_pinned: false,
        last_connected: None,
        created_at: 1.0,
        updated_at: 1.0,
        connection_type: String::new(),
        overrides_json: String::new(),
    };
    record.network_settings_json = settings.write_into("{}");
    let staged = crate::connections::stage_connection_configs(&crate::connections::export_connection_configs(&[record]).unwrap(), 2.0).unwrap();
    let imported = SshSettings::from_network_json(&staged[0].network_settings_json);
    assert_eq!(imported.remote_host, "10.0.0.9");
    assert!(!imported.enabled);
}

#[gpui::test]
fn ssh_tunnel_editing_the_bastion_host_with_a_saved_password_asks_before_sending_it_to_the_new_host(cx: &mut TestAppContext) {
    cx.dispatcher.allow_parking();
    let (app, visual) = dialog_app(cx);
    let pg = crate::ui_fixture::POSTGRES_ID;
    app.update(visual, |this, _| {
        this.tunnel_ui.settings_override.insert(pg.to_string(), SshSettings { save_secrets: true, ..bastion_settings(SshAuthMethod::Password) });
    });
    let inputs = open_fixture_dialog(&app, visual, pg);
    click(visual, "connection-config-tab-tunnel".into());
    assert!(!rendered(visual, "connection-config-ssh-secret-reset".into()), "nothing changed yet");
    fill_row(visual, "connection-config-ssh-host", "evil.example.net");
    assert!(
        rendered(visual, "connection-config-ssh-secret-reset".into()),
        "the dialog says the saved password goes with the old bastion"
    );
    fill_row(visual, "connection-config-ssh-host", BASTION);
    assert!(!rendered(visual, "connection-config-ssh-secret-reset".into()), "back to the saved bastion: no note");
    fill_row(visual, "connection-config-ssh-user", "root");
    assert!(rendered(visual, "connection-config-ssh-secret-reset".into()), "a new user counts too");
    assert!(visual.update(|_, cx| inputs.ssh.bastion_changed(cx)));

    // What Save does to the credential store: the old secret is gone, the typed one is kept.
    let store = crate::credentials::fake::MemoryStore::default();
    use crate::credentials::CredentialStore as _;
    store.write(crate::credentials::SecretKind::SshPassword, "ID", "pw-for-the-old-host").unwrap();
    let old = SshSettings { save_secrets: true, ..bastion_settings(SshAuthMethod::Password) };
    let new = SshSettings { host: "evil.example.net".into(), ..old.clone() };
    crate::ssh_tunnel::persist_secrets(&store, "ID", Some(&old), &new, &crate::ssh_tunnel::SshSecretInput::default()).unwrap();
    assert_eq!(store.load(crate::credentials::SecretKind::SshPassword, "ID").unwrap(), None);
    let typed = crate::ssh_tunnel::SshSecretInput::new(engine_net::Secret::new("pw-for-the-new-host"), engine_net::Secret::default());
    crate::ssh_tunnel::persist_secrets(&store, "ID", Some(&old), &new, &typed).unwrap();
    assert_eq!(store.load(crate::credentials::SecretKind::SshPassword, "ID").unwrap().as_deref(), Some("pw-for-the-new-host"));
    // The same bastion keeps its saved secret when the field is left empty.
    crate::ssh_tunnel::persist_secrets(&store, "ID", Some(&new), &new, &crate::ssh_tunnel::SshSecretInput::default()).unwrap();
    assert!(store.load(crate::credentials::SecretKind::SshPassword, "ID").unwrap().is_some());
}

#[gpui::test]
fn ssh_tunnel_the_ssh_config_picker_loads_in_the_background_and_shows_a_load_error(cx: &mut TestAppContext) {
    cx.dispatcher.allow_parking();
    let (app, visual) = dialog_app(cx);
    let pg = crate::ui_fixture::POSTGRES_ID;
    let inputs = open_fixture_dialog(&app, visual, pg);
    click(visual, "connection-config-tab-tunnel".into());
    // A failing read is shown in the picker, not swallowed.
    visual.update(|window, cx| {
        inputs.ssh.load_config(window, cx, || {
            Err(engine_net::ConfigError::Io { path: "/home/user/.ssh/config".into(), reason: "permission denied".into() })
        })
    });
    let note = inputs.ssh.config_note.clone();
    wait_until(visual, "the config read", |_| matches!(&*note.borrow(), crate::connection_dialog::ConfigNote::LoadFailed(_)));
    settle(visual);
    assert!(rendered(visual, "connection-config-ssh-config-failed".into()));
    assert!(matches!(&*note.borrow(), crate::connection_dialog::ConfigNote::LoadFailed(text) if text.contains("permission denied")));
    // A good read fills the picker; picking a host fills the fields.
    visual.update(|window, cx| {
        inputs.ssh.load_config(window, cx, || {
            engine_net::SshConfig::parse("Host bastion\n  HostName bastion.example.com\n  User deploy\n")
        })
    });
    wait_until(visual, "the config read", |_| true);
    std::thread::sleep(std::time::Duration::from_millis(100));
    settle(visual);
    click(visual, "ssh-enable-switch".into());
    click(visual, "connection-config-ssh-config-select".into());
    visual.simulate_keystrokes("down enter");
    settle(visual);
    assert_eq!(visual.update(|_, cx| inputs.ssh.read(cx)).host, "bastion.example.com");
}

#[gpui::test]
fn ssh_tunnel_the_import_selection_shows_each_connections_tunnel(cx: &mut TestAppContext) {
    let (app, visual, _store) = import_test_app(cx);
    import_pick(visual, "import-menu-navicat", vec![import_fixture("navicat.ncx")]);
    import_selection_dialog(visual);
    assert!(rendered(visual, "import-selection-tunnels".into()), "the dialog says once that some connections use a tunnel");
    let (labels, names, count) = app.read_with(visual, |this, cx| {
        let selection = this.import_selection.as_ref().unwrap().read(cx);
        (selection.tunnel_labels(), selection.names().into_iter().map(|n| n.to_string()).collect::<Vec<_>>(), selection.tunnel_count())
    });
    assert_eq!(count, 1);
    let prod = names.iter().position(|name| name == "Prod PG").expect("Prod PG");
    assert!(rendered(visual, format!("import-row-tunnel-{prod}")), "the bastion sits on the row itself");
    for (label, name) in labels.iter().zip(&names) {
        match name.as_str() {
            "Prod PG" => assert_eq!(label.as_deref(), Some("via SSH deploy@bastion.example.com:2222")),
            _ => assert_eq!(label, &None, "{name}"),
        }
    }
}

// ---- jump hosts and the proxy ----------------------------------------------------------------

/// The tab's body scrolls: wheel it until the middle of `selector` is inside the visible body.
fn reach(visual: &mut gpui::VisualTestContext, selector: &'static str) {
    let mut last = None;
    for _ in 0..40 {
        settle(visual);
        // The viewport is the fixed-height config panel below its tab bar.
        let panel = visual.debug_bounds("connection-config").expect("the config panel");
        let bar = visual.debug_bounds("connection-config-tab-bar").expect("the tab bar");
        let body = gpui::Bounds::from_corners(gpui::point(panel.left(), bar.bottom()), panel.bottom_right());
        let target = visual.debug_bounds(selector).unwrap_or_else(|| panic!("{selector} is rendered"));
        let middle = target.center().y;
        let delta = if middle > body.bottom() - gpui::px(6.0) {
            -80.0
        } else if middle < body.top() + gpui::px(6.0) {
            80.0
        } else {
            return;
        };
        if last == Some((body.origin, target.origin)) {
            panic!("{selector} is out of reach: {body:?} {target:?}");
        }
        last = Some((body.origin, target.origin));
        visual.simulate_event(gpui::ScrollEvent {
            position: body.center(),
            delta: gpui::ScrollDelta::Pixels(gpui::point(gpui::px(0.0), gpui::px(delta))),
            touch_phase: gpui::TouchPhase::Moved,
            modifiers: Default::default(),
            source: gpui::ScrollSource::Pointer,
        });
        visual.run_until_parked();
    }
    panic!("{selector} could not be scrolled into view");
}

fn click_in_tab(visual: &mut gpui::VisualTestContext, selector: &'static str) {
    reach(visual, selector);
    click(visual, selector.into());
}

fn fill_in_tab(visual: &mut gpui::VisualTestContext, selector: &'static str, text: &str) {
    reach(visual, selector);
    fill_row(visual, selector, text);
}

fn pick(visual: &mut gpui::VisualTestContext, select: &'static str, keys: &str) {
    reach(visual, select);
    click(visual, select.into());
    visual.simulate_keystrokes(keys);
    settle(visual);
}

fn jump(host: &str, user: &str, auth: SshAuthMethod) -> crate::ssh_tunnel::JumpSettings {
    crate::ssh_tunnel::JumpSettings {
        host: host.into(),
        user: user.into(),
        auth,
        key_path: if auth == SshAuthMethod::KeyFile { "/keys/jump".into() } else { String::new() },
        ..Default::default()
    }
}

fn proxy_settings(kind: crate::ssh_tunnel::ProxyType) -> crate::ssh_tunnel::ProxySettings {
    crate::ssh_tunnel::ProxySettings {
        enabled: true,
        kind,
        host: "proxy.example.com".into(),
        port: kind.default_port().into(),
        user: String::new(),
        save_secrets: false,
    }
}

#[gpui::test]
fn ssh_tunnel_jump_rows_are_added_moved_removed_and_saved_in_order_without_secrets(cx: &mut TestAppContext) {
    cx.dispatcher.allow_parking();
    let (app, visual) = dialog_app(cx);
    visual.simulate_resize(gpui::size(gpui::px(1300.0), gpui::px(2400.0)));
    settle(visual);
    let pg = crate::ui_fixture::POSTGRES_ID;
    let inputs = open_fixture_dialog(&app, visual, pg);
    click(visual, "connection-config-tab-tunnel".into());
    assert_eq!(inputs.ssh.jump_count(), 0, "no jump hosts by default");
    assert!(rendered(visual, "ssh-jump-add".into()) && !rendered(visual, "ssh-jump-0-row".into()));

    click(visual, "ssh-enable-switch".into());
    fill_row(visual, "connection-config-ssh-host", BASTION);
    fill_row(visual, "connection-config-ssh-user", "deploy");
    fill_row(visual, "connection-config-ssh-password", "bastion-password-needle");

    click_in_tab(visual, "ssh-jump-add");
    click_in_tab(visual, "ssh-jump-add");
    assert_eq!(inputs.ssh.jump_count(), 2);
    assert!(rendered(visual, "ssh-jump-0-row".into()) && rendered(visual, "ssh-jump-1-row".into()));
    fill_in_tab(visual, "ssh-jump-0-host", "jump-a.example.com");
    fill_in_tab(visual, "ssh-jump-0-port", "2201");
    fill_in_tab(visual, "ssh-jump-0-user", "alice");
    assert!(rendered(visual, "ssh-jump-0-secret".into()), "a password row has its secret input");
    fill_in_tab(visual, "ssh-jump-0-secret", "jump-a-password-needle");
    fill_in_tab(visual, "ssh-jump-1-host", "jump-b.example.com");
    fill_in_tab(visual, "ssh-jump-1-user", "bob");

    // The method Select of a row: open, pick "Private key", the row gets its key fields.
    assert!(!rendered(visual, "ssh-jump-1-key".into()));
    pick(visual, "ssh-jump-1-auth", "down enter");
    let read = visual.update(|_, cx| inputs.ssh.read(cx));
    assert_eq!(read.jumps[1].auth, SshAuthMethod::KeyFile, "the row's select updated the tab");
    assert_eq!(read.jumps[0].auth, SshAuthMethod::Password, "and only that row");
    assert!(rendered(visual, "ssh-jump-1-key".into()));
    fill_in_tab(visual, "ssh-jump-1-key", "/keys/jump-b");
    fill_in_tab(visual, "ssh-jump-1-secret", "jump-b-passphrase-needle");

    // Move: down, then back up; the first row can't go up and the last can't go down.
    click_in_tab(visual, "ssh-jump-0-down");
    let hosts = |visual: &mut gpui::VisualTestContext| {
        visual.update(|_, cx| inputs.ssh.read(cx)).jumps.iter().map(|j| j.host.clone()).collect::<Vec<_>>()
    };
    assert_eq!(hosts(visual), ["jump-b.example.com", "jump-a.example.com"]);
    let typed = visual.update(|_, cx| inputs.ssh.read_secrets(cx));
    assert_eq!(
        typed.jumps.iter().map(|secret| secret.expose().to_owned()).collect::<Vec<_>>(),
        ["jump-b-passphrase-needle", "jump-a-password-needle"],
        "a row takes its typed secret along"
    );
    click_in_tab(visual, "ssh-jump-1-up");
    assert_eq!(hosts(visual), ["jump-a.example.com", "jump-b.example.com"]);
    click_in_tab(visual, "ssh-jump-0-up");
    click_in_tab(visual, "ssh-jump-1-down");
    assert_eq!(hosts(visual), ["jump-a.example.com", "jump-b.example.com"], "the end rows stay put");

    // A third row, then remove the middle one.
    click_in_tab(visual, "ssh-jump-add");
    fill_in_tab(visual, "ssh-jump-2-host", "jump-c.example.com");
    fill_in_tab(visual, "ssh-jump-2-user", "carol");
    click_in_tab(visual, "ssh-jump-1-remove");
    assert_eq!(hosts(visual), ["jump-a.example.com", "jump-c.example.com"]);
    assert!(!rendered(visual, "ssh-jump-2-row".into()));

    // Save through the real button: the record has the rows in order and no secret.
    click(visual, "connection-save".into());
    let json = ssh_record(&app, visual);
    let jumps = json["ssh"]["jumps"].as_array().expect("the jump list");
    assert_eq!(jumps.len(), 2);
    assert_eq!((jumps[0]["host"].as_str(), jumps[0]["port"].as_str(), jumps[0]["user"].as_str(), jumps[0]["authMethod"].as_str()), (Some("jump-a.example.com"), Some("2201"), Some("alice"), Some("password")));
    assert_eq!((jumps[1]["host"].as_str(), jumps[1]["user"].as_str()), (Some("jump-c.example.com"), Some("carol")));
    let text = json.to_string();
    for secret in ["bastion-password-needle", "jump-a-password-needle", "jump-b-passphrase-needle"] {
        assert!(!text.contains(secret), "{secret} reached the record: {text}");
    }
    let left = visual.update(|_, cx| inputs.ssh.read_secrets(cx));
    assert!(left.is_empty(), "closing the dialog emptied every secret input, the rows' too");

    // Reopening shows the rows in order.
    let again = open_fixture_dialog(&app, visual, pg);
    click(visual, "connection-config-tab-tunnel".into());
    let reopened = visual.update(|_, cx| again.ssh.read(cx));
    assert_eq!(reopened.jumps.len(), 2);
    assert_eq!(
        (reopened.jumps[0].host.as_str(), reopened.jumps[0].port.as_str(), reopened.jumps[1].host.as_str()),
        ("jump-a.example.com", "2201", "jump-c.example.com")
    );
    assert!(rendered(visual, "ssh-jump-1-row".into()));
    // Validation names the row.
    let mut bad = reopened.clone();
    bad.jumps[1].user.clear();
    assert!(bad.validate().unwrap_err().to_string().starts_with("Jump host 2"));
}

#[gpui::test]
fn ssh_tunnel_the_proxy_section_enables_picks_its_type_in_the_select_and_reopens_with_it(cx: &mut TestAppContext) {
    use crate::ssh_tunnel::ProxyType;
    cx.dispatcher.allow_parking();
    let (app, visual) = dialog_app(cx);
    visual.simulate_resize(gpui::size(gpui::px(1300.0), gpui::px(2400.0)));
    settle(visual);
    let pg = crate::ui_fixture::POSTGRES_ID;
    let inputs = open_fixture_dialog(&app, visual, pg);
    click(visual, "connection-config-tab-tunnel".into());
    assert!(!inputs.ssh.proxy_enabled.get());
    assert!(!rendered(visual, "connection-config-proxy-host".into()), "the fields appear with the switch");
    click_in_tab(visual, "proxy-enable-switch");
    assert!(inputs.ssh.proxy_enabled.get(), "the switch turned the proxy on");
    for selector in ["connection-config-proxy-kind", "connection-config-proxy-host", "connection-config-proxy-user", "connection-config-proxy-password", "connection-config-proxy-save"] {
        assert!(rendered(visual, selector.into()), "{selector}");
    }
    assert!(!rendered(visual, "connection-config-proxy-http-note".into()), "SOCKS5 is the default");

    // The type Select: open, pick the other value, the model follows, the port follows its default.
    assert_eq!(inputs.ssh.proxy_kind.get(), ProxyType::Socks5);
    pick(visual, "connection-config-proxy-kind-select", "down enter");
    assert_eq!(inputs.ssh.proxy_kind.get(), ProxyType::Http, "the select updated the tab");
    assert!(rendered(visual, "connection-config-proxy-http-note".into()), "Basic credentials travel unencrypted: said so");
    assert_eq!(visual.update(|_, cx| inputs.ssh.read(cx)).proxy.port, "8080", "an untouched default port follows the protocol");
    fill_in_tab(visual, "connection-config-proxy-host", "proxy.example.com");
    fill_in_tab(visual, "connection-config-proxy-user", "svc");
    fill_in_tab(visual, "connection-config-proxy-password", "proxy-password-needle");

    // Back to SOCKS5 and again to HTTP, so the reopened dialog proves the last choice.
    pick(visual, "connection-config-proxy-kind-select", "up enter");
    assert_eq!(inputs.ssh.proxy_kind.get(), ProxyType::Socks5);
    pick(visual, "connection-config-proxy-kind-select", "down enter");
    assert_eq!(inputs.ssh.proxy_kind.get(), ProxyType::Http);

    // The proxy alone is a route: saving without the SSH tunnel keeps it, and no secret.
    click(visual, "connection-save".into());
    let json = ssh_record(&app, visual);
    assert_eq!(json["ssh"]["isEnabled"], false);
    assert_eq!(json["proxy"]["isEnabled"], true);
    assert_eq!(json["proxy"]["kind"], "http");
    assert_eq!((json["proxy"]["host"].as_str(), json["proxy"]["user"].as_str()), (Some("proxy.example.com"), Some("svc")));
    assert!(!json.to_string().contains("proxy-password-needle"));
    let left = visual.update(|_, cx| inputs.ssh.read_secrets(cx));
    assert!(left.is_empty());

    let again = open_fixture_dialog(&app, visual, pg);
    click(visual, "connection-config-tab-tunnel".into());
    let reopened = visual.update(|_, cx| again.ssh.read(cx));
    assert!(reopened.proxy.enabled && !reopened.enabled);
    assert_eq!((reopened.proxy.kind, reopened.proxy.host.as_str(), reopened.proxy.port.as_str()), (ProxyType::Http, "proxy.example.com", "8080"));
    assert!(rendered(visual, "connection-config-proxy-http-note".into()), "reopened with HTTP selected");
    // The reopened Select shows the saved choice: one step up is SOCKS5.
    pick(visual, "connection-config-proxy-kind-select", "up enter");
    assert_eq!(again.ssh.proxy_kind.get(), ProxyType::Socks5, "the reopened select started on HTTP CONNECT");
}

#[gpui::test]
fn ssh_tunnel_the_test_button_is_on_for_a_proxy_alone(cx: &mut TestAppContext) {
    use crate::connection_dialog::TestStatus;
    cx.dispatcher.allow_parking();
    let (app, visual) = dialog_app(cx);
    visual.simulate_resize(gpui::size(gpui::px(1300.0), gpui::px(2400.0)));
    settle(visual);
    let opener = FakeOpener::new();
    let opener_for_app = opener.clone();
    app.update(visual, |this, _| this.tunnel_ui.opener = opener_for_app);
    let pg = crate::ui_fixture::POSTGRES_ID;
    let inputs = open_fixture_dialog(&app, visual, pg);
    click(visual, "connection-config-tab-tunnel".into());
    click_in_tab(visual, "proxy-enable-switch");
    fill_in_tab(visual, "connection-config-proxy-host", "proxy.example.com");
    let status = inputs.ssh.test_status.clone();
    click_in_tab(visual, "ssh-test-tunnel");
    let status_for_wait = status.clone();
    wait_until(visual, "the test", move |_| !matches!(&*status_for_wait.borrow(), TestStatus::Running | TestStatus::Idle));
    assert!(matches!(&*status.borrow(), TestStatus::Success { .. }), "{:?}", status.borrow());
    assert!(opener.opened.lock().unwrap().is_empty(), "no SSH");
    assert_eq!(opener.opened_proxy.lock().unwrap().len(), 1, "the proxy route was opened and probed");
}

#[test]
fn ssh_tunnel_the_status_names_the_whole_route() {
    use crate::ssh_tunnel::{ProxyType, SshSettings};
    let mut s = bastion_settings(SshAuthMethod::Agent);
    s.port = "22".into();
    let label = |s: &SshSettings, via: &str| tunnel_ui::route_label(s, via, None, false);
    assert_eq!(label(&s, "deploy@bastion.example.com:22"), "via SSH deploy@bastion.example.com");
    s.jumps = vec![jump("j1.example.com", "a", SshAuthMethod::Agent)];
    assert_eq!(label(&s, "deploy@bastion.example.com:22"), "via SSH deploy@bastion.example.com (1 jump)");
    s.jumps.push(jump("j2.example.com", "b", SshAuthMethod::Agent));
    assert_eq!(label(&s, "deploy@bastion.example.com:22"), "via SSH deploy@bastion.example.com (2 jumps)");
    s.jumps.clear();
    s.proxy = proxy_settings(ProxyType::Socks5);
    s.proxy.port = "1080".into();
    assert_eq!(
        label(&s, "deploy@bastion.example.com:22"),
        "via SSH deploy@bastion.example.com through SOCKS5 proxy proxy.example.com:1080"
    );
    // A proxy alone.
    s.enabled = false;
    assert_eq!(label(&s, "ignored"), "via SOCKS5 proxy proxy.example.com:1080");
    s.proxy.kind = ProxyType::Http;
    s.proxy.port = "3128".into();
    assert_eq!(label(&s, "ignored"), "via HTTP proxy proxy.example.com:3128");
}

fn chain_settings() -> SshSettings {
    SshSettings {
        jumps: vec![jump("jump1.example.com", "hop1", SshAuthMethod::Password), jump("jump2.example.com", "hop2", SshAuthMethod::Agent)],
        ..bastion_settings(SshAuthMethod::Agent)
    }
}

#[gpui::test]
fn ssh_tunnel_a_chain_asks_the_secret_of_the_hop_that_needs_it_and_shows_the_chain_in_the_status(cx: &mut TestAppContext) {
    let mut rig = TunnelRig::new(cx, "redis://cache.example.com:6379/0", chain_settings());
    rig.connect();
    rig.wait_dialog("ssh-secret-input");
    assert_eq!(rig.opener.opened_count(), 0, "nothing is dialled before the jump host's password is known");
    let text = rig.app.read_with(rig.visual, |this, _| this.tunnel_ui.opened_dialogs.clone());
    assert_eq!(text, ["ssh-secret"]);
    click_debug_button(rig.visual, "ssh-secret-input");
    rig.visual.simulate_input("jump1-password-needle");
    rig.click("ssh-secret-ok");
    rig.wait_connected();
    {
        let opened = rig.opener.opened.lock().unwrap();
        assert_eq!(opened.len(), 1);
        assert_eq!(opened[0].jumps.len(), 2);
        assert!(matches!(&opened[0].jumps[0].auth, engine_net::Auth::Password(p) if p.expose() == "jump1-password-needle"));
        assert!(matches!(&opened[0].auth, engine_net::Auth::Agent { .. }), "the answer went to the jump host only");
    }
    assert!(!rig.log_text().contains("jump1-password-needle"));
    let via = rig.app.read_with(rig.visual, |this, _| this.via_ssh(&rig.key)).unwrap();
    assert_eq!(via, format!("via SSH deploy@{BASTION}:2222 (2 jumps)"));
    rig.app.read_with(rig.visual, |this, _| {
        let access = this.secondary_tunnel_access(&rig.key).unwrap();
        assert_eq!(
            access.secrets.get(crate::ssh_tunnel::SecretHop::Jump(0), crate::ssh_tunnel::SecretNeed::Password).map(|s| s.expose().to_owned()).as_deref(),
            Some("jump1-password-needle"),
            "kept for the connection's other sessions, under the hop that asked"
        );
        assert!(access.secrets.password.is_none());
    });
    let key = rig.key.clone();
    rig.app.update(rig.visual, |this, cx| {
        this.workspace.open_query_tab_with_metadata("Redis", TabMetadata { connection_id: key.clone(), ..Default::default() });
        this.reset_active_view();
        cx.notify();
    });
    settle(rig.visual);
    assert!(rig.rendered("status-ssh"));
    // The prompt named the hop (checked on the request itself, the dialog shows `who`).
    let request = crate::ssh_tunnel::SecretRequest {
        need: crate::ssh_tunnel::SecretNeed::Password,
        hop: crate::ssh_tunnel::SecretHop::Jump(0),
        who: chain_settings().hop_label(crate::ssh_tunnel::SecretHop::Jump(0)),
        key_path: String::new(),
        retry: false,
    };
    assert_eq!(request.who, "jump 1 of 2 hop1@jump1.example.com:22");
}

#[gpui::test]
fn ssh_tunnel_a_jump_hosts_key_is_trusted_for_exactly_the_host_and_port_the_dialog_shows(cx: &mut TestAppContext) {
    let mut settings = chain_settings();
    settings.jumps[0].auth = SshAuthMethod::Agent;
    settings.jumps[1].port = "2202".into();
    let mut rig = TunnelRig::new(cx, "redis://cache.example.com:6379/0", settings);
    rig.opener.then(Err(crate::ssh_tunnel::fake::unknown_host_at("jump2.example.com", 2202)));
    rig.connect();
    rig.wait_dialog("ssh-trust-accept");
    assert!(rig.rendered("ssh-trust-host") && rig.rendered("ssh-trust-port"));
    rig.click("ssh-trust-accept");
    rig.wait_connected();
    let trusted = rig.opener.trusted.lock().unwrap().clone();
    assert_eq!(trusted.len(), 1);
    assert_eq!((trusted[0].0.as_str(), trusted[0].1), ("jump2.example.com", 2202), "not the bastion's host:port");
    assert_eq!(rig.opener.opened_count(), 2, "the whole chain was dialled again");
}

#[gpui::test]
fn ssh_tunnel_a_proxy_only_connection_opens_the_proxy_route_and_names_it(cx: &mut TestAppContext) {
    use crate::ssh_tunnel::ProxyType;
    let settings = SshSettings { proxy: proxy_settings(ProxyType::Socks5), ..SshSettings::default() };
    let mut rig = TunnelRig::new(cx, "redis://cache.example.com:6379/0", settings);
    rig.connect();
    rig.wait_connected();
    assert!(rig.opener.opened.lock().unwrap().is_empty(), "no SSH tunnel");
    assert_eq!(rig.opener.opened_proxy.lock().unwrap().len(), 1);
    assert_eq!(rig.redis.connections(), 1, "the database was reached through the proxy route's local port");
    let via = rig.app.read_with(rig.visual, |this, _| this.via_ssh(&rig.key)).unwrap();
    assert_eq!(via, "via SOCKS5 proxy proxy.example.com:1080");
    let key = rig.key.clone();
    rig.app.update(rig.visual, |this, cx| {
        this.workspace.open_query_tab_with_metadata("Redis", TabMetadata { connection_id: key.clone(), ..Default::default() });
        this.reset_active_view();
        cx.notify();
    });
    settle(rig.visual);
    assert!(rig.rendered("status-ssh"));
    let badge = rig.app.read_with(rig.visual, |this, _| this.route_badge(&rig.key));
    assert_eq!(badge, "Proxy");
    // TLS is refused for a proxy-only route exactly as for SSH, before anything is dialled.
    let key = rig.key.clone();
    rig.app.update(rig.visual, |this, cx| this.disconnect_connection(&key, cx));
    settle(rig.visual);
    let before = rig.opener.opened_count();
    rig.app.update(rig.visual, |this, cx| {
        this.ensure_connected_with_uri(key, Some("rediss://cache.example.com:6380/0".to_owned().into()), None, cx);
    });
    let message = rig.wait_failed();
    assert!(message.contains("TLS to Redis"), "{message}");
    assert_eq!(rig.opener.opened_count(), before);
}

#[gpui::test]
fn ssh_tunnel_a_proxy_in_front_of_ssh_asks_the_proxy_password_by_name(cx: &mut TestAppContext) {
    use crate::ssh_tunnel::ProxyType;
    let mut proxy = proxy_settings(ProxyType::Http);
    proxy.user = "svc".into();
    let settings = SshSettings { proxy, ..bastion_settings(SshAuthMethod::Agent) };
    let mut rig = TunnelRig::new(cx, "redis://cache.example.com:6379/0", settings);
    rig.connect();
    rig.wait_dialog("ssh-secret-input");
    click_debug_button(rig.visual, "ssh-secret-input");
    rig.visual.simulate_input("proxy-password-needle");
    rig.click("ssh-secret-ok");
    rig.wait_connected();
    let opened = rig.opener.opened.lock().unwrap().clone();
    let proxy = opened[0].proxy.as_ref().expect("the proxy carries the SSH connection");
    assert_eq!(proxy.credentials.as_ref().unwrap().password.expose(), "proxy-password-needle");
    drop(opened);
    let via = rig.app.read_with(rig.visual, |this, _| this.via_ssh(&rig.key)).unwrap();
    assert_eq!(via, format!("via SSH deploy@{BASTION}:2222 through HTTP proxy proxy.example.com:8080"));
    assert!(!rig.log_text().contains("proxy-password-needle"));
}

#[gpui::test]
fn ssh_tunnel_export_and_import_keep_the_jumps_and_the_proxy_but_switch_them_off(_cx: &mut TestAppContext) {
    use crate::ssh_tunnel::ProxyType;
    let mut proxy = proxy_settings(ProxyType::Socks5);
    proxy.user = "svc".into();
    let settings = SshSettings { proxy, ..chain_settings() };
    let mut record = engine_storage::connection::SavedConnectionRecord {
        id: "11111111-1111-1111-1111-111111111111".into(),
        name: "chain".into(),
        uri: "redis://cache.example.com:6379/0".into(),
        db_type: crate::connections::DB_TYPE_REDIS.into(),
        color_hex: "#34C759".into(),
        save_password: false,
        save_proxy_password: false,
        network_settings_json: String::new(),
        is_pinned: false,
        last_connected: None,
        created_at: 1.0,
        updated_at: 1.0,
        connection_type: String::new(),
        overrides_json: String::new(),
    };
    record.network_settings_json = settings.write_into("{}");
    let export = crate::connections::export_connection_configs(&[record]).unwrap();
    let staged = crate::connections::stage_connection_configs(&export, 2.0).unwrap();
    let imported = SshSettings::parse(&staged[0].network_settings_json).unwrap();
    assert!(!imported.enabled && !imported.proxy.enabled && !imported.routed(), "a file never switches a route on by itself");
    assert_eq!(imported.jumps.iter().map(|j| j.host.as_str()).collect::<Vec<_>>(), ["jump1.example.com", "jump2.example.com"]);
    assert_eq!((imported.proxy.host.as_str(), imported.proxy.user.as_str()), ("proxy.example.com", "svc"));
    assert!(!crate::connections::SavedConnection::from_record(&staged[0]).ssh_tunnel);
}

// ---- review fixes: a saved secret never follows a server it was not entered for ----------------

fn route_record(id: &str, settings: &SshSettings) -> engine_storage::connection::SavedConnectionRecord {
    engine_storage::connection::SavedConnectionRecord {
        id: id.into(),
        name: "route".into(),
        uri: "redis://cache.example.com:6379/0".into(),
        db_type: crate::connections::DB_TYPE_REDIS.into(),
        color_hex: "#34C759".into(),
        save_password: false,
        save_proxy_password: false,
        network_settings_json: settings.write_into("{}"),
        is_pinned: false,
        last_connected: None,
        created_at: 1.0,
        updated_at: 1.0,
        connection_type: String::new(),
        overrides_json: String::new(),
    }
}

fn form_for(settings: SshSettings) -> ConnectionFormState {
    let mut form = ConnectionFormState::new(ConnectionKind::Redis);
    form.name = "route".into();
    form.use_uri = true;
    form.uri = "redis://cache.example.com:6379/0".into();
    form.ssh = Some(settings);
    form
}

#[gpui::test]
fn ssh_tunnel_the_dialogs_test_offers_no_saved_secret_once_a_server_of_the_route_changed(cx: &mut TestAppContext) {
    use crate::ssh_tunnel::ProxyType;
    let (app, visual) = sidebar_test_app(cx);
    let id = "ABCDEF01-1111-4111-8111-111111111111";
    let mut proxy = proxy_settings(ProxyType::Http);
    proxy.user = "svc".into();
    proxy.save_secrets = true;
    let saved = SshSettings { save_secrets: true, proxy, ..chain_settings() };
    let accounts = |app: &gpui::Entity<AppRoot>, visual: &mut gpui::VisualTestContext, record: &engine_storage::connection::SavedConnectionRecord, new: SshSettings| {
        app.read_with(visual, |this, _| this.form_tunnel(&form_for(new), Some(record)).map(|(_, _, accounts)| accounts))
    };
    // The route as saved, on: its saved secrets are offered (the account is the connection's).
    let on = route_record(id, &saved);
    assert_eq!(accounts(&app, visual, &on, saved.clone()).unwrap().len(), 2, "the same route: the connection's accounts");
    // Moving a jump row (or editing one, or the proxy, or the bastion) offers nothing.
    let mut swapped = saved.clone();
    swapped.jumps.swap(0, 1);
    let mut other_proxy = saved.clone();
    other_proxy.proxy.host = "elsewhere.example.com".into();
    let mut other_bastion = saved.clone();
    other_bastion.host = "elsewhere.example.com".into();
    let mut other_jump = saved.clone();
    other_jump.jumps[1].user = "someone-else".into();
    for (what, changed) in [("moved row", swapped), ("proxy", other_proxy), ("bastion", other_bastion), ("jump", other_jump)] {
        assert!(accounts(&app, visual, &on, changed).unwrap().is_empty(), "{what}");
    }
    // The same when the route was stored off (what an import leaves, with its secrets in the store),
    // and when its stored settings cannot be read.
    let mut off = saved.clone();
    off.enabled = false;
    off.proxy.enabled = false;
    let off_record = route_record(id, &off);
    let mut moved_on = saved.clone();
    moved_on.host = "elsewhere.example.com".into();
    moved_on.proxy.host = "elsewhere.example.com".into();
    assert!(accounts(&app, visual, &off_record, moved_on).unwrap().is_empty(), "an imported route that came in off");
    assert_eq!(accounts(&app, visual, &off_record, saved.clone()).unwrap().len(), 2, "unchanged: still offered");
    let mut broken = route_record(id, &saved);
    broken.network_settings_json = r#"{"ssh":5}"#.into();
    assert!(accounts(&app, visual, &broken, saved).unwrap().is_empty(), "unreadable stored settings");
}

#[test]
fn ssh_tunnel_save_after_an_import_that_stored_secrets_clears_them_when_the_server_changes() {
    use crate::connection_import::store::{Batch, ConnectionImportStore as _, MemoryImportStore};
    use crate::connection_import::{ImportKind, ImportedConnection, ImportedJump, ImportedProxy, ImportedSsh};
    use crate::credentials::SecretKind;
    use crate::ssh_tunnel::{persist_secrets, ProxyType, SecretHop, SecretNeed, SshSecretInput, SshSecrets};

    // A file with the bastion password and the proxy password, but no jump host password: the whole
    // route comes in off, and the passwords that were there are stored.
    let mut imported = ImportedConnection {
        host: "db.example.com".into(),
        port: Some(5432),
        username: "u".into(),
        ..ImportedConnection::new("imported".into(), ImportKind::Supported(ConnectionKind::Postgres))
    };
    imported.ssh = Some(ImportedSsh {
        host: "bastion.example.com".into(),
        port: "22".into(),
        user: "deploy".into(),
        auth: SshAuthMethod::Password,
        password: crate::connection_import::Secret::new("bastion-password-of-X"),
        jumps: vec![ImportedJump { host: "jump.example.com".into(), port: "22".into(), user: "hop".into(), auth: SshAuthMethod::Password, ..ImportedJump::default() }],
        ..ImportedSsh::default()
    });
    imported.proxy = Some(ImportedProxy { kind: ProxyType::Http, host: "proxy.example.com".into(), port: "3128".into(), user: "svc".into(), password: crate::connection_import::Secret::new("proxy-password-of-X") });
    let batch = Batch::build(&[imported], 5.0);
    let (record, id) = (batch.records[0].clone(), batch.records[0].id.clone());
    let store = MemoryImportStore::default();
    store.save(batch.records, batch.secrets).unwrap();
    let old = SshSettings::parse(&record.network_settings_json).unwrap();
    assert!(!old.routed(), "the route came in off");
    assert_eq!(store.secret(SecretKind::SshPassword, &id).as_deref(), Some("bastion-password-of-X"));
    assert_eq!(store.secret(SecretKind::Proxy, &id).as_deref(), Some("proxy-password-of-X"));

    // The user points the bastion and the proxy at other servers, completes the jump host and
    // switches the route on. Nothing the old servers were given follows to the new ones.
    let mut new = old.clone();
    new.enabled = true;
    new.host = "bastion-Y.example.com".into();
    new.proxy.enabled = true;
    new.proxy.host = "proxy-Y.example.com".into();
    assert!(SshSettings::route_changed(&old, &new), "the dialog shows its note");
    // What the dialog's Test would load: nothing, the route moved (see the test above); and what a
    // plain load of the saved route gives for the new settings is no longer reachable from the dialog.
    let typed = SshSecretInput::default().with_jumps(vec![engine_net::Secret::new("jump-password")]);
    persist_secrets(&store, &id, Some(&old), &new, &typed).unwrap();
    assert_eq!(store.secret(SecretKind::SshPassword, &id), None, "the bastion password of X is gone");
    assert_eq!(store.secret(SecretKind::Proxy, &id), None, "and so is the proxy password of X");
    assert_eq!(store.secret(SecretKind::SshPassword, &format!("{id}#jump0")).as_deref(), Some("jump-password"), "what was typed is kept");
    let loaded = SshSecrets::load(&store, std::slice::from_ref(&id), &new);
    assert!(loaded.get(SecretHop::Bastion, SecretNeed::Password).is_none());
    assert!(loaded.proxy.is_none());
}

#[gpui::test]
fn ssh_tunnel_removing_a_jump_row_releases_its_widgets(cx: &mut TestAppContext) {
    cx.dispatcher.allow_parking();
    let (app, visual) = dialog_app(cx);
    visual.simulate_resize(gpui::size(gpui::px(1300.0), gpui::px(2400.0)));
    settle(visual);
    let inputs = open_fixture_dialog(&app, visual, crate::ui_fixture::POSTGRES_ID);
    click(visual, "connection-config-tab-tunnel".into());
    click(visual, "ssh-enable-switch".into());
    click_in_tab(visual, "ssh-jump-add");
    let handle = inputs.ssh.jump_host_handle(0).expect("a row");
    assert!(handle.upgrade().is_some());
    click_in_tab(visual, "ssh-jump-0-remove");
    assert_eq!(inputs.ssh.jump_count(), 0);
    settle(visual);
    assert!(handle.upgrade().is_none(), "the row's inputs and its event handlers went with it");
    // Replacing all rows (the picker) releases the old ones too.
    visual.update(|window, cx| inputs.ssh.set_jumps(&[jump("a.example.com", "u", SshAuthMethod::Agent)], &[], window, cx));
    let first = inputs.ssh.jump_host_handle(0).unwrap();
    visual.update(|window, cx| inputs.ssh.set_jumps(&[jump("b.example.com", "u", SshAuthMethod::Agent)], &[], window, cx));
    settle(visual);
    assert!(first.upgrade().is_none());
}

#[test]
fn ssh_tunnel_the_trust_dialog_names_the_jump_hosts_own_host_port_and_login() {
    let settings = chain_settings();
    let mut settings = settings;
    settings.jumps[1].port = "2202".into();
    settings.jumps[1].user = "hop2".into();
    let crate::ssh_tunnel::ConnectFailure::HostKeyUnknown(request) =
        crate::ssh_tunnel::classify(crate::ssh_tunnel::fake::unknown_host_at("jump2.example.com", 2202), &settings)
    else {
        panic!("a host key question");
    };
    let texts = tunnel_ui::trust_texts(&request, "/data/known_hosts");
    assert!(texts.intro.contains("jump2.example.com:2202"), "{}", texts.intro);
    let row = |label: &str| texts.rows.iter().find(|row| row.0 == label).map(|row| row.1.clone());
    assert_eq!(row("Host").as_deref(), Some("jump2.example.com"));
    assert_eq!(row("Port").as_deref(), Some("2202"));
    assert_eq!(row("Login").as_deref(), Some("hop2"), "that hop's login, not the bastion's");
    assert!(texts.outro.contains("/data/known_hosts"));
    // The bastion's own question shows the bastion's login.
    let crate::ssh_tunnel::ConnectFailure::HostKeyUnknown(bastion) =
        crate::ssh_tunnel::classify(crate::ssh_tunnel::fake::unknown_host_at(BASTION, 2222), &settings)
    else {
        panic!("a host key question");
    };
    assert_eq!(tunnel_ui::trust_texts(&bastion, "k").rows.iter().find(|r| r.0 == "Login").unwrap().1, "deploy");
}

#[gpui::test]
fn ssh_tunnel_the_trust_dialog_of_a_jump_host_renders_its_login_row(cx: &mut TestAppContext) {
    let mut settings = chain_settings();
    settings.jumps[0].auth = SshAuthMethod::Agent;
    settings.jumps[1].port = "2202".into();
    let mut rig = TunnelRig::new(cx, "redis://cache.example.com:6379/0", settings);
    rig.opener.then(Err(crate::ssh_tunnel::fake::unknown_host_at("jump2.example.com", 2202)));
    rig.connect();
    rig.wait_dialog("ssh-trust-accept");
    assert!(rig.rendered("ssh-trust-user"), "the login row is on the dialog");
    rig.click("ssh-trust-accept");
    rig.wait_connected();
}

#[gpui::test]
fn ssh_tunnel_the_apply_session_never_sends_one_jump_hosts_secret_to_another(cx: &mut TestAppContext) {
    use crate::ssh_tunnel::{SecretHop, SecretNeed, SshSecrets};
    let mut settings = chain_settings();
    settings.jumps[1].auth = SshAuthMethod::Agent;
    let mut rig = TunnelRig::new(cx, "redis://cache.example.com:6379/0", settings.clone());
    let key = rig.key.clone();
    rig.app.update(rig.visual, |this, _| {
        this.tunnel_ui.secrets.insert(key, SshSecrets::only(SecretHop::Jump(0), SecretNeed::Password, engine_net::Secret::new("J1-password")));
    });
    rig.connect();
    rig.wait_connected();

    // Save on the still-connected connection: jump 0 becomes another server with a typed password.
    // This is what Save leaves behind (connection_ui): the new settings in the record, the new
    // typed secrets in `TunnelUi::secrets`, the old tunnel with its old access still live.
    let mut edited = settings.clone();
    edited.jumps[0].host = "jump-J2.example.com".into();
    let key = rig.key.clone();
    rig.app.update(rig.visual, |this, _| {
        this.tunnel_ui.settings_override.insert(key.clone(), edited);
        this.tunnel_ui.secrets.insert(key, SshSecrets::only(SecretHop::Jump(0), SecretNeed::Password, engine_net::Secret::new("J2-password")));
    });
    let access = rig.app.read_with(rig.visual, |this, _| this.secondary_tunnel_access(&rig.key)).expect("the live tunnel's access");
    assert_eq!(access.settings.jumps[0].host, "jump1.example.com", "the session opens the route the tunnel has");
    let spec = crate::ssh_tunnel::build_spec(&access.settings, &access.secrets, std::path::Path::new("/tmp/kh"), &[engine_net::Target::new("cache.example.com", 6379)]).unwrap();
    assert_eq!(spec.jumps[0].host, "jump1.example.com");
    assert!(
        matches!(&spec.jumps[0].auth, engine_net::Auth::Password(p) if p.expose() == "J1-password"),
        "jump 1 gets jump 1's password and never the one typed for jump J2: {:?}",
        spec.jumps[0].auth
    );
    assert!(!format!("{:?}", access.secrets).contains("J2"));
}
