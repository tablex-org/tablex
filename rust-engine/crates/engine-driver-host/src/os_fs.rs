//! The file-system behaviour that differs per OS, in one place: opening a
//! private file without following links, and the fsyncs that only Unix can do
//! on a directory.

use std::fs::{File, OpenOptions};
use std::io;
use std::path::Path;

/// `FILE_FLAG_OPEN_REPARSE_POINT`: open a symlink / junction itself instead of
/// following it (so it can be refused).
#[cfg(windows)]
const FILE_FLAG_OPEN_REPARSE_POINT: u32 = 0x0020_0000;
#[cfg(windows)]
const FILE_ATTRIBUTE_REPARSE_POINT: u32 = 0x0000_0400;

/// Opens `path` with `options` as a private file that is never reached through
/// a link: Unix opens with mode 0600 and `O_NOFOLLOW`; Windows opens with
/// `FILE_FLAG_OPEN_REPARSE_POINT` and refuses a reparse point (symlink or
/// junction) afterwards. Close-on-exec is the default on Windows.
pub(crate) fn open_private(options: &mut OpenOptions, path: &Path) -> io::Result<File> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options
            .mode(0o600)
            .custom_flags(libc::O_CLOEXEC | libc::O_NOFOLLOW);
    }
    #[cfg(windows)]
    {
        use std::os::windows::fs::OpenOptionsExt;
        options.custom_flags(FILE_FLAG_OPEN_REPARSE_POINT);
    }
    let file = options.open(path)?;
    #[cfg(windows)]
    {
        use std::os::windows::fs::MetadataExt;
        if file.metadata()?.file_attributes() & FILE_ATTRIBUTE_REPARSE_POINT != 0 {
            return Err(io::Error::new(
                io::ErrorKind::PermissionDenied,
                format!("refusing to open a reparse point: {}", path.display()),
            ));
        }
    }
    Ok(file)
}

/// Whether `metadata` (from `symlink_metadata`) describes a link: a symlink on
/// Unix, a symlink or junction (any reparse point) on Windows.
pub(crate) fn is_link(metadata: &std::fs::Metadata) -> bool {
    #[cfg(windows)]
    {
        use std::os::windows::fs::MetadataExt;
        metadata.file_attributes() & FILE_ATTRIBUTE_REPARSE_POINT != 0
    }
    #[cfg(not(windows))]
    {
        metadata.file_type().is_symlink()
    }
}

/// Opens `path` for reading, never through a link and never blocking on a special file: Unix
/// opens with `O_NOFOLLOW | O_NONBLOCK` (a FIFO would otherwise block the open until a writer
/// appears), Windows with `FILE_FLAG_OPEN_REPARSE_POINT`. The opened handle must be a regular
/// file (checked on the handle, so nothing can be swapped in after a path check), else
/// `InvalidInput`.
pub(crate) fn open_regular_read(path: &Path) -> io::Result<File> {
    let mut options = OpenOptions::new();
    options.read(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.custom_flags(libc::O_CLOEXEC | libc::O_NOFOLLOW | libc::O_NONBLOCK);
    }
    #[cfg(windows)]
    {
        use std::os::windows::fs::OpenOptionsExt;
        options.custom_flags(FILE_FLAG_OPEN_REPARSE_POINT);
    }
    let file = options.open(path)?;
    let meta = file.metadata()?;
    if !meta.is_file() || is_link(&meta) {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            format!("{} is not a regular file", path.display()),
        ));
    }
    Ok(file)
}

/// Flushes the file at `path` to disk. Windows needs a write handle for this.
pub(crate) fn sync_file(path: &Path) -> io::Result<()> {
    #[cfg(windows)]
    let file = OpenOptions::new().write(true).open(path)?;
    #[cfg(not(windows))]
    let file = File::open(path)?;
    file.sync_all()
}

/// Flushes a directory entry (after a rename) to disk. Only Unix can open a
/// directory for this; elsewhere the file system journals the rename itself.
pub(crate) fn sync_directory(path: &Path) -> io::Result<()> {
    #[cfg(unix)]
    {
        File::open(path)?.sync_all()
    }
    #[cfg(not(unix))]
    {
        let _ = path;
        Ok(())
    }
}
