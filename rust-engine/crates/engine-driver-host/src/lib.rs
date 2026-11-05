mod arrow;
mod download;
mod dynlib;
mod image;
mod installer;
mod loader;
mod manifest;
mod mongo_shell;
mod os_fs;
mod page;
mod policy;
mod progress;
mod registry;
mod shared_session;
mod signer;
mod sql_format;
mod store;
mod target;
mod tools;
mod trust;
mod verifier;

pub use arrow::{ArrowPageError, ArrowPageLease, ImportedArrowBatch};
pub use download::{
    classify_download_url, stream_to_file, DownloadError, DownloadTransport, DownloadedArtifact,
};
pub use installer::{ActivatedDriver, DriverInstaller, InstallError, UninstallOutcome};
pub use loader::{
    DriverHandle, DriverLoader, DriverOperation, DriverOwners, DriverSession, HostCell,
    HostCellBatch, LoaderError, QueryRunner,
};
pub use manifest::{
    DriverArtifact, DriverCategory, DriverManifest, DriverManifestEntry, DriverRequirement,
    ManifestTool, RequirementStatus, ToolKind, ToolManifestEntry, ToolManifestError,
    MANIFEST_SCHEMA_VERSION, MAX_TOOL_BYTES, MCP_TOOL_ID, MONGODB_DRIVER_ID, MONGO_SHELL_TOOL_ID,
    SQRUFF_TOOL_ID,
};
pub use mongo_shell::{MongoShell, OpenCursor, ShellCancel, ShellCommands, ShellHostError, ShellRun};
pub use page::{PageError, PageLease, PageSlice};
pub use policy::TrustPolicy;
pub use progress::{
    InstallProgress, InstallStep, NoProgress, ProgressSink, PROGRESS_MIN_BYTES,
    PROGRESS_MIN_INTERVAL,
};
pub use registry::{
    resolve_manifest_source, DriverCatalogItem, DriverCatalogState, FullCatalog, ManifestFetcher,
    RegistryError, RegistryService,
};
pub use shared_session::SharedDriverSession;
pub use signer::SignerConfig;
pub use sql_format::{kill_process_tree, run_filter, FormatCancel, FormatHostError, SqlFormatTool};
pub use store::{InstalledDriverRecord, InstalledDriverStore, MigratedInstall, StoreError};
pub use target::{DriverTarget, TargetArch, TargetOs};
pub use tools::{
    InstalledToolRecord, ToolCatalogItem, ToolCatalogState, ToolError, ToolInstaller, ToolStore,
};
pub use trust::{
    public_key_from_base64, public_keys_from_list, signature_source, ManifestTrust,
    ManifestTrustError,
};
pub use verifier::{DriverVerificationError, DriverVerifier};
