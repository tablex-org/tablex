//! The one-time move of the driver store from `<platform data dir>/Drivers` to
//! `<platform data dir>/tablex/Drivers`, over real files in temporary directories.

use engine_driver_host::{InstalledDriverRecord, InstalledDriverStore, LegacyMigration};
use serde_json::{json, Value};
use std::fs;
use std::path::{Path, PathBuf};
use tempfile::TempDir;

struct Rig {
    _temp: TempDir,
    /// Stands for the platform data dir (`~/Library/Application Support`).
    legacy_base: PathBuf,
    /// Stands for `<data dir>/tablex`.
    new_base: PathBuf,
}

fn rig() -> Rig {
    let temp = TempDir::new().expect("temp directory");
    let legacy_base = temp.path().join("Application Support");
    let new_base = legacy_base.join("tablex");
    fs::create_dir_all(&legacy_base).unwrap();
    Rig {
        _temp: temp,
        legacy_base,
        new_base,
    }
}

impl Rig {
    fn legacy_root(&self) -> PathBuf {
        self.legacy_base.join("Drivers")
    }

    fn new_root(&self) -> PathBuf {
        self.new_base.join("Drivers")
    }

    fn store(&self) -> InstalledDriverStore {
        InstalledDriverStore::new(&self.new_base)
    }

    /// A realistic current store: two drivers (one with a previous version), version directories,
    /// the cache and marker files.
    fn install_legacy_store(&self) {
        let v2 = self.legacy_root().join("v2");
        let mongo_new = v2.join("mongodb/0.2.0/libdbengine_mongodb.dylib");
        let mongo_old = v2.join("mongodb/0.1.0/libdbengine_mongodb.dylib");
        let sqlite = v2.join("sqlite/0.3.0/libdbengine_sqlite.dylib");
        for (path, bytes) in [(&mongo_new, "m2"), (&mongo_old, "m1"), (&sqlite, "s3")] {
            fs::create_dir_all(path.parent().unwrap()).unwrap();
            fs::write(path, bytes).unwrap();
        }
        fs::create_dir_all(v2.join("downloads")).unwrap();
        fs::write(v2.join("downloads/partial.artifact"), "half").unwrap();
        for (name, text) in [
            ("registry-cache.json", "{\"cached\":true}"),
            ("registry-cache.json.sig", "sig"),
            ("manifest-generated-at", "1760000000\n"),
            ("last-update-check", "1760000100\n"),
            ("developer-mode", "1\n"),
        ] {
            fs::write(v2.join(name), text).unwrap();
        }
        let mut mongo = record("mongodb", "0.2.0", &mongo_new);
        mongo.previous_version = Some("0.1.0".into());
        mongo.previous_path = Some(mongo_old);
        mongo.previous_sha256 = Some("old-sha".into());
        let store = InstalledDriverStore::new(&self.legacy_base);
        store.save(mongo).unwrap();
        store.save(record("sqlite", "0.3.0", &sqlite)).unwrap();
    }

    fn moved(&self, set_aside: Option<PathBuf>, paths_rewritten: usize) -> LegacyMigration {
        LegacyMigration::Moved {
            from: self.legacy_root(),
            to: self.new_root(),
            set_aside,
            paths_rewritten,
        }
    }
}

fn record(id: &str, version: &str, path: &Path) -> InstalledDriverRecord {
    InstalledDriverRecord {
        id: id.into(),
        version: version.into(),
        path: path.into(),
        sha256: format!("sha-{id}"),
        team_id: "TEAM".into(),
        installed_at: "2026-11-01T00:00:00Z".into(),
        app_abi_version: 2,
        uri_schemes: vec![id.into()],
        license: "MIT".into(),
        source_repo: "https://example.com/repo".into(),
        ..Default::default()
    }
}

/// What the Swift app and early Rust builds wrote: snake_case records, the library flat in `v2`.
fn write_snake_case_store(root: &Path) {
    let v2 = root.join("Drivers/v2");
    fs::create_dir_all(v2.join("backup-2026-10-02")).unwrap();
    fs::write(v2.join("libdbengine_mongodb.dylib"), "flat").unwrap();
    fs::write(v2.join("backup-2026-10-02/old.dylib"), "backup").unwrap();
    fs::write(
        v2.join("installed-drivers.json"),
        r#"[{"id":"mongodb","version":"1","path":"/x/libdbengine_mongodb.dylib","team_id":"T","installed_at":"then"}]"#,
    )
    .unwrap();
}

fn tree(root: &Path) -> Vec<(PathBuf, Option<Vec<u8>>)> {
    let mut found = Vec::new();
    let mut pending = vec![root.to_path_buf()];
    while let Some(dir) = pending.pop() {
        for entry in fs::read_dir(&dir).unwrap() {
            let path = entry.unwrap().path();
            if path.is_dir() {
                found.push((path.strip_prefix(root).unwrap().to_path_buf(), None));
                pending.push(path);
            } else {
                found.push((
                    path.strip_prefix(root).unwrap().to_path_buf(),
                    Some(fs::read(&path).unwrap()),
                ));
            }
        }
    }
    found.sort();
    found
}

#[test]
fn a_current_store_moves_with_its_files_and_the_record_paths_follow() {
    let rig = rig();
    rig.install_legacy_store();
    let before = tree(&rig.legacy_root());

    let outcome = rig.store().migrate_legacy_location(&rig.legacy_base).unwrap();

    // Three paths: mongodb's current and previous, and sqlite's.
    assert_eq!(outcome, rig.moved(None, 3));
    assert!(!rig.legacy_root().exists(), "the legacy directory is gone");
    let after = tree(&rig.new_root());
    let store_file = PathBuf::from("v2/installed-drivers.json");
    let lock_file = PathBuf::from("v2/installed-drivers.lock");
    let strip = |entries: &[(PathBuf, Option<Vec<u8>>)]| {
        entries
            .iter()
            .filter(|(path, _)| *path != store_file && *path != lock_file)
            .cloned()
            .collect::<Vec<_>>()
    };
    assert_eq!(strip(&after), strip(&before), "every other file moved untouched");

    let records = rig.store().load_all().unwrap();
    let mongo = &records["mongodb"];
    let v2 = rig.new_root().join("v2");
    assert_eq!(mongo.path, v2.join("mongodb/0.2.0/libdbengine_mongodb.dylib"));
    assert_eq!(
        mongo.previous_path.as_deref(),
        Some(v2.join("mongodb/0.1.0/libdbengine_mongodb.dylib").as_path())
    );
    assert_eq!(mongo.previous_version.as_deref(), Some("0.1.0"));
    assert_eq!(mongo.previous_sha256.as_deref(), Some("old-sha"));
    assert_eq!(records["sqlite"].path, v2.join("sqlite/0.3.0/libdbengine_sqlite.dylib"));
    for record in records.values() {
        assert!(record.path.is_file(), "{}", record.path.display());
    }
    let raw: Value = serde_json::from_slice(&fs::read(rig.store().store_path()).unwrap()).unwrap();
    assert!(
        raw.to_string().contains("previousPath") && !raw.to_string().contains("Application Support/Drivers"),
        "{raw}"
    );
    // The move keeps working as a store: rollback finds the previous version where it now is.
    assert_eq!(rig.store().rollback("mongodb").unwrap().version, "0.1.0");
}

#[test]
fn a_second_run_does_nothing() {
    let rig = rig();
    rig.install_legacy_store();
    rig.store().migrate_legacy_location(&rig.legacy_base).unwrap();
    let after_first = tree(&rig.new_base);

    let again = rig.store().migrate_legacy_location(&rig.legacy_base).unwrap();

    assert_eq!(again, LegacyMigration::NotNeeded);
    assert_eq!(tree(&rig.new_base), after_first);
    assert!(!rig.legacy_root().exists(), "the legacy directory is not recreated");
}

#[test]
fn nothing_to_migrate_is_a_no_op() {
    let rig = rig();
    assert_eq!(
        rig.store().migrate_legacy_location(&rig.legacy_base).unwrap(),
        LegacyMigration::NotNeeded
    );
    assert_eq!(fs::read_dir(&rig.legacy_base).unwrap().count(), 0, "nothing created");
}

#[test]
fn only_the_new_location_is_left_alone() {
    let rig = rig();
    rig.install_legacy_store();
    rig.store().migrate_legacy_location(&rig.legacy_base).unwrap();
    let before = tree(&rig.new_base);

    let outcome = rig.store().migrate_legacy_location(&rig.legacy_base).unwrap();

    assert_eq!(outcome, LegacyMigration::NotNeeded);
    assert_eq!(tree(&rig.new_base), before);
}

#[test]
fn an_old_snake_case_store_in_the_way_is_renamed_aside_untouched() {
    let rig = rig();
    rig.install_legacy_store();
    write_snake_case_store(&rig.new_base);
    let old = tree(&rig.new_root());

    let outcome = rig.store().migrate_legacy_location(&rig.legacy_base).unwrap();

    let LegacyMigration::Moved { set_aside: Some(aside), .. } = &outcome else {
        panic!("{outcome:?}");
    };
    let name = aside.file_name().unwrap().to_str().unwrap();
    let stamp = name.strip_prefix("Drivers.legacy-").expect(name);
    assert!(
        stamp.len() == 14 && stamp.bytes().all(|byte| byte.is_ascii_digit()),
        "yyyymmddHHMMSS: {name}"
    );
    assert_eq!(aside.parent().unwrap(), rig.new_base);
    assert_eq!(tree(aside), old, "the old store is exactly as it was");
    assert!(rig.store().load_all().unwrap()["sqlite"].path.starts_with(rig.new_root()));
    assert!(!rig.legacy_root().exists());
}

#[test]
fn an_aside_name_already_taken_is_not_overwritten() {
    let rig = rig();
    rig.install_legacy_store();
    write_snake_case_store(&rig.new_base);
    // Same second twice: both asides survive. Pre-create every name the clock could give.
    let first = rig.store().migrate_legacy_location(&rig.legacy_base).unwrap();
    let LegacyMigration::Moved { set_aside: Some(first_aside), .. } = first else {
        panic!("{first:?}");
    };
    rig.install_legacy_store();
    let taken = tree(&first_aside);

    let second = rig.store().migrate_legacy_location(&rig.legacy_base).unwrap();

    let LegacyMigration::Moved { set_aside: Some(second_aside), .. } = second else {
        panic!("{second:?}");
    };
    assert_ne!(second_aside, first_aside);
    assert_eq!(tree(&first_aside), taken, "the first aside is untouched");
}

#[test]
fn a_snake_case_store_at_the_legacy_location_is_not_ours_to_move() {
    let rig = rig();
    write_snake_case_store(&rig.legacy_base);
    let before = tree(&rig.legacy_root());

    let outcome = rig.store().migrate_legacy_location(&rig.legacy_base).unwrap();

    assert_eq!(outcome, LegacyMigration::NotNeeded);
    assert_eq!(tree(&rig.legacy_root()), before);
    assert!(!rig.new_root().exists());
}

#[test]
fn a_record_outside_the_legacy_root_keeps_its_path() {
    let rig = rig();
    let elsewhere = rig.legacy_base.join("elsewhere/libdbengine_redis.dylib");
    InstalledDriverStore::new(&rig.legacy_base)
        .save(record("redis", "1.0.0", &elsewhere))
        .unwrap();

    let outcome = rig.store().migrate_legacy_location(&rig.legacy_base).unwrap();

    assert_eq!(outcome, rig.moved(None, 0));
    assert_eq!(rig.store().load_all().unwrap()["redis"].path, elsewhere);
}

#[test]
fn a_failed_move_leaves_the_legacy_store_whole_and_the_old_target_in_place() {
    let rig = rig();
    rig.install_legacy_store();
    let before = tree(&rig.legacy_root());
    let original = fs::read(InstalledDriverStore::new(&rig.legacy_base).store_path()).unwrap();
    // A regular file where the `tablex` directory should be: the rename cannot happen.
    fs::write(&rig.new_base, "not a directory").unwrap();

    let error = rig
        .store()
        .migrate_legacy_location(&rig.legacy_base)
        .expect_err("cannot move");

    assert!(error.to_string().contains("I/O"), "{error}");
    let mut after = tree(&rig.legacy_root());
    let mut expected = before;
    // The exclusive lock file may be created on first use; the records are what matter.
    after.retain(|(path, _)| path != Path::new("v2/installed-drivers.lock"));
    expected.retain(|(path, _)| path != Path::new("v2/installed-drivers.lock"));
    assert_eq!(after, expected);
    assert_eq!(
        fs::read(InstalledDriverStore::new(&rig.legacy_base).store_path()).unwrap(),
        original,
        "the records name the legacy paths again"
    );
    let still_usable = InstalledDriverStore::new(&rig.legacy_base).load_all().unwrap();
    assert!(still_usable["mongodb"].path.is_file());
    assert_eq!(fs::read_to_string(&rig.new_base).unwrap(), "not a directory");
}

#[test]
fn a_store_whose_json_is_not_a_record_list_is_left_alone() {
    let rig = rig();
    let v2 = rig.legacy_root().join("v2");
    fs::create_dir_all(&v2).unwrap();
    fs::write(v2.join("installed-drivers.json"), json!({"other": 1}).to_string()).unwrap();

    assert_eq!(
        rig.store().migrate_legacy_location(&rig.legacy_base).unwrap(),
        LegacyMigration::NotNeeded
    );
    assert!(v2.join("installed-drivers.json").is_file());
}

#[test]
fn a_run_that_crashed_after_the_rewrite_is_completed_by_the_next() {
    let rig = rig();
    rig.install_legacy_store();
    // The state a crash between "records rewritten" and "directory renamed" leaves behind: the
    // legacy directory with records that already name the new root.
    let legacy = InstalledDriverStore::new(&rig.legacy_base);
    let rebased = legacy
        .load_all()
        .unwrap()
        .into_values()
        .map(|mut record| {
            let rest = record.path.strip_prefix(rig.legacy_root()).unwrap().to_path_buf();
            record.path = rig.new_root().join(rest);
            record.previous_path = record.previous_path.map(|previous| {
                rig.new_root().join(previous.strip_prefix(rig.legacy_root()).unwrap())
            });
            record
        })
        .collect::<Vec<_>>();
    for record in rebased {
        legacy.save(record).unwrap();
    }

    let outcome = rig.store().migrate_legacy_location(&rig.legacy_base).unwrap();

    assert_eq!(outcome, rig.moved(None, 0));
    for record in rig.store().load_all().unwrap().values() {
        assert!(record.path.is_file(), "{}", record.path.display());
    }
}

#[test]
fn two_processes_starting_together_move_the_store_once() {
    let rig = rig();
    rig.install_legacy_store();
    let (store, legacy_base) = (rig.store(), rig.legacy_base.clone());
    let outcomes: Vec<_> = std::thread::scope(|scope| {
        let runs: Vec<_> = (0..4)
            .map(|_| scope.spawn(|| store.migrate_legacy_location(&legacy_base).unwrap()))
            .collect();
        runs.into_iter().map(|run| run.join().unwrap()).collect()
    });

    let moved = outcomes
        .iter()
        .filter(|outcome| matches!(outcome, LegacyMigration::Moved { .. }))
        .count();
    assert_eq!(moved, 1, "{outcomes:?}");
    assert!(!rig.legacy_root().exists(), "nobody recreated the legacy directory");
    assert_eq!(rig.store().load_all().unwrap().len(), 2);
    assert_eq!(
        fs::read_dir(&rig.new_base).unwrap().count(),
        1,
        "no aside directory was made by the losers"
    );
}
