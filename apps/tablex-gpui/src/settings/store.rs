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
use engine_connect::profile::{AgentLevel, MCP_WRITES_IGNORED_FROM_SCHEMA};

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
    Migration {
        to: 6,
        apply: run_the_current_statement,
    },
    Migration {
        to: 7,
        apply: merge_mcp_writes_into_agent_access,
    },
];

/// 7: "Let MCP clients run changes" is gone; the access level decides, and every change is asked
/// about one by one. The file keeps `ai.mcp_writes: true` so an older `tablex-mcp` paired with this
/// app still allows writes on a Read and write connection. When the switch was off, a connection
/// that is set to Read and write could not be changed through MCP until now: the next start tells
/// the user which ones (once, see `access_notice_pending`).
fn merge_mcp_writes_into_agent_access(settings: &mut Settings, _: &Legacy) {
    // With the switch on, a hand-edited Read and write default or type preset (capped at Read until
    // now) widens as well: the user is told about those too.
    let widened = settings.ai.agent_access == AgentLevel::ReadWrite
        || settings.connection_types.iter().any(|ty| ty.agent_access == AgentLevel::ReadWrite);
    settings.ai.access_notice_pending = !settings.ai.mcp_writes || widened;
    settings.ai.mcp_writes = true;
}

/// `ai.mcp_writes` as the file says it (`None`: no such key, or a file that is not an object).
fn mcp_writes_key(text: &str) -> Option<bool> {
    serde_json::from_str::<serde_json::Value>(text).ok()?.get("ai")?.get("mcp_writes")?.as_bool()
}

/// 6: Run / Ctrl+Enter runs the statement at the cursor, as in other database tools, and Run Script
/// (Option+X) the whole script. Earlier versions wrote "Whole script" into every file as the
/// default, so a file that has it gets the new default once.
fn run_the_current_statement(settings: &mut Settings, _: &Legacy) {
    if settings.editor.run_scope == super::RunScope::All {
        settings.editor.run_scope = super::RunScope::Current;
    }
}

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
        // Nothing was switched off by the user: there is nothing to tell them about.
        settings.ai.access_notice_pending = false;
        Loaded {
            settings,
            notice,
            needs_save: true,
        }
    };
    let text = match std::fs::read_to_string(path) {
        Ok(text) => text,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return fresh(None),
        // Not text (damaged): the same as text that is not JSON. Left in place, the next save
        // would replace it.
        Err(error) if error.kind() == std::io::ErrorKind::InvalidData => {
            return set_aside(path, &error, fresh);
        }
        Err(error) => {
            return Loaded {
                notice: Some(format!("Could not read the settings: {error}")),
                needs_save: false,
                ..fresh(None)
            }
        }
    };
    match serde_json::from_str::<Settings>(&text) {
        Ok(mut settings) if settings.schema_version > SCHEMA_VERSION => {
            // The key is `true` from schema 7 on; a file without it reads `false` (the default of
            // the older switch), which a save would then write down as the user's "no".
            if mcp_writes_key(&text).is_none() {
                settings.ai.mcp_writes = true;
            }
            Loaded {
                notice: Some(format!(
                    "The settings were written by a newer version of tablex (format {}); unknown settings are ignored",
                    settings.schema_version
                )),
                needs_save: false,
                settings,
            }
        }
        Ok(mut settings) => {
            let stale = settings.schema_version < SCHEMA_VERSION;
            let from_schema_7 = u64::from(settings.schema_version) >= MCP_WRITES_IGNORED_FROM_SCHEMA;
            let switched_off = from_schema_7 && mcp_writes_key(&text) == Some(false);
            migrate(&mut settings, legacy);
            if from_schema_7 {
                settings.ai.mcp_writes = true;
            }
            // `tablex-mcp` honours an explicit `false` at any schema: the old switch, turned off by an
            // older app that opened a file a newer one wrote (a downgrade and back). This app has no
            // switch to show it: it is put back to `true`, and the one-time notice says what is
            // allowed now, so the user can decide again.
            if switched_off {
                settings.ai.access_notice_pending = true;
            }
            Loaded {
                settings,
                notice: None,
                needs_save: stale || switched_off,
            }
        }
        Err(error) => {
            // Keep the unreadable file for the user; the next save must not
            // destroy it silently.
            set_aside(path, &error, fresh)
        }
    }
}

/// An unreadable settings file is moved aside, and the notice says so only when that worked. When
/// it did not, the file stays where it is and is not written over at once.
fn set_aside(path: &Path, error: &dyn std::fmt::Display, fresh: impl FnOnce(Option<String>) -> Loaded) -> Loaded {
    match gpui_session::quarantine(path) {
        Ok(aside) => fresh(Some(format!(
            "The settings file is corrupted and was set aside as {}: {error}",
            aside.file_name().map_or_else(String::new, |name| name.to_string_lossy().into_owned())
        ))),
        Err(moved) => Loaded {
            needs_save: false,
            ..fresh(Some(format!(
                "The settings file is corrupted ({error}) and could not be set aside ({moved}); it was left as it is"
            )))
        },
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
    fn a_version_five_file_runs_the_current_statement_once_and_keeps_a_later_choice() {
        let dir = temp_dir();
        let path = dir.join(FILE_NAME);
        std::fs::write(&path, r#"{"schema_version": 5, "editor": {"run_scope": "all", "soft_wrap": true}}"#).unwrap();
        let loaded = load(&path, &Legacy::default());
        assert!(loaded.needs_save);
        assert_eq!(loaded.settings.editor.run_scope, super::super::RunScope::Current);
        assert!(loaded.settings.editor.soft_wrap);
        // "Selection" was a choice: it stays.
        std::fs::write(&path, r#"{"schema_version": 5, "editor": {"run_scope": "selection"}}"#).unwrap();
        assert_eq!(load(&path, &Legacy::default()).settings.editor.run_scope, super::super::RunScope::Selection);
        // Whole script chosen again after the upgrade is kept.
        std::fs::write(&path, format!(r#"{{"schema_version": {SCHEMA_VERSION}, "editor": {{"run_scope": "all"}}}}"#)).unwrap();
        assert_eq!(load(&path, &Legacy::default()).settings.editor.run_scope, super::super::RunScope::All);
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
        // (An upgrade from before the switch had it off: the notice is flagged, see migration 7.)
        let flagged = super::super::AiSettings { access_notice_pending: true, ..Default::default() };
        assert_eq!(loaded.settings.ai, flagged);
        assert!(loaded.settings.editor.soft_wrap);
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn a_version_three_file_goes_through_every_step_and_keeps_everything_else() {
        let dir = temp_dir();
        let path = dir.join(FILE_NAME);
        // A version-3 file: everything else is the user's. A stray `mcp_writes` is not trusted at 4
        // (the switch is off), and step 7 then sets the hidden key to `true` and flags the notice.
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
        assert_eq!(SCHEMA_VERSION, 7);
        assert!(settings.ai.mcp_writes && settings.ai.access_notice_pending);
        assert!(settings.editor.soft_wrap && settings.results.page_size == 321);
        // Version 3 -> 5: the model of the file ends up under Anthropic.
        assert_eq!(settings.ai.providers.anthropic.model, "claude-haiku-5-5");
        assert_eq!(settings.ai.effort, engine_ai::Effort::Max);
        assert!(!settings.ai.keep_conversations);
        assert_eq!(settings.shortcuts["workspace.close_tab"], ["secondary-q"]);
        // A file without the key (a plain version-3 file) is the same.
        std::fs::write(
            &path,
            r#"{"schema_version": 3, "ai": {"model": "claude-haiku-5-5"}}"#,
        )
        .unwrap();
        let ai = load(&path, &Legacy::default()).settings.ai;
        assert!(ai.mcp_writes && ai.access_notice_pending);
        // At the current version nothing is touched.
        std::fs::write(
            &path,
            format!(r#"{{"schema_version": {SCHEMA_VERSION}, "ai": {{"mcp_writes": true}}}}"#),
        )
        .unwrap();
        let loaded = load(&path, &Legacy::default());
        assert!(loaded.settings.ai.mcp_writes && !loaded.settings.ai.access_notice_pending);
        assert!(!loaded.needs_save);
        assert!(engine_connect::profile::mcp_writes_enabled(Some(&path)));
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn a_version_six_file_with_the_switch_off_gets_it_on_and_the_notice_flagged_once() {
        let dir = temp_dir();
        let path = dir.join(FILE_NAME);
        let write = |switch: &str| {
            std::fs::write(
                &path,
                format!(
                    r#"{{"schema_version": 6, "ai": {{{switch} "agent_access": "read"}}, "editor": {{"soft_wrap": true}}}}"#
                ),
            )
            .unwrap();
        };
        // Off (or absent: the default was off): on after the step, and the notice is flagged.
        for switch in [r#""mcp_writes": false,"#, ""] {
            write(switch);
            // A new `tablex-mcp` ignores the key at 7; an older one would still have blocked at 6.
            assert!(!engine_connect::profile::mcp_writes_enabled(Some(&path)));
            let loaded = load(&path, &Legacy::default());
            assert!(loaded.needs_save && loaded.notice.is_none());
            let ai = &loaded.settings.ai;
            assert!(ai.mcp_writes && ai.access_notice_pending, "{switch}");
            // What was chosen stays.
            assert_eq!(ai.agent_access, engine_connect::profile::AgentLevel::Read);
            assert!(loaded.settings.editor.soft_wrap);
            // Written back, the file keeps the key `true` for an older `tablex-mcp`, at version 7.
            save(&path, &loaded.settings).unwrap();
            let written: serde_json::Value =
                serde_json::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
            assert_eq!(written["schema_version"], 7);
            assert_eq!(written["ai"]["mcp_writes"], true);
            assert_eq!(written["ai"]["access_notice_pending"], true);
            assert!(engine_connect::profile::mcp_writes_enabled(Some(&path)));
            // Loading it again does not flag anything new or migrate again.
            let again = load(&path, &Legacy::default());
            assert!(!again.needs_save && again.settings == loaded.settings);
        }
        // On already: nothing to tell.
        write(r#""mcp_writes": true,"#);
        let loaded = load(&path, &Legacy::default());
        assert!(loaded.needs_save);
        assert!(loaded.settings.ai.mcp_writes && !loaded.settings.ai.access_notice_pending);
        save(&path, &loaded.settings).unwrap();
        let written = std::fs::read_to_string(&path).unwrap();
        assert!(!written.contains("access_notice_pending"), "a cleared flag is not written");
        // A fresh file has nothing to tell either.
        std::fs::remove_file(&path).unwrap();
        let fresh = load(&path, &Legacy::default());
        assert!(fresh.settings.ai.mcp_writes && !fresh.settings.ai.access_notice_pending);
        assert_eq!(fresh.settings.ai, super::super::AiSettings::default());
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn an_old_switch_turned_off_after_a_downgrade_is_put_back_and_flagged_again() {
        let dir = temp_dir();
        let path = dir.join(FILE_NAME);
        // 0.1.11 wrote schema 7 with the key `true`; an older app turned the switch off and kept the
        // version number: `tablex-mcp` honours the `false`, and the app puts it back with the notice.
        std::fs::write(
            &path,
            format!(r#"{{"schema_version": {SCHEMA_VERSION}, "ai": {{"mcp_writes": false, "agent_access": "read"}}}}"#),
        )
        .unwrap();
        assert!(!engine_connect::profile::mcp_writes_enabled(Some(&path)), "the reader honours the false");
        let loaded = load(&path, &Legacy::default());
        assert!(loaded.settings.ai.mcp_writes && loaded.settings.ai.access_notice_pending);
        assert!(loaded.needs_save, "written back with the key true");
        save(&path, &loaded.settings).unwrap();
        assert!(engine_connect::profile::mcp_writes_enabled(Some(&path)));
        // Once shown (the flag cleared) and written, it does not come back.
        let mut shown = loaded.settings.clone();
        shown.ai.access_notice_pending = false;
        save(&path, &shown).unwrap();
        let again = load(&path, &Legacy::default());
        assert!(!again.settings.ai.access_notice_pending && !again.needs_save);
        // A file without the key at schema 7 is nothing to put back.
        std::fs::write(&path, format!(r#"{{"schema_version": {SCHEMA_VERSION}, "ai": {{}}}}"#)).unwrap();
        let loaded = load(&path, &Legacy::default());
        assert!(!loaded.settings.ai.access_notice_pending && !loaded.needs_save);
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn a_hand_edited_read_write_default_or_type_flags_the_notice_even_with_the_switch_on() {
        let dir = temp_dir();
        let path = dir.join(FILE_NAME);
        let load_text = |text: &str| {
            std::fs::write(&path, text).unwrap();
            load(&path, &Legacy::default()).settings.ai.access_notice_pending
        };
        // Switch on, and a Read and write global default: it widens now.
        assert!(load_text(r#"{"schema_version": 6, "ai": {"mcp_writes": true, "agent_access": "read_write"}}"#));
        // A type preset of Read and write.
        assert!(load_text(
            r##"{"schema_version": 6, "ai": {"mcp_writes": true},
                "connection_types": [{"id": "dev", "name": "Dev", "color": "#000000", "agent_access": "read_write"}]}"##
        ));
        // Switch on and nothing at Read and write: nothing to tell.
        assert!(!load_text(r#"{"schema_version": 6, "ai": {"mcp_writes": true, "agent_access": "read"}}"#));
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn a_hand_edited_read_write_default_is_kept_as_it_is() {
        let dir = temp_dir();
        let path = dir.join(FILE_NAME);
        std::fs::write(
            &path,
            format!(r#"{{"schema_version": {SCHEMA_VERSION}, "ai": {{"agent_access": "read_write"}}}}"#),
        )
        .unwrap();
        let loaded = load(&path, &Legacy::default());
        assert_eq!(loaded.settings.ai.agent_access, engine_connect::profile::AgentLevel::ReadWrite);
        assert!(!loaded.needs_save);
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
        assert_eq!(loaded.settings.schema_version, SCHEMA_VERSION);
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
        assert_eq!(written["schema_version"], SCHEMA_VERSION);
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
        let flagged = super::super::AiSettings { access_notice_pending: true, ..Default::default() };
        assert_eq!(ai, flagged);
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
    fn a_second_corrupt_file_never_replaces_the_first() {
        let dir = temp_dir();
        let path = dir.join(FILE_NAME);
        std::fs::write(&path, "{ first").unwrap();
        load(&path, &Legacy::default());
        std::fs::write(&path, "{ second").unwrap();
        let loaded = load(&path, &Legacy::default());
        assert_eq!(std::fs::read(dir.join("settings.json.corrupt")).unwrap(), b"{ first");
        let aside: Vec<_> = std::fs::read_dir(&dir)
            .unwrap()
            .map(|entry| entry.unwrap().file_name().to_string_lossy().into_owned())
            .filter(|name| name.ends_with(".corrupt") && name != "settings.json.corrupt")
            .collect();
        assert_eq!(aside.len(), 1, "{aside:?}");
        assert_eq!(std::fs::read(dir.join(&aside[0])).unwrap(), b"{ second");
        assert!(loaded.notice.unwrap().contains(&aside[0]), "the notice names the file it went to");
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[cfg(unix)]
    #[test]
    fn a_file_that_cannot_be_set_aside_is_left_alone_and_not_claimed_to_be() {
        use std::os::unix::fs::PermissionsExt;
        let dir = temp_dir();
        let path = dir.join(FILE_NAME);
        std::fs::write(&path, "{ not json").unwrap();
        // A read-only directory: nothing can be renamed in it.
        std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o500)).unwrap();
        let blocked = std::fs::File::create(dir.join("probe")).is_err();
        let loaded = load(&path, &Legacy::default());
        std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o700)).unwrap();
        if blocked {
            assert!(path.exists(), "the original is still there");
            assert!(!loaded.needs_save, "and is not written over at once");
            let notice = loaded.notice.unwrap();
            assert!(notice.contains("could not be set aside") && !notice.contains("was set aside"), "{notice}");
        }
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
