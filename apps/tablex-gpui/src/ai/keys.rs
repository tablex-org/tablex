//! The provider API key in the OS credential store (macOS Keychain, Windows Credential Manager,
//! Linux Secret Service) through the same [`CredentialStore`] the connection passwords use: service
//! `tablex.ai.api_key`, account = the provider id (`anthropic`, `openai`, `openai_compatible`,
//! `ollama`). The key is not in `settings.json`, not in a log, not in any `Debug`; it is handed to
//! the engine as a [`SecretText`] when a request is built (never cached here). Reading or writing
//! can block on the OS: not on the UI thread.

use crate::credentials::{CredentialError, CredentialStore, SecretKind};
use crate::settings::{AiProvider, Choice};
use engine_ai::{ApiKey, KeyError, KeySource};
use std::sync::{Arc, Mutex};

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum KeyStoreError {
    Empty,
    NoStore,
    Store(String),
}

impl std::fmt::Display for KeyStoreError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Empty => f.write_str("Paste the API key first"),
            Self::NoStore => {
                f.write_str("No credential store is available, so the key cannot be kept")
            }
            Self::Store(message) => write!(f, "The credential store refused: {message}"),
        }
    }
}

impl std::error::Error for KeyStoreError {}

impl From<CredentialError> for KeyStoreError {
    fn from(error: CredentialError) -> Self {
        match error {
            CredentialError::NoKeyring => Self::NoStore,
            CredentialError::Platform(message) => Self::Store(message),
        }
    }
}

/// The OS credential store, shared by the keys of every provider.
type SharedStore = Arc<Mutex<Box<dyn CredentialStore + Send>>>;

/// The API key of one provider (its account in the credential store). `Send + Sync`: the engine
/// asks for the key from worker threads while the Settings page saves it. A provider is built with
/// the store of its own account, so it cannot read, or send, another provider's key.
pub struct ApiKeyStore {
    store: SharedStore,
    account: &'static str,
}

impl ApiKeyStore {
    /// The OS credential store, on Anthropic's account.
    pub fn system() -> Self {
        Self::with_store(Box::new(crate::credentials::SystemStore::new()))
    }

    /// An in-memory store (UI scenarios and tests never touch the real one).
    #[cfg(any(test, feature = "ui-scenarios"))]
    pub fn memory() -> Self {
        Self::with_store(Box::new(crate::credentials::fake::MemoryStore::default()))
    }

    /// `store`, on Anthropic's account.
    pub fn with_store(store: Box<dyn CredentialStore + Send>) -> Self {
        Self {
            store: Arc::new(Mutex::new(store)),
            account: AiProvider::Anthropic.id(),
        }
    }

    /// The same credential store on `provider`'s account.
    pub fn for_provider(&self, provider: AiProvider) -> Arc<Self> {
        Arc::new(Self {
            store: Arc::clone(&self.store),
            account: provider.id(),
        })
    }

    fn store(&self) -> std::sync::MutexGuard<'_, Box<dyn CredentialStore + Send>> {
        self.store
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    /// Whether a key is saved. The key itself is read but dropped (wiped) at once.
    pub fn is_set(&self) -> Result<bool, KeyStoreError> {
        Ok(self.load()?.is_some())
    }

    fn load(&self) -> Result<Option<ApiKey>, KeyStoreError> {
        let stored = self.store().load(SecretKind::AiApiKey, self.account)?;
        Ok(stored.filter(|key| !key.is_empty()).map(ApiKey::new))
    }

    /// Replaces the saved key with `key` (surrounding whitespace, such as a pasted newline, is
    /// dropped).
    pub fn save(&self, key: &str) -> Result<(), KeyStoreError> {
        let key = key.trim();
        if key.is_empty() {
            return Err(KeyStoreError::Empty);
        }
        Ok(self
            .store()
            .write(SecretKind::AiApiKey, self.account, key)?)
    }

    /// Forgets the key; nothing saved is fine.
    pub fn remove(&self) -> Result<(), KeyStoreError> {
        Ok(self.store().delete(SecretKind::AiApiKey, self.account)?)
    }
}

impl KeySource for ApiKeyStore {
    fn api_key(&self) -> Result<ApiKey, KeyError> {
        match self.load() {
            Ok(Some(key)) => Ok(key),
            Ok(None) => Err(KeyError::NotSet),
            // The text names the store's problem, never the key.
            Err(error) => Err(KeyError::Unavailable(error.to_string())),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::credentials::fake::MemoryStore;
    use engine_ai::HeaderValue;

    /// The key's text as the transport reads it (the only way to read an `ApiKey`).
    fn text(key: ApiKey) -> String {
        HeaderValue::secret(key).expose().to_owned()
    }

    #[test]
    fn a_key_is_saved_read_and_removed() {
        let keys = ApiKeyStore::memory();
        assert_eq!(keys.is_set(), Ok(false));
        assert_eq!(keys.api_key().unwrap_err(), KeyError::NotSet);
        keys.save("  sk-test-123\n").unwrap();
        assert_eq!(keys.is_set(), Ok(true));
        assert_eq!(text(keys.api_key().unwrap()), "sk-test-123");
        keys.save("sk-other").unwrap();
        assert_eq!(text(keys.api_key().unwrap()), "sk-other");
        keys.remove().unwrap();
        keys.remove().unwrap();
        assert_eq!(keys.is_set(), Ok(false));
    }

    #[test]
    fn each_provider_has_its_own_account_in_the_one_store() {
        let anthropic = ApiKeyStore::memory();
        anthropic.save("sk-anthropic").unwrap();
        let others: Vec<_> = AiProvider::ALL
            .into_iter()
            .filter(|provider| *provider != AiProvider::Anthropic)
            .map(|provider| anthropic.for_provider(provider))
            .collect();
        // Nothing of Anthropic's is readable through the others, and the other way round.
        for other in &others {
            assert_eq!(other.is_set(), Ok(false));
            assert_eq!(other.api_key().unwrap_err(), KeyError::NotSet);
        }
        for (ix, other) in others.iter().enumerate() {
            other.save(&format!("sk-other-{ix}")).unwrap();
        }
        assert_eq!(text(anthropic.api_key().unwrap()), "sk-anthropic");
        for (ix, other) in others.iter().enumerate() {
            assert_eq!(text(other.api_key().unwrap()), format!("sk-other-{ix}"));
        }
        // The account is the provider id.
        let store = MemoryStore::default();
        store
            .write(SecretKind::AiApiKey, "ollama", "sk-ollama")
            .unwrap();
        let ollama = ApiKeyStore::with_store(Box::new(store)).for_provider(AiProvider::Ollama);
        assert_eq!(text(ollama.api_key().unwrap()), "sk-ollama");
    }

    #[test]
    fn an_empty_key_is_refused_and_keeps_the_old_one() {
        let keys = ApiKeyStore::memory();
        keys.save("sk-test").unwrap();
        assert_eq!(keys.save("  \n"), Err(KeyStoreError::Empty));
        assert_eq!(text(keys.api_key().unwrap()), "sk-test");
    }

    #[test]
    fn the_key_has_its_own_service_and_account() {
        let store = MemoryStore::default();
        store
            .write(
                SecretKind::Database,
                AiProvider::Anthropic.id(),
                "db-password",
            )
            .unwrap();
        let keys = ApiKeyStore::with_store(Box::new(store));
        assert_eq!(
            keys.is_set(),
            Ok(false),
            "a connection secret is not the key"
        );
        keys.save("sk-test").unwrap();
        assert!(keys.is_set().unwrap());
    }

    #[test]
    fn a_missing_credential_store_is_a_typed_error_without_the_key() {
        let keys = ApiKeyStore::with_store(Box::new(MemoryStore {
            unavailable: true,
            ..MemoryStore::default()
        }));
        let error = keys.save("sk-secret-value").unwrap_err();
        assert_eq!(error, KeyStoreError::NoStore);
        assert!(!format!("{error} {error:?}").contains("sk-secret-value"));
        match keys.api_key() {
            Err(KeyError::Unavailable(text)) => assert!(!text.contains("sk-secret-value")),
            other => panic!("expected Unavailable, got {other:?}"),
        }
    }
}
