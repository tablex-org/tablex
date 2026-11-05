//! The real MongoDB driver (`dbengine-mongo`, ABI v2) loaded through the host
//! against a live server: the `mongo_aggregate` / `mongo_aggregate_explain`
//! query kinds, `export.mongo_aggregate`, and `operation.cancel` over the C ABI.
//!
//! Opt-in (it needs a server and the built driver):
//!
//! ```text
//! cargo build -p dbengine-mongo --features abi-v2
//! TABLEX_TEST_MONGO_URI=mongodb://127.0.0.1:27999 \
//! TABLEX_TEST_MONGO_DYLIB=<target>/debug/libdbengine_mongo.dylib \
//!   cargo test -p engine-driver-host --test mongo_aggregate_live -- --nocapture
//! ```
use engine_driver_api::{CancelRequest, ExportRequest, MutationRequest, OperationId, QueryRequest};
use engine_driver_host::{DriverLoader, LoaderError};
use std::path::PathBuf;
use std::time::{Duration, Instant};
use tempfile::TempDir;

fn live() -> Option<(String, PathBuf)> {
    let uri = std::env::var("TABLEX_TEST_MONGO_URI").ok()?;
    let dylib = std::env::var("TABLEX_TEST_MONGO_DYLIB").ok()?;
    Some((uri, PathBuf::from(dylib)))
}

fn query(
    operation: u64,
    kind: &str,
    database: &str,
    object: &str,
    payload: serde_json::Value,
) -> String {
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
fn live_driver_serves_aggregate_explain_export_and_cancel_over_the_abi() {
    let Some((uri, dylib)) = live() else {
        eprintln!("mongo_aggregate_live: TABLEX_TEST_MONGO_URI / TABLEX_TEST_MONGO_DYLIB not set, skipped");
        return;
    };
    let temp = TempDir::new().unwrap();
    let staged = temp.path().join("libdbengine_mongo.dylib");
    std::fs::copy(&dylib, &staged).unwrap();
    let handle = DriverLoader.load(&staged).expect("load the mongo driver");
    for feature in [
        engine_driver_api::Capability::QueryMongoAggregate,
        engine_driver_api::Capability::QueryMongoAggregateExplain,
        engine_driver_api::Capability::OperationCancel,
        engine_driver_api::Capability::ExportMongoAggregate,
    ] {
        assert!(handle.capabilities().has(feature), "{feature} advertised");
    }
    let mut session = handle
        .connect(&serde_json::json!({ "uri": uri, "name": "live" }).to_string())
        .expect("connect");
    let database = format!("tablex_host_live_{}", std::process::id());
    for id in 0..30 {
        session
            .mutate(&MutationRequest {
                operation_id: OperationId(1),
                kind: "insert".into(),
                database: Some(database.clone()),
                object: Some("items".into()),
                identity: serde_json::Value::Null,
                payload: serde_json::json!({ "_id": id, "grp": id % 3, "name": format!("n{id}") }),
            })
            .expect("seed");
    }
    let runner = session.query_runner().expect("runner");

    // Preview: capped at sample_size even though the pipeline has its own $limit.
    let page = runner
        .open_query(&query(
            10,
            "mongo_aggregate",
            &database,
            "items",
            serde_json::json!({ "pipeline": "[{ $sort: { _id: 1 } }, { $limit: 1000000 }]", "mode": "preview", "sample_size": 5 }),
        ))
        .expect("preview");
    assert_eq!((page.row_count, page.total_is_exact), (5, false));
    session.adopt_page(&runner, page.page_id).unwrap();
    let row = session.inspect_row(page.page_id, 0).unwrap();
    assert_eq!(row.fields[0].name, "_id");
    session.release_page(page.page_id).unwrap();

    // Explain returns the plan as one inspectable row.
    let plan = runner
        .open_query(&query(
            11,
            "mongo_aggregate_explain",
            &database,
            "items",
            serde_json::json!({ "pipeline": "[{ $match: { grp: 1 } }]", "verbosity": "executionStats" }),
        ))
        .expect("explain");
    assert_eq!(plan.row_count, 1);
    session.adopt_page(&runner, plan.page_id).unwrap();
    let fields = session.inspect_row(plan.page_id, 0).unwrap().fields;
    assert!(
        fields.iter().any(|field| field.name == "executionStats"),
        "{fields:?}"
    );
    session.release_page(plan.page_id).unwrap();

    // A write pipeline is refused until confirmed; previews never write.
    let refused = runner
        .open_query(&query(
            12,
            "mongo_aggregate",
            &database,
            "items",
            serde_json::json!({ "pipeline": "[{ $out: 'copy' }]", "mode": "full" }),
        ))
        .expect_err("needs confirmation");
    assert!(
        matches!(&refused, LoaderError::DriverRejected { code, .. } if code == "query.write_requires_confirmation"),
        "{refused:?}"
    );
    let preview = runner
        .open_query(&query(
            13,
            "mongo_aggregate",
            &database,
            "items",
            serde_json::json!({ "pipeline": "[{ $out: 'copy' }]", "mode": "preview" }),
        ))
        .expect("preview strips $out");
    session.adopt_page(&runner, preview.page_id).unwrap();
    session.release_page(preview.page_id).unwrap();
    let invalid = runner
        .open_query(&query(
            14,
            "mongo_aggregate",
            &database,
            "items",
            serde_json::json!({ "pipeline": "[{ $match: { a: } }]" }),
        ))
        .expect_err("invalid pipeline");
    assert!(
        matches!(&invalid, LoaderError::DriverRejected { code, .. } if code == "query.invalid_pipeline"),
        "{invalid:?}"
    );

    // Export Data through the detached runner.
    let destination = temp.path().join("export.csv");
    let result = runner
        .export(&ExportRequest {
            operation_id: OperationId(15),
            format: "csv".into(),
            destination: destination.to_string_lossy().into_owned(),
            page_id: None,
            query: Some(
                serde_json::from_str(&query(
                    15,
                    "mongo_aggregate",
                    &database,
                    "items",
                    serde_json::json!({ "pipeline": "[{ $sort: { _id: 1 } }]" }),
                ))
                .unwrap(),
            ),
            options: serde_json::Value::Null,
        })
        .expect("export");
    assert_eq!(result.rows_written, 30);
    assert_eq!(
        std::fs::read_to_string(&destination)
            .unwrap()
            .lines()
            .count(),
        31
    );

    // Cancellation: a slow aggregate in a thread, cancelled through the
    // handle while the session stays usable (it is not locked by the query).
    let operation = session.begin_operation().expect("operation");
    let operation_id = operation.id();
    let slow = query(
        operation_id.0,
        "mongo_aggregate",
        &database,
        "items",
        serde_json::json!({
            "pipeline": "[{ $addFields: { s: { $function: { body: 'function() { sleep(1500); return 1 }', args: [], lang: 'js' } } } }]",
            "mode": "preview", "sample_size": 30,
        }),
    );
    let slow_runner = runner.clone();
    let started = Instant::now();
    let worker = std::thread::spawn(move || slow_runner.open_query(&slow));
    std::thread::sleep(Duration::from_millis(400));
    // Another request on the same session works while the slow one runs.
    let nodes = session
        .list_nav_nodes(
            r#"{"parent":null,"database":null,"payload":null,"cursor":null,"limit":50}"#,
        )
        .expect("session is free while a query runs");
    assert!(!nodes.nodes.is_empty());
    handle
        .cancel_operation(session.connection_id(), &CancelRequest { operation_id })
        .expect("cancel");
    let outcome = worker.join().unwrap();
    assert!(
        matches!(&outcome, Err(LoaderError::DriverRejected { code, .. }) if code == "query.cancelled"),
        "{outcome:?}"
    );
    assert!(
        started.elapsed() < Duration::from_millis(1400),
        "cancelled promptly: {:?}",
        started.elapsed()
    );
    drop(operation);
    // Cancelling something that is gone is not an error.
    handle
        .cancel_operation(
            session.connection_id(),
            &CancelRequest {
                operation_id: OperationId(999_999),
            },
        )
        .expect("idempotent cancel");

    // Cleanup.
    session
        .mutate(&MutationRequest {
            operation_id: OperationId(2),
            kind: "drop_database".into(),
            database: Some(database),
            object: None,
            identity: serde_json::Value::Null,
            payload: serde_json::Value::Null,
        })
        .expect("drop test database");
    session.disconnect().unwrap();
    // The runner keeps one driver operation registered for as long as it (or
    // a clone) lives, so shutdown is refused until it is released.
    assert!(matches!(
        handle.shutdown(),
        Err(LoaderError::ActiveOwners { operations: 1, .. })
    ));
    drop(runner);
    handle.shutdown().unwrap();
}
