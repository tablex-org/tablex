//! "Left open overnight" hang tests: a connection whose server stops answering
//! (a laptop that slept, a VPN that dropped, `docker pause`) must not wedge
//! the app. Each scenario runs against the real driver dylib and a throwaway
//! container, and measures what the user would feel:
//!
//! * `paused_server`: a query on a paused server, a cancel of it, a second
//!   query on the same connection, and the disconnect. The cancel must free the
//!   query within the driver's grace period, the second query must not wait
//!   forever, and the disconnect must return.
//! * `restarted_server`: the server restarts under an open connection; the next
//!   call must fail fast with a `*.not_connected` code (the app's reconnect path
//!   engages on it).
//!
//! Opt-in. Only containers named `tablex-hang-*` are ever paused or restarted,
//! and only `127.0.0.1` URIs are accepted:
//!
//! ```text
//! cargo build -p dbengine-postgres --features abi-v2   # likewise mysql, mongo; clickhouse needs none
//! TABLEX_HANG_PG_URI=postgres://tablex:tablex@127.0.0.1:56601/tablex \
//! TABLEX_HANG_PG_CONTAINER=tablex-hang-pg \
//! TABLEX_HANG_PG_DYLIB=<target>/debug/libdbengine_postgres.dylib \
//!   cargo test -p engine-driver-host --test live_hang -- --ignored --nocapture --test-threads=1 pg
//! ```
//! (`MYSQL`, `MONGO`, `CH` replace `PG` for the other drivers.)
#![cfg(unix)]

use engine_driver_host::{DriverLoader, DriverSession, LoaderError, QueryRunner};
use serde_json::json;
use std::path::PathBuf;
use std::process::Command;
use std::sync::mpsc::{channel, Receiver};
use std::time::{Duration, Instant};
use tempfile::TempDir;

/// How long each step is observed before it counts as "hung".
const QUERY_WINDOW: Duration = Duration::from_secs(15);
const CANCEL_WINDOW: Duration = Duration::from_secs(30);
const NEXT_WINDOW: Duration = Duration::from_secs(20);
const DISCONNECT_WINDOW: Duration = Duration::from_secs(30);
const RESTART_WINDOW: Duration = Duration::from_secs(30);

struct Target {
    label: &'static str,
    env: &'static str,
    fast_request: fn(u64) -> String,
    /// After the first statement was abandoned the connection is known to be lost, so the next
    /// call fails at once. Otherwise (a stateless protocol: MongoDB, ClickHouse over HTTP) a new
    /// statement runs on a new socket and, with the server still silent, waits for its own cancel.
    next_fails_at_once: bool,
}

fn pg_request(operation: u64) -> String {
    json!({
        "kind": "sql", "operation_id": operation, "database": null, "object": null,
        "offset": 0, "limit": 100, "payload": { "sql": "SELECT 1", "confirmed": true }
    })
    .to_string()
}

fn mongo_request(operation: u64) -> String {
    json!({
        "kind": "mongo_find", "operation_id": operation, "database": "hang", "object": "docs",
        "offset": 0, "limit": 100,
        "payload": { "filter": "{}", "sort": "{\"_id\":1}", "project": "" }
    })
    .to_string()
}

const PG: Target = Target {
    label: "postgres",
    next_fails_at_once: true,
    env: "PG",
    fast_request: pg_request,
};
const MYSQL: Target = Target {
    label: "mysql",
    next_fails_at_once: true,
    env: "MYSQL",
    fast_request: pg_request,
};
const CH: Target = Target {
    label: "clickhouse",
    next_fails_at_once: false,
    env: "CH",
    fast_request: pg_request,
};
const MONGO: Target = Target {
    label: "mongodb",
    next_fails_at_once: false,
    env: "MONGO",
    fast_request: mongo_request,
};

struct Rig {
    _temp: TempDir,
    session: Option<DriverSession>,
    container: String,
    uri: String,
}

fn var(target: &Target, what: &str) -> String {
    let name = format!("TABLEX_HANG_{}_{what}", target.env);
    std::env::var(&name).unwrap_or_else(|_| panic!("set {name}"))
}

impl Rig {
    fn new(target: &Target) -> Self {
        let uri = var(target, "URI");
        assert!(uri.contains("127.0.0.1"), "refusing to touch {uri}");
        let container = var(target, "CONTAINER");
        assert!(
            container.starts_with("tablex-hang-"),
            "refusing to touch container {container}"
        );
        let dylib = PathBuf::from(var(target, "DYLIB"));
        let temp = TempDir::new().expect("temp dir");
        let staged = temp.path().join(dylib.file_name().unwrap());
        std::fs::copy(&dylib, &staged).expect("stage driver");
        let handle = DriverLoader.load(&staged).expect("load driver");
        let session = handle
            .connect(&json!({ "uri": uri, "name": "hang" }).to_string())
            .expect("connect");
        Self {
            _temp: temp,
            session: Some(session),
            container,
            uri,
        }
    }

    fn session(&self) -> &DriverSession {
        self.session.as_ref().expect("session")
    }

    fn docker(&self, args: &[&str]) {
        assert!(self.container.starts_with("tablex-hang-"));
        let status = Command::new("docker")
            .args(args)
            .arg(&self.container)
            .status()
            .expect("docker");
        assert!(status.success(), "docker {args:?} {}", self.container);
    }
}

type Done = (Instant, Result<u64, LoaderError>);

/// Runs the request on its own thread: a hung call must not hang the test.
fn spawn_query(runner: QueryRunner, request: String) -> Receiver<Done> {
    let (sender, receiver) = channel();
    std::thread::spawn(move || {
        let result = runner.open_query(&request).map(|page| {
            runner.release_page(page.page_id);
            page.row_count
        });
        let _ = sender.send((Instant::now(), result));
    });
    receiver
}

fn code(result: &Result<u64, LoaderError>) -> String {
    match result {
        Ok(rows) => format!("ok({rows} rows)"),
        Err(LoaderError::DriverRejected { code, message, .. }) => {
            format!("{code} ({})", message.chars().take(70).collect::<String>())
        }
        Err(other) => format!("{other}"),
    }
}

fn error_code(done: &Done) -> Option<&str> {
    match &done.1 {
        Err(LoaderError::DriverRejected { code, .. }) => Some(code),
        _ => None,
    }
}

fn new_operation(session: &DriverSession, target: &Target) -> (QueryRunner, String) {
    let runner = session.query_runner().expect("runner");
    let request = (target.fast_request)(runner.operation_id().0);
    (runner, request)
}

fn warm(rig: &Rig, target: &Target) {
    let (runner, request) = new_operation(rig.session(), target);
    let done = spawn_query(runner, request)
        .recv_timeout(Duration::from_secs(20))
        .expect("warm-up query");
    println!("[{}] warm-up: {}", target.label, code(&done.1));
    assert!(done.1.is_ok(), "warm-up query succeeds");
}

fn paused_server(target: &Target) {
    let mut rig = Rig::new(target);
    warm(&rig, target);
    rig.docker(&["pause"]);
    println!("[{}] container paused", target.label);

    // (a) a query on a server that never answers
    let (runner, request) = new_operation(rig.session(), target);
    let first = spawn_query(runner.clone(), request);
    let a = first.recv_timeout(QUERY_WINDOW).ok();
    println!(
        "[{}] (a) returned on its own within {QUERY_WINDOW:?}: {}",
        target.label,
        a.as_ref()
            .map_or("NO".into(), |done| format!("yes: {}", code(&done.1)))
    );

    // (b) a cancel frees it
    let cancel_started = Instant::now();
    let (cancel_sender, cancel_receiver) = channel();
    let cancelling = runner.clone();
    std::thread::spawn(move || {
        let result = cancelling.cancel_operation();
        let _ = cancel_sender.send((Instant::now(), result));
    });
    let freed = match a {
        Some(done) => Some(done),
        None => first.recv_timeout(CANCEL_WINDOW).ok(),
    };
    let cancel_call = cancel_receiver
        .recv_timeout(Duration::from_secs(1))
        .map(|(at, result)| (at - cancel_started, result.map_err(|e| e.to_string())));
    println!(
        "[{}] (b) cancel call: {cancel_call:?}; query freed by cancel: {}",
        target.label,
        freed.as_ref().map_or("NO (still hung)".into(), |done| format!(
            "yes after {:?}: {}",
            done.0 - cancel_started,
            code(&done.1)
        ))
    );

    // (c) a second query on the same connection
    let (runner2, request2) = new_operation(rig.session(), target);
    let second_started = Instant::now();
    let second = spawn_query(runner2.clone(), request2);
    let c = second.recv_timeout(NEXT_WINDOW).ok();
    println!(
        "[{}] (c) second query: {}",
        target.label,
        c.as_ref()
            .map_or(format!("BLOCKED > {NEXT_WINDOW:?}"), |done| format!(
                "returned after {:?}: {}",
                done.0 - second_started,
                code(&done.1)
            ))
    );
    let mut second_freed_by_cancel = None;
    if c.is_none() {
        // The user cancels the second one too.
        let cancel_started = Instant::now();
        let _ = runner2.cancel_operation();
        second_freed_by_cancel = second.recv_timeout(CANCEL_WINDOW).ok();
        println!(
            "[{}] (c') second query cancelled: {}",
            target.label,
            second_freed_by_cancel
                .as_ref()
                .map_or("NO (still hung)".into(), |done| format!(
                    "freed after {:?}: {}",
                    done.0 - cancel_started,
                    code(&done.1)
                ))
        );
    }

    // (d) the disconnect
    let mut session = rig.session.take().expect("session");
    let (sender, receiver) = channel();
    let disconnect_started = Instant::now();
    std::thread::spawn(move || {
        let result = session.disconnect();
        let _ = sender.send((Instant::now(), result.map_err(|e| e.to_string())));
        std::mem::forget(session);
    });
    let d = receiver.recv_timeout(DISCONNECT_WINDOW).ok();
    println!(
        "[{}] (d) disconnect: {}",
        target.label,
        d.as_ref().map_or(format!("BLOCKED > {DISCONNECT_WINDOW:?}"), |(at, r)| format!(
            "returned after {:?}: {r:?}",
            *at - disconnect_started
        ))
    );
    rig.docker(&["unpause"]);

    // What a user needs: the cancel frees the query (within the grace period),
    // the next call does not wait forever, and the app can disconnect.
    let freed = freed.expect("(b) a cancel must free a query on a dead server");
    assert!(
        freed.0 - cancel_started < Duration::from_secs(20),
        "(b) freed too late"
    );
    if target.next_fails_at_once {
        let c = c.expect("(c) a second query must not block forever");
        assert!(
            error_code(&c).is_some_and(|code| code.ends_with("not_connected")),
            "(c) the next call reports a lost connection: {}",
            code(&c.1)
        );
    } else if c.is_none() {
        assert!(
            second_freed_by_cancel.is_some(),
            "(c) the second query is freed by its own cancel"
        );
    }
    assert!(d.is_some(), "(d) disconnect must return");
}

/// Nothing is running when the server stops answering (the laptop slept with the app idle),
/// and the user disconnects, or quits: the goodbye must not wait for the silent server.
fn idle_disconnect(target: &Target) {
    let mut rig = Rig::new(target);
    warm(&rig, target);
    rig.docker(&["pause"]);
    let mut session = rig.session.take().expect("session");
    let (sender, receiver) = channel();
    let started = Instant::now();
    std::thread::spawn(move || {
        let result = session.disconnect();
        let _ = sender.send((Instant::now(), result.map_err(|e| e.to_string())));
        std::mem::forget(session);
    });
    let done = receiver.recv_timeout(DISCONNECT_WINDOW).ok();
    println!(
        "[{}] idle disconnect on a paused server: {}",
        target.label,
        done.as_ref().map_or(format!("BLOCKED > {DISCONNECT_WINDOW:?}"), |(at, r)| format!(
            "returned after {:?}: {r:?}",
            *at - started
        ))
    );
    rig.docker(&["unpause"]);
    let (at, _) = done.expect("the disconnect must return");
    assert!(at - started < Duration::from_secs(5), "the goodbye is bounded");
}

fn restarted_server(target: &Target) {
    let rig = Rig::new(target);
    warm(&rig, target);
    rig.docker(&["restart", "-t", "1"]);
    // Wait until the server takes a new connection again.
    let started = Instant::now();
    loop {
        let attempt = std::panic::catch_unwind(|| Rig::new(target));
        if attempt.is_ok() {
            break;
        }
        assert!(started.elapsed() < RESTART_WINDOW, "the server never came back");
        std::thread::sleep(Duration::from_millis(500));
    }
    println!(
        "[{}] server back after {:?}",
        target.label,
        started.elapsed()
    );
    let mut results = Vec::new();
    for attempt in 1..=3 {
        let (runner, request) = new_operation(rig.session(), target);
        let begun = Instant::now();
        let done = spawn_query(runner, request).recv_timeout(RESTART_WINDOW).ok();
        println!(
            "[{}] after restart, query {attempt}: {}",
            target.label,
            done.as_ref().map_or(format!("HUNG > {RESTART_WINDOW:?}"), |done| format!(
                "{:?}: {}",
                done.0 - begun,
                code(&done.1)
            ))
        );
        results.push(done);
    }
    let first = results[0].as_ref().expect("(L1) the first call after a restart must not hang");
    // Either it reconnects transparently, or it says the connection is lost.
    assert!(
        first.1.is_ok() || error_code(first).is_some_and(|code| code.ends_with("not_connected")),
        "(L1) the first call after a restart is classified as lost: {}",
        code(&first.1)
    );
    let _ = &rig.uri;
}

macro_rules! scenarios {
    ($($name:ident: $target:ident, $kind:ident;)*) => {$(
        #[test]
        #[ignore = "needs a tablex-hang-* container; see the module docs"]
        fn $name() {
            $kind(&$target);
        }
    )*};
}

scenarios! {
    pg_paused_server: PG, paused_server;
    pg_restarted_server: PG, restarted_server;
    pg_idle_disconnect: PG, idle_disconnect;
    mysql_idle_disconnect: MYSQL, idle_disconnect;
    mongo_idle_disconnect: MONGO, idle_disconnect;
    clickhouse_idle_disconnect: CH, idle_disconnect;
    mysql_paused_server: MYSQL, paused_server;
    mysql_restarted_server: MYSQL, restarted_server;
    mongo_paused_server: MONGO, paused_server;
    mongo_restarted_server: MONGO, restarted_server;
    clickhouse_paused_server: CH, paused_server;
    clickhouse_restarted_server: CH, restarted_server;
}
