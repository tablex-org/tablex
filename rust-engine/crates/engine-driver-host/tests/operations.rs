mod common;

use engine_driver_api::{CancelRequest, MutationRequest, OperationId};
use engine_driver_host::SharedDriverSession;
use engine_driver_host::{DriverHandle, DriverLoader, LoaderError};
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
    let output = command.output().expect("build mock driver");
    assert!(
        output.status.success(),
        "mock driver build failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    target_dir
        .join("release")
        .join(common::cdylib_file_name("mock_driver"))
}

fn load_mock(temp: &TempDir) -> Arc<DriverHandle> {
    let staged = temp.path().join(common::driver_file_name("mock"));
    std::fs::copy(build_mock_driver(), &staged).expect("stage mock driver");
    DriverLoader.load(&staged).expect("load mock driver")
}

fn load_legacy_cells_mock(temp: &TempDir) -> Arc<DriverHandle> {
    let staged = temp.path().join(common::driver_file_name("mock"));
    std::fs::copy(
        build_mock_driver_with_features(Some("legacy-cells")),
        &staged,
    )
    .expect("stage legacy cell mock driver");
    DriverLoader
        .load(&staged)
        .expect("load legacy cell mock driver")
}

fn load_malformed_cells_mock(temp: &TempDir) -> Arc<DriverHandle> {
    let staged = temp.path().join(common::driver_file_name("mock"));
    std::fs::copy(
        build_mock_driver_with_features(Some("malformed-cells")),
        &staged,
    )
    .expect("stage malformed cell mock driver");
    DriverLoader
        .load(&staged)
        .expect("load malformed cell mock driver")
}

#[test]
fn operation_ids_are_unique_across_sessions() {
    let temp = TempDir::new().expect("temp directory");
    let handle = load_mock(&temp);
    let mut first = handle
        .connect(r#"{"uri":"mock://one","name":"one"}"#)
        .expect("connect first");
    let mut second = handle
        .connect(r#"{"uri":"mock://two","name":"two"}"#)
        .expect("connect second");

    let first_operation = first.begin_operation().expect("first operation");
    let second_operation = second.begin_operation().expect("second operation");

    assert_ne!(first_operation.id(), second_operation.id());
    assert_eq!(handle.active_operation_count(), 2);

    drop(first_operation);
    assert_eq!(handle.active_operation_count(), 1);
    drop(second_operation);
    assert_eq!(handle.active_operation_count(), 0);

    first.disconnect().expect("disconnect first");
    second.disconnect().expect("disconnect second");
}

#[test]
fn operation_owner_blocks_generation_shutdown_until_dropped() {
    let temp = TempDir::new().expect("temp directory");
    let handle = load_mock(&temp);
    let mut session = handle
        .connect(r#"{"uri":"mock://local","name":"test"}"#)
        .expect("connect");
    let operation = session.begin_operation().expect("operation");

    assert!(matches!(
        handle.shutdown(),
        Err(LoaderError::ActiveOwners { .. })
    ));

    drop(operation);
    session.disconnect().expect("disconnect");
    handle.shutdown().expect("shutdown after cleanup");
}

#[test]
fn handle_can_cancel_without_waiting_for_the_session_mutex() {
    let temp = TempDir::new().expect("temp directory");
    let handle = load_mock(&temp);
    let mut session = handle
        .connect(r#"{"uri":"mock://local","name":"test"}"#)
        .expect("connect");
    let operation = session.begin_operation().expect("operation");

    handle
        .cancel_operation(
            session.connection_id(),
            &CancelRequest {
                operation_id: OperationId(operation.id().0),
            },
        )
        .expect("cancel operation");

    drop(operation);
    session.disconnect().expect("disconnect");
}

#[test]
fn driver_without_page_lease_falls_back_to_cell_batches() {
    let temp = TempDir::new().expect("temp directory");
    let handle = load_legacy_cells_mock(&temp);
    assert!(!handle
        .capabilities()
        .has(engine_driver_api::Capability::QueryPageLease));
    let mut session = handle
        .connect(r#"{"uri":"mock://legacy","name":"legacy"}"#)
        .expect("connect");
    let page = session
        .open_query(r#"{"kind":"mongo_find","operation_id":1,"offset":0,"limit":1,"payload":{}}"#)
        .expect("query");
    assert!(session
        .acquire_page(page.page_id, 0, page.row_count)
        .is_err());

    let batch = session
        .render_cell_batch(
            page.page_id,
            0,
            page.row_count,
            0,
            page.columns.len() as u64,
        )
        .expect("cell batch fallback");
    assert_eq!(batch.cell_count(), 1);
    assert_eq!(batch.cell(0).expect("cell").text, "hello");
    drop(batch);

    let row = session.inspect_row(page.page_id, 0).expect("inspect row");
    assert_eq!(row.fields[0].name, "_id");

    session.release_page(page.page_id).expect("release page");
    session.disconnect().expect("disconnect");
    handle.shutdown().expect("shutdown");
}

#[test]
fn malformed_legacy_cell_batches_are_ignored_safely() {
    let temp = TempDir::new().expect("temp directory");
    let handle = load_malformed_cells_mock(&temp);
    let mut session = handle
        .connect(r#"{"uri":"mock://malformed","name":"malformed"}"#)
        .expect("connect");
    let page = session
        .open_query(r#"{"kind":"mongo_find","operation_id":1,"offset":0,"limit":1,"payload":{}}"#)
        .expect("query");
    let batch = session
        .render_cell_batch(
            page.page_id,
            0,
            page.row_count,
            0,
            page.columns.len() as u64,
        )
        .expect("cell batch");
    assert!(batch.cell(0).is_none());
    drop(batch);

    session.release_page(page.page_id).expect("release page");
    session.disconnect().expect("disconnect");
    handle.shutdown().expect("shutdown");
}

#[test]
fn query_runner_opens_a_query_without_borrowing_the_session() {
    let temp = TempDir::new().expect("temp directory");
    let handle = load_mock(&temp);
    let mut session = handle
        .connect(r#"{"uri":"mock://runner","name":"runner"}"#)
        .expect("connect");
    let runner = session.query_runner().expect("runner");
    // The runner owns what it needs: the session stays free (and mutable)
    // while the query is in flight, which is the point of the API.
    let page = runner
        .open_query(r#"{"kind":"mongo_find","operation_id":1,"offset":0,"limit":1,"payload":{}}"#)
        .expect("query through runner");
    session.adopt_page(&runner, page.page_id).expect("adopt");
    assert!(session
        .acquire_page(page.page_id, 0, page.row_count)
        .is_ok());
    session.release_page(page.page_id).expect("release page");
    session.disconnect().expect("disconnect");
    // The runner holds a driver operation: drop it before retiring the driver.
    drop(runner);
    handle.shutdown().expect("shutdown");
}

#[test]
fn adopting_a_page_after_disconnect_releases_it_instead_of_leaking() {
    let temp = TempDir::new().expect("temp directory");
    let handle = load_mock(&temp);
    let mut session = handle
        .connect(r#"{"uri":"mock://runner2","name":"runner2"}"#)
        .expect("connect");
    let runner = session.query_runner().expect("runner");
    let page = runner
        .open_query(r#"{"kind":"mongo_find","operation_id":1,"offset":0,"limit":1,"payload":{}}"#)
        .expect("query through runner");
    session
        .disconnect()
        .expect("disconnect while the query was running");
    assert!(session.adopt_page(&runner, page.page_id).is_err());
    assert!(
        session.query_runner().is_err(),
        "no new runner on a dead session"
    );
    drop(runner);
    handle.shutdown().expect("shutdown");
}

#[test]
fn a_runner_keeps_its_driver_operation_registered_and_refuses_foreign_sessions() {
    let temp = TempDir::new().expect("temp directory");
    let handle = load_mock(&temp);
    let mut first = handle
        .connect(r#"{"uri":"mock://runner-a","name":"a"}"#)
        .expect("connect a");
    let mut second = handle
        .connect(r#"{"uri":"mock://runner-b","name":"b"}"#)
        .expect("connect b");
    let before = handle.active_operation_count();
    let runner = first.query_runner().expect("runner");
    assert_eq!(
        handle.active_operation_count(),
        before + 1,
        "the runner holds an operation"
    );
    let clone = runner.clone();
    drop(runner);
    assert_eq!(
        handle.active_operation_count(),
        before + 1,
        "a clone keeps it alive"
    );
    let page = clone
        .open_query(r#"{"kind":"mongo_find","operation_id":1,"offset":0,"limit":1,"payload":{}}"#)
        .expect("query");
    // A runner of session A cannot register its page with session B.
    let error = second
        .adopt_page(&clone, page.page_id)
        .expect_err("foreign runner");
    assert!(
        matches!(&error, LoaderError::DriverRejected { code, .. } if code == "session.runner_mismatch"),
        "{error:?}"
    );
    drop(clone);
    assert_eq!(
        handle.active_operation_count(),
        before,
        "the operation is released with the last clone"
    );
    first.disconnect().expect("disconnect a");
    second.disconnect().expect("disconnect b");
    handle.shutdown().expect("shutdown");
}

#[test]
fn a_page_is_freed_through_the_runner_when_the_session_lock_is_poisoned() {
    let temp = TempDir::new().expect("temp directory");
    let handle = load_mock(&temp);
    let session = handle
        .connect(r#"{"uri":"mock://poison","name":"p"}"#)
        .expect("connect");
    let runner = session.query_runner().expect("runner");
    let shared = Arc::new(engine_driver_host::SharedDriverSession::new(session));
    let page = runner
        .open_query(r#"{"kind":"mongo_find","operation_id":1,"offset":0,"limit":1,"payload":{}}"#)
        .expect("query");
    // Poison the lock the way a panicking worker would.
    let poisoner = Arc::clone(&shared);
    let _ = std::thread::spawn(move || {
        let _guard = poisoner.lock().unwrap();
        panic!("poison");
    })
    .join();
    assert!(shared.lock().is_err(), "the session lock is poisoned");
    // The caller cannot adopt the page, so it frees it through the runner:
    // a plain FFI call that needs neither the (poisoned) lock nor the session.
    runner.release_page(page.page_id);
    drop(runner);
}

const FIND: &str = r#"{"kind":"mongo_find","operation_id":1,"offset":0,"limit":1,"payload":{}}"#;

/// Runs `work` on its own thread and fails if it does not finish while the
/// session lock is held elsewhere: the proof that it never waits for the lock.
fn finishes_while_locked<T: Send + 'static>(work: impl FnOnce() -> T + Send + 'static) -> T {
    let (tx, rx) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        let _ = tx.send(work());
    });
    rx.recv_timeout(std::time::Duration::from_secs(10))
        .expect("the call must not wait for the busy session lock")
}

#[test]
fn a_lock_free_runner_works_while_another_thread_holds_the_session_lock() {
    let temp = TempDir::new().expect("temp directory");
    let handle = load_mock(&temp);
    let session = handle
        .connect(r#"{"uri":"mock://lockfree","name":"lockfree"}"#)
        .expect("connect");
    let shared = Arc::new(engine_driver_host::SharedDriverSession::new(session));

    let (locked_tx, locked_rx) = std::sync::mpsc::channel();
    let (release_tx, release_rx) = std::sync::mpsc::channel::<()>();
    let holder = {
        let shared = Arc::clone(&shared);
        std::thread::spawn(move || {
            let _guard = shared.lock().expect("session lock");
            locked_tx.send(()).unwrap();
            let _ = release_rx.recv();
        })
    };
    locked_rx.recv().expect("lock is held");
    assert!(shared.try_lock().is_err(), "the session really is busy");

    let page_id = finishes_while_locked({
        let shared = Arc::clone(&shared);
        move || {
            let operation = shared.begin_operation().expect("operation");
            assert_ne!(
                operation.id().0,
                0,
                "host ids are never the staged placeholder"
            );
            let runner = shared.query_runner().expect("runner without the lock");
            assert_ne!(runner.operation_id(), operation.id(), "ids are unique");
            let page = runner.open_query(FIND).expect("query without the lock");
            let row = runner
                .inspect_row(page.page_id, 0)
                .expect("inspect_row without the lock");
            assert!(!row.fields.is_empty());
            let shared_row = shared
                .inspect_row(page.page_id, 0)
                .expect("SharedDriverSession::inspect_row without the lock");
            assert_eq!(shared_row.fields.len(), row.fields.len());
            page.page_id
        }
    });

    release_tx.send(()).unwrap();
    holder.join().unwrap();
    // The page opened lock-free is adopted once the lock is available, and
    // the normal locked path still works afterwards.
    let runner = shared.query_runner().expect("runner");
    let page = runner.open_query(FIND).expect("second query");
    {
        let mut session = shared.lock().expect("lock");
        session.adopt_page(&runner, page.page_id).expect("adopt");
        session.release_page(page.page_id).expect("release");
        session.disconnect().expect("disconnect");
    }
    runner.release_page(page_id);
    drop(runner);
    handle.shutdown().expect("shutdown after cleanup");
}

#[test]
fn the_driver_cannot_be_retired_under_a_live_lock_free_runner_or_operation() {
    let temp = TempDir::new().expect("temp directory");
    let handle = load_mock(&temp);
    let session = handle
        .connect(r#"{"uri":"mock://pinned","name":"pinned"}"#)
        .expect("connect");
    let shared = Arc::new(engine_driver_host::SharedDriverSession::new(session));
    let before = handle.active_operation_count();

    let runner = shared.query_runner().expect("runner");
    let operation = shared.begin_operation().expect("operation");
    assert_eq!(handle.active_operation_count(), before + 2);
    shared
        .lock()
        .expect("lock")
        .disconnect()
        .expect("disconnect");
    assert!(
        matches!(handle.shutdown(), Err(LoaderError::ActiveOwners { operations, .. }) if operations == 2),
        "a live runner and operation pin the driver"
    );
    // A disconnected session hands out nothing new, still without the lock.
    let _holder = shared.lock().expect("lock");
    assert!(shared.query_runner().is_err());
    assert!(shared.begin_operation().is_err());
    drop(_holder);

    drop(operation);
    assert!(matches!(
        handle.shutdown(),
        Err(LoaderError::ActiveOwners { operations: 1, .. })
    ));
    let kept = Arc::clone(runner.operation());
    drop(runner);
    assert_eq!(
        handle.active_operation_count(),
        before + 1,
        "the runner's operation stays registered while someone else holds it"
    );
    drop(kept);
    assert_eq!(handle.active_operation_count(), before);
    drop(shared);
    handle.shutdown().expect("shutdown after cleanup");
}

fn update_request(payload: serde_json::Value) -> MutationRequest {
    MutationRequest {
        // Replaced by the operation's host-allocated id.
        operation_id: OperationId(0),
        kind: "update".into(),
        database: Some("app".into()),
        object: Some("invoices".into()),
        identity: serde_json::json!({"_id": 1}),
        payload,
    }
}

/// Holds the session lock on another thread until the returned sender fires.
fn hold_session_lock(
    shared: &Arc<SharedDriverSession>,
) -> (std::thread::JoinHandle<()>, std::sync::mpsc::Sender<()>) {
    let (locked_tx, locked_rx) = std::sync::mpsc::channel();
    let (release_tx, release_rx) = std::sync::mpsc::channel::<()>();
    let holder = {
        let shared = Arc::clone(shared);
        std::thread::spawn(move || {
            let _guard = shared.lock().expect("session lock");
            locked_tx.send(()).unwrap();
            let _ = release_rx.recv();
        })
    };
    locked_rx.recv().expect("lock is held");
    assert!(shared.try_lock().is_err(), "the session really is busy");
    (holder, release_tx)
}

#[test]
fn a_lock_free_mutate_runs_while_another_thread_holds_the_session_lock() {
    let temp = TempDir::new().expect("temp directory");
    let handle = load_mock(&temp);
    let mut session = handle
        .connect(r#"{"uri":"mock://mutate-free","name":"mutate-free"}"#)
        .expect("connect");
    let page = session.open_query(FIND).expect("query");
    assert_eq!(page.page_id, 1, "the mock's mutate invalidates page 1");
    assert_eq!(handle.owners().pages, 1);
    let shared = Arc::new(SharedDriverSession::new(session));

    let (holder, release) = hold_session_lock(&shared);
    let result = finishes_while_locked({
        let shared = Arc::clone(&shared);
        move || {
            let operation = shared.begin_operation().expect("operation");
            let result = shared
                .mutate(&operation, update_request(serde_json::json!({"name": "x"})))
                .expect("mutate without the lock");
            // The operation stays registered until the caller drops it.
            drop(operation);
            result
        }
    });
    assert_eq!(result.affected_rows, 1);
    assert_eq!(
        handle.owners().pages,
        1,
        "the invalidation is queued while the session is busy"
    );

    release.send(()).unwrap();
    holder.join().unwrap();
    // The next lock forgets the page the mutation invalidated.
    drop(shared.lock().expect("lock"));
    assert_eq!(handle.owners().pages, 0);

    // With a free session the bookkeeping is applied immediately.
    let page = shared
        .lock()
        .expect("lock")
        .open_query(FIND)
        .expect("query");
    assert_ne!(page.page_id, 1);
    assert_eq!(handle.owners().pages, 1);
    let operation = shared.begin_operation().expect("operation");
    let result = shared
        .mutate(&operation, update_request(serde_json::Value::Null))
        .expect("mutate");
    // The mock reports page 1 again; the new page id differs, so only the
    // unknown id is ignored and the bookkeeping stays consistent.
    assert_eq!(result.invalidated_page_ids, vec![1]);
    assert_eq!(
        handle.owners().pages,
        1,
        "an id the session does not track is ignored"
    );
    drop(operation);
    shared
        .lock()
        .expect("lock")
        .disconnect()
        .expect("disconnect");
    drop(shared);
    handle.shutdown().expect("shutdown after cleanup");
}

#[test]
fn a_lock_free_mutate_is_cancellable_through_its_operation_id() {
    let temp = TempDir::new().expect("temp directory");
    let handle = load_mock(&temp);
    let session = handle
        .connect(r#"{"uri":"mock://mutate-cancel","name":"mutate-cancel"}"#)
        .expect("connect");
    let connection_id = session.connection_id();
    let shared = Arc::new(SharedDriverSession::new(session));
    let (holder, release) = hold_session_lock(&shared);

    let operation = shared.begin_operation().expect("operation");
    let operation_id = operation.id();
    let (tx, rx) = std::sync::mpsc::channel();
    std::thread::spawn({
        let shared = Arc::clone(&shared);
        move || {
            let blocking = update_request(serde_json::json!({"block_until_cancelled": true}));
            let _ = tx.send(shared.mutate(&operation, blocking));
        }
    });
    assert!(
        rx.recv_timeout(std::time::Duration::from_millis(300))
            .is_err(),
        "the mutation is still in flight (and the busy session did not stop it)"
    );

    // The host cancels by id on its own, with the session lock still held.
    handle
        .cancel_operation(connection_id, &CancelRequest { operation_id })
        .expect("cancel");
    let outcome = rx
        .recv_timeout(std::time::Duration::from_secs(10))
        .expect("the cancelled mutation returns");
    assert!(
        matches!(outcome, Err(LoaderError::DriverRejected { ref code, .. }) if code == "operation.cancelled"),
        "{outcome:?}"
    );

    release.send(()).unwrap();
    holder.join().unwrap();
    shared
        .lock()
        .expect("lock")
        .disconnect()
        .expect("disconnect");
    drop(shared);
    handle
        .shutdown()
        .expect("the finished mutation released its operation");
}

#[test]
fn a_lock_free_mutate_pins_the_driver_and_refuses_foreign_operations() {
    let temp = TempDir::new().expect("temp directory");
    let handle = load_mock(&temp);
    let first = Arc::new(SharedDriverSession::new(
        handle
            .connect(r#"{"uri":"mock://pin-a","name":"a"}"#)
            .expect("connect a"),
    ));
    let second = Arc::new(SharedDriverSession::new(
        handle
            .connect(r#"{"uri":"mock://pin-b","name":"b"}"#)
            .expect("connect b"),
    ));
    let foreign = second.begin_operation().expect("operation of b");
    let error = first
        .mutate(&foreign, update_request(serde_json::Value::Null))
        .expect_err("an operation of another session is refused");
    assert!(matches!(
        error,
        LoaderError::DriverRejected { ref code, .. } if code == "session.operation_mismatch"
    ));

    // A held operation keeps the driver from being retired.
    assert!(matches!(
        handle.shutdown(),
        Err(LoaderError::ActiveOwners { operations: 1, .. })
    ));
    drop(foreign);
    // A disconnected session refuses new work, again without the lock.
    first
        .lock()
        .expect("lock")
        .disconnect()
        .expect("disconnect");
    assert!(first.begin_operation().is_err());
    assert!(first
        .describe_object(&serde_json::json!({"object": "items"}))
        .is_err());
    second
        .lock()
        .expect("lock")
        .disconnect()
        .expect("disconnect");
}

#[test]
fn a_lock_free_describe_runs_while_another_thread_holds_the_session_lock() {
    let temp = TempDir::new().expect("temp directory");
    let staged = temp.path().join(common::driver_file_name("mock"));
    std::fs::copy(
        build_mock_driver_with_features(Some("full,describe")),
        &staged,
    )
    .expect("stage mock driver");
    let handle = DriverLoader.load(&staged).expect("load mock driver");
    let session = handle
        .connect(r#"{"uri":"mock://describe-free","name":"describe-free"}"#)
        .expect("connect");
    let connection_id = session.connection_id();
    let shared = Arc::new(SharedDriverSession::new(session));
    let (holder, release) = hold_session_lock(&shared);

    let request = serde_json::json!({"database": "app", "schema": "public", "object": "items"});
    let described = finishes_while_locked({
        let (shared, request) = (Arc::clone(&shared), request.clone());
        move || {
            shared
                .describe_object(&request)
                .expect("describe without the lock")
        }
    });
    assert_eq!(described["connection_id"], connection_id);
    assert_eq!(described["echo"], request);
    assert_eq!(
        handle.active_operation_count(),
        0,
        "the describe's own operation is released"
    );

    release.send(()).unwrap();
    holder.join().unwrap();
    shared
        .lock()
        .expect("lock")
        .disconnect()
        .expect("disconnect");
    drop(shared);
    handle.shutdown().expect("shutdown after cleanup");
}
