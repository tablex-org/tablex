mod common;
use bson::{doc, Bson};
use common::*;
use engine_driver_api::mongo_shell::{CursorRef, ShellResult};

fn cursor_port(total: i32, id: i64) -> FakePort {
    FakePort::new(move |_, command| match command_name(command) {
        "find" => Ok(first_batch("app.items", id, numbered(total.min(50)))),
        "getMore" => Ok(next_batch("app.items", 0, numbered(5))),
        _ => Ok(doc! { "ok": 1.0 }),
    })
}

#[test]
fn find_returns_a_first_page_sized_by_page_size_and_a_cursor_to_continue() {
    let port = cursor_port(50, 4242);
    let mut request = request("db.items.find({ n: { $gte: 1 } }, { n: 1 }).sort({ n: -1 }).skip(2).limit(500)");
    request.page_size = 50;
    let reply = run_with(&request, &port);
    assert_ok(&reply);
    let (database, find) = port.seen().remove(0);
    assert_eq!(database, "app");
    assert_eq!(command_name(&find), "find");
    assert_eq!(find.get_str("find"), Ok("items"));
    assert_eq!(find.get_document("filter").unwrap(), &doc! { "n": { "$gte": 1 } });
    assert_eq!(find.get_document("projection").unwrap(), &doc! { "n": 1 });
    assert_eq!(find.get_document("sort").unwrap(), &doc! { "n": -1 });
    assert_eq!(find.get_i32("skip"), Ok(2));
    assert_eq!(find.get_i32("limit"), Ok(500));
    assert_eq!(find.get_i32("batchSize"), Ok(50), "the first batch is the first page");
    match &reply.result {
        ShellResult::Cursor { documents, cursor } => {
            assert_eq!(documents.len(), 50);
            assert_eq!(
                cursor,
                &Some(CursorRef { id: "4242".into(), database: "app".into(), collection: "items".into(), batch_size: 50 })
            );
        }
        other => panic!("{other:?}"),
    }
    assert_eq!(port.names(), vec!["find"], "the page's cursor is not killed");
}

#[test]
fn an_exhausted_cursor_has_no_continuation() {
    let port = cursor_port(3, 0);
    let reply = run("db.items.find()", &port);
    match &reply.result {
        ShellResult::Cursor { documents, cursor } => {
            assert_eq!(documents.len(), 3);
            assert!(cursor.is_none());
        }
        other => panic!("{other:?}"),
    }
}

#[test]
fn it_continues_the_cursor_the_previous_run_left() {
    let port = cursor_port(50, 4242);
    let mut request = request("it");
    request.cursor = Some(CursorRef { id: "4242".into(), database: "app".into(), collection: "items".into(), batch_size: 20 });
    let reply = run_with(&request, &port);
    assert_ok(&reply);
    let (database, get_more) = port.seen().remove(0);
    assert_eq!(database, "app");
    assert_eq!(get_more.get_i64("getMore"), Ok(4242));
    assert_eq!(get_more.get_str("collection"), Ok("items"));
    assert_eq!(get_more.get_i32("batchSize"), Ok(20));
    match &reply.result {
        ShellResult::Cursor { documents, cursor } => {
            assert_eq!(documents.len(), 5);
            assert!(cursor.is_none(), "the server closed it");
        }
        other => panic!("{other:?}"),
    }
    let none = run("it", &FakePort::ok());
    assert_eq!(none.output, vec!["no cursor"]);
}

#[test]
fn it_in_the_middle_prints_the_documents() {
    let port = cursor_port(50, 4242);
    let mut request = request("it\n1 + 1");
    request.cursor = Some(CursorRef { id: "4242".into(), database: "app".into(), collection: "items".into(), batch_size: 20 });
    let reply = run_with(&request, &port);
    assert_ok(&reply);
    assert_eq!(reply.output.len(), 5);
    assert_eq!(value_text(&reply), "2");
}

#[test]
fn iteration_pages_through_get_more_and_stops_at_the_end() {
    let port = FakePort::new(|_, command| match command_name(command) {
        "find" => Ok(first_batch("app.items", 9, numbered(2))),
        "getMore" => Ok(next_batch("app.items", 0, vec![doc! { "_id": 2, "n": 2 }])),
        _ => Ok(doc! { "ok": 1.0 }),
    });
    let reply = run(
        "var seen = []; var c = db.items.find().batchSize(2);\n\
         while (c.hasNext()) { seen.push(c.next().n); }\n\
         var after = c.tryNext(); var left = c.objsLeftInBatch();\n\
         print(JSON.stringify(seen), after, left, c.isClosed());",
        &port,
    );
    assert_ok(&reply);
    assert_eq!(reply.output, vec!["[0,1,2] null 0 true"]);
    assert_eq!(port.names(), vec!["find", "getMore"]);
    let get_more = &port.seen()[1].1;
    assert_eq!(get_more.get_i32("batchSize"), Ok(2));
}

#[test]
fn for_each_map_to_array_and_next_work_with_typed_values() {
    let port = FakePort::new(|_, command| match command_name(command) {
        "find" => Ok(first_batch("app.items", 0, vec![
            doc! { "_id": 1, "big": 9_007_199_254_740_993_i64, "price": 2.5 },
            doc! { "_id": 2, "big": 5_i64, "price": 3.5 },
        ])),
        _ => Ok(doc! { "ok": 1.0 }),
    });
    let reply = run(
        "var ids = []; db.items.find().forEach(function (d) { ids.push(d._id); });\n\
         var prices = db.items.find().map(function (d) { return d.price * 2; }).toArray();\n\
         var first = db.items.find().next();\n\
         print(ids.join(','), prices.join(','), first.big.toString(), typeof first.big, first.big instanceof Long);",
        &port,
    );
    assert_ok(&reply);
    assert_eq!(reply.output, vec!["1,2 5,7 9007199254740993 object true"]);
}

#[test]
fn a_mapped_cursor_returns_the_mapped_documents_as_the_page() {
    let port = FakePort::new(|_, _| Ok(first_batch("app.items", 0, numbered(2))));
    let reply = run("db.items.find().map(function (d) { return { doubled: d.n * 2 }; })", &port);
    let docs = result_documents(&reply);
    assert_eq!(docs, vec![doc! { "doubled": 0 }, doc! { "doubled": 2 }]);
}

#[test]
fn find_one_asks_for_a_single_batch_and_returns_the_document_or_null() {
    let port = FakePort::new(|_, command| match command_name(command) {
        "find" if command.get_document("filter").unwrap().contains_key("missing") => {
            Ok(first_batch("app.items", 0, vec![]))
        }
        "find" => Ok(first_batch("app.items", 0, vec![doc! { "_id": 7 }])),
        _ => Ok(doc! { "ok": 1.0 }),
    });
    let reply = run("print(JSON.stringify(db.items.findOne({ a: 1 })), db.items.findOne({ missing: 1 }))", &port);
    assert_eq!(reply.output, vec![r#"{"_id":7} null"#]);
    let find = &port.seen()[0].1;
    assert_eq!(find.get_i32("limit"), Ok(1));
    assert_eq!(find.get_bool("singleBatch"), Ok(true));
    let one = run("db.items.findOne({ a: 1 })", &port);
    assert_eq!(result_documents(&one), vec![doc! { "_id": 7 }]);
}

#[test]
fn cursor_count_size_explain_and_modifiers_become_commands() {
    let port = FakePort::new(|_, command| match command_name(command) {
        "count" => Ok(doc! { "n": 12, "ok": 1.0 }),
        "explain" => Ok(doc! { "queryPlanner": { "winningPlan": "COLLSCAN" }, "ok": 1.0 }),
        _ => Ok(first_batch("app.items", 0, vec![])),
    });
    let reply = run(
        "var c = db.items.find({ a: 1 }).skip(3).limit(4).hint({ a: 1 }).collation({ locale: 'en' }).comment('why').maxTimeMS(500).allowDiskUse();\n\
         print(c.count(), c.size(), JSON.stringify(c.explain('executionStats').queryPlanner));\n\
         c.toArray();",
        &port,
    );
    assert_ok(&reply);
    assert_eq!(reply.output, vec![r#"12 12 {"winningPlan":"COLLSCAN"}"#]);
    let seen = port.seen();
    let count = &seen[0].1;
    assert_eq!(count.get_document("query").unwrap(), &doc! { "a": 1 });
    assert!(!count.contains_key("skip"), "count() ignores skip / limit");
    let size = &seen[1].1;
    assert_eq!((size.get_i32("skip"), size.get_i32("limit")), (Ok(3), Ok(4)));
    let explain = &seen[2].1;
    assert_eq!(explain.get_str("verbosity"), Ok("executionStats"));
    assert_eq!(explain.get_document("explain").unwrap().get_str("find"), Ok("items"));
    let find = &seen[3].1;
    assert_eq!(find.get_document("hint").unwrap(), &doc! { "a": 1 });
    assert_eq!(find.get_str("comment"), Ok("why"));
    assert_eq!(find.get_i32("maxTimeMS"), Ok(500));
    assert_eq!(find.get_bool("allowDiskUse"), Ok(true));
    assert_eq!(find.get_document("collation").unwrap(), &doc! { "locale": "en" });
}

#[test]
fn modifiers_after_the_cursor_started_are_an_error() {
    let port = FakePort::new(|_, _| Ok(first_batch("app.items", 0, numbered(1))));
    let reply = run("var c = db.items.find(); c.next(); c.limit(1)", &port);
    assert_eq!(failure_kind(&reply), engine_driver_api::mongo_shell::FailureKind::Script);
}

#[test]
fn aggregate_runs_a_pipeline_with_options_and_an_out_stage_runs_eagerly() {
    let port = FakePort::new(|_, _| Ok(first_batch("app.items", 0, vec![doc! { "total": 3 }])));
    let reply = run(
        "db.items.aggregate([{ $match: { a: 1 } }, { $group: { _id: null, total: { $sum: 1 } } }], { allowDiskUse: true, maxTimeMS: 100 })",
        &port,
    );
    assert_ok(&reply);
    let aggregate = &port.seen()[0].1;
    assert_eq!(aggregate.get_str("aggregate"), Ok("items"));
    assert_eq!(aggregate.get_array("pipeline").unwrap().len(), 2);
    assert_eq!(aggregate.get_bool("allowDiskUse"), Ok(true));
    assert!(aggregate.get_document("cursor").is_ok());
    assert_eq!(result_documents(&reply), vec![doc! { "total": 3 }]);

    let eager = FakePort::new(|_, _| Ok(first_batch("app.items", 0, vec![])));
    let reply = run("db.items.aggregate([{ $match: {} }, { $out: 'copy' }]); print('done')", &eager);
    assert_ok(&reply);
    assert_eq!(eager.names(), vec!["aggregate"], "an $out pipeline runs without being iterated");

    let explained = FakePort::new(|_, _| Ok(doc! { "stages": [], "ok": 1.0 }));
    let reply = run("db.items.aggregate([], { explain: true })", &explained);
    assert_ok(&reply);
    let command = &explained.seen()[0].1;
    assert_eq!(command.get_bool("explain"), Ok(true));
    assert!(!command.contains_key("cursor"));
}

#[test]
fn counts_distinct_and_stats() {
    let port = FakePort::new(|_, command| match command_name(command) {
        "aggregate" => Ok(first_batch("app.items", 0, vec![doc! { "_id": 1, "n": 7_i64 }])),
        "count" => Ok(doc! { "n": 41, "ok": 1.0 }),
        "distinct" => Ok(doc! { "values": ["a", "b"], "ok": 1.0 }),
        "collStats" => Ok(doc! { "size": 10, "storageSize": 20, "totalIndexSize": 5, "ok": 1.0 }),
        _ => Ok(doc! { "ok": 1.0 }),
    });
    let reply = run(
        "print(db.items.countDocuments({ a: 1 }, { limit: 5 }), db.items.estimatedDocumentCount(), db.items.count({ a: 2 }),\n\
         db.items.distinct('k', { a: 1 }).join(), db.items.dataSize(), db.items.totalSize());",
        &port,
    );
    assert_ok(&reply);
    assert_eq!(reply.output, vec!["7 41 41 a,b 10 25"]);
    let seen = port.seen();
    let pipeline = seen[0].1.get_array("pipeline").unwrap();
    assert_eq!(pipeline[0].as_document().unwrap(), &doc! { "$match": { "a": 1 } });
    assert_eq!(pipeline[1].as_document().unwrap(), &doc! { "$limit": 5 });
    assert_eq!(seen[3].1.get_str("key"), Ok("k"));
}

#[test]
fn explainable_find_and_aggregate() {
    let port = FakePort::new(|_, _| Ok(doc! { "queryPlanner": { "namespace": "app.items" }, "ok": 1.0 }));
    let reply = run("db.items.explain('allPlansExecution').find({ a: 1 }).sort({ a: 1 })", &port);
    assert_ok(&reply);
    let explain = &port.seen()[0].1;
    assert_eq!(explain.get_str("verbosity"), Ok("allPlansExecution"));
    assert_eq!(explain.get_document("explain").unwrap().get_document("sort").unwrap(), &doc! { "a": 1 });
    assert!(result_documents(&reply)[0].contains_key("queryPlanner"));
    let aggregate = run("db.items.explain().aggregate([{ $match: {} }])", &port);
    assert_ok(&aggregate);
}

#[test]
fn collection_access_forms_all_name_the_same_collection() {
    let port = FakePort::new(|_, _| Ok(first_batch("app.x", 0, vec![])));
    let reply = run(
        "db.users.find().hasNext(); db['odd-name'].find().hasNext(); db.getCollection('a.b').find().hasNext(); db.logs.archive.find().hasNext(); db.getSiblingDB('other').things.find().hasNext();\n\
         print(db.users.getName(), db.users.getFullName(), db.logs.archive.getName(), String(db.users), db.getName(), db.getSiblingDB('o').getName());",
        &port,
    );
    assert_ok(&reply);
    let seen = port.seen();
    let targets: Vec<(String, String)> = seen
        .iter()
        .map(|(db, command)| (db.clone(), command.get_str("find").unwrap().to_string()))
        .collect();
    assert_eq!(
        targets,
        vec![
            ("app".into(), "users".into()),
            ("app".into(), "odd-name".into()),
            ("app".into(), "a.b".into()),
            ("app".into(), "logs.archive".into()),
            ("other".into(), "things".into()),
        ]
    );
    assert_eq!(reply.output, vec!["users app.users logs.archive app.users app o"]);
}

#[test]
fn db_methods() {
    let port = FakePort::new(|_, command| match command_name(command) {
        "listCollections" => Ok(first_batch("app.$cmd.listCollections", 0, vec![
            doc! { "name": "zeta", "type": "collection" },
            doc! { "name": "alpha", "type": "view" },
        ])),
        "buildInfo" => Ok(doc! { "version": "7.0.4", "ok": 1.0 }),
        "dbStats" => Ok(doc! { "db": "app", "collections": 2, "ok": 1.0 }),
        "serverStatus" => Ok(doc! { "host": "h", "ok": 1.0 }),
        "currentOp" => Ok(doc! { "inprog": [], "ok": 1.0 }),
        "ping" => Ok(doc! { "ok": 1.0 }),
        _ => Ok(doc! { "ok": 1.0 }),
    });
    let reply = run(
        "print(db.getCollectionNames().join(), db.getCollectionInfos({ name: 'alpha' }).length, db.version(), db.stats().collections);\n\
         print(db.serverStatus().host, db.currentOp().inprog.length, db.runCommand({ ping: 1 }).ok, db.runCommand('ping').ok, db.adminCommand({ ping: 1 }).ok);\n\
         db.createCollection('c', { capped: true, size: 100 });\n\
         db.createView('v', 'c', [{ $match: {} }]);\n\
         db.killOp(5);\n\
         db.dropDatabase();\n\
         db.listCollections()",
        &port,
    );
    assert_ok(&reply);
    assert_eq!(reply.output[0], "alpha,zeta 2 7.0.4 2");
    assert_eq!(reply.output[1], "h 0 1 1 1");
    let seen = port.seen();
    let by_name = |name: &str| seen.iter().find(|(_, c)| command_name(c) == name).unwrap().clone();
    assert_eq!(by_name("buildInfo").0, "admin");
    assert_eq!(by_name("currentOp").0, "admin");
    let create = by_name("create").1;
    assert_eq!((create.get_str("create"), create.get_bool("capped")), (Ok("c"), Ok(true)));
    let view = seen.iter().filter(|(_, c)| command_name(c) == "create").nth(1).unwrap().1.clone();
    assert_eq!(view.get_str("viewOn"), Ok("c"));
    assert_eq!(by_name("killOp").1.get_i32("op"), Ok(5));
    assert_eq!(by_name("dropDatabase").0, "app");
    assert!(matches!(&reply.result, ShellResult::Cursor { documents, .. } if documents.len() == 2));
}

#[test]
fn run_command_returns_the_reply_and_throws_on_server_errors() {
    let port = FakePort::new(|_, command| match command_name(command) {
        "boom" => Ok(doc! { "ok": 0.0, "errmsg": "no such command: 'boom'", "code": 59, "codeName": "CommandNotFound" }),
        _ => Ok(doc! { "ok": 1.0, "n": Bson::Int64(3) }),
    });
    let good = run("db.runCommand({ anything: 1 })", &port);
    assert_ok(&good);
    assert_eq!(result_documents(&good)[0].get_i64("n"), Ok(3));
    let bad = run("\n db.runCommand({ boom: 1 })", &port);
    let error = bad.error.expect("server error");
    assert_eq!(error.kind, engine_driver_api::mongo_shell::FailureKind::Server);
    assert_eq!((error.code, error.code_name.as_deref()), (Some(59), Some("CommandNotFound")));
    assert!(error.message.contains("no such command"));
    assert_eq!(error.line, Some(2));
}

#[test]
fn write_errors_in_an_ok_reply_are_server_errors_with_their_code() {
    let port = FakePort::new(|_, _| Ok(doc! {
        "n": 0, "ok": 1.0,
        "writeErrors": [{ "index": 0, "code": 11000, "errmsg": "E11000 duplicate key error collection: app.items" }],
    }));
    let reply = run("db.items.insertOne({ _id: 1 })", &port);
    let error = reply.error.expect("duplicate key");
    assert_eq!(error.code, Some(11000));
    assert!(error.message.starts_with("E11000"));
    assert_eq!(error.kind, engine_driver_api::mongo_shell::FailureKind::Server);
}

#[test]
fn caught_errors_do_not_fail_the_run() {
    let port = FakePort::new(|_, _| Ok(doc! { "ok": 0.0, "errmsg": "nope", "code": 2, "codeName": "BadValue" }));
    let reply = run("try { db.runCommand({ x: 1 }); } catch (e) { print(e.name, e.code, e.codeName, e.message); }", &port);
    assert_ok(&reply);
    assert_eq!(reply.output, vec!["MongoServerError 2 BadValue nope"]);
}

#[test]
fn abandoned_cursors_are_killed_but_the_result_cursor_stays_open() {
    let port = FakePort::new(|_, command| match command_name(command) {
        "find" => Ok(first_batch("app.items", 77, numbered(2))),
        _ => Ok(doc! { "ok": 1.0 }),
    });
    let reply = run("var c = db.items.find().batchSize(2); c.next(); 'done'", &port);
    assert_ok(&reply);
    assert_eq!(port.names(), vec!["find", "killCursors"]);
    let kill = &port.seen()[1].1;
    assert_eq!(kill.get_str("killCursors"), Ok("items"));
    assert_eq!(kill.get_array("cursors").unwrap(), &vec![Bson::Int64(77)]);
}

#[test]
fn toarray_refuses_a_result_over_the_cap_with_a_clear_error() {
    let port = FakePort::new(move |_, command| match command_name(command) {
        "find" => Ok(first_batch("app.items", 5, vec![doc! { "a": 1 }; 40_000])),
        "getMore" => Ok(next_batch("app.items", 5, vec![doc! { "a": 1 }; 40_000])),
        _ => Ok(doc! { "ok": 1.0 }),
    });
    let reply = run("db.items.find().toArray().length", &port);
    assert_eq!(failure_kind(&reply), engine_driver_api::mongo_shell::FailureKind::Limit);
    assert!(reply.error.unwrap().message.contains("toArray()"));
}

#[test]
fn the_page_keeps_the_servers_key_order_even_for_integer_like_keys() {
    let mut odd = bson::Document::new();
    odd.insert("b", 1);
    odd.insert("10", 2);
    odd.insert("2", 3);
    odd.insert("a", doc! { "z": 1, "5": 2, "1": 3 });
    let sent = odd.clone();
    let port = FakePort::new(move |_, _| Ok(first_batch("app.items", 0, vec![sent.clone()])));
    let page = run("db.items.find()", &port);
    let docs = result_documents(&page);
    assert_eq!(docs[0].keys().collect::<Vec<_>>(), vec!["b", "10", "2", "a"]);
    assert_eq!(docs[0].get_document("a").unwrap().keys().collect::<Vec<_>>(), vec!["z", "5", "1"]);
    // After a getMore too (the text of the later batch is what is handed over).
    let sent = odd.clone();
    let port = FakePort::new(move |_, command| match command_name(command) {
        "getMore" => Ok(next_batch("app.items", 0, vec![sent.clone()])),
        _ => Ok(first_batch("app.items", 5, vec![])),
    });
    let mut request = request("it");
    request.cursor = Some(CursorRef { id: "5".into(), database: "app".into(), collection: "items".into(), batch_size: 10 });
    let more = run_with(&request, &port);
    assert_eq!(result_documents(&more)[0].keys().collect::<Vec<_>>(), vec!["b", "10", "2", "a"]);
}

#[test]
fn no_cursor_timeout_is_refused_and_a_failing_script_leaves_no_cursor_open() {
    let port = FakePort::new(|_, command| match command_name(command) {
        "find" => Ok(first_batch("app.items", 77, numbered(2))),
        _ => Ok(doc! { "ok": 1.0 }),
    });
    for script in ["db.items.find().noCursorTimeout()", "db.items.find({}, {}, { noCursorTimeout: true })"] {
        let reply = run(script, &port);
        assert_eq!(failure_kind(&reply), engine_driver_api::mongo_shell::FailureKind::Unsupported, "{script}");
    }
    assert!(port.seen().is_empty());
    let reply = run("var c = db.items.find().batchSize(2); c.next(); throw new Error('boom')", &port);
    assert_eq!(failure_kind(&reply), engine_driver_api::mongo_shell::FailureKind::Script);
    assert_eq!(port.names(), vec!["find", "killCursors"], "the cursor the failed script left is closed");
}
