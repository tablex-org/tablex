mod common;

use common::{mock_dylib, mock_entry, sha256_of};
use engine_driver_host::{
    classify_download_url, stream_to_file, DownloadError, DownloadTransport, DriverInstaller,
    DriverTarget, InstallError, InstallProgress, InstallStep, InstalledDriverStore, LoaderError,
    NoProgress, StoreError, TrustPolicy, UninstallOutcome,
};
use std::fs;
use std::io::{Cursor, Read};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Mutex;
use tempfile::TempDir;

fn installer(store: &InstalledDriverStore) -> DriverInstaller<'_> {
    DriverInstaller::new(DriverTarget::current(), store).with_signature_verification(false)
}

/// Yields at most `chunk` bytes per read, like a network body.
struct Chunked<R> {
    inner: R,
    chunk: usize,
    on_read: Option<Box<dyn FnMut() + Send>>,
}

impl<R: Read> Read for Chunked<R> {
    fn read(&mut self, buffer: &mut [u8]) -> std::io::Result<usize> {
        if let Some(on_read) = self.on_read.as_mut() {
            on_read();
        }
        let limit = buffer.len().min(self.chunk);
        self.inner.read(&mut buffer[..limit])
    }
}

fn chunked(bytes: Vec<u8>, chunk: usize) -> Chunked<Cursor<Vec<u8>>> {
    Chunked {
        inner: Cursor::new(bytes),
        chunk,
        on_read: None,
    }
}

#[test]
fn download_to_install_reports_throttled_progress_and_steps_in_order() {
    let temp = TempDir::new().expect("temp");
    let bytes = fs::read(mock_dylib()).expect("read dylib");
    let entry = mock_entry("1.0.0", "TEAM");
    let seen = Mutex::new(Vec::<InstallProgress>::new());
    let sink = |progress| seen.lock().expect("lock").push(progress);
    let store = InstalledDriverStore::new(temp.path());

    let downloaded = stream_to_file(
        &mut chunked(bytes.clone(), 4096),
        &temp.path().join("artifact.dylib"),
        bytes.len() as u64,
        &AtomicBool::new(false),
        &sink,
    )
    .expect("download");
    assert_eq!(
        downloaded.sha256,
        sha256_of(mock_dylib()),
        "hashed while streaming"
    );
    let record = installer(&store)
        .install_downloaded(&entry, &downloaded, &sink)
        .expect("install");
    installer(&store)
        .activate(&record, &sink)
        .expect("activate");

    let seen = seen.into_inner().expect("lock");
    let mut steps: Vec<InstallStep> = seen.iter().map(|progress| progress.step).collect();
    steps.dedup();
    assert_eq!(
        steps,
        [
            InstallStep::Downloading,
            InstallStep::VerifyingChecksum,
            InstallStep::VerifyingSignature,
            InstallStep::VerifyingAbi,
            InstallStep::Loading,
        ]
    );
    let downloading: Vec<u64> = seen
        .iter()
        .filter(|progress| progress.step == InstallStep::Downloading)
        .map(|progress| progress.bytes_done)
        .collect();
    let chunks = bytes.len().div_ceil(4096);
    assert_eq!(downloading.first(), Some(&0));
    assert_eq!(downloading.last(), Some(&(bytes.len() as u64)));
    assert!(
        downloading.len() < chunks / 4,
        "{} reports for {chunks} chunks is not throttled",
        downloading.len()
    );
    assert!(downloading.windows(2).all(|pair| pair[0] < pair[1]));
}

#[test]
fn cancel_mid_download_leaves_no_partial_file() {
    let temp = TempDir::new().expect("temp");
    let destination = temp.path().join("artifact.dylib");
    let cancelled = std::sync::Arc::new(AtomicBool::new(false));
    let mut reads = 0;
    let flag = cancelled.clone();
    let mut source = chunked(vec![7_u8; 1 << 20], 8192);
    source.on_read = Some(Box::new(move || {
        reads += 1;
        if reads == 5 {
            flag.store(true, Ordering::Release);
        }
    }));

    let error = stream_to_file(&mut source, &destination, 1 << 20, &cancelled, &NoProgress)
        .expect_err("cancelled");

    assert!(matches!(error, DownloadError::Cancelled), "{error:?}");
    assert!(!destination.exists(), "partial file removed");
}

#[test]
fn oversize_and_short_downloads_leave_no_file() {
    let temp = TempDir::new().expect("temp");
    let never = AtomicBool::new(false);
    for (body, expected) in [(100_usize, 50_u64), (10, 50)] {
        let destination = temp.path().join(format!("artifact-{body}"));
        let error = stream_to_file(
            &mut chunked(vec![1; body], 7),
            &destination,
            expected,
            &never,
            &NoProgress,
        )
        .expect_err("rejected");
        match (body, error) {
            (100, DownloadError::TooLarge { limit: 50 })
            | (10, DownloadError::SizeMismatch { .. }) => {}
            (_, other) => panic!("unexpected {other:?}"),
        }
        assert!(!destination.exists());
    }
}

#[test]
fn download_url_policy_is_https_only_and_file_needs_developer() {
    let strict = TrustPolicy::Strict;
    assert_eq!(
        classify_download_url("https://example.com/a.zip", strict).unwrap(),
        DownloadTransport::Http
    );
    assert_eq!(
        classify_download_url("http://localhost:8080/a", strict).unwrap(),
        DownloadTransport::Http
    );
    assert_eq!(
        classify_download_url("http://127.0.0.1/a", strict).unwrap(),
        DownloadTransport::Http
    );
    assert!(matches!(
        classify_download_url("http://example.com/a", strict),
        Err(DownloadError::InsecureUrl(_))
    ));
    assert!(matches!(
        classify_download_url("http://localhost@evil.com/a", strict),
        Err(DownloadError::InsecureUrl(_))
    ));
    assert!(matches!(
        classify_download_url("file:///tmp/a.dylib", strict),
        Err(DownloadError::LocalUrlNotAllowed(_))
    ));
    assert_eq!(
        classify_download_url("file:///tmp/a.dylib", TrustPolicy::Developer).unwrap(),
        DownloadTransport::LocalFile
    );
    assert!(matches!(
        classify_download_url("ftp://x/a", strict),
        Err(DownloadError::UnsupportedScheme(_))
    ));
}

#[test]
fn versioned_install_rollback_and_gc_keep_generations_with_owners() {
    let temp = TempDir::new().expect("temp");
    let store = InstalledDriverStore::new(temp.path());
    let installer = installer(&store);
    let dylib = mock_dylib();

    let v1 = installer
        .install_from_file(&mock_entry("1.0.0", "TEAM"), dylib)
        .expect("v1");
    assert!(v1.previous_version.is_none());
    let old_generation = installer
        .activate(&v1, &NoProgress)
        .expect("load v1")
        .handle;
    let old_session = old_generation
        .connect(r#"{"uri":"mock://local"}"#)
        .expect("session on v1");

    let v2 = installer
        .install_from_file(&mock_entry("2.0.0", "TEAM"), dylib)
        .expect("v2");
    assert_eq!(v2.previous_version.as_deref(), Some("1.0.0"));
    assert_eq!(v2.previous_path.as_ref(), Some(&v1.path));
    assert_ne!(v1.path, v2.path, "never overwritten in place");
    let v2_generation = installer
        .activate(&v2, &NoProgress)
        .expect("load v2")
        .handle;
    assert!(!std::ptr::eq(&*old_generation, &*v2_generation));

    let v3 = installer
        .install_from_file(&mock_entry("3.0.0", "TEAM"), dylib)
        .expect("v3");
    assert_eq!(v3.previous_version.as_deref(), Some("2.0.0"));
    let v3_generation = installer.activate(&v3, &NoProgress).expect("load v3");
    // v1 is older than previous but its session still owns it.
    assert_eq!(
        v3_generation.collected.as_ref().expect("gc ran"),
        &Vec::<std::path::PathBuf>::new()
    );
    assert!(
        v1.path.exists(),
        "old generation kept while a session owns it"
    );
    assert_eq!(installer.driver_owners("mock").expect("owners").sessions, 1);

    drop(old_session);
    drop(old_generation);
    let collected = installer.gc_versions("mock").expect("gc");
    assert_eq!(
        collected,
        vec![v1.path.parent().expect("dir").to_path_buf()]
    );
    assert!(!v1.path.exists());
    assert!(
        v2.path.exists() && v3.path.exists(),
        "current and previous stay"
    );

    let rolled_back = installer.rollback("mock").expect("rollback");
    assert_eq!(rolled_back.version, "2.0.0");
    assert_eq!(rolled_back.path, v2.path);
    assert_eq!(rolled_back.sha256, v2.sha256);
    assert!(rolled_back.previous_version.is_none());
    assert_eq!(store.record("mock").unwrap().unwrap(), rolled_back);
    let again = installer
        .rollback("mock")
        .expect_err("nothing left to roll back to");
    assert!(
        matches!(
            again,
            InstallError::Store(StoreError::NoPreviousVersion { .. })
        ),
        "{again:?}"
    );

    drop(v3_generation);
    drop(v2_generation);
    let collected = installer.gc_versions("mock").expect("gc after rollback");
    assert_eq!(
        collected,
        vec![v3.path.parent().expect("dir").to_path_buf()]
    );
    assert!(rolled_back.path.exists());
}

#[test]
fn same_version_with_other_bytes_goes_to_a_new_directory() {
    let temp = TempDir::new().expect("temp");
    let store = InstalledDriverStore::new(temp.path());
    let installer = installer(&store);
    let first = installer
        .install_from_file(&mock_entry("1.0.0", "TEAM"), mock_dylib())
        .expect("v1");
    fs::write(&first.path, b"different bytes").expect("tamper");

    let second = installer
        .install_from_file(&mock_entry("1.0.0", "TEAM"), mock_dylib())
        .expect("reinstall");

    assert_ne!(second.path, first.path);
    assert_eq!(
        fs::read(&first.path).expect("read"),
        b"different bytes",
        "untouched"
    );
    assert_eq!(sha256_of(&second.path), second.sha256);
    assert_eq!(second.previous_path.as_ref(), Some(&first.path));
}

#[test]
fn flat_installs_migrate_into_versioned_directories() {
    let temp = TempDir::new().expect("temp");
    let store = InstalledDriverStore::new(temp.path());
    let root = temp.path().join("Drivers/v2");
    let mut record = installer(&store)
        .install_from_file(&mock_entry("1.4.0", "TEAM"), mock_dylib())
        .expect("install");
    let versioned = record.path.clone();

    for flat in [
        root.join("mock").join(common::driver_file_name("mock")),
        root.join(common::driver_file_name("mock")),
    ] {
        fs::rename(&record.path, &flat).expect("flatten");
        fs::remove_dir(versioned.parent().expect("dir")).ok();
        record.path = flat.clone();
        store.save(record.clone()).expect("save flat record");

        let migrated = store.migrate_flat_installs().expect("migrate");

        assert_eq!(migrated.len(), 1);
        assert_eq!(
            (migrated[0].from.clone(), migrated[0].to.clone()),
            (flat.clone(), versioned.clone())
        );
        assert!(!flat.exists() && versioned.exists());
        record = store.record("mock").unwrap().unwrap();
        assert_eq!(record.path, versioned);
        assert!(store
            .migrate_flat_installs()
            .expect("idempotent")
            .is_empty());
    }
}

#[test]
fn uninstall_with_open_sessions_returns_typed_owners() {
    let temp = TempDir::new().expect("temp");
    let store = InstalledDriverStore::new(temp.path());
    let installer = installer(&store);
    let record = installer
        .install_from_file(&mock_entry("1.0.0", "TEAM"), mock_dylib())
        .expect("install");
    let handle = installer
        .activate(&record, &NoProgress)
        .expect("load")
        .handle;
    let mut session = handle
        .connect(r#"{"uri":"mock://local"}"#)
        .expect("connect");

    let outcome = installer
        .try_uninstall("mock", Some(&handle))
        .expect("outcome");
    let UninstallOutcome::InUse(owners) = outcome else {
        panic!("expected InUse, got {outcome:?}");
    };
    assert_eq!(owners.sessions, 1);
    assert!(!owners.is_empty());
    // The default API keeps refusing, with the same numbers.
    let refused = installer
        .uninstall("mock", Some(&handle))
        .expect_err("refused");
    assert!(
        matches!(
            refused,
            InstallError::Loader(LoaderError::ActiveOwners { sessions: 1, .. })
        ),
        "{refused:?}"
    );
    assert!(record.path.exists() && store.record("mock").unwrap().is_some());

    // "Disconnect and uninstall": the app ends its sessions, then retries.
    session.disconnect().expect("disconnect");
    drop(session);
    let outcome = installer
        .try_uninstall("mock", Some(&handle))
        .expect("outcome");
    assert!(
        matches!(outcome, UninstallOutcome::Removed(_)),
        "{outcome:?}"
    );
    assert!(!record.path.parent().unwrap().exists());
    assert!(store.record("mock").unwrap().is_none());
    assert_eq!(
        installer.try_uninstall("mock", None).expect("again"),
        UninstallOutcome::NotInstalled
    );
}
