//! Reading, migrating and encoding `settings.json`. Writing goes through
//! `gpui_session::write_atomic` (temp file + rename) off the UI thread; the
//! debounce lives with `AppRoot` (`app/settings_ui`).

use super::{Settings, SCHEMA_VERSION};
use crate::gpui_session;
use crate::sidebar_preferences::SidebarPreferences;
use std::path::Path;

pub use engine_connect::profile::{
    default_settings_path as default_path, SETTINGS_FILE as FILE_NAME,
};

/// Data older versions kept elsewhere, offered to the migrations.
#[derive(Default)]
pub struct Legacy {
    /// `sidebar-preferences.json`: the global choices that now live here
    /// (per-connection colors, order, filters, pins stay in that file).
    pub sidebar: Option<SidebarPreferences>,
}

impl Legacy {
    pub fn read(sidebar_preferences: Option<&Path>) -> Self {
        Self {
            sidebar: sidebar_preferences.and_then(|path| SidebarPreferences::load(path).ok()),
        }
    }
}

/// A typed, numbered migration: brings a settings value of version `to - 1`
/// to version `to`.
struct Migration {
    to: u32,
    apply: fn(&mut Settings, &Legacy),
}

/// Ordered by `to`; the last entry equals [`SCHEMA_VERSION`].
const MIGRATIONS: &[Migration] = &[
    Migration {
        to: 1,
        apply: import_sidebar_preferences,
    },
    Migration {
        to: 2,
        apply: add_connection_types,
    },
    Migration {
        to: 3,
        apply: add_ai_section,
    },
    Migration {
        to: 4,
        apply: add_mcp_writes,
    },
    Migration {
        to: 5,
        apply: move_model_and_address_under_anthropic,
    },
];

/// 5: the model and the address are kept per provider (`ai.providers.<id>`). A version-4 file has
/// them at the top of `ai`, and Anthropic was its only provider. `ai.mcp_writes` does not move.
fn move_model_and_address_under_anthropic(settings: &mut Settings, _: &Legacy) {
    settings.ai.adopt_legacy();
}

/// 4: the switch "Let MCP clients run changes". A version-3 file has none (serde fills `false`); the
/// step makes sure it is off after the upgrade, whatever a hand-edited file says.
fn add_mcp_writes(settings: &mut Settings, _: &Legacy) {
    settings.ai.mcp_writes = false;
}

/// 3: the AI assistant section. A version-2 file has none (serde fills the defaults); the step
/// only makes sure what is there is usable.
fn add_ai_section(settings: &mut Settings, _: &Legacy) {
    settings.ai.normalize();
}

/// 2: connection types. A version-1 file has none: the built-in ones are
/// added; saved connections without a type keep following the global
/// settings.
fn add_connection_types(settings: &mut Settings, _: &Legacy) {
    settings.normalize_connection_types();
}

/// 1: the user choices of `sidebar-preferences.json` that are global
/// settings: connection sort, global search, tab group-by, compact tab titles
/// and the second tab row.
fn import_sidebar_preferences(settings: &mut Settings, legacy: &Legacy) {
    let Some(prefs) = &legacy.sidebar else {
        return;
    };
    settings.navigation.connection_sort = prefs.sort;
    settings.navigation.global_search = prefs.global_search;
    settings.appearance.tab_group_by = prefs.tab_group_by;
    settings.appearance.compact_tab_titles = prefs.compact_tab_titles;
    settings.appearance.tab_rows = prefs.tab_rows;
}

/// Runs every migration newer than `settings.schema_version`.
pub fn migrate(settings: &mut Settings, legacy: &Legacy) {
    let from = settings.schema_version;
    for migration in MIGRATIONS.iter().filter(|m| m.to > from) {
        (migration.apply)(settings, legacy);
        settings.schema_version = migration.to;
    }
    // A hand-edited file may have lost a built-in type or point the API key somewhere unsafe (and a
    // version-5 file has no business holding the old top-level model / address).
    settings.normalize_connection_types();
    settings.ai.drop_legacy();
    settings.ai.normalize();
}

/// What reading the settings file produced.
#[derive(Debug)]
pub struct Loaded {
    pub settings: Settings,
    /// Worth telling the user (unusable file, newer format).
    pub notice: Option<String>,
    /// The file is missing or was migrated: write it back.
    pub needs_save: bool,
}

pub fn load(path: &Path, legacy: &Legacy) -> Loaded {
    let fresh = |notice: Option<String>| {
        let mut settings = Settings {
            schema_version: 0,
            ..Settings::default()
        };
        migrate(&mut settings, legacy);
        Loaded {
            settings,
            notice,
            needs_save: true,
        }
    };
    let text = match std::fs::read_to_string(path) {
        Ok(text) => text,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return fresh(None),
        Err(error) => {
            return Loaded {
                notice: Some(format!("Could not read the settings: {error}")),
                needs_save: false,
                ..fresh(None)
            }
        }
    };
    match serde_json::from_str::<Settings>(&text) {
        Ok(settings) if settings.schema_version > SCHEMA_VERSION => Loaded {
            notice: Some(format!(
                "The settings were written by a newer version of TableX (format {}); unknown settings are ignored",
                settings.schema_version
            )),
            needs_save: false,
            settings,
        },
        Ok(mut settings) => {
            let stale = settings.schema_version < SCHEMA_VERSION;
            migrate(&mut settings, legacy);
            Loaded {
                settings,
                notice: None,
                needs_save: stale,
            }
        }
        Err(error) => {
            // Keep the unreadable file for the user; the next save must not
            // destroy it silently.
            gpui_session::quarantine(path);
            fresh(Some(format!(
                "The settings file is corrupted and was set aside as settings.json.corrupt: {error}"
            )))
        }
    }
}

/// Serializes once; the caller may hand the bytes to a background thread.
pub fn encode(settings: &Settings) -> Result<Vec<u8>, String> {
    serde_json::to_vec_pretty(settings).map_err(|error| error.to_string())
}

#[cfg(test)]
/// Atomic synchronous save (tests and the quit-time flush).
pub fn save(path: &Path, settings: &Settings) -> std::io::Result<()> {
    let bytes = encode(settings).map_err(std::io::Error::other)?;
    gpui_session::write_atomic(path, &bytes)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::settings::{AiProvider, ProviderEntry};
    use crate::sidebar_preferences::{ConnectionSort, TabGroupBy};
    use std::path::PathBuf;

    fn temp_dir() -> PathBuf {
        let dir = std::env::temp_dir().join(format!("tablex-settings-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    /// A `sidebar-preferences.json` as an older build wrote it: global choices
    /// next to per-connection data that must stay where it is.
    const SIDEBAR_SAMPLE: &str = r##"{
        "groups": [{"id": "g1", "name": "Prod", "parent": null, "order": 0}],
        "connections": [{"id": "c1", "group": "g1", "order": 0, "color": "#ff0000"}],
        "sort": "descending",
        "sidebar_width": 300.0,
        "global_search": true,
        "tab_group_by": "database",
        "compact_tab_titles": true,
        "tab_rows": true,
        "default_databases": {"c1": "app"}
    }"##;

    #[test]
    fn migration_one_imports_the_global_sidebar_choices() {
        let dir = temp_dir();
        let sidebar = dir.join("sidebar-preferences.json");
        std::fs::write(&sidebar, SIDEBAR_SAMPLE).unwrap();
        let path = dir.join(FILE_NAME);

        let loaded = load(&path, &Legacy::read(Some(&sidebar)));
        assert!(loaded.needs_save && loaded.notice.is_none());
        let settings = loaded.settings;
        assert_eq!(settings.schema_version, SCHEMA_VERSION);
        assert_eq!(
            settings.navigation.connection_sort,
            ConnectionSort::Descending
        );
        assert!(settings.navigation.global_search);
        assert_eq!(settings.appearance.tab_group_by, TabGroupBy::Database);
        assert!(settings.appearance.compact_tab_titles && settings.appearance.tab_rows);
        // Everything else is untouched.
        assert_eq!(settings.results, super::super::Results::default());

        // Per-connection data stays in the sidebar file.
        let prefs = SidebarPreferences::load(&sidebar).unwrap();
        assert_eq!(prefs.connections[0].color.as_deref(), Some("#ff0000"));
        assert_eq!(
            prefs.default_databases.get("c1").map(String::as_str),
            Some("app")
        );
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn a_version_one_file_gains_the_built_in_connection_types() {
        let dir = temp_dir();
        let path = dir.join(FILE_NAME);
        std::fs::write(
            &path,
            r#"{"schema_version": 1, "editor": {"soft_wrap": true}}"#,
        )
        .unwrap();
        let loaded = load(&path, &Legacy::default());
        assert!(loaded.needs_save, "rewritten at the current version");
        assert_eq!(loaded.settings.schema_version, SCHEMA_VERSION);
        assert!(loaded.settings.editor.soft_wrap, "existing choices survive");
        let ids: Vec<_> = loaded
            .settings
            .connection_types
            .iter()
            .map(|ty| ty.id.as_str())
            .collect();
        assert_eq!(ids, ["development", "test", "production"]);
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn a_version_two_file_gains_the_ai_defaults_and_keeps_its_choices() {
        let dir = temp_dir();
        let path = dir.join(FILE_NAME);
        std::fs::write(
            &path,
            r#"{"schema_version": 2, "editor": {"soft_wrap": true}, "connection_types": []}"#,
        )
        .unwrap();
        let loaded = load(&path, &Legacy::default());
        assert!(loaded.needs_save, "rewritten at the current version");
        assert_eq!(loaded.settings.schema_version, SCHEMA_VERSION);
        assert_eq!(loaded.settings.ai, super::super::AiSettings::default());
        assert!(loaded.settings.editor.soft_wrap);
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn a_version_three_file_gains_the_mcp_switch_off_and_keeps_everything_else() {
        let dir = temp_dir();
        let path = dir.join(FILE_NAME);
        // A version-3 file: everything else is the user's, and a stray `mcp_writes` is not trusted.
        std::fs::write(
            &path,
            r#"{"schema_version": 3, "editor": {"soft_wrap": true}, "results": {"page_size": 321},
                "ai": {"model": "claude-haiku-5-5", "effort": "max", "keep_conversations": false,
                       "mcp_writes": true},
                "shortcuts": {"workspace.close_tab": ["secondary-q"]}}"#,
        )
        .unwrap();
        let loaded = load(&path, &Legacy::default());
        assert!(loaded.needs_save, "rewritten at the current version");
        let settings = loaded.settings;
        assert_eq!(settings.schema_version, SCHEMA_VERSION);
        assert_eq!(SCHEMA_VERSION, 5);
        assert!(!settings.ai.mcp_writes, "off after the upgrade");
        assert!(settings.editor.soft_wrap && settings.results.page_size == 321);
        // Version 3 -> 5: the model of the file ends up under Anthropic.
        assert_eq!(settings.ai.providers.anthropic.model, "claude-haiku-5-5");
        assert_eq!(settings.ai.effort, engine_ai::Effort::Max);
        assert!(!settings.ai.keep_conversations);
        assert_eq!(settings.shortcuts["workspace.close_tab"], ["secondary-q"]);
        // A file without the key (a plain version-3 file) is off too.
        std::fs::write(
            &path,
            r#"{"schema_version": 3, "ai": {"model": "claude-haiku-5-5"}}"#,
        )
        .unwrap();
        assert!(!load(&path, &Legacy::default()).settings.ai.mcp_writes);
        // At the current version the user's choice stands, and the MCP server's reader sees it.
        std::fs::write(
            &path,
            format!(r#"{{"schema_version": {SCHEMA_VERSION}, "ai": {{"mcp_writes": true}}}}"#),
        )
        .unwrap();
        let loaded = load(&path, &Legacy::default());
        assert!(loaded.settings.ai.mcp_writes && !loaded.needs_save);
        assert!(engine_connect::profile::mcp_writes_enabled(Some(&path)));
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn a_version_four_file_keeps_its_model_and_address_under_anthropic() {
        let dir = temp_dir();
        let path = dir.join(FILE_NAME);
        std::fs::write(
            &path,
            r#"{"schema_version": 4, "editor": {"soft_wrap": true},
                "ai": {"provider": "anthropic", "model": "claude-haiku-5-5",
                       "base_url": "http://localhost:4000", "effort": "max", "fallback": false,
                       "keep_conversations": false, "mcp_writes": true}}"#,
        )
        .unwrap();
        // The MCP server reads the switch from the file before and after the upgrade.
        assert!(engine_connect::profile::mcp_writes_enabled(Some(&path)));
        let loaded = load(&path, &Legacy::default());
        assert!(loaded.needs_save, "rewritten at version 5");
        let ai = loaded.settings.ai.clone();
        assert_eq!(loaded.settings.schema_version, 5);
        assert_eq!(ai.providers.anthropic.model, "claude-haiku-5-5");
        assert_eq!(ai.providers.anthropic.base_url, "http://localhost:4000");
        // The other providers start from their defaults, with no model.
        assert_eq!(
            ai.providers.openai,
            ProviderEntry::defaults(AiProvider::OpenAi)
        );
        assert_eq!(ai.providers.ollama.base_url, "http://localhost:11434/v1");
        assert_eq!(ai.providers.ollama.model, "");
        assert_eq!(ai.providers.openai_compatible, ProviderEntry::default());
        // Everything else of the section stays, and `mcp_writes` did not move or change.
        assert_eq!(ai.provider, AiProvider::Anthropic);
        assert_eq!(ai.effort, engine_ai::Effort::Max);
        assert!(!ai.fallback && !ai.keep_conversations && ai.mcp_writes);
        assert!(loaded.settings.editor.soft_wrap);

        // Written back, the file has the new shape, no top-level model / address, and the MCP
        // server's reader still sees the switch where it always looked.
        save(&path, &loaded.settings).unwrap();
        let written: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
        assert_eq!(written["schema_version"], 5);
        assert!(written["ai"].get("model").is_none() && written["ai"].get("base_url").is_none());
        assert_eq!(written["ai"]["mcp_writes"], true);
        assert_eq!(
            written["ai"]["providers"]["anthropic"]["model"],
            "claude-haiku-5-5"
        );
        assert!(engine_connect::profile::mcp_writes_enabled(Some(&path)));
        let again = load(&path, &Legacy::default());
        assert!(!again.needs_save && again.settings == loaded.settings);
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn a_version_three_file_with_a_bad_address_gets_the_default_under_anthropic() {
        let dir = temp_dir();
        let path = dir.join(FILE_NAME);
        std::fs::write(
            &path,
            r#"{"schema_version": 3, "ai": {"model": "  ", "base_url": "http://evil.example.com"}}"#,
        )
        .unwrap();
        let ai = load(&path, &Legacy::default()).settings.ai;
        assert_eq!(ai, super::super::AiSettings::default());
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn an_unsafe_address_in_the_file_is_replaced_on_load_for_each_provider() {
        let dir = temp_dir();
        let path = dir.join(FILE_NAME);
        std::fs::write(
            &path,
            format!(
                r#"{{"schema_version": {SCHEMA_VERSION}, "ai": {{"effort": "max", "model": "stray",
                    "providers": {{
                      "anthropic": {{"model": "", "base_url": "http://evil.example.com"}},
                      "openai": {{"model": "gpt-x", "base_url": "http://evil.example.com/v1"}},
                      "openai_compatible": {{"base_url": "http://10.0.0.5:1234/v1"}},
                      "ollama": {{"model": "qwen3:8b", "base_url": "http://localhost:9999/v1"}}}}}}}}"#
            ),
        )
        .unwrap();
        let ai = load(&path, &Legacy::default()).settings.ai;
        assert_eq!(ai.providers.anthropic.base_url, crate::ai::DEFAULT_BASE_URL);
        assert_eq!(ai.providers.anthropic.model, engine_ai::DEFAULT_MODEL);
        assert_eq!(ai.providers.openai.base_url, engine_ai::OPENAI_BASE_URL);
        assert_eq!(ai.providers.openai.model, "gpt-x", "a chosen model is kept");
        assert_eq!(ai.providers.openai_compatible.base_url, "");
        // A loopback address is the user's to choose.
        assert_eq!(ai.providers.ollama.base_url, "http://localhost:9999/v1");
        assert_eq!(ai.providers.ollama.model, "qwen3:8b");
        assert_eq!(ai.effort, engine_ai::Effort::Max);
        // A version-5 file has no top-level model; a stray one is not read.
        assert_eq!(ai.providers.anthropic.model, engine_ai::DEFAULT_MODEL);
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn edited_connection_types_and_a_lost_built_in_survive_a_reload() {
        let dir = temp_dir();
        let path = dir.join(FILE_NAME);
        let mut settings = Settings::default();
        settings.connection_types[2].read_only = true;
        settings
            .connection_types
            .push(super::super::connection::ConnectionType {
                id: "custom-1".into(),
                name: "Staging".into(),
                ..Default::default()
            });
        save(&path, &settings).unwrap();
        assert_eq!(load(&path, &Legacy::default()).settings, settings);

        // Someone removed Test from the file by hand.
        settings.connection_types.remove(1);
        save(&path, &settings).unwrap();
        let reloaded = load(&path, &Legacy::default()).settings;
        assert!(reloaded.connection_types.iter().any(|ty| ty.id == "test"));
        assert!(reloaded
            .connection_types
            .iter()
            .any(|ty| ty.id == "custom-1" && ty.name == "Staging"));
        assert!(
            reloaded
                .connection_types
                .iter()
                .find(|ty| ty.id == "production")
                .unwrap()
                .read_only
        );
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn shortcut_overrides_persist_and_unknown_ids_load_without_error() {
        let dir = temp_dir();
        let path = dir.join(FILE_NAME);
        let mut settings = Settings::default();
        settings.shortcuts.insert(
            "workspace.close_tab".into(),
            vec!["secondary-shift-w".into()],
        );
        // An unbound action is an empty list, not an absent entry.
        settings
            .shortcuts
            .insert("workspace.open_settings".into(), Vec::new());
        save(&path, &settings).unwrap();
        let loaded = load(&path, &Legacy::default());
        assert!(
            loaded.notice.is_none() && !loaded.needs_save,
            "no migration needed"
        );
        assert_eq!(loaded.settings.shortcuts, settings.shortcuts);

        // A file from another build may name actions this one lacks, and an
        // older file has no shortcuts at all.
        std::fs::write(
            &path,
            format!(
                r#"{{"schema_version": {SCHEMA_VERSION}, "shortcuts": {{"future.action": ["secondary-x"], "workspace.close_tab": ["secondary-q"]}}}}"#
            ),
        )
        .unwrap();
        let loaded = load(&path, &Legacy::default());
        assert!(loaded.notice.is_none());
        assert_eq!(loaded.settings.shortcuts.len(), 2);
        std::fs::write(&path, format!(r#"{{"schema_version": {SCHEMA_VERSION}}}"#)).unwrap();
        assert!(load(&path, &Legacy::default())
            .settings
            .shortcuts
            .is_empty());
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn a_missing_file_without_legacy_data_yields_defaults_to_write() {
        let dir = temp_dir();
        let loaded = load(&dir.join(FILE_NAME), &Legacy::default());
        assert_eq!(loaded.settings, Settings::default());
        assert!(loaded.needs_save);
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn migrations_run_once_and_never_overwrite_later_edits() {
        let dir = temp_dir();
        let sidebar = dir.join("sidebar-preferences.json");
        std::fs::write(&sidebar, SIDEBAR_SAMPLE).unwrap();
        let path = dir.join(FILE_NAME);
        let mut settings = load(&path, &Legacy::read(Some(&sidebar))).settings;
        settings.navigation.connection_sort = ConnectionSort::Ascending;
        save(&path, &settings).unwrap();

        let reloaded = load(&path, &Legacy::read(Some(&sidebar)));
        assert!(!reloaded.needs_save);
        assert_eq!(
            reloaded.settings.navigation.connection_sort,
            ConnectionSort::Ascending
        );
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn an_old_file_is_migrated_and_marked_for_saving() {
        let dir = temp_dir();
        let path = dir.join(FILE_NAME);
        std::fs::write(
            &path,
            r#"{"schema_version": 0, "results": {"null_text": "-"}}"#,
        )
        .unwrap();
        let loaded = load(&path, &Legacy::default());
        assert_eq!(loaded.settings.schema_version, SCHEMA_VERSION);
        assert_eq!(loaded.settings.results.null_text, "-");
        assert!(loaded.needs_save);
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn atomic_save_round_trips_and_leaves_no_temp_files() {
        let dir = temp_dir();
        let path = dir.join("nested").join(FILE_NAME);
        let mut settings = Settings::default();
        settings.results.null_text = "∅".into();
        save(&path, &settings).unwrap();
        save(&path, &settings).unwrap();

        let loaded = load(&path, &Legacy::default());
        assert_eq!(loaded.settings, settings);
        assert!(!loaded.needs_save);
        let leftovers: Vec<_> = std::fs::read_dir(path.parent().unwrap())
            .unwrap()
            .map(|entry| entry.unwrap().file_name().to_string_lossy().into_owned())
            .collect();
        assert_eq!(leftovers, vec![FILE_NAME.to_string()]);
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn a_failed_save_reports_the_error_and_keeps_the_old_file() {
        let dir = temp_dir();
        let path = dir.join(FILE_NAME);
        save(&path, &Settings::default()).unwrap();
        // The target is a directory: the rename cannot replace it.
        let blocked = dir.join("blocked");
        std::fs::create_dir_all(blocked.join("settings.json")).unwrap();
        assert!(save(&blocked.join("settings.json"), &Settings::default()).is_err());
        assert!(load(&path, &Legacy::default()).notice.is_none());
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn a_corrupt_file_is_set_aside_and_defaults_load() {
        let dir = temp_dir();
        let path = dir.join(FILE_NAME);
        std::fs::write(&path, "{ not json").unwrap();
        let loaded = load(&path, &Legacy::default());
        assert_eq!(loaded.settings, Settings::default());
        assert!(loaded.notice.is_some() && loaded.needs_save);
        assert!(!path.exists());
        assert!(dir.join("settings.json.corrupt").exists());
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn a_newer_format_loads_but_is_not_rewritten() {
        let dir = temp_dir();
        let path = dir.join(FILE_NAME);
        std::fs::write(
            &path,
            r#"{"schema_version": 99, "editor": {"soft_wrap": true}}"#,
        )
        .unwrap();
        let loaded = load(&path, &Legacy::default());
        assert!(loaded.settings.editor.soft_wrap);
        assert!(loaded.notice.is_some() && !loaded.needs_save);
        std::fs::remove_dir_all(dir).unwrap();
    }
}
