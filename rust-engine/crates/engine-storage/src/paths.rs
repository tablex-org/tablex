use std::ffi::OsString;
use std::path::PathBuf;

/// Environment variable that replaces the platform data dir (portable installs,
/// tests): stores then live in `$TABLEX_DATA_DIR/<folder>`. Only an absolute path counts, see
/// [`data_dir_override`].
pub const DATA_DIR_ENV: &str = "TABLEX_DATA_DIR";

/// The directory `TABLEX_DATA_DIR` names, or `None` when it is unset, empty or relative. A relative
/// value would mean a different place for every process working directory, and an empty one is
/// what a launcher that "clears" the variable leaves behind: both are treated as unset. The one
/// reader of the variable, so the connection store, `settings.json`, `known_hosts` and the audit
/// log can never disagree about where the data dir is.
pub fn data_dir_override() -> Option<PathBuf> {
    absolute_override(std::env::var_os(DATA_DIR_ENV))
}

fn absolute_override(value: Option<OsString>) -> Option<PathBuf> {
    value.map(PathBuf::from).filter(|path| path.is_absolute())
}

/// `<platform data dir>/<folder>` (`~/Library/Application Support/<folder>` on
/// macOS, `$XDG_DATA_HOME/<folder>` on Linux, `%APPDATA%\<folder>` on Windows),
/// or `./<folder>` when the platform has no data dir. `TABLEX_DATA_DIR`, when
/// set to an absolute path, wins over all of those.
pub(crate) fn data_dir(folder: &str) -> PathBuf {
    data_dir_override()
        .or_else(dirs::data_dir)
        .unwrap_or_else(|| PathBuf::from("."))
        .join(folder)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_an_absolute_value_overrides_the_data_dir() {
        let absolute = std::env::temp_dir();
        assert_eq!(
            absolute_override(Some(absolute.clone().into_os_string())),
            Some(absolute)
        );
        assert_eq!(absolute_override(None), None, "unset");
        assert_eq!(absolute_override(Some(OsString::new())), None, "empty");
        assert_eq!(
            absolute_override(Some("relative/dir".into())),
            None,
            "relative"
        );
        assert_eq!(absolute_override(Some(".".into())), None, "dot");
    }
}
