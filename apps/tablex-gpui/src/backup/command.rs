//! The options of a backup or restore and the command each one becomes.
//!
//! Every value that comes from the user or the server is its own argument (never pasted into a shell
//! line), and written as `--name=value` so a name that starts with `-` cannot become an option. A
//! command line holds no secret: the password is in a private file the environment (PostgreSQL) or the
//! first argument (MySQL, MariaDB) points to.

use std::ffi::OsString;
use std::path::{Path, PathBuf};

use super::endpoint::Endpoint;
use super::tools::{Family, Found, Version};
use crate::connection_tls::{TlsFamily, TlsMode};
use crate::connections::ConnectionKind;

/// Why the options cannot be turned into a command.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum OptionsError {
    CompressionRange(u32),
    CompressionNeedsArchive,
    JobsRange(u32),
    JobsNeedDirectory,
    JobsWithSingleTransaction,
    CleanWithDataOnly,
    NothingSelected,
}

impl std::fmt::Display for OptionsError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::CompressionRange(level) => write!(f, "The compression level is 0 to 9, not {level}."),
            Self::CompressionNeedsArchive => f.write_str("Compression applies to the custom and directory formats."),
            Self::JobsRange(jobs) => write!(f, "The number of jobs is 1 to {MAX_JOBS}, not {jobs}."),
            Self::JobsNeedDirectory => f.write_str("Parallel jobs need the directory format."),
            Self::JobsWithSingleTransaction => f.write_str("Parallel jobs and a single transaction cannot be combined."),
            Self::CleanWithDataOnly => f.write_str("Dropping objects first (clean) or creating the database needs the structure: it cannot be combined with data only."),
            Self::NothingSelected => f.write_str("Select at least one schema or table, or back up the whole database."),
        }
    }
}

impl std::error::Error for OptionsError {}

/// The most parallel jobs a backup or restore may ask for.
pub const MAX_JOBS: u32 = 64;

/// A command to run: the program, its arguments and environment.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Invocation {
    pub program: PathBuf,
    pub args: Vec<OsString>,
    /// Set for the child.
    pub env: Vec<(&'static str, OsString)>,
    /// Removed for the child: variables of the user's shell that would change where the tool connects.
    pub env_remove: &'static [&'static str],
}

impl Invocation {
    /// The command line as the log shows it. It holds no secret.
    pub fn command_line(&self) -> String {
        let quote = |text: String| if text.is_empty() || text.contains(char::is_whitespace) { format!("\"{text}\"") } else { text };
        let name = self.program.file_name().map_or_else(String::new, |name| name.to_string_lossy().into_owned());
        std::iter::once(name).chain(self.args.iter().map(|arg| quote(arg.to_string_lossy().into_owned()))).collect::<Vec<_>>().join(" ")
    }
}

/// libpq variables that would change where or as whom the tool connects, or what it trusts.
const PG_ENV_REMOVE: &[&str] = &[
    "PGPASSWORD",
    "PGPASSFILE",
    "PGHOST",
    "PGHOSTADDR",
    "PGPORT",
    "PGUSER",
    "PGDATABASE",
    "PGSERVICE",
    "PGSERVICEFILE",
    "PGSYSCONFDIR",
    "PGOPTIONS",
    "PGSSLMODE",
    "PGSSLROOTCERT",
    "PGSSLCERT",
    "PGSSLKEY",
    "PGSSLCRL",
    "PGSSLCRLDIR",
    "PGSSLCERTMODE",
    "PGSSLNEGOTIATION",
    "PGSSLMINPROTOCOLVERSION",
    "PGSSLMAXPROTOCOLVERSION",
    "PGGSSENCMODE",
    "PGREQUIREAUTH",
    "PGCHANNELBINDING",
    "PGTARGETSESSIONATTRS",
];
/// `MYSQL_HOME` and the group suffix change which option files and groups are read; the rest change where the client connects.
const MYSQL_ENV_REMOVE: &[&str] = &["MYSQL_PWD", "MYSQL_HOST", "MYSQL_TCP_PORT", "MYSQL_UNIX_PORT", "MYSQL_HOME", "MYSQL_GROUP_SUFFIX", "MYSQL_TEST_LOGIN_FILE"];

fn arg(name: &str, value: impl AsRef<std::ffi::OsStr>) -> OsString {
    let mut text = OsString::from(format!("--{name}="));
    text.push(value);
    text
}

/// PostgreSQL's name for a schema or table in a `--schema` / `--table` pattern: quoted, so case and dots
/// stay as they are and `*` is not a wildcard.
pub fn pg_pattern(parts: &[&str]) -> String {
    parts.iter().map(|part| format!("\"{}\"", part.replace('"', "\"\""))).collect::<Vec<_>>().join(".")
}

/// The value of `--dbname`: a connection string with the one `dbname`, so that a name that holds `=` or starts with `postgresql://` is
/// the name of a database and never a connection string of its own (which would override the host, the TLS mode and the rest).
/// `\` and `'` are escaped the way libpq reads a quoted value.
pub fn dbname_argument(database: &str) -> String {
    format!("dbname='{}'", database.replace('\\', "\\\\").replace('\'', "\\'"))
}

/// How libpq is told where and as whom to connect.
fn pg_connection(endpoint: &Endpoint, database: &str, passfile: Option<&Path>, args: &mut Vec<OsString>, env: &mut Vec<(&'static str, OsString)>) {
    if !endpoint.host.is_empty() {
        args.push(arg("host", &endpoint.host));
    }
    if let Some(port) = endpoint.port {
        args.push(arg("port", port.to_string()));
    }
    if let Some(user) = &endpoint.user {
        args.push(arg("username", user));
    }
    // Never ask: there is no terminal to answer.
    args.push("--no-password".into());
    args.push(arg("dbname", dbname_argument(database)));
    if let Some(passfile) = passfile {
        env.push(("PGPASSFILE", passfile.into()));
    }
    // A tunnel: the connection goes to its local address, while the host stays the name TLS checks and the password is matched on.
    if let Some(addr) = &endpoint.host_addr {
        env.push(("PGHOSTADDR", addr.into()));
    }
    let tls = &endpoint.tls;
    if tls.mode != TlsMode::Default {
        env.push(("PGSSLMODE", TlsFamily::Postgres.label(tls.mode).into()));
    }
    for (name, file) in [("PGSSLROOTCERT", &tls.ca_file), ("PGSSLCERT", &tls.client_cert), ("PGSSLKEY", &tls.client_key)] {
        if !file.trim().is_empty() {
            env.push((name, file.trim().into()));
        }
    }
}

// ---- pg_dump ---------------------------------------------------------------------------------------------

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PgFormat {
    Plain,
    Custom,
    Directory,
    Tar,
}

impl PgFormat {
    pub const ALL: [Self; 4] = [Self::Plain, Self::Custom, Self::Directory, Self::Tar];

    pub fn label(self) -> &'static str {
        match self {
            Self::Plain => "Plain SQL script",
            Self::Custom => "Custom archive (compressed)",
            Self::Directory => "Directory (one file per table)",
            Self::Tar => "Tar archive",
        }
    }

    fn letter(self) -> &'static str {
        match self {
            Self::Plain => "p",
            Self::Custom => "c",
            Self::Directory => "d",
            Self::Tar => "t",
        }
    }

    /// The file name's extension; none for a directory.
    pub fn extension(self) -> Option<&'static str> {
        match self {
            Self::Plain => Some("sql"),
            Self::Custom => Some("dump"),
            Self::Directory => None,
            Self::Tar => Some("tar"),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Content {
    SchemaAndData,
    SchemaOnly,
    DataOnly,
}

impl Content {
    pub const ALL: [Self; 3] = [Self::SchemaAndData, Self::SchemaOnly, Self::DataOnly];

    pub fn label(self) -> &'static str {
        match self {
            Self::SchemaAndData => "Structure and data",
            Self::SchemaOnly => "Structure only",
            Self::DataOnly => "Data only",
        }
    }
}

/// What of the database is dumped.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PgScope {
    Everything,
    Schemas(Vec<String>),
    /// `(schema, table)`.
    Tables(Vec<(String, String)>),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PgDumpOptions {
    pub format: PgFormat,
    pub content: Content,
    pub scope: PgScope,
    pub no_owner: bool,
    pub no_privileges: bool,
    /// Plain format only (an archive takes it at restore): DROP the objects first, with IF EXISTS.
    pub clean: bool,
    /// Plain format only: CREATE DATABASE and connect to it.
    pub create: bool,
    /// Custom and directory formats.
    pub compression: Option<u32>,
    /// Directory format.
    pub jobs: Option<u32>,
}

impl Default for PgDumpOptions {
    fn default() -> Self {
        Self { format: PgFormat::Plain, content: Content::SchemaAndData, scope: PgScope::Everything, no_owner: false, no_privileges: false, clean: false, create: false, compression: None, jobs: None }
    }
}

impl PgDumpOptions {
    pub fn validate(&self) -> Result<(), OptionsError> {
        if let Some(level) = self.compression {
            if level > 9 {
                return Err(OptionsError::CompressionRange(level));
            }
            if !matches!(self.format, PgFormat::Custom | PgFormat::Directory) {
                return Err(OptionsError::CompressionNeedsArchive);
            }
        }
        if let Some(jobs) = self.jobs {
            if !(1..=MAX_JOBS).contains(&jobs) {
                return Err(OptionsError::JobsRange(jobs));
            }
            if jobs > 1 && self.format != PgFormat::Directory {
                return Err(OptionsError::JobsNeedDirectory);
            }
        }
        if self.format == PgFormat::Plain && self.content == Content::DataOnly && (self.clean || self.create) {
            return Err(OptionsError::CleanWithDataOnly);
        }
        match &self.scope {
            PgScope::Schemas(schemas) if schemas.is_empty() => Err(OptionsError::NothingSelected),
            PgScope::Tables(tables) if tables.is_empty() => Err(OptionsError::NothingSelected),
            _ => Ok(()),
        }
    }
}

/// `pg_dump` of `database` into `output` (a file, or a directory for the directory format).
pub fn pg_dump(tool: &Found, endpoint: &Endpoint, database: &str, passfile: Option<&Path>, options: &PgDumpOptions, output: &Path) -> Result<Invocation, OptionsError> {
    options.validate()?;
    let mut args = Vec::new();
    let mut env = Vec::new();
    pg_connection(endpoint, database, passfile, &mut args, &mut env);
    args.push(arg("format", options.format.letter()));
    args.push(arg("file", output));
    match options.content {
        Content::SchemaAndData => {}
        Content::SchemaOnly => args.push("--schema-only".into()),
        Content::DataOnly => args.push("--data-only".into()),
    }
    match &options.scope {
        PgScope::Everything => {}
        PgScope::Schemas(schemas) => args.extend(schemas.iter().map(|schema| arg("schema", pg_pattern(&[schema])))),
        PgScope::Tables(tables) => args.extend(tables.iter().map(|(schema, table)| arg("table", pg_pattern(&[schema, table])))),
    }
    if options.no_owner {
        args.push("--no-owner".into());
    }
    if options.no_privileges {
        args.push("--no-privileges".into());
    }
    if options.format == PgFormat::Plain {
        if options.clean {
            args.extend(["--clean".into(), "--if-exists".into()]);
        }
        if options.create {
            args.push("--create".into());
        }
    }
    args.extend(options.compression.map(|level| arg("compress", level.to_string())));
    args.extend(options.jobs.filter(|jobs| *jobs > 1).map(|jobs| arg("jobs", jobs.to_string())));
    args.push("--verbose".into());
    Ok(Invocation { program: tool.path.clone(), args, env, env_remove: PG_ENV_REMOVE })
}

// ---- pg_restore ------------------------------------------------------------------------------------------

#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct PgRestoreOptions {
    /// DROP the objects first, with IF EXISTS.
    pub clean: bool,
    pub no_owner: bool,
    pub single_transaction: bool,
    pub jobs: Option<u32>,
    /// The archive's table of contents with the entries not wanted commented out (`--use-list`).
    pub list_file: Option<PathBuf>,
}

impl PgRestoreOptions {
    pub fn validate(&self) -> Result<(), OptionsError> {
        match self.jobs {
            Some(jobs) if !(1..=MAX_JOBS).contains(&jobs) => Err(OptionsError::JobsRange(jobs)),
            Some(jobs) if jobs > 1 && self.single_transaction => Err(OptionsError::JobsWithSingleTransaction),
            _ => Ok(()),
        }
    }
}

/// `pg_restore` of `archive` into `database`.
pub fn pg_restore(tool: &Found, endpoint: &Endpoint, database: &str, passfile: Option<&Path>, options: &PgRestoreOptions, archive: &Path) -> Result<Invocation, OptionsError> {
    options.validate()?;
    let mut args = Vec::new();
    let mut env = Vec::new();
    pg_connection(endpoint, database, passfile, &mut args, &mut env);
    if options.clean {
        args.extend(["--clean".into(), "--if-exists".into()]);
    }
    if options.no_owner {
        args.push("--no-owner".into());
    }
    if options.single_transaction {
        args.push("--single-transaction".into());
    }
    args.extend(options.jobs.filter(|jobs| *jobs > 1).map(|jobs| arg("jobs", jobs.to_string())));
    args.extend(options.list_file.as_ref().map(|list| arg("use-list", list)));
    args.push("--verbose".into());
    args.push("--".into());
    args.push(archive.into());
    Ok(Invocation { program: tool.path.clone(), args, env, env_remove: PG_ENV_REMOVE })
}

/// `pg_restore --list`: the table of contents of `archive`. It does not connect.
pub fn pg_restore_list(tool: &Found, archive: &Path) -> Invocation {
    Invocation { program: tool.path.clone(), args: vec!["--list".into(), "--".into(), archive.into()], env: Vec::new(), env_remove: PG_ENV_REMOVE }
}

// ---- mysqldump / mariadb-dump -----------------------------------------------------------------------------

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum MysqlScope {
    /// Every table, view and routine of the database.
    Database,
    Tables(Vec<String>),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MysqlDumpOptions {
    pub scope: MysqlScope,
    /// One transaction with a consistent snapshot, so the tables are read as of one moment without locking them.
    pub single_transaction: bool,
    pub routines: bool,
    pub triggers: bool,
    pub events: bool,
    pub no_data: bool,
    pub add_drop_table: bool,
    pub extended_insert: bool,
    /// CREATE DATABASE and USE statements (whole database only).
    pub create_database: bool,
    /// DROP DATABASE before it (needs `create_database`).
    pub add_drop_database: bool,
}

impl Default for MysqlDumpOptions {
    fn default() -> Self {
        Self { scope: MysqlScope::Database, single_transaction: true, routines: true, triggers: true, events: true, no_data: false, add_drop_table: true, extended_insert: true, create_database: true, add_drop_database: false }
    }
}

impl MysqlDumpOptions {
    pub fn validate(&self) -> Result<(), OptionsError> {
        match &self.scope {
            MysqlScope::Tables(tables) if tables.is_empty() => Err(OptionsError::NothingSelected),
            _ => Ok(()),
        }
    }
}

/// `mysqldump` / `mariadb-dump` of `database` into `output`. `defaults_file` holds the account and the TLS settings; as
/// `--defaults-file` and the first argument, as the client requires, it is the only option file read, so the user's `~/.my.cnf`
/// and `/etc/my.cnf` cannot change where the tool connects, as whom or with which TLS mode. `~/.mylogin.cnf`, which MySQL clients
/// read after it, is pointed at a file that does not exist. `server` is the kind of server dumped and `server_major` its major
/// version, when known.
pub fn mysql_dump(tool: &Found, server: ConnectionKind, server_major: Option<u32>, endpoint: &Endpoint, defaults_file: &Path, options: &MysqlDumpOptions, output: &Path) -> Result<Invocation, OptionsError> {
    options.validate()?;
    let mut args: Vec<OsString> = vec![arg("defaults-file", defaults_file)];
    if options.single_transaction {
        args.push("--single-transaction".into());
    }
    if options.routines {
        args.push("--routines".into());
    }
    if options.events {
        args.push("--events".into());
    }
    if !options.triggers {
        args.push("--skip-triggers".into());
    }
    if options.no_data {
        args.push("--no-data".into());
    }
    if !options.add_drop_table {
        args.push("--skip-add-drop-table".into());
    }
    if !options.extended_insert {
        args.push("--skip-extended-insert".into());
    }
    // Binary values as hexadecimal: the script stays text in any character set.
    args.push("--hex-blob".into());
    // MySQL 8's client asks the server for column statistics (a table MySQL 8.0 added): MariaDB and MySQL before 8 have none and refuse.
    // Only a client that knows the option (8.0.2 and later) is given it.
    let knows_option = tool.version.family == Family::Mysql && tool.version.version >= Version { major: 8, minor: 0, patch: 2 };
    let server_lacks_statistics = server == ConnectionKind::Mariadb || (server == ConnectionKind::Mysql && server_major.is_some_and(|major| major < 8));
    if knows_option && server_lacks_statistics {
        args.push("--column-statistics=0".into());
    }
    args.push("--verbose".into());
    args.push(arg("result-file", output));
    let databases = matches!(options.scope, MysqlScope::Database) && options.create_database;
    if databases {
        if options.add_drop_database {
            // Right after the option file, which stays first.
            args.insert(1, "--add-drop-database".into());
        }
        // An option: it must come before the `--` that ends the options.
        args.push("--databases".into());
    }
    args.push("--".into());
    args.push((&endpoint.database).into());
    if let MysqlScope::Tables(tables) = &options.scope {
        args.extend(tables.iter().map(OsString::from));
    }
    // The login file MySQL clients read last, which could override anything above: one that does not exist.
    let no_login_file = defaults_file.parent().unwrap_or_else(|| Path::new("")).join("no-login.cnf");
    Ok(Invocation { program: tool.path.clone(), args, env: vec![("MYSQL_TEST_LOGIN_FILE", no_login_file.into())], env_remove: MYSQL_ENV_REMOVE })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::backup::tools::{Tool, ToolVersion, Version};

    fn found(tool: Tool, family: Family, major: u32) -> Found {
        Found { tool, path: PathBuf::from(format!("/opt/bin/{}", tool.program())), version: ToolVersion { family, version: Version { major, minor: 0, patch: 36 } } }
    }

    fn pg_endpoint() -> Endpoint {
        Endpoint::parse(ConnectionKind::Postgres, "postgres://ann:hunter2-pass@db.example.com:6543/sales?sslmode=verify-full&sslrootcert=/ca.pem&sslcert=/c.pem&sslkey=/k.pem&hostaddr=127.0.0.1").unwrap()
    }

    fn text(invocation: &Invocation) -> Vec<String> {
        invocation.args.iter().map(|arg| arg.to_string_lossy().into_owned()).collect()
    }

    fn env(invocation: &Invocation, name: &str) -> Option<String> {
        invocation.env.iter().find(|(key, _)| *key == name).map(|(_, value)| value.to_string_lossy().into_owned())
    }

    #[test]
    fn pg_dump_gets_the_connection_the_format_and_every_option_as_separate_arguments() {
        let options = PgDumpOptions {
            format: PgFormat::Directory,
            content: Content::SchemaOnly,
            scope: PgScope::Tables(vec![("public".into(), "Orders".into()), ("we\"ird".into(), "a.b".into())]),
            no_owner: true,
            no_privileges: true,
            compression: Some(5),
            jobs: Some(4),
            ..PgDumpOptions::default()
        };
        let invocation = pg_dump(&found(Tool::PgDump, Family::Postgres, 17), &pg_endpoint(), "sales", Some(Path::new("/tmp/p/pgpass")), &options, Path::new("/out/dir")).unwrap();
        assert_eq!(
            text(&invocation),
            [
                "--host=db.example.com",
                "--port=6543",
                "--username=ann",
                "--no-password",
                "--dbname=dbname='sales'",
                "--format=d",
                "--file=/out/dir",
                "--schema-only",
                "--table=\"public\".\"Orders\"",
                "--table=\"we\"\"ird\".\"a.b\"",
                "--no-owner",
                "--no-privileges",
                "--compress=5",
                "--jobs=4",
                "--verbose",
            ]
        );
        assert_eq!(env(&invocation, "PGPASSFILE").as_deref(), Some("/tmp/p/pgpass"));
        assert_eq!(env(&invocation, "PGSSLMODE").as_deref(), Some("verify-full"));
        assert_eq!(env(&invocation, "PGHOSTADDR").as_deref(), Some("127.0.0.1"));
        assert_eq!((env(&invocation, "PGSSLROOTCERT").as_deref(), env(&invocation, "PGSSLCERT").as_deref(), env(&invocation, "PGSSLKEY").as_deref()), (Some("/ca.pem"), Some("/c.pem"), Some("/k.pem")));
        assert!(invocation.env_remove.contains(&"PGPASSWORD"), "a PGPASSWORD of the user's shell must not win over the connection's");
    }

    #[test]
    fn no_password_is_on_any_command_line_or_in_the_environment() {
        let tool = found(Tool::PgDump, Family::Postgres, 17);
        let endpoint = pg_endpoint();
        let invocation = pg_dump(&tool, &endpoint, "sales", Some(Path::new("/tmp/p/pgpass")), &PgDumpOptions::default(), Path::new("/out/a.sql")).unwrap();
        let restore = pg_restore(&tool, &endpoint, "sales", Some(Path::new("/tmp/p/pgpass")), &PgRestoreOptions::default(), Path::new("/in/a.dump")).unwrap();
        let mysql_endpoint = Endpoint::parse(ConnectionKind::Mysql, "mysql://root:hunter2-pass@127.0.0.1:3306/shop").unwrap();
        let mysql = mysql_dump(&found(Tool::MysqlDump, Family::Mysql, 8), ConnectionKind::Mysql, Some(8), &mysql_endpoint, Path::new("/tmp/p/defaults"), &MysqlDumpOptions::default(), Path::new("/out/a.sql")).unwrap();
        for invocation in [invocation, restore, mysql] {
            let everything = format!("{} {:?} {:?}", invocation.command_line(), invocation.args, invocation.env);
            assert!(!everything.contains("hunter2"), "{everything}");
        }
    }

    #[test]
    fn plain_dumps_take_clean_and_create_and_archives_leave_them_to_the_restore() {
        let tool = found(Tool::PgDump, Family::Postgres, 17);
        let mut options = PgDumpOptions { clean: true, create: true, ..PgDumpOptions::default() };
        let plain = pg_dump(&tool, &pg_endpoint(), "sales", None, &options, Path::new("/o.sql")).unwrap();
        assert!(text(&plain).windows(3).any(|w| w == ["--clean", "--if-exists", "--create"]), "{:?}", text(&plain));
        options.format = PgFormat::Custom;
        let custom = pg_dump(&tool, &pg_endpoint(), "sales", None, &options, Path::new("/o.dump")).unwrap();
        assert!(!text(&custom).iter().any(|arg| arg == "--clean" || arg == "--create"), "pg_dump ignores them for an archive: {:?}", text(&custom));
        assert!(plain.env.iter().all(|(name, _)| *name != "PGPASSFILE"), "no password, no passfile");
    }

    #[test]
    fn options_that_cannot_work_together_are_refused_before_anything_runs() {
        let tool = found(Tool::PgDump, Family::Postgres, 17);
        let run = |options: PgDumpOptions| pg_dump(&tool, &pg_endpoint(), "sales", None, &options, Path::new("/o")).unwrap_err();
        assert_eq!(run(PgDumpOptions { compression: Some(10), format: PgFormat::Custom, ..PgDumpOptions::default() }), OptionsError::CompressionRange(10));
        assert_eq!(run(PgDumpOptions { compression: Some(3), ..PgDumpOptions::default() }), OptionsError::CompressionNeedsArchive);
        assert_eq!(run(PgDumpOptions { jobs: Some(4), format: PgFormat::Custom, ..PgDumpOptions::default() }), OptionsError::JobsNeedDirectory);
        assert_eq!(run(PgDumpOptions { jobs: Some(0), format: PgFormat::Directory, ..PgDumpOptions::default() }), OptionsError::JobsRange(0));
        assert_eq!(run(PgDumpOptions { clean: true, content: Content::DataOnly, ..PgDumpOptions::default() }), OptionsError::CleanWithDataOnly);
        assert_eq!(run(PgDumpOptions { scope: PgScope::Tables(Vec::new()), ..PgDumpOptions::default() }), OptionsError::NothingSelected);
        let restore = |options: PgRestoreOptions| pg_restore(&tool, &pg_endpoint(), "sales", None, &options, Path::new("/a")).unwrap_err();
        assert_eq!(restore(PgRestoreOptions { jobs: Some(4), single_transaction: true, ..PgRestoreOptions::default() }), OptionsError::JobsWithSingleTransaction);
        assert_eq!(restore(PgRestoreOptions { jobs: Some(65), ..PgRestoreOptions::default() }), OptionsError::JobsRange(65));
    }

    #[test]
    fn pg_restore_gets_its_options_the_selection_list_and_the_archive_last() {
        let options = PgRestoreOptions { clean: true, no_owner: true, single_transaction: true, jobs: None, list_file: Some(PathBuf::from("/tmp/p/toc.list")) };
        let invocation = pg_restore(&found(Tool::PgRestore, Family::Postgres, 17), &pg_endpoint(), "target", None, &options, Path::new("/in/a.dump")).unwrap();
        assert_eq!(
            text(&invocation),
            ["--host=db.example.com", "--port=6543", "--username=ann", "--no-password", "--dbname=dbname='target'", "--clean", "--if-exists", "--no-owner", "--single-transaction", "--use-list=/tmp/p/toc.list", "--verbose", "--", "/in/a.dump"]
        );
        let jobs = pg_restore(&found(Tool::PgRestore, Family::Postgres, 17), &pg_endpoint(), "target", None, &PgRestoreOptions { jobs: Some(3), ..PgRestoreOptions::default() }, Path::new("/in/dir")).unwrap();
        assert!(text(&jobs).contains(&"--jobs=3".to_owned()));
        let list = pg_restore_list(&found(Tool::PgRestore, Family::Postgres, 17), Path::new("/in/a.dump"));
        assert_eq!(text(&list), ["--list", "--", "/in/a.dump"]);
    }

    fn mysql_endpoint() -> Endpoint {
        Endpoint::parse(ConnectionKind::Mysql, "mysql://root:hunter2@127.0.0.1:3306/shop").unwrap()
    }

    #[test]
    fn mysqldump_has_the_option_file_first_and_each_option_where_it_belongs() {
        let tool = found(Tool::MysqlDump, Family::Mysql, 8);
        let defaults = Path::new("/tmp/p/defaults");
        let invocation = mysql_dump(&tool, ConnectionKind::Mysql, Some(8), &mysql_endpoint(), defaults, &MysqlDumpOptions::default(), Path::new("/out/shop.sql")).unwrap();
        assert_eq!(text(&invocation), ["--defaults-file=/tmp/p/defaults", "--single-transaction", "--routines", "--events", "--hex-blob", "--verbose", "--result-file=/out/shop.sql", "--databases", "--", "shop"]);

        let options = MysqlDumpOptions {
            scope: MysqlScope::Tables(vec!["orders".into(), "-odd name".into()]),
            single_transaction: false,
            routines: true,
            triggers: false,
            events: true,
            no_data: true,
            add_drop_table: false,
            extended_insert: false,
            create_database: true,
            add_drop_database: true,
        };
        let invocation = mysql_dump(&tool, ConnectionKind::Mysql, Some(8), &mysql_endpoint(), defaults, &options, Path::new("/out/shop.sql")).unwrap();
        assert_eq!(
            text(&invocation),
            [
                "--defaults-file=/tmp/p/defaults",
                "--routines",
                "--events",
                "--skip-triggers",
                "--no-data",
                "--skip-add-drop-table",
                "--skip-extended-insert",
                "--hex-blob",
                "--verbose",
                "--result-file=/out/shop.sql",
                "--",
                "shop",
                "orders",
                "-odd name"
            ]
        );
        assert_eq!(text(&invocation)[0], "--defaults-file=/tmp/p/defaults", "the option file is the first argument");

        let drop_database = MysqlDumpOptions { add_drop_database: true, ..MysqlDumpOptions::default() };
        let invocation = mysql_dump(&tool, ConnectionKind::Mysql, Some(8), &mysql_endpoint(), defaults, &drop_database, Path::new("/o.sql")).unwrap();
        assert_eq!(text(&invocation)[..2], ["--defaults-file=/tmp/p/defaults", "--add-drop-database"]);
        let no_create = MysqlDumpOptions { create_database: false, ..MysqlDumpOptions::default() };
        let invocation = mysql_dump(&tool, ConnectionKind::Mysql, Some(8), &mysql_endpoint(), defaults, &no_create, Path::new("/o.sql")).unwrap();
        assert_eq!(text(&invocation).last().map(String::as_str), Some("shop"));
        assert!(!text(&invocation).contains(&"--databases".to_owned()));
        let databases = text(&invocation);
        let with = text(&mysql_dump(&tool, ConnectionKind::Mysql, Some(8), &mysql_endpoint(), defaults, &MysqlDumpOptions::default(), Path::new("/o.sql")).unwrap());
        assert!(with.iter().position(|arg| arg == "--databases") < with.iter().position(|arg| arg == "--"), "--databases is an option and comes before the end of the options: {with:?}");
        let _ = databases;
        assert_eq!(mysql_dump(&tool, ConnectionKind::Mysql, Some(8), &mysql_endpoint(), defaults, &MysqlDumpOptions { scope: MysqlScope::Tables(Vec::new()), ..MysqlDumpOptions::default() }, Path::new("/o.sql")).unwrap_err(), OptionsError::NothingSelected);
    }

    #[test]
    fn a_mysql_8_client_against_mariadb_skips_the_column_statistics_mariadb_does_not_have() {
        let defaults = Path::new("/d");
        let run = |tool: Found, server| text(&mysql_dump(&tool, server, Some(8), &mysql_endpoint(), defaults, &MysqlDumpOptions::default(), Path::new("/o.sql")).unwrap());
        assert!(run(found(Tool::MysqlDump, Family::Mysql, 8), ConnectionKind::Mariadb).contains(&"--column-statistics=0".to_owned()));
        assert!(!run(found(Tool::MysqlDump, Family::Mysql, 8), ConnectionKind::Mysql).contains(&"--column-statistics=0".to_owned()));
        assert!(!run(found(Tool::MariadbDump, Family::Mariadb, 11), ConnectionKind::Mariadb).contains(&"--column-statistics=0".to_owned()));
    }

    #[test]
    fn the_command_line_of_the_log_quotes_what_has_blanks() {
        let invocation = Invocation { program: PathBuf::from("/opt/bin/pg_dump"), args: vec!["--file=/a b/c.sql".into(), "--verbose".into()], env: Vec::new(), env_remove: &[] };
        assert_eq!(invocation.command_line(), "pg_dump \"--file=/a b/c.sql\" --verbose");
    }

    #[test]
    fn a_database_name_is_never_read_as_a_connection_string() {
        let tool = found(Tool::PgDump, Family::Postgres, 17);
        let endpoint = pg_endpoint();
        for (name, expected) in [
            ("sales", "--dbname=dbname='sales'"),
            ("realdb sslmode=disable", "--dbname=dbname='realdb sslmode=disable'"),
            ("a=b", "--dbname=dbname='a=b'"),
            ("postgresql://evil@elsewhere/db", "--dbname=dbname='postgresql://evil@elsewhere/db'"),
            ("it's a \\ name", "--dbname=dbname='it\\'s a \\\\ name'"),
        ] {
            let dump = pg_dump(&tool, &endpoint, name, None, &PgDumpOptions::default(), Path::new("/o.sql")).unwrap();
            let restore = pg_restore(&found(Tool::PgRestore, Family::Postgres, 17), &endpoint, name, None, &PgRestoreOptions::default(), Path::new("/a.dump")).unwrap();
            for invocation in [dump, restore] {
                let args = text(&invocation);
                assert!(args.contains(&expected.to_owned()), "{name}: {args:?}");
                assert_eq!(args.iter().filter(|arg| arg.starts_with("--dbname=")).count(), 1);
            }
        }
    }

    #[test]
    fn the_option_file_of_a_mysql_client_is_the_only_one_read_and_the_login_file_is_pointed_away() {
        let tool = found(Tool::MysqlDump, Family::Mysql, 8);
        let invocation = mysql_dump(&tool, ConnectionKind::Mysql, Some(8), &mysql_endpoint(), Path::new("/tmp/p/client.cnf"), &MysqlDumpOptions::default(), Path::new("/o.sql")).unwrap();
        assert_eq!(text(&invocation)[0], "--defaults-file=/tmp/p/client.cnf", "--defaults-extra-file would be read before ~/.my.cnf, which could override it");
        assert!(!text(&invocation).iter().any(|arg| arg.contains("defaults-extra-file")));
        assert_eq!(env(&invocation, "MYSQL_TEST_LOGIN_FILE").as_deref(), Some("/tmp/p/no-login.cnf"), "~/.mylogin.cnf is read last and could override the host and the TLS mode");
        for variable in ["MYSQL_HOME", "MYSQL_PWD", "MYSQL_HOST", "MYSQL_GROUP_SUFFIX"] {
            assert!(invocation.env_remove.contains(&variable), "{variable}");
        }
    }

    #[test]
    fn libpq_variables_that_change_trust_or_the_connection_are_cleaned_from_the_environment() {
        let invocation = pg_dump(&found(Tool::PgDump, Family::Postgres, 17), &pg_endpoint(), "sales", None, &PgDumpOptions::default(), Path::new("/o.sql")).unwrap();
        for variable in ["PGGSSENCMODE", "PGSSLNEGOTIATION", "PGSSLCERTMODE", "PGREQUIREAUTH", "PGCHANNELBINDING", "PGTARGETSESSIONATTRS", "PGSSLCRLDIR", "PGSSLCRL", "PGSSLMINPROTOCOLVERSION", "PGSSLMAXPROTOCOLVERSION", "PGSERVICE", "PGSERVICEFILE", "PGSYSCONFDIR"] {
            assert!(invocation.env_remove.contains(&variable), "{variable}");
        }
        assert!(!invocation.env_remove.contains(&"PGPASSCOMMAND"), "not a libpq variable");
    }

    #[test]
    fn column_statistics_are_skipped_for_every_server_that_has_none_and_only_for_clients_that_know_the_option() {
        let args = |tool: Found, server, major| text(&mysql_dump(&tool, server, major, &mysql_endpoint(), Path::new("/d"), &MysqlDumpOptions::default(), Path::new("/o.sql")).unwrap());
        let flag = "--column-statistics=0".to_owned();
        assert!(args(found(Tool::MysqlDump, Family::Mysql, 8), ConnectionKind::Mysql, Some(5)).contains(&flag), "a MySQL 5.7 server has no column statistics");
        assert!(!args(found(Tool::MysqlDump, Family::Mysql, 8), ConnectionKind::Mysql, Some(8)).contains(&flag));
        assert!(!args(found(Tool::MysqlDump, Family::Mysql, 8), ConnectionKind::Mysql, None).contains(&flag), "unknown server: the client's default");
        let old_client = Found { version: ToolVersion { family: Family::Mysql, version: Version { major: 5, minor: 7, patch: 40 } }, ..found(Tool::MysqlDump, Family::Mysql, 8) };
        assert!(!args(old_client, ConnectionKind::Mariadb, Some(11)).contains(&flag), "a 5.7 client does not know the option");
    }
}
