//! The TLS settings of a connection form and how they map onto the URI parameters each driver
//! reads. Pure: no UI, so the dialog tab, the form and the tests share one definition.
//!
//! What each driver really reads (engine evidence in the dialog tab's doc and the plan notes):
//!
//! | family | mode | CA file | client certificate / key | server name |
//! |---|---|---|---|---|
//! | PostgreSQL | `sslmode` (6 modes) | `sslrootcert` (or `system`) | `sslcert` + `sslkey` | none |
//! | MySQL / MariaDB | `ssl-mode` (5 modes + `allow`) | `ssl-ca` | `ssl-cert` + `ssl-key` | `tls-server-name` |
//! | MongoDB (rustls) | `tls`, `tlsAllowInvalidCertificates` | `tlsCAFile` | `tlsCertificateKeyFile` (one PEM) | none |
//!
//! No driver supports a key passphrase (PostgreSQL and MySQL refuse an encrypted key, the MongoDB
//! build has no `cert-key-password` feature), so there is no secret to keep in the credential
//! store, and a passphrase typed into the free-text URL options is refused instead of stored.
//! ClickHouse (`secure`, `ca_cert`) and Redis (`rediss://`) are configured on the Connection tab.

use crate::connections::ConnectionKind;
use percent_encoding::{percent_decode_str, utf8_percent_encode, AsciiSet, NON_ALPHANUMERIC};
use std::fmt;
use std::io;
use std::path::Path;

/// PostgreSQL's `sslrootcert=system`: the OS trust store (`verify-full` only).
pub const SYSTEM_ROOTS: &str = "system";

/// Query values keep `/ - _ . ~` readable; everything else is `%XX`, so a space is `%20` (the
/// PostgreSQL reader does not turn `+` into a space).
pub(crate) const VALUE: &AsciiSet = &NON_ALPHANUMERIC.remove(b'/').remove(b'-').remove(b'_').remove(b'.').remove(b'~');

/// Options whose value is a key passphrase: never stored.
const PASSPHRASE_OPTIONS: [&str; 2] = ["sslpassword", "tlscertificatekeyfilepassword"];

/// Which driver's vocabulary a connection kind speaks.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TlsFamily {
    Postgres,
    /// MySQL and MariaDB share one driver family.
    MySql,
    Mongo,
}

/// The connection mode. `Default` writes nothing and leaves the choice to the driver.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum TlsMode {
    #[default]
    Default,
    Disable,
    Allow,
    Prefer,
    Require,
    VerifyCa,
    VerifyFull,
}

impl TlsFamily {
    pub fn of(kind: ConnectionKind) -> Option<Self> {
        match kind {
            ConnectionKind::Postgres => Some(Self::Postgres),
            ConnectionKind::Mysql | ConnectionKind::Mariadb => Some(Self::MySql),
            ConnectionKind::Mongodb => Some(Self::Mongo),
            ConnectionKind::Sqlite | ConnectionKind::Redis | ConnectionKind::Clickhouse => None,
        }
    }

    /// The modes the driver offers, in the order of the dropdown.
    pub fn modes(self) -> &'static [TlsMode] {
        use TlsMode::*;
        match self {
            Self::Postgres | Self::MySql => {
                &[Default, Disable, Allow, Prefer, Require, VerifyCa, VerifyFull]
            }
            Self::Mongo => &[Default, Disable, VerifyFull, Require],
        }
    }

    /// The mode as the driver names it (dropdown label).
    pub fn label(self, mode: TlsMode) -> &'static str {
        match (self, mode) {
            (Self::Postgres, TlsMode::Default) => "Default (prefer)",
            (Self::Postgres, TlsMode::Disable) => "disable",
            (Self::Postgres, TlsMode::Allow) => "allow",
            (Self::Postgres, TlsMode::Prefer) => "prefer",
            (Self::Postgres, TlsMode::Require) => "require",
            (Self::Postgres, TlsMode::VerifyCa) => "verify-ca",
            (Self::Postgres, TlsMode::VerifyFull) => "verify-full",
            (Self::MySql, TlsMode::Default) => "Default (PREFERRED)",
            (Self::MySql, TlsMode::Disable) => "DISABLED",
            (Self::MySql, TlsMode::Allow) => "allow",
            (Self::MySql, TlsMode::Prefer) => "PREFERRED",
            (Self::MySql, TlsMode::Require) => "REQUIRED",
            (Self::MySql, TlsMode::VerifyCa) => "VERIFY_CA",
            (Self::MySql, TlsMode::VerifyFull) => "VERIFY_IDENTITY",
            (Self::Mongo, TlsMode::Default) => "Default (off)",
            (Self::Mongo, TlsMode::Disable) => "Off",
            (Self::Mongo, TlsMode::VerifyFull) => "On (verify certificate)",
            (Self::Mongo, TlsMode::Require) => "On (skip verification)",
            // Not offered by MongoDB; never selected.
            (Self::Mongo, _) => "",
        }
    }

    /// One line of what the mode does, shown under the dropdown.
    pub fn describe(self, mode: TlsMode) -> &'static str {
        match (self, mode) {
            (Self::Postgres, TlsMode::Default) | (Self::Postgres, TlsMode::Prefer) => {
                "Encrypts when the server offers TLS, otherwise connects in clear text. Protects against passive eavesdropping only."
            }
            (Self::MySql, TlsMode::Default) | (Self::MySql, TlsMode::Prefer) => {
                "Encrypts when the server offers TLS, otherwise connects in clear text. Protects against passive eavesdropping only."
            }
            (_, TlsMode::Disable) => "Never uses TLS.",
            (Self::Mongo, TlsMode::Default) => {
                "No TLS unless a CA or client certificate file is set below."
            }
            (_, TlsMode::Allow) => "Clear text first; TLS only when the server refuses it.",
            (Self::Mongo, TlsMode::Require) => {
                "Encrypts but accepts any server certificate: no protection against an active attacker."
            }
            (_, TlsMode::Require) => {
                "TLS or an error. The certificate is verified against the CA file only when one is set."
            }
            (_, TlsMode::VerifyCa) => {
                "TLS with the certificate chain verified against the CA file (required); the host name is not checked."
            }
            (Self::Mongo, TlsMode::Prefer) => "",
            (_, TlsMode::VerifyFull) => {
                "TLS with the certificate chain and host name verified (the CA file or the system trust store)."
            }
        }
    }

    /// MongoDB reads certificate and key from one PEM file.
    pub fn has_key_file(self) -> bool {
        self != Self::Mongo
    }

    pub fn has_server_name(self) -> bool {
        self == Self::MySql
    }

    /// The mode in the URI; `None` for the driver default.
    fn uri_value(self, mode: TlsMode) -> Option<&'static str> {
        (mode != TlsMode::Default).then(|| self.label(mode))
    }

    /// The mode a URI value names, in any spelling the driver accepts.
    fn parse_mode(self, value: &str) -> Option<TlsMode> {
        let value = value.to_ascii_lowercase().replace('-', "_");
        Some(match (self, value.as_str()) {
            (Self::Mongo, _) => return None,
            (_, "disable" | "disabled") => TlsMode::Disable,
            (_, "allow") => TlsMode::Allow,
            (_, "prefer" | "preferred") => TlsMode::Prefer,
            (_, "require" | "required") => TlsMode::Require,
            (_, "verify_ca") => TlsMode::VerifyCa,
            (_, "verify_full" | "verify_identity") => TlsMode::VerifyFull,
            _ => return None,
        })
    }
}

/// What the TLS tab holds. File paths are kept as typed.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct TlsSettings {
    pub mode: TlsMode,
    pub ca_file: String,
    pub client_cert: String,
    pub client_key: String,
    /// MySQL / MariaDB: the name the certificate is verified for (`tls-server-name`).
    pub server_name: String,
}

/// A TLS setting that cannot be used, with the reason.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TlsError {
    /// `verify-ca` / `VERIFY_CA` without a CA file.
    CaRequired(TlsFamily),
    /// PostgreSQL `sslrootcert=system` with a mode other than `verify-full`.
    SystemRootsNeedVerifyFull,
    /// A client certificate without its key, or the other way round.
    IncompleteClientIdentity(TlsFamily),
    /// MongoDB with TLS off and a certificate file set (the driver rejects the combination).
    FilesWithTlsOff,
    File {
        what: &'static str,
        path: String,
        problem: FileProblem,
    },
    /// A key passphrase in the free-text options.
    PassphraseOption(String),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FileProblem {
    Missing,
    NotAFile,
    Unreadable(io::ErrorKind),
}

impl fmt::Display for TlsError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::CaRequired(family) => write!(
                f,
                "{} needs a CA certificate file; use {} to verify against the system trust store",
                family.label(TlsMode::VerifyCa),
                family.label(TlsMode::VerifyFull),
            ),
            Self::SystemRootsNeedVerifyFull => {
                f.write_str("A CA certificate of \"system\" works only with verify-full")
            }
            Self::IncompleteClientIdentity(family) => {
                if family.has_key_file() {
                    f.write_str("The client certificate and the client key must be given together")
                } else {
                    f.write_str("The client certificate file must hold both the certificate and its key")
                }
            }
            Self::FilesWithTlsOff => f.write_str(
                "TLS is off, but a CA or client certificate file is set; clear the files or turn TLS on",
            ),
            Self::File { what, path, problem } => match problem {
                FileProblem::Missing => write!(f, "The {what} file '{path}' does not exist"),
                FileProblem::NotAFile => write!(f, "The {what} path '{path}' is not a file"),
                FileProblem::Unreadable(kind) => {
                    write!(f, "Cannot read the {what} file '{path}' ({kind})")
                }
            },
            Self::PassphraseOption(name) => write!(
                f,
                "'{name}' would store a key passphrase in the connection URI, and no driver supports encrypted keys; decrypt the key (for example `openssl pkcs8 -in key.pem -out key-plain.pem`) and keep the file private"
            ),
        }
    }
}

impl std::error::Error for TlsError {}

impl TlsSettings {
    /// Moves the TLS parameters of `options` (the URL query as typed) into settings; the rest is
    /// returned untouched. A value the dialog cannot show (`sslmode=bogus`, `require_ssl=true`)
    /// stays in the options.
    pub fn extract(family: TlsFamily, options: &str) -> (Self, String) {
        let mut tls = Self::default();
        let mut kept = Vec::new();
        let (mut mongo_tls, mut mongo_invalid) = (None, None);
        for part in options.split('&').filter(|part| !part.is_empty()) {
            let (name, value) = part.split_once('=').unwrap_or((part, ""));
            let value = percent_decode_str(value).decode_utf8_lossy().into_owned();
            let taken = match (family, name.to_ascii_lowercase().as_str(), name) {
                (TlsFamily::Postgres, _, "sslmode") | (TlsFamily::MySql, _, "ssl-mode" | "sslmode") => {
                    family.parse_mode(&value).map(|mode| tls.mode = mode).is_some()
                }
                (TlsFamily::Postgres, _, "sslrootcert")
                | (TlsFamily::MySql, _, "ssl-ca")
                | (TlsFamily::Mongo, "tlscafile", _) => {
                    tls.ca_file = value;
                    true
                }
                (TlsFamily::Postgres, _, "sslcert")
                | (TlsFamily::MySql, _, "ssl-cert")
                | (TlsFamily::Mongo, "tlscertificatekeyfile", _) => {
                    tls.client_cert = value;
                    true
                }
                (TlsFamily::Postgres, _, "sslkey") | (TlsFamily::MySql, _, "ssl-key") => {
                    tls.client_key = value;
                    true
                }
                (TlsFamily::MySql, _, "tls-server-name") => {
                    tls.server_name = value;
                    true
                }
                (TlsFamily::Mongo, "tls" | "ssl", _) => parse_bool(&value)
                    .map(|on| mongo_tls = Some(on))
                    .is_some(),
                (TlsFamily::Mongo, "tlsallowinvalidcertificates", _) => parse_bool(&value)
                    .map(|on| mongo_invalid = Some(on))
                    .is_some(),
                _ => false,
            };
            if !taken {
                kept.push(part);
            }
        }
        if family == TlsFamily::Mongo {
            let files = !tls.ca_file.is_empty() || !tls.client_cert.is_empty();
            tls.mode = match (mongo_tls, mongo_invalid) {
                (Some(false), _) => TlsMode::Disable,
                (_, Some(true)) => TlsMode::Require,
                (Some(true), _) | (None, Some(false)) => TlsMode::VerifyFull,
                (None, None) if files => TlsMode::VerifyFull,
                (None, None) => TlsMode::Default,
            };
        }
        (tls, kept.join("&"))
    }

    /// The settings of the tab with the blanks filled from `typed` (TLS parameters found in the
    /// free-text options): the tab wins, so a parameter is never written twice.
    pub fn or(mut self, typed: Self) -> Self {
        if self.mode == TlsMode::Default {
            self.mode = typed.mode;
        }
        for (own, other) in [
            (&mut self.ca_file, typed.ca_file),
            (&mut self.client_cert, typed.client_cert),
            (&mut self.client_key, typed.client_key),
            (&mut self.server_name, typed.server_name),
        ] {
            if own.trim().is_empty() {
                *own = other;
            }
        }
        self
    }

    /// Appends the parameters to the URL query `options` (with `&` between).
    pub fn append_to(&self, family: TlsFamily, options: &mut String) {
        let mut push = |name: &str, value: &str| {
            let value = value.trim();
            if value.is_empty() {
                return;
            }
            if !options.is_empty() {
                options.push('&');
            }
            options.push_str(name);
            options.push('=');
            options.extend(utf8_percent_encode(value, VALUE));
        };
        match family {
            TlsFamily::Postgres => {
                push("sslmode", family.uri_value(self.mode).unwrap_or_default());
                push("sslrootcert", &self.ca_file);
                push("sslcert", &self.client_cert);
                push("sslkey", &self.client_key);
            }
            TlsFamily::MySql => {
                push("ssl-mode", family.uri_value(self.mode).unwrap_or_default());
                push("ssl-ca", &self.ca_file);
                push("ssl-cert", &self.client_cert);
                push("ssl-key", &self.client_key);
                push("tls-server-name", &self.server_name);
            }
            TlsFamily::Mongo => {
                match self.mode {
                    TlsMode::Disable => push("tls", "false"),
                    TlsMode::VerifyFull => push("tls", "true"),
                    TlsMode::Require => {
                        push("tls", "true");
                        push("tlsAllowInvalidCertificates", "true");
                    }
                    _ => {}
                }
                push("tlsCAFile", &self.ca_file);
                push("tlsCertificateKeyFile", &self.client_cert);
            }
        }
    }

    /// Combinations the driver refuses, found without touching the disk.
    pub fn check(&self, family: TlsFamily) -> Result<(), TlsError> {
        let (ca, cert, key) = (self.ca_file.trim(), self.client_cert.trim(), self.client_key.trim());
        if family == TlsFamily::Mongo {
            return if self.mode == TlsMode::Disable && !(ca.is_empty() && cert.is_empty()) {
                Err(TlsError::FilesWithTlsOff)
            } else {
                Ok(())
            };
        }
        let system = family == TlsFamily::Postgres && ca == SYSTEM_ROOTS;
        if self.mode == TlsMode::VerifyCa && (ca.is_empty() || system) {
            return Err(TlsError::CaRequired(family));
        }
        if system && self.mode != TlsMode::VerifyFull {
            return Err(TlsError::SystemRootsNeedVerifyFull);
        }
        if cert.is_empty() != key.is_empty() {
            return Err(TlsError::IncompleteClientIdentity(family));
        }
        Ok(())
    }

    /// The files the driver would open exist (not read: the driver parses them). With TLS off
    /// nothing is opened.
    pub fn check_files(&self, family: TlsFamily) -> Result<(), TlsError> {
        if self.mode == TlsMode::Disable {
            return Ok(());
        }
        let cert_what = if family.has_key_file() { "client certificate" } else { "client certificate and key" };
        let ca = self.ca_file.trim();
        let key = if family.has_key_file() { self.client_key.trim() } else { "" };
        for (what, path) in [
            ("CA certificate", if family == TlsFamily::Postgres && ca == SYSTEM_ROOTS { "" } else { ca }),
            (cert_what, self.client_cert.trim()),
            ("client key", key),
        ] {
            if path.is_empty() {
                continue;
            }
            let problem = match std::fs::metadata(Path::new(path)) {
                Ok(meta) if meta.is_file() => continue,
                Ok(_) => FileProblem::NotAFile,
                Err(error) if error.kind() == io::ErrorKind::NotFound => FileProblem::Missing,
                Err(error) => FileProblem::Unreadable(error.kind()),
            };
            return Err(TlsError::File { what, path: path.to_owned(), problem });
        }
        Ok(())
    }

    /// A key passphrase option in the query of `uri` (any family): such a value would be stored.
    pub fn reject_passphrase(uri: &str) -> Result<(), TlsError> {
        let query = uri.split('#').next().and_then(|rest| rest.split_once('?')).map_or("", |(_, query)| query);
        for part in query.split('&') {
            let name = percent_decode_str(part.split_once('=').map_or(part, |(name, _)| name))
                .decode_utf8_lossy()
                .into_owned();
            if PASSPHRASE_OPTIONS.contains(&name.to_ascii_lowercase().as_str()) {
                return Err(TlsError::PassphraseOption(name));
            }
        }
        Ok(())
    }
}

fn parse_bool(value: &str) -> Option<bool> {
    match value {
        "true" => Some(true),
        "false" => Some(false),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn extract(family: TlsFamily, options: &str) -> (TlsSettings, String) {
        TlsSettings::extract(family, options)
    }

    fn written(family: TlsFamily, tls: &TlsSettings) -> String {
        let mut out = String::new();
        tls.append_to(family, &mut out);
        out
    }

    #[test]
    fn postgres_parameters_round_trip_and_the_rest_stays() {
        let options = "application_name=x&sslmode=verify-full&sslrootcert=%2Fcerts%2Fmy%20ca.pem&sslcert=/c/cert.pem&sslkey=/c/key.pem";
        let (tls, rest) = extract(TlsFamily::Postgres, options);
        assert_eq!(rest, "application_name=x");
        assert_eq!(tls.mode, TlsMode::VerifyFull);
        assert_eq!(tls.ca_file, "/certs/my ca.pem");
        assert_eq!((tls.client_cert.as_str(), tls.client_key.as_str()), ("/c/cert.pem", "/c/key.pem"));
        assert_eq!(
            written(TlsFamily::Postgres, &tls),
            "sslmode=verify-full&sslrootcert=/certs/my%20ca.pem&sslcert=/c/cert.pem&sslkey=/c/key.pem"
        );
    }

    #[test]
    fn mysql_accepts_every_spelling_and_writes_its_own() {
        for (value, mode) in [
            ("REQUIRED", TlsMode::Require),
            ("verify_identity", TlsMode::VerifyFull),
            ("verify-full", TlsMode::VerifyFull),
            ("Disabled", TlsMode::Disable),
            ("allow", TlsMode::Allow),
        ] {
            let (tls, rest) = extract(TlsFamily::MySql, &format!("ssl-mode={value}"));
            assert_eq!((tls.mode, rest.as_str()), (mode, ""), "{value}");
        }
        let (tls, _) = extract(TlsFamily::MySql, "sslmode=VERIFY_CA&ssl-ca=/ca.pem&tls-server-name=db.example.com");
        assert_eq!(
            written(TlsFamily::MySql, &tls),
            "ssl-mode=VERIFY_CA&ssl-ca=/ca.pem&tls-server-name=db.example.com"
        );
        // mysql_async's own switches are not the dialog's to move.
        let (tls, rest) = extract(TlsFamily::MySql, "require_ssl=true&ssl-mode=bogus");
        assert_eq!((tls.mode, rest.as_str()), (TlsMode::Default, "require_ssl=true&ssl-mode=bogus"));
    }

    #[test]
    fn mongo_modes_map_to_tls_and_allow_invalid_certificates() {
        for (options, mode) in [
            ("", TlsMode::Default),
            ("tls=false", TlsMode::Disable),
            ("tls=true", TlsMode::VerifyFull),
            ("ssl=true", TlsMode::VerifyFull),
            ("tls=true&tlsAllowInvalidCertificates=true", TlsMode::Require),
            ("tlsCAFile=/ca.pem", TlsMode::VerifyFull),
        ] {
            let (tls, _) = extract(TlsFamily::Mongo, options);
            assert_eq!(tls.mode, mode, "{options}");
        }
        let (tls, rest) = extract(TlsFamily::Mongo, "retryWrites=true&TLSCAFILE=/ca.pem&tlsCertificateKeyFile=/k.pem");
        assert_eq!(rest, "retryWrites=true");
        assert_eq!((tls.ca_file.as_str(), tls.client_cert.as_str()), ("/ca.pem", "/k.pem"));
        assert_eq!(
            written(TlsFamily::Mongo, &TlsSettings { mode: TlsMode::Require, ..tls }),
            "tls=true&tlsAllowInvalidCertificates=true&tlsCAFile=/ca.pem&tlsCertificateKeyFile=/k.pem"
        );
    }

    #[test]
    fn typed_options_fill_blanks_but_the_tab_wins() {
        let typed = TlsSettings { mode: TlsMode::Require, ca_file: "/typed.pem".into(), ..Default::default() };
        let tab = TlsSettings { mode: TlsMode::VerifyFull, ..Default::default() };
        let merged = tab.or(typed);
        assert_eq!((merged.mode, merged.ca_file.as_str()), (TlsMode::VerifyFull, "/typed.pem"));
    }

    #[test]
    fn checks_report_what_the_driver_would_refuse() {
        let verify_ca = TlsSettings { mode: TlsMode::VerifyCa, ..Default::default() };
        assert_eq!(verify_ca.check(TlsFamily::Postgres), Err(TlsError::CaRequired(TlsFamily::Postgres)));
        assert!(verify_ca.check(TlsFamily::MySql).unwrap_err().to_string().contains("VERIFY_CA needs a CA"));
        let system = TlsSettings { mode: TlsMode::Require, ca_file: "system".into(), ..Default::default() };
        assert_eq!(system.check(TlsFamily::Postgres), Err(TlsError::SystemRootsNeedVerifyFull));
        assert!(TlsSettings { mode: TlsMode::VerifyFull, ..system }.check(TlsFamily::Postgres).is_ok());
        let half = TlsSettings { client_cert: "/c.pem".into(), ..Default::default() };
        assert_eq!(half.check(TlsFamily::Postgres), Err(TlsError::IncompleteClientIdentity(TlsFamily::Postgres)));
        assert!(half.check(TlsFamily::Mongo).is_ok());
        let off = TlsSettings { mode: TlsMode::Disable, ca_file: "/ca.pem".into(), ..Default::default() };
        assert_eq!(off.check(TlsFamily::Mongo), Err(TlsError::FilesWithTlsOff));
        assert!(off.check(TlsFamily::Postgres).is_ok());
    }

    #[test]
    fn missing_and_misplaced_files_are_named() {
        let dir = std::env::temp_dir();
        let missing = TlsSettings { ca_file: dir.join("tablex-no-such-ca.pem").display().to_string(), ..Default::default() };
        assert!(matches!(
            missing.check_files(TlsFamily::Postgres),
            Err(TlsError::File { what: "CA certificate", problem: FileProblem::Missing, .. })
        ));
        let directory = TlsSettings { ca_file: dir.display().to_string(), ..Default::default() };
        assert!(matches!(
            directory.check_files(TlsFamily::MySql),
            Err(TlsError::File { problem: FileProblem::NotAFile, .. })
        ));
        // TLS off opens nothing; `system` is not a path.
        assert!(TlsSettings { mode: TlsMode::Disable, ..missing }.check_files(TlsFamily::Postgres).is_ok());
        let system = TlsSettings { mode: TlsMode::VerifyFull, ca_file: "system".into(), ..Default::default() };
        assert!(system.check_files(TlsFamily::Postgres).is_ok());
    }

    #[test]
    fn a_passphrase_option_is_refused_in_any_spelling() {
        assert!(TlsSettings::reject_passphrase("postgresql://u@h/db?sslmode=require").is_ok());
        for uri in [
            "postgresql://u@h/db?sslpassword=hunter2",
            "postgresql://u@h/db?a=1&SSLPASSWORD=x",
            "mongodb://u@h/db?tlsCertificateKeyFilePassword=x",
        ] {
            assert!(matches!(TlsSettings::reject_passphrase(uri), Err(TlsError::PassphraseOption(_))), "{uri}");
        }
    }
}
