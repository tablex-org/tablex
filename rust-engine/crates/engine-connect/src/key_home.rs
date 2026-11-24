//! Where the 32-byte master key of the encrypted secrets table lives ([`crate::secret_store`]).
//!
//! * [`Home::Keyring`]: the OS credential store (macOS Keychain, Windows Credential Manager, Linux
//!   Secret Service). The default, and the only one on macOS and Windows.
//! * [`Home::File`] (Linux and other Unix without a Keychain only): `<app data dir>/master.key`,
//!   the same base64 text as the OS item, written atomically with mode 0600 in a 0700 directory.
//!   Anyone who can read the user's files can decrypt the saved passwords: it is the choice for
//!   a session with no Secret Service (a tiling window manager, SSH, WSL).
//! * [`Home::NoSave`]: nothing is saved: the store answers [`CredentialError::NoKeyring`] and
//!   connections ask for their password when they connect.
//!
//! The choice is the file `<app data dir>/key-home` holding `file` or `none`; no file is
//! [`Home::Keyring`], so an existing user sees no change. Only the key moves between homes, the
//! `secrets` table stays where it is; a switch never makes a new key (the table rule: a key is made
//! only by a write that finds no secrets) and never deletes the old home before the new one holds
//! the key.
//!
//! The rule that keeps a key from being stranded: the key of existing secrets may be in a keyring
//! that merely is not running now (v0.1.10 kept it there; a GNOME user may start tablex once over
//! SSH or in i3). While the `secrets` table has rows and the key is not readable, a switch never
//! makes [`Home::File`] or [`Home::NoSave`] the permanent choice: the choice file would hide the
//! real key, and every saved password would fail even back in the desktop session
//! ([`CredentialError::KeyUnreachable`], or `MasterKeyMissing` when a keyring runs and has no key).
//! [`KeyHomeStore::need`] tells the front end which case it is in; for the unreachable one it
//! offers only "no passwords this session" ([`set_passwords_saved`], not persisted). Both choices
//! stay open when there are no secrets yet (nothing to strand) or the key is readable, and
//! [`Home::NoSave`] also when the key is in the file (it stays there).
//!
//! [`KeyHomeStore`] is the [`CredentialStore`] the encrypted store gets as its "OS store": the
//! master key goes to the current home, every other item (the legacy per-secret items) to the OS
//! store. `tablex-mcp` builds the same store, so it reads what the app saved.

use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

use crate::credentials::{CredentialError, CredentialStore, SecretKind, MASTER_KEY_ACCOUNT, RESET_ACTION};

/// The file that records the choice, in the app data directory.
pub const CHOICE_FILE: &str = "key-home";
/// The master key file of [`Home::File`], in the app data directory.
pub const KEY_FILE: &str = "master.key";

/// Whether this platform can keep the key anywhere but the OS store: Linux and the other Unix
/// systems, where a Secret Service may simply not run. Never macOS (the Keychain is always there)
/// nor Windows (so is the Credential Manager).
pub const FALLBACK_SUPPORTED: bool = cfg!(all(unix, not(target_os = "macos")));

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Home {
    Keyring,
    File,
    NoSave,
}

impl Home {
    /// What [`CHOICE_FILE`] holds (`None`: no file, the default).
    fn marker(self) -> Option<&'static str> {
        match self {
            Self::Keyring => None,
            Self::File => Some("file"),
            Self::NoSave => Some("none"),
        }
    }
}

fn error(message: impl Into<String>) -> CredentialError {
    CredentialError::Platform(message.into())
}

/// What the front end has to ask at the start (see [`KeyHomeStore::need`]).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Need {
    /// Nothing: a keyring runs, a choice was made, or the platform has no choice.
    Nothing,
    /// No keyring, no choice, no secrets yet: where to keep the key.
    Choice,
    /// No keyring, no choice, `secrets` saved secrets: their key is most likely in a keyring that
    /// is not running now. No choice may be made for good.
    KeyUnreachable { secrets: usize },
}

/// Whether the app saves passwords at all this run: `false` after [`Home::NoSave`] was chosen.
/// Connections then behave as ones that save no password (they ask when connecting). Set by the
/// front end from the choice; the default is `true`.
static PASSWORDS_SAVED: AtomicBool = AtomicBool::new(true);

pub fn passwords_saved() -> bool {
    PASSWORDS_SAVED.load(Ordering::Relaxed)
}

pub fn set_passwords_saved(saved: bool) {
    PASSWORDS_SAVED.store(saved, Ordering::Relaxed);
}

pub struct KeyHomeStore<S> {
    os: S,
    dir: Option<PathBuf>,
    fallback: bool,
}

impl<S: CredentialStore> KeyHomeStore<S> {
    /// `os` is the OS credential store; `dir` the app data directory (`None`: there is none, so
    /// the file home cannot be used); `fallback` whether homes other than the OS store exist (see
    /// [`FALLBACK_SUPPORTED`]; with `false` the choice file is not even looked at).
    pub fn new(os: S, dir: Option<PathBuf>, fallback: bool) -> Self {
        Self { os, dir, fallback }
    }

    fn dir(&self) -> Result<&Path, CredentialError> {
        self.dir
            .as_deref()
            .ok_or_else(|| error("there is no application data directory"))
    }

    /// Whether the saved choice is to save passwords at all (`false` only after
    /// [`Home::NoSave`]; an unreadable choice file counts as off too, as the store refuses the key
    /// then).
    pub fn saves_passwords(&self) -> bool {
        self.home().is_ok_and(|home| home != Home::NoSave)
    }

    /// Whether the user can choose where the key lives on this platform.
    pub fn can_choose(&self) -> bool {
        self.fallback
    }

    /// Whether the OS credential store is usable (a Secret Service is running).
    pub fn keyring_available(&self) -> bool {
        self.os.available()
    }

    /// Where the key lives now. An unreadable or unknown choice file is an error: guessing would
    /// risk a second key.
    pub fn home(&self) -> Result<Home, CredentialError> {
        let Some(dir) = self.dir.as_deref().filter(|_| self.fallback) else {
            return Ok(Home::Keyring);
        };
        let path = dir.join(CHOICE_FILE);
        match std::fs::read_to_string(&path) {
            Ok(text) => match text.trim() {
                "file" => Ok(Home::File),
                "none" => Ok(Home::NoSave),
                _ => Err(error(format!(
                    "{} is not a tablex choice (it should say file or none); remove it to be asked again",
                    path.display()
                ))),
            },
            Err(source) if source.kind() == std::io::ErrorKind::NotFound => Ok(Home::Keyring),
            Err(source) => Err(error(format!("cannot read {}: {source}", path.display()))),
        }
    }

    /// The user has to choose: this platform has alternatives, nothing was chosen, and no Secret
    /// Service runs. Initialises the OS store (a D-Bus connection on Linux): call it off the UI
    /// thread.
    pub fn needs_choice(&self) -> bool {
        self.need() == Need::Choice
    }

    /// What to ask at the start; same cost as [`Self::needs_choice`] plus a count of the secrets.
    /// A table that cannot be read counts as empty here: a switch reads it again and says why not.
    pub fn need(&self) -> Need {
        if !(self.fallback && matches!(self.home(), Ok(Home::Keyring)) && !self.os.available()) {
            return Need::Nothing;
        }
        match self.secrets() {
            // A key file left next to a keyring choice (the choice file was deleted by hand) is
            // where the key is: the dialog that offers the file is the right one.
            Ok(secrets) if secrets > 0 && !self.key_path().is_some_and(|path| path.is_file()) => {
                Need::KeyUnreachable { secrets }
            }
            _ => Need::Choice,
        }
    }

    /// The secrets in the app database next to the choice file.
    fn secrets(&self) -> Result<usize, CredentialError> {
        crate::secret_store::saved_secrets(&self.dir()?.join(engine_storage::app_db::FILE_NAME))
    }

    /// Where [`Home::File`] keeps the key (`None`: no data directory).
    pub fn key_path(&self) -> Option<PathBuf> {
        self.dir.as_deref().map(|dir| dir.join(KEY_FILE))
    }

    fn key_file(&self) -> Result<PathBuf, CredentialError> {
        Ok(self.dir()?.join(KEY_FILE))
    }

    fn read_file(&self) -> Result<Option<String>, CredentialError> {
        let path = self.key_file()?;
        match std::fs::read_to_string(&path) {
            Ok(text) => Ok(Some(text.trim().to_owned()).filter(|text| !text.is_empty())),
            Err(source) if source.kind() == std::io::ErrorKind::NotFound => Ok(None),
            Err(source) => Err(error(format!("cannot read {}: {source}", path.display()))),
        }
    }

    fn write_file(&self, text: &str) -> Result<(), CredentialError> {
        write_private(self.dir()?, KEY_FILE, text)
    }

    /// The key in the OS store: `None` also when no Secret Service runs.
    fn read_os(&self) -> Result<Option<String>, CredentialError> {
        match self.os.load(SecretKind::MasterKey, MASTER_KEY_ACCOUNT) {
            Err(CredentialError::NoKeyring) => Ok(None),
            other => other,
        }
    }

    /// The one key both homes may hold. When they differ (a copy that could not be removed went
    /// stale, e.g. a Reset in the other home), the one that opens the table is the key: the other
    /// is replaced by the switch. With nothing sealed yet either will do (the file's); when
    /// neither opens the table, the switch is refused and the text says how to get out.
    fn the_key(&self, file: Option<&String>, os: Option<&String>) -> Result<Option<String>, CredentialError> {
        match (file, os) {
            (Some(file), Some(os)) if file != os => {
                let path = self.dir()?.join(engine_storage::app_db::FILE_NAME);
                let opens = |key: &String| crate::secret_store::key_opens_table(&path, key);
                if opens(file)? != Some(false) {
                    return Ok(Some(file.clone()));
                }
                if opens(os)? == Some(true) {
                    return Ok(Some(os.clone()));
                }
                Err(error(format!(
                    "the master key in {KEY_FILE} and the one in the {label} differ and neither opens the \
                     saved passwords, so the choice stays as it is. Delete the item named \"{service}\" \
                     (account \"{MASTER_KEY_ACCOUNT}\") from the {label}, then use {RESET_ACTION} \
                     (command palette) and enter the passwords again",
                    label = crate::platform::CREDENTIAL_STORE_LABEL,
                    service = SecretKind::MasterKey.service(),
                )))
            }
            (Some(key), _) | (None, Some(key)) => Ok(Some(key.clone())),
            (None, None) => Ok(None),
        }
    }

    /// Moves the master key to `target` and records the choice. The new home holds the key (read
    /// back) before the choice is written, and the old home is cleared only after that, so an
    /// interruption leaves a key in at least one place. A key that does not exist yet is not made
    /// (see the module docs).
    pub fn switch(&self, target: Home) -> Result<(), CredentialError> {
        if !self.fallback {
            return Err(error("the master key can only live in the system keyring on this platform"));
        }
        let current = self.home()?;
        match target {
            Home::NoSave => {
                // The key may sit in a keyring that is not running: say so, don't hide it.
                if current == Home::Keyring && !self.os.available() {
                    match self.secrets()? {
                        0 => {}
                        secrets => return Err(CredentialError::KeyUnreachable { secrets }),
                    }
                }
                self.set_choice(target)
            }
            Home::File => {
                let (file, os) = (self.read_file()?, self.read_os()?);
                let key = self.the_key(file.as_ref(), os.as_ref())?;
                if key.is_none() {
                    match self.secrets()? {
                        0 => {}
                        secrets if !self.os.available() => return Err(CredentialError::KeyUnreachable { secrets }),
                        secrets => {
                            return Err(CredentialError::MasterKeyMissing {
                                secrets,
                                file: self.dir()?.join(engine_storage::app_db::FILE_NAME),
                                key_file: None,
                                stray_key_file: None,
                            })
                        }
                    }
                }
                if let Some(key) = &key {
                    if file.as_ref() != Some(key) {
                        self.write_file(key)?;
                        if self.read_file()?.as_ref() != Some(key) {
                            return Err(error(format!("{KEY_FILE} could not be read back")));
                        }
                    }
                }
                self.set_choice(target)?;
                if os.is_some() {
                    // The key is in the file now; a copy left in the OS store is not a reason to fail.
                    let _ = self.os.delete(SecretKind::MasterKey, MASTER_KEY_ACCOUNT);
                }
                Ok(())
            }
            Home::Keyring => {
                if !self.os.available() {
                    return Err(CredentialError::NoKeyring);
                }
                let (file, os) = (self.read_file()?, self.read_os()?);
                let key = self.the_key(file.as_ref(), os.as_ref())?;
                if let Some(key) = &key {
                    if os.as_ref() != Some(key) {
                        self.os.write(SecretKind::MasterKey, MASTER_KEY_ACCOUNT, key)?;
                        if self.read_os()?.as_ref() != Some(key) {
                            return Err(error("the master key could not be read back from the system keyring"));
                        }
                    }
                }
                self.set_choice(target)?;
                if file.is_some() {
                    if let Ok(path) = self.key_file() {
                        let _ = std::fs::remove_file(path);
                    }
                }
                Ok(())
            }
        }
    }

    /// Records `home` as the choice: the file is written, or removed for the default.
    fn set_choice(&self, home: Home) -> Result<(), CredentialError> {
        let dir = self.dir()?;
        match home.marker() {
            Some(marker) => write_private(dir, CHOICE_FILE, marker),
            None => match std::fs::remove_file(dir.join(CHOICE_FILE)) {
                Ok(()) => Ok(()),
                Err(source) if source.kind() == std::io::ErrorKind::NotFound => Ok(()),
                Err(source) => Err(error(format!("cannot remove {CHOICE_FILE}: {source}"))),
            },
        }
    }
}

/// `dir/name` with `text`, atomically (a temporary file in the same directory, flushed, then
/// renamed over) and private: the directory 0700, the file 0600 from its creation on.
fn write_private(dir: &Path, name: &str, text: &str) -> Result<(), CredentialError> {
    let at = |what: &str, path: &Path, source: std::io::Error| {
        error(format!("cannot {what} {}: {source}", path.display()))
    };
    {
        let mut builder = std::fs::DirBuilder::new();
        builder.recursive(true);
        #[cfg(unix)]
        std::os::unix::fs::DirBuilderExt::mode(&mut builder, 0o700);
        builder.create(dir).map_err(|source| at("create", dir, source))?;
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let meta = std::fs::metadata(dir).map_err(|source| at("inspect", dir, source))?;
        if meta.permissions().mode() & 0o077 != 0 {
            std::fs::set_permissions(dir, std::fs::Permissions::from_mode(0o700))
                .map_err(|source| at("make private", dir, source))?;
        }
    }
    static TMP_COUNTER: AtomicUsize = AtomicUsize::new(0);
    let tmp = dir.join(format!(
        ".{name}.{}-{}.tmp",
        std::process::id(),
        TMP_COUNTER.fetch_add(1, Ordering::Relaxed)
    ));
    let _ = std::fs::remove_file(&tmp);
    let written = (|| {
        let mut options = std::fs::OpenOptions::new();
        options.write(true).create_new(true);
        #[cfg(unix)]
        std::os::unix::fs::OpenOptionsExt::mode(&mut options, 0o600);
        let mut file = options.open(&tmp)?;
        file.write_all(text.as_bytes())?;
        file.sync_all()?;
        std::fs::rename(&tmp, dir.join(name))
    })();
    if let Err(source) = written {
        let _ = std::fs::remove_file(&tmp);
        return Err(at("write", &dir.join(name), source));
    }
    #[cfg(unix)]
    if let Ok(handle) = std::fs::File::open(dir) {
        let _ = handle.sync_all();
    }
    Ok(())
}

/// The file and the choice hold the one master key: no other account lives there.
fn check_master_key_account(account: &str) -> Result<(), CredentialError> {
    if account == MASTER_KEY_ACCOUNT {
        Ok(())
    } else {
        Err(error(format!("there is one master key, not {account:?}")))
    }
}

impl<S: CredentialStore> CredentialStore for KeyHomeStore<S> {
    fn load(&self, kind: SecretKind, account: &str) -> Result<Option<String>, CredentialError> {
        if kind != SecretKind::MasterKey {
            return self.os.load(kind, account);
        }
        check_master_key_account(account)?;
        match self.home()? {
            Home::Keyring => self.os.load(kind, account),
            Home::File => self.read_file(),
            Home::NoSave => Err(CredentialError::NoKeyring),
        }
    }

    fn write(&self, kind: SecretKind, account: &str, password: &str) -> Result<(), CredentialError> {
        if kind != SecretKind::MasterKey {
            return self.os.write(kind, account, password);
        }
        check_master_key_account(account)?;
        match self.home()? {
            Home::Keyring => self.os.write(kind, account, password),
            Home::File => self.write_file(password),
            Home::NoSave => Err(CredentialError::NoKeyring),
        }
    }

    fn delete(&self, kind: SecretKind, account: &str) -> Result<(), CredentialError> {
        if kind != SecretKind::MasterKey {
            return self.os.delete(kind, account);
        }
        check_master_key_account(account)?;
        match self.home()? {
            Home::Keyring => self.os.delete(kind, account),
            Home::File => match std::fs::remove_file(self.key_file()?) {
                Ok(()) => Ok(()),
                Err(source) if source.kind() == std::io::ErrorKind::NotFound => Ok(()),
                Err(source) => Err(error(format!("cannot remove {KEY_FILE}: {source}"))),
            },
            Home::NoSave => Ok(()),
        }
    }

    fn contains(&self, kind: SecretKind, account: &str) -> Result<bool, CredentialError> {
        if kind != SecretKind::MasterKey {
            return self.os.contains(kind, account);
        }
        Ok(self.load(kind, account)?.is_some())
    }

    fn available(&self) -> bool {
        self.os.available()
    }

    fn key_file(&self) -> Option<PathBuf> {
        matches!(self.home(), Ok(Home::File)).then(|| self.key_path()).flatten()
    }

    fn stray_key_file(&self) -> Option<PathBuf> {
        let path = self.key_path()?;
        (matches!(self.home(), Ok(Home::Keyring)) && self.fallback && path.is_file()).then_some(path)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::secret_store::EncryptedStore;
    use std::collections::HashMap;
    use std::sync::{Arc, Mutex};

    /// The OS store: items in memory, and a switch for "no Secret Service runs".
    #[derive(Default)]
    struct Os {
        items: Mutex<HashMap<(SecretKind, String), String>>,
        down: AtomicBool,
        /// A locked collection: reads fail with something other than "no Secret Service".
        locked: AtomicBool,
    }

    impl Os {
        fn has_key(&self) -> bool {
            self.items.lock().unwrap().contains_key(&(SecretKind::MasterKey, MASTER_KEY_ACCOUNT.to_owned()))
        }
    }

    impl CredentialStore for Os {
        fn load(&self, kind: SecretKind, account: &str) -> Result<Option<String>, CredentialError> {
            if self.down.load(Ordering::SeqCst) {
                return Err(CredentialError::NoKeyring);
            }
            if self.locked.load(Ordering::SeqCst) {
                return Err(CredentialError::Platform("collection is locked".into()));
            }
            Ok(self.items.lock().unwrap().get(&(kind, account.to_owned())).cloned())
        }
        fn write(&self, kind: SecretKind, account: &str, password: &str) -> Result<(), CredentialError> {
            if self.down.load(Ordering::SeqCst) {
                return Err(CredentialError::NoKeyring);
            }
            self.items.lock().unwrap().insert((kind, account.to_owned()), password.to_owned());
            Ok(())
        }
        fn delete(&self, kind: SecretKind, account: &str) -> Result<(), CredentialError> {
            if self.down.load(Ordering::SeqCst) {
                return Err(CredentialError::NoKeyring);
            }
            self.items.lock().unwrap().remove(&(kind, account.to_owned()));
            Ok(())
        }
        fn available(&self) -> bool {
            !self.down.load(Ordering::SeqCst)
        }
    }

    struct Fixture {
        dir: tempfile::TempDir,
        os: Arc<Os>,
    }

    impl Fixture {
        fn new() -> Self {
            Self { dir: tempfile::tempdir().unwrap(), os: Arc::new(Os::default()) }
        }
        fn data_dir(&self) -> PathBuf {
            self.dir.path().join("tablex")
        }
        fn homes(&self) -> Arc<KeyHomeStore<Arc<Os>>> {
            Arc::new(KeyHomeStore::new(Arc::clone(&self.os), Some(self.data_dir()), true))
        }
        fn secrets(&self, homes: &Arc<KeyHomeStore<Arc<Os>>>) -> EncryptedStore {
            EncryptedStore::new(
                Box::new(Arc::clone(homes)),
                Some(self.data_dir().join(engine_storage::app_db::FILE_NAME)),
            )
        }
    }

    const DB: SecretKind = SecretKind::Database;
    const KEY: SecretKind = SecretKind::MasterKey;

    #[test]
    fn a_missing_secret_service_is_told_apart_from_a_locked_or_failing_one() {
        let fx = Fixture::new();
        let homes = fx.homes();
        assert!(!homes.needs_choice(), "a Secret Service runs");
        // Locked or denied is an error and no reason to ask for another home.
        fx.os.locked.store(true, Ordering::SeqCst);
        assert!(!homes.needs_choice());
        assert!(matches!(homes.load(KEY, MASTER_KEY_ACCOUNT), Err(CredentialError::Platform(_))));
        fx.os.locked.store(false, Ordering::SeqCst);
        fx.os.down.store(true, Ordering::SeqCst);
        assert!(homes.needs_choice());
        assert_eq!(homes.load(KEY, MASTER_KEY_ACCOUNT), Err(CredentialError::NoKeyring));
        assert_eq!(homes.write(KEY, MASTER_KEY_ACCOUNT, "k"), Err(CredentialError::NoKeyring));
    }

    #[test]
    fn the_choice_is_asked_only_where_homes_other_than_the_os_store_exist_and_only_once() {
        let fx = Fixture::new();
        fx.os.down.store(true, Ordering::SeqCst);
        // macOS and Windows: no fallback, no question, the choice file is not even read.
        let plain = KeyHomeStore::new(Arc::clone(&fx.os), Some(fx.data_dir()), false);
        assert!(!plain.needs_choice());
        assert_eq!(plain.switch(Home::File), Err(error("the master key can only live in the system keyring on this platform")));
        fs_write(&fx.data_dir().join(CHOICE_FILE), "none");
        assert_eq!(plain.home().unwrap(), Home::Keyring);
        std::fs::remove_file(fx.data_dir().join(CHOICE_FILE)).unwrap();

        let homes = fx.homes();
        assert!(homes.needs_choice());
        homes.switch(Home::File).unwrap();
        assert!(!homes.needs_choice(), "asked once");
        assert!(!fx.homes().needs_choice(), "and a new process does not ask again");
        homes.switch(Home::NoSave).unwrap();
        assert!(!fx.homes().needs_choice());
    }

    fn fs_write(path: &Path, text: &str) {
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, text).unwrap();
    }

    #[test]
    fn a_damaged_choice_file_is_an_error_not_a_guess() {
        let fx = Fixture::new();
        fs_write(&fx.data_dir().join(CHOICE_FILE), "banana");
        let homes = fx.homes();
        assert!(homes.home().is_err());
        assert!(homes.load(KEY, MASTER_KEY_ACCOUNT).is_err());
        assert!(homes.write(KEY, MASTER_KEY_ACCOUNT, "k").is_err());
        assert!(!homes.needs_choice());
    }

    #[test]
    fn the_file_home_keeps_the_key_private_and_atomic() {
        let fx = Fixture::new();
        fx.os.down.store(true, Ordering::SeqCst);
        let homes = fx.homes();
        homes.switch(Home::File).unwrap();
        assert_eq!(homes.home().unwrap(), Home::File);
        assert_eq!(homes.load(KEY, MASTER_KEY_ACCOUNT).unwrap(), None);
        homes.write(KEY, MASTER_KEY_ACCOUNT, "a2V5").unwrap();
        assert_eq!(homes.load(KEY, MASTER_KEY_ACCOUNT).unwrap().as_deref(), Some("a2V5"));
        assert_eq!(std::fs::read_to_string(fx.data_dir().join(KEY_FILE)).unwrap(), "a2V5");
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = |path: PathBuf| std::fs::metadata(path).unwrap().permissions().mode() & 0o777;
            assert_eq!(mode(fx.data_dir().join(KEY_FILE)), 0o600);
            assert_eq!(mode(fx.data_dir().join(CHOICE_FILE)), 0o600);
            assert_eq!(mode(fx.data_dir()), 0o700);
        }
        // Rewriting leaves no temporary file behind.
        homes.write(KEY, MASTER_KEY_ACCOUNT, "b3RoZXI=").unwrap();
        let names: Vec<_> = std::fs::read_dir(fx.data_dir()).unwrap().map(|e| e.unwrap().file_name()).collect();
        assert_eq!(names.len(), 2, "{names:?}");
        assert!(!fx.os.has_key(), "the OS store is not touched");
    }

    #[test]
    fn a_directory_that_is_too_open_is_made_private() {
        let fx = Fixture::new();
        fx.os.down.store(true, Ordering::SeqCst);
        std::fs::create_dir_all(fx.data_dir()).unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(fx.data_dir(), std::fs::Permissions::from_mode(0o755)).unwrap();
        }
        fx.homes().switch(Home::File).unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            assert_eq!(std::fs::metadata(fx.data_dir()).unwrap().permissions().mode() & 0o777, 0o700);
        }
    }

    #[test]
    fn no_save_refuses_the_key_and_leaves_what_exists() {
        let fx = Fixture::new();
        let homes = fx.homes();
        fx.secrets(&homes).write(DB, "a", "pw").unwrap();
        assert!(fx.os.has_key());
        homes.switch(Home::NoSave).unwrap();
        assert_eq!(homes.load(KEY, MASTER_KEY_ACCOUNT), Err(CredentialError::NoKeyring));
        assert_eq!(homes.write(KEY, MASTER_KEY_ACCOUNT, "k"), Err(CredentialError::NoKeyring));
        assert!(fx.os.has_key(), "the key is kept: nothing is lost by not saving");
        let store = fx.secrets(&homes);
        assert_eq!(store.write(DB, "b", "x"), Err(CredentialError::NoKeyring));
        assert_eq!(store.load(DB, "a"), Err(CredentialError::NoKeyring));
        // Back to the keyring: the saved password is still there.
        homes.switch(Home::Keyring).unwrap();
        assert_eq!(fx.secrets(&homes).load(DB, "a").unwrap().as_deref(), Some("pw"));
    }

    #[test]
    fn the_keyring_key_moves_to_a_file_and_back_without_losing_a_secret() {
        let fx = Fixture::new();
        let homes = fx.homes();
        fx.secrets(&homes).write(DB, "a", "first").unwrap();
        let key = fx.os.items.lock().unwrap()[&(KEY, MASTER_KEY_ACCOUNT.to_owned())].clone();

        homes.switch(Home::File).unwrap();
        assert!(!fx.os.has_key(), "moved, not copied");
        assert_eq!(std::fs::read_to_string(fx.data_dir().join(KEY_FILE)).unwrap(), key);
        // A new process reads and writes through the file; the OS store may be gone altogether.
        fx.os.down.store(true, Ordering::SeqCst);
        let homes = fx.homes();
        let store = fx.secrets(&homes);
        assert_eq!(store.load(DB, "a").unwrap().as_deref(), Some("first"));
        store.write(DB, "b", "second").unwrap();

        fx.os.down.store(false, Ordering::SeqCst);
        homes.switch(Home::Keyring).unwrap();
        assert!(!fx.data_dir().join(KEY_FILE).exists());
        assert!(!fx.data_dir().join(CHOICE_FILE).exists(), "the default again");
        assert_eq!(fx.os.items.lock().unwrap()[&(KEY, MASTER_KEY_ACCOUNT.to_owned())], key);
        let store = fx.secrets(&fx.homes());
        assert_eq!(store.load(DB, "a").unwrap().as_deref(), Some("first"));
        assert_eq!(store.load(DB, "b").unwrap().as_deref(), Some("second"));
    }

    #[test]
    fn switching_to_the_keyring_needs_one_and_changes_nothing_without() {
        let fx = Fixture::new();
        fx.os.down.store(true, Ordering::SeqCst);
        let homes = fx.homes();
        homes.switch(Home::File).unwrap();
        fx.secrets(&homes).write(DB, "a", "pw").unwrap();
        assert_eq!(homes.switch(Home::Keyring), Err(CredentialError::NoKeyring));
        assert_eq!(homes.home().unwrap(), Home::File);
        assert_eq!(fx.secrets(&fx.homes()).load(DB, "a").unwrap().as_deref(), Some("pw"));
    }

    #[test]
    fn two_different_keys_are_never_merged_and_the_one_that_opens_the_table_wins() {
        let fx = Fixture::new();
        let homes = fx.homes();
        fx.secrets(&homes).write(DB, "a", "pw").unwrap();
        let key = fx.os.items.lock().unwrap()[&(KEY, MASTER_KEY_ACCOUNT.to_owned())].clone();
        // A stale copy in the file (a Reset in the other home): the keyring key opens the table.
        fs_write(&fx.data_dir().join(KEY_FILE), "b3RoZXI=");
        homes.switch(Home::File).unwrap();
        assert_eq!(std::fs::read_to_string(fx.data_dir().join(KEY_FILE)).unwrap(), key, "the stale copy is replaced");
        assert_eq!(fx.secrets(&fx.homes()).load(DB, "a").unwrap().as_deref(), Some("pw"));
    }

    #[test]
    fn a_stale_keyring_copy_after_a_reset_in_the_file_home_does_not_block_the_way_back() {
        let fx = Fixture::new();
        let homes = fx.homes();
        fx.secrets(&homes).write(DB, "a", "old").unwrap();
        homes.switch(Home::File).unwrap();
        // The keyring item could not be removed at the switch: it is still there, stale.
        let stale = "AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA=".to_owned();
        fx.os.items.lock().unwrap().insert((KEY, MASTER_KEY_ACCOUNT.to_owned()), stale);
        // A Reset in the file home: a new key in the file, and the table is sealed with it.
        let homes = fx.homes();
        fx.secrets(&homes).reset().unwrap();
        fx.secrets(&homes).write(DB, "b", "new").unwrap();
        homes.switch(Home::Keyring).unwrap();
        assert!(!fx.data_dir().join(KEY_FILE).exists());
        assert_eq!(fx.secrets(&fx.homes()).load(DB, "b").unwrap().as_deref(), Some("new"));
    }

    #[test]
    fn two_keys_that_both_fail_refuse_with_the_way_out() {
        let fx = Fixture::new();
        let homes = fx.homes();
        fx.secrets(&homes).write(DB, "a", "pw").unwrap();
        fx.os.items.lock().unwrap().insert((KEY, MASTER_KEY_ACCOUNT.to_owned()), "AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA=".into());
        fs_write(&fx.data_dir().join(KEY_FILE), "b3RoZXI=");
        for target in [Home::File, Home::Keyring] {
            let refused = homes.switch(target).unwrap_err().to_string();
            assert!(refused.contains("tablex.secret-store.v1") && refused.contains("master-key"), "{refused}");
            assert!(refused.contains("neither opens") && refused.contains("Delete the item"), "{refused}");
        }
        assert_eq!(homes.home().unwrap(), Home::Keyring);
        assert!(fx.os.has_key());
    }

    #[test]
    fn a_locked_keyring_stops_a_switch_and_nothing_moves() {
        let fx = Fixture::new();
        let homes = fx.homes();
        fx.secrets(&homes).write(DB, "a", "pw").unwrap();
        fx.os.locked.store(true, Ordering::SeqCst);
        assert!(matches!(homes.switch(Home::File), Err(CredentialError::Platform(_))));
        assert!(!fx.data_dir().join(KEY_FILE).exists());
        assert!(!fx.data_dir().join(CHOICE_FILE).exists());
    }

    #[test]
    fn a_missing_file_key_is_not_replaced_while_secrets_exist() {
        let fx = Fixture::new();
        fx.os.down.store(true, Ordering::SeqCst);
        let homes = fx.homes();
        homes.switch(Home::File).unwrap();
        fx.secrets(&homes).write(DB, "a", "pw").unwrap();
        std::fs::remove_file(fx.data_dir().join(KEY_FILE)).unwrap();
        let store = fx.secrets(&fx.homes());
        let missing = store.write(DB, "b", "x").unwrap_err();
        assert!(matches!(missing, CredentialError::MasterKeyMissing { key_file: Some(_), .. }), "{missing:?}");
        assert!(!fx.data_dir().join(KEY_FILE).exists(), "no new key was made");
        // In the file home the way out is the key; the reset that deletes every secret comes last.
        let text = missing.to_string();
        assert!(text.contains("Settings > Security") && text.contains("Move the key to the system keyring"), "{text}");
        assert!(text.contains(KEY_FILE) && crate::credentials::suggests_reset(&text), "{text}");
        assert!(text.find("put the key").unwrap() < text.find("last resort").unwrap(), "{text}");
        assert!(crate::credentials::suggests_reset(&store.load(DB, "a").unwrap_err().to_string()));
    }

    /// The v0.1.10 GNOME user: the key is in the keyring, tablex starts where none runs.
    fn stranded() -> Fixture {
        let fx = Fixture::new();
        let homes = fx.homes();
        fx.secrets(&homes).write(DB, "a", "pw").unwrap();
        fx.os.down.store(true, Ordering::SeqCst);
        fx
    }

    #[test]
    fn the_master_key_is_only_served_under_its_own_account() {
        let fx = Fixture::new();
        let homes = fx.homes();
        homes.switch(Home::File).unwrap();
        for account in ["", "other"] {
            assert!(homes.load(KEY, account).is_err());
            assert!(homes.write(KEY, account, "k").is_err());
            assert!(homes.delete(KEY, account).is_err());
        }
        assert!(!fx.data_dir().join(KEY_FILE).exists());
        homes.write(KEY, MASTER_KEY_ACCOUNT, "k").unwrap();
        assert_eq!(homes.load(KEY, MASTER_KEY_ACCOUNT).unwrap().as_deref(), Some("k"));
    }

    #[test]
    fn a_stray_key_file_turns_the_unreachable_key_into_the_choice_that_offers_the_file() {
        let fx = stranded();
        std::fs::write(fx.data_dir().join(KEY_FILE), "stray").unwrap();
        assert_eq!(fx.homes().need(), Need::Choice);
    }

    #[test]
    fn secrets_with_no_reachable_key_are_not_the_question_of_where_to_keep_a_new_one() {
        let fx = stranded();
        let homes = fx.homes();
        assert_eq!(homes.need(), Need::KeyUnreachable { secrets: 1 });
        assert!(!homes.needs_choice(), "the dialog that offers a file is not the one to show");
        let fresh = Fixture::new();
        fresh.os.down.store(true, Ordering::SeqCst);
        assert_eq!(fresh.homes().need(), Need::Choice, "no secrets yet: nothing to strand");
        assert_eq!(Fixture::new().homes().need(), Need::Nothing, "a keyring runs");
    }

    #[test]
    fn no_permanent_choice_is_made_while_the_key_is_unreachable_and_nothing_is_lost() {
        let fx = stranded();
        let homes = fx.homes();
        for target in [Home::File, Home::NoSave] {
            let refused = homes.switch(target).unwrap_err();
            assert_eq!(refused, CredentialError::KeyUnreachable { secrets: 1 }, "{target:?}");
            assert!(!crate::credentials::suggests_reset(&refused.to_string()));
        }
        assert!(!fx.data_dir().join(CHOICE_FILE).exists(), "no choice file");
        assert!(!fx.data_dir().join(KEY_FILE).exists());
        assert_eq!(homes.home().unwrap(), Home::Keyring);
        // Back in the desktop session everything still opens.
        fx.os.down.store(false, Ordering::SeqCst);
        assert_eq!(fx.secrets(&fx.homes()).load(DB, "a").unwrap().as_deref(), Some("pw"));
        // With the keyring running both choices work again (the key is readable).
        fx.homes().switch(Home::File).unwrap();
        assert_eq!(fx.secrets(&fx.homes()).load(DB, "a").unwrap().as_deref(), Some("pw"));
    }

    #[test]
    fn a_running_keyring_without_the_key_is_a_missing_key_not_a_reason_for_the_file() {
        let fx = Fixture::new();
        let homes = fx.homes();
        fx.secrets(&homes).write(DB, "a", "pw").unwrap();
        fx.os.items.lock().unwrap().clear();
        let refused = homes.switch(Home::File).unwrap_err();
        assert!(matches!(refused, CredentialError::MasterKeyMissing { secrets: 1, key_file: None, .. }), "{refused:?}");
        assert!(!fx.data_dir().join(CHOICE_FILE).exists());
    }

    #[test]
    fn a_missing_key_in_a_running_keyring_names_the_other_keyring_before_the_reset() {
        let fx = Fixture::new();
        fx.secrets(&fx.homes()).write(DB, "a", "pw").unwrap();
        fx.os.items.lock().unwrap().clear();
        let text = fx.secrets(&fx.homes()).load(DB, "a").unwrap_err().to_string();
        assert!(crate::credentials::suggests_reset(&text), "the reset stays the way out: {text}");
        if FALLBACK_SUPPORTED {
            let (other, reset) = (text.find("another keyring or desktop session").unwrap(), text.find("Reset").unwrap());
            assert!(other < reset && text.contains("GNOME") && text.contains("KDE"), "{text}");
            assert!(text.contains("last resort"), "{text}");
        } else {
            assert!(!text.contains("another keyring"), "macOS and Windows keep their wording: {text}");
        }
    }

    #[test]
    fn a_key_file_next_to_a_keyring_choice_is_named_instead_of_the_reset() {
        let fx = Fixture::new();
        let homes = fx.homes();
        fx.secrets(&homes).write(DB, "a", "pw").unwrap();
        homes.switch(Home::File).unwrap();
        // The choice file is deleted by hand: the choice reads as the keyring again, which has no key.
        std::fs::remove_file(fx.data_dir().join(CHOICE_FILE)).unwrap();
        let missing = fx.secrets(&fx.homes()).load(DB, "a").unwrap_err();
        assert!(
            matches!(&missing, CredentialError::MasterKeyMissing { key_file: None, stray_key_file: Some(_), .. }),
            "{missing:?}"
        );
        let text = missing.to_string();
        assert!(text.contains(KEY_FILE) && text.contains("Settings > Security"), "{text}");
        assert!(!crate::credentials::suggests_reset(&text), "{text}");
        // The way the text points to works, and the passwords open.
        fx.homes().switch(Home::File).unwrap();
        assert_eq!(fx.secrets(&fx.homes()).load(DB, "a").unwrap().as_deref(), Some("pw"));
    }

    #[test]
    fn no_save_is_fine_without_a_keyring_when_the_key_is_in_the_file_or_nothing_is_saved() {
        let fx = Fixture::new();
        fx.os.down.store(true, Ordering::SeqCst);
        let homes = fx.homes();
        homes.switch(Home::File).unwrap();
        fx.secrets(&homes).write(DB, "a", "pw").unwrap();
        homes.switch(Home::NoSave).unwrap();
        assert!(fx.data_dir().join(KEY_FILE).exists(), "the key stays");
        let empty = Fixture::new();
        empty.os.down.store(true, Ordering::SeqCst);
        empty.homes().switch(Home::NoSave).unwrap();
    }

    #[test]
    fn concurrent_writes_of_the_same_file_never_share_a_temporary_file() {
        let fx = Fixture::new();
        std::thread::scope(|scope| {
            for n in 0..8 {
                let dir = fx.data_dir();
                scope.spawn(move || {
                    for _ in 0..20 {
                        write_private(&dir, KEY_FILE, &format!("key{n}")).unwrap();
                    }
                });
            }
        });
        let names: Vec<_> = std::fs::read_dir(fx.data_dir()).unwrap().map(|e| e.unwrap().file_name()).collect();
        assert_eq!(names.len(), 1, "{names:?}");
    }

    #[test]
    fn existing_users_are_unchanged_the_os_item_is_the_key_and_no_file_appears() {
        let fx = Fixture::new();
        let homes = fx.homes();
        fx.secrets(&homes).write(DB, "a", "pw").unwrap();
        assert_eq!(fx.secrets(&fx.homes()).load(DB, "a").unwrap().as_deref(), Some("pw"));
        assert!(fx.os.has_key());
        assert!(!fx.data_dir().join(KEY_FILE).exists());
        assert!(!fx.data_dir().join(CHOICE_FILE).exists());
        // Legacy items stay with the OS store in every home.
        fx.os.write(DB, "legacy", "old").unwrap();
        homes.switch(Home::File).unwrap();
        assert_eq!(homes.load(DB, "legacy").unwrap().as_deref(), Some("old"));
    }

    #[test]
    fn the_saved_choice_is_what_a_front_end_applies_at_start() {
        let fx = Fixture::new();
        assert!(fx.homes().saves_passwords());
        fx.homes().switch(Home::NoSave).unwrap();
        assert!(!fx.homes().saves_passwords());
        fx.homes().switch(Home::File).unwrap();
        assert!(fx.homes().saves_passwords());
        fs_write(&fx.data_dir().join(CHOICE_FILE), "banana");
        assert!(!fx.homes().saves_passwords(), "an unreadable choice saves nothing");
    }

    #[test]
    fn the_saved_passwords_flag_defaults_to_on() {
        assert!(passwords_saved());
    }
}
