//! Reading and writing the `.sql` file a query tab is bound to. Both block on the disk: call them
//! off the UI thread.

use std::io::Read as _;
use std::path::{Path, PathBuf};

/// The largest file Open SQL File loads into an editor. A bigger script is for a run-from-file
/// command (it streams statements instead of holding the text in an editor).
pub const MAX_BYTES: u64 = 5 * 1024 * 1024;

#[derive(Debug)]
pub enum SqlFileError {
    Read { path: PathBuf, source: std::io::Error },
    Write { path: PathBuf, source: std::io::Error },
    /// Over [`MAX_BYTES`].
    TooLarge { path: PathBuf },
    /// Not valid UTF-8: nothing is loaded rather than a lossy copy that a Save would write back.
    NotUtf8 { path: PathBuf },
}

impl std::fmt::Display for SqlFileError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Read { path, source } => write!(f, "Could not read {}: {source}", path.display()),
            Self::Write { path, source } => write!(f, "Could not save {}: {source}", path.display()),
            Self::TooLarge { path } => write!(
                f,
                "{} is larger than {} MB, too big to open in an editor. Use Run SQL File to run it without opening it.",
                path.display(),
                MAX_BYTES / (1024 * 1024)
            ),
            Self::NotUtf8 { path } => {
                write!(f, "{} is not UTF-8 text, so it was not opened.", path.display())
            }
        }
    }
}

impl std::error::Error for SqlFileError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Read { source, .. } | Self::Write { source, .. } => Some(source),
            Self::TooLarge { .. } | Self::NotUtf8 { .. } => None,
        }
    }
}

/// The text of `path`. Reads at most one byte past the limit, so a huge file is refused without
/// being loaded.
pub fn read(path: &Path) -> Result<String, SqlFileError> {
    let read_error = |source| SqlFileError::Read { path: path.to_owned(), source };
    let file = std::fs::File::open(path).map_err(read_error)?;
    let mut bytes = Vec::new();
    file.take(MAX_BYTES + 1).read_to_end(&mut bytes).map_err(read_error)?;
    if bytes.len() as u64 > MAX_BYTES {
        return Err(SqlFileError::TooLarge { path: path.to_owned() });
    }
    String::from_utf8(bytes).map_err(|_| SqlFileError::NotUtf8 { path: path.to_owned() })
}

pub fn write(path: &Path, text: &str) -> Result<(), SqlFileError> {
    std::fs::write(path, text.as_bytes())
        .map_err(|source| SqlFileError::Write { path: path.to_owned(), source })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reads_and_writes_text_and_refuses_what_it_cannot_hold() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("a.sql");
        write(&path, "select 'é';\n").unwrap();
        assert_eq!(read(&path).unwrap(), "select 'é';\n");

        std::fs::write(&path, [b's', 0xff, 0xfe]).unwrap();
        assert!(matches!(read(&path), Err(SqlFileError::NotUtf8 { .. })));

        std::fs::write(&path, vec![b' '; MAX_BYTES as usize]).unwrap();
        assert_eq!(read(&path).unwrap().len() as u64, MAX_BYTES);
        std::fs::write(&path, vec![b' '; MAX_BYTES as usize + 1]).unwrap();
        let error = read(&path).unwrap_err();
        assert!(matches!(error, SqlFileError::TooLarge { .. }));
        assert!(error.to_string().contains("5 MB"), "{error}");

        let missing = read(&dir.path().join("none.sql")).unwrap_err();
        assert!(matches!(missing, SqlFileError::Read { .. }) && std::error::Error::source(&missing).is_some());
        let blocked = write(&dir.path().join("no-dir").join("a.sql"), "x").unwrap_err();
        assert!(matches!(blocked, SqlFileError::Write { .. }));
    }
}
