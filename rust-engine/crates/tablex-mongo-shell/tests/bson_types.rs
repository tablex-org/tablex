mod common;
use bson::spec::BinarySubtype;
use bson::{doc, oid::ObjectId, Binary, Bson, DateTime, Decimal128, Document, Timestamp};
use common::*;
use std::str::FromStr;

const ID: &str = "66d4f84d71c18f22d433b003";

fn inserted(script: &str) -> Document {
    let port = FakePort::ok();
    let reply = run(script, &port);
    assert_ok(&reply);
    let seen = port.seen();
    let insert = &seen.iter().find(|(_, c)| command_name(c) == "insert").expect("an insert").1;
    insert.get_array("documents").unwrap()[0].as_document().unwrap().clone()
}

#[test]
fn every_helper_becomes_its_bson_type() {
    let stored = inserted(
        "db.t.insertOne({ _id: 1,\n\
         i32: 5, neg: -2147483648, i32b: NumberInt(7), i32c: Int32('8'), dbl: 1.5, dblWhole: Double(3), big: 3000000000, whole: 4.0,\n\
         nl: NumberLong('9223372036854775807'), nlmin: NumberLong('-9223372036854775808'), nl53: NumberLong('9007199254740993'), nlnum: NumberLong(42), nlneg: Long(-1), nlbig: BigInt('9007199254740995'),\n\
         dec: NumberDecimal('1.50'), dec2: Decimal128('9.99E+6144'), decinf: NumberDecimal('-Infinity'),\n\
         d: ISODate('2020-01-02T03:04:05.678Z'), pre: ISODate('1900-01-01T00:00:00Z'), minus1: new Date(-1), epoch: new Date(0), lit: new Date('2021-05-06T07:08:09Z'), zoned: ISODate('2020-01-01T01:00:00+01:00'), dateonly: ISODate('2020-01-01'),\n\
         oid: ObjectId('66d4f84d71c18f22d433b003'),\n\
         uuid: UUID('01234567-89ab-cdef-0123-456789abcdef'), bin: BinData(0, 'AQID'), bin128: BinData(128, 'AQID'), hexd: HexData(5, '010203'), func: BinData(1, ''),\n\
         ts: Timestamp(10, 20), tsobj: Timestamp({ t: 1, i: 2 }), min: MinKey(), max: MaxKey(),\n\
         re: /ab+c/i, bre: BSONRegExp('x', 'm'), dbref: DBRef('coll', ObjectId('66d4f84d71c18f22d433b003'), 'otherdb'), code: Code('function(){}'), codews: Code('x', { a: 1 }),\n\
         nested: { a: [1, { b: [NumberLong(1), null] }] }, nul: null, str: 'h\\u00e9llo \"q\" \\\\ \\n', bool: true, nan: NaN, inf: Infinity, ninf: -Infinity, negz: -0,\n\
         u: undefined, arr: [undefined, 1] })",
    );
    let id = ObjectId::parse_str(ID).unwrap();
    let expect: Vec<(&str, Bson)> = vec![
        ("_id", Bson::Int32(1)),
        ("i32", Bson::Int32(5)),
        ("neg", Bson::Int32(i32::MIN)),
        ("i32b", Bson::Int32(7)),
        ("i32c", Bson::Int32(8)),
        ("dbl", Bson::Double(1.5)),
        ("dblWhole", Bson::Double(3.0)),
        ("big", Bson::Double(3e9)),
        ("whole", Bson::Int32(4)),
        ("nl", Bson::Int64(i64::MAX)),
        ("nlmin", Bson::Int64(i64::MIN)),
        ("nl53", Bson::Int64(9_007_199_254_740_993)),
        ("nlnum", Bson::Int64(42)),
        ("nlneg", Bson::Int64(-1)),
        ("nlbig", Bson::Int64(9_007_199_254_740_995)),
        ("dec", Bson::Decimal128(Decimal128::from_str("1.50").unwrap())),
        ("dec2", Bson::Decimal128(Decimal128::from_str("9.99E+6144").unwrap())),
        ("decinf", Bson::Decimal128(Decimal128::from_str("-Infinity").unwrap())),
        ("d", Bson::DateTime(DateTime::from_millis(1_577_934_245_678))),
        ("pre", Bson::DateTime(DateTime::from_millis(-2_208_988_800_000))),
        ("minus1", Bson::DateTime(DateTime::from_millis(-1))),
        ("epoch", Bson::DateTime(DateTime::from_millis(0))),
        ("lit", Bson::DateTime(DateTime::from_millis(1_620_284_889_000))),
        ("zoned", Bson::DateTime(DateTime::from_millis(1_577_836_800_000))),
        ("dateonly", Bson::DateTime(DateTime::from_millis(1_577_836_800_000))),
        ("oid", Bson::ObjectId(id)),
        (
            "uuid",
            Bson::Binary(Binary {
                subtype: BinarySubtype::Uuid,
                bytes: vec![0x01, 0x23, 0x45, 0x67, 0x89, 0xab, 0xcd, 0xef, 0x01, 0x23, 0x45, 0x67, 0x89, 0xab, 0xcd, 0xef],
            }),
        ),
        ("bin", Bson::Binary(Binary { subtype: BinarySubtype::Generic, bytes: vec![1, 2, 3] })),
        ("bin128", Bson::Binary(Binary { subtype: BinarySubtype::UserDefined(128), bytes: vec![1, 2, 3] })),
        ("hexd", Bson::Binary(Binary { subtype: BinarySubtype::Md5, bytes: vec![1, 2, 3] })),
        ("func", Bson::Binary(Binary { subtype: BinarySubtype::Function, bytes: vec![] })),
        ("ts", Bson::Timestamp(Timestamp { time: 10, increment: 20 })),
        ("tsobj", Bson::Timestamp(Timestamp { time: 1, increment: 2 })),
        ("min", Bson::MinKey),
        ("max", Bson::MaxKey),
        ("re", Bson::RegularExpression(bson::Regex { pattern: "ab+c".into(), options: "i".into() })),
        ("bre", Bson::RegularExpression(bson::Regex { pattern: "x".into(), options: "m".into() })),
        ("code", Bson::JavaScriptCode("function(){}".into())),
        ("nul", Bson::Null),
        ("bool", Bson::Boolean(true)),
        ("inf", Bson::Double(f64::INFINITY)),
        ("ninf", Bson::Double(f64::NEG_INFINITY)),
        ("str", Bson::String("h\u{e9}llo \"q\" \\ \n".into())),
    ];
    for (key, expected) in expect {
        assert_eq!(stored.get(key), Some(&expected), "field {key}");
    }
    assert!(matches!(stored.get("nan"), Some(Bson::Double(n)) if n.is_nan()));
    assert!(matches!(stored.get("negz"), Some(Bson::Double(n)) if *n == 0.0 && n.is_sign_negative()));
    assert_eq!(
        stored.get("dbref"),
        Some(&Bson::Document(doc! { "$ref": "coll", "$id": id, "$db": "otherdb" }))
    );
    assert_eq!(
        stored.get("codews"),
        Some(&Bson::JavaScriptCodeWithScope(bson::JavaScriptCodeWithScope { code: "x".into(), scope: doc! { "a": 1 } }))
    );
    assert_eq!(
        stored.get("nested"),
        Some(&Bson::Document(doc! { "a": [1, { "b": [Bson::Int64(1), Bson::Null] }] }))
    );
    assert!(!stored.contains_key("u"), "undefined fields are dropped");
    assert_eq!(stored.get("arr"), Some(&Bson::Array(vec![Bson::Null, Bson::Int32(1)])));
}

fn rich() -> Document {
    let id = ObjectId::parse_str(ID).unwrap();
    doc! {
        "_id": id,
        "i32": 5,
        "half": 2.5,
        "i64": Bson::Int64(i64::MAX),
        "i64min": Bson::Int64(i64::MIN),
        "i64small": Bson::Int64(7),
        "dec": Bson::Decimal128(Decimal128::from_str("123.4500").unwrap()),
        "date": DateTime::from_millis(-2_208_988_800_000),
        "dateFar": DateTime::from_millis(253_402_300_800_000),
        "bin": Binary { subtype: BinarySubtype::Generic, bytes: vec![0, 255, 7] },
        "binUser": Binary { subtype: BinarySubtype::UserDefined(200), bytes: vec![9] },
        "uuid": Binary { subtype: BinarySubtype::Uuid, bytes: (0u8..16).collect() },
        "uuidOld": Binary { subtype: BinarySubtype::UuidOld, bytes: (0u8..16).collect() },
        "ts": Timestamp { time: 4_000_000_000, increment: 1 },
        "min": Bson::MinKey,
        "max": Bson::MaxKey,
        "re": Bson::RegularExpression(bson::Regex { pattern: "^a/b".into(), options: "ims".into() }),
        "code": Bson::JavaScriptCode("return 1".into()),
        "codews": Bson::JavaScriptCodeWithScope(bson::JavaScriptCodeWithScope { code: "x".into(), scope: doc! { "k": Bson::Int64(3) } }),
        "ref": Bson::Document(doc! { "$ref": "c", "$id": 5, "$db": "d" }),
        "nested": { "z": [1, { "y": Bson::Int64(2) }, [Bson::Null, "s"]], "a": { "b": true } },
        "weird key": "v",
        "emoji": "\u{1F600} \u{0}",
    }
}

#[test]
fn values_round_trip_through_the_script_without_losing_a_type() {
    let original = rich();
    let sent = original.clone();
    let port = FakePort::new(move |_, command| match command_name(command) {
        "find" => Ok(first_batch("app.t", 0, vec![sent.clone()])),
        _ => Ok(doc! { "ok": 1.0 }),
    });
    let reply = run("var d = db.t.findOne(); db.t2.insertOne(d); db.t3.replaceOne({ _id: d._id }, d);", &port);
    assert_ok(&reply);
    let seen = port.seen();
    let written = seen.iter().find(|(_, c)| command_name(c) == "insert").unwrap().1.get_array("documents").unwrap()[0]
        .as_document()
        .unwrap()
        .clone();
    assert_eq!(written, original, "insert wrote exactly what find read");
    let replaced = seen.iter().find(|(_, c)| command_name(c) == "update").unwrap().1.get_array("updates").unwrap()[0]
        .as_document()
        .unwrap()
        .get_document("u")
        .unwrap()
        .clone();
    assert_eq!(replaced, original);
    // Key order survives too.
    assert_eq!(written.keys().collect::<Vec<_>>(), original.keys().collect::<Vec<_>>());
}

#[test]
fn ejson_round_trips_canonical_and_relaxed() {
    let original = rich();
    let sent = original.clone();
    let port = FakePort::new(move |_, _| Ok(first_batch("app.t", 0, vec![sent.clone()])));
    let reply = run(
        "var d = db.t.findOne(); var text = EJSON.stringify(d, { relaxed: false }); var back = EJSON.parse(text);\n\
         db.t2.insertOne(back); db.t2.insertOne(EJSON.parse(EJSON.stringify(d)));",
        &port,
    );
    assert_ok(&reply);
    let seen = port.seen();
    let inserts: Vec<Document> = seen
        .iter()
        .filter(|(_, c)| command_name(c) == "insert")
        .map(|(_, c)| c.get_array("documents").unwrap()[0].as_document().unwrap().clone())
        .collect();
    assert_eq!(inserts[0], original, "canonical EJSON round trips every type");
    // Relaxed loses the Int64-vs-Int32 distinction for small values only, never the big ones.
    assert_eq!(inserts[1].get("i64"), original.get("i64"));
    assert_eq!(inserts[1].get("date"), original.get("date"));
    assert_eq!(inserts[1].get("dec"), original.get("dec"));
}

#[test]
fn ejson_parse_reads_relaxed_text_and_stringify_has_both_forms() {
    let port = FakePort::ok();
    let reply = run(
        "var d = EJSON.parse('{\"a\":{\"$numberLong\":\"5\"},\"b\":{\"$date\":\"2020-01-01T00:00:00Z\"},\"c\":{\"$oid\":\"66d4f84d71c18f22d433b003\"},\"n\":7,\"x\":1.5}');\n\
         print(d.a instanceof Long, d.b instanceof Date, d.c instanceof ObjectId, d.n, d.x);\n\
         print(EJSON.stringify({ n: NumberLong(5), i: 1, big: NumberLong('9007199254740993'), d: new Date(0) }));\n\
         print(EJSON.stringify({ n: NumberLong(5), i: 1 }, { relaxed: false }));\n\
         print(EJSON.stringify({ a: [1, 2] }, null, 2));",
        &port,
    );
    assert_ok(&reply);
    assert_eq!(reply.output[0], "true true true 7 1.5");
    assert_eq!(reply.output[1], r#"{"n":5,"i":1,"big":{"$numberLong":"9007199254740993"},"d":{"$date":"1970-01-01T00:00:00.000Z"}}"#);
    assert_eq!(reply.output[2], r#"{"n":{"$numberLong":"5"},"i":{"$numberInt":"1"}}"#);
    assert_eq!(reply.output[3], "{\n  \"a\": [\n    1,\n    2\n  ]\n}");
}

#[test]
fn helper_values_behave_like_the_shells() {
    let port = FakePort::ok();
    let reply = run(
        "var id = ObjectId('66d4f84d71c18f22d433b003');\n\
         print(id.toString(), id.toHexString(), id.getTimestamp().toISOString(), id.equals('66d4f84d71c18f22d433b003'), ObjectId.isValid('zz'), ObjectId.isValid(String(id)));\n\
         print(new ObjectId().toString() !== new ObjectId().toString(), JSON.stringify({ id }));\n\
         var l = NumberLong('9007199254740993'); print(l.toString(), l.high, l.low, l.add(1).toString(), l.compare(NumberLong(1)), l + 1 > 9e15, l.toNumber());\n\
         print(NumberInt(5) + 1, Double(2.5) * 2, String(NumberDecimal('1.5')), UUID('01234567-89ab-cdef-0123-456789abcdef').toString(), UUID().toString().length, BinData(0, 'AQID').hex(), Timestamp(5, 6).t);\n\
         print(ISODate('2020-02-03T04:05:06.789Z').toISOString(), typeof ISODate(), new Date(Date.UTC(1969, 11, 31)).getTime());",
        &port,
    );
    assert_ok(&reply);
    assert_eq!(reply.output[0], "66d4f84d71c18f22d433b003 66d4f84d71c18f22d433b003 2024-09-01T23:27:09.000Z true false true");
    assert_eq!(reply.output[1], r#"true {"id":"66d4f84d71c18f22d433b003"}"#);
    assert_eq!(reply.output[2], "9007199254740993 2097152 1 9007199254740994 1 true 9007199254740992");
    assert_eq!(reply.output[3], "6 5 1.5 01234567-89ab-cdef-0123-456789abcdef 36 010203 5");
    assert_eq!(reply.output[4], "2020-02-03T04:05:06.789Z object -86400000");
}

#[test]
fn bad_helper_arguments_are_clear_errors() {
    let port = FakePort::ok();
    for (script, fragment) in [
        ("ObjectId('xyz')", "24 hex"),
        ("NumberLong('1.5')", "numeric string"),
        ("NumberLong('9223372036854775808')", "64-bit"),
        ("NumberInt(2147483648)", "32-bit"),
        ("NumberDecimal('abc')", "Decimal128"),
        ("ISODate('not a date')", "invalid ISODate"),
        ("UUID('1234')", "Invalid UUID"),
        ("BinData(0, '***')", "base64"),
        ("db.t.insertOne({ f: new Date(NaN) })", "Invalid Date"),
        ("db.t.insertOne(db.t.find())", "Cursor"),
        ("var a = {}; a.a = a; db.t.insertOne(a)", "circular"),
    ] {
        let reply = run(script, &port);
        let error = reply.error.unwrap_or_else(|| panic!("{script} should fail"));
        assert!(error.message.contains(fragment), "{script}: {}", error.message);
    }
}

#[test]
fn printing_uses_the_shell_text_of_each_type() {
    let port = FakePort::ok();
    let reply = run(
        "print('s', 1, 2.5, true, null, undefined, { a: NumberLong(1) }, [1, 'x']);\n\
         console.log('log', ObjectId('66d4f84d71c18f22d433b003')); console.error('err'); console.warn({ a: 1 }); console.info(ISODate('2020-01-01T00:00:00Z'));\n\
         printjson({ a: 1, b: [1, 2] });\n\
         print(tojson({ a: new Date(0) }), tojson(5), tojson('str'));\n\
         print(function f() {}, db, db.items);",
        &port,
    );
    assert_ok(&reply);
    assert_eq!(reply.output[0], r#"s 1 2.5 true null undefined { "a": Long('1') } [ 1, "x" ]"#);
    assert_eq!(reply.output[1], "log ObjectId('66d4f84d71c18f22d433b003')");
    assert_eq!(reply.output[2], "err");
    assert_eq!(reply.output[3], r#"{ "a": 1 }"#);
    assert_eq!(reply.output[4], "ISODate('2020-01-01T00:00:00.000Z')");
    assert!(reply.output[5].starts_with("{\n  \"a\": 1,"), "{:?}", reply.output[5]);
    assert_eq!(reply.output[6], "{\n  \"a\": ISODate('1970-01-01T00:00:00.000Z')\n} 5 str");
    assert_eq!(reply.output[7], "[Function: f] [Database] [Collection]");
}
