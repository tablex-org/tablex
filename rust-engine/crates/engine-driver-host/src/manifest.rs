use crate::DriverTarget;
use serde::{Deserialize, Deserializer, Serialize, Serializer};
use thiserror::Error;

/// Manifest `schemaVersion` written by current tooling. A manifest without the
/// field is schema 1; v2 only adds optional fields, so both parse the same way.
pub const MANIFEST_SCHEMA_VERSION: u32 = 2;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct DriverManifest {
    #[serde(default = "default_schema_version", alias = "schema_version")]
    pub schema_version: u32,
    #[serde(default)]
    pub generated_at: Option<String>,
    #[serde(default)]
    pub drivers: Vec<DriverManifestEntry>,
    /// Downloadable tools next to the drivers. Each entry is read on its own:
    /// one this build does not know, or a malformed one, never fails the
    /// drivers (see [`ManifestTool`]).
    #[serde(default, deserialize_with = "lenient_tools")]
    pub tools: Vec<ManifestTool>,
}

/// `tools` read so that it can never fail the drivers: a list is read element by element,
/// `null` is "no tools", and any other value (`{}`, a string, ...) is an unusable entry of the one
/// tool this build knows, which then shows as unavailable.
fn lenient_tools<'de, D: Deserializer<'de>>(
    deserializer: D,
) -> Result<Vec<ManifestTool>, D::Error> {
    Ok(match serde_json::Value::deserialize(deserializer)? {
        serde_json::Value::Array(items) => {
            items.into_iter().map(ManifestTool::from_value).collect()
        }
        serde_json::Value::Null => Vec::new(),
        other => vec![ManifestTool::Invalid {
            id: MCP_TOOL_ID.to_string(),
            error: ToolManifestError::Malformed("`tools` must be a list".into()),
            raw: other,
        }],
    })
}

impl Default for DriverManifest {
    fn default() -> Self {
        Self {
            schema_version: default_schema_version(),
            generated_at: None,
            drivers: Vec::new(),
            tools: Vec::new(),
        }
    }
}

fn default_schema_version() -> u32 {
    1
}

/// Driver family shown as a filter chip. Unknown future values parse as `Other`
/// so a newer manifest never fails to load on an older app.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DriverCategory {
    Sql,
    Nosql,
    Analytics,
    KeyValue,
    #[serde(other)]
    Other,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct DriverManifestEntry {
    pub id: String,
    pub display_name: String,
    pub version: String,
    pub app_abi_version: u32,
    pub uri_schemes: Vec<String>,
    pub features: Vec<String>,
    pub license: String,
    pub source_repo: String,
    pub team_id: String,
    pub artifacts: Vec<DriverArtifact>,
    // Schema v2 (all optional so v1 manifests parse unchanged).
    #[serde(default)]
    pub category: Option<DriverCategory>,
    #[serde(default)]
    pub description: Option<String>,
    #[serde(default, alias = "min_app_version")]
    pub min_app_version: Option<String>,
    #[serde(default, alias = "release_notes_url")]
    pub release_notes_url: Option<String>,
    #[serde(default, alias = "published_at")]
    pub published_at: Option<String>,
}

impl DriverManifestEntry {
    /// The artifact built for `target`: an exact match first, then the macOS
    /// universal build on macOS only.
    pub fn artifact_for_target(&self, target: &DriverTarget) -> Option<&DriverArtifact> {
        artifact_for_target(&self.artifacts, target)
    }
}

fn artifact_for_target<'a>(
    artifacts: &'a [DriverArtifact],
    target: &DriverTarget,
) -> Option<&'a DriverArtifact> {
    let id = target.id();
    let find = |wanted: &str| artifacts.iter().find(|a| a.target_id() == wanted);
    find(&id).or_else(|| target.universal_id().and_then(find))
}

/// One downloadable build. Schema v2 identifies it by `target`
/// (`macos-aarch64`, `linux-x86_64`, `windows-x86_64`, ...). Older manifests
/// only carry `arch` (`arm64`, `x86_64`, `universal`), which always meant
/// macOS. Writers keep `arch` filled for old readers: the macOS value on
/// macOS builds, the target itself elsewhere (which no old reader matches).
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct DriverArtifact {
    #[serde(default)]
    pub arch: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub target: Option<String>,
    pub url: String,
    pub sha256: String,
    pub size_bytes: u64,
}

impl DriverArtifact {
    /// The explicit `target`, else the legacy `arch` read as a macOS build.
    pub fn target_id(&self) -> String {
        match &self.target {
            Some(target) => target.clone(),
            None => match self.arch.as_str() {
                "arm64" => "macos-aarch64".to_string(),
                other => format!("macos-{other}"),
            },
        }
    }
}

// --- tools ------------------------------------------------------------------------------

/// Id of the standalone MCP server.
pub const MCP_TOOL_ID: &str = "tablex-mcp";
/// Id of the MongoDB Shell plugin: an executable add-on of the MongoDB driver.
pub const MONGO_SHELL_TOOL_ID: &str = "tablex-mongo-shell";
/// Id of the SQL formatter tool (sqruff in its own process): free-standing, it needs no driver.
pub const SQRUFF_TOOL_ID: &str = "tablex-sqruff";
/// The driver the MongoDB Shell plugin needs.
pub const MONGODB_DRIVER_ID: &str = "mongodb";
/// Upper bound for a tool build declared in a manifest (the server is ~7 MB).
pub const MAX_TOOL_BYTES: u64 = 256 * 1024 * 1024;

/// What a tool is. A manifest of a newer release may list kinds this build
/// does not know ([`ManifestTool::Ignored`]).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum ToolKind {
    McpServer,
    /// An add-on of the MongoDB driver (it lists `requires`).
    MongoShell,
    /// The SQL formatter (sqruff): a process Format SQL can run instead of the built-in formatter.
    SqlFormatter,
}

impl ToolKind {
    /// Every kind this build knows, in the order the catalog lists them.
    pub const ALL: [Self; 3] = [Self::McpServer, Self::MongoShell, Self::SqlFormatter];

    /// The `kind` string of the manifest.
    pub const fn wire_name(self) -> &'static str {
        match self {
            Self::McpServer => "mcpServer",
            Self::MongoShell => "mongoShell",
            Self::SqlFormatter => "sqlFormatter",
        }
    }

    /// Whether the tool belongs to a driver (listed under it) rather than on its own.
    pub const fn is_addon(self) -> bool {
        matches!(self, Self::MongoShell)
    }

    /// The one id a tool of this kind has.
    pub const fn tool_id(self) -> &'static str {
        match self {
            Self::McpServer => MCP_TOOL_ID,
            Self::MongoShell => MONGO_SHELL_TOOL_ID,
            Self::SqlFormatter => SQRUFF_TOOL_ID,
        }
    }

    /// The kind of a known `(kind, id)` pair of the manifest.
    fn of_entry(kind: &str, id: &str) -> Option<Self> {
        Self::ALL
            .into_iter()
            .find(|candidate| candidate.wire_name() == kind && candidate.tool_id() == id)
    }
}

/// What an add-on needs: a driver, at least this version.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct DriverRequirement {
    pub driver: String,
    pub min_version: String,
}

/// How an installed driver (or none) stands against a [`DriverRequirement`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RequirementStatus {
    Met,
    /// The driver is not installed (or is disabled).
    Missing,
    TooOld { installed: String },
}

impl DriverRequirement {
    /// `installed` is the version of the enabled driver, `None` when there is none. Versions are
    /// compared by their leading dotted numbers (`0.1.0-dev.ab12` counts as 0.1.0); a version with
    /// none (a driver imported from a local file) cannot be compared and is taken as met: the host
    /// still checks the session's capabilities when the add-on runs.
    pub fn check(&self, installed: Option<&str>) -> RequirementStatus {
        let Some(installed) = installed else {
            return RequirementStatus::Missing;
        };
        match (release_numbers(installed), release_numbers(&self.min_version)) {
            (Some(have), Some(need)) if compare_release(&have, &need).is_lt() => {
                RequirementStatus::TooOld {
                    installed: installed.to_string(),
                }
            }
            _ => RequirementStatus::Met,
        }
    }
}

/// `1.6.0-beta.2` -> `[1, 6, 0]`: the dotted numbers before any `-` or `+` suffix.
fn release_numbers(version: &str) -> Option<Vec<u64>> {
    let core = version.trim().trim_start_matches('v');
    let core = core.split(['-', '+']).next().unwrap_or_default();
    core.split('.')
        .map(|part| part.parse().ok())
        .collect::<Option<Vec<u64>>>()
        .filter(|numbers| !numbers.is_empty())
}

fn compare_release(left: &[u64], right: &[u64]) -> std::cmp::Ordering {
    let length = left.len().max(right.len());
    let at = |numbers: &[u64], index: usize| numbers.get(index).copied().unwrap_or(0);
    (0..length)
        .map(|index| at(left, index).cmp(&at(right, index)))
        .find(|ordering| ordering.is_ne())
        .unwrap_or(std::cmp::Ordering::Equal)
}

/// A downloadable tool: one executable per platform target, installed at a
/// stable path (`ToolStore`). It sits inside the signed manifest, so the
/// manifest signature covers it like the drivers.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ToolManifestEntry {
    pub id: String,
    pub kind: ToolKind,
    pub display_name: String,
    pub version: String,
    #[serde(default)]
    pub description: Option<String>,
    #[serde(default, alias = "min_app_version")]
    pub min_app_version: Option<String>,
    /// Same shape and target naming as a driver's artifacts. The file is the
    /// executable itself (no archive).
    pub artifacts: Vec<DriverArtifact>,
    /// SPDX licence of the tool, shown before it is installed (a tool built on third-party code
    /// says so). Absent in older manifests.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub license: Option<String>,
    /// Where its source lives. Absent in older manifests.
    #[serde(default, alias = "source_repo", skip_serializing_if = "Option::is_none")]
    pub source_repo: Option<String>,
    /// Drivers this tool is an add-on of. Absent for a free-standing tool; an older app ignores
    /// the field (and the kinds it adds).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub requires: Vec<DriverRequirement>,
}

impl ToolManifestEntry {
    /// The build for `target` (exact match, then the macOS universal build).
    pub fn artifact_for_target(&self, target: &DriverTarget) -> Option<&DriverArtifact> {
        artifact_for_target(&self.artifacts, target)
    }

    fn validate(&self) -> Result<(), ToolManifestError> {
        let plain_version = !self.version.is_empty()
            && self.version.len() <= 32
            && self
                .version
                .chars()
                .all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '-' | '_'));
        if !plain_version {
            return Err(ToolManifestError::InvalidVersion(self.version.clone()));
        }
        if self.artifacts.is_empty() {
            return Err(ToolManifestError::NoBuilds);
        }
        for requirement in &self.requires {
            let valid = crate::verifier::validate_driver_id(&requirement.driver).is_ok()
                && release_numbers(&requirement.min_version).is_some();
            if !valid {
                return Err(ToolManifestError::InvalidRequirement(format!(
                    "{} {}",
                    requirement.driver, requirement.min_version
                )));
            }
        }
        // An add-on without its driver would install on any app and run against nothing.
        if self.kind == ToolKind::MongoShell
            && !self.requires.iter().any(|requirement| requirement.driver == MONGODB_DRIVER_ID)
        {
            return Err(ToolManifestError::MissingRequirement(MONGODB_DRIVER_ID.into()));
        }
        let mut seen: Vec<String> = Vec::with_capacity(self.artifacts.len());
        for artifact in &self.artifacts {
            let target = artifact.target_id();
            let digest = artifact.sha256.len() == 64
                && artifact.sha256.bytes().all(|b| b.is_ascii_hexdigit());
            if !digest {
                return Err(ToolManifestError::InvalidChecksum { target });
            }
            if artifact.size_bytes == 0 || artifact.size_bytes > MAX_TOOL_BYTES {
                return Err(ToolManifestError::InvalidSize {
                    target,
                    size: artifact.size_bytes,
                });
            }
            if artifact.url.is_empty() {
                return Err(ToolManifestError::EmptyUrl { target });
            }
            if seen.contains(&target) {
                return Err(ToolManifestError::DuplicateTarget { target });
            }
            seen.push(target);
        }
        Ok(())
    }
}

/// Why a tool entry of a known kind is unusable. Fails that tool only.
#[derive(Debug, Clone, PartialEq, Eq, Error)]
pub enum ToolManifestError {
    #[error("tool entry is malformed: {0}")]
    Malformed(String),
    #[error("tool version '{0}' is not a plain version string")]
    InvalidVersion(String),
    #[error("tool lists no builds")]
    NoBuilds,
    #[error("tool build for {target} has no valid SHA-256")]
    InvalidChecksum { target: String },
    #[error("tool build for {target} has an invalid size ({size} bytes)")]
    InvalidSize { target: String, size: u64 },
    #[error("tool build for {target} has no URL")]
    EmptyUrl { target: String },
    #[error("tool lists two builds for {target}")]
    DuplicateTarget { target: String },
    #[error("tool requires '{0}', which is not a driver and a version")]
    InvalidRequirement(String),
    #[error("this add-on must require the {0} driver")]
    MissingRequirement(String),
}

/// One element of the manifest's `tools`, read independently of its siblings.
/// Forward compatible: an entry that is not a known tool is kept verbatim and
/// ignored; a known tool that is malformed fails alone.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ManifestTool {
    /// A known `kind` with its id ([`MCP_TOOL_ID`], [`MONGO_SHELL_TOOL_ID`]), well formed.
    Known(ToolManifestEntry),
    /// A known tool whose entry is unusable.
    Invalid {
        id: String,
        error: ToolManifestError,
        raw: serde_json::Value,
    },
    /// Another kind or id (a newer manifest), or not a tool object at all.
    Ignored(serde_json::Value),
}

impl ManifestTool {
    /// The id of a known tool (usable or not); `None` for an ignored entry.
    pub fn id(&self) -> Option<&str> {
        match self {
            Self::Known(entry) => Some(&entry.id),
            Self::Invalid { id, .. } => Some(id),
            Self::Ignored(_) => None,
        }
    }

    pub fn from_value(value: serde_json::Value) -> Self {
        let text = |key: &str| value.get(key).and_then(serde_json::Value::as_str);
        let Some(kind) = text("kind")
            .zip(text("id"))
            .and_then(|(kind, id)| ToolKind::of_entry(kind, id))
        else {
            return Self::Ignored(value);
        };
        let parsed = serde_json::from_value::<ToolManifestEntry>(value.clone())
            .map_err(|error| ToolManifestError::Malformed(error.to_string()))
            .and_then(|entry| entry.validate().map(|()| entry));
        match parsed {
            Ok(entry) => Self::Known(entry),
            Err(error) => Self::Invalid {
                id: kind.tool_id().to_string(),
                error,
                raw: value,
            },
        }
    }
}

impl<'de> Deserialize<'de> for ManifestTool {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        serde_json::Value::deserialize(deserializer).map(Self::from_value)
    }
}

impl Serialize for ManifestTool {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        match self {
            Self::Known(entry) => entry.serialize(serializer),
            Self::Invalid { raw, .. } | Self::Ignored(raw) => raw.serialize(serializer),
        }
    }
}
