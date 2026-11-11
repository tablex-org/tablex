//! Tests of `tls.rs`: offline unit tests and env-gated live tests.

use super::*;

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

fn parse(uri: &str) -> (String, TlsSettings) {
    TlsSettings::from_uri(uri).unwrap()
}

#[test]
fn prefer_is_the_default_and_a_plain_uri_is_untouched() {
    let (uri, settings) = parse("postgres://u:p@h:5432/db");
    assert_eq!(uri, "postgres://u:p@h:5432/db");
    assert_eq!(settings.mode, SslMode::Prefer);
    assert_eq!(settings.roots, None);
    assert_eq!(settings.client, None);
    assert_eq!(settings.verified(), Verified::None);
}

#[test]
fn every_sslmode_parses_and_an_unknown_one_is_typed() {
    for (value, mode) in [
        ("disable", SslMode::Disable),
        ("allow", SslMode::Allow),
        ("prefer", SslMode::Prefer),
        ("require", SslMode::Require),
        ("verify-ca", SslMode::VerifyCa),
        ("verify-full", SslMode::VerifyFull),
    ] {
        assert_eq!(
            parse(&format!(
                "postgres://h/d?sslmode={value}&sslrootcert=/ca.pem"
            ))
            .1
            .mode,
            mode
        );
    }
    assert_eq!(
        TlsSettings::from_uri("postgres://h/d?sslmode=verify").unwrap_err(),
        TlsConfigError::InvalidSslMode("verify".into())
    );
}

#[test]
fn tls_parameters_are_removed_and_other_parameters_survive_in_order() {
    let (uri, settings) = parse(
        "postgres://u@h/d?application_name=x&sslmode=verify-full&hostaddr=127.0.0.1\
         &sslrootcert=%2Ftmp%2Fca%20x.pem&sslcert=/c.pem&sslkey=/k.pem&sslpassword=secret&channel_binding=require",
    );
    assert_eq!(
        uri,
        "postgres://u@h/d?application_name=x&hostaddr=127.0.0.1&channel_binding=require"
    );
    assert_eq!(settings.mode, SslMode::VerifyFull);
    assert_eq!(
        settings.roots,
        Some(RootCerts::File("/tmp/ca x.pem".into()))
    );
    assert_eq!(settings.client, Some(("/c.pem".into(), "/k.pem".into())));
    assert_eq!(settings.verified(), Verified::Full);
    // The cleaned URI is what tokio-postgres parses, `hostaddr` included.
    let config: Config = uri.parse().unwrap();
    assert_eq!(config.get_hostaddrs().len(), 1);
}

#[test]
fn what_the_handshake_proves_follows_libpq() {
    let verified = |query: &str| parse(&format!("postgres://h/d?{query}")).1.verified();
    assert_eq!(verified("sslmode=require"), Verified::None);
    assert_eq!(verified("sslmode=prefer"), Verified::None);
    // A root file turns require/prefer into a chain check, never a name check.
    assert_eq!(
        verified("sslmode=require&sslrootcert=/ca.pem"),
        Verified::Ca
    );
    assert_eq!(
        verified("sslmode=verify-ca&sslrootcert=/ca.pem"),
        Verified::Ca
    );
    assert_eq!(verified("sslmode=verify-full"), Verified::Full);
    assert_eq!(
        verified("sslmode=verify-full&sslrootcert=system"),
        Verified::Full
    );
}

#[test]
fn system_roots_are_for_verify_full_only() {
    for mode in ["prefer", "require", "allow"] {
        assert!(matches!(
            TlsSettings::from_uri(&format!("postgres://h/d?sslmode={mode}&sslrootcert=system")),
            Err(TlsConfigError::SystemRootsNeedVerifyFull(_))
        ));
    }
    assert!(TlsSettings::from_uri("postgres://h/d?sslmode=verify-full&sslrootcert=system").is_ok());
    assert!(TlsSettings::from_uri("postgres://h/d?sslmode=verify-full").is_ok());
}

#[test]
fn verify_ca_needs_a_ca_file() {
    // Public roots + an unchecked name would authenticate nothing (B1).
    for query in [
        "sslmode=verify-ca",
        "sslmode=verify-ca&sslrootcert=",
        "sslmode=verify-ca&sslrootcert=system",
    ] {
        let error = TlsSettings::from_uri(&format!("postgres://h/d?{query}")).unwrap_err();
        assert_eq!(error, TlsConfigError::VerifyCaNeedsCaFile, "{query}");
        let message = error.to_string();
        assert!(
            message.contains("sslrootcert=<CA file>") && message.contains("verify-full"),
            "{message}"
        );
        // The error reaches the host as connect.tls_config before any network use.
        let backend: BackendError = Transport::prepare(&format!("postgres://h/d?{query}"))
            .err()
            .expect("refused")
            .into();
        assert_eq!(backend.code.as_deref(), Some("connect.tls_config"));
    }
    assert!(TlsSettings::from_uri("postgres://h/d?sslmode=verify-ca&sslrootcert=/ca.pem").is_ok());
}

#[test]
fn a_client_certificate_needs_its_key_and_the_other_way_round() {
    for query in ["sslcert=/c.pem", "sslkey=/k.pem"] {
        assert_eq!(
            TlsSettings::from_uri(&format!("postgres://h/d?{query}")).unwrap_err(),
            TlsConfigError::IncompleteClientIdentity
        );
    }
}

#[test]
fn empty_values_are_unset_like_in_libpq() {
    let (uri, settings) = parse("postgres://h/d?sslmode=&sslrootcert=&sslcert=&sslkey=");
    assert_eq!(uri, "postgres://h/d");
    assert_eq!(settings.mode, SslMode::Prefer);
    assert_eq!(settings.roots, None);
    assert_eq!(settings.client, None);
}

#[test]
fn unreadable_and_empty_files_are_typed_and_leak_nothing() {
    let dir = tempfile::tempdir().unwrap();
    let missing = dir.path().join("nope.pem");
    let uri = format!(
        "postgres://u:hunter2@h/d?sslmode=verify-full&sslrootcert={}",
        missing.display()
    );
    let message = match Transport::prepare(&uri) {
        Err(PrepareError::Tls(error @ TlsConfigError::File { .. })) => error.to_string(),
        other => panic!("expected a file error, got {:?}", other.err()),
    };
    assert!(message.contains("sslrootcert") && message.contains("nope.pem"));
    assert!(!message.contains("hunter2"));

    let garbage = dir.path().join("garbage.pem");
    std::fs::write(&garbage, "not a certificate").unwrap();
    let uri = format!(
        "postgres://h/d?sslmode=verify-full&sslrootcert={}",
        garbage.display()
    );
    assert!(matches!(
        Transport::prepare(&uri),
        Err(PrepareError::Tls(TlsConfigError::NoCertificates { .. }))
    ));
}

#[test]
fn an_encrypted_client_key_is_refused_without_echoing_it() {
    let dir = tempfile::tempdir().unwrap();
    let cert = dir.path().join("c.pem");
    let key = dir.path().join("k.pem");
    // Structure only: the loader refuses the key before parsing any DER.
    std::fs::write(
        &cert,
        "-----BEGIN CERTIFICATE-----\nAAAA\n-----END CERTIFICATE-----\n",
    )
    .unwrap();
    std::fs::write(
        &key,
        "-----BEGIN ENCRYPTED PRIVATE KEY-----\nTOPSECRETKEYMATERIAL\n-----END ENCRYPTED PRIVATE KEY-----\n",
    )
    .unwrap();
    private(&key);
    let error = read_client_identity(&cert, &key).unwrap_err();
    assert!(matches!(error, TlsConfigError::EncryptedPrivateKey { .. }));
    let message = error.to_string();
    assert!(message.contains("password protected"));
    assert!(!message.contains("TOPSECRET"));
}

#[test]
fn a_tls_free_transport_is_plain() {
    let (config, transport) = Transport::prepare("postgres://u@h/d?sslmode=disable").unwrap();
    assert!(matches!(transport.route, Route::Plain));
    assert!(transport.info().is_none());
    assert_eq!(config.get_hosts().len(), 1);
}

#[cfg(unix)]
#[test]
fn unix_sockets_never_use_tls() {
    let (_, transport) =
        Transport::prepare("postgres://u@%2Fvar%2Frun%2Fpostgresql/d?sslmode=require").unwrap();
    assert!(matches!(transport.route, Route::Plain));
}

#[test]
fn modes_map_to_routes() {
    let route = |mode: &str| {
        Transport::prepare(&format!("postgres://u@h/d?sslmode={mode}"))
            .ok()
            .map(|(_, transport)| transport.route)
    };
    assert!(matches!(route("prefer"), Some(Route::Prefer(_))));
    assert!(matches!(route("require"), Some(Route::Require(_))));
    assert!(matches!(route("allow"), Some(Route::Allow(..))));
}

#[test]
fn certificate_failures_get_their_reason() {
    let rejected = |problem: CertificateError| {
        let io = io::Error::new(
            io::ErrorKind::InvalidData,
            rustls::Error::InvalidCertificate(problem),
        );
        TlsError::from_io(&io, "db.example")
    };
    for (problem, expected) in [
        (CertificateError::UnknownIssuer, CertProblem::UnknownIssuer),
        (CertificateError::Expired, CertProblem::Expired),
        (CertificateError::NotValidYet, CertProblem::NotYetValid),
        (CertificateError::NotValidForName, CertProblem::NameMismatch),
        (CertificateError::Revoked, CertProblem::Revoked),
    ] {
        let error = rejected(problem);
        assert_eq!(
            error,
            TlsError::Certificate {
                problem: expected,
                host: "db.example".into()
            }
        );
        let backend = tls_backend_error(&error, SslMode::VerifyFull);
        assert_eq!(backend.code.as_deref(), Some("connect.tls_certificate"));
    }
    let message = tls_backend_error(
        &rejected(CertificateError::UnknownIssuer),
        SslMode::VerifyCa,
    )
    .message;
    assert!(message.contains("unknown issuer") && message.contains("sslrootcert"));
    let alert = io::Error::new(
        io::ErrorKind::InvalidData,
        rustls::Error::AlertReceived(rustls::AlertDescription::HandshakeFailure),
    );
    let backend = tls_backend_error(&TlsError::from_io(&alert, "h"), SslMode::Require);
    assert_eq!(backend.code.as_deref(), Some("connect.tls_handshake"));
    let eof = io::Error::from(io::ErrorKind::UnexpectedEof);
    assert!(matches!(
        TlsError::from_io(&eof, "h"),
        TlsError::Handshake(_)
    ));
}

/// `SEQUENCE { SEQUENCE {tbs}, SEQUENCE { OID, NULL }, BIT STRING }`.
fn certificate_with_signature_oid(oid: &[u8]) -> Vec<u8> {
    let algorithm = [&[0x06, oid.len() as u8][..], oid, &[0x05, 0x00]].concat();
    let body = [
        &[0x30, 0x03, 0x02, 0x01, 0x01][..],
        &[&[0x30, algorithm.len() as u8][..], &algorithm].concat(),
        &[0x03, 0x02, 0x00, 0xff],
    ]
    .concat();
    [&[0x30, body.len() as u8][..], &body].concat()
}

#[test]
fn channel_binding_hashes_follow_the_signature_algorithm() {
    let rsa = |last: u8| vec![0x2a, 0x86, 0x48, 0x86, 0xf7, 0x0d, 0x01, 0x01, last];
    let ecdsa = |last: u8| vec![0x2a, 0x86, 0x48, 0xce, 0x3d, 0x04, 0x03, last];
    for (oid, length) in [
        (rsa(0x05), 32), // sha1WithRSA is upgraded to SHA-256 (RFC 5929 4.1)
        (rsa(0x0b), 32),
        (rsa(0x0c), 48),
        (rsa(0x0d), 64),
        (ecdsa(0x02), 32),
        (ecdsa(0x03), 48),
        (ecdsa(0x04), 64),
        (vec![0x2a, 0x86, 0x48, 0xce, 0x3d, 0x04, 0x01], 32),
    ] {
        let certificate = certificate_with_signature_oid(&oid);
        let hash = server_end_point_hash(&certificate).expect("hash");
        assert_eq!(hash.len(), length, "{oid:02x?}");
    }
    let certificate = certificate_with_signature_oid(&rsa(0x0b));
    assert_eq!(
        server_end_point_hash(&certificate).unwrap(),
        Sha256::digest(&certificate).to_vec()
    );
    // Ed25519 (1.3.101.112) and RSASSA-PSS have no end-point hash here.
    assert!(server_end_point_hash(&certificate_with_signature_oid(&[0x2b, 0x65, 0x70])).is_none());
    assert!(server_end_point_hash(&certificate_with_signature_oid(&rsa(0x0a))).is_none());
    assert!(server_end_point_hash(b"garbage").is_none());
    assert!(server_end_point_hash(&[]).is_none());
}

// ---- parser and configuration hardening ------------------------------------------------------------

#[test]
fn unix_host_with_hostaddr_is_not_plain() {
    // B2: tokio-postgres would dial `hostaddr` over TCP; the Unix host must not turn TLS off.
    for mode in ["verify-full&sslrootcert=/ca.pem", "require", "prefer"] {
        let uri = format!("postgres://u@%2Ftmp/db?hostaddr=10.0.0.5&sslmode={mode}");
        let error = Transport::prepare(&uri).err().expect("refused");
        assert!(
            matches!(
                error,
                PrepareError::Tls(TlsConfigError::UnixHostWithHostaddr)
            ),
            "{mode}: {error}"
        );
        let backend: BackendError = error.into();
        assert_eq!(backend.code.as_deref(), Some("connect.tls_config"));
    }
    // Explicit `disable` is the user's own clear text.
    assert!(Transport::prepare("postgres://u@%2Ftmp/db?hostaddr=10.0.0.5&sslmode=disable").is_ok());
}

#[cfg(unix)]
#[test]
fn a_unix_socket_without_hostaddr_stays_plain() {
    for mode in ["require", "prefer", "verify-full"] {
        let (_, transport) = Transport::prepare(&format!(
            "postgres://u@%2Fvar%2Frun%2Fpostgresql/d?sslmode={mode}"
        ))
        .unwrap();
        assert!(matches!(transport.route, Route::Plain), "{mode}");
    }
}

#[test]
fn hostaddr_needs_a_host_when_tls_is_on() {
    let error = Transport::prepare("postgres:///db?hostaddr=10.0.0.5&sslmode=require")
        .err()
        .expect("refused");
    assert!(matches!(
        error,
        PrepareError::Tls(TlsConfigError::HostaddrWithoutHost)
    ));
    assert!(error.to_string().contains("hostaddr needs host="));
    assert!(Transport::prepare("postgres://h/db?hostaddr=10.0.0.5&sslmode=require").is_ok());
}

#[test]
fn sslmode_seen_only_by_tokio_postgres_is_refused() {
    // The keyword/value form is parsed by tokio-postgres alone: this driver's reader sees no `?`.
    let error = Transport::prepare("host=h user=u sslmode=disable")
        .err()
        .expect("refused");
    assert!(
        matches!(
            error,
            PrepareError::Tls(TlsConfigError::UnrecognizedSslSetting)
        ),
        "{error}"
    );
    assert!(Transport::prepare("host=h user=u sslmode=require").is_err());
    // An unsupported negotiation is refused, never half-honoured.
    let error = Transport::prepare("postgres://h/d?sslnegotiation=direct&sslmode=require")
        .err()
        .expect("refused");
    assert!(matches!(
        error,
        PrepareError::Tls(TlsConfigError::UnsupportedSslNegotiation)
    ));
    assert!(Transport::prepare("postgres://h/d?sslnegotiation=postgres").is_ok());
}

#[test]
fn parameter_names_are_percent_decoded_and_credentials_are_split_like_tokio_postgres() {
    // `ssl%6Dode` is `sslmode` for tokio-postgres, so it must be one for us.
    let (uri, settings) = parse("postgres://h/d?ssl%6Dode=require&application_name=x");
    assert_eq!(settings.mode, SslMode::Require);
    assert_eq!(uri, "postgres://h/d?application_name=x");
    // tokio-postgres cuts the credentials at the first `@` of the whole string, so a `?`
    // inside the password is part of the password and the query starts after the host.
    let (uri, settings) = parse("postgres://u:p?w@h/d?sslmode=require");
    assert_eq!(settings.mode, SslMode::Require);
    assert_eq!(uri, "postgres://u:p?w@h/d");
    let config: Config = uri.parse().unwrap();
    assert_eq!(config.get_password(), Some(&b"p?w"[..]));
    assert_eq!(config.get_hosts().len(), 1);
}

#[test]
fn conflicting_duplicate_parameters_are_an_error() {
    for query in [
        "sslmode=verify-full&sslmode=disable",
        "sslmode=verify-full&sslmode=",
        "sslmode=&sslmode=disable",
        "sslrootcert=/a.pem&sslrootcert=/b.pem",
        "ssl%6Dode=require&sslmode=disable",
    ] {
        assert!(
            matches!(
                TlsSettings::from_uri(&format!("postgres://h/d?{query}")),
                Err(TlsConfigError::ConflictingParameter(_))
            ),
            "{query}"
        );
    }
    // The same value twice is not a conflict.
    assert_eq!(
        parse("postgres://h/d?sslmode=require&sslmode=require")
            .1
            .mode,
        SslMode::Require
    );
}

#[cfg(unix)]
#[test]
fn a_client_key_others_can_read_is_refused_like_libpq() {
    use std::os::unix::fs::PermissionsExt;
    let dir = tempfile::tempdir().unwrap();
    let cert = dir.path().join("c.pem");
    let key = dir.path().join("k.pem");
    std::fs::write(
        &cert,
        "-----BEGIN CERTIFICATE-----\nAAAA\n-----END CERTIFICATE-----\n",
    )
    .unwrap();
    std::fs::write(
        &key,
        "-----BEGIN PRIVATE KEY-----\nTOPSECRET\n-----END PRIVATE KEY-----\n",
    )
    .unwrap();
    for (mode, refused) in [
        (0o644, true),
        (0o640, true),
        (0o604, true),
        (0o600, false),
        (0o400, false),
    ] {
        std::fs::set_permissions(&key, std::fs::Permissions::from_mode(mode)).unwrap();
        let outcome = read_client_identity(&cert, &key);
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
            // Past the permission check the (fake) key is just not a key.
            assert!(
                matches!(outcome.unwrap_err(), TlsConfigError::NoPrivateKey { .. }),
                "{mode:o}"
            );
        }
    }
    // Root-owned keys may be group-readable (0640), never group-writable or world-readable.
    assert!(key_mode_allowed(true, 0o640));
    assert!(key_mode_allowed(true, 0o600));
    assert!(!key_mode_allowed(true, 0o660));
    assert!(!key_mode_allowed(true, 0o604));
    assert!(!key_mode_allowed(false, 0o640));
}

// ---- the verifier, offline (certificates generated here) ---------------------------------------------

mod pki {
    use super::*;
    use rcgen::{
        date_time_ymd, BasicConstraints, CertificateParams, DnType, ExtendedKeyUsagePurpose, IsCa,
        Issuer, KeyPair, KeyUsagePurpose,
    };

    pub struct Authority {
        pub params: CertificateParams,
        pub key: KeyPair,
        pub der: CertificateDer<'static>,
        pub pem: String,
    }

    pub fn authority(name: &str, is_ca: IsCa) -> Authority {
        let mut params = CertificateParams::new(Vec::<String>::new()).unwrap();
        params.distinguished_name.push(DnType::CommonName, name);
        params.is_ca = is_ca;
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

    pub fn ca(name: &str) -> Authority {
        authority(name, IsCa::Ca(BasicConstraints::Unconstrained))
    }

    /// A certificate for `names`, signed by `issuer` (or self-signed when `None`);
    /// `tweak` edits the parameters first. Returns the DER and its private key.
    pub fn leaf(
        issuer: Option<&Authority>,
        names: &[&str],
        tweak: impl FnOnce(&mut CertificateParams),
    ) -> (CertificateDer<'static>, KeyPair) {
        leaf_keyed(issuer, names, tweak, KeyPair::generate().unwrap())
    }

    /// An intermediate CA (optionally name-constrained) signed by `root`.
    pub fn intermediate(root: &Authority, permitted_dns: Option<&str>) -> Authority {
        let mut mid = authority("Intermediate CA", IsCa::Ca(BasicConstraints::Unconstrained));
        if let Some(dns) = permitted_dns {
            mid.params.name_constraints = Some(rcgen::NameConstraints {
                permitted_subtrees: vec![rcgen::GeneralSubtree::DnsName(dns.to_owned())],
                excluded_subtrees: vec![],
            });
        }
        let signed = mid
            .params
            .signed_by(&mid.key, &Issuer::from_params(&root.params, &root.key))
            .unwrap();
        mid.der = signed.der().clone();
        mid.pem = signed.pem();
        mid
    }

    pub fn leaf_keyed(
        issuer: Option<&Authority>,
        names: &[&str],
        tweak: impl FnOnce(&mut CertificateParams),
        key: KeyPair,
    ) -> (CertificateDer<'static>, KeyPair) {
        let mut params = CertificateParams::new(
            names
                .iter()
                .map(|name| (*name).to_owned())
                .collect::<Vec<_>>(),
        )
        .unwrap();
        params.extended_key_usages = vec![ExtendedKeyUsagePurpose::ServerAuth];
        tweak(&mut params);
        let certificate = match issuer {
            Some(issuer) => params
                .signed_by(&key, &Issuer::from_params(&issuer.params, &issuer.key))
                .unwrap(),
            None => params.self_signed(&key).unwrap(),
        };
        (certificate.der().clone(), key)
    }

    pub fn dates(params: &mut CertificateParams, from: i32, to: i32) {
        params.not_before = date_time_ymd(from, 1, 1);
        params.not_after = date_time_ymd(to, 1, 1);
    }
}

fn verifier(trusted: &[&CertificateDer<'static>], check_name: bool) -> PgVerifier {
    let mut store = RootCertStore::empty();
    for certificate in trusted {
        store.add((*certificate).clone()).unwrap();
    }
    PgVerifier {
        roots: Some(Arc::new(store)),
        check_name,
        provider: Arc::new(crypto::ring::default_provider()),
    }
}

fn verify(
    verifier: &PgVerifier,
    end_entity: &CertificateDer<'_>,
    intermediates: &[CertificateDer<'_>],
    host: &str,
) -> Result<(), rustls::Error> {
    verifier
        .verify_server_cert(
            end_entity,
            intermediates,
            &ServerName::try_from(host.to_owned()).unwrap(),
            &[],
            UnixTime::now(),
        )
        .map(drop)
}

fn certificate_error(error: rustls::Error) -> CertProblem {
    match error {
        rustls::Error::InvalidCertificate(problem) => CertProblem::from_rustls(&problem),
        other => panic!("not a certificate error: {other}"),
    }
}

#[test]
fn verify_ca_checks_the_chain_and_ignores_the_name_but_verify_full_does_not() {
    let ca = pki::ca("Test CA");
    let (leaf, _) = pki::leaf(Some(&ca), &["db.example"], |_| {});
    let verify_ca = verifier(&[&ca.der], false);
    let verify_full = verifier(&[&ca.der], true);
    assert!(verify(&verify_ca, &leaf, &[], "db.example").is_ok());
    assert!(verify(&verify_full, &leaf, &[], "db.example").is_ok());
    // Wrong name, trusted chain: only verify-full objects.
    assert!(verify(&verify_ca, &leaf, &[], "other.example").is_ok());
    assert_eq!(
        certificate_error(verify(&verify_full, &leaf, &[], "other.example").unwrap_err()),
        CertProblem::NameMismatch
    );
}

#[test]
fn an_untrusted_chain_fails_even_when_the_name_is_wrong_too() {
    let trusted = pki::ca("Trusted CA");
    let rogue = pki::ca("Rogue CA");
    let (evil, _) = pki::leaf(Some(&rogue), &["evil.example"], |_| {});
    for check_name in [false, true] {
        let verifier = verifier(&[&trusted.der], check_name);
        // Wrong name AND unknown issuer: verify-ca must report the chain, not accept the name error.
        assert_eq!(
            certificate_error(verify(&verifier, &evil, &[], "db.example").unwrap_err()),
            CertProblem::UnknownIssuer
        );
    }
    // A self-signed leaf that is not in the roots.
    let (selfsigned, _) = pki::leaf(None, &["db.example"], |_| {});
    assert_eq!(
        certificate_error(
            verify(
                &verifier(&[&trusted.der], false),
                &selfsigned,
                &[],
                "db.example"
            )
            .unwrap_err()
        ),
        CertProblem::UnknownIssuer
    );
}

#[test]
fn expired_and_not_yet_valid_certificates_are_refused() {
    let ca = pki::ca("Test CA");
    let (expired, _) = pki::leaf(Some(&ca), &["db.example"], |p| pki::dates(p, 2000, 2001));
    let (future, _) = pki::leaf(Some(&ca), &["db.example"], |p| pki::dates(p, 2998, 2999));
    let verifier = verifier(&[&ca.der], false);
    assert_eq!(
        certificate_error(verify(&verifier, &expired, &[], "db.example").unwrap_err()),
        CertProblem::Expired
    );
    assert_eq!(
        certificate_error(verify(&verifier, &future, &[], "db.example").unwrap_err()),
        CertProblem::NotYetValid
    );
}

#[test]
fn a_client_auth_only_certificate_is_not_a_server_certificate() {
    let ca = pki::ca("Test CA");
    let (client_only, _) = pki::leaf(Some(&ca), &["db.example"], |p| {
        p.extended_key_usages = vec![rcgen::ExtendedKeyUsagePurpose::ClientAuth];
    });
    for check_name in [false, true] {
        assert!(verify(
            &verifier(&[&ca.der], check_name),
            &client_only,
            &[],
            "db.example"
        )
        .is_err());
    }
}

#[test]
fn an_issuer_that_is_not_a_ca_cannot_vouch_for_a_leaf() {
    let root = pki::ca("Root CA");
    // An intermediate without the CA constraint, signed by the trusted root.
    let mut middle = pki::authority("Not a CA", rcgen::IsCa::NoCa);
    let signed = middle
        .params
        .signed_by(
            &middle.key,
            &rcgen::Issuer::from_params(&root.params, &root.key),
        )
        .unwrap();
    middle.der = signed.der().clone();
    let (leaf, _) = pki::leaf(Some(&middle), &["db.example"], |_| {});
    for check_name in [false, true] {
        let outcome = verify(
            &verifier(&[&root.der], check_name),
            &leaf,
            std::slice::from_ref(&middle.der),
            "db.example",
        );
        assert!(outcome.is_err(), "check_name={check_name}");
    }
}

#[test]
fn ip_literal_hosts_need_a_matching_ip_san_under_verify_full() {
    let ca = pki::ca("Test CA");
    let (leaf, _) = pki::leaf(Some(&ca), &["127.0.0.1"], |_| {});
    let verify_full = verifier(&[&ca.der], true);
    assert!(verify(&verify_full, &leaf, &[], "127.0.0.1").is_ok());
    assert_eq!(
        certificate_error(verify(&verify_full, &leaf, &[], "127.0.0.2").unwrap_err()),
        CertProblem::NameMismatch
    );
    // A DNS-only certificate does not match an IP literal.
    let (dns_only, _) = pki::leaf(Some(&ca), &["db.example"], |_| {});
    assert!(verify(&verify_full, &dns_only, &[], "127.0.0.1").is_err());
    // verify-ca accepts the other address: chain only.
    assert!(verify(&verifier(&[&ca.der], false), &leaf, &[], "127.0.0.2").is_ok());
}

#[test]
fn no_roots_means_encrypt_only() {
    let (selfsigned, _) = pki::leaf(None, &["whatever.example"], |_| {});
    let encrypt_only = PgVerifier {
        roots: None,
        check_name: false,
        provider: Arc::new(crypto::ring::default_provider()),
    };
    assert!(verify(&encrypt_only, &selfsigned, &[], "db.example").is_ok());
}

// ---- a fake server that speaks only SSLRequest ---------------------------------------------------------

#[test]
fn a_session_left_idle_notices_a_vanished_server_within_minutes() {
    // tokio-postgres' own default is a first probe after 2 hours.
    let (config, _) = Transport::prepare("postgres://u@127.0.0.1:1/d").unwrap();
    assert!(config.get_keepalives());
    assert_eq!(config.get_keepalives_idle(), Duration::from_secs(30));
    assert_eq!(config.get_keepalives_interval(), Some(Duration::from_secs(10)));
    assert_eq!(config.get_keepalives_retries(), Some(3));
    // A URI that sets any of them is left alone.
    let (config, _) =
        Transport::prepare("postgres://u@127.0.0.1:1/d?keepalives_idle=600").unwrap();
    assert_eq!(config.get_keepalives_idle(), Duration::from_secs(600));
    assert_eq!(config.get_keepalives_interval(), None);
}

pub(crate) mod fake {
    use super::*;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio::net::TcpListener;
    use tokio_rustls::TlsAcceptor;

    #[derive(Clone)]
    pub enum Answer {
        /// `N`: no TLS here.
        No,
        /// `S`, then the handshake with this certificate.
        Yes(TlsAcceptor),
        /// Accepts the connection and never answers `SSLRequest`.
        Silent,
    }

    #[derive(Clone, Copy)]
    pub enum Auth {
        /// AuthenticationOk, BackendKeyData, ReadyForQuery.
        Ok,
        /// AuthenticationSASL offering only SCRAM-SHA-256 (no -PLUS).
        ScramWithoutPlus,
        /// Every login fails: 28P01 "password authentication failed".
        WrongPassword,
        /// A `hostssl`-only pg_hba.conf: clear text gets 28000 "... no encryption".
        HostSslOnly,
        /// The same refusal with a localized message: only the SQLSTATE is reliable.
        HostSslOnlyLocalized,
        /// AuthenticationSASL offering SCRAM-SHA-256-PLUS and SCRAM-SHA-256.
        ScramWithPlus,
    }

    /// What the server does after it read a cancel request.
    #[derive(Clone, Copy)]
    pub enum Linger {
        /// Closes the connection at once.
        Close,
        /// Closes it after this long (the real server: after it signalled the backend).
        After(std::time::Duration),
        /// Keeps it open (a server that never closes).
        Forever,
    }

    pub type Log = Arc<Mutex<Vec<(usize, Vec<u8>)>>>;

    pub struct Server {
        pub port: u16,
        /// Per connection (in accept order): the bytes the client sent after the answer.
        pub received: Log,
        /// Cancel connections the server closed (counted right before the close).
        pub closed: Arc<std::sync::atomic::AtomicUsize>,
    }

    impl Server {
        /// Bytes received after the answer on connection `index`.
        pub async fn after_answer(&self, index: usize) -> Vec<u8> {
            for _ in 0..50 {
                if let Some((_, bytes)) = self
                    .received
                    .lock()
                    .unwrap()
                    .iter()
                    .find(|(i, _)| *i == index)
                {
                    return bytes.clone();
                }
                tokio::time::sleep(std::time::Duration::from_millis(20)).await;
            }
            panic!("connection {index} never finished");
        }
    }

    /// An acceptor for `names` signed by a fresh CA, plus the CA as PEM.
    pub fn acceptor(names: &[&str]) -> (TlsAcceptor, String) {
        let ca = pki::ca("Fake server CA");
        let (leaf, key) = pki::leaf(Some(&ca), names, |_| {});
        (acceptor_for(leaf, &key), ca.pem)
    }

    /// A self-signed Ed25519 certificate: its signature algorithm has no
    /// `tls-server-end-point` hash.
    pub fn ed25519_acceptor(names: &[&str]) -> TlsAcceptor {
        let key = rcgen::KeyPair::generate_for(&rcgen::PKCS_ED25519).unwrap();
        let (leaf, key) = pki::leaf_keyed(None, names, |_| {}, key);
        acceptor_for(leaf, &key)
    }

    pub fn acceptor_for(leaf: CertificateDer<'static>, key: &rcgen::KeyPair) -> TlsAcceptor {
        let provider = Arc::new(crypto::ring::default_provider());
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

    pub async fn start(script: Vec<Answer>, auth: Auth) -> Server {
        start_with(script, auth, Linger::Close).await
    }

    pub async fn start_with(script: Vec<Answer>, auth: Auth, linger: Linger) -> Server {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        let received = Arc::new(Mutex::new(Vec::new()));
        let closed = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let (log, count) = (Arc::clone(&received), Arc::clone(&closed));
        tokio::spawn(async move {
            for (index, answer) in script.into_iter().enumerate() {
                let Ok((stream, _)) = listener.accept().await else {
                    return;
                };
                tokio::spawn(connection(
                    stream,
                    answer,
                    (auth, linger),
                    (Arc::clone(&log), Arc::clone(&count)),
                    index,
                ));
            }
        });
        Server {
            port,
            received,
            closed,
        }
    }

    /// What the server does with one connection besides the answer to `SSLRequest`.
    struct Behaviour {
        auth: Auth,
        linger: Linger,
        log: Log,
        closed: Arc<std::sync::atomic::AtomicUsize>,
        index: usize,
    }

    async fn connection(
        mut stream: tokio::net::TcpStream,
        answer: Answer,
        (auth, linger): (Auth, Linger),
        (log, closed): (Log, Arc<std::sync::atomic::AtomicUsize>),
        index: usize,
    ) {
        let behaviour = Behaviour {
            auth,
            linger,
            log,
            closed,
            index,
        };
        let mut first = [0u8; 8];
        if stream.read_exact(&mut first).await.is_err() {
            return;
        }
        // SSLRequest: length 8, code 80877103.
        if first == [0, 0, 0, 8, 4, 0xd2, 0x16, 0x2f] {
            match answer {
                Answer::No => {
                    let _ = stream.write_all(b"N").await;
                    session(stream, false, &behaviour, None).await;
                }
                Answer::Yes(acceptor) => {
                    let _ = stream.write_all(b"S").await;
                    if let Ok(tls) = acceptor.accept(stream).await {
                        session(tls, true, &behaviour, None).await;
                    }
                }
                Answer::Silent => tokio::time::sleep(std::time::Duration::from_secs(60)).await,
            }
        } else {
            session(stream, false, &behaviour, Some(first)).await;
        }
    }

    /// Just enough of the extended protocol for `SELECT current_database()::text`:
    /// every statement answers one text column, `fakedb`; a simple `Query` (the
    /// driver's session `SET`s) answers `SET`.
    async fn answer_queries<S: AsyncRead + AsyncWrite + Unpin>(stream: &mut S) {
        loop {
            let mut head = [0u8; 5];
            if stream.read_exact(&mut head).await.is_err() {
                return;
            }
            let length = u32::from_be_bytes(head[1..5].try_into().unwrap()) as usize;
            let mut body = vec![0u8; length.saturating_sub(4)];
            if stream.read_exact(&mut body).await.is_err() {
                return;
            }
            let reply: Vec<u8> = match head[0] {
                b'P' => vec![b'1', 0, 0, 0, 4],
                b'D' => {
                    // ParameterDescription (no parameters), RowDescription (one text column).
                    let mut reply = vec![b't', 0, 0, 0, 6, 0, 0];
                    let name = b"current_database\0";
                    reply.push(b'T');
                    reply.extend_from_slice(&((4 + 2 + name.len() + 18) as u32).to_be_bytes());
                    reply.extend_from_slice(&1u16.to_be_bytes());
                    reply.extend_from_slice(name);
                    reply.extend_from_slice(&0u32.to_be_bytes());
                    reply.extend_from_slice(&0u16.to_be_bytes());
                    reply.extend_from_slice(&25u32.to_be_bytes());
                    reply.extend_from_slice(&(-1i16).to_be_bytes());
                    reply.extend_from_slice(&(-1i32).to_be_bytes());
                    reply.extend_from_slice(&0u16.to_be_bytes());
                    reply
                }
                b'B' => vec![b'2', 0, 0, 0, 4],
                b'E' => {
                    let mut reply = vec![b'D', 0, 0, 0, 16, 0, 1, 0, 0, 0, 6];
                    reply.extend_from_slice(b"fakedb");
                    reply.extend_from_slice(&[b'C', 0, 0, 0, 13]);
                    reply.extend_from_slice(b"SELECT 1\0");
                    reply
                }
                b'S' => vec![b'Z', 0, 0, 0, 5, b'I'],
                // The driver's session settings (`SET ...` over the simple protocol).
                b'Q' => {
                    let mut reply = vec![b'C', 0, 0, 0, 8];
                    reply.extend_from_slice(b"SET\0");
                    reply.extend_from_slice(&[b'Z', 0, 0, 0, 5, b'I']);
                    reply
                }
                b'X' => return,
                _ => Vec::new(),
            };
            if !reply.is_empty() && stream.write_all(&reply).await.is_err() {
                return;
            }
        }
    }

    async fn session<S: AsyncRead + AsyncWrite + Unpin>(
        mut stream: S,
        tls: bool,
        Behaviour {
            auth,
            linger,
            log,
            closed,
            index,
        }: &Behaviour,
        first: Option<[u8; 8]>,
    ) {
        let (auth, linger, index) = (*auth, *linger, *index);
        let mut seen = Vec::new();
        let mut packet = Vec::new();
        match first {
            Some(first) => packet.extend_from_slice(&first),
            None => {
                let mut length = [0u8; 4];
                if stream.read_exact(&mut length).await.is_ok() {
                    packet.extend_from_slice(&length);
                }
            }
        }
        if packet.len() >= 4 {
            let total = u32::from_be_bytes(packet[..4].try_into().unwrap()) as usize;
            let mut rest = vec![0u8; total.saturating_sub(packet.len())];
            if stream.read_exact(&mut rest).await.is_ok() {
                packet.extend_from_slice(&rest);
            }
            seen.extend_from_slice(&packet);
            // Protocol 3.0 startup (the cancel request has its own code).
            if packet.len() >= 8 && packet[4..8] == [0, 3, 0, 0] {
                let mut reply = Vec::new();
                let mut queries = false;
                let error = |code: &str, message: &str| {
                    let body = format!("SERROR\0VERROR\0C{code}\0M{message}\0\0");
                    let mut packet = vec![b'E'];
                    packet.extend_from_slice(&((body.len() + 4) as u32).to_be_bytes());
                    packet.extend_from_slice(body.as_bytes());
                    packet
                };
                match auth {
                    Auth::WrongPassword => {
                        reply.extend_from_slice(&error(
                            "28P01",
                            "password authentication failed for user \"u\"",
                        ));
                    }
                    Auth::HostSslOnly if !tls => {
                        reply.extend_from_slice(&error(
                            "28000",
                            "no pg_hba.conf entry for host \"127.0.0.1\", user \"u\", database \"d\", no encryption",
                        ));
                    }
                    Auth::HostSslOnlyLocalized if !tls => {
                        reply.extend_from_slice(&error(
                            "28000",
                            "kein pg_hba.conf-Eintrag fuer Host \"127.0.0.1\", Benutzer \"u\"",
                        ));
                    }
                    Auth::ScramWithPlus => {
                        reply.extend_from_slice(&[b'R', 0, 0, 0, 42, 0, 0, 0, 10]);
                        reply.extend_from_slice(b"SCRAM-SHA-256-PLUS\0SCRAM-SHA-256\0\0");
                    }
                    Auth::Ok | Auth::HostSslOnly | Auth::HostSslOnlyLocalized => {
                        queries = true;
                        reply.extend_from_slice(&[b'R', 0, 0, 0, 8, 0, 0, 0, 0]);
                        reply.extend_from_slice(&[b'K', 0, 0, 0, 12, 0, 0, 0, 1, 0, 0, 0, 2]);
                        reply.extend_from_slice(&[b'Z', 0, 0, 0, 5, b'I']);
                    }
                    Auth::ScramWithoutPlus => {
                        reply.extend_from_slice(&[b'R', 0, 0, 0, 23, 0, 0, 0, 10]);
                        reply.extend_from_slice(b"SCRAM-SHA-256\0\0");
                    }
                }
                let _ = stream.write_all(&reply).await;
                if queries {
                    answer_queries(&mut stream).await;
                } else {
                    let mut sink = [0u8; 256];
                    while matches!(stream.read(&mut sink).await, Ok(n) if n > 0) {}
                }
            } else if packet.len() >= 8 {
                // A cancel request: the real server signals the backend, then closes.
                match linger {
                    Linger::Close => {}
                    Linger::After(delay) => tokio::time::sleep(delay).await,
                    // The client's own half-close must not count as the close.
                    Linger::Forever => tokio::time::sleep(std::time::Duration::from_secs(60)).await,
                }
                closed.fetch_add(1, std::sync::atomic::Ordering::AcqRel);
            }
        }
        log.lock().unwrap().push((index, seen));
    }
}

/// The error of a connect that must fail (`Client` is not `Debug`, so no `expect_err`).
fn refusal(outcome: Result<Client, tokio_postgres::Error>) -> tokio_postgres::Error {
    match outcome {
        Err(error) => error,
        Ok(_) => panic!("the connect succeeded"),
    }
}

pub(crate) fn run<F: std::future::Future>(future: F) -> F::Output {
    tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .enable_all()
        .build()
        .unwrap()
        .block_on(future)
}

/// `postgres://u:pw@127.0.0.1:<port>/d?<query>`, with `{ca}` replaced by a CA file path.
pub(crate) fn fake_uri(port: u16, query: &str, ca: &Path) -> String {
    format!(
        "postgres://u:pw@127.0.0.1:{port}/d?{}",
        query.replace("{ca}", &ca.display().to_string())
    )
}

pub(crate) fn write_ca(dir: &Path, pem: &str) -> PathBuf {
    let path = dir.join("ca.pem");
    std::fs::write(&path, pem).unwrap();
    path
}

const REQUIRING_MODES: [&str; 4] = [
    "sslmode=require",
    "sslmode=verify-ca&sslrootcert={ca}",
    "sslmode=verify-full&sslrootcert={ca}",
    "sslmode=prefer",
];

#[test]
fn n_to_the_ssl_request_is_tls_required_for_every_requiring_mode() {
    let dir = tempfile::tempdir().unwrap();
    let ca = write_ca(dir.path(), &pki::ca("Any CA").pem);
    for query in &REQUIRING_MODES[..3] {
        run(async {
            let server = fake::start(vec![fake::Answer::No], fake::Auth::Ok).await;
            let (config, transport) = Transport::prepare(&fake_uri(server.port, query, &ca))
                .unwrap_or_else(|e| panic!("{query}: {e}"));
            let error = refusal(transport.connect(&config).await);
            let backend = transport.connect_error(&error);
            assert_eq!(
                backend.code.as_deref(),
                Some("connect.tls_required"),
                "{query}: {backend:?}"
            );
            assert!(
                backend.message.contains("does not support TLS"),
                "{}",
                backend.message
            );
        });
    }
}

#[test]
fn prefer_goes_to_clear_text_only_when_the_server_says_n() {
    run(async {
        // Connection 0: SSLRequest -> N; connection 1: the clear-text retry.
        let server = fake::start(vec![fake::Answer::No, fake::Answer::No], fake::Auth::Ok).await;
        let (config, transport) = Transport::prepare(&format!(
            "postgres://u@127.0.0.1:{}/d?sslmode=prefer",
            server.port
        ))
        .unwrap();
        let client = transport
            .connect(&config)
            .await
            .expect("falls back to clear text");
        assert!(transport.info().is_none());
        drop(client);
    });
}

#[test]
fn a_session_that_had_tls_never_goes_clear_text_under_prefer() {
    run(async {
        let (acceptor, _) = fake::acceptor(&["127.0.0.1"]);
        // Connection 0 is TLS; connection 1 (a per-database / metadata / mutation connection)
        // is answered `N` by someone in the middle: it must fail, not continue in clear text.
        let server = fake::start(
            vec![
                fake::Answer::Yes(acceptor),
                fake::Answer::No,
                fake::Answer::No,
            ],
            fake::Auth::Ok,
        )
        .await;
        let (config, transport) = Transport::prepare(&format!(
            "postgres://u@127.0.0.1:{}/d?sslmode=prefer",
            server.port
        ))
        .unwrap();
        let first = transport.connect(&config).await.expect("TLS session");
        assert!(transport.info().is_some());
        let error = refusal(transport.connect(&config).await);
        assert_eq!(
            transport.connect_error(&error).code.as_deref(),
            Some("connect.tls_required")
        );
        // The refused connection sent nothing in clear text.
        assert!(server.after_answer(1).await.is_empty());
        drop(first);
    });
}

#[test]
fn a_cancel_request_is_never_sent_in_clear_text() {
    let dir = tempfile::tempdir().unwrap();
    for query in REQUIRING_MODES {
        run(async {
            let (acceptor, ca_pem) = fake::acceptor(&["127.0.0.1"]);
            let ca = write_ca(dir.path(), &ca_pem);
            // Connection 0: the TLS session. Connection 1: its cancel request, answered `N`.
            let server = fake::start(
                vec![fake::Answer::Yes(acceptor), fake::Answer::No],
                fake::Auth::Ok,
            )
            .await;
            let (config, transport) = Transport::prepare(&fake_uri(server.port, query, &ca))
                .unwrap_or_else(|e| panic!("{query}: {e}"));
            let client = transport
                .connect(&config)
                .await
                .unwrap_or_else(|e| panic!("{query}: {e}"));
            let outcome = transport.cancel(&client.cancel_token()).await;
            assert!(
                outcome.is_err(),
                "{query}: the cancel must fail, not go clear text"
            );
            let sent = server.after_answer(1).await;
            assert!(
                sent.is_empty(),
                "{query}: the cancel key left in clear text: {sent:?}"
            );
        });
    }
}

// ---- acknowledged cancel -------------------------------------------------------------------------------

/// What the fake server's session key (process 1, secret 2) makes of a cancel request.
const CANCEL_REQUEST: [u8; 16] = [0, 0, 0, 16, 4, 0xd2, 0x16, 0x2e, 0, 0, 0, 1, 0, 0, 0, 2];
const LATE: std::time::Duration = std::time::Duration::from_millis(300);

fn dial_of(uri: &str) -> Option<DialTarget> {
    Transport::prepare(uri).expect("the URI is valid").1.dial
}

fn tcp(host: &str, port: u16, address: &str) -> Option<DialTarget> {
    Some(DialTarget::Tcp {
        host: host.to_owned(),
        port,
        address: address.parse().unwrap(),
    })
}

fn pinnable(uri: &str) -> Option<(String, u16)> {
    pinnable_name(&uri.parse::<Config>().expect("the URI is valid"))
}

#[test]
fn the_dial_target_follows_the_configuration_when_it_names_the_address() {
    assert_eq!(
        dial_of("postgres://u@127.0.0.1:5433/d?sslmode=disable"),
        tcp("127.0.0.1", 5433, "127.0.0.1")
    );
    assert_eq!(
        dial_of("postgres://u@127.0.0.1/d"),
        tcp("127.0.0.1", 5432, "127.0.0.1")
    );
    assert_eq!(
        dial_of("postgres://u@[::1]:5434/d"),
        tcp("::1", 5434, "::1")
    );
    // `hostaddr`: the socket goes there, the certificate is checked against `host`.
    assert_eq!(
        dial_of("postgres://u@db.example/d?hostaddr=192.0.2.7&sslmode=verify-full"),
        tcp("db.example", 5432, "192.0.2.7")
    );
    assert_eq!(
        dial_of("postgres://u@/d?hostaddr=192.0.2.7&sslmode=disable"),
        tcp("", 5432, "192.0.2.7")
    );
    // A name has none until the session connects; several hosts, ports or addresses never.
    for uri in [
        "postgres://u@db.example:6000/d",
        "postgres://u@a,b/d",
        "postgres://u@a:1,b:2/d",
        "postgres://u@127.0.0.1,127.0.0.2/d",
        "postgres://u@a/d?hostaddr=192.0.2.7,192.0.2.8",
        "postgres://u@a,b/d?port=1,2",
    ] {
        assert_eq!(dial_of(uri), None, "{uri}");
    }
    #[cfg(unix)]
    {
        assert_eq!(
            dial_of("postgres://u@%2Ftmp%2Fpg/d"),
            Some(DialTarget::Unix("/tmp/pg/.s.PGSQL.5432".into()))
        );
        assert_eq!(
            dial_of("postgres://u@/d?host=/tmp/pg&port=5434"),
            Some(DialTarget::Unix("/tmp/pg/.s.PGSQL.5434".into()))
        );
        assert_eq!(dial_of("postgres://u@%2Ftmp%2Fa,%2Ftmp%2Fb/d"), None);
        assert_eq!(dial_of("postgres://u@%2Ftmp%2Fa,db.example/d"), None);
    }
}

#[test]
fn only_a_single_host_name_is_resolved_and_pinned_by_the_session_connect() {
    assert_eq!(
        pinnable("postgres://u@db.example/d"),
        Some(("db.example".to_owned(), 5432))
    );
    assert_eq!(
        pinnable("postgres://u@db.example:6000/d?connect_timeout=3"),
        Some(("db.example".to_owned(), 6000))
    );
    for uri in [
        "postgres://u@127.0.0.1/d",
        "postgres://u@[::1]/d",
        "postgres://u@db.example/d?hostaddr=192.0.2.7",
        "postgres://u@a,b/d",
        "postgres://u@a:1,b:2/d",
        "postgres://u@a/d?port=1,2",
        "postgres://u@db.example/d?load_balance_hosts=random",
        "postgres://u@/d?hostaddr=192.0.2.7",
        "postgres://u@%2Ftmp%2Fpg/d",
        "postgres://u@%2Ftmp%2Fpg/d?hostaddr=192.0.2.7",
    ] {
        assert_eq!(pinnable(uri), None, "{uri}");
    }
}

#[test]
fn a_transport_without_a_dial_target_has_nobody_to_dial() {
    run(async {
        let server = fake::start(vec![fake::Answer::No], fake::Auth::Ok).await;
        let (config, _) = Transport::prepare(&format!(
            "postgres://u@127.0.0.1:{}/d?sslmode=disable",
            server.port
        ))
        .unwrap();
        let client = Transport::default().connect(&config).await.unwrap();
        assert_eq!(
            Transport::default()
                .cancel_acknowledged(&client.cancel_token())
                .await,
            Delivery::Unacknowledged
        );
    });
}

#[test]
fn shutdown_of_an_until_closed_stream_completes_only_after_the_peer_closed() {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    run(async {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let closed = Arc::new(AtomicBool::new(false));
        let flag = Arc::clone(&closed);
        tokio::spawn(async move {
            let (mut peer, _) = listener.accept().await.unwrap();
            let mut request = [0u8; 16];
            peer.read_exact(&mut request).await.unwrap();
            assert_eq!(request, CANCEL_REQUEST);
            // The peer still talks (a TLS alert, say) before it closes: discarded.
            peer.write_all(b"alert").await.unwrap();
            tokio::time::sleep(LATE).await;
            // Like libpq, the client has not closed its side while it waits.
            let kind = peer.try_read(&mut [0u8; 1]).unwrap_err().kind();
            assert_eq!(kind, io::ErrorKind::WouldBlock, "the client closed first");
            flag.store(true, Ordering::Release);
            drop(peer);
        });
        let written = Arc::new(AtomicBool::new(false));
        let mut stream = UntilClosed::new(
            tokio::net::TcpStream::connect(address).await.unwrap(),
            Arc::clone(&written),
        );
        stream.write_all(&CANCEL_REQUEST).await.unwrap();
        stream.flush().await.unwrap();
        assert!(!written.load(Ordering::Acquire), "not before the shutdown");
        stream.shutdown().await.unwrap();
        assert!(
            closed.load(Ordering::Acquire),
            "shutdown returned before the peer closed"
        );
        assert!(written.load(Ordering::Acquire));
    });
    // A peer that never closes keeps the shutdown pending (the caller's timeout ends it).
    run(async {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let _peer = tokio::spawn(async move {
            let (peer, _) = listener.accept().await.unwrap();
            tokio::time::sleep(std::time::Duration::from_secs(10)).await;
            drop(peer);
        });
        let mut stream = UntilClosed::new(
            tokio::net::TcpStream::connect(address).await.unwrap(),
            Arc::default(),
        );
        stream.write_all(&CANCEL_REQUEST).await.unwrap();
        let outcome = tokio::time::timeout(LATE, stream.shutdown()).await;
        assert!(outcome.is_err(), "shutdown did not wait for the peer");
    });
}

fn address(text: &str) -> IpAddr {
    text.parse().unwrap()
}

#[test]
fn a_name_pins_the_first_address_that_accepts_and_nothing_is_resolved_afterwards() {
    use std::sync::atomic::AtomicUsize;
    run(async {
        // Connection 0: the session, 1: its acknowledged cancel, 2: a later connection.
        let server = fake::start(
            vec![fake::Answer::No, fake::Answer::No, fake::Answer::No],
            fake::Auth::Ok,
        )
        .await;
        let (config, mut transport) = Transport::prepare(&format!(
            "postgres://u@db.example:{}/d?sslmode=disable",
            server.port
        ))
        .unwrap();
        assert_eq!(
            transport.dial, None,
            "a name has no target before it connects"
        );
        let lookups = Arc::new(AtomicUsize::new(0));
        let seen = Arc::clone(&lookups);
        let expected_port = server.port;
        // The name resolves to `::1` first, where nothing listens: refused, next address.
        let client = transport
            .connect_session_with(&config, move |host, port| async move {
                seen.fetch_add(1, Ordering::AcqRel);
                assert_eq!((host.as_str(), port), ("db.example", expected_port));
                Ok(vec![address("::1"), address("127.0.0.1")])
            })
            .await
            .expect("the second address accepts");
        assert_eq!(transport.dial, tcp("db.example", server.port, "127.0.0.1"));
        assert_eq!(transport.pinned, Some(address("127.0.0.1")));
        // The cancel goes to the pinned address, and nothing is resolved for it.
        assert_eq!(
            transport.cancel_acknowledged(&client.cancel_token()).await,
            Delivery::Acknowledged
        );
        assert_eq!(server.closed.load(Ordering::Acquire), 1);
        // A later connection (per-database, metadata) goes to the pinned address as well:
        // `db.example` is never looked up (it would not resolve).
        let second = transport.connect(&config).await.expect("pinned connect");
        drop((client, second));
        assert_eq!(lookups.load(Ordering::Acquire), 1);
    });
}

#[test]
fn a_name_that_does_not_resolve_is_connected_the_old_way_without_a_target() {
    run(async {
        for resolution in [Err(io::Error::other("no DNS")), Ok(Vec::new())] {
            let server = fake::start(vec![fake::Answer::No], fake::Auth::Ok).await;
            let (config, mut transport) = Transport::prepare(&format!(
                "postgres://u@localhost:{}/d?sslmode=disable",
                server.port
            ))
            .unwrap();
            // The driver's lookup fails; `tokio-postgres` resolves `localhost` itself.
            let client = transport
                .connect_session_with(&config, move |_, _| async move { resolution })
                .await
                .expect("the old connect");
            assert_eq!(transport.dial, None);
            assert_eq!(transport.pinned, None);
            assert_eq!(
                transport.cancel_acknowledged(&client.cancel_token()).await,
                Delivery::Unacknowledged
            );
        }
    });
}

#[test]
fn when_every_address_fails_the_last_error_is_returned_and_nothing_is_pinned() {
    run(async {
        let dead = std::net::TcpListener::bind("127.0.0.1:0")
            .unwrap()
            .local_addr()
            .unwrap();
        let (config, mut transport) = Transport::prepare(&format!(
            "postgres://u@db.example:{}/d?sslmode=disable",
            dead.port()
        ))
        .unwrap();
        let outcome = transport
            .connect_session_with(&config, |_, _| async {
                Ok(vec![address("::1"), address("127.0.0.1")])
            })
            .await;
        let error = refusal(outcome);
        assert!(
            transport.connect_error(&error).message.contains("connect"),
            "{error:?}"
        );
        assert_eq!((transport.dial, transport.pinned), (None, None));
    });
}

#[test]
fn a_pinned_name_stays_the_tls_server_name() {
    let dir = tempfile::tempdir().unwrap();
    run(async {
        // The certificate is for `db.example`; the socket goes to 127.0.0.1.
        let (acceptor, ca_pem) = fake::acceptor(&["db.example"]);
        let ca = write_ca(dir.path(), &ca_pem);
        let server = fake::start(
            vec![
                fake::Answer::Yes(acceptor.clone()),
                fake::Answer::Yes(acceptor),
            ],
            fake::Auth::Ok,
        )
        .await;
        let (config, mut transport) = Transport::prepare(&format!(
            "postgres://u:pw@db.example:{}/d?sslmode=verify-full&sslrootcert={}",
            server.port,
            ca.display()
        ))
        .unwrap();
        let client = transport
            .connect_session_with(&config, |_, _| async { Ok(vec![address("127.0.0.1")]) })
            .await
            .expect("verified against the name, not the address");
        assert_eq!(
            transport.cancel_acknowledged(&client.cancel_token()).await,
            Delivery::Acknowledged
        );
        assert_eq!(server.closed.load(Ordering::Acquire), 1);
    });
}

#[test]
fn nobody_accepting_the_connection_is_unacknowledged() {
    run(async {
        let server = fake::start(vec![fake::Answer::No], fake::Auth::Ok).await;
        let (config, transport) = Transport::prepare(&format!(
            "postgres://u@127.0.0.1:{}/d?sslmode=disable",
            server.port
        ))
        .unwrap();
        let client = transport.connect(&config).await.unwrap();
        let dead = std::net::TcpListener::bind("127.0.0.1:0")
            .unwrap()
            .local_addr()
            .unwrap();
        let transport = Transport {
            dial: tcp("127.0.0.1", dead.port(), "127.0.0.1"),
            ..transport
        };
        assert_eq!(
            transport.cancel_acknowledged(&client.cancel_token()).await,
            Delivery::Unacknowledged
        );
    });
}

#[test]
fn a_dial_that_hangs_ends_at_its_budget() {
    let dir = tempfile::tempdir().unwrap();
    run(async {
        let (acceptor, ca_pem) = fake::acceptor(&["127.0.0.1"]);
        let ca = write_ca(dir.path(), &ca_pem);
        let session = fake::start(vec![fake::Answer::Yes(acceptor)], fake::Auth::Ok).await;
        // Accepts the connection and never answers the SSLRequest.
        let silent = fake::start(vec![fake::Answer::Silent], fake::Auth::Ok).await;
        let query = "sslmode=verify-full&sslrootcert={ca}";
        let (config, transport) = Transport::prepare(&fake_uri(session.port, query, &ca)).unwrap();
        let client = transport.connect(&config).await.unwrap();
        let transport = Transport {
            dial: tcp("127.0.0.1", silent.port, "127.0.0.1"),
            dial_budget: std::time::Duration::from_millis(200),
            ..transport
        };
        let started = std::time::Instant::now();
        assert_eq!(
            transport.cancel_acknowledged(&client.cancel_token()).await,
            Delivery::Unacknowledged
        );
        assert!(started.elapsed() < std::time::Duration::from_secs(5));
    });
}

#[cfg(unix)]
#[test]
fn an_acknowledged_request_over_a_unix_socket_waits_for_the_close() {
    use tokio::io::AsyncReadExt;
    run(async {
        let server = fake::start(vec![fake::Answer::No], fake::Auth::Ok).await;
        let (config, transport) = Transport::prepare(&format!(
            "postgres://u@127.0.0.1:{}/d?sslmode=disable",
            server.port
        ))
        .unwrap();
        let client = transport.connect(&config).await.unwrap();
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join(".s.PGSQL.5432");
        let listener = tokio::net::UnixListener::bind(&path).unwrap();
        let closed = Arc::new(AtomicBool::new(false));
        let flag = Arc::clone(&closed);
        tokio::spawn(async move {
            let (mut peer, _) = listener.accept().await.unwrap();
            let mut request = [0u8; 16];
            peer.read_exact(&mut request).await.unwrap();
            assert_eq!(request, CANCEL_REQUEST);
            tokio::time::sleep(LATE).await;
            flag.store(true, Ordering::Release);
        });
        let transport = Transport {
            dial: Some(DialTarget::Unix(path)),
            ..transport
        };
        assert_eq!(
            transport.cancel_acknowledged(&client.cancel_token()).await,
            Delivery::Acknowledged
        );
        assert!(closed.load(Ordering::Acquire), "returned before the close");
    });
}

#[test]
fn an_acknowledged_request_is_encrypted_verified_and_never_falls_back_to_clear_text() {
    // `prefer` that negotiated TLS is one of the requiring modes.
    for query in REQUIRING_MODES {
        let dir = tempfile::tempdir().unwrap();
        run(async {
            let (acceptor, ca_pem) = fake::acceptor(&["127.0.0.1"]);
            let ca = write_ca(dir.path(), &ca_pem);
            // Connection 0: the TLS session; 1: the acknowledged cancel (TLS, closes late);
            // 2: a dial whose server answers `N`.
            let server = fake::start_with(
                vec![
                    fake::Answer::Yes(acceptor.clone()),
                    fake::Answer::Yes(acceptor),
                    fake::Answer::No,
                ],
                fake::Auth::Ok,
                fake::Linger::After(LATE),
            )
            .await;
            let (config, transport) = Transport::prepare(&fake_uri(server.port, query, &ca))
                .unwrap_or_else(|e| panic!("{query}: {e}"));
            let client = transport.connect(&config).await.unwrap();
            let handshakes = || {
                transport
                    .tls()
                    .unwrap()
                    .state
                    .handshakes
                    .load(Ordering::Acquire)
            };
            let before = handshakes();
            let token = client.cancel_token();
            assert_eq!(
                transport.cancel_acknowledged(&token).await,
                Delivery::Acknowledged,
                "{query}"
            );
            assert_eq!(handshakes(), before + 1, "{query}: the cancel is TLS");
            assert_eq!(server.closed.load(Ordering::Acquire), 1, "{query}");
            // The server that says `N` to the cancel connection gets nothing in clear text.
            assert_eq!(
                transport.cancel_acknowledged(&token).await,
                Delivery::Unacknowledged,
                "{query}: must fail, not go clear text"
            );
            assert!(server.after_answer(2).await.is_empty(), "{query}");
        });
    }
}

#[test]
fn an_acknowledged_request_after_allow_upgraded_to_tls_is_encrypted() {
    run(async {
        // The plain attempt is refused by a hostssl-only pg_hba.conf, the retry is TLS.
        let (acceptor, _) = fake::acceptor(&["127.0.0.1"]);
        let server = fake::start_with(
            vec![
                fake::Answer::No,
                fake::Answer::Yes(acceptor.clone()),
                fake::Answer::Yes(acceptor),
            ],
            fake::Auth::HostSslOnly,
            fake::Linger::After(LATE),
        )
        .await;
        let (config, transport) = Transport::prepare(&format!(
            "postgres://u@127.0.0.1:{}/d?sslmode=allow",
            server.port
        ))
        .unwrap();
        let client = transport.connect(&config).await.expect("upgraded to TLS");
        let before = transport
            .tls()
            .unwrap()
            .state
            .handshakes
            .load(Ordering::Acquire);
        assert_eq!(
            transport.cancel_acknowledged(&client.cancel_token()).await,
            Delivery::Acknowledged
        );
        let after = transport
            .tls()
            .unwrap()
            .state
            .handshakes
            .load(Ordering::Acquire);
        assert_eq!(after, before + 1, "the cancel is TLS");
        assert_eq!(server.closed.load(Ordering::Acquire), 1);
    });
}

#[test]
fn an_acknowledged_request_after_prefer_fell_back_to_clear_text_is_clear_text() {
    run(async {
        // `N` to the session's SSLRequest; its cancel token is a clear-text one, as the session.
        let server = fake::start_with(
            // 0: the `SSLRequest` that is answered `N`; 1: the clear-text session; 2: its cancel.
            vec![fake::Answer::No, fake::Answer::No, fake::Answer::No],
            fake::Auth::Ok,
            fake::Linger::After(LATE),
        )
        .await;
        let (config, transport) = Transport::prepare(&format!(
            "postgres://u@127.0.0.1:{}/d?sslmode=prefer",
            server.port
        ))
        .unwrap();
        let client = transport.connect(&config).await.expect("clear text");
        assert_eq!(
            transport.cancel_acknowledged(&client.cancel_token()).await,
            Delivery::Acknowledged
        );
        assert_eq!(server.after_answer(2).await, CANCEL_REQUEST);
        assert_eq!(transport.info(), None);
    });
}

#[test]
fn an_acknowledged_request_is_verified_like_the_session() {
    let dir = tempfile::tempdir().unwrap();
    run(async {
        let (good, good_ca) = fake::acceptor(&["127.0.0.1"]);
        let (other, _) = fake::acceptor(&["127.0.0.1"]);
        let ca = write_ca(dir.path(), &good_ca);
        let session = fake::start(vec![fake::Answer::Yes(good)], fake::Auth::Ok).await;
        // Another server, certified by a CA this session does not trust.
        let impostor = fake::start(vec![fake::Answer::Yes(other)], fake::Auth::Ok).await;
        let (config, transport) = Transport::prepare(&fake_uri(
            session.port,
            "sslmode=verify-full&sslrootcert={ca}",
            &ca,
        ))
        .unwrap();
        let client = transport.connect(&config).await.unwrap();
        let transport = Transport {
            dial: tcp("127.0.0.1", impostor.port, "127.0.0.1"),
            ..transport
        };
        assert_eq!(
            transport.cancel_acknowledged(&client.cancel_token()).await,
            Delivery::Unacknowledged,
            "the certificate is not trusted"
        );
        assert_eq!(impostor.closed.load(Ordering::Acquire), 0);
    });
}

#[test]
fn channel_binding_require_fails_without_binding() {
    run(async {
        // A server without TLS that offers SCRAM-SHA-256: there is no channel to bind.
        let server = fake::start(
            vec![fake::Answer::No, fake::Answer::No],
            fake::Auth::ScramWithoutPlus,
        )
        .await;
        let (config, transport) = Transport::prepare(&format!(
            "postgres://u:pw@127.0.0.1:{}/d?sslmode=prefer&channel_binding=require",
            server.port
        ))
        .unwrap();
        let error = refusal(transport.connect(&config).await);
        assert!(
            error.to_string().contains("channel binding")
                || format!("{error:?}").contains("channel binding"),
            "{error:?}"
        );
    });
    run(async {
        // TLS, but the server only offers SCRAM-SHA-256: binding was required and is not offered.
        let (acceptor, _) = fake::acceptor(&["127.0.0.1"]);
        let server = fake::start(
            vec![fake::Answer::Yes(acceptor)],
            fake::Auth::ScramWithoutPlus,
        )
        .await;
        let (config, transport) = Transport::prepare(&format!(
            "postgres://u:pw@127.0.0.1:{}/d?sslmode=require&channel_binding=require",
            server.port
        ))
        .unwrap();
        let error = refusal(transport.connect(&config).await);
        assert!(
            format!("{error:?}").contains("channel binding"),
            "{error:?}"
        );
    });
}

#[test]
fn allow_retries_with_tls_only_when_clear_text_was_refused_for_lack_of_it() {
    run(async {
        // hostssl-only server: the plain attempt is refused ("no encryption"), TLS is the answer.
        let (acceptor, _) = fake::acceptor(&["127.0.0.1"]);
        let server = fake::start(
            vec![fake::Answer::No, fake::Answer::Yes(acceptor)],
            fake::Auth::HostSslOnly,
        )
        .await;
        let (config, transport) = Transport::prepare(&format!(
            "postgres://u@127.0.0.1:{}/d?sslmode=allow",
            server.port
        ))
        .unwrap();
        let client = transport.connect(&config).await.expect("retried with TLS");
        assert!(transport.info().is_some());
        drop(client);
    });
}

#[test]
fn allow_returns_the_servers_answer_to_the_plain_attempt_when_it_is_not_about_tls() {
    run(async {
        // A wrong password on the clear-text attempt is that attempt's error: no TLS retry
        // (the fake accepts one connection only, so a retry would be a connection error).
        let server = fake::start(vec![fake::Answer::No], fake::Auth::WrongPassword).await;
        let (config, transport) = Transport::prepare(&format!(
            "postgres://u:x@127.0.0.1:{}/d?sslmode=allow",
            server.port
        ))
        .unwrap();
        let error = refusal(transport.connect(&config).await);
        assert_eq!(error.code(), Some(&SqlState::INVALID_PASSWORD), "{error:?}");
        let backend = transport.connect_error(&error);
        assert!(
            backend.code.is_none() && backend.message.contains("password authentication failed")
        );
    });
}

// ---- follow-up hardening ------------------------------------------------------------------------------

#[test]
fn the_metadata_connection_never_goes_clear_text_after_a_tls_session_under_prefer() {
    use crate::v2::connection::PgConnection;
    let rt = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .enable_all()
        .build()
        .unwrap();
    let (acceptor, _) = fake::acceptor(&["127.0.0.1"]);
    // Connection 0: the TLS session. Connection 1: the metadata connection, answered `N`
    // by someone in the middle: it must fail instead of continuing in clear text.
    let server = rt.block_on(fake::start(
        vec![
            fake::Answer::Yes(acceptor),
            fake::Answer::No,
            fake::Answer::No,
        ],
        fake::Auth::Ok,
    ));
    let uri = format!("postgres://u@127.0.0.1:{}/d?sslmode=prefer", server.port);
    let session = crate::v2::runtime::block_on(crate::db::postgres::PostgresConn::v2_connect(&uri))
        .unwrap()
        .unwrap_or_else(|e| panic!("{}", e.message));
    assert!(session.v2_tls_info().is_some());
    let connection = PgConnection::new(session);
    let outcome = connection.with_metadata(
        "navigator",
        "catalog",
        |_| false,
        |meta| async move { Ok(meta.default_database().to_owned()) },
    );
    let error = outcome.expect_err("the metadata connection must not downgrade");
    assert!(error.message.contains("TLS"), "{}", error.message);
    assert!(
        rt.block_on(server.after_answer(1)).is_empty(),
        "nothing in clear text"
    );
}

#[test]
fn tls_info_is_none_for_a_plain_session_even_after_a_later_tls_connection() {
    let rt = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .enable_all()
        .build()
        .unwrap();
    let (acceptor, _) = fake::acceptor(&["127.0.0.1"]);
    // Connection 0: SSLRequest -> N; 1: the clear-text session; 2: a later per-database
    // connection, which happens to get TLS.
    let server = rt.block_on(fake::start(
        vec![
            fake::Answer::No,
            fake::Answer::No,
            fake::Answer::Yes(acceptor),
        ],
        fake::Auth::Ok,
    ));
    let uri = format!("postgres://u@127.0.0.1:{}/d?sslmode=prefer", server.port);
    let session = crate::v2::runtime::block_on(crate::db::postgres::PostgresConn::v2_connect(&uri))
        .unwrap()
        .unwrap_or_else(|e| panic!("{}", e.message));
    assert!(session.v2_tls_info().is_none());
    crate::v2::runtime::block_on(session.v2_client("other"))
        .unwrap()
        .expect("later connection");
    assert!(
        session
            .v2_transport()
            .tls()
            .unwrap()
            .state
            .handshakes
            .load(Ordering::Acquire)
            == 1,
        "the later connection did use TLS"
    );
    assert!(
        session.v2_tls_info().is_none(),
        "the session is still the clear-text one"
    );
}

#[test]
fn verify_ca_settings_without_roots_cannot_build_a_config() {
    for roots in [None, Some(RootCerts::System)] {
        let settings = TlsSettings {
            mode: SslMode::VerifyCa,
            roots,
            client: None,
        };
        assert_eq!(
            client_config(&settings).err(),
            Some(TlsConfigError::VerifyCaNeedsCaFile)
        );
        assert!(matches!(
            trust_roots(&settings),
            Err(TlsConfigError::VerifyCaNeedsCaFile)
        ));
    }
    let weak = TlsSettings {
        mode: SslMode::Require,
        roots: Some(RootCerts::System),
        client: None,
    };
    assert!(matches!(
        trust_roots(&weak),
        Err(TlsConfigError::SystemRootsNeedVerifyFull(SslMode::Require))
    ));
    // Encrypt-only modes without roots are the one case that has none.
    let encrypt_only = TlsSettings {
        mode: SslMode::Require,
        roots: None,
        client: None,
    };
    assert!(matches!(trust_roots(&encrypt_only), Ok(None)));
}

#[test]
fn an_at_sign_in_a_query_value_is_refused() {
    // tokio-postgres would read everything before this `@` as credentials and the
    // host would move to "b".
    for uri in [
        "postgres://h/d?application_name=a@b&sslmode=verify-full&sslrootcert=/ca.pem",
        "postgres://h:5432/d?x=a@b",
    ] {
        assert_eq!(
            TlsSettings::from_uri(uri).unwrap_err(),
            TlsConfigError::UnescapedAtSign,
            "{uri}"
        );
        assert!(Transport::prepare(uri).is_err());
    }
    // Escaped, or an `@` that really is the end of credentials, stays fine.
    assert!(TlsSettings::from_uri("postgres://h/d?application_name=a%40b").is_ok());
    assert!(TlsSettings::from_uri("postgres://u:p%40ss@h/d?sslmode=require").is_ok());
    assert!(TlsSettings::from_uri("postgres://u:p?w@h/d?sslmode=require").is_ok());
}

fn two_hosts(a: u16, b: u16, query: &str) -> String {
    format!("postgres://u@127.0.0.1:{a},127.0.0.1:{b}/d?{query}")
}

#[test]
fn prefer_with_two_hosts_does_not_clear_text_a_host_whose_certificate_failed() {
    let dir = tempfile::tempdir().unwrap();
    // The client trusts neither fake CA, so a TLS host fails its certificate check.
    let ca = write_ca(dir.path(), &pki::ca("Someone else").pem);
    run(async {
        let (bad_a, _) = fake::acceptor(&["127.0.0.1"]);
        let (bad_b, _) = fake::acceptor(&["127.0.0.1"]);
        // A second script entry on each host would catch a clear-text retry.
        let a = fake::start(
            vec![fake::Answer::Yes(bad_a), fake::Answer::No],
            fake::Auth::Ok,
        )
        .await;
        let b = fake::start(
            vec![fake::Answer::Yes(bad_b), fake::Answer::No],
            fake::Auth::Ok,
        )
        .await;
        let uri = two_hosts(
            a.port,
            b.port,
            &format!("sslmode=prefer&sslrootcert={}", ca.display()),
        );
        let (config, transport) = Transport::prepare(&uri).unwrap();
        let error = refusal(transport.connect(&config).await);
        assert_eq!(
            transport.connect_error(&error).code.as_deref(),
            Some("connect.tls_certificate")
        );
        tokio::time::sleep(std::time::Duration::from_millis(300)).await;
        for server in [&a, &b] {
            let clear_text = server
                .received
                .lock()
                .unwrap()
                .iter()
                .any(|(_, bytes)| !bytes.is_empty());
            assert!(
                !clear_text,
                "a host with a failed certificate was retried in clear text"
            );
        }
    });
}

#[test]
fn prefer_with_two_hosts_connects_a_host_that_says_n_like_libpq() {
    let dir = tempfile::tempdir().unwrap();
    let ca = write_ca(dir.path(), &pki::ca("Someone else").pem);
    run(async {
        let (bad_a, _) = fake::acceptor(&["127.0.0.1"]);
        // A fails its certificate; B has no TLS (N, then the clear-text startup).
        let a = fake::start(
            vec![fake::Answer::Yes(bad_a), fake::Answer::No],
            fake::Auth::Ok,
        )
        .await;
        let b = fake::start(vec![fake::Answer::No, fake::Answer::No], fake::Auth::Ok).await;
        let uri = two_hosts(
            a.port,
            b.port,
            &format!("sslmode=prefer&sslrootcert={}", ca.display()),
        );
        let (config, transport) = Transport::prepare(&uri).unwrap();
        let client = transport
            .connect(&config)
            .await
            .unwrap_or_else(|e| panic!("{e:?}"));
        drop(client);
        tokio::time::sleep(std::time::Duration::from_millis(300)).await;
        let a_clear = a
            .received
            .lock()
            .unwrap()
            .iter()
            .any(|(_, bytes)| !bytes.is_empty());
        assert!(!a_clear, "host A was only tried over TLS");
        let b_startup = b
            .received
            .lock()
            .unwrap()
            .iter()
            .any(|(_, bytes)| bytes.len() > 8);
        assert!(b_startup, "host B was used in clear text, as libpq does");
    });
    // Once a host gave TLS, a later host that says `N` is not used in clear text.
    run(async {
        let (good, _) = fake::acceptor(&["127.0.0.1"]);
        let a = fake::start(vec![fake::Answer::Yes(good)], fake::Auth::Ok).await;
        let b = fake::start(vec![fake::Answer::No, fake::Answer::No], fake::Auth::Ok).await;
        let (config, transport) =
            Transport::prepare(&two_hosts(a.port, b.port, "sslmode=prefer")).unwrap();
        let client = transport.connect(&config).await.expect("host A over TLS");
        assert!(transport.info().is_some());
        drop(client);
    });
}

#[test]
fn allow_upgrades_on_sqlstate_28000_with_a_localized_message() {
    run(async {
        let (acceptor, _) = fake::acceptor(&["127.0.0.1"]);
        let server = fake::start(
            vec![fake::Answer::No, fake::Answer::Yes(acceptor)],
            fake::Auth::HostSslOnlyLocalized,
        )
        .await;
        let (config, transport) = Transport::prepare(&format!(
            "postgres://u@127.0.0.1:{}/d?sslmode=allow",
            server.port
        ))
        .unwrap();
        let client = transport
            .connect(&config)
            .await
            .expect("upgraded on the SQLSTATE alone");
        assert!(transport.info().is_some());
        drop(client);
    });
}

#[test]
fn a_chain_through_an_intermediate_passes_and_a_missing_intermediate_fails() {
    let root = pki::ca("Root CA");
    let mid = pki::intermediate(&root, None);
    let (leaf, _) = pki::leaf(Some(&mid), &["db.example"], |_| {});
    for check_name in [false, true] {
        let verifier = verifier(&[&root.der], check_name);
        assert!(verify(
            &verifier,
            &leaf,
            std::slice::from_ref(&mid.der),
            "db.example"
        )
        .is_ok());
        // The server did not send its intermediate: the chain cannot be built.
        assert_eq!(
            certificate_error(verify(&verifier, &leaf, &[], "db.example").unwrap_err()),
            CertProblem::UnknownIssuer
        );
    }
}

#[test]
fn a_name_constrained_intermediate_cannot_vouch_for_other_names() {
    let root = pki::ca("Root CA");
    let mid = pki::intermediate(&root, Some("corp.example"));
    let (inside, _) = pki::leaf(Some(&mid), &["db.corp.example"], |_| {});
    let (outside, _) = pki::leaf(Some(&mid), &["db.other.example"], |_| {});
    // Even verify-ca (no name check against the host) enforces the CA's constraints.
    let verifier = verifier(&[&root.der], false);
    let chain = std::slice::from_ref(&mid.der);
    assert!(verify(&verifier, &inside, chain, "db.corp.example").is_ok());
    assert!(verify(&verifier, &outside, chain, "db.other.example").is_err());
}

#[test]
fn channel_binding_require_fails_over_tls_when_only_scram_sha_256_is_offered() {
    run(async {
        let (acceptor, _) = fake::acceptor(&["127.0.0.1"]);
        let server = fake::start(
            vec![fake::Answer::Yes(acceptor)],
            fake::Auth::ScramWithoutPlus,
        )
        .await;
        let (config, transport) = Transport::prepare(&format!(
            "postgres://u:pw@127.0.0.1:{}/d?sslmode=require&channel_binding=require",
            server.port
        ))
        .unwrap();
        let error = refusal(transport.connect(&config).await);
        assert!(
            format!("{error:?}").contains("channel binding"),
            "{error:?}"
        );
    });
}

#[test]
fn an_ed25519_certificate_has_no_end_point_hash_so_required_binding_fails() {
    let uri = |port: u16| {
        format!("postgres://u:pw@127.0.0.1:{port}/d?sslmode=require&channel_binding=require")
    };
    run(async {
        // The server offers SCRAM-SHA-256-PLUS but its certificate (Ed25519) gives nothing to bind to.
        let server = fake::start(
            vec![fake::Answer::Yes(fake::ed25519_acceptor(&["127.0.0.1"]))],
            fake::Auth::ScramWithPlus,
        )
        .await;
        let (config, transport) = Transport::prepare(&uri(server.port)).unwrap();
        let error = refusal(transport.connect(&config).await);
        assert!(
            format!("{error:?}").contains("channel binding"),
            "{error:?}"
        );
    });
    run(async {
        // Control: an ECDSA certificate does bind, so the exchange gets past that gate
        // (and then waits for a server that never answers the SASL response).
        let (acceptor, _) = fake::acceptor(&["127.0.0.1"]);
        let server =
            fake::start(vec![fake::Answer::Yes(acceptor)], fake::Auth::ScramWithPlus).await;
        let (config, transport) = Transport::prepare(&uri(server.port)).unwrap();
        let outcome = tokio::time::timeout(
            std::time::Duration::from_millis(600),
            transport.connect(&config),
        )
        .await;
        assert!(outcome.is_err(), "channel binding was available and used");
    });
}

// ---- live tests ------------------------------------------------------------------------------------
//
// Env-gated (`#[ignore]`): `rust-engine/scripts/test-tls-servers.sh start`, `source` its env file, and
// `TABLEX_TEST_PG_URI` for the plain server. See the script header for the command.

mod live {
    use super::*;
    use crate::db::postgres::PostgresConn;
    use crate::v2::connection::CancelState;
    use crate::v2::runtime;
    use engine_driver_kit::api::OperationId;
    use std::time::Duration;

    fn env(name: &str) -> Option<String> {
        std::env::var(name).ok().filter(|value| !value.is_empty())
    }

    /// The TLS test server (`tablex-test-pg-tls`) and its certificate files.
    struct Tls {
        port: String,
        user: String,
        password: String,
        db: String,
        cert_user: String,
        ca: String,
        other_ca: String,
        name: String,
        client: [(String, String); 3],
        encrypted_key: String,
    }

    fn tls() -> Option<Tls> {
        let Some(port) = env("TABLEX_TEST_PG_TLS_PORT") else {
            eprintln!("skipped: TABLEX_TEST_PG_TLS_PORT is not set (test-tls-servers.sh)");
            return None;
        };
        let var = |name: &str| env(name).unwrap_or_else(|| panic!("{name} is not set"));
        Some(Tls {
            port,
            user: var("TABLEX_TEST_PG_TLS_USER"),
            password: var("TABLEX_TEST_PG_TLS_PASSWORD"),
            db: var("TABLEX_TEST_PG_TLS_DB"),
            cert_user: var("TABLEX_TEST_PG_TLS_CERT_USER"),
            ca: var("TABLEX_TEST_TLS_CA"),
            other_ca: var("TABLEX_TEST_TLS_OTHER_CA"),
            name: var("TABLEX_TEST_TLS_SERVER_NAME"),
            client: [
                (
                    var("TABLEX_TEST_TLS_CLIENT_CERT"),
                    var("TABLEX_TEST_TLS_CLIENT_KEY"),
                ),
                (
                    var("TABLEX_TEST_TLS_CLIENT_CERT_RSA"),
                    var("TABLEX_TEST_TLS_CLIENT_KEY_RSA"),
                ),
                (
                    var("TABLEX_TEST_TLS_CLIENT_CERT_EC"),
                    var("TABLEX_TEST_TLS_CLIENT_KEY_EC"),
                ),
            ],
            encrypted_key: var("TABLEX_TEST_TLS_CLIENT_KEY_ENCRYPTED"),
        })
    }

    impl Tls {
        /// Password login on the TLS server; `query` is appended after `?`.
        fn uri(&self, host: &str, query: &str) -> String {
            format!(
                "postgres://{}:{}@{host}:{}/{}?{query}",
                self.user, self.password, self.port, self.db
            )
        }
    }

    /// The plain server (`TABLEX_TEST_PG_URI`, no query part).
    fn plain_uri() -> Option<String> {
        let uri = env("TABLEX_TEST_PG_URI");
        if uri.is_none() {
            eprintln!("skipped: TABLEX_TEST_PG_URI is not set");
        }
        uri
    }

    fn connect(uri: &str) -> Result<PostgresConn, BackendError> {
        runtime::block_on(PostgresConn::v2_connect(uri)).unwrap()
    }

    fn code(result: &Result<PostgresConn, BackendError>) -> String {
        match result {
            Ok(_) => "connected".to_owned(),
            Err(error) => error.code.as_deref().unwrap_or("connect.failed").to_owned(),
        }
    }

    fn error_text(result: Result<PostgresConn, BackendError>) -> String {
        result.err().expect("connect fails").message
    }

    fn scalar(connection: &PostgresConn, sql: &str) -> String {
        runtime::block_on(async {
            let client = connection.v2_client("").await.unwrap();
            let rows = client.simple_query(sql).await.unwrap();
            rows.iter()
                .find_map(|message| match message {
                    tokio_postgres::SimpleQueryMessage::Row(row) => row.get(0).map(str::to_owned),
                    _ => None,
                })
                .expect("one row")
        })
        .unwrap()
    }

    /// What the SERVER says about this session: encrypted, protocol, and whether a client cert came.
    fn server_view(connection: &PostgresConn) -> String {
        scalar(
            connection,
            "SELECT ssl::text || ' ' || coalesce(version, '-') FROM pg_stat_ssl WHERE pid = pg_backend_pid()",
        )
    }

    #[test]
    #[ignore = "needs test-tls-servers.sh (TABLEX_TEST_PG_TLS_*)"]
    fn sslmode_matrix_against_the_tls_server() {
        let Some(t) = tls() else { return };
        let ca = &t.ca;
        let other = &t.other_ca;
        let name = &t.name;
        // (host, query, expected code, expected verified level when connected)
        let cases: Vec<(&str, String, &str, Option<Verified>)> = vec![
            (
                "127.0.0.1",
                "sslmode=disable".into(),
                "connect.tls_required",
                None,
            ),
            (
                "127.0.0.1",
                "sslmode=allow".into(),
                "connected",
                Some(Verified::None),
            ),
            (
                "127.0.0.1",
                "sslmode=prefer".into(),
                "connected",
                Some(Verified::None),
            ),
            (
                "127.0.0.1",
                String::new(),
                "connected",
                Some(Verified::None),
            ), // libpq default: prefer
            (
                "127.0.0.1",
                "sslmode=require".into(),
                "connected",
                Some(Verified::None),
            ),
            (
                "127.0.0.1",
                format!("sslmode=require&sslrootcert={ca}"),
                "connected",
                Some(Verified::Ca),
            ),
            (
                "127.0.0.1",
                format!("sslmode=require&sslrootcert={other}"),
                "connect.tls_certificate",
                None,
            ),
            // prefer never downgrades after a certificate failure.
            (
                "127.0.0.1",
                format!("sslmode=prefer&sslrootcert={other}"),
                "connect.tls_certificate",
                None,
            ),
            // verify-ca: right chain, any name.
            (
                "127.0.0.1",
                format!("sslmode=verify-ca&sslrootcert={ca}"),
                "connected",
                Some(Verified::Ca),
            ),
            (
                "127.0.0.1",
                format!("sslmode=verify-ca&sslrootcert={other}"),
                "connect.tls_certificate",
                None,
            ),
            // verify-ca never uses public roots: it needs the CA file (B1).
            (
                "127.0.0.1",
                "sslmode=verify-ca".into(),
                "connect.tls_config",
                None,
            ),
            (
                "127.0.0.1",
                "sslmode=verify-ca&sslrootcert=system".into(),
                "connect.tls_config",
                None,
            ),
            // The OS trust store (verify-full) does not know the throwaway CA.
            (
                "localhost",
                "sslmode=verify-full".into(),
                "connect.tls_certificate",
                None,
            ),
            // verify-full: chain and name.
            (
                "localhost",
                format!("sslmode=verify-full&sslrootcert={ca}"),
                "connected",
                Some(Verified::Full),
            ),
            (
                name.as_str(),
                format!("sslmode=verify-full&sslrootcert={ca}&hostaddr=127.0.0.1"),
                "connected",
                Some(Verified::Full),
            ),
            (
                "127.0.0.1",
                format!("sslmode=verify-full&sslrootcert={ca}"),
                "connect.tls_certificate",
                None,
            ),
            (
                "localhost",
                format!("sslmode=verify-full&sslrootcert={other}"),
                "connect.tls_certificate",
                None,
            ),
            // Config errors.
            (
                "127.0.0.1",
                "sslmode=require&sslrootcert=system".into(),
                "connect.tls_config",
                None,
            ),
            (
                "127.0.0.1",
                "sslmode=verify-full&sslrootcert=/nonexistent/ca.pem".into(),
                "connect.tls_config",
                None,
            ),
            (
                "127.0.0.1",
                "sslmode=bogus".into(),
                "connect.tls_config",
                None,
            ),
        ];
        for (host, query, expected, verified) in cases {
            let result = connect(&t.uri(host, &query));
            assert_eq!(
                code(&result),
                expected,
                "host={host} {query}: {result:?}",
                result = result.as_ref().err()
            );
            if let (Ok(connection), Some(verified)) = (&result, verified) {
                let info = connection.v2_tls_info().expect("TLS in use");
                assert_eq!(info.verified, verified, "{query}");
                assert!(matches!(info.version, "TLSv1.2" | "TLSv1.3"), "{info:?}");
                assert!(!info.cipher.is_empty());
                // The server agrees: the backend sees an encrypted session.
                assert!(server_view(connection).starts_with("true TLSv1"), "{query}");
            }
        }
    }

    #[test]
    #[ignore = "needs test-tls-servers.sh (TABLEX_TEST_PG_TLS_*)"]
    fn certificate_errors_name_their_reason() {
        let Some(t) = tls() else { return };
        let unknown = error_text(connect(&t.uri(
            "localhost",
            &format!("sslmode=verify-full&sslrootcert={}", t.other_ca),
        )));
        assert!(
            unknown.contains("unknown issuer") && unknown.contains("sslrootcert"),
            "{unknown}"
        );
        let mismatch = error_text(connect(&t.uri(
            "127.0.0.1",
            &format!("sslmode=verify-full&sslrootcert={}", t.ca),
        )));
        assert!(
            mismatch.contains("name mismatch") && mismatch.contains("127.0.0.1"),
            "{mismatch}"
        );
        let disabled = error_text(connect(&t.uri("127.0.0.1", "sslmode=disable")));
        assert!(
            disabled.contains("requires TLS") && disabled.contains("sslmode=disable"),
            "{disabled}"
        );
        // Nothing secret in any message.
        for message in [&unknown, &mismatch, &disabled] {
            assert!(!message.contains(&t.password), "{message}");
        }
    }

    #[test]
    #[ignore = "needs test-tls-servers.sh and TABLEX_TEST_PG_URI"]
    fn sslmode_matrix_against_the_plain_server() {
        let Some(t) = tls() else { return };
        let Some(base) = plain_uri() else { return };
        let uri = |query: &str| format!("{base}?{query}");
        for query in ["sslmode=disable", "sslmode=allow", "sslmode=prefer"] {
            let result = connect(&uri(query));
            assert_eq!(
                code(&result),
                "connected",
                "{query}: {:?}",
                result.as_ref().err()
            );
            let connection = result.unwrap();
            // `prefer` fell back because the server answered `N`: really clear text.
            assert!(connection.v2_tls_info().is_none(), "{query}");
            assert!(server_view(&connection).starts_with("false"), "{query}");
        }
        // `allow` with a wrong password reports the password error, not a TLS one.
        let wrong = base.replace("tablex:tablex@", "tablex:wrong-password@");
        let result = connect(&format!("{wrong}?sslmode=allow"));
        let text = error_text(result);
        assert!(text.contains("password authentication failed"), "{text}");
        for query in [
            "sslmode=require".to_owned(),
            format!("sslmode=verify-ca&sslrootcert={}", t.ca),
            format!("sslmode=verify-full&sslrootcert={}", t.ca),
        ] {
            let query = query.as_str();
            let result = connect(&uri(query));
            assert_eq!(
                code(&result),
                "connect.tls_required",
                "{query}: {:?}",
                result.as_ref().err()
            );
            let text = error_text(result);
            assert!(text.contains("does not support TLS"), "{text}");
        }
    }

    #[test]
    #[ignore = "needs test-tls-servers.sh (TABLEX_TEST_PG_TLS_*)"]
    fn hostaddr_connects_to_the_address_and_verifies_the_host() {
        let Some(t) = tls() else { return };
        let ca = &t.ca;
        // What an SSH tunnel does: socket to 127.0.0.1, certificate checked for db.tablex.test.
        let ok = connect(&t.uri(
            &t.name,
            &format!("sslmode=verify-full&sslrootcert={ca}&hostaddr=127.0.0.1"),
        ));
        let connection = ok.expect("verify-full against hostaddr");
        assert_eq!(connection.v2_tls_info().unwrap().verified, Verified::Full);
        assert_eq!(scalar(&connection, "SELECT 1"), "1");
        // A name the certificate does not carry is refused even though the address is right.
        let wrong = connect(&t.uri(
            "wrong.tablex.test",
            &format!("sslmode=verify-full&sslrootcert={ca}&hostaddr=127.0.0.1"),
        ));
        assert_eq!(code(&wrong), "connect.tls_certificate");
        assert!(error_text(wrong).contains("wrong.tablex.test"));
        // `host` that does not resolve works too when hostaddr is given (no DNS lookup of the name).
        assert_eq!(
            code(&connect(&t.uri(
                &t.name,
                &format!("sslmode=verify-ca&sslrootcert={ca}&hostaddr=127.0.0.1")
            ))),
            "connected"
        );
    }

    #[test]
    #[ignore = "needs test-tls-servers.sh (TABLEX_TEST_PG_TLS_*)"]
    fn client_certificate_authentication() {
        let Some(t) = tls() else { return };
        for (cert, key) in &t.client {
            let uri = format!(
                "postgres://{}@localhost:{}/{}?sslmode=verify-full&sslrootcert={}&sslcert={cert}&sslkey={key}",
                t.cert_user, t.port, t.db, t.ca
            );
            let connection = connect(&uri).unwrap_or_else(|e| panic!("{cert}: {}", e.message));
            assert_eq!(
                scalar(&connection, "SELECT current_user"),
                t.cert_user,
                "{key}"
            );
            assert_eq!(connection.v2_tls_info().unwrap().verified, Verified::Full);
        }
        // The cert role has no password: without a certificate the server refuses.
        let anonymous = format!(
            "postgres://{}@localhost:{}/{}?sslmode=require",
            t.cert_user, t.port, t.db
        );
        assert!(connect(&anonymous).is_err());
        // A password-protected key is a typed "not supported", and says how to proceed.
        let encrypted = format!(
            "postgres://{}@localhost:{}/{}?sslmode=require&sslcert={}&sslkey={}&sslpassword=whatever",
            t.cert_user, t.port, t.db, t.client[0].0, t.encrypted_key
        );
        let result = connect(&encrypted);
        assert_eq!(code(&result), "connect.tls_config");
        let text = error_text(result);
        assert!(
            text.contains("password protected") && !text.contains("whatever"),
            "{text}"
        );
        // Certificate and key that do not belong together.
        let mismatched = format!(
            "postgres://{}@localhost:{}/{}?sslmode=require&sslcert={}&sslkey={}",
            t.cert_user, t.port, t.db, t.client[0].0, t.client[1].1
        );
        assert_eq!(code(&connect(&mismatched)), "connect.tls_config");
    }

    #[test]
    #[ignore = "needs test-tls-servers.sh (TABLEX_TEST_PG_TLS_*)"]
    fn scram_channel_binding_works_over_tls() {
        let Some(t) = tls() else { return };
        // `channel_binding=require` only succeeds when SCRAM-SHA-256-PLUS was negotiated.
        let uri = t.uri(
            "localhost",
            &format!(
                "sslmode=verify-full&sslrootcert={}&channel_binding=require",
                t.ca
            ),
        );
        let connection = connect(&uri).unwrap_or_else(|e| panic!("{}", e.message));
        assert_eq!(scalar(&connection, "SELECT 1"), "1");
    }

    #[test]
    #[ignore = "needs test-tls-servers.sh (TABLEX_TEST_PG_TLS_*)"]
    fn a_running_query_is_cancelled_over_tls() {
        let Some(t) = tls() else { return };
        let uri = t.uri(
            &t.name,
            &format!(
                "sslmode=verify-full&sslrootcert={}&hostaddr=127.0.0.1",
                t.ca
            ),
        );
        let connection = connect(&uri).unwrap_or_else(|e| panic!("{}", e.message));
        let transport = connection.v2_transport();
        let handshakes = |transport: &Transport| {
            transport
                .tls()
                .unwrap()
                .state
                .handshakes
                .load(Ordering::Acquire)
        };
        let client = runtime::block_on(connection.v2_client(""))
            .unwrap()
            .unwrap();
        let state = std::sync::Arc::new(CancelState::new(transport.clone()));
        let armed = state.arm(OperationId(1), client.cancel_token());
        let before = handshakes(&transport);
        let sleeper = {
            let client = std::sync::Arc::clone(&client);
            std::thread::spawn(move || {
                runtime::block_on(async move { client.simple_query("SELECT pg_sleep(20)").await })
                    .unwrap()
            })
        };
        std::thread::sleep(Duration::from_millis(400));
        let started = std::time::Instant::now();
        state
            .cancel(OperationId(1))
            .expect("cancel request over TLS");
        let error = sleeper.join().unwrap().unwrap_err();
        assert_eq!(error.code(), Some(&SqlState::QUERY_CANCELED));
        assert!(started.elapsed() < Duration::from_secs(3));
        // The cancel request itself did a TLS handshake (verified against the real name).
        assert_eq!(
            handshakes(&transport),
            before + 1,
            "the cancel connection must be TLS"
        );
        // ... and was acknowledged by the server's close: nothing to settle.
        assert!(
            !state.stop_delivering(),
            "an acknowledged request leaves no settle wait"
        );
        drop(armed);
        assert_eq!(scalar(&connection, "SELECT 'still usable'"), "still usable");
    }
}
