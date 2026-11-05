//! OS conventions in one place: where user data lives and how primary
//! shortcuts are spelled. Everything else asks here instead of branching on
//! `target_os` or assuming macOS paths (`$HOME/Library/...`).

use std::path::PathBuf;

// Where TableX keeps its files and what the OS calls its credential store are shared with
// `tablex-mcp`, so they live in `engine-connect`.
pub use engine_connect::platform::{
    app_data_dir, ssh_known_hosts_path, CREDENTIAL_STORE_LABEL,
};

/// Where a save dialog opens when the caller has no better idea.
pub fn save_dialog_dir() -> PathBuf {
    dirs::home_dir().unwrap_or_else(std::env::temp_dir)
}

/// DBeaver's workspace folder holding `data-sources.json`:
/// `~/Library/DBeaverData/…` on macOS, `~/.local/share/DBeaverData/…` on Linux,
/// `%APPDATA%\DBeaverData\…` on Windows. Only a hint for the importer; the
/// folder may not exist.
pub fn dbeaver_workspace_dir() -> Option<PathBuf> {
    let base = if cfg!(target_os = "macos") {
        dirs::home_dir().map(|home| home.join("Library"))
    } else {
        dirs::data_dir()
    };
    base.map(|base| base.join("DBeaverData/workspace6/General/.dbeaver"))
}

/// The `JetBrains` folder with one `DataGrip<version>/options` below it:
/// `~/Library/Application Support/JetBrains` on macOS, `~/.config/JetBrains`
/// on Linux, `%APPDATA%\JetBrains` on Windows.
pub fn datagrip_config_root() -> Option<PathBuf> {
    let base = if cfg!(target_os = "macos") {
        dirs::data_dir()
    } else {
        dirs::config_dir()
    };
    base.map(|base| base.join("JetBrains"))
}

/// `<Downloads>/TableX`, the default target of result exports.
pub fn exports_dir() -> Option<PathBuf> {
    dirs::download_dir()
        .or_else(|| dirs::home_dir().map(|home| home.join("Downloads")))
        .map(|dir| dir.join("TableX"))
}

/// Label of the primary-modifier shortcut `secondary-<key>` (`secondary-shift-<key>`
/// with `shift`): `⌘S` / `⇧⌘L` on macOS, `Ctrl+S` / `Ctrl+Shift+L` elsewhere.
/// `key` is a printable character or `"Enter"`.
pub fn primary_label(shift: bool, key: &str) -> String {
    if cfg!(target_os = "macos") {
        let key = if key == "Enter" { "↵" } else { key };
        format!("{}⌘{key}", if shift { "⇧" } else { "" })
    } else {
        format!("Ctrl+{}{key}", if shift { "Shift+" } else { "" })
    }
}

/// Command-palette footer for a database / schema row: Enter reveals it in the
/// sidebar; these are the other two activations.
pub const PALETTE_NODE_HINT: &str = if cfg!(target_os = "macos") {
    "⇧↵ New Query  ⌘↵ Diagram"
} else {
    "Shift+Enter New Query  Ctrl+Enter Diagram"
};

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn primary_labels_follow_the_platform_convention() {
        if cfg!(target_os = "macos") {
            assert_eq!(primary_label(false, "S"), "⌘S");
            assert_eq!(primary_label(true, "L"), "⇧⌘L");
            assert_eq!(primary_label(false, "Enter"), "⌘↵");
        } else {
            assert_eq!(primary_label(false, "S"), "Ctrl+S");
            assert_eq!(primary_label(true, "L"), "Ctrl+Shift+L");
            assert_eq!(primary_label(false, "Enter"), "Ctrl+Enter");
        }
    }

    #[test]
    fn data_paths_are_platform_dirs_under_the_app_folder() {
        let dir = app_data_dir().expect("a data dir exists");
        assert!(dir.ends_with("tablex"));
        assert!(exports_dir().expect("an exports dir exists").ends_with("TableX"));
    }
}
