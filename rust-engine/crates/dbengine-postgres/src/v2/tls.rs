//! TLS for the PostgreSQL v2 driver: libpq `sslmode` semantics on rustls.
//!
//! This module replaces `dbengine/src/db/pg_transport.rs` (plain TCP) in the
//! `abi-v2` build; the shared `PostgresConn` source calls the same three
//! functions in both ([`Transport::prepare`], [`Transport::connect`],
//! [`Transport::cancel`]), so the user session, the per-database clients, the
//! mutation connections and the cancel side channel all use one configuration.
//!
//! # Connection parameters (URI query)
//!
//! | parameter | meaning |
//! |---|---|
//! | `sslmode` | `disable`, `allow`, `prefer` (default), `require`, `verify-ca`, `verify-full` |
//! | `sslrootcert` | PEM file with one or more CA certificates, or `system` (the OS trust store; `verify-full` only) |
//! | `sslcert`, `sslkey` | client certificate chain and private key (PEM; PKCS#8, PKCS#1 `RSA PRIVATE KEY` or SEC1 `EC PRIVATE KEY`); both or neither |
//! | `sslpassword` | accepted only for an unencrypted key; an encrypted key is `connect.tls_config` "not supported" |
//! | `hostaddr` | socket address (parsed by tokio-postgres): the TCP connection goes there while the certificate is checked against `host`; needs a TCP `host` (not a Unix socket path, not absent) when TLS is on |
//!
//! Nothing is read implicitly (`~/.postgresql/root.crt`, `PGSSL*` variables): the
//! saved connection is the whole configuration. A URI whose "credentials" would
//! contain `/` (an unescaped `@` in a query value, which tokio-postgres reads as the
//! end of the credentials) is refused. `TlsInfo` is the **session** connection's
//! handshake, never a later per-database connection's. The one environment influence is
//! the OS trust store `verify-full` uses without `sslrootcert`:
//! `rustls-native-certs` honours `SSL_CERT_FILE` / `SSL_CERT_DIR` on Linux and
//! loads it once per process. Unsupported and refused with `connect.tls_config`:
//! `sslnegotiation=direct`, an `sslmode` that only tokio-postgres sees, conflicting
//! duplicate TLS parameters, a client key readable by group/others on Unix (libpq's
//! rule: `0600`, or `0640` when owned by root; not checked on Windows).
//!
//! # Semantics (libpq)
//!
//! * `disable`: never TLS.
//! * `allow`: plain first; when the server refuses the plain attempt through
//!   `pg_hba.conf` (SQLSTATE `28000`, which is what a `hostssl`-only file answers; a
//!   wrong password is `28P01`; the localized message is never read) retry with TLS.
//!   Any other server error is returned as it is. A failure after the TLS attempt is
//!   final.
//! * `prefer`: TLS, falling back to clear text **only** when the server answers
//!   `SSLRequest` with `N`. A failed handshake or certificate is an error, never a
//!   downgrade. Once a session negotiated TLS, every later connection of it
//!   (per-database, metadata, mutation) and the cancel request insist on TLS and
//!   fail instead of silently going clear text (the metadata connection shares the
//!   session's transport, so the pin covers it). With several hosts the decision is
//!   made host by host: a host whose handshake or certificate failed is never retried
//!   in clear text, a host answering `N` may be used in clear text only while the
//!   session has not used TLS. **`prefer` and `PREFERRED` (the
//!   defaults) protect against passive eavesdropping only**: an active attacker can
//!   answer `N` to the first connection and read the password. Use `require` or
//!   stronger when that matters.
//! * `require`: TLS or error. The server certificate is verified (chain, not
//!   name) only when `sslrootcert` names a file, as libpq does.
//! * `verify-ca`: chain against the CA file in `sslrootcert`, any name.
//!   **`sslrootcert=<file>` is required**: public roots would let any publicly
//!   issued certificate pass an unchecked name, so the mode would authenticate
//!   nothing (libpq 16+ refuses the same way). Without it: `connect.tls_config`
//!   "use verify-full to verify against the system trust store".
//! * `verify-full`: chain and host name, against `sslrootcert=<file>` or, without
//!   it (or with `sslrootcert=system`), the OS trust store.
//!
//! `sslrootcert=system` with any other mode is refused.
//!
//! # Choices
//!
//! * **Own connector instead of `tokio-postgres-rustls`:** the driver reports the
//!   negotiated version/cipher and how far the server was verified, which the
//!   crate's opaque stream cannot give, and the crate adds `x509-cert` and its
//!   tree. Channel binding (`tls-server-end-point`, SCRAM-SHA-256-PLUS) needs only
//!   the certificate's signature-algorithm OID and a hash, done here in a few lines
//!   on `sha2`, already in the tree through `postgres-protocol`.
//! * **Roots:** `rustls-native-certs` (macOS Security.framework, Windows store,
//!   Linux `SSL_CERT_FILE`/`SSL_CERT_DIR`/system bundles), the loader the Redis and
//!   MongoDB drivers already ship; used by `verify-full` only, loaded once per
//!   process (a CA installed while the app runs needs a restart).
//! * **Verification** calls rustls' public `verify_server_cert_signed_by_trust_anchor`
//!   and, for `verify-full`, `verify_server_name`; `verify-ca` never depends on the
//!   order of rustls' internal checks.
//! * **Provider:** `ring`, passed explicitly (no process-global default is installed
//!   from inside a plugin); TLS 1.2 and 1.3.
//! * SNI is sent for DNS names (rustls omits it for IP literals).

use rustls::client::danger::{HandshakeSignatureValid, ServerCertVerified, ServerCertVerifier};
use rustls::client::{verify_server_cert_signed_by_trust_anchor, verify_server_name};
use rustls::crypto::{self, CryptoProvider};
use rustls::pki_types::pem::PemObject;
use rustls::pki_types::{CertificateDer, PrivateKeyDer, ServerName, UnixTime};
use rustls::server::ParsedCertificate;
use rustls::{
    CertificateError, ClientConfig, DigitallySignedStruct, RootCertStore, SignatureScheme,
};
use serde::Serialize;
use sha2::{Digest, Sha256, Sha384, Sha512};
use std::error::Error as StdError;
use std::fmt;
use std::future::Future;
use std::io;
use std::net::IpAddr;
use std::path::{Path, PathBuf};
use std::pin::Pin;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, OnceLock, PoisonError};
use std::task::{ready, Context, Poll};
use std::time::Duration;
use tokio::io::{AsyncRead, AsyncWrite, ReadBuf};
use tokio_postgres::config::{LoadBalanceHosts, SslMode as PgSslMode, SslNegotiation};
use tokio_postgres::error::SqlState;
use tokio_postgres::tls::{ChannelBinding, MakeTlsConnect, TlsConnect, TlsStream};
use tokio_postgres::{CancelToken, Client, Config, NoTls, Socket};

use engine_driver_kit::BackendError;

// ---- parameters -----------------------------------------------------------------------------------------

/// libpq `sslmode`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum SslMode {
    Disable,
    Allow,
    Prefer,
    Require,
    VerifyCa,
    VerifyFull,
}

impl SslMode {
    fn parse(value: &str) -> Result<Self, TlsConfigError> {
        Ok(match value {
            "disable" => Self::Disable,
            "allow" => Self::Allow,
            "prefer" => Self::Prefer,
            "require" => Self::Require,
            "verify-ca" => Self::VerifyCa,
            "verify-full" => Self::VerifyFull,
            other => return Err(TlsConfigError::InvalidSslMode(other.to_owned())),
        })
    }
}

/// Where trusted CA certificates come from.
#[derive(Debug, Clone, PartialEq, Eq)]
enum RootCerts {
    File(PathBuf),
    System,
}

/// The TLS-relevant part of a connection URI.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct TlsSettings {
    mode: SslMode,
    roots: Option<RootCerts>,
    client: Option<(PathBuf, PathBuf)>,
}

/// How far the server's certificate was checked.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub(crate) enum Verified {
    /// Encrypted, certificate not checked.
    None,
    /// Chain checked against the trusted roots, name not checked.
    Ca,
    /// Chain and host name checked.
    Full,
}

/// Where tokio-postgres puts the start of the query: after the credentials,
/// which it cuts at the first `@` of the whole string (even one inside the
/// query), so this splitter has to agree. Credentials containing `/` are refused:
/// that is an unescaped `@` in a query value (or the path) that would move the host.
fn split_query(uri: &str) -> Result<(&str, &str), TlsConfigError> {
    let prefix = ["postgres://", "postgresql://"]
        .into_iter()
        .find(|prefix| uri.starts_with(prefix))
        .map_or(0, str::len);
    let after_credentials = match uri[prefix..].find('@') {
        Some(at) if uri[prefix..prefix + at].contains('/') => {
            return Err(TlsConfigError::UnescapedAtSign)
        }
        Some(at) => prefix + at + 1,
        None => prefix,
    };
    Ok(match uri[after_credentials..].find('?') {
        Some(question) => {
            let question = after_credentials + question;
            (&uri[..question], &uri[question + 1..])
        }
        None => (uri, ""),
    })
}

/// Records `value` in `slot`; a second, different value is an error (an empty
/// value counts as a value: `sslmode=verify-full&sslmode=` must not quietly mean
/// "default").
fn record(
    slot: &mut Option<String>,
    name: &'static str,
    value: String,
) -> Result<(), TlsConfigError> {
    match slot {
        Some(previous) if *previous != value => Err(TlsConfigError::ConflictingParameter(name)),
        _ => {
            *slot = Some(value);
            Ok(())
        }
    }
}

impl TlsSettings {
    /// Splits `uri` into the URI tokio-postgres can parse and the TLS settings.
    /// The parameters read here are removed; everything else is kept verbatim.
    pub(crate) fn from_uri(uri: &str) -> Result<(String, Self), TlsConfigError> {
        let (base, query) = split_query(uri)?;
        let (mut mode, mut root, mut cert, mut key) = (None, None, None, None);
        let mut kept = Vec::new();
        for pair in query.split('&').filter(|pair| !pair.is_empty()) {
            let (name, value) = pair.split_once('=').unwrap_or((pair, ""));
            let name = percent_encoding::percent_decode_str(name).decode_utf8_lossy();
            let (slot, label) = match name.as_ref() {
                "sslmode" => (&mut mode, "sslmode"),
                "sslrootcert" => (&mut root, "sslrootcert"),
                "sslcert" => (&mut cert, "sslcert"),
                "sslkey" => (&mut key, "sslkey"),
                // Never stored: only its presence matters (see `read_client_identity`).
                "sslpassword" => continue,
                _ => {
                    kept.push(pair);
                    continue;
                }
            };
            let decoded = percent_encoding::percent_decode_str(value).decode_utf8_lossy();
            record(slot, label, decoded.into_owned())?;
        }
        let unset = |value: Option<String>| value.filter(|value| !value.is_empty());
        let mode = unset(mode)
            .as_deref()
            .map_or(Ok(SslMode::Prefer), SslMode::parse)?;
        let roots = unset(root).map(|root| match root.as_str() {
            "system" => RootCerts::System,
            _ => RootCerts::File(PathBuf::from(root)),
        });
        match (mode, &roots) {
            // Public roots would make a name-blind check meaningless.
            (SslMode::VerifyCa, None | Some(RootCerts::System)) => {
                return Err(TlsConfigError::VerifyCaNeedsCaFile)
            }
            (SslMode::VerifyFull, _) => {}
            (_, Some(RootCerts::System)) => {
                return Err(TlsConfigError::SystemRootsNeedVerifyFull(mode))
            }
            _ => {}
        }
        let client = match (unset(cert), unset(key)) {
            (Some(cert), Some(key)) => Some((PathBuf::from(cert), PathBuf::from(key))),
            (None, None) => None,
            _ => return Err(TlsConfigError::IncompleteClientIdentity),
        };
        let cleaned = if kept.is_empty() {
            base.to_owned()
        } else {
            format!("{base}?{}", kept.join("&"))
        };
        Ok((
            cleaned,
            Self {
                mode,
                roots,
                client,
            },
        ))
    }

    /// What a successful handshake proves about the server.
    fn verified(&self) -> Verified {
        match (self.mode, &self.roots) {
            (SslMode::VerifyFull, _) => Verified::Full,
            (SslMode::VerifyCa, _) | (_, Some(_)) => Verified::Ca,
            _ => Verified::None,
        }
    }
}

// ---- errors ---------------------------------------------------------------------------------------------

/// A TLS setting that cannot be used (`connect.tls_config`). Never carries key
/// material or the `sslpassword`; file paths are the user's own input.
#[derive(Debug, PartialEq, Eq)]
pub(crate) enum TlsConfigError {
    InvalidSslMode(String),
    /// `verify-ca` without a CA file (or with `sslrootcert=system`).
    VerifyCaNeedsCaFile,
    SystemRootsNeedVerifyFull(SslMode),
    IncompleteClientIdentity,
    ConflictingParameter(&'static str),
    /// tokio-postgres found an `sslmode` / `sslnegotiation` this driver did not.
    UnrecognizedSslSetting,
    UnsupportedSslNegotiation,
    UnixHostWithHostaddr,
    HostaddrWithoutHost,
    /// An `@` after the host (in a query value): tokio-postgres would read it as the end of the credentials.
    UnescapedAtSign,
    KeyFilePermissions {
        path: String,
        mode: u32,
    },
    File {
        what: &'static str,
        path: String,
        reason: String,
    },
    NoCertificates {
        path: String,
    },
    NoPrivateKey {
        path: String,
    },
    EncryptedPrivateKey {
        path: String,
    },
    NoTrustAnchors,
    ClientIdentity(String),
}

impl fmt::Display for TlsConfigError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::InvalidSslMode(value) => write!(
                f,
                "invalid sslmode '{value}' (use disable, allow, prefer, require, verify-ca or verify-full)"
            ),
            Self::VerifyCaNeedsCaFile => f.write_str(
                "verify-ca needs sslrootcert=<CA file>; use verify-full to verify against the system trust store",
            ),
            Self::SystemRootsNeedVerifyFull(mode) => write!(
                f,
                "sslrootcert=system works only with sslmode=verify-full, not {}",
                mode.name()
            ),
            Self::ConflictingParameter(name) => {
                write!(f, "the URI gives {name} more than once with different values")
            }
            Self::UnrecognizedSslSetting => f.write_str(
                "the URI carries an sslmode or sslnegotiation this driver did not read (check the URI syntax)",
            ),
            Self::UnsupportedSslNegotiation => {
                f.write_str("sslnegotiation=direct is not supported (use the default, postgres)")
            }
            Self::UnescapedAtSign => f.write_str(
                "the URI has an unescaped '@' after the host (in a query value?); percent-encode it as %40",
            ),
            Self::UnixHostWithHostaddr => f.write_str(
                "a Unix-socket host cannot be combined with hostaddr when TLS is on (the connection would go to hostaddr in clear text)",
            ),
            Self::HostaddrWithoutHost => f.write_str(
                "hostaddr needs host=<server name> when TLS is on: the certificate is verified for the name, the socket goes to the address",
            ),
            Self::KeyFilePermissions { path, mode } => write!(
                f,
                "the private key '{path}' has permissions {mode:04o}; it must not be accessible by group or others (chmod 600)"
            ),
            Self::IncompleteClientIdentity => {
                f.write_str("sslcert and sslkey must be given together")
            }
            Self::File { what, path, reason } => write!(f, "cannot read {what} '{path}': {reason}"),
            Self::NoCertificates { path } => {
                write!(f, "no PEM certificate found in '{path}'")
            }
            Self::NoPrivateKey { path } => write!(
                f,
                "no PKCS#8, PKCS#1 or SEC1 private key found in '{path}'"
            ),
            Self::EncryptedPrivateKey { path } => write!(
                f,
                "the private key '{path}' is password protected, which is not supported; decrypt it \
                 (for example `openssl pkcs8 -in key.pem -out key-plain.pem`) and keep the file private"
            ),
            Self::NoTrustAnchors => f.write_str(
                "no trusted CA certificates are available; give sslrootcert=<ca.pem> or install the CA in the OS trust store",
            ),
            Self::ClientIdentity(reason) => {
                write!(f, "the client certificate cannot be used: {reason}")
            }
        }
    }
}

impl StdError for TlsConfigError {}

/// Why [`Transport::prepare`] failed.
#[derive(Debug)]
pub(crate) enum PrepareError {
    Tls(TlsConfigError),
    Uri(tokio_postgres::Error),
}

impl fmt::Display for PrepareError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Tls(error) => write!(f, "invalid TLS setting: {error}"),
            Self::Uri(error) => error.fmt(f),
        }
    }
}

impl From<PrepareError> for BackendError {
    fn from(error: PrepareError) -> Self {
        match error {
            PrepareError::Tls(error) => BackendError::with_code(
                "connect.tls_config",
                format!("invalid TLS setting: {error}"),
            ),
            PrepareError::Uri(error) => {
                BackendError::new(format!("Postgres connect failed: {error}"))
            }
        }
    }
}

/// Why the certificate was refused.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum CertProblem {
    UnknownIssuer,
    Expired,
    NotYetValid,
    NameMismatch,
    Revoked,
    Other(String),
}

impl CertProblem {
    fn from_rustls(problem: &CertificateError) -> Self {
        match problem {
            CertificateError::UnknownIssuer => Self::UnknownIssuer,
            CertificateError::Expired | CertificateError::ExpiredContext { .. } => Self::Expired,
            CertificateError::NotValidYet | CertificateError::NotValidYetContext { .. } => {
                Self::NotYetValid
            }
            CertificateError::NotValidForName | CertificateError::NotValidForNameContext { .. } => {
                Self::NameMismatch
            }
            CertificateError::Revoked => Self::Revoked,
            other => Self::Other(format!("{other:?}")),
        }
    }

    fn label(&self) -> &str {
        match self {
            Self::UnknownIssuer => "unknown issuer",
            Self::Expired => "expired",
            Self::NotYetValid => "not yet valid",
            Self::NameMismatch => "name mismatch",
            Self::Revoked => "revoked",
            Self::Other(reason) => reason,
        }
    }
}

/// What a failed TLS handshake produced; tokio-postgres wraps it, [`Transport::connect_error`] finds it.
#[derive(Debug, PartialEq, Eq)]
pub(crate) enum TlsError {
    Certificate { problem: CertProblem, host: String },
    Handshake(String),
    InvalidServerName(String),
}

impl TlsError {
    fn from_io(error: &io::Error, host: &str) -> Self {
        match error
            .get_ref()
            .and_then(|inner| inner.downcast_ref::<rustls::Error>())
        {
            Some(rustls::Error::InvalidCertificate(problem)) => Self::Certificate {
                problem: CertProblem::from_rustls(problem),
                host: host.to_owned(),
            },
            Some(other) => Self::Handshake(other.to_string()),
            None => Self::Handshake(error.to_string()),
        }
    }
}

impl fmt::Display for TlsError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Certificate { problem, host } => write!(
                f,
                "server certificate for '{host}' rejected: {}",
                problem.label()
            ),
            Self::Handshake(reason) => write!(f, "TLS handshake failed: {reason}"),
            Self::InvalidServerName(host) => {
                write!(f, "'{host}' is not a valid TLS server name")
            }
        }
    }
}

impl StdError for TlsError {}

// ---- certificate verification ---------------------------------------------------------------------------

/// `require` / `verify-ca` / `verify-full` on top of rustls' public WebPKI helpers.
#[derive(Debug)]
struct PgVerifier {
    /// `None`: encrypt only (`prefer` / `require` without a root file).
    roots: Option<Arc<RootCertStore>>,
    check_name: bool,
    provider: Arc<CryptoProvider>,
}

impl ServerCertVerifier for PgVerifier {
    fn verify_server_cert(
        &self,
        end_entity: &CertificateDer<'_>,
        intermediates: &[CertificateDer<'_>],
        server_name: &ServerName<'_>,
        _ocsp_response: &[u8],
        now: UnixTime,
    ) -> Result<ServerCertVerified, rustls::Error> {
        let Some(roots) = &self.roots else {
            return Ok(ServerCertVerified::assertion());
        };
        // The chain and the name are two separate public checks, so `verify-ca`
        // is "the chain check only" by construction. No revocation checking
        // (neither did the verifier this replaces).
        let certificate = ParsedCertificate::try_from(end_entity)?;
        verify_server_cert_signed_by_trust_anchor(
            &certificate,
            roots,
            intermediates,
            now,
            self.provider.signature_verification_algorithms.all,
        )?;
        if self.check_name {
            verify_server_name(&certificate, server_name)?;
        }
        Ok(ServerCertVerified::assertion())
    }

    // The handshake signature is always checked: it proves the server holds the
    // key of the certificate it sent, even when the certificate is not trusted.
    fn verify_tls12_signature(
        &self,
        message: &[u8],
        cert: &CertificateDer<'_>,
        dss: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, rustls::Error> {
        crypto::verify_tls12_signature(
            message,
            cert,
            dss,
            &self.provider.signature_verification_algorithms,
        )
    }

    fn verify_tls13_signature(
        &self,
        message: &[u8],
        cert: &CertificateDer<'_>,
        dss: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, rustls::Error> {
        crypto::verify_tls13_signature(
            message,
            cert,
            dss,
            &self.provider.signature_verification_algorithms,
        )
    }

    fn supported_verify_schemes(&self) -> Vec<SignatureScheme> {
        self.provider
            .signature_verification_algorithms
            .supported_schemes()
    }
}

fn read_pem_file(path: &Path, what: &'static str) -> Result<String, TlsConfigError> {
    std::fs::read_to_string(path).map_err(|error| TlsConfigError::File {
        what,
        path: path.display().to_string(),
        reason: error.kind().to_string(),
    })
}

/// The OS trust store, loaded once per process.
fn system_roots() -> Result<Arc<RootCertStore>, TlsConfigError> {
    static ROOTS: OnceLock<Arc<RootCertStore>> = OnceLock::new();
    let roots = ROOTS.get_or_init(|| {
        let mut store = RootCertStore::empty();
        store.add_parsable_certificates(rustls_native_certs::load_native_certs().certs);
        Arc::new(store)
    });
    if roots.is_empty() {
        return Err(TlsConfigError::NoTrustAnchors);
    }
    Ok(Arc::clone(roots))
}

fn file_roots(path: &Path) -> Result<Arc<RootCertStore>, TlsConfigError> {
    let no_certificates = || TlsConfigError::NoCertificates {
        path: path.display().to_string(),
    };
    let pem = read_pem_file(path, "sslrootcert")?;
    let certs: Vec<CertificateDer<'static>> = CertificateDer::pem_slice_iter(pem.as_bytes())
        .collect::<Result<_, _>>()
        .map_err(|_| no_certificates())?;
    let mut store = RootCertStore::empty();
    store.add_parsable_certificates(certs);
    if store.is_empty() {
        return Err(no_certificates());
    }
    Ok(Arc::new(store))
}

/// libpq: `u=rw` (0600) or less for the owner's own key, `u=rw,g=r` (0640) or less when root owns it.
#[cfg(unix)]
fn key_mode_allowed(owned_by_root: bool, mode: u32) -> bool {
    mode & if owned_by_root { 0o037 } else { 0o077 } == 0
}

/// Reads a private key file: opened once, the mode checked on the open handle (no
/// check-then-read race on the path), and read from that handle. A key that group or
/// others can read is refused (libpq's rule: `0600`, or `0640` when root owns it);
/// Unix only, Windows has no such mode bits.
fn read_private_key_file(path: &Path) -> Result<String, TlsConfigError> {
    use std::io::Read;
    let file_error = |error: io::Error| TlsConfigError::File {
        what: "sslkey",
        path: path.display().to_string(),
        reason: error.kind().to_string(),
    };
    let mut file = std::fs::File::open(path).map_err(file_error)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        let metadata = file.metadata().map_err(file_error)?;
        let mode = metadata.mode() & 0o777;
        if !key_mode_allowed(metadata.uid() == 0, mode) {
            return Err(TlsConfigError::KeyFilePermissions {
                path: path.display().to_string(),
                mode,
            });
        }
    }
    let mut content = String::new();
    file.read_to_string(&mut content).map_err(file_error)?;
    Ok(content)
}

fn read_client_identity(
    cert: &Path,
    key: &Path,
) -> Result<(Vec<CertificateDer<'static>>, PrivateKeyDer<'static>), TlsConfigError> {
    let cert_pem = read_pem_file(cert, "sslcert")?;
    let chain: Vec<CertificateDer<'static>> = CertificateDer::pem_slice_iter(cert_pem.as_bytes())
        .collect::<Result<_, _>>()
        .map_err(|_| TlsConfigError::NoCertificates {
            path: cert.display().to_string(),
        })?;
    if chain.is_empty() {
        return Err(TlsConfigError::NoCertificates {
            path: cert.display().to_string(),
        });
    }
    let key_pem = read_private_key_file(key)?;
    // `ENCRYPTED PRIVATE KEY` (PKCS#8) and `Proc-Type: 4,ENCRYPTED` (legacy PEM):
    // decrypting them needs a KDF + cipher stack this driver does not ship.
    if key_pem.contains("ENCRYPTED") {
        return Err(TlsConfigError::EncryptedPrivateKey {
            path: key.display().to_string(),
        });
    }
    let key = PrivateKeyDer::from_pem_slice(key_pem.as_bytes()).map_err(|_| {
        TlsConfigError::NoPrivateKey {
            path: key.display().to_string(),
        }
    })?;
    Ok((chain, key))
}

/// The roots a mode checks the chain against: the CA file when given, the OS
/// store for `verify-full` without one, nothing (encrypt only) otherwise.
fn trust_roots(settings: &TlsSettings) -> Result<Option<Arc<RootCertStore>>, TlsConfigError> {
    // No catch-all: a new mode or root source must decide here, and a verifying mode
    // without roots is an error, never "encrypt only".
    match (&settings.roots, settings.mode) {
        (Some(RootCerts::File(path)), _) => file_roots(path).map(Some),
        (Some(RootCerts::System) | None, SslMode::VerifyFull) => system_roots().map(Some),
        (Some(RootCerts::System) | None, SslMode::VerifyCa) => {
            Err(TlsConfigError::VerifyCaNeedsCaFile)
        }
        (Some(RootCerts::System), mode) => Err(TlsConfigError::SystemRootsNeedVerifyFull(mode)),
        (None, SslMode::Disable | SslMode::Allow | SslMode::Prefer | SslMode::Require) => Ok(None),
    }
}

/// The rustls configuration for `settings`.
fn client_config(settings: &TlsSettings) -> Result<ClientConfig, TlsConfigError> {
    let provider = Arc::new(crypto::ring::default_provider());
    let verifier = Arc::new(PgVerifier {
        roots: trust_roots(settings)?,
        check_name: settings.mode == SslMode::VerifyFull,
        provider: Arc::clone(&provider),
    });
    let builder = ClientConfig::builder_with_provider(provider)
        .with_safe_default_protocol_versions()
        .map_err(|error| TlsConfigError::ClientIdentity(error.to_string()))?
        .dangerous()
        .with_custom_certificate_verifier(verifier);
    match &settings.client {
        Some((cert, key)) => {
            let (chain, key) = read_client_identity(cert, key)?;
            builder
                .with_client_auth_cert(chain, key)
                .map_err(|error| TlsConfigError::ClientIdentity(error.to_string()))
        }
        None => Ok(builder.with_no_client_auth()),
    }
}

// ---- connector ------------------------------------------------------------------------------------------

/// What the last completed TLS handshake negotiated.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub(crate) struct TlsInfo {
    pub(crate) version: &'static str,
    pub(crate) cipher: String,
    pub(crate) verified: Verified,
}

#[derive(Default)]
struct TlsState {
    /// A handshake completed: this session is a TLS session (see `Route::Prefer`).
    tls_seen: AtomicBool,
    /// Completed handshakes (tests assert that a cancel request is encrypted too).
    #[cfg(test)]
    handshakes: std::sync::atomic::AtomicUsize,
    info: Mutex<Option<TlsInfo>>,
    /// The session connection's handshake, fixed once the session is open; later
    /// connections (per-database, metadata, mutation) never replace it.
    session: OnceLock<Option<TlsInfo>>,
}

/// `MakeTlsConnect` over rustls; cheap to clone (shares the config and state).
#[derive(Clone)]
pub(crate) struct PgTls {
    config: Arc<ClientConfig>,
    verified: Verified,
    state: Arc<TlsState>,
}

pub(crate) struct PgTlsConnect {
    server_name: Result<ServerName<'static>, String>,
    connector: tokio_rustls::TlsConnector,
    verified: Verified,
    state: Arc<TlsState>,
}

pub(crate) struct PgTlsStream<S>(tokio_rustls::client::TlsStream<S>);

impl<S> MakeTlsConnect<S> for PgTls
where
    S: AsyncRead + AsyncWrite + Unpin + Send + 'static,
{
    type Stream = PgTlsStream<S>;
    type TlsConnect = PgTlsConnect;
    type Error = std::convert::Infallible;

    /// `hostname` is the URI's `host` (never `hostaddr`): the name the
    /// certificate must match and the SNI value.
    fn make_tls_connect(&mut self, hostname: &str) -> Result<PgTlsConnect, Self::Error> {
        Ok(PgTlsConnect {
            server_name: ServerName::try_from(hostname.to_owned()).map_err(|_| hostname.to_owned()),
            connector: tokio_rustls::TlsConnector::from(Arc::clone(&self.config)),
            verified: self.verified,
            state: Arc::clone(&self.state),
        })
    }
}

fn tls_version(version: Option<rustls::ProtocolVersion>) -> &'static str {
    match version {
        Some(rustls::ProtocolVersion::TLSv1_3) => "TLSv1.3",
        Some(rustls::ProtocolVersion::TLSv1_2) => "TLSv1.2",
        _ => "unknown",
    }
}

impl<S> TlsConnect<S> for PgTlsConnect
where
    S: AsyncRead + AsyncWrite + Unpin + Send + 'static,
{
    type Stream = PgTlsStream<S>;
    type Error = TlsError;
    type Future = Pin<Box<dyn Future<Output = Result<PgTlsStream<S>, TlsError>> + Send>>;

    fn connect(self, stream: S) -> Self::Future {
        Box::pin(async move {
            let name = self.server_name.map_err(TlsError::InvalidServerName)?;
            let host = name.to_str().into_owned();
            let stream = self
                .connector
                .connect(name, stream)
                .await
                .map_err(|error| TlsError::from_io(&error, &host))?;
            let (_, session) = stream.get_ref();
            let info = TlsInfo {
                version: tls_version(session.protocol_version()),
                cipher: session
                    .negotiated_cipher_suite()
                    .map_or_else(String::new, |suite| format!("{:?}", suite.suite())),
                verified: self.verified,
            };
            *self
                .state
                .info
                .lock()
                .unwrap_or_else(PoisonError::into_inner) = Some(info);
            self.state.tls_seen.store(true, Ordering::Release);
            #[cfg(test)]
            self.state.handshakes.fetch_add(1, Ordering::AcqRel);
            Ok(PgTlsStream(stream))
        })
    }
}

impl<S> AsyncRead for PgTlsStream<S>
where
    S: AsyncRead + AsyncWrite + Unpin,
{
    fn poll_read(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut ReadBuf<'_>,
    ) -> Poll<io::Result<()>> {
        Pin::new(&mut self.0).poll_read(cx, buf)
    }
}

impl<S> AsyncWrite for PgTlsStream<S>
where
    S: AsyncRead + AsyncWrite + Unpin,
{
    fn poll_write(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &[u8],
    ) -> Poll<io::Result<usize>> {
        Pin::new(&mut self.0).poll_write(cx, buf)
    }

    fn poll_flush(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        Pin::new(&mut self.0).poll_flush(cx)
    }

    fn poll_shutdown(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        Pin::new(&mut self.0).poll_shutdown(cx)
    }
}

impl<S> TlsStream for PgTlsStream<S>
where
    S: AsyncRead + AsyncWrite + Unpin,
{
    /// `tls-server-end-point` (RFC 5929) for SCRAM-SHA-256-PLUS.
    fn channel_binding(&self) -> ChannelBinding {
        let (_, session) = self.0.get_ref();
        session
            .peer_certificates()
            .and_then(<[_]>::first)
            .and_then(|cert| server_end_point_hash(cert.as_ref()))
            .map_or_else(ChannelBinding::none, ChannelBinding::tls_server_end_point)
    }
}

/// One DER element: `(tag, content, rest)`.
fn der_element(input: &[u8]) -> Option<(u8, &[u8], &[u8])> {
    let (&tag, rest) = input.split_first()?;
    let (&first, rest) = rest.split_first()?;
    let (length, rest) = if first < 0x80 {
        (usize::from(first), rest)
    } else {
        let count = usize::from(first & 0x7f);
        if count == 0 || count > 4 || rest.len() < count {
            return None;
        }
        let (bytes, rest) = rest.split_at(count);
        (
            bytes.iter().fold(0, |n, &b| (n << 8) | usize::from(b)),
            rest,
        )
    };
    (rest.len() >= length).then(|| (tag, &rest[..length], &rest[length..]))
}

/// The certificate hash `tls-server-end-point` uses: the hash of the signature
/// algorithm (SHA-1 becomes SHA-256; MD5 has no hash here), over the whole DER certificate.
/// `None` for algorithms without a hash (Ed25519, RSASSA-PSS here): the exchange
/// then runs without channel binding.
fn server_end_point_hash(certificate: &[u8]) -> Option<Vec<u8>> {
    // Certificate ::= SEQUENCE { tbsCertificate SEQUENCE, signatureAlgorithm SEQUENCE { OID, .. }, .. }
    let (0x30, body, _) = der_element(certificate)? else {
        return None;
    };
    let (_, _, after_tbs) = der_element(body)?;
    let (0x30, algorithm, _) = der_element(after_tbs)? else {
        return None;
    };
    let (0x06, oid, _) = der_element(algorithm)? else {
        return None;
    };
    // 1.2.840.113549.1.1.{5,11,12,13} and 1.2.840.10045.4.{1,3.2,3.3,3.4}
    const RSA: &[u8] = &[0x2a, 0x86, 0x48, 0x86, 0xf7, 0x0d, 0x01, 0x01];
    const ECDSA_SHA1: &[u8] = &[0x2a, 0x86, 0x48, 0xce, 0x3d, 0x04, 0x01];
    const ECDSA_SHA2: &[u8] = &[0x2a, 0x86, 0x48, 0xce, 0x3d, 0x04, 0x03];
    let digest = if let Some(&[last]) = oid.strip_prefix(RSA) {
        match last {
            0x05 | 0x0b => Sha256::digest(certificate).to_vec(),
            0x0c => Sha384::digest(certificate).to_vec(),
            0x0d => Sha512::digest(certificate).to_vec(),
            _ => return None,
        }
    } else if oid == ECDSA_SHA1 {
        Sha256::digest(certificate).to_vec()
    } else if let Some(&[last]) = oid.strip_prefix(ECDSA_SHA2) {
        match last {
            0x02 => Sha256::digest(certificate).to_vec(),
            0x03 => Sha384::digest(certificate).to_vec(),
            0x04 => Sha512::digest(certificate).to_vec(),
            _ => return None,
        }
    } else {
        return None;
    };
    Some(digest)
}

// ---- transport ------------------------------------------------------------------------------------------

#[derive(Clone)]
enum Route {
    /// `sslmode=disable` (or a Unix socket): no TLS machinery at all.
    Plain,
    /// `require`, `verify-*`: TLS or error.
    Require(PgTls),
    /// `prefer`: TLS; clear text only when the server answers `SSLRequest` with
    /// `N` *and no connection of this session has used TLS yet*.
    Prefer(PgTls),
    /// `allow`: plain first; once a plain attempt was refused for lack of TLS, TLS from then on.
    Allow(PgTls, Arc<AtomicBool>),
}

/// How every connection of a kit connection reaches the server; see the module docs.
#[derive(Clone)]
pub(crate) struct Transport {
    route: Route,
    mode: SslMode,
    /// Where an acknowledged cancel request is dialled (see [`DialTarget`]): derived
    /// from the configuration when it names the address (`hostaddr`, an IP literal, a
    /// Unix socket), or fixed by [`Transport::connect_session`] when it names a host.
    /// `None`: the cancel is the unacknowledged one.
    dial: Option<DialTarget>,
    /// How long the acknowledged dial (connect, TLS handshake, writing the request) may
    /// take before the exact request is sent instead.
    dial_budget: Duration,
    /// The address the session connected to when the configuration names a host: every
    /// later connection of this transport (per-database, metadata, mutation) goes there
    /// too, so a cancel dialled to it reaches the server of the connection it cancels.
    pinned: Option<IpAddr>,
}

/// The longest the acknowledged dial may take before the exact request is sent instead
/// (the configuration's `connect_timeout` when shorter).
const DIAL_BUDGET: Duration = Duration::from_secs(1);

#[cfg(test)]
impl Default for Transport {
    /// Plain TCP: for tests that need a cancel path without a server URI.
    fn default() -> Self {
        Self {
            route: Route::Plain,
            mode: SslMode::Disable,
            dial: None,
            dial_budget: DIAL_BUDGET,
            pinned: None,
        }
    }
}

// ---- acknowledged cancel --------------------------------------------------------------------------------

/// A stream whose shutdown first waits for the peer to close.
///
/// `tokio-postgres` writes a cancel request, shuts the socket down and drops it. The
/// server handles the request in a process of its own and signals the backend *later*;
/// a statement the client sent in the meantime is the one that gets cancelled. libpq
/// and pgjdbc send the request, read until the server closes the connection - which it
/// does only after it sent the signal - and only then close their side. A signal that
/// reaches an idle backend is discarded, so once the close is seen the next command is
/// safe. `CancelToken::cancel_query_raw` shuts the stream it was given down last
/// (through the TLS layer, which first writes its close-notify), so this is where the
/// wait goes. Any end of the peer's side (EOF, reset) is the close; data the peer still
/// sends (TLS alerts, session tickets) is discarded. `written` is raised when the
/// shutdown starts: the request has been flushed by then.
struct UntilClosed<S> {
    inner: S,
    peer_closed: bool,
    written: Arc<AtomicBool>,
}

impl<S> UntilClosed<S> {
    fn new(inner: S, written: Arc<AtomicBool>) -> Self {
        Self {
            inner,
            peer_closed: false,
            written,
        }
    }
}

impl<S: AsyncRead + Unpin> AsyncRead for UntilClosed<S> {
    fn poll_read(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut ReadBuf<'_>,
    ) -> Poll<io::Result<()>> {
        Pin::new(&mut self.inner).poll_read(cx, buf)
    }
}

impl<S: AsyncRead + AsyncWrite + Unpin> AsyncWrite for UntilClosed<S> {
    fn poll_write(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &[u8],
    ) -> Poll<io::Result<usize>> {
        Pin::new(&mut self.inner).poll_write(cx, buf)
    }

    fn poll_flush(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        Pin::new(&mut self.inner).poll_flush(cx)
    }

    fn poll_shutdown(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        let this = &mut *self;
        this.written.store(true, Ordering::Release);
        let mut scratch = [0_u8; 64];
        while !this.peer_closed {
            let mut buf = ReadBuf::new(&mut scratch);
            match ready!(Pin::new(&mut this.inner).poll_read(cx, &mut buf)) {
                Ok(()) if !buf.filled().is_empty() => {}
                Ok(()) => this.peer_closed = true,
                // The peer ended the connection without a clean close: it is gone all the same.
                Err(error)
                    if matches!(
                        error.kind(),
                        io::ErrorKind::ConnectionReset
                            | io::ErrorKind::ConnectionAborted
                            | io::ErrorKind::BrokenPipe
                            | io::ErrorKind::UnexpectedEof
                    ) =>
                {
                    this.peer_closed = true
                }
                // Anything else (the network went away, a timeout) says nothing about the
                // server having handled the request: the delivery is not acknowledged.
                Err(error) => return Poll::Ready(Err(error)),
            }
        }
        // A failing shutdown means the peer is already gone, which is what was waited for.
        let _ = ready!(Pin::new(&mut this.inner).poll_shutdown(cx));
        Poll::Ready(Ok(()))
    }
}

/// Where an acknowledged cancel request is dialled: the one server the session is on.
/// A configuration with several hosts or ports, `load_balance_hosts=random`, or a Unix
/// host combined with `hostaddr` has none (the connect cannot be reproduced faithfully
/// or the session may be on any of them): its cancels are the unacknowledged ones.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum DialTarget {
    /// `host` is the name the certificate is checked against (empty without a host);
    /// `address` is where the session connected.
    Tcp {
        host: String,
        port: u16,
        address: IpAddr,
    },
    /// The socket file of a Unix-socket directory host.
    #[cfg(unix)]
    Unix(PathBuf),
}

impl DialTarget {
    /// The target when the configuration names the address itself: `hostaddr`, an IP
    /// literal or a Unix directory. A host name has none until the session connects
    /// ([`Transport::connect_session`]).
    fn from_config(config: &Config) -> Option<Self> {
        use tokio_postgres::config::Host;
        let port = match config.get_ports() {
            [] => 5432,
            [port] => *port,
            _ => return None,
        };
        let address = match config.get_hostaddrs() {
            [] => None,
            [address] => Some(*address),
            _ => return None,
        };
        match (config.get_hosts(), address) {
            ([Host::Tcp(host)], address) => {
                address
                    .or_else(|| host.parse().ok())
                    .map(|address| Self::Tcp {
                        host: host.clone(),
                        port,
                        address,
                    })
            }
            #[cfg(unix)]
            ([Host::Unix(directory)], None) => {
                Some(Self::Unix(directory.join(format!(".s.PGSQL.{port}"))))
            }
            ([], Some(address)) => Some(Self::Tcp {
                host: String::new(),
                port,
                address,
            }),
            _ => None,
        }
    }
}

/// The host name and port of a configuration whose session connect the driver does
/// itself ([`Transport::connect_session`]): one host that is a name, no `hostaddr`, at
/// most one port, hosts tried in the listed order. Anything else keeps
/// `tokio-postgres`' own connect and has no acknowledged cancel.
fn pinnable_name(config: &Config) -> Option<(String, u16)> {
    use tokio_postgres::config::Host;
    if !config.get_hostaddrs().is_empty()
        || config.get_load_balance_hosts() != LoadBalanceHosts::Disable
    {
        return None;
    }
    let port = match config.get_ports() {
        [] => 5432,
        [port] => *port,
        _ => return None,
    };
    let [Host::Tcp(host)] = config.get_hosts() else {
        return None;
    };
    // An IP literal needs no resolving; a scoped IPv6 address cannot be held as an `IpAddr`.
    (!host.is_empty() && !host.contains('%') && host.parse::<IpAddr>().is_err())
        .then(|| (host.clone(), port))
}

/// What an acknowledged delivery achieved.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Delivery {
    /// The server closed the connection after the request: it has signalled the backend.
    Acknowledged,
    /// Nothing is known to have been delivered, or a request may be on its way: no
    /// target, the dial or handshake failed or took too long, or the close never came.
    Unacknowledged,
}

/// `connect` through `tls` with `mode`, driving the connection on the current runtime.
async fn open<T>(config: &Config, mode: PgSslMode, tls: T) -> Result<Client, tokio_postgres::Error>
where
    T: MakeTlsConnect<Socket>,
    T::Stream: Send + 'static,
{
    let mut config = config.clone();
    config.ssl_mode(mode);
    let (client, connection) = config.connect(tls).await?;
    tokio::spawn(async move {
        if let Err(error) = connection.await {
            eprintln!("[postgres] connection error: {error}");
        }
    });
    Ok(client)
}

/// tokio-postgres' error for a server that answered `SSLRequest` with `N`.
fn server_refused_tls(error: &tokio_postgres::Error) -> bool {
    error.source().is_some_and(|cause| {
        cause.downcast_ref::<TlsError>().is_none()
            && cause.to_string() == "server does not support TLS"
    })
}

/// `prefer` for a configuration with one host: TLS, clear text only on `N` and only
/// while no connection of the session has used TLS.
async fn prefer_one(config: &Config, tls: &PgTls) -> Result<Client, tokio_postgres::Error> {
    if is_unix_socket(config) {
        // libpq never uses TLS on a Unix socket.
        return open(config, PgSslMode::Disable, NoTls).await;
    }
    match open(config, PgSslMode::Require, tls.clone()).await {
        Err(error) if server_refused_tls(&error) && !tls.state.tls_seen.load(Ordering::Acquire) => {
            open(config, PgSslMode::Disable, NoTls).await
        }
        other => other,
    }
}

fn is_unix_socket(config: &Config) -> bool {
    #[cfg(unix)]
    {
        matches!(config.get_hosts(), [tokio_postgres::config::Host::Unix(_)])
    }
    #[cfg(not(unix))]
    {
        let _ = config;
        false
    }
}

/// `config` reduced to its `index`-th host (with its `hostaddr` and port); every
/// other setting is copied.
fn single_host(config: &Config, index: usize) -> Config {
    let mut one = Config::new();
    if let Some(user) = config.get_user() {
        one.user(user);
    }
    if let Some(password) = config.get_password() {
        one.password(password);
    }
    if let Some(dbname) = config.get_dbname() {
        one.dbname(dbname);
    }
    if let Some(options) = config.get_options() {
        one.options(options);
    }
    if let Some(name) = config.get_application_name() {
        one.application_name(name);
    }
    one.ssl_mode(config.get_ssl_mode());
    one.ssl_negotiation(config.get_ssl_negotiation());
    match config.get_hosts().get(index) {
        Some(tokio_postgres::config::Host::Tcp(host)) => {
            one.host(host);
        }
        #[cfg(unix)]
        Some(tokio_postgres::config::Host::Unix(path)) => {
            one.host_path(path);
        }
        None => {}
    }
    if let Some(address) = config.get_hostaddrs().get(index) {
        one.hostaddr(*address);
    }
    let ports = config.get_ports();
    one.port(
        ports
            .get(index)
            .or_else(|| ports.first())
            .copied()
            .unwrap_or(5432),
    );
    if let Some(timeout) = config.get_connect_timeout() {
        one.connect_timeout(*timeout);
    }
    if let Some(timeout) = config.get_tcp_user_timeout() {
        one.tcp_user_timeout(*timeout);
    }
    one.keepalives(config.get_keepalives());
    one.keepalives_idle(config.get_keepalives_idle());
    if let Some(interval) = config.get_keepalives_interval() {
        one.keepalives_interval(interval);
    }
    if let Some(retries) = config.get_keepalives_retries() {
        one.keepalives_retries(retries);
    }
    one.target_session_attrs(config.get_target_session_attrs());
    one.channel_binding(config.get_channel_binding());
    one.load_balance_hosts(config.get_load_balance_hosts());
    one
}

/// pg_hba.conf refusing the connection (a `hostssl`-only file refuses clear text this
/// way): SQLSTATE 28000. Classified by the code, never by the (localized) message; a
/// wrong password is 28P01.
fn clear_text_refused(error: &tokio_postgres::Error) -> bool {
    error
        .as_db_error()
        .is_some_and(|db| *db.code() == SqlState::INVALID_AUTHORIZATION_SPECIFICATION)
}

/// What the parsed configuration implies for TLS: the plain-or-TLS decision plus
/// the combinations that would silently leave TLS out.
fn effective_mode(config: &Config, requested: SslMode) -> Result<SslMode, TlsConfigError> {
    // tokio-postgres reads the URI with its own parser. Whatever it found that this
    // driver's reader did not (the cleaned URI has no `sslmode`) must not matter.
    if config.get_ssl_mode() != PgSslMode::Prefer {
        return Err(TlsConfigError::UnrecognizedSslSetting);
    }
    if config.get_ssl_negotiation() != SslNegotiation::Postgres {
        return Err(TlsConfigError::UnsupportedSslNegotiation);
    }
    if requested == SslMode::Disable {
        return Ok(requested);
    }
    let hosts = config.get_hosts();
    #[cfg(unix)]
    let unix_hosts = hosts
        .iter()
        .filter(|host| matches!(host, tokio_postgres::config::Host::Unix(_)))
        .count();
    #[cfg(not(unix))]
    let unix_hosts = 0;
    let hostaddrs = !config.get_hostaddrs().is_empty();
    if hostaddrs && unix_hosts > 0 {
        // tokio-postgres would dial `hostaddr` over TCP; libpq never uses TLS on a socket.
        return Err(TlsConfigError::UnixHostWithHostaddr);
    }
    if hostaddrs && hosts.is_empty() {
        return Err(TlsConfigError::HostaddrWithoutHost);
    }
    // libpq never uses TLS on a Unix socket.
    Ok(if !hosts.is_empty() && unix_hosts == hosts.len() {
        SslMode::Disable
    } else {
        requested
    })
}

impl Transport {
    /// Parses `uri` (TLS parameters included) into the configuration tokio-postgres
    /// understands and the transport that serves it. Reads the certificate files and,
    /// for `verify-full` without `sslrootcert`, the OS trust store.
    pub(crate) fn prepare(uri: &str) -> Result<(Config, Self), PrepareError> {
        let (cleaned, settings) = TlsSettings::from_uri(uri).map_err(PrepareError::Tls)?;
        let config: Config = cleaned.parse().map_err(PrepareError::Uri)?;
        let mode = effective_mode(&config, settings.mode).map_err(PrepareError::Tls)?;
        let route = if mode == SslMode::Disable {
            Route::Plain
        } else {
            let tls = PgTls {
                config: Arc::new(client_config(&settings).map_err(PrepareError::Tls)?),
                verified: settings.verified(),
                state: Arc::default(),
            };
            match mode {
                SslMode::Allow => Route::Allow(tls, Arc::default()),
                SslMode::Prefer => Route::Prefer(tls),
                _ => Route::Require(tls),
            }
        };
        let dial = DialTarget::from_config(&config);
        let dial_budget = config
            .get_connect_timeout()
            .map_or(DIAL_BUDGET, |timeout| (*timeout).min(DIAL_BUDGET));
        Ok((
            config,
            Self {
                route,
                mode,
                dial,
                dial_budget,
                pinned: None,
            },
        ))
    }

    /// Connects (and drives the connection on the current runtime). After the session
    /// connected to a host name ([`Transport::connect_session`]) every connection goes
    /// to the session's address, the name staying the TLS server name.
    pub(crate) async fn connect(&self, config: &Config) -> Result<Client, tokio_postgres::Error> {
        match self.pinned {
            Some(address) if config.get_hostaddrs().is_empty() => {
                let mut pinned = config.clone();
                pinned.hostaddr(address);
                self.connect_route(&pinned).await
            }
            _ => self.connect_route(config).await,
        }
    }

    /// Opens the user session. For a configuration with one host that is a name, the
    /// name is resolved here and the addresses are tried in the order returned, each as
    /// the configuration with `hostaddr` set (the name stays the TLS server name), through
    /// the same route logic as any connect; the first success is the session and its
    /// address becomes the target of its acknowledged cancels (and of every later
    /// connection of this transport). Any other configuration connects as
    /// [`Transport::connect`] does and has the target its configuration names, if any.
    ///
    /// Like `tokio-postgres` for one host with several addresses: the next address is
    /// tried after any error, the last error is returned, `connect_timeout` applies to
    /// every attempt. Unlike it: the `sslmode` fallbacks (`prefer`, `allow`) are decided
    /// per address, as libpq does, instead of over the whole address list; and when
    /// the name does not resolve, the connect is `tokio-postgres`' own (same error,
    /// one more lookup).
    pub(crate) async fn connect_session(
        &mut self,
        config: &Config,
    ) -> Result<Client, tokio_postgres::Error> {
        self.connect_session_with(config, |host, port| async move {
            let addresses = tokio::net::lookup_host((host.as_str(), port)).await?;
            Ok(addresses.map(|address| address.ip()).collect())
        })
        .await
    }

    async fn connect_session_with<R, F>(
        &mut self,
        config: &Config,
        resolve: R,
    ) -> Result<Client, tokio_postgres::Error>
    where
        R: FnOnce(String, u16) -> F,
        F: Future<Output = io::Result<Vec<IpAddr>>>,
    {
        let Some((host, port)) = pinnable_name(config) else {
            return self.connect(config).await;
        };
        let addresses = match resolve(host.clone(), port).await {
            Ok(addresses) if !addresses.is_empty() => addresses,
            _ => return self.connect(config).await,
        };
        let mut last = None;
        for address in addresses {
            let mut attempt = config.clone();
            attempt.hostaddr(address);
            match self.connect_route(&attempt).await {
                Ok(client) => {
                    self.dial = Some(DialTarget::Tcp {
                        host,
                        port,
                        address,
                    });
                    self.pinned = Some(address);
                    return Ok(client);
                }
                Err(error) => last = Some(error),
            }
        }
        Err(last.expect("at least one address was tried"))
    }

    /// The route logic of a connect (see [`Transport::connect`]).
    async fn connect_route(&self, config: &Config) -> Result<Client, tokio_postgres::Error> {
        match &self.route {
            Route::Plain => open(config, PgSslMode::Disable, NoTls).await,
            Route::Require(tls) => open(config, PgSslMode::Require, tls.clone()).await,
            // `Prefer` is "Require, and clear text if the server says it has no TLS":
            // the client (and the cancel token it hands out) is a `Require` one whenever
            // TLS works, so a cancel can never fall back to clear text behind its back.
            //
            // With several hosts the decision is made host by host (tokio-postgres' own
            // multi-host loop would retry *every* host in clear text after one host said
            // `N`, including a host whose certificate had just failed): a host whose
            // handshake or certificate failed is never retried in clear text, a host that
            // answers `N` may be used in clear text only while no connection of the
            // session has used TLS. Hosts are tried in the listed order
            // (`load_balance_hosts=random` is not applied under `prefer`).
            Route::Prefer(tls) => {
                let hosts = config.get_hosts().len().max(config.get_hostaddrs().len());
                if hosts <= 1 {
                    return prefer_one(config, tls).await;
                }
                let mut last = None;
                for index in 0..hosts {
                    match prefer_one(&single_host(config, index), tls).await {
                        Ok(client) => return Ok(client),
                        Err(error) => last = Some(error),
                    }
                }
                Err(last.expect("more than one host was tried"))
            }
            Route::Allow(tls, tls_needed) => {
                if tls_needed.load(Ordering::Acquire) {
                    return open(config, PgSslMode::Require, tls.clone()).await;
                }
                match open(config, PgSslMode::Disable, NoTls).await {
                    Err(error) if clear_text_refused(&error) => {
                        let client = open(config, PgSslMode::Require, tls.clone()).await?;
                        tls_needed.store(true, Ordering::Release);
                        Ok(client)
                    }
                    // Anything else (wrong password, ...) is the answer to the plain attempt.
                    other => other,
                }
            }
        }
    }

    /// Delivers a cancel request with the same TLS settings as the session.
    pub(crate) async fn cancel(&self, token: &CancelToken) -> Result<(), tokio_postgres::Error> {
        match &self.route {
            Route::Plain => token.cancel_query(NoTls).await,
            Route::Require(tls) | Route::Prefer(tls) | Route::Allow(tls, _) => {
                token.cancel_query(tls.clone()).await
            }
        }
    }

    /// Delivers a cancel request to the session's server and waits for it to close the
    /// connection, which it does after it signalled the backend (see [`UntilClosed`]),
    /// over the session's TLS settings: a cancel never falls back to clear text. The
    /// dial (connect, handshake, writing the request) has a budget of its own; past it
    /// only an attempt whose request is already written is waited for. Everything that
    /// is not an acknowledgement is [`Delivery::Unacknowledged`] - the caller sends the
    /// exact request then.
    pub(crate) async fn cancel_acknowledged(&self, token: &CancelToken) -> Delivery {
        let Some(target) = &self.dial else {
            return Delivery::Unacknowledged;
        };
        let written = Arc::new(AtomicBool::new(false));
        let attempt = async {
            match target {
                DialTarget::Tcp {
                    host,
                    port,
                    address,
                } => {
                    let stream = tokio::net::TcpStream::connect((*address, *port))
                        .await
                        .ok()?;
                    // A request is a few small writes.
                    let _ = stream.set_nodelay(true);
                    self.cancel_over(token, stream, host, &written).await.ok()
                }
                #[cfg(unix)]
                DialTarget::Unix(path) => {
                    let stream = tokio::net::UnixStream::connect(path).await.ok()?;
                    self.cancel_over(token, stream, "", &written).await.ok()
                }
            }
        };
        let mut attempt = std::pin::pin!(attempt);
        let acknowledged = tokio::select! {
            outcome = &mut attempt => outcome,
            () = tokio::time::sleep(self.dial_budget) => {
                if written.load(Ordering::Acquire) { attempt.await } else { None }
            }
        };
        if acknowledged.is_some() {
            Delivery::Acknowledged
        } else {
            Delivery::Unacknowledged
        }
    }

    /// The request over `stream`, with the session's TLS settings and server name `host`.
    async fn cancel_over<S>(
        &self,
        token: &CancelToken,
        stream: S,
        host: &str,
        written: &Arc<AtomicBool>,
    ) -> Result<(), tokio_postgres::Error>
    where
        S: AsyncRead + AsyncWrite + Unpin + Send + 'static,
    {
        let stream = UntilClosed::new(stream, Arc::clone(written));
        match &self.route {
            Route::Plain => token.cancel_query_raw(stream, NoTls).await,
            Route::Require(tls) | Route::Prefer(tls) | Route::Allow(tls, _) => {
                let connect = match MakeTlsConnect::<UntilClosed<S>>::make_tls_connect(
                    &mut tls.clone(),
                    host,
                ) {
                    Ok(connect) => connect,
                    Err(never) => match never {},
                };
                token.cancel_query_raw(stream, connect).await
            }
        }
    }

    /// The target of this transport's acknowledged cancels (tests).
    #[cfg(test)]
    pub(crate) fn dial_target(&self) -> Option<&DialTarget> {
        self.dial.as_ref()
    }

    fn tls(&self) -> Option<&PgTls> {
        match &self.route {
            Route::Plain => None,
            Route::Require(tls) | Route::Prefer(tls) | Route::Allow(tls, _) => Some(tls),
        }
    }

    /// Fixes [`Transport::info`] to the handshake of the connection just opened (the
    /// session). Call it right after the session connects.
    pub(crate) fn seal_session_info(&self) {
        if let Some(tls) = self.tls() {
            let current = tls
                .state
                .info
                .lock()
                .unwrap_or_else(PoisonError::into_inner)
                .clone();
            let _ = tls.state.session.set(current);
        }
    }

    /// The security state of the session connection's handshake; `None` when the
    /// session is not encrypted (`disable`, `prefer` against a server without TLS),
    /// even if a later connection did use TLS. Before the session is sealed: the
    /// most recent handshake.
    pub(crate) fn info(&self) -> Option<TlsInfo> {
        self.tls().and_then(|tls| match tls.state.session.get() {
            Some(session) => session.clone(),
            None => tls
                .state
                .info
                .lock()
                .unwrap_or_else(PoisonError::into_inner)
                .clone(),
        })
    }

    /// Maps a failed connect to a stable code: `connect.tls_certificate`,
    /// `connect.tls_handshake`, `connect.tls_required`, or the plain failure.
    pub(crate) fn connect_error(&self, error: &tokio_postgres::Error) -> BackendError {
        let mut source = error.source();
        while let Some(cause) = source {
            if let Some(tls) = cause.downcast_ref::<TlsError>() {
                return tls_backend_error(tls, self.mode);
            }
            source = cause.source();
        }
        if clear_text_refused(error) && self.mode == SslMode::Disable {
            return BackendError::with_code(
                "connect.tls_required",
                "the server refused the unencrypted connection (pg_hba.conf, SQLSTATE 28000), \
                 probably because it requires TLS, but sslmode=disable; use sslmode=require, \
                 verify-ca or verify-full",
            );
        }
        if server_refused_tls(error) {
            return BackendError::with_code(
                "connect.tls_required",
                format!(
                    "the server does not support TLS (or stopped offering it) but sslmode={} requires it here; use sslmode=disable only if the connection may be unencrypted",
                    self.mode.name()
                ),
            );
        }
        BackendError::new(format!(
            "Postgres connect failed: {}",
            crate::db::postgres::v2_error_detail(error)
        ))
    }
}

impl SslMode {
    fn name(self) -> &'static str {
        match self {
            Self::Disable => "disable",
            Self::Allow => "allow",
            Self::Prefer => "prefer",
            Self::Require => "require",
            Self::VerifyCa => "verify-ca",
            Self::VerifyFull => "verify-full",
        }
    }
}

fn tls_backend_error(error: &TlsError, mode: SslMode) -> BackendError {
    match error {
        TlsError::Certificate { problem, host } => {
            let advice = match problem {
                CertProblem::UnknownIssuer => {
                    "its CA is not trusted; pass sslrootcert=<ca.pem> (or use sslmode=require to skip verification)"
                }
                CertProblem::NameMismatch if mode == SslMode::VerifyFull => {
                    "the certificate does not name this host; connect with the name it carries (hostaddr= keeps the address) or use sslmode=verify-ca with sslrootcert=<ca.pem>"
                }
                CertProblem::Expired | CertProblem::NotYetValid => {
                    "check the server certificate dates and this computer's clock"
                }
                _ => "",
            };
            BackendError::with_code(
                "connect.tls_certificate",
                format!(
                    "server certificate for '{host}' rejected: {}{}{advice}",
                    problem.label(),
                    if advice.is_empty() { "" } else { "; " }
                ),
            )
        }
        TlsError::Handshake(_) | TlsError::InvalidServerName(_) => {
            BackendError::with_code("connect.tls_handshake", error.to_string())
        }
    }
}

#[cfg(test)]
#[path = "tls_tests.rs"]
pub(crate) mod tests;
