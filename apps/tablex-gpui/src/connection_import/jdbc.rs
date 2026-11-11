//! The host, port and database inside a JDBC URL, for the sources (DBeaver,
//! DataGrip) that keep little else.

use super::{parse_port, Secret};
use percent_encoding::percent_decode_str;

#[derive(Debug, Default, PartialEq, Eq)]
pub(super) struct JdbcTarget {
    /// SQLite / DuckDB: the file path.
    pub host: String,
    pub port: Option<u16>,
    pub database: String,
    pub user: String,
    pub password: Option<Secret>,
    /// The URL asks for TLS (`sslmode=require`, `useSSL=true`, an `https` ClickHouse URL...).
    pub tls: bool,
}

/// `jdbc:<subprotocol>://[user[:password]@]host[:port][,more-hosts][/database][?options]`,
/// `jdbc:sqlite:<path>` and the `;key=value` tails of SQL Server URLs.
pub(super) fn parse(jdbc_url: &str) -> JdbcTarget {
    let url = jdbc_url.trim();
    let url = url
        .get(..5)
        .filter(|prefix| prefix.eq_ignore_ascii_case("jdbc:"))
        .map_or(url, |_| &url[5..]);

    let lower = url.to_ascii_lowercase();
    if let Some(scheme) = ["sqlite:", "duckdb:"]
        .into_iter()
        .find(|scheme| lower.starts_with(scheme))
    {
        let path = url[scheme.len()..].split('?').next().unwrap_or_default();
        return JdbcTarget {
            host: expand_home(path),
            ..JdbcTarget::default()
        };
    }

    let Some((scheme, rest)) = url.split_once("://") else {
        // `jdbc:postgresql:mydb`: no host, the database follows the subprotocol.
        return match lower.strip_prefix("postgresql:") {
            Some(_) => JdbcTarget {
                database: percent_decode_str(
                    url["postgresql:".len()..].split(['?', ';']).next().unwrap_or_default(),
                )
                .decode_utf8_lossy()
                .into_owned(),
                ..JdbcTarget::default()
            },
            None => JdbcTarget::default(),
        };
    };
    let (rest, query) = rest.split_once(['?', ';']).unwrap_or((rest, ""));
    let (authority, database) = rest.split_once('/').unwrap_or((rest, ""));
    let (userinfo, hosts) = authority.rsplit_once('@').unwrap_or(("", authority));
    let first = hosts.split(',').next().unwrap_or_default();
    let (host, port) = split_host_port(first);
    let (user, password) = userinfo.split_once(':').unwrap_or((userinfo, ""));
    let decode = |text: &str| percent_decode_str(text).decode_utf8_lossy().into_owned();
    // `user` / `password` may be parameters instead of userinfo (PostgreSQL, MySQL).
    let mut tls = scheme.to_ascii_lowercase().ends_with(":https");
    let (mut param_user, mut param_password) = (String::new(), String::new());
    for (key, value) in query.split(['&', ';']).filter_map(|pair| pair.split_once('=')) {
        let value = decode(value);
        match key.to_ascii_lowercase().as_str() {
            "user" => param_user = value,
            "password" => param_password = value,
            "ssl" | "usessl" | "tls" | "secure" => {
                tls |= matches!(value.to_ascii_lowercase().as_str(), "true" | "1" | "yes" | "on")
            }
            "sslmode" => {
                tls |= matches!(
                    value.to_ascii_lowercase().as_str(),
                    "require" | "required" | "verify-ca" | "verify-full" | "verify_ca" | "verify_identity"
                )
            }
            _ => {}
        }
    }
    let (user, password) = (decode(user), decode(password));
    JdbcTarget {
        host,
        port,
        database: decode(database.split('/').next().unwrap_or_default()),
        user: if user.is_empty() { param_user } else { user },
        password: Secret::new(if password.is_empty() { param_password } else { password }),
        tls,
    }
}

/// The URL of a MongoDB seed list, SRV record or single host with options (`authSource`,
/// `replicaSet`...) that the host/port fields cannot express, with any password removed.
pub(super) fn mongo_multi_host_uri(jdbc_url: &str) -> Option<String> {
    let url = jdbc_url.trim();
    let url = url
        .get(..5)
        .filter(|prefix| prefix.eq_ignore_ascii_case("jdbc:"))
        .map_or(url, |_| &url[5..]);
    let lower = url.to_ascii_lowercase();
    let srv = lower.starts_with("mongodb+srv://");
    if !srv && !lower.starts_with("mongodb://") {
        return None;
    }
    let authority = url.split_once("://")?.1.split(['/', '?', '#']).next()?;
    let hosts = authority.rsplit_once('@').map_or(authority, |(_, hosts)| hosts);
    (srv || hosts.contains(',') || url.contains('?'))
        .then(|| crate::connections::sanitized_uri(url).ok())
        .flatten()
}

/// The user name and password embedded in a URI's userinfo.
pub(super) fn uri_credentials(uri: &str) -> (String, Option<Secret>) {
    let Some(authority) = uri
        .split_once("://")
        .and_then(|(_, rest)| rest.split(['/', '?', '#']).next())
    else {
        return (String::new(), None);
    };
    let Some((userinfo, _)) = authority.rsplit_once('@') else {
        return (String::new(), None);
    };
    let (user, password) = userinfo.split_once(':').unwrap_or((userinfo, ""));
    let decode = |text: &str| percent_decode_str(text).decode_utf8_lossy().into_owned();
    (decode(user), Secret::new(decode(password)))
}

fn split_host_port(authority: &str) -> (String, Option<u16>) {
    if let Some(bracketed) = authority.strip_prefix('[') {
        let Some((host, rest)) = bracketed.split_once(']') else {
            return (authority.to_string(), None);
        };
        return (host.to_string(), rest.strip_prefix(':').and_then(parse_port));
    }
    match authority.rsplit_once(':') {
        Some((host, port)) if !host.is_empty() => (host.to_string(), parse_port(port)),
        _ => (authority.to_string(), None),
    }
}

/// JetBrains stores home-relative paths as `$USER_HOME$/…`.
pub(super) fn expand_home(path: &str) -> String {
    match (path.contains("$USER_HOME$"), dirs::home_dir()) {
        (true, Some(home)) => path.replace("$USER_HOME$", &home.to_string_lossy()),
        _ => path.to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn authority_form_with_port_database_and_credentials() {
        let target = parse("jdbc:postgresql://u%40x:p%2Fw@db.example.com:5433/shop?ssl=true");
        assert_eq!(target.host, "db.example.com");
        assert_eq!(target.port, Some(5433));
        assert_eq!(target.database, "shop");
        assert_eq!(target.user, "u@x");
        assert_eq!(target.password.unwrap().expose(), "p/w");
    }

    #[test]
    fn hosts_without_port_ipv6_multi_host_and_sql_server_tail() {
        assert_eq!(parse("jdbc:mysql://localhost/app").port, None);
        let v6 = parse("jdbc:mysql://[::1]:3307/app");
        assert_eq!((v6.host.as_str(), v6.port), ("::1", Some(3307)));
        let multi = parse("jdbc:mongodb://a:1,b:2/db");
        assert_eq!((multi.host.as_str(), multi.port), ("a", Some(1)));
        let mssql = parse("jdbc:sqlserver://srv:1433;databaseName=x");
        assert_eq!((mssql.host.as_str(), mssql.port), ("srv", Some(1433)));
    }

    #[test]
    fn mongo_seed_lists_and_srv_keep_the_uri_but_lose_the_password() {
        assert_eq!(
            mongo_multi_host_uri("mongodb://u:pw@a:1,b:2/db?replicaSet=r").as_deref(),
            Some("mongodb://u@a:1,b:2/db?replicaSet=r")
        );
        assert!(mongo_multi_host_uri("jdbc:mongodb+srv://c.example.com/db").is_some());
        assert_eq!(mongo_multi_host_uri("mongodb://a:1/db"), None);
        assert_eq!(mongo_multi_host_uri("jdbc:mysql://a,b/db"), None);
    }

    #[test]
    fn sqlite_paths_and_unparsable_urls() {
        assert_eq!(parse("jdbc:sqlite:/tmp/a.db?x=1").host, "/tmp/a.db");
        let home = dirs::home_dir().unwrap();
        assert_eq!(
            parse("jdbc:sqlite:$USER_HOME$/a.db").host,
            format!("{}/a.db", home.display())
        );
        assert_eq!(parse("not a url"), JdbcTarget::default());
    }

    #[test]
    fn options_set_tls_and_carry_the_login_when_userinfo_has_none() {
        let tls = |url: &str| parse(url).tls;
        assert!(tls("jdbc:postgresql://h/db?sslmode=require"));
        assert!(tls("jdbc:postgresql://h/db?sslmode=verify-full"));
        assert!(!tls("jdbc:postgresql://h/db?sslmode=disable"));
        assert!(tls("jdbc:mysql://h/db?useSSL=true&x=1"));
        assert!(!tls("jdbc:mysql://h/db?useSSL=false"));
        assert!(tls("jdbc:mysql://h/db?sslMode=REQUIRED"));
        assert!(tls("jdbc:clickhouse:https://h:8443/db"));
        assert!(!tls("jdbc:clickhouse://h:8123/db"));
        assert!(tls("jdbc:sqlserver://h:1433;encrypt=true;ssl=true"));
        let target = parse("jdbc:postgresql://h/db?user=app%40x&password=p%26w");
        assert_eq!((target.user.as_str(), target.password.unwrap().expose()), ("app@x", "p&w"));
        // The userinfo wins over the parameters.
        assert_eq!(parse("jdbc:postgresql://u1:p1@h/db?user=u2&password=p2").user, "u1");
    }

    #[test]
    fn a_single_host_mongo_url_keeps_its_options_and_a_postgres_url_without_slashes_its_database() {
        assert_eq!(
            mongo_multi_host_uri("jdbc:mongodb://u:pw@a:1/db?authSource=admin&tls=true").as_deref(),
            Some("mongodb://u@a:1/db?authSource=admin&tls=true")
        );
        assert_eq!(mongo_multi_host_uri("mongodb://a:1/db"), None);
        let target = parse("jdbc:postgresql:mydb?ssl=true");
        assert_eq!((target.host.as_str(), target.database.as_str()), ("", "mydb"));
        assert_eq!(parse("jdbc:mysql:mydb"), JdbcTarget::default());
    }
}
