//! TLS for the MySQL / MariaDB driver: URI parameters mapped onto
//! `mysql_async::SslOpts` (rustls, `ring` provider), with the same mode names
//! and semantics as the PostgreSQL driver.
//!
//! # Connection parameters (URI query)
//!
//! | parameter | meaning |
//! |---|---|
//! | `ssl-mode` / `sslmode` | `DISABLED`, `PREFERRED` (default), `REQUIRED`, `VERIFY_CA`, `VERIFY_IDENTITY`; case-insensitive. The PostgreSQL spellings `disable`, `allow`, `prefer`, `require`, `verify-ca`, `verify-full` are accepted too |
//! | `ssl-ca` | PEM (or DER) file with the CA certificate(s) to trust; **required by `VERIFY_CA`** |
//! | `ssl-cert`, `ssl-key` | client certificate chain and private key (PEM; PKCS#8, PKCS#1 or SEC1); both or neither. On Unix the key must not be readable by group or others (`0600`, or `0640` when root owns it; not checked on Windows) |
//! | `tls-server-name` | the name the certificate is verified for and sent as SNI, while the TCP connection goes to the URI host. This is the hook for the SSH tunnel: the URI says `127.0.0.1:<local port>`, `tls-server-name` says `db.example.com`. Under `VERIFY_CA` (chain only) it is the SNI / socket hint, not a name requirement |
//!
//! mysql_async's own `require_ssl`, `verify_ca`, `verify_identity` and
//! `built_in_roots` keep working when no `ssl-mode` is given (`require_ssl=true`
//! then means exactly what mysql_async documents; `ssl-mode` takes precedence
//! over all four). `require_ssl=true&verify_identity=false` without a CA file would
//! be `VERIFY_CA` against public roots and is refused like `VERIFY_CA`.
//! Conflicting duplicate TLS parameters (`ssl-mode` and `sslmode` count as one)
//! are an error.
//!
//! # Semantics (same as the PostgreSQL driver, libpq-style)
//!
//! * `DISABLED`: never TLS.
//! * `allow` (PG spelling only): clear text first; when the server refuses the
//!   plain attempt for lack of TLS (`require_secure_transport`, error 3159) retry
//!   with TLS. Any other server error (wrong password) is returned as it is.
//! * `PREFERRED` (default, like the MySQL client): TLS when the server offers it,
//!   clear text **only** when the server's handshake does not advertise TLS. A
//!   failed handshake or certificate is an error, never a downgrade.
//!   **`PREFERRED` (the default) protects against passive eavesdropping only**: an
//!   active attacker can strip the TLS capability from the server's greeting and
//!   read the password. Use `REQUIRED` or stronger when that matters.
//! * `REQUIRED`: TLS or error.
//! * `PREFERRED` and `REQUIRED` check the certificate chain (not the name) only
//!   when `ssl-ca` is given, as `sslmode=prefer` / `require` do in PostgreSQL.
//! * `VERIFY_CA`: chain against the CA file in `ssl-ca`, any host name.
//!   **`ssl-ca=<file>` is required**: public roots plus an unchecked name would let
//!   any publicly issued certificate pass, so the mode would authenticate nothing
//!   (the MySQL client refuses the same way). Without it: `connect.tls_config`
//!   "use VERIFY_IDENTITY to verify against the system trust store".
//! * `VERIFY_IDENTITY`: chain and host name, against `ssl-ca` or, without it, the
//!   built-in Mozilla roots plus the OS trust store.
//!
//! * **`socket=<path>`** (a local Unix socket): TLS is not used there (mysql_async
//!   dials the socket and never starts a handshake on it). `REQUIRED`, `VERIFY_CA`,
//!   `VERIFY_IDENTITY` and `require_ssl=true` are refused with `connect.tls_config`
//!   ("use ssl-mode=DISABLED or connect over TCP"); `PREFERRED`, `allow` and
//!   `DISABLED` run plain on the socket without attempting TLS, like libpq.
//! * The client certificate and key are read **once** when the URL is parsed (the key
//!   through one open handle whose mode was checked) and reused for every connection.
//!
//! Every connection of a kit connection (user session, catalog connection, the
//! `KILL QUERY` side connection) opens with the options that worked for the
//! first one.
//!
//! mysql_async's rustls backend always trusts the Mozilla roots (unless
//! `built_in_roots=false`); the OS roots are added for `VERIFY_IDENTITY` without
//! `ssl-ca` so a corporate CA installed in the OS store keeps working (the previous
//! native-tls backend read that store). They are loaded once per process
//! (`rustls-native-certs` honours `SSL_CERT_FILE` / `SSL_CERT_DIR` on Linux), and
//! certificates rustls cannot parse are dropped first, so one bad system
//! certificate does not break every verifying connect. A CA installed while the app
//! runs needs a restart.
//!
//! The rustls `ring` provider is installed as this image's process default on first
//! use (tolerating an existing one), so a future aws-lc feature in the dependency
//! graph cannot make `ClientConfig::builder()` panic or pick a provider silently.

use mysql_async::{ClientIdentity, DriverError, Error, IoError, Opts, SslOpts};
use rustls::RootCertStore;
use std::path::{Path, PathBuf};
use std::sync::OnceLock;

/// The `ssl-mode` of a connection.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum Mode {
    Disabled,
    /// PG spelling `allow`.
    Allow,
    Preferred,
    Required,
    VerifyCa,
    VerifyIdentity,
    /// No `ssl-mode`, but mysql_async's own `require_ssl` was given: its `Opts` win.
    Native,
}

impl Mode {
    fn name(self) -> &'static str {
        match self {
            Self::Disabled => "DISABLED",
            Self::Allow => "allow",
            Self::Preferred => "PREFERRED",
            Self::Required => "REQUIRED",
            Self::VerifyCa => "VERIFY_CA",
            Self::VerifyIdentity => "VERIFY_IDENTITY",
            Self::Native => "require_ssl",
        }
    }

    fn parse(value: &str) -> Result<Self, TlsConfigError> {
        Ok(
            match value.to_ascii_lowercase().replace('-', "_").as_str() {
                "disabled" | "disable" => Self::Disabled,
                "allow" => Self::Allow,
                "preferred" | "prefer" => Self::Preferred,
                "required" | "require" => Self::Required,
                "verify_ca" => Self::VerifyCa,
                "verify_identity" | "verify_full" => Self::VerifyIdentity,
                _ => return Err(TlsConfigError::InvalidMode(value.to_owned())),
            },
        )
    }
}

/// The TLS part of a connection URI.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct TlsParams {
    pub(super) mode: Mode,
    /// `ssl-mode` / `sslmode` was in the URL (otherwise mysql_async's own switches may decide).
    explicit_mode: bool,
    ca: Option<PathBuf>,
    client: Option<ClientMaterial>,
    server_name: Option<String>,
}

/// The client certificate and key, read **once** when the URL is parsed (the key through
/// a single open handle whose mode was checked) and handed to mysql_async as buffers, so
/// the session, catalog and `KILL QUERY` connections all use the material that was
/// checked, not whatever the path holds later. The key never shows up in `Debug`.
#[derive(Clone, PartialEq, Eq)]
struct ClientMaterial {
    cert: Vec<u8>,
    key: Vec<u8>,
}

impl std::fmt::Debug for ClientMaterial {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("ClientMaterial(..)")
    }
}

/// A TLS setting that cannot be used (`connect.tls_config`). Never carries key
/// material; file paths are the user's own input.
#[derive(Debug, PartialEq, Eq)]
pub(super) enum TlsConfigError {
    InvalidMode(String),
    /// `VERIFY_CA` (or mysql_async's `verify_identity=false`) without a CA file.
    VerifyCaNeedsCaFile,
    IncompleteClientIdentity,
    ConflictingParameter(&'static str),
    EmptyFile {
        what: &'static str,
        path: String,
    },
    KeyFilePermissions {
        path: String,
        mode: u32,
    },
    /// `socket=`: mysql_async dials the local socket and never starts TLS there.
    TlsOnSocket(&'static str),
    File {
        what: &'static str,
        path: String,
        reason: String,
    },
    EncryptedPrivateKey {
        path: String,
    },
}

impl std::fmt::Display for TlsConfigError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::InvalidMode(value) => write!(
                f,
                "invalid ssl-mode '{value}' (use DISABLED, PREFERRED, REQUIRED, VERIFY_CA or VERIFY_IDENTITY)"
            ),
            Self::VerifyCaNeedsCaFile => f.write_str(
                "VERIFY_CA needs ssl-ca=<CA file>; use VERIFY_IDENTITY to verify against the system trust store",
            ),
            Self::IncompleteClientIdentity => {
                f.write_str("ssl-cert and ssl-key must be given together")
            }
            Self::ConflictingParameter(name) => {
                write!(f, "the URL gives {name} more than once with different values")
            }
            Self::TlsOnSocket(mode) => write!(
                f,
                "TLS is not used on a Unix socket (ssl-mode={mode}); use ssl-mode=DISABLED or connect over TCP"
            ),
            Self::EmptyFile { what, path } => write!(f, "{what} '{path}' is empty"),
            Self::KeyFilePermissions { path, mode } => write!(
                f,
                "the private key '{path}' has permissions {mode:04o}; it must not be accessible by group or others (chmod 600)"
            ),
            Self::File { what, path, reason } => write!(f, "cannot read {what} '{path}': {reason}"),
            Self::EncryptedPrivateKey { path } => write!(
                f,
                "the private key '{path}' is password protected, which is not supported; decrypt it \
                 (for example `openssl pkcs8 -in key.pem -out key-plain.pem`) and keep the file private"
            ),
        }
    }
}

impl TlsParams {
    /// Splits the normalized `mysql://` URL into the URL mysql_async can parse
    /// and the TLS parameters. The parameters read here are removed; everything
    /// else is kept verbatim. Reads `ssl-ca`, `ssl-cert` and `ssl-key` once to
    /// fail early (and typed) instead of inside the handshake.
    pub(super) fn from_url(url: &str) -> Result<(String, Self), TlsConfigError> {
        let (base, query) = url.split_once('?').map_or((url, ""), |(b, q)| (b, q));
        let (mut mode, mut ca, mut cert, mut key, mut server_name) = (None, None, None, None, None);
        let mut native = false;
        let mut kept = Vec::new();
        for pair in query.split('&').filter(|pair| !pair.is_empty()) {
            let (name, value) = pair.split_once('=').unwrap_or((pair, ""));
            // mysql_async decodes parameter names, so `require%5Fssl` is `require_ssl`.
            let name = percent_encoding::percent_decode_str(name).decode_utf8_lossy();
            let (slot, label) = match name.as_ref() {
                "ssl-mode" | "sslmode" => (&mut mode, "ssl-mode"),
                "ssl-ca" => (&mut ca, "ssl-ca"),
                "ssl-cert" => (&mut cert, "ssl-cert"),
                "ssl-key" => (&mut key, "ssl-key"),
                "tls-server-name" => (&mut server_name, "tls-server-name"),
                other => {
                    native |= other == "require_ssl";
                    kept.push(pair);
                    continue;
                }
            };
            let decoded = percent_encoding::percent_decode_str(value).decode_utf8_lossy();
            match slot {
                Some(previous) if *previous != decoded => {
                    return Err(TlsConfigError::ConflictingParameter(label))
                }
                _ => *slot = Some(decoded.into_owned()),
            }
        }
        let unset = |value: Option<String>| value.filter(|value| !value.is_empty());
        let mode = unset(mode);
        let explicit_mode = mode.is_some();
        let mode = match mode {
            Some(value) => Mode::parse(&value)?,
            None if native => Mode::Native,
            None => Mode::Preferred,
        };
        let client_paths = match (unset(cert), unset(key)) {
            (Some(cert), Some(key)) => Some((PathBuf::from(cert), PathBuf::from(key))),
            (None, None) => None,
            _ => return Err(TlsConfigError::IncompleteClientIdentity),
        };
        let mut params = Self {
            mode,
            explicit_mode,
            ca: unset(ca).map(PathBuf::from),
            client: None,
            server_name: unset(server_name),
        };
        // Public roots would make a name-blind check meaningless.
        if mode == Mode::VerifyCa && params.ca.is_none() {
            return Err(TlsConfigError::VerifyCaNeedsCaFile);
        }
        if mode != Mode::Disabled {
            params.check_ca()?;
            params.client = client_paths
                .map(|(cert, key)| read_client_material(&cert, &key))
                .transpose()?;
        }
        let cleaned = if kept.is_empty() {
            base.to_owned()
        } else {
            format!("{base}?{}", kept.join("&"))
        };
        Ok((cleaned, params))
    }

    /// Settles what mysql_async's own parse of the cleaned URL adds: its `require_ssl`
    /// decides when no `ssl-mode` was given (whatever spelling this reader missed),
    /// and a strict native option is never replaced by an accept-all one.
    pub(super) fn reconcile(&mut self, opts: &Opts) -> Result<(), TlsConfigError> {
        if !self.explicit_mode && opts.ssl_opts().is_some() {
            self.mode = Mode::Native;
        }
        // mysql_async dials the socket and its `make_secure` is a no-op there: a mode that
        // must be encrypted would proceed unencrypted. Refuse it; PREFERRED / DISABLED run
        // plain on the socket without attempting TLS (see `attempts`).
        if opts.socket().is_some() {
            let needs_tls = match self.mode {
                Mode::Required | Mode::VerifyCa | Mode::VerifyIdentity => true,
                Mode::Native => opts.ssl_opts().is_some(),
                Mode::Disabled | Mode::Allow | Mode::Preferred => false,
            };
            if needs_tls {
                return Err(TlsConfigError::TlsOnSocket(self.mode.name()));
            }
        }
        if self.mode == Mode::Native {
            if let Some(ssl) = opts.ssl_opts() {
                let pinned = self.ca.is_some() || !ssl.root_certs().is_empty();
                if ssl.skip_domain_validation() && !ssl.accept_invalid_certs() && !pinned {
                    // `verify_identity=false` with the built-in roots: VERIFY_CA against public CAs.
                    return Err(TlsConfigError::VerifyCaNeedsCaFile);
                }
            }
        }
        Ok(())
    }

    fn check_ca(&self) -> Result<(), TlsConfigError> {
        // Bytes, not text: a CA may be DER (mysql_async accepts PEM or DER).
        if let Some(ca) = &self.ca {
            let content = std::fs::read(ca).map_err(|error| TlsConfigError::File {
                what: "ssl-ca",
                path: ca.display().to_string(),
                reason: error.kind().to_string(),
            })?;
            if content.is_empty() {
                return Err(TlsConfigError::EmptyFile {
                    what: "ssl-ca",
                    path: ca.display().to_string(),
                });
            }
        }
        Ok(())
    }

    /// `SslOpts` for the TLS attempt of this mode on top of mysql_async's own
    /// (`native`, from `require_ssl=true`).
    fn ssl_opts(&self, native: Option<&SslOpts>) -> SslOpts {
        let mut ssl = match self.mode {
            Mode::Native => native.cloned().unwrap_or_default(),
            _ => SslOpts::default(),
        };
        let verify_chain = match self.mode {
            Mode::VerifyCa | Mode::VerifyIdentity => true,
            // libpq: with a root certificate on file, `require` / `prefer` check the chain.
            Mode::Allow | Mode::Preferred | Mode::Required => self.ca.is_some(),
            Mode::Disabled | Mode::Native => false,
        };
        if self.mode != Mode::Native {
            ssl = ssl
                .with_danger_accept_invalid_certs(!verify_chain)
                .with_danger_skip_domain_validation(self.mode != Mode::VerifyIdentity);
        }
        if let Some(ca) = &self.ca {
            ssl = ssl
                .with_root_certs(vec![ca.clone().into()])
                .with_disable_built_in_roots(true);
        } else if self.mode == Mode::VerifyIdentity {
            // `VERIFY_CA` never gets here (it needs `ssl-ca`); only the identity check
            // may rely on public and OS roots.
            ssl = ssl.with_root_certs(system_roots().iter().cloned().map(Into::into).collect());
        }
        if let Some(client) = &self.client {
            ssl = ssl.with_client_identity(Some(ClientIdentity::new(
                client.cert.clone().into(),
                client.key.clone().into(),
            )));
        }
        if let Some(name) = &self.server_name {
            ssl = ssl.with_danger_tls_hostname_override(Some(name.clone()));
        }
        ssl
    }

    /// The `Opts` to try, in order, and when to move to the next one.
    fn attempts(&self, opts: &Opts) -> Vec<Opts> {
        let with = |ssl: Option<SslOpts>| -> Opts {
            mysql_async::OptsBuilder::from_opts(opts.clone())
                .ssl_opts(ssl)
                .into()
        };
        let tls = || with(Some(self.ssl_opts(opts.ssl_opts())));
        if opts.socket().is_some() {
            // `reconcile` refused every mode that needs TLS here.
            return vec![with(None)];
        }
        match self.mode {
            Mode::Disabled => vec![with(None)],
            Mode::Allow => vec![with(None), tls()],
            Mode::Preferred => vec![tls(), with(None)],
            Mode::Native => vec![with(
                opts.ssl_opts().map(|native| self.ssl_opts(Some(native))),
            )],
            Mode::Required | Mode::VerifyCa | Mode::VerifyIdentity => vec![tls()],
        }
    }

    /// Whether the failure of attempt `index` lets the next attempt run.
    fn falls_through(&self, error: &Error) -> bool {
        match self.mode {
            // The server did not advertise TLS in its handshake: nothing was sent yet.
            Mode::Preferred => {
                matches!(error, Error::Driver(DriverError::NoClientSslFlagFromServer))
            }
            // `require_secure_transport` answers the plain handshake with error 3159;
            // any other server error (wrong password) is that attempt's answer.
            Mode::Allow => refused_for_lack_of_tls(error),
            _ => false,
        }
    }

    /// Connects with the first attempt that is not refused for a reason the mode
    /// allows to fall through. Returns the connection and the `Opts` that worked
    /// (the catalog and `KILL QUERY` connections reuse them).
    pub(super) async fn open(&self, opts: &Opts) -> Result<(mysql_async::Conn, Opts), Error> {
        ensure_provider();
        let mut attempts = self.attempts(opts).into_iter().peekable();
        loop {
            let attempt = attempts.next().expect("a mode has at least one attempt");
            match connect_ignoring_the_name(attempt).await {
                Ok(connected) => return Ok(connected),
                Err(error) if attempts.peek().is_some() && self.falls_through(&error) => {}
                Err(error) => return Err(error),
            }
        }
    }
}

/// `Conn::new`, honouring `skip_domain_validation`.
///
/// **Workaround for a mysql_async 0.37.1 bug.** Its rustls backend implements
/// `skip_domain_validation` by looking for the text `NotValidForName` in the
/// *Display* of the verification error; rustls 0.23 prints that error in words
/// ("certificate not valid for name ..."), so the option does nothing and
/// `VERIFY_CA` / `REQUIRED` + `ssl-ca` would refuse every certificate whose names
/// differ from the host. There is no hook to supply our own verifier.
///
/// What this does instead: the WebPKI verifier checks the chain *before* the name,
/// so a name mismatch proves the chain is good. The error lists the names the
/// certificate does carry; the one connection retries once with
/// `tls_hostname_override` set to the first of them, which makes the (repeated)
/// chain check and the name check both pass. Nothing is weakened: the retry
/// verifies the same chain against the same roots and a name taken from the
/// certificate itself; a certificate without a usable name keeps failing.
/// Cost: one extra handshake, only in the mismatch case. Regression test:
/// `a_name_mismatch_is_retried_with_a_name_from_the_certificate` and the live
/// `VERIFY_CA` cases.
async fn connect_ignoring_the_name(attempt: Opts) -> Result<(mysql_async::Conn, Opts), Error> {
    let error = match mysql_async::Conn::new(attempt.clone()).await {
        Ok(conn) => return Ok((conn, attempt)),
        Err(error) => error,
    };
    let Some(name) = name_to_accept(&attempt, &error) else {
        return Err(error);
    };
    let ssl = attempt
        .ssl_opts()
        .cloned()
        .map(|ssl| ssl.with_danger_tls_hostname_override(Some(name)));
    let retry: Opts = mysql_async::OptsBuilder::from_opts(attempt)
        .ssl_opts(ssl)
        .into();
    mysql_async::Conn::new(retry.clone())
        .await
        .map(|conn| (conn, retry))
}

/// The name of the certificate to retry with, when `error` is a name mismatch
/// that `attempt` asked to ignore (see [`connect_ignoring_the_name`]).
fn name_to_accept(attempt: &Opts, error: &Error) -> Option<String> {
    let ssl = attempt.ssl_opts()?;
    // Chain checked, name not (`VERIFY_CA`, `REQUIRED` with a CA): a `tls-server-name`
    // there is the SNI / socket hint, not a name requirement, so it is replaced too.
    if ssl.accept_invalid_certs() || !ssl.skip_domain_validation() {
        return None;
    }
    let rustls::Error::InvalidCertificate(rustls::CertificateError::NotValidForNameContext {
        presented,
        ..
    }) = rustls_error(error)?
    else {
        return None;
    };
    presented.iter().find_map(|entry| {
        // rustls-webpki lists the subject alternative names with its `Debug`
        // output: `DnsName("db.example")`, `IpAddress(10.0.0.5)`. Anything else
        // (the format is not API) means no retry, i.e. the mismatch stays an error.
        let name = entry
            .strip_prefix("DnsName(\"")
            .and_then(|rest| rest.strip_suffix("\")"))
            .or_else(|| entry.strip_prefix("IpAddress(")?.strip_suffix(')'))?;
        // A wildcard name is matched by any single label in its place.
        let name = name
            .strip_prefix("*.")
            .map_or_else(|| name.to_owned(), |rest| format!("host.{rest}"));
        rustls::pki_types::ServerName::try_from(name.as_str())
            .is_ok()
            .then_some(name)
    })
}

/// A rustls error somewhere inside `error`, if the failure was a TLS one.
fn rustls_error(error: &Error) -> Option<&rustls::Error> {
    let mut source: Option<&(dyn std::error::Error + 'static)> = Some(error);
    while let Some(cause) = source {
        if let Some(found) = cause.downcast_ref::<rustls::Error>() {
            return Some(found);
        }
        if let Some(io) = cause.downcast_ref::<std::io::Error>() {
            if let Some(found) = io
                .get_ref()
                .and_then(|inner| inner.downcast_ref::<rustls::Error>())
            {
                return Some(found);
            }
        }
        source = cause.source();
    }
    None
}

/// Maps a failed connect to the stable TLS codes; `None` when TLS is not the story.
pub(super) fn connect_error(
    error: &Error,
    params: &TlsParams,
) -> Option<engine_driver_kit::BackendError> {
    use engine_driver_kit::BackendError;
    // Building the connector failed (bad PEM, unusable client key): our setting, not the server.
    if let Error::Io(IoError::Tls(tls)) = error {
        return Some(BackendError::with_code(
            "connect.tls_config",
            format!("invalid TLS setting: {tls}"),
        ));
    }
    if let Some(found) = rustls_error(error) {
        return Some(match found {
            rustls::Error::InvalidCertificate(problem) => {
                use rustls::CertificateError as C;
                let (reason, advice) = match problem {
                    C::UnknownIssuer => (
                        "unknown issuer",
                        "; its CA is not trusted: pass ssl-ca=<ca.pem> (or use ssl-mode=REQUIRED to skip verification)",
                    ),
                    C::NotValidForName | C::NotValidForNameContext { .. } => (
                        "name mismatch",
                        "; the certificate does not name this host: use tls-server-name=<name in the certificate>, or ssl-mode=VERIFY_CA",
                    ),
                    C::Expired | C::ExpiredContext { .. } => (
                        "expired",
                        "; check the server certificate dates and this computer's clock",
                    ),
                    C::NotValidYet | C::NotValidYetContext { .. } => (
                        "not yet valid",
                        "; check the server certificate dates and this computer's clock",
                    ),
                    C::Revoked => ("revoked", ""),
                    _ => ("invalid certificate", ""),
                };
                BackendError::with_code(
                    "connect.tls_certificate",
                    format!("server certificate rejected: {reason}{advice}"),
                )
            }
            other => BackendError::with_code(
                "connect.tls_handshake",
                format!("TLS handshake failed: {other}"),
            ),
        });
    }
    match error {
        Error::Driver(DriverError::NoClientSslFlagFromServer) => Some(BackendError::with_code(
            "connect.tls_required",
            "the server does not support TLS but ssl-mode requires it; use ssl-mode=PREFERRED or DISABLED only if the connection may be unencrypted",
        )),
        Error::Server(_) if refused_for_lack_of_tls(error) => {
            Some(BackendError::with_code(
                "connect.tls_required",
                format!(
                    "the server requires TLS (require_secure_transport) but ssl-mode is {}; use ssl-mode=REQUIRED, VERIFY_CA or VERIFY_IDENTITY",
                    params.mode.name()
                ),
            ))
        }
        _ => None,
    }
}

/// `ER_SECURE_TRANSPORT_REQUIRED` (MySQL and MariaDB): clear text refused for lack of
/// TLS. Classified by the code, never by the (localizable) message.
const ER_SECURE_TRANSPORT_REQUIRED: u16 = 3159;

/// Clear text was refused for lack of TLS.
fn refused_for_lack_of_tls(error: &Error) -> bool {
    matches!(error, Error::Server(server) if server.code == ER_SECURE_TRANSPORT_REQUIRED)
}

/// The `ring` provider as this image's process default (tolerating an existing one).
fn ensure_provider() {
    static INSTALLED: OnceLock<()> = OnceLock::new();
    INSTALLED.get_or_init(|| {
        let _ = rustls::crypto::ring::default_provider().install_default();
    });
}

/// The DER of every certificate in `certs` that rustls accepts as a trust anchor
/// (mysql_async adds them strictly: one bad certificate would fail every connect).
fn usable_roots(certs: Vec<rustls::pki_types::CertificateDer<'static>>) -> Vec<Vec<u8>> {
    let mut scratch = RootCertStore::empty();
    certs
        .into_iter()
        .filter(|cert| scratch.add(cert.clone()).is_ok())
        .map(|cert| cert.as_ref().to_vec())
        .collect()
}

/// The OS trust store, filtered and loaded once per process.
fn system_roots() -> &'static [Vec<u8>] {
    static ROOTS: OnceLock<Vec<Vec<u8>>> = OnceLock::new();
    ROOTS.get_or_init(|| usable_roots(rustls_native_certs::load_native_certs().certs))
}

/// libpq: `u=rw` (0600) or less for the owner's own key, `u=rw,g=r` (0640) or less when root owns it.
#[cfg(unix)]
fn key_mode_allowed(owned_by_root: bool, mode: u32) -> bool {
    mode & if owned_by_root { 0o037 } else { 0o077 } == 0
}

/// Reads the client certificate and key once. The key is opened once, its mode checked on
/// the open handle (no check-then-read race on the path) and read from that handle: a key
/// that group or others can read is refused (Unix; not checked on Windows).
fn read_client_material(cert: &Path, key: &Path) -> Result<ClientMaterial, TlsConfigError> {
    use std::io::Read;
    let cert_bytes = std::fs::read(cert).map_err(|error| TlsConfigError::File {
        what: "ssl-cert",
        path: cert.display().to_string(),
        reason: error.kind().to_string(),
    })?;
    let key_error = |error: std::io::Error| TlsConfigError::File {
        what: "ssl-key",
        path: key.display().to_string(),
        reason: error.kind().to_string(),
    };
    let mut file = std::fs::File::open(key).map_err(key_error)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        let metadata = file.metadata().map_err(key_error)?;
        let mode = metadata.mode() & 0o777;
        if !key_mode_allowed(metadata.uid() == 0, mode) {
            return Err(TlsConfigError::KeyFilePermissions {
                path: key.display().to_string(),
                mode,
            });
        }
    }
    let mut key_bytes = Vec::new();
    file.read_to_end(&mut key_bytes).map_err(key_error)?;
    // `ENCRYPTED PRIVATE KEY` (PKCS#8) and `Proc-Type: 4,ENCRYPTED` (legacy PEM).
    if String::from_utf8_lossy(&key_bytes).contains("ENCRYPTED") {
        return Err(TlsConfigError::EncryptedPrivateKey {
            path: key.display().to_string(),
        });
    }
    Ok(ClientMaterial {
        cert: cert_bytes,
        key: key_bytes,
    })
}

impl From<TlsConfigError> for engine_driver_kit::BackendError {
    fn from(error: TlsConfigError) -> Self {
        Self::with_code(
            "connect.tls_config",
            format!("invalid TLS setting: {error}"),
        )
    }
}

#[cfg(test)]
#[path = "tls_tests.rs"]
mod tests;
