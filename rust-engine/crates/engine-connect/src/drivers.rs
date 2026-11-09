//! Where the installed drivers live, which manifest keys and trust policy this build applies, and
//! how a driver is loaded for a connection: the part of the app's driver handling that has no UI and
//! no network client (downloads, the catalog fetcher and update checks stay in the app).
//!
//! [`load_driver`] is the one place a driver is opened for a connection: the installed record, the
//! SHA-256 it was verified with, the loader's check and the identity / URI-scheme match. Failures are
//! typed ([`DriverLoadError`]) so a front end can offer to install a missing driver.

use engine_driver_host::{
    DriverHandle, DriverLoader, DriverTarget, InstalledDriverStore, LegacyMigration,
    ManifestTrust, ManifestTrustError, SignerConfig, TrustPolicy,
};
use std::path::{Path, PathBuf};
use std::sync::Arc;

/// Production manifest: the fixed `drivers-catalog` release of the TableX
/// repository, which `release-drivers.yml` refreshes after every driver
/// release (the artifacts stay on their versioned `drivers-v*` release).
/// Not `releases/latest`: app releases share the repository and would take
/// "latest" without a manifest. `TABLEX_DRIVER_MANIFEST_URL` overrides it.
pub const DEFAULT_MANIFEST_SOURCE: &str =
    "https://github.com/tablex-org/tablex/releases/download/drivers-catalog/drivers-manifest.json";

/// Official ed25519 public keys (comma-separated base64) that sign the
/// official `drivers-manifest.json.sig`. Public keys are not secrets, so they
/// live in the source: every build from source verifies the official catalog.
/// The private half is only the `DRIVER_MANIFEST_SIGNING_KEY` secret of the
/// `release` environment. A rotation lists the old and the new key here.
const OFFICIAL_MANIFEST_PUBLIC_KEYS: &str = "d3AP0ATwJT0nMlUz5e84cAjxGPzvEYazlqhH7E+TmZI=";

/// The release keys of this build: a fork that signs its own catalog sets
/// `TABLEX_MANIFEST_PUBLIC_KEYS` at build time, which replaces the official
/// keys; otherwise the official keys apply.
const RELEASE_MANIFEST_PUBLIC_KEYS: Option<&str> = match option_env!("TABLEX_MANIFEST_PUBLIC_KEYS")
{
    Some(keys) => Some(keys),
    None => Some(OFFICIAL_MANIFEST_PUBLIC_KEYS),
};

/// DEVELOPMENT key (its private half lives outside the repository). It is
/// compiled in only for debug builds or an explicit `dev-keys` build, so an
/// official build can never trust it.
/// TODO(release): delete once a production key exists (docs/plans/2026-10-08-driver-manager.md).
#[cfg(any(debug_assertions, feature = "dev-keys"))]
const DEV_MANIFEST_PUBLIC_KEY: Option<&str> = Some("PcjR2auT+V+YvquR+phRcdjpAX33Rvh+dyHjSg6QCqM=");
#[cfg(not(any(debug_assertions, feature = "dev-keys")))]
const DEV_MANIFEST_PUBLIC_KEY: Option<&str> = None;

/// Why a build cannot verify a remote catalog; shown in the Driver Manager.
pub const NO_SIGNING_KEY_MESSAGE: &str = "This build has no driver signing key, so the remote driver catalog cannot be verified and was not fetched. This build was made with an empty TABLEX_MANIFEST_PUBLIC_KEYS; rebuild without it to use the official keys, or turn on Developer mode (\u{22EF} menu): it reads dist/drivers/drivers-manifest.json next to the app or the working directory, or the manifest named by TABLEX_DRIVER_MANIFEST_URL.";

/// The keys a build trusts: the build-time list plus the development key when
/// the build has one. Both are injected so the selection is testable.
pub fn trusted_manifest_keys(
    release_keys: Option<&str>,
    dev_key: Option<&str>,
) -> Result<Vec<[u8; 32]>, ManifestTrustError> {
    let mut keys = engine_driver_host::public_keys_from_list(release_keys.unwrap_or_default())?;
    if let Some(dev_key) = dev_key {
        keys.push(engine_driver_host::public_key_from_base64(dev_key)?);
    }
    Ok(keys)
}

/// Team ID that "Import local driver" accepts outside developer mode.
/// TODO(release): set to the TableX signing Team ID; empty means no local
/// import is possible in strict mode (the verifier refuses an unpinned team).
pub const PINNED_DRIVER_TEAM_ID: &str = "";

/// Authenticode publisher that "Import local driver" accepts on Windows
/// outside developer mode. Empty means no publisher is pinned there, and the
/// signed manifest's SHA-256 is the only check (Linux has no native layer).
/// TODO(release): set once Windows drivers are signed.
pub const PINNED_DRIVER_WINDOWS_PUBLISHER: &str = "";

/// Publisher identities pinned per OS for locally imported drivers.
pub fn pinned_signers() -> SignerConfig {
    SignerConfig {
        macos_team_id: PINNED_DRIVER_TEAM_ID.into(),
        windows_publisher: PINNED_DRIVER_WINDOWS_PUBLISHER.into(),
        // A local file has no signed manifest behind it.
        manifest_attested: false,
    }
}

/// Where the catalog comes from: `TABLEX_DRIVER_MANIFEST_URL`, else a local
/// `dist/drivers/drivers-manifest.json` (what `dev-drivers.sh` writes) for
/// debug builds and for Developer mode, else the official release.
pub fn manifest_source(policy: TrustPolicy) -> String {
    let local = cfg!(debug_assertions) || policy == TrustPolicy::Developer;
    resolve_manifest_source(
        std::env::var("TABLEX_DRIVER_MANIFEST_URL").ok(),
        local.then(local_manifest_candidates).into_iter().flatten(),
    )
}

const LOCAL_MANIFEST: &str = "dist/drivers/drivers-manifest.json";

/// The working directory, then the folders above the executable (a build in
/// `target/<profile>/` sits two levels below the repository root).
fn local_manifest_candidates() -> Vec<PathBuf> {
    let exe = std::env::current_exe().ok();
    std::env::current_dir()
        .ok()
        .into_iter()
        .chain(
            exe.iter()
                .flat_map(|exe| exe.ancestors().skip(1).take(4).map(Path::to_path_buf)),
        )
        .map(|directory| directory.join(LOCAL_MANIFEST))
        .collect()
}

/// `explicit` wins; else the first existing local candidate; else the
/// official source. A strict release build passes no candidates.
fn resolve_manifest_source(
    explicit: Option<String>,
    local_candidates: impl IntoIterator<Item = PathBuf>,
) -> String {
    explicit
        .or_else(|| {
            local_candidates
                .into_iter()
                .filter(|path| path.is_file())
                .find_map(|path| url::Url::from_file_path(path).ok())
                .map(|url| url.to_string())
        })
        .unwrap_or_else(|| DEFAULT_MANIFEST_SOURCE.to_string())
}

/// The platform this build runs on; the manifest artifact is picked by it.
pub fn current_target() -> DriverTarget {
    DriverTarget::current()
}

/// Environment variable that replaces the directory the drivers are installed under (tests,
/// portable installs). Only an absolute path counts, see [`driver_data_dir_override`].
pub const DRIVER_DATA_DIR_ENV: &str = "TABLEX_DRIVER_DATA_DIR";

/// The directory `TABLEX_DRIVER_DATA_DIR` names, or `None` when it is unset, empty or relative: the
/// same rule as `TABLEX_DATA_DIR` (`engine_storage::paths::data_dir_override`), because a relative
/// path would mean a different driver store for every working directory.
pub fn driver_data_dir_override() -> Option<PathBuf> {
    absolute_dir(std::env::var_os(DRIVER_DATA_DIR_ENV))
}

fn absolute_dir(value: Option<std::ffi::OsString>) -> Option<PathBuf> {
    value.map(PathBuf::from).filter(|path| path.is_absolute())
}

/// Where the driver store is for this run (`<path>/Drivers/v2`), and what finding it did.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DriverDataDir {
    pub path: PathBuf,
    /// The legacy store was moved to its new place just now (an Info line for the log).
    pub moved: Option<String>,
    /// The move failed and `path` is the legacy location for this run (a Warning line).
    pub warning: Option<String>,
}

/// The base directory of the driver store: `TABLEX_DRIVER_DATA_DIR` when set, else
/// `<data dir>/tablex` (the folder every other TableX file is in, `TABLEX_DATA_DIR` honoured).
///
/// Releases up to 0.1.0 kept the store in `<platform data dir>/Drivers/v2`; unless one of the two
/// variables is set (then the run is isolated and must not take the real store along) a store of
/// the current format there is moved to the new place first
/// ([`InstalledDriverStore::migrate_legacy_location`]). Call it once at startup, before the store
/// is opened; both front ends do, so they agree on the path.
pub fn resolve_driver_data_dir() -> DriverDataDir {
    resolve_with(
        driver_data_dir_override(),
        engine_storage::paths::data_dir_override(),
        dirs::data_dir(),
    )
}

fn resolve_with(
    driver_override: Option<PathBuf>,
    data_override: Option<PathBuf>,
    platform_data_dir: Option<PathBuf>,
) -> DriverDataDir {
    let mut resolved = DriverDataDir {
        path: PathBuf::new(),
        moved: None,
        warning: None,
    };
    if let Some(path) = driver_override {
        resolved.path = path;
        return resolved;
    }
    let new_base = data_override
        .clone()
        .or_else(|| platform_data_dir.clone())
        .unwrap_or_else(|| PathBuf::from("."))
        .join(crate::platform::APP_DIR);
    resolved.path = new_base.clone();
    let (None, Some(legacy_base)) = (data_override, platform_data_dir) else {
        return resolved;
    };
    match InstalledDriverStore::new(&new_base).migrate_legacy_location(&legacy_base) {
        Ok(LegacyMigration::NotNeeded) => {}
        Ok(LegacyMigration::Moved {
            from,
            to,
            set_aside,
            ..
        }) => {
            let aside = set_aside.map_or_else(String::new, |aside| {
                format!(" (the folder that was there is now {})", aside.display())
            });
            resolved.moved = Some(format!(
                "Moved the driver store from {} to {}{aside}",
                from.display(),
                to.display()
            ));
        }
        Err(error) => {
            resolved.warning = Some(format!(
                "Could not move the driver store from {} to {}: {error}. Using the old location for this run.",
                legacy_base.join("Drivers").display(),
                new_base.join("Drivers").display(),
            ));
            resolved.path = legacy_base;
        }
    }
    resolved
}

/// Developer mode only for the exact value `1`; anything else is strict.
pub fn trust_policy_from(value: Option<&str>) -> TrustPolicy {
    match value {
        Some("1") => TrustPolicy::Developer,
        _ => TrustPolicy::Strict,
    }
}

/// The effective policy: the user's persisted Developer mode, or (only when
/// `env_honoured`, i.e. debug builds) `TABLEX_DRIVER_DEV=1`.
pub fn resolve_trust_policy(
    env_value: Option<&str>,
    user_enabled: bool,
    env_honoured: bool,
) -> TrustPolicy {
    if user_enabled {
        TrustPolicy::Developer
    } else if env_honoured {
        trust_policy_from(env_value)
    } else {
        TrustPolicy::Strict
    }
}

/// The policy for the current build and the user's Developer mode choice.
/// `TABLEX_DRIVER_DEV` is read once, at startup, and only in debug builds.
pub fn effective_trust_policy(user_enabled: bool) -> TrustPolicy {
    static ENV: std::sync::OnceLock<Option<String>> = std::sync::OnceLock::new();
    let env = ENV.get_or_init(|| std::env::var("TABLEX_DRIVER_DEV").ok());
    resolve_trust_policy(env.as_deref(), user_enabled, cfg!(debug_assertions))
}

fn developer_mode_path(store: &InstalledDriverStore) -> PathBuf {
    store
        .base_directory()
        .join("Drivers")
        .join("v2")
        .join("developer-mode")
}

/// Whether the user turned Developer mode on (a marker file in the driver
/// store: it is a trust setting of the drivers, and survives restarts).
pub fn read_developer_mode(store: &InstalledDriverStore) -> bool {
    std::fs::read_to_string(developer_mode_path(store)).is_ok_and(|text| text.trim() == "1")
}

pub fn write_developer_mode(store: &InstalledDriverStore, enabled: bool) -> std::io::Result<()> {
    let path = developer_mode_path(store);
    if !enabled {
        return match std::fs::remove_file(path) {
            Err(error) if error.kind() != std::io::ErrorKind::NotFound => Err(error),
            _ => Ok(()),
        };
    }
    if let Some(directory) = path.parent() {
        std::fs::create_dir_all(directory)?;
    }
    std::fs::write(path, "1\n")
}

/// Manifest verification against the keys compiled into this build.
pub fn manifest_trust(policy: TrustPolicy) -> Result<ManifestTrust, ManifestTrustError> {
    let keys = trusted_manifest_keys(RELEASE_MANIFEST_PUBLIC_KEYS, DEV_MANIFEST_PUBLIC_KEY)?;
    ManifestTrust::new(&keys, policy)
}

/// Refuses a catalog fetch that could never verify: `trust` has no key and
/// `source` is not a local manifest that developer mode accepts unsigned.
pub fn ensure_catalog_verifiable(trust: &ManifestTrust, source: &str) -> Result<(), String> {
    if !trust.has_keys() && trust.requires_signature(source) {
        return Err(NO_SIGNING_KEY_MESSAGE.to_string());
    }
    Ok(())
}

pub fn validate_loaded_driver(
    driver_id: &str,
    uri: &str,
    capabilities: &engine_driver_api::DriverCapabilities,
) -> Result<(), String> {
    if capabilities.driver != driver_id {
        return Err(format!(
            "loaded driver '{}' does not match connection driver '{driver_id}'",
            capabilities.driver
        ));
    }
    let scheme = uri
        .split_once("://")
        .map(|(scheme, _)| scheme)
        .filter(|scheme| !scheme.is_empty())
        .ok_or_else(|| "Invalid connection URI".to_string())?;
    if !capabilities
        .uri_schemes
        .iter()
        .any(|candidate| candidate.eq_ignore_ascii_case(scheme))
    {
        return Err(format!(
            "driver '{driver_id}' does not support URI scheme '{scheme}'"
        ));
    }
    Ok(())
}

/// Why a driver could not be opened for a connection. `Display` is the text the app has always shown.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum DriverLoadError {
    /// No installed record for the driver: the front end can offer to install it.
    #[error("Driver '{driver_id}' is not installed; open Driver Manager to install it")]
    NotInstalled { driver_id: String },
    #[error("Driver '{driver_id}' is disabled; enable it in Driver Manager")]
    Disabled { driver_id: String },
    #[error("Driver '{driver_id}' artifact is missing at {}", path.display())]
    ArtifactMissing { driver_id: String, path: PathBuf },
    /// The driver store could not be read.
    #[error("{0}")]
    Store(String),
    /// The artifact failed the integrity check or did not load.
    #[error("{0}")]
    Load(String),
    /// The driver loaded but is not the one for the connection or does not serve its URI scheme.
    #[error("{0}")]
    Mismatch(String),
}

/// Opens the installed driver `driver_id` for a connection with `uri`: the record must exist, be
/// enabled and point at a file whose SHA-256 is the one recorded when it was verified; the loaded
/// driver must be that driver and serve the URI's scheme.
pub fn load_driver(
    store: &InstalledDriverStore,
    driver_id: &str,
    uri: &str,
) -> Result<Arc<DriverHandle>, DriverLoadError> {
    let records = store
        .load_all()
        .map_err(|error| DriverLoadError::Store(error.to_string()))?;
    let record = records
        .get(driver_id)
        .ok_or_else(|| DriverLoadError::NotInstalled {
            driver_id: driver_id.to_owned(),
        })?;
    if record.disabled {
        return Err(DriverLoadError::Disabled {
            driver_id: driver_id.to_owned(),
        });
    }
    if !record.path.exists() {
        return Err(DriverLoadError::ArtifactMissing {
            driver_id: driver_id.to_owned(),
            path: record.path.clone(),
        });
    }
    let handle = DriverLoader
        .load_verified(&record.path, &record.sha256)
        .map_err(|error| DriverLoadError::Load(error.to_string()))?;
    validate_loaded_driver(driver_id, uri, handle.capabilities())
        .map_err(DriverLoadError::Mismatch)?;
    Ok(handle)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicU64, Ordering};

    static TEST_SEQUENCE: AtomicU64 = AtomicU64::new(1);

    fn temp_store() -> (InstalledDriverStore, PathBuf) {
        let dir = std::env::temp_dir().join(format!(
            "engine-connect-drivers-{}-{}",
            std::process::id(),
            TEST_SEQUENCE.fetch_add(1, Ordering::Relaxed)
        ));
        std::fs::create_dir_all(&dir).expect("create temp dir");
        (InstalledDriverStore::new(&dir), dir)
    }

    /// A platform data dir holding a store in the 0.1.0 place (`<platform>/Drivers/v2`).
    fn platform_with_legacy_store() -> (tempfile::TempDir, PathBuf) {
        let temp = tempfile::tempdir().expect("temp dir");
        let platform = temp.path().join("Application Support");
        let library = platform.join("Drivers/v2/sqlite/1.0.0/libdbengine_sqlite.dylib");
        std::fs::create_dir_all(library.parent().unwrap()).unwrap();
        std::fs::write(&library, "lib").unwrap();
        InstalledDriverStore::new(&platform)
            .save(engine_driver_host::InstalledDriverRecord {
                id: "sqlite".into(),
                version: "1.0.0".into(),
                path: library,
                ..Default::default()
            })
            .unwrap();
        (temp, platform)
    }

    #[test]
    fn the_store_defaults_to_the_app_folder_and_a_0_1_0_store_is_moved_there() {
        let (_temp, platform) = platform_with_legacy_store();

        let resolved = resolve_with(None, None, Some(platform.clone()));

        assert_eq!(resolved.path, platform.join("tablex"));
        assert!(resolved.warning.is_none());
        assert!(resolved.moved.is_some_and(|text| text.contains("Moved the driver store")));
        assert!(!platform.join("Drivers").exists());
        let store = InstalledDriverStore::new(&resolved.path);
        let record = store.record("sqlite").unwrap().expect("record kept");
        assert_eq!(
            record.path,
            platform.join("tablex/Drivers/v2/sqlite/1.0.0/libdbengine_sqlite.dylib")
        );
        assert!(record.path.is_file());
        // Said once: the next start finds nothing to move.
        let again = resolve_with(None, None, Some(platform.clone()));
        assert_eq!((again.path, again.moved, again.warning), (resolved.path, None, None));
    }

    #[test]
    fn an_override_variable_means_an_isolated_run_that_never_takes_the_real_store() {
        let (_temp, platform) = platform_with_legacy_store();
        let other = platform.parent().unwrap().join("portable");

        let driver_dir = resolve_with(Some(other.clone()), None, Some(platform.clone()));
        assert_eq!((driver_dir.path.as_path(), driver_dir.moved), (other.as_path(), None));

        let data_dir = resolve_with(None, Some(other.clone()), Some(platform.clone()));
        assert_eq!(data_dir.path, other.join("tablex"), "TABLEX_DATA_DIR moves the store too");
        assert_eq!((data_dir.moved, data_dir.warning), (None, None));

        assert!(platform.join("Drivers/v2/installed-drivers.json").is_file(), "untouched");
        assert!(!other.exists(), "nothing created");
    }

    #[test]
    fn a_move_that_fails_keeps_the_legacy_location_for_this_run_with_a_warning() {
        let (_temp, platform) = platform_with_legacy_store();
        // A file where the `tablex` folder should be.
        std::fs::write(platform.join("tablex"), "in the way").unwrap();

        let resolved = resolve_with(None, None, Some(platform.clone()));

        assert_eq!(resolved.path, platform, "the store is still opened where it is");
        assert!(resolved.moved.is_none());
        assert!(resolved.warning.is_some_and(|text| text.contains("Using the old location")));
        let record = InstalledDriverStore::new(&resolved.path).record("sqlite").unwrap().unwrap();
        assert!(record.path.is_file());
    }

    #[test]
    fn with_no_platform_data_dir_the_store_is_under_the_working_directory() {
        let resolved = resolve_with(None, None, None);
        assert_eq!(resolved.path, PathBuf::from(".").join("tablex"));
        assert!(resolved.moved.is_none() && resolved.warning.is_none());
    }

    #[test]
    fn only_an_absolute_value_replaces_the_driver_data_dir() {
        let absolute = std::env::temp_dir();
        assert_eq!(
            absolute_dir(Some(absolute.clone().into_os_string())),
            Some(absolute)
        );
        for ignored in [None, Some(""), Some("."), Some("relative/dir")] {
            assert_eq!(absolute_dir(ignored.map(Into::into)), None, "{ignored:?}");
        }
    }

    #[test]
    fn a_driver_that_is_not_installed_is_a_typed_error_with_the_usual_text() {
        let (store, dir) = temp_store();
        let error = load_driver(&store, "postgresql", "postgres://h/d")
            .err()
            .expect("not installed");
        assert_eq!(
            error,
            DriverLoadError::NotInstalled {
                driver_id: "postgresql".into()
            }
        );
        assert_eq!(
            error.to_string(),
            "Driver 'postgresql' is not installed; open Driver Manager to install it"
        );
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn a_disabled_or_missing_artifact_is_refused_before_anything_loads() {
        let (store, dir) = temp_store();
        let record = engine_driver_host::InstalledDriverRecord {
            id: "sqlite".into(),
            path: dir.join("nowhere.dylib"),
            ..Default::default()
        };
        store.save(record.clone()).expect("save");
        let missing = load_driver(&store, "sqlite", "sqlite:///x")
            .err()
            .expect("missing");
        assert!(
            matches!(missing, DriverLoadError::ArtifactMissing { .. }),
            "{missing:?}"
        );
        assert!(
            missing.to_string().contains("artifact is missing at"),
            "{missing}"
        );
        store
            .save(engine_driver_host::InstalledDriverRecord {
                disabled: true,
                ..record
            })
            .expect("save");
        let disabled = load_driver(&store, "sqlite", "sqlite:///x")
            .err()
            .expect("disabled");
        assert_eq!(
            disabled.to_string(),
            "Driver 'sqlite' is disabled; enable it in Driver Manager"
        );
        let _ = std::fs::remove_dir_all(dir);
    }

    const DEV_KEY: &str = "PcjR2auT+V+YvquR+phRcdjpAX33Rvh+dyHjSg6QCqM=";

    /// 32 bytes of 0x01 / 0x02 (only the length is checked when parsing).
    const KEY_1: &str = "AQEBAQEBAQEBAQEBAQEBAQEBAQEBAQEBAQEBAQEBAQE=";
    const KEY_2: &str = "AgICAgICAgICAgICAgICAgICAgICAgICAgICAgICAgI=";

    #[test]
    fn only_the_exact_value_one_enables_developer_mode() {
        assert_eq!(trust_policy_from(Some("1")), TrustPolicy::Developer);
        for value in [None, Some(""), Some("0"), Some("true"), Some("yes")] {
            assert_eq!(trust_policy_from(value), TrustPolicy::Strict, "{value:?}");
        }
    }

    #[test]
    fn the_catalog_source_is_explicit_then_local_then_official() {
        let dir =
            std::env::temp_dir().join(format!("tablex-manifest-source-{}", uuid::Uuid::new_v4()));
        let local = dir.join(LOCAL_MANIFEST);
        std::fs::create_dir_all(local.parent().unwrap()).unwrap();
        std::fs::write(&local, "{}").unwrap();
        let missing = dir.join("nowhere").join(LOCAL_MANIFEST);
        let local_url = url::Url::from_file_path(&local).unwrap().to_string();

        // A strict release build offers no candidates: always the official one.
        assert_eq!(resolve_manifest_source(None, []), DEFAULT_MANIFEST_SOURCE);
        // Developer mode / debug: the first candidate that exists.
        assert_eq!(
            resolve_manifest_source(None, [missing.clone(), local.clone()]),
            local_url
        );
        assert_eq!(
            resolve_manifest_source(None, [missing]),
            DEFAULT_MANIFEST_SOURCE
        );
        // The environment variable always wins.
        assert_eq!(
            resolve_manifest_source(Some("https://example.com/m.json".into()), [local]),
            "https://example.com/m.json"
        );
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn compiled_in_manifest_keys_are_valid_ed25519_keys() {
        assert!(manifest_trust(TrustPolicy::Strict).is_ok());
    }

    #[test]
    fn a_build_from_source_trusts_the_official_key_and_a_fork_can_replace_it() {
        let official =
            trusted_manifest_keys(Some(OFFICIAL_MANIFEST_PUBLIC_KEYS), None).expect("official");
        assert_eq!(official.len(), 1);
        if option_env!("TABLEX_MANIFEST_PUBLIC_KEYS").is_none() {
            assert_eq!(
                RELEASE_MANIFEST_PUBLIC_KEYS,
                Some(OFFICIAL_MANIFEST_PUBLIC_KEYS)
            );
            let trust = manifest_trust(TrustPolicy::Strict).expect("trust");
            assert!(ensure_catalog_verifiable(&trust, DEFAULT_MANIFEST_SOURCE).is_ok());
        }
    }

    #[test]
    fn this_build_trusts_the_dev_key_only_when_it_compiles_one_in() {
        let dev = cfg!(any(debug_assertions, feature = "dev-keys"));
        assert_eq!(DEV_MANIFEST_PUBLIC_KEY.is_some(), dev);
        let keys = trusted_manifest_keys(None, DEV_MANIFEST_PUBLIC_KEY).expect("keys");
        assert_eq!(keys.len(), usize::from(dev));
    }

    #[test]
    fn a_release_build_without_keys_trusts_none() {
        assert!(trusted_manifest_keys(None, None).expect("none").is_empty());
        assert!(trusted_manifest_keys(Some("  "), None)
            .expect("blank")
            .is_empty());
    }

    #[test]
    fn several_release_keys_are_all_trusted_for_rotation() {
        let list = format!("{KEY_1}, {KEY_2}");
        assert_eq!(
            trusted_manifest_keys(Some(&list), None).expect("keys"),
            vec![[1; 32], [2; 32]]
        );
    }

    #[test]
    fn the_dev_key_is_added_after_the_release_keys_when_the_build_has_one() {
        let keys = trusted_manifest_keys(Some(KEY_1), Some(DEV_KEY)).expect("keys");
        assert_eq!(keys.len(), 2);
        assert_eq!(keys[0], [1; 32]);
        assert_eq!(
            keys[1],
            engine_driver_host::public_key_from_base64(DEV_KEY).expect("dev key")
        );
    }

    #[test]
    fn an_invalid_key_is_a_typed_error_not_a_silently_dropped_key() {
        for list in ["not-base64!", "AAAA", &format!("{KEY_1},")] {
            assert!(
                matches!(
                    trusted_manifest_keys(Some(list), None),
                    Err(ManifestTrustError::InvalidKey(_))
                ),
                "{list}"
            );
        }
        assert!(matches!(
            trusted_manifest_keys(None, Some("nope")),
            Err(ManifestTrustError::InvalidKey(_))
        ));
    }

    #[test]
    fn a_build_without_a_key_refuses_a_remote_catalog_before_fetching() {
        let none = ManifestTrust::new(&[], TrustPolicy::Strict).expect("trust");
        let refusal = ensure_catalog_verifiable(&none, "https://github.com/x/y/m.json")
            .expect_err("no key, remote source");
        assert!(
            refusal.starts_with("This build has no driver signing key"),
            "{refusal}"
        );
        assert!(refusal.contains("Developer mode"), "{refusal}");
        assert!(
            ensure_catalog_verifiable(&none, "file:///tmp/m.json").is_err(),
            "strict: local is signed too"
        );

        let developer = ManifestTrust::new(&[], TrustPolicy::Developer).expect("trust");
        assert!(
            ensure_catalog_verifiable(&developer, "file:///tmp/m.json").is_ok(),
            "local manifests need no key"
        );
        assert!(
            ensure_catalog_verifiable(&developer, "https://h/m.json").is_err(),
            "remote is still verified"
        );

        let key = engine_driver_host::public_key_from_base64(DEV_KEY).expect("key");
        let keyed = ManifestTrust::new(&[key], TrustPolicy::Strict).expect("trust");
        assert!(ensure_catalog_verifiable(&keyed, "https://h/m.json").is_ok());
    }

    #[test]
    fn developer_mode_comes_from_the_user_or_from_the_env_in_debug_builds_only() {
        use TrustPolicy::{Developer, Strict};
        // (env value, user enabled, env honoured) -> policy
        let cases = [
            (None, false, true, Strict),
            (None, false, false, Strict),
            (Some("1"), false, true, Developer),
            (Some("1"), false, false, Strict),
            (Some("true"), false, true, Strict),
            (None, true, true, Developer),
            (None, true, false, Developer),
            (Some("1"), true, false, Developer),
        ];
        for (env, user, honoured, expected) in cases {
            assert_eq!(
                resolve_trust_policy(env, user, honoured),
                expected,
                "{env:?} {user} {honoured}"
            );
        }
    }

    #[test]
    fn the_developer_mode_choice_round_trips_through_the_driver_store() {
        let (store, dir) = temp_store();
        assert!(!read_developer_mode(&store), "off by default");
        write_developer_mode(&store, true).expect("enable");
        assert!(read_developer_mode(&store));
        assert!(
            read_developer_mode(&InstalledDriverStore::new(&dir)),
            "persisted on disk"
        );
        write_developer_mode(&store, false).expect("disable");
        assert!(!read_developer_mode(&store));
        write_developer_mode(&store, false).expect("disabling twice is fine");
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn validates_loaded_driver_identity_and_uri_scheme() {
        let mut capabilities =
            engine_driver_api::DriverCapabilities::new("mongodb", "MongoDB", Vec::new());
        capabilities.uri_schemes = vec!["mongodb".into()];

        assert!(validate_loaded_driver("mongodb", "mongodb://localhost", &capabilities).is_ok());
        assert!(validate_loaded_driver(
            "mongodb",
            "mongodb://root@192.0.2.11:27017,192.0.2.12:27017,192.0.2.13:27017/",
            &capabilities
        )
        .is_ok());
        assert!(validate_loaded_driver("mysql", "mongodb://localhost", &capabilities).is_err());
        assert!(validate_loaded_driver("mongodb", "mysql://localhost", &capabilities).is_err());
    }
}
