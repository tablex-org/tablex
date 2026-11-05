//! A backup or a restore from start to end, without a window: reach the server (through the connection's own
//! SSH tunnel when it has one), find the tool, write the private files, run the tool, and put the result in place
//! or take the partial one away. The app wraps this in a tab; the tests run it against fake tools.
//!
//! * A backup is written to `<name>.<id>.tablex-part` (the id is this run's own: two runs for one name never meet) and renamed
//!   when the tool succeeded: a failed or cancelled run never leaves a half file where a backup should be, and never replaces
//!   an older good one.
//! * The password is in a private file for the length of the run ([`PrivateDir`]) and nowhere else.
//! * The tunnel is held until the tool has ended.

use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant};

use engine_connect::records::RuntimeUri;
use engine_connect::tool_endpoint::ToolTunnelError;
use engine_connect::tunnel::ssh::{SessionTunnel, TunnelAccess};

use super::command::{self, Invocation, MysqlDumpOptions, OptionsError, PgDumpOptions, PgFormat, PgRestoreOptions};
use super::endpoint::{Endpoint, EndpointError};
use super::process::{self, CancelToken, Ending, Execution, RunState, Stream};
use super::secrets::{self, PassfileError, PrivateDir, PrivateDirs};
use super::toc::Toc;
use super::tools::{Found, Locator, Need, Tool, ToolError, ToolPaths};
use crate::connection_tls::TlsMode;
use crate::connections::{ConnectionKind, SavedConnection};

/// The suffix of the file or directory a backup is written to until it is complete.
pub const PARTIAL_SUFFIX: &str = ".tablex-part";

/// Why a job stopped before or while running its tool.
#[derive(Debug)]
pub enum JobError {
    /// The connection's address could not be made (the credential store, a missing password).
    Uri(String),
    Endpoint(EndpointError),
    Tunnel(String),
    /// The connection goes through a tunnel and none is open.
    TunnelNotOpen,
    /// The password file cannot carry this user or password.
    Passfile(PassfileError),
    /// `pg_restore` restores PostgreSQL archives only.
    NotPostgres,
    /// MySQL's client cannot check a server name through a tunnel (it connects to 127.0.0.1).
    TlsNameThroughTunnel,
    Tool(ToolError),
    Options(OptionsError),
    /// The private directory for the password could not be made.
    Private(std::io::Error),
    Output(OutputError),
    /// The tool could not be started.
    Start(std::io::Error),
    /// The archive could not be read.
    Archive(String),
    /// Cancelled before the tool started.
    Cancelled,
}

impl std::fmt::Display for JobError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Uri(message) | Self::Tunnel(message) | Self::Archive(message) => f.write_str(&crate::sessions::redact_secrets(message)),
            Self::Endpoint(error) => error.fmt(f),
            Self::TunnelNotOpen => f.write_str("Refusing to connect directly: this connection is configured to go through an SSH tunnel, which is not open."),
            Self::TlsNameThroughTunnel => f.write_str("The mysqldump family cannot verify the server's name (VERIFY_IDENTITY) through an SSH tunnel: it connects to the tunnel's local address. Use VERIFY_CA in the connection's TLS settings for the backup."),
            Self::Passfile(error) => error.fmt(f),
            Self::NotPostgres => f.write_str("pg_restore restores into PostgreSQL only: a MySQL or MariaDB backup is an SQL script, which Run SQL File restores."),
            Self::Tool(error) => error.fmt(f),
            Self::Options(error) => error.fmt(f),
            Self::Private(error) => write!(f, "Could not make a private folder for the password: {error}"),
            Self::Output(error) => error.fmt(f),
            Self::Start(error) => write!(f, "The tool could not be started: {error}"),
            Self::Cancelled => f.write_str("Cancelled before the tool started."),
        }
    }
}

impl std::error::Error for JobError {}

impl From<EndpointError> for JobError {
    fn from(error: EndpointError) -> Self {
        Self::Endpoint(error)
    }
}

impl From<PassfileError> for JobError {
    fn from(error: PassfileError) -> Self {
        Self::Passfile(error)
    }
}

impl From<ToolError> for JobError {
    fn from(error: ToolError) -> Self {
        Self::Tool(error)
    }
}

impl From<OptionsError> for JobError {
    fn from(error: OptionsError) -> Self {
        Self::Options(error)
    }
}

impl From<OutputError> for JobError {
    fn from(error: OutputError) -> Self {
        Self::Output(error)
    }
}

// ---- reaching the server ----------------------------------------------------------------------------------

/// What a job needs to reach the server, taken on the UI thread; the credential store and the tunnel are
/// touched later, on the worker.
pub struct ToolConnection {
    pub saved: SavedConnection,
    /// The open session's own URI (a password typed and not saved is only there), else the saved one is read.
    pub live_uri: Option<RuntimeUri>,
    /// The tunnel settings of the open connection, when it has a tunnel.
    pub tunnel: Option<TunnelAccess>,
}

/// An endpoint the tool can connect to, and the tunnel it goes through, which lives as long as this does.
pub struct Connected {
    pub endpoint: Endpoint,
    pub via: Option<String>,
    _tunnel: Option<SessionTunnel>,
}

impl ToolConnection {
    /// Blocking: reads the credentials, opens the connection's own tunnel (a second one, like any other session's)
    /// and points the endpoint at its local port.
    pub fn connect(self) -> Result<Connected, JobError> {
        let kind = self.saved.kind;
        let uri = match self.live_uri {
            Some(uri) => uri,
            None => crate::sessions::engine_uri(&self.saved).map_err(JobError::Uri)?,
        };
        match self.tunnel {
            Some(access) => {
                let (parts, tunnel) = uri.tool_endpoint_through(kind, &access).map_err(|error| match error {
                    ToolTunnelError::Tunnel(failure) => JobError::Tunnel(failure.message()),
                    ToolTunnelError::Endpoint(error) => JobError::Endpoint(error),
                })?;
                let endpoint = Endpoint::from_tool(kind, parts)?;
                if matches!(kind, ConnectionKind::Mysql | ConnectionKind::Mariadb) && endpoint.tls.mode == TlsMode::VerifyFull {
                    return Err(JobError::TlsNameThroughTunnel);
                }
                Ok(Connected { endpoint, via: Some(tunnel.via()), _tunnel: Some(tunnel) })
            }
            None if self.saved.ssh_tunnel => Err(JobError::TunnelNotOpen),
            None => Ok(Connected { endpoint: Endpoint::from_tool(kind, uri.tool_endpoint(kind)?)?, via: None, _tunnel: None }),
        }
    }
}

/// The tool programs that can back up a `kind` server, best first.
pub fn dump_tools(kind: ConnectionKind) -> &'static [Tool] {
    match kind {
        ConnectionKind::Mariadb => &[Tool::MariadbDump, Tool::MysqlDump],
        ConnectionKind::Mysql => &[Tool::MysqlDump],
        _ => &[Tool::PgDump],
    }
}

/// The tool a backup (`restore: false`) or a PostgreSQL restore would run, or why there is none. Blocking (it runs
/// `--version` of each candidate).
pub fn find_tool(kind: ConnectionKind, restore: bool, paths: &ToolPaths, locator: &Locator, server_major: Option<u32>) -> Result<Found, ToolError> {
    let tools: &[Tool] = if restore { &[Tool::PgRestore] } else { dump_tools(kind) };
    let min_major = (matches!(kind, ConnectionKind::Postgres)).then_some(server_major).flatten();
    locator.resolve(Need { tools, min_major }, paths)
}

// ---- the output ------------------------------------------------------------------------------------------

/// Why the place for a backup cannot be used.
#[derive(Debug)]
pub enum OutputError {
    NoFolder(PathBuf),
    NotEmpty(PathBuf),
    Io { path: PathBuf, source: std::io::Error },
    /// The older backup was moved aside to make room, and could not be put back: it is at `aside`.
    PreviousMoved { path: PathBuf, aside: PathBuf, source: std::io::Error },
}

impl std::fmt::Display for OutputError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::NoFolder(path) => write!(f, "The folder {} does not exist.", path.display()),
            Self::NotEmpty(path) => write!(f, "{} is a folder with files in it: pg_dump writes a directory backup into an empty or new folder.", path.display()),
            Self::Io { path, source } => write!(f, "{}: {source}", path.display()),
            Self::PreviousMoved { path, aside, source } => write!(f, "{}: {source}. The older backup is at {}: move it back by hand.", path.display(), aside.display()),
        }
    }
}

impl std::error::Error for OutputError {}

/// Where a backup goes: written at `partial`, moved to `target` when complete.
#[derive(Debug)]
pub struct OutputPlan {
    target: PathBuf,
    partial: PathBuf,
    directory: bool,
}

impl OutputPlan {
    pub fn new(target: &Path, directory: bool) -> Result<Self, OutputError> {
        let parent = target.parent().filter(|parent| !parent.as_os_str().is_empty()).unwrap_or_else(|| Path::new("."));
        if !parent.is_dir() {
            return Err(OutputError::NoFolder(parent.to_owned()));
        }
        if directory && target.is_dir() && std::fs::read_dir(target).map_err(|source| OutputError::Io { path: target.to_owned(), source })?.next().is_some() {
            return Err(OutputError::NotEmpty(target.to_owned()));
        }
        // A name of this run's own: another run for the same target (or one that crashed) has its own and is never touched.
        let mut name = target.file_name().unwrap_or_default().to_owned();
        name.push(format!(".{}{PARTIAL_SUFFIX}", &uuid::Uuid::new_v4().to_simple().to_string()[..8]));
        Ok(Self { target: target.to_owned(), partial: parent.join(name), directory })
    }

    pub fn partial(&self) -> &Path {
        &self.partial
    }

    pub fn target(&self) -> &Path {
        &self.target
    }

    /// Puts the finished backup where the user asked.
    ///
    /// A file is renamed over the old one in one step (atomic on Unix, and replacing on Windows): there is no moment without a
    /// backup. A directory cannot be renamed over a folder with files in it, and the old one is moved aside first, removed
    /// only once the new one is in place, and put back if that fails. On `Err` the finished backup is still at `partial`.
    pub fn commit(&self) -> Result<(), OutputError> {
        let io = |path: &Path, source| OutputError::Io { path: path.to_owned(), source };
        if !self.directory || !self.target.exists() {
            return std::fs::rename(&self.partial, &self.target).map_err(|source| io(&self.target, source));
        }
        let mut aside_name = self.target.file_name().unwrap_or_default().to_owned();
        aside_name.push(format!(".{}.tablex-old", &uuid::Uuid::new_v4().to_simple().to_string()[..8]));
        let aside = self.target.with_file_name(aside_name);
        std::fs::rename(&self.target, &aside).map_err(|source| io(&self.target, source))?;
        if let Err(source) = std::fs::rename(&self.partial, &self.target) {
            // Put the old one back; when that fails too, say where it is.
            return Err(match std::fs::rename(&aside, &self.target) {
                Ok(()) => io(&self.target, source),
                Err(_) => OutputError::PreviousMoved { path: self.target.clone(), aside, source },
            });
        }
        // The new backup is in place: the old one (a folder, or a file where the new one is a folder) is only a leftover now.
        let _ = if aside.is_dir() { std::fs::remove_dir_all(&aside) } else { std::fs::remove_file(&aside) };
        Ok(())
    }

    /// What to tell the user when the finished backup could not be put in place.
    pub fn stranded(&self) -> String {
        format!("The finished backup is at {}: move it to {} by hand.", self.partial.display(), self.target.display())
    }

    /// Takes the partial backup away. `Some` with the reason when it could not be removed.
    pub fn discard(self) -> Option<String> {
        let removed = if self.directory || self.partial.is_dir() { std::fs::remove_dir_all(&self.partial) } else { std::fs::remove_file(&self.partial) };
        match removed {
            Err(error) if error.kind() != std::io::ErrorKind::NotFound => Some(format!("The unfinished file {} could not be removed ({error}): delete it by hand.", self.partial.display())),
            _ => None,
        }
    }
}

// ---- the report ------------------------------------------------------------------------------------------

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Operation {
    Backup,
    Restore,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Outcome {
    Completed,
    Failed(String),
    Cancelled,
}

/// What a finished job tells.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Report {
    pub operation: Operation,
    pub outcome: Outcome,
    /// `pg_dump 17.4 (/opt/...)`.
    pub tool: Option<String>,
    /// The command as started: it holds no secret.
    pub command: Option<String>,
    /// The backup file or directory, once complete.
    pub output: Option<PathBuf>,
    /// The size of the output.
    pub bytes: u64,
    pub elapsed: Duration,
    /// The last lines the tool printed on its error stream.
    pub errors: Vec<String>,
    /// A partial file that could not be removed.
    pub leftover: Option<String>,
    /// The SSH route the tool went through.
    pub via: Option<String>,
}

impl Report {
    fn new(operation: Operation) -> Self {
        Self { operation, outcome: Outcome::Cancelled, tool: None, command: None, output: None, bytes: 0, elapsed: Duration::ZERO, errors: Vec::new(), leftover: None, via: None }
    }

    fn failed(operation: Operation, error: &JobError, started: Instant) -> Self {
        Self { outcome: Outcome::Failed(error.to_string()), elapsed: started.elapsed(), ..Self::new(operation) }
    }

    /// The job stopped before its tool started: cancelled, or failed with `error`.
    fn stopped_before_the_tool(operation: Operation, error: &JobError, started: Instant) -> Self {
        match error {
            JobError::Cancelled => Self { elapsed: started.elapsed(), ..Self::new(operation) },
            error => Self::failed(operation, error, started),
        }
    }

    /// The job panicked: nothing is known of what it did.
    pub fn panicked(operation: Operation, started: Instant) -> Self {
        let what = match operation {
            Operation::Backup => "The backup stopped unexpectedly. The unfinished file may still be beside the chosen name (it ends in .tablex-part).",
            Operation::Restore => "The restore stopped unexpectedly. Part of the backup may have been restored: check the database before restoring again.",
        };
        Self { outcome: Outcome::Failed(what.to_owned()), elapsed: started.elapsed(), ..Self::new(operation) }
    }
}

/// Why a tool that ended by itself failed, in a line.
fn failure_text(tool: &Found, code: Option<i32>, errors: &[String]) -> String {
    let how = match code {
        Some(code) => format!("{} failed (exit code {code})", tool.program()),
        None => format!("{} was stopped by a signal", tool.program()),
    };
    // The line that names the error, not the verbose line printed after it.
    let named = errors.iter().rev().find(|line| ["error", "ERROR", "FATAL", "failed", "denied", "Got error"].iter().any(|word| line.contains(word)));
    match named.or(errors.last()) {
        Some(line) => format!("{how}: {line}"),
        None => how,
    }
}

// ---- running ---------------------------------------------------------------------------------------------

/// What a backup runs.
pub enum DumpOptions {
    Postgres(PgDumpOptions),
    Mysql(MysqlDumpOptions),
}

pub struct BackupJob {
    pub connection: ToolConnection,
    /// The database dumped (the connection's own when empty).
    pub database: String,
    pub options: DumpOptions,
    /// The file (a directory for the directory format) the backup is moved to when complete.
    pub output: PathBuf,
    pub paths: ToolPaths,
    /// PostgreSQL: the server's major version, the oldest `pg_dump` allowed.
    pub server_major: Option<u32>,
    /// Where the run's private directory is registered (the application removes them all when it quits).
    pub dirs: PrivateDirs,
}

/// Everything a started tool needs kept alive until it ends.
struct Prepared {
    tool: Found,
    invocation: Invocation,
    connected: Connected,
    _private: PrivateDir,
}

/// A backup, start to end. Blocking: call it from a worker thread. `notify` is called when the counters changed.
pub fn run_backup(job: BackupJob, locator: &Locator, state: &Arc<RunState>, cancel: &Arc<CancelToken>, notify: &dyn Fn()) -> Report {
    let started = Instant::now();
    let kind = job.connection.saved.kind;
    let directory = matches!(&job.options, DumpOptions::Postgres(options) if options.format == PgFormat::Directory);
    let plan = match OutputPlan::new(&job.output, directory) {
        Ok(plan) => plan,
        Err(error) => return Report::failed(Operation::Backup, &error.into(), started),
    };
    let prepared = (|| -> Result<Prepared, JobError> {
        let BackupJob { connection, database, options, paths, server_major, dirs, .. } = job;
        let tool = find_tool(kind, false, &paths, locator, server_major)?;
        let mut connected = connection.connect()?;
        if !database.is_empty() {
            connected.endpoint.database = database;
        }
        state.scrub_secrets(connected.endpoint.password_text().map(str::to_owned).into_iter().collect());
        // Cancelled while connecting (a closed tab, a quit): no password file is made.
        if cancel.is_cancelled() {
            return Err(JobError::Cancelled);
        }
        let private = dirs.create().map_err(JobError::Private)?;
        let invocation = match &options {
            DumpOptions::Postgres(options) => {
                let passfile = secrets::pgpass(&connected.endpoint)?.map(|content| private.write_file("pgpass", &content)).transpose().map_err(JobError::Private)?;
                command::pg_dump(&tool, &connected.endpoint, &connected.endpoint.database, passfile.as_deref(), options, plan.partial())?
            }
            DumpOptions::Mysql(options) => {
                let defaults = private.write_file("client.cnf", &secrets::mysql_defaults(&connected.endpoint, tool.version.family)).map_err(JobError::Private)?;
                command::mysql_dump(&tool, kind, server_major, &connected.endpoint, &defaults, options, plan.partial())?
            }
        };
        Ok(Prepared { tool, invocation, connected, _private: private })
    })();
    let prepared = match prepared {
        Ok(prepared) => prepared,
        Err(error) => {
            let leftover = plan.discard();
            return Report { leftover, ..Report::stopped_before_the_tool(Operation::Backup, &error, started) };
        }
    };
    state.push(Stream::Command, &format!("$ {}", prepared.invocation.command_line()));
    let finished = process::run(&Execution { invocation: &prepared.invocation, watch: Some(plan.partial()) }, state, cancel, notify);
    let mut report = Report::new(Operation::Backup);
    report.tool = Some(prepared.tool.describe());
    report.command = Some(prepared.invocation.command_line());
    report.via = prepared.connected.via.clone();
    report.errors = state.stderr_tail();
    report.bytes = process::size_of(plan.partial());
    match finished {
        Ok(finished) => {
            report.elapsed = finished.elapsed;
            match finished.ending {
                // A cancel always wins: nothing a cancelled run made is committed, whatever the tool's exit said.
                Ending::Exited { success: true, .. } if !cancel.is_cancelled() => {
                    let target = plan.target().to_owned();
                    match plan.commit() {
                        Ok(()) => {
                            report.outcome = Outcome::Completed;
                            report.bytes = process::size_of(&target);
                            report.output = Some(target);
                        }
                        Err(error) => {
                            report.outcome = Outcome::Failed(format!("The backup was made but could not be put in place: {error}"));
                            report.leftover = Some(plan.stranded());
                        }
                    }
                }
                Ending::Exited { success: false, code } => {
                    report.outcome = Outcome::Failed(failure_text(&prepared.tool, code, &report.errors));
                    report.leftover = plan.discard();
                }
                Ending::Exited { success: true, .. } | Ending::Cancelled => {
                    report.outcome = Outcome::Cancelled;
                    report.leftover = plan.discard();
                }
            }
        }
        Err(error) => {
            report.outcome = Outcome::Failed(JobError::Start(error).to_string());
            report.leftover = plan.discard();
        }
    }
    report
}

pub struct RestoreJob {
    pub connection: ToolConnection,
    pub database: String,
    pub archive: PathBuf,
    pub options: PgRestoreOptions,
    /// The table of contents and the entries wanted, when not all of them are.
    pub selection: Option<(Toc, HashSet<u32>)>,
    pub paths: ToolPaths,
    pub server_major: Option<u32>,
    pub dirs: PrivateDirs,
}

/// A restore of an archive with `pg_restore`, start to end. Blocking: call it from a worker thread.
pub fn run_restore(job: RestoreJob, locator: &Locator, state: &Arc<RunState>, cancel: &Arc<CancelToken>, notify: &dyn Fn()) -> Report {
    let started = Instant::now();
    let kind = job.connection.saved.kind;
    // Before anything else is touched: `pg_restore` reads PostgreSQL archives and connects to PostgreSQL only.
    if kind != ConnectionKind::Postgres {
        return Report::failed(Operation::Restore, &JobError::NotPostgres, started);
    }
    let prepared = (|| -> Result<Prepared, JobError> {
        let RestoreJob { connection, database, archive, mut options, selection, paths, server_major, dirs } = job;
        let tool = find_tool(kind, true, &paths, locator, server_major)?;
        let mut connected = connection.connect()?;
        if !database.is_empty() {
            connected.endpoint.database = database;
        }
        state.scrub_secrets(connected.endpoint.password_text().map(str::to_owned).into_iter().collect());
        // Cancelled while connecting (a closed tab, a quit): no password file is made.
        if cancel.is_cancelled() {
            return Err(JobError::Cancelled);
        }
        let private = dirs.create().map_err(JobError::Private)?;
        let passfile = secrets::pgpass(&connected.endpoint)?.map(|content| private.write_file("pgpass", &content)).transpose().map_err(JobError::Private)?;
        if let Some((toc, wanted)) = selection {
            let list = private.write_file("restore.list", &engine_net::Secret::new(toc.use_list(&wanted))).map_err(JobError::Private)?;
            options.list_file = Some(list);
        }
        let invocation = command::pg_restore(&tool, &connected.endpoint, &connected.endpoint.database, passfile.as_deref(), &options, &archive)?;
        Ok(Prepared { tool, invocation, connected, _private: private })
    })();
    let prepared = match prepared {
        Ok(prepared) => prepared,
        Err(error) => return Report::stopped_before_the_tool(Operation::Restore, &error, started),
    };
    state.push(Stream::Command, &format!("$ {}", prepared.invocation.command_line()));
    let finished = process::run(&Execution { invocation: &prepared.invocation, watch: None }, state, cancel, notify);
    let mut report = Report::new(Operation::Restore);
    report.tool = Some(prepared.tool.describe());
    report.command = Some(prepared.invocation.command_line());
    report.via = prepared.connected.via.clone();
    report.errors = state.stderr_tail();
    match finished {
        Ok(finished) => {
            report.elapsed = finished.elapsed;
            report.outcome = match finished.ending {
                Ending::Exited { success: true, .. } => Outcome::Completed,
                Ending::Exited { code, .. } => Outcome::Failed(failure_text(&prepared.tool, code, &report.errors)),
                Ending::Cancelled => Outcome::Cancelled,
            };
        }
        Err(error) => report.outcome = Outcome::Failed(JobError::Start(error).to_string()),
    }
    report
}

/// The table of contents of an archive, from `pg_restore --list`. Blocking; ended by `cancel` or by [`process::CAPTURE_TIMEOUT`].
pub fn list_toc(archive: &Path, paths: &ToolPaths, locator: &Locator, server_major: Option<u32>, cancel: &Arc<CancelToken>) -> Result<Toc, JobError> {
    let tool = find_tool(ConnectionKind::Postgres, true, paths, locator, server_major)?;
    let output = process::capture(&command::pg_restore_list(&tool, archive), cancel, process::CAPTURE_TIMEOUT).map_err(|error| JobError::Archive(error.to_string()))?;
    if !output.status.success() {
        let text = String::from_utf8_lossy(&output.stderr);
        let reason = text.lines().rev().find(|line| !line.trim().is_empty()).unwrap_or("it could not read the archive");
        return Err(JobError::Archive(format!("{}: {reason}", tool.program())));
    }
    Ok(Toc::parse(&String::from_utf8_lossy(&output.stdout)))
}

#[cfg(all(test, unix))]
mod tests {
    use std::os::unix::fs::PermissionsExt as _;
    use std::sync::atomic::{AtomicBool, Ordering};

    use super::*;
    use crate::backup::tools::SearchRoot;
    use crate::ssh_tunnel::fake::FakeOpener;
    use crate::ssh_tunnel::{SshAuthMethod, SshSecrets, SshSettings};

    const PASSWORD: &str = "hunter2-pass";

    fn saved(kind: ConnectionKind, ssh_tunnel: bool) -> SavedConnection {
        SavedConnection { id: uuid::Uuid::new_v4(), name: "test".into(), uri: String::new(), kind, save_password: false, is_pinned: false, disabled: false, ssh_tunnel }
    }

    fn connection(kind: ConnectionKind, uri: &str) -> ToolConnection {
        ToolConnection { saved: saved(kind, false), live_uri: Some(RuntimeUri::new(uri)), tunnel: None }
    }

    fn pg_connection() -> ToolConnection {
        connection(ConnectionKind::Postgres, &format!("postgres://ann:{PASSWORD}@127.0.0.1:5999/sales?sslmode=require&sslrootcert=/ca.pem"))
    }

    fn write_script(path: &Path, body: &str) {
        std::fs::write(path, format!("#!/bin/sh\n{body}\n")).unwrap();
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o755)).unwrap();
    }

    /// A folder with a fake `name` whose run writes what it saw to `record` and its output to `--file=` / `--result-file=`.
    struct FakeTools {
        dir: tempfile::TempDir,
    }

    impl FakeTools {
        fn new() -> Self {
            Self { dir: tempfile::tempdir().unwrap() }
        }

        fn record(&self) -> String {
            std::fs::read_to_string(self.dir.path().join("record")).unwrap_or_default()
        }

        fn locator(&self) -> Locator {
            Locator::with_roots(vec![SearchRoot::dir(self.dir.path().join("bin"))])
        }

        fn install(&self, name: &str, version_line: &str, run: &str) {
            let bin = self.dir.path().join("bin");
            std::fs::create_dir_all(&bin).unwrap();
            let record = self.dir.path().join("record");
            let mode = "$(stat -c %a \"$1\" 2>/dev/null || stat -f %Lp \"$1\")";
            write_script(
                &bin.join(name),
                &format!(
                    "case \"$1\" in --version) echo '{version_line}'; exit 0;; esac\n\
                     out=''; for a in \"$@\"; do case \"$a\" in --file=*) out=\"${{a#--file=}}\";; --result-file=*) out=\"${{a#--result-file=}}\";; esac; done\n\
                     mode() {{ set -- \"$1\"; echo \"{mode}\"; }}\n\
                     {{ echo \"ARGS=$*\"; echo \"PGPASSFILE=$PGPASSFILE\"; echo \"PGSSLMODE=$PGSSLMODE PGSSLROOTCERT=$PGSSLROOTCERT PGHOSTADDR=$PGHOSTADDR\"; echo \"LOGIN=$MYSQL_TEST_LOGIN_FILE\"; \
                        if [ -n \"$PGPASSFILE\" ]; then echo \"PGPASS_CONTENT=$(cat \"$PGPASSFILE\")\"; echo \"PGPASS_MODE=$(mode \"$PGPASSFILE\")\"; echo \"DIR_MODE=$(mode \"$(dirname \"$PGPASSFILE\")\")\"; fi; \
                        case \"$1\" in --defaults-file=*) f=\"${{1#--defaults-file=}}\"; echo \"DEFAULTS_CONTENT=$(cat \"$f\")\"; echo \"DEFAULTS_MODE=$(mode \"$f\")\";; esac; }} > '{record}'\n\
                     {run}",
                    record = record.display()
                ),
            );
        }
    }

    /// The unfinished file a run left beside its output, if any.
fn partial_in(dir: &Path) -> Option<PathBuf> {
    std::fs::read_dir(dir).ok()?.flatten().map(|entry| entry.path()).find(|path| path.to_string_lossy().ends_with(PARTIAL_SUFFIX))
}

fn state_and_cancel() -> (Arc<RunState>, Arc<CancelToken>) {
        (RunState::new(), Arc::new(CancelToken::default()))
    }

    fn pg_job(connection: ToolConnection, output: PathBuf, tools: &FakeTools) -> BackupJob {
        let paths = ToolPaths { pg_dump: tools.dir.path().join("bin/pg_dump").display().to_string(), ..ToolPaths::default() };
        BackupJob { connection, database: "sales".into(), options: DumpOptions::Postgres(PgDumpOptions { format: PgFormat::Custom, ..PgDumpOptions::default() }), output, paths, server_major: Some(17), dirs: PrivateDirs::default() }
    }

    const WRITE_OUTPUT: &str = "echo 'pg_dump: dumping contents of table \"public.items\"' >&2\nprintf 'dump' > \"$out\"\nexit 0";

    #[test]
    fn a_backup_runs_the_tool_with_the_password_only_in_a_private_file_and_leaves_no_trace() {
        let tools = FakeTools::new();
        tools.install("pg_dump", "pg_dump (PostgreSQL) 17.4", WRITE_OUTPUT);
        let out = tempfile::tempdir().unwrap();
        let output = out.path().join("sales.dump");
        let (state, cancel) = state_and_cancel();
        let report = run_backup(pg_job(pg_connection(), output.clone(), &tools), &tools.locator(), &state, &cancel, &|| {});

        assert_eq!(report.outcome, Outcome::Completed, "{report:?}");
        assert_eq!(std::fs::read_to_string(&output).unwrap(), "dump");
        assert_eq!(report.output.as_deref(), Some(output.as_path()));
        assert_eq!(report.bytes, 4);
        assert!(report.tool.as_deref().is_some_and(|tool| tool.starts_with("pg_dump 17.4")));
        assert_eq!(std::fs::read_dir(out.path()).unwrap().count(), 1, "no .tablex-part left beside the backup");

        let record = tools.record();
        assert!(record.contains("--host=127.0.0.1 --port=5999 --username=ann --no-password --dbname=dbname='sales' --format=c"), "{record}");
        assert!(!record.lines().next().unwrap().contains(PASSWORD), "no password in argv: {record}");
        assert!(record.contains(&format!("PGPASS_CONTENT=127.0.0.1:5999:*:ann:{PASSWORD}")), "{record}");
        assert!(record.contains("PGPASS_MODE=600") && record.contains("DIR_MODE=700"), "{record}");
        assert!(record.contains("PGSSLMODE=require PGSSLROOTCERT=/ca.pem PGHOSTADDR=\n"), "{record}");
        let passfile = record.lines().find_map(|line| line.strip_prefix("PGPASSFILE=")).unwrap();
        assert!(!Path::new(passfile).exists(), "the passfile is deleted after the run");
        assert!(!Path::new(passfile).parent().unwrap().exists(), "and its folder");

        let log: Vec<String> = state.range(0..20).into_iter().map(|line| line.text).collect();
        assert!(log[0].starts_with("$ pg_dump ") && !log.join("\n").contains(PASSWORD), "{log:?}");
        assert!(log.iter().any(|line| line.contains("dumping contents of table")));
    }

    #[test]
    fn a_failed_backup_removes_its_partial_file_and_keeps_the_older_backup() {
        let tools = FakeTools::new();
        tools.install("pg_dump", "pg_dump (PostgreSQL) 17.4", "printf 'half' > \"$out\"\necho \"pg_dump: error: connection to server failed: password authentication failed for user ann\" >&2\nexit 1");
        let out = tempfile::tempdir().unwrap();
        let output = out.path().join("sales.dump");
        std::fs::write(&output, "older good backup").unwrap();
        let (state, cancel) = state_and_cancel();
        let report = run_backup(pg_job(pg_connection(), output.clone(), &tools), &tools.locator(), &state, &cancel, &|| {});
        let Outcome::Failed(message) = &report.outcome else { panic!("{report:?}") };
        assert!(message.contains("exit code 1") && message.contains("password authentication failed"), "{message}");
        assert_eq!(std::fs::read_to_string(&output).unwrap(), "older good backup");
        assert_eq!(std::fs::read_dir(out.path()).unwrap().count(), 1, "the half file is gone");
        assert!(report.leftover.is_none());
    }

    #[test]
    fn a_cancelled_backup_stops_the_tool_and_removes_the_partial_file() {
        let tools = FakeTools::new();
        tools.install("pg_dump", "pg_dump (PostgreSQL) 17.4", "printf 'half' > \"$out\"\necho started >&2\nsleep 60");
        let out = tempfile::tempdir().unwrap();
        let output = out.path().join("sales.dump");
        let (state, cancel) = state_and_cancel();
        let worker = {
            let (job, locator, state, cancel) = (pg_job(pg_connection(), output.clone(), &tools), tools.locator(), state.clone(), cancel.clone());
            std::thread::spawn(move || run_backup(job, &locator, &state, &cancel, &|| {}))
        };
        let deadline = Instant::now() + Duration::from_secs(10);
        while !(partial_in(out.path()).is_some() && state.snapshot().lines_seen >= 2) {
            assert!(Instant::now() < deadline, "the tool started and wrote");
            std::thread::sleep(Duration::from_millis(10));
        }
        cancel.cancel();
        let report = worker.join().unwrap();
        assert_eq!(report.outcome, Outcome::Cancelled);
        assert!(partial_in(out.path()).is_none() && !output.exists(), "nothing of the backup is left");
        assert!(report.elapsed < Duration::from_secs(10));
    }

    #[test]
    fn a_backup_cancelled_while_connecting_makes_no_password_file_and_never_starts_the_tool() {
        let tools = FakeTools::new();
        tools.install("pg_dump", "pg_dump (PostgreSQL) 17.4", WRITE_OUTPUT);
        let out = tempfile::tempdir().unwrap();
        let output = out.path().join("sales.dump");
        let (state, cancel) = state_and_cancel();
        cancel.cancel();
        let job = pg_job(pg_connection(), output.clone(), &tools);
        let dirs = job.dirs.clone();
        let report = run_backup(job, &tools.locator(), &state, &cancel, &|| {});
        assert_eq!(report.outcome, Outcome::Cancelled, "{report:?}");
        assert_eq!(dirs.len(), 0, "no private folder is left");
        // It stopped before the command was made, so before the password file it needs was written.
        assert!(report.command.is_none() && report.tool.is_none(), "{report:?}");
        assert!(!tools.record().contains("ARGS="), "the tool never ran");
        assert!(partial_in(out.path()).is_none() && !output.exists());
    }

    #[test]
    fn a_tool_that_ignores_the_cancel_and_ends_well_is_a_cancelled_backup_and_nothing_is_committed() {
        let tools = FakeTools::new();
        // The request ends its sleep, not the script: it goes on, has written everything, and exits 0.
        tools.install("pg_dump", "pg_dump (PostgreSQL) 17.4", "trap '' TERM\nprintf 'a complete dump' > \"$out\"\necho started >&2\nsleep 3\nexit 0");
        let out = tempfile::tempdir().unwrap();
        let output = out.path().join("sales.dump");
        std::fs::write(&output, "older backup").unwrap();
        let (state, cancel) = state_and_cancel();
        let worker = {
            let (job, locator, state, cancel) = (pg_job(pg_connection(), output.clone(), &tools), tools.locator(), state.clone(), cancel.clone());
            std::thread::spawn(move || run_backup(job, &locator, &state, &cancel, &|| {}))
        };
        let deadline = Instant::now() + Duration::from_secs(10);
        while state.snapshot().lines_seen < 2 {
            assert!(Instant::now() < deadline, "the tool started");
            std::thread::sleep(Duration::from_millis(10));
        }
        cancel.cancel();
        let report = worker.join().unwrap();
        assert_eq!(report.outcome, Outcome::Cancelled, "{report:?}");
        assert_eq!(std::fs::read_to_string(&output).unwrap(), "older backup", "the cancelled run did not replace the older backup");
        assert!(partial_in(out.path()).is_none() && report.output.is_none(), "the partial file is removed");
    }

    #[test]
    fn a_pg_dump_older_than_the_server_is_refused_before_anything_is_written() {
        let tools = FakeTools::new();
        tools.install("pg_dump", "pg_dump (PostgreSQL) 15.6", WRITE_OUTPUT);
        let out = tempfile::tempdir().unwrap();
        let output = out.path().join("sales.dump");
        let (state, cancel) = state_and_cancel();
        let report = run_backup(pg_job(pg_connection(), output.clone(), &tools), &tools.locator(), &state, &cancel, &|| {});
        let Outcome::Failed(message) = &report.outcome else { panic!("{report:?}") };
        assert!(message.contains("15.6") && message.contains("PostgreSQL 17"), "{message}");
        assert_eq!(tools.record(), "", "the tool never ran");
        assert_eq!(std::fs::read_dir(out.path()).unwrap().count(), 0);
    }

    #[test]
    fn a_mysql_backup_has_the_option_file_first_and_the_password_only_in_it() {
        let tools = FakeTools::new();
        tools.install("mysqldump", "mysqldump  Ver 8.0.36 for Linux on x86_64", "echo '-- Retrieving table structure for table items...' >&2\nprintf -- '-- dump' > \"$out\"\nexit 0");
        let out = tempfile::tempdir().unwrap();
        let output = out.path().join("shop.sql");
        let connection = connection(ConnectionKind::Mysql, &format!("mysql://root:{PASSWORD}@127.0.0.1:3306/shop?ssl-mode=REQUIRED"));
        let paths = ToolPaths { mysqldump: tools.dir.path().join("bin/mysqldump").display().to_string(), ..ToolPaths::default() };
        let job = BackupJob { connection, database: String::new(), options: DumpOptions::Mysql(MysqlDumpOptions::default()), output: output.clone(), paths, server_major: None, dirs: PrivateDirs::default() };
        let (state, cancel) = state_and_cancel();
        let report = run_backup(job, &tools.locator(), &state, &cancel, &|| {});
        assert_eq!(report.outcome, Outcome::Completed, "{report:?}");
        assert_eq!(std::fs::read_to_string(&output).unwrap(), "-- dump");
        let record = tools.record();
        let args = record.lines().next().unwrap();
        assert!(args.starts_with("ARGS=--defaults-file="), "the option file is read alone: {args}");
        let login = record.lines().find_map(|line| line.strip_prefix("LOGIN=")).unwrap();
        assert!(login.ends_with("no-login.cnf") && !Path::new(login).exists(), "~/.mylogin.cnf is pointed at a file that does not exist: {login}");
        assert!(!args.contains(PASSWORD) && args.contains("--single-transaction") && args.contains("--databases -- shop"), "{args}");
        assert!(record.contains("DEFAULTS_MODE=600"), "{record}");
        for line in ["[client]", "host=\"127.0.0.1\"", "protocol=tcp", "port=3306", "user=\"root\"", &format!("password=\"{PASSWORD}\""), "ssl-mode=REQUIRED"] {
            assert!(record.contains(line), "{line} in {record}");
        }
        let defaults = args.split(' ').next().unwrap().trim_start_matches("ARGS=--defaults-file=");
        assert!(!Path::new(defaults).exists(), "the option file is deleted after the run");
    }

    #[test]
    fn a_backup_of_a_directory_is_written_beside_and_moved_into_place_when_complete() {
        let tools = FakeTools::new();
        tools.install("pg_dump", "pg_dump (PostgreSQL) 17.4", "mkdir \"$out\"\nprintf 'toc' > \"$out/toc.dat\"\nprintf 'data' > \"$out/1.dat.gz\"\nexit 0");
        let out = tempfile::tempdir().unwrap();
        let output = out.path().join("sales-dir");
        let mut job = pg_job(pg_connection(), output.clone(), &tools);
        job.options = DumpOptions::Postgres(PgDumpOptions { format: PgFormat::Directory, jobs: Some(4), ..PgDumpOptions::default() });
        let (state, cancel) = state_and_cancel();
        let report = run_backup(job, &tools.locator(), &state, &cancel, &|| {});
        assert_eq!(report.outcome, Outcome::Completed, "{report:?}");
        assert!(output.join("toc.dat").is_file() && partial_in(out.path()).is_none());
        assert_eq!(report.bytes, 7);
        // A folder with files in it is not written into.
        let (state, cancel) = state_and_cancel();
        let mut again = pg_job(pg_connection(), output.clone(), &tools);
        again.options = DumpOptions::Postgres(PgDumpOptions { format: PgFormat::Directory, ..PgDumpOptions::default() });
        let report = run_backup(again, &tools.locator(), &state, &cancel, &|| {});
        assert!(matches!(&report.outcome, Outcome::Failed(message) if message.contains("folder with files in it")), "{report:?}");
        assert!(output.join("toc.dat").is_file(), "the first backup is untouched");
    }

    #[test]
    fn a_restore_runs_pg_restore_with_the_selection_list_and_removes_the_files_after() {
        let tools = FakeTools::new();
        tools.install("pg_restore", "pg_restore (PostgreSQL) 17.4", "list=''; for a in \"$@\"; do case \"$a\" in --use-list=*) list=\"${a#--use-list=}\"; cat \"$list\" >> \"$(dirname \"$0\")/../list\";; esac; done\necho 'pg_restore: processing data for table \"public.items\"' >&2\nexit 0");
        let archive = tools.dir.path().join("a.dump");
        std::fs::write(&archive, "PGDMP").unwrap();
        let toc = Toc::parse("; header\n218; 1259 16385 TABLE public items tablex\n3352; 0 16385 TABLE DATA public items tablex\n99; 1259 16399 TABLE public other tablex\n");
        let paths = ToolPaths { pg_restore: tools.dir.path().join("bin/pg_restore").display().to_string(), ..ToolPaths::default() };
        let job = RestoreJob {
            connection: pg_connection(),
            database: "target".into(),
            archive: archive.clone(),
            options: PgRestoreOptions { clean: true, no_owner: true, ..PgRestoreOptions::default() },
            selection: Some((toc, HashSet::from([218, 3352]))),
            paths,
            server_major: Some(17),
            dirs: PrivateDirs::default(),
        };
        let (state, cancel) = state_and_cancel();
        let report = run_restore(job, &tools.locator(), &state, &cancel, &|| {});
        assert_eq!(report.outcome, Outcome::Completed, "{report:?}");
        let record = tools.record();
        assert!(record.contains("--dbname=dbname='target' --clean --if-exists --no-owner --use-list="), "{record}");
        assert!(record.contains(&format!("-- {}", archive.display())), "{record}");
        let list = std::fs::read_to_string(tools.dir.path().join("list")).unwrap();
        assert!(list.contains("\n218; 1259") && list.contains(";99; 1259"), "{list}");
        let passfile = record.lines().find_map(|line| line.strip_prefix("PGPASSFILE=")).unwrap();
        assert!(!Path::new(passfile).parent().unwrap().exists());
    }

    #[test]
    fn the_listing_of_an_archive_is_read_and_a_bad_archive_says_why() {
        let tools = FakeTools::new();
        tools.install("pg_restore", "pg_restore (PostgreSQL) 17.4", "case \"$*\" in *bad*) echo 'pg_restore: error: unsupported version (1.16) in file header' >&2; exit 1;; esac\necho '218; 1259 16385 TABLE public items tablex'");
        let paths = ToolPaths { pg_restore: tools.dir.path().join("bin/pg_restore").display().to_string(), ..ToolPaths::default() };
        let toc = list_toc(Path::new("/x/a.dump"), &paths, &tools.locator(), Some(17), &Arc::new(CancelToken::default())).unwrap();
        assert_eq!(toc.entries.len(), 1);
        let error = list_toc(Path::new("/x/bad.dump"), &paths, &tools.locator(), Some(17), &Arc::new(CancelToken::default())).unwrap_err();
        assert!(error.to_string().contains("unsupported version"), "{error}");
    }

    #[test]
    fn a_backup_replaces_an_older_file_in_one_step_and_an_older_folder_only_once_the_new_one_is_in() {
        let dir = tempfile::tempdir().unwrap();
        // A file: renamed over the old one, which is never missing in between.
        let target = dir.path().join("b.dump");
        std::fs::write(&target, "old").unwrap();
        let plan = OutputPlan::new(&target, false).unwrap();
        std::fs::write(plan.partial(), "new").unwrap();
        plan.commit().unwrap();
        assert_eq!(std::fs::read_to_string(&target).unwrap(), "new");
        assert!(partial_in(dir.path()).is_none());

        // A folder backup over an older backup that is a file: the file moved aside is removed as a file, and nothing is left.
        let was_file = dir.path().join("was-file");
        std::fs::write(&was_file, "older file backup").unwrap();
        let plan = OutputPlan::new(&was_file, true).unwrap();
        std::fs::create_dir(plan.partial()).unwrap();
        std::fs::write(plan.partial().join("toc.dat"), "new").unwrap();
        plan.commit().unwrap();
        assert!(was_file.join("toc.dat").is_file());
        assert!(std::fs::read_dir(dir.path()).unwrap().flatten().all(|entry| !entry.file_name().to_string_lossy().ends_with(".tablex-old")), "the older file is not left aside");
        std::fs::remove_dir_all(&was_file).unwrap();

        // A file whose replacement cannot be moved in: the older backup is still there, untouched.
        let kept = dir.path().join("kept.dump");
        std::fs::write(&kept, "older good backup").unwrap();
        let plan = OutputPlan { target: kept.clone(), partial: dir.path().join("gone.tablex-part"), directory: false };
        assert!(plan.commit().is_err());
        assert_eq!(std::fs::read_to_string(&kept).unwrap(), "older good backup", "the old file is not removed before the new one is in place");
        std::fs::remove_file(&kept).unwrap();

        // A folder: the new one goes in, the old one is gone after.
        let folder = dir.path().join("f");
        std::fs::create_dir(&folder).unwrap();
        let plan = OutputPlan::new(&folder, true).unwrap();
        std::fs::create_dir(plan.partial()).unwrap();
        std::fs::write(plan.partial().join("toc.dat"), "new").unwrap();
        plan.commit().unwrap();
        assert!(folder.join("toc.dat").is_file());
        assert_eq!(std::fs::read_dir(dir.path()).unwrap().count(), 2, "no folder moved aside is left: {:?}", std::fs::read_dir(dir.path()).unwrap().flatten().map(|e| e.file_name()).collect::<Vec<_>>());

        // A folder whose replacement cannot be moved in: the old one is put back, the finished one is told about.
        std::fs::remove_dir_all(&folder).unwrap();
        std::fs::create_dir(&folder).unwrap();
        std::fs::write(folder.join("keep.dat"), "old backup").unwrap();
        let plan = OutputPlan { target: folder.clone(), partial: dir.path().join("missing.tablex-part"), directory: true };
        assert!(plan.commit().is_err());
        assert_eq!(std::fs::read_to_string(folder.join("keep.dat")).unwrap(), "old backup", "the older backup is back where it was");
        assert!(plan.stranded().contains("missing.tablex-part") && plan.stranded().contains("by hand"));
        assert_eq!(std::fs::read_dir(dir.path()).unwrap().flatten().filter(|e| e.file_name().to_string_lossy().ends_with(".tablex-old")).count(), 0);
    }

    #[test]
    fn two_runs_for_one_target_each_have_a_file_of_their_own_and_never_delete_the_other() {
        let dir = tempfile::tempdir().unwrap();
        let target = dir.path().join("same.dump");
        let (first, second) = (OutputPlan::new(&target, false).unwrap(), OutputPlan::new(&target, false).unwrap());
        assert_ne!(first.partial(), second.partial());
        std::fs::write(first.partial(), "first").unwrap();
        // A third run starting, and the second giving up, touch nothing of the first's.
        let _third = OutputPlan::new(&target, false).unwrap();
        assert!(second.discard().is_none());
        assert_eq!(std::fs::read_to_string(first.partial()).unwrap(), "first");
        // A leftover of a run that crashed is not this run's to delete either.
        let stale = dir.path().join("same.dump.deadbeef.tablex-part");
        std::fs::write(&stale, "crashed").unwrap();
        let _fourth = OutputPlan::new(&target, false).unwrap();
        assert!(stale.exists());
    }

    #[test]
    fn a_restore_of_a_mysql_connection_is_refused_before_the_tool_is_touched() {
        let tools = FakeTools::new();
        tools.install("pg_restore", "pg_restore (PostgreSQL) 17.4", "exit 0");
        let archive = tools.dir.path().join("a.dump");
        std::fs::write(&archive, "PGDMP").unwrap();
        let paths = ToolPaths { pg_restore: tools.dir.path().join("bin/pg_restore").display().to_string(), ..ToolPaths::default() };
        let job = RestoreJob { connection: connection(ConnectionKind::Mysql, "mysql://root:pw@127.0.0.1:3306/shop"), database: "shop".into(), archive, options: PgRestoreOptions::default(), selection: None, paths, server_major: Some(8), dirs: PrivateDirs::default() };
        let (state, cancel) = state_and_cancel();
        let report = run_restore(job, &tools.locator(), &state, &cancel, &|| {});
        assert!(matches!(&report.outcome, Outcome::Failed(message) if message.contains("PostgreSQL only")), "{report:?}");
        assert_eq!(tools.record(), "", "the tool never ran");
    }

    #[test]
    fn a_password_with_a_line_break_is_refused_before_the_tool_starts() {
        let tools = FakeTools::new();
        tools.install("pg_dump", "pg_dump (PostgreSQL) 17.4", WRITE_OUTPUT);
        let out = tempfile::tempdir().unwrap();
        let job = pg_job(connection(ConnectionKind::Postgres, "postgres://ann:pw%0Aevil@127.0.0.1:5999/sales"), out.path().join("a.dump"), &tools);
        let (state, cancel) = state_and_cancel();
        let report = run_backup(job, &tools.locator(), &state, &cancel, &|| {});
        assert!(matches!(&report.outcome, Outcome::Failed(message) if message.contains("line break") && !message.contains("evil")), "{report:?}");
        assert_eq!(tools.record(), "");
        assert!(partial_in(out.path()).is_none());
    }

    fn bastion(opener: Arc<FakeOpener>) -> TunnelAccess {
        let settings = SshSettings { enabled: true, host: "bastion.example.com".into(), port: "2222".into(), user: "deploy".into(), auth: SshAuthMethod::Agent, save_secrets: false, ..SshSettings::default() };
        TunnelAccess { opener, settings, secrets: SshSecrets::default(), known_hosts: Some(std::env::temp_dir().join("tablex-test-known-hosts")), prompts: Arc::new(engine_net::NoPrompts), asked: Arc::new(AtomicBool::new(false)), shared: None }
    }

    #[test]
    fn a_tunnelled_connection_is_dumped_through_its_own_local_port_for_as_long_as_the_tool_runs() {
        let tools = FakeTools::new();
        let opener = FakeOpener::new();
        let drops = opener.drops.clone();
        // The tool looks at the tunnel's counter through a file the test updates: it must still be open while the tool runs.
        tools.install("pg_dump", "pg_dump (PostgreSQL) 17.4", WRITE_OUTPUT);
        let out = tempfile::tempdir().unwrap();
        let connection = ToolConnection { saved: saved(ConnectionKind::Postgres, true), live_uri: Some(RuntimeUri::new(format!("postgres://ann:{PASSWORD}@db.internal.example.com:5432/sales"))), tunnel: Some(bastion(opener.clone())) };
        let (state, cancel) = state_and_cancel();
        let report = run_backup(pg_job(connection, out.path().join("t.dump"), &tools), &tools.locator(), &state, &cancel, &|| {});
        assert_eq!(report.outcome, Outcome::Completed, "{report:?}");
        let record = tools.record();
        assert!(record.contains("--host=db.internal.example.com --port=41001 "), "the tool connects to the tunnel's local port: {record}");
        assert!(record.contains("PGHOSTADDR=127.0.0.1"), "at the tunnel's local address, while the host stays the name TLS checks: {record}");
        assert!(record.contains("PGPASS_CONTENT=db.internal.example.com:41001:*:ann:"), "the password file matches that host and port: {record}");
        let opened = opener.opened.lock().unwrap();
        assert_eq!((opened.len(), opened[0].targets[0].host.as_str(), opened[0].targets[0].port), (1, "db.internal.example.com", 5432));
        assert_eq!(drops.load(Ordering::SeqCst), 1, "the tunnel is closed when the job ends");
        assert!(report.via.as_deref().is_some_and(|via| via.contains("bastion.example.com")));
    }

    #[test]
    fn mysql_name_verification_through_a_tunnel_is_refused_not_quietly_weakened() {
        let uri = "mysql://root@db.internal.example.com:3306/shop?ssl-mode=VERIFY_IDENTITY&ssl-ca=/ca.pem";
        let through = |uri: &str| ToolConnection { saved: saved(ConnectionKind::Mysql, true), live_uri: Some(RuntimeUri::new(uri)), tunnel: Some(bastion(FakeOpener::new())) }.connect().map(|connected| connected.endpoint);
        assert!(matches!(through(uri), Err(JobError::TlsNameThroughTunnel)));
        let endpoint = through(&uri.replace("VERIFY_IDENTITY", "VERIFY_CA")).ok().unwrap();
        assert_eq!((endpoint.host.as_str(), endpoint.port, endpoint.tls.mode), ("127.0.0.1", Some(41001), TlsMode::VerifyCa));
    }

    #[test]
    fn a_connection_that_needs_a_tunnel_never_connects_directly() {
        let tools = FakeTools::new();
        tools.install("pg_dump", "pg_dump (PostgreSQL) 17.4", WRITE_OUTPUT);
        let connection = ToolConnection { saved: saved(ConnectionKind::Postgres, true), live_uri: Some(RuntimeUri::new("postgres://ann@db.internal.example.com/sales")), tunnel: None };
        let out = tempfile::tempdir().unwrap();
        let (state, cancel) = state_and_cancel();
        let report = run_backup(pg_job(connection, out.path().join("t.dump"), &tools), &tools.locator(), &state, &cancel, &|| {});
        assert!(matches!(&report.outcome, Outcome::Failed(message) if message.contains("SSH tunnel")), "{report:?}");
        assert_eq!(tools.record(), "");
    }

    #[test]
    fn errors_never_carry_the_password() {
        let tools = FakeTools::new();
        tools.install("pg_dump", "pg_dump (PostgreSQL) 17.4", &format!("echo \"pg_dump: error: could not connect with password {PASSWORD} to postgres://ann:{PASSWORD}@h/db\" >&2\nexit 1"));
        let out = tempfile::tempdir().unwrap();
        let (state, cancel) = state_and_cancel();
        let report = run_backup(pg_job(pg_connection(), out.path().join("t.dump"), &tools), &tools.locator(), &state, &cancel, &|| {});
        let everything = format!("{report:?} {:?}", state.range(0..20));
        assert!(!everything.contains(PASSWORD), "{everything}");
        assert!(everything.contains("***") || everything.contains("<redacted-uri>"), "{everything}");
    }
}
