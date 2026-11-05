//! JetBrains DataGrip / IDE data sources: `dataSources.xml` (shared) plus
//! `dataSources.local.xml` (user name, database product). Mirrors DBX's
//! `datagripImport.ts`. DataGrip keeps passwords in the OS keychain or a
//! KeePass file under its own entry names, so no password is imported; groups
//! are not imported.

use super::{
    find_product, jdbc, normalize, ImportError, ImportKind, ImportedConnection, ImportedProxy, ImportedSsh,
};
use crate::connections::ConnectionKind;
use crate::ssh_tunnel::{ProxyType, SshAuthMethod};
use std::collections::HashSet;
use std::path::{Path, PathBuf};

#[derive(Debug, PartialEq, Eq)]
pub struct DataGripFiles {
    pub data_sources: PathBuf,
    pub local: Option<PathBuf>,
    /// `sshConfigs.xml` files: the project's (next to `dataSources.xml`) and the IDE's own in its
    /// `options` folder, where bastions shared by every project are kept.
    pub ssh_configs: Vec<PathBuf>,
}

fn file_name(path: &Path) -> String {
    path.file_name()
        .map(|name| name.to_string_lossy().to_ascii_lowercase())
        .unwrap_or_default()
}

/// Folders that can hold `dataSources.xml` below one the user picked: a
/// project (`.idea`), a config folder (`options`) or the `JetBrains` folder
/// with its `DataGrip<version>` children (newest first).
fn candidate_dirs(dir: &Path) -> Vec<PathBuf> {
    let mut dirs = vec![dir.to_path_buf(), dir.join("options"), dir.join(".idea")];
    let mut products: Vec<_> = std::fs::read_dir(dir)
        .into_iter()
        .flatten()
        .filter_map(Result::ok)
        .map(|entry| entry.path())
        .filter(|path| path.is_dir() && file_name(path).starts_with("datagrip"))
        .collect();
    products.sort();
    dirs.extend(products.into_iter().rev().map(|product| product.join("options")));
    dirs
}

/// Resolves what the picker returned (files and/or folders).
pub fn locate(paths: &[PathBuf]) -> Result<DataGripFiles, ImportError> {
    // Tests never look at the user's own IDE folders.
    let app_root = if cfg!(test) { None } else { crate::platform::datagrip_config_root() };
    locate_with(paths, app_root.as_deref())
}

/// [`locate`] with the IDE's per-OS config root (`JetBrains/`) given.
pub fn locate_with(paths: &[PathBuf], app_root: Option<&Path>) -> Result<DataGripFiles, ImportError> {
    let mut data_sources = None;
    let mut local = None;
    for path in paths {
        if path.is_dir() {
            let found = candidate_dirs(path)
                .into_iter()
                .map(|dir| dir.join("dataSources.xml"))
                .find(|file| file.is_file());
            data_sources = data_sources.or(found);
        } else {
            match file_name(path).as_str() {
                "datasources.xml" => data_sources = data_sources.or(Some(path.clone())),
                "datasources.local.xml" => local = Some(path.clone()),
                _ => {}
            }
        }
    }
    let data_sources = data_sources.ok_or(ImportError::MissingFile("dataSources.xml"))?;
    let local = local.or_else(|| {
        Some(data_sources.with_file_name("dataSources.local.xml")).filter(|file| file.is_file())
    });
    let project = data_sources.with_file_name("sshConfigs.xml");
    let application = app_root.and_then(|root| {
        candidate_dirs(root)
            .into_iter()
            .map(|dir| dir.join("sshConfigs.xml"))
            .find(|file| file.is_file())
    });
    let ssh_configs: Vec<PathBuf> = std::iter::once(project)
        .filter(|file| file.is_file())
        .chain(application)
        .fold(Vec::new(), |mut all, file| {
            if !all.contains(&file) {
                all.push(file);
            }
            all
        });
    Ok(DataGripFiles {
        data_sources,
        local,
        ssh_configs,
    })
}

#[derive(Default)]
struct Fragment {
    uuid: String,
    name: String,
    driver_ref: String,
    jdbc_url: String,
    driver_class: String,
    username: String,
    product: String,
    /// The id of the `sshConfig` this data source tunnels through, when its tunnel is on.
    ssh_config_id: String,
}

fn child_text(element: roxmltree::Node, tag: &str) -> String {
    element
        .descendants()
        .find(|node| node.has_tag_name(tag))
        .and_then(|node| node.text())
        .map(|text| text.trim().to_string())
        .unwrap_or_default()
}

fn parse_document(bytes: &[u8]) -> Result<String, ImportError> {
    let text = std::str::from_utf8(bytes).map_err(|_| ImportError::InvalidXml)?;
    Ok(text.trim_start_matches('\u{feff}').to_string())
}

/// Shared (`local == false`) or local fragments, in file order.
fn fragments(xml: &str, local: bool) -> Result<Vec<Fragment>, ImportError> {
    let document = roxmltree::Document::parse(xml).map_err(|_| ImportError::InvalidXml)?;
    Ok(document
        .descendants()
        .filter(|node| node.has_tag_name("data-source"))
        .filter_map(|element| {
            let uuid = element.attribute("uuid")?.to_string();
            let mut fragment = Fragment {
                uuid,
                name: element.attribute("name").unwrap_or_default().to_string(),
                username: child_text(element, "user-name"),
                ..Fragment::default()
            };
            // `<ssh-properties><enabled>true</enabled><ssh-config-id>…</ssh-config-id>`; older
            // files spell them as `<option name="…" value="…"/>`.
            if let Some(ssh) = element.descendants().find(|node| node.has_tag_name("ssh-properties")) {
                let option = |name: &str| {
                    ssh.descendants()
                        .find(|node| node.has_tag_name("option") && node.attribute("name") == Some(name))
                        .and_then(|node| node.attribute("value"))
                        .unwrap_or_default()
                        .to_string()
                };
                let enabled = Some(child_text(ssh, "enabled"))
                    .filter(|text| !text.is_empty())
                    .unwrap_or_else(|| option("enabled"));
                let config_id = Some(child_text(ssh, "ssh-config-id"))
                    .filter(|text| !text.is_empty())
                    .unwrap_or_else(|| option("sshConfigId"));
                if enabled.eq_ignore_ascii_case("true") {
                    fragment.ssh_config_id = config_id;
                }
            }
            if local {
                fragment.product = element
                    .descendants()
                    .find(|node| node.has_tag_name("database-info"))
                    .and_then(|info| info.attribute("product"))
                    .unwrap_or_default()
                    .to_string();
            } else {
                fragment.driver_ref = child_text(element, "driver-ref");
                fragment.jdbc_url = child_text(element, "jdbc-url");
                fragment.driver_class = child_text(element, "jdbc-driver");
            }
            Some(fragment)
        })
        .collect())
}

/// Shared first, then local-only sources; local fills user name and product.
fn merge(shared: Vec<Fragment>, local: Vec<Fragment>) -> Vec<Fragment> {
    let mut merged = shared;
    for extra in local {
        match merged.iter_mut().find(|f| f.uuid == extra.uuid) {
            Some(existing) => {
                if !extra.username.is_empty() {
                    existing.username = extra.username;
                }
                existing.product = extra.product;
                if !extra.ssh_config_id.is_empty() {
                    existing.ssh_config_id = extra.ssh_config_id;
                }
                if existing.name.is_empty() {
                    existing.name = extra.name;
                }
            }
            None => merged.push(extra),
        }
    }
    merged.retain(|fragment| !fragment.jdbc_url.is_empty());
    merged
}

fn subprotocol(jdbc_url: &str) -> String {
    jdbc_url
        .trim()
        .get(5..)
        .filter(|_| jdbc_url.trim()[..5].eq_ignore_ascii_case("jdbc:"))
        .map(|rest| rest.chars().take_while(|c| !matches!(c, ':' | '/')).collect())
        .unwrap_or_default()
}

/// Driver reference first, then the product the server reported, the JDBC
/// subprotocol and the driver class.
fn kind_of(fragment: &Fragment) -> Option<ImportKind> {
    let reference = fragment.driver_ref.split('.').next().unwrap_or_default();
    let class = normalize(&fragment.driver_class);
    let product = [
        normalize(reference),
        normalize(&fragment.product),
        normalize(&subprotocol(&fragment.jdbc_url)),
        class.clone(),
    ]
    .iter()
    .find_map(|text| find_product(text));
    match product {
        Some(product) => Some(ImportKind::of(product)),
        // A custom driver nobody recognises is no usable connection.
        None if fragment.driver_ref.is_empty() => None,
        None => Some(ImportKind::Unsupported(
            Some(fragment.driver_class.as_str())
                .filter(|class| !class.is_empty())
                .unwrap_or(fragment.driver_ref.as_str())
                .to_string(),
        )),
    }
}

/// The proxy an `sshConfig` names, when the file carries one: `proxyHost` / `proxyPort` /
/// `proxyType` (`HTTP`, otherwise SOCKS) / `proxyUsername`. DataGrip keeps proxy passwords in its
/// keychain, so a proxy with a login imports off. Written from DataGrip's file format as
/// documented, tested on hand-written fixtures only.
fn proxy_of(attribute: &dyn Fn(&str) -> String) -> Option<ImportedProxy> {
    let host = attribute("proxyHost");
    if host.is_empty() {
        return None;
    }
    let kind = if attribute("proxyType").to_ascii_uppercase().contains("HTTP") {
        ProxyType::Http
    } else {
        ProxyType::Socks5
    };
    Some(ImportedProxy {
        kind,
        host,
        port: super::parse_port(&attribute("proxyPort"))
            .map_or_else(|| kind.default_port().to_owned(), |port| port.to_string()),
        user: attribute("proxyUsername"),
        password: None,
    })
}

/// `sshConfigs.xml`: `<sshConfig id host port username authType keyPath …/>` by id. The login
/// password lives in DataGrip's keychain and is not read, so a password tunnel imports off.
fn ssh_configs(xml: &[&[u8]]) -> Vec<(String, ImportedSsh, Option<ImportedProxy>)> {
    xml.iter().flat_map(|bytes| ssh_configs_of(bytes)).collect()
}

fn ssh_configs_of(xml: &[u8]) -> Vec<(String, ImportedSsh, Option<ImportedProxy>)> {
    let Some(text) = parse_document(xml).ok() else {
        return Vec::new();
    };
    let Ok(document) = roxmltree::Document::parse(&text) else {
        return Vec::new();
    };
    document
        .descendants()
        .filter(|node| node.has_tag_name("sshConfig"))
        .filter_map(|node| {
            let attribute = |name: &str| node.attribute(name).unwrap_or_default().trim().to_string();
            let id = attribute("id");
            let auth = match attribute("authType").as_str() {
                "KEY_PAIR" => SshAuthMethod::KeyFile,
                "OPEN_SSH" => SshAuthMethod::Agent,
                _ => SshAuthMethod::Password,
            };
            (!id.is_empty() && !attribute("host").is_empty()).then(|| {
                (
                    id,
                    ImportedSsh {
                        host: attribute("host"),
                        port: super::parse_port(&attribute("port")).unwrap_or(22).to_string(),
                        user: attribute("username"),
                        auth,
                        key_path: if auth == SshAuthMethod::KeyFile {
                            attribute("keyPath")
                        } else {
                            String::new()
                        },
                        password: None,
                        passphrase: None,
                        jumps: Vec::new(),
                    },
                    proxy_of(&attribute),
                )
            })
        })
        .collect()
}

pub fn parse(
    data_sources: &[u8],
    local: Option<&[u8]>,
    ssh_configs_xml: &[&[u8]],
) -> Result<Vec<ImportedConnection>, ImportError> {
    let bastions = ssh_configs(ssh_configs_xml);
    let shared = fragments(&parse_document(data_sources)?, false)?;
    let local = match local {
        // The local file is optional; a damaged one only loses user names.
        Some(bytes) => parse_document(bytes)
            .and_then(|xml| fragments(&xml, true))
            .unwrap_or_default(),
        None => Vec::new(),
    };
    let mut seen = HashSet::new();
    Ok(merge(shared, local)
        .into_iter()
        .filter_map(|fragment| {
            let kind = kind_of(&fragment)?;
            let sqlite = kind.supported() == Some(ConnectionKind::Sqlite);
            let target = jdbc::parse(&fragment.jdbc_url);
            let host = if target.host.is_empty() && !sqlite {
                "127.0.0.1".to_string()
            } else {
                target.host
            };
            let name = Some(fragment.name.clone())
                .filter(|name| !name.is_empty())
                .or(Some(target.database.clone()).filter(|database| !database.is_empty()))
                .unwrap_or_else(|| host.clone());
            let mut connection = ImportedConnection::new(name, kind);
            connection.host = host;
            connection.port = target.port;
            connection.database = if sqlite { String::new() } else { target.database };
            connection.username = if fragment.username.is_empty() {
                target.user
            } else {
                fragment.username
            };
            if connection.kind.supported() == Some(ConnectionKind::Mongodb) {
                connection.uri = jdbc::mongo_multi_host_uri(&fragment.jdbc_url);
            }
            if let Some((_, ssh, proxy)) =
                bastions.iter().find(|(id, ..)| *id == fragment.ssh_config_id)
            {
                connection.ssh = Some(ssh.clone());
                connection.proxy = proxy.clone();
            }
            Some(connection)
        })
        .filter(|c| {
            seen.insert((c.name.clone(), c.kind.label(), c.host.clone(), c.port, c.database.clone()))
        })
        .collect())
}

#[cfg(test)]
mod tests {
    use super::*;

    const SHARED: &[u8] = include_bytes!("fixtures/datagrip/dataSources.xml");
    const LOCAL: &[u8] = include_bytes!("fixtures/datagrip/dataSources.local.xml");

    fn by_name<'a>(all: &'a [ImportedConnection], name: &str) -> &'a ImportedConnection {
        all.iter().find(|c| c.name == name).unwrap_or_else(|| panic!("{name} imported"))
    }

    #[test]
    fn merges_shared_and_local_files_into_connections_without_passwords() {
        let all = parse(SHARED, Some(LOCAL), &[]).unwrap();
        let pg = by_name(&all, "prod-postgres");
        assert_eq!(pg.kind, ImportKind::Supported(ConnectionKind::Postgres));
        assert_eq!(
            (pg.host.as_str(), pg.port, pg.database.as_str(), pg.username.as_str()),
            ("pg.example.com", Some(5432), "orders", "deploy")
        );
        assert!(all.iter().all(|c| c.password.is_none()));

        let mysql = by_name(&all, "local-mysql");
        assert_eq!(mysql.kind, ImportKind::Supported(ConnectionKind::Mysql));
        assert_eq!((mysql.host.as_str(), mysql.port, mysql.username.as_str()), ("localhost", Some(3306), "root"));

        let maria = by_name(&all, "maria");
        assert_eq!(maria.kind, ImportKind::Supported(ConnectionKind::Mariadb));

        let sqlite = by_name(&all, "notes.db");
        assert_eq!(sqlite.kind, ImportKind::Supported(ConnectionKind::Sqlite));
        assert_eq!(sqlite.host, "/Users/me/notes.db");

        let mongo = by_name(&all, "mongo-rs");
        assert_eq!(mongo.kind, ImportKind::Supported(ConnectionKind::Mongodb));
        assert_eq!(mongo.uri.as_deref(), Some("mongodb://m1.example.com:27017,m2.example.com:27017/app"));
    }

    #[test]
    fn user_names_need_the_local_file_and_the_product_hint_fills_in_a_missing_driver_ref() {
        let shared_only = parse(SHARED, None, &[]).unwrap();
        assert!(by_name(&shared_only, "prod-postgres").username.is_empty());
        // No <driver-ref>: the product recorded in the local file decides.
        let all = parse(SHARED, Some(LOCAL), &[]).unwrap();
        assert_eq!(by_name(&all, "custom-driver-mysql").kind, ImportKind::Supported(ConnectionKind::Mysql));
        // A custom driver nothing recognises is dropped, not listed.
        assert!(all.iter().all(|c| c.name != "mystery"));
    }

    #[test]
    fn a_data_source_with_an_ssh_tunnel_takes_its_bastion_from_ssh_configs() {
        let shared = br#"<project version="4"><component name="DataSourceManagerImpl">
          <data-source source="LOCAL" name="via key" uuid="aaaaaaaa-0000-4000-8000-000000000001">
            <driver-ref>postgresql</driver-ref><jdbc-driver>org.postgresql.Driver</jdbc-driver>
            <jdbc-url>jdbc:postgresql://db.internal:5432/orders</jdbc-url>
            <ssh-properties><enabled>true</enabled><ssh-config-id>cfg-key</ssh-config-id></ssh-properties>
          </data-source>
          <data-source source="LOCAL" name="via password" uuid="aaaaaaaa-0000-4000-8000-000000000002">
            <driver-ref>postgresql</driver-ref><jdbc-driver>org.postgresql.Driver</jdbc-driver>
            <jdbc-url>jdbc:postgresql://db2.internal:5432/orders</jdbc-url>
            <ssh-properties><enabled>true</enabled><ssh-config-id>cfg-pw</ssh-config-id></ssh-properties>
          </data-source>
          <data-source source="LOCAL" name="tunnel off" uuid="aaaaaaaa-0000-4000-8000-000000000003">
            <driver-ref>postgresql</driver-ref><jdbc-driver>org.postgresql.Driver</jdbc-driver>
            <jdbc-url>jdbc:postgresql://db3.internal:5432/orders</jdbc-url>
            <ssh-properties><enabled>false</enabled><ssh-config-id>cfg-key</ssh-config-id></ssh-properties>
          </data-source>
        </component></project>"#;
        let configs = br#"<project version="4"><component name="SshConfigs"><configs>
          <sshConfig authType="KEY_PAIR" host="bastion.example.com" id="cfg-key" keyPath="/keys/id_rsa" port="2200" username="deploy" useOpenSSHConfig="false" />
          <sshConfig authType="PASSWORD" host="jump.example.com" id="cfg-pw" port="22" username="ops" useOpenSSHConfig="false" />
        </configs></component></project>"#;
        let all = parse(shared, None, &[configs]).unwrap();
        let key = by_name(&all, "via key").ssh.clone().expect("the key bastion");
        assert_eq!(
            (key.host.as_str(), key.port.as_str(), key.user.as_str(), key.auth, key.key_path.as_str()),
            ("bastion.example.com", "2200", "deploy", SshAuthMethod::KeyFile, "/keys/id_rsa")
        );
        assert!(key.usable());
        let password = by_name(&all, "via password").ssh.clone().expect("the password bastion");
        assert!(!password.usable(), "DataGrip keeps the SSH password in its keychain: imported off");
        assert!(by_name(&all, "tunnel off").ssh.is_none());
        // Without sshConfigs.xml nothing can be resolved.
        assert!(parse(shared, None, &[]).unwrap().iter().all(|c| c.ssh.is_none()));
    }

    #[test]
    fn a_proxy_named_by_the_ssh_config_is_imported_and_stays_off_when_it_needs_a_login() {
        let shared = br#"<project version="4"><component name="DataSourceManagerImpl">
          <data-source source="LOCAL" name="proxied" uuid="aaaaaaaa-0000-4000-8000-000000000011">
            <driver-ref>postgresql</driver-ref><jdbc-driver>org.postgresql.Driver</jdbc-driver>
            <jdbc-url>jdbc:postgresql://db.internal:5432/orders</jdbc-url>
            <ssh-properties><enabled>true</enabled><ssh-config-id>cfg-open</ssh-config-id></ssh-properties>
          </data-source>
          <data-source source="LOCAL" name="proxied login" uuid="aaaaaaaa-0000-4000-8000-000000000012">
            <driver-ref>postgresql</driver-ref><jdbc-driver>org.postgresql.Driver</jdbc-driver>
            <jdbc-url>jdbc:postgresql://db2.internal:5432/orders</jdbc-url>
            <ssh-properties><enabled>true</enabled><ssh-config-id>cfg-login</ssh-config-id></ssh-properties>
          </data-source>
        </component></project>"#;
        let configs = br#"<project version="4"><component name="SshConfigs"><configs>
          <sshConfig authType="OPEN_SSH" host="bastion.example.com" id="cfg-open" port="22" username="deploy" proxyHost="proxy.example.com" proxyPort="3128" proxyType="HTTP" />
          <sshConfig authType="OPEN_SSH" host="bastion.example.com" id="cfg-login" port="22" username="deploy" proxyHost="proxy.example.com" proxyType="SOCKS5" proxyUsername="px" />
        </configs></component></project>"#;
        let all = parse(shared, None, &[configs]).unwrap();
        let open = by_name(&all, "proxied");
        let proxy = open.proxy.as_ref().expect("the proxy");
        assert_eq!((proxy.kind, proxy.host.as_str(), proxy.port.as_str()), (ProxyType::Http, "proxy.example.com", "3128"));
        assert!(open.route_usable());
        let login = by_name(&all, "proxied login");
        assert_eq!(login.proxy.as_ref().unwrap().port, "1080", "the protocol's default port");
        assert!(!login.route_usable(), "DataGrip keeps the proxy password in its keychain");
    }

    #[test]
    fn unsupported_products_are_listed() {
        let all = parse(SHARED, Some(LOCAL), &[]).unwrap();
        assert_eq!(by_name(&all, "warehouse-mssql").kind, ImportKind::Unsupported("SQL Server".into()));
        assert_eq!(by_name(&all, "ledger-oracle").kind, ImportKind::Unsupported("Oracle".into()));
        assert_eq!(all.len(), 8);
    }

    #[test]
    fn rejects_non_xml_and_tolerates_a_damaged_local_file() {
        assert_eq!(parse(b"{}", None, &[]), Err(ImportError::InvalidXml));
        let with_bad_local = parse(SHARED, Some(b"<broken"), &[]).unwrap();
        assert_eq!(with_bad_local.len(), parse(SHARED, None, &[]).unwrap().len());
    }

    #[test]
    fn locate_finds_files_in_idea_options_and_jetbrains_folders() {
        let root = std::env::temp_dir().join(format!("tablex-datagrip-locate-{}", uuid::Uuid::new_v4()));
        let options = root.join("JetBrains/DataGrip2024.3/options");
        let older = root.join("JetBrains/DataGrip2023.1/options");
        for dir in [&options, &older] {
            std::fs::create_dir_all(dir).unwrap();
            std::fs::write(dir.join("dataSources.xml"), SHARED).unwrap();
        }
        std::fs::write(options.join("dataSources.local.xml"), LOCAL).unwrap();

        for pick in [options.clone(), options.parent().unwrap().to_path_buf(), root.join("JetBrains"), options.join("dataSources.xml")] {
            let files = locate(&[pick]).unwrap();
            assert_eq!(files.data_sources, options.join("dataSources.xml"));
            assert_eq!(files.local, Some(options.join("dataSources.local.xml")));
        }
        assert_eq!(locate(std::slice::from_ref(&root)), Err(ImportError::MissingFile("dataSources.xml")));
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn the_ides_own_ssh_configs_are_found_next_to_the_project_ones() {
        let root = std::env::temp_dir().join(format!("tablex-datagrip-ssh-{}", uuid::Uuid::new_v4()));
        let project = root.join("project/.idea");
        let ide = root.join("JetBrains/DataGrip2024.3/options");
        for dir in [&project, &ide] {
            std::fs::create_dir_all(dir).unwrap();
        }
        std::fs::write(project.join("dataSources.xml"), SHARED).unwrap();
        // Only the IDE knows this bastion.
        std::fs::write(ide.join("sshConfigs.xml"), "<project/>").unwrap();
        let files = locate_with(std::slice::from_ref(&project), Some(&root.join("JetBrains"))).unwrap();
        assert_eq!(files.ssh_configs, vec![ide.join("sshConfigs.xml")]);
        std::fs::write(project.join("sshConfigs.xml"), "<project/>").unwrap();
        let both = locate_with(std::slice::from_ref(&project), Some(&root.join("JetBrains"))).unwrap();
        assert_eq!(both.ssh_configs, vec![project.join("sshConfigs.xml"), ide.join("sshConfigs.xml")]);
        // Without an IDE root only the project's file counts.
        assert_eq!(locate_with(&[project], None).unwrap().ssh_configs.len(), 1);
        std::fs::remove_dir_all(root).unwrap();
    }
}
