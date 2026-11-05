//! Connection URI parsing:
//! `clickhouse://user:pass@host:8123/db?secure=true&ca_cert=/path/ca.pem&<setting>=<value>`
//! (`http://` and `https://` are accepted too).
//!
//! * `secure=true` (or `ssl=true`, or an `https://` URI) selects TLS; the default
//!   port is 8123, or 8443 with TLS (`http`/`https` URIs without a port use the
//!   scheme's own default, as for any URL).
//! * `ca_cert=<path>` trusts exactly the certificates of that PEM (or DER) file
//!   instead of the bundled web roots.
//! * Every other query parameter is a ClickHouse **setting** sent with each
//!   request (`max_execution_time=30`, `dialect_type=ANSI`, ...). The names the
//!   driver manages itself are refused instead of silently ignored.
//! * The user defaults to `default`. Credentials never leave the process except
//!   in the `X-ClickHouse-User` / `X-ClickHouse-Key` headers, and neither
//!   [`Config`]'s `Debug` output nor any error message contains them.

use engine_driver_kit::BackendError;
use percent_encoding::percent_decode_str;
use std::fmt;
use std::path::PathBuf;
use url::Url;

const DEFAULT_PORT: u16 = 8123;
const DEFAULT_TLS_PORT: u16 = 8443;
const DEFAULT_USER: &str = "default";

/// Names the driver sets itself (a user value would fight it).
const RESERVED: [&str; 9] = [
    "query",
    "database",
    "default_format",
    "query_id",
    "session_id",
    "session_check",
    "session_timeout",
    "user",
    "password",
];

#[derive(Clone, PartialEq, Eq)]
pub(crate) struct Config {
    pub secure: bool,
    /// Host as it appears in a URL (IPv6 literals keep their brackets).
    pub host: String,
    pub port: u16,
    pub user: String,
    pub password: Option<String>,
    pub database: Option<String>,
    pub ca_cert: Option<PathBuf>,
    /// Server settings sent with every request, in URI order.
    pub settings: Vec<(String, String)>,
}

impl fmt::Debug for Config {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Config")
            .field("secure", &self.secure)
            .field("host", &self.host)
            .field("port", &self.port)
            .field("user", &self.user)
            .field("password", &self.password.as_ref().map(|_| "***"))
            .field("database", &self.database)
            .field("ca_cert", &self.ca_cert)
            .field("settings", &self.settings)
            .finish()
    }
}

#[derive(Debug, PartialEq, Eq)]
pub(crate) enum ConfigError {
    Invalid(String),
    Scheme(String),
    MissingHost,
    Path,
    Flag { name: &'static str, value: String },
    Setting(String),
    Reserved(String),
}

impl fmt::Display for ConfigError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Invalid(reason) => write!(f, "invalid ClickHouse URI: {reason}"),
            Self::Scheme(scheme) => write!(
                f,
                "unsupported URI scheme `{scheme}` (use clickhouse://, http:// or https://)"
            ),
            Self::MissingHost => f.write_str("the ClickHouse URI has no host"),
            Self::Path => f.write_str("the ClickHouse URI path must be just the database name"),
            Self::Flag { name, value } => {
                write!(f, "`{name}` must be true or false, got `{value}`")
            }
            Self::Setting(name) => write!(f, "`{name}` is not a valid ClickHouse setting name"),
            Self::Reserved(name) => write!(
                f,
                "`{name}` is managed by the driver and cannot be set in the URI"
            ),
        }
    }
}

impl std::error::Error for ConfigError {}

impl From<ConfigError> for BackendError {
    fn from(error: ConfigError) -> Self {
        BackendError::with_code("connect.bad_request", error.to_string())
    }
}

fn decode(value: &str) -> String {
    percent_decode_str(value).decode_utf8_lossy().into_owned()
}

fn parse_flag(name: &'static str, value: &str) -> Result<bool, ConfigError> {
    match value.to_ascii_lowercase().as_str() {
        "true" | "1" | "yes" | "on" => Ok(true),
        "false" | "0" | "no" | "off" => Ok(false),
        _ => Err(ConfigError::Flag {
            name,
            value: value.to_owned(),
        }),
    }
}

fn is_setting_name(name: &str) -> bool {
    let mut chars = name.chars();
    chars
        .next()
        .is_some_and(|first| first.is_ascii_alphabetic() || first == '_')
        && chars.all(|c| c.is_ascii_alphanumeric() || c == '_')
}

/// `~/ca.pem` -> `<home dir>/ca.pem` (`$HOME` on Unix, the profile on Windows).
fn expand_home(path: &str) -> PathBuf {
    match (path.strip_prefix("~/"), dirs::home_dir()) {
        (Some(rest), Some(home)) => home.join(rest),
        _ => PathBuf::from(path),
    }
}

impl Config {
    pub(crate) fn parse(uri: &str) -> Result<Self, ConfigError> {
        let url =
            Url::parse(uri.trim()).map_err(|error| ConfigError::Invalid(error.to_string()))?;
        let scheme_secure = match url.scheme() {
            "clickhouse" | "http" => false,
            "https" => true,
            other => return Err(ConfigError::Scheme(other.to_owned())),
        };
        let host = url
            .host_str()
            .filter(|host| !host.is_empty())
            .ok_or(ConfigError::MissingHost)?
            .to_owned();
        let mut secure = scheme_secure;
        let mut database = None;
        let mut ca_cert = None;
        let mut settings = Vec::new();
        for (name, value) in url.query_pairs() {
            match name.as_ref() {
                "secure" => secure = parse_flag("secure", &value)?,
                "ssl" => secure = parse_flag("ssl", &value)?,
                "ca_cert" if !value.is_empty() => ca_cert = Some(expand_home(&value)),
                "database" if !value.is_empty() => database = Some(value.into_owned()),
                name if RESERVED.contains(&name)
                    || name.starts_with("param_")
                    || name.starts_with("output_format_arrow_") =>
                {
                    return Err(ConfigError::Reserved(name.to_owned()));
                }
                name if !is_setting_name(name) => {
                    return Err(ConfigError::Setting(name.to_owned()));
                }
                name => settings.push((name.to_owned(), value.into_owned())),
            }
        }
        // `http://host` means port 80 like any URL; `clickhouse://host` means
        // ClickHouse's own default.
        let port = url.port_or_known_default().unwrap_or(if secure {
            DEFAULT_TLS_PORT
        } else {
            DEFAULT_PORT
        });
        let path = url.path().trim_matches('/');
        if path.contains('/') {
            return Err(ConfigError::Path);
        }
        if !path.is_empty() {
            database = Some(decode(path));
        }
        let user = match decode(url.username()) {
            user if user.is_empty() => DEFAULT_USER.to_owned(),
            user => user,
        };
        Ok(Self {
            secure,
            host,
            port,
            user,
            password: url
                .password()
                .map(decode)
                .filter(|password| !password.is_empty()),
            database,
            ca_cert,
            settings,
        })
    }

    /// `http(s)://host:port` (no trailing slash).
    pub(crate) fn base_url(&self) -> String {
        format!(
            "{}://{}:{}",
            if self.secure { "https" } else { "http" },
            self.host,
            self.port
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tilde_expands_to_the_platform_home_dir() {
        let home = dirs::home_dir().expect("a home dir exists");
        assert_eq!(expand_home("~/ca.pem"), home.join("ca.pem"));
        assert_eq!(expand_home("/etc/ca.pem"), PathBuf::from("/etc/ca.pem"));
    }

    #[test]
    fn parses_the_documented_form() {
        let config = Config::parse(
            "clickhouse://alice:s%3Acr%40t@db.example.com:9000/analytics?secure=true&ca_cert=/etc/ca.pem&max_execution_time=30&dialect_type=ANSI",
        )
        .unwrap();
        assert!(config.secure);
        assert_eq!(config.base_url(), "https://db.example.com:9000");
        assert_eq!(config.user, "alice");
        assert_eq!(config.password.as_deref(), Some("s:cr@t"));
        assert_eq!(config.database.as_deref(), Some("analytics"));
        assert_eq!(config.ca_cert, Some(PathBuf::from("/etc/ca.pem")));
        assert_eq!(
            config.settings,
            [
                ("max_execution_time".to_owned(), "30".to_owned()),
                ("dialect_type".to_owned(), "ANSI".to_owned())
            ]
        );
    }

    #[test]
    fn defaults_follow_clickhouse() {
        let plain = Config::parse("clickhouse://localhost").unwrap();
        assert_eq!(plain.base_url(), "http://localhost:8123");
        assert_eq!(
            (plain.user.as_str(), plain.password.as_deref()),
            ("default", None)
        );
        assert_eq!(plain.database, None);
        let tls = Config::parse("clickhouse://localhost?ssl=1").unwrap();
        assert_eq!(tls.base_url(), "https://localhost:8443");
        // http(s) URIs use the URL's own default port when none is given.
        assert_eq!(Config::parse("http://h").unwrap().base_url(), "http://h:80");
        assert_eq!(
            Config::parse("https://h/db").unwrap().base_url(),
            "https://h:443"
        );
        assert_eq!(
            Config::parse("https://h:8443/db")
                .unwrap()
                .database
                .as_deref(),
            Some("db")
        );
        assert_eq!(
            Config::parse("clickhouse://[::1]:8123").unwrap().base_url(),
            "http://[::1]:8123"
        );
    }

    #[test]
    fn rejects_what_the_driver_cannot_honour() {
        assert_eq!(
            Config::parse("postgres://h").unwrap_err(),
            ConfigError::Scheme("postgres".into())
        );
        assert_eq!(
            Config::parse("clickhouse://h/a/b").unwrap_err(),
            ConfigError::Path
        );
        assert_eq!(
            Config::parse("clickhouse://h?default_format=JSON").unwrap_err(),
            ConfigError::Reserved("default_format".into())
        );
        assert_eq!(
            Config::parse("clickhouse://h?output_format_arrow_string_as_string=0").unwrap_err(),
            ConfigError::Reserved("output_format_arrow_string_as_string".into())
        );
        assert_eq!(
            Config::parse("clickhouse://h?bad-name=1").unwrap_err(),
            ConfigError::Setting("bad-name".into())
        );
        assert!(matches!(
            Config::parse("clickhouse://h?secure=maybe").unwrap_err(),
            ConfigError::Flag { name: "secure", .. }
        ));
        assert!(Config::parse("not a uri").is_err());
    }

    #[test]
    fn secrets_never_reach_debug_output_or_errors() {
        let uri = "clickhouse://alice:hunter2@h:8123/db?secure=maybe";
        let error = Config::parse(uri).unwrap_err();
        assert!(!error.to_string().contains("hunter2"), "{error}");
        let config = Config::parse("clickhouse://alice:hunter2@h:8123/db").unwrap();
        let shown = format!("{config:?}");
        assert!(!shown.contains("hunter2"), "{shown}");
        assert!(shown.contains("***") && shown.contains("alice"), "{shown}");
        let bad_port = Config::parse("clickhouse://alice:hunter2@h:99999999/db").unwrap_err();
        assert!(!bad_port.to_string().contains("hunter2"), "{bad_port}");
        let backend = BackendError::from(error);
        assert_eq!(backend.code.as_deref(), Some("connect.bad_request"));
        assert!(!backend.message.contains("hunter2"));
    }
}
