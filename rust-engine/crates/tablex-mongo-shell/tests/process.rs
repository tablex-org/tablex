//! The real `tablex-mongo-shell` executable driven by the host bridge, with a
//! fake connection behind it.

use engine_driver_api::mongo_shell::{
    write_frame, CommandFailure, CommandFailureKind, CommandReply, FailureKind, ShellRequest, ShellResult, ToChild,
};
use engine_driver_host::{MongoShell, OpenCursor, ShellCommands, ShellHostError};
use serde_json::value::RawValue;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

type Handler = Box<dyn Fn(&str, &str) -> Result<Box<RawValue>, CommandFailure> + Send + Sync>;

struct Fake {
    seen: Mutex<Vec<(String, String, bool)>>,
    handler: Handler,
    cancelled: AtomicBool,
    killed: Mutex<Vec<OpenCursor>>,
}

impl Fake {
    fn new(handler: impl Fn(&str, &str) -> Result<Box<RawValue>, CommandFailure> + Send + Sync + 'static) -> Arc<Self> {
        Arc::new(Self {
            seen: Mutex::default(),
            handler: Box::new(handler),
            cancelled: AtomicBool::new(false),
            killed: Mutex::default(),
        })
    }

    /// Replies `{ok: 1}` to everything.
    fn ok() -> Arc<Self> {
        Self::new(|_, _| Ok(RawValue::from_string(r#"{"ok":{"$numberDouble":"1.0"}}"#.into()).unwrap()))
    }

    fn seen(&self) -> Vec<(String, String, bool)> {
        self.seen.lock().unwrap().clone()
    }
}

impl ShellCommands for Fake {
    fn run(&self, database: &str, command: &RawValue, read_only: bool) -> Result<Box<RawValue>, CommandFailure> {
        self.seen.lock().unwrap().push((database.into(), command.get().into(), read_only));
        (self.handler)(database, command.get())
    }

    fn cancel(&self) {
        self.cancelled.store(true, Ordering::SeqCst);
    }

    fn kill_cursors(&self, cursors: &[OpenCursor]) {
        self.killed.lock().unwrap().extend_from_slice(cursors);
    }
}

fn shell() -> MongoShell {
    MongoShell::new(env!("CARGO_BIN_EXE_tablex-mongo-shell"))
}

fn request(script: &str) -> ShellRequest {
    ShellRequest {
        script: script.into(),
        database: "app".into(),
        allow_writes: true,
        page_size: 50,
        timeout_ms: None,
        cursor: None,
    }
}

fn raw(text: &str) -> Box<RawValue> {
    RawValue::from_string(text.into()).unwrap()
}

#[cfg(unix)]
fn process_exists(pid: u32) -> bool {
    std::process::Command::new("kill")
        .args(["-0", &pid.to_string()])
        .stderr(std::process::Stdio::null())
        .status()
        .map(|status| status.success())
        .unwrap_or(false)
}

#[test]
fn a_script_runs_in_the_child_and_its_commands_reach_the_host_in_order() {
    let fake = Fake::new(|_, command| {
        if command.starts_with(r#"{"find""#) {
            Ok(raw(r#"{"cursor":{"id":{"$numberLong":"0"},"ns":"app.items","firstBatch":[{"_id":{"$numberInt":"1"},"z":{"$numberLong":"9007199254740993"},"a":{"$numberInt":"2"}}]},"ok":{"$numberDouble":"1.0"}}"#))
        } else {
            Ok(raw(r#"{"ok":{"$numberDouble":"1.0"}}"#))
        }
    });
    let run = shell()
        .run_with(fake.clone(), request("print('hello'); db.runCommand({ ping: 1 }); db.items.find({ k: NumberLong(5) })"))
        .unwrap();
    let reply = run.wait().unwrap();
    assert!(reply.error.is_none(), "{:?}", reply.error);
    assert_eq!(reply.output, vec!["hello"]);
    match &reply.result {
        ShellResult::Cursor { documents, cursor } => {
            assert_eq!(documents[0].get(), r#"{"_id":{"$numberInt":"1"},"z":{"$numberLong":"9007199254740993"},"a":{"$numberInt":"2"}}"#);
            assert!(cursor.is_none());
        }
        other => panic!("{other:?}"),
    }
    let seen = fake.seen();
    assert_eq!(seen.len(), 2);
    assert_eq!(seen[0], ("app".to_string(), r#"{"ping":{"$numberInt":"1"}}"#.to_string(), false));
    assert!(seen[1].1.contains(r#""filter":{"k":{"$numberLong":"5"}}"#), "{}", seen[1].1);
}

#[test]
fn read_only_runs_are_marked_read_only_and_refused_writes_never_reach_the_host() {
    let fake = Fake::ok();
    let mut read_only = request("db.items.insertOne({ a: 1 })");
    read_only.allow_writes = false;
    let reply = shell().run_with(fake.clone(), read_only).unwrap().wait().unwrap();
    assert_eq!(reply.error.unwrap().kind, FailureKind::ReadOnly);
    assert!(fake.seen().is_empty());
    let mut reads = request("db.runCommand({ ping: 1 })");
    reads.allow_writes = false;
    shell().run_with(fake.clone(), reads).unwrap().wait().unwrap();
    assert!(fake.seen()[0].2, "the host is told to enforce read-only too");
}

#[test]
fn failures_of_a_command_come_back_as_the_scripts_errors() {
    let fake = Fake::new(|_, _| {
        Err(CommandFailure { kind: CommandFailureKind::Failed, code: None, message: "connection reset".into() })
    });
    let reply = shell().run_with(fake, request("db.runCommand({ ping: 1 })")).unwrap().wait().unwrap();
    let error = reply.error.unwrap();
    assert_eq!(error.kind, FailureKind::Server);
    assert_eq!(error.message, "connection reset");
}

#[test]
fn a_big_reply_survives_the_pipe() {
    let big = "x".repeat(5 * 1024 * 1024);
    let reply_text = format!(r#"{{"blob":"{big}","ok":{{"$numberDouble":"1.0"}}}}"#);
    let fake = Fake::new(move |_, _| Ok(raw(&reply_text)));
    let reply = shell()
        .run_with(fake, request("print(db.runCommand({ ping: 1 }).blob.length)"))
        .unwrap()
        .wait()
        .unwrap();
    assert_eq!(reply.output, vec![(5 * 1024 * 1024).to_string()]);
}

#[test]
fn an_infinite_loop_is_stopped_by_killing_the_child() {
    let run = shell().run_with(Fake::ok(), request("while (true) {}")).unwrap();
    let pid = run.process_id().expect("running");
    std::thread::sleep(Duration::from_millis(300));
    assert!(!run.is_finished());
    #[cfg(unix)]
    assert!(process_exists(pid));
    let started = Instant::now();
    run.cancel();
    assert!(matches!(run.wait(), Err(ShellHostError::Cancelled)));
    assert!(started.elapsed() < Duration::from_secs(5));
    #[cfg(unix)]
    assert!(!process_exists(pid), "the child process is gone");
}

#[test]
fn stopping_a_run_blocked_in_a_server_command_cancels_the_driver_operation_too() {
    let fake = Fake::new(|_, _| {
        std::thread::sleep(Duration::from_millis(200));
        Ok(raw(r#"{"ok":{"$numberDouble":"1.0"}}"#))
    });
    let run = shell().run_with(fake.clone(), request("db.runCommand({ ping: 1 })")).unwrap();
    std::thread::sleep(Duration::from_millis(100));
    run.cancel();
    assert!(matches!(run.wait(), Err(ShellHostError::Cancelled)));
    assert!(fake.cancelled.load(Ordering::SeqCst));
}

#[test]
fn the_time_limit_ends_a_loop_with_the_output_so_far() {
    let mut limited = request("print('before'); while (true) {}");
    limited.timeout_ms = Some(1_000);
    let started = Instant::now();
    let reply = shell().run_with(Fake::ok(), limited).unwrap().wait().unwrap();
    assert_eq!(reply.error.as_ref().unwrap().kind, FailureKind::Timeout);
    assert_eq!(reply.output, vec!["before"]);
    assert!(started.elapsed() < Duration::from_secs(5));
}

#[test]
fn the_time_limit_also_ends_a_script_stuck_in_a_server_call() {
    let release = Arc::new(AtomicBool::new(false));
    let gate = release.clone();
    let fake = Fake::new(move |_, _| {
        // Blocks until the host cancels the operation (it sets `cancelled` through `cancel`).
        let started = Instant::now();
        while !gate.load(Ordering::SeqCst) && started.elapsed() < Duration::from_secs(20) {
            std::thread::sleep(Duration::from_millis(10));
        }
        Err(CommandFailure { kind: CommandFailureKind::Cancelled, code: None, message: "cancelled".into() })
    });
    struct Wrapper(Arc<Fake>, Arc<AtomicBool>);
    impl ShellCommands for Wrapper {
        fn run(&self, d: &str, c: &RawValue, r: bool) -> Result<Box<RawValue>, CommandFailure> {
            self.0.run(d, c, r)
        }
        fn cancel(&self) {
            self.1.store(true, Ordering::SeqCst);
            self.0.cancel();
        }
    }
    let mut limited = request("print('start'); db.runCommand({ ping: 1 })");
    limited.timeout_ms = Some(1_500);
    let started = Instant::now();
    let reply = shell().run_with(Arc::new(Wrapper(fake, release)), limited).unwrap().wait().unwrap();
    assert_eq!(reply.error.unwrap().kind, FailureKind::Timeout);
    assert_eq!(reply.output, vec!["start"]);
    assert!(started.elapsed() < Duration::from_secs(10));
}

fn find_with_open_cursor() -> Arc<Fake> {
    Fake::new(|_, command| {
        if command.starts_with(r#"{"find""#) {
            Ok(raw(r#"{"cursor":{"id":{"$numberLong":"77"},"ns":"app.items","firstBatch":[{"_id":{"$numberInt":"1"}}]},"ok":{"$numberDouble":"1.0"}}"#))
        } else {
            Ok(raw(r#"{"ok":{"$numberDouble":"1.0"}}"#))
        }
    })
}

fn wait_for_commands(fake: &Fake, count: usize) {
    let started = Instant::now();
    while fake.seen().len() < count {
        assert!(started.elapsed() < Duration::from_secs(10), "the script never sent its command");
        std::thread::sleep(Duration::from_millis(10));
    }
}

#[test]
fn a_stopped_run_closes_the_cursors_it_left_open() {
    let fake = find_with_open_cursor();
    let run = shell()
        .run_with(fake.clone(), request("var c = db.items.find().batchSize(1); c.next(); while (true) {}"))
        .unwrap();
    // The find has been executed by the host, so its reply is tracked even if the
    // Stop lands before the host has written that reply to the child.
    wait_for_commands(&fake, 1);
    run.cancel();
    assert!(matches!(run.wait(), Err(ShellHostError::Cancelled)));
    assert_eq!(
        *fake.killed.lock().unwrap(),
        vec![OpenCursor { database: "app".into(), collection: "items".into(), id: 77 }]
    );
}

#[test]
fn a_timed_out_run_closes_its_cursors_too_but_a_clean_run_leaves_the_result_cursor_alone() {
    let fake = find_with_open_cursor();
    let mut limited = request("var c = db.items.find().batchSize(1); c.next(); while (true) {}");
    limited.timeout_ms = Some(1_500);
    let reply = shell().run_with(fake.clone(), limited).unwrap().wait().unwrap();
    assert_eq!(reply.error.unwrap().kind, FailureKind::Timeout);
    assert_eq!(fake.killed.lock().unwrap().len(), 1);

    // A clean run that returns the cursor for "More" must not close it.
    let clean = find_with_open_cursor();
    let reply = shell().run_with(clean.clone(), request("db.items.find()")).unwrap().wait().unwrap();
    assert!(reply.error.is_none());
    assert!(clean.killed.lock().unwrap().is_empty());

    // A cursor the script exhausted or closed is not remembered.
    let closed = Fake::new(|_, command| {
        if command.starts_with(r#"{"find""#) {
            Ok(raw(r#"{"cursor":{"id":{"$numberLong":"5"},"ns":"app.items","firstBatch":[]},"ok":{"$numberDouble":"1.0"}}"#))
        } else {
            Ok(raw(r#"{"ok":{"$numberDouble":"1.0"}}"#))
        }
    });
    let mut script = request("var c = db.items.find().batchSize(1); c.hasNext(); c.close(); while (true) {}");
    script.timeout_ms = Some(1_500);
    shell().run_with(closed.clone(), script).unwrap().wait().unwrap();
    assert!(closed.killed.lock().unwrap().is_empty(), "closed by the script itself");
}

#[test]
#[cfg(unix)]
fn dropping_a_run_kills_the_child() {
    let run = shell().run_with(Fake::ok(), request("while (true) {}")).unwrap();
    let pid = run.process_id().unwrap();
    std::thread::sleep(Duration::from_millis(200));
    assert!(process_exists(pid));
    drop(run);
    let started = Instant::now();
    while process_exists(pid) {
        assert!(started.elapsed() < Duration::from_secs(5), "child still alive");
        std::thread::sleep(Duration::from_millis(20));
    }
}

#[test]
fn a_missing_executable_is_a_typed_spawn_error() {
    let result = MongoShell::new("/definitely/not/here/tablex-mongo-shell").run_with(Fake::ok(), request("1"));
    assert!(matches!(result, Err(ShellHostError::Spawn { .. })));
}

/// Stand-ins for a broken plugin: tiny shell scripts speaking (or botching) the protocol.
#[cfg(unix)]
mod broken_plugins {
    use super::*;
    use std::os::unix::fs::PermissionsExt;

    /// `printf` octal escapes of a frame: 4-byte big-endian length, then the JSON.
    fn frame(json: &str) -> String {
        let mut bytes = (json.len() as u32).to_be_bytes().to_vec();
        bytes.extend_from_slice(json.as_bytes());
        bytes.iter().map(|b| format!("\\{:03o}", b)).collect()
    }

    fn plugin(dir: &tempdir::Dir, body: &str) -> MongoShell {
        let path = dir.0.join("plugin.sh");
        std::fs::write(&path, format!("#!/bin/sh\n{body}\n")).unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
        MongoShell::new(path)
    }

    mod tempdir {
        pub struct Dir(pub std::path::PathBuf);
        impl Dir {
            pub fn new(name: &str) -> Self {
                let path = std::env::temp_dir().join(format!("tablex-plugin-test-{}-{name}", std::process::id()));
                let _ = std::fs::remove_dir_all(&path);
                std::fs::create_dir_all(&path).unwrap();
                Self(path)
            }
        }
        impl Drop for Dir {
            fn drop(&mut self) {
                let _ = std::fs::remove_dir_all(&self.0);
            }
        }
    }

    fn hello(protocol: u32) -> String {
        frame(&format!(r#"{{"hello":{{"protocol":{protocol}}}}}"#))
    }

    fn run(shell: &MongoShell) -> Result<engine_driver_api::mongo_shell::ShellReply, ShellHostError> {
        shell.run_with(Fake::ok(), request("1")).unwrap().wait()
    }

    #[test]
    fn exiting_before_the_handshake_is_a_crash() {
        let dir = tempdir::Dir::new("exit");
        let result = run(&plugin(&dir, "echo 'fatal: missing library' >&2; exit 3"));
        match result {
            Err(ShellHostError::Crashed(detail)) => assert!(detail.contains("missing library"), "{detail}"),
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn a_different_protocol_version_is_refused() {
        let dir = tempdir::Dir::new("version");
        let result = run(&plugin(&dir, &format!("printf '{}'; sleep 5", hello(99))));
        assert!(matches!(result, Err(ShellHostError::ProtocolMismatch { found: 99, expected: 1 })), "{result:?}");
    }

    #[test]
    fn dying_mid_run_without_done_is_a_crash_not_a_hang() {
        let dir = tempdir::Dir::new("die");
        let result = run(&plugin(&dir, &format!("printf '{}'; cat >/dev/null & kill -9 $$", hello(1))));
        assert!(matches!(result, Err(ShellHostError::Crashed(_))), "{result:?}");
        let aborted = run(&plugin(&dir, &format!("printf '{}'; kill -ABRT $$", hello(1))));
        assert!(matches!(aborted, Err(ShellHostError::Crashed(_))), "{aborted:?}");
    }

    #[test]
    fn malformed_and_oversized_frames_kill_the_child_with_a_protocol_error() {
        let dir = tempdir::Dir::new("garbage");
        let junk = frame("this is not json");
        let result = run(&plugin(&dir, &format!("printf '{}{}'; sleep 30", hello(1), junk)));
        assert!(matches!(result, Err(ShellHostError::Protocol(_))), "{result:?}");
        // A declared length of ~4 GiB is refused before anything is allocated.
        let started = Instant::now();
        let huge = run(&plugin(&dir, &format!("printf '{}\\377\\377\\377\\377'; sleep 30", hello(1))));
        assert!(matches!(huge, Err(ShellHostError::Protocol(_))), "{huge:?}");
        assert!(started.elapsed() < Duration::from_secs(10), "the sleeping child was killed");
        let not_hello = run(&plugin(&dir, &format!("printf '{}'; sleep 30", frame(r#"{"done":{"output":[],"result":{"kind":"none"},"database_after":"x"}}"#))));
        assert!(matches!(not_hello, Err(ShellHostError::Protocol(_))), "{not_hello:?}");
    }

    #[test]
    fn a_repeated_command_id_is_a_protocol_error() {
        let dir = tempdir::Dir::new("ids");
        let command = frame(r#"{"command":{"id":1,"database":"app","command":{"ping":1}}}"#);
        let result = run(&plugin(&dir, &format!("printf '{}{}{}'; sleep 30", hello(1), command, command)));
        assert!(matches!(result, Err(ShellHostError::Protocol(_))), "{result:?}");
    }

    #[test]
    fn the_child_starts_with_an_empty_environment_in_a_private_directory() {
        let dir = tempdir::Dir::new("env");
        std::env::set_var("TABLEX_SECRET_FOR_TEST", "leak");
        let result = run(&plugin(&dir, "echo \"pwd=$(pwd)\" >&2; env >&2; echo \"files=$(ls -A | wc -l)\" >&2; exit 4"));
        let Err(ShellHostError::Crashed(detail)) = result else { panic!("{result:?}") };
        assert!(detail.contains("tablex-mongo-shell-"), "{detail}");
        assert!(!detail.contains("TABLEX_SECRET_FOR_TEST") && !detail.contains("HOME="), "{detail}");
        let files = detail.lines().find_map(|line| line.strip_prefix("files=")).expect("files line");
        assert_eq!(files.trim(), "0", "{detail}");
        let directory = detail.lines().find_map(|line| line.split("pwd=").nth(1)).expect("pwd line");
        assert!(!std::path::Path::new(directory.trim()).exists(), "the working directory is removed with the run");
    }

    /// A stand-in plugin that is dead the moment it has said its piece: it
    /// never waits for the host to read anything.
    fn plugin_that_sends_a_find_and_dies(dir: &tempdir::Dir) -> MongoShell {
        let find = frame(r#"{"command":{"id":1,"database":"app","command":{"find":"items"}}}"#);
        plugin(dir, &format!("printf '{}{}'; kill -9 $$", hello(1), find))
    }

    /// The invariant, whichever way the race between the child's death and the
    /// host's first write goes: a command the host executed has its cursor
    /// closed, and one it never executed (the child was already dead when the
    /// run request was written) is never forwarded.
    fn assert_executed_commands_are_cleaned_up(plugin: &MongoShell) -> usize {
        let fake = find_with_open_cursor();
        let result = plugin.run_with(fake.clone(), request("1")).unwrap().wait();
        assert!(matches!(result, Err(ShellHostError::Crashed(_))), "{result:?}");
        let executed = fake.seen().len();
        let killed = fake.killed.lock().unwrap().clone();
        assert!(executed <= 1, "{executed} commands from one frame");
        assert_eq!(
            killed.len(),
            executed,
            "{executed} command(s) executed but {} cursor(s) closed",
            killed.len()
        );
        if executed == 1 {
            assert_eq!(killed, vec![OpenCursor { database: "app".into(), collection: "items".into(), id: 77 }]);
        }
        executed
    }

    #[test]
    fn a_crash_after_a_cursor_was_opened_still_closes_it() {
        // Deterministic: the child waits until the host has written the run
        // request AND the reply to its command (their exact sizes are known),
        // so the command was certainly executed before the child dies.
        let dir = tempdir::Dir::new("crash_cursor");
        let request = request("1");
        let mut host_to_child = Vec::new();
        write_frame(&mut host_to_child, &ToChild::Run(request.clone())).unwrap();
        let reply = CommandReply {
            id: 1,
            ok: Some(raw(r#"{"cursor":{"id":{"$numberLong":"77"},"ns":"app.items","firstBatch":[{"_id":{"$numberInt":"1"}}]},"ok":{"$numberDouble":"1.0"}}"#)),
            error: None,
        };
        write_frame(&mut host_to_child, &ToChild::Reply(reply)).unwrap();
        let find = frame(r#"{"command":{"id":1,"database":"app","command":{"find":"items"}}}"#);
        let plugin = plugin(
            &dir,
            &format!(
                "printf '{}{}'; dd bs=1 count={} of=/dev/null 2>/dev/null; kill -9 $$",
                hello(1),
                find,
                host_to_child.len()
            ),
        );
        let fake = find_with_open_cursor();
        let result = plugin.run_with(fake.clone(), request).unwrap().wait();
        assert!(matches!(result, Err(ShellHostError::Crashed(_))), "{result:?}");
        assert_eq!(fake.seen().len(), 1, "the find was forwarded");
        assert_eq!(
            *fake.killed.lock().unwrap(),
            vec![OpenCursor { database: "app".into(), collection: "items".into(), id: 77 }]
        );
    }

    #[test]
    fn a_child_that_dies_at_any_moment_never_leaves_an_executed_cursor_open() {
        let dir = tempdir::Dir::new("crash_race");
        let plugin = plugin_that_sends_a_find_and_dies(&dir);
        // Under load the child sometimes dies before the host has written the run
        // request; then no command is executed and none must be forwarded. Either
        // way, a cursor exists exactly when it was executed, and is closed.
        for _ in 0..200 {
            assert_executed_commands_are_cleaned_up(&plugin);
        }
    }

    #[test]
    fn a_noisy_stderr_is_capped_and_never_blocks_the_child() {
        let dir = tempdir::Dir::new("noise");
        let result = run(&plugin(&dir, "i=0; while [ $i -lt 3000 ]; do echo 'noise noise noise noise noise noise noise noise' >&2; i=$((i+1)); done; exit 5"));
        let Err(ShellHostError::Crashed(detail)) = result else { panic!("{result:?}") };
        assert!(detail.len() < 9 * 1024, "{}", detail.len());
    }
}
