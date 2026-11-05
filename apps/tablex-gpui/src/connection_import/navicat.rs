//! Navicat `.ncx` connection exports (XML). Passwords are Navicat 12+ AES.
//! Mirrors DBX's `navicatImport.ts`: any element carrying a type and a name or
//! host is a connection; attribute and child-element spellings vary between
//! Navicat versions, so keys are matched case- and punctuation-insensitively.

use super::{
    crypto, find_product, normalize, parse_port, ImportError, ImportKind, ImportedConnection,
    ImportedProxy, ImportedSsh, Secret,
};
use crate::connections::ConnectionKind;
use crate::ssh_tunnel::{ProxyType, SshAuthMethod};
use percent_encoding::{utf8_percent_encode, NON_ALPHANUMERIC};
use std::collections::{HashMap, HashSet};

/// Element data keyed by normalized name.
#[derive(Default)]
struct Node {
    tag: String,
    values: HashMap<String, String>,
    members: Vec<(String, u16)>,
}

impl Node {
    /// First non-empty value among `keys` (normalized before lookup).
    fn any(&self, keys: &[&str]) -> &str {
        keys.iter()
            .filter_map(|key| self.values.get(&normalize(key)))
            .map(|value| value.trim())
            .find(|value| !value.is_empty())
            .unwrap_or_default()
    }

    fn flag(&self, keys: &[&str]) -> bool {
        is_truthy(self.any(keys))
    }

    fn name(&self) -> &str {
        self.any(&["name", "connectionName", "connName", "caption", "title"])
    }

    fn host(&self) -> &str {
        self.any(&["host", "server", "hostname", "serverHost", "address"])
    }

    fn sqlite_path(&self) -> &str {
        self.any(&[
            "databaseFile",
            "databaseFileName",
            "databaseFilename",
            "filename",
            "fileName",
            "path",
            "databasePath",
            "dbPath",
            "dbFile",
            "sqliteFile",
            "sqlitePath",
            "database",
            "databaseName",
        ])
    }
}

fn is_truthy(value: &str) -> bool {
    matches!(
        value.trim().to_ascii_lowercase().as_str(),
        "1" | "true" | "yes" | "y" | "on" | "checked"
    )
}

fn attributes(element: roxmltree::Node) -> HashMap<String, String> {
    element
        .attributes()
        .map(|attr| (normalize(attr.name()), attr.value().to_string()))
        .collect()
}

fn first<'a>(values: &'a HashMap<String, String>, keys: &[&str]) -> &'a str {
    keys.iter()
        .filter_map(|key| values.get(&normalize(key)))
        .map(|value| value.trim())
        .find(|value| !value.is_empty())
        .unwrap_or_default()
}

fn read_node(element: roxmltree::Node) -> Node {
    let mut node = Node {
        tag: element.tag_name().name().to_string(),
        values: attributes(element),
        members: Vec::new(),
    };
    for child in element.children().filter(roxmltree::Node::is_element) {
        let tag = normalize(child.tag_name().name());
        let child_values = attributes(child);
        if tag == "member" {
            let host = first(&child_values, &["hostname", "host", "address"]);
            if !host.is_empty() {
                let port = parse_port(first(&child_values, &["port"])).unwrap_or(27017);
                node.members.push((host.to_string(), port));
            }
            continue;
        }
        let text = child.text().map(str::trim).unwrap_or_default();
        let key = first(&child_values, &["name", "key", "property", "field"]);
        let value = Some(first(&child_values, &["value", "val", "text", "data"]))
            .filter(|value| !value.is_empty())
            .unwrap_or(text);
        if !key.is_empty() && !value.is_empty() {
            node.values.insert(normalize(key), value.to_string());
        }
        let is_leaf = !child.children().any(|c| c.is_element());
        if is_leaf && !text.is_empty() && !node.values.contains_key(&tag) {
            node.values.insert(tag.clone(), text.to_string());
        }
        for (name, value) in &child_values {
            node.values.insert(format!("{tag}{name}"), value.clone());
        }
    }
    node
}

fn is_candidate(node: &Node) -> bool {
    if matches!(normalize(&node.tag).as_str(), "member" | "advance") {
        return false;
    }
    let kind = node.any(&["connType", "databaseType", "driver", "connectionType", "type"]);
    let has_target = !node.host().is_empty() || !node.sqlite_path().is_empty();
    has_target || (!node.name().is_empty() && !kind.is_empty())
}

/// The product of a node, `None` for non-database entries (SFTP, SSH…) and
/// for nodes that name no type at all.
fn product_of(node: &Node, port: Option<u16>) -> Option<ImportKind> {
    let raw = node.any(&["connType", "databaseType", "driver", "dbType", "connectionType", "type"]);
    let key = normalize(if raw.is_empty() { &node.tag } else { raw });

    let numeric = match key.as_str() {
        "1" => Some(ConnectionKind::Mysql),
        "2" => Some(ConnectionKind::Postgres),
        "3" => Some(ConnectionKind::Sqlite),
        "5" => Some(ConnectionKind::Mariadb),
        "8" => Some(ConnectionKind::Mongodb),
        "9" => Some(ConnectionKind::Redis),
        _ => None,
    };
    if let Some(kind) = numeric {
        return Some(ImportKind::from_kind(kind));
    }
    match key.as_str() {
        "4" => return Some(ImportKind::Unsupported("Oracle".into())),
        "7" => return Some(ImportKind::Unsupported("SQL Server".into())),
        _ => {}
    }

    // Vendor forks report the wire protocol's ConnType and name themselves
    // in ServiceProvider.
    let provider = normalize(node.any(&["serviceProvider"]));
    if provider.contains("oceanbase") {
        return Some(ImportKind::Unsupported("OceanBase".into()));
    }
    if provider.contains("gaussdb") || provider.contains("huaweicloudgauss") {
        return Some(ImportKind::Unsupported("GaussDB".into()));
    }

    if let Some(product) = find_product(&key) {
        return Some(ImportKind::of(product));
    }
    if matches!(key.as_str(), "http" | "https" | "ftp" | "sftp" | "ssh") {
        return None;
    }
    let by_port = match port {
        Some(6379) => Some(ImportKind::from_kind(ConnectionKind::Redis)),
        Some(27017) => Some(ImportKind::from_kind(ConnectionKind::Mongodb)),
        Some(5432) => Some(ImportKind::from_kind(ConnectionKind::Postgres)),
        Some(3306) => Some(ImportKind::from_kind(ConnectionKind::Mysql)),
        Some(1433) => Some(ImportKind::Unsupported("SQL Server".into())),
        Some(1521) => Some(ImportKind::Unsupported("Oracle".into())),
        _ => None,
    };
    if by_port.is_some() {
        return by_port;
    }
    (!raw.is_empty()).then(|| ImportKind::Unsupported(raw.to_string()))
}

fn ssh_of(node: &Node) -> Option<ImportedSsh> {
    if !node.flag(&["ssh", "useSsh", "sshEnabled", "enableSsh", "useSshTunnel", "sshTunnelEnabled"])
    {
        return None;
    }
    let host = node.any(&["sshHost", "sshTunnelHost", "tunnelHost"]);
    let user = node.any(&["sshUserName", "sshUsername", "sshUser", "sshTunnelUserName", "sshTunnelUsername", "tunnelUserName"]);
    // A half-filled tunnel would make an otherwise valid connection unusable.
    if host.is_empty() || user.is_empty() {
        return None;
    }
    let method = normalize(node.any(&["sshAuthenMethod", "sshAuthMethod", "sshAuthenticationMethod", "sshAuthentication", "sshAuthType"]));
    let key_path = node.any(&["sshPrivateKey", "sshKeyFile", "sshKeyPath", "sshIdentityFile", "sshTunnelPrivateKey"]);
    let uses_key = method.contains("key") || (!method.contains("password") && !key_path.is_empty());
    let port = parse_port(node.any(&["sshPort", "sshTunnelPort", "tunnelPort"])).unwrap_or(22);
    let secret = |names: &[&str]| {
        Secret::new(crypto::navicat_password(node.any(names)).unwrap_or_default())
    };
    Some(ImportedSsh {
        host: host.to_string(),
        port: port.to_string(),
        user: user.to_string(),
        auth: if uses_key { SshAuthMethod::KeyFile } else { SshAuthMethod::Password },
        key_path: if uses_key { key_path.to_string() } else { String::new() },
        // Navicat's tunnel password is encrypted like the connection password.
        password: if uses_key { None } else { secret(&["sshPassword", "sshTunnelPassword", "tunnelPassword"]) },
        passphrase: if uses_key { secret(&["sshPassphrase", "sshKeyPassphrase"]) } else { None },
        jumps: Vec::new(),
    })
}

/// The proxy of the connection, when the file carries one (`UseProxy`, `ProxyHost`, `ProxyPort`,
/// `ProxyType`, `ProxyUserName`, `ProxyPassword`: encrypted like the connection password). Written
/// from the attribute spellings Navicat uses for the SSH tunnel, tested on hand-written fixtures
/// only; a proxy with a login whose password cannot be decrypted imports off.
fn proxy_of(node: &Node) -> Option<ImportedProxy> {
    if !node.flag(&["useProxy", "proxy", "proxyEnabled", "enableProxy"]) {
        return None;
    }
    let host = node.any(&["proxyHost", "proxyServer"]);
    if host.is_empty() {
        return None;
    }
    let kind = if normalize(node.any(&["proxyType", "proxyProtocol"])).contains("http") {
        ProxyType::Http
    } else {
        ProxyType::Socks5
    };
    Some(ImportedProxy {
        kind,
        host: host.to_string(),
        port: parse_port(node.any(&["proxyPort"]))
            .map_or_else(|| kind.default_port().to_owned(), |port| port.to_string()),
        user: node.any(&["proxyUserName", "proxyUsername", "proxyUser"]).to_string(),
        password: Secret::new(
            crypto::navicat_password(node.any(&["proxyPassword"])).unwrap_or_default(),
        ),
    })
}

/// `mongodb://user@a:1,b:2/db?replicaSet=…`: Navicat leaves `Host` at a
/// `localhost` placeholder and lists the real seeds as `<Member>`s.
fn mongo_replica_uri(node: &Node, username: &str) -> Option<(String, String)> {
    if node.members.is_empty() {
        return None;
    }
    let srv = is_truthy(node.any(&["useSRVRecord", "srvRecord"]));
    let encode = |text: &str| utf8_percent_encode(text, NON_ALPHANUMERIC).to_string();
    let hosts = if srv {
        encode(&node.members[0].0)
    } else {
        node.members
            .iter()
            .map(|(host, port)| {
                if host.contains(':') {
                    format!("[{host}]:{port}")
                } else {
                    format!("{host}:{port}")
                }
            })
            .collect::<Vec<_>>()
            .join(",")
    };
    let database = node.any(&["advanceDatabase", "database", "databaseName"]);
    let mut query = url::form_urlencoded::Serializer::new(String::new());
    let mut param = |key: &str, value: &str| {
        if !value.is_empty() {
            query.append_pair(key, value);
        }
    };
    param("replicaSet", node.any(&["replicaSetName", "replicaSet"]));
    param("authSource", node.any(&["authSource"]));
    let mechanism = node.any(&["authMechanism"]);
    if !matches!(mechanism.to_ascii_lowercase().as_str(), "" | "password" | "default" | "none" | "scram") {
        param("authMechanism", &mechanism.to_ascii_uppercase());
    }
    let preference = node.any(&["readPreference"]);
    if !preference.is_empty() && !preference.eq_ignore_ascii_case("primary") {
        let mut chars = preference.chars();
        let lowered: String = chars
            .next()
            .map(|c| c.to_ascii_lowercase().to_string() + chars.as_str())
            .unwrap_or_default();
        param("readPreference", &lowered);
    }
    for (key, names) in [("retryWrites", "retryWrites"), ("retryReads", "retryReads")] {
        let value = node.any(&[names]);
        if !value.is_empty() {
            param(key, if is_truthy(value) { "true" } else { "false" });
        }
    }
    if node.flag(&["ssl", "useSsl"]) {
        param("tls", "true");
    }
    let query = query.finish();
    let userinfo = if username.is_empty() {
        String::new()
    } else {
        format!("{}@", encode(username))
    };
    let path = if database.is_empty() {
        String::new()
    } else {
        format!("/{}", encode(database))
    };
    let uri = format!(
        "{}://{userinfo}{hosts}{path}{}{query}",
        if srv { "mongodb+srv" } else { "mongodb" },
        if query.is_empty() { "" } else { "?" }
    );
    Some((uri, database.to_string()))
}

fn connection_of(node: &Node) -> Option<ImportedConnection> {
    let port = parse_port(node.any(&["port", "serverPort"]));
    let kind = product_of(node, port)?;
    let sqlite = kind.supported() == Some(ConnectionKind::Sqlite);
    let sqlite_path = if sqlite { node.sqlite_path() } else { "" };
    let host = if !sqlite_path.is_empty() {
        sqlite_path
    } else if !node.host().is_empty() {
        node.host()
    } else if sqlite {
        ""
    } else {
        "127.0.0.1"
    };
    let name = Some(node.name())
        .filter(|name| !name.is_empty())
        .or(Some(node.host()).filter(|host| !host.is_empty()))
        .or(Some(sqlite_path).filter(|path| !path.is_empty()))
        .map(str::to_string)
        .unwrap_or_else(|| kind.label());

    let mut connection = ImportedConnection::new(name, kind);
    connection.host = host.to_string();
    connection.port = port;
    connection.username = node.any(&["user", "username", "userName", "uid"]).to_string();
    if !sqlite {
        connection.database = node
            .any(&["database", "databaseName", "initialDatabase", "serviceName", "sid", "schema"])
            .to_string();
    }
    connection.password = Secret::new(
        crypto::navicat_password(node.any(&["password"])).unwrap_or_default(),
    );
    connection.tls = node.flag(&["ssl", "useSsl"]);
    connection.ssh = ssh_of(node);
    connection.proxy = proxy_of(node);
    if connection.kind.supported() == Some(ConnectionKind::Mongodb) {
        if let Some((uri, database)) = mongo_replica_uri(node, &connection.username) {
            connection.host = node.members[0].0.clone();
            connection.port = Some(node.members[0].1);
            connection.database = database;
            connection.uri = Some(uri);
        }
    }
    Some(connection)
}

pub fn parse(bytes: &[u8]) -> Result<Vec<ImportedConnection>, ImportError> {
    let text = std::str::from_utf8(bytes).map_err(|_| ImportError::InvalidXml)?;
    let document = roxmltree::Document::parse(text.trim_start_matches('\u{feff}'))
        .map_err(|_| ImportError::InvalidXml)?;
    let mut seen = HashSet::new();
    let connections = document
        .descendants()
        .filter(roxmltree::Node::is_element)
        .map(read_node)
        .filter(is_candidate)
        .filter_map(|node| connection_of(&node))
        .filter(|c| {
            seen.insert((
                c.name.clone(),
                c.kind.label(),
                c.host.clone(),
                c.port,
                c.database.clone(),
                c.uri.clone(),
            ))
        })
        .collect();
    Ok(connections)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ssh_tunnel::SshSettings;

    // "navicat-secret" and "ssh-secret" under Navicat's published AES key.
    const FIXTURE: &str = include_str!("fixtures/navicat.ncx");

    fn by_name<'a>(all: &'a [ImportedConnection], name: &str) -> &'a ImportedConnection {
        all.iter().find(|c| c.name == name).unwrap_or_else(|| panic!("{name} imported"))
    }

    #[test]
    fn a_password_tunnel_is_usable_only_with_its_decrypted_password() {
        let xml = r#"<?xml version="1.0"?><Connections>
          <Connection ConnectionName="With secret" ConnType="POSTGRESQL" Host="pg.example.com" Port="5432" UserName="app" Database="d" SSH="true" SSH_Host="jump.example.com" SSH_Port="22" SSH_UserName="ops" SSH_AuthenMethod="PASSWORD" SSH_Password="D2843183A61DB143951F074BDAE39E1B" />
          <Connection ConnectionName="No secret" ConnType="POSTGRESQL" Host="pg2.example.com" Port="5432" UserName="app" Database="d" SSH="true" SSH_Host="jump.example.com" SSH_Port="22" SSH_UserName="ops" SSH_AuthenMethod="PASSWORD" />
        </Connections>"#;
        let all = parse(xml.as_bytes()).unwrap();
        let with = by_name(&all, "With secret");
        let ssh = with.ssh.as_ref().unwrap();
        assert_eq!(ssh.auth, SshAuthMethod::Password);
        assert_eq!(ssh.password.as_ref().unwrap().expose(), "ssh-secret");
        assert!(ssh.usable());
        let record = with.to_record(1.0).unwrap();
        assert!(SshSettings::from_network_json(&record.network_settings_json).enabled);
        assert!(!record.network_settings_json.contains("ssh-secret"), "the secret never enters the record");

        let without = by_name(&all, "No secret");
        assert!(!without.ssh.as_ref().unwrap().usable());
        let record = without.to_record(1.0).unwrap();
        let ssh = SshSettings::from_network_json(&record.network_settings_json);
        assert!(!ssh.enabled, "imported disabled: the secret is missing");
        assert_eq!((ssh.host.as_str(), ssh.user.as_str()), ("jump.example.com", "ops"));
    }

    #[test]
    fn a_proxy_comes_with_its_login_and_is_off_when_the_password_is_missing() {
        let xml = r#"<?xml version="1.0"?><Connections>
          <Connection ConnectionName="Open proxy" ConnType="POSTGRESQL" Host="pg.example.com" Port="5432" UserName="app" Database="d" UseProxy="true" ProxyType="SOCKS5" ProxyHost="proxy.example.com" ProxyPort="1081" />
          <Connection ConnectionName="Login proxy" ConnType="POSTGRESQL" Host="pg2.example.com" Port="5432" UserName="app" Database="d" UseProxy="true" ProxyType="HTTP" ProxyHost="proxy.example.com" ProxyUserName="px" />
          <Connection ConnectionName="No proxy" ConnType="POSTGRESQL" Host="pg3.example.com" Port="5432" UserName="app" Database="d" ProxyHost="ignored.example.com" />
        </Connections>"#;
        let all = parse(xml.as_bytes()).unwrap();
        let open = by_name(&all, "Open proxy");
        let proxy = open.proxy.as_ref().unwrap();
        assert_eq!((proxy.kind, proxy.host.as_str(), proxy.port.as_str()), (ProxyType::Socks5, "proxy.example.com", "1081"));
        assert!(open.route_usable());
        let json: serde_json::Value = serde_json::from_str(&open.to_record(1.0).unwrap().network_settings_json).unwrap();
        assert_eq!(json["proxy"]["isEnabled"], true);
        assert_eq!(json["ssh"]["isEnabled"], false, "no tunnel was imported");
        let login = by_name(&all, "Login proxy");
        assert_eq!(login.proxy.as_ref().unwrap().kind, ProxyType::Http);
        assert!(!login.route_usable(), "the login has no password in the file");
        let json: serde_json::Value = serde_json::from_str(&login.to_record(1.0).unwrap().network_settings_json).unwrap();
        assert_eq!(json["proxy"]["isEnabled"], false);
        assert_eq!(json["proxy"]["user"], "px");
        assert!(by_name(&all, "No proxy").proxy.is_none(), "a proxy that is switched off is not imported");
    }

    #[test]
    fn imports_fields_decrypts_passwords_and_maps_ssh() {
        let all = parse(FIXTURE.as_bytes()).unwrap();
        let mysql = by_name(&all, "Local MySQL");
        assert_eq!(mysql.kind, ImportKind::Supported(ConnectionKind::Mysql));
        assert_eq!(
            (mysql.host.as_str(), mysql.port, mysql.username.as_str(), mysql.database.as_str()),
            ("127.0.0.1", Some(3306), "root", "shop")
        );
        assert_eq!(mysql.password.as_ref().unwrap().expose(), "navicat-secret");
        assert!(mysql.ssh.is_none());

        let pg = by_name(&all, "Prod PG");
        assert_eq!(pg.kind, ImportKind::Supported(ConnectionKind::Postgres));
        assert_eq!(pg.host, "pg.example.com");
        assert_eq!(pg.port, Some(5432));
        assert!(pg.tls);
        assert_eq!(
            pg.ssh,
            Some(ImportedSsh {
                host: "bastion.example.com".into(),
                port: "2222".into(),
                user: "deploy".into(),
                auth: SshAuthMethod::KeyFile,
                key_path: "/keys/id_ed25519".into(),
                password: None,
                passphrase: None,
                jumps: Vec::new(),
            })
        );
        assert!(!format!("{pg:?}").contains("ssh-secret"));
        let record = pg.to_record(1.0).unwrap();
        let ssh = SshSettings::from_network_json(&record.network_settings_json);
        assert!(ssh.enabled, "a key file is all a key tunnel needs");
        assert_eq!(
            (ssh.host.as_str(), ssh.port.as_str(), ssh.user.as_str(), ssh.key_path.as_str(), ssh.auth),
            ("bastion.example.com", "2222", "deploy", "/keys/id_ed25519", SshAuthMethod::KeyFile)
        );
        assert!(!record.network_settings_json.contains("ssh-secret"));

        let sqlite = by_name(&all, "Notes");
        assert_eq!(sqlite.kind, ImportKind::Supported(ConnectionKind::Sqlite));
        assert_eq!(sqlite.host, "/Users/me/notes.sqlite");
        assert_eq!(sqlite.password, None);

        let maria = by_name(&all, "Maria");
        assert_eq!(maria.kind, ImportKind::Supported(ConnectionKind::Mariadb));
    }

    #[test]
    fn mongodb_replica_set_becomes_a_multi_host_uri_without_the_password() {
        let all = parse(FIXTURE.as_bytes()).unwrap();
        let mongo = by_name(&all, "Replica");
        assert_eq!(mongo.kind, ImportKind::Supported(ConnectionKind::Mongodb));
        assert_eq!(
            mongo.uri.as_deref(),
            Some("mongodb://reader@m1.example.com:27017,m2.example.com:27018/app?replicaSet=rs0&authSource=admin&readPreference=secondaryPreferred&retryWrites=true&tls=true")
        );
        assert_eq!(mongo.host, "m1.example.com");
        assert_eq!(mongo.password.as_ref().unwrap().expose(), "navicat-secret");
    }

    #[test]
    fn unsupported_types_are_kept_for_the_dialog_and_non_database_entries_dropped() {
        let all = parse(FIXTURE.as_bytes()).unwrap();
        assert_eq!(
            by_name(&all, "Ledger").kind,
            ImportKind::Unsupported("Oracle".into())
        );
        assert_eq!(
            by_name(&all, "Warehouse").kind,
            ImportKind::Unsupported("SQL Server".into())
        );
        assert_eq!(
            by_name(&all, "Cache").kind,
            ImportKind::Supported(ConnectionKind::Redis)
        );
        assert!(all.iter().all(|c| c.name != "File server"));
        // The same connection listed twice is imported once.
        assert_eq!(all.iter().filter(|c| c.name == "Local MySQL").count(), 1);
        assert_eq!(all.len(), 8);
        assert_eq!(
            super::super::unsupported_labels(all.iter()),
            ["Oracle", "SQL Server"]
        );
    }

    #[test]
    fn rejects_non_xml_and_yields_nothing_for_other_xml() {
        assert_eq!(parse(b"not xml"), Err(ImportError::InvalidXml));
        assert_eq!(parse(&[0xff, 0xfe]), Err(ImportError::InvalidXml));
        assert_eq!(parse(b"<Connections Ver=\"1.5\"/>"), Ok(Vec::new()));
        // DTDs are refused, not expanded.
        assert_eq!(
            parse(b"<!DOCTYPE a [<!ENTITY x \"y\">]><a/>"),
            Err(ImportError::InvalidXml)
        );
    }
}
