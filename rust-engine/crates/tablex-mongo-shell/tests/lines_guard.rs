mod common;
use bson::doc;
use common::*;
use engine_driver_api::mongo_shell::{FailureKind, ShellResult};

#[test]
fn use_switches_the_database_for_what_follows_and_is_reported_after() {
    let port = FakePort::new(|_, _| Ok(first_batch("shop.items", 0, vec![])));
    let reply = run("use shop\ndb.items.find().hasNext()\nprint(db.getName())\nuse 'other-db'", &port);
    assert_ok(&reply);
    assert_eq!(port.seen()[0].0, "shop");
    assert_eq!(reply.output, vec!["switched to db shop", "shop", "switched to db other-db"]);
    assert_eq!(reply.database_after, "other-db");
}

#[test]
fn a_reassigned_db_counts_as_the_database_after() {
    let reply = run("db = db.getSiblingDB('reassigned')", &FakePort::ok());
    assert_ok(&reply);
    assert_eq!(reply.database_after, "reassigned");
    let failed = run("use second\nthrow new Error('x')", &FakePort::ok());
    assert!(failed.error.is_some());
    assert_eq!(failed.database_after, "second", "the database is reported even when the run failed");
}

#[test]
fn show_commands_as_the_last_statement_return_documents_elsewhere_they_print() {
    let port = FakePort::new(|_, command| match command_name(command) {
        "listDatabases" => Ok(doc! { "databases": [
            { "name": "zeta", "sizeOnDisk": 2048_i64, "empty": false },
            { "name": "admin", "sizeOnDisk": 40960_i64, "empty": false },
        ], "ok": 1.0 }),
        "listCollections" => Ok(first_batch("app.$cmd.listCollections", 0, vec![
            doc! { "name": "orders", "type": "collection" },
            doc! { "name": "users", "type": "collection" },
        ])),
        "usersInfo" => Ok(doc! { "users": [{ "user": "bob" }], "ok": 1.0 }),
        "rolesInfo" => Ok(doc! { "roles": [{ "role": "r1" }], "ok": 1.0 }),
        _ => Ok(doc! { "ok": 1.0 }),
    });
    let last = run("show dbs", &port);
    assert_eq!(result_documents(&last), vec![
        doc! { "name": "admin", "sizeOnDisk": 40960_i64, "empty": false },
        doc! { "name": "zeta", "sizeOnDisk": 2048_i64, "empty": false },
    ]);
    assert!(last.output.is_empty());
    let middle = run("show databases\nshow collections\n1", &port);
    assert_eq!(middle.output.len(), 2);
    assert!(middle.output[0].starts_with("admin") && middle.output[0].contains("40.00 KiB"), "{:?}", middle.output);
    assert!(middle.output[0].contains("zeta"));
    assert_eq!(middle.output[1], "orders\nusers");
    assert_eq!(value_text(&middle), "1");
    let collections = run("show tables", &port);
    assert_eq!(result_documents(&collections)[1], doc! { "name": "users", "type": "collection" });
    assert_eq!(result_documents(&run("show users", &port)), vec![doc! { "user": "bob" }]);
    assert_eq!(result_documents(&run("show roles", &port)), vec![doc! { "role": "r1" }]);
    let unknown = run("show nonsense", &port);
    assert_eq!(failure_kind(&unknown), FailureKind::Script);
    assert!(unknown.error.unwrap().message.contains("Unknown shell command"));
    assert_eq!(port.seen().iter().find(|(_, c)| command_name(c) == "listDatabases").unwrap().0, "admin");
}

#[test]
fn line_command_words_inside_strings_and_comments_are_just_text() {
    let port = FakePort::ok();
    let reply = run("var s = `\nuse other\nshow dbs\n`;\n/*\nit\n*/\nprint(s.trim().split('\\n').length, db.getName())", &port);
    assert_ok(&reply);
    assert_eq!(reply.output, vec!["2 app"]);
    assert!(port.seen().is_empty());
}

const WRITE_SCRIPTS: &[&str] = &[
    "db.items.insertOne({ a: 1 })",
    "db.items.insertMany([{ a: 1 }])",
    "db.items.insert({ a: 1 })",
    "db.items.updateOne({}, { $set: { a: 1 } })",
    "db.items.updateMany({}, { $set: { a: 1 } })",
    "db.items.update({}, { a: 1 })",
    "db.items.replaceOne({}, { a: 1 })",
    "db.items.deleteOne({})",
    "db.items.deleteMany({})",
    "db.items.remove({})",
    "db.items.findOneAndUpdate({}, { $set: { a: 1 } })",
    "db.items.findOneAndReplace({}, { a: 1 })",
    "db.items.findOneAndDelete({})",
    "db.items.bulkWrite([{ insertOne: { document: { a: 1 } } }])",
    "db.items.createIndex({ a: 1 })",
    "db.items.createIndexes([{ a: 1 }])",
    "db.items.dropIndex('a_1')",
    "db.items.dropIndexes()",
    "db.items.drop()",
    "db.items.renameCollection('x')",
    "db.dropDatabase()",
    "db.createCollection('x')",
    "db.createView('v', 'items', [])",
    "db.killOp(1)",
    "db.runCommand({ insert: 'items', documents: [{}] })",
    "db.runCommand({ delete: 'items', deletes: [] })",
    "db.runCommand({ drop: 'items' })",
    "db.runCommand({ createIndexes: 'items', indexes: [] })",
    "db.runCommand({ findAndModify: 'items', remove: true })",
    "db.runCommand({ collMod: 'items' })",
    "db.adminCommand({ shutdown: 1 })",
    "db.adminCommand({ renameCollection: 'a.b', to: 'a.c' })",
    "db.adminCommand({ dropDatabase: 1 })",
    "db.items.aggregate([{ $match: {} }, { $out: 'copy' }])",
    "db.items.aggregate([{ $merge: { into: 'copy' } }]).toArray()",
    "db.runCommand({ aggregate: 'items', pipeline: [{ $out: 'x' }], cursor: {} })",
    "db.runCommand({ explain: { delete: 'items', deletes: [] } })",
    "db.getSiblingDB('other').things.insertOne({})",
    "db['items'].deleteMany({})",
    "db.getCollection('items').drop()",
    "var c = db.items; c.insertOne({})",
];

#[test]
fn read_only_runs_refuse_every_write_before_it_reaches_the_server() {
    for script in WRITE_SCRIPTS {
        let port = FakePort::ok();
        let mut request = request(script);
        request.allow_writes = false;
        let reply = run_with(&request, &port);
        assert_eq!(failure_kind(&reply), FailureKind::ReadOnly, "{script}: {:?}", reply.error);
        assert!(reply.error.unwrap().message.contains("read-only"), "{script}");
        assert!(port.seen().is_empty(), "{script} reached the server: {:?}", port.seen());
    }
}

#[test]
fn read_only_runs_still_read() {
    let port = FakePort::new(|_, command| match command_name(command) {
        "find" | "aggregate" | "listCollections" | "listIndexes" => Ok(first_batch("app.items", 0, numbered(1))),
        "count" => Ok(doc! { "n": 1, "ok": 1.0 }),
        "buildInfo" => Ok(doc! { "version": "7.0", "ok": 1.0 }),
        _ => Ok(doc! { "ok": 1.0 }),
    });
    let mut request = request(
        "db.items.find().toArray(); db.items.aggregate([{ $match: {} }]).toArray(); db.items.countDocuments({}); db.items.getIndexes();\n\
         db.getCollectionNames(); db.version(); db.runCommand({ ping: 1 }); db.adminCommand({ listDatabases: 1 });\n\
         db.items.explain().find({}).toArray(); db.items.distinct('a'); 'ok'",
    );
    request.allow_writes = false;
    let reply = run_with(&request, &port);
    assert_ok(&reply);
    assert_eq!(value_text(&reply), "\"ok\"");
    assert!(port.names().len() >= 9);
}

#[test]
fn a_caught_refusal_is_still_a_refusal_and_nothing_is_sent() {
    let port = FakePort::ok();
    let mut request = request("try { db.items.insertOne({}) } catch (e) { print(e.kind, e.name) }; db.items.deleteMany({})");
    request.allow_writes = false;
    let reply = run_with(&request, &port);
    assert_eq!(reply.output, vec!["read_only ShellError"]);
    assert_eq!(failure_kind(&reply), FailureKind::ReadOnly);
    assert!(port.seen().is_empty());
}

#[test]
fn a_driver_refusal_is_a_read_only_error_too() {
    let port = FakePort::new(|_, _| {
        Err(tablex_mongo_shell::CommandError::Refused {
            code: "query.read_only_violation".into(),
            message: "driver says no".into(),
        })
    });
    let reply = run("db.runCommand({ ping: 1 })", &port);
    assert_eq!(failure_kind(&reply), FailureKind::ReadOnly);
    let other = FakePort::new(|_, _| {
        Err(tablex_mongo_shell::CommandError::Refused { code: "query.bad_request".into(), message: "bad".into() })
    });
    assert_eq!(failure_kind(&run("db.runCommand({ ping: 1 })", &other)), FailureKind::Server);
}

#[test]
fn unsupported_surface_fails_clearly() {
    for script in [
        "load('file.js')",
        "require('fs')",
        "var m = new Mongo('host')",
        "connect('host/db')",
        "exit",
        "exit()",
        "quit()",
        "process.exit(1)",
        "process.env.HOME",
        "fs.readFileSync('/etc/passwd')",
        "require('child_process').exec('ls')",
        "cat('/etc/passwd')",
        "run('ls')",
        "fetch('http://example.com')",
        "setTimeout(function () {}, 10)",
        "hostname()",
    ] {
        let port = FakePort::ok();
        let reply = run(script, &port);
        assert_eq!(failure_kind(&reply), FailureKind::Unsupported, "{script}: {:?}", reply.error);
        assert!(reply.error.unwrap().message.contains("not available"), "{script}");
        assert!(port.seen().is_empty());
    }
}

#[test]
fn no_script_can_reach_the_host_object_or_internals() {
    let port = FakePort::ok();
    let reply = run(
        "print(typeof host, typeof globalThis.host, typeof __host, typeof execute, typeof serialize, typeof STATE, typeof std);",
        &port,
    );
    assert_ok(&reply);
    assert_eq!(reply.output, vec!["undefined undefined undefined undefined undefined undefined undefined"]);
}

#[test]
fn top_level_await_works_and_the_result_is_the_awaited_value() {
    let port = FakePort::new(|_, _| Ok(first_batch("app.items", 0, numbered(2))));
    let reply = run("const docs = await db.items.find().toArray();\ndocs.length", &port);
    assert_ok(&reply);
    let direct = run("await db.items.find().toArray()", &port);
    assert_eq!(result_documents(&direct).len(), 2);
    assert!(matches!(direct.result, ShellResult::Documents { .. }));
    let words = run("var awaiting = 1; var x = { await_it: 2 }; awaiting", &port);
    assert_eq!(value_text(&words), "1");
}

#[test]
fn syntax_and_runtime_errors_carry_line_and_column() {
    let port = FakePort::ok();
    let syntax = run("var a = 1;\nvar b = ;\n", &port);
    let error = syntax.error.unwrap();
    assert_eq!(error.kind, FailureKind::Syntax);
    assert_eq!((error.line, error.column), (Some(2), Some(9)));
    let reference = run("print(1);\n  missing.thing", &port);
    let error = reference.error.unwrap();
    assert_eq!(error.kind, FailureKind::Script);
    assert_eq!((error.line, error.column), (Some(2), Some(3)));
    assert_eq!(reference.output, vec!["1"], "what was printed before the error is kept");
    let inside = run("function boom() {\n  throw new TypeError('inner');\n}\nboom();", &port);
    let error = inside.error.unwrap();
    assert_eq!((error.line, error.message.as_str()), (Some(2), "inner"));
    let thrown = run("throw 'plain string'", &port);
    assert!(thrown.error.unwrap().message.contains("plain string"));
    let in_helper = run("\n\nObjectId('zzz')", &port);
    assert_eq!(in_helper.error.unwrap().line, Some(3), "the position is the user's line, not the shell's");
}

#[test]
fn a_bad_request_is_reported_not_run() {
    let mut request = request("1");
    request.database = " ".into();
    let reply = run_with(&request, &FakePort::ok());
    assert_eq!(failure_kind(&reply), FailureKind::BadRequest);
}
