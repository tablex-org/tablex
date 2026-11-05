use engine_driver_host::{
    resolve_manifest_source, DriverCatalogState, DriverManifest, InstalledDriverRecord,
    InstalledDriverStore, ManifestFetcher, RegistryError, RegistryService,
};
use engine_driver_host::{DriverTarget, TargetArch, TargetOs};
use futures::executor::block_on;

const MAC_ARM64: DriverTarget = DriverTarget::new(TargetOs::Macos, TargetArch::Aarch64);
const MAC_X86_64: DriverTarget = DriverTarget::new(TargetOs::Macos, TargetArch::X86_64);
use std::collections::VecDeque;
use std::future::Future;
use std::pin::Pin;
use std::sync::Mutex;
use tempfile::TempDir;

const MANIFEST: &str = r#"{
  "generatedAt":"2026-08-31T00:00:00Z",
  "drivers":[
    {
      "id":"mongodb",
      "displayName":"MongoDB",
      "version":"2.0.0",
      "appAbiVersion":2,
      "uriSchemes":["mongodb","mongodb+srv"],
      "features":["navigator","query.find"],
      "license":"Apache-2.0",
      "sourceRepo":"https://github.com/tablex-org/tablex",
      "teamId":"ABCDE12345",
      "artifacts":[
        {"arch":"arm64","url":"https://example.com/mongodb.zip","sha256":"abc","sizeBytes":123}
      ]
    },
    {
      "id":"future-db",
      "displayName":"Future DB",
      "version":"1.0.0",
      "appAbiVersion":3,
      "uriSchemes":["future"],
      "features":[],
      "license":"MIT",
      "sourceRepo":"https://github.com/tablex-org/tablex",
      "teamId":"ABCDE12345",
      "artifacts":[
        {"arch":"arm64","url":"https://example.com/future.zip","sha256":"def","sizeBytes":456}
      ]
    }
  ],
  "tools":[]
}"#;

struct StubFetcher {
    responses: Mutex<VecDeque<Result<Vec<u8>, RegistryError>>>,
}

impl StubFetcher {
    fn new(responses: impl IntoIterator<Item = Result<Vec<u8>, RegistryError>>) -> Self {
        Self {
            responses: Mutex::new(responses.into_iter().collect()),
        }
    }
}

impl ManifestFetcher for StubFetcher {
    fn fetch<'a>(
        &'a self,
        _source: &'a str,
    ) -> Pin<Box<dyn Future<Output = Result<Vec<u8>, RegistryError>> + Send + 'a>> {
        Box::pin(async move {
            self.responses
                .lock()
                .expect("fetch responses lock")
                .pop_front()
                .expect("stub response")
        })
    }
}

fn installed_record(id: &str, version: &str) -> InstalledDriverRecord {
    InstalledDriverRecord {
        id: id.into(),
        version: version.into(),
        path: format!("/tmp/{id}/libdbengine_{id}.dylib").into(),
        sha256: "abc".into(),
        team_id: "ABCDE12345".into(),
        installed_at: "2026-08-31T00:00:00Z".into(),
        app_abi_version: 2,
        uri_schemes: vec![id.into()],
        license: "Apache-2.0".into(),
        source_repo: "https://github.com/tablex-org/tablex".into(),
        disabled: false,
        ..Default::default()
    }
}

#[test]
fn decodes_current_camel_case_manifest_and_selects_architecture() {
    let manifest: DriverManifest = serde_json::from_str(MANIFEST).expect("decode manifest");
    let mongodb = manifest
        .drivers
        .iter()
        .find(|driver| driver.id == "mongodb")
        .expect("mongodb entry");

    assert_eq!(
        manifest.generated_at.as_deref(),
        Some("2026-08-31T00:00:00Z")
    );
    assert_eq!(mongodb.app_abi_version, 2);
    assert_eq!(
        mongodb
            .artifact_for_target(&MAC_ARM64)
            .expect("arm64")
            .size_bytes,
        123
    );
    assert!(mongodb.artifact_for_target(&MAC_X86_64).is_none());
    let linux = DriverTarget::new(TargetOs::Linux, TargetArch::Aarch64);
    assert!(
        mongodb.artifact_for_target(&linux).is_none(),
        "a legacy arch-only artifact is a macOS build"
    );
}

#[test]
fn resolves_explicit_then_environment_then_default_source() {
    assert_eq!(
        resolve_manifest_source(
            Some("file:///tmp/explicit.json"),
            Some("file:///tmp/environment.json"),
            "https://example.com/default.json"
        ),
        "file:///tmp/explicit.json"
    );
    assert_eq!(
        resolve_manifest_source(
            None,
            Some("file:///tmp/environment.json"),
            "https://example.com/default.json"
        ),
        "file:///tmp/environment.json"
    );
    assert_eq!(
        resolve_manifest_source(None, None, "https://example.com/default.json"),
        "https://example.com/default.json"
    );
}

#[test]
fn catalog_reports_available_update_and_incompatible_states() {
    let temp = TempDir::new().expect("temp directory");
    let store = InstalledDriverStore::new(temp.path());
    store
        .save(installed_record("mongodb", "1.5.0"))
        .expect("save installed mongodb");
    let fetcher = StubFetcher::new([Ok(MANIFEST.as_bytes().to_vec())]);
    let registry = RegistryService::new("https://example.com/manifest.json", &fetcher, &store);

    let catalog = block_on(registry.catalog(&MAC_ARM64)).expect("catalog");
    let mongodb = catalog
        .iter()
        .find(|item| item.id == "mongodb")
        .expect("mongodb catalog item");
    let future = catalog
        .iter()
        .find(|item| item.id == "future-db")
        .expect("future catalog item");

    assert_eq!(
        mongodb.state,
        DriverCatalogState::UpdateAvailable {
            installed: "1.5.0".into(),
            latest: "2.0.0".into()
        }
    );
    assert_eq!(
        future.state,
        DriverCatalogState::Unavailable {
            reason: "Requires driver ABI 3".into()
        }
    );
}

#[test]
fn catalog_names_the_platform_without_a_build() {
    let temp = TempDir::new().expect("temp directory");
    let store = InstalledDriverStore::new(temp.path());
    let fetcher = StubFetcher::new([Ok(MANIFEST.as_bytes().to_vec())]);
    let registry = RegistryService::new("https://example.com/manifest.json", &fetcher, &store);
    let linux = DriverTarget::new(TargetOs::Linux, TargetArch::X86_64);

    let catalog = block_on(registry.catalog(&linux)).expect("catalog");

    assert_eq!(
        catalog
            .iter()
            .find(|item| item.id == "mongodb")
            .expect("mongodb")
            .state,
        DriverCatalogState::Unavailable {
            reason: "No build for Linux x86_64".into()
        }
    );
}

#[test]
fn catalog_preserves_persisted_disabled_state() {
    let temp = TempDir::new().expect("temp directory");
    let store = InstalledDriverStore::new(temp.path());
    let mut record = installed_record("mongodb", "2.0.0");
    record.disabled = true;
    store.save(record).expect("save disabled driver");
    let fetcher = StubFetcher::new([Ok(MANIFEST.as_bytes().to_vec())]);
    let registry = RegistryService::new("https://example.com/manifest.json", &fetcher, &store);

    let catalog = block_on(registry.catalog(&MAC_ARM64)).expect("catalog");

    assert!(
        catalog
            .iter()
            .find(|item| item.id == "mongodb")
            .expect("mongodb catalog item")
            .disabled
    );
}

#[test]
fn failed_fetch_uses_the_last_valid_cached_manifest() {
    let temp = TempDir::new().expect("temp directory");
    let store = InstalledDriverStore::new(temp.path());
    let successful = StubFetcher::new([Ok(MANIFEST.as_bytes().to_vec())]);
    let registry = RegistryService::new("https://example.com/manifest.json", &successful, &store);
    block_on(registry.catalog(&MAC_ARM64)).expect("prime cache");

    let failing = StubFetcher::new([Err(RegistryError::Fetch("offline".into()))]);
    let offline_registry =
        RegistryService::new("https://example.com/manifest.json", &failing, &store);
    let catalog = block_on(offline_registry.catalog(&MAC_ARM64)).expect("cached catalog");

    assert!(catalog.iter().any(|item| item.id == "mongodb"));
}

#[test]
fn failed_fetch_without_cache_keeps_installed_drivers_available() {
    let temp = TempDir::new().expect("temp directory");
    let store = InstalledDriverStore::new(temp.path());
    store
        .save(installed_record("sqlite", "2.0.0"))
        .expect("save installed sqlite");
    let failing = StubFetcher::new([Err(RegistryError::Fetch("offline".into()))]);
    let registry = RegistryService::new("https://example.com/manifest.json", &failing, &store);

    let catalog = block_on(registry.catalog(&MAC_ARM64)).expect("installed-only catalog");

    assert_eq!(catalog.len(), 1);
    assert_eq!(catalog[0].id, "sqlite");
    assert_eq!(
        catalog[0].state,
        DriverCatalogState::Installed {
            version: "2.0.0".into()
        }
    );
}
