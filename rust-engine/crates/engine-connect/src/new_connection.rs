//! The record of a connection made from plain values instead of the New Connection dialog: what
//! `tablex-mcp`'s `add_connection` stores. It builds the same record the dialog does (the
//! uppercase UUID, the persisted URI without a password, the driver id, `save_password`, the
//! connection type, no tunnel) from the same pieces of [`crate::records`], and it accepts less:
//! no tunnel or proxy, no certificate or key files, and only the URI options that
//! [`SHAREABLE_OPTIONS`] lists. The password is never part of the record or of its URI: the caller
//! stores it in the credential store.

use crate::records::{self, ConnectionKind, SHAREABLE_OPTIONS};
use engine_storage::connection::SavedConnectionRecord;

/// The longest connection name.
pub const NAME_MAX_CHARS: usize = 128;
/// The longest URI, path, host, database or user name.
pub const URI_MAX_CHARS: usize = 2048;
pub const HOST_MAX_CHARS: usize = 253;
pub const FIELD_MAX_CHARS: usize = 256;
/// The longest password.
pub const PASSWORD_MAX_CHARS: usize = 1024;

/// The sidebar colour a new connection gets in the dialog.
const DEFAULT_COLOR: &str = "#34C759";

/// The TLS choice of a connection, the part of the dialog's TLS tab that needs no file.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Tls {
    Disable,
    Require,
    VerifyFull,
}

impl Tls {
    pub const IDS: [&'static str; 3] = ["disable", "require", "verify_full"];

    pub fn from_id(id: &str) -> Option<Self> {
        Some(match id {
            "disable" => Self::Disable,
            "require" => Self::Require,
            "verify_full" => Self::VerifyFull,
            _ => return None,
        })
    }
}

/// What the caller gives. Exactly one of `uri` and the fields; the password is not here.
#[derive(Debug, Clone)]
pub struct NewConnection {
    pub name: String,
    pub kind: ConnectionKind,
    pub uri: Option<String>,
    pub host: Option<String>,
    pub port: Option<i64>,
    pub database: Option<String>,
    pub user: Option<String>,
    /// SQLite: the database file.
    pub path: Option<String>,
    pub tls: Option<Tls>,
    /// The id of a connection type that exists (the caller resolves it).
    pub connection_type: String,
}

/// A new connection id, in the spelling the dialog uses.
pub fn new_id() -> String {
    uuid::Uuid::new_v4().to_string()
}

/// A name for the sidebar: not blank, short, one line.
pub fn check_name(name: &str) -> Result<String, String> {
    let name = name.trim();
    if name.is_empty() {
        return Err("the connection name is empty".into());
    }
    if name.chars().count() > NAME_MAX_CHARS {
        return Err(format!("the connection name is longer than {NAME_MAX_CHARS} characters"));
    }
    if name.chars().any(char::is_control) {
        return Err("the connection name has a control character or a line break".into());
    }
    Ok(name.to_owned())
}

/// A password that can be stored: not empty, short, no NUL.
pub fn check_password(password: &str) -> Result<(), String> {
    if password.is_empty() {
        return Err("the password is empty; leave it out for a connection without a saved password".into());
    }
    if password.chars().count() > PASSWORD_MAX_CHARS {
        return Err(format!("the password is longer than {PASSWORD_MAX_CHARS} characters"));
    }
    if password.contains('\0') {
        return Err("the password has a NUL character".into());
    }
    Ok(())
}

fn check_text(what: &str, text: &str, max: usize) -> Result<String, String> {
    let text = text.trim();
    if text.is_empty() {
        return Err(format!("`{what}` is empty"));
    }
    if text.chars().count() > max {
        return Err(format!("`{what}` is longer than {max} characters"));
    }
    if text.chars().any(char::is_control) {
        return Err(format!("`{what}` has a control character or a line break"));
    }
    Ok(text.to_owned())
}

/// The record `new` describes, with `id` (any UUID spelling) and the time `now` (seconds).
pub fn build_record(new: &NewConnection, id: &str, now: f64) -> Result<SavedConnectionRecord, String> {
    let name = check_name(&new.name)?;
    let fields = [&new.host, &new.database, &new.user, &new.path]
        .iter()
        .any(|field| field.is_some())
        || new.port.is_some();
    let uri = match (&new.uri, fields) {
        (Some(_), true) => return Err("give either `uri` or the separate fields, not both".into()),
        (None, false) => {
            return Err(if new.kind == ConnectionKind::Sqlite {
                "give `path` (or a `uri`)".into()
            } else {
                "give `uri`, or `host` and the other fields".into()
            })
        }
        (Some(_), false) if new.tls.is_some() => {
            return Err("`options.tls` goes with the separate fields; put the TLS option in the `uri`".into())
        }
        (Some(uri), false) => from_uri(new.kind, uri)?,
        (None, true) => from_fields(new)?,
    };
    Ok(SavedConnectionRecord {
        id: records::storage_connection_id(id),
        name,
        uri,
        db_type: new.kind.driver_id().to_owned(),
        color_hex: DEFAULT_COLOR.to_owned(),
        save_password: false,
        save_proxy_password: false,
        network_settings_json: "{}".to_owned(),
        is_pinned: false,
        last_connected: None,
        created_at: now,
        updated_at: now,
        connection_type: new.connection_type.clone(),
        overrides_json: String::new(),
    })
}

fn sqlite_uri(path: &str) -> Result<String, String> {
    let path = check_text("path", path, URI_MAX_CHARS)?;
    if !std::path::Path::new(&path).is_absolute() {
        return Err("the SQLite `path` must be an absolute path".into());
    }
    Ok(format!("sqlite://{path}"))
}

fn from_fields(new: &NewConnection) -> Result<String, String> {
    if new.kind == ConnectionKind::Sqlite {
        if new.host.is_some() || new.port.is_some() || new.database.is_some() || new.user.is_some() || new.tls.is_some() {
            return Err("a SQLite connection takes only `path`".into());
        }
        return sqlite_uri(new.path.as_deref().unwrap_or_default());
    }
    if new.path.is_some() {
        return Err("`path` is for SQLite; give `host` for this kind".into());
    }
    let host = check_text("host", new.host.as_deref().unwrap_or_default(), HOST_MAX_CHARS)?;
    if !host
        .chars()
        .all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '-' | '_' | '[' | ']' | ':' | '%'))
    {
        return Err("`host` is not a host name or address (no port, path or user in it)".into());
    }
    let tls = new.tls;
    let scheme = match new.kind {
        ConnectionKind::Postgres => "postgresql",
        ConnectionKind::Redis if matches!(tls, Some(Tls::Require | Tls::VerifyFull)) => "rediss",
        other => other.expected_scheme(),
    };
    let mut url = url::Url::parse(&format!("{scheme}://{host}")).map_err(|_| "`host` is not a host name or address".to_owned())?;
    if url.host().is_none() || url.path() != "" && url.path() != "/" {
        return Err("`host` is not a host name or address".into());
    }
    if let Some(user) = &new.user {
        let user = check_text("user", user, FIELD_MAX_CHARS)?;
        url.set_username(&user).map_err(|()| "`user` cannot be used with this host".to_owned())?;
    }
    if let Some(port) = new.port {
        let port = u16::try_from(port).ok().filter(|port| *port != 0).ok_or("`port` must be between 1 and 65535")?;
        url.set_port(Some(port)).map_err(|()| "`port` cannot be used with this host".to_owned())?;
    }
    if let Some(database) = &new.database {
        let database = check_text("database", database, FIELD_MAX_CHARS)?;
        url.set_path(&format!("/{database}"));
    }
    if let Some(tls) = tls {
        let pair = match (new.kind, tls) {
            (ConnectionKind::Postgres, Tls::Disable) => Some(("sslmode", "disable")),
            (ConnectionKind::Postgres, Tls::Require) => Some(("sslmode", "require")),
            (ConnectionKind::Postgres, Tls::VerifyFull) => Some(("sslmode", "verify-full")),
            (ConnectionKind::Mysql | ConnectionKind::Mariadb, Tls::Disable) => Some(("ssl-mode", "DISABLED")),
            (ConnectionKind::Mysql | ConnectionKind::Mariadb, Tls::Require) => Some(("ssl-mode", "REQUIRED")),
            (ConnectionKind::Mysql | ConnectionKind::Mariadb, Tls::VerifyFull) => Some(("ssl-mode", "VERIFY_IDENTITY")),
            (ConnectionKind::Mongodb, Tls::Disable) => Some(("tls", "false")),
            (ConnectionKind::Mongodb, Tls::VerifyFull) => Some(("tls", "true")),
            (ConnectionKind::Mongodb, Tls::Require) => {
                return Err("MongoDB `options.tls` is `disable` or `verify_full`: `require` would accept any certificate".into())
            }
            // The scheme carries it.
            (ConnectionKind::Redis, _) => None,
            (ConnectionKind::Clickhouse, Tls::Disable) => None,
            (ConnectionKind::Clickhouse, _) => Some(("secure", "true")),
            (ConnectionKind::Sqlite, _) => None,
        };
        if let Some((key, value)) = pair {
            url.query_pairs_mut().append_pair(key, value);
        }
    }
    records::validate_uri(new.kind, url.as_str())?;
    Ok(url.to_string())
}

/// Whether the userinfo of `uri` holds a password.
fn has_password(kind: ConnectionKind, uri: &str) -> bool {
    if kind == ConnectionKind::Mongodb {
        let rest = uri.split_once("://").map_or("", |(_, rest)| rest);
        let authority = rest.split(['/', '?']).next().unwrap_or_default();
        return authority.rsplit_once('@').is_some_and(|(userinfo, _)| userinfo.contains(':'));
    }
    url::Url::parse(uri).is_ok_and(|url| url.password().is_some())
}

fn from_uri(kind: ConnectionKind, uri: &str) -> Result<String, String> {
    let uri = records::normalize_uri_scheme(&check_text("uri", uri, URI_MAX_CHARS)?);
    if uri.chars().any(char::is_whitespace) {
        return Err("`uri` has a blank in it".into());
    }
    if kind == ConnectionKind::Sqlite {
        let path = uri
            .get(..9)
            .filter(|scheme| scheme.eq_ignore_ascii_case("sqlite://"))
            .map(|_| &uri[9..])
            .ok_or("the SQLite `uri` must start with sqlite://")?;
        return sqlite_uri(path);
    }
    if uri.contains('#') {
        return Err("`uri` has a fragment (#), which is not accepted".into());
    }
    records::validate_uri(kind, &uri)?;
    if has_password(kind, &uri) {
        return Err("the `uri` has a password in it; leave it out of the `uri` and give it as `password`".into());
    }
    if let Some((_, query)) = uri.split_once('?') {
        for (key, _) in url::form_urlencoded::parse(query.as_bytes()) {
            let lower = key.to_ascii_lowercase();
            if lower == "ca_cert" || !SHAREABLE_OPTIONS.contains(&lower.as_str()) {
                let shown: String = key.chars().take(40).collect();
                return Err(format!(
                    "the `uri` option `{shown}` is not accepted here; options are limited to routing and TLS choices ({}), set the rest in the tablex app",
                    SHAREABLE_OPTIONS.iter().filter(|option| **option != "ca_cert").copied().collect::<Vec<_>>().join(", ")
                ));
            }
        }
    }
    records::sanitized_uri(&uri)
}

#[cfg(test)]
mod tests {
    use super::*;

    const ID: &str = "7b1d3f0e-3a55-4d8e-9a54-0c6f2f4e9a11";

    fn new(kind: ConnectionKind) -> NewConnection {
        NewConnection {
            name: "local-pg".into(),
            kind,
            uri: None,
            host: None,
            port: None,
            database: None,
            user: None,
            path: None,
            tls: None,
            connection_type: "development".into(),
        }
    }

    fn fields(kind: ConnectionKind) -> NewConnection {
        NewConnection {
            host: Some("db.local".into()),
            port: Some(5432),
            database: Some("shop".into()),
            user: Some("app".into()),
            ..new(kind)
        }
    }

    #[test]
    fn fields_build_the_record_the_dialog_builds() {
        let record = build_record(&fields(ConnectionKind::Postgres), ID, 10.0).unwrap();
        assert_eq!(record.id, "7B1D3F0E-3A55-4D8E-9A54-0C6F2F4E9A11");
        assert_eq!(record.uri, "postgresql://app@db.local:5432/shop");
        assert_eq!(record.db_type, "postgresql");
        assert_eq!(record.connection_type, "development");
        assert_eq!((record.network_settings_json.as_str(), record.overrides_json.as_str()), ("{}", ""));
        assert!(!record.save_password && !record.is_pinned);
        assert_eq!((record.created_at, record.updated_at), (10.0, 10.0));
    }

    #[test]
    fn every_kind_gets_its_own_scheme() {
        for (kind, scheme) in [
            (ConnectionKind::Mysql, "mysql"),
            (ConnectionKind::Mariadb, "mariadb"),
            (ConnectionKind::Mongodb, "mongodb"),
            (ConnectionKind::Redis, "redis"),
            (ConnectionKind::Clickhouse, "clickhouse"),
        ] {
            let record = build_record(&fields(kind), ID, 0.0).unwrap();
            assert!(record.uri.starts_with(&format!("{scheme}://app@db.local:5432/shop")), "{}", record.uri);
        }
    }

    #[test]
    fn a_user_with_special_characters_is_encoded() {
        let mut input = fields(ConnectionKind::Postgres);
        input.user = Some("a b@c".into());
        let record = build_record(&input, ID, 0.0).unwrap();
        assert!(record.uri.starts_with("postgresql://a%20b%40c@db.local"), "{}", record.uri);
    }

    #[test]
    fn tls_is_written_the_way_the_dialog_writes_it() {
        let with = |kind, tls| build_record(&NewConnection { tls: Some(tls), ..fields(kind) }, ID, 0.0).map(|r| r.uri);
        assert!(with(ConnectionKind::Postgres, Tls::VerifyFull).unwrap().ends_with("?sslmode=verify-full"));
        assert!(with(ConnectionKind::Mysql, Tls::Require).unwrap().ends_with("?ssl-mode=REQUIRED"));
        assert!(with(ConnectionKind::Mongodb, Tls::Disable).unwrap().ends_with("?tls=false"));
        assert!(with(ConnectionKind::Redis, Tls::Require).unwrap().starts_with("rediss://"));
        assert!(with(ConnectionKind::Clickhouse, Tls::Require).unwrap().ends_with("?secure=true"));
        assert!(with(ConnectionKind::Mongodb, Tls::Require).unwrap_err().contains("any certificate"));
        let mut sqlite = new(ConnectionKind::Sqlite);
        sqlite.path = Some("/tmp/shop.db".into());
        sqlite.tls = Some(Tls::Require);
        assert!(build_record(&sqlite, ID, 0.0).is_err());
    }

    #[test]
    fn sqlite_takes_an_absolute_path() {
        let mut input = new(ConnectionKind::Sqlite);
        input.path = Some("/tmp/shop.db".into());
        assert_eq!(build_record(&input, ID, 0.0).unwrap().uri, "sqlite:///tmp/shop.db");
        input.path = Some("shop.db".into());
        assert!(build_record(&input, ID, 0.0).unwrap_err().contains("absolute"));
        let mut by_uri = new(ConnectionKind::Sqlite);
        by_uri.uri = Some("SQLITE:///tmp/shop.db".into());
        assert_eq!(build_record(&by_uri, ID, 0.0).unwrap().uri, "sqlite:///tmp/shop.db");
        by_uri.uri = Some("postgresql://h/db".into());
        assert!(build_record(&by_uri, ID, 0.0).is_err());
    }

    #[test]
    fn a_uri_is_normalised_and_checked_against_the_kind() {
        let mut input = new(ConnectionKind::Postgres);
        input.uri = Some("POSTGRESQL://app@db.local:5432/shop?sslmode=require".into());
        assert_eq!(build_record(&input, ID, 0.0).unwrap().uri, "postgresql://app@db.local:5432/shop?sslmode=require");
        input.uri = Some("mysql://app@db.local/shop".into());
        assert!(build_record(&input, ID, 0.0).unwrap_err().contains("does not match"));
        input.uri = Some("not a uri".into());
        assert!(build_record(&input, ID, 0.0).is_err());
    }

    #[test]
    fn a_password_in_the_uri_is_refused_and_not_echoed() {
        for (kind, uri) in [
            (ConnectionKind::Postgres, "postgresql://app:hunter2-secret@db.local/shop"),
            (ConnectionKind::Mongodb, "mongodb://app:hunter2-secret@db.local,db2.local/shop"),
        ] {
            let mut input = new(kind);
            input.uri = Some(uri.into());
            let error = build_record(&input, ID, 0.0).unwrap_err();
            assert!(error.contains("password") && !error.contains("hunter2"), "{error}");
        }
    }

    #[test]
    fn only_routing_options_and_no_fragment_are_accepted_in_a_uri() {
        let mut input = new(ConnectionKind::Postgres);
        for uri in [
            "postgresql://db.local/shop?sslrootcert=/etc/ca.pem",
            "postgresql://db.local/shop?password=hunter2",
            "postgresql://db.local/shop#frag",
            "clickhouse://db.local/shop?ca_cert=/etc/ca.pem",
        ] {
            input.kind = if uri.starts_with("click") { ConnectionKind::Clickhouse } else { ConnectionKind::Postgres };
            input.uri = Some(uri.into());
            let error = build_record(&input, ID, 0.0).unwrap_err();
            assert!(!error.contains("hunter2"), "{error}");
        }
        input.kind = ConnectionKind::Mongodb;
        input.uri = Some("mongodb+srv://app@cluster.local/shop?authSource=admin&tls=true".into());
        assert!(build_record(&input, ID, 0.0).is_ok());
    }

    #[test]
    fn bad_input_is_refused_with_a_reason() {
        let bad = |change: &dyn Fn(&mut NewConnection)| {
            let mut input = fields(ConnectionKind::Postgres);
            change(&mut input);
            build_record(&input, ID, 0.0).unwrap_err()
        };
        assert!(bad(&|i| i.name = "  ".into()).contains("empty"));
        assert!(bad(&|i| i.name = "x".repeat(129)).contains("longer"));
        assert!(bad(&|i| i.name = "a\nb".into()).contains("line break"));
        assert!(bad(&|i| i.port = Some(0)).contains("1 and 65535"));
        assert!(bad(&|i| i.port = Some(70_000)).contains("1 and 65535"));
        assert!(bad(&|i| i.port = Some(-1)).contains("1 and 65535"));
        assert!(bad(&|i| i.host = Some("db.local/evil".into())).contains("host"));
        assert!(bad(&|i| i.host = Some("user@db.local".into())).contains("host"));
        assert!(bad(&|i| i.host = Some(" ".into())).contains("empty"));
        assert!(bad(&|i| i.path = Some("/tmp/x.db".into())).contains("SQLite"));
        assert!(bad(&|i| i.uri = Some("postgresql://h/db".into())).contains("not both"));
        assert!(bad(&|i| *i = new(ConnectionKind::Postgres)).contains("`uri`"));
    }

    #[test]
    fn a_password_is_checked_not_judged() {
        assert!(check_password("").is_err());
        assert!(check_password("a\0b").is_err());
        assert!(check_password(&"x".repeat(1025)).is_err());
        assert!(check_password("p@ss/w0rd:Zq81").is_ok());
    }
}
