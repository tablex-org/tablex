//! The bearer token of the HTTP transport: where it comes from, how it is created and kept, and how
//! a presented one is compared. The token is a secret: nothing here formats it into an error, a
//! log line or a `Debug` output; only `--print-token` writes it, to stdout, on request.

use std::ffi::OsString;
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::sync::Arc;

use subtle::ConstantTimeEq;

/// The environment variable that carries the token (it wins over the file).
pub const TOKEN_ENV: &str = "TABLEX_MCP_TOKEN";

/// The token file in the tablex data directory.
pub const TOKEN_FILE: &str = "mcp-http-token";

/// A token from the environment must be at least this long (a generated one is 64 characters).
pub const MIN_TOKEN_CHARS: usize = 16;

/// Random bytes of a generated token (256 bits, written as 64 hex characters).
const GENERATED_BYTES: usize = 32;

/// Where a token was read from (what the server says at start, never the value).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TokenSource {
    Env,
    File(PathBuf),
}

#[derive(Debug, thiserror::Error)]
pub enum TokenError {
    #[error("{TOKEN_ENV} is not valid UTF-8")]
    EnvNotUtf8,
    #[error("the token is not usable: it must be at least {MIN_TOKEN_CHARS} printable ASCII characters without spaces")]
    Unusable,
    #[error("this platform has no data directory, so there is nowhere to keep a token; set {TOKEN_ENV}")]
    NoDataDir,
    #[error("the token file {path} is readable by other users; delete it (a new token is made on the next start) or restrict it to your account")]
    TooOpen { path: PathBuf },
    #[error("the token file {path} is not a regular file")]
    NotAFile { path: PathBuf },
    #[error("the token file {path} belongs to another user; delete it (a new token is made on the next start)")]
    NotYours { path: PathBuf },
    #[error("cannot use the token file {path}: {source}")]
    Io {
        path: PathBuf,
        #[source]
        source: io::Error,
    },
    #[error("cannot get random bytes for a token: {0}")]
    Random(getrandom::Error),
}

/// A bearer token. Cloning shares the bytes; `Debug` hides them.
#[derive(Clone)]
pub struct Token(Arc<[u8]>);

impl std::fmt::Debug for Token {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("Token(..)")
    }
}

impl Token {
    /// `text` as a token, if it is one: long enough, printable ASCII, no spaces (so it fits a
    /// header value and cannot be mistaken for the `Bearer` scheme's own syntax).
    pub fn parse(text: &str) -> Result<Self, TokenError> {
        let usable = text.len() >= MIN_TOKEN_CHARS && text.bytes().all(|b| b.is_ascii_graphic());
        if usable {
            Ok(Self(Arc::from(text.as_bytes())))
        } else {
            Err(TokenError::Unusable)
        }
    }

    /// Whether `presented` is this token. Constant time in the content; the length is not
    /// secret (a generated token's is fixed and public).
    pub fn matches(&self, presented: &[u8]) -> bool {
        self.0.ct_eq(presented).into()
    }

    /// The token as text, for `--print-token` only.
    pub fn reveal(&self) -> &str {
        // `parse` admits printable ASCII only.
        std::str::from_utf8(&self.0).unwrap_or_default()
    }

    fn generate() -> Result<Self, TokenError> {
        let mut bytes = [0u8; GENERATED_BYTES];
        getrandom::fill(&mut bytes).map_err(TokenError::Random)?;
        let hex: String = bytes.iter().map(|byte| format!("{byte:02x}")).collect();
        Self::parse(&hex)
    }
}

/// The default token file: `<tablex data dir>/mcp-http-token`.
pub fn default_path() -> Option<PathBuf> {
    engine_connect::platform::app_data_dir().map(|dir| dir.join(TOKEN_FILE))
}

/// The token to serve with: `env` (the value of [`TOKEN_ENV`], when set) or else the one in the
/// file at `path`, created with a fresh random token on first use (`0600` on Unix; on Windows
/// the file inherits the ACL of the user's profile directory it lives in).
pub fn load(
    env: Option<OsString>,
    path: Option<&Path>,
) -> Result<(Token, TokenSource), TokenError> {
    if let Some(value) = env {
        let value = value.into_string().map_err(|_| TokenError::EnvNotUtf8)?;
        return Ok((Token::parse(&value)?, TokenSource::Env));
    }
    let path = path.ok_or(TokenError::NoDataDir)?;
    let io_error = |source| TokenError::Io {
        path: path.to_path_buf(),
        source,
    };
    #[cfg(unix)]
    if let Some(parent) = path.parent().filter(|parent| !parent.as_os_str().is_empty()) {
        restrict_dir(parent).map_err(io_error)?;
    }
    match read_file(path) {
        Err(TokenError::Io { source, .. }) if source.kind() == io::ErrorKind::NotFound => {}
        found => return found.map(|token| (token, TokenSource::File(path.to_path_buf()))),
    }
    let token = Token::generate()?;
    if let Some(parent) = path.parent().filter(|parent| !parent.as_os_str().is_empty()) {
        create_private_dir(parent).map_err(io_error)?;
    }
    match create_file(path, &token) {
        Ok(()) => {}
        // Another start made it first: use theirs.
        Err(error) if error.kind() == io::ErrorKind::AlreadyExists => {
            return read_file(path).map(|token| (token, TokenSource::File(path.to_path_buf())));
        }
        Err(error) => {
            let _ = std::fs::remove_file(path);
            return Err(io_error(error));
        }
    }
    Ok((token, TokenSource::File(path.to_path_buf())))
}

/// Opens `path` for reading and checks the file that was opened (not the path, which can change
/// between a check and the open): a regular file, owned by this user, not open to others. A link is
/// not followed.
fn open_token_file(path: &Path) -> Result<std::fs::File, TokenError> {
    let io_error = |source| TokenError::Io {
        path: path.to_path_buf(),
        source,
    };
    let not_a_file = || TokenError::NotAFile {
        path: path.to_path_buf(),
    };
    let mut options = std::fs::OpenOptions::new();
    options.read(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.custom_flags(libc::O_NOFOLLOW);
    }
    let file = match options.open(path) {
        Ok(file) => file,
        #[cfg(unix)]
        Err(error) if error.raw_os_error() == Some(libc::ELOOP) => return Err(not_a_file()),
        Err(error) => return Err(io_error(error)),
    };
    let metadata = file.metadata().map_err(io_error)?;
    if !metadata.is_file() {
        return Err(not_a_file());
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::{MetadataExt, PermissionsExt};
        // SAFETY: `geteuid` takes no arguments, touches no memory and cannot fail.
        if metadata.uid() != unsafe { libc::geteuid() } {
            return Err(TokenError::NotYours {
                path: path.to_path_buf(),
            });
        }
        if metadata.permissions().mode() & 0o077 != 0 {
            return Err(TokenError::TooOpen {
                path: path.to_path_buf(),
            });
        }
    }
    #[cfg(not(unix))]
    if std::fs::symlink_metadata(path).is_ok_and(|link| link.file_type().is_symlink()) {
        return Err(not_a_file());
    }
    Ok(file)
}

fn read_file(path: &Path) -> Result<Token, TokenError> {
    use std::io::Read;
    let mut text = String::new();
    open_token_file(path)?
        .take(1024)
        .read_to_string(&mut text)
        .map_err(|source| TokenError::Io {
            path: path.to_path_buf(),
            source,
        })?;
    Token::parse(text.trim())
}

/// The directory of the token file is for this user only: one that exists with wider permissions
/// (a directory made by another program, or by an older version) is set to `0700`, so a token
/// file that is replaced cannot be read or swapped by others. (Not on Windows: the directory
/// inherits the ACL of the user's profile.)
#[cfg(unix)]
fn restrict_dir(dir: &Path) -> io::Result<()> {
    use std::os::unix::fs::PermissionsExt;
    match std::fs::metadata(dir) {
        Ok(metadata) if metadata.is_dir() && metadata.permissions().mode() & 0o077 != 0 => {
            std::fs::set_permissions(dir, std::fs::Permissions::from_mode(0o700))
        }
        _ => Ok(()),
    }
}

fn create_file(path: &Path, token: &Token) -> io::Result<()> {
    let mut options = std::fs::OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    let mut file = options.open(path)?;
    file.write_all(token.reveal().as_bytes())?;
    file.write_all(b"\n")?;
    file.sync_all()
}

fn create_private_dir(dir: &Path) -> io::Result<()> {
    let mut builder = std::fs::DirBuilder::new();
    builder.recursive(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::DirBuilderExt;
        builder.mode(0o700);
    }
    builder.create(dir)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_token_is_long_printable_ascii_without_spaces() {
        assert!(Token::parse("0123456789abcdef").is_ok());
        for bad in ["", "short", "0123456789abcde", "0123456789 abcdef", "0123456789abcdé!!", "0123456789abcdef\n"] {
            assert!(matches!(Token::parse(bad), Err(TokenError::Unusable)), "{bad:?}");
        }
    }

    #[test]
    fn the_comparison_accepts_only_the_exact_token() {
        let token = Token::parse("0123456789abcdef").unwrap();
        assert!(token.matches(b"0123456789abcdef"));
        for wrong in [&b""[..], b"0123456789abcdeg", b"0123456789abcde", b"0123456789abcdef0", b"0123456789ABCDEF"] {
            assert!(!token.matches(wrong), "{wrong:?}");
        }
    }

    #[test]
    fn debug_never_shows_the_token() {
        let token = Token::parse("0123456789abcdef").unwrap();
        assert!(!format!("{token:?}").contains("0123456789"));
    }

    #[test]
    fn the_environment_wins_and_a_bad_value_is_an_error_not_a_fallback() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join(TOKEN_FILE);
        let (token, source) = load(Some("0123456789abcdef".into()), Some(&path)).unwrap();
        assert_eq!(token.reveal(), "0123456789abcdef");
        assert_eq!(source, TokenSource::Env);
        assert!(!path.exists(), "no file is made when the environment has the token");
        assert!(matches!(
            load(Some("short".into()), Some(&path)),
            Err(TokenError::Unusable)
        ));
        assert!(matches!(
            load(Some("".into()), Some(&path)),
            Err(TokenError::Unusable)
        ));
    }

    #[test]
    fn the_file_is_made_on_first_use_and_read_back_after() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("nested").join(TOKEN_FILE);
        let (first, source) = load(None, Some(&path)).unwrap();
        assert_eq!(source, TokenSource::File(path.clone()));
        assert_eq!(first.reveal().len(), GENERATED_BYTES * 2);
        let (second, _) = load(None, Some(&path)).unwrap();
        assert_eq!(first.reveal(), second.reveal());
        let (other, _) = load(None, Some(&dir.path().join("other"))).unwrap();
        assert_ne!(first.reveal(), other.reveal(), "each file gets its own random token");
    }

    #[cfg(unix)]
    #[test]
    fn the_file_is_private_and_a_looser_one_is_refused() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("private").join(TOKEN_FILE);
        load(None, Some(&path)).unwrap();
        let mode = |p: &Path| std::fs::metadata(p).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode(&path), 0o600);
        assert_eq!(mode(path.parent().unwrap()), 0o700);
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o644)).unwrap();
        assert!(matches!(load(None, Some(&path)), Err(TokenError::TooOpen { .. })));
    }

    #[cfg(unix)]
    #[test]
    fn a_directory_that_is_open_to_others_is_closed_to_them() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().unwrap();
        std::fs::set_permissions(dir.path(), std::fs::Permissions::from_mode(0o755)).unwrap();
        let path = dir.path().join(TOKEN_FILE);
        load(None, Some(&path)).unwrap();
        let mode = |p: &Path| std::fs::metadata(p).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode(dir.path()), 0o700);
        assert_eq!(mode(&path), 0o600);
        // An existing token in an open directory closes it too.
        std::fs::set_permissions(dir.path(), std::fs::Permissions::from_mode(0o777)).unwrap();
        load(None, Some(&path)).unwrap();
        assert_eq!(mode(dir.path()), 0o700);
    }

    #[cfg(unix)]
    #[test]
    fn a_token_file_is_checked_on_the_file_that_is_read() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join(TOKEN_FILE);
        std::fs::write(&path, "0123456789abcdef0123456789abcdef\n").unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600)).unwrap();
        // Owned by this user, 0600: read through the open file.
        assert_eq!(open_token_file(&path).map(|_| ()).ok(), Some(()));
        // 0640 and 0604 are open to others too; a directory is no token file.
        for mode in [0o640, 0o604, 0o660] {
            std::fs::set_permissions(&path, std::fs::Permissions::from_mode(mode)).unwrap();
            assert!(matches!(open_token_file(&path), Err(TokenError::TooOpen { .. })), "{mode:o}");
        }
        assert!(matches!(open_token_file(dir.path()), Err(TokenError::NotAFile { .. })));
    }

    #[cfg(unix)]
    #[test]
    fn a_link_in_place_of_the_file_is_refused() {
        let dir = tempfile::tempdir().unwrap();
        let target = dir.path().join("target");
        std::fs::write(&target, "0123456789abcdef0123456789abcdef").unwrap();
        let path = dir.path().join(TOKEN_FILE);
        std::os::unix::fs::symlink(&target, &path).unwrap();
        assert!(matches!(load(None, Some(&path)), Err(TokenError::NotAFile { .. })));
    }

    #[test]
    fn a_file_with_a_bad_token_is_an_error_and_is_not_overwritten() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join(TOKEN_FILE);
        std::fs::write(&path, "short\n").unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600)).unwrap();
        }
        assert!(matches!(load(None, Some(&path)), Err(TokenError::Unusable)));
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "short\n");
    }

    #[test]
    fn no_data_dir_and_no_environment_is_an_error() {
        assert!(matches!(load(None, None), Err(TokenError::NoDataDir)));
    }
}
