//! Upgrade compatibility of what the app keeps for the user: the data dir of every earlier release
//! (`fixtures/compat/<version>/`, made-up data, see `fixtures/compat/README.md`) is read with the
//! current code, every value must come out as it went in, and a save followed by a load must lose
//! nothing. The checks are on the *files as that release wrote them*, so a renamed field, a changed
//! enum name, a new field without a default or a changed key format fails here instead of in a
//! user's data dir. Never point these tests at a real data dir or at the OS credential store.
//!
//! `generate_fixture_set` (ignored) writes the files of the current build, for the fixtures of the
//! release being made (the README has the steps). The app database of the set is made by
//! `engine-connect/tests/upgrade_compat.rs`; this one adds the documents the app owns to it.

use crate::aggregate::{AggregateOptions, DisabledStage, EditorMode};
use crate::error_log::{Area, LogStore, Severity, StoredEntry};
use crate::gpui_session::{self, GpuiSession, LoadOutcome, SessionAggregate, SessionGroup, SessionTab, SessionTree};
use crate::navigator::{NavigatorNodeId, ObjectGroup};
use crate::panes::{PaneLayoutSpec, PaneNodeSpec, SplitAxis, StripSettings, TabGroupKey};
use crate::pinned_column::PinnedColumn;
use crate::query_mode::QueryMode;
use crate::settings::registry::{self, Kind, SettingDescriptor, Value as SettingValue};
use crate::settings::{store, AiProvider, ProviderEntry, Settings};
use crate::sidebar_preferences::{
    Bookmark, BookmarkTarget, ConnectionDisplay, ConnectionGroup, ConnectionSort, FolderFilter, FolderKey, FolderKind, SidebarPreferences,
    TabGroupBy, VisibleObjectFilter,
};
use crate::sql_filter::{SqlFilterConjunction, SqlFilterMode, SqlFilterRule};
use crate::workspace::WorkspaceState;
use engine_driver_api::sql::{SchemaObjectKind, SchemaObjectRef};
use engine_storage::documents;
use serde_json::{json, Value};
use std::collections::{BTreeMap, HashMap};
use std::path::{Path, PathBuf};

/// The releases whose fixture sets are checked. Append the version when its fixtures are added.
const VERSIONS: &[&str] = &["v0.1.10", "v0.1.11"];

/// `(version, json path)` of values a later migration rewrites on purpose (a settings migration, say):
/// the survival checks skip them. Nothing is listed while no migration touches a released value.
const MIGRATED: &[(&str, &str)] = &[];

/// The connections of the fixtures as the app names them (lowercase); the same ones as in
/// `engine-connect/tests/upgrade_compat.rs`, which holds their records.
const SHOP: &str = "aaaaaaaa-0000-4000-8000-000000000001";
const ANALYTICS: &str = "aaaaaaaa-0000-4000-8000-000000000003";
const ORDERS: &str = "aaaaaaaa-0000-4000-8000-000000000004";
const CACHE: &str = "aaaaaaaa-0000-4000-8000-000000000005";

fn fixture_root(version: &str) -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../../fixtures/compat").join(version)
}

fn copy_dir(from: &Path, to: &Path) {
    std::fs::create_dir_all(to).unwrap();
    for entry in std::fs::read_dir(from).unwrap() {
        let entry = entry.unwrap();
        let target = to.join(entry.file_name());
        if entry.file_type().unwrap().is_dir() {
            copy_dir(&entry.path(), &target);
        } else {
            std::fs::copy(entry.path(), target).unwrap();
        }
    }
}

/// A private copy of a release's fixture dir (the code under test writes: never the checked-in files).
fn copy_of(version: &str) -> tempfile::TempDir {
    let root = fixture_root(version);
    assert!(root.join("tablex").is_dir(), "{version}: missing fixture dir {}", root.display());
    let dir = tempfile::tempdir().unwrap();
    copy_dir(&root, dir.path());
    dir
}

/// The value at a dotted path.
fn lookup<'a>(value: &'a Value, path: &str) -> Option<&'a Value> {
    path.split('.').try_fold(value, |at, part| at.get(part))
}

/// The dotted paths where two JSON documents differ.
fn changed_places(path: &str, before: &Value, after: &Value, places: &mut Vec<String>) {
    match (before, after) {
        (Value::Object(before), Value::Object(after)) => {
            for (key, value) in after {
                let here = if path.is_empty() { key.clone() } else { format!("{path}.{key}") };
                match before.get(key) {
                    Some(old) => changed_places(&here, old, value, places),
                    None => places.push(here),
                }
            }
        }
        _ if before != after => places.push(path.to_owned()),
        _ => {}
    }
}

fn read_json(path: &Path) -> Value {
    serde_json::from_str(&std::fs::read_to_string(path).unwrap_or_else(|error| panic!("{}: {error}", path.display()))).unwrap()
}

/// Every value of `old` is in `new` at the same place: no key was dropped or renamed, no value
/// changed, arrays and scalars equal. `new` may hold more (a later release adds fields).
fn assert_survives(version: &str, file: &str, old: &Value, new: &Value) {
    fn walk(version: &str, file: &str, path: &str, old: &Value, new: &Value) {
        if MIGRATED.iter().any(|(v, p)| *v == version && *p == path) {
            return;
        }
        match (old, new) {
            (Value::Object(old), Value::Object(new)) => {
                for (key, old_value) in old {
                    let here = if path.is_empty() { key.clone() } else { format!("{path}.{key}") };
                    match new.get(key) {
                        Some(new_value) => walk(version, file, &here, old_value, new_value),
                        None => panic!("{version}: {file}: `{here}` was in the file and is gone after load and save"),
                    }
                }
            }
            _ => assert_eq!(old, new, "{version}: {file}: `{path}` changed on load and save"),
        }
    }
    walk(version, file, "", old, new);
}

// ---- the data of the fixture sets ----------------------------------------------------------------

/// A value of the setting different from its default that the kind accepts.
fn other_value(descriptor: &SettingDescriptor) -> SettingValue {
    match (&descriptor.kind, &descriptor.default) {
        (Kind::Toggle, SettingValue::Bool(value)) => SettingValue::Bool(!value),
        (Kind::Number { min, max, .. }, SettingValue::Number(value)) => {
            SettingValue::Number(if (value - min).abs() > f64::EPSILON { *min } else { *max })
        }
        (Kind::Text { .. } | Kind::Font | Kind::Model, SettingValue::Text(value)) => SettingValue::Text(format!("{value}x")),
        (Kind::Choice { options }, SettingValue::Choice(value)) => {
            SettingValue::Choice(options.iter().map(|(id, _)| *id).rev().find(|id| id != value).unwrap())
        }
        other => panic!("{}: default does not fit its kind: {other:?}", descriptor.id),
    }
}

/// Settings with every setting of the page away from its default, the AI section, a custom
/// connection type and shortcut overrides filled in.
fn populated_settings() -> Settings {
    use crate::settings::registry::Section;
    let mut settings = Settings::default();
    for descriptor in registry::registry().iter().filter(|descriptor| descriptor.section != Section::Ai) {
        descriptor.set(&mut settings, other_value(descriptor)).unwrap_or_else(|error| panic!("{}: {error}", descriptor.id));
    }
    settings.ai.provider = AiProvider::OpenAiCompatible;
    settings.ai.providers.anthropic = ProviderEntry { model: "claude-example-1".into(), base_url: "https://api.anthropic.com".into() };
    settings.ai.providers.openai = ProviderEntry { model: "gpt-example".into(), base_url: "https://api.openai.com/v1".into() };
    settings.ai.providers.openai_compatible = ProviderEntry { model: "llm-example".into(), base_url: "https://llm.example.com/v1".into() };
    settings.ai.providers.ollama = ProviderEntry { model: "qwen-example".into(), base_url: "http://localhost:11434/v1".into() };
    settings.ai.effort = engine_ai::Effort::High;
    settings.ai.fallback = false;
    settings.ai.keep_conversations = false;
    settings.ai.mcp_writes = true;
    settings.ai.agent_access = engine_connect::profile::AgentLevel::Read;
    settings.connection_types.push(engine_connect::profile::ConnectionType {
        id: "staging".into(),
        name: "Staging".into(),
        color: "#f59e0b".into(),
        confirm_execute: true,
        read_only: true,
        auto_commit: false,
        smart_commit: true,
        idle_transaction_secs: Some(120),
        agent_access: engine_connect::profile::AgentLevel::Schema,
        ..Default::default()
    });
    settings.shortcuts = BTreeMap::from([
        ("workspace.close_tab".to_owned(), vec!["secondary-shift-w".to_owned()]),
        ("workspace.toggle_assistant".to_owned(), vec![]),
        // An id this build does not know (an older or newer one wrote it): kept.
        ("example.removed_action".to_owned(), vec!["secondary-alt-k".to_owned(), "f13".to_owned()]),
    ]);
    settings
}

fn populated_sidebar() -> SidebarPreferences {
    let display = |id: &str, group: Option<&str>, order: usize, color: Option<&str>, hidden: bool| ConnectionDisplay {
        id: id.into(),
        group: group.map(str::to_owned),
        order,
        color: color.map(str::to_owned),
        hidden,
    };
    SidebarPreferences {
        groups: vec![
            ConnectionGroup { id: "work".into(), name: "Work".into(), parent: None, order: 0 },
            ConnectionGroup { id: "work-data".into(), name: "Data".into(), parent: Some("work".into()), order: 0 },
        ],
        connections: vec![
            display(SHOP, Some("work"), 0, Some("#e11d48"), false),
            display(ANALYTICS, Some("work-data"), 0, Some("#0ea5e9"), false),
            display(ORDERS, None, 1, None, true),
            display(CACHE, None, 2, Some("#f59e0b"), false),
        ],
        sort: ConnectionSort::Descending,
        sidebar_width: Some(312.5),
        assistant_open: true,
        assistant_width: Some(404.0),
        visible_filters: HashMap::from([(
            SHOP.to_owned(),
            VisibleObjectFilter {
                databases: Some(vec!["shop".into(), "reporting".into()]),
                patterns: vec!["^app_.*".into()],
                schemas: HashMap::from([("shop".to_owned(), Some(vec!["public".to_owned()])), ("reporting".to_owned(), None)]),
                show_system: false,
            },
        )]),
        folder_filters: vec![FolderFilter {
            key: FolderKey {
                connection: SHOP.into(),
                database: "shop".into(),
                schema: "public".into(),
                folder: FolderKind::Objects(ObjectGroup::Tables),
            },
            include: vec!["order*".into()],
            exclude: vec!["*_old".into()],
        }],
        bookmarks: vec![Bookmark {
            id: "bookmark-1".into(),
            name: "Orders".into(),
            target: BookmarkTarget::Object { connection: SHOP.into(), database: "shop".into(), schema: "public".into(), object: "orders".into() },
        }],
        global_search: true,
        tab_group_by: TabGroupBy::Database,
        compact_tab_titles: true,
        tab_rows: true,
        default_databases: HashMap::from([(SHOP.to_owned(), "shop".to_owned())]),
    }
}

fn populated_session() -> GpuiSession {
    let tab = |kind: &str, title: &str, connection: &str, database: &str| SessionTab {
        kind: kind.into(),
        title: title.into(),
        connection_id: connection.into(),
        database: database.into(),
        ..Default::default()
    };
    let tabs = vec![
        tab("welcome", "Welcome", "", ""),
        SessionTab {
            schema: Some("public".into()),
            is_pinned: true,
            sql: "SELECT date_trunc('month', created_at) AS month, sum(total)\nFROM orders\nGROUP BY 1".into(),
            output_limit: 500,
            file_path: Some("/Users/example/sql/revenue.sql".into()),
            file_saved: Some("SELECT 1 -- as saved".into()),
            ..tab("query", "Revenue by month", SHOP, "shop")
        },
        SessionTab {
            schema: Some("public".into()),
            collection: "orders".into(),
            query_mode: Some(QueryMode::SqlTable),
            table_where: "total > 10".into(),
            table_order_by: "id DESC".into(),
            table_where_draft: Some("total > 20".into()),
            table_filter_rules: vec![SqlFilterRule {
                id: 1,
                column: "status".into(),
                mode: SqlFilterMode::NotEquals,
                value: "cancelled".into(),
                end_value: String::new(),
                conjunction: SqlFilterConjunction::Or,
                disabled: true,
            }],
            table_filter_where: Some("status <> 'cancelled'".into()),
            table_hidden_columns: vec!["internal_note".into()],
            pinned_columns: vec![PinnedColumn::new("id", 0), PinnedColumn::new("total", 1)],
            ..tab("query", "orders", SHOP, "shop")
        },
        SessionTab {
            collection: "carts".into(),
            query_mode: Some(QueryMode::MongoAggregate),
            aggregate: Some(SessionAggregate {
                mode: EditorMode::Stages,
                preview: false,
                text: r#"[{"$match":{"open":true}},{"$limit":10}]"#.into(),
                options: AggregateOptions {
                    sample_size: 50,
                    max_time_ms: 5000,
                    allow_disk_use: true,
                    collation: r#"{ locale: "en" }"#.into(),
                    disabled: vec![DisabledStage { at: 1, operator: "$limit".into(), body: "10".into() }],
                    // Only saved rows hold the shell text; a tab's session state does not.
                    shell_text: None,
                },
                name: Some("Open carts".into()),
                saved_id: Some("agg-1".into()),
            }),
            ..tab("query", "Open carts", ORDERS, "orders")
        },
        tab("connection", "Shop", SHOP, ""),
        SessionTab { schema: Some("public".into()), ..tab("diagram", "shop diagram", SHOP, "shop") },
        SessionTab {
            source: Some(SchemaObjectRef {
                database: Some("shop".into()),
                schema: Some("public".into()),
                kind: SchemaObjectKind::Function,
                name: "total_for".into(),
                arguments: Some("integer".into()),
                table: None,
                identity: Some("oid:16401".into()),
            }),
            ..tab("source", "total_for", SHOP, "shop")
        },
        SessionTab { collection: "a2V5LTE=".into(), query_mode: Some(QueryMode::RedisKey), ..tab("query", "key-1", CACHE, "0") },
        SessionTab { query_mode: Some(QueryMode::Unknown("futureMode".into())), ..tab("query", "From the future", SHOP, "shop") },
    ];
    GpuiSession {
        tabs,
        active: Some(1),
        collapsed_groups: vec![SessionGroup { connection_id: SHOP.into(), database: String::new() }],
        panes: Some(PaneLayoutSpec {
            root: PaneNodeSpec::Split {
                axis: SplitAxis::Row,
                ratios: vec![600, 400],
                children: vec![
                    PaneNodeSpec::Pane {
                        tabs: vec![0, 1, 2, 3],
                        active: Some(1),
                        // Grouped by connection: the group has no database.
                        collapsed: vec![TabGroupKey { connection_id: SHOP.into(), database: String::new() }],
                        strip: Some(StripSettings { group_by: TabGroupBy::Connection, rows: true }),
                    },
                    PaneNodeSpec::Pane { tabs: vec![4, 5, 6, 7, 8], active: Some(5), collapsed: Vec::new(), strip: None },
                ],
            },
            focused: 0,
        }),
        tree: vec![SessionTree {
            connection_id: SHOP.into(),
            nodes: [
                NavigatorNodeId::Connection { connection: SHOP.into() },
                NavigatorNodeId::Database { connection: SHOP.into(), database: "shop".into() },
                NavigatorNodeId::ObjectGroup { connection: SHOP.into(), database: "shop".into(), schema: "public".into(), group: ObjectGroup::Tables },
            ]
            .iter()
            .map(|node| serde_json::to_value(node).unwrap())
            .collect(),
        }],
        ..Default::default()
    }
}

fn log_entries() -> Vec<StoredEntry> {
    let entry = |at_ms: i64, severity, area, message: &str, repeat: u32| StoredEntry {
        at_ms,
        severity,
        area,
        connection_id: Some(SHOP.into()),
        connection_name: Some("Shop (production)".into()),
        code: Some("42P01".into()),
        retryable: false,
        message: message.into(),
        details: Some("detail line".into()),
        repeat,
    };
    vec![
        entry(1_700_000_000_000, Severity::Error, Area::Query, "relation \"missing\" does not exist", 3),
        entry(1_700_000_100_000, Severity::Warning, Area::Connect, "connection slow", 1),
        entry(1_700_000_200_000, Severity::Info, Area::App, "update check skipped", 1),
    ]
}

/// Writes the files the app owns of a release into `$TABLEX_COMPAT_OUT` (a data dir: `tablex/…`, and
/// `config/TableX/…` for what lives in the config dir). Run at the release commit after the generator of
/// `engine-connect`: see `fixtures/compat/README.md`.
#[test]
#[ignore = "writes the fixture set of a release (see fixtures/compat/README.md)"]
fn generate_fixture_set() {
    let out = PathBuf::from(std::env::var_os("TABLEX_COMPAT_OUT").expect("TABLEX_COMPAT_OUT names the output dir"));
    assert!(out.is_absolute(), "TABLEX_COMPAT_OUT must be absolute");
    let tablex = out.join("tablex");
    let db = tablex.join(engine_storage::app_db::FILE_NAME);
    assert!(db.is_file(), "run the generator of engine-connect first: it makes the database");
    assert!(!tablex.join("settings.json").exists(), "the fixture set exists already");

    std::fs::write(tablex.join("settings.json"), store::encode(&populated_settings()).unwrap()).unwrap();
    populated_sidebar().save(&tablex.join("sidebar-preferences.json")).unwrap();

    gpui_session::save(&db, &gpui_session::encode(&populated_session()).unwrap()).unwrap();
    super::updates::check::State { last_check: Some(1_700_000_000), skipped: Some("0.1.9".into()) }.save(&db).unwrap();
    generate_conversation(&db);
    generate_diagram(&tablex);

    let root = tablex.join("Projects").join("General");
    crate::saved_sql::save_sql(&root, SHOP, "shop", "monthly revenue", "SELECT 1 AS revenue;", 1_700_000_000.0).unwrap();
    crate::saved_sql::save_script(&root, ORDERS, "orders", "open carts", "db.carts.find({ open: true })", 1_700_000_100.0).unwrap();

    let log = LogStore::new(tablex.join("logs"));
    log.append(&log_entries().iter().map(StoredEntry::to_line).collect::<String>()).unwrap();

    // `<config dir>/TableX`; its own folder here (on macOS it is the data dir under another case).
    let config = out.join("config").join("TableX");
    super::results::copy::write_copy_default(&config.join("grid-copy-default.json"), super::results::copy::CopyFormat::SqlInsert).unwrap();
    super::results::snowflake::SnowflakeStore::configure_worker(&config.join("snowflake"), 17).unwrap();

    // One file: fold the write-ahead log in so no `-wal` / `-shm` is left beside it.
    let conn = engine_storage::app_db::open(&db).unwrap();
    conn.query_row("PRAGMA wal_checkpoint(TRUNCATE)", [], |_| Ok(())).unwrap();
}

const CONVERSATION_ID: &str = "conversation-1";

fn conversation_header() -> super::assistant::store::Header {
    super::assistant::store::Header {
        updated: 1_700_000_000_000,
        created: 1_699_999_000_000,
        ..super::assistant::store::Header::start(
            CONVERSATION_ID.into(),
            "why is the orders query slow?".into(),
            "anthropic",
            "claude-example-1",
            engine_connect::profile::AgentAccess::Read,
        )
    }
}

fn conversation() -> engine_ai::Conversation {
    serde_json::from_value(json!({
        "scope": {"connection_id": SHOP, "database": "shop", "schema": "public"},
        "context": null,
        "messages": [
            {"role": "user", "content": [{"type": "text", "text": "why is the orders query slow?"}]},
            {"role": "assistant", "content": [{"type": "text", "text": "There is no index on status."}]}
        ]
    }))
    .unwrap()
}

fn generate_conversation(db: &Path) {
    let store = super::assistant::store::ConversationStore::new(db.to_path_buf());
    let encoded = super::assistant::store::encode(&conversation_header(), &conversation()).unwrap();
    store.write(SHOP, &encoded).unwrap();
}

fn diagram_key() -> super::results::diagram::store::DiagramKey {
    super::results::diagram::store::DiagramKey { connection: SHOP.into(), database: "shop".into(), schema: Some("public".into()), table: None }
}

fn generate_diagram(tablex: &Path) {
    use super::results::diagram::store::{LayoutStore, SavedDiagram, SavedEntity};
    let saved = SavedDiagram {
        key: diagram_key(),
        entities: vec![
            SavedEntity { schema: Some("public".into()), table: "orders".into(), x: 40.5, y: 80.0, extra: BTreeMap::new() },
            SavedEntity { schema: Some("public".into()), table: "customers".into(), x: 420.0, y: 80.0, extra: BTreeMap::new() },
        ],
        ..SavedDiagram::default()
    };
    LayoutStore::new(tablex.to_path_buf()).save(&diagram_key(), &saved).unwrap();
}

// ---- settings ----------------------------------------------------------------------------------------

#[test]
fn settings_of_every_release_load_with_every_value_and_survive_a_save() {
    for version in VERSIONS {
        let dir = copy_of(version);
        let tablex = dir.path().join("tablex");
        let path = tablex.join("settings.json");
        let original = std::fs::read(&path).unwrap();
        let old = read_json(&path);

        let legacy = store::Legacy::read(Some(&tablex.join("sidebar-preferences.json")));
        let loaded = store::load(&path, &legacy);
        assert_eq!(loaded.notice, None, "{version}: the settings of a released version load without a notice");
        let stale = old["schema_version"].as_u64().unwrap() < u64::from(crate::settings::SCHEMA_VERSION);
        assert_eq!(loaded.needs_save, stale, "{version}: only a file of an older schema is written back");
        assert_eq!(std::fs::read(&path).unwrap(), original, "{version}: loading does not write");
        assert!(!tablex.join("settings.json.corrupt").exists(), "{version}: the file was set aside as corrupt");
        assert_ne!(loaded.settings, Settings::default(), "{version}: the values were read");

        // Every value of the file is still there after the settings are written again.
        let new: Value = serde_json::from_slice(&store::encode(&loaded.settings).unwrap()).unwrap();
        assert_survives(version, "settings.json", &old, &new);

        // A setting the file does not have (it came with a later release) reads as its default. The
        // JSON places of a setting are found by changing it in the defaults and looking where the
        // JSON moves. (The AI rows are per provider: left to the checks above.)
        let default_json = serde_json::to_value(Settings::default()).unwrap();
        for descriptor in registry::registry().iter().filter(|descriptor| descriptor.section != registry::Section::Ai) {
            let mut changed = Settings::default();
            descriptor.set(&mut changed, other_value(descriptor)).unwrap();
            let mut places = Vec::new();
            changed_places("", &default_json, &serde_json::to_value(&changed).unwrap(), &mut places);
            assert!(!places.is_empty(), "{}: changing it moves nothing in settings.json", descriptor.id);
            if places.iter().all(|place| lookup(&old, place).is_none()) {
                assert_eq!(descriptor.get(&loaded.settings), descriptor.default, "{version}: {} is missing from the file and must read as its default", descriptor.id);
            }
        }

        // A save and a load again change nothing.
        store::save(&path, &loaded.settings).unwrap();
        let again = store::load(&path, &legacy);
        assert_eq!((again.notice, again.needs_save), (None, false), "{version}");
        assert_eq!(again.settings, loaded.settings, "{version}: save then load");

        // `tablex-mcp` reads the same file through `engine-connect`.
        let profile = engine_connect::profile::ProfileSettings::load(&path).unwrap();
        assert_eq!(profile.connection_types, loaded.settings.connection_types, "{version}");
        assert_eq!(engine_connect::profile::read_mcp_writes(&path).unwrap(), loaded.settings.ai.mcp_writes, "{version}");
    }
}

#[test]
fn a_settings_file_that_cannot_be_read_is_set_aside_not_overwritten() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("settings.json");
    // Not JSON, and a file that is not even text: both must stay for the user to recover.
    for (index, bytes) in [b"{ \"editor\": ".to_vec(), vec![0xFF, 0xFE, 0x00, 0xA5, 0x80]].into_iter().enumerate() {
        std::fs::write(&path, &bytes).unwrap();
        let _ = std::fs::remove_file(dir.path().join("settings.json.corrupt"));
        let loaded = store::load(&path, &store::Legacy::default());
        assert!(loaded.notice.is_some(), "case {index}: the user is told");
        assert_eq!(std::fs::read(dir.path().join("settings.json.corrupt")).unwrap(), bytes, "case {index}: the original is kept");
        assert_eq!(loaded.settings.schema_version, crate::settings::SCHEMA_VERSION);
    }
}

#[test]
fn a_settings_file_of_a_newer_release_is_read_and_not_rewritten_by_the_load() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("settings.json");
    let text = r#"{"schema_version": 99, "editor": {"tab_size": 7, "setting_from_the_future": true}, "future_section": {"a": 1}}"#;
    std::fs::write(&path, text).unwrap();
    let loaded = store::load(&path, &store::Legacy::default());
    assert!(loaded.notice.is_some() && !loaded.needs_save);
    assert_eq!(loaded.settings.editor.tab_size, 7, "what this build knows is used");
    assert_eq!(std::fs::read_to_string(&path).unwrap(), text);
}

// ---- sidebar preferences ---------------------------------------------------------------------------------

#[test]
fn sidebar_preferences_of_every_release_load_with_every_value_and_survive_a_save() {
    for version in VERSIONS {
        let dir = copy_of(version);
        let path = dir.path().join("tablex").join("sidebar-preferences.json");
        let old = read_json(&path);
        let loaded = SidebarPreferences::load(&path).unwrap_or_else(|error| panic!("{version}: {error}"));
        assert_ne!(loaded, SidebarPreferences::default(), "{version}");

        let new = serde_json::to_value(&loaded).unwrap();
        assert_survives(version, "sidebar-preferences.json", &old, &new);

        // What the file said, read as the app reads it.
        for connection in old["connections"].as_array().unwrap() {
            let id = connection["id"].as_str().unwrap();
            let display = loaded.connections.iter().find(|item| item.id == id).unwrap_or_else(|| panic!("{version}: {id} is gone"));
            assert_eq!(display.color.as_deref(), connection["color"].as_str(), "{version}: the color of {id}");
            assert_eq!(display.hidden, connection["hidden"].as_bool().unwrap_or(false), "{version}: hidden of {id}");
        }
        assert_eq!(loaded.groups.len(), old["groups"].as_array().unwrap().len(), "{version}");
        for (connection, filter) in old["visible_filters"].as_object().unwrap() {
            assert!(loaded.visible_filters.contains_key(connection), "{version}: the visible filter of {connection}");
            assert_eq!(serde_json::to_value(&loaded.visible_filters[connection]).unwrap()["patterns"], filter["patterns"], "{version}");
        }

        // A save and a load again change nothing, and what a later release added is empty or its
        // default for a file that predates it.
        loaded.save(&path).unwrap();
        assert_eq!(SidebarPreferences::load(&path).unwrap(), loaded, "{version}: save then load");
        for (field, empty) in [("folder_filters", loaded.folder_filters.is_empty()), ("bookmarks", loaded.bookmarks.is_empty())] {
            if old.get(field).is_none() {
                assert!(empty, "{version}: `{field}` is missing from the file and must be empty");
            }
        }
    }
}

#[test]
fn a_sidebar_preferences_file_that_cannot_be_read_is_never_written_over() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("sidebar-preferences.json");
    for bytes in [b"{ \"groups\": [".to_vec(), br#"{"sort": "sideways"}"#.to_vec(), vec![0xFF, 0xFE, 0x00]] {
        std::fs::write(&path, &bytes).unwrap();
        assert!(SidebarPreferences::load(&path).is_err());
        // The one writer that works from the file on disk (the assistant dock) leaves it alone.
        assert!(SidebarPreferences::update_assistant_layout(&path, true, 400.0).is_err());
        assert_eq!(std::fs::read(&path).unwrap(), bytes);
    }
}

// ---- session, tree and the other documents of the app ----------------------------------------------------------

/// The panes of a session in visual order: their tabs, active tab and strip settings.
#[allow(clippy::type_complexity)]
fn leaves(session: &GpuiSession) -> Vec<(Vec<SessionTab>, Option<SessionTab>, Option<StripSettings>)> {
    fn walk(session: &GpuiSession, node: &PaneNodeSpec, out: &mut Vec<(Vec<SessionTab>, Option<SessionTab>, Option<StripSettings>)>) {
        match node {
            PaneNodeSpec::Pane { tabs, active, strip, .. } => out.push((
                tabs.iter().map(|index| session.tabs[*index].clone()).collect(),
                active.map(|index| session.tabs[index].clone()),
                *strip,
            )),
            PaneNodeSpec::Split { children, .. } => children.iter().for_each(|child| walk(session, child, out)),
        }
    }
    let mut out = Vec::new();
    if let Some(panes) = &session.panes {
        walk(session, &panes.root, &mut out);
    }
    out
}

#[test]
fn the_session_of_every_release_loads_restores_and_snapshots_back_to_what_it_was() {
    for version in VERSIONS {
        let dir = copy_of(version);
        let db = dir.path().join("tablex").join(engine_storage::app_db::FILE_NAME);
        let stored = documents::get(&db, documents::SESSION, documents::SESSION_NAME).unwrap().unwrap_or_else(|| panic!("{version}: no session"));
        let old: Value = serde_json::from_str(&stored).unwrap();
        let LoadOutcome::Loaded(session) = gpui_session::load(&db) else { panic!("{version}: the session did not load") };
        assert!(!session.tabs.is_empty());

        // Every value of the stored session is in what the app holds of it.
        assert_survives(version, "session", &old, &serde_json::to_value(&session).unwrap());

        // Restoring the tabs and taking the snapshot again gives the same tabs, pins, active tabs
        // and pane layout. (The order of the whole list follows the panes and their grouping, so
        // the comparison is per pane.)
        let mut workspace = WorkspaceState::new();
        workspace.restore_snapshot(&session, |_| true);
        let back = workspace.snapshot();
        let sorted = |tabs: &[SessionTab]| {
            let mut titles: Vec<String> = tabs.iter().map(|tab| format!("{tab:?}")).collect();
            titles.sort();
            titles
        };
        assert_eq!(sorted(&back.tabs), sorted(&session.tabs), "{version}: tabs");
        assert_eq!(back.active.map(|index| &back.tabs[index]), session.active.map(|index| &session.tabs[index]), "{version}: active tab");
        assert_eq!(back.collapsed_groups, session.collapsed_groups, "{version}: collapsed groups");
        let (old_panes, new_panes) = (leaves(&session), leaves(&back));
        assert_eq!(old_panes.len(), new_panes.len(), "{version}: panes");
        for ((old_tabs, old_active, old_strip), (new_tabs, new_active, new_strip)) in old_panes.into_iter().zip(new_panes) {
            assert_eq!(sorted(&old_tabs), sorted(&new_tabs), "{version}: the tabs of a pane");
            assert_eq!(old_active, new_active, "{version}: the active tab of a pane");
            assert_eq!(old_strip.unwrap_or_default(), new_strip.unwrap_or_default(), "{version}: the tab strip of a pane");
        }
        assert_eq!(back.panes.as_ref().map(|panes| panes.focused), session.panes.as_ref().map(|panes| panes.focused), "{version}: focused pane");

        // Whatever the tree remembered either reads or is skipped (a node of another release).
        for tree in &session.tree {
            for node in &tree.nodes {
                let _ = serde_json::from_value::<NavigatorNodeId>(node.clone());
            }
        }

        // A save through the app's writer and a load again keep the session.
        let bytes = gpui_session::encode(&session).unwrap();
        gpui_session::save(&db, &bytes).unwrap();
        let LoadOutcome::Loaded(again) = gpui_session::load(&db) else { panic!("{version}: reload") };
        assert_eq!(again, session, "{version}: save then load");
    }
}

#[test]
fn a_session_the_app_cannot_use_is_kept_aside_not_destroyed() {
    let dir = tempfile::tempdir().unwrap();
    let db = dir.path().join(engine_storage::app_db::FILE_NAME);
    for text in ["{ \"tabs\": [", r#"{"version": 99, "tabs": []}"#, r#"{"tabs": "not a list"}"#] {
        documents::put(&db, documents::SESSION, documents::SESSION_NAME, text).unwrap();
        assert!(matches!(gpui_session::load(&db), LoadOutcome::Unusable(_)), "{text}");
        gpui_session::quarantine_session(&db);
        assert_eq!(documents::get(&db, documents::SESSION, documents::SESSION_CORRUPT_NAME).unwrap().as_deref(), Some(text));
        assert!(matches!(gpui_session::load(&db), LoadOutcome::Missing));
    }
}

#[test]
fn the_update_check_state_of_every_release_is_read() {
    for version in VERSIONS {
        let dir = copy_of(version);
        let db = dir.path().join("tablex").join(engine_storage::app_db::FILE_NAME);
        let old: Value = serde_json::from_str(&documents::get(&db, documents::UPDATE_CHECK, documents::UPDATE_CHECK_NAME).unwrap().unwrap()).unwrap();
        let state = super::updates::check::State::load(&db);
        assert_eq!(state.last_check, old["last_check"].as_u64(), "{version}");
        assert_eq!(state.skipped.as_deref(), old["skipped"].as_str(), "{version}");
    }
}

#[test]
fn the_diagram_layouts_of_every_release_load_and_keep_what_a_later_release_wrote() {
    use super::results::diagram::store::LayoutStore;
    for version in VERSIONS {
        let dir = copy_of(version);
        let tablex = dir.path().join("tablex");
        let store = LayoutStore::new(tablex.clone());
        let key = diagram_key();
        let raw = store.raw(&key).unwrap_or_else(|| panic!("{version}: no diagram under the name of its key"));
        let saved = store.load(&key).unwrap().unwrap_or_else(|| panic!("{version}: the diagram did not load"));
        assert_survives(version, "diagram", &serde_json::from_str(&raw).unwrap(), &serde_json::to_value(&saved).unwrap());
        assert_eq!(saved.entities.len(), 2, "{version}");
        // Written back it is the same, so the next open finds the same positions.
        store.save(&key, &saved).unwrap();
        assert_eq!(store.load(&key).unwrap(), Some(saved), "{version}");
    }
}

#[test]
fn the_assistant_conversations_of_every_release_list_and_load() {
    use super::assistant::store::{ConversationStore, Header};
    for version in VERSIONS {
        let dir = copy_of(version);
        let store = ConversationStore::new(dir.path().join("tablex").join(engine_storage::app_db::FILE_NAME));
        let listing = store.list(SHOP).unwrap();
        assert!(listing.problems.is_empty(), "{version}: {:?}", listing.problems);
        let entry = listing.entries.iter().find(|entry| entry.header.id == CONVERSATION_ID).unwrap_or_else(|| panic!("{version}: conversation"));
        assert_eq!(entry.header, Header { seq: 2, ..conversation_header() }, "{version}");
        let loaded = store.load(SHOP, CONVERSATION_ID).unwrap();
        assert_eq!(loaded.conversation, conversation(), "{version}");
        // Writing it again (a new message arrives) keeps it readable.
        let encoded = super::assistant::store::encode(&loaded.header, &loaded.conversation).unwrap();
        assert!(store.write(SHOP, &encoded).is_ok() || store.load(SHOP, CONVERSATION_ID).is_ok());
    }
}

#[test]
fn the_pinned_rows_of_every_release_find_their_rows_again() {
    for version in VERSIONS {
        let dir = copy_of(version);
        let db = dir.path().join("tablex").join(engine_storage::app_db::FILE_NAME);
        let pins = engine_storage::pinned_nodes::list_all(&db).unwrap();
        assert!(!pins.is_empty(), "{version}");
        for (connection, key) in &pins {
            let id = NavigatorNodeId::from_pin_key(&connection.to_lowercase(), key);
            assert!(id.is_some(), "{version}: the pin {key} of {connection} is not read");
            assert_eq!(id.unwrap().pin_key().as_deref(), Some(key.as_str()), "{version}: written back as it was");
        }
    }
}

// ---- files beside the database ------------------------------------------------------------------------------

#[test]
fn saved_scripts_the_error_log_and_the_small_files_of_every_release_are_read() {
    for version in VERSIONS {
        let dir = copy_of(version);
        let tablex = dir.path().join("tablex");

        let roots = crate::saved_sql::project_roots_at(&tablex.join("Projects"), false).unwrap();
        let sql = crate::saved_sql::recent_sql(&roots, SHOP).unwrap();
        assert_eq!(sql.len(), 1, "{version}: saved SQL");
        assert_eq!((sql[0].title.as_str(), sql[0].database.as_str()), ("monthly revenue", "shop"));
        assert_eq!(crate::saved_sql::open_sql(&sql[0]).unwrap(), "SELECT 1 AS revenue;");
        let scripts = crate::saved_sql::recent_scripts(&roots, ORDERS).unwrap();
        assert_eq!(scripts.len(), 1, "{version}: saved scripts");
        assert_eq!(crate::saved_sql::open_sql(&scripts[0]).unwrap(), "db.carts.find({ open: true })");
        // Saving another one keeps the earlier ones.
        crate::saved_sql::save_sql(&roots[0], SHOP, "shop", "second", "SELECT 2;", 1_700_001_000.0).unwrap();
        assert_eq!(crate::saved_sql::recent_sql(&roots, SHOP).unwrap().len(), 2);

        let log = LogStore::new(tablex.join("logs"));
        let loaded = log.load_recent(100);
        let expected = log_entries();
        assert_eq!(loaded.len(), expected.len(), "{version}: error log entries");
        for (loaded, expected) in loaded.iter().zip(&expected) {
            assert_eq!(serde_json::to_value(loaded).unwrap(), serde_json::to_value(expected).unwrap(), "{version}");
        }
        // Appending keeps the old lines.
        log.append(&expected[0].to_line()).unwrap();
        assert_eq!(log.load_recent(100).len(), expected.len() + 1);

        let config = dir.path().join("config").join("TableX");
        assert_eq!(super::results::copy::read_copy_default(&config.join("grid-copy-default.json")), super::results::copy::CopyFormat::SqlInsert, "{version}");
        let worker = config.join("snowflake");
        // The worker file is private to its owner; git keeps no modes.
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt as _;
            std::fs::set_permissions(&worker, std::fs::Permissions::from_mode(0o700)).unwrap();
            std::fs::set_permissions(worker.join("worker.json"), std::fs::Permissions::from_mode(0o600)).unwrap();
        }
        assert_eq!(super::results::snowflake::SnowflakeStore::read_worker(&worker), Ok(Some(17)), "{version}");
    }
}

// ---- the formats this release introduced, pinned ------------------------------------------------------------

/// The text of what the sidebar preferences and the session tree keep of the features of 0.1.11, written
/// down so that a rename of a field, a variant or a tag fails here: it would make the files that
/// release's users have unreadable. When a format has to change, keep the old reading (an alias, a
/// default) and add the new text here.
#[test]
fn the_persisted_forms_of_the_features_of_this_release_are_pinned() {
    let prefs = populated_sidebar();
    let json = serde_json::to_value(&prefs).unwrap();
    assert_eq!(
        json["folder_filters"],
        json!([{
            "key": {"connection": SHOP, "database": "shop", "schema": "public", "folder": {"objects": "Tables"}},
            "include": ["order*"],
            "exclude": ["*_old"]
        }])
    );
    assert_eq!(
        json["bookmarks"],
        json!([{
            "id": "bookmark-1",
            "name": "Orders",
            "target": {"kind": "object", "connection": SHOP, "database": "shop", "schema": "public", "object": "orders"}
        }])
    );
    assert_eq!(
        serde_json::to_value(NavigatorNodeId::Schema { connection: SHOP.into(), database: "shop".into(), schema: "public".into() }).unwrap(),
        json!({"Schema": {"connection": SHOP, "database": "shop", "schema": "public"}})
    );
    // Every variant of the groups reads back.
    for group in ObjectGroup::ALL {
        let value = serde_json::to_value(group).unwrap();
        assert_eq!(serde_json::from_value::<ObjectGroup>(value).unwrap(), group);
    }
    // The file names and keys the app looks for.
    assert_eq!(crate::settings::store::FILE_NAME, "settings.json");
    assert_eq!(engine_storage::app_db::FILE_NAME, "tablex.db");
    assert_eq!(gpui_session::SESSION_VERSION, 1);
}
