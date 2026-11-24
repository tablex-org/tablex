//! Saved connection passwords behind one [`CredentialStore`] trait. Front ends use
//! [`default_store`]: the encrypted `secrets` table of the app database ([`crate::secret_store`])
//! whose one master key sits in the OS credential store (macOS Keychain, Windows Credential
//! Manager, Linux Secret Service, all through the `keyring` crate), so the OS asks about one item,
//! not one per secret.
//!
//! [`SystemStore`] is the OS store itself, one item per secret. It is where the Swift app and
//! earlier versions kept them (the `keyring` Apple store addresses a generic password by the same
//! service (`tablex.connection.*`) and account (connection id) attributes the previous
//! Security.framework backend used), so it stays as the source of the lazy migration into the
//! encrypted table. Secrets are never logged; errors carry only the store's own message.

use crate::platform::CREDENTIAL_STORE_LABEL;

/// Service identifiers, matching Swift `SecretKind` raw values.
const DB_SERVICE: &str = "tablex.connection.password";
const PROXY_SERVICE: &str = "tablex.connection.proxy.password";
const SSH_PASSWORD_SERVICE: &str = "tablex.connection.ssh.password";
const SSH_PASSPHRASE_SERVICE: &str = "tablex.connection.ssh.passphrase";
/// The AI assistant's provider API key (the account is the provider id, not a connection id).
const AI_API_KEY_SERVICE: &str = "tablex.ai.api_key";
/// The accounts of [`SecretKind::AiApiKey`]: the provider ids of the assistant's settings. What
/// [`CredentialStore::reset`] deletes among the items of earlier versions.
pub(crate) const AI_API_KEY_ACCOUNTS: [&str; 4] = ["anthropic", "openai", "openai_compatible", "ollama"];
/// The one item the encrypted secrets table's key lives in (account [`MASTER_KEY_ACCOUNT`]).
const MASTER_KEY_SERVICE: &str = "tablex.secret-store.v1";
pub(crate) const MASTER_KEY_ACCOUNT: &str = "master-key";

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
    /// The master key of the encrypted secrets table. Reserved to [`crate::secret_store`]: not in
    /// [`SecretKind::ALL`], and the encrypted store refuses it.
    MasterKey,
}

/// The account of the secrets of the jump host at `index` (0 = dialled first): the connection's
/// account plus the index, under the SSH password / passphrase services.
pub fn jump_account(account: &str, index: usize) -> String {
    format!("{account}#jump{index}")
}

impl SecretKind {
    /// The secrets that belong to a connection (what `delete_all` clears).
    pub(crate) const ALL: [Self; 4] = [
        Self::Database,
        Self::Proxy,
        Self::SshPassword,
        Self::SshPassphrase,
    ];

    pub(crate) fn service(self) -> &'static str {
        match self {
            Self::Database => DB_SERVICE,
            Self::Proxy => PROXY_SERVICE,
            Self::SshPassword => SSH_PASSWORD_SERVICE,
            Self::SshPassphrase => SSH_PASSPHRASE_SERVICE,
            Self::AiApiKey => AI_API_KEY_SERVICE,
            Self::MasterKey => MASTER_KEY_SERVICE,
        }
    }
}

/// The name of the app action that removes every saved password and key so that new ones can be
/// saved (the command palette entry "Reset Saved Passwords…"); the error texts that point to it
/// carry this name, which is how [`suggests_reset`] finds them.
pub const RESET_ACTION: &str = "Reset Saved Passwords";

/// Whether an error text sends the user to [`RESET_ACTION`].
pub fn suggests_reset(message: &str) -> bool {
    message.contains(RESET_ACTION)
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CredentialError {
    /// No credential store is running (e.g. no Secret Service on a minimal
    /// Linux session). Nothing can be saved or read; it is not a crash.
    NoKeyring,
    /// The app database holds `secrets` saved secrets but the master key that opens
    /// them is gone from the credential store (or is not the key of the table).
    MasterKeyMissing {
        secrets: usize,
        file: std::path::PathBuf,
        /// The key file of the file home ([`crate::key_home::Home::File`]) when the key lives
        /// there: the way out is then not [`RESET_ACTION`] but getting the key back.
        key_file: Option<std::path::PathBuf>,
        /// A key file that exists although the choice says the keyring (the choice file was
        /// removed by hand): the key is most likely in it, and the way out is to switch to the
        /// file home, not [`RESET_ACTION`].
        stray_key_file: Option<std::path::PathBuf>,
    },
    /// The app database holds `secrets` saved secrets, no system keyring is running and the
    /// key is not in a local file: it is most likely in the keyring of the desktop session, which
    /// is out of reach now. Nothing is lost; the way out is a running (or unlocked) keyring, never
    /// [`RESET_ACTION`], and the choices that would strand the key are refused.
    KeyUnreachable {
        secrets: usize,
    },
    /// The OS would have to ask the user before it hands the secret over, and this process turned
    /// that off (`tablex-mcp` while serving: a dialog nobody sees would hang the agent), or the
    /// login keychain is locked. The fix is the one-time authorisation, not a retry.
    InteractionNotAllowed,
    /// The user answered the OS's question with Deny or Cancel (the macOS Keychain dialog).
    UserRefused,
    Platform(String),
}

/// What an agent is told when [`CredentialError::InteractionNotAllowed`] stops an open (its code is
/// `credentials.keychain_access_needed`). `command` is how to run the authorisation from a
/// terminal: the absolute, shell-quoted path of the installed tool when the front end knows it (a
/// tool installed by the Driver Manager is not on `PATH`).
pub fn keychain_access_needed(command: Option<&str>) -> String {
    format!(
        "tablex cannot read the saved password without asking macOS (or the login keychain is \
         locked). Open tablex \u{2192} Driver Manager \u{2192} Tools \u{2192} tablex-mcp \u{2192} Allow \
         Keychain access (or run {} in a terminal), then restart the MCP client.",
        command.unwrap_or("`tablex-mcp --authorize-keychain`"),
    )
}

impl std::fmt::Display for CredentialError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::NoKeyring => write!(
                f,
                "No {CREDENTIAL_STORE_LABEL} available; the password will not be saved"
            ),
            // The choice says keyring, a key file is there: the passwords may well be fine.
            Self::MasterKeyMissing { secrets, key_file: None, stray_key_file: Some(stray), .. } => write!(
                f,
                "The master key of {secrets} saved {} is not in the {CREDENTIAL_STORE_LABEL}, but \
                 the key file {} exists. Open Settings > Security and choose \"Store the key in a \
                 local file\" to use it.",
                if *secrets == 1 { "secret" } else { "secrets" },
                stray.display(),
            ),
            Self::MasterKeyMissing { secrets, key_file: None, .. } => write!(
                f,
                "The master key of {secrets} saved {} is missing from the {CREDENTIAL_STORE_LABEL} \
                 (or does not match), so they cannot be read.{} Use {RESET_ACTION} (command \
                 palette) and enter the passwords again{}.",
                if *secrets == 1 { "secret" } else { "secrets" },
                // Where several keyrings exist, the key may be in the other one: not a reset yet.
                if crate::key_home::FALLBACK_SUPPORTED {
                    " If you use another keyring or desktop session (for example GNOME and KDE), \
                     open tablex there first: the key is most likely in that one."
                } else {
                    ""
                },
                if crate::key_home::FALLBACK_SUPPORTED { " only as the last resort" } else { "" },
            ),
            // The key is meant to be in a file: the passwords may well be fine, so the reset is
            // only the last resort.
            Self::MasterKeyMissing { secrets, key_file: Some(key_file), .. } => write!(
                f,
                "The master key of {secrets} saved {} is missing from {} (or does not match), so \
                 they cannot be read. If the key is still in the system keyring, open Settings > \
                 Security and choose \"Move the key to the system keyring\"; otherwise put the key \
                 file back, or, if it is gone for good, use {RESET_ACTION} (command palette) as the \
                 last resort and enter the passwords again.",
                if *secrets == 1 { "secret" } else { "secrets" },
                key_file.display(),
            ),
            Self::KeyUnreachable { secrets } => write!(
                f,
                "The key of {secrets} saved {} is most likely in a system keyring that is not \
                 running now. Start or unlock it (or run tablex in the desktop session) and \
                 restart tablex; nothing was changed.",
                if *secrets == 1 { "secret" } else { "secrets" },
            ),
            // Short: also shown by `--authorize-keychain`, which is what the user just ran.
            Self::InteractionNotAllowed => f.write_str(
                "macOS would have to ask before it hands the item over (or the login keychain is locked)",
            ),
            Self::UserRefused => f.write_str("access to the item was refused"),
            Self::Platform(message) => f.write_str(message),
        }
    }
}

impl std::error::Error for CredentialError {}

/// The Security.framework result code behind a platform error, when it is one.
#[cfg(target_os = "macos")]
fn os_status(source: &(dyn std::error::Error + 'static)) -> Option<i32> {
    source
        .downcast_ref::<security_framework::base::Error>()
        .map(|error| error.code())
}

#[cfg(not(target_os = "macos"))]
fn os_status(_: &(dyn std::error::Error + 'static)) -> Option<i32> {
    None
}

/// `errSecInteractionNotAllowed`: asking is switched off (or the login keychain is locked).
const INTERACTION_NOT_ALLOWED: i32 = -25308;
/// `errSecUserCanceled` and `errSecAuthFailed`: the answer to the dialog was Cancel / Deny.
const USER_REFUSED: [i32; 2] = [-128, -25293];

/// The typed error of a Security.framework result code (`None`: not one of the typed ones).
fn typed_status(code: i32) -> Option<CredentialError> {
    match code {
        INTERACTION_NOT_ALLOWED => Some(CredentialError::InteractionNotAllowed),
        code if USER_REFUSED.contains(&code) => Some(CredentialError::UserRefused),
        _ => None,
    }
}

impl From<keyring::Error> for CredentialError {
    fn from(error: keyring::Error) -> Self {
        if let keyring::Error::PlatformFailure(source) | keyring::Error::NoStorageAccess(source) = &error {
            if let Some(typed) = os_status(&**source).and_then(typed_status) {
                return typed;
            }
        }
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

    /// Whether a secret is stored, without reading it where the store can tell: the macOS Keychain
    /// asks the user before it hands out an item's data, never for its attributes.
    fn contains(&self, kind: SecretKind, account: &str) -> Result<bool, CredentialError> {
        Ok(self.load(kind, account)?.is_some())
    }

    /// Removes every saved secret of the store so that new ones can be saved ([`RESET_ACTION`]),
    /// the secrets that earlier versions kept as items of the OS store of their own included. The
    /// answer is what could not be removed from the OS store (best effort: empty when all went),
    /// for the front end to say; an error means nothing was reset. Only the encrypted store of the
    /// app database has a table to clear; the others cannot.
    fn reset(&self) -> Result<Vec<String>, CredentialError> {
        Err(CredentialError::Platform(
            "this credential store cannot be reset".into(),
        ))
    }

    /// Gets the store ready for reads: the encrypted store reads its master key (the one item the
    /// OS asks about). Nothing to do for a store without one.
    fn unlock(&self) -> Result<(), CredentialError> {
        Ok(())
    }

    /// Whether the store can be used at all: `false` when none runs (a Linux session without a
    /// Secret Service). Not a read of any secret.
    fn available(&self) -> bool {
        true
    }

    /// The file that holds the master key, when the key lives in one
    /// ([`crate::key_home::Home::File`]); `None` for every store that keeps it elsewhere.
    fn key_file(&self) -> Option<std::path::PathBuf> {
        None
    }

    /// A key file that exists although the key is meant to be elsewhere (see
    /// [`CredentialError::MasterKeyMissing`]); `None` for every store but the key homes.
    fn stray_key_file(&self) -> Option<std::path::PathBuf> {
        None
    }

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
            .filter_map(|(kind, account)| self.delete(kind, &account).err().map(|error| (kind, error)))
            .collect();
        // One cause that needs the user stays typed: the caller says what to do about it.
        if !failures.is_empty() && failures.iter().all(|(_, error)| *error == CredentialError::InteractionNotAllowed) {
            return Err(CredentialError::InteractionNotAllowed);
        }
        if failures.is_empty() {
            Ok(())
        } else {
            let texts: Vec<_> = failures.iter().map(|(kind, error)| format!("{kind:?}: {error}")).collect();
            Err(CredentialError::Platform(texts.join("; ")))
        }
    }
}

/// A store shared behind an `Arc` (the app's) is a store.
impl<T: CredentialStore + ?Sized> CredentialStore for std::sync::Arc<T> {
    fn load(&self, kind: SecretKind, account: &str) -> Result<Option<String>, CredentialError> {
        (**self).load(kind, account)
    }
    fn write(&self, kind: SecretKind, account: &str, password: &str) -> Result<(), CredentialError> {
        (**self).write(kind, account, password)
    }
    fn delete(&self, kind: SecretKind, account: &str) -> Result<(), CredentialError> {
        (**self).delete(kind, account)
    }
    fn contains(&self, kind: SecretKind, account: &str) -> Result<bool, CredentialError> {
        (**self).contains(kind, account)
    }
    fn unlock(&self) -> Result<(), CredentialError> {
        (**self).unlock()
    }
    fn reset(&self) -> Result<Vec<String>, CredentialError> {
        (**self).reset()
    }
    fn available(&self) -> bool {
        (**self).available()
    }
    fn key_file(&self) -> Option<std::path::PathBuf> {
        (**self).key_file()
    }
    fn stray_key_file(&self) -> Option<std::path::PathBuf> {
        (**self).stray_key_file()
    }
}

/// Where the master key lives: the OS credential store, or on Linux a file or nowhere when the
/// user chose so ([`crate::key_home`]). One per process, shared by [`default_store`] and the
/// front end's choice dialog and settings.
pub fn key_store() -> std::sync::Arc<crate::key_home::KeyHomeStore<SystemStore>> {
    static STORE: std::sync::OnceLock<std::sync::Arc<crate::key_home::KeyHomeStore<SystemStore>>> =
        std::sync::OnceLock::new();
    std::sync::Arc::clone(STORE.get_or_init(|| {
        std::sync::Arc::new(crate::key_home::KeyHomeStore::new(
            SystemStore::new(),
            engine_storage::app_db::default_path().parent().map(std::path::Path::to_owned),
            crate::key_home::FALLBACK_SUPPORTED,
        ))
    }))
}

/// Makes the process-wide "passwords saved" switch ([`crate::key_home::set_passwords_saved`]) follow
/// the saved choice: after "Don't save passwords" every connection asks for its password. Every
/// front end (the app, `tablex-mcp`) calls it at start, before it reads the saved connections.
pub fn apply_saved_choice() {
    crate::key_home::set_passwords_saved(key_store().saves_passwords());
}

/// The store every front end shares: the encrypted secrets table of the app database (`tablex.db` in the app data directory) with
/// its master key in the OS credential store (or where [`key_store`] says). One per process, so the key is read once.
pub fn default_store() -> std::sync::Arc<crate::secret_store::EncryptedStore> {
    static STORE: std::sync::OnceLock<std::sync::Arc<crate::secret_store::EncryptedStore>> =
        std::sync::OnceLock::new();
    std::sync::Arc::clone(STORE.get_or_init(|| {
        std::sync::Arc::new(crate::secret_store::EncryptedStore::new(
            Box::new(key_store()),
            Some(engine_storage::app_db::default_path()),
        ))
    }))
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
            Err(keyring::Error::NoEntry) => Ok(None),
            // A damaged master key item must not read as "no key": a new one would orphan the secrets.
            Err(keyring::Error::BadEncoding(_)) if kind != SecretKind::MasterKey => Ok(None),
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

    /// The item's attributes only: reading its data would make macOS ask the user (again) when this
    /// build is not on the item's access list, as with an item an earlier version saved.
    #[cfg(target_os = "macos")]
    fn contains(&self, kind: SecretKind, account: &str) -> Result<bool, CredentialError> {
        use security_framework::item::{ItemClass, ItemSearchOptions, Limit};
        /// `errSecItemNotFound`.
        const NOT_FOUND: i32 = -25300;
        let service = self.service_override.as_deref().unwrap_or(kind.service());
        let found = ItemSearchOptions::new()
            .class(ItemClass::generic_password())
            .service(service)
            .account(account)
            .load_attributes(true)
            .limit(Limit::Max(1))
            .search();
        match found {
            Ok(items) => Ok(!items.is_empty()),
            Err(error) if error.code() == NOT_FOUND => Ok(false),
            Err(error) => Err(typed_status(error.code()).unwrap_or_else(|| CredentialError::Platform(error.to_string()))),
        }
    }

    /// Whether a Secret Service (Linux), the Keychain or the Credential Manager is there: the
    /// store was initialised (once per process: a provider started later needs a restart).
    fn available(&self) -> bool {
        keyring::Entry::store_status().is_ok()
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

        fn available(&self) -> bool {
            !self.unavailable
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
    fn an_interaction_not_allowed_keychain_error_is_typed_and_names_the_fix() {
        // Only error values are built: the Keychain is not touched.
        #[cfg(target_os = "macos")]
        {
            let status = |code| {
                keyring::Error::PlatformFailure(Box::new(security_framework::base::Error::from_code(code)))
            };
            assert_eq!(CredentialError::from(status(-25308)), CredentialError::InteractionNotAllowed);
            // Cancel and Deny are the user's answer, not a failure of the store.
            assert_eq!(CredentialError::from(status(-128)), CredentialError::UserRefused);
            assert_eq!(CredentialError::from(status(-25293)), CredentialError::UserRefused);
            assert!(matches!(CredentialError::from(status(-25299)), CredentialError::Platform(_)));
        }
        // What an agent is told: the way to authorise, the locked keychain, the command given.
        let text = keychain_access_needed(None);
        assert!(text.contains("Allow Keychain access") && text.contains("login keychain is locked"), "{text}");
        assert!(text.contains("`tablex-mcp --authorize-keychain`"), "{text}");
        assert!(keychain_access_needed(Some("'/x y/tablex-mcp' --authorize-keychain"))
            .contains("run '/x y/tablex-mcp' --authorize-keychain in a terminal"));
        // The error's own text is short: `--authorize-keychain` prints it to the user who just ran it.
        let short = CredentialError::InteractionNotAllowed.to_string();
        assert!(!short.contains("Driver Manager") && short.contains("login keychain is locked"), "{short}");
    }

    #[test]
    fn delete_all_keeps_an_interaction_error_typed_and_flattens_mixed_failures() {
        struct Asks(bool);
        impl CredentialStore for Asks {
            fn load(&self, _: SecretKind, _: &str) -> Result<Option<String>, CredentialError> {
                Ok(None)
            }
            fn write(&self, _: SecretKind, _: &str, _: &str) -> Result<(), CredentialError> {
                Ok(())
            }
            fn delete(&self, kind: SecretKind, _: &str) -> Result<(), CredentialError> {
                if self.0 && kind == SecretKind::Proxy {
                    Err(CredentialError::Platform("keychain locked".into()))
                } else {
                    Err(CredentialError::InteractionNotAllowed)
                }
            }
        }
        assert_eq!(Asks(false).delete_all("id"), Err(CredentialError::InteractionNotAllowed));
        let mixed = Asks(true).delete_all("id").unwrap_err();
        assert!(matches!(&mixed, CredentialError::Platform(text) if text.contains("Proxy: keychain locked")), "{mixed}");
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

    /// Real Credential Manager / Secret Service round trip against a unique service; run with
    /// `cargo test -p engine-connect -- --ignored real_store_round_trip`. The item is deleted at the end.
    #[cfg(not(target_os = "macos"))]
    #[test]
    #[ignore = "touches the real credential store"]
    fn real_store_round_trip() {
        let store = SystemStore {
            service_override: Some(format!("tablex.test.{}", uuid::Uuid::new_v4())),
        };
        assert!(store.available(), "a credential store runs");
        let account = "ROUND-TRIP-ACCOUNT";
        assert_eq!(store.load(SecretKind::Database, account).unwrap(), None);
        store.save(SecretKind::Database, account, "p@ss wörd").unwrap();
        assert_eq!(store.load(SecretKind::Database, account).unwrap().as_deref(), Some("p@ss wörd"));
        store.save(SecretKind::Database, account, "second").unwrap();
        assert_eq!(store.load(SecretKind::Database, account).unwrap().as_deref(), Some("second"));
        store.delete(SecretKind::Database, account).unwrap();
        assert_eq!(store.load(SecretKind::Database, account).unwrap(), None);
        store.delete(SecretKind::Database, account).unwrap();
    }

    /// Where no Secret Service runs (no session bus, or a bus with nobody owning
    /// `org.freedesktop.secrets`): `cargo test -p engine-connect -- --ignored real_store_without_service`
    /// with `DBUS_SESSION_BUS_ADDRESS` pointing at such a bus.
    #[cfg(all(unix, not(target_os = "macos")))]
    #[test]
    #[ignore = "needs a session with no Secret Service"]
    fn real_store_without_service_is_no_keyring_not_another_error() {
        let store = SystemStore {
            service_override: Some(format!("tablex.test.{}", uuid::Uuid::new_v4())),
        };
        assert!(!store.available());
        assert_eq!(store.load(SecretKind::Database, "a"), Err(CredentialError::NoKeyring));
        assert_eq!(store.save(SecretKind::Database, "a", "pw"), Err(CredentialError::NoKeyring));
        store.delete(SecretKind::Database, "a").unwrap();
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
