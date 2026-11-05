mod common;
use bson::doc;
use common::*;
use engine_driver_api::mongo_shell::FailureKind;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::{Duration, Instant};

#[test]
fn output_is_capped_at_ten_thousand_lines_and_says_so() {
    let reply = run("for (var i = 0; i < 30000; i++) print('line ' + i); 'finished'", &FakePort::ok());
    assert_ok(&reply);
    assert!(reply.output_truncated);
    assert_eq!(reply.output.len(), 10_001);
    assert_eq!(reply.output[9_999], "line 9999");
    assert!(reply.output[10_000].contains("truncated"));
    assert_eq!(value_text(&reply), "\"finished\"", "the script still ran to its end");
}

#[test]
fn output_is_capped_at_four_mebibytes() {
    let reply = run("var big = 'x'.repeat(1024 * 1024); for (var i = 0; i < 10; i++) print(big);", &FakePort::ok());
    assert!(reply.output_truncated);
    assert_eq!(reply.output.len(), 5, "four whole megabytes, then the note");
    assert!(reply.output[4].contains("4 MiB"));
}

#[test]
fn an_infinite_loop_is_ended_by_a_cancel() {
    let polls = std::sync::Arc::new(AtomicUsize::new(0));
    struct FlipPort(std::sync::Arc<AtomicUsize>);
    impl tablex_mongo_shell::CommandPort for FlipPort {
        fn run_command(&self, _: &str, _: bson::Document) -> Result<bson::Document, tablex_mongo_shell::CommandError> {
            Ok(doc! { "ok": 1.0 })
        }
        fn is_cancelled(&self) -> bool {
            self.0.fetch_add(1, Ordering::SeqCst) > 20
        }
    }
    let started = Instant::now();
    let reply = tablex_mongo_shell::run_script(&request("var n = 0; print('start'); while (true) { n++; }"), &FlipPort(polls.clone()));
    assert_eq!(failure_kind(&reply), FailureKind::Cancelled);
    assert_eq!(reply.output, vec!["start"]);
    assert!(started.elapsed() < Duration::from_secs(10));
    assert!(polls.load(Ordering::SeqCst) > 20);
    // try / catch cannot swallow the stop.
    let polls = std::sync::Arc::new(AtomicUsize::new(0));
    let caught = tablex_mongo_shell::run_script(
        &request("while (true) { try { while (true) {} } catch (e) { print('swallowed'); } }"),
        &FlipPort(polls),
    );
    assert_eq!(failure_kind(&caught), FailureKind::Cancelled);
    assert!(caught.output.is_empty());
}

#[test]
fn a_cancel_that_was_already_set_stops_before_any_command_is_sent() {
    let port = FakePort::ok();
    port.cancelled.store(true, Ordering::SeqCst);
    let reply = run("db.items.insertOne({})", &port);
    assert_eq!(failure_kind(&reply), FailureKind::Cancelled);
    assert!(port.seen().is_empty());
}

#[test]
fn a_cancelled_command_ends_the_run() {
    let port = FakePort::new(|_, _| Err(tablex_mongo_shell::CommandError::Cancelled));
    let reply = run("try { db.runCommand({ ping: 1 }) } catch (e) {}\nwhile (true) {}", &port);
    assert_eq!(failure_kind(&reply), FailureKind::Cancelled);
}

#[test]
fn sleep_is_cancelled_promptly() {
    let polls = std::sync::Arc::new(AtomicUsize::new(0));
    struct FlipPort(std::sync::Arc<AtomicUsize>);
    impl tablex_mongo_shell::CommandPort for FlipPort {
        fn run_command(&self, _: &str, _: bson::Document) -> Result<bson::Document, tablex_mongo_shell::CommandError> {
            Ok(doc! { "ok": 1.0 })
        }
        fn is_cancelled(&self) -> bool {
            self.0.fetch_add(1, Ordering::SeqCst) > 5
        }
    }
    let started = Instant::now();
    let reply = tablex_mongo_shell::run_script(&request("sleep(60000); print('not reached')"), &FlipPort(polls));
    assert_eq!(failure_kind(&reply), FailureKind::Cancelled);
    assert!(started.elapsed() < Duration::from_secs(5), "{:?}", started.elapsed());
    assert!(reply.output.is_empty());
}

#[test]
fn sleep_waits_roughly_the_time_asked() {
    let started = Instant::now();
    let reply = run("sleep(120); print('after')", &FakePort::ok());
    assert_ok(&reply);
    assert!(started.elapsed() >= Duration::from_millis(110));
    assert_eq!(failure_kind(&run("sleep(-1)", &FakePort::ok())), FailureKind::Script);
}

#[test]
fn the_wall_clock_timeout_stops_loops_and_sleeps() {
    let mut loop_request = request("while (true) {}");
    loop_request.timeout_ms = Some(150);
    let started = Instant::now();
    let reply = run_with(&loop_request, &FakePort::ok());
    assert_eq!(failure_kind(&reply), FailureKind::Timeout);
    assert!(reply.error.unwrap().message.contains("150 ms"));
    assert!(started.elapsed() < Duration::from_secs(5));
    let mut sleeper = request("sleep(60000)");
    sleeper.timeout_ms = Some(100);
    let started = Instant::now();
    assert_eq!(failure_kind(&run_with(&sleeper, &FakePort::ok())), FailureKind::Timeout);
    assert!(started.elapsed() < Duration::from_secs(5));
    let mut commands = request("for (var i = 0; i < 100000; i++) db.runCommand({ ping: 1 })");
    commands.timeout_ms = Some(100);
    let slow = FakePort::new(|_, _| {
        std::thread::sleep(Duration::from_millis(20));
        Ok(doc! { "ok": 1.0 })
    });
    assert_eq!(failure_kind(&run_with(&commands, &slow)), FailureKind::Timeout);
}

#[test]
fn a_script_that_allocates_too_much_fails_cleanly() {
    let started = Instant::now();
    let reply = run(
        "var keep = []; var chunk = 'x'.repeat(1 << 20); for (var i = 0; ; i++) { keep.push(chunk + i); }",
        &FakePort::ok(),
    );
    assert_eq!(failure_kind(&reply), FailureKind::Memory, "{:?}", reply.error);
    assert!(started.elapsed() < Duration::from_secs(60));
    // The failure leaves nothing behind: the next run is unaffected.
    assert_ok(&run("1 + 1", &FakePort::ok()));
}

#[test]
fn runaway_recursion_is_a_script_error_not_a_crash() {
    let reply = run("function f() { return f() + 1; } f()", &FakePort::ok());
    assert_eq!(failure_kind(&reply), FailureKind::Script);
    assert!(reply.error.unwrap().message.contains("stack"));
}

#[test]
fn each_run_has_fresh_globals() {
    let port = FakePort::ok();
    assert_ok(&run("var leaked = 42; globalThis.other = 1;", &port));
    let reply = run("print(typeof leaked, typeof other)", &port);
    assert_eq!(reply.output, vec!["undefined undefined"]);
}

#[test]
fn a_script_cannot_hang_the_shells_own_steps_by_replacing_what_they_use() {
    // The shell reads the final database, releases cursors and describes errors
    // without calling anything the script can replace; replacing it all changes nothing.
    let started = Instant::now();
    let reply = run(
        "Object.getOwnPropertyDescriptor = function () { while (true) {} };\n\
         Reflect.get = function () { while (true) {} }; Reflect.apply = function () { while (true) {} };\n\
         WeakSet.prototype.has = function () { while (true) {} }; WeakSet.prototype.add = function () { while (true) {} };\n\
         db = db.getSiblingDB('elsewhere'); 1",
        &FakePort::ok(),
    );
    assert_ok(&reply);
    assert_eq!(value_text(&reply), "1");
    assert_eq!(reply.database_after, "elsewhere");
    assert!(started.elapsed() < Duration::from_secs(10));
}

#[test]
fn user_code_reached_by_the_result_or_the_error_is_still_stoppable() {
    for script in [
        // A Proxy as the last value: describing it runs its trap.
        "new Proxy({}, { get: function () { while (true) {} } })",
        // An error whose message getter never returns.
        "throw { get message() { while (true) {} } }",
        // A cursor method replaced by an endless loop, with a cursor left open.
        "Object.getPrototypeOf(db.items.find()).close = function () { while (true) {} };\n\
         var c = db.items.find().batchSize(1); c.next(); throw new Error('stop here');",
    ] {
        let port = FakePort::new(|_, command| match command_name(command) {
            "find" => Ok(first_batch("app.items", 9, numbered(1))),
            _ => Ok(doc! { "ok": 1.0 }),
        });
        let mut limited = request(script);
        limited.timeout_ms = Some(300);
        let started = Instant::now();
        let reply = run_with(&limited, &port);
        assert!(started.elapsed() < Duration::from_secs(10), "{script}: {:?}", started.elapsed());
        assert!(reply.error.is_some(), "{script}");
    }
}

#[test]
fn replaced_object_builtins_cannot_plant_a_getter_in_a_new_database() {
    // A new database object is built by `getSiblingDB` after the script replaced
    // the builtins its constructor and proxy use; the name the shell reads while it
    // cannot be interrupted must still be the shell's own.
    let started = Instant::now();
    let reply = run(
        "var hang = { get: function () { while (true) {} } };\n\
         Object.defineProperty = function (target, key) { return Reflect.defineProperty(target, key, hang) && target; };\n\
         Object.freeze = function () { return { get name() { while (true) {} } }; };\n\
         Proxy = function (target) { return { get name() { while (true) {} } }; };\n\
         db = db.getSiblingDB('planted'); 1",
        &FakePort::ok(),
    );
    assert_ok(&reply);
    assert_eq!(reply.database_after, "planted");
    assert!(started.elapsed() < Duration::from_secs(10));
}

#[test]
fn a_script_cannot_turn_the_database_name_into_a_hanging_getter() {
    let started = Instant::now();
    let reply = run(
        "var symbols = Object.getOwnPropertySymbols(db); var state = db[symbols[0]]; var blocked = 0;\n\
         try { Object.defineProperty(state, 'name', { get: function () { while (true) {} } }); } catch (e) { blocked++; }\n\
         try { Object.defineProperty(db, symbols[0], { value: { get name() { while (true) {} } } }); } catch (e) { blocked++; }\n\
         try { state.name = 'x'; } catch (e) { blocked++; }\n\
         db = db.getSiblingDB('kept'); print(blocked); 1",
        &FakePort::ok(),
    );
    assert_ok(&reply);
    assert_eq!(reply.output, vec!["3"], "every attempt to alter the state was refused");
    assert_eq!(reply.database_after, "kept");
    assert!(started.elapsed() < Duration::from_secs(10));
}
