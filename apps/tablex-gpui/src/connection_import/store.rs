//! Where imported connections go: records into the saved-connection store and
//! passwords into the OS credential store, never into the record. The app
//! talks to [`ConnectionImportStore`], so UI scenarios and tests run against
//! [`MemoryImportStore`] and cannot touch the user's connections or keychain.

use super::{ImportedConnection, Secret};
use crate::connections::SavedConnection;
use crate::credentials::{CredentialError, CredentialStore, SecretKind, SystemStore};
use crate::ssh_tunnel::SshSettings;
use engine_storage::connection::SavedConnectionRecord;
use std::collections::{HashMap, HashSet};
use std::sync::{Mutex, PoisonError};

/// A secret to store under a record's id (the database password, an SSH password or passphrase).
#[derive(Debug, Clone)]
pub struct ImportSecret {
    pub account: String,
    pub kind: SecretKind,
    pub secret: Secret,
}

/// Records ready to save and the secrets to store under their ids.
#[derive(Debug, Default)]
pub struct Batch {
    pub records: Vec<SavedConnectionRecord>,
    pub secrets: Vec<ImportSecret>,
    /// Connections whose fields could not form a valid record.
    pub invalid: usize,
}

impl Batch {
    pub fn build(selected: &[ImportedConnection], now: f64) -> Self {
        let mut batch = Self::default();
        for connection in selected {
            match connection.to_record(now) {
                Ok(record) => {
                    if let Some(secret) = &connection.password {
                        batch.secrets.push(ImportSecret {
                            account: record.id.clone(),
                            kind: SecretKind::Database,
                            secret: secret.clone(),
                        });
                    }
                    for (kind, secret) in connection.ssh.iter().flat_map(|ssh| ssh.secrets()) {
                        batch.secrets.push(ImportSecret {
                            account: record.id.clone(),
                            kind,
                            secret,
                        });
                    }
                    for (index, kind, secret) in
                        connection.ssh.iter().flat_map(|ssh| ssh.jump_secrets())
                    {
                        batch.secrets.push(ImportSecret {
                            account: crate::credentials::jump_account(&record.id, index),
                            kind,
                            secret,
                        });
                    }
                    let proxy_password = connection
                        .proxy
                        .as_ref()
                        .filter(|proxy| !proxy.user.is_empty())
                        .and_then(|proxy| proxy.password.clone());
                    if let Some(secret) = proxy_password {
                        batch.secrets.push(ImportSecret {
                            account: record.id.clone(),
                            kind: SecretKind::Proxy,
                            secret,
                        });
                    }
                    batch.records.push(record);
                }
                Err(_) => batch.invalid += 1,
            }
        }
        batch
    }

    /// Drops the records of connections that are already saved: the same name, type, host, port,
    /// database and user (the URI before its options), so importing a file twice does not double
    /// the list. Their secrets go with them. Returns how many were dropped.
    pub fn skip_existing(&mut self, existing: &[SavedConnection]) -> usize {
        let identity = |name: &str, db_type: &str, uri: &str| {
            (name.to_owned(), db_type.to_owned(), uri.split(['?', '#']).next().unwrap_or_default().to_owned())
        };
        let saved: HashSet<_> = existing
            .iter()
            .map(|connection| identity(&connection.name, connection.kind.driver_id(), &connection.uri))
            .collect();
        let (kept, dropped): (Vec<_>, Vec<_>) = std::mem::take(&mut self.records)
            .into_iter()
            .partition(|record| !saved.contains(&identity(&record.name, &record.db_type, &record.uri)));
        self.records = kept;
        // Secrets are stored under the record id (and `<id>#jump<n>` for jump hosts).
        self.secrets.retain(|secret| {
            !dropped.iter().any(|record| {
                secret.account == record.id || secret.account.starts_with(&format!("{}#", record.id))
            })
        });
        dropped.len()
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SaveOutcome {
    pub passwords_saved: usize,
    /// Passwords the credential store refused; those connections are saved
    /// without a stored password.
    pub passwords_failed: usize,
}

/// The saved-connection store refused the batch; nothing was changed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SaveError(pub String);

impl std::fmt::Display for SaveError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "Could not save connections: {}", self.0)
    }
}

impl std::error::Error for SaveError {}

pub trait ConnectionImportStore: Send + Sync {
    /// Blocking (OS credential store, SQLite): call off the UI thread.
    fn save(
        &self,
        records: Vec<SavedConnectionRecord>,
        secrets: Vec<ImportSecret>,
    ) -> Result<SaveOutcome, SaveError>;
}

/// Passwords first, so a saved record never promises a password that is not
/// there; if the records then fail, the passwords are taken back.
fn save_with(
    credentials: &dyn CredentialStore,
    persist: impl FnOnce(&[SavedConnectionRecord]) -> Result<(), String>,
    mut records: Vec<SavedConnectionRecord>,
    secrets: Vec<ImportSecret>,
) -> Result<SaveOutcome, SaveError> {
    let mut stored = Vec::new();
    let mut failed = HashSet::new();
    for ImportSecret { account, kind, secret } in secrets {
        match credentials.write(kind, &account, secret.expose()) {
            Ok(()) => stored.push((kind, account)),
            Err(_) => {
                failed.insert((kind, account));
            }
        }
    }
    for record in records.iter_mut() {
        if failed.contains(&(SecretKind::Database, record.id.clone())) {
            record.save_password = false;
        }
        // A tunnel or proxy secret the credential store refused is asked for at connect time instead.
        let refused = |kinds: &[SecretKind], account: &dyn Fn(usize) -> String| {
            kinds.iter().any(|kind| {
                (0..engine_net::MAX_JUMPS)
                    .any(|index| failed.contains(&(*kind, account(index))))
            })
        };
        let ssh_failed = failed.contains(&(SecretKind::SshPassword, record.id.clone()))
            || failed.contains(&(SecretKind::SshPassphrase, record.id.clone()))
            || refused(&[SecretKind::SshPassword, SecretKind::SshPassphrase], &|index| {
                crate::credentials::jump_account(&record.id, index)
            });
        let proxy_failed = failed.contains(&(SecretKind::Proxy, record.id.clone()));
        if ssh_failed || proxy_failed {
            let mut route = SshSettings::from_network_json(&record.network_settings_json);
            if ssh_failed {
                route.save_secrets = false;
            }
            if proxy_failed {
                route.proxy.save_secrets = false;
            }
            record.network_settings_json = route.write_into(&record.network_settings_json);
        }
    }
    if let Err(error) = persist(&records) {
        for (kind, account) in &stored {
            let _ = credentials.delete(*kind, account);
        }
        return Err(SaveError(error));
    }
    Ok(SaveOutcome {
        passwords_saved: stored.len(),
        passwords_failed: failed.len(),
    })
}

/// The real stores: `connections.db` and the OS keychain.
pub struct SystemImportStore;

impl ConnectionImportStore for SystemImportStore {
    fn save(
        &self,
        records: Vec<SavedConnectionRecord>,
        secrets: Vec<ImportSecret>,
    ) -> Result<SaveOutcome, SaveError> {
        save_with(
            &SystemStore::new(),
            |records| {
                engine_storage::connection::upsert_connections_batch(records)
                    .map_err(|error| error.to_string())
            },
            records,
            secrets,
        )
    }
}

/// Keeps everything in memory: UI scenarios and tests.
#[derive(Default)]
pub struct MemoryImportStore {
    records: Mutex<Vec<SavedConnectionRecord>>,
    secrets: Mutex<HashMap<(SecretKind, String), String>>,
}

impl MemoryImportStore {
    #[cfg(test)]
    pub fn records(&self) -> Vec<SavedConnectionRecord> {
        self.records.lock().unwrap_or_else(PoisonError::into_inner).clone()
    }

    #[cfg(test)]
    pub fn password(&self, id: &str) -> Option<String> {
        self.secret(SecretKind::Database, id)
    }

    #[cfg(test)]
    pub fn secret(&self, kind: SecretKind, id: &str) -> Option<String> {
        self.secrets
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .get(&(kind, id.to_string()))
            .cloned()
    }
}

impl CredentialStore for MemoryImportStore {
    fn load(&self, kind: SecretKind, account: &str) -> Result<Option<String>, CredentialError> {
        Ok(self
            .secrets
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .get(&(kind, account.to_string()))
            .cloned())
    }

    fn write(&self, kind: SecretKind, account: &str, password: &str) -> Result<(), CredentialError> {
        self.secrets
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .insert((kind, account.to_string()), password.to_string());
        Ok(())
    }

    fn delete(&self, kind: SecretKind, account: &str) -> Result<(), CredentialError> {
        self.secrets
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .remove(&(kind, account.to_string()));
        Ok(())
    }
}

impl ConnectionImportStore for MemoryImportStore {
    fn save(
        &self,
        records: Vec<SavedConnectionRecord>,
        secrets: Vec<ImportSecret>,
    ) -> Result<SaveOutcome, SaveError> {
        save_with(
            self,
            |records| {
                self.records
                    .lock()
                    .unwrap_or_else(PoisonError::into_inner)
                    .extend_from_slice(records);
                Ok(())
            },
            records,
            secrets,
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::connection_import::{ImportKind, ImportedConnection};
    use crate::connections::ConnectionKind;
    use crate::credentials::fake::MemoryStore;

    fn connection(name: &str, password: &str) -> ImportedConnection {
        let mut connection =
            ImportedConnection::new(name.into(), ImportKind::Supported(ConnectionKind::Mysql));
        connection.host = "db.example.com".into();
        connection.username = "u".into();
        connection.password = Secret::new(password);
        connection
    }

    #[test]
    fn batch_keeps_passwords_out_of_the_records_and_counts_unusable_ones() {
        let oracle = ImportedConnection::new("o".into(), ImportKind::Unsupported("Oracle".into()));
        let batch = Batch::build(&[connection("a", "pw-a"), connection("b", ""), oracle], 5.0);
        assert_eq!(batch.records.len(), 2);
        assert_eq!(batch.invalid, 1);
        assert_eq!(batch.secrets.len(), 1);
        assert_eq!(batch.secrets[0].account, batch.records[0].id);
        assert_eq!(batch.secrets[0].kind, SecretKind::Database);
        assert!(batch.records[0].save_password);
        assert!(!batch.records[1].save_password);
        assert!(!serde_json::to_string(&batch.records).unwrap().contains("pw-a"));
        assert!(!format!("{batch:?}").contains("pw-a"));
    }

    #[test]
    fn memory_store_saves_records_and_passwords() {
        let store = MemoryImportStore::default();
        let batch = Batch::build(&[connection("a", "pw-a")], 5.0);
        let id = batch.records[0].id.clone();
        let outcome = ConnectionImportStore::save(&store, batch.records, batch.secrets).unwrap();
        assert_eq!(outcome, SaveOutcome { passwords_saved: 1, passwords_failed: 0 });
        assert_eq!(store.records().len(), 1);
        assert_eq!(store.password(&id).as_deref(), Some("pw-a"));
    }

    #[test]
    fn unavailable_keychain_saves_the_record_without_a_password_flag() {
        let keychain = MemoryStore { unavailable: true, ..MemoryStore::default() };
        let batch = Batch::build(&[connection("a", "pw-a")], 5.0);
        let mut saved = Vec::new();
        let outcome = save_with(
            &keychain,
            |records| {
                saved = records.to_vec();
                Ok(())
            },
            batch.records,
            batch.secrets,
        )
        .unwrap();
        assert_eq!(outcome, SaveOutcome { passwords_saved: 0, passwords_failed: 1 });
        assert!(!saved[0].save_password);
    }

    #[test]
    fn a_failed_record_save_takes_the_passwords_back() {
        let keychain = MemoryStore::default();
        let batch = Batch::build(&[connection("a", "pw-a")], 5.0);
        let id = batch.records[0].id.clone();
        let error = save_with(&keychain, |_| Err("disk full".into()), batch.records, batch.secrets)
            .unwrap_err();
        assert_eq!(error.to_string(), "Could not save connections: disk full");
        assert_eq!(keychain.load(SecretKind::Database, &id).unwrap(), None);
    }

    #[test]
    fn jump_and_proxy_secrets_go_to_their_own_accounts_and_never_into_the_record() {
        use crate::connection_import::{ImportedJump, ImportedProxy, ImportedSsh};
        use crate::ssh_tunnel::{ProxyType, SshAuthMethod};
        let mut routed = connection("routed", "pw-db");
        routed.ssh = Some(ImportedSsh {
            host: "bastion.example.com".into(),
            port: "22".into(),
            user: "deploy".into(),
            auth: SshAuthMethod::Agent,
            jumps: vec![
                ImportedJump { host: "j1".into(), port: "2201".into(), user: "a".into(), auth: SshAuthMethod::Password, password: Secret::new("pw-jump1"), ..ImportedJump::default() },
                ImportedJump { host: "j2".into(), port: "22".into(), user: "b".into(), auth: SshAuthMethod::KeyFile, key_path: "/k".into(), passphrase: Secret::new("phrase-jump2"), ..ImportedJump::default() },
            ],
            ..ImportedSsh::default()
        });
        routed.proxy = Some(ImportedProxy { kind: ProxyType::Socks5, host: "p".into(), port: "1080".into(), user: "px".into(), password: Secret::new("pw-proxy") });
        assert!(routed.route_usable());
        let label = routed.tunnel_label().unwrap();
        assert_eq!(label, "via SSH deploy@bastion.example.com (2 jumps) through SOCKS5 proxy p:1080");
        let batch = Batch::build(&[routed], 5.0);
        let id = batch.records[0].id.clone();
        let route = SshSettings::parse(&batch.records[0].network_settings_json).unwrap();
        assert!(route.enabled && route.proxy.enabled && route.jumps.len() == 2);
        let text = serde_json::to_string(&batch.records).unwrap();
        for secret in ["pw-jump1", "phrase-jump2", "pw-proxy"] {
            assert!(!text.contains(secret) && !format!("{batch:?}").contains(secret), "{secret}");
        }
        let store = MemoryImportStore::default();
        ConnectionImportStore::save(&store, batch.records, batch.secrets).unwrap();
        assert_eq!(store.secret(SecretKind::SshPassword, &format!("{id}#jump0")).as_deref(), Some("pw-jump1"));
        assert_eq!(store.secret(SecretKind::SshPassphrase, &format!("{id}#jump1")).as_deref(), Some("phrase-jump2"));
        assert_eq!(store.secret(SecretKind::Proxy, &id).as_deref(), Some("pw-proxy"));
    }

    #[test]
    fn a_hop_secret_the_keychain_refuses_is_asked_at_connect_time_instead() {
        use crate::connection_import::{ImportedProxy, ImportedSsh};
        use crate::ssh_tunnel::ProxyType;
        let mut routed = connection("routed", "");
        routed.ssh = Some(ImportedSsh { host: "b".into(), port: "22".into(), user: "u".into(), auth: crate::ssh_tunnel::SshAuthMethod::Agent, ..ImportedSsh::default() });
        routed.proxy = Some(ImportedProxy { kind: ProxyType::Http, host: "p".into(), port: "8080".into(), user: "px".into(), password: Secret::new("pw-proxy") });
        let batch = Batch::build(&[routed], 5.0);
        let keychain = MemoryStore { unavailable: true, ..MemoryStore::default() };
        let mut saved = Vec::new();
        save_with(&keychain, |records| { saved = records.to_vec(); Ok(()) }, batch.records, batch.secrets).unwrap();
        let route = SshSettings::parse(&saved[0].network_settings_json).unwrap();
        assert!(!route.proxy.save_secrets, "the proxy password will be asked for");
        assert!(route.save_secrets, "nothing was refused for the SSH side");
    }

    #[test]
    fn skipping_saved_connections_takes_their_secrets_along() {
        use crate::connection_import::{ImportedJump, ImportedSsh};
        use crate::ssh_tunnel::SshAuthMethod;
        let mut routed = connection("same", "pw-db");
        routed.ssh = Some(ImportedSsh {
            host: "b".into(), port: "22".into(), user: "u".into(), auth: SshAuthMethod::Agent,
            jumps: vec![ImportedJump { host: "j".into(), port: "22".into(), user: "a".into(), auth: SshAuthMethod::Password, password: Secret::new("pw-jump"), ..ImportedJump::default() }],
            ..ImportedSsh::default()
        });
        let mut batch = Batch::build(&[routed, connection("new", "pw-new")], 5.0);
        let first = batch.records[0].clone();
        let saved = vec![crate::connections::SavedConnection::from_record(&first)];
        assert_eq!(batch.secrets.len(), 3);
        assert_eq!(batch.skip_existing(&saved), 1);
        assert_eq!(batch.records.len(), 1);
        assert_eq!(batch.records[0].name, "new");
        assert_eq!(batch.secrets.len(), 1, "no secret of the skipped connection is written again");
        assert_eq!(batch.secrets[0].account, batch.records[0].id);
    }
}
