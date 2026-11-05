//! Host side of the SQL formatter: processes that format text and are never loaded into the app.
//!
//! Two kinds run here, with the same care for the child:
//!
//! * [`SqlFormatTool::format`]: the optional `tablex-sqruff` tool, spoken to in the framed protocol
//!   of [`engine_driver_api::sql_format`]. It runs with a cleared environment.
//! * [`run_filter`]: a command the user configured (reads SQL on stdin, writes it on stdout). The
//!   caller builds the [`Command`] (program, arguments, the environment the user's tool needs).
//!
//! Either is started per request with no shell, in a private working directory made for the run (mode
//! 0700 on Unix, removed after it: a formatter may read configuration from its working directory,
//! and a shared `/tmp` is not a place to take configuration from), in its own process
//! group on Unix (so a wrapper script's children die with it), with stdin / stdout piped and stderr
//! kept (its tail goes into the error). The caller blocks: run it on a background thread. It ends
//! when the child answers, or at the time limit ([`FormatHostError::TimedOut`]), or when
//! [`FormatCancel::cancel`] is called from any thread ([`FormatHostError::Cancelled`]): both kill the
//! child. Anything the child writes beyond [`MAX_FRAME_BYTES`] is refused.

use engine_driver_api::sql_format::{
    FailureKind, FormatRequest, FromTool, ToTool, MAX_FRAME_BYTES, PROTOCOL_VERSION,
};
use engine_driver_api::mongo_shell::{read_frame, write_frame, FrameError};
use std::io::{BufReader, Read, Write};
use std::path::{Path, PathBuf};
use std::process::{Child, ChildStdin, ChildStdout, Command, ExitStatus, Stdio};
use std::sync::atomic::{AtomicU8, Ordering};
use std::sync::mpsc::{self, RecvTimeoutError};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

/// Most stderr kept for an error message.
const STDERR_TAIL_BYTES: usize = 4 * 1024;
/// How long to wait for the end of stderr once the child is gone (a grandchild may hold the pipe).
const STDERR_GRACE: Duration = Duration::from_secs(1);

#[derive(Debug, thiserror::Error)]
pub enum FormatHostError {
    /// The program does not exist (the tool was removed, or the command is mistyped).
    #[error("{} was not found", .path.display())]
    NotInstalled { path: PathBuf },
    #[error("cannot start {}: {source}", .path.display())]
    Spawn {
        path: PathBuf,
        source: std::io::Error,
    },
    #[error("the formatter speaks protocol {found}, this app speaks {expected}")]
    ProtocolMismatch { found: u32, expected: u32 },
    /// A message or output that is not what was expected (malformed frame, not UTF-8, no hello).
    #[error("the formatter sent an invalid answer: {0}")]
    Protocol(String),
    /// The child ended without an answer, or (a command) with a failing status.
    #[error("{}", crashed_message(.status, .stderr))]
    Crashed {
        status: Option<String>,
        stderr: String,
    },
    #[error("the formatter ran longer than {} s and was stopped", .0.as_secs_f32().ceil())]
    TimedOut(Duration),
    /// The tool answered that it could not format.
    #[error("{message}")]
    Failed { kind: FailureKind, message: String },
    #[error("the formatting was stopped")]
    Cancelled,
    /// A request or answer larger than [`MAX_FRAME_BYTES`].
    #[error("the formatter's text is larger than {} MiB", MAX_FRAME_BYTES / (1024 * 1024))]
    TooLarge,
    #[error("the formatter's pipe failed: {0}")]
    Io(#[source] std::io::Error),
}

fn crashed_message(status: &Option<String>, stderr: &str) -> String {
    match (status, stderr.is_empty()) {
        (Some(status), true) => format!("the formatter stopped ({status})"),
        (Some(status), false) => format!("the formatter stopped ({status}): {stderr}"),
        (None, true) => "the formatter stopped unexpectedly".to_owned(),
        (None, false) => format!("the formatter stopped unexpectedly: {stderr}"),
    }
}

const STOP_NONE: u8 = 0;
const STOP_CANCELLED: u8 = 1;
const STOP_TIMED_OUT: u8 = 2;

#[derive(Default)]
struct Shared {
    stop: AtomicU8,
    child: Mutex<Option<Child>>,
}

impl Shared {
    fn stop_with(&self, reason: u8) {
        let _ = self
            .stop
            .compare_exchange(STOP_NONE, reason, Ordering::SeqCst, Ordering::SeqCst);
    }

    /// Kills the child (and, on Unix, its process group) while it is still in the slot. A child
    /// whose status was collected is taken out of the slot first (`wait_for_exit`), so a pid that
    /// may belong to someone else by then is never signalled.
    fn kill_child(&self) {
        if let Ok(mut child) = self.child.lock() {
            if let Some(child) = child.as_mut() {
                kill_process_tree(child);
            }
        }
    }
}

/// Kills `child` and what it started: its process group on Unix (the child must have been spawned
/// with `process_group(0)`), its process tree on Windows. For a child that is still unreaped, so its
/// pid cannot belong to anyone else.
pub fn kill_process_tree(child: &mut Child) {
    #[cfg(unix)]
    {
        // SAFETY: `kill(2)` with a negative pid signals the process group the child leads
        // (`process_group(0)` at spawn); it touches no memory of ours.
        unsafe {
            libc::kill(-(child.id() as libc::pid_t), libc::SIGKILL);
        }
    }
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        // No process groups: ask Windows to end the whole tree (a `.cmd` shim starts node, which
        // `Child::kill` alone would leave running). Best effort: `taskkill` itself may be missing.
        let _ = Command::new("taskkill")
            .args(["/T", "/F", "/PID", &child.id().to_string()])
            .creation_flags(0x0800_0000)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status();
    }
    let _ = child.kill();
}

/// What a `work` closure may do to the child: end it (before it joins a thread that waits on it).
struct Killer(Arc<Shared>);

impl Killer {
    fn kill(&self) {
        self.0.kill_child();
    }
}

/// The working directory of one run: new, empty, private to the user (0700), removed with the run.
struct PrivateDir(PathBuf);

impl PrivateDir {
    fn create() -> std::io::Result<Self> {
        use std::sync::atomic::AtomicU64;
        static NEXT: AtomicU64 = AtomicU64::new(0);
        let mut last = None;
        for _ in 0..4 {
            let unique = format!(
                "tablex-format-{}-{}-{}",
                std::process::id(),
                std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .map_or(0, |elapsed| elapsed.as_nanos()),
                NEXT.fetch_add(1, Ordering::Relaxed)
            );
            let path = std::env::temp_dir().join(unique);
            let mut builder = std::fs::DirBuilder::new();
            #[cfg(unix)]
            {
                use std::os::unix::fs::DirBuilderExt;
                builder.mode(0o700);
            }
            // `create` (not `create_dir_all`) fails on a name that exists: nothing planted is adopted.
            match builder.create(&path) {
                Ok(()) => return Ok(Self(path)),
                Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => last = Some(error),
                Err(error) => return Err(error),
            }
        }
        Err(last.unwrap_or_else(|| std::io::Error::other("no free directory name")))
    }

    fn path(&self) -> &Path {
        &self.0
    }
}

impl Drop for PrivateDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

/// Stops one format from any thread. One per run; cancelling it before the run starts makes the
/// run end at once as [`FormatHostError::Cancelled`].
#[derive(Clone, Default)]
pub struct FormatCancel(Arc<Shared>);

impl std::fmt::Debug for FormatCancel {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("FormatCancel").field("cancelled", &self.is_cancelled()).finish()
    }
}

impl FormatCancel {
    pub fn new() -> Self {
        Self::default()
    }

    /// Kills the child if it runs; idempotent, and nothing once the run has ended.
    pub fn cancel(&self) {
        self.0.stop_with(STOP_CANCELLED);
        self.0.kill_child();
    }

    pub fn is_cancelled(&self) -> bool {
        self.0.stop.load(Ordering::SeqCst) == STOP_CANCELLED
    }
}

/// The `tablex-sqruff` executable (or any tool speaking the same protocol).
#[derive(Debug, Clone)]
pub struct SqlFormatTool {
    executable: PathBuf,
}

impl SqlFormatTool {
    /// `executable` is the installed tool. Signature and manifest handling belong to the installer.
    pub fn new(executable: impl Into<PathBuf>) -> Self {
        Self {
            executable: executable.into(),
        }
    }

    pub fn executable(&self) -> &Path {
        &self.executable
    }

    /// Formats `request.sql`: hello, the request, the answer. Blocks.
    pub fn format(
        &self,
        request: FormatRequest,
        timeout: Duration,
        cancel: &FormatCancel,
    ) -> Result<String, FormatHostError> {
        let mut command = Command::new(&self.executable);
        command.env_clear();
        #[cfg(windows)]
        {
            // The system root is how Windows finds its own DLLs.
            if let Some(root) = std::env::var_os("SystemRoot") {
                command.env("SystemRoot", root);
            }
        }
        let run = supervise(command, timeout, cancel, false, move |mut stdin, stdout, _| {
            let mut stdout = BufReader::new(stdout);
            match read_frame::<FromTool>(&mut stdout).map_err(frame_error)? {
                Some(FromTool::Hello { protocol }) if protocol == PROTOCOL_VERSION => {}
                Some(FromTool::Hello { protocol }) => {
                    return Err(FormatHostError::ProtocolMismatch {
                        found: protocol,
                        expected: PROTOCOL_VERSION,
                    })
                }
                Some(_) => return Err(FormatHostError::Protocol("the first frame was not hello".into())),
                None => return Err(no_answer()),
            }
            let request = ToTool::Format(FormatRequest {
                protocol: PROTOCOL_VERSION,
                ..request
            });
            // A tool that already died: its end of the pipe is gone, and the read below says why.
            match write_frame(&mut stdin, &request) {
                Ok(()) => {}
                Err(FrameError::Io(_)) => return Err(no_answer()),
                Err(error) => return Err(frame_error(error)),
            }
            match read_frame::<FromTool>(&mut stdout).map_err(frame_error)? {
                Some(FromTool::Done { formatted }) => Ok(formatted),
                Some(FromTool::Failed(failure)) => Err(FormatHostError::Failed {
                    kind: failure.kind,
                    message: failure.message,
                }),
                Some(FromTool::Hello { .. }) => Err(FormatHostError::Protocol("a second hello".into())),
                None => Err(no_answer()),
            }
        })?;
        Ok(run.value)
    }
}

/// Runs a command that reads SQL on stdin and writes the formatted SQL on stdout. An exit status
/// that is not one of `accepted_exit_codes` (empty: only 0) is [`FormatHostError::Crashed`] with the
/// stderr tail; a command that writes its result and exits 1 to say "violations remain" (sqruff,
/// sqlfluff `fix`) is run with `[0, 1]`. `command` needs only its program, arguments and
/// environment; stdio and the working directory are set here.
pub fn run_filter(
    command: Command,
    sql: &str,
    timeout: Duration,
    cancel: &FormatCancel,
    accepted_exit_codes: &[i32],
) -> Result<String, FormatHostError> {
    let input = sql.to_owned();
    let run = supervise(command, timeout, cancel, true, move |stdin, mut stdout, killer| {
        std::thread::scope(|scope| {
            // Written on its own thread: a filter may answer before it has read everything.
            let writer = scope.spawn(move || {
                let mut stdin = stdin;
                // A filter that closes its stdin early (an error) is reported by its status.
                let _ = stdin.write_all(input.as_bytes());
            });
            let mut output = Vec::new();
            let read = (&mut stdout)
                .take(MAX_FRAME_BYTES + 1)
                .read_to_end(&mut output)
                .map_err(FormatHostError::Io);
            let too_large = output.len() as u64 > MAX_FRAME_BYTES;
            if too_large || read.is_err() {
                // Before the join: a filter that stopped reading its input leaves the writer
                // blocked on a full pipe, which only the end of the process frees.
                killer.kill();
            }
            let _ = writer.join();
            read?;
            if too_large {
                return Err(FormatHostError::TooLarge);
            }
            Ok(output)
        })
    })?;
    let accepted = |code: i32| if accepted_exit_codes.is_empty() { code == 0 } else { accepted_exit_codes.contains(&code) };
    if let Some(status) = run.status.filter(|status| !status.code().is_some_and(accepted)) {
        return Err(FormatHostError::Crashed {
            status: Some(status.to_string()),
            stderr: run.stderr,
        });
    }
    String::from_utf8(run.value)
        .map_err(|_| FormatHostError::Protocol("the output is not UTF-8 text".into()))
}

fn no_answer() -> FormatHostError {
    FormatHostError::Crashed {
        status: None,
        stderr: String::new(),
    }
}

fn frame_error(error: FrameError) -> FormatHostError {
    match error {
        FrameError::TooLarge(_) => FormatHostError::TooLarge,
        FrameError::Malformed(message) => FormatHostError::Protocol(message),
        // The child died (or was killed) mid-frame, or the pipe broke.
        FrameError::Truncated | FrameError::Io(_) => no_answer(),
    }
}

struct Supervised<T> {
    value: T,
    /// `Some` when the exit status was waited for.
    status: Option<ExitStatus>,
    stderr: String,
}

/// Spawns `command`, runs `work` on its pipes on a worker thread and waits for it, for at most
/// `timeout`. With `wait_exit` the child's exit status is collected after `work` (it should be
/// finishing: the pipes are closed).
fn supervise<T: Send + 'static>(
    mut command: Command,
    timeout: Duration,
    cancel: &FormatCancel,
    wait_exit: bool,
    work: impl FnOnce(ChildStdin, ChildStdout, Killer) -> Result<T, FormatHostError> + Send + 'static,
) -> Result<Supervised<T>, FormatHostError> {
    let path = PathBuf::from(command.get_program());
    let shared = &cancel.0;
    if cancel.is_cancelled() {
        return Err(FormatHostError::Cancelled);
    }
    // Kept until the child is gone (dropped at the end of this function).
    let work_dir = PrivateDir::create().map_err(FormatHostError::Io)?;
    command
        .current_dir(work_dir.path())
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        command.process_group(0);
    }
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        command.creation_flags(0x0800_0000); // no console window
    }
    let mut child = command.spawn().map_err(|source| match source.kind() {
        std::io::ErrorKind::NotFound => FormatHostError::NotInstalled { path: path.clone() },
        _ => FormatHostError::Spawn { path: path.clone(), source },
    })?;
    let (Some(stdin), Some(stdout), Some(stderr)) = (child.stdin.take(), child.stdout.take(), child.stderr.take())
    else {
        kill_process_tree(&mut child);
        let _ = child.wait();
        return Err(FormatHostError::Io(std::io::Error::other("the formatter's pipes are missing")));
    };
    if let Ok(mut slot) = shared.child.lock() {
        *slot = Some(child);
    }
    // A cancel that came between the check above and the child being reachable.
    if cancel.is_cancelled() {
        shared.kill_child();
    }
    let started = Instant::now();
    let stderr_tail = Arc::new(Mutex::new(Vec::new()));
    let stderr_done = spawn_stderr_reader(stderr, Arc::clone(&stderr_tail));
    let (send, receive) = mpsc::channel();
    let killer = Killer(Arc::clone(&cancel.0));
    let worker = std::thread::Builder::new()
        .name("tablex-sql-format-host".into())
        .spawn(move || {
            let _ = send.send(work(stdin, stdout, killer));
        });
    let outcome = match worker {
        Ok(_) => match receive.recv_timeout(timeout) {
            Ok(outcome) => outcome,
            Err(RecvTimeoutError::Timeout) => {
                shared.stop_with(STOP_TIMED_OUT);
                Err(FormatHostError::TimedOut(timeout))
            }
            Err(RecvTimeoutError::Disconnected) => Err(no_answer()),
        },
        Err(error) => Err(FormatHostError::Io(error)),
    };
    // The exit status of a filter that answered: it is closing, so wait out the rest of the limit.
    let mut status = None;
    if wait_exit && outcome.is_ok() {
        status = wait_for_exit(shared, timeout.saturating_sub(started.elapsed()));
        if status.is_none() {
            shared.stop_with(STOP_TIMED_OUT);
        }
    }
    shared.kill_child();
    let reaped = shared
        .child
        .lock()
        .ok()
        .and_then(|mut slot| slot.take())
        .and_then(|mut child| child.wait().ok());
    let status = status.or(reaped);
    let stderr = || {
        let _ = stderr_done.recv_timeout(STDERR_GRACE);
        stderr_tail
            .lock()
            .map(|tail| String::from_utf8_lossy(&tail).trim().to_owned())
            .unwrap_or_default()
    };
    match (shared.stop.load(Ordering::SeqCst), outcome) {
        (STOP_CANCELLED, _) => Err(FormatHostError::Cancelled),
        (STOP_TIMED_OUT, _) => Err(FormatHostError::TimedOut(timeout)),
        (_, Ok(value)) => Ok(Supervised {
            value,
            status: wait_exit.then_some(status).flatten(),
            stderr: stderr(),
        }),
        (_, Err(FormatHostError::Crashed { .. })) => Err(FormatHostError::Crashed {
            status: status.map(|status| status.to_string()),
            stderr: stderr(),
        }),
        (_, Err(other)) => Err(other),
    }
}

/// Waits for the child to exit, up to `limit`. `None`: it is still running.
fn wait_for_exit(shared: &Shared, limit: Duration) -> Option<ExitStatus> {
    let deadline = Instant::now() + limit;
    loop {
        let polled = shared
            .child
            .lock()
            .ok()
            .and_then(|mut slot| slot.as_mut().map(Child::try_wait));
        match polled {
            Some(Ok(Some(status))) => {
                // Reaped: out of the slot, so nothing signals its (reusable) pid any more.
                if let Ok(mut slot) = shared.child.lock() {
                    slot.take();
                }
                return Some(status);
            }
            Some(Ok(None)) if Instant::now() < deadline => {
                std::thread::sleep(Duration::from_millis(2));
            }
            _ => return None,
        }
    }
}

fn spawn_stderr_reader(
    mut stderr: impl Read + Send + 'static,
    tail: Arc<Mutex<Vec<u8>>>,
) -> mpsc::Receiver<()> {
    let (done, finished) = mpsc::channel();
    let _ = std::thread::Builder::new()
        .name("tablex-sql-format-stderr".into())
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
