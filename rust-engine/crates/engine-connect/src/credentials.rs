//! OS credential storage for saved connection passwords, behind one
//! [`CredentialStore`] trait: macOS Keychain, Windows Credential Manager and the
//! Linux Secret Service, all through the `keyring` crate.
//!
//! macOS keeps reading the items the Swift app wrote: the `keyring` Apple store
//! addresses a generic password by the same service (`tablex.connection.*`) and
//! account (connection id) attributes the previous Security.framework backend
//! used. Secrets are never logged; errors carry only the store's own message.

use crate::platform::CREDENTIAL_STORE_LABEL;

/// Service identifiers, matching Swift `SecretKind` raw values.
const DB_SERVICE: &str = "tablex.connection.password";
const PROXY_SERVICE: &str = "tablex.connection.proxy.password";
const SSH_PASSWORD_SERVICE: &str = "tablex.connection.ssh.password";
const SSH_PASSPHRASE_SERVICE: &str = "tablex.connection.ssh.passphrase";
/// The AI assistant's provider API key (the account is the provider id, not a connection id).
const AI_API_KEY_SERVICE: &str = "tablex.ai.api_key";

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum SecretKind {
    Database,
    Proxy,
    /// The SSH tunnel's login password.
    SshPassword,
    /// The passphrase of the SSH tunnel's private key file.
    SshPassphrase,
    /// The AI assistant's API key; the account is the provider id (`anthropic`). Not a connection
    /// secret: it is not in [`SecretKind::ALL`], so deleting a connection never touches it.
    AiApiKey,
}

/// The account of the secrets of the jump host at `index` (0 = dialled first): the connection's
/// account plus the index, under the SSH password / passphrase services.
pub fn jump_account(account: &str, index: usize) -> String {
    format!("{account}#jump{index}")
}

impl SecretKind {
    /// The secrets that belong to a connection (what `delete_all` clears).
    const ALL: [Self; 4] = [
        Self::Database,
        Self::Proxy,
        Self::SshPassword,
        Self::SshPassphrase,
    ];

    fn service(self) -> &'static str {
        match self {
            Self::Database => DB_SERVICE,
            Self::Proxy => PROXY_SERVICE,
            Self::SshPassword => SSH_PASSWORD_SERVICE,
            Self::SshPassphrase => SSH_PASSPHRASE_SERVICE,
            Self::AiApiKey => AI_API_KEY_SERVICE,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CredentialError {
    /// No credential store is running (e.g. no Secret Service on a minimal
    /// Linux session). Nothing can be saved or read; it is not a crash.
    NoKeyring,
    Platform(String),
}

impl std::fmt::Display for CredentialError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::NoKeyring => write!(
                f,
                "No {CREDENTIAL_STORE_LABEL} available; the password will not be saved"
            ),
            Self::Platform(message) => f.write_str(message),
        }
    }
}

impl std::error::Error for CredentialError {}

impl From<keyring::Error> for CredentialError {
    fn from(error: keyring::Error) -> Self {
        match error {
            keyring::Error::NoDefaultStore => Self::NoKeyring,
            other => Self::Platform(other.to_string()),
        }
    }
}

/// Where connection secrets live. `account` is the connection id.
pub trait CredentialStore {
    /// `Ok(None)` when nothing is stored (or the stored bytes are not UTF-8).
    fn load(&self, kind: SecretKind, account: &str) -> Result<Option<String>, CredentialError>;
    fn write(&self, kind: SecretKind, account: &str, password: &str)
        -> Result<(), CredentialError>;
    /// Succeeds when there is nothing to delete.
    fn delete(&self, kind: SecretKind, account: &str) -> Result<(), CredentialError>;

    /// Stores `password`; an empty one clears the entry instead.
    fn save(&self, kind: SecretKind, account: &str, password: &str) -> Result<(), CredentialError> {
        if password.is_empty() {
            self.delete(kind, account)
        } else {
            self.write(kind, account, password)
        }
    }

    /// Deletes every secret of a connection (also those of its jump hosts), attempting each
    /// entry even when another fails, and reports all real failures.
    fn delete_all(&self, account: &str) -> Result<(), CredentialError> {
        let jumps = (0..engine_net::MAX_JUMPS).flat_map(|index| {
            [SecretKind::SshPassword, SecretKind::SshPassphrase]
                .into_iter()
                .map(move |kind| (kind, jump_account(account, index)))
        });
        let failures: Vec<_> = SecretKind::ALL
            .into_iter()
            .map(|kind| (kind, account.to_owned()))
            .chain(jumps)
            .filter_map(|(kind, account)| {
                self.delete(kind, &account)
                    .err()
                    .map(|error| format!("{kind:?}: {error}"))
            })
            .collect();
        if failures.is_empty() {
            Ok(())
        } else {
            Err(CredentialError::Platform(failures.join("; ")))
        }
    }
}

/// The OS credential store. `service_override` replaces both service names
/// (isolated entries for the opt-in real-store test).
#[derive(Debug, Default, Clone)]
pub struct SystemStore {
    service_override: Option<String>,
}

impl SystemStore {
    pub const fn new() -> Self {
        Self {
            service_override: None,
        }
    }

    fn entry(&self, kind: SecretKind, account: &str) -> Result<keyring::Entry, CredentialError> {
        let service = self.service_override.as_deref().unwrap_or(kind.service());
        Ok(keyring::Entry::new(service, account)?)
    }
}

impl CredentialStore for SystemStore {
    fn load(&self, kind: SecretKind, account: &str) -> Result<Option<String>, CredentialError> {
        match self.entry(kind, account)?.get_password() {
            Ok(password) => Ok(Some(password)),
            Err(keyring::Error::NoEntry | keyring::Error::BadEncoding(_)) => Ok(None),
            Err(error) => Err(error.into()),
        }
    }

    fn write(
        &self,
        kind: SecretKind,
        account: &str,
        password: &str,
    ) -> Result<(), CredentialError> {
        Ok(self.entry(kind, account)?.set_password(password)?)
    }

    fn delete(&self, kind: SecretKind, account: &str) -> Result<(), CredentialError> {
        match self.entry(kind, account) {
            // No store running: nothing was ever saved here.
            Err(CredentialError::NoKeyring) => Ok(()),
            Err(error) => Err(error),
            Ok(entry) => match entry.delete_credential() {
                Ok(()) | Err(keyring::Error::NoEntry) => Ok(()),
                Err(error) => Err(error.into()),
            },
        }
    }
}

#[cfg(any(test, feature = "testing"))]
pub mod fake {
    use super::*;
    use std::{cell::RefCell, collections::HashMap};

    /// In-memory store; `failing` makes deleting that kind fail.
    #[derive(Default)]
    pub struct MemoryStore {
        pub items: RefCell<HashMap<(SecretKind, String), String>>,
        pub failing: Option<SecretKind>,
        pub unavailable: bool,
    }

    impl CredentialStore for MemoryStore {
        fn load(&self, kind: SecretKind, account: &str) -> Result<Option<String>, CredentialError> {
            if self.unavailable {
                return Err(CredentialError::NoKeyring);
            }
            Ok(self
                .items
                .borrow()
                .get(&(kind, account.to_string()))
                .cloned())
        }

        fn write(
            &self,
            kind: SecretKind,
            account: &str,
            password: &str,
        ) -> Result<(), CredentialError> {
            if self.unavailable {
                return Err(CredentialError::NoKeyring);
            }
            self.items
                .borrow_mut()
                .insert((kind, account.to_string()), password.to_string());
            Ok(())
        }

        fn delete(&self, kind: SecretKind, account: &str) -> Result<(), CredentialError> {
            if self.failing == Some(kind) {
                return Err(CredentialError::Platform("keychain locked".into()));
            }
            self.items.borrow_mut().remove(&(kind, account.to_string()));
            Ok(())
        }
    }
}

#[cfg(test)]
mod tests {
    use super::fake::MemoryStore;
    use super::*;

    #[test]
    fn save_load_and_clear_round_trip_per_kind() {
        let store = MemoryStore::default();
        store.save(SecretKind::Database, "id", "pw").unwrap();
        store.save(SecretKind::Proxy, "id", "proxy-pw").unwrap();
        assert_eq!(
            store.load(SecretKind::Database, "id").unwrap().as_deref(),
            Some("pw")
        );
        assert_eq!(
            store.load(SecretKind::Proxy, "id").unwrap().as_deref(),
            Some("proxy-pw")
        );
        store.save(SecretKind::Database, "id", "").unwrap();
        assert_eq!(store.load(SecretKind::Database, "id").unwrap(), None);
        assert_eq!(
            store.load(SecretKind::Proxy, "id").unwrap().as_deref(),
            Some("proxy-pw")
        );
    }

    #[test]
    fn delete_all_attempts_every_kind_and_reports_real_failures() {
        let store = MemoryStore {
            failing: Some(SecretKind::Database),
            ..MemoryStore::default()
        };
        store.write(SecretKind::Proxy, "id", "proxy-pw").unwrap();
        let error = store.delete_all("id").expect_err("database delete fails");
        assert!(error.to_string().contains("Database: keychain locked"));
        // The proxy secret was still cleaned up.
        assert_eq!(store.load(SecretKind::Proxy, "id").unwrap(), None);

        let clean = MemoryStore::default();
        assert!(clean.delete_all("missing").is_ok());
    }

    #[test]
    fn the_ai_key_has_its_own_service_and_survives_deleting_a_connection() {
        assert_eq!(SecretKind::AiApiKey.service(), "tablex.ai.api_key");
        assert!(!SecretKind::ALL.contains(&SecretKind::AiApiKey));
        let store = MemoryStore::default();
        store
            .save(SecretKind::AiApiKey, "anthropic", "sk-test")
            .unwrap();
        store.save(SecretKind::Database, "anthropic", "pw").unwrap();
        store.delete_all("anthropic").unwrap();
        assert_eq!(store.load(SecretKind::Database, "anthropic").unwrap(), None);
        assert_eq!(
            store
                .load(SecretKind::AiApiKey, "anthropic")
                .unwrap()
                .as_deref(),
            Some("sk-test")
        );
    }

    #[test]
    fn missing_keyring_is_a_typed_error_with_the_platform_label() {
        let store = MemoryStore {
            unavailable: true,
            ..MemoryStore::default()
        };
        let error = store.save(SecretKind::Database, "id", "pw").unwrap_err();
        assert_eq!(error, CredentialError::NoKeyring);
        assert_eq!(
            error.to_string(),
            format!("No {CREDENTIAL_STORE_LABEL} available; the password will not be saved")
        );
        assert_eq!(
            CredentialError::from(keyring::Error::NoDefaultStore),
            CredentialError::NoKeyring
        );
    }

    /// Real Keychain round trip against a unique service; run with
    /// `cargo test -p engine-connect -- --ignored keychain_round_trip`.
    /// Also proves items written by the old Security.framework backend (and the
    /// Swift app) under the same service/account stay readable.
    #[cfg(target_os = "macos")]
    #[test]
    #[ignore = "touches the real login Keychain"]
    fn keychain_round_trip_is_compatible_with_security_framework_items() {
        let service = format!("tablex.test.{}", uuid::Uuid::new_v4());
        let store = SystemStore {
            service_override: Some(service.clone()),
        };
        let account = "ROUND-TRIP-ACCOUNT";
        let cleanup = || {
            let _ = security_framework::passwords::delete_generic_password(&service, account);
        };
        cleanup();

        assert_eq!(store.load(SecretKind::Database, account).unwrap(), None);
        store
            .save(SecretKind::Database, account, "p@ss wörd")
            .unwrap();
        assert_eq!(
            store
                .load(SecretKind::Database, account)
                .unwrap()
                .as_deref(),
            Some("p@ss wörd")
        );
        // Written by keyring, read by the old backend's API.
        assert_eq!(
            security_framework::passwords::get_generic_password(&service, account).unwrap(),
            "p@ss wörd".as_bytes()
        );
        // Written by the old backend's API, read by keyring (overwrites in place).
        security_framework::passwords::set_generic_password(&service, account, b"legacy").unwrap();
        assert_eq!(
            store
                .load(SecretKind::Database, account)
                .unwrap()
                .as_deref(),
            Some("legacy")
        );
        store.delete(SecretKind::Database, account).unwrap();
        assert_eq!(store.load(SecretKind::Database, account).unwrap(), None);
        // Deleting what is gone is fine.
        store.delete(SecretKind::Database, account).unwrap();
        cleanup();
    }
}
