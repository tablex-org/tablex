//! The local channel between the tablex app and `tablex-mcp`: a Unix domain socket in a folder of its
//! own in the app data dir (`<app data dir>/channel/app.sock`, mode 0600, in a 0700 folder, so only
//! this user) over which the MCP server asks the
//! running app to show something (open a table, put a query in an editor tab) or to reload the
//! saved connections.
//!
//! The protocol is one JSON object per line in each direction, [`MAX_REQUEST_BYTES`] at most:
//!
//! ```text
//! {"request":"ping","protocol":1}
//! {"request":"open_object","connection_id":"...","database":"shop","schema":"public","object":"orders"}
//! {"request":"show_query","connection_id":"...","database":"shop","text":"SELECT 1","run":true}
//! {"request":"connections_changed"}
//! {"request":"approval","nonce":"...","mac":"...","ask":{"id":"...","title":"Run this statement",...}}
//! ```
//!
//! and `{"ok":true}` (a ping also says `"protocol"` and the `"launch"` it is talking to) or
//! `{"ok":false,"error":"..."}` back. A connection may carry several requests; [`AppClient`] sends a
//! ping first and checks the version.
//!
//! **Approval.** `approval` puts a question (an [`ApprovalAsk`]) to the user in the app, for an MCP
//! client that cannot ask through MCP elicitation. Its reply comes when the user answers, and is
//! `{"ok":true,"answer":{"id":"...","approve":true,"proof":"..."}}`. Because that answer lets a
//! statement run, nothing is trusted that need not be:
//! - **The boundary is the socket.** It is private (0600, made in a 0700 staging folder and moved
//!   into place), in a folder of its own (`channel`, created 0700 whatever the umask, so a data
//!   folder that others can write to, as umask 002 makes it, does not matter); a client connects only
//!   to a socket that is a socket, owned by its own user and closed to group and others, in a folder
//!   that is its user's and closed to everybody else (checked without following links), whose peer
//!   process is of its own user ([`AppClient`]); the listener serves only peers of its own user. A
//!   program of another user cannot be talked to, and cannot talk.
//! - **The token proves the peer can read a file.** On every launch the listener writes a random
//!   token to `app.token` beside the socket (0600; the client reads it only if it is a regular file
//!   of its own user closed to others). The token is **never sent**: it is the key of an HMAC-SHA256
//!   challenge-response. The request carries a fresh `nonce` and `mac = HMAC(token, "ask" | id |
//!   nonce | digest of the question)`; the app does not show a question whose mac is wrong or whose
//!   nonce it has seen. The answer carries `proof = HMAC(token, "answer" | id | nonce | choice |
//!   digest of the question)`, which the client checks. So a program that merely bound the socket
//!   cannot approve anything or answer for the app, and an answer is bound to exactly the question
//!   that was shown (the digest covers every field of it), to this request (the nonce) and to the
//!   user's choice. It does not stop a process of the same user that can read the token file: that
//!   user can already edit the app's settings.
//! - The id is random per request. An answer for another id, with a wrong proof, or after the first
//!   accepted answer is ignored; the question is one connection long, so an answer cannot outlive it.
//!   Closing the connection (a cancelled call, a dead client) withdraws the question.
//! - A launch id (a hash of the token) is in the ping's answer, so a client that read the token of
//!   one launch and met the socket of another finds out and stops.
//!
//! [`Listener`] runs on threads of its own: accepting, reading and writing never touch the app's
//! UI thread. The app's [`Handler`] is called on a connection thread and is where the request is
//! handed to the UI. Only Unix has the socket: elsewhere [`Listener::bind`] and [`AppClient::send`]
//! say that the platform is not supported.

use crate::platform;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::path::PathBuf;
use std::sync::Arc;

/// The version in the ping: a client and an app that disagree do not talk.
pub const PROTOCOL: u32 = 1;
/// The longest request (and reply) line, newline excluded.
pub const MAX_REQUEST_BYTES: usize = 1 << 20;
/// The socket's file name in the app data dir.
pub const SOCKET_FILE: &str = "app.sock";
/// The title of the question that asks to commit a transaction (the app treats it as click-only
/// whatever else the question says).
pub const COMMIT_TITLE: &str = "Commit this transaction";
/// The longest id of an approval question.
pub const MAX_ID_CHARS: usize = 64;
/// The longest text of one field of an approval question (a statement may be longer: see
/// [`ApprovalAsk::check`]).
pub const MAX_FIELD_CHARS: usize = 400;

/// The token file that belongs to the socket at `socket`: `app.token` beside `app.sock`.
pub fn token_path_for(socket: &std::path::Path) -> PathBuf {
    socket.with_extension("token")
}

/// A random id for one approval question (32 hex digits).
pub fn new_request_id() -> String {
    random_hex(16)
}

fn random_hex(bytes: usize) -> String {
    let mut buffer = vec![0u8; bytes];
    // Without randomness there is no safe id or token: a fixed one would be guessable.
    getrandom::getrandom(&mut buffer).expect("the operating system has randomness");
    hex(&buffer)
}

/// The user id of this process.
#[cfg(unix)]
fn euid() -> u32 {
    // SAFETY: `geteuid` has no arguments and cannot fail.
    unsafe { libc::geteuid() }
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|byte| format!("{byte:02x}")).collect()
}

/// HMAC-SHA256 (RFC 2104).
fn hmac_sha256(key: &[u8], message: &[u8]) -> [u8; 32] {
    let mut block = [0u8; 64];
    if key.len() > block.len() {
        block[..32].copy_from_slice(&Sha256::digest(key));
    } else {
        block[..key.len()].copy_from_slice(key);
    }
    let mut inner = Sha256::new();
    inner.update(block.map(|byte| byte ^ 0x36));
    inner.update(message);
    let mut outer = Sha256::new();
    outer.update(block.map(|byte| byte ^ 0x5c));
    outer.update(inner.finalize());
    outer.finalize().into()
}

/// `fields`, each preceded by its length (8 bytes), so no field can run into the next.
fn frame(fields: &[&[u8]]) -> Vec<u8> {
    let mut out = Vec::new();
    for field in fields {
        out.extend_from_slice(&(field.len() as u64).to_be_bytes());
        out.extend_from_slice(field);
    }
    out
}

/// The MAC of a request: `HMAC(token, "ask" | id | nonce | digest of the question)`.
pub fn ask_mac(token: &str, ask: &ApprovalAsk, nonce: &str) -> String {
    let digest = ask.digest();
    let message = frame(&[b"tablex-approval-ask-v2", ask.id.as_bytes(), nonce.as_bytes(), &digest]);
    hex(&hmac_sha256(token.as_bytes(), &message))
}

/// The proof of an answer: `HMAC(token, "answer" | id | nonce | choice | digest of the question)`.
pub fn answer_proof(token: &str, ask: &ApprovalAsk, nonce: &str, approve: bool) -> String {
    let digest = ask.digest();
    let choice: &[u8] = if approve { b"approve" } else { b"decline" };
    let message = frame(&[b"tablex-approval-answer-v2", ask.id.as_bytes(), nonce.as_bytes(), choice, &digest]);
    hex(&hmac_sha256(token.as_bytes(), &message))
}

/// Names one launch of the app without giving its token away (sent in the answer to a ping).
pub fn launch_id(token: &str) -> String {
    hex(&Sha256::digest(frame(&[b"tablex-launch-v2", token.as_bytes()])))[..16].to_owned()
}

/// A nonce is 16 to 64 hex digits.
fn nonce_ok(nonce: &str) -> bool {
    (16..=64).contains(&nonce.len()) && nonce.bytes().all(|byte| byte.is_ascii_hexdigit())
}

/// Removes the socket and the token an older tablex left directly in the app data dir (they moved
/// into `channel/`), unless that older tablex still answers. Best effort; nothing else is touched.
pub fn remove_legacy_files() {
    #[cfg(unix)]
    if let Some(dir) = platform::app_data_dir() {
        imp::remove_legacy(&dir);
    }
}

/// Whether two secrets are equal, in a time that does not depend on where they differ.
fn same_secret(a: &str, b: &str) -> bool {
    a.len() == b.len() && a.bytes().zip(b.bytes()).fold(0u8, |acc, (x, y)| acc | (x ^ y)) == 0
}

/// The folder of the socket and the token in the app data dir.
pub const CHANNEL_DIR: &str = "channel";

/// Where the running app listens: `<app data dir>/channel/app.sock` (`TABLEX_DATA_DIR` moves it with
/// every other tablex file).
pub fn socket_path() -> Option<PathBuf> {
    platform::app_data_dir().map(|dir| socket_path_in(&dir))
}

/// The longest socket path used as is: a Unix socket address holds about 104 bytes, and binding
/// goes through a staging folder beside it (`.app.sock.<pid>.tmp/s`) that adds about 24.
const MAX_SOCKET_PATH: usize = 80;

/// `<dir>/channel/app.sock`, or, when that is too long for a socket address (a deep
/// `TABLEX_DATA_DIR`), a socket in the user's runtime folder named after `dir`, so the app and the
/// MCP server still meet.
fn socket_path_in(dir: &std::path::Path) -> PathBuf {
    let path = dir.join(CHANNEL_DIR).join(SOCKET_FILE);
    if path.as_os_str().len() <= MAX_SOCKET_PATH {
        return path;
    }
    let digest = Sha256::digest(dir.as_os_str().as_encoded_bytes());
    let name: String = digest.iter().take(8).map(|byte| format!("{byte:02x}")).collect();
    runtime_dir().join(format!("tablex-{name}.sock"))
}

/// The folder a long socket path goes in: `XDG_RUNTIME_DIR` (the user's own, Linux), else a folder
/// of this user in the temp folder.
fn runtime_dir() -> PathBuf {
    std::env::var_os("XDG_RUNTIME_DIR")
        .map(PathBuf::from)
        .filter(|dir| dir.is_absolute())
        .unwrap_or_else(per_user_temp_dir)
}

/// On macOS the temp folder is the user's own (0700). Elsewhere it may be shared (Linux `/tmp`,
/// whose names anybody can take), so a folder `<temp>/tablex-<uid>` is made in it, private (0700);
/// one of another user, or one others can write to, is refused where the socket is used (the folder
/// of a socket has to be its user's and closed to writing by others).
#[cfg(target_os = "macos")]
fn per_user_temp_dir() -> PathBuf {
    std::env::temp_dir()
}

#[cfg(all(unix, not(target_os = "macos")))]
fn per_user_temp_dir() -> PathBuf {
    use std::os::unix::fs::DirBuilderExt;
    let dir = std::env::temp_dir().join(format!("tablex-{}", euid()));
    let _ = std::fs::DirBuilder::new().mode(0o700).create(&dir);
    dir
}

#[cfg(not(unix))]
fn per_user_temp_dir() -> PathBuf {
    std::env::temp_dir()
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "request", rename_all = "snake_case", deny_unknown_fields)]
pub enum Request {
    /// The client's protocol version; answered by the channel itself, never given to a [`Handler`].
    Ping { protocol: u32 },
    /// Reveal the table / view / collection in the sidebar and open it as a double click does.
    OpenObject {
        connection_id: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        database: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        schema: Option<String>,
        object: String,
    },
    /// Open a query tab with `text` in the editor; with `run` it goes through the Run button's path.
    ShowQuery {
        connection_id: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        database: Option<String>,
        text: String,
        run: bool,
    },
    /// The saved connections changed in the store: reload them.
    ConnectionsChanged {},
    /// Put a question to the user and answer when they have. `mac` signs the question with the
    /// launch's token (see the module docs); the channel checks it itself: a handler gets this
    /// request with an empty `nonce` and `mac`, and only from an authenticated connection.
    Approval { nonce: String, mac: String, ask: ApprovalAsk },
    /// Not a request on the wire: the channel gives it to the handler when the connection that
    /// asked the question ends before it was answered, so the question is taken off the screen.
    #[serde(skip)]
    Withdraw { id: String },
}

/// What the user is asked: the same facts the MCP client would put to them through elicitation.
/// Everything is plain text; the app shows it as it is (after spelling out characters without a
/// shape of their own), never as markup.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ApprovalAsk {
    /// Random, one per question; the answer is for this id only.
    pub id: String,
    /// "Run this statement", "Commit this transaction", ...
    pub title: String,
    /// The MCP client's own name for itself, when it gave one (not verified).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub client: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub connection: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub connection_type: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub database: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub collection: Option<String>,
    /// Why the statement is dangerous (the danger line), when it is.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub danger: Option<String>,
    /// The question can be approved only by a mouse click (a dangerous statement, a commit): the
    /// app ignores the keyboard on its Approve button. The app treats a `danger` as click-only too.
    #[serde(default, skip_serializing_if = "is_false")]
    pub click_only: bool,
    /// Other lines the question says (a script, a transaction), in order.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub notes: Vec<String>,
    /// What runs, each in a block of its own: a statement, a command, every change of a commit.
    #[serde(default)]
    pub statements: Vec<String>,
}

fn is_false(value: &bool) -> bool {
    !*value
}

impl ApprovalAsk {
    /// A digest of every field of the question, each with its length (and an absent field told
    /// from an empty one), so what is signed is exactly what is shown.
    pub fn digest(&self) -> [u8; 32] {
        let mut parts: Vec<Vec<u8>> = vec![self.id.clone().into_bytes(), self.title.clone().into_bytes()];
        for field in [
            &self.client,
            &self.connection,
            &self.connection_type,
            &self.database,
            &self.collection,
            &self.danger,
        ] {
            parts.push(match field {
                Some(text) => [&[1u8][..], text.as_bytes()].concat(),
                None => vec![0],
            });
        }
        parts.push(vec![u8::from(self.click_only)]);
        for list in [&self.notes, &self.statements] {
            parts.push((list.len() as u64).to_be_bytes().to_vec());
            parts.extend(list.iter().map(|text| text.clone().into_bytes()));
        }
        let fields: Vec<&[u8]> = parts.iter().map(Vec::as_slice).collect();
        Sha256::digest(frame(&fields)).into()
    }

    /// Why this question cannot be shown, if it cannot: a bad id, no title or nothing to show, a
    /// field far longer than any real name. The statement sizes are the sender's to limit (it
    /// refuses what cannot be reviewed); the whole line is capped at [`MAX_REQUEST_BYTES`].
    pub fn check(&self) -> Result<(), String> {
        let id_ok = !self.id.is_empty()
            && self.id.chars().count() <= MAX_ID_CHARS
            && self.id.chars().all(|c| c.is_ascii_alphanumeric() || c == '-');
        if !id_ok {
            return Err("the question has no usable id".into());
        }
        let short = |text: &str| text.chars().count() <= MAX_FIELD_CHARS;
        let fields_ok = short(&self.title)
            && [&self.client, &self.connection, &self.connection_type, &self.database, &self.collection]
                .into_iter()
                .flatten()
                .all(|text| short(text));
        if self.title.trim().is_empty() || !fields_ok {
            return Err("the question has no title, or a field that is too long".into());
        }
        if self.statements.is_empty() || self.statements.iter().all(|text| text.trim().is_empty()) {
            return Err("the question shows nothing to approve".into());
        }
        Ok(())
    }
}

/// The answer to an approval question, signed by the app ([`approval_proof`]).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ApprovalAnswer {
    pub id: String,
    pub approve: bool,
    /// Set by the channel, not by the handler.
    #[serde(default)]
    pub proof: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Reply {
    pub ok: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
    /// Only in the answer to a ping.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub protocol: Option<u32>,
    /// Only in the answer to an approval question.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub answer: Option<ApprovalAnswer>,
    /// Only in the answer to a ping: which launch of the app this is.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub launch: Option<String>,
}

impl Reply {
    pub fn ok() -> Self {
        Self {
            ok: true,
            error: None,
            protocol: None,
            answer: None,
            launch: None,
        }
    }

    pub fn error(message: impl Into<String>) -> Self {
        Self {
            ok: false,
            error: Some(message.into()),
            protocol: None,
            answer: None,
            launch: None,
        }
    }

    /// The user's answer to an approval question. The channel adds the id and the proof.
    pub fn approval(approve: bool) -> Self {
        Self {
            answer: Some(ApprovalAnswer {
                id: String::new(),
                approve,
                proof: String::new(),
            }),
            ..Self::ok()
        }
    }
}

/// What the app does with a request (never a [`Request::Ping`]). Called on a connection thread: it
/// may wait for the UI, and its answer is the reply.
pub type Handler = Arc<dyn Fn(Request) -> Reply + Send + Sync>;

/// One request line to its reply: a ping is answered here, a line that is not a request is an
/// error reply (the connection stays usable), everything else is the handler's.
pub fn handle_line(line: &[u8], handler: &dyn Fn(Request) -> Reply) -> Reply {
    match serde_json::from_slice::<Request>(line) {
        Ok(request) => handle_request(request, handler),
        Err(error) => Reply::error(format!("not a request: {error}")),
    }
}

/// A parsed request to its reply (see [`handle_line`]). A question is not answered here: only the
/// connection that checked its token may put one to the handler.
fn handle_request(request: Request, handler: &dyn Fn(Request) -> Reply) -> Reply {
    match request {
        Request::Ping { protocol } if protocol == PROTOCOL => Reply {
            protocol: Some(PROTOCOL),
            ..Reply::ok()
        },
        Request::Ping { protocol } => Reply {
            protocol: Some(PROTOCOL),
            ..Reply::error(format!(
                "unsupported protocol {protocol}: this tablex speaks protocol {PROTOCOL}"
            ))
        },
        Request::Approval { .. } | Request::Withdraw { .. } => {
            Reply::error("a question is only taken on an authenticated connection")
        }
        request => handler(request),
    }
}

#[derive(Debug, thiserror::Error)]
pub enum BindError {
    /// Another tablex answers on the socket: it keeps it.
    #[error("another tablex is already listening on {0}")]
    InUse(PathBuf),
    #[error("{0}")]
    Io(#[from] std::io::Error),
    #[error("the app channel is not supported on this platform")]
    Unsupported,
}

#[derive(Debug, thiserror::Error)]
pub enum ClientError {
    #[error("tablex is not running. Open tablex and try again.")]
    NotRunning,
    #[error("tablex cannot be reached from a program on this platform yet: the app connection exists on macOS and Linux only, not on Windows")]
    Unsupported,
    /// The channel broke (an I/O error, a reply that is not one, another protocol version).
    #[error("could not talk to tablex: {0}")]
    Failed(String),
    /// The caller gave up on a question while it was open (the app was told to withdraw it).
    #[error("the question was withdrawn")]
    Cancelled,
    /// tablex understood the request and said no.
    #[error("tablex could not do it: {0}")]
    Refused(String),
}

pub use imp::{AppClient, Listener};

#[cfg(unix)]
mod imp {
    use super::*;
    use std::io::{self, Read, Write};
    use std::os::unix::fs::{DirBuilderExt, FileTypeExt, MetadataExt, PermissionsExt};
    use std::os::unix::net::{UnixListener, UnixStream};
    use std::path::Path;
    use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
    use std::thread::JoinHandle;
    use std::time::{Duration, Instant};

    /// The user id of the process at the other end of `stream`, where the platform tells
    /// (`None`: it does not, and the socket's own permissions are all there is).
    #[cfg(any(target_os = "macos", target_os = "ios", target_os = "freebsd", target_os = "openbsd", target_os = "netbsd", target_os = "dragonfly"))]
    fn peer_uid(stream: &UnixStream) -> Option<u32> {
        use std::os::fd::AsRawFd;
        let (mut uid, mut gid) = (0 as libc::uid_t, 0 as libc::gid_t);
        // SAFETY: the descriptor is open for the call, and both pointers are to live variables.
        let status = unsafe { libc::getpeereid(stream.as_raw_fd(), &mut uid, &mut gid) };
        (status == 0).then_some(uid)
    }

    #[cfg(any(target_os = "linux", target_os = "android"))]
    fn peer_uid(stream: &UnixStream) -> Option<u32> {
        use std::os::fd::AsRawFd;
        let mut credentials = libc::ucred { pid: 0, uid: 0, gid: 0 };
        let mut length = std::mem::size_of::<libc::ucred>() as libc::socklen_t;
        // SAFETY: the descriptor is open for the call; `credentials` and `length` are live and
        // `length` is the size of `credentials`.
        let status = unsafe {
            libc::getsockopt(
                stream.as_raw_fd(),
                libc::SOL_SOCKET,
                libc::SO_PEERCRED,
                (&mut credentials as *mut libc::ucred).cast(),
                &mut length,
            )
        };
        (status == 0).then_some(credentials.uid)
    }

    #[cfg(not(any(target_os = "macos", target_os = "ios", target_os = "freebsd", target_os = "openbsd", target_os = "netbsd", target_os = "dragonfly", target_os = "linux", target_os = "android")))]
    fn peer_uid(_stream: &UnixStream) -> Option<u32> {
        None
    }

    /// Whether the process at the other end is not known to be of this user: another user's, or
    /// one whose user the platform does not tell.
    fn peer_is_stranger(stream: &UnixStream) -> bool {
        peer_uid(stream).is_none_or(|uid| uid != euid())
    }

    /// Why the folder of the socket at `path` cannot be trusted, if it cannot: it has to be this
    /// user's and closed to writing by group and others (anyone who can write there can replace the
    /// socket and the token file, whatever their modes).
    fn folder_untrusted(path: &Path) -> Option<&'static str> {
        let parent = path.parent()?;
        // Not through a link: a link to a folder is another folder that may be anyone's.
        let Ok(meta) = std::fs::symlink_metadata(parent) else {
            return Some("cannot be looked at");
        };
        if !meta.file_type().is_dir() {
            Some("is not a folder")
        } else if meta.uid() != euid() {
            Some("belongs to another user")
        } else if meta.mode() & 0o077 != 0 {
            Some("is open to other users")
        } else {
            None
        }
    }

    /// See [`super::remove_legacy_files`].
    pub(super) fn remove_legacy(dir: &Path) {
        let (socket, token) = (dir.join(SOCKET_FILE), token_path_for(&dir.join(SOCKET_FILE)));
        match std::fs::symlink_metadata(&socket) {
            Ok(meta) if meta.file_type().is_socket() && meta.uid() == euid() => {
                // Somebody answers: an older tablex that is still running keeps its files.
                if UnixStream::connect(&socket).is_ok() {
                    return;
                }
                let _ = std::fs::remove_file(&socket);
            }
            Ok(_) => return,
            Err(_) => {}
        }
        // Only a token file: exactly 64 hex digits (and a newline), and ours.
        let ours = std::fs::symlink_metadata(&token)
            .is_ok_and(|meta| meta.file_type().is_file() && meta.uid() == euid() && meta.len() <= 65);
        let looks_like_one = ours
            && std::fs::read_to_string(&token).is_ok_and(|text| {
                let text = text.strip_suffix('\n').unwrap_or(&text);
                text.len() == 64 && text.bytes().all(|byte| byte.is_ascii_hexdigit())
            });
        if looks_like_one {
            let _ = std::fs::remove_file(&token);
        }
    }

    /// The longest a socket address (and so the path of a socket) may be: `sun_path` holds 104
    /// bytes on macOS (108 on Linux), the last for the end of the string.
    const MAX_SUN_PATH: usize = 103;

    /// Why the socket at `path` cannot be bound or reached for its length, if it cannot.
    fn path_too_long(path: &Path) -> Option<String> {
        // Binding goes through a staging folder beside it: `<folder>/.<name>.<pid>.tmp/s`.
        let name = path.file_name().map_or(0, |name| name.len());
        let folder = path.parent().map_or(0, |folder| folder.as_os_str().len());
        let staged = folder + name + 24;
        (staged > MAX_SUN_PATH).then(|| {
            format!(
                "the socket path {} is too long for a Unix socket ({staged} of {MAX_SUN_PATH} bytes): \
                 set XDG_RUNTIME_DIR or TMPDIR to a shorter folder",
                path.display()
            )
        })
    }

    /// Why `path` is not a file this user can trust to be theirs and private, if it is not: it must
    /// be (a `socket` or a regular file) owned by this user and closed to group and others, and
    /// never reached through a link.
    fn untrusted(meta: &std::fs::Metadata, socket: bool) -> Option<&'static str> {
        let kind_ok = if socket { meta.file_type().is_socket() } else { meta.file_type().is_file() };
        if !kind_ok {
            return Some("is not the kind of file it should be");
        }
        if meta.uid() != euid() {
            return Some("belongs to another user");
        }
        if meta.mode() & 0o077 != 0 {
            return Some("is open to other users");
        }
        None
    }

    /// The token of the app listening at `socket`, if the token file is the app's own and private.
    fn read_token(socket: &Path) -> Result<String, ClientError> {
        use std::os::unix::fs::OpenOptionsExt;
        let path = token_path_for(socket);
        let mut file = std::fs::OpenOptions::new()
            .read(true)
            .custom_flags(libc::O_NOFOLLOW)
            .open(&path)
            .map_err(|error| match error.kind() {
                io::ErrorKind::NotFound => ClientError::Failed(
                    "this tablex has no approval token: update tablex, then try again".into(),
                ),
                _ => ClientError::Failed(format!("the approval token cannot be read: {error}")),
            })?;
        let meta = file
            .metadata()
            .map_err(|error| ClientError::Failed(error.to_string()))?;
        if let Some(why) = untrusted(&meta, false) {
            return Err(ClientError::Failed(format!("the approval token {why}")));
        }
        let mut token = String::new();
        (&mut file)
            .take(256)
            .read_to_string(&mut token)
            .map_err(|error| ClientError::Failed(format!("the approval token cannot be read: {error}")))?;
        let token = token.trim().to_owned();
        if token.len() < 32 || !token.bytes().all(|byte| byte.is_ascii_hexdigit()) {
            return Err(ClientError::Failed("the approval token is not one".into()));
        }
        Ok(token)
    }

    /// How long a question's reader waits at most between two looks at whether the caller gave up.
    const ASK_TICK: Duration = Duration::from_millis(100);
    /// Lines that are not the answer, in a row, after which the question is given up.
    const MAX_STRAY_LINES: usize = 8;
    /// "No limit" for a line deadline.
    const NEVER: Duration = Duration::from_secs(u32::MAX as u64);

    /// Connections served at once; one more is told it is busy and closed.
    const MAX_CONNECTIONS: usize = 8;
    /// A request line has to arrive within this long of the start of the wait for it (a client that
    /// trickles bytes, or says nothing, loses its connection).
    const LINE_DEADLINE: Duration = Duration::from_secs(10);
    /// How often a blocked read wakes to look at the deadline.
    const READ_TICK: Duration = Duration::from_secs(1);
    const WRITE_TIMEOUT: Duration = Duration::from_secs(5);
    /// How long a client waits for the answers (the app answers when the UI has taken the request).
    const CLIENT_TIMEOUT: Duration = Duration::from_secs(40);

    /// The longest a connection that has not asked a question stays (a program that pings
    /// forever does not hold one of the few places for ever).
    const CONNECTION_LIFETIME: Duration = if cfg!(test) { Duration::from_secs(3) } else { Duration::from_secs(30) };

    #[derive(Debug, PartialEq, Eq)]
    pub(super) enum LineError {
        TooLong,
        Timeout,
        Io(String),
    }

    impl std::fmt::Display for LineError {
        fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
            match self {
                Self::TooLong => write!(f, "a line is longer than {MAX_REQUEST_BYTES} bytes"),
                Self::Timeout => write!(f, "no answer in time"),
                Self::Io(error) => write!(f, "{error}"),
            }
        }
    }

    /// What a connection is checked against: the launch's token, the launch id that goes in a ping's
    /// answer, and the nonces already seen (a question that is sent twice is shown once).
    struct Auth {
        token: String,
        launch: String,
        seen: std::sync::Mutex<std::collections::VecDeque<String>>,
    }

    impl Auth {
        fn new(token: &str) -> Self {
            Self { token: token.to_owned(), launch: launch_id(token), seen: Default::default() }
        }

        /// Records `nonce`; `false` if it was seen (the newest 1024 are remembered).
        fn fresh(&self, nonce: &str) -> bool {
            let mut seen = self.seen.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
            if seen.iter().any(|old| old == nonce) {
                return false;
            }
            seen.push_back(nonce.to_owned());
            if seen.len() > 1024 {
                seen.pop_front();
            }
            true
        }
    }

    /// Lines out of a byte stream, with a cap on their length and a deadline for each.
    pub(super) struct Lines<R> {
        source: R,
        buffer: Vec<u8>,
    }

    impl<R: Read> Lines<R> {
        pub(super) fn new(source: R) -> Self {
            Self {
                source,
                buffer: Vec::new(),
            }
        }

        /// The next line without its newline (`None`: the stream ended between lines).
        pub(super) fn next(&mut self, deadline: Duration) -> Result<Option<Vec<u8>>, LineError> {
            let started = Instant::now();
            loop {
                if let Some(end) = self.buffer.iter().position(|byte| *byte == b'\n') {
                    if end > MAX_REQUEST_BYTES {
                        return Err(LineError::TooLong);
                    }
                    let mut line: Vec<u8> = self.buffer.drain(..=end).collect();
                    line.pop();
                    if line.last() == Some(&b'\r') {
                        line.pop();
                    }
                    return Ok(Some(line));
                }
                if self.buffer.len() > MAX_REQUEST_BYTES {
                    return Err(LineError::TooLong);
                }
                if started.elapsed() > deadline {
                    return Err(LineError::Timeout);
                }
                let mut chunk = [0u8; 8192];
                match self.source.read(&mut chunk) {
                    Ok(0) => return Ok(None),
                    Ok(read) => self.buffer.extend_from_slice(&chunk[..read]),
                    Err(error)
                        if matches!(
                            error.kind(),
                            io::ErrorKind::WouldBlock
                                | io::ErrorKind::TimedOut
                                | io::ErrorKind::Interrupted
                        ) => {}
                    Err(error) => return Err(LineError::Io(error.to_string())),
                }
            }
        }
    }

    fn write_reply(stream: &mut UnixStream, reply: &Reply) -> io::Result<()> {
        let mut line = serde_json::to_vec(reply).map_err(io::Error::other)?;
        line.push(b'\n');
        stream.write_all(&line)
    }

    fn serve_connection(mut stream: UnixStream, handler: &Handler, auth: &Auth) {
        // The socket is private to the user, but a peer of another user is never served.
        if peer_is_stranger(&stream) {
            return;
        }
        if stream.set_read_timeout(Some(READ_TICK)).is_err()
            || stream.set_write_timeout(Some(WRITE_TIMEOUT)).is_err()
        {
            return;
        }
        let Ok(reader) = stream.try_clone() else {
            return;
        };
        let started = Instant::now();
        let mut lines = Lines::new(reader);
        loop {
            // Whoever has not asked a question has only so long.
            let left = CONNECTION_LIFETIME.saturating_sub(started.elapsed());
            if left.is_zero() {
                return;
            }
            let reply = match lines.next(LINE_DEADLINE.min(left)) {
                Ok(Some(line)) => match serde_json::from_slice::<Request>(&line) {
                    Ok(Request::Approval { nonce, mac, ask }) => {
                        // A question is one connection long: whatever happens, it ends it.
                        let reply = if !nonce_ok(&nonce) || !same_secret(&mac, &ask_mac(&auth.token, &ask, &nonce)) {
                            Reply::error("not authorised: the question is not signed with this launch's token")
                        } else if !auth.fresh(&nonce) {
                            Reply::error("this question was already sent")
                        } else if let Err(why) = ask.check() {
                            Reply::error(why)
                        } else {
                            ask_user(&stream, &mut lines, ask, &nonce, handler, auth)
                        };
                        let _ = write_reply(&mut stream, &reply);
                        return;
                    }
                    Ok(request) => {
                        let ping = matches!(request, Request::Ping { .. });
                        let mut reply = handle_request(request, handler.as_ref());
                        if ping && reply.ok {
                            reply.launch = Some(auth.launch.clone());
                        }
                        reply
                    }
                    Err(error) => Reply::error(format!("not a request: {error}")),
                },
                Ok(None) | Err(LineError::Io(_)) | Err(LineError::Timeout) => return,
                Err(LineError::TooLong) => {
                    // The rest of the line cannot be told from the next request: close.
                    let _ = write_reply(
                        &mut stream,
                        &Reply::error(format!("request is longer than {MAX_REQUEST_BYTES} bytes")),
                    );
                    return;
                }
            };
            if write_reply(&mut stream, &reply).is_err() {
                return;
            }
        }
    }

    /// Hands `ask` to the app and waits for the user's answer, which may take minutes. Meanwhile
    /// this thread watches the connection: when the client closes it (a cancelled call, a client
    /// that died) the question is withdrawn. The answer is stamped with `ask`'s id and signed over
    /// the question, the nonce and the choice.
    fn ask_user(
        stream: &UnixStream,
        lines: &mut Lines<UnixStream>,
        ask: ApprovalAsk,
        nonce: &str,
        handler: &Handler,
        auth: &Auth,
    ) -> Reply {
        let signed = ask.clone();
        let id = ask.id.clone();
        let Ok(wake) = stream.try_clone() else {
            return Reply::error("tablex could not take the question");
        };
        let (done, answered) = std::sync::mpsc::channel();
        let handed = Request::Approval {
            nonce: String::new(),
            mac: String::new(),
            ask,
        };
        let reply = std::thread::scope(|scope| {
            scope.spawn(|| {
                let _ = done.send(handler(handed));
                // Wakes the watching read below; the answer is already in the channel.
                let _ = wake.shutdown(std::net::Shutdown::Read);
            });
            // Anything the client sends meanwhile is not a request this connection answers.
            while let Ok(Some(_)) = lines.next(NEVER) {}
            match answered.try_recv() {
                Ok(reply) => reply,
                Err(_) => {
                    // The client went first: take the question down and let the handler finish.
                    let _ = handler(Request::Withdraw { id: id.clone() });
                    Reply::error("the question was withdrawn")
                }
            }
        });
        match reply.answer {
            Some(answer) if reply.ok => {
                let proof = answer_proof(&auth.token, &signed, nonce, answer.approve);
                Reply {
                    answer: Some(ApprovalAnswer { id, approve: answer.approve, proof }),
                    ..Reply::ok()
                }
            }
            _ if reply.ok => Reply::error("tablex gave no answer"),
            _ => reply,
        }
    }

    /// The app's end: removes its socket file when dropped.
    pub struct Listener {
        path: PathBuf,
        stop: Arc<AtomicBool>,
        accepting: Option<JoinHandle<()>>,
        /// Device and inode of the socket file this listener made.
        identity: (u64, u64),
        /// The token file this listener wrote, and what is in it.
        token_file: (PathBuf, Arc<str>),
    }

    impl std::fmt::Debug for Listener {
        fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
            f.debug_struct("Listener").field("path", &self.path).finish()
        }
    }

    impl Listener {
        /// Starts listening on `path` and returns at once; the accepting runs on its own thread.
        ///
        /// A socket another tablex answers on is left alone ([`BindError::InUse`]); a file nobody
        /// answers on (a crashed run) is replaced; a file that is not a socket is never touched.
        /// The socket is made in a private (0700) folder, set to 0600 and then moved into place, so
        /// no other user can connect at any moment. The launch's token is written (0600) to
        /// `app.token` beside it before the socket appears.
        pub fn bind(path: &Path, handler: Handler) -> Result<Self, BindError> {
            let parent = path
                .parent()
                .ok_or_else(|| io::Error::other("the socket path has no folder"))?;
            if let Some(why) = path_too_long(path) {
                return Err(io::Error::other(why).into());
            }
            // The folder of the channel is made 0700 (the umask only takes bits away); a folder that
            // is there already must be as private, or nothing is listened on in it.
            if let Some(ancestor) = parent.parent() {
                std::fs::create_dir_all(ancestor)?;
            }
            match std::fs::DirBuilder::new().mode(0o700).create(parent) {
                Ok(()) => {}
                Err(error) if error.kind() == io::ErrorKind::AlreadyExists => {}
                Err(error) => return Err(error.into()),
            }
            if let Some(why) = folder_untrusted(path) {
                return Err(io::Error::other(format!(
                    "the folder of the socket {why}; not listening in it (it has to be yours, mode 0700)"
                ))
                .into());
            }
            match std::fs::symlink_metadata(path) {
                Ok(meta) if !meta.file_type().is_socket() => {
                    return Err(io::Error::other(format!(
                        "{} exists and is not a socket; not replacing it",
                        path.display()
                    ))
                    .into());
                }
                Ok(_) => {
                    // A connection that fails means nobody is behind the file: left over from a
                    // run that did not clean up.
                    if UnixStream::connect(path).is_ok() {
                        return Err(BindError::InUse(path.to_path_buf()));
                    }
                }
                Err(error) if error.kind() == io::ErrorKind::NotFound => {}
                Err(error) => return Err(error.into()),
            }
            let staging = parent.join(format!(
                ".{}.{}.tmp",
                path.file_name().map_or_else(|| "sock".into(), |n| n.to_string_lossy()),
                std::process::id()
            ));
            let _ = std::fs::remove_dir_all(&staging);
            std::fs::DirBuilder::new().mode(0o700).create(&staging)?;
            let token: Arc<str> = random_hex(32).into();
            let auth = Arc::new(Auth::new(&token));
            let token_path = token_path_for(path);
            let made = (|| {
                use std::os::unix::fs::OpenOptionsExt;
                let socket = staging.join("s");
                let listener = UnixListener::bind(&socket)?;
                std::fs::set_permissions(&socket, std::fs::Permissions::from_mode(0o600))?;
                // Made private from the first byte, in the private folder, then moved into place
                // (the file of a crashed run is replaced).
                let file = staging.join("t");
                let mut out = std::fs::OpenOptions::new()
                    .write(true)
                    .create_new(true)
                    .mode(0o600)
                    .open(&file)?;
                out.write_all(token.as_bytes())?;
                drop(out);
                std::fs::rename(&file, &token_path)?;
                if let Err(error) = std::fs::rename(&socket, path) {
                    let _ = std::fs::remove_file(&token_path);
                    return Err(error);
                }
                Ok::<_, io::Error>(listener)
            })();
            let _ = std::fs::remove_dir_all(&staging);
            let listener = made?;
            let meta = std::fs::symlink_metadata(path)?;
            let stop = Arc::new(AtomicBool::new(false));
            let accepting = {
                let stop = Arc::clone(&stop);
                std::thread::Builder::new()
                    .name("tablex-app-channel".into())
                    .spawn(move || accept_loop(&listener, &stop, &handler, &auth))?
            };
            Ok(Self {
                path: path.to_path_buf(),
                stop,
                accepting: Some(accepting),
                identity: (meta.dev(), meta.ino()),
                token_file: (token_path, token),
            })
        }

        pub fn path(&self) -> &Path {
            &self.path
        }
    }

    fn accept_loop(listener: &UnixListener, stop: &AtomicBool, handler: &Handler, auth: &Arc<Auth>) {
        let active = Arc::new(AtomicUsize::new(0));
        for stream in listener.incoming() {
            if stop.load(Ordering::SeqCst) {
                return;
            }
            let Ok(mut stream) = stream else {
                continue;
            };
            if active.fetch_add(1, Ordering::SeqCst) >= MAX_CONNECTIONS {
                active.fetch_sub(1, Ordering::SeqCst);
                let _ = stream.set_write_timeout(Some(WRITE_TIMEOUT));
                let _ = write_reply(&mut stream, &Reply::error("tablex is busy: try again"));
                continue;
            }
            let (handler, active, auth) = (Arc::clone(handler), Arc::clone(&active), Arc::clone(auth));
            let spawned = std::thread::Builder::new()
                .name("tablex-app-channel-conn".into())
                .spawn({
                    let active = Arc::clone(&active);
                    move || {
                        serve_connection(stream, &handler, &auth);
                        active.fetch_sub(1, Ordering::SeqCst);
                    }
                });
            if spawned.is_err() {
                active.fetch_sub(1, Ordering::SeqCst);
            }
        }
    }

    impl Drop for Listener {
        fn drop(&mut self) {
            self.stop.store(true, Ordering::SeqCst);
            // Only the file this listener made is its to remove or to knock on (another tablex may
            // have replaced it since: then it is not touched, and this listener's thread, which
            // nothing can reach any more, ends with the process).
            let ours = std::fs::symlink_metadata(&self.path)
                .is_ok_and(|meta| (meta.dev(), meta.ino()) == self.identity);
            if !ours {
                return;
            }
            // `accept` only returns for a connection: make one, so the thread sees the flag.
            let _ = UnixStream::connect(&self.path);
            if let Some(thread) = self.accepting.take() {
                let _ = thread.join();
            }
            let _ = std::fs::remove_file(&self.path);
            // The token goes too, unless another launch has already written its own.
            let (token_path, token) = &self.token_file;
            if std::fs::read_to_string(token_path).is_ok_and(|text| text == **token) {
                let _ = std::fs::remove_file(token_path);
            }
        }
    }

    /// The MCP server's end.
    #[derive(Debug, Clone)]
    pub struct AppClient {
        path: PathBuf,
    }

    impl AppClient {
        pub fn new(path: PathBuf) -> Self {
            Self { path }
        }

        /// The client for the socket in the app data dir.
        pub fn at_default_location() -> Option<Self> {
            socket_path().map(Self::new)
        }

        /// The connection to the app: a socket that is a socket, owned by this user and closed to
        /// others (a program of another user that made one where this one looks cannot be talked
        /// to), whose peer is a process of this user.
        fn open(&self) -> Result<UnixStream, ClientError> {
            if let Some(why) = path_too_long(&self.path) {
                return Err(ClientError::Failed(why));
            }
            let meta = std::fs::symlink_metadata(&self.path).map_err(|error| match error.kind() {
                io::ErrorKind::NotFound => ClientError::NotRunning,
                _ => ClientError::Failed(error.to_string()),
            })?;
            if let Some(why) = untrusted(&meta, true) {
                return Err(ClientError::Failed(format!("the tablex socket {why}")));
            }
            if let Some(why) = folder_untrusted(&self.path) {
                return Err(ClientError::Failed(format!("the folder of the tablex socket {why}")));
            }
            let stream = UnixStream::connect(&self.path).map_err(|error| match error.kind() {
                io::ErrorKind::NotFound | io::ErrorKind::ConnectionRefused => {
                    ClientError::NotRunning
                }
                _ => ClientError::Failed(error.to_string()),
            })?;
            if peer_is_stranger(&stream) {
                return Err(ClientError::Failed(
                    "the program behind the tablex socket is not run by this user".into(),
                ));
            }
            Ok(stream)
        }

        /// Writes the ping (and `request` right behind it, when there is one) and reads the ping's
        /// answer. With `launch`, the app has to be that launch of it (the one whose token was read).
        fn start<'a>(
            stream: &'a UnixStream,
            request: Option<&Request>,
            tick: Duration,
            launch: Option<&str>,
        ) -> Result<Lines<&'a UnixStream>, ClientError> {
            let failed = |error: io::Error| ClientError::Failed(error.to_string());
            stream.set_read_timeout(Some(tick)).map_err(failed)?;
            stream
                .set_write_timeout(Some(WRITE_TIMEOUT))
                .map_err(failed)?;
            let mut out = serde_json::to_vec(&Request::Ping { protocol: PROTOCOL })
                .map_err(|error| ClientError::Failed(error.to_string()))?;
            out.push(b'\n');
            if let Some(request) = request {
                out.extend(
                    serde_json::to_vec(request).map_err(|error| ClientError::Failed(error.to_string()))?,
                );
                out.push(b'\n');
            }
            (&*stream).write_all(&out).map_err(failed)?;
            let mut lines = Lines::new(stream);
            let hello = Self::read(&mut lines, CLIENT_TIMEOUT)?;
            if !hello.ok || hello.protocol != Some(PROTOCOL) {
                return Err(ClientError::Failed(hello.error.unwrap_or_else(|| {
                    format!(
                        "tablex speaks protocol {}, this program protocol {PROTOCOL}: update the older one",
                        hello.protocol.map_or_else(|| "none".to_owned(), |protocol| protocol.to_string())
                    )
                })));
            }
            if launch.is_some_and(|launch| hello.launch.as_deref() != Some(launch)) {
                return Err(ClientError::Failed(
                    "tablex was restarted while this program was connecting: try again".into(),
                ));
            }
            Ok(lines)
        }

        fn read(lines: &mut Lines<&UnixStream>, deadline: Duration) -> Result<Reply, ClientError> {
            let line = lines
                .next(deadline)
                .map_err(|error| ClientError::Failed(error.to_string()))?
                .ok_or_else(|| ClientError::Failed("tablex closed the connection".into()))?;
            serde_json::from_slice(&line)
                .map_err(|error| ClientError::Failed(format!("unreadable reply: {error}")))
        }

        /// Sends `request` (after a ping that checks the protocol) and returns tablex's answer.
        pub fn send(&self, request: &Request) -> Result<Reply, ClientError> {
            let stream = self.open()?;
            let mut lines = Self::start(&stream, Some(request), READ_TICK, None)?;
            let reply = Self::read(&mut lines, CLIENT_TIMEOUT)?;
            if reply.ok {
                Ok(reply)
            } else {
                Err(ClientError::Refused(
                    reply.error.unwrap_or_else(|| "no reason given".into()),
                ))
            }
        }

        /// Whether a tablex that can take a question answers here: the socket and the peer are
        /// trusted ([`open`](Self::open)), the token file is the app's own, and a ping is answered.
        pub fn reachable(&self) -> Result<(), ClientError> {
            let stream = self.open()?;
            let token = read_token(&self.path)?;
            Self::start(&stream, None, ASK_TICK, Some(&launch_id(&token))).map(drop)
        }

        /// Puts `ask` to the user in the app and returns whether they approved. Waits for as long
        /// as the user takes; `cancelled` is looked at about every [`ASK_TICK`], and when it says
        /// so the connection is closed, which takes the question off the app's screen
        /// ([`ClientError::Cancelled`]). An app that closes, or a connection that drops, is an
        /// error: never an approval.
        ///
        /// The token is never sent: the request carries a fresh nonce and a MAC over the question
        /// ([`ask_mac`]), and an answer counts only if it is for `ask.id` and carries the proof
        /// ([`answer_proof`]) over the same question, this nonce and its choice. One that is not is
        /// ignored, and [`MAX_STRAY_LINES`] of them end the question. The question is sent only once
        /// the app has said it is the launch whose token was read.
        pub fn ask_approval(
            &self,
            ask: &ApprovalAsk,
            cancelled: &dyn Fn() -> bool,
        ) -> Result<bool, ClientError> {
            let stream = self.open()?;
            let token = read_token(&self.path)?;
            let mut lines = Self::start(&stream, None, ASK_TICK, Some(&launch_id(&token)))?;
            let nonce = random_hex(16);
            let request = Request::Approval {
                mac: ask_mac(&token, ask, &nonce),
                nonce: nonce.clone(),
                ask: ask.clone(),
            };
            let mut line = serde_json::to_vec(&request).map_err(|error| ClientError::Failed(error.to_string()))?;
            line.push(b'\n');
            (&stream).write_all(&line).map_err(|error| ClientError::Failed(error.to_string()))?;
            let mut stray = 0;
            loop {
                if cancelled() {
                    return Err(ClientError::Cancelled);
                }
                let reply = match lines.next(ASK_TICK) {
                    Ok(Some(line)) => serde_json::from_slice::<Reply>(&line).ok(),
                    Ok(None) => {
                        return Err(ClientError::Failed("tablex closed the connection".into()))
                    }
                    Err(LineError::Timeout) => continue,
                    Err(error) => return Err(ClientError::Failed(error.to_string())),
                };
                match reply {
                    Some(Reply { ok: false, error, .. }) => {
                        return Err(ClientError::Refused(
                            error.unwrap_or_else(|| "no reason given".into()),
                        ))
                    }
                    Some(Reply { answer: Some(answer), .. })
                        if answer.id == ask.id
                            && same_secret(&answer.proof, &answer_proof(&token, ask, &nonce, answer.approve)) =>
                    {
                        return Ok(answer.approve);
                    }
                    _ => {
                        stray += 1;
                        if stray >= MAX_STRAY_LINES {
                            return Err(ClientError::Failed(
                                "tablex sent answers that are not for this question".into(),
                            ));
                        }
                    }
                }
            }
        }
    }
}

#[cfg(not(unix))]
mod imp {
    use super::*;
    use std::path::Path;

    #[derive(Debug)]
    pub struct Listener;

    impl Listener {
        pub fn bind(_path: &Path, _handler: Handler) -> Result<Self, BindError> {
            Err(BindError::Unsupported)
        }

        pub fn path(&self) -> &Path {
            Path::new("")
        }
    }

    #[derive(Debug, Clone)]
    pub struct AppClient;

    impl AppClient {
        pub fn new(_path: PathBuf) -> Self {
            Self
        }

        pub fn at_default_location() -> Option<Self> {
            socket_path().map(Self::new)
        }

        pub fn send(&self, _request: &Request) -> Result<Reply, ClientError> {
            Err(ClientError::Unsupported)
        }

        pub fn reachable(&self) -> Result<(), ClientError> {
            Err(ClientError::Unsupported)
        }

        pub fn ask_approval(
            &self,
            _ask: &ApprovalAsk,
            _cancelled: &dyn Fn() -> bool,
        ) -> Result<bool, ClientError> {
            Err(ClientError::Unsupported)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn parse(text: &str) -> Request {
        serde_json::from_str(text).unwrap()
    }

    #[test]
    fn requests_and_replies_round_trip_with_the_documented_shape() {
        let requests = [
            (
                Request::Ping { protocol: PROTOCOL },
                json!({"request": "ping", "protocol": 1}),
            ),
            (
                Request::OpenObject {
                    connection_id: "c1".into(),
                    database: Some("shop".into()),
                    schema: Some("public".into()),
                    object: "orders".into(),
                },
                json!({"request": "open_object", "connection_id": "c1", "database": "shop", "schema": "public", "object": "orders"}),
            ),
            (
                Request::OpenObject {
                    connection_id: "c1".into(),
                    database: None,
                    schema: None,
                    object: "items".into(),
                },
                json!({"request": "open_object", "connection_id": "c1", "object": "items"}),
            ),
            (
                Request::ShowQuery {
                    connection_id: "c1".into(),
                    database: None,
                    text: "SELECT 1".into(),
                    run: true,
                },
                json!({"request": "show_query", "connection_id": "c1", "text": "SELECT 1", "run": true}),
            ),
            (
                Request::ConnectionsChanged {},
                json!({"request": "connections_changed"}),
            ),
        ];
        for (request, wire) in requests {
            assert_eq!(serde_json::to_value(&request).unwrap(), wire);
            assert_eq!(serde_json::from_value::<Request>(wire).unwrap(), request);
        }
        assert_eq!(serde_json::to_string(&Reply::ok()).unwrap(), r#"{"ok":true}"#);
        assert_eq!(
            serde_json::to_string(&Reply::error("no")).unwrap(),
            r#"{"ok":false,"error":"no"}"#
        );
    }

    #[test]
    fn a_line_is_a_request_or_an_error_reply() {
        let seen = std::sync::Mutex::new(Vec::new());
        let handler = |request: Request| {
            seen.lock().unwrap().push(request);
            Reply::ok()
        };
        // A ping is the channel's own, and never reaches the handler.
        let hello = handle_line(br#"{"request":"ping","protocol":1}"#, &handler);
        assert_eq!((hello.ok, hello.protocol), (true, Some(PROTOCOL)));
        let wrong = handle_line(br#"{"request":"ping","protocol":2}"#, &handler);
        assert!(!wrong.ok);
        assert!(wrong.error.unwrap().contains("unsupported protocol 2"));
        assert!(seen.lock().unwrap().is_empty());
        // Garbage, an unknown request, an unknown field and a missing field are errors.
        for bad in [
            &b"not json"[..],
            b"",
            b"[1]",
            br#"{"request":"drop_everything"}"#,
            br#"{"request":"connections_changed","surprise":1}"#,
            br#"{"request":"show_query","connection_id":"c","text":"x"}"#,
            br#"{"request":"open_object","object":"orders"}"#,
            b"\xff\xfe",
        ] {
            let reply = handle_line(bad, &handler);
            assert!(!reply.ok, "{}", String::from_utf8_lossy(bad));
            assert!(reply.error.unwrap().starts_with("not a request"));
        }
        assert!(seen.lock().unwrap().is_empty());
        let reply = handle_line(br#"{"request":"connections_changed"}"#, &handler);
        assert!(reply.ok);
        assert_eq!(*seen.lock().unwrap(), [Request::ConnectionsChanged {}]);
        let _ = parse(r#"{"request":"connections_changed"}"#);
    }

    #[test]
    fn an_approval_question_has_the_documented_wire_shape_and_the_withdrawal_is_not_a_request() {
        let ask = ApprovalAsk {
            id: "0123".into(),
            title: "Run this statement".into(),
            client: Some("agent".into()),
            connection: Some("Warehouse".into()),
            connection_type: Some("Development".into()),
            statements: vec!["DELETE FROM t WHERE a = 1".into()],
            ..ApprovalAsk::default()
        };
        let request = Request::Approval { nonce: "0123456789abcdef".into(), mac: "mac".into(), ask };
        let wire = json!({"request": "approval", "nonce": "0123456789abcdef", "mac": "mac", "ask": {
            "id": "0123", "title": "Run this statement", "client": "agent", "connection": "Warehouse",
            "connection_type": "Development", "statements": ["DELETE FROM t WHERE a = 1"]}});
        assert_eq!(serde_json::to_value(&request).unwrap(), wire);
        assert_eq!(serde_json::from_value::<Request>(wire).unwrap(), request);
        // The answer: the id, the choice and the proof.
        let reply = Reply { answer: Some(ApprovalAnswer { id: "0123".into(), approve: true, proof: "p".into() }), ..Reply::ok() };
        assert_eq!(serde_json::to_value(&reply).unwrap(), json!({"ok": true, "answer": {"id": "0123", "approve": true, "proof": "p"}}));
        // Only the channel makes a withdrawal: nobody can send one.
        assert!(serde_json::from_str::<Request>(r#"{"request":"withdraw","id":"0123"}"#).is_err());
        // An approval is never given to a handler by the line path (which has not checked a token).
        let handler = |_: Request| -> Reply { panic!("not for the handler") };
        let line = serde_json::to_vec(&request).unwrap();
        assert!(!handle_line(&line, &handler).ok);
    }

    #[test]
    fn a_question_that_cannot_be_shown_is_refused_before_the_app_sees_it() {
        let good = ApprovalAsk { id: "ab-12".into(), title: "T".into(), statements: vec!["x".into()], ..ApprovalAsk::default() };
        assert_eq!(good.check(), Ok(()));
        let bad = |change: &dyn Fn(&mut ApprovalAsk)| {
            let mut ask = good.clone();
            change(&mut ask);
            ask.check().is_err()
        };
        assert!(bad(&|ask| ask.id = String::new()));
        assert!(bad(&|ask| ask.id = "a b".into()));
        assert!(bad(&|ask| ask.id = "a".repeat(MAX_ID_CHARS + 1)));
        assert!(bad(&|ask| ask.title = "  ".into()));
        assert!(bad(&|ask| ask.connection = Some("c".repeat(MAX_FIELD_CHARS + 1))));
        assert!(bad(&|ask| ask.statements = Vec::new()));
        assert!(bad(&|ask| ask.statements = vec![" ".into()]));
    }

    #[test]
    fn hmac_sha256_matches_the_rfc_4231_vectors() {
        let hex = |bytes: [u8; 32]| super::hex(&bytes);
        assert_eq!(
            hex(hmac_sha256(&[0x0b; 20], b"Hi There")),
            "b0344c61d8db38535ca8afceaf0bf12b881dc200c9833da726e9376c2e32cff7"
        );
        assert_eq!(
            hex(hmac_sha256(b"Jefe", b"what do ya want for nothing?")),
            "5bdcc146bf60754e6a042426089575c75a003f089d2739839dec58b964ec3843"
        );
        // A key longer than a block is hashed first (RFC 4231 test case 6).
        assert_eq!(
            hex(hmac_sha256(&[0xaa; 131], b"Test Using Larger Than Block-Size Key - Hash Key First")),
            "60e431591ee0b67f0d8a26aacbf5b77f8e0bc6213728c5140546040f0ee37f54"
        );
    }

    #[test]
    fn the_mac_and_the_proof_bind_the_token_the_question_the_nonce_and_the_choice() {
        let ask = ApprovalAsk { id: "id1".into(), title: "T".into(), statements: vec!["DELETE FROM t WHERE a = 1".into()], ..ApprovalAsk::default() };
        let nonce = "0123456789abcdef0123456789abcdef";
        let mac = ask_mac("tok", &ask, nonce);
        let proof = answer_proof("tok", &ask, nonce, true);
        assert_eq!((mac.len(), proof.len()), (64, 64));
        assert_ne!(mac, proof, "an ask is not an answer");
        assert_eq!(mac, ask_mac("tok", &ask, nonce));
        let mut other_statement = ask.clone();
        other_statement.statements = vec!["DELETE FROM t".into()];
        let mut other_note = ask.clone();
        other_note.notes = vec!["x".into()];
        let mut other_danger = ask.clone();
        other_danger.danger = Some(String::new());
        let mut other_click = ask.clone();
        other_click.click_only = true;
        // Any change to the question, the token, the nonce or the choice is a different MAC / proof.
        for other in [ask_mac("tok2", &ask, nonce), ask_mac("tok", &ask, "fedcba9876543210fedcba9876543210"), ask_mac("tok", &other_statement, nonce), ask_mac("tok", &other_note, nonce), ask_mac("tok", &other_danger, nonce), ask_mac("tok", &other_click, nonce)] {
            assert_ne!(mac, other);
        }
        for other in [
            answer_proof("tok2", &ask, nonce, true),
            answer_proof("tok", &ask, "fedcba9876543210fedcba9876543210", true),
            answer_proof("tok", &ask, nonce, false),
            answer_proof("tok", &other_statement, nonce, true),
        ] {
            assert_ne!(proof, other);
        }
        // Fields cannot run into one another: moving text between neighbours changes the digest.
        let mut a = ask.clone();
        let mut b = ask.clone();
        (a.title, a.client) = ("ab".into(), Some("c".into()));
        (b.title, b.client) = ("a".into(), Some("bc".into()));
        assert_ne!(a.digest(), b.digest());
        assert_ne!(launch_id("tok"), launch_id("tok2"));
        assert!(!launch_id("tok").contains("tok"));
        assert!(same_secret("abc", "abc") && !same_secret("abc", "abd") && !same_secret("abc", "ab"));
        assert_ne!(new_request_id(), new_request_id());
        assert_eq!(new_request_id().len(), 32);
        assert!(nonce_ok(nonce) && !nonce_ok("short") && !nonce_ok(&"g".repeat(32)));
    }

    #[test]
    fn the_socket_lives_in_the_app_data_dir() {
        assert!(socket_path()
            .expect("a data dir exists")
            .ends_with("tablex/channel/app.sock"));
    }

    #[cfg(unix)]
    mod socket {
        use super::*;
        use std::io::{Read, Write};
        use std::os::unix::fs::PermissionsExt;
        use std::os::unix::net::{UnixListener, UnixStream};
        use std::path::Path;
        use std::sync::Mutex;
        use std::time::Duration;
        use super::super::imp::{LineError, Lines};

        /// A handler that records what it was asked and answers `reply`.
        fn recording(reply: Reply) -> (Handler, Arc<Mutex<Vec<Request>>>) {
            let seen = Arc::new(Mutex::new(Vec::new()));
            let log = Arc::clone(&seen);
            (
                Arc::new(move |request| {
                    log.lock().unwrap().push(request);
                    reply.clone()
                }),
                seen,
            )
        }

        fn dir() -> tempfile::TempDir {
            tempfile::Builder::new().prefix("tx").permissions(<std::fs::Permissions as std::os::unix::fs::PermissionsExt>::from_mode(0o700)).tempdir().unwrap()
        }

        fn open(path: &Path) -> Request {
            let _ = path;
            Request::OpenObject {
                connection_id: "c1".into(),
                database: Some("shop".into()),
                schema: None,
                object: "orders".into(),
            }
        }

        /// Lines from a raw connection to the socket.
        fn exchange(path: &Path, send: &[u8]) -> Vec<String> {
            let mut stream = UnixStream::connect(path).unwrap();
            stream.set_read_timeout(Some(Duration::from_secs(5))).unwrap();
            // The server may close before it has read everything (a too long line).
            let _ = stream.write_all(send);
            let _ = stream.shutdown(std::net::Shutdown::Write);
            let mut text = String::new();
            let _ = stream.read_to_string(&mut text);
            text.lines().map(str::to_owned).collect()
        }

        #[test]
        fn a_request_goes_through_the_socket_and_the_reply_comes_back() {
            let dir = dir();
            let path = dir.path().join("app.sock");
            let (handler, seen) = recording(Reply::ok());
            let listener = Listener::bind(&path, handler).unwrap();
            assert_eq!(listener.path(), path);
            let client = AppClient::new(path.clone());
            assert_eq!(client.send(&open(&path)).unwrap(), Reply::ok());
            client.send(&Request::ConnectionsChanged {}).unwrap();
            assert_eq!(
                *seen.lock().unwrap(),
                [open(&path), Request::ConnectionsChanged {}]
            );
        }

        #[test]
        fn a_refusal_is_an_error_with_the_apps_words() {
            let dir = dir();
            let path = dir.path().join("app.sock");
            let (handler, _) = recording(Reply::error("no such connection"));
            let _listener = Listener::bind(&path, handler).unwrap();
            match AppClient::new(path.clone()).send(&open(&path)) {
                Err(ClientError::Refused(message)) => assert_eq!(message, "no such connection"),
                other => panic!("{other:?}"),
            }
        }

        #[test]
        fn no_listener_means_not_running() {
            let dir = dir();
            let path = dir.path().join("app.sock");
            assert!(matches!(
                AppClient::new(path.clone()).send(&Request::ConnectionsChanged {}),
                Err(ClientError::NotRunning)
            ));
            let message = ClientError::NotRunning.to_string();
            assert!(message.contains("Open tablex and try again"), "{message}");
        }

        #[test]
        fn the_socket_is_private_to_the_user_and_nothing_is_left_beside_it() {
            let dir = dir();
            let path = dir.path().join("app.sock");
            let (handler, _) = recording(Reply::ok());
            let _listener = Listener::bind(&path, handler).unwrap();
            let mode = std::fs::metadata(&path).unwrap().permissions().mode() & 0o777;
            assert_eq!(mode, 0o600);
            let mut names: Vec<_> = std::fs::read_dir(dir.path())
                .unwrap()
                .map(|entry| entry.unwrap().file_name())
                .collect();
            names.sort();
            assert_eq!(names, ["app.sock", "app.token"], "the staging folder is gone");
        }

        #[test]
        fn a_file_left_by_a_crashed_run_is_replaced() {
            let dir = dir();
            let path = dir.path().join("app.sock");
            // A socket file nobody listens on: dropping a `UnixListener` leaves the file.
            drop(UnixListener::bind(&path).unwrap());
            assert!(path.exists());
            assert!(UnixStream::connect(&path).is_err());
            let (handler, _) = recording(Reply::ok());
            let _listener = Listener::bind(&path, handler).unwrap();
            AppClient::new(path).send(&Request::ConnectionsChanged {}).unwrap();
        }

        #[test]
        fn a_file_that_is_not_a_socket_is_never_replaced() {
            let dir = dir();
            let path = dir.path().join("app.sock");
            std::fs::write(&path, "mine").unwrap();
            let (handler, _) = recording(Reply::ok());
            assert!(matches!(
                Listener::bind(&path, handler),
                Err(BindError::Io(_))
            ));
            assert_eq!(std::fs::read_to_string(&path).unwrap(), "mine");
        }

        #[test]
        fn a_second_instance_does_not_steal_the_socket() {
            let dir = dir();
            let path = dir.path().join("app.sock");
            let (first, first_seen) = recording(Reply::ok());
            let first_listener = Listener::bind(&path, first).unwrap();
            let (second, second_seen) = recording(Reply::ok());
            assert!(matches!(
                Listener::bind(&path, second),
                Err(BindError::InUse(_))
            ));
            // The first still answers, and the refused second leaves the file alone when it goes.
            AppClient::new(path.clone())
                .send(&Request::ConnectionsChanged {})
                .unwrap();
            assert_eq!(first_seen.lock().unwrap().len(), 1);
            assert!(second_seen.lock().unwrap().is_empty());
            drop(first_listener);
        }

        #[test]
        fn the_file_is_removed_when_the_listener_goes() {
            let dir = dir();
            let path = dir.path().join("app.sock");
            let (handler, _) = recording(Reply::ok());
            let listener = Listener::bind(&path, handler).unwrap();
            assert!(path.exists());
            drop(listener);
            assert!(!path.exists());
            assert!(matches!(
                AppClient::new(path).send(&Request::ConnectionsChanged {}),
                Err(ClientError::NotRunning)
            ));
        }

        #[test]
        fn a_listener_does_not_remove_a_socket_another_instance_put_there() {
            let dir = dir();
            let path = dir.path().join("app.sock");
            let (first, _) = recording(Reply::ok());
            let old = Listener::bind(&path, first).unwrap();
            // Another instance took the path over (the file was replaced).
            std::fs::remove_file(&path).unwrap();
            let (second, _) = recording(Reply::ok());
            let new = Listener::bind(&path, second).unwrap();
            drop(old);
            assert!(path.exists(), "the newer socket stays");
            drop(new);
            assert!(!path.exists());
        }

        #[test]
        fn a_garbage_line_gets_an_error_and_the_connection_goes_on() {
            let dir = dir();
            let path = dir.path().join("app.sock");
            let (handler, seen) = recording(Reply::ok());
            let _listener = Listener::bind(&path, handler).unwrap();
            let replies = exchange(
                &path,
                b"{{{{\n{\"request\":\"ping\",\"protocol\":9}\n{\"request\":\"connections_changed\"}\n",
            );
            assert_eq!(replies.len(), 3, "{replies:?}");
            assert!(replies[0].contains("not a request"), "{replies:?}");
            assert!(replies[1].contains("unsupported protocol 9"), "{replies:?}");
            assert_eq!(replies[2], r#"{"ok":true}"#);
            assert_eq!(seen.lock().unwrap().len(), 1);
        }

        #[test]
        fn a_request_over_the_cap_is_refused_and_the_connection_closed() {
            let dir = dir();
            let path = dir.path().join("app.sock");
            let (handler, seen) = recording(Reply::ok());
            let _listener = Listener::bind(&path, handler).unwrap();
            let mut big = br#"{"request":"show_query","connection_id":"c","run":false,"text":""#.to_vec();
            big.extend(std::iter::repeat_n(b'x', MAX_REQUEST_BYTES + 10));
            big.extend_from_slice(b"\"}\n{\"request\":\"connections_changed\"}\n");
            let replies = exchange(&path, &big);
            assert_eq!(replies.len(), 1, "{replies:?}");
            assert!(replies[0].contains("longer than"), "{replies:?}");
            assert!(seen.lock().unwrap().is_empty());
            // A request just under the cap is fine.
            let mut fits = br#"{"request":"show_query","connection_id":"c","run":false,"text":""#.to_vec();
            fits.extend(std::iter::repeat_n(b'x', MAX_REQUEST_BYTES - 200));
            fits.extend_from_slice(b"\"}\n");
            assert_eq!(exchange(&path, &fits), [r#"{"ok":true}"#]);
        }

        /// A reader that never has anything: a client that connected and said nothing.
        struct Silent;
        impl Read for Silent {
            fn read(&mut self, _: &mut [u8]) -> std::io::Result<usize> {
                std::thread::sleep(Duration::from_millis(5));
                Err(std::io::ErrorKind::WouldBlock.into())
            }
        }

        #[test]
        fn a_client_that_sends_nothing_times_out() {
            let mut lines = Lines::new(Silent);
            assert_eq!(
                lines.next(Duration::from_millis(50)),
                Err(LineError::Timeout)
            );
        }

        /// A listener whose handler answers every question with `approve`.
        fn approving(path: &Path, approve: bool) -> (Listener, Arc<Mutex<Vec<Request>>>) {
            let seen = Arc::new(Mutex::new(Vec::new()));
            let log = Arc::clone(&seen);
            let listener = Listener::bind(
                path,
                Arc::new(move |request| {
                    log.lock().unwrap().push(request.clone());
                    match request {
                        Request::Approval { .. } => Reply::approval(approve),
                        _ => Reply::ok(),
                    }
                }),
            )
            .unwrap();
            (listener, seen)
        }

        fn ask(id: &str) -> ApprovalAsk {
            ApprovalAsk { id: id.into(), title: "Run this statement".into(), statements: vec!["DELETE FROM t WHERE a = 1".into()], ..ApprovalAsk::default() }
        }

        #[test]
        fn a_question_is_answered_with_the_choice_and_the_apps_proof() {
            let dir = dir();
            let path = dir.path().join("app.sock");
            for approve in [true, false] {
                let (_listener, seen) = approving(&path, approve);
                let client = AppClient::new(path.clone());
                assert_eq!(client.reachable().ok(), Some(()));
                assert_eq!(client.ask_approval(&ask("q1"), &|| false).unwrap(), approve);
                let seen = seen.lock().unwrap();
                // The handler got the question without the token.
                assert!(matches!(&seen[..], [Request::Approval { nonce, mac, ask }] if nonce.is_empty() && mac.is_empty() && ask.id == "q1"), "{seen:?}");
                drop(seen);
                drop(_listener);
            }
        }

        #[test]
        fn the_token_file_is_private_per_launch_and_goes_with_the_listener() {
            let dir = dir();
            let path = dir.path().join("app.sock");
            let token_file = token_path_for(&path);
            assert_eq!(token_file, dir.path().join("app.token"));
            let (listener, _) = approving(&path, true);
            let meta = std::fs::metadata(&token_file).unwrap();
            assert_eq!(meta.permissions().mode() & 0o777, 0o600);
            let first = std::fs::read_to_string(&token_file).unwrap();
            assert_eq!(first.len(), 64);
            assert!(first.bytes().all(|b| b.is_ascii_hexdigit()));
            drop(listener);
            assert!(!token_file.exists(), "removed with the socket");
            // The next launch has a token of its own.
            let (_next, _) = approving(&path, true);
            assert_ne!(std::fs::read_to_string(&token_file).unwrap(), first);
        }

        #[test]
        fn a_question_not_signed_with_the_launchs_token_is_never_shown_to_the_app() {
            let dir = dir();
            let path = dir.path().join("app.sock");
            let (_listener, seen) = approving(&path, true);
            let token = std::fs::read_to_string(token_path_for(&path)).unwrap();
            let nonce = |n: u8| format!("{n:02x}").repeat(16);
            let line = |mac: &dyn Fn(&ApprovalAsk, &str) -> String, nonce: &str| {
                let ask = ask("q1");
                let request = Request::Approval { nonce: nonce.into(), mac: mac(&ask, nonce), ask };
                let mut bytes = br#"{"request":"ping","protocol":1}"#.to_vec();
                bytes.push(b'\n');
                bytes.extend(serde_json::to_vec(&request).unwrap());
                bytes.push(b'\n');
                bytes
            };
            // No mac, a mac of another token, a mac of a nonce that is not this one, a malformed nonce.
            type MacOf<'a> = &'a dyn Fn(&ApprovalAsk, &str) -> String;
            let wrong: [(MacOf, String); 4] = [
                (&|_, _| String::new(), nonce(1)),
                (&|ask, nonce| ask_mac(&"0".repeat(64), ask, nonce), nonce(2)),
                (&|ask, _| ask_mac(&token, ask, &"ee".repeat(16)), nonce(3)),
                (&|ask, nonce| ask_mac(&token, ask, nonce), "xyz".into()),
            ];
            for (mac, nonce) in &wrong {
                let replies = exchange(&path, &line(mac, nonce));
                assert_eq!(replies.len(), 2, "{replies:?}");
                let reply: Reply = serde_json::from_str(&replies[1]).unwrap();
                assert!(!reply.ok && reply.answer.is_none(), "{reply:?}");
                assert!(reply.error.unwrap().contains("not authorised"));
            }
            assert!(seen.lock().unwrap().is_empty(), "the handler was never called");
            // The right mac is accepted (the connection stays open while the question is: no
            // half-close, which would withdraw it), and the answer proves the same nonce and question.
            let good = line(&|ask, nonce| ask_mac(&token, ask, nonce), &nonce(9));
            let ask_of = ask("q1");
            let mut stream = UnixStream::connect(&path).unwrap();
            stream.set_read_timeout(Some(Duration::from_secs(5))).unwrap();
            stream.write_all(&good).unwrap();
            let mut text = String::new();
            let mut chunk = [0u8; 4096];
            while text.lines().count() < 2 {
                let read = stream.read(&mut chunk).unwrap();
                assert!(read > 0, "the app answered");
                text.push_str(&String::from_utf8_lossy(&chunk[..read]));
            }
            let hello: Reply = serde_json::from_str(text.lines().next().unwrap()).unwrap();
            assert_eq!(hello.launch.as_deref(), Some(launch_id(&token).as_str()), "the ping says which launch");
            let reply: Reply = serde_json::from_str(text.lines().nth(1).unwrap()).unwrap();
            let answer = reply.answer.expect("an answer");
            assert_eq!((answer.id.as_str(), answer.approve), ("q1", true));
            assert_eq!(answer.proof, answer_proof(&token, &ask_of, &nonce(9), true));
            // The same line again (a replay) is not shown a second time.
            let replies = exchange(&path, &good);
            let reply: Reply = serde_json::from_str(&replies[1]).unwrap();
            assert!(!reply.ok && reply.error.unwrap().contains("already sent"), "{replies:?}");
            assert_eq!(seen.lock().unwrap().len(), 1);
        }

        #[test]
        fn the_client_never_sends_the_token() {
            let dir = dir();
            let path = dir.path().join("app.sock");
            let token = "ab".repeat(32);
            std::fs::write(token_path_for(&path), &token).unwrap();
            std::fs::set_permissions(token_path_for(&path), std::fs::Permissions::from_mode(0o600)).unwrap();
            let listener = UnixListener::bind(&path).unwrap();
            std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600)).unwrap();
            let seen = Arc::new(Mutex::new(String::new()));
            let log = Arc::clone(&seen);
            let impostor = std::thread::spawn(move || {
                let (mut stream, _) = listener.accept().unwrap();
                stream.set_read_timeout(Some(Duration::from_secs(5))).unwrap();
                // Answers the ping as a launch that has the token's launch id, then records what comes.
                let mut buffer = Vec::new();
                let mut chunk = [0u8; 4096];
                let hello = format!("{{\"ok\":true,\"protocol\":1,\"launch\":\"{}\"}}\n", launch_id(&token));
                let mut greeted = false;
                while buffer.iter().filter(|b| **b == b'\n').count() < 2 {
                    let Ok(read) = stream.read(&mut chunk) else { break };
                    if read == 0 { break }
                    buffer.extend_from_slice(&chunk[..read]);
                    if !greeted && buffer.contains(&b'\n') {
                        stream.write_all(hello.as_bytes()).unwrap();
                        greeted = true;
                    }
                }
                *log.lock().unwrap() = String::from_utf8_lossy(&buffer).into_owned();
                // Never answers: holds the connection until the caller gives up.
                std::thread::sleep(Duration::from_millis(1500));
            });
            let client = AppClient::new(path.clone());
            // The impostor never answers: the question is given up when the caller cancels.
            let start = std::time::Instant::now();
            let result = client.ask_approval(&ask("q1"), &|| start.elapsed() > Duration::from_millis(400));
            assert!(matches!(result, Err(ClientError::Cancelled)), "{result:?}");
            impostor.join().unwrap();
            let received = seen.lock().unwrap().clone();
            assert!(received.contains("\"request\":\"approval\""), "{received}");
            assert!(!received.contains(&"ab".repeat(32)), "the token never travels: {received}");
            assert!(!received.contains("\"token\""), "{received}");
        }

        #[test]
        fn an_app_of_another_launch_is_not_asked() {
            let dir = dir();
            let path = dir.path().join("app.sock");
            let (_listener, seen) = approving(&path, true);
            // The token file of another launch (a race between two launches, or a stale file).
            let token_file = token_path_for(&path);
            std::fs::write(&token_file, "cd".repeat(32)).unwrap();
            let client = AppClient::new(path.clone());
            let error = client.ask_approval(&ask("q1"), &|| false).unwrap_err();
            assert!(error.to_string().contains("restarted"), "{error}");
            assert!(client.reachable().is_err());
            assert!(seen.lock().unwrap().is_empty(), "the question was not sent");
        }

        #[test]
        fn a_connection_that_asks_nothing_does_not_stay_for_ever() {
            let dir = dir();
            let path = dir.path().join("app.sock");
            let (_listener, _) = approving(&path, true);
            let mut stream = UnixStream::connect(&path).unwrap();
            stream.set_read_timeout(Some(Duration::from_secs(10))).unwrap();
            let started = std::time::Instant::now();
            let mut chunk = [0u8; 256];
            // Pings, one after another: each is answered, until the connection's time is up.
            loop {
                if stream.write_all(b"{\"request\":\"ping\",\"protocol\":1}\n").is_err() {
                    break;
                }
                match stream.read(&mut chunk) {
                    Ok(0) | Err(_) => break,
                    Ok(_) => {}
                }
                std::thread::sleep(Duration::from_millis(200));
                assert!(started.elapsed() < Duration::from_secs(8), "still connected");
            }
            assert!(started.elapsed() >= Duration::from_secs(2), "not cut off at once");
        }

        #[test]
        fn a_socket_in_a_folder_others_can_enter_is_not_used() {
            let dir = dir();
            let path = dir.path().join("app.sock");
            let (_listener, _) = approving(&path, true);
            let client = AppClient::new(path.clone());
            assert!(client.ask_approval(&ask("q1"), &|| false).unwrap());
            // Group or others can write to it, or only look into it: not used.
            for mode in [0o777, 0o770, 0o750, 0o705] {
                std::fs::set_permissions(dir.path(), std::fs::Permissions::from_mode(mode)).unwrap();
                let error = client.ask_approval(&ask("q2"), &|| false).unwrap_err();
                assert!(error.to_string().contains("is open to other users"), "{mode:o}: {error}");
            }
            std::fs::set_permissions(dir.path(), std::fs::Permissions::from_mode(0o700)).unwrap();
            // The folder reached through a link is not trusted either.
            let link = dir.path().join("link");
            let elsewhere = tempfile::Builder::new().prefix("tx").permissions(<std::fs::Permissions as std::os::unix::fs::PermissionsExt>::from_mode(0o700)).tempdir().unwrap();
            std::os::unix::fs::symlink(elsewhere.path(), &link).unwrap();
            let linked = AppClient::new(link.join("app.sock"));
            assert!(linked.ask_approval(&ask("q3"), &|| false).is_err());
            // Nobody listens in a folder that is not private either.
            let other = tempfile::Builder::new().prefix("tx").permissions(<std::fs::Permissions as std::os::unix::fs::PermissionsExt>::from_mode(0o700)).tempdir().unwrap();
            std::fs::set_permissions(other.path(), std::fs::Permissions::from_mode(0o775)).unwrap();
            let (handler, _) = recording(Reply::ok());
            let error = Listener::bind(&other.path().join("app.sock"), handler).unwrap_err();
            assert!(error.to_string().contains("is open to other users"), "{error}");
        }

        #[test]
        fn a_data_folder_others_can_write_to_does_not_disable_the_channel() {
            // Fedora and RHEL make folders 0775 (umask 002): the channel has a folder of its own,
            // made 0700, so the socket works there, and `connections_changed` and the rest with it.
            // A short folder name: the socket path stays under the address limit.
            let data = tempfile::Builder::new()
                .prefix("d")
                .permissions(<std::fs::Permissions as std::os::unix::fs::PermissionsExt>::from_mode(0o775))
                .tempdir_in("/tmp")
                .unwrap();
            std::fs::set_permissions(data.path(), std::fs::Permissions::from_mode(0o775)).unwrap();
            let path = socket_path_in(data.path());
            assert_eq!(path, data.path().join("channel").join("app.sock"));
            let (_listener, seen) = approving(&path, true);
            let mode = |p: &Path| std::fs::metadata(p).unwrap().permissions().mode() & 0o777;
            assert_eq!(mode(path.parent().unwrap()), 0o700);
            assert_eq!(mode(data.path()), 0o775, "the data folder is left as it was");
            let client = AppClient::new(path.clone());
            assert!(client.ask_approval(&ask("q1"), &|| false).unwrap());
            assert!(client.send(&Request::ConnectionsChanged {}).is_ok());
            assert!(seen.lock().unwrap().iter().any(|request| matches!(request, Request::ConnectionsChanged {})));
        }

        #[test]
        fn a_socket_path_too_long_for_a_unix_socket_is_refused_with_a_clear_message() {
            let dir = dir();
            let mut deep = dir.path().to_path_buf();
            while deep.as_os_str().len() < 100 {
                deep.push("a-long-folder-name");
            }
            let (handler, _) = recording(Reply::ok());
            let error = Listener::bind(&deep.join("app.sock"), handler).unwrap_err();
            let text = error.to_string();
            assert!(text.contains("too long for a Unix socket") && text.contains("XDG_RUNTIME_DIR or TMPDIR"), "{text}");
            assert!(!deep.exists(), "nothing was made");
            let client = AppClient::new(deep.join("app.sock"));
            assert!(client.reachable().unwrap_err().to_string().contains("too long"));
        }

        #[test]
        fn a_channel_folder_that_is_not_private_is_not_used_and_files_of_older_versions_go() {
            let data = dir();
            let channel = data.path().join("channel");
            std::fs::create_dir(&channel).unwrap();
            std::fs::set_permissions(&channel, std::fs::Permissions::from_mode(0o755)).unwrap();
            let (handler, _) = recording(Reply::ok());
            assert!(Listener::bind(&channel.join("app.sock"), handler).is_err(), "not forced: refused");
            // The socket and the token an older tablex left in the data folder: removed when nobody
            // answers, kept when somebody does.
            let old = data.path().join("app.sock");
            let (answering, _) = approving(&old, true);
            super::super::imp::remove_legacy(data.path());
            assert!(old.exists() && token_path_for(&old).exists(), "an older tablex that still runs keeps its files");
            drop(answering);
            let stale = UnixListener::bind(data.path().join("app.sock")).unwrap();
            std::fs::write(token_path_for(&old), "ab".repeat(32)).unwrap();
            drop(stale);
            super::super::imp::remove_legacy(data.path());
            assert!(!old.exists() && !token_path_for(&old).exists());
            // A token file is exactly 64 hex digits (and a newline): anything else is not removed.
            std::fs::remove_file(&old).ok();
            let token = token_path_for(&old);
            for (text, removed) in [("ab".repeat(32), true), (format!("{}\n", "cd".repeat(32)), true), ("ab".repeat(31), false), ("zz".repeat(32), false), ("ab".repeat(33), false)] {
                std::fs::write(&token, &text).unwrap();
                super::super::imp::remove_legacy(data.path());
                assert_eq!(!token.exists(), removed, "{text:?}");
            }
            std::fs::remove_file(&token).ok();
            // A file that is not a socket is left alone.
            std::fs::write(&old, "mine").unwrap();
            super::super::imp::remove_legacy(data.path());
            assert_eq!(std::fs::read_to_string(&old).unwrap(), "mine");
        }

        #[test]
        fn lines_split_on_newlines_and_drop_carriage_returns() {
            let mut lines = Lines::new(&b"a\r\nbb\n\nlast"[..]);
            let second = Duration::from_secs(1);
            assert_eq!(lines.next(second).unwrap(), Some(b"a".to_vec()));
            assert_eq!(lines.next(second).unwrap(), Some(b"bb".to_vec()));
            assert_eq!(lines.next(second).unwrap(), Some(Vec::new()));
            // The stream ends in the middle of a line: no request.
            assert_eq!(lines.next(second).unwrap(), None);
        }
    }
}

#[cfg(test)]
mod path_tests {
    use super::*;

    #[test]
    fn a_deep_data_dir_gets_a_short_socket_path_both_sides_agree_on() {
        let short = std::path::Path::new("/Users/someone/Library/Application Support/tablex");
        assert_eq!(socket_path_in(short), short.join(CHANNEL_DIR).join(SOCKET_FILE));
        let deep = std::path::Path::new("/private/tmp/a-very-long-folder-name-for-tests/another-long-folder/and-one-more/tablex");
        let path = socket_path_in(deep);
        // Bound through the staging folder `<parent>/.<name>.<pid>.tmp/s`: within a socket address.
        let staged = path.parent().unwrap().as_os_str().len() + path.file_name().unwrap().len() + ".9999999.tmp/s".len() + 2;
        assert!(staged < 104, "{path:?} stages at {staged} bytes");
        assert_eq!(path, socket_path_in(deep), "the same for the app and the MCP server");
        assert_ne!(path, socket_path_in(std::path::Path::new("/private/tmp/other-long-folder-name-for-tests/another-long-folder/tablex")));
        assert!(path.file_name().unwrap().to_string_lossy().starts_with("tablex-"));
    }
}
