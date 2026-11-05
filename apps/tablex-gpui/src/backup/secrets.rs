//! The private files a native tool reads its password from, so that no password is ever on a command line
//! (every user of the machine can read those) or in the tool's environment.
//!
//! The files live in a directory only this user can enter (`0700` on Unix; the per-user temporary folder
//! on Windows, whose access list is the user's), each created `0600`. The directory and everything in it
//! is deleted when the [`PrivateDir`] is dropped: when the run ends, is cancelled, fails or panics.

use std::collections::HashSet;
use std::io::Write as _;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use engine_net::Secret;

use super::endpoint::Endpoint;
use super::tools::Family;
use crate::connection_tls::{TlsMode, TlsSettings};

/// The private directories of the runs that are alive, so that the application can remove them all at once when it quits
/// (a quit gives the workers less time than they may need to do it themselves). Once [`remove_all`](Self::remove_all) ran the
/// registry is closed: no directory and no file can be made in it any more, so a worker that was still connecting leaves nothing.
#[derive(Debug, Clone, Default)]
pub struct PrivateDirs(Arc<Mutex<Registry>>);

#[derive(Debug, Default)]
struct Registry {
    live: HashSet<PathBuf>,
    closed: bool,
}

/// The application is quitting: the registry of private directories is closed.
fn closed_error() -> std::io::Error {
    std::io::Error::other("the application is quitting")
}

impl PrivateDirs {
    fn lock(&self) -> std::sync::MutexGuard<'_, Registry> {
        self.0.lock().unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    /// A new private directory, registered until it is dropped. Made under the lock, so that [`remove_all`](Self::remove_all)
    /// either sees it or comes before it and refuses it.
    pub fn create(&self) -> std::io::Result<PrivateDir> {
        let mut registry = self.lock();
        if registry.closed {
            return Err(closed_error());
        }
        let mut dir = PrivateDir::create()?;
        registry.live.insert(dir.path.clone());
        dir.registry = Some(self.clone());
        Ok(dir)
    }

    /// Removes every directory that is alive, with its files, on the calling thread, and closes the registry; errors are ignored.
    /// The tools that use them are being stopped: they read their password once, when they connect. A directory that is dropped
    /// later finds it gone and does not mind. Held under the lock, so that no file is being written while its directory goes.
    pub fn remove_all(&self) {
        let mut registry = self.lock();
        registry.closed = true;
        for path in registry.live.drain() {
            let _ = std::fs::remove_dir_all(path);
        }
    }

    #[cfg(test)]
    pub fn len(&self) -> usize {
        self.lock().live.len()
    }
}

/// A directory of the current user's, removed with its files when it is dropped.
#[derive(Debug)]
pub struct PrivateDir {
    path: PathBuf,
    registry: Option<PrivateDirs>,
}

impl PrivateDir {
    /// A new, empty directory in the temporary folder, named at random.
    pub fn create() -> std::io::Result<Self> {
        Self::create_in(&std::env::temp_dir())
    }

    pub fn create_in(parent: &Path) -> std::io::Result<Self> {
        let path = parent.join(format!("tablex-tool-{}", uuid::Uuid::new_v4().to_simple()));
        let mut builder = std::fs::DirBuilder::new();
        #[cfg(unix)]
        std::os::unix::fs::DirBuilderExt::mode(&mut builder, 0o700);
        builder.create(&path)?;
        Ok(Self { path, registry: None })
    }

    /// Writes `contents` to the new file `name`, readable by this user only.
    pub fn write_file(&self, name: &str, contents: &Secret) -> std::io::Result<PathBuf> {
        let registry = self.registry.as_ref().map(PrivateDirs::lock);
        if registry.as_ref().is_some_and(|registry| registry.closed) {
            return Err(closed_error());
        }
        let path = self.path.join(name);
        let mut options = std::fs::OpenOptions::new();
        options.write(true).create_new(true);
        #[cfg(unix)]
        std::os::unix::fs::OpenOptionsExt::mode(&mut options, 0o600);
        let mut file = options.open(&path)?;
        file.write_all(contents.expose().as_bytes())?;
        file.sync_all()?;
        Ok(path)
    }

    #[cfg(test)]
    pub fn path(&self) -> &Path {
        &self.path
    }
}

impl Drop for PrivateDir {
    fn drop(&mut self) {
        if let Some(registry) = &self.registry {
            registry.lock().live.remove(&self.path);
        }
        // Emptied first: a file that cannot be removed should not hide behind a directory that can.
        if let Ok(entries) = std::fs::read_dir(&self.path) {
            for entry in entries.flatten() {
                let _ = std::fs::remove_file(entry.path());
            }
        }
        let _ = std::fs::remove_dir(&self.path);
    }
}

/// Why the password file cannot be written.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PassfileError {
    /// A line break in the user or the password would end the line of the password file: it cannot be written, and no
    /// other line is made out of it.
    LineBreak(&'static str),
}

impl std::fmt::Display for PassfileError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::LineBreak(field) => write!(f, "The {field} of this connection holds a line break, which PostgreSQL's password file cannot carry. Native tools cannot be used with it."),
        }
    }
}

impl std::error::Error for PassfileError {}

/// One field of a `.pgpass` line. `\` and `:` inside a field are escaped with `\`.
fn pgpass_field(field: &str) -> String {
    field.replace('\\', "\\\\").replace(':', "\\:")
}

/// The `.pgpass` content that gives `endpoint`'s password to libpq for any database of that server and
/// user, or `None` when there is no password to give. A user or a password with a line break is refused.
pub fn pgpass(endpoint: &Endpoint) -> Result<Option<Secret>, PassfileError> {
    let Some(password) = endpoint.password_text() else { return Ok(None) };
    let has_break = |text: &str| text.contains(['\n', '\r']);
    if endpoint.user.as_deref().is_some_and(has_break) {
        return Err(PassfileError::LineBreak("user name"));
    }
    if has_break(password) {
        return Err(PassfileError::LineBreak("password"));
    }
    // A socket directory is not a host name the file can be matched on: libpq matches `localhost` for a socket.
    let host = if endpoint.host.is_empty() || endpoint.host.starts_with('/') { "*".to_owned() } else { pgpass_field(&endpoint.host) };
    let port = endpoint.port.map_or_else(|| "*".to_owned(), |port| port.to_string());
    let user = endpoint.user.as_deref().map_or_else(|| "*".to_owned(), pgpass_field);
    Ok(Some(Secret::new(format!("{host}:{port}:*:{user}:{}\n", pgpass_field(password)))))
}

/// A value of an option file, quoted: inside double quotes `\` and `"` are escaped, and a line break
/// cannot end the line.
fn option_value(value: &str) -> String {
    let mut quoted = String::with_capacity(value.len() + 2);
    quoted.push('"');
    for ch in value.chars() {
        match ch {
            '\\' => quoted.push_str("\\\\"),
            '"' => quoted.push_str("\\\""),
            '\n' => quoted.push_str("\\n"),
            '\r' => quoted.push_str("\\r"),
            '\t' => quoted.push_str("\\t"),
            other => quoted.push(other),
        }
    }
    quoted.push('"');
    quoted
}

/// The TLS options of the `[client]` group, in the words of the client that reads them: MySQL's `ssl-mode`,
/// MariaDB's `ssl` / `skip-ssl` / `ssl-verify-server-cert`. Files are given whenever they are set.
fn tls_lines(tls: &TlsSettings, family: Family, lines: &mut Vec<String>) {
    match family {
        Family::Mariadb => match tls.mode {
            TlsMode::Disable => lines.push("skip-ssl".to_owned()),
            TlsMode::Require | TlsMode::VerifyCa => lines.push("ssl".to_owned()),
            TlsMode::VerifyFull => lines.extend(["ssl".to_owned(), "ssl-verify-server-cert".to_owned()]),
            TlsMode::Default | TlsMode::Allow | TlsMode::Prefer => {}
        },
        _ => {
            let mode = match tls.mode {
                TlsMode::Disable => Some("DISABLED"),
                TlsMode::Prefer => Some("PREFERRED"),
                TlsMode::Require => Some("REQUIRED"),
                TlsMode::VerifyCa => Some("VERIFY_CA"),
                TlsMode::VerifyFull => Some("VERIFY_IDENTITY"),
                // The client has no clear-text-first mode: its own default (prefer) is the nearest.
                TlsMode::Default | TlsMode::Allow => None,
            };
            lines.extend(mode.map(|mode| format!("ssl-mode={mode}")));
        }
    }
    for (name, file) in [("ssl-ca", &tls.ca_file), ("ssl-cert", &tls.client_cert), ("ssl-key", &tls.client_key)] {
        if !file.trim().is_empty() {
            lines.push(format!("{name}={}", option_value(file.trim())));
        }
    }
}

/// The option file for `--defaults-file`: the account, the server and the TLS settings of `endpoint`.
/// `family` is the client's (`Mysql` or `Mariadb`).
pub fn mysql_defaults(endpoint: &Endpoint, family: Family) -> Secret {
    let mut lines = vec!["[client]".to_owned()];
    if !endpoint.host.is_empty() {
        lines.push(format!("host={}", option_value(&endpoint.host)));
        // `localhost` would otherwise mean the local socket, and the port would be ignored.
        lines.push("protocol=tcp".to_owned());
    }
    lines.extend(endpoint.port.map(|port| format!("port={port}")));
    lines.extend(endpoint.user.as_deref().map(|user| format!("user={}", option_value(user))));
    lines.extend(endpoint.password_text().map(|password| format!("password={}", option_value(password))));
    tls_lines(&endpoint.tls, family, &mut lines);
    lines.push(String::new());
    Secret::new(lines.join("\n"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::connections::ConnectionKind;

    fn endpoint(kind: ConnectionKind, uri: &str) -> Endpoint {
        Endpoint::parse(kind, uri).unwrap()
    }

    #[test]
    fn a_passfile_line_escapes_what_libpq_reads_specially() {
        let content = pgpass(&endpoint(ConnectionKind::Postgres, "postgres://ann:p%3Ass%5Cw%40rd@db.example.com:6543/sales")).unwrap().unwrap();
        assert_eq!(content.expose(), "db.example.com:6543:*:ann:p\\:ss\\\\w@rd\n");
        assert!(pgpass(&endpoint(ConnectionKind::Postgres, "postgres://ann@h/db")).unwrap().is_none(), "no password, no file");
        // A socket directory is matched with `*`.
        let mut on_socket = endpoint(ConnectionKind::Postgres, "postgres://ann:pw@h/db");
        on_socket.host = "/var/run/postgresql".into();
        let socket = pgpass(&on_socket).unwrap().unwrap();
        assert_eq!(socket.expose(), "*:*:*:ann:pw\n");
    }

    #[test]
    fn a_line_break_in_the_user_or_the_password_is_refused_not_turned_into_another_line() {
        let refused = |uri: &str| pgpass(&endpoint(ConnectionKind::Postgres, uri)).unwrap_err();
        assert_eq!(refused("postgres://ann:pw%0Aevil.host:5432:*:root:x@h/db"), PassfileError::LineBreak("password"));
        assert_eq!(refused("postgres://an%0Dn:pw@h/db"), PassfileError::LineBreak("user name"));
    }

    #[test]
    fn a_defaults_file_quotes_the_account_and_keeps_the_password_out_of_other_places() {
        let endpoint = endpoint(ConnectionKind::Mysql, "mysql://ro%22ot:pa%22ss%5Cwo%0Ard@localhost:3307/shop?ssl-mode=VERIFY_CA&ssl-ca=/etc/ca.pem");
        let content = mysql_defaults(&endpoint, Family::Mysql);
        let text = content.expose();
        assert_eq!(text, "[client]\nhost=\"localhost\"\nprotocol=tcp\nport=3307\nuser=\"ro\\\"ot\"\npassword=\"pa\\\"ss\\\\wo\\nrd\"\nssl-mode=VERIFY_CA\nssl-ca=\"/etc/ca.pem\"\n");
        assert!(!format!("{content:?}").contains("pa"), "the debug text of the content is hidden");
    }

    #[test]
    fn mariadb_clients_get_their_own_tls_words() {
        let tls = |uri: &str| mysql_defaults(&endpoint(ConnectionKind::Mariadb, uri), Family::Mariadb).expose().to_owned();
        assert!(tls("mariadb://u@h/db?ssl-mode=DISABLED").contains("\nskip-ssl\n"));
        assert!(tls("mariadb://u@h/db?ssl-mode=REQUIRED").contains("\nssl\n"));
        let verify = tls("mariadb://u@h/db?ssl-mode=VERIFY_IDENTITY&ssl-ca=/ca.pem&ssl-cert=/c.pem&ssl-key=/k.pem");
        for line in ["\nssl\n", "\nssl-verify-server-cert\n", "\nssl-ca=\"/ca.pem\"\n", "\nssl-cert=\"/c.pem\"\n", "\nssl-key=\"/k.pem\"\n"] {
            assert!(verify.contains(line), "{line:?} in {verify}");
        }
        assert!(!verify.contains("ssl-mode"), "MariaDB clients do not know ssl-mode");
    }

    #[cfg(unix)]
    #[test]
    fn the_directory_and_its_files_are_private_and_removed_with_it() {
        use std::os::unix::fs::PermissionsExt as _;
        let parent = tempfile::tempdir().unwrap();
        let dir = PrivateDir::create_in(parent.path()).unwrap();
        let file = dir.write_file("pgpass", &Secret::new("secret-content")).unwrap();
        assert_eq!(std::fs::metadata(dir.path()).unwrap().permissions().mode() & 0o777, 0o700);
        assert_eq!(std::fs::metadata(&file).unwrap().permissions().mode() & 0o777, 0o600);
        assert_eq!(std::fs::read_to_string(&file).unwrap(), "secret-content");
        assert!(dir.write_file("pgpass", &Secret::new("again")).is_err(), "a file is never overwritten");
        let path = dir.path().to_owned();
        drop(dir);
        assert!(!path.exists() && !file.exists());
    }

    #[cfg(unix)]
    #[test]
    fn the_directory_is_removed_when_the_run_panics() {
        let parent = tempfile::tempdir().unwrap();
        let seen = std::sync::Mutex::new(None);
        let outcome = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            let dir = PrivateDir::create_in(parent.path()).unwrap();
            dir.write_file("defaults", &Secret::new("password=x")).unwrap();
            *seen.lock().unwrap() = Some(dir.path().to_owned());
            panic!("the run failed");
        }));
        assert!(outcome.is_err());
        let path = seen.lock().unwrap().take().unwrap();
        assert!(!path.exists(), "the private directory outlived the panic");
    }

    #[test]
    fn every_live_directory_can_be_removed_at_once_and_a_later_drop_does_not_mind() {
        let registry = PrivateDirs::default();
        let (first, second) = (registry.create().unwrap(), registry.create().unwrap());
        let file = first.write_file("pgpass", &Secret::new("secret")).unwrap();
        assert_eq!(registry.len(), 2);
        registry.remove_all();
        assert!(!file.exists() && !first.path().exists() && !second.path().exists());
        assert_eq!(registry.len(), 0);
        // The registry is closed: a worker that was still connecting can make neither a directory nor a file.
        assert!(first.write_file("client.cnf", &Secret::new("secret")).is_err());
        assert!(registry.create().is_err());
        drop((first, second));
        assert_eq!(registry.len(), 0);
    }

    #[test]
    fn a_dropped_directory_leaves_the_registry_and_its_files_go_with_it() {
        let registry = PrivateDirs::default();
        let dir = registry.create().unwrap();
        let file = dir.write_file("pgpass", &Secret::new("secret")).unwrap();
        let path = dir.path().to_owned();
        assert_eq!(registry.len(), 1);
        drop(dir);
        assert!(!file.exists() && !path.exists());
        assert_eq!(registry.len(), 0);
    }
}
