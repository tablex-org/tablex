//! The SQL formatter host runner against fake tools: shell scripts that speak (or break) the
//! framed protocol, and ordinary commands as filters. The real `tablex-sqruff` has its own test.
#![cfg(unix)]

use engine_driver_api::mongo_shell::write_frame;
use engine_driver_api::sql_format::{
    Dialect, Failure, FailureKind, FormatRequest, FromTool, KeywordCase, MAX_FRAME_BYTES,
    PROTOCOL_VERSION,
};
use engine_driver_host::{run_filter, FormatCancel, FormatHostError, SqlFormatTool};
use serde::Serialize;
use std::os::unix::fs::PermissionsExt;
use std::path::PathBuf;
use std::process::Command;
use std::time::{Duration, Instant};

const LIMIT: Duration = Duration::from_secs(10);

fn request() -> FormatRequest {
    FormatRequest {
        protocol: PROTOCOL_VERSION,
        sql: "select 1".into(),
        dialect: Dialect::Postgres,
        keyword_case: KeywordCase::Upper,
        indent: 2,
    }
}

/// `printf` of the exact bytes of one frame, as octal escapes (a POSIX `printf` reads them).
fn printf_frame(frame: &impl Serialize) -> String {
    let mut bytes = Vec::new();
    write_frame(&mut bytes, frame).unwrap();
    printf_bytes(&bytes)
}

fn printf_bytes(bytes: &[u8]) -> String {
    let escapes: String = bytes.iter().map(|byte| format!("\\{byte:03o}")).collect();
    format!("printf '{escapes}'")
}

fn hello() -> String {
    printf_frame(&FromTool::Hello { protocol: PROTOCOL_VERSION })
}

fn done(text: &str) -> String {
    printf_frame(&FromTool::Done { formatted: text.into() })
}

/// A directory kept for the test; scripts are executable files in it.
struct Scripts(PathBuf);

impl Scripts {
    fn new(name: &str) -> Self {
        let dir = std::env::temp_dir().join(format!("tablex-sqlfmt-{name}-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        Self(dir)
    }

    fn tool(&self, body: &str) -> SqlFormatTool {
        let path = self.0.join("tool.sh");
        std::fs::write(&path, format!("#!/bin/sh\n{body}\n")).unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
        SqlFormatTool::new(path)
    }
}

impl Drop for Scripts {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

fn run(tool: &SqlFormatTool) -> Result<String, FormatHostError> {
    tool.format(request(), LIMIT, &FormatCancel::new())
}

/// hello, wait for the request's first byte, then `after`.
fn tool_script(after: &str) -> String {
    format!("{}\nhead -c 1 >/dev/null\n{after}", hello())
}

#[test]
fn a_tool_that_answers_formats_in_an_empty_environment() {
    std::env::set_var("TABLEX_FAKE_FORMATTER_SECRET", "leak");
    let dir = Scripts::new("ok");
    // Exits 3 (no answer) if the parent's environment reached it.
    let tool = dir.tool(&tool_script(&format!("[ -z \"$TABLEX_FAKE_FORMATTER_SECRET\" ] || exit 3\n{}", done("SELECT 1"))));
    assert_eq!(run(&tool).unwrap(), "SELECT 1");
}

#[test]
fn another_protocol_version_ends_the_run() {
    let dir = Scripts::new("version");
    let tool = dir.tool(&format!("{}\nsleep 5", printf_frame(&FromTool::Hello { protocol: 99 })));
    let started = Instant::now();
    match run(&tool) {
        Err(FormatHostError::ProtocolMismatch { found: 99, expected }) => assert_eq!(expected, PROTOCOL_VERSION),
        other => panic!("{other:?}"),
    }
    assert!(started.elapsed() < Duration::from_secs(4), "the child was killed, not waited out");
}

#[test]
fn a_tool_that_dies_reports_its_status_and_stderr() {
    let dir = Scripts::new("crash");
    let tool = dir.tool(&tool_script("echo 'kaboom: out of memory' >&2\nexit 3"));
    match run(&tool) {
        Err(FormatHostError::Crashed { status, stderr }) => {
            assert!(status.is_some_and(|status| status.contains('3')), "exit status");
            assert_eq!(stderr, "kaboom: out of memory");
        }
        other => panic!("{other:?}"),
    }
    // Before any hello.
    let tool = dir.tool("echo 'no hello' >&2\nexit 1");
    assert!(matches!(run(&tool), Err(FormatHostError::Crashed { stderr, .. }) if stderr == "no hello"));
}

#[test]
fn a_typed_failure_of_the_tool_is_passed_on() {
    let dir = Scripts::new("failed");
    let failure = FromTool::Failed(Failure { kind: FailureKind::Parse, message: "line 3: unexpected ')'".into() });
    let tool = dir.tool(&tool_script(&printf_frame(&failure)));
    match run(&tool) {
        Err(FormatHostError::Failed { kind: FailureKind::Parse, message }) => assert_eq!(message, "line 3: unexpected ')'"),
        other => panic!("{other:?}"),
    }
}

#[test]
fn garbage_and_oversized_frames_are_refused() {
    let dir = Scripts::new("garbage");
    let junk = [&4u32.to_be_bytes()[..], b"nope"].concat();
    let tool = dir.tool(&tool_script(&format!("{}\nsleep 5", printf_bytes(&junk))));
    assert!(matches!(run(&tool), Err(FormatHostError::Protocol(_))));
    // A header that declares more than the cap: refused before anything is read or allocated.
    let huge = (MAX_FRAME_BYTES as u32 + 1).to_be_bytes();
    let tool = dir.tool(&tool_script(&format!("{}\nsleep 5", printf_bytes(&huge))));
    let started = Instant::now();
    assert!(matches!(run(&tool), Err(FormatHostError::TooLarge)));
    assert!(started.elapsed() < Duration::from_secs(4));
    // An answer that is not an answer.
    let tool = dir.tool(&tool_script(&hello()));
    assert!(matches!(run(&tool), Err(FormatHostError::Protocol(m)) if m.contains("second hello")));
}

#[test]
fn a_tool_that_hangs_is_stopped_at_the_limit_with_its_children() {
    let dir = Scripts::new("timeout");
    let marker = dir.0.join("grandchild-alive");
    // The tool's child keeps running after the tool is killed, unless the whole group is.
    let tool = dir.tool(&tool_script(&format!("(sleep 2; touch '{}') &\nwait", marker.display())));
    let started = Instant::now();
    let outcome = tool.format(request(), Duration::from_millis(300), &FormatCancel::new());
    assert!(matches!(outcome, Err(FormatHostError::TimedOut(_))), "{outcome:?}");
    assert!(started.elapsed() < Duration::from_secs(2), "{:?}", started.elapsed());
    std::thread::sleep(Duration::from_millis(2500));
    assert!(!marker.exists(), "the process group was killed with the tool");
}

#[test]
fn cancel_stops_a_running_tool_and_a_cancelled_run_never_starts() {
    let dir = Scripts::new("cancel");
    let tool = dir.tool(&tool_script("sleep 30"));
    let cancel = FormatCancel::new();
    let handle = cancel.clone();
    let worker = {
        let tool = tool.clone();
        std::thread::spawn(move || tool.format(request(), LIMIT, &cancel))
    };
    std::thread::sleep(Duration::from_millis(300));
    let started = Instant::now();
    handle.cancel();
    handle.cancel();
    let outcome = worker.join().unwrap();
    assert!(matches!(outcome, Err(FormatHostError::Cancelled)), "{outcome:?}");
    assert!(started.elapsed() < Duration::from_secs(3), "killed, not waited out");

    let cancelled = FormatCancel::new();
    cancelled.cancel();
    let missing = SqlFormatTool::new("/nonexistent/tablex-sqruff");
    assert!(matches!(missing.format(request(), LIMIT, &cancelled), Err(FormatHostError::Cancelled)));
}

#[test]
fn a_missing_executable_is_not_installed() {
    let missing = SqlFormatTool::new("/nonexistent/dir/tablex-sqruff");
    match run(&missing) {
        Err(FormatHostError::NotInstalled { path }) => assert!(path.ends_with("tablex-sqruff")),
        other => panic!("{other:?}"),
    }
    let message = run(&missing).unwrap_err().to_string();
    assert!(message.contains("was not found"), "{message}");
}

// ---- filters (the user's command) -------------------------------------------------------

fn sh(script: &str) -> Command {
    let mut command = Command::new("/bin/sh");
    command.arg("-c").arg(script);
    command
}

#[test]
fn a_filter_reads_stdin_and_writes_stdout() {
    let upper = Command::new("tr");
    let mut upper = upper;
    upper.args(["a-z", "A-Z"]);
    assert_eq!(run_filter(upper, "select a from t", LIMIT, &FormatCancel::new(), &[]).unwrap(), "SELECT A FROM T");
    // More than a pipe buffer each way: neither side may deadlock.
    let big = "select 1;\n".repeat(200_000);
    let echoed = run_filter(Command::new("cat"), &big, LIMIT, &FormatCancel::new(), &[]).unwrap();
    assert_eq!(echoed.len(), big.len());
}

#[test]
fn a_filter_that_fails_reports_status_and_stderr() {
    let outcome = run_filter(sh("echo 'syntax error at 1:7' >&2; exit 2"), "x", LIMIT, &FormatCancel::new(), &[]);
    match outcome {
        Err(FormatHostError::Crashed { status, stderr }) => {
            assert!(status.is_some_and(|status| status.contains('2')));
            assert_eq!(stderr, "syntax error at 1:7");
        }
        other => panic!("{other:?}"),
    }
    // Fails before it reads its input: the broken pipe is not the error.
    let outcome = run_filter(sh("echo nope >&2; exit 1"), &"x".repeat(1 << 20), LIMIT, &FormatCancel::new(), &[]);
    assert!(matches!(outcome, Err(FormatHostError::Crashed { stderr, .. }) if stderr == "nope"));
}

#[test]
fn a_filter_is_stopped_at_the_limit_and_on_cancel() {
    let started = Instant::now();
    let outcome = run_filter(sh("sleep 30"), "x", Duration::from_millis(300), &FormatCancel::new(), &[]);
    assert!(matches!(outcome, Err(FormatHostError::TimedOut(_))), "{outcome:?}");
    assert!(started.elapsed() < Duration::from_secs(3));

    let cancel = FormatCancel::new();
    let handle = cancel.clone();
    let worker = std::thread::spawn(move || run_filter(sh("sleep 30"), "x", LIMIT, &cancel, &[]));
    std::thread::sleep(Duration::from_millis(300));
    handle.cancel();
    assert!(matches!(worker.join().unwrap(), Err(FormatHostError::Cancelled)));
}

#[test]
fn a_filter_answer_is_bounded_and_must_be_text() {
    let big = run_filter(sh("head -c 70000000 /dev/zero"), "x", LIMIT, &FormatCancel::new(), &[]);
    assert!(matches!(big, Err(FormatHostError::TooLarge)), "{big:?}");
    let binary = run_filter(sh("printf '\\377\\376'"), "x", LIMIT, &FormatCancel::new(), &[]);
    assert!(matches!(binary, Err(FormatHostError::Protocol(_))), "{binary:?}");
    let missing = run_filter(Command::new("/nonexistent/pg_format"), "x", LIMIT, &FormatCancel::new(), &[]);
    assert!(matches!(missing, Err(FormatHostError::NotInstalled { .. })));
}

#[test]
fn every_run_gets_its_own_private_working_directory_that_is_removed_after_it() {
    // A filter prints where it runs and who may enter.
    let report = |_: usize| run_filter(sh("pwd -P; ls -ld ."), "x", LIMIT, &FormatCancel::new(), &[]).unwrap();
    let (first, second) = (report(0), report(1));
    let lines = |text: &str| text.lines().map(str::to_owned).collect::<Vec<_>>();
    let (first, second) = (lines(&first), lines(&second));
    assert_ne!(first[0], second[0], "a new directory per run");
    let temp = std::env::temp_dir().canonicalize().unwrap();
    assert!(std::path::Path::new(&first[0]).starts_with(&temp) && first[0] != temp.to_string_lossy(), "{first:?}");
    assert!(first[1].starts_with("drwx------"), "private to the user: {}", first[1]);
    assert!(!std::path::Path::new(&first[0]).exists(), "removed after the run");

    // The same for a tool: it leaves its cwd in a file of ours.
    let dir = Scripts::new("cwd");
    let marker = dir.0.join("cwd.txt");
    let tool = dir.tool(&format!("pwd -P > '{}'\n{}", marker.display(), tool_script(&done("SELECT 1"))));
    run(&tool).unwrap();
    let cwd = std::fs::read_to_string(&marker).unwrap();
    assert!(std::path::Path::new(cwd.trim()).starts_with(&temp) && !std::path::Path::new(cwd.trim()).exists());
}

#[test]
fn a_command_may_exit_with_an_accepted_status_after_writing_its_output() {
    // sqruff / sqlfluff `fix`: the fixed text on stdout, exit 1 when violations remain.
    let writes_and_exits = |code: i32| sh(&format!("cat >/dev/null; printf 'SELECT 1'; echo 'unfixable: LT05' >&2; exit {code}"));
    let cancel = FormatCancel::new;
    assert!(matches!(
        run_filter(writes_and_exits(1), "x", LIMIT, &cancel(), &[]),
        Err(FormatHostError::Crashed { stderr, .. }) if stderr == "unfixable: LT05"
    ), "by default only 0 is success");
    assert_eq!(run_filter(writes_and_exits(1), "x", LIMIT, &cancel(), &[0, 1]).unwrap(), "SELECT 1");
    assert_eq!(run_filter(writes_and_exits(0), "x", LIMIT, &cancel(), &[0, 1]).unwrap(), "SELECT 1");
    // Any other status is still a failure, and so is being killed by a signal.
    assert!(matches!(run_filter(writes_and_exits(2), "x", LIMIT, &cancel(), &[0, 1]), Err(FormatHostError::Crashed { .. })));
    assert!(matches!(run_filter(sh("kill -9 $$"), "x", LIMIT, &cancel(), &[0, 1]), Err(FormatHostError::Crashed { .. })));
}

#[test]
fn an_oversized_answer_ends_the_run_at_once_even_while_the_input_is_still_being_written() {
    // The filter never reads its (large) input and floods stdout: the writer is blocked on a full
    // pipe, and the answer must be TooLarge, not a timeout.
    let input = "select 1;\n".repeat(2_000_000);
    let started = Instant::now();
    let outcome = run_filter(sh("head -c 70000000 /dev/zero"), &input, Duration::from_secs(30), &FormatCancel::new(), &[]);
    assert!(matches!(outcome, Err(FormatHostError::TooLarge)), "{outcome:?}");
    assert!(started.elapsed() < Duration::from_secs(10), "{:?}", started.elapsed());
}
