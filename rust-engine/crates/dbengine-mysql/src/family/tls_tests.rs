//! Tests of `tls.rs`: offline unit tests, a throwaway MySQL-protocol listener, and certificates
//! generated in the test (no containers).

use super::*;

/// A file in `dir` (a CA, certificate or key: contents are irrelevant to the parser).
fn file(dir: &Path, name: &str, content: &[u8]) -> PathBuf {
    let path = dir.join(name);
    std::fs::write(&path, content).unwrap();
    path
}

/// `chmod 600` (a key file other users can read is refused).
fn private(path: &Path) {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600)).unwrap();
    }
    #[cfg(not(unix))]
    let _ = path;
}

fn parse(url: &str) -> (String, TlsParams) {
    TlsParams::from_url(url).unwrap()
}

#[test]
fn preferred_is_the_default_and_a_plain_url_is_untouched() {
    let (url, params) = parse("mysql://u:p@h:3306/db");
    assert_eq!(url, "mysql://u:p@h:3306/db");
    assert_eq!(params.mode, Mode::Preferred);
    assert_eq!(
        (params.ca, params.client, params.server_name),
        (None, None, None)
    );
}

#[test]
fn every_mode_spelling_parses() {
    let dir = tempfile::tempdir().unwrap();
    let ca = file(dir.path(), "ca.pem", b"x");
    for (value, mode) in [
        ("DISABLED", Mode::Disabled),
        ("disabled", Mode::Disabled),
        ("PREFERRED", Mode::Preferred),
        ("REQUIRED", Mode::Required),
        ("VERIFY_CA", Mode::VerifyCa),
        ("VERIFY_IDENTITY", Mode::VerifyIdentity),
        ("disable", Mode::Disabled),
        ("allow", Mode::Allow),
        ("prefer", Mode::Preferred),
        ("require", Mode::Required),
        ("verify-ca", Mode::VerifyCa),
        ("verify-full", Mode::VerifyIdentity),
    ] {
        for name in ["ssl-mode", "sslmode"] {
            assert_eq!(
                parse(&format!(
                    "mysql://h/d?{name}={value}&ssl-ca={}",
                    ca.display()
                ))
                .1
                .mode,
                mode,
                "{name}={value}"
            );
        }
    }
    assert_eq!(
        TlsParams::from_url("mysql://h/d?ssl-mode=maybe").unwrap_err(),
        TlsConfigError::InvalidMode("maybe".into())
    );
}

#[test]
fn tls_parameters_are_removed_and_the_rest_survives_in_order() {
    let dir = tempfile::tempdir().unwrap();
    let (ca, cert, key) = (
        dir.path().join("ca.pem"),
        dir.path().join("c.pem"),
        dir.path().join("k.pem"),
    );
    for path in [&ca, &cert, &key] {
        std::fs::write(path, "x").unwrap();
    }
    private(&key);
    let url = format!(
        "mysql://u@h/d?prefer_socket=false&ssl-mode=VERIFY_IDENTITY&ssl-ca={}&ssl-cert={}&ssl-key={}&tls-server-name=db.example&wait_timeout=30",
        ca.display(), cert.display(), key.display()
    );
    let (cleaned, params) = parse(&url);
    assert_eq!(cleaned, "mysql://u@h/d?prefer_socket=false&wait_timeout=30");
    assert_eq!(params.mode, Mode::VerifyIdentity);
    assert_eq!(params.server_name.as_deref(), Some("db.example"));
    // mysql_async parses what is left.
    Opts::from_url(&cleaned).unwrap();
}

#[test]
fn mysql_async_s_own_require_ssl_still_decides_without_ssl_mode() {
    let (cleaned, params) = parse("mysql://h/d?require_ssl=true&verify_ca=false");
    assert_eq!(params.mode, Mode::Native);
    assert_eq!(cleaned, "mysql://h/d?require_ssl=true&verify_ca=false");
    let opts = Opts::from_url(&cleaned).unwrap();
    let attempts = params.attempts(&opts);
    assert_eq!(attempts.len(), 1);
    assert!(attempts[0].ssl_opts().unwrap().accept_invalid_certs());
    // `require_ssl=false` is also native: no TLS, no preferred attempt.
    let (cleaned, params) = parse("mysql://h/d?require_ssl=false");
    assert_eq!(params.mode, Mode::Native);
    assert!(params.attempts(&Opts::from_url(&cleaned).unwrap())[0]
        .ssl_opts()
        .is_none());
    // ssl-mode wins over the native switches.
    assert_eq!(
        parse("mysql://h/d?require_ssl=true&ssl-mode=DISABLED")
            .1
            .mode,
        Mode::Disabled
    );
}

#[test]
fn modes_map_to_ssl_opts_and_attempt_order() {
    let attempts = |query: &str| {
        let (cleaned, params) = parse(&format!("mysql://h/d?{query}"));
        let opts = Opts::from_url(&cleaned).unwrap();
        params.attempts(&opts)
    };
    // DISABLED: one clear-text attempt.
    let disabled = attempts("ssl-mode=DISABLED");
    assert_eq!(disabled.len(), 1);
    assert!(disabled[0].ssl_opts().is_none());
    // PREFERRED: TLS without any check, then clear text.
    let preferred = attempts("");
    assert_eq!(preferred.len(), 2);
    let tls = preferred[0].ssl_opts().unwrap();
    assert!(tls.accept_invalid_certs() && tls.skip_domain_validation());
    assert!(preferred[1].ssl_opts().is_none());
    // allow: clear text, then TLS.
    let allow = attempts("sslmode=allow");
    assert!(allow[0].ssl_opts().is_none() && allow[1].ssl_opts().is_some());
    // REQUIRED: TLS only; unverified without a CA.
    let required = attempts("ssl-mode=REQUIRED");
    assert_eq!(required.len(), 1);
    assert!(required[0].ssl_opts().unwrap().accept_invalid_certs());
    // VERIFY_CA: chain against the CA file, any name; VERIFY_IDENTITY: chain and name.
    let dir = tempfile::tempdir().unwrap();
    let ca_file = file(dir.path(), "ca.pem", b"x");
    let ca = attempts(&format!("ssl-mode=VERIFY_CA&ssl-ca={}", ca_file.display()));
    let ca = ca[0].ssl_opts().unwrap();
    assert!(!ca.accept_invalid_certs() && ca.skip_domain_validation());
    assert!(
        ca.disable_built_in_roots() && ca.root_certs().len() == 1,
        "only the CA file is trusted"
    );
    let identity = attempts("ssl-mode=VERIFY_IDENTITY");
    let identity = identity[0].ssl_opts().unwrap();
    assert!(!identity.accept_invalid_certs() && !identity.skip_domain_validation());
    assert!(!identity.disable_built_in_roots());
}

#[test]
fn a_ca_file_pins_the_roots_and_turns_required_into_a_chain_check() {
    let dir = tempfile::tempdir().unwrap();
    let ca = dir.path().join("ca.pem");
    std::fs::write(&ca, "x").unwrap();
    let url = format!(
        "mysql://h/d?ssl-mode=REQUIRED&ssl-ca={}&tls-server-name=db.example",
        ca.display()
    );
    let (cleaned, params) = parse(&url);
    let opts = Opts::from_url(&cleaned).unwrap();
    let attempts = params.attempts(&opts);
    let ssl = attempts[0].ssl_opts().unwrap();
    assert!(
        !ssl.accept_invalid_certs(),
        "a CA file means the chain is checked"
    );
    assert!(
        ssl.skip_domain_validation(),
        "REQUIRED never checks the name"
    );
    assert_eq!(ssl.root_certs().len(), 1);
    assert!(ssl.disable_built_in_roots());
    assert_eq!(ssl.tls_hostname_override(), Some("db.example"));
}

#[test]
fn only_the_server_not_offering_tls_lets_preferred_fall_back() {
    let preferred = parse("mysql://h/d").1;
    assert!(preferred.falls_through(&Error::Driver(DriverError::NoClientSslFlagFromServer)));
    // A failed handshake, a bad password, a closed socket: errors, not downgrades.
    assert!(!preferred.falls_through(&Error::Other("tls".into())));
    assert!(!preferred.falls_through(&Error::Io(IoError::Io(std::io::Error::other("reset")))));
    assert!(
        !preferred.falls_through(&Error::Server(mysql_async::ServerError {
            code: 1045,
            message: "Access denied".into(),
            state: "28000".into(),
        }))
    );
    let required = parse("mysql://h/d?ssl-mode=REQUIRED").1;
    assert!(!required.falls_through(&Error::Driver(DriverError::NoClientSslFlagFromServer)));
}

#[test]
fn incomplete_identity_and_bad_files_are_typed_and_leak_nothing() {
    assert_eq!(
        TlsParams::from_url("mysql://h/d?ssl-cert=/c.pem").unwrap_err(),
        TlsConfigError::IncompleteClientIdentity
    );
    let missing =
        TlsParams::from_url("mysql://u:hunter2@h/d?ssl-ca=/nonexistent/ca.pem").unwrap_err();
    assert!(matches!(
        missing,
        TlsConfigError::File { what: "ssl-ca", .. }
    ));
    assert!(!missing.to_string().contains("hunter2"));
    // DISABLED never reads the files.
    assert!(
        TlsParams::from_url("mysql://h/d?ssl-mode=DISABLED&ssl-ca=/nonexistent/ca.pem").is_ok()
    );

    let dir = tempfile::tempdir().unwrap();
    let (cert, key) = (dir.path().join("c.pem"), dir.path().join("k.pem"));
    std::fs::write(&cert, "x").unwrap();
    std::fs::write(
        &key,
        "-----BEGIN ENCRYPTED PRIVATE KEY-----\nTOPSECRET\n-----END ENCRYPTED PRIVATE KEY-----\n",
    )
    .unwrap();
    private(&key);
    let error = TlsParams::from_url(&format!(
        "mysql://h/d?ssl-cert={}&ssl-key={}",
        cert.display(),
        key.display()
    ))
    .unwrap_err();
    assert!(matches!(error, TlsConfigError::EncryptedPrivateKey { .. }));
    assert!(!error.to_string().contains("TOPSECRET"));
    let backend: engine_driver_kit::BackendError = error.into();
    assert_eq!(backend.code.as_deref(), Some("connect.tls_config"));
}

#[test]
fn tls_failures_get_stable_codes() {
    let params = parse("mysql://h/d").1;
    let cert = |problem: rustls::CertificateError| {
        let io = std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            rustls::Error::InvalidCertificate(problem),
        );
        connect_error(&Error::Io(IoError::Io(io)), &params).unwrap()
    };
    let unknown = cert(rustls::CertificateError::UnknownIssuer);
    assert_eq!(unknown.code.as_deref(), Some("connect.tls_certificate"));
    assert!(unknown.message.contains("unknown issuer") && unknown.message.contains("ssl-ca"));
    assert!(cert(rustls::CertificateError::NotValidForName)
        .message
        .contains("name mismatch"));
    assert!(cert(rustls::CertificateError::Expired)
        .message
        .contains("expired"));
    let alert = std::io::Error::new(
        std::io::ErrorKind::InvalidData,
        rustls::Error::AlertReceived(rustls::AlertDescription::HandshakeFailure),
    );
    let handshake = connect_error(&Error::Io(IoError::Io(alert)), &params).unwrap();
    assert_eq!(handshake.code.as_deref(), Some("connect.tls_handshake"));
    let required = connect_error(
        &Error::Driver(DriverError::NoClientSslFlagFromServer),
        &params,
    )
    .unwrap();
    assert_eq!(required.code.as_deref(), Some("connect.tls_required"));
    let refused = connect_error(
        &Error::Server(mysql_async::ServerError {
            code: 3159,
            message: "Connections using insecure transport are prohibited".into(),
            state: "HY000".into(),
        }),
        &parse("mysql://h/d?ssl-mode=DISABLED").1,
    )
    .unwrap();
    assert_eq!(refused.code.as_deref(), Some("connect.tls_required"));
    assert!(refused.message.contains("require_secure_transport"));
    // Everything else is not a TLS story.
    assert!(connect_error(&Error::Other("boom".into()), &params).is_none());
}

#[test]
fn a_name_mismatch_is_retried_with_a_name_from_the_certificate() {
    let url = |query: &str| {
        let (cleaned, params) = parse(&format!("mysql://h/d?{query}"));
        let opts = Opts::from_url(&cleaned).unwrap();
        params.attempts(&opts).remove(0)
    };
    let mismatch = |presented: &[&str]| {
        let io = std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            rustls::Error::InvalidCertificate(rustls::CertificateError::NotValidForNameContext {
                expected: rustls::pki_types::ServerName::try_from("127.0.0.1")
                    .unwrap()
                    .to_owned(),
                presented: presented.iter().map(|name| (*name).to_owned()).collect(),
            }),
        );
        Error::Io(IoError::Io(io))
    };
    let dir = tempfile::tempdir().unwrap();
    let ca = dir.path().join("ca.pem");
    std::fs::write(&ca, "x").unwrap();
    // The chain is checked, the name is not (VERIFY_CA, or REQUIRED with a CA): retry.
    let verify_ca = url(&format!("ssl-mode=VERIFY_CA&ssl-ca={}", ca.display()));
    assert_eq!(
        name_to_accept(
            &verify_ca,
            &mismatch(&["DnsName(\"localhost\")", "DnsName(\"db.example\")"])
        )
        .as_deref(),
        Some("localhost")
    );
    assert_eq!(
        name_to_accept(&verify_ca, &mismatch(&["DnsName(\"*.example.com\")"])).as_deref(),
        Some("host.example.com")
    );
    assert_eq!(
        name_to_accept(
            &verify_ca,
            &mismatch(&["IpAddress(10.0.0.5)", "DnsName(\"db.example\")"])
        )
        .as_deref(),
        Some("10.0.0.5")
    );
    assert_eq!(
        name_to_accept(
            &verify_ca,
            &mismatch(&["DirectoryName", "Unsupported(0x01)"])
        ),
        None
    );
    assert_eq!(
        name_to_accept(&verify_ca, &mismatch(&[])),
        None,
        "a certificate without names keeps failing"
    );
    let required_ca = url(&format!("ssl-mode=REQUIRED&ssl-ca={}", ca.display()));
    assert!(name_to_accept(&required_ca, &mismatch(&["DnsName(\"localhost\")"])).is_some());
    // The name is part of the check (VERIFY_IDENTITY), the chain is not (REQUIRED without CA),
    // or the caller named the server: never retried.
    assert_eq!(
        name_to_accept(
            &url("ssl-mode=VERIFY_IDENTITY"),
            &mismatch(&["DnsName(\"localhost\")"])
        ),
        None
    );
    assert_eq!(
        name_to_accept(
            &url("ssl-mode=REQUIRED"),
            &mismatch(&["DnsName(\"localhost\")"])
        ),
        None
    );
    // N8: under VERIFY_CA a `tls-server-name` (what the SSH tunnel adds) is the SNI / socket
    // hint, not a name requirement: the mismatch is still retried with a certificate name.
    assert_eq!(
        name_to_accept(
            &url(&format!(
                "ssl-mode=VERIFY_CA&ssl-ca={}&tls-server-name=a.example",
                ca.display()
            )),
            &mismatch(&["DnsName(\"localhost\")"])
        )
        .as_deref(),
        Some("localhost")
    );
    // ... but never under VERIFY_IDENTITY, where the name is the requirement.
    assert_eq!(
        name_to_accept(
            &url("ssl-mode=VERIFY_IDENTITY&tls-server-name=a.example"),
            &mismatch(&["DnsName(\"localhost\")"])
        ),
        None
    );
    assert_eq!(
        name_to_accept(
            &url("ssl-mode=DISABLED"),
            &mismatch(&["DnsName(\"localhost\")"])
        ),
        None
    );
    // Other failures are not retried.
    assert_eq!(
        name_to_accept(&verify_ca, &Error::Other("boom".into())),
        None
    );
}

// ---- hardening ---------------------------------------------------------------------------------------

#[test]
fn verify_ca_needs_a_ca_file() {
    // Public roots + an unchecked name would authenticate nothing (B1).
    for query in [
        "ssl-mode=VERIFY_CA",
        "ssl-mode=VERIFY_CA&ssl-ca=",
        "sslmode=verify-ca",
    ] {
        let error = TlsParams::from_url(&format!("mysql://h/d?{query}")).unwrap_err();
        assert_eq!(error, TlsConfigError::VerifyCaNeedsCaFile, "{query}");
        let message = error.to_string();
        assert!(
            message.contains("ssl-ca=<CA file>") && message.contains("VERIFY_IDENTITY"),
            "{message}"
        );
        let backend: engine_driver_kit::BackendError = error.into();
        assert_eq!(backend.code.as_deref(), Some("connect.tls_config"));
    }
    // VERIFY_IDENTITY may use the built-in and OS roots; VERIFY_CA with a CA file is fine.
    assert!(TlsParams::from_url("mysql://h/d?ssl-mode=VERIFY_IDENTITY").is_ok());
    let dir = tempfile::tempdir().unwrap();
    let ca = file(dir.path(), "ca.pem", b"x");
    assert!(TlsParams::from_url(&format!(
        "mysql://h/d?ssl-mode=VERIFY_CA&ssl-ca={}",
        ca.display()
    ))
    .is_ok());
}

#[test]
fn mysql_async_s_verify_identity_false_is_verify_ca_against_public_roots_and_is_refused() {
    let reconcile = |query: &str| {
        let (cleaned, mut params) = parse(&format!("mysql://h/d?{query}"));
        let opts = Opts::from_url(&cleaned).unwrap();
        params.reconcile(&opts)
    };
    assert_eq!(
        reconcile("require_ssl=true&verify_identity=false"),
        Err(TlsConfigError::VerifyCaNeedsCaFile)
    );
    // The documented strict default and the explicit accept-all keep working.
    assert_eq!(reconcile("require_ssl=true"), Ok(()));
    assert_eq!(reconcile("require_ssl=true&verify_ca=false"), Ok(()));
    assert_eq!(reconcile("require_ssl=false"), Ok(()));
    assert_eq!(reconcile(""), Ok(()));
}

#[test]
fn a_percent_encoded_native_option_name_does_not_turn_strict_into_accept_all() {
    // `require%5Fssl=true` is `require_ssl=true` for mysql_async (and the url crate).
    let (cleaned, mut params) = parse("mysql://h/d?require%5Fssl=true&verify%5Fidentity=true");
    assert_eq!(params.mode, Mode::Native);
    let opts = Opts::from_url(&cleaned).unwrap();
    assert!(opts.ssl_opts().is_some(), "mysql_async saw require_ssl");
    params.reconcile(&opts).unwrap();
    let attempts = params.attempts(&opts);
    assert_eq!(attempts.len(), 1);
    let ssl = attempts[0].ssl_opts().expect("TLS");
    assert!(
        !ssl.accept_invalid_certs() && !ssl.skip_domain_validation(),
        "mysql_async's strict options survive"
    );
    // Even if this reader had missed the name, the safety net follows mysql_async.
    let mut missed = TlsParams {
        mode: Mode::Preferred,
        explicit_mode: false,
        ..params.clone()
    };
    missed.reconcile(&opts).unwrap();
    assert_eq!(missed.mode, Mode::Native);
    // And a parameter name with an encoded `ssl-mode` is read too.
    assert_eq!(
        parse("mysql://h/d?ssl%2Dmode=DISABLED").1.mode,
        Mode::Disabled
    );
}

#[test]
fn conflicting_duplicate_parameters_are_an_error() {
    for query in [
        "ssl-mode=REQUIRED&ssl-mode=DISABLED",
        "ssl-mode=REQUIRED&sslmode=disable",
        "ssl-mode=REQUIRED&ssl-mode=",
        "ssl-mode=&sslmode=disable",
        "tls-server-name=a.example&tls-server-name=b.example",
    ] {
        assert!(
            matches!(
                TlsParams::from_url(&format!("mysql://h/d?{query}")),
                Err(TlsConfigError::ConflictingParameter(_))
            ),
            "{query}"
        );
    }
    assert!(TlsParams::from_url("mysql://h/d?ssl-mode=REQUIRED&sslmode=REQUIRED").is_ok());
    assert!(TlsParams::from_url("mysql://h/d?ssl-mode=REQUIRED&ssl-mode=REQUIRED").is_ok());
}

#[test]
fn an_empty_ca_file_is_refused_and_a_der_one_is_accepted() {
    let dir = tempfile::tempdir().unwrap();
    let empty = file(dir.path(), "empty.pem", b"");
    let error = TlsParams::from_url(&format!(
        "mysql://h/d?ssl-mode=REQUIRED&ssl-ca={}",
        empty.display()
    ))
    .unwrap_err();
    assert!(
        matches!(error, TlsConfigError::EmptyFile { what: "ssl-ca", .. }),
        "{error}"
    );
    // DER is binary (not UTF-8): mysql_async reads it, so the file check must not choke on it.
    let der = file(dir.path(), "ca.der", &[0x30, 0x82, 0xff, 0xfe, 0x00, 0x80]);
    assert!(TlsParams::from_url(&format!(
        "mysql://h/d?ssl-mode=REQUIRED&ssl-ca={}",
        der.display()
    ))
    .is_ok());
}

#[cfg(unix)]
#[test]
fn a_client_key_others_can_read_is_refused_like_libpq() {
    use std::os::unix::fs::PermissionsExt;
    let dir = tempfile::tempdir().unwrap();
    let cert = file(dir.path(), "c.pem", b"x");
    let key = file(
        dir.path(),
        "k.pem",
        b"-----BEGIN PRIVATE KEY-----\nTOPSECRET\n-----END PRIVATE KEY-----\n",
    );
    let url = format!(
        "mysql://h/d?ssl-mode=REQUIRED&ssl-cert={}&ssl-key={}",
        cert.display(),
        key.display()
    );
    for (mode, refused) in [
        (0o644, true),
        (0o640, true),
        (0o604, true),
        (0o600, false),
        (0o400, false),
    ] {
        std::fs::set_permissions(&key, std::fs::Permissions::from_mode(mode)).unwrap();
        let outcome = TlsParams::from_url(&url);
        if refused {
            let error = outcome.unwrap_err();
            assert!(
                matches!(error, TlsConfigError::KeyFilePermissions { .. }),
                "{mode:o}"
            );
            let message = error.to_string();
            assert!(
                message.contains("k.pem") && message.contains(&format!("{mode:04o}")),
                "{message}"
            );
            assert!(!message.contains("TOPSECRET"));
        } else {
            assert!(outcome.is_ok(), "{mode:o}");
        }
    }
    assert!(
        key_mode_allowed(true, 0o640)
            && !key_mode_allowed(true, 0o660)
            && !key_mode_allowed(true, 0o604)
    );
    assert!(!key_mode_allowed(false, 0o640));
}

#[test]
fn one_unparsable_system_root_does_not_break_the_others() {
    let ca = pki::ca("A real CA");
    let usable = usable_roots(vec![
        rustls::pki_types::CertificateDer::from(vec![1, 2, 3]),
        ca.der.clone(),
        rustls::pki_types::CertificateDer::from(Vec::new()),
    ]);
    assert_eq!(usable, vec![ca.der.as_ref().to_vec()]);
    // What is left is exactly what mysql_async adds strictly.
    let mut store = RootCertStore::empty();
    for der in usable {
        store
            .add(rustls::pki_types::CertificateDer::from(der))
            .unwrap();
    }
    assert_eq!(store.len(), 1);
    // The cached OS list is already filtered.
    let mut check = RootCertStore::empty();
    for der in system_roots() {
        check
            .add(rustls::pki_types::CertificateDer::from(der.clone()))
            .unwrap();
    }
}

#[test]
fn allow_retries_only_when_clear_text_was_refused_for_lack_of_tls() {
    let allow = parse("mysql://h/d?sslmode=allow").1;
    let server = |code: u16, message: &str| {
        Error::Server(mysql_async::ServerError {
            code,
            message: message.into(),
            state: "HY000".into(),
        })
    };
    assert!(allow.falls_through(&server(
        3159,
        "Connections using insecure transport are prohibited while --require_secure_transport=ON."
    )));
    assert!(!allow.falls_through(&server(
        1045,
        "Access denied for user 'u'@'h' (using password: YES)"
    )));
    assert!(!allow.falls_through(&Error::Other("boom".into())));
    // By the error code, never by the message: a localized 3159 retries, a look-alike does not.
    assert!(allow.falls_through(&server(
        3159,
        "Verbindungen ueber unsicheren Transport sind verboten"
    )));
    assert!(!allow.falls_through(&server(
        1045,
        "Connections using insecure transport are prohibited"
    )));
}

#[test]
fn the_ring_provider_is_installed_once_and_tolerates_an_existing_default() {
    ensure_provider();
    ensure_provider();
    assert!(rustls::crypto::CryptoProvider::get_default().is_some());
}

// ---- certificates and a throwaway MySQL-protocol listener ---------------------------------------------

mod pki {
    use rcgen::{
        date_time_ymd, BasicConstraints, CertificateParams, DnType, ExtendedKeyUsagePurpose, IsCa,
        Issuer, KeyPair, KeyUsagePurpose,
    };

    pub struct Authority {
        pub params: CertificateParams,
        pub key: KeyPair,
        pub der: rustls::pki_types::CertificateDer<'static>,
        pub pem: String,
    }

    pub fn ca(name: &str) -> Authority {
        let mut params = CertificateParams::new(Vec::<String>::new()).unwrap();
        params.distinguished_name.push(DnType::CommonName, name);
        params.is_ca = IsCa::Ca(BasicConstraints::Unconstrained);
        params.key_usages = vec![KeyUsagePurpose::KeyCertSign, KeyUsagePurpose::CrlSign];
        let key = KeyPair::generate().unwrap();
        let certificate = params.self_signed(&key).unwrap();
        Authority {
            der: certificate.der().clone(),
            pem: certificate.pem(),
            params,
            key,
        }
    }

    #[derive(Clone, Copy)]
    pub enum Flaw {
        None,
        Expired,
        NotYetValid,
        /// `clientAuth` only: not a server certificate.
        ClientAuthOnly,
        /// Signed by nobody the client trusts: its own issuer.
        SelfSigned,
    }

    /// A leaf for `names` under `issuer`, with `flaw`.
    pub fn leaf(
        issuer: &Authority,
        names: &[&str],
        flaw: Flaw,
    ) -> (rustls::pki_types::CertificateDer<'static>, KeyPair) {
        let mut params = CertificateParams::new(
            names
                .iter()
                .map(|name| (*name).to_owned())
                .collect::<Vec<_>>(),
        )
        .unwrap();
        params.extended_key_usages = vec![ExtendedKeyUsagePurpose::ServerAuth];
        match flaw {
            Flaw::Expired => {
                params.not_before = date_time_ymd(2000, 1, 1);
                params.not_after = date_time_ymd(2001, 1, 1);
            }
            Flaw::NotYetValid => {
                params.not_before = date_time_ymd(2998, 1, 1);
                params.not_after = date_time_ymd(2999, 1, 1);
            }
            Flaw::ClientAuthOnly => {
                params.extended_key_usages = vec![ExtendedKeyUsagePurpose::ClientAuth]
            }
            Flaw::None | Flaw::SelfSigned => {}
        }
        let key = KeyPair::generate().unwrap();
        let certificate = match flaw {
            Flaw::SelfSigned => params.self_signed(&key).unwrap(),
            _ => params
                .signed_by(&key, &Issuer::from_params(&issuer.params, &issuer.key))
                .unwrap(),
        };
        (certificate.der().clone(), key)
    }
}

mod fake {
    use std::sync::{Arc, Mutex};
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio::net::TcpListener;
    use tokio_rustls::TlsAcceptor;

    pub fn acceptor(
        leaf: rustls::pki_types::CertificateDer<'static>,
        key: &rcgen::KeyPair,
    ) -> TlsAcceptor {
        let provider = Arc::new(rustls::crypto::ring::default_provider());
        let config = rustls::ServerConfig::builder_with_provider(provider)
            .with_safe_default_protocol_versions()
            .unwrap()
            .with_no_client_auth()
            .with_single_cert(
                vec![leaf],
                rustls::pki_types::PrivatePkcs8KeyDer::from(key.serialize_der()).into(),
            )
            .unwrap();
        TlsAcceptor::from(Arc::new(config))
    }

    /// The initial handshake packet of a MySQL 8 server; `ssl` sets CLIENT_SSL.
    fn greeting(ssl: bool) -> Vec<u8> {
        let capabilities: u32 =
            0x0001 | 0x0200 | 0x8000 | 0x0008_0000 | if ssl { 0x0800 } else { 0 };
        let mut payload = vec![0x0a];
        payload.extend_from_slice(b"8.0.99\0");
        payload.extend_from_slice(&1u32.to_le_bytes());
        payload.extend_from_slice(b"abcdefgh");
        payload.push(0);
        payload.extend_from_slice(&(capabilities as u16).to_le_bytes());
        payload.push(0x21);
        payload.extend_from_slice(&2u16.to_le_bytes());
        payload.extend_from_slice(&((capabilities >> 16) as u16).to_le_bytes());
        payload.push(21);
        payload.extend_from_slice(&[0; 10]);
        payload.extend_from_slice(b"ijklmnopqrst\0");
        payload.extend_from_slice(b"mysql_native_password\0");
        let mut packet = (payload.len() as u32).to_le_bytes()[..3].to_vec();
        packet.push(0);
        packet.extend_from_slice(&payload);
        packet
    }

    /// What the client did on one connection.
    #[derive(Debug, Clone, PartialEq, Eq)]
    pub enum Seen {
        /// The 32-byte SSLRequest, then a TLS handshake that `accepted` or not.
        SslRequest { accepted: bool },
        /// A handshake response in clear text (the client did not ask for TLS).
        ClearText,
    }

    /// Serves `connections` connections; `acceptor: None` advertises no TLS.
    pub async fn start(
        acceptor: Option<TlsAcceptor>,
        connections: usize,
    ) -> (u16, Arc<Mutex<Vec<Seen>>>) {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        let seen = Arc::new(Mutex::new(Vec::new()));
        let log = Arc::clone(&seen);
        tokio::spawn(async move {
            for _ in 0..connections {
                let Ok((mut stream, _)) = listener.accept().await else {
                    return;
                };
                let acceptor = acceptor.clone();
                let log = Arc::clone(&log);
                tokio::spawn(async move {
                    if stream
                        .write_all(&greeting(acceptor.is_some()))
                        .await
                        .is_err()
                    {
                        return;
                    }
                    let mut header = [0u8; 4];
                    if stream.read_exact(&mut header).await.is_err() {
                        return;
                    }
                    let length = u32::from_le_bytes([header[0], header[1], header[2], 0]) as usize;
                    let mut body = vec![0u8; length];
                    if stream.read_exact(&mut body).await.is_err() {
                        return;
                    }
                    match acceptor {
                        // SSLRequest: sequence 1, a 32-byte payload.
                        Some(acceptor) if length == 32 && header[3] == 1 => {
                            let accepted = match acceptor.accept(stream).await {
                                Ok(mut tls) => {
                                    // Past TLS the fake has nothing more to say.
                                    let mut sink = [0u8; 64];
                                    let _ = tls.read(&mut sink).await;
                                    true
                                }
                                Err(_) => false,
                            };
                            log.lock().unwrap().push(Seen::SslRequest { accepted });
                        }
                        _ => log.lock().unwrap().push(Seen::ClearText),
                    }
                });
            }
        });
        (port, seen)
    }
}

fn runtime() -> tokio::runtime::Runtime {
    tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .enable_all()
        .build()
        .unwrap()
}

/// `Ok(())`: connected. `Err(Some(error))`: refused with a TLS code. `Err(None)`: failed for another reason
/// (past the TLS handshake the fake just hangs up).
fn connect_fake(
    runtime: &tokio::runtime::Runtime,
    url: &str,
) -> Result<(), Option<engine_driver_kit::BackendError>> {
    let (cleaned, mut params) = TlsParams::from_url(url).unwrap();
    let opts = Opts::from_url(&cleaned).unwrap();
    params.reconcile(&opts).unwrap();
    let outcome = runtime
        .block_on(async {
            tokio::time::timeout(std::time::Duration::from_secs(10), params.open(&opts)).await
        })
        .expect("timed out");
    match outcome {
        Ok(_) => Ok(()),
        Err(error) => Err(connect_error(&error, &params)),
    }
}

fn code(outcome: &Result<(), Option<engine_driver_kit::BackendError>>) -> Option<String> {
    match outcome {
        Err(Some(error)) => error.code.as_deref().map(str::to_owned),
        _ => None,
    }
}

fn fake_url(port: u16, query: &str, ca: &Path) -> String {
    format!(
        "mysql://u:pw@127.0.0.1:{port}/d?{}",
        query.replace("{ca}", &ca.display().to_string())
    )
}

type Leaf = (rustls::pki_types::CertificateDer<'static>, rcgen::KeyPair);

/// One connect attempt against a fake server presenting `leaf` (`None`: no TLS offered); `ca` is the
/// CA file handed to the driver. Returns the outcome and what the fake saw.
fn against(
    ca: &pki::Authority,
    leaf: Option<Leaf>,
    query: &str,
    connections: usize,
) -> (
    Result<(), Option<engine_driver_kit::BackendError>>,
    Vec<fake::Seen>,
) {
    let runtime = runtime();
    let dir = tempfile::tempdir().unwrap();
    let ca_file = file(dir.path(), "ca.pem", ca.pem.as_bytes());
    let (port, seen) = runtime.block_on(fake::start(
        leaf.map(|(der, key)| fake::acceptor(der, &key)),
        connections,
    ));
    let outcome = connect_fake(&runtime, &fake_url(port, query, &ca_file));
    std::thread::sleep(std::time::Duration::from_millis(300));
    let seen = seen.lock().unwrap().clone();
    (outcome, seen)
}

const REFUSED: [fake::Seen; 1] = [fake::Seen::SslRequest { accepted: false }];

#[test]
fn the_server_certificate_must_be_valid_trusted_and_meant_for_servers() {
    use pki::Flaw;
    let ca = pki::ca("Test CA");
    let rogue = pki::ca("Rogue CA");
    for (flaw, reason) in [
        (Flaw::Expired, "expired"),
        (Flaw::NotYetValid, "not yet valid"),
        (Flaw::ClientAuthOnly, "invalid certificate"),
        (Flaw::SelfSigned, "unknown issuer"),
    ] {
        for query in [
            "ssl-mode=VERIFY_IDENTITY&ssl-ca={ca}",
            "ssl-mode=VERIFY_CA&ssl-ca={ca}",
            "ssl-mode=REQUIRED&ssl-ca={ca}",
        ] {
            let (outcome, seen) =
                against(&ca, Some(pki::leaf(&ca, &["127.0.0.1"], flaw)), query, 1);
            let error = outcome
                .err()
                .flatten()
                .unwrap_or_else(|| panic!("{reason} accepted under {query}"));
            assert_eq!(
                error.code.as_deref(),
                Some("connect.tls_certificate"),
                "{reason} {query}"
            );
            assert!(
                error.message.contains(reason),
                "{reason} {query}: {}",
                error.message
            );
            assert_eq!(
                seen, REFUSED,
                "{reason} {query}: the handshake must have been refused"
            );
        }
    }
    // Signed by a CA the client was not given (wrong name too): the chain is what fails.
    let (outcome, seen) = against(
        &ca,
        Some(pki::leaf(&rogue, &["evil.example"], Flaw::None)),
        "ssl-mode=VERIFY_CA&ssl-ca={ca}",
        1,
    );
    let error = outcome.err().flatten().expect("refused");
    assert!(
        error.message.contains("unknown issuer"),
        "{}",
        error.message
    );
    assert_eq!(seen, REFUSED);
    // A good certificate gets past TLS (then the fake hangs up: not a TLS failure).
    let (outcome, seen) = against(
        &ca,
        Some(pki::leaf(&ca, &["127.0.0.1"], Flaw::None)),
        "ssl-mode=VERIFY_IDENTITY&ssl-ca={ca}",
        1,
    );
    assert_eq!(outcome.err(), Some(None));
    assert_eq!(seen, [fake::Seen::SslRequest { accepted: true }]);
}

#[test]
fn verify_ca_ignores_the_name_and_verify_identity_does_not() {
    use pki::Flaw;
    let ca = pki::ca("Test CA");
    let wrong_name = || Some(pki::leaf(&ca, &["db.example"], Flaw::None));
    let past_tls = [
        fake::Seen::SslRequest { accepted: false },
        fake::Seen::SslRequest { accepted: true },
    ];
    // VERIFY_CA: the first handshake fails on the name, the retry uses a name from the certificate.
    let (outcome, seen) = against(&ca, wrong_name(), "ssl-mode=VERIFY_CA&ssl-ca={ca}", 2);
    assert_eq!(outcome.err(), Some(None));
    assert_eq!(seen, past_tls);
    // N8: the SSH tunnel's `tls-server-name` is a hint under VERIFY_CA, not a requirement.
    let (outcome, seen) = against(
        &ca,
        wrong_name(),
        "ssl-mode=VERIFY_CA&ssl-ca={ca}&tls-server-name=wrong.example",
        2,
    );
    assert_eq!(outcome.err(), Some(None));
    assert_eq!(seen, past_tls);
    // VERIFY_IDENTITY: a name mismatch is refused, with or without an override ...
    for query in [
        "ssl-mode=VERIFY_IDENTITY&ssl-ca={ca}",
        "ssl-mode=VERIFY_IDENTITY&ssl-ca={ca}&tls-server-name=wrong.example",
    ] {
        let (outcome, seen) = against(&ca, wrong_name(), query, 1);
        let error = outcome.err().flatten().expect("refused");
        assert!(
            error.message.contains("name mismatch"),
            "{query}: {}",
            error.message
        );
        assert_eq!(seen, REFUSED, "{query}");
    }
    // ... and the right override passes (the tunnel case: socket 127.0.0.1, name db.example).
    let (outcome, seen) = against(
        &ca,
        wrong_name(),
        "ssl-mode=VERIFY_IDENTITY&ssl-ca={ca}&tls-server-name=db.example",
        1,
    );
    assert_eq!(outcome.err(), Some(None));
    assert_eq!(seen, [fake::Seen::SslRequest { accepted: true }]);
}

#[test]
fn preferred_falls_back_only_when_the_server_offers_no_tls() {
    let ca = pki::ca("Test CA");
    // No CLIENT_SSL in the greeting: PREFERRED retries in clear text, REQUIRED stops.
    let (outcome, seen) = against(&ca, None, "ssl-mode=PREFERRED", 2);
    assert_eq!(outcome.err(), Some(None));
    assert_eq!(seen, [fake::Seen::ClearText]);
    let (outcome, seen) = against(&ca, None, "ssl-mode=REQUIRED", 1);
    assert_eq!(code(&outcome).as_deref(), Some("connect.tls_required"));
    assert!(
        seen.is_empty(),
        "nothing is sent to a server that offers no TLS: {seen:?}"
    );
    // A TLS-capable server whose certificate is refused is an error, never a clear-text retry.
    let (outcome, seen) = against(
        &ca,
        Some(pki::leaf(&ca, &["127.0.0.1"], pki::Flaw::Expired)),
        "ssl-mode=PREFERRED&ssl-ca={ca}",
        2,
    );
    assert_eq!(code(&outcome).as_deref(), Some("connect.tls_certificate"));
    assert_eq!(seen, REFUSED);
}

#[test]
fn socket_with_a_tls_mode_is_refused_or_plain_by_rule() {
    let outcome = |query: &str| {
        let (cleaned, mut params) = parse(&format!("mysql://h/d?socket=%2Ftmp%2Fm.sock&{query}"));
        let opts = Opts::from_url(&cleaned).unwrap();
        assert_eq!(opts.socket(), Some("/tmp/m.sock"), "{query}");
        let verdict = params.reconcile(&opts);
        let attempts = params.attempts(&opts);
        (verdict, attempts)
    };
    let dir = tempfile::tempdir().unwrap();
    let ca = file(dir.path(), "ca.pem", b"x");
    for query in [
        "ssl-mode=REQUIRED".to_owned(),
        format!("ssl-mode=VERIFY_CA&ssl-ca={}", ca.display()),
        "ssl-mode=VERIFY_IDENTITY".to_owned(),
        "require_ssl=true".to_owned(),
    ] {
        let (verdict, _) = outcome(&query);
        let error = verdict.expect_err(&query);
        assert!(matches!(error, TlsConfigError::TlsOnSocket(_)), "{query}");
        assert!(
            error
                .to_string()
                .contains("use ssl-mode=DISABLED or connect over TCP"),
            "{error}"
        );
        let backend: engine_driver_kit::BackendError = error.into();
        assert_eq!(backend.code.as_deref(), Some("connect.tls_config"));
    }
    // PREFERRED (the default), allow and DISABLED: one plain attempt, no TLS tried.
    for query in [
        "ssl-mode=PREFERRED",
        "sslmode=allow",
        "ssl-mode=DISABLED",
        "require_ssl=false",
    ] {
        let (verdict, attempts) = outcome(query);
        assert_eq!(verdict, Ok(()), "{query}");
        assert_eq!(attempts.len(), 1, "{query}");
        assert!(
            attempts[0].ssl_opts().is_none(),
            "{query}: no TLS attempt on a socket"
        );
    }
    // Without a socket the same modes keep their TLS attempts.
    let (cleaned, mut params) = parse("mysql://h/d?ssl-mode=REQUIRED");
    let opts = Opts::from_url(&cleaned).unwrap();
    assert_eq!(params.reconcile(&opts), Ok(()));
}

#[test]
fn the_client_key_is_read_once_and_shared_by_every_connection() {
    let dir = tempfile::tempdir().unwrap();
    let cert = file(dir.path(), "c.pem", b"CERT-BYTES");
    let key = file(
        dir.path(),
        "k.pem",
        b"-----BEGIN PRIVATE KEY-----\nKEY-BYTES\n-----END PRIVATE KEY-----\n",
    );
    private(&key);
    let (cleaned, params) = parse(&format!(
        "mysql://h/d?ssl-mode=REQUIRED&ssl-cert={}&ssl-key={}",
        cert.display(),
        key.display()
    ));
    // Later changes to the files (or their permissions) do not reach the connections.
    std::fs::write(&key, "replaced").unwrap();
    std::fs::remove_file(&cert).unwrap();
    let opts = Opts::from_url(&cleaned).unwrap();
    let attempts = params.attempts(&opts);
    let identity = attempts[0]
        .ssl_opts()
        .unwrap()
        .client_identity()
        .expect("identity");
    // `read()` of a buffer needs no file: the material is in memory.
    let (cert_bytes, key_bytes) = runtime().block_on(async {
        (
            identity.cert_chain().read().await.unwrap().into_owned(),
            identity.priv_key().read().await.unwrap().into_owned(),
        )
    });
    assert_eq!(cert_bytes, b"CERT-BYTES");
    assert!(String::from_utf8(key_bytes).unwrap().contains("KEY-BYTES"));
    // ... and the key never shows up in Debug output.
    assert!(!format!("{params:?}").contains("KEY-BYTES"));
}

#[test]
fn a_kill_connection_fails_when_client_ssl_is_stripped_after_a_tls_session() {
    let ca = pki::ca("Test CA");
    let runtime = runtime();
    // A server that no longer advertises CLIENT_SSL (someone in the middle stripped it).
    let (port, seen) = runtime.block_on(fake::start(None, 2));
    let (cleaned, mut params) = TlsParams::from_url(&format!(
        "mysql://u:pw@127.0.0.1:{port}/d?ssl-mode=PREFERRED"
    ))
    .unwrap();
    let opts = Opts::from_url(&cleaned).unwrap();
    params.reconcile(&opts).unwrap();
    // After a TLS session `open` hands back the TLS attempt itself, and the canceller and
    // the catalog connection are built from it: there is no clear-text attempt behind it.
    let worked = params.attempts(&opts).remove(0);
    assert!(worked.ssl_opts().is_some());
    let kill_opts: Opts = mysql_async::OptsBuilder::from_opts(worked)
        .db_name(None::<String>)
        .into();
    let error = match runtime.block_on(async { mysql_async::Conn::new(kill_opts).await }) {
        Err(error) => error,
        Ok(_) => panic!("the KILL connection must not fall back to clear text"),
    };
    assert!(
        matches!(error, Error::Driver(DriverError::NoClientSslFlagFromServer)),
        "{error}"
    );
    assert!(!params.falls_through(&error) || params.mode == Mode::Preferred);
    std::thread::sleep(std::time::Duration::from_millis(200));
    assert!(
        seen.lock().unwrap().is_empty(),
        "nothing was sent in clear text: {:?}",
        seen.lock().unwrap()
    );
    drop(ca);
}
