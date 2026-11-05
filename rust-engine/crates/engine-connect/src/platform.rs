//! Where TableX keeps its files and what the OS calls its credential store. The app's
//! `platform` module re-exports these next to its own UI-only conventions (shortcut labels,
//! default folders of dialogs), so `tablex-mcp` and the app agree on every path.

use std::path::PathBuf;

/// Directory under the platform data dir that holds every TableX file.
const APP_DIR: &str = "tablex";

/// `~/Library/Application Support/tablex` on macOS, `$XDG_DATA_HOME/tablex`
/// (default `~/.local/share/tablex`) on Linux, `%APPDATA%\tablex` on Windows.
/// `TABLEX_DATA_DIR`, when set to an absolute path, replaces the platform data dir
/// (`$TABLEX_DATA_DIR/tablex`), the same switch (and the same reader,
/// `engine_storage::paths::data_dir_override`) the connection store uses, so one variable moves
/// every TableX file; an empty or relative value is ignored.
pub fn app_data_dir() -> Option<PathBuf> {
    engine_storage::paths::data_dir_override()
        .or_else(dirs::data_dir)
        .map(|dir| dir.join(APP_DIR))
}

/// The agent audit log shared by every front end (`tablex-mcp`, later the in-app assistant):
/// `<app data dir>/audit.jsonl` (rotated to `audit.jsonl.1` ...).
pub fn audit_log_path() -> Option<PathBuf> {
    app_data_dir().map(|dir| dir.join("audit.jsonl"))
}

/// The app-owned SSH `known_hosts` (hosts the user confirmed in "Trust this host?"):
/// `<app data dir>/known_hosts`. The user's own `~/.ssh/known_hosts` is only read.
pub fn ssh_known_hosts_path() -> Option<PathBuf> {
    app_data_dir().map(|dir| dir.join("known_hosts"))
}

/// Where `settings.json` lives: `<config dir>/tablex`. macOS keeps it in the
/// existing data dir (`~/Library/Application Support/tablex`, which is also
/// `dirs::config_dir()` there); Linux uses `$XDG_CONFIG_HOME/tablex`
/// (default `~/.config/tablex`), Windows `%APPDATA%\tablex`. With a valid `TABLEX_DATA_DIR` it is the
/// app data dir on every platform.
pub fn config_dir() -> Option<PathBuf> {
    if cfg!(target_os = "macos") || engine_storage::paths::data_dir_override().is_some() {
        app_data_dir()
    } else {
        dirs::config_dir().map(|dir| dir.join(APP_DIR))
    }
}

/// What the OS calls the store holding saved connection passwords, for messages
/// ("Saved password was not found in Keychain").
pub const CREDENTIAL_STORE_LABEL: &str = if cfg!(target_os = "macos") {
    "Keychain"
} else if cfg!(target_os = "windows") {
    "Credential Manager"
} else {
    "system keyring"
};

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn data_paths_are_platform_dirs_under_the_app_folder() {
        assert!(app_data_dir()
            .expect("a data dir exists")
            .ends_with("tablex"));
        assert!(config_dir()
            .expect("a config dir exists")
            .ends_with("tablex"));
        assert!(ssh_known_hosts_path()
            .expect("a data dir exists")
            .ends_with("tablex/known_hosts"));
        assert!(audit_log_path()
            .expect("a data dir exists")
            .ends_with("tablex/audit.jsonl"));
    }

    #[test]
    fn data_dir_env_is_ignored_when_empty_or_relative_and_moves_every_file_when_absolute() {
        use engine_storage::paths::DATA_DIR_ENV;
        let platform_default = app_data_dir();
        let absolute = std::env::temp_dir().join("tablex-platform-env-test");
        // The only test that sets the variable; the others only look at path suffixes.
        for ignored in ["", "relative/dir", "."] {
            std::env::set_var(DATA_DIR_ENV, ignored);
            assert_eq!(app_data_dir(), platform_default, "{ignored:?} is unset");
            assert_eq!(
                audit_log_path(),
                platform_default.clone().map(|d| d.join("audit.jsonl"))
            );
        }
        std::env::set_var(DATA_DIR_ENV, &absolute);
        let root = absolute.join("tablex");
        assert_eq!(app_data_dir(), Some(root.clone()));
        assert_eq!(config_dir(), Some(root.clone()));
        assert_eq!(audit_log_path(), Some(root.join("audit.jsonl")));
        assert_eq!(ssh_known_hosts_path(), Some(root.join("known_hosts")));
        std::env::remove_var(DATA_DIR_ENV);
    }
}
