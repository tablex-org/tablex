//! Host -> MongoDB Shell plugin (a child process) -> real MongoDB driver
//! (`dbengine-mongo`, ABI v2) -> a live server.
//!
//! Opt-in (`#[ignore]`): it needs a PRIVATE mongod, the built driver and the
//! built plugin. Start a throwaway server on a port nothing else uses:
//!
//! ```text
//! mongod --dbpath <tmp dir> --port 27999 --bind_ip 127.0.0.1      # macOS rejects --fork
//! cargo build -p dbengine-mongo --features abi-v2 && cargo build -p tablex-mongo-shell
//! TABLEX_TEST_MONGO_URI=mongodb://127.0.0.1:27999 \
//! TABLEX_TEST_MONGO_DYLIB=<target>/debug/libdbengine_mongo.dylib \
//! TABLEX_TEST_SHELL_EXE=<target>/debug/tablex-mongo-shell \
//!   cargo test -p engine-driver-host --test mongo_shell_live -- --include-ignored --test-threads=1
//! ```
//!
//! The `auth_enabled_*` tests also need a mongod started with `--auth` on a fresh
//! dbpath (same port) and `TABLEX_TEST_MONGO_AUTH=1`; they skip without it, and
//! the others run against a mongod without `--auth`.
use engine_driver_api::mongo_shell::{CursorRef, FailureKind, ShellReply, ShellRequest, ShellResult};
use engine_driver_host::{DriverLoader, DriverSession, MongoShell, ShellHostError};
use std::path::PathBuf;
use std::time::{Duration, Instant};
use tempfile::TempDir;

struct Live {
    uri: String,
    session: DriverSession,
    shell: MongoShell,
    database: String,
    _temp: TempDir,
}

fn live(name: &str) -> Option<Live> {
    let uri = std::env::var("TABLEX_TEST_MONGO_URI").ok()?;
    connect(name, &uri)
}

/// The auth-enabled variant: `TABLEX_TEST_MONGO_AUTH=1` says the private mongod
/// on 27999 was started with `--auth` (see the module docs). The test creates its
/// own root user through the localhost exception, then connects with it.
const AUTH_USER: &str = "tablexroot";
const AUTH_PASSWORD: &str = "private-test-password";

fn live_with_auth(name: &str) -> Option<Live> {
    std::env::var("TABLEX_TEST_MONGO_AUTH").ok()?;
    let uri = std::env::var("TABLEX_TEST_MONGO_URI").ok()?;
    let anonymous = connect(&format!("{name}_setup"), &uri)?;
    let runner = anonymous.session.query_runner().expect("runner");
    let request = format!(
        r#"{{"operation_id":{},"database":"admin","command":{{"createUser":"{AUTH_USER}","pwd":"{AUTH_PASSWORD}","roles":[{{"role":"root","db":"admin"}}]}}}}"#,
        runner.operation_id().0
    );
    // Ok the first time (localhost exception); afterwards the user exists and this is refused.
    let _ = runner.mongo_command(&request);
    drop(runner);
    let unauthenticated = anonymous.run("db.getCollectionNames()");
    assert!(unauthenticated.error.is_some(), "this server must have auth enabled (--auth)");
    drop(anonymous);
    let authenticated = uri.replacen("mongodb://", &format!("mongodb://{AUTH_USER}:{AUTH_PASSWORD}@"), 1) + "/?authSource=admin";
    let live = connect(name, &authenticated)?;
    let denied = live.run("db.runCommand({ ping: 1 }).ok");
    assert!(denied.error.is_none(), "the authenticated session must work: {:?}", denied.error);
    Some(live)
}

fn connect(name: &str, uri: &str) -> Option<Live> {
    let dylib = PathBuf::from(std::env::var("TABLEX_TEST_MONGO_DYLIB").ok()?);
    let exe = PathBuf::from(std::env::var("TABLEX_TEST_SHELL_EXE").ok()?);
    assert!(
        uri.contains("27999") && !uri.contains("27017"),
        "the live shell tests only run against the private mongod on port 27999, not {uri}"
    );
    let temp = TempDir::new().unwrap();
    let staged = temp.path().join(dylib.file_name().unwrap());
    std::fs::copy(&dylib, &staged).unwrap();
    let handle = DriverLoader.load(&staged).expect("load the mongo driver");
    let session = handle
        .connect(&serde_json::json!({ "uri": uri, "name": "live" }).to_string())
        .expect("connect");
    Some(Live {
        uri: uri.to_string(),
        session,
        shell: MongoShell::new(exe),
        database: format!("tablex_shell_{name}_{}", std::process::id()),
        _temp: temp,
    })
}

impl Live {
    fn request(&self, script: &str, allow_writes: bool) -> ShellRequest {
        ShellRequest {
            script: script.into(),
            database: self.database.clone(),
            allow_writes,
            page_size: 50,
            timeout_ms: Some(60_000),
            cursor: None,
        }
    }

    fn run(&self, script: &str) -> ShellReply {
        self.run_request(self.request(script, true))
    }

    fn run_read_only(&self, script: &str) -> ShellReply {
        self.run_request(self.request(script, false))
    }

    fn run_request(&self, request: ShellRequest) -> ShellReply {
        self.shell.run(&self.session, request).expect("start").wait().expect("run")
    }

    fn ok(&self, script: &str) -> ShellReply {
        let reply = self.run(script);
        assert!(reply.error.is_none(), "{script}: {:?}", reply.error);
        reply
    }

    /// The text a script prints with `print`.
    fn printed(&self, script: &str) -> String {
        self.ok(script).output.join("\n")
    }

    /// A second connection to the same server: raw commands on one connection
    /// run one at a time, so watching a busy connection needs another one.
    fn other_connection(&self) -> Live {
        let mut other = connect("monitor", &self.uri).expect("second connection");
        other.database = self.database.clone();
        other
    }

    fn drop_database(&self) {
        let _ = self.run("db.dropDatabase()");
    }
}

fn documents(reply: &ShellReply) -> Vec<serde_json::Value> {
    match &reply.result {
        ShellResult::Documents { documents } | ShellResult::Cursor { documents, .. } => {
            documents.iter().map(|d| serde_json::from_str(d.get()).unwrap()).collect()
        }
        other => panic!("not documents: {other:?} / {:?}", reply.error),
    }
}

#[test]
#[ignore = "needs a private mongod (see the module docs)"]
fn find_pages_through_cursor_continuations_across_runs() {
    let Some(live) = live("paging") else { return };
    live.ok("var docs = []; for (var i = 0; i < 120; i++) docs.push({ _id: i, n: NumberLong(i), tag: 'row' + i }); db.items.insertMany(docs);");
    let first = live.run("db.items.find().sort({ _id: 1 })");
    assert!(first.error.is_none(), "{:?}", first.error);
    let ShellResult::Cursor { documents: page_one, cursor } = &first.result else { panic!("{:?}", first.result) };
    assert_eq!(page_one.len(), 50);
    let cursor: CursorRef = cursor.clone().expect("the server has more");
    assert_eq!((cursor.database.as_str(), cursor.collection.as_str()), (live.database.as_str(), "items"));

    // "More": a separate run (a separate child process) continues the same server cursor.
    let mut more = live.request("it", true);
    more.cursor = Some(cursor);
    let second = live.run_request(more);
    assert!(second.error.is_none(), "{:?}", second.error);
    let ShellResult::Cursor { documents: page_two, cursor } = &second.result else { panic!() };
    assert_eq!(page_two.len(), 50);
    let first_id = |text: &str| serde_json::from_str::<serde_json::Value>(text).unwrap()["_id"]["$numberInt"].clone();
    assert_eq!(first_id(page_two[0].get()), "50");
    let mut third = live.request("it", true);
    third.cursor = cursor.clone();
    let third = live.run_request(third);
    let ShellResult::Cursor { documents: page_three, cursor } = &third.result else { panic!() };
    assert_eq!(page_three.len(), 20);
    assert!(cursor.is_none(), "exhausted");
    assert_eq!(first_id(page_three[0].get()), "100");
    // Int64 survives as Int64 in the page.
    let last: serde_json::Value = serde_json::from_str(page_three[19].get()).unwrap();
    assert_eq!(last["n"]["$numberLong"], "119");
    live.drop_database();
}

#[test]
#[ignore = "needs a private mongod (see the module docs)"]
fn aggregate_for_each_map_to_array_and_counts() {
    let Some(live) = live("reads") else { return };
    live.ok("db.sales.insertMany([{ k: 'a', v: 1 }, { k: 'a', v: 2 }, { k: 'b', v: 10 }, { k: 'b', v: 20 }, { k: 'c', v: 5 }]);");
    let grouped = live.ok("db.sales.aggregate([{ $group: { _id: '$k', total: { $sum: '$v' } } }, { $sort: { _id: 1 } }])");
    let totals: Vec<(String, i64)> = documents(&grouped)
        .iter()
        .map(|d| (d["_id"].as_str().unwrap().to_string(), d["total"]["$numberInt"].as_str().unwrap().parse().unwrap()))
        .collect();
    assert_eq!(totals, vec![("a".into(), 3), ("b".into(), 30), ("c".into(), 5)]);
    let printed = live.printed(
        "var seen = []; db.sales.find().sort({ v: 1 }).forEach(function (d) { seen.push(d.v); });\n\
         var doubled = db.sales.find({ k: 'b' }).sort({ v: 1 }).map(function (d) { return d.v * 2; }).toArray();\n\
         print(seen.join(','), doubled.join(','), db.sales.countDocuments({ k: 'a' }), db.sales.estimatedDocumentCount(), db.sales.distinct('k').sort().join(''), db.sales.find({ v: { $gt: 4 } }).count(), db.sales.find().itcount());",
    );
    assert_eq!(printed, "1,2,5,10,20 20,40 2 5 abc 3 5");
    let one = live.ok("db.sales.findOne({ k: 'c' })");
    assert_eq!(documents(&one)[0]["v"]["$numberInt"], "5");
    let plan = live.ok("db.sales.find({ k: 'a' }).explain('queryPlanner')");
    assert!(documents(&plan)[0].get("queryPlanner").is_some());
    live.drop_database();
}

#[test]
#[ignore = "needs a private mongod (see the module docs)"]
fn writes_work_with_allow_writes_and_are_refused_without_it() {
    let Some(live) = live("writes") else { return };
    live.ok("db.t.insertOne({ _id: 1, v: 1, state: 'new' });");
    live.ok("db.t.insertMany([{ _id: 2, v: 2 }, { _id: 3, v: 3 }, { _id: 4, v: 4 }]);");
    live.ok("db.t.updateOne({ _id: 1 }, { $set: { state: 'seen' } }); db.t.updateMany({ v: { $gte: 3 } }, { $inc: { v: 100 } }); db.t.replaceOne({ _id: 2 }, { v: 22 });");
    assert_eq!(live.printed("print(db.t.findOne({ _id: 1 }).state, db.t.findOne({ _id: 3 }).v, db.t.findOne({ _id: 2 }).v)"), "seen 103 22");
    live.ok("var old = db.t.findOneAndUpdate({ _id: 1 }, { $set: { state: 'again' } }, { returnDocument: 'after' }); db.t.findOneAndReplace({ _id: 4 }, { v: 44 }); db.t.findOneAndDelete({ _id: 3 });");
    assert_eq!(live.printed("print(db.t.countDocuments({}), db.t.findOne({ _id: 1 }).state, db.t.findOne({ _id: 4 }).v)"), "3 again 44");
    let bulk = live.ok("var r = db.t.bulkWrite([{ insertOne: { document: { _id: 10, v: 10 } } }, { updateOne: { filter: { _id: 10 }, update: { $set: { v: 11 } } } }, { deleteOne: { filter: { _id: 2 } } }]); print(r.insertedCount, r.modifiedCount, r.deletedCount)");
    assert_eq!(bulk.output, vec!["1 1 1"]);
    live.ok("db.t.createIndex({ v: 1 }, { name: 'v_idx' }); db.t.deleteMany({ v: { $lt: 0 } });");
    assert_eq!(live.printed("print(db.t.getIndexes().map(function (i) { return i.name; }).sort().join())"), "_id_,v_idx");
    live.ok("db.t.dropIndex('v_idx')");

    let before = live.printed("print(db.t.countDocuments({}), JSON.stringify(db.t.find().sort({ _id: 1 }).toArray()))");
    for script in [
        "db.t.insertOne({ _id: 99 })",
        "db.t.updateMany({}, { $set: { hacked: true } })",
        "db.t.deleteMany({})",
        "db.t.drop()",
        "db.dropDatabase()",
        "db.t.createIndex({ x: 1 })",
        "db.runCommand({ delete: 't', deletes: [{ q: {}, limit: 0 }] })",
        "db.t.aggregate([{ $out: 'stolen' }])",
        "db.t.renameCollection('t2')",
        "db.runCommand({ profile: -1, slowms: 1 })",
    ] {
        let reply = live.run_read_only(script);
        assert_eq!(reply.error.as_ref().map(|e| e.kind), Some(FailureKind::ReadOnly), "{script}: {:?}", reply.error);
    }
    let after = live.run_read_only("print(db.t.countDocuments({}), JSON.stringify(db.t.find().sort({ _id: 1 }).toArray()))");
    assert!(after.error.is_none());
    assert_eq!(after.output.join("\n"), before, "nothing changed");
    assert_eq!(live.printed("print(db.getCollectionNames().join())"), "t");
    live.drop_database();
}

#[test]
#[ignore = "needs a private mongod (see the module docs)"]
fn the_drivers_own_read_only_allowlist_refuses_before_the_server_when_called_directly() {
    let Some(live) = live("driver_guard") else { return };
    live.ok("db.t.insertOne({ _id: 1 })");
    let runner = live.session.query_runner().expect("runner");
    let call = |command: &str, read_only: bool| {
        let request = format!(
            r#"{{"operation_id":{},"database":"{}","command":{command},"read_only":{read_only}}}"#,
            runner.operation_id().0,
            live.database
        );
        runner.mongo_command(&request).expect("the symbol answers")
    };
    for refused in [
        r#"{"insert":"t","documents":[{"_id":2}]}"#,
        r#"{"delete":"t","deletes":[{"q":{},"limit":0}]}"#,
        r#"{"drop":"t"}"#,
        r#"{"aggregate":"t","pipeline":[{"$out":"x"}],"cursor":{}}"#,
        r#"{"explain":{"update":"t","updates":[]}}"#,
        r#"{"profile":{"$numberInt":"-1"},"slowms":{"$numberInt":"1"}}"#,
        r#"{"features":{"$numberInt":"1"},"oidReset":true}"#,
    ] {
        let envelope: serde_json::Value = serde_json::from_str(&call(refused, true)).unwrap();
        assert_eq!(envelope["error"]["code"], "query.read_only_violation", "{refused}: {envelope}");
    }
    let count: serde_json::Value = serde_json::from_str(&call(r#"{"count":"t"}"#, true)).unwrap();
    assert_eq!(count["ok"], true, "{count}");
    assert_eq!(count["value"]["reply"]["n"]["$numberInt"], "1", "nothing was inserted or deleted: {count}");
    // A server-side refusal is a reply with ok: 0, like on the wire.
    let missing: serde_json::Value = serde_json::from_str(&call(r#"{"find":"t","filter":{"$badOperator":1}}"#, true)).unwrap();
    assert_eq!(missing["ok"], true);
    assert_eq!(missing["value"]["reply"]["ok"]["$numberDouble"], "0.0", "{missing}");
    live.drop_database();
}

#[test]
#[ignore = "needs a private mongod (see the module docs)"]
fn use_show_and_run_command() {
    let Some(live) = live("use_show") else { return };
    live.ok("db.first.insertOne({ a: 1 }); db.second.insertOne({ a: 1 });");
    let reply = live.ok(&format!("use {}\nshow collections", live.database));
    let names: Vec<String> = documents(&reply).iter().map(|d| d["name"].as_str().unwrap().to_string()).collect();
    assert_eq!(names, vec!["first", "second"]);
    assert_eq!(reply.database_after, live.database);
    let dbs = live.ok("show dbs");
    assert!(documents(&dbs).iter().any(|d| d["name"] == live.database.as_str()));
    let switched = live.ok("use other_db_for_shell_test\ndb.getName()");
    assert_eq!(switched.database_after, "other_db_for_shell_test");
    let ping = live.printed("print(db.runCommand({ ping: 1 }).ok, db.adminCommand({ ping: 1 }).ok, db.version().split('.').length >= 2, Object.keys(db.runCommand({ buildInfo: 1 })).length > 3)");
    assert_eq!(ping, "1 1 true true");
    let read_only_ping = live.run_read_only("db.runCommand({ ping: 1 }).ok");
    assert!(read_only_ping.error.is_none());
    let refused = live.run_read_only("db.runCommand({ drop: 'first' })");
    assert_eq!(refused.error.unwrap().kind, FailureKind::ReadOnly);
    assert_eq!(live.printed("print(db.getCollectionNames().join())"), "first,second");
    let server_error = live.run("db.runCommand({ definitelyNotACommand: 1 })");
    let error = server_error.error.unwrap();
    assert_eq!((error.kind, error.code, error.code_name.as_deref()), (FailureKind::Server, Some(59), Some("CommandNotFound")));
    live.drop_database();
}

#[test]
#[ignore = "needs a private mongod (see the module docs)"]
fn bson_values_round_trip_through_the_server_with_their_types() {
    let Some(live) = live("bson") else { return };
    live.ok(
        "db.types.insertOne({ _id: 1, i32: 5, dbl: 1.5, i64: NumberLong('9007199254740993'), i64max: NumberLong('9223372036854775807'), i64min: NumberLong('-9223372036854775808'),\n\
           dec: NumberDecimal('123.4500'), old: ISODate('1901-02-03T04:05:06.007Z'), now: ISODate('2024-09-01T23:27:09Z'), oid: ObjectId('66d4f84d0000000000000003'),\n\
           uuid: UUID('01234567-89ab-cdef-0123-456789abcdef'), bin: BinData(0, 'AQID'), binUser: BinData(130, 'AQID'), ts: Timestamp(5, 6), min: MinKey(), max: MaxKey(),\n\
           re: /^a.c/i, code: Code('return 1'), ref: DBRef('other', ObjectId('66d4f84d0000000000000003')), nested: { a: [1, { b: NumberLong(7) }] }, nul: null, str: 'h\\u00e9llo' });",
    );
    let types = live.ok("db.types.aggregate([{ $project: { _id: 0, i32: { $type: '$i32' }, dbl: { $type: '$dbl' }, i64: { $type: '$i64' }, dec: { $type: '$dec' }, old: { $type: '$old' }, oid: { $type: '$oid' }, uuid: { $type: '$uuid' }, ts: { $type: '$ts' }, min: { $type: '$min' }, max: { $type: '$max' }, re: { $type: '$re' }, nul: { $type: '$nul' }, nestedLong: { $type: '$nested.a.1.b' } } }])");
    let doc = &documents(&types)[0];
    for (field, expected) in [
        ("i32", "int"), ("dbl", "double"), ("i64", "long"), ("dec", "decimal"), ("old", "date"), ("oid", "objectId"),
        ("uuid", "binData"), ("ts", "timestamp"), ("min", "minKey"), ("max", "maxKey"), ("re", "regex"), ("nul", "null"),
    ] {
        assert_eq!(doc[field], expected, "{field}");
    }
    // Read back, write elsewhere, and compare the stored documents exactly.
    live.ok("var d = db.types.findOne({ _id: 1 }); d._id = 2; db.types.insertOne(d);");
    let same = live.printed(
        "var a = db.types.findOne({ _id: 1 }); var b = db.types.findOne({ _id: 2 }); b._id = 1;\n\
         print(EJSON.stringify(a, { relaxed: false }) === EJSON.stringify(b, { relaxed: false }), a.i64.toString(), a.i64max.toString(), a.i64min.toString(), a.old.toISOString(), a.dec.toString(), a.uuid.toString(), a.ts.t);",
    );
    assert_eq!(
        same,
        "true 9007199254740993 9223372036854775807 -9223372036854775808 1901-02-03T04:05:06.007Z 123.4500 01234567-89ab-cdef-0123-456789abcdef 5"
    );
    let copy_types = live.ok("db.types.aggregate([{ $match: { _id: 2 } }, { $project: { _id: 0, i64: { $type: '$i64' }, dec: { $type: '$dec' }, old: { $type: '$old' }, uuid: { $type: '$uuid' } } }])");
    assert_eq!(documents(&copy_types)[0]["i64"], "long");
    assert_eq!(documents(&copy_types)[0]["dec"], "decimal");
    live.drop_database();
}

#[test]
#[ignore = "needs a private mongod (see the module docs)"]
fn a_cancelled_infinite_loop_and_a_cancelled_sleep_stop_promptly() {
    let Some(live) = live("cancel_js") else { return };
    for script in ["while (true) {}", "print('start'); sleep(600000)"] {
        let run = live.shell.run(&live.session, live.request(script, true)).unwrap();
        let pid = run.process_id().unwrap();
        std::thread::sleep(Duration::from_millis(400));
        assert!(!run.is_finished(), "{script}");
        let started = Instant::now();
        run.cancel();
        assert!(matches!(run.wait(), Err(ShellHostError::Cancelled)));
        assert!(started.elapsed() < Duration::from_secs(5), "{script}");
        #[cfg(unix)]
        assert!(
            !std::process::Command::new("kill").args(["-0", &pid.to_string()]).stderr(std::process::Stdio::null()).status().unwrap().success(),
            "the plugin process is gone"
        );
        let _ = pid;
    }
    // The connection is fine afterwards.
    assert_eq!(live.printed("print(db.runCommand({ ping: 1 }).ok)"), "1");
}

#[test]
#[ignore = "needs a private mongod (see the module docs)"]
fn a_cancelled_long_server_operation_is_killed_on_the_server() {
    let Some(live) = live("cancel_server") else { return };
    live.ok("db.slow.insertMany([{ a: 1 }, { a: 2 }, { a: 3 }])");
    let monitor = live.other_connection();
    let run = live
        .shell
        .run(&live.session, live.request("db.slow.find({ $where: 'sleep(30000) || true' }).toArray()", true))
        .unwrap();
    wait_until("the slow find to start", || running_slow_finds(&monitor) == "1");
    let started = Instant::now();
    run.cancel();
    assert!(matches!(run.wait(), Err(ShellHostError::Cancelled)));
    assert!(started.elapsed() < Duration::from_secs(5), "{:?}", started.elapsed());
    wait_until("the server to stop the cancelled find", || running_slow_finds(&monitor) == "0");
    live.drop_database();
}

#[test]
#[ignore = "needs a private mongod (see the module docs)"]
fn the_time_limit_ends_a_script_waiting_on_the_server() {
    let Some(live) = live("timeout") else { return };
    live.ok("db.slow.insertOne({ a: 1 })");
    let monitor = live.other_connection();
    let mut request = live.request("print('begin'); db.slow.find({ $where: 'sleep(30000) || true' }).toArray()", true);
    request.timeout_ms = Some(1000);
    let started = Instant::now();
    let reply = live.run_request(request);
    assert_eq!(reply.error.as_ref().map(|e| e.kind), Some(FailureKind::Timeout), "{:?}", reply.error);
    assert_eq!(reply.output, vec!["begin"]);
    assert!(started.elapsed() < Duration::from_secs(10));
    wait_until("the server to stop the timed-out find", || running_slow_finds(&monitor) == "0");
    live.drop_database();
}

// ---- cursors, types and kills ---------------------------------------------------

fn idle_cursors(live: &Live, collection: &str) -> String {
    live.printed(&format!(
        "print(db.getSiblingDB('admin').aggregate([{{ $currentOp: {{ allUsers: true, idleCursors: true }} }}, {{ $match: {{ type: 'idleCursor', ns: '{}.{collection}' }} }}]).toArray().length)",
        live.database
    ))
}

fn wait_until(what: &str, mut done: impl FnMut() -> bool) {
    let deadline = Instant::now() + Duration::from_secs(15);
    while !done() {
        assert!(Instant::now() < deadline, "timed out waiting for {what}");
        std::thread::sleep(Duration::from_millis(200));
    }
}

/// `monitor` is a second connection (see [`Live::other_connection`]).
fn running_slow_finds(monitor: &Live) -> String {
    monitor.printed(&format!(
        "print(db.getSiblingDB('admin').aggregate([{{ $currentOp: {{ allUsers: true }} }}, {{ $match: {{ 'command.find': 'slow', ns: '{}.slow' }} }}]).toArray().length)",
        monitor.database
    ))
}

/// What the plugin does with the cursors it opens, against whatever server mode
/// `live` is connected to.
fn cursor_scenarios(live: &Live) {
    live.ok("var docs = []; for (var i = 0; i < 30; i++) docs.push({ _id: i }); db.items.insertMany(docs);");
    assert_eq!(idle_cursors(live, "items"), "0");

    // A script that opens a cursor and ends: the shell closes it on the server.
    live.ok("var c = db.items.find().batchSize(2); c.next(); 'done'");
    assert_eq!(idle_cursors(live, "items"), "0", "a cursor left open by a script is closed");
    live.ok("var c = db.items.find().batchSize(2); c.next(); c.close();");
    assert_eq!(idle_cursors(live, "items"), "0", "close() kills the server cursor");
    let failed = live.run("var c = db.items.find().batchSize(2); c.next(); throw new Error('boom')");
    assert!(failed.error.is_some());
    assert_eq!(idle_cursors(live, "items"), "0", "a failing script leaves no cursor");

    // The result cursor stays open for "More", then ends with it.
    let mut small_page = live.request("db.items.find().sort({ _id: 1 })", true);
    small_page.page_size = 10;
    let first = live.run_request(small_page);
    let ShellResult::Cursor { cursor, .. } = &first.result else { panic!("{:?}", first.result) };
    assert_eq!(idle_cursors(live, "items"), "1", "the page's cursor is the host's to continue");
    let mut next = cursor.clone();
    let mut pages = 1;
    while let Some(reference) = next {
        let mut more = live.request("it", true);
        more.cursor = Some(reference);
        let rest = live.run_request(more);
        let ShellResult::Cursor { cursor, .. } = &rest.result else { panic!("{:?}", rest.result) };
        next = cursor.clone();
        pages += 1;
    }
    assert_eq!(pages, 3, "30 documents in pages of 10");
    assert_eq!(idle_cursors(live, "items"), "0");

    // A stopped run: the host closes what the script had open.
    let monitor = live.other_connection();
    let run = live
        .shell
        .run(&live.session, live.request("var c = db.items.find().batchSize(2); c.next(); while (true) {}", true))
        .unwrap();
    wait_until("the cursor to open", || idle_cursors(&monitor, "items") == "1");
    run.cancel();
    assert!(matches!(run.wait(), Err(ShellHostError::Cancelled)));
    wait_until("the host to close the stopped run's cursor", || idle_cursors(&monitor, "items") == "0");
}

/// Raw commands on one connection share one session and run one at a time:
/// stopping a run must only ever stop that run's own command.
fn session_sharing_scenarios(live: &Live) {
    live.ok("db.slow.insertOne({ a: 1 })");
    let monitor = live.other_connection();
    let finite = "db.slow.find({ $where: 'sleep(2500) || true' }).toArray().length";
    let forever = "db.slow.find({ $where: 'sleep(30000) || true' }).toArray().length";

    // Stop a run that is queued behind a running one: the running one completes.
    let running = live.shell.run(&live.session, live.request(finite, true)).unwrap();
    wait_until("the first slow find to start", || running_slow_finds(&monitor) == "1");
    let queued = live.shell.run(&live.session, live.request(finite, true)).unwrap();
    std::thread::sleep(Duration::from_millis(300));
    queued.cancel();
    assert!(matches!(queued.wait(), Err(ShellHostError::Cancelled)));
    let reply = running.wait().expect("the running run was not stopped");
    assert!(reply.error.is_none(), "stopping the queued run killed the running one: {:?}", reply.error);
    assert!(matches!(&reply.result, ShellResult::Value { text } if text == "1"), "{:?}", reply.result);

    // Stop the running run while another is queued behind it: the next one is not killed.
    let first = live.shell.run(&live.session, live.request(forever, true)).unwrap();
    wait_until("the long slow find to start", || running_slow_finds(&monitor) == "1");
    let next = live.shell.run(&live.session, live.request(finite, true)).unwrap();
    std::thread::sleep(Duration::from_millis(300));
    first.cancel();
    assert!(matches!(first.wait(), Err(ShellHostError::Cancelled)));
    let reply = next.wait().expect("the next run");
    assert!(reply.error.is_none(), "stopping the first run killed the next one: {:?}", reply.error);
    assert!(matches!(&reply.result, ShellResult::Value { text } if text == "1"), "{:?}", reply.result);
    wait_until("the stopped find to be gone", || running_slow_finds(&monitor) == "0");
}

#[test]
#[ignore = "needs a private mongod (see the module docs)"]
fn stopping_a_run_never_stops_another_run_on_the_same_connection() {
    let Some(live) = live("shared_session") else { return };
    session_sharing_scenarios(&live);
    live.drop_database();
}

#[test]
#[ignore = "needs a private mongod (see the module docs)"]
fn cursors_are_closed_on_the_server_by_scripts_and_by_stop() {
    let Some(live) = live("cursors") else { return };
    cursor_scenarios(&live);
    live.drop_database();
}

#[test]
#[ignore = "needs a private mongod started with --auth (see the module docs)"]
fn auth_enabled_cursors_writes_guard_and_cancel() {
    let Some(live) = live_with_auth("auth") else { return };
    cursor_scenarios(&live);
    session_sharing_scenarios(&live);
    // Writes, the read-only guard and paging work on an authenticated session too.
    live.ok("db.t.insertMany([{ _id: 1, v: -1 }, { _id: 2, v: 2 }]); db.t.updateOne({ _id: 2 }, { $inc: { v: -5 } });");
    assert_eq!(live.printed("print(db.t.findOne({ _id: 2 }).v, db.t.countDocuments({}))"), "-3 2");
    let refused = live.run_read_only("db.t.deleteMany({})");
    assert_eq!(refused.error.unwrap().kind, FailureKind::ReadOnly);
    assert_eq!(live.printed("print(db.t.countDocuments({}))"), "2");
    // A server operation under auth is killed by Stop (found through the session, not a comment).
    live.ok("db.slow.insertOne({ a: 1 })");
    let run = live.shell.run(&live.session, live.request("db.slow.find({ $where: 'sleep(30000) || true' }).toArray()", true)).unwrap();
    let monitor = live.other_connection();
    wait_until("the slow find to start", || running_slow_finds(&monitor) == "1");
    run.cancel();
    assert!(matches!(run.wait(), Err(ShellHostError::Cancelled)));
    wait_until("the server to stop the slow find", || running_slow_finds(&monitor) == "0");
    live.drop_database();
}

#[test]
#[ignore = "needs a private mongod (see the module docs)"]
fn stop_kills_the_server_operation_even_when_the_script_set_its_own_comment() {
    let Some(live) = live("own_comment") else { return };
    live.ok("db.slow.insertOne({ a: 1 })");
    let run = live
        .shell
        .run(&live.session, live.request("db.slow.find({ $where: 'sleep(30000) || true' }).comment('my own tag').toArray()", true))
        .unwrap();
    let monitor = live.other_connection();
    wait_until("the slow find to start", || running_slow_finds(&monitor) == "1");
    run.cancel();
    assert!(matches!(run.wait(), Err(ShellHostError::Cancelled)));
    wait_until("the server to stop the commented find", || running_slow_finds(&monitor) == "0");
    live.drop_database();
}

#[test]
#[ignore = "needs a private mongod (see the module docs)"]
fn negative_integers_longs_and_doubles_keep_their_types_on_the_server() {
    let Some(live) = live("numbers") else { return };
    // A collection whose validator only takes real Int32s in `a`: it rejects what used to arrive as Int64.
    live.ok("db.createCollection('typed', { validator: { $jsonSchema: { bsonType: 'object', properties: { a: { bsonType: 'int' }, n: { bsonType: 'int' } } } } });");
    live.ok("db.typed.insertOne({ _id: -5, a: -1, n: 10 }); db.typed.updateOne({ _id: -5 }, { $inc: { n: -1, a: -3 } });");
    live.ok("db.nums.insertOne({ _id: 1, i32: -1, i32min: -2147483648, i64: NumberLong(-1), i64min: NumberLong('-9223372036854775808'), dbl: -1.5, dblWhole: Double(-2), big: -3000000000, arr: [-1, NumberLong(-2), -0.5], nested: { x: -7 } });");
    let rows = live.ok("db.typed.aggregate([{ $project: { _id: 0, id: { $type: '$_id' }, a: { $type: '$a' }, n: { $type: '$n' }, aValue: '$a', nValue: '$n' } }])");
    let row = &documents(&rows)[0];
    assert_eq!((row["id"].as_str(), row["a"].as_str(), row["n"].as_str()), (Some("int"), Some("int"), Some("int")), "{row}");
    assert_eq!((row["aValue"]["$numberInt"].as_str(), row["nValue"]["$numberInt"].as_str()), (Some("-4"), Some("9")));
    let types = live.ok("db.nums.aggregate([{ $project: { _id: 0, i32: { $type: '$i32' }, i32min: { $type: '$i32min' }, i64: { $type: '$i64' }, i64min: { $type: '$i64min' }, dbl: { $type: '$dbl' }, dblWhole: { $type: '$dblWhole' }, big: { $type: '$big' }, arr0: { $type: { $arrayElemAt: ['$arr', 0] } }, arr1: { $type: { $arrayElemAt: ['$arr', 1] } }, arr2: { $type: { $arrayElemAt: ['$arr', 2] } }, nested: { $type: '$nested.x' } } }])");
    let doc = &documents(&types)[0];
    for (field, expected) in [
        ("i32", "int"), ("i32min", "int"), ("i64", "long"), ("i64min", "long"), ("dbl", "double"), ("dblWhole", "double"),
        ("big", "double"), ("arr0", "int"), ("arr1", "long"), ("arr2", "double"), ("nested", "int"),
    ] {
        assert_eq!(doc[field], expected, "{field}");
    }
    live.drop_database();
}


#[test]
#[ignore = "needs a private mongod (see the module docs)"]
fn the_runs_time_limit_reaches_the_server_as_max_time_ms() {
    let Some(live) = live("max_time") else { return };
    live.ok("db.slow.insertOne({ a: 1 })");
    let monitor = live.other_connection();
    let mut request = live.request("db.slow.find({ $where: 'sleep(30000) || true' }).toArray()", true);
    request.timeout_ms = Some(20_000);
    let run = live.shell.run(&live.session, request).unwrap();
    let with_limit = format!(
        "print(db.getSiblingDB('admin').aggregate([{{ $currentOp: {{ allUsers: true }} }}, {{ $match: {{ 'command.find': 'slow', ns: '{}.slow', 'command.maxTimeMS': {{ $gt: 1000, $lte: 22000 }} }} }}]).toArray().length)",
        live.database
    );
    wait_until("a slow find carrying the run's remaining time as maxTimeMS", || monitor.printed(&with_limit) == "1");
    run.cancel();
    assert!(matches!(run.wait(), Err(ShellHostError::Cancelled)));
    wait_until("the server to stop the find", || running_slow_finds(&monitor) == "0");
    live.drop_database();
}
