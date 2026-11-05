//! Finding the server's own client tools (`pg_dump`, `pg_restore`, `mysqldump`, `mariadb-dump`) and
//! telling which version each one is.
//!
//! The order of the search: the path set in Settings for the tool, then the directories of `PATH`, then
//! the places the operating system's installers use ([`platform_roots`], one function per OS). Each
//! candidate is asked for `--version`; of the ones that answer, the newest that is allowed wins. A
//! PostgreSQL tool older than the server is refused with the versions named: `pg_dump` cannot read a
//! newer server.

use std::collections::HashSet;
use std::io::Read as _;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

/// How long `--version` may take before the candidate is given up on.
const PROBE_TIMEOUT: Duration = Duration::from_secs(5);

/// A client program.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Tool {
    PgDump,
    PgRestore,
    MysqlDump,
    MariadbDump,
}

impl Tool {
    /// The program's name without the platform's executable suffix.
    pub fn program(self) -> &'static str {
        match self {
            Self::PgDump => "pg_dump",
            Self::PgRestore => "pg_restore",
            Self::MysqlDump => "mysqldump",
            Self::MariadbDump => "mariadb-dump",
        }
    }

    fn file_name(self) -> String {
        format!("{}{}", self.program(), std::env::consts::EXE_SUFFIX)
    }
}

/// The paths Settings sets for each tool; empty: look for it.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ToolPaths {
    pub pg_dump: String,
    pub pg_restore: String,
    pub mysqldump: String,
    pub mariadb_dump: String,
}

impl ToolPaths {
    pub fn get(&self, tool: Tool) -> Option<&str> {
        let path = match tool {
            Tool::PgDump => &self.pg_dump,
            Tool::PgRestore => &self.pg_restore,
            Tool::MysqlDump => &self.mysqldump,
            Tool::MariadbDump => &self.mariadb_dump,
        };
        Some(path.trim()).filter(|path| !path.is_empty())
    }
}

/// Which server a tool was built for.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Family {
    Postgres,
    Mysql,
    Mariadb,
}

/// `major.minor.patch` of a program.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct Version {
    pub major: u32,
    pub minor: u32,
    pub patch: u32,
}

impl std::fmt::Display for Version {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}.{}", self.major, self.minor)?;
        if self.patch > 0 {
            write!(f, ".{}", self.patch)?;
        }
        Ok(())
    }
}

/// What `--version` said.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ToolVersion {
    pub family: Family,
    pub version: Version,
}

/// The numbers at the start of `text` (`17.4`, `8.0.36-0ubuntu1`, `15beta1`, `18devel`), up to three.
fn leading_version(text: &str) -> Option<Version> {
    let mut numbers = [0u32; 3];
    let mut count = 0;
    let mut rest = text;
    while count < 3 {
        let digits = rest.find(|c: char| !c.is_ascii_digit()).unwrap_or(rest.len());
        if digits == 0 {
            break;
        }
        numbers[count] = rest[..digits].parse().ok()?;
        count += 1;
        rest = &rest[digits..];
        match rest.strip_prefix('.') {
            Some(after) if after.starts_with(|c: char| c.is_ascii_digit()) => rest = after,
            _ => break,
        }
    }
    (count > 0).then_some(Version { major: numbers[0], minor: numbers[1], patch: numbers[2] })
}

/// Reads the output of `<tool> --version`:
///
/// * `pg_dump (PostgreSQL) 17.4`, `pg_restore (PostgreSQL) 16.2 (Ubuntu 16.2-1.pgdg22.04+1)`, `15beta1`;
/// * `mysqldump  Ver 8.0.36 for macos14.2 on arm64 (Homebrew)`;
/// * MariaDB's `mysqldump  Ver 10.19 Distrib 10.11.6-MariaDB, for debian-linux-gnu` and
///   `mariadb-dump from 11.4.2-MariaDB, client 10.19 for linux-systemd`.
pub fn parse_version(tool: Tool, output: &str) -> Option<ToolVersion> {
    let line = output.lines().find(|line| !line.trim().is_empty())?;
    match tool {
        Tool::PgDump | Tool::PgRestore => {
            let after = line.split_once("(PostgreSQL)")?.1.trim_start();
            Some(ToolVersion { family: Family::Postgres, version: leading_version(after)? })
        }
        Tool::MysqlDump | Tool::MariadbDump => {
            let mariadb = line.contains("MariaDB") || line.contains("mariadb-dump");
            // The server's own number: after `Distrib` (MariaDB's mysqldump), `from` (mariadb-dump) or `Ver`.
            let number = [" Distrib ", " from ", " Ver "].iter().find_map(|marker| line.split_once(marker).map(|(_, after)| after.trim_start()))?;
            // MariaDB's `Ver 10.19 Distrib ...` names the client protocol first: `Distrib` wins above, `Ver` is the last resort.
            Some(ToolVersion { family: if mariadb { Family::Mariadb } else { Family::Mysql }, version: leading_version(number)? })
        }
    }
}

/// Why a program could not be asked for its version.
#[derive(Debug)]
pub enum ProbeError {
    Spawn(std::io::Error),
    TimedOut,
    Unreadable,
}

impl std::fmt::Display for ProbeError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Spawn(error) => write!(f, "it could not be started: {error}"),
            Self::TimedOut => f.write_str("it did not answer `--version`"),
            Self::Unreadable => f.write_str("its `--version` answer was not recognised"),
        }
    }
}

impl std::error::Error for ProbeError {}

/// Runs `<path> --version` and reads the answer. Blocking, bounded by [`PROBE_TIMEOUT`]: for the program, and for its output
/// (a wrapper script whose child keeps the pipe open past the wrapper's end does not hang the search).
pub fn probe(tool: Tool, path: &Path) -> Result<ToolVersion, ProbeError> {
    let mut command = Command::new(path);
    command.arg("--version").stdin(Stdio::null()).stdout(Stdio::piped()).stderr(Stdio::null());
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt as _;
        // No console window for a program of a window application.
        command.creation_flags(0x0800_0000);
    }
    let mut child = command.spawn().map_err(ProbeError::Spawn)?;
    let mut stdout = child.stdout.take().ok_or(ProbeError::Unreadable)?;
    let (sender, answer) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        // The first line that says something is the version: not waiting for the end of the stream.
        let mut reader = std::io::BufReader::new(stdout.by_ref().take(64 * 1024));
        let mut text = String::new();
        for _ in 0..16 {
            match std::io::BufRead::read_line(&mut reader, &mut text) {
                Ok(0) | Err(_) => break,
                Ok(_) if !text.trim().is_empty() => break,
                Ok(_) => {}
            }
        }
        let _ = sender.send(text);
    });
    let deadline = Instant::now() + PROBE_TIMEOUT;
    loop {
        match child.try_wait() {
            Ok(Some(_)) => break,
            Ok(None) if Instant::now() < deadline => std::thread::sleep(Duration::from_millis(5)),
            _ => {
                let _ = child.kill();
                let _ = child.wait();
                return Err(ProbeError::TimedOut);
            }
        }
    }
    // What was printed is there once the program ended, unless something it started holds the pipe: not waited for.
    let text = answer.recv_timeout(Duration::from_secs(2)).map_err(|_| ProbeError::TimedOut)?;
    parse_version(tool, &text).ok_or(ProbeError::Unreadable)
}

/// A place the search looks in.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SearchRoot {
    /// This directory.
    Dir(PathBuf),
    /// `parent/<name starting with prefix>/tail` for every such name (`/usr/lib/postgresql/*/bin`).
    Glob { parent: PathBuf, prefix: String, tail: PathBuf },
}

impl SearchRoot {
    pub fn dir(path: impl Into<PathBuf>) -> Self {
        Self::Dir(path.into())
    }

    pub fn glob(parent: impl Into<PathBuf>, prefix: &str, tail: impl Into<PathBuf>) -> Self {
        Self::Glob { parent: parent.into(), prefix: prefix.to_owned(), tail: tail.into() }
    }

    fn directories(&self) -> Vec<PathBuf> {
        match self {
            Self::Dir(path) => vec![path.clone()],
            Self::Glob { parent, prefix, tail } => {
                let mut found: Vec<PathBuf> = std::fs::read_dir(parent)
                    .into_iter()
                    .flatten()
                    .flatten()
                    .filter(|entry| entry.file_name().to_string_lossy().starts_with(prefix.as_str()))
                    .map(|entry| entry.path().join(tail))
                    .collect();
                found.sort();
                found
            }
        }
    }
}

/// Where the installers of each operating system put the tools, besides `PATH`.
#[cfg(target_os = "macos")]
fn platform_roots() -> Vec<SearchRoot> {
    let mut roots = Vec::new();
    // Homebrew: Apple silicon, then Intel.
    for prefix in ["/opt/homebrew", "/usr/local"] {
        let opt = Path::new(prefix).join("opt");
        roots.push(SearchRoot::glob(&opt, "postgresql@", "bin"));
        for keg in ["postgresql", "libpq", "mysql-client", "mysql", "mariadb"] {
            roots.push(SearchRoot::dir(opt.join(keg).join("bin")));
        }
        roots.push(SearchRoot::dir(Path::new(prefix).join("bin")));
    }
    // Postgres.app, and the installer of mysql.com.
    roots.push(SearchRoot::glob("/Applications/Postgres.app/Contents/Versions", "", "bin"));
    roots.push(SearchRoot::dir("/usr/local/mysql/bin"));
    roots
}

#[cfg(target_os = "linux")]
fn platform_roots() -> Vec<SearchRoot> {
    vec![
        SearchRoot::glob("/usr/lib/postgresql", "", "bin"),
        SearchRoot::glob("/usr", "pgsql-", "bin"),
        SearchRoot::dir("/usr/bin"),
        SearchRoot::dir("/usr/local/bin"),
        SearchRoot::dir("/usr/local/mysql/bin"),
    ]
}

#[cfg(windows)]
fn platform_roots() -> Vec<SearchRoot> {
    let mut roots = Vec::new();
    for variable in ["ProgramFiles", "ProgramFiles(x86)", "ProgramW6432"] {
        let Some(base) = std::env::var_os(variable).map(PathBuf::from) else { continue };
        roots.push(SearchRoot::glob(base.join("PostgreSQL"), "", "bin"));
        roots.push(SearchRoot::glob(&base, "MySQL Server", "bin"));
        roots.push(SearchRoot::glob(&base, "MariaDB", "bin"));
        roots.push(SearchRoot::glob(base.join("MySQL"), "MySQL Server", "bin"));
    }
    roots
}

#[cfg(not(any(target_os = "macos", target_os = "linux", windows)))]
fn platform_roots() -> Vec<SearchRoot> {
    vec![SearchRoot::dir("/usr/local/bin")]
}

/// The directories of a `PATH` value that are searched: the absolute ones. A relative entry (`.`, an empty one) means the working folder,
/// where a program planted by whoever put the user there would be found first.
fn path_roots(path: &std::ffi::OsStr) -> Vec<SearchRoot> {
    std::env::split_paths(path).filter(|dir| dir.is_absolute()).map(SearchRoot::Dir).collect()
}

/// Where the search looks, in order.
#[derive(Debug, Clone, Default)]
pub struct Locator {
    roots: Vec<SearchRoot>,
}

/// Which tool programs [`Locator::resolve`] may answer with, and what the answer must satisfy.
#[derive(Debug, Clone, Copy)]
pub struct Need<'a> {
    /// The programs wanted, best first (`mariadb-dump`, then `mysqldump`).
    pub tools: &'a [Tool],
    /// A PostgreSQL tool must be at least this major version (the server's).
    pub min_major: Option<u32>,
}

/// Why no tool could be used.
#[derive(Debug)]
pub enum ToolError {
    /// Nothing named like it was found where the search looks.
    NotFound { tool: Tool, searched: usize },
    /// The path set in Settings does not lead to a working tool.
    Configured { tool: Tool, path: PathBuf, problem: ProbeError },
    /// The newest one found is older than the server.
    TooOld { tool: Tool, path: PathBuf, found: Version, needed: u32 },
}

impl std::fmt::Display for ToolError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::NotFound { tool, searched } => write!(
                f,
                "{} was not found (looked in PATH and {searched} usual install folders). Install the {} client tools, or set the path of {} in Settings, Query & Connections.",
                tool.program(),
                if matches!(tool, Tool::PgDump | Tool::PgRestore) { "PostgreSQL" } else { "MySQL or MariaDB" },
                tool.program()
            ),
            Self::Configured { tool, path, problem } => write!(f, "The {} set in Settings ({}) cannot be used: {problem}.", tool.program(), path.display()),
            Self::TooOld { tool, path, found, needed } => write!(
                f,
                "{} {found} ({}) is older than the server (PostgreSQL {needed}); it cannot read a newer server. Install the PostgreSQL {needed} client tools, or set the path of a newer {} in Settings, Query & Connections.",
                tool.program(),
                path.display(),
                tool.program()
            ),
        }
    }
}

impl std::error::Error for ToolError {}

/// A tool that will be run.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Found {
    pub tool: Tool,
    pub path: PathBuf,
    pub version: ToolVersion,
}

impl Found {
    /// `pg_dump 17.4 (/opt/homebrew/opt/postgresql@17/bin/pg_dump)`. Named after the program that runs: a `mariadb-dump` set as the
    /// mysqldump in Settings reads `mariadb-dump 11.4.2`.
    pub fn describe(&self) -> String {
        format!("{} {} ({})", self.program(), self.version.version, self.path.display())
    }

    /// The name of the program that runs (`mariadb-dump`, `mysqldump-8.0`), which a path set in Settings may give another name than
    /// [`Tool::program`]; the platform's executable suffix is left out.
    pub fn program(&self) -> &str {
        let name = self.path.file_name().and_then(|name| name.to_str()).unwrap_or_default();
        match name.strip_suffix(std::env::consts::EXE_SUFFIX).unwrap_or(name) {
            "" => self.tool.program(),
            program => program,
        }
    }
}

impl Locator {
    /// `PATH`, then the usual places of this operating system.
    pub fn system() -> Self {
        let mut roots = std::env::var_os("PATH").map(|path| path_roots(&path)).unwrap_or_default();
        roots.extend(platform_roots());
        Self { roots }
    }

    /// Only these places (tests).
    #[cfg(test)]
    pub fn with_roots(roots: Vec<SearchRoot>) -> Self {
        Self { roots }
    }

    /// Every existing file called like `tool` in the places searched, each once.
    fn candidates(&self, tool: Tool) -> (Vec<PathBuf>, usize) {
        let name = tool.file_name();
        let mut seen = HashSet::new();
        let mut directories = 0;
        let mut found = Vec::new();
        for root in &self.roots {
            for dir in root.directories() {
                directories += 1;
                let path = dir.join(&name);
                if path.is_file() && seen.insert(std::fs::canonicalize(&path).unwrap_or_else(|_| path.clone())) {
                    found.push(path);
                }
            }
        }
        (found, directories)
    }

    /// The tool to run for `need`: a path from Settings first, else the newest allowed of the ones found.
    pub fn resolve(&self, need: Need<'_>, paths: &ToolPaths) -> Result<Found, ToolError> {
        self.resolve_with(need, paths, &probe)
    }

    fn resolve_with(&self, need: Need<'_>, paths: &ToolPaths, probe: &dyn Fn(Tool, &Path) -> Result<ToolVersion, ProbeError>) -> Result<Found, ToolError> {
        let allowed = |found: &Found| need.min_major.is_none_or(|needed| found.version.family != Family::Postgres || found.version.version.major >= needed);
        let too_old = |found: Found, needed: u32| ToolError::TooOld { tool: found.tool, path: found.path, found: found.version.version, needed };
        // A path the user set is theirs to get right: it is used, or the reason it is not is told.
        for tool in need.tools {
            let Some(configured) = paths.get(*tool) else { continue };
            let path = PathBuf::from(configured);
            let version = probe(*tool, &path).map_err(|problem| ToolError::Configured { tool: *tool, path: path.clone(), problem })?;
            let found = Found { tool: *tool, path, version };
            return match need.min_major {
                Some(needed) if !allowed(&found) => Err(too_old(found, needed)),
                _ => Ok(found),
            };
        }
        let mut oldest_refused: Option<Found> = None;
        let mut searched = 0;
        for tool in need.tools {
            let (candidates, directories) = self.candidates(*tool);
            searched = searched.max(directories);
            let mut usable = Vec::new();
            for path in candidates {
                let Ok(version) = probe(*tool, &path) else { continue };
                let found = Found { tool: *tool, path, version };
                if allowed(&found) {
                    usable.push(found);
                } else if oldest_refused.as_ref().is_none_or(|refused| found.version.version > refused.version.version) {
                    oldest_refused = Some(found);
                }
            }
            // The newest; among equals the earlier place in the search wins.
            if let Some(best) = usable.into_iter().reduce(|best, next| if next.version.version > best.version.version { next } else { best }) {
                return Ok(best);
            }
        }
        match (oldest_refused, need.min_major) {
            (Some(refused), Some(needed)) => Err(too_old(refused, needed)),
            _ => Err(ToolError::NotFound { tool: need.tools[0], searched }),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn version(tool: Tool, text: &str) -> Option<(Family, String)> {
        parse_version(tool, text).map(|parsed| (parsed.family, parsed.version.to_string()))
    }

    #[test]
    fn postgres_versions_are_read_in_every_spelling() {
        assert_eq!(version(Tool::PgDump, "pg_dump (PostgreSQL) 17.4\n"), Some((Family::Postgres, "17.4".into())));
        assert_eq!(version(Tool::PgRestore, "pg_restore (PostgreSQL) 16.2 (Ubuntu 16.2-1.pgdg22.04+1)"), Some((Family::Postgres, "16.2".into())));
        assert_eq!(version(Tool::PgDump, "pg_dump (PostgreSQL) 9.6.24"), Some((Family::Postgres, "9.6.24".into())));
        assert_eq!(version(Tool::PgDump, "pg_dump (PostgreSQL) 15beta1"), Some((Family::Postgres, "15.0".into())));
        assert_eq!(version(Tool::PgDump, "pg_dump (PostgreSQL) 18devel"), Some((Family::Postgres, "18.0".into())));
        assert_eq!(version(Tool::PgDump, "pg_dump (EnterpriseDB) 16"), None);
    }

    #[test]
    fn mysql_and_mariadb_versions_are_told_apart() {
        assert_eq!(version(Tool::MysqlDump, "mysqldump  Ver 8.0.36 for macos14.2 on arm64 (Homebrew)"), Some((Family::Mysql, "8.0.36".into())));
        assert_eq!(version(Tool::MysqlDump, "mysqldump  Ver 8.4.0 for Linux on x86_64 (MySQL Community Server - GPL)"), Some((Family::Mysql, "8.4".into())));
        assert_eq!(version(Tool::MysqlDump, "mysqldump  Ver 8.0.36-0ubuntu0.22.04.1 for Linux on x86_64 ((Ubuntu))"), Some((Family::Mysql, "8.0.36".into())));
        // MariaDB's old name for the program: the server's number is after `Distrib`.
        assert_eq!(version(Tool::MysqlDump, "mysqldump  Ver 10.19 Distrib 10.11.6-MariaDB, for debian-linux-gnu (x86_64)"), Some((Family::Mariadb, "10.11.6".into())));
        assert_eq!(version(Tool::MariadbDump, "mariadb-dump from 11.4.2-MariaDB, client 10.19 for linux-systemd (x86_64)"), Some((Family::Mariadb, "11.4.2".into())));
        assert_eq!(version(Tool::MariadbDump, ""), None);
    }

    /// The report names the program that ran, not the setting it was found through.
    #[test]
    fn a_found_tool_is_named_after_the_program_that_runs() {
        let found = |tool, path: &str| Found { tool, path: PathBuf::from(path), version: ToolVersion { family: Family::Mariadb, version: Version { major: 11, minor: 4, patch: 2 } } };
        assert_eq!(found(Tool::MysqlDump, "/opt/mariadb/bin/mariadb-dump").describe(), "mariadb-dump 11.4.2 (/opt/mariadb/bin/mariadb-dump)");
        assert_eq!(found(Tool::MysqlDump, "/usr/bin/mysqldump").describe(), "mysqldump 11.4.2 (/usr/bin/mysqldump)");
        assert_eq!(found(Tool::MysqlDump, "/").describe(), "mysqldump 11.4.2 (/)");
        assert_eq!(found(Tool::MysqlDump, "/opt/mysql/bin/mysqldump-8.0").program(), "mysqldump-8.0");
        assert_eq!(found(Tool::MysqlDump, &format!("/opt/bin/mariadb-dump{}", std::env::consts::EXE_SUFFIX)).program(), "mariadb-dump");
    }

    #[cfg(unix)]
    mod on_disk {
        use super::*;
        use std::os::unix::fs::PermissionsExt as _;

        /// A program that prints `answer` for `--version`.
        pub(super) fn fake(dir: &Path, name: &str, answer: &str) -> PathBuf {
            std::fs::create_dir_all(dir).unwrap();
            let path = dir.join(name);
            std::fs::write(&path, format!("#!/bin/sh\necho '{answer}'\n")).unwrap();
            std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
            path
        }

        fn pg(version: &str) -> String {
            format!("pg_dump (PostgreSQL) {version}")
        }

        fn need(min_major: Option<u32>) -> Need<'static> {
            Need { tools: &[Tool::PgDump], min_major }
        }

        #[test]
        fn the_newest_allowed_version_in_a_fake_tree_wins_and_a_set_path_comes_first() {
            let tree = tempfile::tempdir().unwrap();
            let root = tree.path();
            fake(&root.join("lib/postgresql/15/bin"), "pg_dump", &pg("15.6"));
            let newest = fake(&root.join("lib/postgresql/17/bin"), "pg_dump", &pg("17.4"));
            fake(&root.join("lib/postgresql/16/bin"), "pg_dump", &pg("16.2"));
            fake(&root.join("path-dir"), "pg_dump", &pg("14.1"));
            let locator = Locator::with_roots(vec![SearchRoot::dir(root.join("path-dir")), SearchRoot::glob(root.join("lib/postgresql"), "", "bin")]);

            // No server version: the newest of all.
            assert_eq!(locator.resolve(need(None), &ToolPaths::default()).unwrap().path, newest);
            // PostgreSQL 16 server: 17.4 and 16.2 qualify, the newest of them is chosen.
            let found = locator.resolve(need(Some(16)), &ToolPaths::default()).unwrap();
            assert_eq!((found.path.clone(), found.version.version.major), (newest.clone(), 17));
            // Only the one on PATH is 14: too old for a 17 server only if nothing newer exists.
            assert_eq!(locator.resolve(need(Some(17)), &ToolPaths::default()).unwrap().path, newest);

            // A path from Settings beats the search, even a newer one on disk.
            let configured = fake(&root.join("mine"), "pg_dump", &pg("16.9"));
            let paths = ToolPaths { pg_dump: configured.display().to_string(), ..ToolPaths::default() };
            assert_eq!(locator.resolve(need(None), &paths).unwrap().path, configured);
        }

        #[test]
        fn a_tool_older_than_the_server_is_refused_with_both_versions_named() {
            let tree = tempfile::tempdir().unwrap();
            fake(&tree.path().join("a"), "pg_dump", &pg("15.6"));
            fake(&tree.path().join("b"), "pg_dump", &pg("16.2"));
            let locator = Locator::with_roots(vec![SearchRoot::dir(tree.path().join("a")), SearchRoot::dir(tree.path().join("b"))]);
            let error = locator.resolve(need(Some(17)), &ToolPaths::default()).unwrap_err();
            let text = error.to_string();
            assert!(matches!(error, ToolError::TooOld { found, needed: 17, .. } if found.major == 16), "{text}");
            assert!(text.contains("16.2") && text.contains("PostgreSQL 17") && text.contains("Settings"), "{text}");

            // The same for a path from Settings: it is not silently skipped.
            let old = fake(&tree.path().join("c"), "pg_dump", &pg("12.1"));
            let paths = ToolPaths { pg_dump: old.display().to_string(), ..ToolPaths::default() };
            assert!(matches!(locator.resolve(need(Some(17)), &paths).unwrap_err(), ToolError::TooOld { .. }));
        }

        #[test]
        fn a_missing_or_broken_tool_says_where_to_look() {
            let tree = tempfile::tempdir().unwrap();
            let locator = Locator::with_roots(vec![SearchRoot::dir(tree.path().join("nothing")), SearchRoot::glob(tree.path().join("none"), "", "bin")]);
            let error = locator.resolve(need(None), &ToolPaths::default()).unwrap_err();
            assert!(matches!(error, ToolError::NotFound { tool: Tool::PgDump, searched: 1 }), "{error}");
            assert!(error.to_string().contains("set the path"), "{error}");

            let garbage = fake(tree.path(), "pg_dump", "not a version");
            let paths = ToolPaths { pg_dump: garbage.display().to_string(), ..ToolPaths::default() };
            assert!(matches!(locator.resolve(need(None), &paths).unwrap_err(), ToolError::Configured { problem: ProbeError::Unreadable, .. }));
            let paths = ToolPaths { pg_dump: tree.path().join("absent").display().to_string(), ..ToolPaths::default() };
            assert!(matches!(locator.resolve(need(None), &paths).unwrap_err(), ToolError::Configured { problem: ProbeError::Spawn(_), .. }));
        }

        #[test]
        fn mariadb_prefers_its_own_dump_and_falls_back_to_mysqldump() {
            let tree = tempfile::tempdir().unwrap();
            let mysql = fake(&tree.path().join("a"), "mysqldump", "mysqldump  Ver 8.0.36 for Linux on x86_64");
            let locator = Locator::with_roots(vec![SearchRoot::dir(tree.path().join("a")), SearchRoot::dir(tree.path().join("b"))]);
            let need = Need { tools: &[Tool::MariadbDump, Tool::MysqlDump], min_major: None };
            assert_eq!(locator.resolve(need, &ToolPaths::default()).unwrap().path, mysql);
            let maria = fake(&tree.path().join("b"), "mariadb-dump", "mariadb-dump from 11.4.2-MariaDB, client 10.19 for linux");
            assert_eq!(locator.resolve(need, &ToolPaths::default()).unwrap().path, maria);
        }

        #[test]
        fn a_wrapper_whose_child_keeps_the_pipe_open_does_not_hang_the_search() {
            let tree = tempfile::tempdir().unwrap();
            let wrapper = fake(tree.path(), "pg_dump", "pg_dump (PostgreSQL) 17.4");
            std::fs::write(&wrapper, "#!/bin/sh\nsleep 30 &\necho 'pg_dump (PostgreSQL) 17.4'\n").unwrap();
            let started = Instant::now();
            assert_eq!(probe(Tool::PgDump, &wrapper).unwrap().version.major, 17);
            assert!(started.elapsed() < Duration::from_secs(5), "{:?}", started.elapsed());
        }

        #[test]
        fn a_program_that_never_answers_is_given_up_on() {
            // Not slept for 5 s: a probe of a program that exits at once, with no answer, is unreadable.
            let tree = tempfile::tempdir().unwrap();
            let silent = tree.path().join("pg_dump");
            std::fs::write(&silent, "#!/bin/sh\nexit 0\n").unwrap();
            std::fs::set_permissions(&silent, std::fs::Permissions::from_mode(0o755)).unwrap();
            assert!(matches!(probe(Tool::PgDump, &silent), Err(ProbeError::Unreadable)));
        }
    }

    #[cfg(unix)]
    #[test]
    fn relative_entries_of_the_path_are_not_searched() {
        let roots = path_roots(std::ffi::OsStr::new("/usr/bin::.:bin:/opt/pg/bin"));
        assert_eq!(roots, vec![SearchRoot::dir("/usr/bin"), SearchRoot::dir("/opt/pg/bin")]);
    }

    #[test]
    fn the_install_folders_of_this_operating_system_are_searched_after_path() {
        let roots = platform_roots();
        assert!(!roots.is_empty());
        #[cfg(target_os = "macos")]
        {
            assert!(roots.contains(&SearchRoot::glob("/opt/homebrew/opt", "postgresql@", "bin")));
            assert!(roots.contains(&SearchRoot::dir("/usr/local/opt/mysql-client/bin")));
            assert!(roots.contains(&SearchRoot::glob("/Applications/Postgres.app/Contents/Versions", "", "bin")));
        }
        #[cfg(target_os = "linux")]
        assert!(roots.contains(&SearchRoot::glob("/usr/lib/postgresql", "", "bin")));
    }
}
