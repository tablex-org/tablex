//! Cross-platform home directory helpers (`~/.ssh/...`).

use std::path::{Path, PathBuf};

/// The user's home directory (`$HOME` on Unix, the profile folder on Windows).
pub fn home_dir() -> Option<PathBuf> {
    std::env::home_dir()
}

/// `~/.ssh/known_hosts`, or `None` when no home directory can be determined.
pub fn default_user_known_hosts() -> Option<PathBuf> {
    home_dir().map(|home| home.join(".ssh").join("known_hosts"))
}

/// The machine-wide known_hosts: `/etc/ssh/ssh_known_hosts` on Unix, `%ProgramData%\ssh\ssh_known_hosts` on
/// Windows. Read-only for this crate.
pub fn default_system_known_hosts() -> Option<PathBuf> {
    #[cfg(unix)]
    {
        Some(PathBuf::from("/etc/ssh/ssh_known_hosts"))
    }
    #[cfg(windows)]
    {
        std::env::var_os("ProgramData")
            .map(|dir| PathBuf::from(dir).join("ssh").join("ssh_known_hosts"))
    }
    #[cfg(not(any(unix, windows)))]
    {
        None
    }
}

/// `~/.ssh/config`, or `None` when no home directory can be determined.
pub fn default_ssh_config() -> Option<PathBuf> {
    home_dir().map(|home| home.join(".ssh").join("config"))
}

/// Expands a leading `~` or `~/` against `home`; any other path is returned unchanged.
pub(crate) fn expand_tilde_with(path: &str, home: Option<&Path>) -> PathBuf {
    let Some(home) = home else {
        return PathBuf::from(path);
    };
    if path == "~" {
        return home.to_path_buf();
    }
    match path.strip_prefix("~/").or_else(|| path.strip_prefix("~\\")) {
        Some(rest) => home.join(rest),
        None => PathBuf::from(path),
    }
}

/// [`expand_tilde_with`] using the real home directory.
pub fn expand_tilde(path: &str) -> PathBuf {
    expand_tilde_with(path, home_dir().as_deref())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn expands_only_a_leading_tilde() {
        let home = Path::new("/home/example");
        assert_eq!(
            expand_tilde_with("~/.ssh/id", Some(home)),
            home.join(".ssh/id")
        );
        assert_eq!(expand_tilde_with("~", Some(home)), home);
        assert_eq!(
            expand_tilde_with("/etc/~/x", Some(home)),
            PathBuf::from("/etc/~/x")
        );
        assert_eq!(
            expand_tilde_with("~other/x", Some(home)),
            PathBuf::from("~other/x")
        );
        assert_eq!(expand_tilde_with("~/x", None), PathBuf::from("~/x"));
    }
}
