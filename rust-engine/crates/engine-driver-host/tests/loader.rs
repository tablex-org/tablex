mod common;

use engine_driver_api::{CancelRequest, ExportRequest, MutationRequest, OperationId, QueryRequest};
use engine_driver_host::{DriverHandle, DriverLoader, LoaderError, SharedDriverSession};
use sha2::{Digest, Sha256};
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::Arc;
use tempfile::TempDir;

fn build_mock_driver() -> PathBuf {
    build_mock_driver_with_features(None)
}

fn build_mock_driver_with_features(features: Option<&str>) -> PathBuf {
    common::built_once(&common::mock_key(features.unwrap_or("")), |target_dir| {
        cargo_build_mock(target_dir, features)
    })
}

fn cargo_build_mock(target_dir: &Path, features: Option<&str>) -> PathBuf {
    let manifest = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/mock-driver");
    let mut command = Command::new(env!("CARGO"));
    command
        .arg("build")
        .arg("--manifest-path")
        .arg(manifest.join("Cargo.toml"))
        .arg("--release")
        .arg("--target-dir")
        .arg(target_dir);
    if let Some(features) = features {
        command
            .arg("--no-default-features")
            .arg("--features")
            .arg(features);
    }
    let output = command.output().expect("cargo build mock-driver");
    assert!(
        output.status.success(),
        "mock-driver build failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    target_dir
        .join("release")
        .join(common::cdylib_file_name("mock_driver"))
}

fn stage_mock_with_features(temp: &TempDir, features: &str) -> PathBuf {
    let source = build_mock_driver_with_features(Some(features));
    let staged = temp.path().join(common::driver_file_name("mock"));
    fs::copy(source, &staged).expect("stage mock dylib");
    staged
}

/// Copy the mock dylib into a temp dir: loading by copy proves we never
/// dlopen a path we do not own for the lifetime of the handle.
fn stage_mock(temp: &TempDir) -> PathBuf {
    let source = build_mock_driver();
    let staged = temp.path().join(common::driver_file_name("mock"));
    fs::copy(source, &staged).expect("stage mock dylib");
    staged
}

fn load_mock(path: &Path) -> Arc<DriverHandle> {
    DriverLoader.load(path).expect("load mock driver")
}

fn sha256(path: &Path) -> String {
    format!(
        "{:x}",
        Sha256::digest(fs::read(path).expect("read artifact"))
    )
}

#[test]
fn loads_driver_and_validates_capabilities() {
    let temp = TempDir::new().expect("temp directory");
    let staged = stage_mock(&temp);
    let handle = load_mock(&staged);

    let capabilities = handle.capabilities();
    assert_eq!(capabilities.abi_version, 2);
    assert_eq!(capabilities.driver, "mock");
    assert!(capabilities.uri_schemes.contains(&"mock".to_string()));
    assert!(capabilities.has_token("query.find"));
}

#[test]
fn concurrent_loads_share_one_driver_handle() {
    let temp = TempDir::new().expect("temp directory");
    let staged = stage_mock(&temp);
    let staged = staged.as_path();
    let barrier = Arc::new(std::sync::Barrier::new(4));
    let handles = std::thread::scope(|scope| {
        let threads = (0..4)
            .map(|_| {
                let barrier = Arc::clone(&barrier);
                scope.spawn(move || {
                    barrier.wait();
                    load_mock(staged)
                })
            })
            .collect::<Vec<_>>();
        threads
            .into_iter()
            .map(|thread| thread.join().expect("load thread"))
            .collect::<Vec<_>>()
    });

    assert!(handles
        .windows(2)
        .all(|pair| Arc::ptr_eq(&pair[0], &pair[1])));
}

#[test]
fn reload_waits_for_last_drop_to_finish_shutdown() {
    let temp = TempDir::new().expect("temp directory");
    let staged = stage_mock_with_features(&temp, "full,drop-barrier,init-marker");
    let entered = temp.path().join("shutdown-entered");
    let release = temp.path().join("shutdown-release");
    let reload_marker = temp.path().join("reload-init-marker");
    std::env::set_var("TABLEX_MOCK_SHUTDOWN_ENTERED", &entered);
    std::env::set_var("TABLEX_MOCK_SHUTDOWN_RELEASE", &release);

    let first = load_mock(&staged);
    let dropper = std::thread::spawn(move || drop(first));
    for _ in 0..1000 {
        if entered.exists() {
            break;
        }
        std::thread::sleep(std::time::Duration::from_millis(1));
    }
    assert!(entered.exists(), "last-drop shutdown did not enter barrier");

    std::env::set_var("TABLEX_MOCK_INIT_MARKER", &reload_marker);
    let reload_path = staged.clone();
    let reloader = std::thread::spawn(move || load_mock(&reload_path));
    for _ in 0..5000 {
        if reload_marker.exists() {
            break;
        }
        std::thread::sleep(std::time::Duration::from_millis(1));
    }
    let reload_started = reload_marker.exists();

    let unrelated_temp = TempDir::new().expect("unrelated temp directory");
    let unrelated = stage_mock_with_features(&unrelated_temp, "full,init-marker");
    let unrelated_marker = unrelated_temp.path().join("init-marker");
    std::env::set_var("TABLEX_MOCK_INIT_MARKER", &unrelated_marker);
    let unrelated_path = unrelated.clone();
    let unrelated_loader = std::thread::spawn(move || load_mock(&unrelated_path));
    for _ in 0..5000 {
        if unrelated_marker.exists() {
            break;
        }
        std::thread::sleep(std::time::Duration::from_millis(1));
    }
    let unrelated_finished = unrelated_marker.exists();

    fs::write(&release, b"release").expect("release shutdown barrier");
    dropper.join().expect("drop thread");
    let second = reloader.join().expect("reload thread");
    assert!(!reload_started, "reload started before shutdown finished");
    let unrelated_handle = unrelated_loader.join().expect("unrelated load thread");
    assert!(
        unrelated_finished,
        "retiring artifact blocked unrelated artifact"
    );
    unrelated_handle
        .shutdown()
        .expect("shutdown unrelated driver");
    second.shutdown().expect("shutdown reloaded generation");
    std::env::remove_var("TABLEX_MOCK_SHUTDOWN_ENTERED");
    std::env::remove_var("TABLEX_MOCK_SHUTDOWN_RELEASE");
    std::env::remove_var("TABLEX_MOCK_INIT_MARKER");
}

#[test]
fn loader_cache_does_not_keep_last_handle_alive() {
    let temp = TempDir::new().expect("temp directory");
    let staged = stage_mock(&temp);
    let handle = load_mock(&staged);
    let weak = Arc::downgrade(&handle);

    assert_eq!(Arc::strong_count(&handle), 1);
    drop(handle);

    assert!(weak.upgrade().is_none());
}

#[test]
fn closed_generation_is_not_returned_from_loader_cache() {
    let temp = TempDir::new().expect("temp directory");
    let staged = stage_mock(&temp);
    let first = load_mock(&staged);
    first.shutdown().expect("shutdown first generation");

    let second = load_mock(&staged);

    assert!(!Arc::ptr_eq(&first, &second));
    drop(first);
    let third = load_mock(&staged);
    assert!(Arc::ptr_eq(&second, &third));
    second.shutdown().expect("shutdown second generation");
}

#[test]
fn verified_load_does_not_reuse_a_different_artifact_generation() {
    let temp = TempDir::new().expect("temp directory");
    let staged = stage_mock(&temp);
    let original_sha256 = sha256(&staged);
    let first = DriverLoader
        .load_verified(&staged, &original_sha256)
        .expect("load original artifact");

    let replacement = build_mock_driver_with_features(Some("readonly"));
    fs::copy(replacement, &staged).expect("replace staged artifact");
    let replacement_sha256 = sha256(&staged);
    let error = match DriverLoader.load_verified(&staged, &replacement_sha256) {
        Ok(_) => panic!("verified load reused a different artifact"),
        Err(error) => error,
    };
    assert!(matches!(error, LoaderError::ArtifactChanged { .. }));

    first.shutdown().expect("shutdown original generation");
}

#[test]
fn rejects_dylib_with_missing_abi_symbol() {
    let temp = TempDir::new().expect("temp directory");
    let fake = temp.path().join("libfake.dylib");
    fs::write(&fake, vec![0u8; 1024]).expect("write fake");

    let error = DriverLoader.load(&fake).err().expect("non-dylib rejected");
    assert!(matches!(error, LoaderError::DlOpenFailed { .. }));
}

#[test]
fn rejects_driver_before_dlopen_when_checksum_is_wrong() {
    let temp = TempDir::new().expect("temp directory");
    let staged = stage_mock(&temp);

    let error = match DriverLoader.load_verified(&staged, &"0".repeat(64)) {
        Ok(_) => panic!("checksum mismatch was not rejected"),
        Err(error) => error,
    };

    assert!(matches!(error, LoaderError::ChecksumMismatch { .. }));
}

#[test]
fn connect_browse_query_and_disconnect_roundtrip() {
    let temp = TempDir::new().expect("temp directory");
    let staged = stage_mock(&temp);
    let handle = load_mock(&staged);

    let mut session = handle
        .connect(r#"{"uri":"mock://local","name":"test"}"#)
        .expect("connect mock");
    assert!(session.connection_id() > 0);

    let nav_page = session
        .list_nav_nodes(
            r#"{"parent":null,"database":null,"payload":null,"cursor":null,"limit":100}"#,
        )
        .expect("list nav nodes");
    assert_eq!(nav_page.nodes.len(), 1);
    assert_eq!(nav_page.nodes[0].kind, "database");

    let query_page = session
        .open_query(
            r#"{"kind":"mongo_find","operation_id":1,"database":"app","object":"invoices","offset":0,"limit":100,"payload":{}}"#,
        )
        .expect("open query");
    assert_eq!(query_page.row_count, 1);
    assert!(query_page.total_is_exact);

    let batch = session
        .render_cell_batch(query_page.page_id, 0, 1, 0, 1)
        .expect("render cell batch");
    assert_eq!(batch.cell_count(), 1);
    let cell = batch.cell(0).expect("cell zero");
    assert_eq!(cell.row, 0);
    assert_eq!(cell.text, "hello");
    drop(batch);

    let row = session
        .inspect_row(query_page.page_id, 0)
        .expect("inspect row");
    assert_eq!(row.page_id, query_page.page_id);
    assert_eq!(row.fields[0].native_type, "object_id");

    session
        .release_page(query_page.page_id)
        .expect("release page");

    session.disconnect().expect("disconnect");
}

#[test]
fn dispatches_capability_gated_operations() {
    let temp = TempDir::new().expect("temp directory");
    let staged = stage_mock(&temp);
    let handle = load_mock(&staged);
    let mut session = handle
        .connect(r#"{"uri":"mock://local","name":"test"}"#)
        .expect("connect");

    let mutation = session
        .mutate(&MutationRequest {
            operation_id: OperationId(10),
            kind: "update".into(),
            database: Some("app".into()),
            object: Some("invoices".into()),
            identity: serde_json::json!({"_id": 1}),
            payload: serde_json::json!({"name": "updated"}),
        })
        .expect("mutate");
    assert_eq!(mutation.affected_rows, 1);

    session
        .cancel_operation(&CancelRequest {
            operation_id: OperationId(11),
        })
        .expect("cancel");

    let exported = session
        .export(&ExportRequest {
            operation_id: OperationId(12),
            format: "json".into(),
            destination: "/tmp/mock.json".into(),
            page_id: None,
            query: Some(QueryRequest {
                kind: "mock".into(),
                operation_id: OperationId(12),
                database: Some("app".into()),
                object: Some("invoices".into()),
                offset: 0,
                limit: 100,
                payload: serde_json::json!({}),
            }),
            options: serde_json::json!({}),
        })
        .expect("export");
    assert_eq!(exported.rows_written, 1);

    session.disconnect().expect("disconnect");
}

#[test]
fn old_mutation_driver_does_not_receive_unadvertised_database_create() {
    let temp = TempDir::new().expect("temp directory");
    let staged = stage_mock(&temp);
    let handle = load_mock(&staged);
    assert!(!handle
        .capabilities()
        .has(engine_driver_api::Capability::DatabaseCreate));
    let mut session = handle.connect(r#"{"uri":"mock://local"}"#).unwrap();
    let request = MutationRequest {
        operation_id: OperationId(40),
        kind: "create_database".into(),
        database: Some("fixture".into()),
        object: None,
        identity: serde_json::Value::Null,
        payload: serde_json::Value::Null,
    };
    assert!(matches!(
        session.mutate(&request),
        Err(LoaderError::UnsupportedFeature { .. })
    ));
    session.disconnect().unwrap();
}

#[test]
fn mongo_package_manifest_advertises_database_create_when_driver_does() {
    let script = include_str!("../../../scripts/package-driver.sh");
    let mongo = script
        .split("mongodb)")
        .nth(1)
        .unwrap()
        .split(";;")
        .next()
        .unwrap();
    assert!(mongo.contains("\"database.create\""));
    assert!(mongo.contains("\"query.mongo_script\""));
}

#[test]
fn unadvertised_operation_is_rejected_without_symbol() {
    let temp = TempDir::new().expect("temp directory");
    let staged = stage_mock_with_features(&temp, "readonly");
    let handle = load_mock(&staged);
    let mut session = handle
        .connect(r#"{"uri":"mock://local","name":"test"}"#)
        .expect("connect");

    let error = session
        .mutate(&MutationRequest {
            operation_id: OperationId(10),
            kind: "update".into(),
            database: None,
            object: None,
            identity: serde_json::json!({}),
            payload: serde_json::json!({}),
        })
        .expect_err("read-only driver rejects mutation");

    assert!(matches!(error, LoaderError::UnsupportedFeature { .. }));
    session.disconnect().expect("disconnect");
}

#[test]
fn advertised_operation_requires_matching_symbol() {
    let temp = TempDir::new().expect("temp directory");
    let staged = stage_mock_with_features(&temp, "missing-mutate");

    let error = DriverLoader
        .load(&staged)
        .err()
        .expect("missing capability symbol rejected");

    assert!(matches!(
        error,
        LoaderError::MissingSymbol { ref symbol } if symbol == "dbengine_mutate"
    ));
}

#[test]
fn describe_object_is_callable_when_capability_and_symbol_are_present() {
    let temp = TempDir::new().expect("temp directory");
    let staged = stage_mock_with_features(&temp, "full,describe");
    let handle = load_mock(&staged);
    assert!(handle
        .capabilities()
        .has(engine_driver_api::Capability::MetadataTableProperties));
    let mut session = handle
        .connect(r#"{"uri":"mock://local","name":"test"}"#)
        .expect("connect");

    let request = serde_json::json!({"database": "app", "schema": "public", "object": "items"});
    let value = session
        .describe_object(&request)
        .expect("describe_object reaches the driver");

    assert_eq!(value["connection_id"], session.connection_id());
    assert_eq!(value["echo"], request);
    session.disconnect().expect("disconnect");
}

#[test]
fn describe_object_without_the_capability_is_unsupported_and_never_calls_the_driver() {
    let temp = TempDir::new().expect("temp directory");
    // `full` has no metadata.table_properties and no dbengine_describe_object.
    let staged = stage_mock_with_features(&temp, "full");
    let handle = load_mock(&staged);
    assert!(!handle
        .capabilities()
        .has(engine_driver_api::Capability::MetadataTableProperties));
    let mut session = handle
        .connect(r#"{"uri":"mock://local","name":"test"}"#)
        .expect("connect");

    let error = session
        .describe_object(&serde_json::json!({"object": "items"}))
        .expect_err("capability absent");

    assert!(matches!(
        error,
        LoaderError::UnsupportedFeature { ref feature } if feature == "metadata.table_properties"
    ));
    session.disconnect().expect("disconnect");
}

#[test]
fn describe_object_capability_requires_matching_symbol() {
    let temp = TempDir::new().expect("temp directory");
    let staged = stage_mock_with_features(&temp, "full,describe-missing-symbol");

    let error = DriverLoader
        .load(&staged)
        .err()
        .expect("advertised capability without its symbol is rejected");

    assert!(matches!(
        error,
        LoaderError::MissingSymbol { ref symbol } if symbol == "dbengine_describe_object"
    ));
}

#[test]
fn the_dialect_in_the_connect_reply_belongs_to_that_connection() {
    let temp = TempDir::new().expect("temp directory");
    let staged = stage_mock(&temp);
    let handle = load_mock(&staged);

    let plain = handle
        .connect(r#"{"uri":"mock://local","name":"plain"}"#)
        .expect("connect mock");
    assert_eq!(plain.connection_dialect(), None, "no field: no dialect");

    let session = handle
        .connect(r#"{"uri":"mock://no-backslash-escapes","name":"strict"}"#)
        .expect("connect mock");
    let dialect = session.connection_dialect().expect("the reply's dialect");
    assert!(!dialect.string_backslash_escapes);
    assert_eq!(dialect.identifier_quote, "`");
    // The other connection of the same driver is unaffected.
    assert_eq!(plain.connection_dialect(), None);
}

#[test]
fn connect_failure_returns_driver_error() {
    let temp = TempDir::new().expect("temp directory");
    let staged = stage_mock(&temp);
    let handle = load_mock(&staged);

    let error = handle
        .connect(r#"{"uri":"mock://fail","name":"bad"}"#)
        .err()
        .expect("connect fails");
    assert!(matches!(
        error,
        LoaderError::DriverRejected {
            retryable: false,
            ..
        }
    ));
}

#[test]
fn driver_rejection_carries_the_retryable_flag() {
    let temp = TempDir::new().expect("temp directory");
    let staged = stage_mock(&temp);
    let handle = load_mock(&staged);

    let error = handle
        .connect(r#"{"uri":"mock://retryable","name":"flaky"}"#)
        .err()
        .expect("connect fails");
    match error {
        LoaderError::DriverRejected {
            code,
            message,
            retryable,
        } => {
            assert_eq!(code, "connect.interrupted");
            assert_eq!(message, "mock connect interrupted");
            assert!(
                retryable,
                "the driver's retryable flag must reach the caller"
            );
        }
        other => panic!("expected DriverRejected, got {other}"),
    }
}

#[test]
fn session_keeps_generation_alive_after_handle_drop() {
    let temp = TempDir::new().expect("temp directory");
    let staged = stage_mock(&temp);
    let handle = load_mock(&staged);
    let mut session = handle
        .connect(r#"{"uri":"mock://local","name":"test"}"#)
        .expect("connect");

    drop(handle);

    let nav_page = session
        .list_nav_nodes(
            r#"{"parent":null,"database":null,"payload":null,"cursor":null,"limit":100}"#,
        )
        .expect("session still usable after handle drop");
    assert_eq!(nav_page.nodes[0].id, "database:app");

    session.disconnect().expect("disconnect");
}

#[test]
fn disconnected_session_rejects_driver_calls() {
    let temp = TempDir::new().expect("temp directory");
    let staged = stage_mock(&temp);
    let handle = load_mock(&staged);
    let mut session = handle
        .connect(r#"{"uri":"mock://local","name":"test"}"#)
        .expect("connect");
    session.disconnect().expect("disconnect");

    let error = session
        .list_nav_nodes(
            r#"{"parent":null,"database":null,"payload":null,"cursor":null,"limit":100}"#,
        )
        .expect_err("disconnected session accepted a driver call");
    assert!(matches!(
        error,
        LoaderError::DriverRejected { ref code, .. } if code == "session.not_connected"
    ));
}

#[test]
fn shutdown_rejects_live_session_then_rejects_new_calls() {
    let temp = TempDir::new().expect("temp directory");
    let staged = stage_mock(&temp);
    let handle = load_mock(&staged);
    let mut session = handle
        .connect(r#"{"uri":"mock://local","name":"test"}"#)
        .expect("connect");

    let error = handle.shutdown().expect_err("live session blocks shutdown");
    assert!(matches!(error, LoaderError::ActiveOwners { .. }));

    session.disconnect().expect("disconnect");
    handle.shutdown().expect("shutdown idle generation");

    let error = handle
        .connect(r#"{"uri":"mock://local2","name":"second"}"#)
        .err()
        .expect("new connects rejected after shutdown");
    assert!(matches!(error, LoaderError::Shutdown));
}

#[test]
fn shutdown_rejects_live_page_and_cell_batch() {
    let temp = TempDir::new().expect("temp directory");
    let staged = stage_mock(&temp);
    let handle = load_mock(&staged);
    let mut session = handle
        .connect(r#"{"uri":"mock://local","name":"test"}"#)
        .expect("connect");
    let page = session
        .open_query(
            r#"{"kind":"mongo_find","operation_id":1,"database":"app","object":"invoices","offset":0,"limit":100,"payload":{}}"#,
        )
        .expect("open query");
    let batch = session
        .render_cell_batch(page.page_id, 0, 1, 0, 1)
        .expect("render batch");

    session.disconnect().expect("disconnect releases pages");
    let error = handle.shutdown().expect_err("live batch blocks shutdown");
    assert!(matches!(error, LoaderError::ActiveOwners { .. }));

    drop(batch);
    handle.shutdown().expect("shutdown after batch release");
}

#[test]
fn queued_page_release_is_flushed_before_next_query() {
    let temp = TempDir::new().expect("temp directory");
    let staged = stage_mock(&temp);
    let handle = load_mock(&staged);
    let session = handle
        .connect(r#"{"uri":"mock://local","name":"test"}"#)
        .expect("connect");
    let shared = SharedDriverSession::new(session);

    let (displayed_page, stale_page) = {
        let mut session = shared.lock().expect("lock session");
        let displayed_page = session
            .open_query(
                r#"{"kind":"mongo_find","operation_id":1,"database":"app","object":"invoices","offset":0,"limit":100,"payload":{}}"#,
            )
            .expect("open displayed query");
        let stale_page = session
            .open_query(
                r#"{"kind":"mongo_find","operation_id":2,"database":"app","object":"invoices","offset":0,"limit":100,"payload":{}}"#,
            )
            .expect("open stale query");
        (displayed_page, stale_page)
    };

    let held = shared.lock().expect("hold session lock");
    shared.queue_page_release(stale_page.page_id);
    drop(held);

    let inspected = shared.try_lock().expect("try lock session");
    let error = handle
        .shutdown()
        .expect_err("try_lock must not flush queued releases");
    assert!(matches!(
        error,
        LoaderError::ActiveOwners {
            sessions: 1,
            pages: 2,
            ..
        }
    ));
    drop(inspected);

    let mut session = shared.lock().expect("flush queued release");
    let next_page = session
        .open_query(
            r#"{"kind":"mongo_find","operation_id":3,"database":"app","object":"invoices","offset":0,"limit":100,"payload":{}}"#,
        )
        .expect("open next query");
    assert_ne!(displayed_page.page_id, stale_page.page_id);
    assert_ne!(stale_page.page_id, next_page.page_id);

    let error = handle
        .shutdown()
        .expect_err("active session and pages block shutdown");
    assert!(matches!(
        error,
        LoaderError::ActiveOwners {
            sessions: 1,
            pages: 2,
            ..
        }
    ));

    drop(session);
    drop(shared);
    handle.shutdown().expect("shutdown idle generation");
}

#[test]
fn repeated_load_and_unload_cycles_are_stable() {
    for _ in 0..5 {
        let temp = TempDir::new().expect("temp directory");
        let staged = stage_mock(&temp);
        let handle = load_mock(&staged);
        let mut session = handle
            .connect(r#"{"uri":"mock://local","name":"test"}"#)
            .expect("connect");
        session.disconnect().expect("disconnect");
        handle.shutdown().expect("shutdown");
        drop(session);
        drop(handle);
    }
}

fn build_real_mongo_driver() -> PathBuf {
    common::built_once("dbengine-mongo", cargo_build_mongo)
}

fn cargo_build_mongo(target_dir: &Path) -> PathBuf {
    let build = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("..")
        .join("..")
        .join("scripts")
        .join("build-mongo-driver.sh");
    let output = Command::new("bash")
        .arg(build)
        .env("CARGO", env!("CARGO"))
        .arg("--target-dir")
        .arg(target_dir)
        .output()
        .expect("cargo build dbengine-mongo");
    assert!(
        output.status.success(),
        "dbengine-mongo build failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    target_dir
        .join("release")
        .join(common::cdylib_file_name("dbengine_mongo"))
}

#[test]
fn loads_real_mongo_driver_and_validates_capabilities() {
    let temp = TempDir::new().expect("temp directory");
    let source = build_real_mongo_driver();
    let staged = temp.path().join(common::driver_file_name("mongo"));
    fs::copy(source, &staged).expect("stage mongo dylib");

    let handle = DriverLoader.load(&staged).expect("load mongo driver");
    let capabilities = handle.capabilities();
    assert_eq!(capabilities.abi_version, 2);
    assert_eq!(capabilities.driver, "mongodb");
    assert!(capabilities
        .uri_schemes
        .iter()
        .any(|scheme| scheme == "mongodb"));
    assert!(capabilities.has(engine_driver_api::Capability::DatabaseCreate));
    assert!(capabilities.has(engine_driver_api::Capability::QueryMongoScript));
}

#[test]
#[ignore = "requires scripts/test-mongo-driver-ephemeral.sh with an owned loopback mongod"]
fn ephemeral_mongo_script_create_enumerate_query_and_release() {
    let uri = std::env::var("TABLEX_EPHEMERAL_MONGO_URI").expect("owned ephemeral URI is required");
    let owner =
        std::env::var("TABLEX_EPHEMERAL_MONGO_OWNER").expect("owned test marker is required");
    let pidfile =
        std::env::var("TABLEX_EPHEMERAL_MONGO_PIDFILE").expect("owned mongod pidfile is required");
    assert!(
        owner.starts_with("ephem_")
            && owner.len() >= 14
            && owner.len() < 40
            && owner
                .bytes()
                .all(|byte| byte.is_ascii_alphanumeric() || byte == b'_')
    );
    assert!(Path::new(&pidfile).exists() && pidfile.contains("tablex-mongo-ephemeral."));
    let port = uri
        .strip_prefix("mongodb://127.0.0.1:")
        .and_then(|rest| rest.split_once('/').map(|(port, _)| port))
        .and_then(|port| port.parse::<u16>().ok())
        .expect("URI must use owned loopback port");
    assert!(
        port > 1024 && port != 27017 && uri.contains("directConnection=true") && !uri.contains('@')
    );

    let temp = TempDir::new().unwrap();
    let source = PathBuf::from(
        std::env::var_os("TABLEX_EPHEMERAL_MONGO_DRIVER")
            .expect("isolated driver build is required"),
    );
    assert!(source.canonicalize().unwrap().starts_with(
        Path::new(&pidfile)
            .parent()
            .unwrap()
            .canonicalize()
            .unwrap()
    ));
    let staged = temp.path().join(common::driver_file_name("mongo"));
    fs::copy(source, &staged).unwrap();
    let handle = DriverLoader.load(&staged).unwrap();
    assert!(handle
        .capabilities()
        .has(engine_driver_api::Capability::QueryMongoScript));
    assert!(handle
        .capabilities()
        .has(engine_driver_api::Capability::DatabaseCreate));
    let mut session = handle
        .connect(&serde_json::json!({"uri": uri}).to_string())
        .unwrap();
    let database = format!("tablex_{owner}");
    let create = MutationRequest {
        operation_id: OperationId(1),
        kind: "create_database".into(),
        database: Some(database.clone()),
        object: None,
        identity: serde_json::Value::Null,
        payload: serde_json::Value::Null,
    };
    session.mutate(&create).unwrap();
    let page = session.list_nav_nodes(&serde_json::json!({"parent":null,"database":null,"payload":null,"cursor":null,"limit":200}).to_string()).unwrap();
    assert!(page.nodes.iter().any(|node| node.label == database));
    let collections = session.list_nav_nodes(&serde_json::json!({"parent":format!("database:{database}"),"database":database,"payload":null,"cursor":null,"limit":200}).to_string()).unwrap();
    assert!(collections
        .nodes
        .iter()
        .any(|node| node.label == "tablex_init"));
    session
        .mutate(&MutationRequest {
            operation_id: OperationId(2),
            kind: "insert".into(),
            database: Some(database.clone()),
            object: Some("orders".into()),
            identity: serde_json::Value::Null,
            payload: serde_json::json!({"active":true,"value":42}),
        })
        .unwrap();
    let request = |script: &str, offset: u64| {
        serde_json::json!({
            "kind":"mongo_script", "operation_id":offset + 3,
            "database":database, "object":null, "offset":offset, "limit":1,
            "payload":{"script":script}
        })
        .to_string()
    };
    let result = session
        .open_query(&request("db.orders.find({ active: true })", 0))
        .unwrap();
    assert_eq!(result.row_count, 1);
    assert_eq!(result.total_count, Some(1));
    let lease = session.acquire_page(result.page_id, 0, 1).unwrap();
    assert!(!lease.as_bytes().is_empty());
    drop(lease);
    session.release_page(result.page_id).unwrap();
    assert!(
        matches!(session.open_query(&request("db.orders.deleteMany({})", 0)),
        Err(LoaderError::DriverRejected { code, .. }) if code == "query.unsupported")
    );
    session.disconnect().unwrap();
    handle.shutdown().unwrap();
}

#[test]
#[ignore = "requires a reachable MongoDB server"]
fn connects_real_mongo_driver() {
    let temp = TempDir::new().expect("temp directory");
    let source = build_real_mongo_driver();
    let staged = temp.path().join(common::driver_file_name("mongo"));
    fs::copy(source, &staged).expect("stage mongo dylib");

    let handle = DriverLoader.load(&staged).expect("load mongo driver");
    let uri = std::env::var("TABLEX_MONGO_URI")
        .unwrap_or_else(|_| "mongodb://localhost:27017".to_string());
    let mut session = handle
        .connect(&serde_json::json!({"uri": uri}).to_string())
        .expect("connect mongo");
    assert!(session.connection_id() > 0);

    session.disconnect().expect("disconnect mongo");
    handle.shutdown().expect("shutdown mongo");
}
