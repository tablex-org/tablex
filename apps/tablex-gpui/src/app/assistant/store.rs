//! Conversations kept on this computer: one JSON file each, plain files, no database.
//!
//! `<root>/<connection id>/<conversation id>.json`, where `<root>` is
//! `<app data dir>/assistant/conversations` (`TABLEX_DATA_DIR` moves it with every other file).
//!
//! # The file
//!
//! ```text
//! {"format":1,"id":"...","title":"...","created":1760000000000,"updated":1760000100000,
//! "provider":"anthropic","model":"...","access":"read",
//! "conversation":{ ...what `engine_ai::Conversation` serializes to... }}
//! ```
//!
//! The header fields come first and end the file's first line (the only newline outside strings,
//! `serde_json` escapes the ones inside): the list reads a bounded prefix of each file
//! ([`HEADER_READ`]) and parses just that line, so drawing the History list never parses a
//! conversation (up to [`MAX_FILE_BYTES`] each). There is no second file (no index): the header
//! and the body are one write, so they cannot disagree after a crash. A header that reads but a
//! body the engine refuses shows in the list and fails when it is opened (once reported, then
//! left out of the list).
//!
//! # Writes
//!
//! Off the UI thread. A temporary file in the same directory is written, flushed to disk and
//! renamed over the target ([`ConversationStore::stage`], [`ConversationStore::commit`]): a crash
//! in between leaves the old file and a `.tmp` that listing ignores and the next write or listing
//! removes. On Unix the file is created with mode 0600 and the directories with 0700. The rename
//! relies on `std::fs::rename` replacing an existing target atomically (POSIX `rename`; on Windows
//! `MoveFileExW` with replace-existing, which fails, leaving the old file, when another program
//! holds the target open without share-delete).
//!
//! # Limits and trust
//!
//! A conversation whose file would exceed [`MAX_FILE_BYTES`] is not written ([`StoreError::TooLarge`];
//! the encoder stops at the limit instead of building the whole text). At most
//! [`MAX_PER_CONNECTION`] files per connection: the ones updated longest ago go. Every id that
//! becomes part of a path is validated first ([`valid_id`]: ASCII letters, digits, `-` and `_`, at
//! most [`MAX_ID_CHARS`], no Windows device name), so nothing outside the root is ever read,
//! written or deleted; a symlink where a file or a connection's directory should be is not
//! followed. The root itself is held to the same rule: a conversations folder that is a link is
//! not used at all ([`StoreError::RootIsLink`]: nothing is listed, read, written or deleted
//! through it), whichever call finds out first.
//!
//! A delete refuses the writes that were already on their way, and no others: one connection's
//! delete bumps that connection's epoch, "delete all" the global one ([`Epoch`]); a delete of one
//! conversation marks its id only when a file was removed. What is on disk can hold query results: nothing of the API key or any credential is
//! in a `Conversation`, and this module is given no secret.

use engine_agent::sanitize::clean;
use engine_agent::AgentAccess;
use engine_ai::Conversation;
use serde::{Deserialize, Serialize};
use std::fmt;
use std::fs;
use std::io::{self, Read, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Mutex;

/// The version of the file format this build reads and writes.
pub const FORMAT: u32 = 1;
/// The largest file written (and read).
pub const MAX_FILE_BYTES: usize = 8 * 1024 * 1024;
/// Conversations kept per connection; the least recently updated go.
pub const MAX_PER_CONNECTION: usize = 50;
/// The prefix of a file read to get its header (the header line is a few hundred bytes).
pub const HEADER_READ: usize = 16 * 1024;
/// Files of one connection looked at by a listing, whatever the directory holds.
const MAX_LISTED: usize = 200;
/// Longest title, in characters.
pub const TITLE_CHARS: usize = 80;
/// Longest id that becomes part of a path.
pub const MAX_ID_CHARS: usize = 64;
/// Longest provider / model text kept from a header.
const LABEL_CHARS: usize = 100;

/// What the header of a file says.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Header {
    pub format: u32,
    pub id: String,
    /// The first user message on one line, cut at [`TITLE_CHARS`] characters.
    pub title: String,
    /// Milliseconds since the Unix epoch.
    pub created: u64,
    pub updated: u64,
    pub provider: String,
    pub model: String,
    /// The highest level the conversation has run at: it holds data up to that level.
    pub access: AgentAccess,
    /// How many messages the conversation has: only ever grows, so a write with fewer than the
    /// file has is an older write (a clock set back cannot make a newer one look old).
    #[serde(default)]
    pub seq: u64,
}

impl Header {
    /// A header for a conversation that starts now (tests).
    #[cfg(test)]
    pub fn start(
        id: String,
        title: String,
        provider: &str,
        model: &str,
        access: AgentAccess,
    ) -> Self {
        let now = now_ms();
        Self {
            format: FORMAT,
            id,
            title,
            created: now,
            updated: now,
            provider: provider.to_owned(),
            model: model.to_owned(),
            access,
            seq: 0,
        }
    }

    /// What a header read from a file may claim: bounded texts, one line.
    fn tidy(mut self) -> Self {
        self.title = clean(&one_line(&self.title), TITLE_CHARS);
        self.provider = clean(&one_line(&self.provider), LABEL_CHARS);
        self.model = clean(&one_line(&self.model), LABEL_CHARS);
        self
    }
}

/// What the store had been through when a run began: how many times everything was deleted and how
/// many times the one connection's conversations were.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Epoch {
    everything: u64,
    connection: u64,
}

/// A conversation as it goes to disk: the bytes, built off the UI thread.
#[derive(Debug)]
pub struct Encoded {
    pub id: String,
    /// The header's `seq`.
    pub seq: u64,
    /// The store's [`ConversationStore::epoch`] for the conversation's connection when the run
    /// that made this began: a delete of everything, or of that connection, since then refuses the
    /// write (a delete of another connection does not).
    pub epoch: Epoch,
    pub bytes: Vec<u8>,
}

/// A conversation read back.
pub struct Loaded {
    pub header: Header,
    pub conversation: Conversation,
}

/// One row of a listing: a readable header.
#[derive(Debug, Clone, PartialEq)]
pub struct Entry {
    pub header: Header,
    pub bytes: u64,
}

/// A file a listing could not use.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Problem {
    pub file: String,
    pub bytes: u64,
    pub why: String,
}

#[derive(Debug, Default)]
pub struct Listing {
    /// Newest update first.
    pub entries: Vec<Entry>,
    pub problems: Vec<Problem>,
}

/// Why an id cannot be part of a path.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum IdError {
    Empty,
    TooLong,
    BadCharacter(char),
    /// A name Windows reserves for a device (`con`, `nul`, `com1`, ...).
    Reserved,
}

impl fmt::Display for IdError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Empty => f.write_str("the id is empty"),
            Self::TooLong => write!(f, "the id is longer than {MAX_ID_CHARS} characters"),
            Self::BadCharacter(ch) => write!(f, "the id holds {ch:?}, which a file name may not"),
            Self::Reserved => f.write_str("the id is a reserved file name"),
        }
    }
}

#[derive(Debug)]
pub enum StoreError {
    BadId(IdError),
    /// The file or directory is not there.
    Missing,
    /// A symlink or a directory where a file should be.
    NotAFile,
    /// The conversation would not fit [`MAX_FILE_BYTES`].
    TooLarge,
    /// The file is not a conversation of this build (bad JSON, refused by the engine, another
    /// connection's, an id that is not its name).
    Damaged(String),
    /// Written by a newer version of the app.
    NewerFormat(u32),
    /// The folder conversations are kept in is a link: it is not used at all (a link could lead
    /// anywhere, and a delete must not follow it).
    RootIsLink,
    /// A newer write of the same conversation is on disk already.
    Stale,
    /// The conversation, its connection or everything was deleted after the run that made the
    /// write began: nothing is written back.
    Detached,
    Io {
        what: &'static str,
        source: io::Error,
    },
}

impl fmt::Display for StoreError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::BadId(why) => write!(f, "not a valid id: {why}"),
            Self::Missing => f.write_str("the saved conversation is not there any more"),
            Self::NotAFile => f.write_str("it is not an ordinary file"),
            Self::TooLarge => write!(
                f,
                "the conversation is larger than {} MiB",
                MAX_FILE_BYTES / (1024 * 1024)
            ),
            Self::Damaged(why) => write!(f, "the file cannot be read: {why}"),
            Self::NewerFormat(version) => write!(
                f,
                "it was saved by a newer version of TableX (format {version}; this one reads {FORMAT})"
            ),
            Self::RootIsLink => f.write_str(
                "the folder for conversations is a link, which TableX does not use; replace it \
                 with a real folder",
            ),
            Self::Stale => f.write_str("a newer version of it is saved already"),
            Self::Detached => f.write_str("it was deleted while it was being written"),
            Self::Io { what, source } => write!(f, "{what}: {source}"),
        }
    }
}

impl std::error::Error for StoreError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Io { source, .. } => Some(source),
            _ => None,
        }
    }
}

impl StoreError {
    fn io(what: &'static str) -> impl FnOnce(io::Error) -> Self {
        move |source| Self::Io { what, source }
    }
}

/// Milliseconds since the Unix epoch (0 before it).
pub fn now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |since| {
            u64::try_from(since.as_millis()).unwrap_or(u64::MAX)
        })
}

/// `text` on one line: every run of white space (line breaks, tabs) is one space.
fn one_line(text: &str) -> String {
    text.split_whitespace().collect::<Vec<_>>().join(" ")
}

/// The title of a conversation: its first message on one line, invisible and direction-changing
/// characters removed and other control characters escaped, cut at [`TITLE_CHARS`].
pub fn title_of(first_message: &str) -> String {
    clean(&one_line(first_message), TITLE_CHARS)
}

/// Checks that `id` can be one component of a path under the root: 1 to [`MAX_ID_CHARS`]
/// characters, ASCII letters, digits, `-` and `_` only (no separator, no dot, so no `..`), and not
/// a name Windows keeps for a device.
pub fn valid_id(id: &str) -> Result<&str, IdError> {
    if id.is_empty() {
        return Err(IdError::Empty);
    }
    if id.chars().count() > MAX_ID_CHARS {
        return Err(IdError::TooLong);
    }
    if let Some(ch) = id
        .chars()
        .find(|ch| !(ch.is_ascii_alphanumeric() || *ch == '-' || *ch == '_'))
    {
        return Err(IdError::BadCharacter(ch));
    }
    let lower = id.to_ascii_lowercase();
    let device = matches!(lower.as_str(), "con" | "prn" | "aux" | "nul")
        || lower
            .strip_prefix("com")
            .or_else(|| lower.strip_prefix("lpt"))
            .is_some_and(|digit| matches!(digit.as_bytes(), [b'1'..=b'9']));
    if device {
        return Err(IdError::Reserved);
    }
    Ok(id)
}

/// A level as a number that keeps its order (for a cell several threads read).
pub fn level_to_u8(level: AgentAccess) -> u8 {
    AgentAccess::ALL
        .iter()
        .position(|known| *known == level)
        .map_or(0, |ix| ix as u8)
}

/// The level a number stands for (`ReadWrite` for anything above the last).
pub fn level_from_u8(number: u8) -> AgentAccess {
    AgentAccess::ALL
        .get(usize::from(number))
        .copied()
        .unwrap_or(AgentAccess::ReadWrite)
}

/// What a stored tool result shows about the statement that made it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Ran {
    /// A statement that returned rows (or a Redis reply, or something else).
    Read,
    /// A statement that changed something: it reports affected rows, or returned no result set at
    /// all (DDL).
    Wrote { rows: Option<u64> },
}

/// How the result `content` of the tool `name` reads, if it was a successful statement tool.
pub fn ran(name: &str, content: &str) -> Option<Ran> {
    use engine_agent::ToolName;
    let tool = ToolName::parse(name)?;
    if !matches!(tool, ToolName::ExecuteQuery | ToolName::ExecuteRedisCommand) {
        return None;
    }
    let json: serde_json::Value = serde_json::from_str(content).ok()?;
    if let Some(rows) = json
        .get("affected_rows")
        .and_then(serde_json::Value::as_u64)
    {
        return Some(Ran::Wrote { rows: Some(rows) });
    }
    let no_result_set = tool == ToolName::ExecuteQuery
        && json
            .get("columns")
            .and_then(serde_json::Value::as_array)
            .is_some_and(Vec::is_empty);
    Some(if no_result_set {
        Ran::Wrote { rows: None }
    } else {
        Ran::Read
    })
}

/// The least level a conversation can have run at, from what it holds: a successful statement
/// is `Read`, one that changed something `ReadWrite`. A header that says less is wrong (or
/// edited), and the conversation holds data of that level either way. The tools are the
/// engine's own ([`engine_agent::ToolName`]), not names written out here.
///
/// What this cannot see (accepted, and why it is safe enough): a statement whose result is an
/// error (nothing was read or changed that the result shows), a Redis write that replies without
/// `affected_rows` (it reads as a read; the session ran at the level the header records), and a
/// `SELECT` that returns zero columns (it reads as a write: the floor errs high, so such a
/// conversation is never offered at a level it did not run at).
pub fn access_floor(conversation: &Conversation) -> AgentAccess {
    use engine_ai::{Message, UserBlock};
    let mut names: std::collections::HashMap<&str, &str> = std::collections::HashMap::new();
    let mut floor = AgentAccess::Off;
    for message in conversation.messages() {
        match message {
            Message::Assistant { content } => {
                for block in content {
                    if let (Some(id), Some(name)) = (
                        block.get("id").and_then(serde_json::Value::as_str),
                        block.get("name").and_then(serde_json::Value::as_str),
                    ) {
                        names.insert(id, name);
                    }
                }
            }
            Message::User { content } => {
                for block in content {
                    if let UserBlock::ToolResult {
                        tool_use_id,
                        content,
                        is_error: false,
                    } = block
                    {
                        let level = match names
                            .get(tool_use_id.as_str())
                            .and_then(|name| ran(name, content))
                        {
                            Some(Ran::Read) => AgentAccess::Read,
                            Some(Ran::Wrote { .. }) => AgentAccess::ReadWrite,
                            None => AgentAccess::Off,
                        };
                        floor = floor.max(level);
                    }
                }
            }
            Message::System { .. } => {}
        }
    }
    floor
}

/// Writes at most `max` bytes: the encoder stops there instead of building a huge text.
struct Capped<'a> {
    bytes: &'a mut Vec<u8>,
    max: usize,
}

impl Write for Capped<'_> {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        if self.bytes.len() + buf.len() > self.max {
            return Err(io::Error::new(io::ErrorKind::OutOfMemory, "over the limit"));
        }
        self.bytes.extend_from_slice(buf);
        Ok(buf.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

/// The bytes of the file for `conversation`: the header's line, then the conversation. `updated`
/// is the header's own. [`StoreError::TooLarge`] when it would exceed [`MAX_FILE_BYTES`].
pub fn encode(header: &Header, conversation: &Conversation) -> Result<Encoded, StoreError> {
    let seq = conversation.messages().len() as u64;
    let header = Header {
        seq,
        ..header.clone()
    };
    let mut line =
        serde_json::to_string(&header).map_err(|error| StoreError::Damaged(error.to_string()))?;
    // `{"format":1,...}` loses its closing brace: the object goes on after the newline.
    line.pop();
    let mut bytes = Vec::with_capacity(line.len() + 4096);
    bytes.extend_from_slice(line.as_bytes());
    bytes.extend_from_slice(b",\n\"conversation\":");
    serde_json::to_writer(
        Capped {
            bytes: &mut bytes,
            max: MAX_FILE_BYTES - 1,
        },
        conversation,
    )
    .map_err(|error| {
        if error.is_io() {
            StoreError::TooLarge
        } else {
            StoreError::Damaged(error.to_string())
        }
    })?;
    bytes.push(b'}');
    Ok(Encoded {
        id: header.id.clone(),
        seq,
        epoch: Epoch::default(),
        bytes,
    })
}

/// The header out of the first bytes of a file.
fn parse_header(prefix: &[u8]) -> Result<Header, StoreError> {
    let end = prefix
        .iter()
        .position(|byte| *byte == b'\n')
        .ok_or_else(|| StoreError::Damaged("no header line".into()))?;
    let mut line = prefix[..end].to_vec();
    if line.pop() != Some(b',') {
        return Err(StoreError::Damaged("the header line is cut".into()));
    }
    line.push(b'}');
    #[derive(Deserialize)]
    struct Probe {
        format: u32,
    }
    let probe: Probe = serde_json::from_slice(&line)
        .map_err(|error| StoreError::Damaged(format!("header: {error}")))?;
    if probe.format != FORMAT {
        return Err(StoreError::NewerFormat(probe.format));
    }
    serde_json::from_slice::<Header>(&line)
        .map(Header::tidy)
        .map_err(|error| StoreError::Damaged(format!("header: {error}")))
}

/// A temporary file written and flushed, not yet renamed over its target.
#[derive(Debug)]
pub struct Staged {
    temp: PathBuf,
    target: PathBuf,
}

/// What a write did.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Written {
    /// Files removed to stay within [`MAX_PER_CONNECTION`].
    pub pruned: usize,
}

pub struct ConversationStore {
    root: PathBuf,
    /// One writer / cleaner at a time within this process (temporary files are removed under it).
    lock: Mutex<()>,
    /// Bumped by every delete of everything.
    epoch: AtomicU64,
    /// Bumped by every delete of one connection's conversations (by its folder name).
    connection_epochs: Mutex<std::collections::HashMap<String, u64>>,
    /// Conversations deleted one by one in this process: their id is not written again.
    deleted: Mutex<std::collections::HashSet<String>>,
    /// The root was found to be a link (by any call): Settings says so.
    root_is_link: std::sync::atomic::AtomicBool,
}

static NONCE: AtomicU64 = AtomicU64::new(0);

impl ConversationStore {
    pub fn new(root: PathBuf) -> Self {
        Self {
            root,
            lock: Mutex::new(()),
            epoch: AtomicU64::new(0),
            connection_epochs: Mutex::default(),
            deleted: Mutex::default(),
            root_is_link: std::sync::atomic::AtomicBool::new(false),
        }
    }

    /// See [`Encoded::epoch`]: a write that carries this is refused after a delete of everything
    /// or of `connection`'s conversations, and after nothing else.
    pub fn epoch(&self, connection: &str) -> Epoch {
        Epoch {
            everything: self.epoch.load(Ordering::SeqCst),
            connection: self
                .connection_epochs
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner())
                .get(&connection.to_ascii_lowercase())
                .copied()
                .unwrap_or(0),
        }
    }

    fn bump_connection(&self, connection: &str) {
        *self
            .connection_epochs
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .entry(connection.to_ascii_lowercase())
            .or_insert(0) += 1;
    }

    /// The folder was found to be a link, by this call or an earlier one.
    pub fn root_is_link(&self) -> bool {
        self.root_is_link.load(Ordering::SeqCst)
    }

    /// The one rule for the root: if it exists it is a real folder, not a link (whatever the
    /// other calls do with it, none of them goes through one). A root that is not there yet is
    /// fine; it is created by the first write.
    pub fn check_root(&self) -> Result<(), StoreError> {
        match fs::symlink_metadata(&self.root) {
            Ok(meta) if meta.file_type().is_symlink() => {
                self.root_is_link.store(true, Ordering::SeqCst);
                Err(StoreError::RootIsLink)
            }
            Ok(meta) if meta.is_dir() => {
                self.root_is_link.store(false, Ordering::SeqCst);
                Ok(())
            }
            Ok(_) => Err(StoreError::NotAFile),
            Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(()),
            Err(source) => Err(StoreError::Io {
                what: "could not look at the folder",
                source,
            }),
        }
    }

    pub fn root(&self) -> &Path {
        &self.root
    }

    fn locked(&self) -> std::sync::MutexGuard<'_, ()> {
        self.lock
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    /// `<root>/<connection>`: the connection id, validated and lower-cased (ids are compared
    /// without regard to case, and a file system may do the same).
    fn connection_dir(&self, connection: &str) -> Result<PathBuf, StoreError> {
        let id = valid_id(connection).map_err(StoreError::BadId)?;
        Ok(self.root.join(id.to_ascii_lowercase()))
    }

    fn file_path(&self, connection: &str, id: &str) -> Result<PathBuf, StoreError> {
        let id = valid_id(id).map_err(StoreError::BadId)?;
        Ok(self.connection_dir(connection)?.join(format!("{id}.json")))
    }

    // ---- writing ------------------------------------------------------------------------------

    /// Writes `encoded` as the conversation `connection` / `encoded.id`: staged, then committed,
    /// then the connection's directory is kept within [`MAX_PER_CONNECTION`]. A file with more
    /// messages than `encoded` has is not replaced ([`StoreError::Stale`]: two writes of one
    /// conversation that raced, or a file that says more than it should).
    pub fn write(&self, connection: &str, encoded: &Encoded) -> Result<Written, StoreError> {
        let _guard = self.locked();
        let staged = self.stage_locked(connection, encoded)?;
        let dir = staged
            .target
            .parent()
            .map(Path::to_path_buf)
            .unwrap_or_else(|| self.root.clone());
        self.commit(staged)?;
        let pruned = self.prune_locked(&dir);
        Ok(Written { pruned })
    }

    /// The first half of a write: the temporary file, complete and flushed (tests make a crash
    /// between the two halves).
    #[cfg(test)]
    pub fn stage(&self, connection: &str, encoded: &Encoded) -> Result<Staged, StoreError> {
        let _guard = self.locked();
        self.stage_locked(connection, encoded)
    }

    fn stage_locked(&self, connection: &str, encoded: &Encoded) -> Result<Staged, StoreError> {
        let target = self.file_path(connection, &encoded.id)?;
        self.check_root()?;
        if encoded.bytes.len() > MAX_FILE_BYTES {
            return Err(StoreError::TooLarge);
        }
        if encoded.epoch != self.epoch(connection)
            || self
                .deleted
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner())
                .contains(&encoded.id)
        {
            return Err(StoreError::Detached);
        }
        let dir = target
            .parent()
            .map(Path::to_path_buf)
            .ok_or(StoreError::Missing)?;
        create_private_dir(&dir).map_err(StoreError::io("could not create the folder"))?;
        refuse_link(&dir)?;
        Self::remove_leftovers(&dir);
        if let Ok(existing) = read_prefix(&target) {
            if parse_header(&existing).is_ok_and(|header| header.seq > encoded.seq) {
                return Err(StoreError::Stale);
            }
        }
        let temp = dir.join(format!(
            ".{}.{}-{}.tmp",
            encoded.id,
            std::process::id(),
            NONCE.fetch_add(1, Ordering::Relaxed)
        ));
        let written = (|| -> io::Result<()> {
            let mut file = create_private_file(&temp)?;
            file.write_all(&encoded.bytes)?;
            file.sync_all()
        })();
        if let Err(source) = written {
            let _ = fs::remove_file(&temp);
            return Err(StoreError::Io {
                what: "could not write the conversation",
                source,
            });
        }
        Ok(Staged { temp, target })
    }

    /// The second half: the temporary file replaces the target in one step.
    pub fn commit(&self, staged: Staged) -> Result<(), StoreError> {
        let renamed = fs::rename(&staged.temp, &staged.target);
        if let Err(source) = renamed {
            let _ = fs::remove_file(&staged.temp);
            return Err(StoreError::Io {
                what: "could not replace the saved conversation",
                source,
            });
        }
        sync_dir(staged.target.parent());
        Ok(())
    }

    /// Removes temporary files a crash left behind (under the lock: no write of this process is
    /// between its staging and its commit).
    fn remove_leftovers(dir: &Path) {
        let Ok(entries) = fs::read_dir(dir) else {
            return;
        };
        for entry in entries.flatten() {
            let name = entry.file_name();
            let name = name.to_string_lossy();
            if name.starts_with('.') && name.ends_with(".tmp") {
                let _ = fs::remove_file(entry.path());
            }
        }
    }

    /// Deletes the least recently updated files beyond [`MAX_PER_CONNECTION`]. Returns how many.
    fn prune_locked(&self, dir: &Path) -> usize {
        let Ok(entries) = fs::read_dir(dir) else {
            return 0;
        };
        let paths: Vec<PathBuf> = entries
            .flatten()
            .filter(|entry| {
                let name = entry.file_name();
                let name = name.to_string_lossy();
                name.strip_suffix(".json")
                    .is_some_and(|stem| valid_id(stem).is_ok())
                    && entry.file_type().is_ok_and(|kind| kind.is_file())
            })
            .map(|entry| entry.path())
            .collect();
        // Only a directory over the cap has headers read: the common write costs one listing.
        if paths.len() <= MAX_PER_CONNECTION {
            return 0;
        }
        let mut files: Vec<(u64, PathBuf)> = paths
            .into_iter()
            .map(|path| (self.updated_of(&path), path))
            .collect();
        files.sort();
        let extra = files.len() - MAX_PER_CONNECTION;
        files
            .into_iter()
            .take(extra)
            .filter(|(_, path)| fs::remove_file(path).is_ok())
            .count()
    }

    /// When a file was last updated: its header says, else (a header that does not read, a
    /// newer format) the file system does.
    fn updated_of(&self, path: &Path) -> u64 {
        read_prefix(path)
            .ok()
            .and_then(|prefix| parse_header(&prefix).ok())
            .map(|header| header.updated)
            .or_else(|| {
                fs::metadata(path)
                    .and_then(|meta| meta.modified())
                    .ok()
                    .and_then(|time| time.duration_since(std::time::UNIX_EPOCH).ok())
                    .map(|since| u64::try_from(since.as_millis()).unwrap_or(u64::MAX))
            })
            .unwrap_or(0)
    }

    // ---- reading ------------------------------------------------------------------------------

    /// The headers of `connection`'s conversations, newest first, and the files that could not be
    /// read. Reads [`HEADER_READ`] bytes of each file at most and removes leftover temporary files.
    pub fn list(&self, connection: &str) -> Result<Listing, StoreError> {
        let dir = self.connection_dir(connection)?;
        let _guard = self.locked();
        self.check_root()?;
        let mut listing = Listing::default();
        match fs::symlink_metadata(&dir) {
            Ok(meta) if meta.is_dir() => {}
            Ok(_) => return Err(StoreError::NotAFile),
            Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(listing),
            Err(source) => {
                return Err(StoreError::Io {
                    what: "could not look at the folder",
                    source,
                })
            }
        }
        Self::remove_leftovers(&dir);
        let entries = fs::read_dir(&dir).map_err(StoreError::io("could not read the folder"))?;
        // Entries that are not conversations are skipped before the cap, so junk cannot hide any.
        let ours = entries.flatten().filter_map(|entry| {
            let name = entry.file_name().to_string_lossy().into_owned();
            let stem = name.strip_suffix(".json")?.to_owned();
            (valid_id(&stem).is_ok() && entry.file_type().is_ok_and(|kind| kind.is_file()))
                .then_some((entry, name, stem))
        });
        for (entry, name, stem) in ours.take(MAX_LISTED) {
            let bytes = entry.metadata().map_or(0, |meta| meta.len());
            let read = read_prefix(&entry.path())
                .map_err(StoreError::io("could not read the file"))
                .and_then(|prefix| parse_header(&prefix))
                .and_then(|header| {
                    if header.id == stem {
                        Ok(header)
                    } else {
                        Err(StoreError::Damaged(
                            "the id inside is not the file's".into(),
                        ))
                    }
                });
            match read {
                Ok(header) => listing.entries.push(Entry { header, bytes }),
                Err(error) => listing.problems.push(Problem {
                    file: name,
                    bytes,
                    why: error.to_string(),
                }),
            }
        }
        listing.entries.sort_by(|a, b| {
            b.header
                .updated
                .cmp(&a.header.updated)
                .then_with(|| a.header.id.cmp(&b.header.id))
        });
        listing.entries.truncate(MAX_PER_CONNECTION);
        Ok(listing)
    }

    /// Reads one conversation completely. Everything that is wrong with the file is an error;
    /// nothing is half loaded.
    pub fn load(&self, connection: &str, id: &str) -> Result<Loaded, StoreError> {
        let path = self.file_path(connection, id)?;
        self.check_root()?;
        match fs::symlink_metadata(&path) {
            Ok(meta) if meta.is_file() => {
                if meta.len() > u64::try_from(MAX_FILE_BYTES).unwrap_or(u64::MAX) {
                    return Err(StoreError::TooLarge);
                }
            }
            Ok(_) => return Err(StoreError::NotAFile),
            Err(error) if error.kind() == io::ErrorKind::NotFound => {
                return Err(StoreError::Missing)
            }
            Err(source) => {
                return Err(StoreError::Io {
                    what: "could not look at the file",
                    source,
                })
            }
        }
        let bytes = fs::read(&path).map_err(StoreError::io("could not read the file"))?;
        let header = parse_header(&bytes[..bytes.len().min(HEADER_READ)])?;
        if header.id != id {
            return Err(StoreError::Damaged(
                "the id inside is not the file's".into(),
            ));
        }
        #[derive(Deserialize)]
        struct Body {
            conversation: Conversation,
        }
        let Body { conversation } = serde_json::from_slice(&bytes)
            .map_err(|error| StoreError::Damaged(error.to_string()))?;
        match conversation.scope() {
            Some(scope) if scope.connection_id.eq_ignore_ascii_case(connection) => {}
            _ => {
                return Err(StoreError::Damaged(
                    "the conversation is about another connection".into(),
                ))
            }
        }
        // What the file says about its level is not trusted below what the conversation shows.
        // Nor is its message count: a wrong one would refuse every later write as stale, or
        // ask before every replace.
        let header = Header {
            access: header.access.max(access_floor(&conversation)),
            seq: conversation.messages().len() as u64,
            ..header
        };
        Ok(Loaded {
            header,
            conversation,
        })
    }

    // ---- deleting -----------------------------------------------------------------------------

    /// Deletes one conversation; whether there was one. Its id is not written again by this
    /// process (a run still going that holds it) once a file was removed: a delete that finds
    /// nothing (a stale row, another connection's id) marks nothing.
    pub fn delete(&self, connection: &str, id: &str) -> Result<bool, StoreError> {
        let path = self.file_path(connection, id)?;
        let _guard = self.locked();
        self.check_root()?;
        match fs::remove_file(&path) {
            Ok(()) => {
                self.deleted
                    .lock()
                    .unwrap_or_else(|poisoned| poisoned.into_inner())
                    .insert(id.to_owned());
                Ok(true)
            }
            Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(false),
            Err(source) => Err(StoreError::Io {
                what: "could not delete the conversation",
                source,
            }),
        }
    }

    /// Deletes everything kept for `connection` (the connection itself was deleted, or the user
    /// asked). Only what this module wrote goes: `<valid id>.json` files and its temporary files,
    /// then the folder if it is empty. Writes that were started before this are refused.
    pub fn delete_connection(&self, connection: &str) -> Result<DeleteReport, StoreError> {
        let dir = self.connection_dir(connection)?;
        let _guard = self.locked();
        self.check_root()?;
        self.bump_connection(connection);
        let mut report = DeleteReport::default();
        clear_connection_dir(&dir, &mut report);
        Ok(report)
    }

    /// Deletes every conversation of every connection, as [`Self::delete_connection`] does for
    /// each folder of the root whose name is a valid connection id. A failure does not stop the
    /// others: the report has the count and the first error. A root that is itself a link is
    /// refused, and nothing outside it is touched.
    pub fn delete_all(&self) -> Result<DeleteReport, StoreError> {
        let _guard = self.locked();
        self.epoch.fetch_add(1, Ordering::SeqCst);
        self.check_root()?;
        if !self.root.exists() {
            return Ok(DeleteReport::default());
        }
        let entries =
            fs::read_dir(&self.root).map_err(StoreError::io("could not read the folder"))?;
        let mut report = DeleteReport::default();
        for entry in entries.flatten() {
            let name = entry.file_name();
            if valid_id(&name.to_string_lossy()).is_ok() {
                clear_connection_dir(&entry.path(), &mut report);
            }
        }
        Ok(report)
    }
}

/// What a delete of many conversations did: how many files went, and the first thing that went
/// wrong (the others were still tried).
#[derive(Debug, Default)]
pub struct DeleteReport {
    pub deleted: usize,
    pub error: Option<StoreError>,
}

impl DeleteReport {
    fn failed(&mut self, error: StoreError) {
        self.error.get_or_insert(error);
    }
}

/// Removes the conversations and temporary files this module wrote in one connection's folder
/// (a real folder: a link or a file in its place is not ours), then the folder if it is empty.
fn clear_connection_dir(dir: &Path, report: &mut DeleteReport) {
    match fs::symlink_metadata(dir) {
        Ok(meta) if meta.is_dir() => {}
        Ok(_) => return,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return,
        Err(source) => {
            return report.failed(StoreError::Io {
                what: "could not look at the folder",
                source,
            })
        }
    }
    let entries = match fs::read_dir(dir) {
        Ok(entries) => entries,
        Err(source) => {
            return report.failed(StoreError::Io {
                what: "could not read the folder",
                source,
            })
        }
    };
    for entry in entries.flatten() {
        // A regular file only: a link is not what this module writes.
        if !entry.file_type().is_ok_and(|kind| kind.is_file()) {
            continue;
        }
        let name = entry.file_name().to_string_lossy().into_owned();
        let conversation = name
            .strip_suffix(".json")
            .is_some_and(|stem| valid_id(stem).is_ok());
        let leftover = name.starts_with('.') && name.ends_with(".tmp");
        if !(conversation || leftover) {
            continue;
        }
        match fs::remove_file(entry.path()) {
            Ok(()) => report.deleted += usize::from(conversation),
            Err(source) => report.failed(StoreError::Io {
                what: "could not delete a conversation",
                source,
            }),
        }
    }
    // Empty now, or something else lives there: either way not an error.
    let _ = fs::remove_dir(dir);
}

/// At most [`HEADER_READ`] bytes from the start of `path` (a symlink is not followed).
fn read_prefix(path: &Path) -> io::Result<Vec<u8>> {
    if fs::symlink_metadata(path)?.file_type().is_symlink() {
        return Err(io::Error::new(io::ErrorKind::InvalidInput, "a link"));
    }
    let mut prefix = Vec::with_capacity(1024);
    fs::File::open(path)?
        .take(HEADER_READ as u64)
        .read_to_end(&mut prefix)?;
    Ok(prefix)
}

/// A symlink in place of the connection's directory is not followed.
fn refuse_link(dir: &Path) -> Result<(), StoreError> {
    match fs::symlink_metadata(dir) {
        Ok(meta) if meta.is_dir() => Ok(()),
        Ok(_) => Err(StoreError::NotAFile),
        Err(source) => Err(StoreError::Io {
            what: "could not look at the folder",
            source,
        }),
    }
}

fn create_private_dir(dir: &Path) -> io::Result<()> {
    let mut builder = fs::DirBuilder::new();
    builder.recursive(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::DirBuilderExt;
        builder.mode(0o700);
    }
    builder.create(dir)
}

fn create_private_file(path: &Path) -> io::Result<fs::File> {
    let mut options = fs::OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    options.open(path)
}

/// Makes the rename itself durable where the platform allows it (a directory can be opened and
/// flushed on Unix); a failure here costs durability, not correctness.
fn sync_dir(dir: Option<&Path>) {
    #[cfg(unix)]
    if let Some(dir) = dir {
        if let Ok(handle) = fs::File::open(dir) {
            let _ = handle.sync_all();
        }
    }
    #[cfg(not(unix))]
    let _ = dir;
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    const CONNECTION: &str = "22222222-2222-4222-8222-222222222222";

    fn temp_store() -> (tempfile::TempDir, ConversationStore) {
        let dir = tempfile::tempdir().unwrap();
        let store = ConversationStore::new(dir.path().join("assistant").join("conversations"));
        (dir, store)
    }

    fn conversation(text: &str) -> Conversation {
        serde_json::from_value(json!({
            "scope": {"connection_id": CONNECTION, "database": "warehouse", "schema": "public"},
            "context": null,
            "messages": [
                {"role": "user", "content": [{"type": "text", "text": text}]},
                {"role": "assistant", "content": [{"type": "text", "text": "Done."}]}
            ]
        }))
        .unwrap()
    }

    fn header(id: &str, updated: u64) -> Header {
        Header {
            updated,
            created: 1,
            ..Header::start(
                id.into(),
                title_of("how many orders?"),
                "anthropic",
                "claude-test",
                AgentAccess::Read,
            )
        }
    }

    fn put(store: &ConversationStore, id: &str, updated: u64) -> Written {
        let encoded = encode(&header(id, updated), &conversation("how many orders?")).unwrap();
        store.write(CONNECTION, &encoded).unwrap()
    }

    #[test]
    fn a_conversation_comes_back_exactly_and_the_file_has_the_documented_shape() {
        let (_dir, store) = temp_store();
        let original = conversation("how many orders?");
        let head = header("c1", 5);
        store
            .write(CONNECTION, &encode(&head, &original).unwrap())
            .unwrap();
        let loaded = store.load(CONNECTION, "c1").unwrap();
        assert_eq!(loaded.conversation, original);
        assert_eq!(
            loaded.header,
            Header { seq: 2, ..head },
            "the header, with the message count the encoder adds"
        );
        assert_eq!(
            serde_json::to_string(&loaded.conversation).unwrap(),
            serde_json::to_string(&original).unwrap(),
            "byte for byte through JSON"
        );
        // Header first, on its own line; the whole file is one JSON object.
        let text = fs::read_to_string(store.file_path(CONNECTION, "c1").unwrap()).unwrap();
        let (first, rest) = text.split_once('\n').unwrap();
        assert!(
            first.starts_with("{\"format\":1,\"id\":\"c1\",\"title\":"),
            "{first}"
        );
        assert!(first.ends_with(','));
        assert!(rest.starts_with("\"conversation\":{"));
        let whole: serde_json::Value = serde_json::from_str(&text).unwrap();
        assert_eq!(whole["access"], "read");
    }

    #[test]
    fn listing_reads_only_headers_newest_first() {
        let (_dir, store) = temp_store();
        put(&store, "old", 10);
        put(&store, "new", 30);
        put(&store, "mid", 20);
        let listing = store.list(CONNECTION).unwrap();
        let ids: Vec<_> = listing
            .entries
            .iter()
            .map(|e| e.header.id.as_str())
            .collect();
        assert_eq!(ids, ["new", "mid", "old"]);
        assert!(listing.problems.is_empty());
        // A body that is not JSON at all does not matter to a listing: only the prefix is read.
        let path = store.file_path(CONNECTION, "mid").unwrap();
        let text = fs::read_to_string(&path).unwrap();
        let (first, _) = text.split_once('\n').unwrap();
        fs::write(&path, format!("{first}\n\"conversation\":{{ not json")).unwrap();
        assert_eq!(store.list(CONNECTION).unwrap().entries.len(), 3);
        // ... and fails when it is opened.
        assert!(matches!(
            store.load(CONNECTION, "mid"),
            Err(StoreError::Damaged(_))
        ));
    }

    #[test]
    fn a_crash_between_the_temporary_file_and_the_rename_leaves_the_old_file() {
        let (_dir, store) = temp_store();
        put(&store, "c1", 10);
        let before = store.load(CONNECTION, "c1").unwrap();
        let newer = encode(&header("c1", 99), &conversation("a different question")).unwrap();
        let staged = store.stage(CONNECTION, &newer).unwrap();
        // "Crash": the staged file is never committed.
        drop(staged);
        let dir = store.connection_dir(CONNECTION).unwrap();
        let leftovers: Vec<_> = fs::read_dir(&dir)
            .unwrap()
            .flatten()
            .filter(|e| e.file_name().to_string_lossy().ends_with(".tmp"))
            .collect();
        assert_eq!(leftovers.len(), 1, "the temporary file is on disk");
        assert_eq!(
            store.load(CONNECTION, "c1").unwrap().conversation,
            before.conversation
        );
        let ids: Vec<_> = store
            .list(CONNECTION)
            .unwrap()
            .entries
            .iter()
            .map(|e| e.header.id.clone())
            .collect();
        assert_eq!(ids, ["c1"], "the temporary file is not a conversation");
        assert!(
            !fs::read_dir(&dir)
                .unwrap()
                .flatten()
                .any(|e| e.file_name().to_string_lossy().ends_with(".tmp")),
            "listing cleaned it"
        );
        // A leftover in front of the next write is cleaned by it as well.
        fs::write(dir.join(".x.1-1.tmp"), b"junk").unwrap();
        put(&store, "c1", 11);
        assert!(!dir.join(".x.1-1.tmp").exists());
    }

    #[test]
    fn a_staged_file_commits_into_place() {
        let (_dir, store) = temp_store();
        put(&store, "c1", 10);
        let newer = encode(&header("c1", 99), &conversation("second")).unwrap();
        let staged = store.stage(CONNECTION, &newer).unwrap();
        store.commit(staged).unwrap();
        assert_eq!(
            store.list(CONNECTION).unwrap().entries[0].header.updated,
            99
        );
    }

    /// A conversation with two more turns than [`conversation`].
    fn longer(text: &str) -> Conversation {
        serde_json::from_value(json!({
            "scope": {"connection_id": CONNECTION},
            "context": null,
            "messages": [
                {"role": "user", "content": [{"type": "text", "text": text}]},
                {"role": "assistant", "content": [{"type": "text", "text": "One."}]},
                {"role": "user", "content": [{"type": "text", "text": "and?"}]},
                {"role": "assistant", "content": [{"type": "text", "text": "Two."}]}
            ]
        }))
        .unwrap()
    }

    #[test]
    fn a_write_with_fewer_messages_does_not_replace_one_with_more_whatever_the_clock_says() {
        let (_dir, store) = temp_store();
        let newer = encode(&header("c1", 50), &longer("q")).unwrap();
        store.write(CONNECTION, &newer).unwrap();
        // A later clock reading, fewer messages: older.
        let older = encode(&header("c1", 9_999_999), &conversation("older")).unwrap();
        assert!(matches!(
            store.write(CONNECTION, &older),
            Err(StoreError::Stale)
        ));
        // An earlier clock reading (the clock was set back), more messages: newer.
        let mut head = header("c1", 1);
        head.created = 0;
        let grown = serde_json::from_value::<Conversation>(json!({
            "scope": {"connection_id": CONNECTION},
            "context": null,
            "messages": [
                {"role": "user", "content": [{"type": "text", "text": "q"}]},
                {"role": "assistant", "content": [{"type": "text", "text": "One."}]},
                {"role": "user", "content": [{"type": "text", "text": "and?"}]},
                {"role": "assistant", "content": [{"type": "text", "text": "Two."}]},
                {"role": "user", "content": [{"type": "text", "text": "more"}]},
                {"role": "assistant", "content": [{"type": "text", "text": "Three."}]}
            ]
        }))
        .unwrap();
        store
            .write(CONNECTION, &encode(&head, &grown).unwrap())
            .unwrap();
        assert_eq!(
            store
                .load(CONNECTION, "c1")
                .unwrap()
                .conversation
                .messages()
                .len(),
            6
        );
    }

    #[test]
    fn a_delete_refuses_the_writes_that_began_before_it() {
        let (_dir, store) = temp_store();
        let begun = store.epoch(CONNECTION);
        let mut late = encode(&header("c1", 5), &conversation("q")).unwrap();
        late.epoch = begun;
        store.write(CONNECTION, &late).unwrap();
        // One conversation deleted: its id is not written again.
        assert!(store.delete(CONNECTION, "c1").unwrap());
        assert!(matches!(
            store.write(CONNECTION, &late),
            Err(StoreError::Detached)
        ));
        // A connection or everything deleted: no write of a run that began before.
        let mut other = encode(&header("c2", 5), &conversation("q")).unwrap();
        other.epoch = store.epoch(CONNECTION);
        store.write(CONNECTION, &other).unwrap();
        assert_eq!(store.delete_connection(CONNECTION).unwrap().deleted, 1);
        assert!(matches!(
            store.write(CONNECTION, &other),
            Err(StoreError::Detached)
        ));
        assert!(
            !store.connection_dir(CONNECTION).unwrap().exists(),
            "nothing recreated"
        );
        other.epoch = store.epoch(CONNECTION);
        store.write(CONNECTION, &other).unwrap();
        store.delete_all().unwrap();
        assert!(matches!(
            store.write(CONNECTION, &other),
            Err(StoreError::Detached)
        ));
    }

    #[test]
    fn a_delete_of_one_connection_refuses_the_writes_of_that_connection_only() {
        const OTHER: &str = "33333333-3333-4333-8333-333333333333";
        let (_dir, store) = temp_store();
        let mut on_other = encode(&header("c9", 5), &conversation("q")).unwrap();
        on_other.epoch = store.epoch(OTHER);
        let mut on_first = encode(&header("c1", 5), &conversation("q")).unwrap();
        on_first.epoch = store.epoch(CONNECTION);
        // Everything of the first connection goes while both runs are in flight.
        store.delete_connection(CONNECTION).unwrap();
        assert!(matches!(
            store.write(CONNECTION, &on_first),
            Err(StoreError::Detached)
        ));
        store
            .write(OTHER, &on_other)
            .expect("a delete of another connection does not refuse this one");
        // A delete of everything refuses every run that began before it.
        let mut late = encode(&header("c8", 5), &conversation("q")).unwrap();
        late.epoch = store.epoch(OTHER);
        store.delete_all().unwrap();
        assert!(matches!(
            store.write(OTHER, &late),
            Err(StoreError::Detached)
        ));
    }

    #[test]
    fn a_delete_that_removed_nothing_marks_nothing_for_a_conversation_written_later() {
        const OTHER: &str = "33333333-3333-4333-8333-333333333333";
        let (_dir, store) = temp_store();
        // Another connection's id on a stale row, or an id that was never written: nothing is
        // deleted, so nothing is marked and the conversation may be written afterwards.
        assert!(!store.delete(OTHER, "c1").unwrap());
        let encoded = encode(&header("c1", 5), &conversation("q")).unwrap();
        store.write(CONNECTION, &encoded).expect("not refused");
        assert!(store.delete(CONNECTION, "c1").unwrap());
        assert!(matches!(
            store.write(CONNECTION, &encoded),
            Err(StoreError::Detached)
        ));
    }

    #[test]
    fn what_a_file_holds_raises_the_level_its_header_claims() {
        let (_dir, store) = temp_store();
        // A header that says schema_only over a conversation that holds rows.
        let rows = serde_json::from_value::<Conversation>(json!({
            "scope": {"connection_id": CONNECTION}, "context": null,
            "messages": [
                {"role": "user", "content": [{"type": "text", "text": "q"}]},
                {"role": "assistant", "content": [{"type": "tool_use", "id": "t", "name": "execute_query", "input": {}}]},
                {"role": "user", "content": [{"type": "tool_result", "tool_use_id": "t",
                    "content": "{\"columns\":[{\"name\":\"a\",\"type\":\"int\"}],\"rows\":[[\"1\"]],\"row_count\":1}"}]},
                {"role": "assistant", "content": [{"type": "text", "text": "One row."}]}
            ]
        }))
        .unwrap();
        let mut head = header("c1", 5);
        head.access = AgentAccess::SchemaOnly;
        store
            .write(CONNECTION, &encode(&head, &rows).unwrap())
            .unwrap();
        assert_eq!(
            store.load(CONNECTION, "c1").unwrap().header.access,
            AgentAccess::Read
        );
        assert_eq!(access_floor(&rows), AgentAccess::Read);
        // A write that returned no result set, or reported rows, is read-write.
        for result in [
            "{\"columns\":[],\"rows\":[],\"row_count\":0}",
            "{\"affected_rows\":3}",
        ] {
            let write = serde_json::from_value::<Conversation>(json!({
                "scope": {"connection_id": CONNECTION}, "context": null,
                "messages": [
                    {"role": "user", "content": [{"type": "text", "text": "q"}]},
                    {"role": "assistant", "content": [{"type": "tool_use", "id": "t", "name": "execute_query", "input": {}}]},
                    {"role": "user", "content": [{"type": "tool_result", "tool_use_id": "t", "content": result}]},
                    {"role": "assistant", "content": [{"type": "text", "text": "Done."}]}
                ]
            }))
            .unwrap();
            assert_eq!(access_floor(&write), AgentAccess::ReadWrite, "{result}");
        }
        // Schema tools and failed statements add nothing: a schema listing, a statement whose
        // result is an error, and a tool of a name that is not one of the engine's.
        let nothing = serde_json::from_value::<Conversation>(json!({
            "scope": {"connection_id": CONNECTION}, "context": null,
            "messages": [
                {"role": "user", "content": [{"type": "text", "text": "q"}]},
                {"role": "assistant", "content": [
                    {"type": "tool_use", "id": "s", "name": "list_tables", "input": {}},
                    {"type": "tool_use", "id": "e", "name": "execute_query", "input": {}},
                    {"type": "tool_use", "id": "x", "name": "not_a_tool", "input": {}}
                ]},
                {"role": "user", "content": [
                    {"type": "tool_result", "tool_use_id": "s",
                     "content": "{\"tables\":[{\"name\":\"orders\"}]}"},
                    {"type": "tool_result", "tool_use_id": "e", "is_error": true,
                     "content": "{\"affected_rows\":3}"},
                    {"type": "tool_result", "tool_use_id": "x",
                     "content": "{\"affected_rows\":3}"}
                ]},
                {"role": "assistant", "content": [{"type": "text", "text": "Done."}]}
            ]
        }))
        .unwrap();
        assert_eq!(access_floor(&nothing), AgentAccess::Off);
        // A Redis command's reply is a read, and one that reports affected rows a write.
        let redis = |result: &str| {
            serde_json::from_value::<Conversation>(json!({
                "scope": {"connection_id": CONNECTION}, "context": null,
                "messages": [
                    {"role": "user", "content": [{"type": "text", "text": "q"}]},
                    {"role": "assistant", "content": [{"type": "tool_use", "id": "t",
                        "name": "execute_redis_command", "input": {}}]},
                    {"role": "user", "content": [{"type": "tool_result", "tool_use_id": "t",
                        "content": result}]},
                    {"role": "assistant", "content": [{"type": "text", "text": "Done."}]}
                ]
            }))
            .unwrap()
        };
        assert_eq!(
            access_floor(&redis("{\"reply\":\"PONG\"}")),
            AgentAccess::Read
        );
        assert_eq!(
            access_floor(&redis("{\"affected_rows\":1}")),
            AgentAccess::ReadWrite
        );
    }

    #[test]
    fn junk_in_the_folder_cannot_hide_conversations_from_a_listing() {
        let (_dir, store) = temp_store();
        put(&store, "real", 1);
        let dir = store.connection_dir(CONNECTION).unwrap();
        for n in 0..(MAX_LISTED + 50) {
            fs::write(dir.join(format!("junk {n}.txt")), b"x").unwrap();
        }
        assert_eq!(store.list(CONNECTION).unwrap().entries.len(), 1);
    }

    #[test]
    fn delete_all_removes_only_what_the_store_wrote_and_carries_on_past_a_failure() {
        let (dir, store) = temp_store();
        let other = "33333333-3333-4333-8333-333333333333";
        put(&store, "a", 1);
        store
            .write(other, &encode(&header("z", 1), &conversation("q")).unwrap())
            .unwrap();
        // Things this module did not write: a stranger's file, a nested folder, a folder with
        // a name that is no connection id.
        let mine = store.connection_dir(CONNECTION).unwrap();
        fs::write(mine.join("notes.txt"), b"mine").unwrap();
        fs::create_dir_all(mine.join("nested")).unwrap();
        fs::create_dir_all(store.root().join("not a connection")).unwrap();
        fs::write(store.root().join("not a connection").join("x.json"), b"x").unwrap();
        let report = store.delete_all().unwrap();
        assert_eq!((report.deleted, report.error.is_none()), (2, true));
        assert!(mine.join("notes.txt").exists() && mine.join("nested").exists());
        assert!(
            !store.connection_dir(other).unwrap().exists(),
            "an empty folder goes"
        );
        assert!(store
            .root()
            .join("not a connection")
            .join("x.json")
            .exists());
        let _ = dir;
    }

    #[cfg(unix)]
    #[test]
    fn a_root_that_is_a_link_is_used_for_nothing_at_all() {
        use std::os::unix::fs::symlink;
        let (dir, _) = temp_store();
        // A real store with a conversation, and a link to its folder where another store's root is.
        let target = dir.path().join("elsewhere");
        let real = ConversationStore::new(target.clone());
        put(&real, "kept", 1);
        let victim = target.join(CONNECTION);
        let linked = ConversationStore::new(dir.path().join("link"));
        symlink(&target, linked.root()).unwrap();
        assert!(!linked.root_is_link(), "unknown until something looks");

        let encoded = encode(&header("new", 5), &conversation("q")).unwrap();
        assert!(matches!(
            linked.write(CONNECTION, &encoded),
            Err(StoreError::RootIsLink)
        ));
        assert!(matches!(
            linked.list(CONNECTION),
            Err(StoreError::RootIsLink)
        ));
        assert!(matches!(
            linked.load(CONNECTION, "kept"),
            Err(StoreError::RootIsLink)
        ));
        assert!(matches!(
            linked.delete(CONNECTION, "kept"),
            Err(StoreError::RootIsLink)
        ));
        assert!(matches!(
            linked.delete_connection(CONNECTION),
            Err(StoreError::RootIsLink)
        ));
        assert!(matches!(linked.delete_all(), Err(StoreError::RootIsLink)));
        assert!(linked.root_is_link());
        // Nothing was written through the link and nothing behind it was touched.
        assert!(victim.join("kept.json").exists());
        assert!(!victim.join("new.json").exists());
        assert_eq!(real.list(CONNECTION).unwrap().entries.len(), 1);
    }

    #[cfg(unix)]
    #[test]
    fn delete_all_reports_the_first_error_after_clearing_the_rest() {
        use std::os::unix::fs::PermissionsExt;
        let (_dir, store) = temp_store();
        put(&store, "a", 1);
        let other = "33333333-3333-4333-8333-333333333333";
        store
            .write(other, &encode(&header("z", 1), &conversation("q")).unwrap())
            .unwrap();
        let locked = store.connection_dir(other).unwrap();
        fs::set_permissions(&locked, fs::Permissions::from_mode(0o500)).unwrap();
        // A user who can write anywhere (root) cannot make this fail: say so and stop, rather
        // than assert something else.
        let probe = locked.join("probe");
        let blocked = fs::File::create(&probe).is_err();
        let _ = fs::remove_file(&probe);
        let report = store.delete_all().unwrap();
        fs::set_permissions(&locked, fs::Permissions::from_mode(0o700)).unwrap();
        if !blocked {
            eprintln!("skipped: this user can write into a read-only folder");
            return;
        }
        assert_eq!(
            report.deleted, 1,
            "the other folder was cleared, whatever the order"
        );
        assert!(report.error.is_some(), "and the failure is reported");
        assert!(!store.connection_dir(CONNECTION).unwrap().exists());
    }

    #[test]
    fn a_file_over_eight_mib_is_not_written_and_the_old_one_stays() {
        let (_dir, store) = temp_store();
        put(&store, "c1", 10);
        let big = conversation(&"x".repeat(MAX_FILE_BYTES));
        assert!(matches!(
            encode(&header("c1", 20), &big),
            Err(StoreError::TooLarge)
        ));
        // Just under the limit is fine.
        let fits = conversation(&"x".repeat(MAX_FILE_BYTES - 4096));
        let encoded = encode(&header("c1", 20), &fits).unwrap();
        assert!(encoded.bytes.len() < MAX_FILE_BYTES);
        assert_eq!(
            store.list(CONNECTION).unwrap().entries[0].header.updated,
            10
        );
    }

    #[test]
    fn only_fifty_stay_and_the_least_recently_updated_go() {
        let (_dir, store) = temp_store();
        for n in 0..MAX_PER_CONNECTION as u64 {
            put(&store, &format!("c{n:02}"), 100 + n);
        }
        let written = put(&store, "newest", 1000);
        assert_eq!(written.pruned, 1);
        let listing = store.list(CONNECTION).unwrap();
        assert_eq!(listing.entries.len(), MAX_PER_CONNECTION);
        assert_eq!(listing.entries[0].header.id, "newest");
        assert!(
            !listing.entries.iter().any(|e| e.header.id == "c00"),
            "the one updated longest ago is gone"
        );
        assert!(listing.entries.iter().any(|e| e.header.id == "c01"));
    }

    #[test]
    fn a_file_that_does_not_read_is_listed_as_a_problem_and_never_panics() {
        let (_dir, store) = temp_store();
        put(&store, "good", 10);
        let dir = store.connection_dir(CONNECTION).unwrap();
        let good = fs::read_to_string(dir.join("good.json")).unwrap();
        let cases: Vec<(&str, Vec<u8>)> = vec![
            ("empty", Vec::new()),
            ("garbage", b"\x00\xff not json at all".to_vec()),
            (
                "no_newline",
                b"{\"format\":1,\"id\":\"no_newline\"}".to_vec(),
            ),
            (
                "cut_header",
                b"{\"format\":1,\"id\":\"cut_header\"\n\"conversation\":{}}".to_vec(),
            ),
            (
                "newer",
                b"{\"format\":2,\"id\":\"newer\",\"anything\":[1,2],\n\"conversation\":{}}"
                    .to_vec(),
            ),
            (
                "other_id",
                good.replace("\"id\":\"good\"", "\"id\":\"someone\"")
                    .into_bytes(),
            ),
            (
                "bad_access",
                good.replace("\"id\":\"good\"", "\"id\":\"bad_access\"")
                    .replace("\"access\":\"read\"", "\"access\":\"root\"")
                    .into_bytes(),
            ),
        ];
        for (name, bytes) in &cases {
            fs::write(dir.join(format!("{name}.json")), bytes).unwrap();
        }
        // Files that are not ours are not even looked at.
        fs::write(dir.join("notes.txt"), b"x").unwrap();
        fs::write(dir.join("bad name.json"), b"x").unwrap();
        let listing = store.list(CONNECTION).unwrap();
        assert_eq!(listing.entries.len(), 1);
        let mut files: Vec<_> = listing.problems.iter().map(|p| p.file.clone()).collect();
        files.sort();
        let mut expected: Vec<_> = cases.iter().map(|(n, _)| format!("{n}.json")).collect();
        expected.sort();
        assert_eq!(files, expected);
        assert!(listing
            .problems
            .iter()
            .any(|p| p.file == "newer.json" && p.why.contains("newer version")));
        // Opening each of them fails cleanly too.
        for (name, _) in &cases {
            assert!(store.load(CONNECTION, name).is_err(), "{name}");
        }
    }

    #[test]
    fn a_conversation_the_engine_refuses_or_another_connections_is_not_loaded() {
        let (_dir, store) = temp_store();
        put(&store, "good", 10);
        let dir = store.connection_dir(CONNECTION).unwrap();
        let text = fs::read_to_string(dir.join("good.json")).unwrap();
        // Two user messages in a row: the engine's own validation refuses it.
        let refused = text
            .replace("\"id\":\"good\"", "\"id\":\"refused\"")
            .replace("\"role\":\"assistant\"", "\"role\":\"user\"");
        fs::write(dir.join("refused.json"), refused).unwrap();
        assert!(matches!(
            store.load(CONNECTION, "refused"),
            Err(StoreError::Damaged(_))
        ));
        let elsewhere = text
            .replace("\"id\":\"good\"", "\"id\":\"elsewhere\"")
            .replace(CONNECTION, "99999999-9999-4999-8999-999999999999");
        fs::write(dir.join("elsewhere.json"), elsewhere).unwrap();
        assert!(matches!(
            store.load(CONNECTION, "elsewhere"),
            Err(StoreError::Damaged(why)) if why.contains("another connection")
        ));
        assert!(store.load(CONNECTION, "good").is_ok());
    }

    #[test]
    fn an_id_that_could_leave_the_folder_is_refused_everywhere() {
        let (dir, store) = temp_store();
        let outside = dir.path().join("secret.json");
        fs::write(&outside, b"do not touch").unwrap();
        let long = "a".repeat(MAX_ID_CHARS + 1);
        let bad = [
            "", "..", ".", "../x", "a/b", "a\\b", "a b", "a.json", "nul", "COM1", "lpt9", "x\0y",
            "\u{202e}", &long,
        ];
        let encoded = encode(&header("c1", 1), &conversation("q")).unwrap();
        for id in bad {
            assert!(valid_id(id).is_err(), "{id:?} must not be an id");
            assert!(store.list(id).is_err(), "list {id:?}");
            assert!(store.load(id, "c1").is_err(), "load {id:?}");
            assert!(store.load(CONNECTION, id).is_err(), "load id {id:?}");
            assert!(store.delete(id, "c1").is_err());
            assert!(store.delete(CONNECTION, id).is_err());
            assert!(store.delete_connection(id).is_err());
            assert!(store.write(id, &encoded).is_err(), "write {id:?}");
            let wrong = Encoded {
                id: id.to_owned(),
                seq: 0,
                epoch: Epoch::default(),
                bytes: Vec::new(),
            };
            assert!(store.write(CONNECTION, &wrong).is_err(), "write as {id:?}");
        }
        assert!(valid_id("22222222-2222-4222-8222-222222222222").is_ok());
        assert!(valid_id("Abc_123-x").is_ok());
        assert_eq!(fs::read(&outside).unwrap(), b"do not touch");
        assert!(!store.root().exists() || fs::read_dir(store.root()).unwrap().next().is_none());
    }

    #[cfg(unix)]
    #[test]
    fn files_are_private_and_a_link_is_not_followed() {
        use std::os::unix::fs::{symlink, PermissionsExt};
        let (dir, store) = temp_store();
        put(&store, "c1", 10);
        let path = store.file_path(CONNECTION, "c1").unwrap();
        assert_eq!(
            fs::metadata(&path).unwrap().permissions().mode() & 0o777,
            0o600
        );
        assert_eq!(
            fs::metadata(path.parent().unwrap())
                .unwrap()
                .permissions()
                .mode()
                & 0o777,
            0o700
        );
        // A symlink where a file should be: not listed as a conversation, not loaded, and
        // nothing behind it is read.
        let outside = dir.path().join("outside.json");
        fs::write(&outside, fs::read(&path).unwrap()).unwrap();
        symlink(&outside, path.parent().unwrap().join("linked.json")).unwrap();
        assert!(matches!(
            store.load(CONNECTION, "linked"),
            Err(StoreError::NotAFile)
        ));
        assert_eq!(store.list(CONNECTION).unwrap().entries.len(), 1);
        // Deleting everything leaves a link alone, and never touches what it points at.
        assert_eq!(store.delete_all().unwrap().deleted, 1);
        assert!(outside.exists());
        assert!(path.parent().unwrap().join("linked.json").exists());
    }

    #[test]
    fn deleting_one_a_connection_or_everything_counts_what_went() {
        let (_dir, store) = temp_store();
        let other = "33333333-3333-4333-8333-333333333333";
        put(&store, "a", 1);
        put(&store, "b", 2);
        put(&store, "c", 3);
        let elsewhere = encode(&header("z", 1), &conversation("q")).unwrap();
        store.write(other, &elsewhere).unwrap();
        assert!(store.delete(CONNECTION, "a").unwrap());
        assert!(!store.delete(CONNECTION, "a").unwrap(), "already gone");
        assert_eq!(store.delete_connection(CONNECTION).unwrap().deleted, 2);
        assert!(!store.connection_dir(CONNECTION).unwrap().exists());
        assert_eq!(
            store.list(other).unwrap().entries.len(),
            1,
            "the other one stays"
        );
        assert_eq!(store.delete_all().unwrap().deleted, 1);
        assert_eq!(
            store.delete_all().unwrap().deleted,
            0,
            "nothing left, and no root is no error"
        );
    }

    #[test]
    fn a_title_is_one_clean_line_of_at_most_eighty_characters() {
        assert_eq!(
            title_of("  how many\n\torders\r\nare there? "),
            "how many orders are there?"
        );
        let long = title_of(&"word ".repeat(40));
        assert_eq!(long.chars().count(), TITLE_CHARS + 1);
        assert!(long.ends_with('\u{2026}'));
        assert_eq!(
            title_of("a\u{202e}b\u{200b}c\u{7}"),
            "abc\\x07",
            "bidi and zero-width go, controls are escaped"
        );
        assert_eq!(title_of(""), "");
        // A header read from a file is tidied the same way.
        let mut head = header("c1", 1);
        head.title = format!("{}\n{}", "t".repeat(500), "\u{202e}");
        let tidy = head.tidy();
        assert_eq!(tidy.title.chars().count(), TITLE_CHARS + 1);
    }
}
