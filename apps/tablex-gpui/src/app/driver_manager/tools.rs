//! Driver Manager → Tools: install / update / uninstall `tablex-mcp` (the standalone MCP server) at
//! its stable path and hand out the client configuration. The host (`engine_driver_host::tools`)
//! does the downloading, checking and replacing; this file is the state the tab shows, the work
//! it starts off the UI thread, and the two texts the copy buttons produce.

use super::super::mongo_shell::AddonInstallError;
use super::*;
use engine_driver_host::{
    DownloadError, DriverRequirement, InstalledToolRecord, RequirementStatus, ToolCatalogItem,
    ToolCatalogState, ToolError, ToolInstaller, ToolKind, ToolManifestEntry, ToolStore, MCP_TOOL_ID,
    MONGODB_DRIVER_ID, MONGO_SHELL_TOOL_ID, SQRUFF_TOOL_ID,
};
use std::fmt;

/// One line under the tool: what the server may do. The label is the switch in Settings → AI.
pub(in crate::app) const MCP_CAPABILITY_NOTE: &str = "It reads according to each connection's assistant access (the names of your connections are listed to the client even where access is off), and changes data only when \"Let MCP clients run changes\" is on in Settings \u{2192} AI, the connection has \"Read and write\" access and it is not of a production type.";

/// Under the "Allow Keychain access" button (macOS).
pub(in crate::app) const KEYCHAIN_NOTE: &str =
    "macOS asks once per saved password; choose Always Allow. Needed again after an update.";

/// Shown when the manifest does not describe the tool.
const MCP_FALLBACK_DESCRIPTION: &str =
    "Lets MCP clients (Claude Code, Claude Desktop, Cursor) use your saved connections.";
const FORMATTER_FALLBACK_DESCRIPTION: &str = "Formats SQL with sqruff, in a separate process.";

/// One line under a free-standing tool: what it is for.
const FORMATTER_NOTE: &str = "Choose it in Settings \u{203A} Editor \u{203A} Format SQL. It runs in its own process, changes only layout and the case of keywords, and every result is checked against your text before it replaces it. The built-in formatter stays the default.";

// --- state ----------------------------------------------------------------------------------

/// The install of a tool, running or failed (a done or cancelled one leaves no job).
#[derive(Debug)]
pub(in crate::app) struct ToolJob {
    /// Which tool: one install runs at a time, whichever tool it is.
    pub tool_id: String,
    pub kind: JobKind,
    pub state: InstallState,
    pub cancel: Arc<AtomicBool>,
    /// The trust policy and epoch it was created under (see [`Origin`]).
    pub origin: Origin,
}

impl ToolJob {
    fn new(kind: JobKind, entry: &ToolManifestEntry, origin: Origin) -> Self {
        let total = entry
            .artifact_for_target(&crate::drivers::current_target())
            .map_or(0, |artifact| artifact.size_bytes);
        Self {
            tool_id: entry.id.clone(),
            kind,
            state: InstallState::Downloading { bytes: 0, total },
            cancel: Arc::new(AtomicBool::new(false)),
            origin,
        }
    }

    /// A cancel was requested but the worker has not stopped yet.
    pub(in crate::app) fn cancelling(&self) -> bool {
        self.state.is_running() && self.cancel.load(Ordering::Acquire)
    }
}

pub(in crate::app) struct ToolsUi {
    /// `<app data dir>/tools`; `None` when the platform has no data directory.
    pub store: Option<Arc<ToolStore>>,
    /// From the last snapshot (installed state read off the UI thread).
    pub items: Vec<ToolCatalogItem>,
    pub job: Option<ToolJob>,
    pub task: Option<Task<()>>,
    /// The last uninstall that failed (the tool's id, its code and whether trying again can help);
    /// shown like an install failure until the next action.
    pub failure: Option<(String, InstallFailure)>,
    /// An add-on waits for its driver's install: "Install MongoDB driver first" queued both, and
    /// the add-on installs when the driver is in.
    pub after_driver: Option<String>,
    /// The "Allow Keychain access" run (`tablex-mcp --authorize-keychain`): one at a time.
    pub keychain_task: Option<Task<()>>,
    /// What the last run showed the user (also what tests read; the toast itself is not kept).
    pub keychain_notice: Option<String>,
}

impl ToolsUi {
    pub(super) fn new() -> Self {
        Self {
            store: crate::platform::app_data_dir()
                .map(|directory| Arc::new(ToolStore::new(directory.join("tools")))),
            items: Vec::new(),
            job: None,
            task: None,
            failure: None,
            after_driver: None,
            keychain_task: None,
            keychain_notice: None,
        }
    }

    /// The Keychain authorisation is running (its button shows it).
    pub(in crate::app) fn keychain_running(&self) -> bool {
        self.keychain_task.is_some()
    }

    /// The MCP server's row.
    pub(in crate::app) fn item(&self) -> Option<&ToolCatalogItem> {
        self.item_of(MCP_TOOL_ID)
    }

    /// The tools listed in the Tools section: those that stand on their own, in catalog order.
    pub(in crate::app) fn standalone(&self) -> impl Iterator<Item = &ToolCatalogItem> {
        self.items.iter().filter(|item| !item.kind.is_addon())
    }

    /// Whether SQL Formatter (sqruff) is installed (Settings offers it then).
    pub(in crate::app) fn formatter_installed(&self) -> bool {
        self.item_of(SQRUFF_TOOL_ID).is_some_and(|item| item.installed.is_some())
    }

    pub(in crate::app) fn item_of(&self, id: &str) -> Option<&ToolCatalogItem> {
        self.items.iter().find(|item| item.id == id)
    }

    /// The MongoDB Shell add-on's row.
    pub(in crate::app) fn shell_item(&self) -> Option<&ToolCatalogItem> {
        self.item_of(MONGO_SHELL_TOOL_ID)
    }

    /// The install job of tool `id`, when that is the tool the (single) job slot belongs to.
    pub(in crate::app) fn job_of(&self, id: &str) -> Option<&ToolJob> {
        self.job.as_ref().filter(|job| job.tool_id == id)
    }

    fn failure_of(&self, id: &str) -> Option<&InstallFailure> {
        self.failure
            .as_ref()
            .filter(|(tool, _)| tool == id)
            .map(|(_, failure)| failure)
    }

    /// The failure a tool's row shows: a failed install, else the last failed uninstall.
    pub(in crate::app) fn row_failure(&self, id: &str) -> Option<&InstallFailure> {
        self.job_of(id)
            .and_then(|job| match &job.state {
                InstallState::Failed(failure) => Some(failure),
                _ => None,
            })
            .or_else(|| self.failure_of(id))
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(in crate::app) enum ToolAction {
    Install,
    Update,
    /// After a failed install.
    Retry,
    /// The install is running: ✕.
    Cancel,
    Uninstall,
    /// An add-on whose driver is not installed: install the driver, then the add-on.
    InstallDriverFirst,
    /// An add-on whose driver is too old: update the driver, then install the add-on.
    UpdateDriverFirst,
}

/// What an add-on needs from its driver and how the installed driver stands: what the row shows
/// as the reason, and whether the Driver Manager can fix it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(in crate::app) struct AddonNeed {
    pub requirement: DriverRequirement,
    pub status: RequirementStatus,
    /// The driver's display name, for the sentence.
    pub driver_name: String,
    /// The catalog offers a build of the driver that meets the requirement.
    pub fixable: bool,
}

impl AddonNeed {
    /// "Needs MongoDB driver ≥ 0.2.0 (not installed)" / "... (0.1.0 installed)".
    pub(in crate::app) fn reason(&self) -> String {
        let minimum = &self.requirement.min_version;
        match &self.status {
            RequirementStatus::Met => format!("Needs {} driver \u{2265} {minimum}", self.driver_name),
            RequirementStatus::Missing => {
                format!("Needs {} driver \u{2265} {minimum} (not installed)", self.driver_name)
            }
            RequirementStatus::TooOld { installed } => format!(
                "Needs {} driver \u{2265} {minimum} ({installed} installed)",
                self.driver_name
            ),
        }
    }

    /// The row's text: what it needs, short enough for the room beside the button (the whole
    /// [`Self::reason`] is its tooltip).
    pub(in crate::app) fn short_reason(&self) -> String {
        format!("Needs {} \u{2265} {}", self.driver_name, self.requirement.min_version)
    }

    /// The button that fixes it, when the catalog can.
    pub(in crate::app) fn fix_action(&self) -> Option<ToolAction> {
        match (&self.status, self.fixable) {
            (RequirementStatus::Missing, true) => Some(ToolAction::InstallDriverFirst),
            (RequirementStatus::TooOld { .. }, true) => Some(ToolAction::UpdateDriverFirst),
            _ => None,
        }
    }
}

/// The buttons an add-on row offers: [`tool_actions`], except that Install / Update / Retry give
/// way to the driver's own install while `need` (an unmet requirement) holds. A running job only
/// offers its cancel.
pub(in crate::app) fn addon_actions(
    item: &ToolCatalogItem,
    job: Option<&ToolJob>,
    need: Option<&AddonNeed>,
) -> Vec<ToolAction> {
    let actions = tool_actions(item, job);
    let Some(need) = need.filter(|_| !job.is_some_and(|job| job.state.is_active())) else {
        return actions;
    };
    let mut result = Vec::with_capacity(actions.len());
    for action in actions {
        match action {
            ToolAction::Install | ToolAction::Update | ToolAction::Retry => {
                result.extend(need.fix_action().filter(|fix| !result.contains(fix)));
            }
            other => result.push(other),
        }
    }
    result
}

/// The catalog's entry when it can be installed now: listed, valid, built for this platform and
/// not gated by the app version. A withdrawn or unavailable build is not.
pub(in crate::app) fn installable_entry(item: &ToolCatalogItem) -> Option<&ToolManifestEntry> {
    item.entry
        .as_ref()
        .filter(|_| !matches!(item.state, ToolCatalogState::Unavailable { .. }))
}

/// The buttons a tool row offers, in order.
pub(in crate::app) fn tool_actions(
    item: &ToolCatalogItem,
    job: Option<&ToolJob>,
) -> Vec<ToolAction> {
    let installed = item.installed.is_some();
    match job.map(|job| &job.state) {
        Some(state) if state.is_active() => return vec![ToolAction::Cancel],
        Some(InstallState::Failed(_)) => {
            return [
                installable_entry(item).map(|_| ToolAction::Retry),
                installed.then_some(ToolAction::Uninstall),
            ]
            .into_iter()
            .flatten()
            .collect()
        }
        _ => {}
    }
    let primary = match (&item.state, installable_entry(item)) {
        (ToolCatalogState::Available, Some(_)) => Some(ToolAction::Install),
        (ToolCatalogState::UpdateAvailable { .. }, Some(_)) => Some(ToolAction::Update),
        _ => None,
    };
    [primary, installed.then_some(ToolAction::Uninstall)]
        .into_iter()
        .flatten()
        .collect()
}

/// `(installed line, available line)`: "Installed 1.0.0" / "Not installed", "Available 1.1.0".
pub(in crate::app) fn tool_version_lines(item: &ToolCatalogItem) -> (String, Option<String>) {
    let installed = match &item.installed {
        Some(record) => format!("Installed {}", record.version),
        None => "Not installed".to_string(),
    };
    let available = item
        .entry
        .as_ref()
        .map(|entry| format!("Available {}", entry.version));
    (installed, available)
}

pub(in crate::app) fn tool_description(item: &ToolCatalogItem) -> &str {
    item.description.as_deref().unwrap_or(match item.kind {
        ToolKind::SqlFormatter => FORMATTER_FALLBACK_DESCRIPTION,
        _ => MCP_FALLBACK_DESCRIPTION,
    })
}

/// What a free-standing tool is for, in a line under its row.
pub(in crate::app) fn tool_note(kind: ToolKind) -> &'static str {
    match kind {
        ToolKind::SqlFormatter => FORMATTER_NOTE,
        ToolKind::McpServer | ToolKind::MongoShell => MCP_CAPABILITY_NOTE,
    }
}

// --- failures -------------------------------------------------------------------------------

impl From<&ToolError> for InstallFailure {
    fn from(error: &ToolError) -> Self {
        let (code, retryable) = match error {
            ToolError::InvalidId(_) => ("tool.invalid_id", false),
            ToolError::NoBuild { .. } => ("tool.no_build", false),
            ToolError::Download(download) => match download {
                DownloadError::Cancelled => ("tool.cancelled", false),
                DownloadError::Io(_) => ("tool.download_failed", true),
                DownloadError::TooLarge { .. } => ("tool.size_mismatch", false),
                DownloadError::SizeMismatch { .. } => ("tool.size_mismatch", true),
                DownloadError::InsecureUrl(_) => ("tool.insecure_url", false),
                DownloadError::LocalUrlNotAllowed(_) => ("tool.local_url_not_allowed", false),
                DownloadError::UnsupportedScheme(_) => ("tool.unsupported_url", false),
            },
            ToolError::ChecksumMismatch { .. } => ("tool.checksum_mismatch", false),
            ToolError::LinkRefused { .. } => ("tool.link_refused", false),
            ToolError::UnexpectedEntry { .. } => ("tool.unexpected_entry", false),
            ToolError::InUse { .. } => ("tool.in_use", true),
            ToolError::AccessDenied { .. } => ("tool.access_denied", true),
            ToolError::SharedWritable { .. } => ("tool.folder_writable", false),
            ToolError::ForeignOwner { .. } => ("tool.folder_owner", false),
            ToolError::RequiresDriver { .. } => ("tool.requires_driver", false),
            ToolError::Store(_) => ("tool.driver_store", true),
            ToolError::Record(_) => ("tool.record_invalid", false),
            ToolError::Io(_) => ("tool.io", true),
        };
        Self::new(code, error.to_string(), retryable)
    }
}

// --- the texts the copy buttons produce -----------------------------------------------------------

#[derive(Clone, Debug, PartialEq, Eq)]
pub(in crate::app) enum ClientTextError {
    /// JSON and shell lines are text; a path that is not valid UTF-8 cannot be written faithfully.
    NotUtf8,
    /// A character a shell would interpret inside the quotes (or a control character).
    UnsafeForShell(char),
    /// A Windows network path (`\\server\share`): Git Bash rewrites it.
    UncPath,
}

impl fmt::Display for ClientTextError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::NotUtf8 => write!(
                f,
                "The install path is not valid UTF-8, so it cannot be written into a configuration"
            ),
            Self::UnsafeForShell(c) => write!(
                f,
                "The install path contains {c:?}, which the command line would interpret. Use \"Copy client configuration\" instead"
            ),
            Self::UncPath => write!(
                f,
                "The install path is a network path, which some shells rewrite. Use \"Copy client configuration\" instead"
            ),
        }
    }
}

impl std::error::Error for ClientTextError {}

/// How the Claude Code command line is quoted: the shell family of the OS the app runs on.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(in crate::app) enum ShellQuoting {
    /// macOS and Linux (sh, bash, zsh, fish).
    Posix,
    /// Windows (cmd and PowerShell).
    Windows,
}

impl ShellQuoting {
    pub(in crate::app) const fn native() -> Self {
        if cfg!(windows) {
            Self::Windows
        } else {
            Self::Posix
        }
    }

    /// `claude mcp add tablex -- <quoted path>`.
    ///
    /// * [`Self::Posix`]: the path in single quotes, which no POSIX shell (and not fish) expands
    ///   anything inside. A `'` is written `'\''` (close, escaped quote, reopen) and a `\` as
    ///   `'\\'` (close, escaped backslash, reopen): outside the quotes `\'` and `\\` mean the
    ///   same character in sh, bash, zsh and fish, whereas a backslash INSIDE single quotes is
    ///   literal in the first three and an escape in fish. Spaces, `$`, backticks and `*` are
    ///   literal. Control characters (ESC, newline, ...) are refused: they do not belong in a
    ///   path anyone pastes into a terminal.
    /// * [`Self::Windows`]: the path in double quotes, which cmd and PowerShell both use to keep a
    ///   path with spaces as one argument. A Windows file name cannot contain `"`. What those
    ///   shells still interpret inside double quotes is refused rather than guessed at, since no
    ///   single quoting is right for both: `%` (cmd), `!` (cmd with delayed expansion), `$` and
    ///   the backtick (PowerShell), the typographic double quotes U+201C / U+201D / U+201E
    ///   (PowerShell reads them as `"`), control characters, and a `\\server\share` path (Git
    ///   Bash rewrites it).
    pub(in crate::app) fn command(self, path: &Path) -> Result<String, ClientTextError> {
        let path = path.to_str().ok_or(ClientTextError::NotUtf8)?;
        if let Some(c) = path.chars().find(|c| c.is_control()) {
            return Err(ClientTextError::UnsafeForShell(c));
        }
        let quoted = match self {
            Self::Posix => {
                let mut quoted = String::with_capacity(path.len() + 2);
                quoted.push('\'');
                for c in path.chars() {
                    match c {
                        '\'' => quoted.push_str("'\\''"),
                        '\\' => quoted.push_str("'\\\\'"),
                        other => quoted.push(other),
                    }
                }
                quoted.push('\'');
                quoted
            }
            Self::Windows => {
                if path.starts_with("\\\\") {
                    return Err(ClientTextError::UncPath);
                }
                if let Some(c) = path.chars().find(|c| {
                    matches!(
                        c,
                        '"' | '%' | '!' | '$' | '`' | '\u{201C}' | '\u{201D}' | '\u{201E}'
                    )
                }) {
                    return Err(ClientTextError::UnsafeForShell(c));
                }
                format!("\"{path}\"")
            }
        };
        Ok(format!("claude mcp add tablex -- {quoted}"))
    }
}

/// The block MCP clients take: `{"mcpServers": {"tablex": {"command": "<path>", "args": []}}}`,
/// written by a JSON library, so a path with quotes, backslashes (Windows) or Unicode is valid JSON.
/// Structs, not a `json!` map, so the key order (`command` first) does not depend on whether some
/// dependency turned on serde_json's `preserve_order`.
pub(in crate::app) fn client_config_json(path: &Path) -> Result<String, ClientTextError> {
    #[derive(serde::Serialize)]
    struct Server<'a> {
        command: &'a str,
        args: [&'a str; 0],
    }
    #[derive(serde::Serialize)]
    struct Servers<'a> {
        tablex: Server<'a>,
    }
    #[derive(serde::Serialize)]
    struct Config<'a> {
        #[serde(rename = "mcpServers")]
        mcp_servers: Servers<'a>,
    }
    let command = path.to_str().ok_or(ClientTextError::NotUtf8)?;
    let config = Config {
        mcp_servers: Servers {
            tablex: Server { command, args: [] },
        },
    };
    Ok(serde_json::to_string_pretty(&config).expect("plain strings always serialize"))
}

/// `text` as a Markdown code span that renders exactly this text (no escapes apply inside one):
/// the fence is longer than any run of backticks in the text, and a space on each side keeps a
/// leading or trailing backtick from merging into it (CommonMark strips one space each side).
pub(in crate::app) fn code_span(text: &str) -> String {
    let longest = text
        .split(|c| c != '`')
        .map(str::len)
        .max()
        .unwrap_or_default();
    let fence = "`".repeat(longest + 1);
    format!("{fence} {text} {fence}")
}

/// Runs `<tool> --authorize-keychain` and returns the text to show: its output on success, its
/// error (else its output) on failure. Blocks until the user has answered every macOS prompt.
fn run_keychain_authorization(tool: &Path) -> String {
    let output = match std::process::Command::new(tool)
        .arg("--authorize-keychain")
        .stdin(std::process::Stdio::null())
        .output()
    {
        Ok(output) => output,
        Err(error) => return format!("Could not run the MCP server: {error}"),
    };
    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);
    let text = if output.status.success() || stderr.trim().is_empty() {
        stdout
    } else {
        stderr
    };
    let text = brief(text.trim());
    if text.is_empty() {
        match output.status.code() {
            Some(code) if !output.status.success() => format!("The MCP server exited with code {code}"),
            _ => "Done".to_string(),
        }
    } else {
        text
    }
}

/// The first three lines of `text`, and its last one when there are more (the verdict).
fn brief(text: &str) -> String {
    let lines: Vec<&str> = text.lines().collect();
    if lines.len() <= 4 {
        return lines.join("\n");
    }
    format!("{}\n\u{2026}\n{}", lines[..3].join("\n"), lines[lines.len() - 1])
}

// --- work -----------------------------------------------------------------------------------------

/// Download, verify and install `entry`. A cancel at any point (or any error after the cancel flag
/// was raised) is a [`JobError::Cancelled`] and leaves no partial file behind.
fn run_tool_install(
    store: &ToolStore,
    drivers: &InstalledDriverStore,
    policy: TrustPolicy,
    entry: &ToolManifestEntry,
    cancel: &Arc<AtomicBool>,
    sink: &dyn ProgressSink,
) -> Result<InstalledToolRecord, JobError> {
    ToolInstaller::new(crate::drivers::current_target(), store)
        .with_trust_policy(policy)
        .with_drivers(drivers)
        .install(
            entry,
            |artifact, transport| {
                crate::drivers::open_artifact(
                    &artifact.url,
                    artifact.size_bytes,
                    transport,
                    policy,
                    cancel,
                )
            },
            cancel,
            sink,
        )
        .map_err(|error| {
            if error.is_cancelled() || cancel.load(Ordering::Acquire) {
                JobError::Cancelled
            } else {
                JobError::Failed(InstallFailure::from(&error))
            }
        })
}

impl AppRoot {
    /// A button of tool `id`'s row (the MCP server, the SQL formatter, or the MongoDB Shell add-on).
    pub(in crate::app) fn run_tool_action_of(
        &mut self,
        id: &str,
        action: ToolAction,
        cx: &mut Context<Self>,
    ) {
        let Some(item) = self.driver_ui.tools.item_of(id) else {
            return;
        };
        // Only the catalog's CURRENT, installable entry is ever installed (also by Retry): a
        // build that was withdrawn, or is unavailable here, is not resurrected from a failed job.
        let entry = installable_entry(item).cloned();
        let job = self.driver_ui.tools.job_of(id);
        let retry_kind = job.map(|job| job.kind);
        let failed = job.is_some_and(|job| matches!(job.state, InstallState::Failed(_)));
        match (action, entry) {
            (ToolAction::Install, Some(entry)) => {
                self.start_tool_install(entry, JobKind::Install, cx)
            }
            (ToolAction::Update, Some(entry)) => {
                self.start_tool_install(entry, JobKind::Update, cx)
            }
            (ToolAction::Retry, entry) => {
                if let (true, Some(kind), Some(entry)) = (failed, retry_kind, entry) {
                    self.driver_ui.tools.job = None;
                    self.start_tool_install(entry, kind, cx);
                } else if failed {
                    // Nothing installable any more: the failure is history.
                    self.driver_ui.tools.job = None;
                }
            }
            (ToolAction::Cancel, _) => {
                if let Some(job) = self
                    .driver_ui
                    .tools
                    .job_of(id)
                    .filter(|job| job.state.is_active())
                {
                    job.cancel.store(true, Ordering::Release);
                }
                // The driver it waited for is not cancelled, only the add-on that was to follow.
                if self.driver_ui.tools.after_driver.as_deref() == Some(id) {
                    self.driver_ui.tools.after_driver = None;
                }
            }
            (ToolAction::Uninstall, _) => self.uninstall_tool(id, cx),
            (ToolAction::InstallDriverFirst | ToolAction::UpdateDriverFirst, _) => {
                self.install_driver_for_addon(id, cx)
            }
            _ => {}
        }
        cx.notify();
    }

    /// How the installed drivers stand against the requirements of add-on `item`: `None` when
    /// they are met (or the add-on has none).
    pub(in crate::app) fn addon_need(&self, item: &ToolCatalogItem) -> Option<AddonNeed> {
        let ui = &self.driver_ui;
        item.requires().iter().find_map(|requirement| {
            // Only an installed, enabled driver counts (the set follows an uninstall at once).
            let installed = ui
                .installed
                .contains(&requirement.driver)
                .then(|| ui.records.get(&requirement.driver))
                .flatten()
                .map(|record| record.version.as_str());
            let status = requirement.check(installed);
            if status == RequirementStatus::Met {
                return None;
            }
            let row = ui.model.row(&requirement.driver);
            // The catalog's build must satisfy the requirement, or installing it fixes nothing.
            let fixable = row
                .filter(|row| !matches!(row.status, RowStatus::Unavailable(_)))
                .and_then(|row| row.entry.as_ref())
                .is_some_and(|entry| {
                    requirement.check(Some(&entry.version)) == RequirementStatus::Met
                });
            Some(AddonNeed {
                requirement: requirement.clone(),
                status,
                driver_name: row.map_or_else(
                    || requirement.driver.clone(),
                    |row| row.name.clone(),
                ),
                fixable,
            })
        })
    }

    /// "Install MongoDB driver first": queues the driver, then the add-on follows.
    fn install_driver_for_addon(&mut self, id: &str, cx: &mut Context<Self>) {
        let Some(need) = self
            .driver_ui
            .tools
            .item_of(id)
            .and_then(|item| self.addon_need(item))
        else {
            return;
        };
        let driver = need.requirement.driver.clone();
        let Some(entry) = self
            .driver_ui
            .model
            .row(&driver)
            .filter(|row| !matches!(row.status, RowStatus::Unavailable(_)))
            .and_then(|row| row.entry.clone())
            .filter(|_| need.fixable)
        else {
            return;
        };
        let kind = match need.status {
            RequirementStatus::TooOld { .. } => JobKind::Update,
            _ => JobKind::Install,
        };
        match self.enqueue_driver_install(entry, kind, cx) {
            Enqueued::Queued | Enqueued::AlreadyActive => {
                self.driver_ui.tools.after_driver = Some(id.to_string());
            }
            Enqueued::Stale => {}
        }
    }

    /// The driver install an add-on waited for left the queue without finishing (a queued job
    /// cancelled, "Cancel all", a catalog no longer trusted): the add-on is given up, and a script
    /// waiting for it fails with the reason, instead of waiting for ever.
    pub(in crate::app) fn drop_addon_wait_for_missing_driver(&mut self, cx: &mut Context<Self>) {
        let Some(tool) = self.driver_ui.tools.after_driver.clone() else {
            return;
        };
        let coming = self.driver_ui.tools.item_of(&tool).is_some_and(|item| {
            item.requires().iter().any(|need| {
                self.driver_ui
                    .queue
                    .job(&need.driver)
                    .is_some_and(|job| job.state.is_active())
            })
        });
        if !coming {
            self.driver_ui.tools.after_driver = None;
            self.on_addon_install_ended(&tool, Err(AddonInstallError::DriverNotInstalled), cx);
        }
    }

    /// A driver install ended: the add-on that waited for it installs now, or is given up.
    pub(in crate::app) fn on_driver_install_for_addon(
        &mut self,
        driver: &str,
        outcome: &PromptInstall,
        cx: &mut Context<Self>,
    ) {
        let Some(tool) = self.driver_ui.tools.after_driver.clone() else {
            return;
        };
        let waits_for = self
            .driver_ui
            .tools
            .item_of(&tool)
            .is_some_and(|item| item.requires().iter().any(|need| need.driver == driver));
        if !waits_for {
            return;
        }
        self.driver_ui.tools.after_driver = None;
        if !matches!(outcome, PromptInstall::Installed) {
            self.on_addon_install_ended(&tool, Err(AddonInstallError::DriverNotInstalled), cx);
            return;
        }
        let item = self.driver_ui.tools.item_of(&tool);
        // Already installed (only its driver was old): nothing more to install, it can run now.
        if item.is_some_and(|item| item.installed.is_some()) {
            self.on_addon_install_ended(&tool, Ok(()), cx);
            return;
        }
        let (entry, kind) = match item {
            Some(item) => (
                installable_entry(item).cloned(),
                if item.installed.is_some() { JobKind::Update } else { JobKind::Install },
            ),
            None => (None, JobKind::Install),
        };
        match entry {
            Some(entry) => self.start_tool_install(entry, kind, cx),
            None => self.on_addon_install_ended(&tool, Err(AddonInstallError::NotOffered), cx),
        }
    }

    fn start_tool_install(
        &mut self,
        entry: ToolManifestEntry,
        kind: JobKind,
        cx: &mut Context<Self>,
    ) {
        let tools = &self.driver_ui.tools;
        // One operation on the tool at a time: not while its install runs, nor while an
        // operation (an uninstall) is still working on the same folder.
        if self.scenario_ui.service.is_some()
            || !self.driver_ui.catalog_is_current()
            || self.driver_ui.op_task.is_some()
            || tools.job.as_ref().is_some_and(|job| job.state.is_active())
        {
            let id = entry.id.clone();
            self.on_addon_install_ended(&id, Err(AddonInstallError::Busy), cx);
            return;
        }
        let Some(store) = tools.store.clone() else {
            self.driver_ui.catalog_error = Some(
                "TableX has no data directory on this system, so tools cannot be installed".into(),
            );
            return;
        };
        let job = ToolJob::new(kind, &entry, self.driver_ui.current_origin());
        let cancel = Arc::clone(&job.cancel);
        self.driver_ui.tools.job = Some(job);
        self.driver_ui.tools.failure = None;
        let (policy, drivers) = (self.driver_ui.policy, self.driver_ui.store.clone());
        let (tx, rx) = smol::channel::unbounded::<InstallProgress>();
        let worker = cx.background_executor().spawn(async move {
            smol::unblock(move || {
                // Throttled by the host (>= 64 KiB or 50 ms): a bounded trickle.
                let sink = move |progress: InstallProgress| {
                    let _ = tx.try_send(progress);
                };
                run_tool_install(&store, &drivers, policy, &entry, &cancel, &sink)
            })
            .await
        });
        self.driver_ui.tools.task = Some(cx.spawn(async move |this, cx| {
            // The sender lives in the worker's sink: the loop ends with the job.
            while let Ok(mut latest) = rx.recv().await {
                while let Ok(next) = rx.try_recv() {
                    latest = next;
                }
                if this
                    .update(cx, |this, cx| this.on_tool_progress(latest, cx))
                    .is_err()
                {
                    return;
                }
            }
            let result = worker.await;
            let _ = this.update(cx, |this, cx| this.on_tool_finished(result, cx));
        }));
        // The tab that asked for the add-on shows its progress too.
        self.on_addon_install_started(cx);
    }

    fn on_tool_progress(&mut self, progress: InstallProgress, cx: &mut Context<Self>) {
        let tool = self.driver_ui.tools.job.as_ref().map(|job| job.tool_id.clone());
        if let Some(job) = self.driver_ui.tools.job.as_mut() {
            job.state.apply(progress);
        }
        if self.driver_manager_visible() {
            cx.notify();
        }
        if let Some(tool) = tool {
            self.on_addon_install_progress(&tool, cx);
        }
    }

    fn on_tool_finished(
        &mut self,
        result: Result<InstalledToolRecord, JobError>,
        cx: &mut Context<Self>,
    ) {
        let current = self.driver_ui.current_origin();
        let tools = &mut self.driver_ui.tools;
        tools.task = None;
        let tool_id = tools
            .job
            .as_ref()
            .map_or_else(|| MCP_TOOL_ID.to_string(), |job| job.tool_id.clone());
        let name = tools
            .item_of(&tool_id)
            .map_or_else(|| tool_id.clone(), |item| item.display_name.clone());
        // A job made before the catalog stopped being trusted: its result is not adopted (the row
        // does not claim an install it did not choose) and a failure is not kept for Retry.
        let from_trusted = tools.job.as_ref().is_some_and(|job| job.origin == current);
        if !from_trusted {
            tools.job = None;
            if result.is_ok() {
                self.report_warning(
                    Area::Driver,
                    None,
                    format!("{name} finished installing from a catalog that is no longer trusted; check it in the Driver Manager"),
                );
                // It is on disk now: show what is there.
                self.reload_driver_catalog(cx);
            }
            self.on_addon_install_ended(&tool_id, Err(AddonInstallError::CatalogNotTrusted), cx);
            cx.notify();
            return;
        }
        match result {
            Ok(record) => {
                tools.job = None;
                // Show the new state at once; the reload below re-reads it from disk.
                if let Some(item) = tools.items.iter_mut().find(|item| item.id == record.id) {
                    item.state = ToolCatalogState::Installed {
                        version: record.version.clone(),
                    };
                    item.problem = None;
                    item.installed = Some(record.clone());
                }
                self.reload_driver_catalog(cx);
                // Settings › Editor can choose SQL Formatter (sqruff) the moment it is installed.
                self.rescan_format_tools_if_seen(cx);
                self.on_addon_install_ended(&tool_id, Ok(()), cx);
            }
            Err(JobError::Cancelled) => {
                tools.job = None;
                self.on_addon_install_ended(&tool_id, Err(AddonInstallError::Cancelled), cx);
            }
            Err(JobError::Failed(failure)) => {
                let mut report = crate::error_log::Report::new(
                    crate::error_log::Severity::Error,
                    Area::Driver,
                    format!("{name}: {}", failure.message),
                );
                report.code = Some(failure.code.to_string());
                report.retryable = failure.retryable;
                let message = failure.message.clone();
                if let Some(job) = tools.job.as_mut() {
                    job.state = InstallState::Failed(failure);
                }
                self.report_event(None, report);
                self.on_addon_install_ended(&tool_id, Err(AddonInstallError::Failed(message)), cx);
            }
        }
        cx.notify();
    }

    fn uninstall_tool(&mut self, id: &str, cx: &mut Context<Self>) {
        let Some(store) = self.driver_ui.tools.store.clone() else {
            return;
        };
        if self
            .driver_ui
            .tools
            .job
            .as_ref()
            .is_some_and(|job| job.state.is_active())
        {
            return;
        }
        let id = id.to_string();
        let work_id = id.clone();
        let started = self.run_driver_operation(
            format!("Uninstalling {id}..."),
            move || {
                Ok(
                    match ToolInstaller::new(crate::drivers::current_target(), &store)
                        .uninstall(&work_id)
                    {
                        Ok(_) => OpResult::ToolRemoved,
                        // Typed: the row keeps the code and whether trying again can help.
                        Err(error) => {
                            OpResult::ToolUninstallFailed(work_id, InstallFailure::from(&error))
                        }
                    },
                )
            },
            cx,
        );
        if started {
            self.driver_ui.tools.job = None;
            self.driver_ui.tools.failure = None;
        }
    }

    pub(super) fn copy_tool_client_config(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.copy_tool_text(
            "Client configuration copied",
            client_config_json,
            window,
            cx,
        );
    }

    pub(super) fn copy_tool_claude_command(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.copy_tool_text(
            "Claude Code command copied",
            |path| ShellQuoting::native().command(path),
            window,
            cx,
        );
    }

    /// "Allow Keychain access": runs the installed `tablex-mcp --authorize-keychain` off the UI
    /// thread (macOS asks, once per saved password, while it runs) and shows what it printed.
    /// One run at a time.
    pub(super) fn allow_tool_keychain_access(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.driver_ui.tools.keychain_running() {
            return;
        }
        let Some(path) = self
            .driver_ui
            .tools
            .item()
            .filter(|item| item.installed.is_some())
            .map(|item| item.path.clone())
        else {
            return;
        };
        let work = cx
            .background_executor()
            .spawn(async move { smol::unblock(move || run_keychain_authorization(&path)).await });
        self.driver_ui.tools.keychain_task = Some(cx.spawn_in(window, async move |this, cx| {
            let notice = work.await;
            let _ = this.update_in(cx, |this, window, cx| {
                this.driver_ui.tools.keychain_task = None;
                this.driver_ui.tools.keychain_notice = Some(notice.clone());
                window.push_notification(notice, cx);
                cx.notify();
            });
        }));
        cx.notify();
    }

    fn copy_tool_text(
        &mut self,
        notice: &'static str,
        text: impl FnOnce(&Path) -> Result<String, ClientTextError>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(path) = self
            .driver_ui
            .tools
            .item()
            .filter(|item| item.installed.is_some())
            .map(|item| item.path.clone())
        else {
            return;
        };
        match text(&path) {
            Ok(text) => {
                cx.write_to_clipboard(gpui::ClipboardItem::new_string(text));
                window.push_notification(notice, cx);
            }
            // Say why instead of copying something that would be read differently.
            Err(error) => window.push_notification(error.to_string(), cx),
        }
    }

    /// UI scenarios: the tool row as installed (`installed`) or still to install. Nothing touches
    /// the disk or the network.
    pub(in crate::app) fn seed_driver_tools_fixture(&mut self, installed: bool) {
        let target = crate::drivers::current_target();
        let version = "1.0.0";
        let size = 7_168_848;
        let entry = ToolManifestEntry {
            id: MCP_TOOL_ID.into(),
            kind: engine_driver_host::ToolKind::McpServer,
            display_name: "TableX MCP server".into(),
            version: version.into(),
            description: Some(MCP_FALLBACK_DESCRIPTION.into()),
            min_app_version: None,
            license: Some("AGPL-3.0-only".into()),
            source_repo: Some("https://github.com/tablex-org/tablex".into()),
            requires: Vec::new(),
            artifacts: vec![engine_driver_host::DriverArtifact {
                target: Some(target.id()),
                url: "https://example.com/tablex-mcp".into(),
                sha256: "00".repeat(32),
                size_bytes: size,
                ..Default::default()
            }],
        };
        let record = installed.then(|| InstalledToolRecord {
            id: MCP_TOOL_ID.into(),
            version: version.into(),
            sha256: "ab".repeat(32),
            size_bytes: size,
            installed_at: "2026-10-14T09:30:00Z".into(),
            requires: Vec::new(),
        });
        self.driver_ui.tools.items = vec![ToolCatalogItem {
            id: MCP_TOOL_ID.into(),
            kind: engine_driver_host::ToolKind::McpServer,
            display_name: entry.display_name.clone(),
            version: version.into(),
            description: entry.description.clone(),
            state: if installed {
                ToolCatalogState::Installed {
                    version: version.into(),
                }
            } else {
                ToolCatalogState::Available
            },
            entry: Some(entry),
            installed: record,
            path: PathBuf::from(
                "/Users/you/Library/Application Support/tablex/tools/tablex-mcp/tablex-mcp",
            ),
            problem: None,
        }];
    }
}

/// UI scenarios and captures: which state the SQL formatter's row is in.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(in crate::app) enum FormatterSeed {
    Available,
    /// The two below are for tests (a build that is not a test never seeds them).
    #[cfg_attr(not(test), allow(dead_code))]
    Installed,
}

impl AppRoot {
    /// SQL Formatter (sqruff) in the tools list, without touching the disk or the network.
    pub(in crate::app) fn seed_sql_formatter_fixture(&mut self, seed: FormatterSeed) {
        let target = crate::drivers::current_target();
        let size = FORMATTER_FIXTURE_BYTES;
        let (installed_version, latest) = match seed {
            FormatterSeed::Installed => (Some("0.1.0"), "0.1.0"),
            FormatterSeed::Available => (None, "0.1.0"),
        };
        let entry = ToolManifestEntry {
            id: SQRUFF_TOOL_ID.into(),
            kind: ToolKind::SqlFormatter,
            display_name: "SQL Formatter (sqruff)".into(),
            version: latest.into(),
            description: Some("Formats SQL in the editor with sqruff, in a separate process.".into()),
            min_app_version: None,
            license: Some("Apache-2.0".into()),
            source_repo: Some("https://github.com/quarylabs/sqruff".into()),
            requires: Vec::new(),
            artifacts: vec![engine_driver_host::DriverArtifact {
                target: Some(target.id()),
                url: "https://example.com/tablex-sqruff".into(),
                sha256: "00".repeat(32),
                size_bytes: size,
                ..Default::default()
            }],
        };
        let record = installed_version.map(|version| InstalledToolRecord {
            id: SQRUFF_TOOL_ID.into(),
            version: version.into(),
            sha256: "ef".repeat(32),
            size_bytes: size,
            installed_at: "2026-10-22T09:30:00Z".into(),
            requires: Vec::new(),
        });
        let state = match installed_version {
            Some(version) if version == latest => ToolCatalogState::Installed { version: version.into() },
            Some(version) => ToolCatalogState::UpdateAvailable { installed: version.into(), latest: latest.into() },
            None => ToolCatalogState::Available,
        };
        self.driver_ui.tools.items.retain(|item| item.id != SQRUFF_TOOL_ID);
        self.driver_ui.tools.items.push(ToolCatalogItem {
            id: SQRUFF_TOOL_ID.into(),
            kind: ToolKind::SqlFormatter,
            display_name: entry.display_name.clone(),
            version: latest.into(),
            description: entry.description.clone(),
            state,
            entry: Some(entry),
            installed: record,
            path: PathBuf::from("/Users/you/Library/Application Support/tablex/tools/tablex-sqruff/tablex-sqruff"),
            problem: None,
        });
    }
}

/// Size of the formatter in the fixtures (the dist build, to the kilobyte).
const FORMATTER_FIXTURE_BYTES: u64 = 9_000_000;

/// UI scenarios and captures: which state the MongoDB Shell add-on's row is in.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(in crate::app) enum ShellAddonSeed {
    /// Not installed, its driver is fine: Install.
    Available,
    Installed,
    /// Installed 0.1.0, 0.2.0 offered: Update.
    Update,
    /// Not installed and the installed MongoDB driver is older than it needs: the button
    /// updates the driver first.
    NeedsDriver,
}

impl ShellAddonSeed {
    /// `TABLEX_SEED_SHELL_ADDON=<available|installed|update|needs-driver>` (capture-only).
    pub(in crate::app) fn from_environment() -> Self {
        match std::env::var("TABLEX_SEED_SHELL_ADDON").as_deref() {
            Ok("installed") => Self::Installed,
            Ok("update") => Self::Update,
            Ok("needs-driver") => Self::NeedsDriver,
            _ => Self::Available,
        }
    }
}

impl AppRoot {
    /// The MongoDB Shell add-on next to the MCP server in the tools list, without touching the
    /// disk or the network. Its driver is the fixture's MongoDB (2.0.3 installed, 2.1.0 offered).
    pub(in crate::app) fn seed_shell_addon_fixture(&mut self, seed: ShellAddonSeed) {
        let target = crate::drivers::current_target();
        let size = 3_622_512;
        let (installed_version, latest) = match seed {
            ShellAddonSeed::Installed => (Some("0.2.0"), "0.2.0"),
            ShellAddonSeed::Update => (Some("0.1.0"), "0.2.0"),
            ShellAddonSeed::Available | ShellAddonSeed::NeedsDriver => (None, "0.2.0"),
        };
        let minimum = if seed == ShellAddonSeed::NeedsDriver { "2.1.0" } else { "2.0.0" };
        let requires = vec![DriverRequirement {
            driver: MONGODB_DRIVER_ID.into(),
            min_version: minimum.into(),
        }];
        let entry = ToolManifestEntry {
            id: MONGO_SHELL_TOOL_ID.into(),
            kind: ToolKind::MongoShell,
            display_name: "MongoDB Shell".into(),
            version: latest.into(),
            description: Some("Runs mongosh-style scripts in a MongoDB query tab.".into()),
            min_app_version: None,
            license: Some("AGPL-3.0-only".into()),
            source_repo: Some("https://github.com/tablex-org/tablex".into()),
            requires: requires.clone(),
            artifacts: vec![engine_driver_host::DriverArtifact {
                target: Some(target.id()),
                url: "https://example.com/tablex-mongo-shell".into(),
                sha256: "00".repeat(32),
                size_bytes: size,
                ..Default::default()
            }],
        };
        let record = installed_version.map(|version| InstalledToolRecord {
            id: MONGO_SHELL_TOOL_ID.into(),
            version: version.into(),
            sha256: "cd".repeat(32),
            size_bytes: size,
            installed_at: "2026-10-16T09:30:00Z".into(),
            requires,
        });
        let state = match installed_version {
            Some(version) if version == latest => ToolCatalogState::Installed { version: version.into() },
            Some(version) => ToolCatalogState::UpdateAvailable {
                installed: version.into(),
                latest: latest.into(),
            },
            None => ToolCatalogState::Available,
        };
        self.driver_ui.tools.items.retain(|item| item.id != MONGO_SHELL_TOOL_ID);
        self.driver_ui.tools.items.push(ToolCatalogItem {
            id: MONGO_SHELL_TOOL_ID.into(),
            kind: ToolKind::MongoShell,
            display_name: entry.display_name.clone(),
            version: latest.into(),
            description: entry.description.clone(),
            state,
            entry: Some(entry),
            installed: record,
            path: PathBuf::from(
                "/Users/you/Library/Application Support/tablex/tools/tablex-mongo-shell/tablex-mongo-shell",
            ),
            problem: None,
        });
    }
}
