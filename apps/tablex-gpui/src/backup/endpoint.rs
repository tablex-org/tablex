//! Where a native client tool connects: the host, port, user, password, database and TLS settings a
//! connection's URI holds, read into plain fields (the tool gets them as arguments, environment and
//! private files, never as the URI).

use engine_connect::tool_endpoint::ToolEndpoint;
use engine_net::Secret;

use crate::connection_tls::{TlsFamily, TlsSettings};
use crate::connections::ConnectionKind;

/// What a connection's URI says about reaching the server.
#[derive(Debug, Clone)]
pub struct Endpoint {
    /// Empty: the tool's own default (a local socket).
    pub host: String,
    /// PostgreSQL's `hostaddr`: the address the connection really goes to while `host` stays the name TLS checks
    /// and the password file is matched on. A tunnel sets it to its local address.
    pub host_addr: Option<String>,
    pub port: Option<u16>,
    pub user: Option<String>,
    pub password: Option<Secret>,
    pub database: String,
    pub tls: TlsSettings,
}

/// Why a URI cannot be used by a native tool.
pub use engine_connect::tool_endpoint::ToolEndpointError as EndpointError;

impl Endpoint {
    /// Reads `uri`, a `kind` connection's address that carries no secret the app must protect (a saved connection's, or a
    /// test's). The runtime URI of an open session is read inside `engine-connect` ([`Self::from_tool`]).
    pub fn parse(kind: ConnectionKind, uri: &str) -> Result<Self, EndpointError> {
        Self::from_tool(kind, ToolEndpoint::parse(kind, uri)?)
    }

    /// The endpoint for the parts `engine-connect` read from a connection's URI: the TLS settings come out of its query.
    pub fn from_tool(kind: ConnectionKind, parts: ToolEndpoint) -> Result<Self, EndpointError> {
        let family = TlsFamily::of(kind).filter(|family| *family != TlsFamily::Mongo).ok_or(EndpointError::UnsupportedKind(kind))?;
        let (tls, _other_options) = TlsSettings::extract(family, &parts.query);
        Ok(Self { host: parts.host, host_addr: parts.host_addr, port: parts.port, user: parts.user, password: parts.password, database: parts.database, tls })
    }

    /// The password text, for the private file of the tool and for scrubbing its output.
    pub fn password_text(&self) -> Option<&str> {
        self.password.as_ref().map(Secret::expose)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::connection_tls::TlsMode;

    #[test]
    fn a_postgres_uri_gives_its_parts_decoded_and_its_tls_settings() {
        let endpoint = Endpoint::parse(
            ConnectionKind::Postgres,
            "postgres://ann%40corp:p%3Ass%2Fw%40rd@db.example.com:6543/sales%20db?sslmode=verify-full&sslrootcert=/etc/ca.pem&sslcert=/c.pem&sslkey=/k.pem&application_name=x",
        )
        .unwrap();
        assert_eq!((endpoint.host.as_str(), endpoint.port, endpoint.user.as_deref(), endpoint.database.as_str()), ("db.example.com", Some(6543), Some("ann@corp"), "sales db"));
        assert_eq!(endpoint.password_text(), Some("p:ss/w@rd"));
        assert_eq!(endpoint.tls.mode, TlsMode::VerifyFull);
        assert_eq!((endpoint.tls.ca_file.as_str(), endpoint.tls.client_cert.as_str(), endpoint.tls.client_key.as_str()), ("/etc/ca.pem", "/c.pem", "/k.pem"));
    }

    #[test]
    fn a_mysql_uri_gives_its_parts_and_a_missing_password_stays_missing() {
        let endpoint = Endpoint::parse(ConnectionKind::Mysql, "mysql://root@127.0.0.1:3306/shop?ssl-mode=REQUIRED&ssl-ca=/ca.pem").unwrap();
        assert_eq!((endpoint.host.as_str(), endpoint.port, endpoint.user.as_deref(), endpoint.database.as_str()), ("127.0.0.1", Some(3306), Some("root"), "shop"));
        assert!(endpoint.password.is_none());
        assert_eq!((endpoint.tls.mode, endpoint.tls.ca_file.as_str()), (TlsMode::Require, "/ca.pem"));
        let mariadb = Endpoint::parse(ConnectionKind::Mariadb, "mariadb://u:p@h/db").unwrap();
        assert_eq!(mariadb.password_text(), Some("p"));
    }

    #[test]
    fn the_hostaddr_of_a_tunnelled_uri_is_kept_apart_from_the_host() {
        let endpoint = Endpoint::parse(ConnectionKind::Postgres, "postgres://u@db.internal.example.com:41001/sales?sslmode=verify-full&hostaddr=127.0.0.1").unwrap();
        assert_eq!((endpoint.host.as_str(), endpoint.host_addr.as_deref(), endpoint.port), ("db.internal.example.com", Some("127.0.0.1"), Some(41001)));
        assert!(Endpoint::parse(ConnectionKind::Postgres, "postgres://u@h/db").unwrap().host_addr.is_none());
    }

    #[test]
    fn a_socket_directory_in_the_query_is_the_host() {
        let endpoint = Endpoint::parse(ConnectionKind::Postgres, "postgres:///app?host=/var/run/postgresql").unwrap();
        assert_eq!((endpoint.host.as_str(), endpoint.database.as_str()), ("/var/run/postgresql", "app"));
    }

    #[test]
    fn other_kinds_and_other_schemes_are_refused_without_echoing_the_uri() {
        assert_eq!(Endpoint::parse(ConnectionKind::Sqlite, "sqlite://x.db").unwrap_err(), EndpointError::UnsupportedKind(ConnectionKind::Sqlite));
        assert_eq!(Endpoint::parse(ConnectionKind::Postgres, "mysql://u:secret@h/db").unwrap_err(), EndpointError::Scheme("mysql".into()));
        let error = Endpoint::parse(ConnectionKind::Postgres, "not a uri with secret").unwrap_err();
        assert_eq!(error, EndpointError::Invalid);
        assert!(!format!("{error:?} {error}").contains("secret"));
    }

    #[test]
    fn the_debug_text_of_an_endpoint_hides_the_password() {
        let endpoint = Endpoint::parse(ConnectionKind::Postgres, "postgres://u:hunter2@h/db").unwrap();
        assert!(!format!("{endpoint:?}").contains("hunter2"));
    }
}
