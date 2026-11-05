// The TLS/SSL tab of the connection dialog, driven through the real controls (the mode dropdown,
// the path inputs, the file picker, Test and Save) on the fixture connections.

/// A file the TLS checks can find, removed by the test's guard.
struct TlsFile(std::path::PathBuf);

impl TlsFile {
    fn new(name: &str) -> Self {
        let path = std::env::temp_dir().join(format!("tablex-tls-ui-{}-{name}", uuid::Uuid::new_v4()));
        std::fs::write(&path, "-----BEGIN CERTIFICATE-----\n").unwrap();
        Self(path)
    }

    fn text(&self) -> String {
        self.0.display().to_string()
    }
}

impl Drop for TlsFile {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.0);
    }
}

fn tls_dialog(
    app: &gpui::Entity<AppRoot>,
    visual: &mut gpui::VisualTestContext,
    connection: &str,
) -> ConnectionDialogInputs {
    let inputs = open_fixture_dialog(app, visual, connection);
    click(visual, "connection-config-tab-tls".into());
    inputs
}

/// Clicks into the path input of `row` (right of the 110px label, in its first line) and types `text` over what is there.
fn tls_type(visual: &mut gpui::VisualTestContext, row: &'static str, text: &str) {
    let bounds = visual.debug_bounds(row).unwrap_or_else(|| panic!("{row} is rendered"));
    visual.simulate_click(
        gpui::point(bounds.origin.x + gpui::px(160.0), bounds.origin.y + gpui::px(16.0)),
        Default::default(),
    );
    visual.simulate_keystrokes("secondary-a");
    visual.simulate_input(text);
    settle(visual);
}

/// Opens the mode dropdown and moves `downs` rows from the shown mode.
fn tls_pick_mode(visual: &mut gpui::VisualTestContext, keys: &str) {
    click(visual, "connection-config-ssl-mode-select".into());
    visual.simulate_keystrokes(keys);
    settle(visual);
}

fn tls_shown(
    inputs: &ConnectionDialogInputs,
    visual: &mut gpui::VisualTestContext,
) -> (Option<String>, Option<String>) {
    visual.update(|_, cx| inputs.tls.shown_for_test(cx))
}

fn tls_saved_uri(app: &gpui::Entity<AppRoot>, visual: &mut gpui::VisualTestContext, id: &str) -> String {
    app.read_with(visual, |this, _| {
        this.saved_connections.iter().find(|saved| saved.matches_id(id)).unwrap().uri.clone()
    })
}

/// A saved connection of `kind` to edit (the sample fixture has no MySQL / MariaDB).
fn tls_add_connection(
    app: &gpui::Entity<AppRoot>,
    visual: &mut gpui::VisualTestContext,
    kind: ConnectionKind,
    uri: &str,
) -> String {
    let id = uuid::Uuid::new_v4();
    let mut form = ConnectionFormState {
        id: Some(id),
        name: "TLS".into(),
        kind,
        uri: uri.into(),
        use_uri: true,
        ..Default::default()
    };
    form.parse_uri_fields();
    let record = form.to_record(1.0, None).unwrap();
    app.update(visual, |this, _| {
        this.saved_connections.push(crate::connections::SavedConnection::from_record(&record));
    });
    id.to_string()
}

#[gpui::test]
fn postgres_tls_tab_edits_validates_saves_and_reopens(cx: &mut TestAppContext) {
    cx.dispatcher.allow_parking();
    let (app, visual) = dialog_app(cx);
    let pg = crate::ui_fixture::POSTGRES_ID;
    let (ca, cert, key) = (TlsFile::new("ca.pem"), TlsFile::new("cert.pem"), TlsFile::new("key.pem"));
    let inputs = tls_dialog(&app, visual, pg);

    // The "not supported" stub is gone; the tab offers the driver's modes, on the default.
    assert!(!rendered(visual, "connection-config-tls-notice".into()));
    for row in ["connection-config-ssl-mode", "connection-config-ssl-ca", "connection-config-ssl-cert", "connection-config-ssl-key"] {
        assert!(rendered(visual, row.into()), "{row}");
    }
    assert!(rendered(visual, "connection-config-ssl-server-name-unsupported".into()), "PostgreSQL has no server-name override and says so");
    assert_eq!(tls_shown(&inputs, visual), (Some("Default (prefer)".into()), None));

    // verify-ca (Default, disable, allow, prefer, require, verify-ca): refused without a CA, inline.
    tls_pick_mode(visual, "down down down down down enter");
    let (label, problem) = tls_shown(&inputs, visual);
    assert_eq!(label.as_deref(), Some("verify-ca"));
    assert!(problem.unwrap().contains("verify-ca needs a CA certificate file"));
    assert!(rendered(visual, "connection-config-tls-error".into()));
    // Save is blocked and nothing is stored.
    let before = tls_saved_uri(&app, visual, pg);
    click(visual, "connection-save".into());
    assert_eq!(tls_saved_uri(&app, visual, pg), before);
    assert!(rendered(visual, "connection-config-tls-error".into()), "the dialog stays open on the error");
    // The refusal is an alert on top of the dialog; close it.
    visual.simulate_keystrokes("escape");
    settle(visual);

    // Typing the CA clears the inline error as soon as the path is there.
    tls_type(visual, "connection-config-ssl-ca", &ca.text());
    assert_eq!(tls_shown(&inputs, visual).1, None);
    assert!(!rendered(visual, "connection-config-tls-error".into()));

    // The file picker fills the client certificate.
    click(visual, "tls-browse-cert".into());
    assert!(visual.did_prompt_for_paths(), "Browse asks the platform for a file");
    let chosen = cert.0.clone();
    visual.simulate_path_prompt_response(move |options| {
        assert!(options.files && !options.multiple);
        Some(vec![chosen.clone()])
    });
    settle(visual);
    visual.cx.executor().advance_clock(Duration::from_millis(50));
    settle(visual);
    assert_eq!(visual.update(|_, cx| inputs.tls.read(cx)).client_cert, cert.text());

    // A certificate without its key is an inline error; so is a key that is not there.
    assert!(tls_shown(&inputs, visual).1.unwrap().contains("together"));
    let missing = std::env::temp_dir().join("tablex-tls-ui-missing-key.pem").display().to_string();
    tls_type(visual, "connection-config-ssl-key", &missing);
    assert_eq!(tls_shown(&inputs, visual).1, None, "the disk is only read on Test / Save");
    click(visual, "test-connection".into());
    let status = inputs.test_status.borrow().clone();
    assert!(
        matches!(&status, crate::connection_dialog::TestStatus::Failure(message) if message.contains("client key file") && message.contains("does not exist")),
        "{status:?}"
    );

    // With the key in place the password stays out of the stored URI and Save keeps everything.
    tls_type(visual, "connection-config-ssl-key", &key.text());
    visual.update(|window, cx| inputs.password.update(cx, |state, cx| state.set_value("s3cret-value", window, cx)));
    click(visual, "connection-save".into());
    let uri = tls_saved_uri(&app, visual, pg);
    assert!(uri.contains("sslmode=verify-ca"), "{uri}");
    for file in [&ca, &cert, &key] {
        let encoded = percent_encoding::utf8_percent_encode(&file.text(), crate::connection_tls::VALUE).to_string();
        assert!(uri.contains(&encoded), "{uri}");
    }
    assert!(!uri.contains("s3cret-value"), "no secret in the stored URI: {uri}");

    // Reopening shows the saved values, the mode and no leftover in the URL params.
    let again = tls_dialog(&app, visual, pg);
    assert_eq!(tls_shown(&again, visual), (Some("verify-ca".into()), None));
    let tls = visual.update(|_, cx| again.tls.read(cx));
    assert_eq!((tls.ca_file, tls.client_cert, tls.client_key), (ca.text(), cert.text(), key.text()));
    assert_eq!(visual.update(|_, cx| again.uri_options.read(cx).value().to_string()), "");
    // Saving again without touching it writes the same URI back.
    click(visual, "connection-save".into());
    assert_eq!(tls_saved_uri(&app, visual, pg), uri);

    // Switching to a mode that needs nothing clears the error and updates the URI.
    let third = tls_dialog(&app, visual, pg);
    tls_pick_mode(visual, "up enter");
    assert_eq!(tls_shown(&third, visual).0.as_deref(), Some("require"));
    click(visual, "connection-save".into());
    let uri = tls_saved_uri(&app, visual, pg);
    assert!(uri.contains("sslmode=require") && !uri.contains("verify-ca"), "{uri}");
}

#[gpui::test]
fn mysql_and_mariadb_tls_tab_uses_the_mysql_modes_and_server_name(cx: &mut TestAppContext) {
    cx.dispatcher.allow_parking();
    let (app, visual) = dialog_app(cx);
    let ca = TlsFile::new("my-ca.pem");
    for (kind, scheme) in [(ConnectionKind::Mysql, "mysql"), (ConnectionKind::Mariadb, "mariadb")] {
        let id = tls_add_connection(&app, visual, kind, &format!("{scheme}://app@db.example.invalid/shop"));
        let inputs = tls_dialog(&app, visual, &id);
        assert_eq!(tls_shown(&inputs, visual).0.as_deref(), Some("Default (PREFERRED)"));
        assert!(!rendered(visual, "connection-config-ssl-server-name-unsupported".into()), "the server name is editable");
        assert!(rendered(visual, "connection-config-tls-key-note".into()));

        // VERIFY_IDENTITY (Default, DISABLED, allow, PREFERRED, REQUIRED, VERIFY_CA, VERIFY_IDENTITY).
        tls_pick_mode(visual, "down down down down down down enter");
        assert_eq!(tls_shown(&inputs, visual).0.as_deref(), Some("VERIFY_IDENTITY"));
        tls_type(visual, "connection-config-ssl-ca", &ca.text());
        tls_type(visual, "connection-config-ssl-server-name", "db.internal");
        click(visual, "connection-save".into());
        let uri = tls_saved_uri(&app, visual, &id);
        assert!(uri.contains("ssl-mode=VERIFY_IDENTITY&ssl-ca="), "{uri}");
        assert!(uri.ends_with("&tls-server-name=db.internal"), "{uri}");

        let again = tls_dialog(&app, visual, &id);
        assert_eq!(tls_shown(&again, visual), (Some("VERIFY_IDENTITY".into()), None));
        assert_eq!(visual.update(|_, cx| again.tls.read(cx)).server_name, "db.internal");
    }
}

#[gpui::test]
fn mongodb_tls_tab_offers_its_own_modes_and_one_certificate_file(cx: &mut TestAppContext) {
    cx.dispatcher.allow_parking();
    let (app, visual) = dialog_app(cx);
    let mongo = crate::ui_fixture::MONGO_ID;
    let pem = TlsFile::new("mongo-client.pem");
    let inputs = tls_dialog(&app, visual, mongo);
    assert_eq!(tls_shown(&inputs, visual).0.as_deref(), Some("Default (off)"));
    assert!(rendered(visual, "connection-config-ssl-key-unsupported".into()), "the key shares the certificate file");
    assert!(rendered(visual, "connection-config-ssl-server-name-unsupported".into()));
    assert!(!rendered(visual, "connection-config-tls-key-note".into()));

    // Default (off), Off, On (verify certificate), On (skip verification).
    tls_pick_mode(visual, "down down down enter");
    assert_eq!(tls_shown(&inputs, visual).0.as_deref(), Some("On (skip verification)"));
    tls_type(visual, "connection-config-ssl-cert", &pem.text());
    click(visual, "connection-save".into());
    let uri = tls_saved_uri(&app, visual, mongo);
    assert!(uri.contains("tls=true") && uri.contains("tlsAllowInvalidCertificates=true"), "{uri}");
    assert!(uri.contains("tlsCertificateKeyFile="), "{uri}");

    // Off with a certificate file set is refused (the driver rejects the combination).
    let again = tls_dialog(&app, visual, mongo);
    assert_eq!(tls_shown(&again, visual).0.as_deref(), Some("On (skip verification)"));
    tls_pick_mode(visual, "up up enter");
    let (label, problem) = tls_shown(&again, visual);
    assert_eq!(label.as_deref(), Some("Off"));
    assert!(problem.unwrap().contains("TLS is off"));
    let before = tls_saved_uri(&app, visual, mongo);
    click(visual, "connection-save".into());
    assert_eq!(tls_saved_uri(&app, visual, mongo), before);
    visual.simulate_keystrokes("escape");
    settle(visual);

    // A kind change keeps only modes the driver offers.
    again.kind.set(ConnectionKind::Postgres);
    visual.update(|window, cx| again.tls.sync_kind(window, cx));
    assert_eq!(tls_shown(&again, visual).0.as_deref(), Some("disable"));
    again.kind.set(ConnectionKind::Mongodb);
}

// ---- live: the settings the dialog writes reach the real drivers ------------------------------
//
// All `#[ignore]`d. The form below is the one the TLS tab fills; its runtime URI goes to the real
// driver (no tunnel). Run:
//
//   TABLEX_TEST_TLS_DIR=<scratch dir> rust-engine/scripts/test-tls-servers.sh start all
//   source "$(TABLEX_TEST_TLS_DIR=<scratch dir> rust-engine/scripts/test-tls-servers.sh env)"
//   cargo test -p tablex-gpui live_tls -- --ignored --test-threads=1
//
// The server certificate names `localhost` and `db.tablex.test` (no IP address).

struct LiveTls {
    host: String,
    port: String,
    user: String,
    password: String,
    database: String,
    ca: String,
    other_ca: String,
    server_name: String,
    client_cert: String,
    client_key: String,
}

impl LiveTls {
    fn from_env(prefix: &str) -> Option<Self> {
        let var = |name: String| std::env::var(name).ok().filter(|value| !value.is_empty());
        let own = |suffix: &str| var(format!("TABLEX_TEST_{prefix}_TLS_{suffix}"));
        let shared = |suffix: &str| var(format!("TABLEX_TEST_TLS_{suffix}"));
        let live = (|| {
            Some(Self {
                host: own("HOST")?,
                port: own("PORT")?,
                user: own("USER")?,
                password: own("PASSWORD")?,
                database: own("DB")?,
                ca: shared("CA")?,
                other_ca: shared("OTHER_CA")?,
                server_name: shared("SERVER_NAME")?,
                client_cert: shared("CLIENT_CERT")?,
                client_key: shared("CLIENT_KEY")?,
            })
        })();
        if live.is_none() {
            eprintln!("skipped: source the env file of rust-engine/scripts/test-tls-servers.sh");
        }
        live
    }

    /// The form the dialog would hold for this server.
    fn form(&self, kind: ConnectionKind, host: &str, tls: crate::connection_tls::TlsSettings) -> ConnectionFormState {
        let mut form = ConnectionFormState::new(kind);
        form.name = "live tls".into();
        form.host = host.into();
        form.port = self.port.clone();
        form.username = self.user.clone();
        form.password = self.password.clone();
        form.database = self.database.clone();
        form.tls = tls;
        form
    }
}

/// Validates the form like Test does, connects the runtime URI on the real driver and returns the
/// status (and the first cell of a trivial query when connected).
fn live_tls_connect(cx: &mut TestAppContext, live: &LiveKind, form: &ConnectionFormState) -> Result<String, String> {
    form.validate()?;
    let uri = form.runtime_uri()?;
    let mut rig = LiveRig::new(cx, live, uri, SshSettings::default());
    match rig.connect() {
        ConnectionStatus::Connected => {
            let cell = rig.query_first_cell();
            rig.disconnect();
            Ok(cell)
        }
        ConnectionStatus::Failed(message) => Err(message),
        other => panic!("{other:?}"),
    }
}

fn live_tls_settings(mode: crate::connection_tls::TlsMode, ca: &str) -> crate::connection_tls::TlsSettings {
    crate::connection_tls::TlsSettings { mode, ca_file: ca.into(), ..Default::default() }
}

#[gpui::test]
#[ignore = "needs tablex-test-pg-tls (test-tls-servers.sh)"]
fn live_tls_postgresql_modes_and_client_certificate_reach_the_driver(cx: &mut TestAppContext) {
    use crate::connection_tls::TlsMode;
    let Some(t) = LiveTls::from_env("PG") else { return };
    // The server accepts TLS only: verify-full with the right CA and a name it carries connects.
    let form = t.form(ConnectionKind::Postgres, "localhost", live_tls_settings(TlsMode::VerifyFull, &t.ca));
    assert_eq!(live_tls_connect(cx, &PG, &form), Ok("1".to_string()));
    // verify-ca does not look at the name: 127.0.0.1 is not in the certificate.
    let form = t.form(ConnectionKind::Postgres, &t.host, live_tls_settings(TlsMode::VerifyCa, &t.ca));
    assert_eq!(live_tls_connect(cx, &PG, &form), Ok("1".to_string()));
    // verify-full does.
    let form = t.form(ConnectionKind::Postgres, &t.host, live_tls_settings(TlsMode::VerifyFull, &t.ca));
    let error = live_tls_connect(cx, &PG, &form).unwrap_err();
    assert!(error.to_lowercase().contains("name") || error.contains("tls_certificate"), "{error}");
    // A CA that did not sign the certificate is refused.
    let form = t.form(ConnectionKind::Postgres, "localhost", live_tls_settings(TlsMode::VerifyFull, &t.other_ca));
    assert!(live_tls_connect(cx, &PG, &form).is_err());
    // `disable` reaches the driver too: the hostssl-only server refuses clear text.
    let form = t.form(ConnectionKind::Postgres, "localhost", live_tls_settings(TlsMode::Disable, ""));
    assert!(live_tls_connect(cx, &PG, &form).is_err());
    // The form refuses verify-ca without a CA before any connection.
    let form = t.form(ConnectionKind::Postgres, "localhost", live_tls_settings(TlsMode::VerifyCa, ""));
    assert!(live_tls_connect(cx, &PG, &form).unwrap_err().contains("verify-ca needs"));
    // Client certificate and key: the role `tablex_cert` authenticates by its certificate alone.
    let mut form = t.form(
        ConnectionKind::Postgres,
        "localhost",
        crate::connection_tls::TlsSettings {
            mode: TlsMode::VerifyFull,
            ca_file: t.ca.clone(),
            client_cert: t.client_cert.clone(),
            client_key: t.client_key.clone(),
            ..Default::default()
        },
    );
    form.username = std::env::var("TABLEX_TEST_PG_TLS_CERT_USER").expect("the certificate role");
    form.password.clear();
    assert_eq!(live_tls_connect(cx, &PG, &form), Ok("1".to_string()));
}

fn live_tls_mysql_family(cx: &mut TestAppContext, live: &LiveKind, kind: ConnectionKind, prefix: &str) {
    use crate::connection_tls::{TlsMode, TlsSettings};
    let Some(t) = LiveTls::from_env(prefix) else { return };
    // The tunnel's trick, by hand: connect to the IP, verify the certificate's own name.
    let named = TlsSettings { server_name: t.server_name.clone(), ..live_tls_settings(TlsMode::VerifyFull, &t.ca) };
    let form = t.form(kind, &t.host, named);
    assert_eq!(live_tls_connect(cx, live, &form), Ok("1".to_string()));
    // Without the server name the IP does not match the certificate.
    let form = t.form(kind, &t.host, live_tls_settings(TlsMode::VerifyFull, &t.ca));
    assert!(live_tls_connect(cx, live, &form).is_err());
    // VERIFY_CA ignores the name.
    let form = t.form(kind, &t.host, live_tls_settings(TlsMode::VerifyCa, &t.ca));
    assert_eq!(live_tls_connect(cx, live, &form), Ok("1".to_string()));
    // `DISABLED` reaches the driver: require_secure_transport refuses clear text.
    let form = t.form(kind, &t.host, live_tls_settings(TlsMode::Disable, ""));
    assert!(live_tls_connect(cx, live, &form).is_err());
    // The X509 account (same password) also needs the client certificate and key.
    let x509 = std::env::var(format!("TABLEX_TEST_{prefix}_TLS_X509_USER")).expect("the X509 account");
    let identity = TlsSettings {
        client_cert: t.client_cert.clone(),
        client_key: t.client_key.clone(),
        ..live_tls_settings(TlsMode::VerifyCa, &t.ca)
    };
    let mut form = t.form(kind, &t.host, identity);
    form.username = x509;
    assert_eq!(live_tls_connect(cx, live, &form), Ok("1".to_string()));
    form.tls = live_tls_settings(TlsMode::VerifyCa, &t.ca);
    assert!(live_tls_connect(cx, live, &form).is_err(), "REQUIRE X509 without the certificate");
}

#[gpui::test]
#[ignore = "needs tablex-test-mysql-tls (test-tls-servers.sh)"]
fn live_tls_mysql_modes_server_name_and_client_certificate_reach_the_driver(cx: &mut TestAppContext) {
    live_tls_mysql_family(cx, &MYSQL, ConnectionKind::Mysql, "MYSQL");
}

#[gpui::test]
#[ignore = "needs tablex-test-mariadb-tls (test-tls-servers.sh)"]
fn live_tls_mariadb_modes_server_name_and_client_certificate_reach_the_driver(cx: &mut TestAppContext) {
    live_tls_mysql_family(cx, &MARIADB, ConnectionKind::Mariadb, "MARIADB");
}
