//! Running a native tool as a child process off the UI thread: its output lines go to a capped log, the
//! size of its output file is the progress, and a cancel stops the tool and everything it started.
//!
//! * The log keeps the last [`LOG_CAP`] lines (older ones are counted, not kept) and cuts a line at
//!   [`LINE_CAP`] bytes, so a tool that prints a lot costs a bounded amount of memory. The window reads
//!   only the lines it shows ([`RunState::range`]).
//! * The window is told at most every [`TICK`] and only when something changed.
//! * Cancel asks the tool's process group to end (`SIGTERM`; `taskkill /T` on Windows) and, if it has not
//!   after [`KILL_GRACE`], kills it. The tool starts as the leader of its own group, so a parallel `pg_dump`'s
//!   workers go with it and never outlive the run.

use std::collections::VecDeque;
use std::io::BufRead;
use std::ops::Range;
use std::path::Path;
use std::process::{Command, ExitStatus, Stdio};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{mpsc, Arc, Mutex, OnceLock};
use std::time::{Duration, Instant};

use super::command::Invocation;

/// The most lines the log keeps.
pub const LOG_CAP: usize = 2_000;
/// The most bytes of a line the log keeps.
pub const LINE_CAP: usize = 4_096;
/// How many of the last error lines are kept for the summary.
const STDERR_TAIL: usize = 12;
/// The window hears of progress at most this often.
pub const TICK: Duration = Duration::from_millis(120);
/// How long a tool may take to end after it was asked to.
const KILL_GRACE: Duration = Duration::from_secs(3);
/// How long the output of a tool that ended may still arrive (a straggler holding the pipe open).
const DRAIN_GRACE: Duration = Duration::from_secs(2);
/// Secrets shorter than this are not scrubbed from the log: they would mangle ordinary words.
const MIN_SCRUBBED: usize = 4;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Stream {
    /// A line of ours: the command that was started.
    Command,
    Out,
    Err,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LogLine {
    pub stream: Stream,
    pub text: String,
}

#[derive(Debug, Default)]
struct LogBuffer {
    lines: VecDeque<LogLine>,
    /// Lines pushed out by newer ones.
    dropped: u64,
    last: Option<String>,
    stderr: VecDeque<String>,
}

/// What the window reads of a run.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Snapshot {
    /// Bytes the tool has written to its output so far.
    pub bytes: u64,
    /// Lines the tool printed, kept or not.
    pub lines_seen: u64,
    /// Lines the log holds.
    pub log_len: usize,
    pub dropped: u64,
    /// The last line printed: what the tool is doing.
    pub last: Option<String>,
}

/// The shared state of one run: the log and the counters.
pub struct RunState {
    log: Mutex<LogBuffer>,
    bytes: AtomicU64,
    seen: AtomicU64,
    /// Texts that must never reach the log; known once the connection's password is read.
    secrets: OnceLock<Vec<String>>,
}

impl std::fmt::Debug for RunState {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        // Not the scrubbed texts: they are secrets.
        f.debug_struct("RunState").field("snapshot", &self.snapshot()).finish_non_exhaustive()
    }
}

impl RunState {
    pub fn new() -> Arc<Self> {
        Arc::new(Self { log: Mutex::new(LogBuffer::default()), bytes: AtomicU64::new(0), seen: AtomicU64::new(0), secrets: OnceLock::new() })
    }

    /// Texts to replace by `***` in everything the tool prints (the password). Set once.
    pub fn scrub_secrets(&self, secrets: Vec<String>) {
        let _ = self.secrets.set(secrets.into_iter().filter(|secret| secret.len() >= MIN_SCRUBBED).collect());
    }

    fn scrub(&self, text: &str) -> String {
        let mut clean = crate::sessions::redact_secrets(text);
        for secret in self.secrets.get().into_iter().flatten() {
            if clean.contains(secret.as_str()) {
                clean = clean.replace(secret.as_str(), "***");
            }
        }
        clean
    }

    /// Adds a line. It is scrubbed, cut at [`LINE_CAP`] and pushes the oldest line out when the log is full.
    pub fn push(&self, stream: Stream, text: &str) {
        let text = self.scrub(text);
        self.seen.fetch_add(1, Ordering::Relaxed);
        let mut log = self.log.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
        if stream == Stream::Err {
            if log.stderr.len() == STDERR_TAIL {
                log.stderr.pop_front();
            }
            log.stderr.push_back(text.clone());
        }
        if log.lines.len() == LOG_CAP {
            log.lines.pop_front();
            log.dropped += 1;
        }
        log.last = Some(text.clone());
        log.lines.push_back(LogLine { stream, text });
    }

    /// The lines at `range` of the log as it is now (the range is cut to what exists).
    pub fn range(&self, range: Range<usize>) -> Vec<LogLine> {
        let log = self.log.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
        log.lines.iter().skip(range.start).take(range.len()).cloned().collect()
    }

    pub fn log_len(&self) -> usize {
        self.log.lock().unwrap_or_else(|poisoned| poisoned.into_inner()).lines.len()
    }

    /// The last error lines the tool printed.
    pub fn stderr_tail(&self) -> Vec<String> {
        self.log.lock().unwrap_or_else(|poisoned| poisoned.into_inner()).stderr.iter().cloned().collect()
    }

    pub fn set_bytes(&self, bytes: u64) {
        self.bytes.store(bytes, Ordering::Relaxed);
    }

    pub fn snapshot(&self) -> Snapshot {
        let log = self.log.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
        Snapshot { bytes: self.bytes.load(Ordering::Relaxed), lines_seen: self.seen.load(Ordering::Relaxed), log_len: log.lines.len(), dropped: log.dropped, last: log.last.clone() }
    }
}

/// Stops a run from any thread, before or after the tool started.
#[derive(Debug, Default)]
pub struct CancelToken {
    cancelled: std::sync::atomic::AtomicBool,
    /// The tool's process id while it runs. The lock is held while it is signalled and while it is cleared, and it is
    /// cleared after the tool has ended but before it is reaped (see [`wait_for_exit`]): a process id is not reused
    /// until its process is reaped, so a number that is here is never another process's.
    pid: Mutex<Option<u32>>,
}

impl CancelToken {
    pub fn cancel(&self) {
        self.cancelled.store(true, Ordering::SeqCst);
        if let Some(pid) = *self.pid.lock().unwrap_or_else(|poisoned| poisoned.into_inner()) {
            terminate(pid, false);
        }
    }

    pub fn is_cancelled(&self) -> bool {
        self.cancelled.load(Ordering::SeqCst)
    }

    fn attach(&self, pid: u32) {
        *self.pid.lock().unwrap_or_else(|poisoned| poisoned.into_inner()) = Some(pid);
        // Cancelled between the check before the start and now.
        if self.is_cancelled() {
            terminate(pid, false);
        }
    }

    fn detach(&self) {
        *self.pid.lock().unwrap_or_else(|poisoned| poisoned.into_inner()) = None;
    }

    fn force(&self) {
        if let Some(pid) = *self.pid.lock().unwrap_or_else(|poisoned| poisoned.into_inner()) {
            terminate(pid, true);
        }
    }
}

/// Asks the process group led by `pid` to end; `force` kills it.
#[cfg(unix)]
fn terminate(pid: u32, force: bool) {
    let signal = if force { libc::SIGKILL } else { libc::SIGTERM };
    let Ok(pid) = i32::try_from(pid) else { return };
    // SAFETY: `kill` takes two integers and touches no memory of ours. The tool was started as the leader of its own
    // process group (`process_group(0)`), so `-pid` is that group. Only the group is signalled, never `pid` alone: when
    // the group is gone the call just fails, and a number is never aimed at a process that is not the tool's.
    unsafe {
        libc::kill(-pid, signal);
    }
}

/// Waits until the process `pid` (a child of ours) has ended, without reaping it: its number stays its own until
/// [`std::process::Child::wait`] reaps it. `false` when that cannot be done (the wait failed: not our child, no such process).
#[cfg(unix)]
fn wait_for_exit(pid: u32) -> bool {
    // SAFETY: `waitid` fills the `siginfo_t` we give it and reads nothing else of ours. WNOWAIT leaves the zombie to `wait`.
    unsafe {
        let mut info: libc::siginfo_t = std::mem::zeroed();
        loop {
            if libc::waitid(libc::P_PID, pid as libc::id_t, &mut info, libc::WEXITED | libc::WNOWAIT) == 0 {
                return true;
            }
            if std::io::Error::last_os_error().kind() != std::io::ErrorKind::Interrupted {
                return false;
            }
        }
    }
}

/// Windows keeps a process's number until the last handle to it (the `Child`'s) is closed, and has no way to wait without
/// reaping: [`end_of`] waits, then lets go of the token, with the `Child` still open.
#[cfg(not(unix))]
fn wait_for_exit(_pid: u32) -> bool {
    false
}

/// Waits for `child` to end and lets go of the cancel token, in the order that keeps a signal from ever reaching another
/// process: when the end can be seen without reaping the process (Unix) the token goes first, while its number is still held by
/// the zombie; when it cannot (Windows, or a wait that failed) the token stays attached until the wait returned, and goes
/// before the `Child` (the open handle) does. `Err` is the wait's own.
fn end_of(child: &mut std::process::Child, cancel: &CancelToken) -> std::io::Result<ExitStatus> {
    end_of_with(child, cancel, wait_for_exit)
}

fn end_of_with(child: &mut std::process::Child, cancel: &CancelToken, peek: impl FnOnce(u32) -> bool) -> std::io::Result<ExitStatus> {
    if peek(child.id()) {
        cancel.detach();
        child.wait()
    } else {
        let status = child.wait();
        cancel.detach();
        status
    }
}

#[cfg(windows)]
fn terminate(pid: u32, _force: bool) {
    use std::os::windows::process::CommandExt as _;
    const CREATE_NO_WINDOW: u32 = 0x0800_0000;
    // The system's own program, by its absolute path: never one found first on the PATH or in the working folder.
    let root = std::env::var_os("SystemRoot").map(std::path::PathBuf::from).filter(|root| root.is_absolute()).unwrap_or_else(|| std::path::PathBuf::from(r"C:\Windows"));
    // `/T`: the process and the ones it started.
    let _ = Command::new(root.join("System32").join("taskkill.exe")).args(["/PID", &pid.to_string(), "/T", "/F"]).creation_flags(CREATE_NO_WINDOW).stdout(Stdio::null()).stderr(Stdio::null()).status();
}

#[cfg(not(any(unix, windows)))]
fn terminate(_pid: u32, _force: bool) {}

/// What to run and where its progress shows.
pub struct Execution<'a> {
    pub invocation: &'a Invocation,
    /// The file (or directory) the tool writes: its size is the progress.
    pub watch: Option<&'a Path>,
}

/// How the tool ended.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Ending {
    /// It ended by itself. `code` is `None` when a signal ended it.
    Exited { code: Option<i32>, success: bool },
    /// It was asked to stop.
    Cancelled,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Finished {
    pub ending: Ending,
    pub elapsed: Duration,
}

enum Event {
    Exit(std::io::Result<ExitStatus>),
    Eof,
}

/// The bytes of the file at `path`, or of the files directly in it.
pub fn size_of(path: &Path) -> u64 {
    match std::fs::metadata(path) {
        Ok(meta) if meta.is_dir() => std::fs::read_dir(path).into_iter().flatten().flatten().filter_map(|entry| entry.metadata().ok()).filter(|meta| meta.is_file()).map(|meta| meta.len()).sum(),
        Ok(meta) => meta.len(),
        Err(_) => 0,
    }
}

/// Reads one line of at most `cap` bytes into `line` (the rest of a longer one is dropped). `false` at the end of the stream.
fn read_line_capped(reader: &mut impl BufRead, line: &mut Vec<u8>, cap: usize) -> std::io::Result<bool> {
    line.clear();
    let mut any = false;
    loop {
        let available = reader.fill_buf()?;
        if available.is_empty() {
            return Ok(any);
        }
        any = true;
        let (used, complete) = match available.iter().position(|byte| *byte == b'\n') {
            Some(end) => (end + 1, true),
            None => (available.len(), false),
        };
        let room = cap.saturating_sub(line.len());
        line.extend_from_slice(&available[..used.min(room)]);
        reader.consume(used);
        if complete {
            return Ok(true);
        }
    }
}

fn pump(stream: impl std::io::Read + Send + 'static, kind: Stream, state: Arc<RunState>, events: mpsc::Sender<Event>) -> std::thread::JoinHandle<()> {
    std::thread::spawn(move || {
        let mut reader = std::io::BufReader::new(stream);
        let mut line = Vec::new();
        while let Ok(true) = read_line_capped(&mut reader, &mut line, LINE_CAP) {
            let text = String::from_utf8_lossy(&line);
            let truncated = line.len() >= LINE_CAP && !line.ends_with(b"\n");
            let mut text = text.trim_end_matches(['\r', '\n']).to_owned();
            if truncated {
                text.push('\u{2026}');
            }
            state.push(kind, &text);
        }
        let _ = events.send(Event::Eof);
    })
}

/// The command of `invocation`: its environment, no input, its own process group (so that a cancel reaches what it
/// starts), and no console window on Windows.
fn command_of(invocation: &Invocation) -> Command {
    let mut command = Command::new(&invocation.program);
    command.args(&invocation.args).stdin(Stdio::null());
    for name in invocation.env_remove {
        command.env_remove(name);
    }
    for (name, value) in &invocation.env {
        command.env(name, value);
    }
    #[cfg(unix)]
    std::os::unix::process::CommandExt::process_group(&mut command, 0);
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt as _;
        command.creation_flags(0x0800_0000);
    }
    command
}

/// Starts the tool, streams its output into `state` and waits for it. Blocking: call it from a worker thread.
/// `notify` is called when the counters changed, at most every [`TICK`]. `Err` only when the program cannot be started.
pub fn run(exec: &Execution<'_>, state: &Arc<RunState>, cancel: &Arc<CancelToken>, notify: &dyn Fn()) -> std::io::Result<Finished> {
    let started = Instant::now();
    if cancel.is_cancelled() {
        return Ok(Finished { ending: Ending::Cancelled, elapsed: started.elapsed() });
    }
    let mut child = command_of(exec.invocation).stdout(Stdio::piped()).stderr(Stdio::piped()).spawn()?;
    cancel.attach(child.id());
    let (events, inbox) = mpsc::channel();
    let readers = [
        child.stdout.take().map(|out| pump(out, Stream::Out, state.clone(), events.clone())),
        child.stderr.take().map(|err| pump(err, Stream::Err, state.clone(), events.clone())),
    ];
    let expected_eofs = readers.iter().flatten().count();
    let waiter = {
        let (events, cancel) = (events.clone(), cancel.clone());
        std::thread::spawn(move || {
            let status = end_of(&mut child, &cancel);
            let _ = events.send(Event::Exit(status));
        })
    };
    drop(events);

    let (mut exit, mut eofs, mut drain_until) = (None, 0, None);
    let (mut last_counts, mut cancelled_at) = ((u64::MAX, u64::MAX), None::<Instant>);
    loop {
        match inbox.recv_timeout(TICK) {
            Ok(Event::Exit(status)) => {
                exit = Some(status);
                drain_until = Some(Instant::now() + DRAIN_GRACE);
            }
            Ok(Event::Eof) => eofs += 1,
            Err(mpsc::RecvTimeoutError::Timeout) => {}
            Err(mpsc::RecvTimeoutError::Disconnected) => break,
        }
        if let Some(path) = exec.watch {
            state.set_bytes(size_of(path));
        }
        let snapshot = state.snapshot();
        if (snapshot.bytes, snapshot.lines_seen) != last_counts {
            last_counts = (snapshot.bytes, snapshot.lines_seen);
            notify();
        }
        if cancel.is_cancelled() && exit.is_none() && cancelled_at.get_or_insert_with(Instant::now).elapsed() > KILL_GRACE {
            cancel.force();
        }
        if exit.is_some() && (eofs >= expected_eofs || drain_until.is_some_and(|until| Instant::now() >= until)) {
            break;
        }
    }
    let _ = waiter.join();
    if eofs >= expected_eofs {
        readers.into_iter().flatten().for_each(|reader| {
            let _ = reader.join();
        });
    }
    if let Some(path) = exec.watch {
        state.set_bytes(size_of(path));
    }
    notify();
    let status = exit.ok_or_else(|| std::io::Error::other("the tool's process was lost"))??;
    // A cancel always wins: a tool that ignored the request and ended well still ended because it was asked to (or too late to matter),
    // and nothing it made is kept.
    let ending = if cancel.is_cancelled() {
        Ending::Cancelled
    } else {
        Ending::Exited { code: status.code(), success: status.success() }
    };
    Ok(Finished { ending, elapsed: started.elapsed() })
}

/// The most a captured answer may hold: the table of contents of a very large database is some hundred megabytes at most.
pub const CAPTURE_CAP: u64 = 256 * 1024 * 1024;
/// How long a captured tool may take.
pub const CAPTURE_TIMEOUT: Duration = Duration::from_secs(120);

/// What a tool run for its answer printed.
#[derive(Debug)]
pub struct Captured {
    pub status: ExitStatus,
    pub stdout: Vec<u8>,
    pub stderr: Vec<u8>,
}

/// Why a tool run for its answer gave none.
#[derive(Debug)]
pub enum CaptureError {
    Start(std::io::Error),
    Cancelled,
    TimedOut(Duration),
    /// The answer is longer than [`CAPTURE_CAP`].
    TooLarge,
}

impl std::fmt::Display for CaptureError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Start(error) => write!(f, "The tool could not be started: {error}"),
            Self::Cancelled => f.write_str("Cancelled."),
            Self::TimedOut(after) => write!(f, "The tool did not answer within {} seconds.", after.as_secs()),
            Self::TooLarge => f.write_str("The tool's answer is too large to read."),
        }
    }
}

impl std::error::Error for CaptureError {}

/// Runs a tool that prints an answer (the table of contents of an archive) and returns its output. Blocking: it ends when the tool
/// does, when `cancel` is cancelled (the tool and what it started are stopped) or after `timeout`.
pub fn capture(invocation: &Invocation, cancel: &Arc<CancelToken>, timeout: Duration) -> Result<Captured, CaptureError> {
    if cancel.is_cancelled() {
        return Err(CaptureError::Cancelled);
    }
    let mut child = command_of(invocation).stdout(Stdio::piped()).stderr(Stdio::piped()).spawn().map_err(CaptureError::Start)?;
    cancel.attach(child.id());
    let read = |stream: Option<Box<dyn std::io::Read + Send>>| {
        let (sender, receiver) = mpsc::channel();
        std::thread::spawn(move || {
            let mut bytes = Vec::new();
            if let Some(stream) = stream {
                let _ = std::io::Read::read_to_end(&mut std::io::Read::take(stream, CAPTURE_CAP + 1), &mut bytes);
            }
            let _ = sender.send(bytes);
        });
        receiver
    };
    let stdout = read(child.stdout.take().map(|out| Box::new(out) as Box<dyn std::io::Read + Send>));
    let stderr = read(child.stderr.take().map(|err| Box::new(err) as Box<dyn std::io::Read + Send>));
    let (exit, status) = mpsc::channel();
    let waiter = {
        let cancel = cancel.clone();
        std::thread::spawn(move || {
            let status = end_of(&mut child, &cancel);
            let _ = exit.send(status);
        })
    };
    let status = match status.recv_timeout(timeout) {
        Ok(status) => status,
        Err(_) => {
            // Too long: stopped like a cancel, and waited for so that nothing is left running.
            cancel.cancel();
            cancel.force();
            let _ = status.recv_timeout(KILL_GRACE);
            return Err(CaptureError::TimedOut(timeout));
        }
    };
    let _ = waiter.join();
    let status = status.map_err(CaptureError::Start)?;
    // The output of a tool that ended is complete once its pipes close; a straggler holding one is not waited for.
    let take = |receiver: mpsc::Receiver<Vec<u8>>| receiver.recv_timeout(DRAIN_GRACE).unwrap_or_default();
    let (stdout, stderr) = (take(stdout), take(stderr));
    if cancel.is_cancelled() {
        return Err(CaptureError::Cancelled);
    }
    if stdout.len() as u64 > CAPTURE_CAP {
        return Err(CaptureError::TooLarge);
    }
    Ok(Captured { status, stdout, stderr })
}

#[cfg(all(test, unix))]
mod tests {
    use std::os::unix::fs::PermissionsExt as _;

    use super::*;

    /// A script `name` in `dir` with `body`.
    fn script(dir: &Path, name: &str, body: &str) -> std::path::PathBuf {
        let path = dir.join(name);
        std::fs::write(&path, format!("#!/bin/sh\n{body}\n")).unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
        path
    }

    fn invocation(program: std::path::PathBuf, args: &[&str]) -> Invocation {
        Invocation { program, args: args.iter().map(Into::into).collect(), env: Vec::new(), env_remove: &[] }
    }

    #[test]
    fn the_log_keeps_the_last_lines_counts_the_rest_and_cuts_long_lines() {
        let dir = tempfile::tempdir().unwrap();
        let tool = script(dir.path(), "noisy", "echo oops >&2; sleep 0.3; i=0; while [ $i -lt 5000 ]; do echo \"line $i\"; i=$((i+1)); done; head -c 20000 /dev/zero | tr '\\0' x; echo; exit 0");
        let state = RunState::new();
        let finished = run(&Execution { invocation: &invocation(tool, &[]), watch: None }, &state, &Arc::new(CancelToken::default()), &|| {}).unwrap();
        assert_eq!(finished.ending, Ending::Exited { code: Some(0), success: true });
        let snapshot = state.snapshot();
        assert_eq!(snapshot.lines_seen, 5002);
        assert_eq!(snapshot.log_len, LOG_CAP, "at most {LOG_CAP} lines are kept");
        assert_eq!(snapshot.dropped, 5002 - LOG_CAP as u64);
        let kept = state.range(0..LOG_CAP);
        assert_eq!(kept.len(), LOG_CAP);
        let last = kept.last().unwrap();
        assert!(last.text.chars().count() <= LINE_CAP + 1 && last.text.ends_with('\u{2026}'), "the long line is cut: {}", last.text.len());
        assert_eq!(kept[LOG_CAP - 2].text, "line 4999", "the newest lines are the ones kept");
        assert_eq!(state.range(LOG_CAP - 2..LOG_CAP + 50).len(), 2, "a range past the end is cut to what exists");
        assert_eq!(state.stderr_tail(), ["oops"]);
    }

    #[test]
    fn an_error_exit_is_reported_with_its_code_and_the_last_error_lines() {
        let dir = tempfile::tempdir().unwrap();
        let tool = script(dir.path(), "failing", "echo 'pg_dump: error: connection refused' >&2; exit 3");
        let state = RunState::new();
        let finished = run(&Execution { invocation: &invocation(tool, &[]), watch: None }, &state, &Arc::new(CancelToken::default()), &|| {}).unwrap();
        assert_eq!(finished.ending, Ending::Exited { code: Some(3), success: false });
        assert_eq!(state.stderr_tail(), ["pg_dump: error: connection refused"]);
        let missing = run(&Execution { invocation: &invocation(dir.path().join("absent"), &[]), watch: None }, &state, &Arc::new(CancelToken::default()), &|| {});
        assert!(missing.is_err(), "a program that cannot be started is an error of the start");
    }

    #[test]
    fn cancel_ends_the_tool_and_the_processes_it_started() {
        let dir = tempfile::tempdir().unwrap();
        // Prints its helper's pid, then waits for it: as a parallel pg_dump waits for its workers.
        let tool = script(dir.path(), "sleeper", "sleep 60 &\necho \"helper $!\"\nwait");
        let state = RunState::new();
        let cancel = Arc::new(CancelToken::default());
        let worker = {
            let (state, cancel) = (state.clone(), cancel.clone());
            std::thread::spawn(move || run(&Execution { invocation: &invocation(tool, &[]), watch: None }, &state, &cancel, &|| {}))
        };
        let deadline = Instant::now() + Duration::from_secs(10);
        let helper = loop {
            if let Some(pid) = state.range(0..10).iter().find_map(|line| line.text.strip_prefix("helper ").and_then(|pid| pid.parse::<i32>().ok())) {
                break pid;
            }
            assert!(Instant::now() < deadline, "the tool printed its helper");
            std::thread::sleep(Duration::from_millis(10));
        };
        // SAFETY: signal 0 only checks that the process exists.
        let alive = |pid: i32| unsafe { libc::kill(pid, 0) == 0 };
        assert!(alive(helper));
        let asked = Instant::now();
        cancel.cancel();
        let finished = worker.join().unwrap().unwrap();
        assert_eq!(finished.ending, Ending::Cancelled);
        assert!(asked.elapsed() < Duration::from_secs(5), "the tool ended when asked, not after its sleep");
        let gone = (0..200).any(|_| {
            std::thread::sleep(Duration::from_millis(10));
            !alive(helper)
        });
        assert!(gone, "the process the tool started is gone with it");
    }

    #[test]
    fn a_cancel_before_the_start_never_starts_the_tool() {
        let dir = tempfile::tempdir().unwrap();
        let marker = dir.path().join("ran");
        let tool = script(dir.path(), "marker", &format!("touch {}", marker.display()));
        let cancel = Arc::new(CancelToken::default());
        cancel.cancel();
        let finished = run(&Execution { invocation: &invocation(tool, &[]), watch: None }, &RunState::new(), &cancel, &|| {}).unwrap();
        assert_eq!(finished.ending, Ending::Cancelled);
        assert!(!marker.exists());
    }

    #[test]
    fn a_tool_that_ignores_the_request_is_killed_after_the_grace_period() {
        let dir = tempfile::tempdir().unwrap();
        let tool = script(dir.path(), "stubborn", "trap '' TERM\necho ready\nwhile true; do sleep 0.05; done");
        let state = RunState::new();
        let cancel = Arc::new(CancelToken::default());
        let worker = {
            let (state, cancel) = (state.clone(), cancel.clone());
            std::thread::spawn(move || run(&Execution { invocation: &invocation(tool, &[]), watch: None }, &state, &cancel, &|| {}))
        };
        while state.log_len() == 0 {
            std::thread::sleep(Duration::from_millis(10));
        }
        cancel.cancel();
        let finished = worker.join().unwrap().unwrap();
        assert_eq!(finished.ending, Ending::Cancelled);
        assert!(finished.elapsed >= KILL_GRACE, "{:?}", finished.elapsed);
    }

    #[test]
    fn the_size_of_the_output_is_the_progress_and_the_window_is_told_when_it_changes() {
        let dir = tempfile::tempdir().unwrap();
        let output = dir.path().join("out.sql");
        let tool = script(dir.path(), "writer", &format!("for i in 1 2 3; do head -c 1000 /dev/zero >> {out}; sleep 0.2; done", out = output.display()));
        let state = RunState::new();
        let notes = std::sync::atomic::AtomicU32::new(0);
        let finished = run(&Execution { invocation: &invocation(tool, &[]), watch: Some(&output) }, &state, &Arc::new(CancelToken::default()), &|| {
            notes.fetch_add(1, Ordering::Relaxed);
        })
        .unwrap();
        assert!(matches!(finished.ending, Ending::Exited { success: true, .. }));
        assert_eq!(state.snapshot().bytes, 3000);
        let notes = notes.load(Ordering::Relaxed);
        assert!((2..=12).contains(&notes), "throttled, not once per write or per line: {notes}");
        // A directory counts the files in it.
        std::fs::write(dir.path().join("a.dat"), [0u8; 10]).unwrap();
        assert!(size_of(dir.path()) >= 3010);
    }

    #[test]
    fn the_environment_is_set_and_cleaned_and_secrets_are_scrubbed_from_the_log() {
        let dir = tempfile::tempdir().unwrap();
        std::env::set_var("TABLEX_R6_REMOVE_ME", "inherited");
        let tool = script(dir.path(), "env", "echo \"remove=${TABLEX_R6_REMOVE_ME:-unset} set=$TABLEX_R6_SET\"\necho \"password hunter2-pass and postgres://u:hunter2-pass@h/db\" >&2");
        let mut call = invocation(tool, &[]);
        call.env = vec![("TABLEX_R6_SET", "yes".into())];
        call.env_remove = &["TABLEX_R6_REMOVE_ME"];
        let state = RunState::new();
        state.scrub_secrets(vec!["hunter2-pass".to_owned(), "ab".to_owned()]);
        run(&Execution { invocation: &call, watch: None }, &state, &Arc::new(CancelToken::default()), &|| {}).unwrap();
        let lines: Vec<String> = state.range(0..10).into_iter().map(|line| line.text).collect();
        // The two streams are read by two threads: no order between their lines.
        assert!(lines.iter().any(|line| line == "remove=unset set=yes"), "{lines:?}");
        assert!(lines.iter().any(|line| line.contains("password *** and")) && !lines.join(" ").contains("hunter2"), "{lines:?}");
    }

    #[test]
    fn the_end_of_a_tool_is_waited_for_without_reaping_it_so_its_number_is_still_its_own() {
        let mut child = Command::new("sh").args(["-c", "exit 3"]).spawn().unwrap();
        let pid = child.id();
        assert!(wait_for_exit(pid));
        // SAFETY: signal 0 only asks whether the process exists: an ended child that was not reaped still does.
        assert_eq!(unsafe { libc::kill(pid as i32, 0) }, 0, "the number is still held by the zombie: it cannot be another process's");
        assert_eq!(child.wait().unwrap().code(), Some(3));
    }

    #[test]
    fn a_captured_tool_gives_its_output_and_stops_on_a_cancel_or_a_timeout() {
        let dir = tempfile::tempdir().unwrap();
        let answer = script(dir.path(), "answer", "echo one; echo two >&2; exit 0");
        let captured = capture(&invocation(answer, &[]), &Arc::new(CancelToken::default()), Duration::from_secs(10)).unwrap();
        assert!(captured.status.success());
        assert_eq!((String::from_utf8_lossy(&captured.stdout).trim(), String::from_utf8_lossy(&captured.stderr).trim()), ("one", "two"));

        // Too slow: stopped, with what it started.
        let slow = script(dir.path(), "slow", "sleep 60 &\nwait");
        let started = Instant::now();
        let error = capture(&invocation(slow.clone(), &[]), &Arc::new(CancelToken::default()), Duration::from_millis(300)).unwrap_err();
        assert!(matches!(error, CaptureError::TimedOut(_)), "{error}");
        assert!(started.elapsed() < Duration::from_secs(10));

        // Cancelled from another thread.
        let cancel = Arc::new(CancelToken::default());
        let canceller = {
            let cancel = cancel.clone();
            std::thread::spawn(move || {
                std::thread::sleep(Duration::from_millis(300));
                cancel.cancel();
            })
        };
        let started = Instant::now();
        let error = capture(&invocation(slow, &[]), &cancel, Duration::from_secs(60)).unwrap_err();
        canceller.join().unwrap();
        assert!(matches!(error, CaptureError::Cancelled), "{error}");
        assert!(started.elapsed() < Duration::from_secs(10), "the cancel ended it, not the timeout");
        // Never started when it was cancelled before.
        assert!(matches!(capture(&invocation(dir.path().join("answer"), &[]), &cancel, Duration::from_secs(1)), Err(CaptureError::Cancelled)));
    }

    #[test]
    fn a_wait_that_cannot_see_the_end_without_reaping_keeps_the_token_attached_until_the_process_was_reaped() {
        let cancel = Arc::new(CancelToken::default());
        let mut child = Command::new("sleep").arg("0.4").spawn().unwrap();
        cancel.attach(child.id());
        let waiting = {
            let cancel = cancel.clone();
            std::thread::spawn(move || end_of_with(&mut child, &cancel, |_| false))
        };
        std::thread::sleep(Duration::from_millis(150));
        assert!(cancel.pid.lock().unwrap().is_some(), "still attached while the process runs");
        assert!(waiting.join().unwrap().unwrap().success());
        assert!(cancel.pid.lock().unwrap().is_none(), "let go once it was waited for");
        // Not our child: the end cannot be seen without reaping, and the answer says so.
        assert!(!wait_for_exit(1));
    }

    #[test]
    fn a_cancel_wins_over_a_tool_that_ignores_it_and_ends_well() {
        let dir = tempfile::tempdir().unwrap();
        // The request ends its helper, not the script, which goes on and exits 0.
        let tool = script(dir.path(), "stubborn-ok", "trap '' TERM\necho started\nsleep 5\nexit 0");
        let state = RunState::new();
        let cancel = Arc::new(CancelToken::default());
        let worker = {
            let (state, cancel) = (state.clone(), cancel.clone());
            std::thread::spawn(move || run(&Execution { invocation: &invocation(tool, &[]), watch: None }, &state, &cancel, &|| {}))
        };
        while state.log_len() == 0 {
            std::thread::sleep(Duration::from_millis(10));
        }
        cancel.cancel();
        let finished = worker.join().unwrap().unwrap();
        assert_eq!(finished.ending, Ending::Cancelled, "exit 0 after a cancel is still a cancel");
    }
}
