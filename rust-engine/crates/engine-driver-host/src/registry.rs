use crate::{
    signature_source, DriverManifest, DriverManifestEntry, DriverTarget, InstalledDriverStore,
    ManifestTrust, ManifestTrustError, StoreError, ToolCatalogItem, ToolError, ToolKind,
    ToolStore,
};
use engine_driver_api::ABI_VERSION;
use std::collections::BTreeSet;
use std::fs::{self, OpenOptions};
use std::future::Future;
use std::io::Write;
use std::path::Path;
use std::pin::Pin;
use std::sync::atomic::{AtomicU64, Ordering};
use thiserror::Error;

static CACHE_TEMP_SEQUENCE: AtomicU64 = AtomicU64::new(1);

#[derive(Debug, Error)]
pub enum RegistryError {
    #[error("driver registry fetch failed: {0}")]
    Fetch(String),
    #[error("driver registry I/O failed: {0}")]
    Io(#[from] std::io::Error),
    #[error("driver registry JSON is invalid: {0}")]
    Json(#[from] serde_json::Error),
    #[error(transparent)]
    Store(#[from] StoreError),
    /// The manifest failed signature or anti-replay verification. Never
    /// papered over with the cached manifest.
    #[error(transparent)]
    Manifest(#[from] ManifestTrustError),
    #[error(transparent)]
    Tool(#[from] ToolError),
}

pub trait ManifestFetcher: Send + Sync {
    fn fetch<'a>(
        &'a self,
        source: &'a str,
    ) -> Pin<Box<dyn Future<Output = Result<Vec<u8>, RegistryError>> + Send + 'a>>;
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DriverCatalogState {
    Available,
    Installed { version: String },
    UpdateAvailable { installed: String, latest: String },
    Unavailable { reason: String },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DriverCatalogItem {
    pub id: String,
    pub display_name: String,
    pub version: String,
    pub state: DriverCatalogState,
    pub manifest: Option<DriverManifestEntry>,
    pub disabled: bool,
}

/// The drivers and the tools of one manifest, merged with what is installed.
#[derive(Debug)]
pub struct FullCatalog {
    pub drivers: Vec<DriverCatalogItem>,
    pub tools: Vec<ToolCatalogItem>,
}

pub struct RegistryService<'a, F> {
    source: String,
    fetcher: &'a F,
    store: &'a InstalledDriverStore,
    app_version: Option<String>,
    trust: Option<ManifestTrust>,
}

impl<'a, F: ManifestFetcher> RegistryService<'a, F> {
    pub fn new(source: impl Into<String>, fetcher: &'a F, store: &'a InstalledDriverStore) -> Self {
        Self {
            source: source.into(),
            fetcher,
            store,
            app_version: None,
            trust: None,
        }
    }

    /// Gate entries whose `minAppVersion` is newer than `app_version`
    /// (`Unavailable("Needs TableX ≥ x")`). Without it nothing is gated.
    pub fn with_app_version(mut self, app_version: impl Into<String>) -> Self {
        self.app_version = Some(app_version.into());
        self
    }

    /// Require a verified manifest: the detached `<source>.sig` is fetched with
    /// the same fetcher and checked, and replays are rejected. Production
    /// callers must set this; without it the manifest is trusted as fetched.
    pub fn with_manifest_trust(mut self, trust: ManifestTrust) -> Self {
        self.trust = Some(trust);
        self
    }

    pub async fn catalog(
        &self,
        target: &DriverTarget,
    ) -> Result<Vec<DriverCatalogItem>, RegistryError> {
        let manifest = self.load_manifest().await?;
        self.driver_items(manifest.drivers, target)
    }

    /// [`Self::catalog`] plus the manifest's tools merged with `tools`. A tool entry that is
    /// unknown to this build is skipped; a malformed known one appears as unavailable. Neither
    /// affects the drivers.
    pub async fn catalog_with_tools(
        &self,
        target: &DriverTarget,
        tools: &ToolStore,
    ) -> Result<FullCatalog, RegistryError> {
        let manifest = self.load_manifest().await?;
        let tool_items = ToolKind::ALL
            .into_iter()
            .map(|kind| {
                let listed = manifest
                    .tools
                    .iter()
                    .find(|tool| tool.id() == Some(kind.tool_id()));
                ToolCatalogItem::resolve(kind, listed, tools, target, |entry| {
                    self.unmet_requirement(entry.min_app_version.as_deref())
                        .map(str::to_string)
                })
            })
            .filter_map(Result::transpose)
            .collect::<Result<Vec<_>, _>>()?;
        Ok(FullCatalog {
            drivers: self.driver_items(manifest.drivers, target)?,
            tools: tool_items,
        })
    }

    fn driver_items(
        &self,
        drivers: Vec<DriverManifestEntry>,
        target: &DriverTarget,
    ) -> Result<Vec<DriverCatalogItem>, RegistryError> {
        let installed = self.store.load_all()?;
        let manifest_ids: BTreeSet<String> = drivers.iter().map(|entry| entry.id.clone()).collect();
        let mut catalog = Vec::with_capacity(drivers.len() + installed.len());

        for entry in drivers {
            let disabled = installed
                .get(&entry.id)
                .is_some_and(|record| record.disabled);
            let state =
                if let Some(minimum) = self.unmet_requirement(entry.min_app_version.as_deref()) {
                    DriverCatalogState::Unavailable {
                        reason: format!("Needs TableX \u{2265} {minimum}"),
                    }
                } else if entry.app_abi_version != ABI_VERSION {
                    DriverCatalogState::Unavailable {
                        reason: format!("Requires driver ABI {}", entry.app_abi_version),
                    }
                } else if entry.artifact_for_target(target).is_none() {
                    DriverCatalogState::Unavailable {
                        reason: format!("No build for {target}"),
                    }
                } else if let Some(record) = installed.get(&entry.id) {
                    if record.version == entry.version {
                        DriverCatalogState::Installed {
                            version: record.version.clone(),
                        }
                    } else {
                        DriverCatalogState::UpdateAvailable {
                            installed: record.version.clone(),
                            latest: entry.version.clone(),
                        }
                    }
                } else {
                    DriverCatalogState::Available
                };
            catalog.push(DriverCatalogItem {
                id: entry.id.clone(),
                display_name: entry.display_name.clone(),
                version: entry.version.clone(),
                state,
                manifest: Some(entry),
                disabled,
            });
        }

        for record in installed.values() {
            if manifest_ids.contains(&record.id) {
                continue;
            }
            catalog.push(DriverCatalogItem {
                id: record.id.clone(),
                display_name: record.id.clone(),
                version: record.version.clone(),
                state: DriverCatalogState::Installed {
                    version: record.version.clone(),
                },
                manifest: None,
                disabled: record.disabled,
            });
        }

        catalog.sort_by(|left, right| left.display_name.cmp(&right.display_name));
        Ok(catalog)
    }

    fn unmet_requirement<'e>(&self, minimum: Option<&'e str>) -> Option<&'e str> {
        let minimum = minimum?;
        let app = self.app_version.as_deref()?;
        (compare_versions(app, minimum)? == std::cmp::Ordering::Less).then_some(minimum)
    }

    async fn load_manifest(&self) -> Result<DriverManifest, RegistryError> {
        match self.fetcher.fetch(&self.source).await {
            Ok(data) => match self.parse_fetched(&data).await {
                Ok((manifest, signature)) => {
                    self.write_cache(&data, signature.as_deref())?;
                    Ok(manifest)
                }
                Err(RegistryError::Json(error))
                | Err(RegistryError::Manifest(ManifestTrustError::Json(error))) => {
                    self.cached_or_empty(RegistryError::Json(error))
                }
                Err(error) => Err(error),
            },
            Err(error) => self.cached_or_empty(error),
        }
    }

    /// The manifest, and the detached signature it was verified with (`None` when none was needed
    /// or no trust is configured).
    async fn parse_fetched(
        &self,
        data: &[u8],
    ) -> Result<(DriverManifest, Option<Vec<u8>>), RegistryError> {
        let Some(trust) = &self.trust else {
            return Ok((serde_json::from_slice(data)?, None));
        };
        let signature = if trust.requires_signature(&self.source) {
            self.fetcher
                .fetch(&signature_source(&self.source))
                .await
                .ok()
        } else {
            None
        };
        let manifest = trust.verify(self.store, &self.source, data, signature.as_deref())?;
        Ok((manifest, signature))
    }

    fn cached_or_empty(
        &self,
        original_error: RegistryError,
    ) -> Result<DriverManifest, RegistryError> {
        match self.read_cache() {
            Ok(manifest) => Ok(manifest),
            Err(RegistryError::Io(error)) if error.kind() == std::io::ErrorKind::NotFound => {
                Ok(DriverManifest::default())
            }
            // The anti-replay record could not be read or written: that is a store failure to
            // report, not a verdict on the cache.
            Err(error @ RegistryError::Manifest(ManifestTrustError::Store(_))) => Err(error),
            // A cache this trust does not vouch for (unsigned, written under another policy,
            // tampered with, replayed) is no cache.
            Err(RegistryError::Manifest(_)) => Ok(DriverManifest::default()),
            Err(_) => Err(original_error),
        }
    }

    /// With a trust configured the cache is checked again, exactly like a fetched manifest: the
    /// signature stored beside it must verify under the CURRENT trust and policy, so a manifest
    /// that was accepted unsigned (Developer mode, a local file) is never served in Strict mode.
    fn read_cache(&self) -> Result<DriverManifest, RegistryError> {
        let data = fs::read(self.store.registry_cache_path())?;
        let Some(trust) = &self.trust else {
            return Ok(serde_json::from_slice(&data)?);
        };
        let signature = match fs::read(self.cache_signature_path()) {
            Ok(signature) => Some(signature),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => None,
            Err(error) => return Err(error.into()),
        };
        Ok(trust.verify(self.store, &self.source, &data, signature.as_deref())?)
    }

    fn cache_signature_path(&self) -> std::path::PathBuf {
        let mut name = self.store.registry_cache_path().into_os_string();
        name.push(".sig");
        name.into()
    }

    /// Stores the manifest bytes and, when it was verified with a signature, the signature beside
    /// them (removing an older one otherwise). The two files are separate: a crash between them
    /// leaves a pair that does not verify, which reads as "no cache".
    fn write_cache(&self, data: &[u8], signature: Option<&[u8]>) -> Result<(), RegistryError> {
        self.write_cache_bytes(&self.store.registry_cache_path(), data)?;
        let signature_path = self.cache_signature_path();
        match signature {
            Some(signature) => self.write_cache_bytes(&signature_path, signature),
            None => match fs::remove_file(&signature_path) {
                Err(error) if error.kind() != std::io::ErrorKind::NotFound => Err(error.into()),
                _ => Ok(()),
            },
        }
    }

    fn write_cache_bytes(&self, path: &Path, data: &[u8]) -> Result<(), RegistryError> {
        let path = path.to_path_buf();
        let directory = path.parent().expect("registry cache has parent");
        fs::create_dir_all(directory)?;
        let temp = directory.join(format!(
            "registry-cache-{}-{}.json.tmp",
            std::process::id(),
            CACHE_TEMP_SEQUENCE.fetch_add(1, Ordering::Relaxed)
        ));
        let result = write_cache_file(directory, &path, &temp, data);
        if result.is_err() {
            let _ = fs::remove_file(temp);
        }
        result
    }
}

fn write_cache_file(
    directory: &Path,
    path: &Path,
    temp: &Path,
    data: &[u8],
) -> Result<(), RegistryError> {
    let mut file = OpenOptions::new()
        .create(true)
        .truncate(true)
        .write(true)
        .open(temp)?;
    file.write_all(data)?;
    file.sync_all()?;
    fs::rename(temp, path)?;
    crate::os_fs::sync_directory(directory)?;
    Ok(())
}

pub fn resolve_manifest_source(
    explicit: Option<&str>,
    environment_override: Option<&str>,
    default: &str,
) -> String {
    explicit
        .or(environment_override)
        .unwrap_or(default)
        .to_string()
}

/// Compare dotted numeric versions ("1.6", "1.6.0-beta" -> 1.6.0). `None` when
/// either side has no leading number.
fn compare_versions(left: &str, right: &str) -> Option<std::cmp::Ordering> {
    fn components(version: &str) -> Option<Vec<u64>> {
        let parts: Vec<u64> = version
            .trim()
            .trim_start_matches('v')
            .split('.')
            .map(|part| {
                let digits = part
                    .find(|c: char| !c.is_ascii_digit())
                    .map_or(part, |end| &part[..end]);
                digits.parse().ok()
            })
            .collect::<Option<_>>()?;
        Some(parts)
    }
    let (mut left, mut right) = (components(left)?, components(right)?);
    let length = left.len().max(right.len());
    left.resize(length, 0);
    right.resize(length, 0);
    Some(left.cmp(&right))
}
