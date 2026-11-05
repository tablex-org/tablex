//! The real MongoDB driver (`dbengine-mongo`, ABI v2) against a live server:
//! every BSON type survives read -> edit of another field -> read, `_id` of every
//! type addresses exactly its own document, a row without `_id` cannot be
//! written, an unmatched write is an error, and typed values in find filters
//! (shell constructors, Extended JSON, scripts) select the right documents.
//!
//! Opt-in (it needs a server and the built driver):
//!
//! ```text
//! cargo build -p dbengine-mongo --features abi-v2
//! TABLEX_TEST_MONGO_URI=mongodb://127.0.0.1:27999 \
//! TABLEX_TEST_MONGO_DYLIB=<target>/debug/libdbengine_mongo.dylib \
//!   cargo test -p engine-driver-host --test mongo_values_live -- --nocapture
//! ```
use engine_driver_api::{MutationRequest, OperationId, QueryRequest, RowFields};
use engine_driver_host::{DriverLoader, DriverSession, LoaderError};
use serde_json::{json, Value};
use std::path::PathBuf;
use tempfile::TempDir;

fn live() -> Option<(String, PathBuf)> {
    let uri = std::env::var("TABLEX_TEST_MONGO_URI").ok()?;
    let dylib = std::env::var("TABLEX_TEST_MONGO_DYLIB").ok()?;
    // The private throwaway server only.
    assert!(uri.contains("127.0.0.1:27999"), "refusing to touch {uri}");
    Some((uri, PathBuf::from(dylib)))
}

struct Rig {
    session: DriverSession,
    database: String,
    operation: u64,
}

impl Rig {
    fn operation(&mut self) -> OperationId {
        self.operation += 1;
        OperationId(self.operation)
    }

    fn write(
        &mut self,
        kind: &str,
        collection: &str,
        identity: Value,
        payload: Value,
    ) -> Result<(), LoaderError> {
        let request = MutationRequest {
            operation_id: self.operation(),
            kind: kind.into(),
            database: Some(self.database.clone()),
            object: Some(collection.into()),
            identity,
            payload,
        };
        self.session.mutate(&request).map(|_| ())
    }

    fn find(&mut self, collection: &str, filter: &str, project: &str) -> (u64, u64) {
        let request = serde_json::to_string(&QueryRequest {
            kind: "mongo_find".into(),
            operation_id: self.operation(),
            database: Some(self.database.clone()),
            object: Some(collection.into()),
            offset: 0,
            limit: 100,
            payload: json!({ "filter": filter, "sort": "{\"_id\":1}", "project": project }),
        })
        .unwrap();
        self.query(&request)
    }

    fn script(&mut self, script: &str) -> (u64, u64) {
        let request = serde_json::to_string(&QueryRequest {
            kind: "mongo_script".into(),
            operation_id: self.operation(),
            database: Some(self.database.clone()),
            object: None,
            offset: 0,
            limit: 100,
            payload: json!({ "script": script }),
        })
        .unwrap();
        self.query(&request)
    }

    fn query(&mut self, request: &str) -> (u64, u64) {
        let runner = self.session.query_runner().expect("runner");
        let page = runner.open_query(request).expect("query");
        self.session.adopt_page(&runner, page.page_id).unwrap();
        (page.page_id, page.row_count)
    }

    fn rows(&mut self, collection: &str, filter: &str) -> Vec<RowFields> {
        let (page, count) = self.find(collection, filter, "");
        let rows = (0..count)
            .map(|row| self.session.inspect_row(page, row).expect("inspect"))
            .collect();
        self.session.release_page(page).unwrap();
        rows
    }
}

fn code(error: LoaderError) -> String {
    match error {
        LoaderError::DriverRejected { code, .. } => code,
        other => panic!("not a driver rejection: {other:?}"),
    }
}

fn field<'a>(row: &'a RowFields, name: &str) -> &'a engine_driver_api::RowField {
    row.fields
        .iter()
        .find(|field| field.name == name)
        .unwrap_or_else(|| panic!("no field {name}"))
}

/// The canonical Extended JSON of one document of every BSON type.
fn every_type() -> Value {
    json!({
        "_id": {"$oid": "66d4f84d0000000000000003"},
        "i32": {"$numberInt": "5"}, "i32_neg": {"$numberInt": "-5"}, "i32_min": {"$numberInt": "-2147483648"},
        "i64_small": {"$numberLong": "5"}, "i64_neg": {"$numberLong": "-5"}, "i64_big": {"$numberLong": "9007199254740993"},
        "dbl_whole": {"$numberDouble": "5.0"}, "dbl": {"$numberDouble": "0.30000000000000004"},
        "dbl_nan": {"$numberDouble": "NaN"}, "dbl_inf": {"$numberDouble": "Infinity"}, "dbl_ninf": {"$numberDouble": "-Infinity"},
        "dbl_negzero": {"$numberDouble": "-0.0"},
        "dec": {"$numberDecimal": "12345678901234567890.25"},
        "str": "NULL", "empty": "", "yes": true, "nothing": null,
        "undef": {"$undefined": true},
        "date": {"$date": {"$numberLong": "1700000000123"}},
        "bin": {"$binary": {"base64": "AAH/", "subType": "00"}},
        "bin_user": {"$binary": {"base64": "CQ==", "subType": "90"}},
        "uuid": {"$binary": {"base64": "AAECAwQFBgcICQoLDA0ODw==", "subType": "04"}},
        "uuid_old": {"$binary": {"base64": "AAECAwQFBgcICQoLDA0ODw==", "subType": "03"}},
        "re": {"$regularExpression": {"pattern": "^a.b$", "options": "i"}},
        "ts": {"$timestamp": {"t": 1700000000, "i": 7}},
        "min": {"$minKey": 1}, "max": {"$maxKey": 1},
        "sym": {"$symbol": "s"}, "js": {"$code": "x => 1"},
        "js_scope": {"$code": "f", "$scope": {"a": {"$numberLong": "1"}}},
        "doc": {"z": {"$numberLong": "12345"}, "a": {"n": [{"$numberInt": "1"}, {"$numberDouble": "2.5"}, {"$numberLong": "3"}]}},
        "arr": [{"k": "v"}, [], {"$numberLong": "7"}],
    })
}

fn canonical_of(row: &RowFields) -> Value {
    Value::Object(
        row.fields
            .iter()
            .map(|field| (field.name.clone(), field.value.clone()))
            .collect(),
    )
}

#[test]
fn live_values_round_trip_and_writes_address_exactly_one_document() {
    let Some((uri, dylib)) = live() else {
        eprintln!(
            "mongo_values_live: TABLEX_TEST_MONGO_URI / TABLEX_TEST_MONGO_DYLIB not set, skipped"
        );
        return;
    };
    let temp = TempDir::new().unwrap();
    let staged = temp.path().join("libdbengine_mongo.dylib");
    std::fs::copy(&dylib, &staged).unwrap();
    let handle = DriverLoader.load(&staged).expect("load the mongo driver");
    let session = handle
        .connect(&json!({ "uri": uri, "name": "live" }).to_string())
        .expect("connect");
    let mut rig = Rig {
        session,
        database: format!("tablex_values_{}", std::process::id()),
        operation: 100,
    };

    // ---- every type: insert from canonical, read, edit ANOTHER field, read ----
    let canonical = every_type();
    rig.write("insert", "types", Value::Null, canonical.clone())
        .expect("insert");
    let before = rig.rows("types", "{}");
    assert_eq!(before.len(), 1);
    assert_eq!(
        canonical_of(&before[0]),
        canonical,
        "read == what was inserted, in key order"
    );
    assert_eq!(
        canonical_of(&before[0])
            .as_object()
            .unwrap()
            .keys()
            .collect::<Vec<_>>(),
        canonical.as_object().unwrap().keys().collect::<Vec<_>>()
    );
    assert!(field(&before[0], "dbl_nan").value["$numberDouble"] == "NaN");
    rig.write(
        "update",
        "types",
        before[0].mutation_identity.clone(),
        json!({"$set": {"str": "changed"}}),
    )
    .expect("edit another field");
    let after = rig.rows("types", "{}");
    let mut expected = canonical.clone();
    expected["str"] = json!("changed");
    assert_eq!(
        canonical_of(&after[0]),
        expected,
        "only `str` changed; every other type intact"
    );

    // Editing a nested document / array cell as a whole keeps its untouched members' types.
    let mut nested = field(&after[0], "doc").value.clone();
    nested["a"]["extra"] = json!({"$numberInt": "9"});
    rig.write(
        "update",
        "types",
        after[0].mutation_identity.clone(),
        json!({"$set": {"doc": nested.clone()}}),
    )
    .expect("edit nested");
    let edited = rig.rows("types", "{}");
    assert_eq!(field(&edited[0], "doc").value, nested);
    assert_eq!(
        field(&edited[0], "i64_small").value,
        json!({"$numberLong": "5"})
    );

    // ---- duplicate / insert from canonical keep Int32 vs Int64 regardless of sign and size ----
    let mut duplicate = canonical_of(&edited[0]);
    duplicate.as_object_mut().unwrap().remove("_id");
    rig.write("insert", "types", Value::Null, duplicate.clone())
        .expect("duplicate");
    let copies = rig.rows("types", "{}");
    assert_eq!(copies.len(), 2);
    let mut copy = canonical_of(&copies[1]);
    copy.as_object_mut().unwrap().remove("_id");
    assert_eq!(copy, duplicate, "the duplicate has the same BSON types");

    // ---- `_id` of every type identifies exactly its own document ----
    let ids = [
        ("oid", json!({"$oid": "66d4f84d00000000000000aa"})),
        ("string", json!("alpha")),
        ("i32", json!({"$numberInt": "-7"})),
        ("i64", json!({"$numberLong": "9007199254740993"})),
        ("double", json!({"$numberDouble": "1.5"})),
        ("nan", json!({"$numberDouble": "NaN"})),
        ("date", json!({"$date": {"$numberLong": "1700000000123"}})),
        (
            "uuid",
            json!({"$binary": {"base64": "AAECAwQFBgcICQoLDA0ODw==", "subType": "04"}}),
        ),
        (
            "embedded",
            json!({"b": {"$numberLong": "1"}, "a": {"$numberInt": "2"}}),
        ),
        ("null", Value::Null),
    ];
    for (label, id) in &ids {
        rig.write(
            "insert",
            "ids",
            Value::Null,
            json!({"_id": id, "label": label, "n": {"$numberInt": "0"}}),
        )
        .unwrap_or_else(|error| panic!("insert {label}: {error:?}"));
    }
    let rows = rig.rows("ids", "{}");
    assert_eq!(rows.len(), ids.len());
    for (label, id) in &ids {
        let rows = rig.rows("ids", "{}");
        let row = rows
            .iter()
            .find(|row| field(row, "label").value == json!(label))
            .unwrap();
        assert_eq!(row.mutation_identity, json!({"_id": id}), "{label}");
        rig.write(
            "update",
            "ids",
            row.mutation_identity.clone(),
            json!({"$set": {"n": {"$numberInt": "1"}}}),
        )
        .unwrap_or_else(|error| panic!("update {label}: {error:?}"));
        let changed: Vec<_> = rig
            .rows("ids", "{}")
            .into_iter()
            .filter(|row| field(row, "n").value == json!({"$numberInt": "1"}))
            .map(|row| field(&row, "label").value.clone())
            .collect();
        assert_eq!(changed, [json!(label)], "only {label} changed");
        rig.write(
            "update",
            "ids",
            row.mutation_identity.clone(),
            json!({"$set": {"n": {"$numberInt": "0"}}}),
        )
        .unwrap();
    }
    // Delete of each leaves all the others.
    for (index, (label, _)) in ids.iter().enumerate() {
        let rows = rig.rows("ids", "{}");
        let row = rows
            .iter()
            .find(|row| field(row, "label").value == json!(label))
            .unwrap();
        rig.write("delete", "ids", row.mutation_identity.clone(), json!({}))
            .unwrap();
        assert_eq!(
            rig.rows("ids", "{}").len(),
            ids.len() - index - 1,
            "deleted exactly {label}"
        );
    }

    // ---- an unmatched write is an error with the contract code ----
    rig.write(
        "insert",
        "gone",
        Value::Null,
        json!({"_id": {"$numberInt": "1"}, "v": 1}),
    )
    .unwrap();
    let identity = rig.rows("gone", "{}")[0].mutation_identity.clone();
    rig.write("delete", "gone", identity.clone(), json!({}))
        .unwrap();
    for (kind, payload) in [("delete", json!({})), ("update", json!({"$set": {"v": 2}}))] {
        let error = rig
            .write(kind, "gone", identity.clone(), payload)
            .unwrap_err();
        assert_eq!(code(error), "mutation.no_rows_affected", "{kind}");
    }
    // A write that changes nothing but matches is still a success.
    rig.write(
        "insert",
        "gone",
        Value::Null,
        json!({"_id": {"$numberInt": "2"}, "v": {"$numberInt": "1"}}),
    )
    .unwrap();
    rig.write(
        "update",
        "gone",
        json!({"_id": {"$numberInt": "2"}}),
        json!({"$set": {"v": {"$numberInt": "1"}}}),
    )
    .expect("matched, unchanged");

    // ---- a row without `_id` (projection) cannot be written; no other filter is accepted ----
    rig.write(
        "insert",
        "proj",
        Value::Null,
        json!({"_id": {"$numberInt": "1"}, "name": "a"}),
    )
    .unwrap();
    rig.write(
        "insert",
        "proj",
        Value::Null,
        json!({"_id": {"$numberInt": "2"}, "name": "a"}),
    )
    .unwrap();
    let (page, count) = rig.find("proj", "{}", "{\"_id\":0,\"name\":1}");
    assert_eq!(count, 2);
    let row = rig.session.inspect_row(page, 0).unwrap();
    rig.session.release_page(page).unwrap();
    assert!(
        row.mutation_identity.is_null(),
        "{:?}",
        row.mutation_identity
    );
    assert!(row.fields.iter().all(|field| !field.editable));
    for identity in [
        row.mutation_identity.clone(),
        json!({"rowIndex": 0}),
        json!({"name": "a"}),
        json!({"_id": {"$gte": 0}}),
    ] {
        let error = rig
            .write(
                "update",
                "proj",
                identity.clone(),
                json!({"$set": {"name": "b"}}),
            )
            .unwrap_err();
        assert_eq!(code(error), "mutation.bad_request", "{identity}");
        let error = rig
            .write("delete", "proj", identity, json!({}))
            .unwrap_err();
        assert_eq!(code(error), "mutation.bad_request");
    }
    assert!(
        rig.rows("proj", "{ name: 'a' }").len() == 2,
        "nothing was touched"
    );

    // ---- dotted and `$`-prefixed top-level field names are read-only ----
    rig.write(
        "insert",
        "dotted",
        Value::Null,
        json!({"_id": {"$numberInt": "1"}, "a.b": 1, "plain": 2, "$set": 3}),
    )
    .expect("the server stores such names");
    let row = &rig.rows("dotted", "{}")[0];
    assert!(!field(row, "a.b").editable);
    assert!(!field(row, "$set").editable);
    assert!(field(row, "plain").editable);

    // ---- typed values in find filters: shell constructors, Extended JSON, scripts ----
    for (index, millis) in [1_722_470_400_000_i64, 1_726_358_400_000, 1_729_382_400_000]
        .into_iter()
        .enumerate()
    {
        rig.write(
            "insert",
            "typed",
            Value::Null,
            json!({
                "_id": {"$oid": format!("66d4f84d000000000000000{index}")},
                "at": {"$date": {"$numberLong": millis.to_string()}},
                "big": {"$numberLong": (9_007_199_254_740_993_i64 + index as i64).to_string()},
                "price": {"$numberDecimal": format!("{index}.50")},
                "name": format!("n{index}"),
            }),
        )
        .unwrap();
    }
    let names = |rig: &mut Rig, filter: &str| -> Vec<String> {
        rig.rows("typed", filter)
            .iter()
            .map(|row| field(row, "name").value.as_str().unwrap().to_string())
            .collect()
    };
    let september_on = ["n1", "n2"];
    for filter in [
        r#"{ at: { $gte: ISODate("2024-09-01T00:00:00Z") } }"#,
        r#"{ at: { $gte: new Date("2024-09-01") } }"#,
        r#"{ "at": { "$gte": { "$date": "2024-09-01T00:00:00Z" } } }"#,
        r#"{ "at": { "$gte": { "$date": { "$numberLong": "1725148800000" } } } }"#,
    ] {
        assert_eq!(names(&mut rig, filter), september_on, "{filter}");
    }
    for filter in [
        r#"{ _id: ObjectId("66d4f84d0000000000000001") }"#,
        r#"{ "_id": { "$oid": "66d4f84d0000000000000001" } }"#,
    ] {
        assert_eq!(names(&mut rig, filter), ["n1"], "{filter}");
    }
    for filter in [
        r#"{ big: NumberLong("9007199254740994") }"#,
        r#"{ big: { "$numberLong": "9007199254740994" } }"#,
    ] {
        assert_eq!(names(&mut rig, filter), ["n1"], "{filter}");
    }
    for filter in [
        r#"{ price: Decimal128("1.50") }"#,
        r#"{ price: NumberDecimal("1.50") }"#,
        r#"{ "price": { "$numberDecimal": "1.50" } }"#,
    ] {
        assert_eq!(names(&mut rig, filter), ["n1"], "{filter}");
    }
    let (page, count) =
        rig.script(r#"db.typed.find({ at: { $gte: ISODate("2024-09-01T00:00:00Z") } })"#);
    assert_eq!(
        count, 2,
        "a script's ISODate compares as a date, not a sub-document"
    );
    assert!(rig.session.inspect_row(page, 0).is_ok());
    rig.session.release_page(page).unwrap();
    let (page, count) =
        rig.script(r#"db.typed.find({ _id: ObjectId("66d4f84d0000000000000002") })"#);
    assert_eq!(count, 1);
    rig.session.release_page(page).unwrap();
    let (page, count) = rig.script(r#"db.typed.find({ big: NumberLong("9007199254740995") })"#);
    assert_eq!(count, 1);
    rig.session.release_page(page).unwrap();

    // ---- column types of a mixed column ----
    rig.write(
        "insert",
        "mixed",
        Value::Null,
        json!({"_id": {"$numberInt": "1"}, "v": {"$numberInt": "1"}}),
    )
    .unwrap();
    rig.write(
        "insert",
        "mixed",
        Value::Null,
        json!({"_id": {"$numberInt": "2"}, "v": "text"}),
    )
    .unwrap();
    let request = serde_json::to_string(&QueryRequest {
        kind: "mongo_find".into(),
        operation_id: rig.operation(),
        database: Some(rig.database.clone()),
        object: Some("mixed".into()),
        offset: 0,
        limit: 10,
        payload: json!({"filter": "{}", "sort": "{}", "project": ""}),
    })
    .unwrap();
    let runner = rig.session.query_runner().unwrap();
    let page = runner.open_query(&request).unwrap();
    rig.session.adopt_page(&runner, page.page_id).unwrap();
    let v = page
        .columns
        .iter()
        .find(|column| column.name == "v")
        .unwrap();
    assert_eq!(v.data_type, "mixed");

    let _ = rig.write("drop_database", "", Value::Null, json!({}));
    rig.session.disconnect().unwrap();
}
