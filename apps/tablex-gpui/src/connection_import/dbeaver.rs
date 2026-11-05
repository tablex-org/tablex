//! DBeaver workspaces: `data-sources.json` plus the AES-encrypted
//! `credentials-config.json` next to it. Mirrors DBX's `dbeaverImport.ts`.
//! Folders are not imported.

use super::{
    crypto, find_product, jdbc, normalize, parse_port, ImportError, ImportKind,
    ImportedConnection, ImportedJump, ImportedProxy, ImportedSsh, Secret,
};
use crate::connections::ConnectionKind;
use crate::ssh_tunnel::{ProxyType, SshAuthMethod};
use serde_json::Value;
use std::collections::HashSet;
use std::path::{Path, PathBuf};

#[derive(Debug, PartialEq, Eq)]
pub struct DbeaverFiles {
    pub data_sources: PathBuf,
    pub credentials: Option<PathBuf>,
}

/// Where `data-sources.json` sits below a folder the user may have picked:
/// the `.dbeaver` folder itself, or any parent up to `DBeaverData`.
const WORKSPACE_SUBDIRS: &[&str] = &[
    "",
    ".dbeaver",
    "General/.dbeaver",
    "workspace6/General/.dbeaver",
    "DBeaverData/workspace6/General/.dbeaver",
];

fn file_name(path: &Path) -> String {
    path.file_name()
        .map(|name| name.to_string_lossy().to_ascii_lowercase())
        .unwrap_or_default()
}

fn is_data_sources(name: &str) -> bool {
    name.starts_with("data-sources") && name.ends_with(".json")
}

fn is_credentials(name: &str) -> bool {
    name.starts_with("credentials-config") && name.ends_with(".json")
}

/// `data-sources.json` → `credentials-config.json`;
/// `data-sources-extra.json` → `credentials-config-extra.json`.
fn sibling_credentials(data_sources: &Path) -> PathBuf {
    let name = file_name(data_sources);
    let credentials = match name.strip_prefix("data-sources-") {
        Some(rest) => format!("credentials-config-{rest}"),
        None => "credentials-config.json".to_string(),
    };
    data_sources.with_file_name(credentials)
}

/// Resolves what the picker returned (files and/or folders) to the data
/// sources file and its credentials file.
pub fn locate(paths: &[PathBuf]) -> Result<DbeaverFiles, ImportError> {
    let mut data_sources = None;
    let mut credentials = None;
    for path in paths {
        if path.is_dir() {
            let found = WORKSPACE_SUBDIRS
                .iter()
                .map(|sub| path.join(sub))
                .find_map(|dir| {
                    let plain = dir.join("data-sources.json");
                    if plain.is_file() {
                        return Some(plain);
                    }
                    let mut others: Vec<_> = std::fs::read_dir(&dir)
                        .ok()?
                        .filter_map(Result::ok)
                        .map(|entry| entry.path())
                        .filter(|p| p.is_file() && is_data_sources(&file_name(p)))
                        .collect();
                    others.sort();
                    others.into_iter().next()
                });
            data_sources = data_sources.or(found);
        } else if is_credentials(&file_name(path)) {
            credentials = Some(path.clone());
        } else if path.extension().is_some_and(|ext| ext == "json") {
            data_sources = data_sources.or(Some(path.clone()));
        }
    }
    let data_sources = data_sources.ok_or(ImportError::MissingFile("data-sources.json"))?;
    let credentials =
        credentials.or_else(|| Some(sibling_credentials(&data_sources)).filter(|p| p.is_file()));
    Ok(DbeaverFiles {
        data_sources,
        credentials,
    })
}

fn text(value: Option<&Value>) -> String {
    match value {
        Some(Value::String(text)) => text.trim().to_string(),
        Some(Value::Number(number)) => number.to_string(),
        _ => String::new(),
    }
}

fn first_text(values: &[Option<&Value>]) -> String {
    values
        .iter()
        .map(|value| text(*value))
        .find(|value| !value.is_empty())
        .unwrap_or_default()
}

/// `credentials-config.json` once decrypted: `{ "<id>": { "#connection": { user, password } } }`.
fn decrypt_credentials(file: Option<&[u8]>) -> Value {
    file.and_then(crypto::dbeaver_credentials)
        .and_then(|json| serde_json::from_slice(&json).ok())
        .unwrap_or(Value::Null)
}

fn entries(parsed: &Value) -> Vec<(String, &Value)> {
    let source = ["connections", "dataSources", "datasources"]
        .iter()
        .find_map(|key| parsed.get(key))
        .unwrap_or(parsed);
    match source {
        Value::Object(map) => map
            .iter()
            .filter(|(_, entry)| entry.is_object())
            .map(|(id, entry)| {
                let own_id = text(entry.get("id"));
                (if own_id.is_empty() { id.clone() } else { own_id }, entry)
            })
            .collect(),
        Value::Array(list) => list
            .iter()
            .filter(|entry| entry.is_object())
            .map(|entry| {
                let id = first_text(&[entry.get("id"), entry.get("uuid"), entry.get("name")]);
                (id, entry)
            })
            .collect(),
        _ => Vec::new(),
    }
}

fn authentication(properties: &Value, prefix: &str) -> SshAuthMethod {
    match text(properties.get(format!("{prefix}authType"))).as_str() {
        "PUBLIC_KEY" => SshAuthMethod::KeyFile,
        "AGENT" => SshAuthMethod::Agent,
        _ => SshAuthMethod::Password,
    }
}

/// A flag DBeaver wrote as a boolean or as text.
fn flag(value: Option<&Value>) -> bool {
    match value {
        Some(Value::Bool(flag)) => *flag,
        Some(Value::String(text)) => text.eq_ignore_ascii_case("true"),
        _ => false,
    }
}

/// The jump servers of the `ssh_tunnel` handler: `jumpServer.count` of them in the handler's
/// properties, `jumpServer<i>.enabled` / `.host` / `.port` / `.name` (the login) / `.authType` /
/// `.keyPath`, and `jumpServer<i>.password` as a secure property (a key's passphrase). DBeaver
/// stops at the first one that is not enabled and keeps at most five.
fn jumps_of(properties: &Value, secure: &Value) -> (Vec<ImportedJump>, Option<&'static str>) {
    const MAX: usize = 5;
    let count = text(properties.get("jumpServer.count")).parse::<usize>().unwrap_or(MAX);
    let mut note = (count > MAX).then_some("more jump servers than DBeaver allows");
    let mut jumps = Vec::new();
    for index in 0..count.min(MAX) {
        let prefix = format!("jumpServer{index}.");
        if !flag(properties.get(format!("{prefix}enabled"))) {
            break;
        }
        let host = text(properties.get(format!("{prefix}host")));
        if host.is_empty() {
            // Enabled without a host: DBeaver refuses to connect; the hop is not guessed at.
            note = Some("a jump server has no host");
            break;
        }
        let auth = authentication(properties, &prefix);
        let key = format!("{prefix}password");
        // Secure properties sit in the credentials file next to the login, directly or under
        // `properties`.
        let password = first_text(&[
            secure.get(&key),
            secure.get("properties").and_then(|nested| nested.get(&key)),
            properties.get(&key),
        ]);
        jumps.push(ImportedJump {
            host,
            port: parse_port(&text(properties.get(format!("{prefix}port")))).unwrap_or(22).to_string(),
            user: text(properties.get(format!("{prefix}name"))),
            auth,
            key_path: if auth == SshAuthMethod::KeyFile {
                text(properties.get(format!("{prefix}keyPath")))
            } else {
                String::new()
            },
            password: Secret::new(password.clone()),
            passphrase: Secret::new(password),
        });
    }
    (jumps, note)
}

/// The `socks_proxy` network handler: `socks-host` / `socks-port` in its properties, the login in
/// the handler or the credentials file under `network/socks_proxy`. DBeaver's SOCKS proxy is a
/// plain `java.net.Proxy`; it is imported as SOCKS5.
fn proxy_of(config: &Value, id: &str, credentials: &Value) -> Option<ImportedProxy> {
    let handler = config.get("handlers")?.get("socks_proxy")?;
    if !flag(handler.get("enabled")) {
        return None;
    }
    let properties = handler.get("properties").unwrap_or(&Value::Null);
    let host = text(properties.get("socks-host"));
    if host.is_empty() {
        return None;
    }
    let secure = credentials
        .get(id)
        .and_then(|entry| entry.get("network/socks_proxy"))
        .unwrap_or(&Value::Null);
    Some(ImportedProxy {
        kind: ProxyType::Socks5,
        host,
        port: parse_port(&text(properties.get("socks-port"))).unwrap_or(1080).to_string(),
        user: first_text(&[secure.get("user"), handler.get("user")]),
        password: Secret::new(first_text(&[secure.get("password"), handler.get("password")])),
    })
}

/// The `ssh_tunnel` network handler of a data source with its jump servers. Its login
/// name and password sit in `credentials-config.json` under `network/ssh_tunnel`; for a key the
/// "password" is the key's passphrase.
fn ssh_of(config: &Value, id: &str, credentials: &Value) -> Option<(ImportedSsh, Option<&'static str>)> {
    let handler = config.get("handlers")?.get("ssh_tunnel")?;
    if handler.get("enabled").and_then(Value::as_bool) != Some(true) {
        return None;
    }
    let properties = handler.get("properties").unwrap_or(&Value::Null);
    let host = text(properties.get("host"));
    if host.is_empty() {
        return None;
    }
    let secure = credentials
        .get(id)
        .and_then(|entry| entry.get("network/ssh_tunnel"))
        .unwrap_or(&Value::Null);
    let user = first_text(&[secure.get("user"), handler.get("user")]);
    let password = first_text(&[secure.get("password"), handler.get("password")]);
    let auth = authentication(properties, "");
    let (jumps, note) = jumps_of(properties, secure);
    Some((ImportedSsh {
        host,
        port: parse_port(&text(properties.get("port"))).unwrap_or(22).to_string(),
        user,
        auth,
        key_path: if auth == SshAuthMethod::KeyFile {
            text(properties.get("keyPath"))
        } else {
            String::new()
        },
        password: Secret::new(password.clone()),
        passphrase: Secret::new(password),
        jumps,
    }, note))
}

fn connection_of(id: &str, entry: &Value, credentials: &Value) -> Option<ImportedConnection> {
    let config = entry.get("configuration").unwrap_or(&Value::Null);
    let url = text(config.get("url"));

    let provider_and_driver = format!(
        "{} {}",
        normalize(&text(entry.get("provider"))),
        normalize(&text(entry.get("driver")))
    );
    let product = find_product(&normalize(&text(entry.get("driver"))))
        .or_else(|| find_product(&provider_and_driver))
        .or_else(|| find_product(&normalize(&url)));
    let kind = match product {
        Some(product) => ImportKind::of(product),
        None => ImportKind::Unsupported(
            Some(text(entry.get("driver")))
                .filter(|driver| !driver.is_empty())
                .unwrap_or_else(|| "JDBC".into()),
        ),
    };
    let sqlite = kind.supported() == Some(ConnectionKind::Sqlite);

    let target = jdbc::parse(&url);
    let secure = credentials
        .get(id)
        .and_then(|entry| entry.get("#connection"))
        .unwrap_or(&Value::Null);
    let inline = config.get("credentials").unwrap_or(&Value::Null);
    let username = first_text(&[
        secure.get("user"),
        secure.get("username"),
        inline.get("user"),
        inline.get("username"),
        config.get("user"),
    ]);
    let password = first_text(&[secure.get("password"), inline.get("password"), config.get("password")]);

    let port = parse_port(&first_text(&[config.get("port"), config.get("host-port")])).or(target.port);
    let database = first_text(&[
        config.get("database"),
        config.get("database-name"),
        config.get("schema"),
    ]);
    let database = Some(database)
        .filter(|database| !database.is_empty())
        .unwrap_or_else(|| target.database.clone());
    let mut host = first_text(&[config.get("host"), config.get("host-name")]);
    if host.is_empty() {
        host = target.host.clone();
    }
    if host.is_empty() {
        host = if sqlite { database.clone() } else { "127.0.0.1".into() };
    }
    let name = Some(text(entry.get("name")))
        .filter(|name| !name.is_empty())
        .or(Some(database.clone()).filter(|database| !database.is_empty() && !sqlite))
        .or(Some(host.clone()).filter(|host| !host.is_empty()))
        .unwrap_or_else(|| kind.label());

    let mut connection = ImportedConnection::new(name, kind);
    connection.host = host;
    connection.port = port;
    connection.database = if sqlite { String::new() } else { database };
    connection.username = Some(username)
        .filter(|user| !user.is_empty())
        .unwrap_or(target.user.clone());
    connection.password = Secret::new(password).or(target.password);
    if let Some((ssh, note)) = ssh_of(config, id, credentials) {
        connection.ssh = Some(ssh);
        connection.route_note = note;
    }
    connection.proxy = proxy_of(config, id, credentials);
    let socks_enabled = flag(config.get("handlers").and_then(|h| h.get("socks_proxy")).and_then(|h| h.get("enabled")));
    if socks_enabled && connection.proxy.is_none() {
        connection.route_note = Some("the SOCKS proxy has no host");
    }
    if connection.kind.supported() == Some(ConnectionKind::Mongodb) {
        connection.uri = jdbc::mongo_multi_host_uri(&url);
    }
    Some(connection)
}

pub fn parse(
    data_sources: &[u8],
    credentials: Option<&[u8]>,
) -> Result<Vec<ImportedConnection>, ImportError> {
    let parsed: Value =
        serde_json::from_slice(data_sources).map_err(|_| ImportError::InvalidJson)?;
    if !parsed.is_object() && !parsed.is_array() {
        return Err(ImportError::NotThisFormat("a DBeaver data-sources.json"));
    }
    let credentials = decrypt_credentials(credentials);
    let mut seen = HashSet::new();
    Ok(entries(&parsed)
        .into_iter()
        .filter(|(id, _)| !id.is_empty())
        .filter_map(|(id, entry)| connection_of(&id, entry, &credentials))
        .filter(|c| {
            seen.insert((c.name.clone(), c.kind.label(), c.host.clone(), c.port, c.database.clone()))
        })
        .collect())
}

#[cfg(test)]
mod tests {
    use super::*;

    const DATA_SOURCES: &[u8] = include_bytes!("fixtures/dbeaver/data-sources.json");
    /// Encrypted with `openssl enc -aes-128-cbc` under DBeaver's default key.
    const CREDENTIALS: &[u8] = include_bytes!("fixtures/dbeaver/credentials-config.json");

    fn by_name<'a>(all: &'a [ImportedConnection], name: &str) -> &'a ImportedConnection {
        all.iter().find(|c| c.name == name).unwrap_or_else(|| panic!("{name} imported"))
    }

    #[test]
    fn imports_fields_and_decrypts_credentials() {
        let all = parse(DATA_SOURCES, Some(CREDENTIALS)).unwrap();
        let pg = by_name(&all, "Local Postgres");
        assert_eq!(pg.kind, ImportKind::Supported(ConnectionKind::Postgres));
        assert_eq!(
            (pg.host.as_str(), pg.port, pg.database.as_str(), pg.username.as_str()),
            ("localhost", Some(5432), "appdb", "postgres")
        );
        assert_eq!(pg.password.as_ref().unwrap().expose(), "pg-secret");
        assert_eq!(by_name(&all, "Reporting MySQL").password.as_ref().unwrap().expose(), "mysql-secret");

        let mysql = by_name(&all, "Reporting MySQL");
        assert_eq!(mysql.kind, ImportKind::Supported(ConnectionKind::Mysql));
        assert_eq!((mysql.host.as_str(), mysql.port), ("mysql.example.com", Some(3306)));
        assert_eq!(mysql.username, "report");

        // Host, port and database come from the JDBC URL when the fields are absent.
        let maria = by_name(&all, "Maria from URL");
        assert_eq!(maria.kind, ImportKind::Supported(ConnectionKind::Mariadb));
        assert_eq!(
            (maria.host.as_str(), maria.port, maria.database.as_str()),
            ("maria.example.com", Some(3307), "inventory")
        );

        let sqlite = by_name(&all, "Local file");
        assert_eq!(sqlite.kind, ImportKind::Supported(ConnectionKind::Sqlite));
        assert_eq!(sqlite.host, "/Users/me/data/app.db");
        assert_eq!(sqlite.database, "");

        let mongo = by_name(&all, "Mongo cluster");
        assert_eq!(mongo.kind, ImportKind::Supported(ConnectionKind::Mongodb));
        assert_eq!(
            mongo.uri.as_deref(),
            Some("mongodb://a.example.com:27017,b.example.com:27017/app?replicaSet=rs0")
        );
    }

    #[test]
    fn without_credentials_there_are_no_passwords_and_bad_files_are_ignored() {
        let none = parse(DATA_SOURCES, None).unwrap();
        assert!(by_name(&none, "Local Postgres").password.is_none());
        assert!(by_name(&none, "Reporting MySQL").password.is_none());
        assert!(by_name(&none, "Local Postgres").username.is_empty());
        // Inline user / password of an older workspace still come through.
        let inline = by_name(&none, "Legacy inline");
        assert_eq!(inline.username, "legacy");
        assert_eq!(inline.password.as_ref().unwrap().expose(), "inline-secret");
        let garbage = parse(DATA_SOURCES, Some(b"short")).unwrap();
        assert_eq!(garbage.len(), none.len());
    }

    #[test]
    fn unsupported_drivers_are_listed_and_duplicates_collapse() {
        let all = parse(DATA_SOURCES, Some(CREDENTIALS)).unwrap();
        assert_eq!(by_name(&all, "Finance Oracle").kind, ImportKind::Unsupported("Oracle".into()));
        assert_eq!(by_name(&all, "Unknown thing").kind, ImportKind::Unsupported("acme-custom".into()));
        assert_eq!(all.iter().filter(|c| c.name == "Local Postgres").count(), 1);
        assert_eq!(all.len(), 9);
    }

    #[test]
    fn the_ssh_tunnel_handler_maps_onto_the_tunnel_fields() {
        let data_sources = br#"{"connections":{
          "pg-1":{"provider":"postgresql","driver":"postgres-jdbc","name":"Tunnelled key","configuration":{"host":"db.internal","port":"5432","database":"d","handlers":{"ssh_tunnel":{"type":"tunnel","enabled":true,"save-password":true,"user":"deploy","password":"phrase-secret","properties":{"host":"bastion.example.com","port":2222,"authType":"PUBLIC_KEY","keyPath":"/keys/id_ed25519"}}}}},
          "pg-2":{"provider":"postgresql","driver":"postgres-jdbc","name":"Tunnelled password","configuration":{"host":"db2.internal","port":"5432","database":"d","handlers":{"ssh_tunnel":{"type":"tunnel","enabled":true,"properties":{"host":"bastion.example.com","authType":"PASSWORD"}}}}},
          "pg-3":{"provider":"postgresql","driver":"postgres-jdbc","name":"Off","configuration":{"host":"db3.internal","port":"5432","handlers":{"ssh_tunnel":{"type":"tunnel","enabled":false,"properties":{"host":"bastion.example.com"}}}}}
        }}"#;
        let all = parse(data_sources, None).unwrap();
        let key = by_name(&all, "Tunnelled key").ssh.clone().unwrap();
        assert_eq!(
            (key.host.as_str(), key.port.as_str(), key.user.as_str(), key.auth, key.key_path.as_str()),
            ("bastion.example.com", "2222", "deploy", SshAuthMethod::KeyFile, "/keys/id_ed25519")
        );
        assert!(key.usable());
        assert_eq!(key.secrets()[0].1.expose(), "phrase-secret", "a key's password is its passphrase");
        let password = by_name(&all, "Tunnelled password").ssh.clone().unwrap();
        assert_eq!((password.auth, password.port.as_str()), (SshAuthMethod::Password, "22"));
        assert!(!password.usable(), "DBeaver did not save the tunnel password: imported off");
        assert!(by_name(&all, "Off").ssh.is_none(), "a disabled tunnel is not imported");
    }

    #[test]
    fn rejects_non_json() {
        assert_eq!(parse(b"<xml/>", None), Err(ImportError::InvalidJson));
        assert_eq!(parse(b"\"text\"", None), Err(ImportError::NotThisFormat("a DBeaver data-sources.json")));
        assert_eq!(parse(b"{}", None), Ok(Vec::new()));
    }

    #[test]
    fn locate_accepts_the_dot_dbeaver_folder_a_parent_or_files() {
        let root = std::env::temp_dir().join(format!("tablex-dbeaver-locate-{}", uuid::Uuid::new_v4()));
        let dot = root.join("workspace6/General/.dbeaver");
        std::fs::create_dir_all(&dot).unwrap();
        std::fs::write(dot.join("data-sources.json"), DATA_SOURCES).unwrap();
        std::fs::write(dot.join("credentials-config.json"), CREDENTIALS).unwrap();

        for pick in [dot.clone(), root.clone(), dot.join("data-sources.json")] {
            let files = locate(&[pick]).unwrap();
            assert_eq!(files.data_sources, dot.join("data-sources.json"));
            assert_eq!(files.credentials, Some(dot.join("credentials-config.json")));
        }
        assert_eq!(
            locate(&[dot.join("credentials-config.json"), dot.join("data-sources.json")])
                .unwrap()
                .credentials,
            Some(dot.join("credentials-config.json"))
        );
        assert_eq!(
            locate(&[std::env::temp_dir().join("tablex-no-such-dir-xyz")]),
            Err(ImportError::MissingFile("data-sources.json"))
        );
        assert_eq!(
            sibling_credentials(Path::new("/x/data-sources-extra.json")),
            Path::new("/x/credentials-config-extra.json")
        );
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn jump_servers_and_the_socks_proxy_of_the_handlers_are_imported() {
        let json = br#"{"connections": {"conn-1": {"name": "Behind jumps", "driver": "postgres-jdbc", "provider": "postgresql",
          "configuration": {"host": "db.internal", "port": "5432", "database": "app", "url": "jdbc:postgresql://db.internal:5432/app",
            "handlers": {
              "ssh_tunnel": {"type": "tunnel", "enabled": true, "user": "deploy", "properties": {
                "host": "bastion.example.com", "port": 22, "authType": "AGENT",
                "jumpServer.count": 3,
                "jumpServer0.enabled": true, "jumpServer0.host": "jump1.example.com", "jumpServer0.port": 2201, "jumpServer0.name": "hop1", "jumpServer0.authType": "PASSWORD",
                "jumpServer1.enabled": true, "jumpServer1.host": "jump2.example.com", "jumpServer1.port": 22, "jumpServer1.name": "hop2", "jumpServer1.authType": "PUBLIC_KEY", "jumpServer1.keyPath": "/keys/j2",
                "jumpServer2.enabled": false, "jumpServer2.host": "ignored.example.com"}},
              "socks_proxy": {"type": "network", "enabled": true, "user": "px", "properties": {"socks-host": "proxy.example.com", "socks-port": 1081}}}}}}}"#;
        let all = parse(json, None).unwrap();
        let connection = by_name(&all, "Behind jumps");
        let ssh = connection.ssh.as_ref().unwrap();
        assert_eq!(ssh.jumps.len(), 2, "DBeaver stops at the first jump server that is not enabled");
        assert_eq!((ssh.jumps[0].host.as_str(), ssh.jumps[0].port.as_str(), ssh.jumps[0].user.as_str(), ssh.jumps[0].auth), ("jump1.example.com", "2201", "hop1", SshAuthMethod::Password));
        assert_eq!((ssh.jumps[1].auth, ssh.jumps[1].key_path.as_str()), (SshAuthMethod::KeyFile, "/keys/j2"));
        let proxy = connection.proxy.as_ref().unwrap();
        assert_eq!((proxy.kind, proxy.host.as_str(), proxy.port.as_str(), proxy.user.as_str()), (ProxyType::Socks5, "proxy.example.com", "1081", "px"));
        // No jump password and no proxy password in the file: the whole route stays off.
        assert!(!connection.route_usable());
        let record = connection.to_record(1.0).unwrap();
        let route = crate::ssh_tunnel::SshSettings::parse(&record.network_settings_json).unwrap();
        assert!(!route.enabled && !route.proxy.enabled);
        assert_eq!(route.jumps.len(), 2, "the hops are kept for the user to complete");
        assert_eq!(route.proxy.host, "proxy.example.com");
    }

    #[test]
    fn a_hop_that_cannot_be_imported_turns_the_route_off_and_the_selection_says_so() {
        let json = br#"{"connections": {
          "a": {"name": "Too many", "driver": "postgres-jdbc", "provider": "postgresql",
            "configuration": {"host": "db", "port": "5432", "database": "app", "url": "jdbc:postgresql://db:5432/app",
              "handlers": {"ssh_tunnel": {"type": "tunnel", "enabled": true, "properties": {"host": "b.example.com", "port": 22, "authType": "AGENT", "jumpServer.count": 7}}}}},
          "b": {"name": "Hostless jump", "driver": "postgres-jdbc", "provider": "postgresql",
            "configuration": {"host": "db", "port": "5432", "database": "app", "url": "jdbc:postgresql://db:5432/app",
              "handlers": {"ssh_tunnel": {"type": "tunnel", "enabled": true, "properties": {"host": "b.example.com", "port": 22, "authType": "AGENT", "jumpServer.count": 1, "jumpServer0.enabled": true, "jumpServer0.authType": "AGENT"}}}}},
          "c": {"name": "Hostless socks", "driver": "postgres-jdbc", "provider": "postgresql",
            "configuration": {"host": "db", "port": "5432", "database": "app", "url": "jdbc:postgresql://db:5432/app",
              "handlers": {"socks_proxy": {"type": "network", "enabled": true, "properties": {}}}}}}}"#;
        let all = parse(json, None).unwrap();
        for (name, note) in [("Too many", "more jump servers"), ("Hostless jump", "no host"), ("Hostless socks", "no host")] {
            let connection = by_name(&all, name);
            assert!(connection.route_note.is_some_and(|text| text.contains(note)), "{name}: {:?}", connection.route_note);
            assert!(!connection.route_usable(), "{name}");
            assert!(connection.tunnel_label().unwrap().contains("(off: "), "{name}: {:?}", connection.tunnel_label());
            assert!(connection.has_route(), "{name}: counted in the selection");
            let route = crate::ssh_tunnel::SshSettings::parse(&connection.to_record(1.0).unwrap().network_settings_json).unwrap();
            assert!(!route.routed(), "{name}");
        }
    }
}
