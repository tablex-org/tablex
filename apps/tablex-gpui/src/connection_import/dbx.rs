//! A DBX connections export (`dbx-connections.json`), as written by DBX's
//! `exportConnectionsToFile`:
//!
//! * plain: `{ "connections": [ConnectionConfig…], "layout"?, "tunnelProfiles"? }`
//!   (also `{ "format": "dbx-config", … }` or a bare array);
//! * protected by a passphrase: `{ "format": "dbx-encrypted", "version": 1,
//!   "salt", "iv", "data" }` (base64), where `data` is the AES-256-GCM
//!   encryption (tag appended) of the plain JSON under a PBKDF2-HMAC-SHA256
//!   key, 100 000 rounds.
//!
//! Groups (`layout`) and shared tunnel profiles are not imported.

use super::{
    crypto, find_product, jdbc, normalize, parse_port, ImportError, ImportKind,
    ImportedConnection, ImportedJump, ImportedProxy, ImportedSsh, Secret,
};
use crate::connections::ConnectionKind;
use crate::ssh_tunnel::{ProxyType, SshAuthMethod};
use base64::Engine as _;
use serde_json::Value;

fn text(value: &Value, key: &str) -> String {
    match value.get(key) {
        Some(Value::String(text)) => text.trim().to_string(),
        Some(Value::Number(number)) => number.to_string(),
        _ => String::new(),
    }
}

/// Whether `bytes` is a passphrase-protected export.
pub fn is_encrypted(bytes: &[u8]) -> bool {
    serde_json::from_slice::<Value>(bytes)
        .ok()
        .is_some_and(|value| {
            text(&value, "format") == "dbx-encrypted"
                && ["salt", "iv", "data"]
                    .iter()
                    .all(|key| value.get(key).is_some_and(Value::is_string))
        })
}

fn decrypt(envelope: &Value, passphrase: &str) -> Result<Vec<u8>, ImportError> {
    let field = |key: &str| {
        base64::engine::general_purpose::STANDARD
            .decode(text(envelope, key))
            .map_err(|_| ImportError::InvalidJson)
    };
    let salt = field("salt")?;
    let iv: [u8; 12] = field("iv")?
        .try_into()
        .map_err(|_| ImportError::InvalidJson)?;
    let data = field("data")?;
    crypto::aes256_gcm_decrypt(&crypto::dbx_key(passphrase, &salt), &iv, &data)
        .ok_or(ImportError::WrongPassphrase)
}

fn product_of(entry: &Value) -> ImportKind {
    // The profile names forks and vendors (`mariadb`, `cockroachdb`,
    // `oceanbase`) that share a `db_type` with another product.
    let product = ["driver_profile", "db_type", "driver_label"]
        .iter()
        .find_map(|key| find_product(&normalize(&text(entry, key))));
    match product {
        Some(product) => ImportKind::of(product),
        None => ImportKind::Unsupported(
            Some(text(entry, "driver_label"))
                .filter(|label| !label.is_empty())
                .unwrap_or_else(|| text(entry, "db_type")),
        ),
    }
}

fn connection_of(entry: &Value) -> ImportedConnection {
    let kind = product_of(entry);
    let sqlite = kind.supported() == Some(ConnectionKind::Sqlite);
    let mut connection = ImportedConnection::new(text(entry, "name"), kind);
    if connection.name.is_empty() {
        connection.name = Some(text(entry, "host"))
            .filter(|host| !host.is_empty())
            .unwrap_or_else(|| connection.kind.label());
    }
    connection.host = text(entry, "host");
    connection.port = parse_port(&text(entry, "port"));
    connection.database = if sqlite { String::new() } else { text(entry, "database") };
    connection.username = text(entry, "username");
    connection.password = Secret::new(text(entry, "password"));
    connection.tls = entry.get("ssl").and_then(Value::as_bool).unwrap_or(false);

    let url = text(entry, "connection_string");
    if connection.kind.supported() == Some(ConnectionKind::Mongodb) && !url.is_empty() {
        let (user, password) = jdbc::uri_credentials(&url);
        connection.uri = crate::connections::sanitized_uri(&url).ok();
        if connection.username.is_empty() {
            connection.username = user;
        }
        connection.password = connection.password.take().or(password);
    }

    let layers: Vec<&Value> = entry
        .get("transport_layers")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter(|layer| layer.get("enabled").and_then(Value::as_bool) != Some(false))
        .collect();
    (connection.ssh, connection.proxy, connection.route_note) = route_of(&layers);
    connection
}

/// DBX chains its transport layers in order, each reached through the one before. TableX's route
/// is: an optional proxy, then SSH servers (all but the last are jump hosts, the last is the
/// bastion, at most `MAX_JUMPS + 1`). A chain of exactly that shape is imported; any other (a
/// proxy after an SSH server, two proxies, an HTTP script tunnel, too many servers, a layer
/// without a host) is not translated at all, and the connection says why: half of a route would
/// connect some other way than DBX does.
fn route_of(layers: &[&Value]) -> (Option<ImportedSsh>, Option<ImportedProxy>, Option<&'static str>) {
    const UNSUPPORTED: &str = "its transport layers are not a proxy followed by SSH servers";
    let kind = |layer: &Value| text(layer, "type");
    let (proxy, ssh_layers) = match layers.split_first() {
        Some((first, rest)) if kind(first) == "proxy" => (Some(*first), rest),
        _ => (None, layers),
    };
    if ssh_layers.iter().any(|layer| kind(layer) != "ssh") {
        return (None, None, Some(UNSUPPORTED));
    }
    if ssh_layers.len() > engine_net::MAX_JUMPS + 1 {
        return (None, None, Some("too many SSH servers in its transport layers"));
    }
    let proxy = proxy.map(proxy_of);
    let hops: Vec<ImportedSsh> = ssh_layers.iter().map(|layer| ssh_of(layer)).collect();
    if proxy.as_ref().is_some_and(|proxy| proxy.host.is_empty()) || hops.iter().any(|hop| hop.host.is_empty()) {
        return (None, None, Some("a transport layer has no host"));
    }
    let Some((bastion, jumps)) = hops.split_last() else {
        return (None, proxy, None);
    };
    let mut ssh = bastion.clone();
    ssh.jumps = jumps
        .iter()
        .map(|hop| ImportedJump {
            host: hop.host.clone(),
            port: hop.port.clone(),
            user: hop.user.clone(),
            auth: hop.auth,
            key_path: hop.key_path.clone(),
            password: hop.password.clone(),
            passphrase: hop.passphrase.clone(),
        })
        .collect();
    (Some(ssh), proxy, None)
}

fn proxy_of(layer: &Value) -> ImportedProxy {
    let kind = match text(layer, "proxy_type").to_ascii_lowercase().as_str() {
        "http" | "https" => ProxyType::Http,
        _ => ProxyType::Socks5,
    };
    ImportedProxy {
        kind,
        host: text(layer, "host"),
        port: parse_port(&text(layer, "port"))
            .map_or_else(|| kind.default_port().to_owned(), |port| port.to_string()),
        user: text(layer, "username"),
        password: Secret::new(text(layer, "password")),
    }
}

/// DBX's login method: `password`, `key`, `key+password`, `agent` or `none`; an older export has
/// none and DBX probes key > password > agent by which fields are filled.
fn ssh_of(layer: &Value) -> ImportedSsh {
    let key_path = text(layer, "key_path");
    let password = text(layer, "password");
    let uses_agent = layer.get("use_ssh_agent").and_then(Value::as_bool) == Some(true);
    let auth = match text(layer, "auth_method").as_str() {
        "key" | "key+password" => SshAuthMethod::KeyFile,
        "agent" => SshAuthMethod::Agent,
        "password" | "none" => SshAuthMethod::Password,
        _ if !key_path.is_empty() => SshAuthMethod::KeyFile,
        _ if uses_agent => SshAuthMethod::Agent,
        _ => SshAuthMethod::Password,
    };
    ImportedSsh {
        host: text(layer, "host"),
        port: parse_port(&text(layer, "port")).unwrap_or(22).to_string(),
        user: text(layer, "user"),
        auth,
        key_path: if auth == SshAuthMethod::KeyFile { key_path } else { String::new() },
        password: Secret::new(password),
        passphrase: Secret::new(text(layer, "key_passphrase")),
        jumps: Vec::new(),
    }
}

/// `passphrase` is required for (and only used by) an encrypted export.
pub fn parse(bytes: &[u8], passphrase: Option<&str>) -> Result<Vec<ImportedConnection>, ImportError> {
    let mut value: Value = serde_json::from_slice(bytes).map_err(|_| ImportError::InvalidJson)?;
    if is_encrypted(bytes) {
        let passphrase = passphrase.ok_or(ImportError::Encrypted)?;
        value = serde_json::from_slice(&decrypt(&value, passphrase)?)
            .map_err(|_| ImportError::WrongPassphrase)?;
    }
    let list = match &value {
        Value::Array(list) => list,
        other => other
            .get("connections")
            .and_then(Value::as_array)
            .ok_or(ImportError::NotThisFormat("a DBX connections export"))?,
    };
    Ok(list
        .iter()
        .filter(|entry| entry.is_object())
        .map(connection_of)
        .collect())
}

#[cfg(test)]
mod tests {
    use super::*;

    const PLAIN: &[u8] = include_bytes!("fixtures/dbx/plain.json");
    /// The same content, exported by DBX's `encryptConfig` algorithm (Node
    /// WebCrypto) with the passphrase below.
    const ENCRYPTED: &[u8] = include_bytes!("fixtures/dbx/encrypted.json");
    const PASSPHRASE: &str = "correct horse battery";

    fn by_name<'a>(all: &'a [ImportedConnection], name: &str) -> &'a ImportedConnection {
        all.iter().find(|c| c.name == name).unwrap_or_else(|| panic!("{name} imported"))
    }

    #[test]
    fn imports_fields_passwords_ssh_and_tls() {
        let all = parse(PLAIN, None).unwrap();
        assert_eq!(all.len(), 9);
        let pg = by_name(&all, "Prod PG");
        assert_eq!(pg.kind, ImportKind::Supported(ConnectionKind::Postgres));
        assert_eq!(
            (pg.host.as_str(), pg.port, pg.database.as_str(), pg.username.as_str()),
            ("pg.example.com", Some(5432), "orders", "app")
        );
        assert_eq!(pg.password.as_ref().unwrap().expose(), "pg-pass");
        assert!(pg.tls);
        let ssh = pg.ssh.as_ref().expect("the ssh layer");
        assert_eq!(
            (ssh.host.as_str(), ssh.port.as_str(), ssh.user.as_str(), ssh.key_path.as_str(), ssh.auth),
            ("bastion.example.com", "2222", "deploy", "/keys/id_ed25519", SshAuthMethod::KeyFile)
        );
        // The key method does not use the login password; the tunnel is usable with the key file.
        assert!(ssh.usable());
        assert!(ssh.secrets().is_empty(), "no passphrase was exported");
        assert!(!format!("{pg:?}").contains("ssh-pass"));

        assert_eq!(by_name(&all, "Reporting").kind, ImportKind::Supported(ConnectionKind::Mysql));
        assert_eq!(by_name(&all, "Maria").kind, ImportKind::Supported(ConnectionKind::Mariadb));
        let sqlite = by_name(&all, "Notes");
        assert_eq!(sqlite.kind, ImportKind::Supported(ConnectionKind::Sqlite));
        assert_eq!(sqlite.host, "/Users/me/app.db");
        assert_eq!(sqlite.port, None);
        assert_eq!(by_name(&all, "Events").kind, ImportKind::Supported(ConnectionKind::Clickhouse));
    }

    #[test]
    fn mongodb_connection_string_loses_its_password_to_the_secret() {
        let all = parse(PLAIN, None).unwrap();
        let mongo = by_name(&all, "Replica set");
        assert_eq!(mongo.uri.as_deref(), Some("mongodb://root@a.example.com:27017,b.example.com:27017/app?replicaSet=rs0"));
        assert_eq!(mongo.username, "root");
        assert_eq!(mongo.password.as_ref().unwrap().expose(), "s3cr:t");
    }

    #[test]
    fn unsupported_products_are_listed() {
        let all = parse(PLAIN, None).unwrap();
        assert_eq!(by_name(&all, "Ledger").kind, ImportKind::Unsupported("Oracle".into()));
        assert_eq!(by_name(&all, "Cache").kind, ImportKind::Supported(ConnectionKind::Redis));
        assert_eq!(by_name(&all, "Warehouse").kind, ImportKind::Unsupported("SQL Server".into()));
    }

    #[test]
    fn encrypted_export_needs_the_passphrase_and_matches_the_plain_one() {
        assert!(is_encrypted(ENCRYPTED));
        assert!(!is_encrypted(PLAIN));
        assert_eq!(parse(ENCRYPTED, None), Err(ImportError::Encrypted));
        assert_eq!(parse(ENCRYPTED, Some("nope")), Err(ImportError::WrongPassphrase));
        assert_eq!(parse(ENCRYPTED, Some(PASSPHRASE)), parse(PLAIN, None));
    }

    #[test]
    fn every_dbx_login_method_maps_onto_the_tunnel_fields() {
        let json = br#"[
          {"name":"pw","db_type":"postgres","host":"h1.example.com","port":5432,"transport_layers":[{"type":"ssh","host":"b.example.com","port":22,"user":"u","password":"login-secret","auth_method":"password"}]},
          {"name":"nopw","db_type":"postgres","host":"h2.example.com","port":5432,"transport_layers":[{"type":"ssh","host":"b.example.com","user":"u","auth_method":"password"}]},
          {"name":"agent","db_type":"postgres","host":"h3.example.com","port":5432,"transport_layers":[{"type":"ssh","host":"b.example.com","user":"u","auth_method":"agent"}]},
          {"name":"key","db_type":"postgres","host":"h4.example.com","port":5432,"transport_layers":[{"type":"ssh","host":"b.example.com","user":"u","auth_method":"key+password","key_path":"/k","key_passphrase":"phrase-secret","password":"login-secret"}]},
          {"name":"legacy","db_type":"postgres","host":"h5.example.com","port":5432,"transport_layers":[{"type":"ssh","host":"b.example.com","user":"u","key_path":"/k"}]}
        ]"#;
        let all = parse(json, None).unwrap();
        let ssh = |name: &str| by_name(&all, name).ssh.clone().expect(name);
        assert_eq!(ssh("pw").auth, SshAuthMethod::Password);
        assert!(ssh("pw").usable());
        assert_eq!(ssh("pw").secrets().len(), 1);
        assert!(!ssh("nopw").usable(), "a password tunnel without the password stays off");
        assert_eq!(ssh("agent").auth, SshAuthMethod::Agent);
        assert!(ssh("agent").usable() && ssh("agent").secrets().is_empty());
        let key = ssh("key");
        assert_eq!((key.auth, key.key_path.as_str()), (SshAuthMethod::KeyFile, "/k"));
        let secrets = key.secrets();
        assert_eq!(secrets.len(), 1, "only the passphrase belongs to a key tunnel");
        assert_eq!(secrets[0].1.expose(), "phrase-secret");
        assert_eq!(ssh("legacy").auth, SshAuthMethod::KeyFile, "an older export is probed by its fields");
        for connection in &all {
            let record = connection.to_record(1.0).unwrap();
            assert!(!record.network_settings_json.contains("secret"), "{}", record.network_settings_json);
        }
    }

    #[test]
    fn accepts_the_older_shapes_and_rejects_other_json() {
        let bare = br#"[{"name":"x","db_type":"postgres","host":"h","port":5432}]"#;
        assert_eq!(parse(bare, None).unwrap().len(), 1);
        let tagged = br#"{"format":"dbx-config","version":1,"connections":[]}"#;
        assert_eq!(parse(tagged, None), Ok(Vec::new()));
        assert_eq!(parse(b"{}", None), Err(ImportError::NotThisFormat("a DBX connections export")));
        assert_eq!(parse(b"nope", None), Err(ImportError::InvalidJson));
    }

    fn layer(kind: &str, host: &str) -> Value {
        match kind {
            "proxy" => serde_json::json!({"type": "proxy", "enabled": true, "proxy_type": "http", "host": host, "port": 3128, "username": "px", "password": "px-pass"}),
            _ => serde_json::json!({"type": "ssh", "enabled": true, "host": host, "port": 22, "user": "ops", "auth_method": "password", "password": format!("{host}-pass")}),
        }
    }

    fn route(kinds: &[(&str, &str)]) -> (Option<ImportedSsh>, Option<ImportedProxy>, Option<&'static str>) {
        let layers: Vec<Value> = kinds.iter().map(|(kind, host)| layer(kind, host)).collect();
        route_of(&layers.iter().collect::<Vec<_>>())
    }

    #[test]
    fn transport_layers_become_a_proxy_jump_hosts_and_a_bastion() {
        let (ssh, proxy, _) = route(&[("proxy", "proxy.example.com"), ("ssh", "jump1.example.com"), ("ssh", "jump2.example.com"), ("ssh", "bastion.example.com")]);
        let ssh = ssh.unwrap();
        assert_eq!(ssh.host, "bastion.example.com", "the last SSH layer is the bastion");
        assert_eq!(ssh.jumps.iter().map(|jump| jump.host.as_str()).collect::<Vec<_>>(), ["jump1.example.com", "jump2.example.com"]);
        assert_eq!(ssh.jumps[0].password.as_ref().unwrap().expose(), "jump1.example.com-pass");
        assert!(ssh.usable());
        let proxy = proxy.unwrap();
        assert_eq!((proxy.kind, proxy.host.as_str(), proxy.port.as_str(), proxy.user.as_str()), (ProxyType::Http, "proxy.example.com", "3128", "px"));
        assert!(proxy.usable());

        // A proxy alone is the whole route.
        let (ssh, proxy, _) = route(&[("proxy", "proxy.example.com")]);
        assert!(ssh.is_none() && proxy.is_some());
        // SSH servers without a proxy.
        let (ssh, proxy, _) = route(&[("ssh", "a.example.com"), ("ssh", "b.example.com")]);
        assert_eq!(ssh.unwrap().jumps.len(), 1);
        assert!(proxy.is_none());
        // A shape TableX cannot express is not translated at all.
        for kinds in [
            vec![("ssh", "a.example.com"), ("proxy", "proxy.example.com")],
            vec![("proxy", "p1.example.com"), ("proxy", "p2.example.com")],
        ] {
            let (ssh, proxy, note) = route(&kinds);
            assert!(ssh.is_none() && proxy.is_none() && note.is_some(), "{kinds:?}");
        }
        // Too many servers, and a layer without a host: no route, and the connection says why.
        let many: Vec<(&str, String)> = (0..10).map(|i| ("ssh", format!("h{i}.example.com"))).collect();
        let many: Vec<(&str, &str)> = many.iter().map(|(k, h)| (*k, h.as_str())).collect();
        let (ssh, proxy, note) = route(&many);
        assert!(ssh.is_none() && proxy.is_none());
        assert!(note.unwrap().contains("too many"));
        let (ssh, _, note) = route(&[("ssh", ""), ("ssh", "b.example.com")]);
        assert!(ssh.is_none() && note.is_some());
        // Exactly the most TableX can chain still imports.
        let max: Vec<(&str, String)> = (0..9).map(|i| ("ssh", format!("h{i}.example.com"))).collect();
        let max: Vec<(&str, &str)> = max.iter().map(|(k, h)| (*k, h.as_str())).collect();
        assert_eq!(route(&max).0.unwrap().jumps.len(), 8);
    }
}
