//! Host side of the optional MongoDB Shell plugin.
//!
//! The plugin (`tablex-mongo-shell`) is an executable, never code loaded into
//! the app. [`MongoShell::run`] spawns it for one script (no shell, no
//! arguments, a cleared environment, a temporary working directory, stdin /
//! stdout as the framed protocol of [`engine_driver_api::mongo_shell`], stderr
//! captured and capped) and a worker thread pumps frames: every `command` frame
//! goes to the session's `mongo_command` op (`read_only` unless the run allows
//! writes) and its reply goes back. The caller never blocks: [`ShellRun`]
//! starts the work and is polled or waited on.
//!
//! Stop is [`ShellRun::cancel`]: it cancels the in-flight driver operation and
//! kills the child. A wall-clock limit first cancels the driver operation (the
//! script ends with what it printed so far), then kills the child if it still
//! lingers. A child that exits without `done` is [`ShellHostError::Crashed`];
//! a malformed or oversized frame kills it and is [`ShellHostError::Protocol`].
//! Dropping a [`ShellRun`] kills the child.

use crate::loader::{LoaderError, QueryRunner};
use engine_driver_api::mongo_shell::{
    read_frame, write_frame, CommandCall, CommandFailure, CommandFailureKind, CommandReply,
    FailureKind, FrameError, FromChild, ShellReply, ShellRequest, ToChild, PROTOCOL_VERSION,
};
use engine_driver_api::Capability;
use serde::Deserialize;
use serde_json::value::RawValue;
use std::io::{BufReader, Read};
use std::path::{Path, PathBuf};
use std::process::{Child, ChildStdin, ChildStdout, Command, Stdio};
use std::sync::atomic::{AtomicU8, Ordering};
use std::sync::mpsc::{self, RecvTimeoutError};
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;
use std::time::Duration;

/// How long past `timeout_ms` the script may take to wind down after its
/// driver operation was cancelled before the child is killed.
const KILL_GRACE: Duration = Duration::from_secs(3);
/// How long past `timeout_ms` the host waits before it cancels the driver
/// operation (the child enforces the limit itself while JavaScript runs).
const CANCEL_GRACE: Duration = Duration::from_millis(500);
/// Most stderr kept for the crash message.
const STDERR_TAIL_BYTES: usize = 8 * 1024;

#[derive(Debug, thiserror::Error)]
pub enum ShellHostError {
    #[error("cannot start the MongoDB Shell plugin {path}: {source}")]
    Spawn {
        path: PathBuf,
        source: std::io::Error,
    },
    #[error("the MongoDB Shell plugin speaks protocol {found}, this app speaks {expected}")]
    ProtocolMismatch { found: u32, expected: u32 },
    #[error("the MongoDB Shell plugin sent an invalid message: {0}")]
    Protocol(String),
    #[error("the shell plugin stopped unexpectedly{0}")]
    Crashed(String),
    #[error("the script was stopped")]
    Cancelled,
    #[error("the script ran longer than {0} ms and was stopped")]
    TimedOut(u64),
    #[error("this MongoDB driver cannot run shell scripts (it lacks `mongo.command`)")]
    Unsupported,
    #[error("the shell plugin's pipe failed: {0}")]
    Io(#[source] std::io::Error),
    #[error(transparent)]
    Driver(#[from] LoaderError),
}

/// Where a script's server commands go. Implemented over a driver session;
/// tests use a fake. Called from the run's worker thread.
pub trait ShellCommands: Send + Sync + 'static {
    /// Runs `command` (canonical Extended JSON) in `database`; the reply is the
    /// server's reply document as canonical Extended JSON.
    fn run(
        &self,
        database: &str,
        command: &RawValue,
        read_only: bool,
    ) -> Result<Box<RawValue>, CommandFailure>;

    /// Stops the command in flight, if any, and makes the next one fail as cancelled.
    fn cancel(&self);

    /// Closes server cursors a stopped or failed run left open. Runs after the
    /// run's own operation was cancelled, so it must use a fresh one. Best
    /// effort; the default does nothing.
    fn kill_cursors(&self, _cursors: &[OpenCursor]) {}
}

/// A server cursor a script opened and had not exhausted or closed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OpenCursor {
    pub database: String,
    pub collection: String,
    pub id: i64,
}

/// The MongoDB Shell plugin executable.
#[derive(Debug, Clone)]
pub struct MongoShell {
    executable: PathBuf,
}

impl MongoShell {
    /// `executable` is the installed `tablex-mongo-shell` binary. Nothing runs
    /// until [`MongoShell::run`]; signature and manifest handling belong to
    /// the installer.
    pub fn new(executable: impl Into<PathBuf>) -> Self {
        Self {
            executable: executable.into(),
        }
    }

    pub fn executable(&self) -> &Path {
        &self.executable
    }

    /// Starts `request` against `session`. Returns at once; the work runs on a
    /// worker thread (see [`ShellRun`]).
    pub fn run(
        &self,
        session: &crate::DriverSession,
        request: ShellRequest,
    ) -> Result<ShellRun, ShellHostError> {
        if !session.capabilities().has(Capability::MongoCommand) {
            return Err(ShellHostError::Unsupported);
        }
        let commands = RunnerCommands {
            runner: session.query_runner()?,
            // The run's own operation is cancelled by Stop; closing cursors needs another.
            cleanup: session.query_runner()?,
            deadline: request
                .timeout_ms
                .map(|ms| std::time::Instant::now() + Duration::from_millis(ms)),
        };
        self.run_with(Arc::new(commands), request)
    }

    /// [`MongoShell::run`] against any [`ShellCommands`].
    pub fn run_with(
        &self,
        commands: Arc<dyn ShellCommands>,
        request: ShellRequest,
    ) -> Result<ShellRun, ShellHostError> {
        ShellRun::start(&self.executable, commands, request)
    }
}

/// `mongo_command` over one session's query runner.
struct RunnerCommands {
    runner: QueryRunner,
    cleanup: QueryRunner,
    /// When the run's time limit ends, if it has one.
    deadline: Option<std::time::Instant>,
}

/// How long past the time limit the server may keep working on a command: the
/// host cancels first (at `CANCEL_GRACE`), this only bounds a server nobody can
/// reach any more.
const SERVER_TIME_GRACE: Duration = Duration::from_secs(1);

fn command_request(
    runner: &QueryRunner,
    database: &str,
    command: &str,
    read_only: bool,
    max_time_ms: Option<u64>,
) -> String {
    let limit = max_time_ms.map_or_else(String::new, |ms| format!(r#","max_time_ms":{ms}"#));
    format!(
        r#"{{"operation_id":{},"database":{},"command":{},"read_only":{}{limit}}}"#,
        runner.operation_id().0,
        serde_json::Value::String(database.to_owned()),
        command,
        read_only
    )
}

#[derive(Deserialize)]
struct Envelope {
    ok: bool,
    #[serde(default)]
    value: Option<ReplyBody>,
    #[serde(default)]
    error: Option<ErrorBody>,
}

#[derive(Deserialize)]
struct ReplyBody {
    reply: Box<RawValue>,
}

#[derive(Deserialize)]
struct ErrorBody {
    code: String,
    message: String,
}

fn failed(message: impl Into<String>) -> CommandFailure {
    CommandFailure {
        kind: CommandFailureKind::Failed,
        code: None,
        message: message.into(),
    }
}

impl ShellCommands for RunnerCommands {
    fn run(
        &self,
        database: &str,
        command: &RawValue,
        read_only: bool,
    ) -> Result<Box<RawValue>, CommandFailure> {
        // The server's own time limit follows the run's, so a command nobody
        // can cancel any more still ends.
        let max_time_ms = self.deadline.map(|deadline| {
            let left = deadline.saturating_duration_since(std::time::Instant::now());
            u64::try_from((left + SERVER_TIME_GRACE).as_millis()).unwrap_or(u64::MAX)
        });
        let request = command_request(&self.runner, database, command.get(), read_only, max_time_ms);
        let response = self
            .runner
            .mongo_command(&request)
            .map_err(|error| failed(error.to_string()))?;
        let envelope: Envelope =
            serde_json::from_str(&response).map_err(|error| failed(format!("bad driver reply: {error}")))?;
        match envelope {
            Envelope {
                ok: true,
                value: Some(body),
                ..
            } => Ok(body.reply),
            Envelope {
                error: Some(error), ..
            } => Err(match error.code.as_str() {
                "query.cancelled" => CommandFailure {
                    kind: CommandFailureKind::Cancelled,
                    code: None,
                    message: error.message,
                },
                "query.read_only_violation" | "query.bad_request" | "query.unsupported" => {
                    CommandFailure {
                        kind: CommandFailureKind::Refused,
                        code: Some(error.code.clone()),
                        message: error.message,
                    }
                }
                _ => failed(error.message),
            }),
            _ => Err(failed("the driver's reply has neither a value nor an error")),
        }
    }

    fn cancel(&self) {
        // Best effort: the run is being torn down either way.
        let _ = self.runner.cancel_operation();
    }

    fn kill_cursors(&self, cursors: &[OpenCursor]) {
        let runner = self.cleanup.clone();
        kill_cursors_within(CLEANUP_BUDGET, cursors, move |database, command, max_time_ms| {
            let request = command_request(&runner, database, command, true, Some(max_time_ms));
            let _ = runner.mongo_command(&request);
        });
    }
}

/// The whole cleanup of an abnormal end may take this long, however many cursors are open
/// and whether or not the server answers.
const CLEANUP_BUDGET: Duration = Duration::from_secs(5);

/// One `killCursors` per namespace (all its ids), sent in turn by `send(database, command,
/// max_time_ms)`, within `budget` in total: each command gets what is left of it as its server
/// limit, and the caller stops waiting when the budget is spent (a `send` stuck on an unreachable
/// server is left to finish on its own thread).
fn kill_cursors_within(
    budget: Duration,
    cursors: &[OpenCursor],
    send: impl Fn(&str, &str, u64) + Send + 'static,
) {
    let mut groups: Vec<(String, String, Vec<i64>)> = Vec::new();
    for cursor in cursors {
        match groups
            .iter_mut()
            .find(|(database, collection, _)| *database == cursor.database && *collection == cursor.collection)
        {
            Some((_, _, ids)) => ids.push(cursor.id),
            None => groups.push((cursor.database.clone(), cursor.collection.clone(), vec![cursor.id])),
        }
    }
    if groups.is_empty() {
        return;
    }
    let started = std::time::Instant::now();
    let (done, finished) = mpsc::channel::<()>();
    let spawned = std::thread::Builder::new()
        .name("tablex-mongo-shell-cursors".into())
        .spawn(move || {
            for (database, collection, ids) in groups {
                let left = budget.saturating_sub(started.elapsed());
                if left.is_zero() {
                    break;
                }
                let ids: Vec<String> = ids.iter().map(|id| format!(r#"{{"$numberLong":"{id}"}}"#)).collect();
                let command = format!(
                    r#"{{"killCursors":{},"cursors":[{}]}}"#,
                    serde_json::Value::String(collection),
                    ids.join(",")
                );
                send(&database, &command, left.as_millis().max(1) as u64);
            }
            let _ = done.send(());
        });
    if spawned.is_ok() {
        let _ = finished.recv_timeout(budget);
    }
}

/// Server cursors the script has open, learned from the replies that pass by.
#[derive(Default)]
struct CursorTracker {
    open: Vec<OpenCursor>,
}

#[derive(Deserialize)]
struct LongTag {
    #[serde(rename = "$numberLong")]
    value: String,
}

#[derive(Deserialize)]
struct CommandProbe {
    #[serde(rename = "getMore", default)]
    get_more: Option<LongTag>,
    #[serde(default)]
    cursors: Vec<LongTag>,
}

#[derive(Deserialize)]
struct ReplyProbe {
    cursor: CursorProbe,
}

#[derive(Deserialize)]
struct CursorProbe {
    id: LongTag,
    ns: String,
}

impl CursorTracker {
    /// Notes what `reply` (to `call`) says about open cursors. Never fails: a
    /// reply that does not look like a cursor reply is ignored.
    fn observe(&mut self, call: &CommandCall, reply: &RawValue) {
        let command = call.command.get();
        if command.starts_with(r#"{"killCursors""#) {
            if let Ok(probe) = serde_json::from_str::<CommandProbe>(command) {
                let killed: Vec<i64> = probe.cursors.iter().filter_map(|tag| tag.value.parse().ok()).collect();
                self.open.retain(|cursor| !killed.contains(&cursor.id));
            }
            return;
        }
        if !reply.get().contains(r#""cursor""#) {
            return;
        }
        let Ok(ReplyProbe { cursor }) = serde_json::from_str::<ReplyProbe>(reply.get()) else {
            return;
        };
        let Ok(id) = cursor.id.value.parse::<i64>() else {
            return;
        };
        if id == 0 {
            let finished = command
                .starts_with(r#"{"getMore""#)
                .then(|| serde_json::from_str::<CommandProbe>(command).ok())
                .flatten()
                .and_then(|probe| probe.get_more)
                .and_then(|tag| tag.value.parse::<i64>().ok());
            if let Some(finished) = finished {
                self.open.retain(|cursor| cursor.id != finished);
            }
        } else if !self.open.iter().any(|open| open.id == id) {
            let collection = cursor.ns.split_once('.').map_or(cursor.ns.as_str(), |(_, rest)| rest);
            self.open.push(OpenCursor {
                database: call.database.clone(),
                collection: collection.to_owned(),
                id,
            });
        }
    }
}

const STOP_NONE: u8 = 0;
const STOP_CANCELLED: u8 = 1;
const STOP_TIMED_OUT: u8 = 2;

struct Shared {
    child: Mutex<Option<Child>>,
    stop: AtomicU8,
    commands: Arc<dyn ShellCommands>,
}

impl Shared {
    /// What Stop does while the run is alive: nothing once the child was reaped (`wait` ended
    /// the run). Killing through the host's own `Child` is safe even if the process already
    /// exited: it is not reaped until `wait`, so its pid cannot belong to anyone else yet.
    fn cancel(&self) {
        let alive = self.child.lock().is_ok_and(|child| child.is_some());
        if alive {
            self.stop_with(STOP_CANCELLED);
            self.commands.cancel();
            self.kill_child();
        }
    }

    fn kill_child(&self) {
        if let Ok(mut child) = self.child.lock() {
            if let Some(child) = child.as_mut() {
                let _ = child.kill();
            }
        }
    }

    fn stop_with(&self, reason: u8) {
        let _ = self
            .stop
            .compare_exchange(STOP_NONE, reason, Ordering::SeqCst, Ordering::SeqCst);
    }
}

/// Stops one script run from any thread: see [`ShellRun::cancel_handle`].
#[derive(Clone)]
pub struct ShellCancel {
    shared: Arc<Shared>,
}

impl ShellCancel {
    /// Does exactly what [`ShellRun::cancel`] does (cancels the in-flight driver operation and
    /// kills the child through the host's own handle). Idempotent, and nothing once the run
    /// has ended.
    pub fn cancel(&self) {
        self.shared.cancel();
    }
}

/// One script run in flight. [`ShellRun::wait`] for the result; dropping it
/// stops the run.
pub struct ShellRun {
    shared: Arc<Shared>,
    worker: Option<JoinHandle<Result<ShellReply, ShellHostError>>>,
    timeout_ms: Option<u64>,
    /// Ends the watchdog when the run does (dropped with the run).
    _watchdog_stop: Option<mpsc::Sender<()>>,
    work_dir: Option<TempDir>,
    stderr_tail: Arc<Mutex<Vec<u8>>>,
    /// Fires when the stderr reader has seen the end of the pipe, so the crash
    /// message is built from everything the child wrote.
    stderr_done: mpsc::Receiver<()>,
}

impl ShellRun {
    fn start(
        executable: &Path,
        commands: Arc<dyn ShellCommands>,
        request: ShellRequest,
    ) -> Result<Self, ShellHostError> {
        let work_dir = TempDir::create().map_err(ShellHostError::Io)?;
        let mut command = Command::new(executable);
        command
            .env_clear()
            .current_dir(work_dir.path())
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        #[cfg(windows)]
        {
            use std::os::windows::process::CommandExt;
            // The system root is how Windows finds its own DLLs; no console window.
            if let Some(root) = std::env::var_os("SystemRoot") {
                command.env("SystemRoot", root);
            }
            command.creation_flags(0x0800_0000);
        }
        let mut child = command.spawn().map_err(|source| ShellHostError::Spawn {
            path: executable.to_path_buf(),
            source,
        })?;
        let (stdin, stdout, stderr) = match (child.stdin.take(), child.stdout.take(), child.stderr.take()) {
            (Some(stdin), Some(stdout), Some(stderr)) => (stdin, stdout, stderr),
            _ => {
                let _ = child.kill();
                let _ = child.wait();
                return Err(ShellHostError::Io(std::io::Error::other("the plugin's pipes are missing")));
            }
        };
        let stderr_tail = Arc::new(Mutex::new(Vec::new()));
        let stderr_done = spawn_stderr_reader(stderr, Arc::clone(&stderr_tail));
        let shared = Arc::new(Shared {
            child: Mutex::new(Some(child)),
            stop: AtomicU8::new(STOP_NONE),
            commands,
        });
        let timeout_ms = request.timeout_ms;
        let watchdog_stop = timeout_ms.map(|ms| spawn_watchdog(Arc::clone(&shared), ms));
        let worker = {
            let shared = Arc::clone(&shared);
            std::thread::Builder::new()
                .name("tablex-mongo-shell-host".into())
                .spawn(move || {
                    let mut cursors = CursorTracker::default();
                    let outcome = pump(&shared, stdin, stdout, request, &mut cursors);
                    // A run that did not end cleanly (stopped, timed out, crashed,
                    // failed) must not leave server cursors behind.
                    let ended_badly = ended_badly(&outcome, shared.stop.load(Ordering::SeqCst));
                    if ended_badly && !cursors.open.is_empty() {
                        shared.commands.kill_cursors(&cursors.open);
                    }
                    outcome
                })
                .map_err(ShellHostError::Io)?
        };
        Ok(Self {
            shared,
            worker: Some(worker),
            timeout_ms,
            _watchdog_stop: watchdog_stop,
            work_dir: Some(work_dir),
            stderr_tail,
            stderr_done,
        })
    }

    /// Stops the run: cancels the in-flight driver operation and kills the
    /// child. [`ShellRun::wait`] then returns [`ShellHostError::Cancelled`].
    pub fn cancel(&self) {
        self.shared.cancel();
    }

    /// A handle that stops this run from another thread, also while [`ShellRun::wait`]
    /// blocks on it (`wait` takes the run, so `cancel` itself cannot be called then).
    pub fn cancel_handle(&self) -> ShellCancel {
        ShellCancel {
            shared: Arc::clone(&self.shared),
        }
    }

    /// The plugin process's id while it is alive.
    pub fn process_id(&self) -> Option<u32> {
        self.shared
            .child
            .lock()
            .ok()
            .and_then(|child| child.as_ref().map(Child::id))
    }

    /// Whether the run is over (so [`ShellRun::wait`] will not block).
    pub fn is_finished(&self) -> bool {
        self.worker.as_ref().is_none_or(JoinHandle::is_finished)
    }

    /// Blocks until the run ends. Call it from a worker thread, not the UI's.
    pub fn wait(mut self) -> Result<ShellReply, ShellHostError> {
        let outcome = match self.worker.take().map(JoinHandle::join) {
            Some(Ok(outcome)) => outcome,
            Some(Err(_)) => Err(ShellHostError::Crashed(" (the host's worker panicked)".into())),
            None => Err(ShellHostError::Cancelled),
        };
        self.shared.kill_child();
        let status = self
            .shared
            .child
            .lock()
            .ok()
            .and_then(|mut child| child.take())
            .and_then(|mut child| child.wait().ok());
        drop(self.work_dir.take());
        match (self.shared.stop.load(Ordering::SeqCst), outcome) {
            (STOP_CANCELLED, _) => Err(ShellHostError::Cancelled),
            (STOP_TIMED_OUT, Ok(mut reply)) => {
                // The cancel the timeout caused ends the script as "cancelled": say why.
                if let Some(error) = reply.error.as_mut().filter(|e| e.kind == FailureKind::Cancelled) {
                    error.kind = FailureKind::Timeout;
                    error.message = format!(
                        "the script ran longer than {} ms and was stopped",
                        self.timeout_ms.unwrap_or_default()
                    );
                }
                Ok(reply)
            }
            (STOP_TIMED_OUT, Err(_)) => Err(ShellHostError::TimedOut(self.timeout_ms.unwrap_or_default())),
            (_, Err(ShellHostError::Crashed(_))) => {
                let stderr = self.stderr_text();
                let detail = match (status, stderr.is_empty()) {
                    (Some(status), true) => format!(" ({status})"),
                    (Some(status), false) => format!(" ({status}): {stderr}"),
                    (None, true) => String::new(),
                    (None, false) => format!(": {stderr}"),
                };
                Err(ShellHostError::Crashed(detail))
            }
            (_, outcome) => outcome,
        }
    }

    fn stderr_text(&self) -> String {
        // The child is dead, so its end of the pipe is closed (a lingering
        // grandchild could hold it: do not wait for ever).
        let _ = self.stderr_done.recv_timeout(Duration::from_secs(2));
        let tail = self
            .stderr_tail
            .lock()
            .map(|tail| String::from_utf8_lossy(&tail).trim().to_owned())
            .unwrap_or_default();
        tail
    }
}

impl Drop for ShellRun {
    fn drop(&mut self) {
        if self.worker.is_some() {
            self.shared.stop_with(STOP_CANCELLED);
            self.shared.commands.cancel();
        }
        self.shared.kill_child();
        if let Some(child) = self.shared.child.lock().ok().and_then(|mut child| child.take()) {
            let mut child = child;
            let _ = child.wait();
        }
    }
}

/// Whether the cursors a run left open must be closed. Judged from how the
/// run ended: a time limit that fires just after a clean `done` must not close
/// the result cursor the reply hands on (a user Stop, though, discards even a
/// finished reply).
fn ended_badly(outcome: &Result<ShellReply, ShellHostError>, stop: u8) -> bool {
    match outcome {
        Err(_) => true,
        Ok(reply) => reply.error.is_some() || stop == STOP_CANCELLED,
    }
}

/// The worker: handshake, send the request, then answer commands until `done`.
fn pump(
    shared: &Shared,
    mut stdin: ChildStdin,
    stdout: ChildStdout,
    request: ShellRequest,
    cursors: &mut CursorTracker,
) -> Result<ShellReply, ShellHostError> {
    let mut stdout = BufReader::new(stdout);
    let crashed = || ShellHostError::Crashed(String::new());
    let hello = read_child_frame(shared, &mut stdout)?.ok_or_else(crashed)?;
    match hello {
        FromChild::Hello { protocol } if protocol == PROTOCOL_VERSION => {}
        FromChild::Hello { protocol } => {
            shared.kill_child();
            return Err(ShellHostError::ProtocolMismatch {
                found: protocol,
                expected: PROTOCOL_VERSION,
            });
        }
        _ => {
            shared.kill_child();
            return Err(ShellHostError::Protocol("the first frame was not hello".into()));
        }
    }
    let read_only = !request.allow_writes;
    write_frame(&mut stdin, &ToChild::Run(request)).map_err(|_| crashed())?;
    let mut last_id = 0;
    loop {
        match read_child_frame(shared, &mut stdout)?.ok_or_else(crashed)? {
            FromChild::Done(reply) => return Ok(reply),
            FromChild::Command(call) => {
                if call.id <= last_id {
                    shared.kill_child();
                    return Err(ShellHostError::Protocol(format!(
                        "command id {} is not after {last_id}",
                        call.id
                    )));
                }
                last_id = call.id;
                let reply = answer(shared, &call, read_only);
                if let Some(document) = &reply.ok {
                    cursors.observe(&call, document);
                }
                write_frame(&mut stdin, &ToChild::Reply(reply)).map_err(|_| crashed())?;
            }
            FromChild::Hello { .. } => {
                shared.kill_child();
                return Err(ShellHostError::Protocol("a second hello".into()));
            }
        }
    }
}

fn read_child_frame(
    shared: &Shared,
    stdout: &mut BufReader<ChildStdout>,
) -> Result<Option<FromChild>, ShellHostError> {
    match read_frame::<FromChild>(stdout) {
        Ok(frame) => Ok(frame),
        Err(FrameError::Malformed(message)) => {
            shared.kill_child();
            Err(ShellHostError::Protocol(message))
        }
        Err(error @ FrameError::TooLarge(_)) => {
            shared.kill_child();
            Err(ShellHostError::Protocol(error.to_string()))
        }
        // The child died (or was killed) mid-frame or the pipe broke.
        Err(FrameError::Truncated | FrameError::Io(_)) => Err(ShellHostError::Crashed(String::new())),
    }
}

fn answer(shared: &Shared, call: &CommandCall, read_only: bool) -> CommandReply {
    if shared.stop.load(Ordering::SeqCst) == STOP_CANCELLED {
        return refusal(call.id, cancelled_failure());
    }
    match shared.commands.run(&call.database, &call.command, read_only) {
        Ok(document) => {
            // Never hand the child a frame it must refuse.
            if document.get().len() as u64 > engine_driver_api::mongo_shell::MAX_FRAME_BYTES - 1024 {
                return refusal(call.id, failed("the server's reply is larger than the shell can take (64 MiB)"));
            }
            CommandReply {
                id: call.id,
                ok: Some(document),
                error: None,
            }
        }
        Err(failure) => refusal(call.id, failure),
    }
}

fn cancelled_failure() -> CommandFailure {
    CommandFailure {
        kind: CommandFailureKind::Cancelled,
        code: None,
        message: "operation cancelled".into(),
    }
}

fn refusal(id: u64, failure: CommandFailure) -> CommandReply {
    CommandReply {
        id,
        ok: None,
        error: Some(failure),
    }
}

/// Cancels the driver operation at the time limit (the child ends the script
/// with what it printed), then kills the child if it lingers.
fn spawn_watchdog(shared: Arc<Shared>, timeout_ms: u64) -> mpsc::Sender<()> {
    let (stop, stopped) = mpsc::channel::<()>();
    let limit = Duration::from_millis(timeout_ms);
    let _ = std::thread::Builder::new()
        .name("tablex-mongo-shell-timeout".into())
        .spawn(move || {
            if !matches!(stopped.recv_timeout(limit + CANCEL_GRACE), Err(RecvTimeoutError::Timeout)) {
                return;
            }
            shared.stop_with(STOP_TIMED_OUT);
            shared.commands.cancel();
            if matches!(stopped.recv_timeout(KILL_GRACE), Err(RecvTimeoutError::Timeout)) {
                shared.kill_child();
            }
        });
    stop
}

fn spawn_stderr_reader(
    mut stderr: impl Read + Send + 'static,
    tail: Arc<Mutex<Vec<u8>>>,
) -> mpsc::Receiver<()> {
    let (done, finished) = mpsc::channel();
    let _ = std::thread::Builder::new()
        .name("tablex-mongo-shell-stderr".into())
        .spawn(move || {
            let mut chunk = [0u8; 1024];
            while let Ok(count) = stderr.read(&mut chunk) {
                if count == 0 {
                    break;
                }
                if let Ok(mut tail) = tail.lock() {
                    tail.extend_from_slice(&chunk[..count]);
                    let excess = tail.len().saturating_sub(STDERR_TAIL_BYTES);
                    tail.drain(..excess);
                }
            }
            let _ = done.send(());
        });
    finished
}

/// The plugin's working directory: empty, private, removed with the run.
struct TempDir(PathBuf);

impl TempDir {
    fn create() -> std::io::Result<Self> {
        use std::sync::atomic::AtomicU64;
        static NEXT: AtomicU64 = AtomicU64::new(0);
        let unique = format!(
            "tablex-mongo-shell-{}-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map_or(0, |elapsed| elapsed.as_nanos()),
            NEXT.fetch_add(1, Ordering::Relaxed)
        );
        let path = std::env::temp_dir().join(unique);
        std::fs::create_dir(&path)?;
        Ok(Self(path))
    }

    fn path(&self) -> &Path {
        &self.0
    }
}

impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn call(database: &str, command: &str) -> CommandCall {
        CommandCall {
            id: 1,
            database: database.into(),
            command: RawValue::from_string(command.into()).unwrap(),
        }
    }

    fn reply(text: &str) -> Box<RawValue> {
        RawValue::from_string(text.into()).unwrap()
    }

    fn open(tracker: &CursorTracker) -> Vec<(String, String, i64)> {
        tracker.open.iter().map(|c| (c.database.clone(), c.collection.clone(), c.id)).collect()
    }

    #[test]
    fn the_tracker_follows_cursors_from_open_to_exhausted_or_killed() {
        let mut tracker = CursorTracker::default();
        let find = call("app", r#"{"find":"items"}"#);
        tracker.observe(&find, &reply(r#"{"cursor":{"firstBatch":[{"a":1}],"id":{"$numberLong":"77"},"ns":"app.items"},"ok":{"$numberDouble":"1.0"}}"#));
        tracker.observe(&find, &reply(r#"{"cursor":{"firstBatch":[],"id":{"$numberLong":"78"},"ns":"app.logs.old"}}"#));
        // The same cursor seen again is not duplicated.
        let more = call("app", r#"{"getMore":{"$numberLong":"77"},"collection":"items"}"#);
        tracker.observe(&more, &reply(r#"{"cursor":{"nextBatch":[{"a":2}],"id":{"$numberLong":"77"},"ns":"app.items"}}"#));
        assert_eq!(open(&tracker), vec![("app".into(), "items".into(), 77), ("app".into(), "logs.old".into(), 78)]);
        // Exhausted by a getMore (id 0).
        tracker.observe(&more, &reply(r#"{"cursor":{"nextBatch":[],"id":{"$numberLong":"0"},"ns":"app.items"}}"#));
        assert_eq!(open(&tracker), vec![("app".into(), "logs.old".into(), 78)]);
        // Killed by the script.
        let kill = call("app", r#"{"killCursors":"logs.old","cursors":[{"$numberLong":"78"}]}"#);
        tracker.observe(&kill, &reply(r#"{"cursorsKilled":[{"$numberLong":"78"}],"ok":{"$numberDouble":"1.0"}}"#));
        assert!(tracker.open.is_empty());
    }

    #[test]
    fn replies_that_are_not_cursor_replies_or_that_are_garbled_are_ignored() {
        let mut tracker = CursorTracker::default();
        let command = call("app", r#"{"count":"items"}"#);
        tracker.observe(&command, &reply(r#"{"n":{"$numberInt":"3"},"ok":{"$numberDouble":"1.0"}}"#));
        tracker.observe(&command, &reply(r#"{"cursor":"not an object"}"#));
        tracker.observe(&command, &reply(r#"{"cursor":{"id":{"$numberLong":"zz"},"ns":"a.b"}}"#));
        tracker.observe(&call("app", r#"{"getMore":5}"#), &reply(r#"{"cursor":{"id":{"$numberLong":"0"},"ns":"a.b"}}"#));
        assert!(tracker.open.is_empty());
    }

    fn namespace(database: &str, collection: &str, id: i64) -> OpenCursor {
        OpenCursor { database: database.into(), collection: collection.into(), id }
    }

    #[test]
    fn cleanup_sends_one_kill_cursors_per_namespace_with_all_its_ids() {
        let sent = Arc::new(Mutex::new(Vec::<(String, String, u64)>::new()));
        let log = Arc::clone(&sent);
        kill_cursors_within(
            Duration::from_secs(5),
            &[namespace("app", "items", 1), namespace("app", "logs", 2), namespace("app", "items", 3), namespace("other", "items", 4)],
            move |database, command, max_ms| log.lock().unwrap().push((database.into(), command.into(), max_ms)),
        );
        let sent = sent.lock().unwrap();
        assert_eq!(sent.len(), 3, "three namespaces: {sent:?}");
        assert_eq!(sent[0].0, "app");
        assert_eq!(
            sent[0].1,
            r#"{"killCursors":"items","cursors":[{"$numberLong":"1"},{"$numberLong":"3"}]}"#
        );
        assert_eq!((sent[1].0.as_str(), sent[2].0.as_str()), ("app", "other"));
        assert!(sent.iter().all(|(_, _, ms)| (1..=5_000).contains(ms)));
        kill_cursors_within(Duration::from_secs(5), &[], |_, _, _| panic!("nothing is open"));
    }

    #[test]
    fn cleanup_against_a_server_that_never_answers_ends_within_its_budget() {
        let (_never, stuck) = mpsc::channel::<()>();
        let stuck = Mutex::new(stuck);
        let started = std::time::Instant::now();
        kill_cursors_within(
            Duration::from_millis(300),
            &[namespace("app", "a", 1), namespace("app", "b", 2), namespace("app", "c", 3)],
            // A driver call that blocks for good (the sender is kept alive by the test).
            move |_, _, _| {
                let _ = stuck.lock().unwrap().recv();
            },
        );
        let took = started.elapsed();
        assert!(took >= Duration::from_millis(250) && took < Duration::from_secs(2), "{took:?}");
    }

    struct CountingCommands(std::sync::atomic::AtomicUsize);

    impl ShellCommands for CountingCommands {
        fn run(&self, _: &str, _: &RawValue, _: bool) -> Result<Box<RawValue>, CommandFailure> {
            Err(failed("no server"))
        }

        fn cancel(&self) {
            self.0.fetch_add(1, Ordering::SeqCst);
        }
    }

    /// An executable that never says hello: the run blocks until it is stopped.
    #[cfg(unix)]
    fn silent_plugin() -> (PathBuf, PathBuf) {
        use std::os::unix::fs::PermissionsExt;
        let dir = std::env::temp_dir().join(format!("tablex-shell-cancel-{}-{}", std::process::id(), std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map_or(0, |d| d.as_nanos())));
        std::fs::create_dir_all(&dir).unwrap();
        let exe = dir.join("plugin.sh");
        std::fs::write(&exe, "#!/bin/sh\nexec sleep 60\n").unwrap();
        std::fs::set_permissions(&exe, std::fs::Permissions::from_mode(0o755)).unwrap();
        (dir, exe)
    }

    #[cfg(unix)]
    #[test]
    fn a_cancel_handle_stops_a_run_that_another_thread_waits_on() {
        let (dir, exe) = silent_plugin();
        let commands = Arc::new(CountingCommands(Default::default()));
        let request = ShellRequest {
            script: "1".into(),
            database: "app".into(),
            allow_writes: false,
            page_size: 50,
            timeout_ms: None,
            cursor: None,
        };
        let run = MongoShell::new(&exe).run_with(commands.clone(), request).expect("starts");
        let handle = run.cancel_handle();
        let waiter = std::thread::spawn(move || run.wait());
        std::thread::sleep(Duration::from_millis(200));
        let started = std::time::Instant::now();
        handle.cancel();
        handle.clone().cancel(); // idempotent, from a clone
        let outcome = waiter.join().unwrap();
        assert!(matches!(outcome, Err(ShellHostError::Cancelled)), "{outcome:?}");
        assert!(started.elapsed() < Duration::from_secs(5), "the child was killed, not waited out");
        let cancels = commands.0.load(Ordering::SeqCst);
        assert!(cancels >= 1, "the driver operation was cancelled");
        // After the run ended it is a no-op: no driver operation is cancelled any more.
        handle.cancel();
        assert_eq!(commands.0.load(Ordering::SeqCst), cancels);
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn a_time_limit_after_a_clean_done_does_not_close_the_result_cursor() {
        use engine_driver_api::mongo_shell::{FailureKind, ShellFailure, ShellResult};
        let reply = |error: Option<FailureKind>| ShellReply {
            output: vec![],
            output_truncated: false,
            result: ShellResult::None,
            database_after: "app".into(),
            error: error.map(|kind| ShellFailure {
                kind,
                message: String::new(),
                line: None,
                column: None,
                code: None,
                code_name: None,
            }),
        };
        // Clean `done`: only a user Stop makes it bad (the reply is discarded then).
        assert!(!ended_badly(&Ok(reply(None)), STOP_NONE));
        assert!(!ended_badly(&Ok(reply(None)), STOP_TIMED_OUT), "the watchdog fired after done");
        assert!(ended_badly(&Ok(reply(None)), STOP_CANCELLED));
        // A script that failed, or was stopped, or no `done` at all.
        assert!(ended_badly(&Ok(reply(Some(FailureKind::Script))), STOP_NONE));
        assert!(ended_badly(&Ok(reply(Some(FailureKind::Timeout))), STOP_TIMED_OUT));
        assert!(ended_badly(&Err(ShellHostError::Crashed(String::new())), STOP_NONE));
        assert!(ended_badly(&Err(ShellHostError::Cancelled), STOP_CANCELLED));
    }
}
