//! The local channel between the tablex app and `tablex-mcp`: a Unix domain socket in the app data
//! dir (`<app data dir>/app.sock`, mode 0600, so only this user) over which the MCP server asks the
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
//! ```
//!
//! and `{"ok":true}` (a ping also says `"protocol"`) or `{"ok":false,"error":"..."}` back. A
//! connection may carry several requests; [`AppClient`] sends a ping first and checks the version.
//!
//! [`Listener`] runs on threads of its own: accepting, reading and writing never touch the app's
//! UI thread. The app's [`Handler`] is called on a connection thread and is where the request is
//! handed to the UI. Only Unix has the socket: elsewhere [`Listener::bind`] and [`AppClient::send`]
//! say that the platform is not supported.

use crate::platform;
use serde::{Deserialize, Serialize};
use std::path::PathBuf;
use std::sync::Arc;

/// The version in the ping: a client and an app that disagree do not talk.
pub const PROTOCOL: u32 = 1;
/// The longest request (and reply) line, newline excluded.
pub const MAX_REQUEST_BYTES: usize = 1 << 20;
/// The socket's file name in the app data dir.
pub const SOCKET_FILE: &str = "app.sock";

/// Where the running app listens: `<app data dir>/app.sock` (`TABLEX_DATA_DIR` moves it with every
/// other tablex file).
pub fn socket_path() -> Option<PathBuf> {
    platform::app_data_dir().map(|dir| socket_path_in(&dir))
}

/// The longest socket path used as is: a Unix socket address holds about 104 bytes, and binding
/// goes through a staging folder beside it that adds about 30.
const MAX_SOCKET_PATH: usize = 70;

/// `<dir>/app.sock`, or, when that is too long for a socket address (a deep `TABLEX_DATA_DIR`), a
/// socket in the user's runtime folder named after `dir`, so the app and the MCP server still meet.
fn socket_path_in(dir: &std::path::Path) -> PathBuf {
    let path = dir.join(SOCKET_FILE);
    if path.as_os_str().len() <= MAX_SOCKET_PATH {
        return path;
    }
    use sha2::{Digest, Sha256};
    let digest = Sha256::digest(dir.as_os_str().as_encoded_bytes());
    let name: String = digest.iter().take(8).map(|byte| format!("{byte:02x}")).collect();
    // `XDG_RUNTIME_DIR` is the user's own (Linux); macOS's temp folder is per user too.
    let runtime = std::env::var_os("XDG_RUNTIME_DIR")
        .map(PathBuf::from)
        .filter(|dir| dir.is_absolute())
        .unwrap_or_else(std::env::temp_dir);
    runtime.join(format!("tablex-{name}.sock"))
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
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Reply {
    pub ok: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
    /// Only in the answer to a ping.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub protocol: Option<u32>,
}

impl Reply {
    pub fn ok() -> Self {
        Self {
            ok: true,
            error: None,
            protocol: None,
        }
    }

    pub fn error(message: impl Into<String>) -> Self {
        Self {
            ok: false,
            error: Some(message.into()),
            protocol: None,
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
        Ok(Request::Ping { protocol }) if protocol == PROTOCOL => Reply {
            ok: true,
            error: None,
            protocol: Some(PROTOCOL),
        },
        Ok(Request::Ping { protocol }) => Reply {
            ok: false,
            error: Some(format!(
                "unsupported protocol {protocol}: this tablex speaks protocol {PROTOCOL}"
            )),
            protocol: Some(PROTOCOL),
        },
        Ok(request) => handler(request),
        Err(error) => Reply::error(format!("not a request: {error}")),
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
    #[error("tablex cannot be reached from a program on this platform: the app channel is not supported here")]
    Unsupported,
    /// The channel broke (an I/O error, a reply that is not one, another protocol version).
    #[error("could not talk to tablex: {0}")]
    Failed(String),
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

    #[derive(Debug, PartialEq, Eq)]
    pub(super) enum LineError {
        TooLong,
        Timeout,
        Io(String),
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

    fn serve_connection(mut stream: UnixStream, handler: &Handler) {
        if stream.set_read_timeout(Some(READ_TICK)).is_err()
            || stream.set_write_timeout(Some(WRITE_TIMEOUT)).is_err()
        {
            return;
        }
        let Ok(reader) = stream.try_clone() else {
            return;
        };
        let mut lines = Lines::new(reader);
        loop {
            let reply = match lines.next(LINE_DEADLINE) {
                Ok(Some(line)) => handle_line(&line, handler.as_ref()),
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

    /// The app's end: removes its socket file when dropped.
    pub struct Listener {
        path: PathBuf,
        stop: Arc<AtomicBool>,
        accepting: Option<JoinHandle<()>>,
        /// Device and inode of the socket file this listener made.
        identity: (u64, u64),
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
        /// no other user can connect at any moment.
        pub fn bind(path: &Path, handler: Handler) -> Result<Self, BindError> {
            let parent = path
                .parent()
                .ok_or_else(|| io::Error::other("the socket path has no folder"))?;
            std::fs::create_dir_all(parent)?;
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
            let made = (|| {
                let socket = staging.join("s");
                let listener = UnixListener::bind(&socket)?;
                std::fs::set_permissions(&socket, std::fs::Permissions::from_mode(0o600))?;
                std::fs::rename(&socket, path)?;
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
                    .spawn(move || accept_loop(&listener, &stop, &handler))?
            };
            Ok(Self {
                path: path.to_path_buf(),
                stop,
                accepting: Some(accepting),
                identity: (meta.dev(), meta.ino()),
            })
        }

        pub fn path(&self) -> &Path {
            &self.path
        }
    }

    fn accept_loop(listener: &UnixListener, stop: &AtomicBool, handler: &Handler) {
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
            let (handler, active) = (Arc::clone(handler), Arc::clone(&active));
            let spawned = std::thread::Builder::new()
                .name("tablex-app-channel-conn".into())
                .spawn({
                    let active = Arc::clone(&active);
                    move || {
                        serve_connection(stream, &handler);
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

        /// Sends `request` (after a ping that checks the protocol) and returns tablex's answer.
        pub fn send(&self, request: &Request) -> Result<Reply, ClientError> {
            let mut stream = UnixStream::connect(&self.path).map_err(|error| match error.kind() {
                io::ErrorKind::NotFound | io::ErrorKind::ConnectionRefused => {
                    ClientError::NotRunning
                }
                _ => ClientError::Failed(error.to_string()),
            })?;
            let failed = |error: io::Error| ClientError::Failed(error.to_string());
            stream.set_read_timeout(Some(READ_TICK)).map_err(failed)?;
            stream
                .set_write_timeout(Some(WRITE_TIMEOUT))
                .map_err(failed)?;
            let mut out = serde_json::to_vec(&Request::Ping { protocol: PROTOCOL })
                .map_err(|error| ClientError::Failed(error.to_string()))?;
            out.push(b'\n');
            out.extend(
                serde_json::to_vec(request).map_err(|error| ClientError::Failed(error.to_string()))?,
            );
            out.push(b'\n');
            stream.write_all(&out).map_err(failed)?;
            let mut lines = Lines::new(&stream);
            let mut read = || -> Result<Reply, ClientError> {
                let line = lines
                    .next(CLIENT_TIMEOUT)
                    .map_err(|error| ClientError::Failed(format!("{error:?}")))?
                    .ok_or_else(|| ClientError::Failed("tablex closed the connection".into()))?;
                serde_json::from_slice(&line)
                    .map_err(|error| ClientError::Failed(format!("unreadable reply: {error}")))
            };
            let hello = read()?;
            if !hello.ok || hello.protocol != Some(PROTOCOL) {
                return Err(ClientError::Failed(hello.error.unwrap_or_else(|| {
                    format!(
                        "tablex speaks protocol {:?}, this program protocol {PROTOCOL}: update the older one",
                        hello.protocol
                    )
                })));
            }
            let reply = read()?;
            if reply.ok {
                Ok(reply)
            } else {
                Err(ClientError::Refused(
                    reply.error.unwrap_or_else(|| "no reason given".into()),
                ))
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
    fn the_socket_lives_in_the_app_data_dir() {
        assert!(socket_path()
            .expect("a data dir exists")
            .ends_with("tablex/app.sock"));
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
            tempfile::Builder::new().prefix("tx").tempdir().unwrap()
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
            let names: Vec<_> = std::fs::read_dir(dir.path())
                .unwrap()
                .map(|entry| entry.unwrap().file_name())
                .collect();
            assert_eq!(names, ["app.sock"], "the staging folder is gone");
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
        assert_eq!(socket_path_in(short), short.join(SOCKET_FILE));
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
