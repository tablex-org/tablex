//! Which pages a write drops, against the real MongoDB driver (`dbengine-mongo`,
//! ABI v2) on a live server. Its own test binary: the driver keeps one page
//! cache per process, so it must not share one with another live test.
//!
//! Opt-in, like `mongo_aggregate_live`: set `TABLEX_TEST_MONGO_URI` and
//! `TABLEX_TEST_MONGO_DYLIB`.
use engine_driver_api::{MutationRequest, OperationId, QueryRequest};
use engine_driver_host::DriverLoader;
use std::path::PathBuf;
use tempfile::TempDir;

fn live() -> Option<(String, PathBuf)> {
    let uri = std::env::var("TABLEX_TEST_MONGO_URI").ok()?;
    let dylib = std::env::var("TABLEX_TEST_MONGO_DYLIB").ok()?;
    Some((uri, PathBuf::from(dylib)))
}

fn query(operation: u64, kind: &str, database: &str, object: &str, payload: serde_json::Value) -> String {
    serde_json::to_string(&QueryRequest {
        kind: kind.into(),
        operation_id: OperationId(operation),
        database: Some(database.into()),
        object: Some(object.into()),
        offset: 0,
        limit: 50,
        payload,
    })
    .unwrap()
}

#[test]
fn live_write_drops_only_the_pages_it_can_make_stale() {
    let Some((uri, dylib)) = live() else {
        eprintln!("mongo_aggregate_live: TABLEX_TEST_MONGO_URI / TABLEX_TEST_MONGO_DYLIB not set, skipped");
        return;
    };
    let temp = TempDir::new().unwrap();
    let staged = temp.path().join("libdbengine_mongo.dylib");
    std::fs::copy(&dylib, &staged).unwrap();
    let handle = DriverLoader.load(&staged).expect("load the mongo driver");
    let mut session = handle
        .connect(&serde_json::json!({ "uri": uri, "name": "live" }).to_string())
        .expect("connect");
    let database = format!("tablex_host_scope_{}", std::process::id());
    let write = |session: &mut engine_driver_host::DriverSession, kind: &str, database: &str, object: Option<&str>, payload| {
        session.mutate(&MutationRequest {
            operation_id: OperationId(1),
            kind: kind.into(),
            database: Some(database.into()),
            object: object.map(str::to_string),
            identity: serde_json::Value::Null,
            payload,
        })
    };
    for collection in ["orders", "users"] {
        write(&mut session, "insert", &database, Some(collection), serde_json::json!({ "_id": 1 }))
            .expect("seed");
    }
    let runner = session.query_runner().expect("runner");
    let find = |session: &mut engine_driver_host::DriverSession, collection: &str| {
        let page = runner
            .open_query(&query(
                20,
                "mongo_find",
                &database,
                collection,
                serde_json::json!({ "filter": "{}", "sort": "{\"_id\":1}", "project": "" }),
            ))
            .expect("find");
        session.adopt_page(&runner, page.page_id).unwrap();
        page.page_id
    };
    let orders = find(&mut session, "orders");
    let users = find(&mut session, "users");

    // A write to `orders` makes only the `orders` page stale.
    let result = write(&mut session, "insert", &database, Some("orders"), serde_json::json!({ "_id": 2 }))
        .expect("insert");
    assert_eq!(result.invalidated_page_ids, [orders]);
    assert!(session.inspect_row(users, 0).is_ok(), "the users page survives");
    assert!(session.inspect_row(orders, 0).is_err(), "the orders page is gone");

    // Creating a database cannot stale any page.
    let created = write(&mut session, "create_database", &format!("{database}_new"), None, serde_json::Value::Null)
        .expect("create database");
    assert!(created.invalidated_page_ids.is_empty(), "{created:?}");

    // Administrative changes may touch anything: every page goes.
    let dropped = write(&mut session, "drop_database", &format!("{database}_new"), None, serde_json::json!({}))
        .map(|result| result.invalidated_page_ids);
    if let Ok(ids) = dropped {
        assert!(ids.contains(&users), "{ids:?}");
    }
    let _ = write(&mut session, "drop_database", &database, None, serde_json::json!({}));
}
