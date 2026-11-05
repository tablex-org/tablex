//! Where a native client tool (`pg_dump`, `mysqldump`, ...) connects, read from a connection's URI inside this crate so
//! that the URI's text, which carries the password, never leaves it: the caller gets the parts, and the password as a
//! [`Secret`] that it can only read to write the tool's own private file.

use engine_net::Secret;
use percent_encoding::percent_decode_str;

use crate::records::{ConnectionKind, RuntimeUri};
use crate::tunnel::ssh::{self, ConnectFailure, SessionTunnel, TunnelAccess};

/// What a URI says about reaching a PostgreSQL, MySQL or MariaDB server.
#[derive(Clone)]
pub struct ToolEndpoint {
    /// Empty: the tool's own default (a local socket).
    pub host: String,
    /// PostgreSQL's `hostaddr`: the address the connection really goes to while `host` stays the name TLS checks
    /// and the password file is matched on. A tunnel sets it to its local address.
    pub host_addr: Option<String>,
    pub port: Option<u16>,
    pub user: Option<String>,
    pub password: Option<Secret>,
    pub database: String,
    /// The URI's query as written (`sslmode=require&sslrootcert=...`): TLS settings. A parameter that holds a secret or names a
    /// secret file (`password`, `passfile`, `sslpassword`) is not kept.
    pub query: String,
}

/// Query parameters that carry a secret or point at a file of secrets: never kept.
const SECRET_PARAMETERS: [&str; 4] = ["password", "passfile", "sslpassword", "tlscertificatekeyfilepassword"];

fn without_secrets(query: &str) -> String {
    query
        .split('&')
        .filter(|part| !part.is_empty())
        .filter(|part| {
            let name = part.split_once('=').map_or(*part, |(name, _)| name);
            !SECRET_PARAMETERS.iter().any(|secret| name.eq_ignore_ascii_case(secret))
        })
        .collect::<Vec<_>>()
        .join("&")
}

/// Prints the shape of the endpoint, never a value that could be a secret: not the password, not the query's values.
impl std::fmt::Debug for ToolEndpoint {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let names: Vec<&str> = self.query.split('&').filter(|part| !part.is_empty()).map(|part| part.split_once('=').map_or(part, |(name, _)| name)).collect();
        f.debug_struct("ToolEndpoint")
            .field("host", &self.host)
            .field("port", &self.port)
            .field("user", &self.user)
            .field("database", &self.database)
            .field("has_password", &self.password.is_some())
            .field("query_parameters", &names)
            .finish_non_exhaustive()
    }
}

/// Why a URI cannot be used by a native tool.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ToolEndpointError {
    /// The connection is not a PostgreSQL, MySQL or MariaDB one.
    UnsupportedKind(ConnectionKind),
    /// The URI does not parse. The text is not kept: it holds the password.
    Invalid,
    /// The URI's scheme is not the connection kind's.
    Scheme(String),
}

impl std::fmt::Display for ToolEndpointError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::UnsupportedKind(_) => f.write_str("Native backup tools exist for PostgreSQL, MySQL and MariaDB connections."),
            Self::Invalid => f.write_str("The connection's address could not be read."),
            Self::Scheme(scheme) => write!(f, "The connection's address starts with {scheme}://, which does not belong to this kind of connection."),
        }
    }
}

impl std::error::Error for ToolEndpointError {}

/// Why a tool's endpoint could not be made through a tunnel.
#[derive(Debug)]
pub enum ToolTunnelError {
    Tunnel(ConnectFailure),
    Endpoint(ToolEndpointError),
}

fn decoded(text: &str) -> String {
    percent_decode_str(text).decode_utf8_lossy().into_owned()
}

impl ToolEndpoint {
    /// Reads `uri`, a URI of a `kind` connection (a saved one without its password, or any text in tests).
    pub fn parse(kind: ConnectionKind, uri: &str) -> Result<Self, ToolEndpointError> {
        let postgres = match kind {
            ConnectionKind::Postgres => true,
            ConnectionKind::Mysql | ConnectionKind::Mariadb => false,
            other => return Err(ToolEndpointError::UnsupportedKind(other)),
        };
        let url = url::Url::parse(uri).map_err(|_| ToolEndpointError::Invalid)?;
        let scheme_ok = if postgres { matches!(url.scheme(), "postgres" | "postgresql") } else { matches!(url.scheme(), "mysql" | "mariadb") };
        if !scheme_ok {
            return Err(ToolEndpointError::Scheme(url.scheme().to_owned()));
        }
        let query_pair = |name: &str| url.query_pairs().find(|(key, _)| key == name).map(|(_, value)| value.into_owned());
        // `postgres:///db?host=/var/run/postgresql`: libpq takes a socket directory as the host.
        let host = url.host_str().filter(|host| !host.is_empty()).map(decoded).or_else(|| query_pair("host")).unwrap_or_default();
        let database = Some(url.path().trim_matches('/')).filter(|path| !path.is_empty()).map(decoded).or_else(|| query_pair("dbname")).unwrap_or_default();
        Ok(Self {
            host,
            host_addr: query_pair("hostaddr").filter(|addr| !addr.is_empty()),
            port: url.port(),
            user: Some(decoded(url.username())).filter(|user| !user.is_empty()),
            password: url.password().map(|password| Secret::new(decoded(password))).filter(|password| !password.is_empty()),
            database,
            query: without_secrets(url.query().unwrap_or_default()),
        })
    }
}

impl RuntimeUri {
    /// The endpoint a native tool connects to for this URI.
    pub fn tool_endpoint(&self, kind: ConnectionKind) -> Result<ToolEndpoint, ToolEndpointError> {
        ToolEndpoint::parse(kind, self.expose())
    }

    /// The endpoint of a native tool for this URI through a tunnel of its own, opened from `access` (blocking), and the
    /// tunnel, which has to outlive the tool.
    pub fn tool_endpoint_through(&self, kind: ConnectionKind, access: &TunnelAccess) -> Result<(ToolEndpoint, SessionTunnel), ToolTunnelError> {
        let (rewritten, tunnel) = ssh::open_for_session(access, kind, self.expose()).map_err(ToolTunnelError::Tunnel)?;
        // The rewritten address carries the password like the original: wiped when it is dropped.
        let rewritten = zeroize::Zeroizing::new(rewritten);
        Ok((ToolEndpoint::parse(kind, &rewritten).map_err(ToolTunnelError::Endpoint)?, tunnel))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_parts_come_out_decoded_and_the_password_stays_a_secret() {
        let uri = RuntimeUri::new("postgres://ann%40corp:p%3Ass@db.example.com:6543/sales%20db?sslmode=require&hostaddr=127.0.0.1");
        let endpoint = uri.tool_endpoint(ConnectionKind::Postgres).unwrap();
        assert_eq!((endpoint.host.as_str(), endpoint.host_addr.as_deref(), endpoint.port, endpoint.user.as_deref(), endpoint.database.as_str()), ("db.example.com", Some("127.0.0.1"), Some(6543), Some("ann@corp"), "sales db"));
        assert_eq!(endpoint.password.as_ref().map(Secret::expose), Some("p:ss"));
        assert_eq!(endpoint.query, "sslmode=require&hostaddr=127.0.0.1");
        assert!(!format!("{endpoint:?}").contains("p:ss"));
    }

    #[test]
    fn a_secret_in_the_query_is_not_kept_and_debug_prints_no_value() {
        let uri = RuntimeUri::new("postgres://u:pw-in-userinfo@h/d?sslmode=require&password=pw-in-query&PassFile=/secrets/pgpass&sslpassword=key-pass&sslrootcert=/ca.pem");
        let endpoint = uri.tool_endpoint(ConnectionKind::Postgres).unwrap();
        assert_eq!(endpoint.query, "sslmode=require&sslrootcert=/ca.pem");
        let shown = format!("{endpoint:?}");
        for secret in ["pw-in-userinfo", "pw-in-query", "/secrets/pgpass", "key-pass", "/ca.pem", "require"] {
            assert!(!shown.contains(secret), "{secret} in {shown}");
        }
        assert!(shown.contains("sslmode") && shown.contains("has_password: true"), "{shown}");
        let mysql = ToolEndpoint::parse(ConnectionKind::Mysql, "mysql://u@h/d?ssl-mode=REQUIRED&password=x").unwrap();
        assert_eq!(mysql.query, "ssl-mode=REQUIRED");
    }

    #[test]
    fn other_kinds_and_schemes_are_refused_without_echoing_the_uri() {
        assert_eq!(ToolEndpoint::parse(ConnectionKind::Sqlite, "sqlite://x").unwrap_err(), ToolEndpointError::UnsupportedKind(ConnectionKind::Sqlite));
        assert_eq!(ToolEndpoint::parse(ConnectionKind::Postgres, "mysql://u:secret@h/d").unwrap_err(), ToolEndpointError::Scheme("mysql".into()));
        assert_eq!(ToolEndpoint::parse(ConnectionKind::Mysql, "not a uri secret").unwrap_err(), ToolEndpointError::Invalid);
    }
}
