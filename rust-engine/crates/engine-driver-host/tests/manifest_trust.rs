mod common;

use base64::engine::general_purpose::STANDARD;
use base64::Engine;
use common::{mock_dylib, mock_entry};
use ed25519_dalek::{Signer, SigningKey};
use engine_driver_host::{
    public_key_from_base64, DriverCatalogState, DriverCategory, DriverInstaller, DriverManifest,
    DriverTarget, DriverVerificationError, InstallError, InstalledDriverStore, ManifestFetcher,
    ManifestTrust, ManifestTrustError, RegistryError, RegistryService, TrustPolicy,
};
use engine_driver_host::{TargetArch, TargetOs};
use futures::executor::block_on;
use std::collections::VecDeque;
use std::future::Future;
use std::pin::Pin;
use std::sync::Mutex;
use tempfile::TempDir;

/// The fixtures' `"arch":"arm64"` artifacts are legacy macOS entries.
const MAC_ARM64: DriverTarget = DriverTarget::new(TargetOs::Macos, TargetArch::Aarch64);

const HTTPS: &str = "https://example.com/drivers-manifest.json";
const FILE: &str = "file:///tmp/dist/drivers-manifest.json";

fn signing_key(seed: u8) -> SigningKey {
    SigningKey::from_bytes(&[seed; 32])
}

fn trust(policy: TrustPolicy) -> ManifestTrust {
    ManifestTrust::new(&[signing_key(1).verifying_key().to_bytes()], policy).expect("trust")
}

fn manifest(generated_at: &str) -> Vec<u8> {
    format!(r#"{{"schemaVersion":2,"generatedAt":"{generated_at}","drivers":[]}}"#).into_bytes()
}

fn sign(key: &SigningKey, bytes: &[u8]) -> Vec<u8> {
    format!("{}\n", STANDARD.encode(key.sign(bytes).to_bytes())).into_bytes()
}

fn store() -> (TempDir, InstalledDriverStore) {
    let temp = TempDir::new().expect("temp");
    let store = InstalledDriverStore::new(temp.path());
    (temp, store)
}

#[test]
fn good_signature_is_accepted() {
    let (_temp, store) = store();
    let body = manifest("2026-10-08T12:00:00Z");
    let parsed = trust(TrustPolicy::Strict)
        .verify(&store, HTTPS, &body, Some(&sign(&signing_key(1), &body)))
        .expect("verified");
    assert_eq!(parsed.generated_at.as_deref(), Some("2026-10-08T12:00:00Z"));
}

#[test]
fn tampered_wrong_key_and_malformed_signatures_are_rejected() {
    let (_temp, store) = store();
    let trust = trust(TrustPolicy::Strict);
    let body = manifest("2026-10-08T12:00:00Z");
    let signature = sign(&signing_key(1), &body);

    let mut tampered = body.clone();
    tampered.extend_from_slice(b" ");
    assert!(matches!(
        trust.verify(&store, HTTPS, &tampered, Some(&signature)),
        Err(ManifestTrustError::SignatureInvalid)
    ));
    assert!(matches!(
        trust.verify(&store, HTTPS, &body, Some(&sign(&signing_key(2), &body))),
        Err(ManifestTrustError::SignatureInvalid)
    ));
    assert!(matches!(
        trust.verify(&store, HTTPS, &body, Some(b"not base64 !!")),
        Err(ManifestTrustError::SignatureMalformed(_))
    ));
    assert!(matches!(
        trust.verify(
            &store,
            HTTPS,
            &body,
            Some(STANDARD.encode([0_u8; 10]).as_bytes())
        ),
        Err(ManifestTrustError::SignatureMalformed(_))
    ));
}

#[test]
fn missing_signature_is_rejected_in_strict_mode_even_for_file_sources() {
    let (_temp, store) = store();
    let body = manifest("2026-10-08T12:00:00Z");
    for source in [HTTPS, FILE] {
        assert!(matches!(
            trust(TrustPolicy::Strict).verify(&store, source, &body, None),
            Err(ManifestTrustError::SignatureMissing)
        ));
    }
}

#[test]
fn replayed_older_manifest_is_rejected_but_same_or_newer_is_accepted() {
    let (_temp, store) = store();
    let trust = trust(TrustPolicy::Strict);
    let key = signing_key(1);
    let verify = |generated_at: &str| {
        let body = manifest(generated_at);
        trust.verify(&store, HTTPS, &body, Some(&sign(&key, &body)))
    };

    verify("2026-10-08T12:00:00Z").expect("first");
    assert!(matches!(
        verify("2026-10-08T11:59:59Z"),
        Err(ManifestTrustError::Replayed { .. })
    ));
    // Same instant in another zone notation is not older.
    verify("2026-10-08T14:00:00+02:00").expect("same instant");
    verify("2026-10-09T00:00:00Z").expect("newer");
    assert!(matches!(
        verify("2026-10-08T12:00:00Z"),
        Err(ManifestTrustError::Replayed { .. })
    ));
    // The watermark survives a new store handle (persisted on disk).
    let reopened = InstalledDriverStore::new(store.base_directory());
    let body = manifest("2026-10-08T12:00:00Z");
    assert!(matches!(
        trust.verify(&reopened, HTTPS, &body, Some(&sign(&key, &body))),
        Err(ManifestTrustError::Replayed { .. })
    ));
}

#[test]
fn signed_manifest_without_a_usable_timestamp_is_rejected() {
    let (_temp, store) = store();
    let trust = trust(TrustPolicy::Strict);
    for body in [
        br#"{"drivers":[]}"#.to_vec(),
        br#"{"generatedAt":"soon","drivers":[]}"#.to_vec(),
    ] {
        assert!(matches!(
            trust.verify(&store, HTTPS, &body, Some(&sign(&signing_key(1), &body))),
            Err(ManifestTrustError::MissingGeneratedAt)
        ));
    }
}

#[test]
fn developer_policy_accepts_unsigned_file_manifests_only() {
    let (_temp, store) = store();
    let developer = trust(TrustPolicy::Developer);
    let body = manifest("2020-01-01T00:00:00Z");

    developer
        .verify(&store, FILE, &body, None)
        .expect("unsigned file manifest");
    // No replay tracking for dev manifests: rebuilding one never gets stuck.
    developer
        .verify(&store, FILE, &manifest("2019-01-01T00:00:00Z"), None)
        .expect("older dev manifest");
    assert!(matches!(
        developer.verify(&store, HTTPS, &body, None),
        Err(ManifestTrustError::SignatureMissing)
    ));
    // A signed https manifest verifies as in strict mode.
    developer
        .verify(&store, HTTPS, &body, Some(&sign(&signing_key(1), &body)))
        .expect("signed https");
}

#[test]
fn key_helpers_round_trip() {
    let encoded = STANDARD.encode(signing_key(1).verifying_key().to_bytes());
    assert_eq!(
        public_key_from_base64(&encoded).unwrap(),
        signing_key(1).verifying_key().to_bytes()
    );
    assert!(matches!(
        public_key_from_base64("AAAA"),
        Err(ManifestTrustError::InvalidKey(_))
    ));
    assert!(matches!(
        ManifestTrust::new(&[], TrustPolicy::Strict)
            .unwrap()
            .verify(
                &store().1,
                HTTPS,
                &manifest("2026-10-08T12:00:00Z"),
                Some(b"AAAA")
            ),
        Err(ManifestTrustError::NoTrustedKeys)
    ));
}

struct QueueFetcher(Mutex<VecDeque<Result<Vec<u8>, RegistryError>>>);

impl QueueFetcher {
    fn new(responses: impl IntoIterator<Item = Result<Vec<u8>, RegistryError>>) -> Self {
        Self(Mutex::new(responses.into_iter().collect()))
    }
}

impl ManifestFetcher for QueueFetcher {
    fn fetch<'a>(
        &'a self,
        _source: &'a str,
    ) -> Pin<Box<dyn Future<Output = Result<Vec<u8>, RegistryError>> + Send + 'a>> {
        Box::pin(async move {
            self.0
                .lock()
                .expect("lock")
                .pop_front()
                .unwrap_or_else(|| Err(RegistryError::Fetch("no more responses".into())))
        })
    }
}

#[test]
fn registry_requires_a_valid_signature_and_never_falls_back_to_cache_on_attack() {
    let (_temp, store) = store();
    let key = signing_key(1);
    let good = manifest("2026-10-08T12:00:00Z");
    let older = manifest("2026-10-07T12:00:00Z");
    let fetcher = QueueFetcher::new([
        Ok(good.clone()),
        Ok(sign(&key, &good)),
        Ok(older.clone()),
        Ok(sign(&key, &older)),
        Ok(good.clone()),
        Err(RegistryError::Fetch("404".into())),
    ]);
    let service = RegistryService::new(HTTPS, &fetcher, &store)
        .with_manifest_trust(trust(TrustPolicy::Strict));

    block_on(service.catalog(&MAC_ARM64)).expect("signed manifest");
    assert!(matches!(
        block_on(service.catalog(&MAC_ARM64)),
        Err(RegistryError::Manifest(ManifestTrustError::Replayed { .. }))
    ));
    assert!(matches!(
        block_on(service.catalog(&MAC_ARM64)),
        Err(RegistryError::Manifest(
            ManifestTrustError::SignatureMissing
        ))
    ));
}

const V2_MANIFEST: &str = r#"{
  "schemaVersion": 2,
  "generatedAt": "2026-10-08T12:00:00Z",
  "drivers": [
    {"id":"clickhouse","displayName":"ClickHouse","version":"1.0.0","appAbiVersion":2,
     "uriSchemes":["clickhouse"],"features":[],"license":"MIT","sourceRepo":"https://example.com",
     "teamId":"T","artifacts":[{"arch":"arm64","url":"https://example.com/a.zip","sha256":"a","sizeBytes":1}],
     "category":"analytics","description":"Columnar","minAppVersion":"1.6.0",
     "releaseNotesUrl":"https://example.com/notes","publishedAt":"2026-10-01T00:00:00Z"},
    {"id":"redis","displayName":"Redis","version":"1.0.0","appAbiVersion":2,
     "uriSchemes":["redis"],"features":[],"license":"MIT","sourceRepo":"https://example.com",
     "teamId":"T","artifacts":[{"arch":"arm64","url":"https://example.com/b.zip","sha256":"b","sizeBytes":1}],
     "category":"some_future_category"}
  ]
}"#;

const V1_MANIFEST: &str = r#"{"generatedAt":"2026-08-31T00:00:00Z","drivers":[
    {"id":"redis","displayName":"Redis","version":"1.0.0","appAbiVersion":2,
     "uriSchemes":["redis"],"features":[],"license":"MIT","sourceRepo":"https://example.com",
     "teamId":"T","artifacts":[]}],"tools":[]}"#;

#[test]
fn manifest_v1_and_v2_both_parse() {
    let v1: DriverManifest = serde_json::from_str(V1_MANIFEST).expect("v1");
    assert_eq!(v1.schema_version, 1);
    let redis = &v1.drivers[0];
    assert!(
        redis.category.is_none() && redis.description.is_none() && redis.min_app_version.is_none()
    );
    assert!(redis.release_notes_url.is_none() && redis.published_at.is_none());

    let v2: DriverManifest = serde_json::from_str(V2_MANIFEST).expect("v2");
    assert_eq!(v2.schema_version, 2);
    let clickhouse = &v2.drivers[0];
    assert_eq!(clickhouse.category, Some(DriverCategory::Analytics));
    assert_eq!(clickhouse.description.as_deref(), Some("Columnar"));
    assert_eq!(clickhouse.min_app_version.as_deref(), Some("1.6.0"));
    assert_eq!(
        clickhouse.release_notes_url.as_deref(),
        Some("https://example.com/notes")
    );
    assert_eq!(
        clickhouse.published_at.as_deref(),
        Some("2026-10-01T00:00:00Z")
    );
    assert_eq!(
        v2.drivers[1].category,
        Some(DriverCategory::Other),
        "unknown category tolerated"
    );
    for (text, expected) in [
        ("sql", DriverCategory::Sql),
        ("nosql", DriverCategory::Nosql),
        ("key_value", DriverCategory::KeyValue),
    ] {
        assert_eq!(
            serde_json::from_str::<DriverCategory>(&format!("\"{text}\"")).unwrap(),
            expected
        );
    }
}

#[test]
fn min_app_version_gates_the_catalog() {
    let state_for = |app: Option<&str>| {
        let (_temp, store) = store();
        let fetcher = QueueFetcher::new([Ok(V2_MANIFEST.as_bytes().to_vec())]);
        let mut service = RegistryService::new(FILE, &fetcher, &store);
        if let Some(app) = app {
            service = service.with_app_version(app);
        }
        let catalog = block_on(service.catalog(&MAC_ARM64)).expect("catalog");
        catalog
            .into_iter()
            .find(|item| item.id == "clickhouse")
            .expect("clickhouse")
            .state
    };

    assert_eq!(
        state_for(Some("1.5.9")),
        DriverCatalogState::Unavailable {
            reason: "Needs TableX \u{2265} 1.6.0".into()
        }
    );
    assert_eq!(state_for(Some("1.6")), DriverCatalogState::Available);
    assert_eq!(
        state_for(Some("2.0.0-beta.1")),
        DriverCatalogState::Available
    );
    assert_eq!(state_for(None), DriverCatalogState::Available);
}

#[test]
fn an_ad_hoc_signature_installs_when_the_signed_manifest_pins_no_team_id() {
    // The mock dylib is ad-hoc signed by the linker: valid signature, no Team ID.
    // A release without a Developer ID ships exactly that, and its signed
    // manifest says so with an empty `team_id`; the SHA-256 pins the file.
    let (_temp, store) = store();
    let adhoc_entry = mock_entry("1.0.0", "");

    // A manifest that pins a Team ID still requires it, under either policy.
    let pinned = DriverInstaller::new(DriverTarget::current(), &store)
        .install_from_file(&mock_entry("1.0.0", "ABCDE12345"), mock_dylib());
    assert!(
        matches!(
            pinned,
            Err(InstallError::Verification(
                DriverVerificationError::TeamIdMismatch { .. }
            ))
        ),
        "{pinned:?}"
    );
    assert!(store.record("mock").unwrap().is_none());

    let strict = DriverInstaller::new(DriverTarget::current(), &store)
        .install_from_file(&adhoc_entry, mock_dylib())
        .expect("strict mode accepts what the signed manifest attests");
    assert_eq!(strict.team_id, "");

    let developer = DriverInstaller::new(DriverTarget::current(), &store)
        .with_trust_policy(TrustPolicy::Developer)
        .install_from_file(&mock_entry("1.0.1", ""), mock_dylib())
        .expect("developer mode accepts ad-hoc");
    assert_eq!(developer.team_id, "");
}

#[test]
fn a_document_of_another_kind_is_not_a_catalog() {
    let parse = |text: &str| serde_json::from_str::<DriverManifest>(text);
    assert!(parse(r#"{"drivers":[]}"#).is_ok());
    assert!(parse(r#"{"kind":"tablex-driver-catalog","drivers":[]}"#).is_ok());
    // The app's update manifest (signed by the same key) is not read as a catalog.
    let update = r#"{"kind":"tablex-app-update","schema":1,"version":"0.2.0","generatedAt":"2026-11-11T00:00:00Z","artifacts":[]}"#;
    assert!(parse(update).is_err());
    assert!(parse(r#"{"kind":"tablex-app-update","drivers":[]}"#).is_err());
    assert!(parse(r#"{"kind":null,"drivers":[]}"#).is_err());
}
