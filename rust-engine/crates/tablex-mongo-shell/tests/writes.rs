mod common;
use bson::{doc, Bson};
use common::*;
use engine_driver_api::mongo_shell::FailureKind;

fn write_port() -> FakePort {
    FakePort::new(|_, command| match command_name(command) {
        "insert" => Ok(doc! { "n": command.get_array("documents").unwrap().len() as i32, "ok": 1.0 }),
        "update" => Ok(doc! { "n": 3, "nModified": 2, "ok": 1.0 }),
        "delete" => Ok(doc! { "n": 4, "ok": 1.0 }),
        "findAndModify" => Ok(doc! { "value": { "_id": 1, "state": "old" }, "ok": 1.0 }),
        "createIndexes" => Ok(doc! { "ok": 1.0 }),
        _ => Ok(doc! { "ok": 1.0 }),
    })
}

#[test]
fn insert_one_adds_an_object_id_and_reports_it() {
    let port = write_port();
    let reply = run("var r = db.items.insertOne({ a: 1 }); print(r.acknowledged, String(r.insertedId).length, r.insertedId instanceof ObjectId); r", &port);
    assert_ok(&reply);
    assert_eq!(reply.output, vec!["true 24 true"]);
    let insert = &port.seen()[0].1;
    assert_eq!(insert.get_str("insert"), Ok("items"));
    let docs = insert.get_array("documents").unwrap();
    let stored = docs[0].as_document().unwrap();
    assert!(matches!(stored.get("_id"), Some(Bson::ObjectId(_))));
    assert_eq!(stored.keys().next().map(String::as_str), Some("_id"), "_id first, as the server stores it");
    assert!(value_text(&reply).contains("\"acknowledged\": true"), "{}", value_text(&reply));
    let given = run("db.items.insertOne({ _id: 'mine', a: 1 })", &port);
    assert!(value_text(&given).contains("\"insertedId\": \"mine\""), "{}", value_text(&given));
}

#[test]
fn insert_many_splits_big_batches_and_numbers_the_ids() {
    let port = write_port();
    let reply = run("var docs = []; for (var i = 0; i < 2500; i++) docs.push({ _id: i }); var r = db.items.insertMany(docs, { ordered: false }); print(Object.keys(r.insertedIds).length, r.insertedIds[2499]);", &port);
    assert_ok(&reply);
    assert_eq!(reply.output, vec!["2500 2499"]);
    let seen = port.seen();
    assert_eq!(seen.len(), 3, "1000 + 1000 + 500");
    assert!(seen.iter().all(|(_, c)| c.get_bool("ordered") == Ok(false)));
    let legacy = run("db.items.insert([{ a: 1 }, { a: 2 }]); db.items.insert({ a: 3 })", &port);
    assert_ok(&legacy);
}

#[test]
fn updates_validate_operators_and_report_counts() {
    let port = write_port();
    let reply = run(
        "var r = db.items.updateMany({ a: 1 }, { $set: { b: 2 } }, { upsert: true, arrayFilters: [{ 'x.k': 1 }], hint: 'h' });\n\
         print(r.matchedCount, r.modifiedCount, r.upsertedCount, r.insertedId);\n\
         db.items.updateOne({ a: 1 }, [{ $set: { c: 1 } }]);\n\
         db.items.replaceOne({ a: 1 }, { z: 1 });\n\
         db.items.update({ a: 1 }, { z: 2 });\n\
         db.items.update({ a: 1 }, { $set: { z: 2 } }, { multi: true });",
        &port,
    );
    assert_ok(&reply);
    assert_eq!(reply.output, vec!["3 2 0 null"]);
    let seen = port.seen();
    let spec = seen[0].1.get_array("updates").unwrap()[0].as_document().unwrap().clone();
    assert_eq!(spec.get_bool("multi"), Ok(true));
    assert_eq!(spec.get_bool("upsert"), Ok(true));
    assert_eq!(spec.get_str("hint"), Ok("h"));
    assert!(spec.get_array("arrayFilters").is_ok());
    let single = seen[1].1.get_array("updates").unwrap()[0].as_document().unwrap().clone();
    assert_eq!(single.get_bool("multi"), Ok(false));
    assert!(matches!(single.get("u"), Some(Bson::Array(_))));
    assert_eq!(seen[3].1.get_array("updates").unwrap()[0].as_document().unwrap().get_document("u").unwrap(), &doc! { "z": 2 });
    assert_eq!(seen[4].1.get_array("updates").unwrap()[0].as_document().unwrap().get_bool("multi"), Ok(true));

    for (script, fragment) in [
        ("db.items.updateOne({}, { a: 1 })", "atomic operators"),
        ("db.items.updateMany({}, { a: 1 })", "atomic operators"),
        ("db.items.replaceOne({}, { $set: { a: 1 } })", "must not contain atomic operators"),
        ("db.items.findOneAndUpdate({}, { a: 1 })", "atomic operators"),
    ] {
        let reply = run(script, &write_port());
        assert_eq!(failure_kind(&reply), FailureKind::Script, "{script}");
        assert!(reply.error.unwrap().message.contains(fragment), "{script}");
    }
}

#[test]
fn an_upsert_reports_the_inserted_id() {
    let port = FakePort::new(|_, _| Ok(doc! { "n": 1, "nModified": 0, "upserted": [{ "index": 0, "_id": 99 }], "ok": 1.0 }));
    let reply = run("var r = db.items.updateOne({ a: 1 }, { $set: { a: 2 } }, { upsert: true }); print(r.matchedCount, r.upsertedCount, r.insertedId)", &port);
    assert_eq!(reply.output, vec!["0 1 99"]);
}

#[test]
fn deletes_and_the_legacy_remove() {
    let port = write_port();
    let reply = run(
        "print(db.items.deleteOne({ a: 1 }).deletedCount, db.items.deleteMany({}).deletedCount, db.items.remove({ a: 1 }).deletedCount);\n\
         db.items.remove({ a: 1 }, true); db.items.remove({ a: 1 }, { justOne: true });",
        &port,
    );
    assert_ok(&reply);
    assert_eq!(reply.output, vec!["4 4 4"]);
    let limits: Vec<i32> = port
        .seen()
        .iter()
        .map(|(_, c)| c.get_array("deletes").unwrap()[0].as_document().unwrap().get_i32("limit").unwrap())
        .collect();
    assert_eq!(limits, vec![1, 0, 0, 1, 1]);
}

#[test]
fn find_and_modify_family() {
    let port = write_port();
    let reply = run(
        "var before = db.items.findOneAndUpdate({ a: 1 }, { $set: { state: 'new' } }, { sort: { a: 1 }, projection: { state: 1 }, upsert: true });\n\
         db.items.findOneAndUpdate({ a: 1 }, { $inc: { n: 1 } }, { returnDocument: 'after' });\n\
         db.items.findOneAndReplace({ a: 1 }, { b: 1 }, { returnNewDocument: true });\n\
         db.items.findOneAndDelete({ a: 1 });\n\
         print(before.state);",
        &port,
    );
    assert_ok(&reply);
    assert_eq!(reply.output, vec!["old"]);
    let seen = port.seen();
    let first = &seen[0].1;
    assert_eq!(first.get_document("update").unwrap(), &doc! { "$set": { "state": "new" } });
    assert_eq!(first.get_document("sort").unwrap(), &doc! { "a": 1 });
    assert_eq!(first.get_document("fields").unwrap(), &doc! { "state": 1 });
    assert_eq!(first.get_bool("upsert"), Ok(true));
    assert!(!first.contains_key("new"));
    assert_eq!(seen[1].1.get_bool("new"), Ok(true));
    assert_eq!(seen[2].1.get_bool("new"), Ok(true));
    assert_eq!(seen[3].1.get_bool("remove"), Ok(true));
    let bad = run("db.items.findOneAndUpdate({}, { $set: { a: 1 } }, { returnDocument: 'maybe' })", &port);
    assert_eq!(failure_kind(&bad), FailureKind::Script);
}

#[test]
fn a_find_and_modify_that_matches_nothing_gives_null() {
    let port = FakePort::new(|_, _| Ok(doc! { "value": Bson::Null, "ok": 1.0 }));
    let reply = run("print(db.items.findOneAndDelete({ a: 1 }))", &port);
    assert_eq!(reply.output, vec!["null"]);
}

#[test]
fn bulk_write_groups_consecutive_operations_and_keeps_global_indexes() {
    let port = FakePort::new(|_, command| match command_name(command) {
        "insert" => Ok(doc! { "n": 2, "ok": 1.0 }),
        "update" => Ok(doc! { "n": 3, "nModified": 1, "upserted": [{ "index": 1, "_id": "u" }], "ok": 1.0 }),
        "delete" => Ok(doc! { "n": 5, "ok": 1.0 }),
        _ => Ok(doc! { "ok": 1.0 }),
    });
    let reply = run(
        "var r = db.items.bulkWrite([\n\
           { insertOne: { document: { _id: 1 } } },\n\
           { insertOne: { document: { _id: 2 } } },\n\
           { updateOne: { filter: { _id: 1 }, update: { $set: { a: 1 } } } },\n\
           { updateMany: { filter: {}, update: { $inc: { n: 1 } }, upsert: true } },\n\
           { replaceOne: { filter: { _id: 2 }, replacement: { z: 1 } } },\n\
           { deleteMany: { filter: {} } },\n\
           { deleteOne: { filter: { _id: 1 } } }\n\
         ], { ordered: true });\n\
         print(r.insertedCount, r.matchedCount, r.modifiedCount, r.upsertedCount, r.deletedCount, JSON.stringify(r.insertedIds), JSON.stringify(r.upsertedIds));",
        &port,
    );
    assert_ok(&reply);
    assert_eq!(reply.output, vec![r#"2 2 1 1 5 {"0":1,"1":2} {"3":"u"}"#]);
    assert_eq!(port.names(), vec!["insert", "update", "delete"]);
    let seen = port.seen();
    assert_eq!(seen[1].1.get_array("updates").unwrap().len(), 3);
    assert_eq!(seen[2].1.get_array("deletes").unwrap().len(), 2);
    let bad = run("db.items.bulkWrite([{ nonsense: {} }])", &port);
    assert_eq!(failure_kind(&bad), FailureKind::Script);
    let empty = run("db.items.bulkWrite([])", &port);
    assert_eq!(failure_kind(&empty), FailureKind::Script);
}

#[test]
fn indexes_collection_admin_and_rename() {
    let port = FakePort::new(|_, command| match command_name(command) {
        "listIndexes" => Ok(first_batch("app.items.$cmd.listIndexes.items", 0, vec![
            doc! { "v": 2, "key": { "_id": 1 }, "name": "_id_" },
            doc! { "v": 2, "key": { "b": -1, "a": 1 }, "name": "b_-1_a_1" },
        ])),
        "drop" if command.get_str("drop") == Ok("gone") => {
            Ok(doc! { "ok": 0.0, "errmsg": "ns not found", "code": 26, "codeName": "NamespaceNotFound" })
        }
        _ => Ok(doc! { "ok": 1.0 }),
    });
    let reply = run(
        "var name = db.items.createIndex({ a: 1, b: -1 }, { unique: true, sparse: true });\n\
         var names = db.items.createIndexes([{ x: 1 }, { y: 'text' }], { background: false });\n\
         var indexes = db.items.getIndexes();\n\
         db.items.dropIndex('a_1_b_-1'); db.items.dropIndex({ x: 1 }); db.items.dropIndexes(); db.items.dropIndexes(['a', 'b']);\n\
         print(name, names.join(), indexes.length, JSON.stringify(Object.keys(indexes[1].key)), db.items.drop(), db.gone.drop());\n\
         db.items.renameCollection('items2', true);",
        &port,
    );
    assert_ok(&reply);
    assert_eq!(reply.output, vec!["a_1_b_-1 x_1,y_text 2 [\"b\",\"a\"] true false"], "{:?}", reply.output);
    let seen = port.seen();
    let create = &seen[0].1;
    let spec = create.get_array("indexes").unwrap()[0].as_document().unwrap().clone();
    assert_eq!(spec.get_document("key").unwrap(), &doc! { "a": 1, "b": -1 });
    assert_eq!(spec.get_str("name"), Ok("a_1_b_-1"));
    assert_eq!(spec.get_bool("unique"), Ok(true));
    assert_eq!(seen[1].1.get_array("indexes").unwrap().len(), 2);
    let drops: Vec<&Bson> = seen.iter().filter(|(_, c)| command_name(c) == "dropIndexes").map(|(_, c)| c.get("index").unwrap()).collect();
    assert_eq!(drops.len(), 4);
    assert_eq!(drops[2], &Bson::String("*".into()));
    let rename = &seen.last().unwrap();
    assert_eq!(rename.0, "admin");
    assert_eq!(rename.1.get_str("renameCollection"), Ok("app.items"));
    assert_eq!(rename.1.get_str("to"), Ok("app.items2"));
    assert_eq!(rename.1.get_bool("dropTarget"), Ok(true));
}

#[test]
fn stats_and_the_unsupported_collection_methods() {
    let port = FakePort::new(|_, _| Ok(doc! { "ns": "app.items", "count": 3, "ok": 1.0 }));
    let reply = run("db.items.stats({ scale: 1024 }).count", &port);
    assert_eq!(value_text(&reply), "3");
    assert_eq!(port.seen()[0].1.get_i32("scale"), Ok(1024));
    for script in ["db.items.watch()", "db.items.mapReduce()", "db.watch()", "db.getMongo()"] {
        assert_eq!(failure_kind(&run(script, &port)), FailureKind::Unsupported, "{script}");
    }
}

/// A port that keeps the exact command text it was sent.
struct TextPort(std::sync::Mutex<Vec<String>>);

impl tablex_mongo_shell::CommandPort for TextPort {
    fn run_command(&self, _: &str, _: bson::Document) -> Result<bson::Document, tablex_mongo_shell::CommandError> {
        unreachable!("the text path is used")
    }
    fn run_command_text(&self, _: &str, command: &str) -> Result<String, tablex_mongo_shell::CommandError> {
        self.0.lock().unwrap().push(command.to_string());
        Ok(r#"{"n":{"$numberInt":"1"},"nModified":{"$numberInt":"1"},"ok":{"$numberDouble":"1.0"}}"#.to_string())
    }
}

#[test]
fn every_typed_number_is_sent_tagged_so_negative_integers_stay_int32() {
    let port = TextPort(Default::default());
    let reply = tablex_mongo_shell::run_script(
        &request("db.t.insertOne({ _id: -7, a: -1, b: NumberInt(-2), c: NumberLong(-3), d: -1.5, e: 2147483648, f: [-1, 0, 1] });\n\
                  db.t.updateOne({ k: -1 }, { $inc: { n: -1 } });"),
        &port,
    );
    assert!(reply.error.is_none(), "{:?}", reply.error);
    let sent = port.0.lock().unwrap().clone();
    let bare = regex::Regex::new(r"[:\[,]-?[0-9]").unwrap();
    for text in &sent {
        // No bare JSON number is left anywhere in a command: every number is
        // a string inside a `$number...` tag.
        assert!(!bare.is_match(text), "a bare number in {text}");
    }
    assert!(sent[0].contains(r#""a":{"$numberInt":"-1"}"#), "{}", sent[0]);
    assert!(sent[0].contains(r#""c":{"$numberLong":"-3"}"#));
    assert!(sent[0].contains(r#""d":{"$numberDouble":"-1.5"}"#));
    assert!(sent[0].contains(r#""e":{"$numberDouble":"2147483648"}"#));
    assert!(sent[1].contains(r#""$inc":{"n":{"$numberInt":"-1"}}"#), "{}", sent[1]);
    // And both parsers (the guard's and the driver's) agree on what that text means.
    let parsed = tablex_mongo_shell::text::parse_document(&sent[0]).unwrap();
    let document = parsed.get_array("documents").unwrap()[0].as_document().unwrap().clone();
    assert_eq!(document.get("a"), Some(&Bson::Int32(-1)));
    assert_eq!(document.get("b"), Some(&Bson::Int32(-2)));
    assert_eq!(document.get("c"), Some(&Bson::Int64(-3)));
    assert_eq!(document.get("d"), Some(&Bson::Double(-1.5)));
}
