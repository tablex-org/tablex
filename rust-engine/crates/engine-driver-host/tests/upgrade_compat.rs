//! Upgrade compatibility of the driver store (`<data dir>/tablex/Drivers/v2/`): the files an earlier
//! release wrote (`fixtures/compat/<version>/tablex/Drivers/v2/`, made-up records, see
//! `fixtures/compat/README.md`) are read by this build, every record and marker survives the store's
//! own writes, and the one-version-back rollback still works on them. The store is only touched
//! through its public API, in temporary copies.

use engine_driver_host::{InstalledDriverRecord, InstalledDriverStore, LegacyMigration};
use serde_json::Value;
use std::fs;
use std::path::{Path, PathBuf};

/// The releases whose fixture sets are checked. Append the version when its fixtures are added.
const VERSIONS: &[&str] = &["v0.1.10", "v0.1.11"];

/// The folder the fixture's absolute record paths are under (a made-up user's data dir); the test
/// replaces it with its temporary directory.
const FIXTURE_BASE: &str = "/Users/example/Library/Application Support/tablex";

fn fixture_v2(version: &str) -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../../../fixtures/compat")
        .join(version)
        .join("tablex/Drivers/v2")
}

/// A temporary data dir (`base`) holding the fixture's `Drivers/v2`, with the record paths moved
/// under `base` and an (empty) file at every path a record names.
fn rig(version: &str, at: impl Fn(&Path) -> PathBuf) -> (tempfile::TempDir, PathBuf, Value) {
    let temp = tempfile::tempdir().unwrap();
    let base = at(temp.path());
    let v2 = base.join("Drivers/v2");
    fs::create_dir_all(&v2).unwrap();
    for entry in fs::read_dir(fixture_v2(version)).unwrap() {
        let entry = entry.unwrap();
        let name = entry.file_name();
        let mut text = fs::read_to_string(entry.path()).unwrap();
        if name == "installed-drivers.json" {
            // The base goes into JSON strings: escaped, so a Windows path's backslashes stay valid.
            let escaped = serde_json::to_string(base.to_str().unwrap()).unwrap();
            text = text.replace(FIXTURE_BASE, &escaped[1..escaped.len() - 1]);
        }
        fs::write(v2.join(name), text).unwrap();
    }
    let records: Value = serde_json::from_str(&fs::read_to_string(v2.join("installed-drivers.json")).unwrap()).unwrap();
    for record in records.as_array().unwrap() {
        for key in ["path", "previousPath"] {
            if let Some(path) = record[key].as_str() {
                fs::create_dir_all(Path::new(path).parent().unwrap()).unwrap();
                fs::write(path, b"stub").unwrap();
            }
        }
    }
    (temp, base, records)
}

/// Every value of `old` is in `new` at the same place (new may hold more).
fn assert_survives(what: &str, path: &str, old: &Value, new: &Value) {
    match (old, new) {
        (Value::Object(old), Value::Object(new)) => {
            for (key, value) in old {
                let here = format!("{path}.{key}");
                let found = new.get(key).unwrap_or_else(|| panic!("{what}: `{here}` is gone"));
                assert_survives(what, &here, value, found);
            }
        }
        (Value::Array(old), Value::Array(new)) if old.len() == new.len() => {
            for (index, (old, new)) in old.iter().zip(new).enumerate() {
                assert_survives(what, &format!("{path}[{index}]"), old, new);
            }
        }
        _ => assert_eq!(old, new, "{what}: `{path}` changed"),
    }
}

fn written(store: &InstalledDriverStore) -> Value {
    serde_json::from_str(&fs::read_to_string(store.store_path()).unwrap()).unwrap()
}

#[test]
fn the_driver_store_of_every_release_loads_and_every_record_survives_the_stores_own_writes() {
    for version in VERSIONS {
        let (_temp, base, old) = rig(version, |temp| temp.join("tablex"));
        let store = InstalledDriverStore::new(&base);
        let records = store.load_all().unwrap_or_else(|error| panic!("{version}: {error}"));
        assert_eq!(records.len(), old.as_array().unwrap().len(), "{version}");

        let mongo = &records["mongodb"];
        assert_eq!((mongo.version.as_str(), mongo.app_abi_version, mongo.disabled), ("0.2.0", 2, false), "{version}");
        assert_eq!(mongo.uri_schemes, ["mongodb", "mongodb+srv"], "{version}");
        assert_eq!(mongo.previous_version.as_deref(), Some("0.1.0"), "{version}");
        assert!(mongo.path.starts_with(&base) && mongo.previous_path.as_ref().unwrap().starts_with(&base));
        assert!(records["sqlite"].disabled && records["sqlite"].previous_version.is_none(), "{version}");

        // A write that touches one record (any install, enable or disable does) keeps all the
        // others and every field of the file as it was.
        assert!(store.set_disabled("sqlite", false).unwrap());
        assert!(store.set_disabled("sqlite", true).unwrap());
        store.save(records["mongodb"].clone()).unwrap();
        assert_survives(&format!("{version}: installed-drivers.json"), "", &old_with_base(&old, &base), &written(&store));
        assert_eq!(store.load_all().unwrap(), records, "{version}: save then load");

        // The previous version is still the way back.
        let rolled = store.rollback("mongodb").unwrap();
        assert_eq!(rolled.version, "0.1.0", "{version}");
        assert!(rolled.previous_version.is_none());

        // A new install keeps the then-current one as the previous version.
        let mut next: InstalledDriverRecord = records["sqlite"].clone();
        next.version = "0.4.0".into();
        next.path = base.join("Drivers/v2/sqlite/0.4.0/libdbengine_sqlite.dylib");
        store.save_installed(next).unwrap();
        let sqlite = store.record("sqlite").unwrap().unwrap();
        assert_eq!(sqlite.previous_version.as_deref(), Some("0.3.0"), "{version}");
    }
}

/// The fixture's records with their paths under `base`.
fn old_with_base(old: &Value, base: &Path) -> Value {
    serde_json::from_str(&old.to_string().replace(FIXTURE_BASE, base.to_str().unwrap())).unwrap()
}

#[test]
fn the_manifest_watermark_and_the_update_check_marker_of_every_release_still_count() {
    for version in VERSIONS {
        let (_temp, base, _) = rig(version, |temp| temp.join("tablex"));
        let store = InstalledDriverStore::new(&base);
        let watermark: i64 = fs::read_to_string(store.manifest_watermark_path()).unwrap().trim().parse().unwrap();
        // A manifest older than the one this install accepted is still a replay.
        assert!(!store.accept_manifest_time(watermark - 1).unwrap(), "{version}");
        assert!(store.accept_manifest_time(watermark).unwrap(), "{version}");
        assert!(store.accept_manifest_time(watermark + 1).unwrap(), "{version}");
        assert!(!store.accept_manifest_time(watermark).unwrap(), "{version}: the watermark moved forward");
        // The marker is a file the app reads as seconds; this build must not have to rewrite it.
        let marker: i64 = fs::read_to_string(store.store_path().with_file_name("last-update-check")).unwrap().trim().parse().unwrap();
        assert!(marker > 0, "{version}");
    }
}

#[test]
fn a_store_of_every_release_at_the_legacy_location_moves_and_keeps_its_records() {
    for version in VERSIONS {
        // `<platform data dir>/Drivers` as the first releases kept it, next to `tablex/`.
        let (temp, legacy_base, old) = rig(version, |temp| temp.join("Application Support"));
        let new_base = legacy_base.join("tablex");
        // The fixture's paths are under `<base>/Drivers`: rebuilt below the legacy root.
        let store = InstalledDriverStore::new(&new_base);
        match store.migrate_legacy_location(&legacy_base).unwrap() {
            LegacyMigration::Moved { to, paths_rewritten, .. } => {
                assert_eq!(to, new_base.join("Drivers"), "{version}");
                assert_eq!(paths_rewritten, 3, "{version}: two paths of mongodb, one of sqlite");
            }
            other => panic!("{version}: {other:?}"),
        }
        let records = store.load_all().unwrap();
        assert_eq!(records.len(), old.as_array().unwrap().len(), "{version}");
        for record in records.values() {
            assert!(record.path.starts_with(new_base.join("Drivers")) && record.path.is_file(), "{version}: {}", record.path.display());
        }
        assert!(!legacy_base.join("Drivers").exists(), "{version}");
        drop(temp);
    }
}
