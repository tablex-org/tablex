//! Conversations kept on this computer: one row each in the app database, `tablex.db`
//! (`assistant_conversations`, see [`engine_storage::documents`]).
//!
//! A row is `(connection id, conversation id)`; its `body` is the document below, and `header`
//! repeats the first line of it as JSON, so drawing the History list reads no body. Before the
//! database each conversation was a file `<app data dir>/assistant/conversations/<connection
//! id>/<conversation id>.json`: the first use moves those in (the ones that read; see
//! [`engine_storage::documents`]) and leaves them as they were. They are not written any more.
//! The ones that could not be moved in (unreadable, too large, damaged) are listed as problems and
//! never tried again. A conversation can hold query results, so deleting one, a connection's or
//! all of them deletes the old files of them too.
//!
//! # The document
//!
//! ```text
//! {"format":1,"id":"...","title":"...","created":1760000000000,"updated":1760000100000,
//! "provider":"anthropic","model":"...","access":"read",
//! "conversation":{ ...what `engine_ai::Conversation` serializes to... }}
//! ```
//!
//! The header fields come first and end the document's first line (the only newline outside
//! strings, `serde_json` escapes the ones inside). A header that reads but a body the engine
//! refuses shows in the list and fails when it is opened (once reported, then left out of the
//! list). The header and the body are one write, so they cannot disagree after a crash.
//!
//! # Writes
//!
//! Off the UI thread, one transaction of the database (`BEGIN IMMEDIATE`: the app and any other
//! process wait for each other), which also keeps the connection within [`MAX_PER_CONNECTION`].
//! The database file is private (mode 0600).
//!
//! # Limits and trust
//!
//! A conversation whose document would exceed [`MAX_FILE_BYTES`] is not written
//! ([`StoreError::TooLarge`]; the encoder stops at the limit instead of building the whole text).
//! At most [`MAX_PER_CONNECTION`] conversations per connection: the ones updated longest ago go.
//! Ids are validated first ([`valid_id`]: ASCII letters, digits, `-` and `_`, at most
//! [`MAX_ID_CHARS`], no Windows device name): they were file names and stay that restricted, so
//! what a row holds is the same set of names whatever reads it.
//!
//! A delete refuses the writes that were already on their way, and no others: one connection's
//! delete bumps that connection's epoch, "delete all" the global one ([`Epoch`]); a delete of one
//! conversation marks its id only when a row was removed. What is stored can hold query results:
//! nothing of the API key or any credential is in a `Conversation`, and this module is given no
//! secret.

use engine_agent::sanitize::clean;
use engine_agent::AgentAccess;
use engine_ai::Conversation;
use serde::{Deserialize, Serialize};
use engine_storage::documents::{self, ConversationDoc, PutConversation};
use std::fmt;
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Mutex;

/// The version of the file format this build reads and writes.
pub const FORMAT: u32 = 1;
/// The largest file written (and read).
pub const MAX_FILE_BYTES: usize = documents::MAX_CONVERSATION_BYTES;
/// Conversations kept per connection; the least recently updated go.
pub const MAX_PER_CONNECTION: usize = 50;
/// The prefix of a document read to get its header (the header line is a few hundred bytes).
pub const HEADER_READ: usize = 16 * 1024;
/// Rows of one connection looked at by a listing.
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

/// A row a listing could not use.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Problem {
    /// The conversation's id.
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
    /// The conversation is not there.
    Missing,
    /// The conversation would not fit [`MAX_FILE_BYTES`].
    TooLarge,
    /// The stored conversation is not one of this build (bad JSON, refused by the engine,
    /// another connection's, an id that is not its row's).
    Damaged(String),
    /// Written by a newer version of the app.
    NewerFormat(u32),
    /// A newer write of the same conversation is stored already.
    Stale,
    /// The conversation, its connection or everything was deleted after the run that made the
    /// write began: nothing is written back.
    Detached,
    /// The app database could not be used (damaged, from a newer tablex, an old file that could
    /// not be moved in, busy for too long); the text says which.
    Db(String),
}

impl fmt::Display for StoreError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::BadId(why) => write!(f, "not a valid id: {why}"),
            Self::Missing => f.write_str("the saved conversation is not there any more"),
            Self::TooLarge => write!(
                f,
                "the conversation is larger than {} MiB",
                MAX_FILE_BYTES / (1024 * 1024)
            ),
            Self::Damaged(why) => write!(f, "the saved conversation cannot be read: {why}"),
            Self::NewerFormat(version) => write!(
                f,
                "it was saved by a newer version of tablex (format {version}; this one reads {FORMAT})"
            ),
            Self::Stale => f.write_str("a newer version of it is saved already"),
            Self::Detached => f.write_str("it was deleted while it was being written"),
            Self::Db(why) => f.write_str(why),
        }
    }
}

impl std::error::Error for StoreError {}

impl From<engine_storage::app_db::AppDbError> for StoreError {
    fn from(error: engine_storage::app_db::AppDbError) -> Self {
        Self::Db(error.to_string())
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
    /// A statement that returned rows (or a Redis reply, MongoDB documents or something else).
    Read,
    /// A statement that changed something: it reports affected rows, or returned no result set at
    /// all (DDL).
    Wrote { rows: Option<u64> },
}

/// How the result `content` of the tool `name` reads, if it was a successful statement tool.
pub fn ran(name: &str, content: &str) -> Option<Ran> {
    use engine_agent::ToolName;
    let tool = ToolName::parse(name)?;
    if !matches!(
        tool,
        ToolName::ExecuteQuery
            | ToolName::ExecuteBatch
            | ToolName::ExecuteRedisCommand
            | ToolName::MongoFind
            | ToolName::MongoAggregate
            | ToolName::MongoCount
            | ToolName::MongoInsert
            | ToolName::MongoUpdate
            | ToolName::MongoDelete
    ) {
        return None;
    }
    let json: serde_json::Value = serde_json::from_str(content).ok()?;
    // A MongoDB write always changed (or tried to change) data: `n` documents inserted, matched or
    // deleted, `n_modified` of them changed by an update.
    if tool.is_mongo_write() {
        let rows = json
            .get("n_modified")
            .or_else(|| json.get("n"))
            .and_then(serde_json::Value::as_u64);
        return Some(Ran::Wrote { rows });
    }
    // A script reports the write statements that took effect (`changes`) and the rows they changed
    // (`affected_rows`, present only then): one that changed nothing read.
    if tool == ToolName::ExecuteBatch {
        let wrote = json
            .get("changes")
            .and_then(serde_json::Value::as_u64)
            .is_some_and(|changes| changes > 0);
        return Some(if wrote {
            Ran::Wrote {
                rows: json
                    .get("affected_rows")
                    .and_then(serde_json::Value::as_u64),
            }
        } else {
            Ran::Read
        });
    }
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
/// is `Read`, one that changed something `ReadWrite` (so is any MongoDB write, and a script that
/// changed something). A header that says less is wrong (or
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

/// The bytes of the document for `conversation`: the header's line, then the conversation. `updated`
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

/// The header object (JSON) out of the first bytes of a document: its first line, which ends in
/// the comma the object goes on after.
fn header_json(prefix: &[u8]) -> Result<String, StoreError> {
    let end = prefix
        .iter()
        .position(|byte| *byte == b'\n')
        .ok_or_else(|| StoreError::Damaged("no header line".into()))?;
    let mut line = prefix[..end].to_vec();
    if line.pop() != Some(b',') {
        return Err(StoreError::Damaged("the header line is cut".into()));
    }
    line.push(b'}');
    String::from_utf8(line).map_err(|error| StoreError::Damaged(format!("header: {error}")))
}

/// The header a stored header object says.
fn header_from_json(json: &str) -> Result<Header, StoreError> {
    #[derive(Deserialize)]
    struct Probe {
        format: u32,
    }
    let probe: Probe = serde_json::from_str(json)
        .map_err(|error| StoreError::Damaged(format!("header: {error}")))?;
    if probe.format != FORMAT {
        return Err(StoreError::NewerFormat(probe.format));
    }
    serde_json::from_str::<Header>(json)
        .map(Header::tidy)
        .map_err(|error| StoreError::Damaged(format!("header: {error}")))
}

/// The header out of the first bytes of a document.
fn parse_header(prefix: &[u8]) -> Result<Header, StoreError> {
    header_from_json(&header_json(prefix)?)
}

/// What a write did.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Written {
    /// Conversations removed to stay within [`MAX_PER_CONNECTION`].
    pub pruned: usize,
}

pub struct ConversationStore {
    /// The app database.
    db: PathBuf,
    /// One writer / deleter at a time within this process: a delete and the write it refuses
    /// cannot interleave.
    lock: Mutex<()>,
    /// Bumped by every delete of everything.
    epoch: AtomicU64,
    /// Bumped by every delete of one connection's conversations (by its lower-cased id).
    connection_epochs: Mutex<std::collections::HashMap<String, u64>>,
    /// Conversations deleted one by one in this process: their id is not written again.
    deleted: Mutex<std::collections::HashSet<String>>,
}

impl ConversationStore {
    /// The conversations of the app database at `db` (the old files beside `tablex.db` are moved
    /// in on first use; see the module docs).
    pub fn new(db: PathBuf) -> Self {
        Self {
            db,
            lock: Mutex::new(()),
            epoch: AtomicU64::new(0),
            connection_epochs: Mutex::default(),
            deleted: Mutex::default(),
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

    /// Opens the database once (moving an old folder of conversations in), so a problem with it
    /// is known before anything is listed or written.
    pub fn check(&self) -> Result<(), StoreError> {
        let _guard = self.locked();
        Ok(documents::check_conversations(&self.db)?)
    }

    /// The database the conversations are in.
    pub fn db(&self) -> &Path {
        &self.db
    }

    fn locked(&self) -> std::sync::MutexGuard<'_, ()> {
        self.lock
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    /// The connection id, validated and lower-cased (ids are compared without regard to case).
    fn connection_key(connection: &str) -> Result<String, StoreError> {
        let id = valid_id(connection).map_err(StoreError::BadId)?;
        Ok(id.to_ascii_lowercase())
    }

    // ---- writing ------------------------------------------------------------------------------

    /// Writes `encoded` as the conversation `connection` / `encoded.id`, then keeps the
    /// connection within [`MAX_PER_CONNECTION`]. A stored conversation with more messages than
    /// `encoded` has is not replaced ([`StoreError::Stale`]: two writes of one conversation that
    /// raced, or a row that says more than it should).
    pub fn write(&self, connection: &str, encoded: &Encoded) -> Result<Written, StoreError> {
        let id = valid_id(&encoded.id).map_err(StoreError::BadId)?;
        let key = Self::connection_key(connection)?;
        let _guard = self.locked();
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
        let body = std::str::from_utf8(&encoded.bytes)
            .map_err(|error| StoreError::Damaged(error.to_string()))?;
        let prefix = &encoded.bytes[..encoded.bytes.len().min(HEADER_READ)];
        let json = header_json(prefix)?;
        let header = header_from_json(&json)?;
        let doc = ConversationDoc {
            connection: &key,
            id,
            updated: header.updated,
            seq: encoded.seq,
            header: &json,
            body,
        };
        match documents::put_conversation(&self.db, &doc, MAX_PER_CONNECTION)? {
            PutConversation::Written { pruned } => Ok(Written { pruned }),
            PutConversation::Stale => Err(StoreError::Stale),
        }
    }

    // ---- reading ------------------------------------------------------------------------------

    /// The headers of `connection`'s conversations, newest first, and the rows that could not be
    /// read. Reads no body.
    pub fn list(&self, connection: &str) -> Result<Listing, StoreError> {
        let key = Self::connection_key(connection)?;
        let _guard = self.locked();
        let mut listing = Listing::default();
        for row in documents::list_conversations(&self.db, &key, MAX_LISTED)? {
            // Only names this module could have written are conversations.
            if valid_id(&row.id).is_err() {
                continue;
            }
            let read = header_from_json(&row.header).and_then(|header| {
                if header.id == row.id {
                    Ok(header)
                } else {
                    Err(StoreError::Damaged("the id inside is not the row's".into()))
                }
            });
            match read {
                Ok(header) => listing.entries.push(Entry {
                    header,
                    bytes: row.size,
                }),
                Err(error) => listing.problems.push(Problem {
                    file: row.id,
                    bytes: row.size,
                    why: error.to_string(),
                }),
            }
        }
        // Old files of the previous version that were not moved in (damaged, unreadable, too
        // large) are not lost track of: they are problems the user sees, and a delete removes them.
        for old in documents::old_conversation_problems(&self.db, &key)? {
            listing.problems.push(Problem {
                file: format!("{}.json", old.id),
                bytes: old.bytes,
                why: old.why,
            });
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

    /// Reads one conversation completely. Everything that is wrong with it is an error; nothing
    /// is half loaded.
    pub fn load(&self, connection: &str, id: &str) -> Result<Loaded, StoreError> {
        let id = valid_id(id).map_err(StoreError::BadId)?;
        let key = Self::connection_key(connection)?;
        let body = match documents::load_conversation(&self.db, &key, id, MAX_FILE_BYTES)? {
            Ok(Some(body)) => body,
            Ok(None) => return Err(StoreError::Missing),
            Err(_) => return Err(StoreError::TooLarge),
        };
        let bytes = body.into_bytes();
        let header = parse_header(&bytes[..bytes.len().min(HEADER_READ)])?;
        if header.id != id {
            return Err(StoreError::Damaged(
                "the id inside is not the conversation's".into(),
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
        // What the header says about its level is not trusted below what the conversation shows.
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
    /// process (a run still going that holds it) once a row was removed: a delete that finds
    /// nothing (a stale row, another connection's id) marks nothing.
    pub fn delete(&self, connection: &str, id: &str) -> Result<bool, StoreError> {
        let id = valid_id(id).map_err(StoreError::BadId)?;
        let key = Self::connection_key(connection)?;
        let _guard = self.locked();
        let (removed, failed) = documents::delete_conversation(&self.db, &key, id)?;
        if removed {
            self.deleted
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner())
                .insert(id.to_owned());
        }
        // The row is gone; the old file of it (the previous version's copy) is deleted too, and a
        // failure to is said.
        match failed {
            Some(error) => Err(error.into()),
            None => Ok(removed),
        }
    }

    /// Deletes everything kept for `connection` (the connection itself was deleted, or the user
    /// asked). Writes that were started before this are refused.
    pub fn delete_connection(&self, connection: &str) -> Result<DeleteReport, StoreError> {
        let key = Self::connection_key(connection)?;
        let _guard = self.locked();
        self.bump_connection(connection);
        let (deleted, failed) = documents::delete_conversations(&self.db, Some(&key))?;
        Ok(DeleteReport {
            deleted,
            error: failed.map(StoreError::from),
        })
    }

    /// Deletes every conversation of every connection. Writes that were started before this are
    /// refused.
    pub fn delete_all(&self) -> Result<DeleteReport, StoreError> {
        let _guard = self.locked();
        self.epoch.fetch_add(1, Ordering::SeqCst);
        let (deleted, failed) = documents::delete_conversations(&self.db, None)?;
        Ok(DeleteReport {
            deleted,
            error: failed.map(StoreError::from),
        })
    }
}

/// What a delete of many conversations did: how many went, and the first thing that went wrong
/// (a delete of the database rows is one transaction; what can fail on its own is the deleting
/// of an old file of the previous version, which goes with them).
#[derive(Debug, Default)]
pub struct DeleteReport {
    pub deleted: usize,
    pub error: Option<StoreError>,
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use std::fs;

    const CONNECTION: &str = "22222222-2222-4222-8222-222222222222";

    fn temp_store() -> (tempfile::TempDir, ConversationStore) {
        let dir = tempfile::tempdir().unwrap();
        let store = ConversationStore::new(dir.path().join("tablex.db"));
        (dir, store)
    }

    /// The stored document of a conversation, as the database has it.
    fn stored(store: &ConversationStore, id: &str) -> String {
        documents::load_conversation(store.db(), CONNECTION, id, usize::MAX)
            .unwrap()
            .unwrap()
            .expect("a stored conversation")
    }

    /// A row put in as it is: whatever a damaged, hand-edited or newer database could hold.
    fn plant(store: &ConversationStore, connection: &str, id: &str, header: &str, body: &str) {
        // A sequence number above any a test writes: this replaces what is there.
        let doc = ConversationDoc { connection, id, updated: 1, seq: 1000, header, body };
        documents::put_conversation(store.db(), &doc, MAX_PER_CONNECTION).unwrap();
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
    fn a_conversation_comes_back_exactly_and_the_document_has_the_documented_shape() {
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
        let text = stored(&store, "c1");
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
        // A body that is not JSON at all does not matter to a listing: only the header is read.
        let text = stored(&store, "mid");
        let (first, _) = text.split_once('\n').unwrap();
        let header_json = first.strip_suffix(',').unwrap().to_owned() + "}";
        plant(&store, CONNECTION, "mid", &header_json, &format!("{first}\n\"conversation\":{{ not json"));
        assert_eq!(store.list(CONNECTION).unwrap().entries.len(), 3);
        // ... and fails when it is opened.
        assert!(matches!(
            store.load(CONNECTION, "mid"),
            Err(StoreError::Damaged(_))
        ));
    }

    #[test]
    fn stores_on_one_database_see_each_other_at_once_and_a_listing_keeps_no_cache() {
        let (dir, store) = temp_store();
        let other = ConversationStore::new(dir.path().join("tablex.db"));
        put(&store, "c1", 10);
        assert_eq!(other.list(CONNECTION).unwrap().entries.len(), 1);
        put(&other, "c2", 20);
        let ids: Vec<_> = store.list(CONNECTION).unwrap().entries.iter().map(|e| e.header.id.clone()).collect();
        assert_eq!(ids, ["c2", "c1"]);
        assert!(other.delete(CONNECTION, "c1").unwrap());
        assert!(matches!(store.load(CONNECTION, "c1"), Err(StoreError::Missing)));
    }

    #[test]
    fn a_stored_conversation_is_replaced_whole_by_a_newer_write_of_it() {
        let (_dir, store) = temp_store();
        put(&store, "c1", 10);
        let newer = encode(&header("c1", 99), &conversation("second")).unwrap();
        store.write(CONNECTION, &newer).unwrap();
        let listing = store.list(CONNECTION).unwrap();
        assert_eq!(listing.entries.len(), 1);
        assert_eq!(listing.entries[0].header.updated, 99);
        assert_eq!(listing.entries[0].bytes, newer.bytes.len() as u64);
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
            store.list(CONNECTION).unwrap().entries.is_empty(),
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
        // The MongoDB read tools: a result of one is a read.
        for tool in ["mongo_find", "mongo_aggregate", "mongo_count"] {
            let mongo = serde_json::from_value::<Conversation>(json!({
                "scope": {"connection_id": CONNECTION}, "context": null,
                "messages": [
                    {"role": "user", "content": [{"type": "text", "text": "q"}]},
                    {"role": "assistant", "content": [{"type": "tool_use", "id": "t",
                        "name": tool, "input": {}}]},
                    {"role": "user", "content": [{"type": "tool_result", "tool_use_id": "t",
                        "content": "{\"count\":3}"}]},
                    {"role": "assistant", "content": [{"type": "text", "text": "Done."}]}
                ]
            }))
            .unwrap();
            assert_eq!(access_floor(&mongo), AgentAccess::Read, "{tool}");
        }
        // A MongoDB write is a write whatever its result says, and a script is one when it changed
        // something.
        let result_of = |tool: &str, result: &str| {
            serde_json::from_value::<Conversation>(json!({
                "scope": {"connection_id": CONNECTION}, "context": null,
                "messages": [
                    {"role": "user", "content": [{"type": "text", "text": "q"}]},
                    {"role": "assistant", "content": [{"type": "tool_use", "id": "t",
                        "name": tool, "input": {}}]},
                    {"role": "user", "content": [{"type": "tool_result", "tool_use_id": "t",
                        "content": result}]},
                    {"role": "assistant", "content": [{"type": "text", "text": "Done."}]}
                ]
            }))
            .unwrap()
        };
        for tool in ["mongo_insert", "mongo_update", "mongo_delete"] {
            for result in ["{\"n\":2,\"elapsed_ms\":1}", "{\"n\":0,\"n_modified\":0,\"elapsed_ms\":1}"] {
                assert_eq!(
                    access_floor(&result_of(tool, result)),
                    AgentAccess::ReadWrite,
                    "{tool} {result}"
                );
            }
        }
        assert_eq!(
            ran("mongo_update", "{\"n\":3,\"n_modified\":2}"),
            Some(Ran::Wrote { rows: Some(2) })
        );
        assert_eq!(ran("mongo_insert", "{\"n\":3}"), Some(Ran::Wrote { rows: Some(3) }));
        let batch = |result: &str| access_floor(&result_of("execute_batch", result));
        assert_eq!(
            batch("{\"statement_count\":2,\"statements\":[],\"skipped\":0,\"ok\":true,\"changes\":1,\"affected_rows\":4,\"elapsed_ms\":1}"),
            AgentAccess::ReadWrite
        );
        // A DDL statement has no row count, and still changed something.
        assert_eq!(
            batch("{\"statement_count\":1,\"statements\":[],\"skipped\":0,\"ok\":true,\"changes\":1,\"affected_rows\":0,\"elapsed_ms\":1}"),
            AgentAccess::ReadWrite
        );
        assert_eq!(
            batch("{\"statement_count\":2,\"statements\":[],\"skipped\":0,\"ok\":true,\"changes\":0,\"elapsed_ms\":1}"),
            AgentAccess::Read
        );
        // A script rolled back changed nothing.
        assert_eq!(
            batch("{\"statement_count\":2,\"statements\":[],\"skipped\":1,\"ok\":false,\"transaction\":\"rolled_back\",\"changes\":0,\"elapsed_ms\":1}"),
            AgentAccess::Read
        );
    }

    #[test]
    fn rows_with_names_this_module_never_wrote_cannot_hide_conversations_from_a_listing() {
        let (_dir, store) = temp_store();
        put(&store, "real", 1);
        for n in 0..(MAX_LISTED + 50) {
            // Newer than the real one, and not an id: skipped before anything is shown.
            let doc = ConversationDoc {
                connection: CONNECTION,
                id: &format!("junk {n}.txt"),
                updated: 1000 + n as u64,
                seq: 1,
                header: "{}",
                body: "x",
            };
            documents::put_conversation(store.db(), &doc, usize::MAX).unwrap();
        }
        let listing = store.list(CONNECTION).unwrap();
        assert_eq!(listing.entries.len(), 1);
        assert!(listing.problems.is_empty());
    }

    #[test]
    fn delete_all_removes_every_connections_conversations_and_nothing_else_of_the_database() {
        let (_dir, store) = temp_store();
        let other = "33333333-3333-4333-8333-333333333333";
        put(&store, "a", 1);
        store
            .write(other, &encode(&header("z", 1), &conversation("q")).unwrap())
            .unwrap();
        let session = documents::SESSION;
        documents::put(store.db(), session, "workspace", "{}").unwrap();
        let report = store.delete_all().unwrap();
        assert_eq!((report.deleted, report.error.is_none()), (2, true));
        assert!(store.list(CONNECTION).unwrap().entries.is_empty());
        assert!(store.list(other).unwrap().entries.is_empty());
        assert_eq!(documents::get(store.db(), session, "workspace").unwrap().as_deref(), Some("{}"));
    }

    #[test]
    fn a_database_that_is_not_valid_is_an_error_naming_it_and_is_left_alone() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("tablex.db");
        fs::write(&path, vec![0xA5u8; 4096]).unwrap();
        let store = ConversationStore::new(path.clone());
        let encoded = encode(&header("c1", 5), &conversation("q")).unwrap();
        for error in [
            store.check().unwrap_err(),
            store.list(CONNECTION).err().unwrap(),
            store.write(CONNECTION, &encoded).unwrap_err(),
            store.load(CONNECTION, "c1").err().unwrap(),
            store.delete_all().unwrap_err(),
        ] {
            let StoreError::Db(why) = &error else { panic!("{error:?}") };
            assert!(why.contains(&path.display().to_string()), "{why}");
        }
        assert_eq!(fs::read(&path).unwrap(), vec![0xA5u8; 4096]);
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
    fn a_row_that_does_not_read_is_listed_as_a_problem_and_never_panics() {
        let (_dir, store) = temp_store();
        put(&store, "good", 10);
        let good_body = stored(&store, "good");
        let good_header = good_body.split_once('\n').unwrap().0.strip_suffix(',').unwrap().to_owned() + "}";
        let cases: Vec<(&str, String, String)> = vec![
            ("empty", String::new(), String::new()),
            ("garbage", "\u{0}\u{7f} not json at all".into(), "\u{0}".into()),
            ("cut_header", "{\"format\":1,\"id\":\"cut_header\"".into(), "{}".into()),
            (
                "newer",
                "{\"format\":2,\"id\":\"newer\",\"anything\":[1,2]}".into(),
                "{\"format\":2,\"id\":\"newer\",\"anything\":[1,2],\n\"conversation\":{}}".into(),
            ),
            ("other_id", good_header.clone(), good_body.clone()),
            (
                "bad_access",
                good_header.replace("\"id\":\"good\"", "\"id\":\"bad_access\"").replace("\"access\":\"read\"", "\"access\":\"root\""),
                good_body.replace("\"id\":\"good\"", "\"id\":\"bad_access\"").replace("\"access\":\"read\"", "\"access\":\"root\""),
            ),
        ];
        for (name, header, body) in &cases {
            plant(&store, CONNECTION, name, header, body);
        }
        let listing = store.list(CONNECTION).unwrap();
        assert_eq!(listing.entries.len(), 1);
        let mut files: Vec<_> = listing.problems.iter().map(|p| p.file.clone()).collect();
        files.sort();
        let mut expected: Vec<_> = cases.iter().map(|(n, _, _)| n.to_string()).collect();
        expected.sort();
        assert_eq!(files, expected);
        assert!(listing
            .problems
            .iter()
            .any(|p| p.file == "newer" && p.why.contains("newer version")));
        // Opening each of them fails cleanly too.
        for (name, _, _) in &cases {
            assert!(store.load(CONNECTION, name).is_err(), "{name}");
        }
    }

    #[test]
    fn a_conversation_the_engine_refuses_or_another_connections_is_not_loaded() {
        let (_dir, store) = temp_store();
        put(&store, "good", 10);
        let text = stored(&store, "good");
        let header_of = |text: &str| text.split_once('\n').unwrap().0.strip_suffix(',').unwrap().to_owned() + "}";
        // Two user messages in a row: the engine's own validation refuses it.
        let refused = text
            .replace("\"id\":\"good\"", "\"id\":\"refused\"")
            .replace("\"role\":\"assistant\"", "\"role\":\"user\"");
        plant(&store, CONNECTION, "refused", &header_of(&refused), &refused);
        assert!(matches!(
            store.load(CONNECTION, "refused"),
            Err(StoreError::Damaged(_))
        ));
        let elsewhere = text
            .replace("\"id\":\"good\"", "\"id\":\"elsewhere\"")
            .replace(CONNECTION, "99999999-9999-4999-8999-999999999999");
        plant(&store, CONNECTION, "elsewhere", &header_of(&elsewhere), &elsewhere);
        assert!(matches!(
            store.load(CONNECTION, "elsewhere"),
            Err(StoreError::Damaged(why)) if why.contains("another connection")
        ));
        assert!(store.load(CONNECTION, "good").is_ok());
    }

    #[test]
    fn an_id_that_could_leave_the_folder_is_refused_everywhere() {
        let (_dir, store) = temp_store();
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
        assert!(store.list(CONNECTION).unwrap().entries.is_empty(), "nothing was stored");
    }

    #[cfg(unix)]
    #[test]
    fn the_database_file_is_private() {
        use std::os::unix::fs::PermissionsExt;
        let (_dir, store) = temp_store();
        put(&store, "c1", 10);
        assert_eq!(fs::metadata(store.db()).unwrap().permissions().mode() & 0o777, 0o600);
    }

    #[test]
    fn conversations_saved_as_files_before_are_moved_in_once_and_the_files_stay_as_they_were() {
        let (dir, store) = temp_store();
        // The file the old store wrote is exactly `encode`'s bytes.
        let encoded = encode(&header("c1", 50), &conversation("old question")).unwrap();
        let folder = dir.path().join("assistant").join("conversations").join(CONNECTION.to_uppercase());
        fs::create_dir_all(&folder).unwrap();
        fs::write(folder.join("c1.json"), &encoded.bytes).unwrap();
        fs::write(folder.join("notes.txt"), b"mine").unwrap();
        let loaded = store.load(CONNECTION, "c1").unwrap();
        assert_eq!(loaded.conversation, conversation("old question"));
        assert_eq!(loaded.header.updated, 50);
        assert_eq!(store.list(CONNECTION).unwrap().entries.len(), 1);
        assert_eq!(fs::read(folder.join("c1.json")).unwrap(), encoded.bytes, "the old file is not touched");
        assert_eq!(fs::read(folder.join("notes.txt")).unwrap(), b"mine");
        // Deleted here: not brought back from the old file by the next start.
        assert!(store.delete(CONNECTION, "c1").unwrap());
        assert!(ConversationStore::new(dir.path().join("tablex.db")).list(CONNECTION).unwrap().entries.is_empty());
        // It can hold query results: the user's delete took the old copy with it, and nothing else.
        assert!(!folder.join("c1.json").exists());
        assert_eq!(fs::read(folder.join("notes.txt")).unwrap(), b"mine");
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
        assert!(store.list(CONNECTION).unwrap().entries.is_empty());
        assert_eq!(
            store.list(other).unwrap().entries.len(),
            1,
            "the other one stays"
        );
        assert_eq!(store.delete_all().unwrap().deleted, 1);
        assert_eq!(
            store.delete_all().unwrap().deleted,
            0,
            "nothing left is no error"
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
