//! The connection URI:
//!
//! `sqlserver://user:password@host[:port][/database][?encrypt=..&trustServerCertificate=..]`
//! (`mssql://` is the same).
//!
//! | parameter | meaning |
//! |---|---|
//! | `encrypt` | `true` (default: everything is encrypted, the connect fails when the server cannot), `strict` (TDS 8.0, TLS before the first packet), `false` (only the login is encrypted) |
//! | `trustServerCertificate` | `true` accepts any certificate (self-signed servers); never use it on an untrusted network |
//! | `ca_cert` | PEM file of the CA that signed the server certificate; without it (and without `trustServerCertificate`) the OS trust store is used |
//! | `hostNameInCertificate` | the name the certificate is checked against when it differs from `host` (an SSH tunnel connects to 127.0.0.1) |
//! | `applicationName` | shown in `sys.dm_exec_sessions.program_name` (default `tablex`) |
//! | `applicationIntent` | `ReadOnly` asks for a read-only replica |
//! | `loginTimeout` / `connectTimeout` | seconds for the TCP connect plus the login (default 15) |
//!
//! Not supported, and refused with `connect.bad_request` instead of being ignored: named
//! instances (`instance`; they need the SQL Browser lookup, connect by port instead) and Windows
//! integrated authentication (`integratedSecurity`).

use engine_driver_kit::BackendError;
use percent_encoding::percent_decode_str;
use std::time::Duration;
use url::Url;

pub(crate) const DEFAULT_PORT: u16 = 1433;
const DEFAULT_CONNECT_TIMEOUT: Duration = Duration::from_secs(15);

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Encrypt {
    /// Only the login packet is encrypted.
    Off,
    On,
    /// TDS 8.0: the TLS handshake comes before any TDS packet.
    Strict,
}

#[derive(Clone)]
pub(crate) struct Config {
    pub host: String,
    pub port: u16,
    pub database: Option<String>,
    pub user: String,
    pub password: String,
    pub encrypt: Encrypt,
    pub trust_server_certificate: bool,
    pub ca_cert: Option<String>,
    pub certificate_host: Option<String>,
    pub application_name: String,
    pub read_only_intent: bool,
    pub connect_timeout: Duration,
}

// Never prints the password.
impl std::fmt::Debug for Config {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Config")
            .field("host", &self.host)
            .field("port", &self.port)
            .field("database", &self.database)
            .field("user", &self.user)
            .finish_non_exhaustive()
    }
}

fn bad(message: impl Into<String>) -> BackendError {
    BackendError::with_code("connect.bad_request", message)
}

fn decode(text: &str) -> String {
    percent_decode_str(text).decode_utf8_lossy().into_owned()
}

fn flag(name: &str, value: &str) -> Result<bool, BackendError> {
    match value.to_ascii_lowercase().as_str() {
        "true" | "yes" | "1" | "on" => Ok(true),
        "false" | "no" | "0" | "off" => Ok(false),
        _ => Err(bad(format!("`{name}` must be true or false, not `{value}`"))),
    }
}

impl Config {
    pub(crate) fn parse(uri: &str) -> Result<Self, BackendError> {
        let url = Url::parse(uri).map_err(|_| bad("SQL Server URI could not be parsed"))?;
        if !matches!(url.scheme(), "sqlserver" | "mssql") {
            return Err(bad("the scheme must be sqlserver:// (or mssql://)"));
        }
        let host = url
            .host_str()
            .map(|host| host.trim_start_matches('[').trim_end_matches(']'))
            .filter(|host| !host.is_empty())
            .ok_or_else(|| bad("the SQL Server URI has no host"))?
            .to_owned();
        let database = url
            .path_segments()
            .and_then(|mut segments| segments.next())
            .map(decode)
            .filter(|name| !name.is_empty());
        let mut config = Self {
            host,
            port: url.port().unwrap_or(DEFAULT_PORT),
            database,
            user: decode(url.username()),
            password: url.password().map(decode).unwrap_or_default(),
            encrypt: Encrypt::On,
            trust_server_certificate: false,
            ca_cert: None,
            certificate_host: None,
            application_name: "tablex".to_owned(),
            read_only_intent: false,
            connect_timeout: DEFAULT_CONNECT_TIMEOUT,
        };
        for (name, value) in url.query_pairs() {
            let value = value.as_ref();
            match name.to_ascii_lowercase().replace('_', "").as_str() {
                "encrypt" => {
                    config.encrypt = if value.eq_ignore_ascii_case("strict") {
                        Encrypt::Strict
                    } else if flag("encrypt", value)? {
                        Encrypt::On
                    } else {
                        Encrypt::Off
                    }
                }
                "trustservercertificate" | "trustcert" => {
                    config.trust_server_certificate = flag("trustServerCertificate", value)?;
                }
                "cacert" => config.ca_cert = Some(value.to_owned()).filter(|path| !path.is_empty()),
                "hostnameincertificate" => {
                    config.certificate_host = Some(value.to_owned()).filter(|name| !name.is_empty());
                }
                "applicationname" | "app" => config.application_name = value.to_owned(),
                "applicationintent" => {
                    config.read_only_intent = value.eq_ignore_ascii_case("readonly");
                }
                "logintimeout" | "connecttimeout" => {
                    let seconds = value
                        .parse::<u64>()
                        .ok()
                        .filter(|seconds| (1..=600).contains(seconds))
                        .ok_or_else(|| bad("the timeout must be 1 to 600 seconds"))?;
                    config.connect_timeout = Duration::from_secs(seconds);
                }
                "instance" | "instancename" => {
                    return Err(bad(
                        "named instances are not supported (they need the SQL Browser lookup); connect by host and port",
                    ))
                }
                "integratedsecurity" | "authentication" | "trusted" | "trustedconnection" => {
                    return Err(bad(
                        "Windows integrated authentication is not supported; use a SQL Server login",
                    ))
                }
                other => {
                    return Err(bad(format!(
                        "unknown SQL Server URI parameter `{other}`"
                    )))
                }
            }
        }
        if config.trust_server_certificate && config.ca_cert.is_some() {
            return Err(bad(
                "`trustServerCertificate` and `ca_cert` exclude each other: either accept any certificate or pin a CA",
            ));
        }
        if config.user.is_empty() {
            return Err(bad(
                "the SQL Server URI has no user (Windows authentication is not supported)",
            ));
        }
        Ok(config)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse(uri: &str) -> Config {
        Config::parse(uri).expect("parses")
    }

    fn refused(uri: &str) -> String {
        Config::parse(uri).expect_err("refused").message
    }

    #[test]
    fn a_plain_uri_gets_the_defaults() {
        let config = parse("sqlserver://sa:pw@db.example.test");
        assert_eq!(config.certificate_host, None);
        assert_eq!(config.host, "db.example.test");
        assert_eq!(config.port, 1433);
        assert_eq!(config.database, None);
        assert_eq!((config.user.as_str(), config.password.as_str()), ("sa", "pw"));
        assert_eq!(config.encrypt, Encrypt::On);
        assert!(!config.trust_server_certificate);
        assert_eq!(config.application_name, "tablex");
        assert_eq!(config.connect_timeout, Duration::from_secs(15));
    }

    #[test]
    fn credentials_database_and_options_are_percent_decoded() {
        let config = parse(
            "mssql://us%40er:p%3Aw%2Fd@[::1]:14330/my%20db?encrypt=strict&trustServerCertificate=false&applicationName=a%20b&loginTimeout=5&applicationIntent=ReadOnly",
        );
        assert_eq!((config.user.as_str(), config.password.as_str()), ("us@er", "p:w/d"));
        assert_eq!(config.host, "::1");
        assert_eq!(config.port, 14330);
        assert_eq!(config.database.as_deref(), Some("my db"));
        assert_eq!(config.encrypt, Encrypt::Strict);
        assert_eq!(config.application_name, "a b");
        assert_eq!(config.connect_timeout, Duration::from_secs(5));
        assert!(config.read_only_intent);
    }

    #[test]
    fn encrypt_and_trust_flags_take_the_usual_spellings() {
        assert_eq!(parse("sqlserver://u@h?encrypt=false").encrypt, Encrypt::Off);
        assert_eq!(parse("sqlserver://u@h?Encrypt=YES").encrypt, Encrypt::On);
        assert!(parse("sqlserver://u@h?trust_server_certificate=1").trust_server_certificate);
        assert!(refused("sqlserver://u@h?encrypt=maybe").contains("true or false"));
        assert_eq!(
            parse("sqlserver://u@127.0.0.1?hostNameInCertificate=db.example.test").certificate_host.as_deref(),
            Some("db.example.test")
        );
    }

    #[test]
    fn what_cannot_be_honoured_is_refused_not_ignored() {
        assert!(refused("sqlserver://u@h?instance=SQLEXPRESS").contains("named instances"));
        assert!(refused("sqlserver://u@h?integratedSecurity=true").contains("Windows"));
        assert!(refused("sqlserver://h").contains("no user"));
        assert!(refused("sqlserver://u@h?bogus=1").contains("unknown"));
        assert!(refused("postgres://u@h").contains("scheme"));
        assert!(refused("sqlserver://u@h?trustServerCertificate=true&ca_cert=/x.pem")
            .contains("exclude"));
        assert!(refused("sqlserver://u@h?loginTimeout=0").contains("timeout"));
    }

    #[test]
    fn debug_never_shows_the_password() {
        let text = format!("{:?}", parse("sqlserver://sa:hunter2@h"));
        assert!(!text.contains("hunter2"), "{text}");
    }
}
