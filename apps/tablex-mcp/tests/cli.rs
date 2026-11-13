//! The built binary: command-line flags, and smoke tests over its real stdio. The binary is built
//! with the `test-credentials` feature (see `Cargo.toml`), so it never touches the OS keychain.

mod support;

use std::ffi::OsStr;
use std::io::{BufRead, BufReader, Read, Write};
use std::process::{Child, Command, Output, Stdio};
use std::sync::mpsc::{self, Receiver};
use std::time::{Duration, Instant};

use serde_json::{json, Value};
use support::*;

const EXE: &str = env!("CARGO_BIN_EXE_tablex-mcp");
const WAIT: Duration = Duration::from_secs(60);

/// The binary with every TableX directory pointed into the temporary fixture.
fn command() -> Command {
    let shared = shared();
    let mut command = Command::new(EXE);
    command
        .env("TABLEX_DATA_DIR", shared.data_dir())
        .env("TABLEX_DRIVER_DATA_DIR", shared.driver_dir());
    command
}

fn run(args: &[&str]) -> Output {
    command().args(args).output().expect("run tablex-mcp")
}

#[test]
fn version_prints_the_name_and_version_and_exits_zero() {
    let output = run(&["--version"]);
    assert_eq!(output.status.code(), Some(0));
    assert_eq!(
        String::from_utf8_lossy(&output.stdout),
        format!("tablex-mcp {}\n", env!("CARGO_PKG_VERSION"))
    );
    assert!(output.stderr.is_empty());
}

#[test]
fn help_prints_the_usage_to_stdout() {
    for flag in ["--help", "-h"] {
        let output = run(&[flag]);
        assert_eq!(output.status.code(), Some(0));
        let out = String::from_utf8_lossy(&output.stdout);
        assert!(
            out.contains("Usage: tablex-mcp") && out.contains("--print-config"),
            "{out}"
        );
    }
}

#[test]
fn print_config_is_a_ready_to_paste_client_snippet_with_an_absolute_command() {
    let output = run(&["--print-config"]);
    assert_eq!(output.status.code(), Some(0));
    let config: Value = serde_json::from_slice(&output.stdout).expect("valid JSON");
    let entry = &config["mcpServers"]["tablex"];
    assert_eq!(entry["args"], json!([]));
    let command = std::path::Path::new(entry["command"].as_str().expect("a command"));
    assert!(command.is_absolute(), "{command:?}");
    assert_eq!(
        std::fs::canonicalize(command).unwrap(),
        std::fs::canonicalize(EXE).unwrap()
    );
    assert_eq!(config["mcpServers"].as_object().unwrap().len(), 1);
}

#[test]
fn an_unknown_argument_prints_the_usage_to_stderr_and_exits_two() {
    for args in [&["--frobnicate"][..], &["serve"], &["--version", "--help"]] {
        let output = run(args);
        assert_eq!(output.status.code(), Some(2), "{args:?}");
        assert!(output.stdout.is_empty(), "nothing on stdout: {args:?}");
        assert!(String::from_utf8_lossy(&output.stderr).contains("Usage: tablex-mcp"));
    }
}

/// The name `Fixture::connection` gives the connection (`fixture <n>`, n from the id's last digits).
fn fixture_name(conn: &Conn) -> String {
    format!("fixture {}", conn.id[24..].trim_start_matches('0'))
}

#[test]
fn authorize_keychain_reads_each_connection_open_to_agents_and_skips_the_others() {
    let fixture = Fixture::new();
    let open = fixture.connection(Some("read"));
    let off = fixture.connection(None);
    let output = run(&["--authorize-keychain"]);
    let out = String::from_utf8_lossy(&output.stdout);
    let err = String::from_utf8_lossy(&output.stderr);
    assert_eq!(output.status.code(), Some(0), "{out}{err}");
    if !cfg!(target_os = "macos") {
        assert!(out.contains("nothing to authorize"), "{out}");
        return;
    }
    // The test build's store is empty and a SQLite connection saves nothing, so nothing is read
    // from the real Keychain and nothing is asked.
    assert!(
        out.contains(&format!("{}: nothing saved\n", fixture_name(&open))),
        "{out}"
    );
    assert!(!out.contains(&fixture_name(&off)), "{out}");
    assert!(out.contains("off for agents and"), "{out}");
    assert!(out.contains("skipped"), "{out}");
    // The verify child ran and was happy.
    assert!(out.contains("Keychain access is allowed for "), "{out}");
    assert!(out.contains("Restart your MCP client"), "{out}");
}

#[test]
fn the_hidden_verify_command_exits_zero_and_is_not_in_the_usage() {
    let fixture = Fixture::new();
    fixture.connection(Some("read"));
    let output = run(&["--verify-keychain-access"]);
    assert_eq!(output.status.code(), Some(0));
    assert!(output.stdout.is_empty());
    let help = run(&["--help"]);
    let help = String::from_utf8_lossy(&help.stdout);
    assert!(help.contains("--authorize-keychain"), "{help}");
    assert!(!help.contains("verify-keychain-access"), "{help}");
}

#[cfg(unix)]
#[test]
fn a_non_utf8_argument_is_an_unknown_argument_not_a_panic() {
    use std::os::unix::ffi::OsStrExt;
    let output = command()
        .arg(OsStr::from_bytes(b"--\xff"))
        .output()
        .unwrap();
    assert_eq!(output.status.code(), Some(2), "not the panic exit code 101");
    assert!(output.stdout.is_empty());
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("not valid UTF-8") && stderr.contains("Usage: tablex-mcp"),
        "{stderr}"
    );
}

#[cfg(unix)]
#[test]
fn print_config_from_a_non_utf8_path_is_an_error_not_a_panic() {
    use std::os::unix::ffi::OsStrExt;
    let dir = tempfile::tempdir().unwrap();
    let copy = dir.path().join(OsStr::from_bytes(b"bin-\xff"));
    // Some file systems (APFS) refuse such a name: then there is nothing to test here.
    if std::fs::copy(EXE, &copy).is_err() {
        eprintln!("skipped: this file system refuses non-UTF-8 names");
        return;
    }
    let output = Command::new(&copy).arg("--print-config").output().unwrap();
    assert_eq!(output.status.code(), Some(1));
    assert!(output.stdout.is_empty());
    assert!(String::from_utf8_lossy(&output.stderr).contains("not valid UTF-8"));
}

/// A running server, its stdout lines and its stderr.
struct Session {
    child: Child,
    lines: Receiver<String>,
    seen: Vec<String>,
    /// Collects stderr until the server closes it (it exits).
    stderr: Option<std::thread::JoinHandle<String>>,
}

impl Session {
    fn start() -> Self {
        Self::spawn(&mut command())
    }

    fn spawn(command: &mut Command) -> Self {
        let mut child = command
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .expect("spawn tablex-mcp");
        let (sender, lines) = mpsc::channel();
        let stdout = child.stdout.take().unwrap();
        std::thread::spawn(move || {
            for line in BufReader::new(stdout).lines().map_while(Result::ok) {
                if sender.send(line).is_err() {
                    break;
                }
            }
        });
        // Keep stderr so the child never blocks on a full pipe, and so tests can read it.
        let mut pipe = child.stderr.take().unwrap();
        let stderr = Some(std::thread::spawn(move || {
            let mut text = String::new();
            let _ = pipe.read_to_string(&mut text);
            text
        }));
        Self {
            child,
            lines,
            seen: Vec::new(),
            stderr,
        }
    }

    /// Everything the server wrote to stderr. Call it once, after the server has exited: it
    /// waits for the collector to reach the end of the pipe, so nothing written is missed.
    fn stderr(&mut self) -> String {
        self.stderr
            .take()
            .expect("stderr is read once")
            .join()
            .expect("stderr collector")
    }

    fn send(&mut self, message: &Value) {
        let stdin = self.child.stdin.as_mut().expect("stdin open");
        writeln!(stdin, "{message}").unwrap();
        stdin.flush().unwrap();
    }

    fn next_line(&mut self) -> Option<Value> {
        let line = self.lines.recv_timeout(WAIT).ok()?;
        self.seen.push(line.clone());
        Some(serde_json::from_str(&line).unwrap_or_else(|_| panic!("not JSON on stdout: {line}")))
    }

    /// Sends a request and returns its response (nothing else is expected in between).
    fn request(&mut self, id: u64, method: &str, params: Value) -> Value {
        self.send(&json!({"jsonrpc": "2.0", "id": id, "method": method, "params": params}));
        let response = self.next_line().expect("a response");
        assert_eq!(response["id"], id, "{response}");
        response
    }

    fn call(&mut self, id: u64, tool: &str, arguments: Value) -> Value {
        let response = self.request(
            id,
            "tools/call",
            json!({"name": tool, "arguments": arguments}),
        );
        response["result"].clone()
    }

    fn initialize(&mut self) {
        self.initialize_with(json!({}));
    }

    fn initialize_with(&mut self, capabilities: Value) {
        let response = self.request(
            1,
            "initialize",
            json!({"protocolVersion": "2025-06-18", "capabilities": capabilities,
                   "clientInfo": {"name": "smoke", "version": "0"}}),
        );
        assert_eq!(response["result"]["serverInfo"]["name"], "tablex-mcp");
        self.send(&json!({"jsonrpc": "2.0", "method": "notifications/initialized"}));
    }

    fn close_stdin(&mut self) {
        drop(self.child.stdin.take());
    }

    fn wait_for_exit(&mut self) -> Option<i32> {
        let deadline = Instant::now() + WAIT;
        while Instant::now() < deadline {
            if let Some(status) = self.child.try_wait().unwrap() {
                return status.code();
            }
            std::thread::sleep(Duration::from_millis(10));
        }
        let _ = self.child.kill();
        None
    }

    /// Everything the server wrote to stdout must be a JSON-RPC 2.0 frame.
    fn assert_only_frames(&mut self) {
        while let Ok(line) = self.lines.recv_timeout(Duration::from_millis(200)) {
            self.seen.push(line);
        }
        assert!(!self.seen.is_empty());
        for line in &self.seen {
            let frame: Value = serde_json::from_str(line)
                .unwrap_or_else(|_| panic!("stdout carries a non-JSON line: {line:?}"));
            assert_eq!(frame["jsonrpc"], "2.0", "{line}");
            let response = frame.get("id").is_some()
                && (frame.get("result").is_some() || frame.get("error").is_some());
            let request_or_notification = frame["method"].is_string();
            assert!(
                response || request_or_notification,
                "not a JSON-RPC frame: {line}"
            );
        }
    }
}

#[test]
fn stdout_carries_only_json_rpc_frames_and_stdin_eof_exits_zero() {
    // A real SQLite connection (no saved password) in the store both processes share, so the
    // driver really loads.
    let conn = Fixture::new().connection(Some("read"));
    let mut session = Session::start();
    session.initialize();

    let tools = session.request(2, "tools/list", json!({}));
    let names: Vec<&str> = tools["result"]["tools"]
        .as_array()
        .unwrap()
        .iter()
        .filter_map(|tool| tool["name"].as_str())
        .collect();
    assert!(
        names.contains(&"execute_query") && names.contains(&"list_connections"),
        "{names:?}"
    );

    let listed = session.call(3, "list_connections", json!({}));
    assert!(listed["structuredContent"]["connections"]
        .to_string()
        .contains(&conn.id));
    let opened = session.call(4, "open_session", json!({"connection_id": conn.id}));
    assert_eq!(opened["isError"], false, "{opened}");
    let rows = session.call(
        5,
        "execute_query",
        json!({"connection_id": conn.id, "statement": "SELECT label FROM items ORDER BY id"}),
    );
    assert!(
        rows["structuredContent"].to_string().contains("alpha"),
        "{rows}"
    );

    session.close_stdin();
    assert_eq!(session.wait_for_exit(), Some(0), "exits 0 on stdin EOF");
    session.assert_only_frames();

    // The binary under test is the credential-free build, and it announces the data directory.
    let stderr = session.stderr();
    let data_dir = shared().data_dir();
    assert!(stderr.contains("TEST BUILD (test-credentials)"), "{stderr}");
    assert!(
        stderr.contains(&format!("using the data directory {}", data_dir.display())),
        "{stderr}"
    );

    // The binary wrote its own audit log (in the redirected data dir), as the `mcp` front end.
    let audit = data_dir.join("tablex").join("audit.jsonl");
    let entries = engine_agent::read_entries(&audit, 100).unwrap().entries;
    assert!(entries
        .iter()
        .any(|entry| entry.tool == "execute_query" && entry.front_end == "mcp"));
}

/// The message of an `isError` tool result.
fn error_text(result: &Value) -> String {
    assert_eq!(result["isError"], true, "{result}");
    let body: Value = serde_json::from_str(result["content"][0]["text"].as_str().unwrap()).unwrap();
    body["error"]["message"].as_str().unwrap().to_owned()
}

#[test]
fn the_real_binary_reads_the_switch_from_the_apps_settings_file_and_asks_the_client() {
    use tablex_mcp::approver::{REASON_NO_ELICITATION, REASON_SWITCH_OFF};
    let conn = Fixture::new().connection(Some("read_write"));
    // The binary gets a data directory of its own, with a copy of the connection store (a SQLite
    // file without a write-ahead log: what is committed is in the file), so the settings file this
    // test rewrites is not shared with the other tests.
    let own = tempfile::tempdir().unwrap();
    let app_dir = own.path().join("tablex");
    std::fs::create_dir_all(&app_dir).unwrap();
    std::fs::copy(
        shared().data_dir().join("tablex").join("connections.db"),
        app_dir.join("connections.db"),
    )
    .unwrap();
    let settings = app_dir.join("settings.json");
    let start = || {
        Session::spawn(
            Command::new(EXE)
                .env("TABLEX_DATA_DIR", own.path())
                .env("TABLEX_DRIVER_DATA_DIR", shared().driver_dir()),
        )
    };
    let write =
        json!({"connection_id": conn.id, "statement": "INSERT INTO items(label) VALUES ('cli')"});
    let count = json!({"connection_id": conn.id, "statement": "SELECT count(*) FROM items WHERE label = 'cli'"});

    // Switch on in the file the binary's own data directory holds, and a client that cannot be
    // asked: the reason is the client, not the switch.
    std::fs::write(
        &settings,
        r#"{"schema_version": 4, "ai": {"mcp_writes": true}}"#,
    )
    .unwrap();
    let mut session = start();
    session.initialize();
    let opened = session.call(2, "open_session", json!({"connection_id": conn.id}));
    assert_eq!(opened["isError"], false, "{opened}");
    let refused = session.call(3, "execute_query", write.clone());
    assert!(
        error_text(&refused).ends_with(REASON_NO_ELICITATION),
        "{}",
        error_text(&refused)
    );
    // Read at the time of the write: switched off, the next write names the switch.
    std::fs::write(
        &settings,
        r#"{"schema_version": 4, "ai": {"mcp_writes": false}}"#,
    )
    .unwrap();
    let refused = session.call(4, "execute_query", write.clone());
    assert!(
        error_text(&refused).ends_with(REASON_SWITCH_OFF),
        "{}",
        error_text(&refused)
    );
    session.close_stdin();
    assert_eq!(session.wait_for_exit(), Some(0));

    // A client that declares elicitation is asked, and a yes runs the statement.
    std::fs::write(
        &settings,
        r#"{"schema_version": 4, "ai": {"mcp_writes": true}}"#,
    )
    .unwrap();
    let mut session = start();
    session.initialize_with(json!({"elicitation": {}}));
    session.call(2, "open_session", json!({"connection_id": conn.id}));
    session.send(&json!({"jsonrpc": "2.0", "id": 3, "method": "tools/call",
        "params": {"name": "execute_query", "arguments": write}}));
    let question = session.next_line().expect("a question");
    assert_eq!(question["method"], "elicitation/create", "{question}");
    assert_eq!(
        question["params"]["requestedSchema"]["required"],
        json!(["run"])
    );
    assert!(question["params"]["message"]
        .as_str()
        .unwrap()
        .contains("INSERT INTO items"));
    session.send(&json!({"jsonrpc": "2.0", "id": question["id"],
        "result": {"action": "accept", "content": {"run": true}}}));
    let answer = session.next_line().expect("the call's result");
    assert_eq!(answer["id"], 3, "{answer}");
    assert_eq!(answer["result"]["isError"], false, "{answer}");
    let counted = session.call(4, "execute_query", count);
    assert!(
        counted["structuredContent"].to_string().contains('1'),
        "{counted}"
    );
    session.close_stdin();
    assert_eq!(session.wait_for_exit(), Some(0));
    let audit = app_dir.join("audit.jsonl");
    let entries = engine_agent::read_entries(&audit, 100).unwrap().entries;
    assert!(entries
        .iter()
        .any(|entry| entry.approval.as_deref() == Some("approved") && entry.front_end == "mcp"));
}

#[test]
fn the_server_ends_with_exit_zero_when_the_client_leaves_before_initializing() {
    let mut session = Session::start();
    session.close_stdin();
    assert_eq!(session.wait_for_exit(), Some(0));
}

#[cfg(unix)]
#[test]
fn an_empty_or_relative_data_dir_is_ignored_and_said_so() {
    // The platform default then applies; point it at a temp dir so the real one is never touched.
    let home = tempfile::tempdir().unwrap();
    for value in ["", "relative/dir", "."] {
        let mut session = Session::spawn(
            Command::new(EXE)
                .env("TABLEX_DATA_DIR", value)
                .env("HOME", home.path())
                .env("XDG_DATA_HOME", home.path().join("data"))
                .env("XDG_CONFIG_HOME", home.path().join("config"))
                .env("TABLEX_DRIVER_DATA_DIR", shared().driver_dir()),
        );
        session.close_stdin();
        assert_eq!(session.wait_for_exit(), Some(0), "{value}");
        let stderr = session.stderr();
        assert!(
            !stderr.contains("using the data directory"),
            "{value}: {stderr}"
        );
        // An empty value is the same as unset (silent); a relative one is said to be ignored.
        assert_eq!(
            stderr.contains("ignoring TABLEX_DATA_DIR"),
            !value.is_empty(),
            "{value}: {stderr}"
        );
    }
    assert!(
        !std::path::Path::new("relative").exists(),
        "nothing was created relative to the working directory"
    );
}

#[cfg(unix)]
#[test]
fn an_empty_or_relative_driver_dir_is_ignored_and_said_so_and_an_absolute_one_is_named() {
    // The platform default then applies; point it at a temp dir so the real one is never touched.
    let home = tempfile::tempdir().unwrap();
    let with_driver_dir = |value: &std::ffi::OsStr| {
        let mut with = command();
        with.env("HOME", home.path())
            .env("XDG_DATA_HOME", home.path().join("data"))
            .env("TABLEX_DRIVER_DATA_DIR", value);
        with
    };
    for value in ["", "relative/drivers", "."] {
        let mut session = Session::spawn(&mut with_driver_dir(value.as_ref()));
        session.close_stdin();
        assert_eq!(session.wait_for_exit(), Some(0), "{value}");
        let stderr = session.stderr();
        assert!(
            !stderr.contains("using the driver directory"),
            "{value}: {stderr}"
        );
        // An empty value is the same as unset (silent); a relative one is said to be ignored.
        assert_eq!(
            stderr.contains("ignoring TABLEX_DRIVER_DATA_DIR"),
            !value.is_empty(),
            "{value}: {stderr}"
        );
    }
    assert!(
        !std::path::Path::new("relative").exists(),
        "nothing was created relative to the working directory"
    );
    let drivers = shared().driver_dir();
    let mut session = Session::spawn(&mut with_driver_dir(drivers.as_os_str()));
    session.close_stdin();
    assert_eq!(session.wait_for_exit(), Some(0));
    assert!(session
        .stderr()
        .contains(&format!("using the driver directory {}", drivers.display())),);
}

/// A store in the place releases up to 0.1.0 used (`<platform data dir>/Drivers/v2`) is moved to
/// `<platform data dir>/tablex/Drivers/v2` at start, with its record paths, and the move is said
/// once on stderr. No `TABLEX_*` variable is set, so this is the default path of a real install.
#[cfg(unix)]
#[test]
fn a_store_in_the_0_1_0_place_is_moved_into_the_app_folder_at_start() {
    use engine_driver_host::{InstalledDriverRecord, InstalledDriverStore};
    let home = tempfile::tempdir().unwrap();
    let platform = if cfg!(target_os = "macos") {
        home.path().join("Library/Application Support")
    } else {
        home.path().join("data")
    };
    let library = platform.join("Drivers/v2/sqlite/1.0.0/libdbengine_sqlite.dylib");
    std::fs::create_dir_all(library.parent().unwrap()).unwrap();
    std::fs::write(&library, "lib").unwrap();
    InstalledDriverStore::new(&platform)
        .save(InstalledDriverRecord {
            id: "sqlite".into(),
            version: "1.0.0".into(),
            path: library,
            ..Default::default()
        })
        .unwrap();
    let mut command = command();
    command
        .env_remove("TABLEX_DATA_DIR")
        .env_remove("TABLEX_DRIVER_DATA_DIR")
        .env("HOME", home.path())
        .env("XDG_DATA_HOME", home.path().join("data"));

    for run in 0..2 {
        let mut session = Session::spawn(&mut command);
        session.close_stdin();
        assert_eq!(session.wait_for_exit(), Some(0));
        // Said by the first start only.
        assert_eq!(
            session.stderr().contains("Moved the driver store"),
            run == 0
        );
    }

    assert!(!platform.join("Drivers").exists());
    let record = InstalledDriverStore::new(platform.join("tablex"))
        .record("sqlite")
        .unwrap()
        .expect("record moved");
    assert_eq!(
        record.path,
        platform.join("tablex/Drivers/v2/sqlite/1.0.0/libdbengine_sqlite.dylib")
    );
    assert!(record.path.is_file());
}

#[cfg(unix)]
#[test]
fn sigterm_ends_the_server_with_exit_zero() {
    let mut session = Session::start();
    session.initialize();
    terminate(&session);
    assert_eq!(session.wait_for_exit(), Some(0));
    session.assert_only_frames();
}

#[cfg(unix)]
fn terminate(session: &Session) {
    let status = Command::new("kill")
        .args(["-TERM", &session.child.id().to_string()])
        .status()
        .unwrap();
    assert!(status.success());
}

/// Starts a statement that never ends and waits until the layer says it runs: a second statement
/// on the session is refused with `agent.limit`. A probe can catch the session at the instant the
/// long statement arrives, and then the long one is refused and sent again.
#[cfg(unix)]
fn start_endless_statement(session: &mut Session, conn: &str) {
    const FOREVER: &str = "WITH RECURSIVE c(x) AS (SELECT 1 UNION ALL SELECT x + 1 FROM c \
                           WHERE x < 1000000000000) SELECT count(*) FROM c";
    let tool_call = |id: u64, statement: &str| {
        json!({"jsonrpc": "2.0", "id": id, "method": "tools/call", "params": {
            "name": "execute_query",
            "arguments": {"connection_id": conn, "statement": statement}}})
    };
    let mut next_id = 100;
    loop {
        let endless = next_id;
        next_id += 1;
        session.send(&tool_call(endless, FOREVER));
        let mut endless_answered = false;
        loop {
            let probe = next_id;
            next_id += 1;
            session.send(&tool_call(probe, "SELECT 1"));
            let answer = loop {
                let frame = session.next_line().expect("a frame");
                if frame["id"] == endless {
                    endless_answered = true;
                } else if frame["id"] == probe {
                    break frame;
                }
            };
            let text = answer["result"]["content"][0]["text"]
                .as_str()
                .unwrap_or("");
            if text.contains("agent.limit") {
                return;
            }
            if endless_answered {
                break; // the endless statement was the one refused: send it again
            }
            std::thread::sleep(Duration::from_millis(5));
        }
    }
}

#[cfg(unix)]
#[test]
fn sigterm_with_a_statement_running_cancels_it_and_exits_zero() {
    let conn = Fixture::new().connection(Some("read"));
    let mut session = Session::start();
    session.initialize();
    let opened = session.call(2, "open_session", json!({"connection_id": conn.id}));
    assert_eq!(opened["isError"], false, "{opened}");
    start_endless_statement(&mut session, &conn.id);

    terminate(&session);
    assert_eq!(session.wait_for_exit(), Some(0));
    session.assert_only_frames();

    let audit = shared().data_dir().join("tablex").join("audit.jsonl");
    let entries = engine_agent::read_entries(&audit, 10_000).unwrap().entries;
    assert!(
        entries.iter().any(|entry| entry.tool == "execute_query"
            && entry.connection_id.as_deref() == Some(conn.id.as_str())
            && entry.outcome == "agent.cancelled"),
        "the running statement was cancelled by the signal"
    );
}
