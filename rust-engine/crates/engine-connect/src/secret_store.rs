//! [`EncryptedStore`]: every saved secret in the `secrets` table of the app database
//! (`<app data dir>/tablex.db`, see [`engine_storage::app_db`]), sealed with AES-256-GCM under a
//! master key that is the only item kept in the OS credential store. The OS asks (macOS: the
//! Keychain prompt) about that one item, not one per password.
//!
//! ```sql
//! secrets(name TEXT PRIMARY KEY, nonce BLOB NOT NULL, ct BLOB NOT NULL, updated_at INTEGER NOT NULL)
//! ```
//!
//! `name` is `<service>:<account>`.
//!
//! * Each write uses a fresh 96-bit nonce; the associated data is the entry name, so a ciphertext
//!   moved to another row fails to open instead of answering with the wrong secret.
//! * The master key is 32 random bytes (base64 in the OS item), read once per process. It is
//!   created only by a write that finds the table without secrets: an item that is missing while
//!   secrets exist, or that exists but cannot be read, is an error (a new key would orphan every
//!   secret). A reserved check row sealed with the key lets a write notice a stale key (a reset
//!   elsewhere): it re-reads the OS item once and otherwise refuses, so the table never holds two
//!   keys. [`EncryptedStore::reset`] drops the secrets (only that table: connections, history and
//!   the rest of the database stay) and makes a fresh key; the app offers it as "Reset Saved
//!   Passwords" and the errors above point to it.
//! * Writes run in one `BEGIN IMMEDIATE` transaction (the lock between the app and `tablex-mcp`;
//!   SQLite waits for it up to the busy timeout), after re-reading what they decide on inside
//!   it. The key is read from the OS before the transaction. The OS is asked inside it only to
//!   create the key, to re-check a stale one, or to see that a legacy item still exists; never
//!   while a Rust mutex is held. Reads are plain `SELECT`s (a WAL reader sees one consistent
//!   snapshot and never blocks a writer). A connection lives for one operation, so there is no
//!   cache to keep fresh. A database that is damaged or from a newer TableX is an error naming
//!   it, never replaced.
//! * Secrets the one-item-per-secret [`SystemStore`](crate::credentials::SystemStore) holds
//!   (written by earlier versions) are copied in lazily: a `load` that finds nothing in the table
//!   asks that store and a hit is inserted unless a row appeared meanwhile. The item is **not**
//!   deleted by the copy, and a `write` leaves it too: an older TableX that may still run reads
//!   only those items, and must keep working. Removing legacy items that were copied is for a
//!   later release. The one exception is [`delete`](CredentialStore::delete): when the user
//!   removes a password (or its connection) the legacy item goes with it, or the next `load`
//!   would copy it back.
//!
//! Nothing here logs or prints a secret; `Debug` shows none.

use std::path::{Path, PathBuf};
use std::sync::{Mutex, MutexGuard};
use std::time::{SystemTime, UNIX_EPOCH};

use aes_gcm::aead::{Aead, Payload};
use aes_gcm::{Aes256Gcm, KeyInit, Nonce};
use base64::{engine::general_purpose::STANDARD, Engine as _};
use engine_storage::app_db::{self, AppDbError};
use rusqlite::{params, Connection, OptionalExtension};
use zeroize::Zeroizing;

use crate::credentials::{CredentialError, CredentialStore, SecretKind, MASTER_KEY_ACCOUNT, RESET_ACTION};
use crate::platform::CREDENTIAL_STORE_LABEL;

const KEY_LEN: usize = 32;
const NONCE_LEN: usize = 12;
/// A reserved row sealed with the master key (no real kind can have this name): a key that does
/// not open it is not the key of this table.
const CHECK_NAME: &str = "tablex.secret-store.v1:check";
const CHECK_TEXT: &[u8] = b"tablex";

type Key = Zeroizing<[u8; KEY_LEN]>;

fn error(message: impl Into<String>) -> CredentialError {
    CredentialError::Platform(message.into())
}

fn db_error(path: &Path, what: &str, source: rusqlite::Error) -> CredentialError {
    error(format!(
        "app database {}: cannot {what}: {source}",
        path.display()
    ))
}

fn open_error(source: AppDbError) -> CredentialError {
    error(source.to_string())
}

/// One sealed row.
#[derive(Clone)]
struct Entry {
    nonce: Vec<u8>,
    ct: Vec<u8>,
}

#[derive(Default)]
struct State {
    key: Option<Key>,
}

pub struct EncryptedStore {
    /// Holds the master key, and the secrets of earlier versions (one item each).
    os: Box<dyn CredentialStore + Send + Sync>,
    path: Option<PathBuf>,
    state: Mutex<State>,
}

impl std::fmt::Debug for EncryptedStore {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("EncryptedStore")
            .field("path", &self.path)
            .finish_non_exhaustive()
    }
}

/// The name of a row; also its associated data.
fn entry_name(kind: SecretKind, account: &str) -> String {
    format!("{}:{account}", kind.service())
}

fn reserved(kind: SecretKind) -> Result<(), CredentialError> {
    if kind == SecretKind::MasterKey {
        Err(error("the master key is not a saved secret"))
    } else {
        Ok(())
    }
}

fn seal(key: &Key, name: &str, plain: &[u8]) -> Result<Entry, CredentialError> {
    let mut nonce = [0u8; NONCE_LEN];
    getrandom::getrandom(&mut nonce).map_err(|e| error(format!("no random bytes: {e}")))?;
    let ct = Aes256Gcm::new(aes_gcm::Key::<Aes256Gcm>::from_slice(&key[..]))
        .encrypt(
            Nonce::from_slice(&nonce),
            Payload {
                msg: plain,
                aad: name.as_bytes(),
            },
        )
        .map_err(|_| error("cannot encrypt the secret"))?;
    Ok(Entry {
        nonce: nonce.to_vec(),
        ct,
    })
}

fn open(key: &Key, name: &str, entry: &Entry) -> Result<String, CredentialError> {
    let damaged = || {
        error(format!(
            "a saved secret cannot be decrypted (the app database or its master key changed); \
             use {RESET_ACTION} (command palette) and enter the passwords again"
        ))
    };
    if entry.nonce.len() != NONCE_LEN {
        return Err(damaged());
    }
    let plain = Zeroizing::new(
        Aes256Gcm::new(aes_gcm::Key::<Aes256Gcm>::from_slice(&key[..]))
            .decrypt(
                Nonce::from_slice(&entry.nonce),
                Payload {
                    msg: &entry.ct,
                    aad: name.as_bytes(),
                },
            )
            .map_err(|_| damaged())?,
    );
    std::str::from_utf8(&plain)
        .map(str::to_owned)
        .map_err(|_| damaged())
}

/// The key out of the OS item's text, decoded straight into wiped buffers.
fn decode_key(text: &str) -> Result<Key, CredentialError> {
    let damaged = || error(format!(
            "the master key item in the {CREDENTIAL_STORE_LABEL} is damaged; use {RESET_ACTION} \
             (command palette) and enter the passwords again"
        ));
    let mut buffer = Zeroizing::new([0u8; KEY_LEN + 16]);
    let len = STANDARD
        .decode_slice(text.trim(), &mut buffer[..])
        .map_err(|_| damaged())?;
    if len != KEY_LEN {
        return Err(damaged());
    }
    let mut key = Zeroizing::new([0u8; KEY_LEN]);
    key.copy_from_slice(&buffer[..KEY_LEN]);
    Ok(key)
}

fn get_entry(conn: &Connection, name: &str) -> rusqlite::Result<Option<Entry>> {
    conn.query_row(
        "SELECT nonce, ct FROM secrets WHERE name = ?1",
        [name],
        |row| {
            Ok(Entry {
                nonce: row.get(0)?,
                ct: row.get(1)?,
            })
        },
    )
    .optional()
}

/// The row a key is checked against: the check row, else the first secret; `None` for an empty
/// table.
fn probe(conn: &Connection) -> rusqlite::Result<Option<(String, Entry)>> {
    conn.query_row(
        "SELECT name, nonce, ct FROM secrets ORDER BY name = ?1 DESC, name LIMIT 1",
        [CHECK_NAME],
        |row| {
            Ok((
                row.get(0)?,
                Entry {
                    nonce: row.get(1)?,
                    ct: row.get(2)?,
                },
            ))
        },
    )
    .optional()
}

/// The secrets of the table (all rows but the check row).
fn real_count(conn: &Connection) -> rusqlite::Result<usize> {
    conn.query_row(
        "SELECT COUNT(*) FROM secrets WHERE name != ?1",
        [CHECK_NAME],
        |row| row.get::<_, i64>(0),
    )
    .map(|count| count as usize)
}

fn put_entry(conn: &Connection, name: &str, entry: &Entry) -> rusqlite::Result<()> {
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |elapsed| elapsed.as_secs() as i64);
    conn.execute(
        "INSERT INTO secrets (name, nonce, ct, updated_at) VALUES (?1, ?2, ?3, ?4)
         ON CONFLICT(name) DO UPDATE SET nonce = excluded.nonce, ct = excluded.ct,
                                         updated_at = excluded.updated_at",
        params![name, entry.nonce, entry.ct, now],
    )
    .map(drop)
}

impl EncryptedStore {
    /// `os` holds the master key and the legacy items; `path` is the app database (`None`: no
    /// data directory, so nothing can be saved).
    pub fn new(os: Box<dyn CredentialStore + Send + Sync>, path: Option<PathBuf>) -> Self {
        Self {
            os,
            path,
            state: Mutex::new(State::default()),
        }
    }

    fn state(&self) -> MutexGuard<'_, State> {
        self.state
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    fn path(&self) -> Result<&Path, CredentialError> {
        self.path
            .as_deref()
            .ok_or_else(|| error("app database: there is no application data directory"))
    }

    /// The database for reading or deleting: `None` when it does not exist yet (nothing is
    /// created by looking).
    fn open_existing(path: &Path) -> Result<Option<Connection>, CredentialError> {
        match path.try_exists() {
            Ok(true) => app_db::open(path).map(Some).map_err(open_error),
            Ok(false) => Ok(None),
            Err(e) => Err(error(format!(
                "app database {}: cannot inspect the file: {e}",
                path.display()
            ))),
        }
    }

    /// The master key item as the OS has it (`None`: no such item). Nothing is held while the OS
    /// is asked, and anything but a clean "no item" is an error.
    fn read_key(&self) -> Result<Option<Key>, CredentialError> {
        match self.os.load(SecretKind::MasterKey, MASTER_KEY_ACCOUNT)? {
            Some(text) => decode_key(&Zeroizing::new(text)).map(Some),
            None => Ok(None),
        }
    }

    /// The master key, from memory or else read once from the OS. A key that got into memory
    /// meanwhile wins over the one just read.
    fn key(&self) -> Result<Option<Key>, CredentialError> {
        let cached = self.state().key.clone();
        if cached.is_some() {
            return Ok(cached);
        }
        let Some(key) = self.read_key()? else {
            return Ok(None);
        };
        Ok(Some(self.state().key.get_or_insert(key).clone()))
    }

    /// Replaces the key in memory by `key` only if it still is `stale` (or empty): a newer key put
    /// there by someone else is never rolled back.
    fn install_key(&self, stale: Option<&Key>, key: &Key) {
        let mut state = self.state();
        if state.key.is_none() || state.key.as_deref() == stale.map(|stale| &**stale) {
            state.key = Some(key.clone());
        }
    }

    fn create_key(&self) -> Result<Key, CredentialError> {
        let mut key = Zeroizing::new([0u8; KEY_LEN]);
        getrandom::getrandom(&mut key[..]).map_err(|e| error(format!("no random bytes: {e}")))?;
        let mut text = Zeroizing::new([0u8; 48]);
        let len = STANDARD
            .encode_slice(&key[..], &mut text[..])
            .map_err(|_| error("cannot encode the master key"))?;
        let text = std::str::from_utf8(&text[..len]).map_err(|_| error("cannot encode the master key"))?;
        self.os.write(SecretKind::MasterKey, MASTER_KEY_ACCOUNT, text)?;
        Ok(key)
    }

    /// The key to seal with, inside the write transaction. `cached` is the key in memory (or just
    /// read). It is checked against `probe` (the check row, else any row): a key that does not
    /// open it is stale (a reset elsewhere), so the OS is asked once more; one that still does not
    /// open it is an error, never a second key in the table. With no key at all one is made only
    /// when the table holds no secrets (`count`, read inside the transaction).
    fn verified_key(
        &self,
        path: &Path,
        probe: Option<&(String, Entry)>,
        count: usize,
        cached: Option<Key>,
    ) -> Result<Key, CredentialError> {
        let opens = |key: &Key| probe.is_none_or(|(name, entry)| open(key, name, entry).is_ok());
        let key = match &cached {
            Some(key) if opens(key) => key.clone(),
            _ => match self.read_key()? {
                Some(fresh) if opens(&fresh) => fresh,
                Some(_) => return Err(Self::missing_key(path, count)),
                None if count == 0 => self.create_key()?,
                None => return Err(Self::missing_key(path, count)),
            },
        };
        self.install_key(cached.as_ref(), &key);
        Ok(key)
    }

    /// What a missing (or no longer matching) master key means for `count` secrets of the table.
    fn missing_key(path: &Path, count: usize) -> CredentialError {
        CredentialError::MasterKeyMissing {
            secrets: count,
            file: path.to_owned(),
        }
    }

    /// Puts `password` in the table, creating the master key when the table has no secrets and the
    /// OS no key. `migrate`: the secret comes from a legacy item, so it is written only if the item
    /// still exists (asked before the lock) and the table has no row for it yet (asked under the
    /// lock): `Ok(false)` otherwise.
    fn write_row(
        &self,
        kind: SecretKind,
        account: &str,
        password: &str,
        migrate: bool,
    ) -> Result<bool, CredentialError> {
        let path = self.path()?;
        let name = entry_name(kind, account);
        let at = |what: &'static str| move |source| db_error(path, what, source);
        // The OS may prompt for the key: ask before anything is locked.
        let known = self.key()?;
        // A copy of a legacy item is made only while the item is there: asked before the lock (the
        // OS may be slow and no transaction waits for it), the row is what the lock decides on.
        if migrate && self.os.load(kind, account)?.is_none() {
            return Ok(false);
        }
        let mut conn = app_db::open(path).map_err(open_error)?;
        let tx = app_db::write_transaction(&mut conn).map_err(at("lock the database"))?;
        if migrate && get_entry(&tx, &name).map_err(at("read"))?.is_some() {
            return Ok(false);
        }
        let probe = probe(&tx).map_err(at("read"))?;
        let count = real_count(&tx).map_err(at("read"))?;
        // The key in memory is the newest one (a reset in this process replaced it).
        let cached = self.state().key.clone().or(known);
        let key = self.verified_key(path, probe.as_ref(), count, cached)?;
        let check_ok = probe
            .as_ref()
            .is_some_and(|(row, entry)| row == CHECK_NAME && open(&key, CHECK_NAME, entry).is_ok());
        if !check_ok {
            put_entry(&tx, CHECK_NAME, &seal(&key, CHECK_NAME, CHECK_TEXT)?).map_err(at("write"))?;
        }
        put_entry(&tx, &name, &seal(&key, &name, password.as_bytes())?).map_err(at("write"))?;
        tx.commit().map_err(at("commit"))?;
        Ok(true)
    }

    /// Whether a `load` that finds a secret only as an item of an earlier version copies it into
    /// the table: in the app, once it moved the connections into the database (see
    /// `engine_storage::legacy`). Until then the older TableX that may still run beside is the one
    /// that owns those items, and a reader that is not the app must not touch the table or the key.
    fn copies_legacy_items(&self) -> bool {
        engine_storage::legacy::import_allowed()
            && self
                .path
                .as_deref()
                .is_some_and(|path| engine_storage::legacy::is_imported(path, "connections"))
    }

    /// Removes the row when it is there (no master key needed).
    fn delete_row(&self, kind: SecretKind, account: &str) -> Result<(), CredentialError> {
        let Some(path) = self.path.as_deref() else {
            return Ok(());
        };
        let at = |what: &'static str| move |source| db_error(path, what, source);
        let name = entry_name(kind, account);
        let Some(mut conn) = Self::open_existing(path)? else {
            return Ok(());
        };
        if get_entry(&conn, &name).map_err(at("read"))?.is_none() {
            return Ok(());
        }
        let tx = app_db::write_transaction(&mut conn).map_err(at("lock the database"))?;
        tx.execute("DELETE FROM secrets WHERE name = ?1", [&name])
            .map_err(at("delete"))?;
        tx.commit().map_err(at("commit"))
    }

    /// The row `name`, decrypted (`None`: no such row, or no data directory).
    fn load_row(&self, name: &str) -> Result<Option<String>, CredentialError> {
        let Some(path) = self.path.as_deref() else {
            return Ok(None);
        };
        let at = |what: &'static str| move |source| db_error(path, what, source);
        let Some(conn) = Self::open_existing(path)? else {
            return Ok(None);
        };
        let Some(entry) = get_entry(&conn, name).map_err(at("read"))? else {
            return Ok(None);
        };
        let count = real_count(&conn).map_err(at("read"))?;
        // The OS may be asked next: nothing of the database stays open.
        drop(conn);
        let key = self.key()?.ok_or_else(|| Self::missing_key(path, count))?;
        match open(&key, name, &entry) {
            Ok(password) => Ok(Some(password)),
            Err(first) => {
                // A reset by another process may have changed the key under this one: look once
                // more. The key in memory is replaced only if it still is the one that failed.
                match self.read_key()? {
                    Some(fresh) if *fresh != *key => {
                        let opened = open(&fresh, name, &entry)?;
                        self.install_key(Some(&key), &fresh);
                        Ok(Some(opened))
                    }
                    _ => Err(first),
                }
            }
        }
    }

    /// Forgets every saved secret in the table and makes a fresh master key ("Reset Saved
    /// Passwords", see [`RESET_ACTION`]). Nothing else in the database is touched. The
    /// passwords must be entered again; secrets still held as items of their own (earlier
    /// versions) are unaffected. Other running TableX processes pick the new key up on their next
    /// read. A damaged or newer database is an error, not repaired: the file holds more than the
    /// secrets.
    pub fn reset(&self) -> Result<Vec<String>, CredentialError> {
        let path = self.path()?;
        let at = |what: &'static str| move |source| db_error(path, what, source);
        let mut conn = app_db::open(path).map_err(open_error)?;
        let tx = app_db::write_transaction(&mut conn).map_err(at("lock the database"))?;
        // Whose items earlier versions may have kept in the OS store: every saved connection
        // (under the id as written, upper and lower case: the app looked up both) and every
        // account of the table. Read now: the table is emptied next.
        let (accounts, mut leftovers) = self.legacy_accounts(&tx, path).map_err(at("read"))?;
        tx.execute("DELETE FROM secrets", []).map_err(at("delete"))?;
        // Until the commit the old rows are still the table: if the key cannot be written, the
        // transaction ends and the old key still opens them.
        let key = self.create_key()?;
        self.state().key = Some(key.clone());
        put_entry(&tx, CHECK_NAME, &seal(&key, CHECK_NAME, CHECK_TEXT)?).map_err(at("write"))?;
        tx.commit().map_err(at("commit"))?;
        // The items of earlier versions would be copied back by the next `load`: they go too. The
        // table is reset already, so a failure here is reported, not an error. They go after the
        // commit, not before: a reset that then failed would have lost passwords. The cost is a
        // window of a few moments in which another process's `load` may copy an item into the new
        // table before it is deleted here (it is then in the table until the next reset).
        for account in &accounts {
            if let Err(error) = self.os.delete_all(account) {
                leftovers.push(format!("{account}: {error}"));
            }
        }
        for account in crate::credentials::AI_API_KEY_ACCOUNTS {
            if let Err(error) = self.os.delete(SecretKind::AiApiKey, account) {
                leftovers.push(format!("AI key {account}: {error}"));
            }
        }
        Ok(leftovers)
    }

    /// Every account an item of an earlier version could be under (see [`Self::reset`]), and what
    /// could not be listed (said to the user with what could not be deleted).
    fn legacy_accounts(&self, conn: &Connection, path: &Path) -> rusqlite::Result<(Vec<String>, Vec<String>)> {
        let mut unlisted = Vec::new();
        let mut accounts = std::collections::BTreeSet::new();
        let mut add = |account: &str| {
            for variant in [account.to_owned(), account.to_uppercase(), account.to_lowercase()] {
                accounts.insert(variant);
            }
        };
        let connections = conn.prepare("SELECT id FROM saved_connections").and_then(|mut stmt| {
            stmt.query_map([], |row| row.get::<_, String>(0))?.collect::<rusqlite::Result<Vec<_>>>()
        })?;
        connections.iter().for_each(|id| add(id));
        // Connections not moved into the database yet are in the old file (the app works on it
        // for the run when the move failed): their items must go as well.
        if !engine_storage::legacy::is_imported(path, "connections") {
            match engine_storage::connection::old_connection_ids(path) {
                Ok(ids) => ids.iter().for_each(|id| add(id)),
                Err(error) => unlisted.push(format!(
                    "the saved connections could not be listed ({error}): their older items were not looked for"
                )),
            }
        }
        let mut stmt = conn.prepare("SELECT name FROM secrets WHERE name != ?1")?;
        let names = stmt
            .query_map([CHECK_NAME], |row| row.get::<_, String>(0))?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        for name in names {
            // `<service>:<account>`; a jump host's account is its connection's plus `#jumpN`.
            if let Some((_, account)) = name.split_once(':') {
                add(account.split("#jump").next().unwrap_or(account));
            }
        }
        Ok((accounts.into_iter().collect(), unlisted))
    }
}

impl CredentialStore for EncryptedStore {
    fn load(&self, kind: SecretKind, account: &str) -> Result<Option<String>, CredentialError> {
        reserved(kind)?;
        let name = entry_name(kind, account);
        if let Some(password) = self.load_row(&name)? {
            return Ok(Some(password));
        }
        // Not in the table: an earlier version may have kept it as an item of its own.
        let Some(password) = self.os.load(kind, account)? else {
            return Ok(None);
        };
        // Only the app copies it in (and may create the master key doing so): a process that may
        // not import (tablex-mcp), or the app before it moved the connections in, answers from
        // the item and writes nothing.
        if !self.copies_legacy_items() {
            return Ok(Some(password));
        }
        // Legacy cleanup (deleting the item after the copy) is for a later release: an older
        // build that may still run reads nothing else.
        match self.write_row(kind, account, &password, true) {
            Ok(true) => Ok(Some(password)),
            // Not copied: a newer secret was saved meanwhile (it wins, the old item is left
            // alone), or the item was deleted meanwhile (it stays deleted).
            Ok(false) => Ok(self.load_row(&name)?),
            // Could not be moved (yet): it is still the answer.
            Err(_) => Ok(Some(password)),
        }
    }

    fn write(&self, kind: SecretKind, account: &str, password: &str) -> Result<(), CredentialError> {
        reserved(kind)?;
        // The table answers first from now on. An old item that stays is left alone (see the
        // module docs): an older TableX may still read it.
        self.write_row(kind, account, password, false)?;
        Ok(())
    }

    fn delete(&self, kind: SecretKind, account: &str) -> Result<(), CredentialError> {
        reserved(kind)?;
        let row = self.delete_row(kind, account);
        // A legacy item that stays would answer `load` again, so its failure is reported.
        let legacy = self.os.delete(kind, account);
        row.and(legacy)
    }

    fn unlock(&self) -> Result<(), CredentialError> {
        self.key().map(|_| ())
    }

    fn reset(&self) -> Result<Vec<String>, CredentialError> {
        Self::reset(self)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;
    use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
    use std::fs;
    use std::sync::Arc;

    /// The OS credential store: master key and legacy items in memory.
    #[derive(Default)]
    struct Os {
        items: Mutex<HashMap<(SecretKind, String), String>>,
        loads: AtomicUsize,
        key_writes: AtomicUsize,
        deny_key_read: AtomicBool,
        fail_delete: AtomicBool,
        fail_key_write: AtomicBool,
        /// Runs once, after a legacy (non master key) item was read: another process acting
        /// while this one waited on the OS.
        after_legacy_read: Mutex<Option<Box<dyn FnOnce() + Send>>>,
    }

    impl Os {
        fn put(&self, kind: SecretKind, account: &str, value: &str) {
            self.items
                .lock()
                .unwrap()
                .insert((kind, account.to_owned()), value.to_owned());
        }
        fn has(&self, kind: SecretKind, account: &str) -> bool {
            self.items
                .lock()
                .unwrap()
                .contains_key(&(kind, account.to_owned()))
        }
    }

    impl CredentialStore for Os {
        fn load(&self, kind: SecretKind, account: &str) -> Result<Option<String>, CredentialError> {
            self.loads.fetch_add(1, Ordering::SeqCst);
            if kind == SecretKind::MasterKey && self.deny_key_read.load(Ordering::SeqCst) {
                return Err(CredentialError::Platform("user canceled".into()));
            }
            let found = self
                .items
                .lock()
                .unwrap()
                .get(&(kind, account.to_owned()))
                .cloned();
            if kind != SecretKind::MasterKey && found.is_some() {
                let hook = self.after_legacy_read.lock().unwrap().take();
                if let Some(hook) = hook {
                    hook();
                }
            }
            Ok(found)
        }
        fn write(&self, kind: SecretKind, account: &str, password: &str) -> Result<(), CredentialError> {
            if kind == SecretKind::MasterKey {
                if self.fail_key_write.load(Ordering::SeqCst) {
                    return Err(CredentialError::Platform("keychain locked".into()));
                }
                self.key_writes.fetch_add(1, Ordering::SeqCst);
            }
            self.put(kind, account, password);
            Ok(())
        }
        fn delete(&self, kind: SecretKind, account: &str) -> Result<(), CredentialError> {
            if self.fail_delete.load(Ordering::SeqCst) {
                return Err(CredentialError::Platform("keychain locked".into()));
            }
            self.items.lock().unwrap().remove(&(kind, account.to_owned()));
            Ok(())
        }
    }

    struct Fixture {
        dir: tempfile::TempDir,
        os: Arc<Os>,
    }

    impl Fixture {
        fn new() -> Self {
            Self {
                dir: tempfile::tempdir().unwrap(),
                os: Arc::new(Os::default()),
            }
        }
        fn path(&self) -> PathBuf {
            self.dir.path().join("tablex").join(app_db::FILE_NAME)
        }
        /// The app has moved the connections into the database (the marker is there): a `load`
        /// that finds a legacy item copies it in.
        fn app_moved_in(&self) {
            let conn = app_db::open(&self.path()).unwrap();
            conn.execute("INSERT OR IGNORE INTO imports (name, at) VALUES ('connections', 1)", []).unwrap();
        }
        /// A store instance (a process) over the shared OS store and file.
        fn store(&self) -> EncryptedStore {
            EncryptedStore::new(Box::new(Arc::clone(&self.os)), Some(self.path()))
        }
    }

    const DB: SecretKind = SecretKind::Database;

    #[test]
    fn round_trip_overwrite_and_delete() {
        let fx = Fixture::new();
        let store = fx.store();
        assert_eq!(store.load(DB, "a").unwrap(), None);
        store.write(DB, "a", "first").unwrap();
        assert_eq!(store.load(DB, "a").unwrap().as_deref(), Some("first"));
        store.write(DB, "a", "p@ss wörd").unwrap();
        assert_eq!(store.load(DB, "a").unwrap().as_deref(), Some("p@ss wörd"));
        // Another instance reads what this one wrote.
        assert_eq!(fx.store().load(DB, "a").unwrap().as_deref(), Some("p@ss wörd"));
        store.delete(DB, "a").unwrap();
        assert_eq!(store.load(DB, "a").unwrap(), None);
        store.delete(DB, "a").unwrap();
    }

    #[test]
    fn an_empty_password_deletes_through_save() {
        let fx = Fixture::new();
        let store = fx.store();
        store.save(DB, "a", "pw").unwrap();
        store.save(SecretKind::Proxy, "a", "proxy").unwrap();
        store.save(DB, "a", "").unwrap();
        assert_eq!(store.load(DB, "a").unwrap(), None);
        assert_eq!(store.load(SecretKind::Proxy, "a").unwrap().as_deref(), Some("proxy"));
    }

    #[test]
    fn the_database_holds_no_plaintext_and_is_private() {
        let fx = Fixture::new();
        let store = fx.store();
        store.write(DB, "conn-1", "hunter2-needle").unwrap();
        // Nothing of the secret is in the database file, nor in its write-ahead log if one is
        // left.
        for suffix in ["", "-wal"] {
            let file = format!("{}{suffix}", fx.path().display());
            if let Ok(bytes) = fs::read(&file) {
                assert!(!bytes.windows(14).any(|w| w == b"hunter2-needle"), "{file}");
            }
        }
        let raw = raw(&fx);
        let (nonce, ct): (Vec<u8>, Vec<u8>) = raw
            .query_row(
                "SELECT nonce, ct FROM secrets WHERE name = ?1",
                [entry_name(DB, "conn-1")],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .unwrap();
        assert_eq!(nonce.len(), NONCE_LEN);
        assert!(!ct.windows(14).any(|w| w == b"hunter2-needle"));
        #[cfg(unix)]
        for suffix in ["", "-wal", "-shm"] {
            use std::os::unix::fs::PermissionsExt;
            let file = format!("{}{suffix}", fx.path().display());
            if let Ok(meta) = fs::metadata(&file) {
                assert_eq!(meta.permissions().mode() & 0o777, 0o600, "{file}");
            }
        }
    }

    #[test]
    fn the_schema_version_is_recorded_and_the_secrets_table_exists() {
        let fx = Fixture::new();
        fx.store().write(DB, "a", "x").unwrap();
        let raw = raw(&fx);
        let version: u32 = raw.pragma_query_value(None, "user_version", |r| r.get(0)).unwrap();
        assert_eq!(version, app_db::SCHEMA_VERSION);
        let mode: String = raw.pragma_query_value(None, "journal_mode", |r| r.get(0)).unwrap();
        assert_eq!(mode, "wal");
    }

    /// A plain connection to the database, for looking at and editing rows.
    fn raw(fx: &Fixture) -> Connection {
        let conn = Connection::open(fx.path()).unwrap();
        conn.busy_timeout(std::time::Duration::from_secs(5)).unwrap();
        conn
    }

    fn edit_entry(fx: &Fixture, name: &str, change: impl FnOnce(&mut Entry)) {
        let conn = raw(fx);
        let mut entry = get_entry(&conn, name).unwrap().unwrap();
        change(&mut entry);
        put_entry(&conn, name, &entry).unwrap();
    }

    fn flip_first_ct_byte(entry: &mut Entry) {
        entry.ct[0] ^= 1;
    }

    #[test]
    fn a_tampered_or_swapped_entry_is_an_error_not_a_wrong_secret() {
        let fx = Fixture::new();
        let store = fx.store();
        store.write(DB, "a", "secret-a").unwrap();
        store.write(DB, "b", "secret-b").unwrap();

        let (a, b) = (entry_name(DB, "a"), entry_name(DB, "b"));
        let conn = raw(&fx);
        let (entry_a, entry_b) = (get_entry(&conn, &a).unwrap().unwrap(), get_entry(&conn, &b).unwrap().unwrap());
        put_entry(&conn, &a, &entry_b).unwrap();
        put_entry(&conn, &b, &entry_a).unwrap();
        drop(conn);
        let swapped = fx.store();
        assert!(swapped.load(DB, "a").is_err());
        assert!(swapped.load(DB, "b").is_err());

        let fresh = Fixture::new();
        let store = fresh.store();
        store.write(DB, "a", "secret-a").unwrap();
        edit_entry(&fresh, &entry_name(DB, "a"), flip_first_ct_byte);
        let error = fresh.store().load(DB, "a").unwrap_err();
        assert!(!error.to_string().contains("secret-a"));
        let text = error.to_string();
        assert!(text.contains("use Reset Saved Passwords (command palette)"), "{text}");
        assert!(crate::credentials::suggests_reset(&text));
        assert!(!text.contains("delete") && !text.contains("aside"), "{text}");
    }

    #[test]
    fn the_master_key_is_created_once_and_reused_by_every_instance() {
        let fx = Fixture::new();
        fx.store().write(DB, "a", "one").unwrap();
        fx.store().write(DB, "b", "two").unwrap();
        let store = fx.store();
        store.write(DB, "c", "three").unwrap();
        assert_eq!(fx.os.key_writes.load(Ordering::SeqCst), 1);
        assert_eq!(fx.store().load(DB, "a").unwrap().as_deref(), Some("one"));
        // Read once per instance, then served from memory.
        let before = fx.os.loads.load(Ordering::SeqCst);
        store.load(DB, "a").unwrap();
        store.load(DB, "b").unwrap();
        assert_eq!(fx.os.loads.load(Ordering::SeqCst), before);
    }

    #[test]
    fn an_unreadable_master_key_is_an_error_and_no_new_key_is_made() {
        let fx = Fixture::new();
        fx.store().write(DB, "a", "one").unwrap();
        fx.os.deny_key_read.store(true, Ordering::SeqCst);
        let store = fx.store();
        assert!(store.load(DB, "a").is_err());
        assert!(store.write(DB, "b", "two").is_err());
        assert!(store.unlock().is_err());
        assert_eq!(fx.os.key_writes.load(Ordering::SeqCst), 1, "no second key");
        // Back to readable: nothing was lost.
        fx.os.deny_key_read.store(false, Ordering::SeqCst);
        assert_eq!(fx.store().load(DB, "a").unwrap().as_deref(), Some("one"));
        assert_eq!(fx.store().load(DB, "b").unwrap(), None);
    }

    #[test]
    fn a_damaged_key_item_is_an_error_and_is_not_replaced() {
        let fx = Fixture::new();
        fx.os.put(SecretKind::MasterKey, MASTER_KEY_ACCOUNT, "not-a-key");
        assert!(fx.store().write(DB, "a", "pw").is_err());
        assert_eq!(fx.os.key_writes.load(Ordering::SeqCst), 0);
        assert!(!fx.path().exists());
    }

    #[test]
    fn unlock_reads_the_key_and_is_fine_before_one_exists() {
        let fx = Fixture::new();
        fx.store().unlock().unwrap();
        assert_eq!(fx.os.key_writes.load(Ordering::SeqCst), 0);
        fx.store().write(DB, "a", "pw").unwrap();
        fx.store().unlock().unwrap();
    }

    #[test]
    fn legacy_items_move_into_the_file_on_first_load() {
        let fx = Fixture::new();
        fx.app_moved_in();
        fx.os.put(DB, "a", "legacy-pw");
        let store = fx.store();
        assert_eq!(store.load(DB, "a").unwrap().as_deref(), Some("legacy-pw"));
        assert!(fx.os.has(DB, "a"), "the legacy item is copied, not moved: an older build may read it");
        let loads = fx.os.loads.load(Ordering::SeqCst);
        assert_eq!(store.load(DB, "a").unwrap().as_deref(), Some("legacy-pw"));
        assert_eq!(fx.os.loads.load(Ordering::SeqCst), loads, "the second load stays in the file");
        assert_eq!(fx.store().load(DB, "a").unwrap().as_deref(), Some("legacy-pw"));
    }

    #[test]
    fn a_legacy_secret_is_still_returned_when_it_cannot_be_moved() {
        let fx = Fixture::new();
        fx.os.put(DB, "a", "legacy-pw");
        fx.os.put(SecretKind::MasterKey, MASTER_KEY_ACCOUNT, "not-a-key");
        assert_eq!(fx.store().load(DB, "a").unwrap().as_deref(), Some("legacy-pw"));
        assert!(fx.os.has(DB, "a"), "kept: it is not in the file");
    }

    #[test]
    fn writing_answers_from_the_table_and_leaves_the_legacy_item_deleting_clears_both() {
        let fx = Fixture::new();
        fx.app_moved_in();
        fx.os.put(DB, "a", "old");
        let store = fx.store();
        store.write(DB, "a", "new").unwrap();
        assert!(fx.os.has(DB, "a"), "left for an older build");
        assert_eq!(store.load(DB, "a").unwrap().as_deref(), Some("new"), "the table answers first");

        fx.os.put(DB, "b", "legacy");
        assert_eq!(store.load(DB, "b").unwrap().as_deref(), Some("legacy"), "copied");
        assert!(fx.os.has(DB, "b"));
        store.delete(DB, "b").unwrap();
        assert!(!fx.os.has(DB, "b"), "the user removed it: the legacy item goes too");
        assert_eq!(store.load(DB, "b").unwrap(), None, "and is not copied back");
    }

    #[test]
    fn a_legacy_item_that_cannot_be_deleted_is_reported() {
        let fx = Fixture::new();
        let store = fx.store();
        store.write(DB, "a", "pw").unwrap();
        fx.os.fail_delete.store(true, Ordering::SeqCst);
        assert!(store.delete(DB, "a").is_err());
    }

    #[test]
    fn two_instances_on_one_file_keep_each_others_entries() {
        let fx = Fixture::new();
        let (first, second) = (fx.store(), fx.store());
        first.write(DB, "a", "from-first").unwrap();
        second.write(DB, "b", "from-second").unwrap();
        first.write(SecretKind::Proxy, "a", "proxy").unwrap();
        for store in [&first, &second, &fx.store()] {
            assert_eq!(store.load(DB, "a").unwrap().as_deref(), Some("from-first"));
            assert_eq!(store.load(DB, "b").unwrap().as_deref(), Some("from-second"));
            assert_eq!(store.load(SecretKind::Proxy, "a").unwrap().as_deref(), Some("proxy"));
        }
        second.delete(DB, "a").unwrap();
        assert_eq!(first.load(DB, "a").unwrap(), None);
        assert_eq!(first.load(DB, "b").unwrap().as_deref(), Some("from-second"));
    }

    #[test]
    fn threads_writing_through_separate_instances_lose_nothing() {
        let fx = Fixture::new();
        fx.store().write(DB, "seed", "x").unwrap();
        std::thread::scope(|scope| {
            for n in 0..4 {
                let store = fx.store();
                scope.spawn(move || {
                    for i in 0..5 {
                        store.write(DB, &format!("t{n}-{i}"), "pw").unwrap();
                    }
                });
            }
        });
        let store = fx.store();
        for n in 0..4 {
            for i in 0..5 {
                assert!(store.load(DB, &format!("t{n}-{i}")).unwrap().is_some(), "t{n}-{i}");
            }
        }
    }

    #[test]
    fn delete_all_clears_a_connection_including_its_jump_hosts() {
        let fx = Fixture::new();
        let store = fx.store();
        for kind in SecretKind::ALL {
            store.write(kind, "c", "pw").unwrap();
        }
        store.write(SecretKind::SshPassword, &crate::credentials::jump_account("c", 0), "j0").unwrap();
        store.write(SecretKind::SshPassphrase, &crate::credentials::jump_account("c", 1), "j1").unwrap();
        store.write(DB, "other", "keep").unwrap();
        store.write(SecretKind::AiApiKey, "c", "ai").unwrap();
        store.delete_all("c").unwrap();
        for kind in SecretKind::ALL {
            assert_eq!(store.load(kind, "c").unwrap(), None, "{kind:?}");
        }
        assert_eq!(store.load(SecretKind::SshPassword, &crate::credentials::jump_account("c", 0)).unwrap(), None);
        assert_eq!(store.load(SecretKind::SshPassphrase, &crate::credentials::jump_account("c", 1)).unwrap(), None);
        assert_eq!(store.load(DB, "other").unwrap().as_deref(), Some("keep"));
        assert_eq!(store.load(SecretKind::AiApiKey, "c").unwrap().as_deref(), Some("ai"));
    }

    #[test]
    fn a_damaged_or_newer_database_is_an_error_and_is_left_alone() {
        let fx = Fixture::new();
        fs::create_dir_all(fx.path().parent().unwrap()).unwrap();
        fs::write(fx.path(), b"{ not json").unwrap();
        let store = fx.store();
        assert!(store.load(DB, "a").is_err());
        assert!(store.write(DB, "a", "pw").is_err());
        assert!(store.delete(DB, "a").is_err());
        assert!(store.reset().is_err());
        assert_eq!(fs::read(fx.path()).unwrap(), b"{ not json");

        // A database from a newer TableX: turned away by every operation, rows kept.
        let newer = Fixture::new();
        newer.store().write(DB, "a", "pw").unwrap();
        raw(&newer)
            .pragma_update(None, "user_version", app_db::SCHEMA_VERSION + 1)
            .unwrap();
        // One whose changes older builds cannot work with (it says so in `meta`).
        raw(&newer)
            .execute(
                "UPDATE meta SET value = ?1 WHERE key = 'min_reader_version'",
                [(app_db::SCHEMA_VERSION + 1).to_string()],
            )
            .unwrap();
        let before = file_text(&newer);
        let store = newer.store();
        for result in [
            store.write(DB, "b", "pw").map(drop),
            store.load(DB, "a").map(drop),
            store.delete(DB, "a"),
            store.reset().map(drop),
        ] {
            let message = result.unwrap_err().to_string();
            assert!(message.contains("newer TableX"), "{message}");
            assert!(message.contains(&newer.path().display().to_string()), "{message}");
        }
        let version: u32 = raw(&newer).pragma_query_value(None, "user_version", |r| r.get(0)).unwrap();
        assert_eq!(version, app_db::SCHEMA_VERSION + 1);
        assert_eq!(file_text(&newer), before);
    }

    #[test]
    fn the_master_key_kind_is_not_a_saved_secret_and_debug_shows_none() {
        let fx = Fixture::new();
        let store = fx.store();
        store.write(DB, "a", "very-secret-text").unwrap();
        assert!(store.load(SecretKind::MasterKey, MASTER_KEY_ACCOUNT).is_err());
        assert!(store.write(SecretKind::MasterKey, MASTER_KEY_ACCOUNT, "x").is_err());
        assert!(store.delete(SecretKind::MasterKey, MASTER_KEY_ACCOUNT).is_err());
        assert!(!format!("{store:?}").contains("very-secret-text"));
    }

    #[test]
    fn no_data_directory_cannot_save_but_still_reads_legacy_items() {
        let os = Arc::new(Os::default());
        os.put(DB, "a", "legacy");
        let store = EncryptedStore::new(Box::new(Arc::clone(&os)), None);
        assert_eq!(store.load(DB, "a").unwrap().as_deref(), Some("legacy"));
        assert!(os.has(DB, "a"), "not moved: nowhere to move it");
        assert!(store.write(DB, "a", "x").is_err());
    }

    /// Every row of the table (check row included), as stored.
    fn file_text(fx: &Fixture) -> Vec<(String, Vec<u8>, Vec<u8>)> {
        let conn = raw(fx);
        let mut statement = conn.prepare("SELECT name, nonce, ct FROM secrets ORDER BY name").unwrap();
        statement
            .query_map([], |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)))
            .unwrap()
            .map(Result::unwrap)
            .collect()
    }

    #[test]
    fn first_time_key_creation_races_make_exactly_one_key() {
        let fx = Fixture::new();
        std::thread::scope(|scope| {
            for n in 0..6 {
                let store = fx.store();
                scope.spawn(move || store.write(DB, &format!("c{n}"), &format!("pw{n}")).unwrap());
            }
        });
        assert_eq!(fx.os.key_writes.load(Ordering::SeqCst), 1);
        let store = fx.store();
        for n in 0..6 {
            assert_eq!(store.load(DB, &format!("c{n}")).unwrap(), Some(format!("pw{n}")));
        }
    }

    #[test]
    fn a_missing_key_with_entries_is_a_typed_error_and_changes_nothing() {
        let fx = Fixture::new();
        fx.store().write(DB, "a", "one").unwrap();
        fx.os.items.lock().unwrap().remove(&(SecretKind::MasterKey, MASTER_KEY_ACCOUNT.to_owned()));
        let before = file_text(&fx);
        let writes = fx.os.key_writes.load(Ordering::SeqCst);
        let store = fx.store();
        let typed = store.write(DB, "b", "two").unwrap_err();
        assert!(
            matches!(&typed, CredentialError::MasterKeyMissing { secrets: 1, file } if *file == fx.path()),
            "{typed:?}"
        );
        let write = typed.to_string();
        assert!(write.contains("master key of 1 saved secret is missing"), "{write}");
        assert!(write.contains("Use Reset Saved Passwords (command palette) and enter the passwords again"), "{write}");
        assert!(crate::credentials::suggests_reset(&write));
        assert!(!write.contains("delete") && !write.contains("aside") && !write.contains(app_db::FILE_NAME), "{write}");
        let load = store.load(DB, "a").unwrap_err().to_string();
        assert!(load.contains("master key"), "{load}");
        assert!(!crate::records::is_missing_password_error(&load));
        assert_eq!(fx.os.key_writes.load(Ordering::SeqCst), writes, "no new key");
        assert_eq!(file_text(&fx), before, "entries untouched");
        // Deleting needs no key.
        store.delete(DB, "a").unwrap();
    }

    #[test]
    fn reset_drops_the_entries_and_makes_a_fresh_working_key() {
        let fx = Fixture::new();
        fx.store().write(DB, "a", "one").unwrap();
        fx.os.items.lock().unwrap().remove(&(SecretKind::MasterKey, MASTER_KEY_ACCOUNT.to_owned()));
        let store = fx.store();
        assert!(store.write(DB, "b", "two").is_err());
        store.reset().unwrap();
        assert_eq!(fx.os.key_writes.load(Ordering::SeqCst), 2);
        assert_eq!(store.load(DB, "a").unwrap(), None);
        store.write(DB, "b", "two").unwrap();
        assert_eq!(fx.store().load(DB, "b").unwrap().as_deref(), Some("two"));
        assert_eq!(all_entries_open(&fx), 2, "check, b");
    }

    #[test]
    fn reset_clears_only_the_secrets_not_connections_or_history() {
        let fx = Fixture::new();
        let store = fx.store();
        store.write(DB, "a", "one").unwrap();
        let conn = app_db::open(&fx.path()).unwrap();
        conn.execute_batch(
            "INSERT INTO saved_connections VALUES ('c','Name','u','postgres','#fff',1,0,'{}',0,NULL,1,1,'','');
             INSERT INTO query_history (conn_uuid, database, collection, timestamp) VALUES ('c','d','x',1.0);",
        )
        .unwrap();
        store.reset().unwrap();
        let rows = |table: &str| -> i64 {
            conn.query_row(&format!("SELECT COUNT(*) FROM {table}"), [], |r| r.get(0)).unwrap()
        };
        assert_eq!((rows("saved_connections"), rows("query_history")), (1, 1));
        assert_eq!(rows("secrets"), 1, "only the check row of the fresh key");
        assert_eq!(store.load(DB, "a").unwrap(), None);
    }

    #[test]
    fn reset_also_removes_the_items_earlier_versions_kept_so_nothing_is_copied_back() {
        use crate::credentials::jump_account;
        let fx = Fixture::new();
        fx.app_moved_in();
        let store = fx.store();
        store.write(DB, "seed", "x").unwrap();
        let conn = raw(&fx);
        conn.execute_batch(
            "INSERT INTO saved_connections VALUES ('abc-1','Kept','u','postgres','#fff',1,0,'{}',0,NULL,1,1,'','');
             INSERT INTO query_history (conn_uuid, database, collection, timestamp) VALUES ('abc-1','d','x',1.0);",
        )
        .unwrap();
        // Items of an earlier version, under the id as the app asked for it, a jump host's
        // account, the other kinds, the AI key, and one of a connection that is gone.
        let items = [
            (DB, "ABC-1".to_owned()),
            (DB, "abc-1".to_owned()),
            (SecretKind::Proxy, "ABC-1".to_owned()),
            (SecretKind::SshPassword, jump_account("ABC-1", 1)),
            (SecretKind::SshPassphrase, "ABC-1".to_owned()),
            (SecretKind::AiApiKey, "anthropic".to_owned()),
            (SecretKind::AiApiKey, "ollama".to_owned()),
            (DB, "GONE-2".to_owned()),
        ];
        for (kind, account) in &items {
            fx.os.put(*kind, account, "legacy");
        }
        // A load copied one into the table (as the app does) and the table has an entry of the
        // connection that is gone.
        assert_eq!(store.load(DB, "ABC-1").unwrap().as_deref(), Some("legacy"));
        store.write(DB, "GONE-2", "t").unwrap();

        assert!(store.reset().unwrap().is_empty(), "everything could be removed");
        for (kind, account) in &items {
            assert!(!fx.os.has(*kind, account), "{kind:?} {account} is still in the OS store");
            assert_eq!(store.load(*kind, account).unwrap(), None, "{kind:?} {account} came back");
        }
        assert!(fx.os.has(SecretKind::MasterKey, MASTER_KEY_ACCOUNT), "the new key is kept");
        let rows = |table: &str| -> i64 {
            conn.query_row(&format!("SELECT COUNT(*) FROM {table}"), [], |r| r.get(0)).unwrap()
        };
        assert_eq!((rows("saved_connections"), rows("query_history")), (1, 1), "connections and history are kept");

        // What the OS will not let go is said, and the table is reset all the same.
        fx.os.put(DB, "ABC-1", "stuck");
        fx.os.fail_delete.store(true, Ordering::SeqCst);
        let left = store.reset().unwrap();
        assert!(left.iter().any(|line| line.contains("ABC-1")), "{left:?}");
        fx.os.fail_delete.store(false, Ordering::SeqCst);
    }

    #[test]
    fn reset_through_an_arc_of_the_store_reaches_the_store() {
        use crate::credentials::CredentialStore as _;
        let fx = Fixture::new();
        let store = Arc::new(fx.store());
        store.write(DB, "a", "pw").unwrap();
        // The trait method of `Arc<T>`, as the app holds the shared store: not the default.
        assert!(CredentialStore::reset(&store).unwrap().is_empty());
        assert_eq!(store.load(DB, "a").unwrap(), None);
        let shared: Arc<dyn CredentialStore + Send + Sync> = store;
        shared.reset().unwrap();
    }

    #[test]
    fn reset_before_the_connections_moved_in_looks_for_the_items_of_the_old_connection_file() {
        let fx = Fixture::new();
        let store = fx.store();
        store.write(DB, "seed", "x").unwrap();
        // The connections are still in the old file (the move failed or has not happened).
        let old = fx.path().parent().unwrap().join("connections.db");
        rusqlite::Connection::open(&old)
            .unwrap()
            .execute_batch(
                "CREATE TABLE saved_connections (id TEXT PRIMARY KEY, name TEXT);
                 INSERT INTO saved_connections VALUES ('only-in-old-file', 'n');",
            )
            .unwrap();
        fx.os.put(DB, "ONLY-IN-OLD-FILE", "legacy");
        fx.os.put(SecretKind::SshPassword, "only-in-old-file#jump0", "jump");
        assert!(store.reset().unwrap().is_empty());
        assert!(!fx.os.has(DB, "ONLY-IN-OLD-FILE") && !fx.os.has(SecretKind::SshPassword, "only-in-old-file#jump0"));
        // An old file that cannot be read is said: its connections' items cannot be found.
        fs::write(&old, vec![0xA5u8; 4096]).unwrap();
        let left = store.reset().unwrap();
        assert!(left.iter().any(|line| line.contains("could not be listed") && line.contains("connections.db")), "{left:?}");
    }

    #[test]
    fn a_load_before_the_app_moved_the_connections_in_reads_the_item_and_writes_nothing() {
        let fx = Fixture::new();
        fx.os.put(DB, "a", "legacy-pw");
        let store = fx.store();
        assert_eq!(store.load(DB, "a").unwrap().as_deref(), Some("legacy-pw"));
        assert_eq!(store.load(DB, "a").unwrap().as_deref(), Some("legacy-pw"), "and again");
        assert!(!fx.path().exists(), "no database was made");
        assert_eq!(fx.os.key_writes.load(Ordering::SeqCst), 0, "no master key was created");
        assert!(!fx.os.has(SecretKind::MasterKey, MASTER_KEY_ACCOUNT));
        // A database that exists but is not moved in yet: still no row, still no key.
        app_db::open(&fx.path()).unwrap();
        assert_eq!(fx.store().load(DB, "a").unwrap().as_deref(), Some("legacy-pw"));
        assert_eq!(file_text(&fx).len(), 0, "no row");
        assert_eq!(fx.os.key_writes.load(Ordering::SeqCst), 0);
        // Once the app moved them in, the next load copies it (as the app does).
        fx.app_moved_in();
        assert_eq!(fx.store().load(DB, "a").unwrap().as_deref(), Some("legacy-pw"));
        assert_eq!(file_text(&fx).len(), 2, "the copy and the check row");
    }

    #[test]
    fn a_reset_that_cannot_write_the_key_keeps_the_old_secrets() {
        let fx = Fixture::new();
        let store = fx.store();
        store.write(DB, "a", "one").unwrap();
        let before = file_text(&fx);
        fx.os.fail_key_write.store(true, Ordering::SeqCst);
        assert!(store.reset().is_err());
        assert_eq!(file_text(&fx), before);
        assert_eq!(fx.store().load(DB, "a").unwrap().as_deref(), Some("one"));
    }

    #[test]
    fn a_reset_by_another_process_is_picked_up_by_a_reader_holding_the_old_key() {
        let fx = Fixture::new();
        let reader = fx.store();
        fx.store().write(DB, "a", "one").unwrap();
        assert_eq!(reader.load(DB, "a").unwrap().as_deref(), Some("one"));
        let other = fx.store();
        other.reset().unwrap();
        other.write(DB, "a", "two").unwrap();
        assert_eq!(reader.load(DB, "a").unwrap().as_deref(), Some("two"));
    }

    #[test]
    fn a_secret_saved_while_the_legacy_item_was_read_is_not_overwritten_by_the_migration() {
        let fx = Fixture::new();
        fx.app_moved_in();
        fx.store().write(SecretKind::Proxy, "seed", "x").unwrap();
        fx.os.put(DB, "a", "old");
        let other = fx.store();
        let os = Arc::clone(&fx.os);
        *fx.os.after_legacy_read.lock().unwrap() = Some(Box::new(move || {
            other.write(DB, "a", "newer").unwrap();
            // The other process's cleanup is not what is under test: the old item is still there.
            os.put(DB, "a", "old");
        }));
        let store = fx.store();
        assert_eq!(store.load(DB, "a").unwrap().as_deref(), Some("newer"));
        assert!(fx.os.has(DB, "a"), "the file's entry is newer: the old item is left alone");
        assert_eq!(fx.store().load(DB, "a").unwrap().as_deref(), Some("newer"));
    }

    /// Every row of the table (check row included) opens with the key the OS holds now.
    fn all_entries_open(fx: &Fixture) -> usize {
        let key = fx.store().read_key().unwrap().expect("a key");
        let rows = file_text(fx);
        for (name, nonce, ct) in &rows {
            open(&key, name, &Entry { nonce: nonce.clone(), ct: ct.clone() })
                .unwrap_or_else(|_| panic!("{name} does not open"));
        }
        rows.len()
    }

    #[test]
    fn a_write_from_a_stale_instance_after_a_cross_process_reset_never_mixes_keys() {
        let fx = Fixture::new();
        let stale = fx.store();
        stale.write(DB, "a", "one").unwrap();
        let other = fx.store();
        other.reset().unwrap();
        other.write(DB, "b", "two").unwrap();
        // The stale instance re-reads the key from the OS and carries on with the new one.
        stale.write(DB, "c", "three").unwrap();
        assert_eq!(all_entries_open(&fx), 3, "check, b, c");
        assert_eq!(fx.store().load(DB, "c").unwrap().as_deref(), Some("three"));
        assert_eq!(fx.store().load(DB, "a").unwrap(), None);

        // When the OS key is not the key of the file, the write is refused and changes nothing.
        let before = file_text(&fx);
        fx.os.put(SecretKind::MasterKey, MASTER_KEY_ACCOUNT, &STANDARD.encode([9u8; KEY_LEN]));
        // (The stale instance itself still holds the key that opens the file, so it can write.)
        let refused = fx.store().write(DB, "d", "four").unwrap_err();
        assert!(matches!(refused, CredentialError::MasterKeyMissing { secrets: 2, .. }), "{refused:?}");
        assert_eq!(file_text(&fx), before);
    }

    #[test]
    fn a_table_from_before_the_check_row_gets_one_on_the_next_write() {
        let fx = Fixture::new();
        let store = fx.store();
        store.write(DB, "a", "one").unwrap();
        let remove_check = || {
            raw(&fx).execute("DELETE FROM secrets WHERE name = ?1", [CHECK_NAME]).unwrap();
        };
        remove_check();
        fx.store().write(DB, "b", "two").unwrap();
        assert_eq!(all_entries_open(&fx), 3);
        // Without the check row a key that does not open the other rows is refused too.
        remove_check();
        fx.os.put(SecretKind::MasterKey, MASTER_KEY_ACCOUNT, &STANDARD.encode([9u8; KEY_LEN]));
        assert!(fx.store().write(DB, "c", "three").is_err());
    }

    #[test]
    fn a_reset_in_this_process_racing_writers_leaves_one_key_in_the_file() {
        let fx = Fixture::new();
        let store = fx.store();
        store.write(DB, "seed", "x").unwrap();
        std::thread::scope(|scope| {
            for n in 0..3 {
                let store = &store;
                scope.spawn(move || {
                    for i in 0..8 {
                        store.write(DB, &format!("t{n}-{i}"), "pw").unwrap();
                    }
                });
            }
            scope.spawn(|| {
                for _ in 0..4 {
                    store.reset().unwrap();
                }
            });
        });
        all_entries_open(&fx);
        assert_eq!(fx.store().load(DB, "seed").unwrap(), None);
        store.write(DB, "after", "ok").unwrap();
        assert_eq!(fx.store().load(DB, "after").unwrap().as_deref(), Some("ok"));
        all_entries_open(&fx);
    }

    #[test]
    fn a_failed_decrypt_retry_keeps_the_key_and_never_rolls_back_a_newer_one() {
        let fx = Fixture::new();
        let store = fx.store();
        store.write(DB, "a", "one").unwrap();
        let old = store.key().unwrap().unwrap();
        // The OS key was replaced by one that opens nothing: the read fails and the key stays.
        fx.os.put(SecretKind::MasterKey, MASTER_KEY_ACCOUNT, &STANDARD.encode([9u8; KEY_LEN]));
        edit_entry(&fx, &entry_name(DB, "a"), flip_first_ct_byte);
        assert!(store.load(DB, "a").is_err());
        assert_eq!(store.state().key.as_deref(), Some(&*old));
        // Replacing a key that is no longer the one in memory does nothing.
        let newer = Zeroizing::new([5u8; KEY_LEN]);
        store.state().key = Some(newer.clone());
        store.install_key(Some(&old), &Zeroizing::new([6u8; KEY_LEN]));
        assert_eq!(store.state().key.as_deref(), Some(&*newer));
        store.install_key(Some(&newer), &Zeroizing::new([6u8; KEY_LEN]));
        assert_eq!(store.state().key.as_deref(), Some(&[6u8; KEY_LEN]));
    }

    #[test]
    fn a_legacy_item_deleted_while_it_was_being_read_is_not_resurrected() {
        let fx = Fixture::new();
        fx.app_moved_in();
        fx.os.put(DB, "a", "old");
        let other = fx.store();
        *fx.os.after_legacy_read.lock().unwrap() = Some(Box::new(move || {
            other.delete(DB, "a").unwrap();
        }));
        let store = fx.store();
        assert_eq!(store.load(DB, "a").unwrap(), None);
        assert!(!fx.os.has(DB, "a"));
        assert_eq!(fx.store().load(DB, "a").unwrap(), None);
        assert!(file_text(&fx).iter().all(|(name, _, _)| *name != entry_name(DB, "a")));
    }

    /// `tablex.db` holds the connections, history and everything else: no text the app shows for a
    /// problem with the secrets or the database may tell the user to move or delete it.
    #[test]
    fn no_error_text_tells_the_user_to_move_or_delete_the_database() {
        let path = PathBuf::from("/data/tablex.db");
        let sqlite = |code| AppDbError::Sqlite {
            path: path.clone(),
            source: rusqlite::Error::SqliteFailure(rusqlite::ffi::Error::new(code), None),
        };
        // The one text that does tell the user to rename `tablex.db`: the database itself cannot be
        // read, and that is the only way out. It says what is lost.
        let damaged = [
            sqlite(rusqlite::ffi::SQLITE_NOTADB).to_string(),
            sqlite(rusqlite::ffi::SQLITE_CORRUPT).to_string(),
        ];
        for text in &damaged {
            assert!(text.contains("rename tablex.db") && text.contains("old files"), "{text}");
            assert!(text.contains("not in those files") && text.contains("lost"), "what is lost is said: {text}");
            assert!(!text.to_lowercase().contains("delete"), "{text}");
        }
        let mut texts = vec![
            CredentialError::NoKeyring.to_string(),
            CredentialError::MasterKeyMissing { secrets: 1, file: path.clone() }.to_string(),
            CredentialError::MasterKeyMissing { secrets: 5, file: path.clone() }.to_string(),
            CredentialError::Platform("anything".into()).to_string(),
            AppDbError::Io { path: path.clone(), source: std::io::Error::other("no space") }.to_string(),
            AppDbError::Newer { path: path.clone(), found: 99 }.to_string(),
            AppDbError::Import { file: path.clone(), message: "locked".into() }.to_string(),
            sqlite(rusqlite::ffi::SQLITE_BUSY).to_string(),
            // What the store itself says: a master key item that is damaged, a secret that does not open.
            decode_key("not base64 at all").unwrap_err().to_string(),
            decode_key("AAAA").unwrap_err().to_string(),
            open(&Zeroizing::new([1u8; KEY_LEN]), "n", &Entry { nonce: vec![0; 3], ct: vec![] }).unwrap_err().to_string(),
        ];
        let fx = Fixture::new();
        fs::create_dir_all(fx.path().parent().unwrap()).unwrap();
        fs::write(fx.path(), vec![0xA5u8; 4096]).unwrap();
        assert!(fx.store().load(DB, "a").unwrap_err().to_string().contains("rename tablex.db"));
        for text in &texts {
            let lower = text.to_lowercase();
            assert!(
                !lower.contains("delete") && !lower.contains("aside") && !lower.contains("move the") && !lower.contains("move it"),
                "tells the user to move or delete the database: {text}"
            );
        }
        // The two that point to the way out say which: the Reset action.
        for text in [&texts[1], &texts[8], &texts[10]] {
            assert!(crate::credentials::suggests_reset(text), "{text}");
        }
    }

    #[test]
    fn a_damaged_database_error_names_the_file_and_the_way_out() {
        let fx = Fixture::new();
        fs::create_dir_all(fx.path().parent().unwrap()).unwrap();
        fs::write(fx.path(), vec![0xA5u8; 4096]).unwrap();
        let message = fx.store().load(DB, "a").unwrap_err().to_string();
        assert!(message.contains(&fx.path().display().to_string()), "{message}");
        assert!(message.contains("restore it from a backup"), "{message}");
        assert!(!message.contains("delete"), "{message}");
        assert_eq!(fs::read(fx.path()).unwrap(), vec![0xA5u8; 4096]);
    }

    #[test]
    fn a_secret_that_is_not_utf8_is_an_error() {
        let fx = Fixture::new();
        let store = fx.store();
        store.write(DB, "a", "x").unwrap();
        let key = store.key().unwrap().unwrap();
        let name = entry_name(DB, "a");
        put_entry(&raw(&fx), &name, &seal(&key, &name, &[0xff, 0xfe]).unwrap()).unwrap();
        assert!(fx.store().load(DB, "a").is_err());
    }

    #[test]
    fn a_write_succeeds_while_another_connection_holds_a_read_transaction() {
        let fx = Fixture::new();
        fx.store().write(DB, "a", "one").unwrap();
        let reader = raw(&fx);
        reader.execute_batch("BEGIN").unwrap();
        let rows: i64 = reader.query_row("SELECT COUNT(*) FROM secrets", [], |r| r.get(0)).unwrap();
        assert_eq!(rows, 2, "check, a");
        // The write does not wait for the reader, and the reader still sees its snapshot.
        fx.store().write(DB, "b", "two").unwrap();
        fx.store().delete(DB, "a").unwrap();
        let during: i64 = reader.query_row("SELECT COUNT(*) FROM secrets", [], |r| r.get(0)).unwrap();
        assert_eq!(during, 2);
        reader.execute_batch("COMMIT").unwrap();
        assert_eq!(fx.store().load(DB, "b").unwrap().as_deref(), Some("two"));
        assert_eq!(fx.store().load(DB, "a").unwrap(), None);
    }

    #[test]
    fn a_write_waits_for_another_writer_and_loses_nothing() {
        let fx = Fixture::new();
        fx.store().write(DB, "seed", "x").unwrap();
        let mut holder = raw(&fx);
        let tx = app_db::write_transaction(&mut holder).unwrap();
        let waiting = {
            let store = fx.store();
            std::thread::spawn(move || store.write(DB, "late", "pw").unwrap())
        };
        std::thread::sleep(std::time::Duration::from_millis(300));
        assert!(!waiting.is_finished(), "the write waits for the lock");
        tx.commit().unwrap();
        waiting.join().unwrap();
        assert_eq!(fx.store().load(DB, "late").unwrap().as_deref(), Some("pw"));
    }

    #[test]
    fn loads_and_deletes_do_not_create_the_database() {
        let fx = Fixture::new();
        let store = fx.store();
        assert_eq!(store.load(DB, "a").unwrap(), None);
        store.delete(DB, "a").unwrap();
        assert!(!fx.path().exists());
    }
}
