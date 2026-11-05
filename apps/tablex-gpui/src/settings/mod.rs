//! The typed application settings (`<config dir>/tablex/settings.json`).
//!
//! `Settings` is plain data: sections as structs, `#[serde(default)]`
//! everywhere, unknown fields ignored. The rest of the app reads it through the
//! cheap `Arc<Settings>` snapshot on `AppRoot`; nothing here touches the disk
//! except [`store`]. [`registry`] describes every setting once (title, kind,
//! default, getter / setter) and drives the Settings page, its search and the
//! reset buttons.

pub mod connection;
pub mod registry;
pub mod store;

use crate::sidebar_preferences::{ConnectionSort, TabGroupBy};
use engine_ai::Effort;
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

/// Schema version written by this build. Bump it together with a new
/// migration in [`store`].
pub const SCHEMA_VERSION: u32 = 5;

/// A closed set of named values a setting can take (`Select` options).
pub trait Choice: Copy + PartialEq + 'static {
    /// `(stored id, label)` in display order. Ids equal the serde names.
    const OPTIONS: &'static [(&'static str, &'static str)];
    fn id(self) -> &'static str;
    /// The label the option has in a Select.
    fn label(self) -> &'static str {
        Self::OPTIONS.iter().find(|(id, _)| *id == self.id()).map_or("", |(_, label)| label)
    }
    fn from_id(id: &str) -> Option<Self>;
}

macro_rules! impl_choice {
    ($name:ident { $($variant:ident => ($id:literal, $label:literal)),+ $(,)? }) => {
        impl Choice for $name {
            const OPTIONS: &'static [(&'static str, &'static str)] = &[$(($id, $label)),+];
            fn id(self) -> &'static str {
                match self { $(Self::$variant => $id),+ }
            }
            fn from_id(id: &str) -> Option<Self> {
                match id { $($id => Some(Self::$variant),)+ _ => None }
            }
        }
    };
}

macro_rules! choice_enum {
    ($(#[$meta:meta])* $name:ident { $($variant:ident => ($id:literal, $label:literal)),+ $(,)? }) => {
        $(#[$meta])*
        #[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
        #[serde(rename_all = "snake_case")]
        pub enum $name { $($variant),+ }
        impl_choice!($name { $($variant => ($id, $label)),+ });
    };
}

impl_choice!(ConnectionSort {
    Manual => ("manual", "Manual"),
    Ascending => ("ascending", "Name A-Z"),
    Descending => ("descending", "Name Z-A"),
});

impl_choice!(TabGroupBy {
    None => ("none", "Do not group"),
    Connection => ("connection", "Connection"),
    Database => ("database", "Database"),
});

choice_enum!(
    /// What the editor's Run executes.
    RunScope {
        Current => ("current", "Current statement"),
        Selection => ("selection", "Selection (else everything)"),
        All => ("all", "Whole script"),
    }
);

choice_enum!(
    /// What a script run does when a statement fails.
    OnScriptError {
        Stop => ("stop", "Stop at the failing statement"),
        Continue => ("continue", "Run the remaining statements"),
    }
);

choice_enum!(
    /// The case Format SQL gives keywords. `Keep` leaves them as typed.
    KeywordCase {
        Upper => ("upper", "UPPER CASE"),
        Lower => ("lower", "lower case"),
        Keep => ("keep", "As typed"),
    }
);

choice_enum!(
    /// Which formatter Format SQL runs.
    FormatterChoice {
        BuiltIn => ("built_in", "Built-in"),
        Sqruff => ("sqruff", "SQL Formatter (sqruff)"),
        External => ("external", "External command"),
    }
);

choice_enum!(
    /// How boolean cells are drawn.
    BooleanDisplay {
        Text => ("text", "Text"),
        Checkbox => ("checkbox", "Checkbox"),
    }
);

choice_enum!(
    /// Date-time cell format.
    DateTimeFormat {
        Source => ("source", "As returned by the database"),
        Iso => ("iso", "YYYY-MM-DD HH:MM:SS"),
        IsoMillis => ("iso_millis", "YYYY-MM-DD HH:MM:SS.mmm"),
    }
);

choice_enum!(
    /// Text form of binary cells.
    BinaryDisplay {
        Hex => ("hex", "Hexadecimal"),
        Base64 => ("base64", "Base64"),
    }
);

choice_enum!(
    /// Default extended-JSON flavour of MongoDB documents.
    MongoJsonMode {
        Relaxed => ("relaxed", "Relaxed"),
        Canonical => ("canonical", "Canonical"),
    }
);

choice_enum!(
    /// Format of a plain copy (matches the grid's copy-format names).
    CopyDefault {
        Smart => ("smart", "Smart"),
        Raw => ("raw", "Raw values"),
        Tsv => ("tsv", "TSV"),
        TsvWithHeaders => ("tsv_with_headers", "TSV with headers"),
        Csv => ("csv", "CSV"),
        CsvWithHeaders => ("csv_with_headers", "CSV with headers"),
        Json => ("json", "JSON"),
        Markdown => ("markdown", "Markdown"),
    }
);

impl_choice!(Effort {
    Low => ("low", "Low"),
    Medium => ("medium", "Medium"),
    High => ("high", "High"),
    XHigh => ("xhigh", "Extra high"),
    Max => ("max", "Max"),
});

/// Who answers the assistant's questions. The API key (account = the id), the model and the
/// address are kept for each one.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum AiProvider {
    #[serde(rename = "anthropic")]
    Anthropic,
    #[serde(rename = "openai")]
    OpenAi,
    #[serde(rename = "openai_compatible")]
    OpenAiCompatible,
    #[serde(rename = "ollama")]
    Ollama,
}

impl_choice!(AiProvider {
    Anthropic => ("anthropic", "Anthropic"),
    OpenAi => ("openai", "OpenAI"),
    OpenAiCompatible => ("openai_compatible", "OpenAI-compatible"),
    Ollama => ("ollama", "Ollama"),
});

impl AiProvider {
    pub const ALL: [Self; 4] = [
        Self::Anthropic,
        Self::OpenAi,
        Self::OpenAiCompatible,
        Self::Ollama,
    ];

    /// The name the Settings select and the assistant panel show.
    pub fn label(self) -> &'static str {
        Self::OPTIONS
            .iter()
            .find(|(id, _)| *id == self.id())
            .map_or("", |(_, label)| *label)
    }

    /// The flavor of the OpenAI wire this provider speaks; `None` for Anthropic's own API.
    pub fn flavor(self) -> Option<engine_ai::Flavor> {
        match self {
            Self::Anthropic => None,
            Self::OpenAi => Some(engine_ai::Flavor::OpenAi),
            Self::OpenAiCompatible => Some(engine_ai::Flavor::Compatible),
            Self::Ollama => Some(engine_ai::Flavor::Ollama),
        }
    }

    /// Whether the assistant cannot start without a saved API key (the engine's rule per flavor).
    pub fn requires_key(self) -> bool {
        self.flavor().is_none_or(engine_ai::Flavor::requires_key)
    }

    /// The model a new setup starts with; only Anthropic has one, the others ask to choose.
    pub fn default_model(self) -> &'static str {
        match self {
            Self::Anthropic => engine_ai::DEFAULT_MODEL,
            _ => "",
        }
    }

    /// The address a new setup starts with; empty for a compatible server, which can be anywhere.
    pub fn default_base_url(self) -> &'static str {
        match self.flavor() {
            None => crate::ai::DEFAULT_BASE_URL,
            Some(flavor) => flavor.default_base_url().unwrap_or_default(),
        }
    }
}

choice_enum!(
    /// What double-clicking a table or view in the navigator opens.
    TableOpenAction {
        Data => ("data", "Open data"),
        Properties => ("properties", "Open properties"),
        Diagram => ("diagram", "Open diagram"),
    }
);

choice_enum!(
    /// Which tabs come back after a restart.
    RestoreTabs {
        All => ("all", "All tabs"),
        Pinned => ("pinned", "Pinned tabs only"),
        None => ("none", "None"),
    }
);

choice_enum!(
    /// The color theme; `System` follows the OS appearance live.
    ThemeChoice {
        Dark => ("dark", "Dark"),
        Light => ("light", "Light"),
        System => ("system", "System (follow the OS)"),
    }
);

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Appearance {
    /// Dark, Light, or follow the OS.
    pub theme: ThemeChoice,
    /// Whole-UI zoom factor (needs the layout to stop using fixed pixels).
    pub ui_scale: f32,
    /// Interface font family; empty = the system UI font.
    pub interface_font: String,
    /// Result grid font family; empty = the interface font.
    pub grid_font: String,
    pub grid_font_size: f32,
    /// Editor font family; empty = the platform monospace font.
    pub editor_font: String,
    pub editor_font_size: f32,
    /// Default "Group tabs by" of the tab bar.
    pub tab_group_by: TabGroupBy,
    /// Default "Show tabs in a second row".
    pub tab_rows: bool,
    /// "Shorten tab titles": drop the `@database` suffix.
    pub compact_tab_titles: bool,
    /// Debug metric in the status bar; on by default only in debug builds.
    pub show_fps: bool,
}

impl Default for Appearance {
    fn default() -> Self {
        Self {
            theme: ThemeChoice::Dark,
            ui_scale: 1.0,
            interface_font: String::new(),
            grid_font: String::new(),
            grid_font_size: 12.0,
            editor_font: String::new(),
            editor_font_size: 13.0,
            tab_group_by: TabGroupBy::None,
            tab_rows: false,
            compact_tab_titles: false,
            show_fps: cfg!(any(debug_assertions, feature = "perf-diagnostics")),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct EditorSettings {
    pub line_numbers: bool,
    pub soft_wrap: bool,
    pub tab_size: u32,
    /// Auto-close brackets and quotes.
    pub auto_close: bool,
    pub autocomplete_auto_trigger: bool,
    pub autocomplete_insert_space: bool,
    pub autocomplete_max_items: u32,
    pub highlight_current_statement: bool,
    pub run_scope: RunScope,
    /// A script of several statements: what a failing statement does to the rest.
    pub on_script_error: OnScriptError,
    /// Ask for the value of `:name` / `${name}` parameters before running.
    pub parameters: bool,
    /// Result tabs a script run keeps; statements past it still run and appear in Statistics.
    pub max_result_tabs: u32,
    /// Global switch on top of the driver's own confirmation of dangerous
    /// statements: off runs them without asking.
    pub confirm_dangerous: bool,
    pub confirm_unsaved_close: bool,
    /// Format SQL: the case of keywords.
    pub format_keyword_case: KeywordCase,
    /// Format SQL: spaces per indent level; 0 follows [`Self::tab_size`].
    pub format_indent: u32,
    /// Format SQL: blank lines after a statement's `;`.
    pub format_lines_between_statements: u32,
    /// Format SQL: the formatter. The built-in one is the default; the others run as separate
    /// processes.
    pub formatter: FormatterChoice,
    /// Format SQL, external command: the program (a name looked up on PATH, or a full path).
    pub format_external_program: String,
    /// Format SQL, external command: its arguments, split on blanks (quote an argument with
    /// spaces); `{dialect}` and `{language}` stand for the connection's dialect.
    pub format_external_args: String,
    /// Format SQL, external command: the exit codes that count as success, separated by commas
    /// (`0,1` for a linter that exits 1 when it left something unfixed).
    pub format_external_exit_codes: String,
    /// Format SQL: seconds a tool or external command may take before it is stopped.
    pub format_timeout: u32,
}

impl EditorSettings {
    /// What Format SQL does with these settings. `context` is what only the open tab knows.
    pub fn sql_formatter(&self, context: crate::sql_format::FormatContext) -> crate::sql_format::SqlFormatter {
        use crate::sql_format::{BuiltInOptions, ExternalOptions, SqlFormatter, ToolOptions};
        let indent = if self.format_indent == 0 { self.tab_size } else { self.format_indent };
        let indent = indent.clamp(1, 16) as u8;
        let timeout = std::time::Duration::from_secs(u64::from(self.format_timeout.clamp(1, 600)));
        match self.formatter {
            FormatterChoice::BuiltIn => SqlFormatter::BuiltIn(BuiltInOptions {
                keyword_case: self.format_keyword_case,
                indent,
                lines_between_statements: self.format_lines_between_statements.min(5) as u8,
            }),
            FormatterChoice::Sqruff => SqlFormatter::Tool(ToolOptions {
                context,
                keyword_case: self.format_keyword_case,
                indent,
                timeout,
            }),
            FormatterChoice::External => SqlFormatter::External(ExternalOptions {
                context,
                program: self.format_external_program.trim().to_owned(),
                args: self.format_external_args.clone(),
                exit_codes: self.format_external_exit_codes.clone(),
                timeout,
            }),
        }
    }
}

impl Default for EditorSettings {
    fn default() -> Self {
        Self {
            line_numbers: true,
            soft_wrap: false,
            tab_size: 2,
            auto_close: true,
            autocomplete_auto_trigger: true,
            autocomplete_insert_space: false,
            autocomplete_max_items: 50,
            highlight_current_statement: false,
            run_scope: RunScope::All,
            on_script_error: OnScriptError::Stop,
            parameters: true,
            max_result_tabs: 20,
            confirm_dangerous: true,
            confirm_unsaved_close: false,
            format_keyword_case: KeywordCase::Upper,
            format_indent: 0,
            format_lines_between_statements: 1,
            formatter: FormatterChoice::BuiltIn,
            format_external_program: String::new(),
            format_external_args: String::new(),
            format_external_exit_codes: "0".into(),
            format_timeout: 10,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct ExportDefaults {
    pub delimiter: String,
    pub header: bool,
    pub null_text: String,
    /// Target folder; empty = the platform downloads folder.
    pub folder: String,
}

impl Default for ExportDefaults {
    fn default() -> Self {
        Self {
            delimiter: ",".into(),
            header: true,
            null_text: String::new(),
            folder: String::new(),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Results {
    /// Rows per page of a new tab.
    pub page_size: u32,
    pub null_text: String,
    pub right_align_numbers: bool,
    pub show_row_numbers: bool,
    pub column_width_default: u32,
    pub column_width_min: u32,
    pub column_width_max: u32,
    /// Size columns from a sample of their values instead of the default.
    pub auto_size_columns: bool,
    pub boolean_display: BooleanDisplay,
    pub datetime_format: DateTimeFormat,
    pub binary_display: BinaryDisplay,
    pub binary_preview_max: u32,
    pub mongo_json_mode: MongoJsonMode,
    pub default_copy_format: CopyDefault,
    pub export: ExportDefaults,
}

impl Default for Results {
    fn default() -> Self {
        Self {
            page_size: crate::query::DEFAULT_OUTPUT_LIMIT as u32,
            null_text: "NULL".into(),
            right_align_numbers: true,
            show_row_numbers: true,
            column_width_default: 160,
            column_width_min: 60,
            column_width_max: 480,
            auto_size_columns: false,
            boolean_display: BooleanDisplay::Text,
            datetime_format: DateTimeFormat::Source,
            binary_display: BinaryDisplay::Hex,
            binary_preview_max: 64,
            mongo_json_mode: MongoJsonMode::Relaxed,
            default_copy_format: CopyDefault::Smart,
            export: ExportDefaults::default(),
        }
    }
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct QuerySettings {
    /// Seconds a query may run; 0 = no limit.
    pub query_timeout_secs: u32,
    /// Seconds to wait for a connection; 0 = the driver default.
    pub connect_timeout_secs: u32,
    /// Backup and restore: the native client tools to run. Empty = look for the tool (PATH, then the
    /// places each operating system installs it).
    pub pg_dump_path: String,
    pub pg_restore_path: String,
    pub mysqldump_path: String,
    pub mariadb_dump_path: String,
}

impl QuerySettings {
    /// The tool paths for Backup and Restore.
    pub fn tool_paths(&self) -> crate::backup::tools::ToolPaths {
        crate::backup::tools::ToolPaths {
            pg_dump: self.pg_dump_path.clone(),
            pg_restore: self.pg_restore_path.clone(),
            mysqldump: self.mysqldump_path.clone(),
            mariadb_dump: self.mariadb_dump_path.clone(),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Navigation {
    pub connection_sort: ConnectionSort,
    /// Sidebar search also crawls unloaded catalogs ("global search").
    pub global_search: bool,
    pub show_system_objects: bool,
    pub table_double_click: TableOpenAction,
    /// A single click on a table opens it (instead of only selecting it).
    pub single_click_opens: bool,
    pub restore_tabs: RestoreTabs,
    pub open_data_tabs_next_to_active: bool,
    pub navigator_page_size: u32,
}

impl Default for Navigation {
    fn default() -> Self {
        Self {
            connection_sort: ConnectionSort::Manual,
            global_search: false,
            show_system_objects: true,
            table_double_click: TableOpenAction::Data,
            single_click_opens: false,
            restore_tabs: RestoreTabs::All,
            open_data_tabs_next_to_active: false,
            navigator_page_size: 200,
        }
    }
}

/// What is kept for one provider: its model and its address.
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct ProviderEntry {
    /// The provider's model id; empty until one is chosen (Anthropic starts with a default).
    pub model: String,
    /// The provider's address; `https://`, or `http://` to this machine only.
    pub base_url: String,
}

impl ProviderEntry {
    fn defaults(provider: AiProvider) -> Self {
        Self {
            model: provider.default_model().to_owned(),
            base_url: provider.default_base_url().to_owned(),
        }
    }
}

/// `ai.providers.<id>`: one entry per provider, so switching never hands one provider's model id or
/// address to another.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct AiProviders {
    pub anthropic: ProviderEntry,
    pub openai: ProviderEntry,
    pub openai_compatible: ProviderEntry,
    pub ollama: ProviderEntry,
}

impl Default for AiProviders {
    fn default() -> Self {
        Self {
            anthropic: ProviderEntry::defaults(AiProvider::Anthropic),
            openai: ProviderEntry::defaults(AiProvider::OpenAi),
            openai_compatible: ProviderEntry::defaults(AiProvider::OpenAiCompatible),
            ollama: ProviderEntry::defaults(AiProvider::Ollama),
        }
    }
}

impl AiProviders {
    pub fn entry(&self, provider: AiProvider) -> &ProviderEntry {
        match provider {
            AiProvider::Anthropic => &self.anthropic,
            AiProvider::OpenAi => &self.openai,
            AiProvider::OpenAiCompatible => &self.openai_compatible,
            AiProvider::Ollama => &self.ollama,
        }
    }

    pub fn entry_mut(&mut self, provider: AiProvider) -> &mut ProviderEntry {
        match provider {
            AiProvider::Anthropic => &mut self.anthropic,
            AiProvider::OpenAi => &mut self.openai,
            AiProvider::OpenAiCompatible => &mut self.openai_compatible,
            AiProvider::Ollama => &mut self.ollama,
        }
    }
}

/// The two fields a schema-4 file kept at the top of `ai`; read once, moved under Anthropic by the
/// 4 -> 5 migration, never written.
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct LegacyAi {
    #[serde(default, skip_serializing)]
    model: Option<String>,
    #[serde(default, skip_serializing)]
    base_url: Option<String>,
}

/// The AI assistant. The API key is **not** here: it lives in the OS credential store
/// (`ai::keys`), never in this file.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct AiSettings {
    pub provider: AiProvider,
    /// Model and address of each provider.
    pub providers: AiProviders,
    /// How much the model thinks (`output_config.effort`).
    pub effort: Effort,
    /// Let Anthropic re-run a request its safety checks declined on a fallback model (the other
    /// providers have no such thing).
    pub fallback: bool,
    /// Keep finished conversations as files on this computer (History). They can hold query
    /// results. Off stops writing; it deletes nothing.
    pub keep_conversations: bool,
    /// "Let MCP clients run changes": `tablex-mcp` may ask the MCP client to approve statements that
    /// change data on connections whose assistant access is "Read and write" (never on production
    /// connections). Off by default; the MCP server reads this file at the time of each write and
    /// treats anything but `true` as off.
    pub mcp_writes: bool,
    #[serde(flatten)]
    legacy: LegacyAi,
}

impl Default for AiSettings {
    fn default() -> Self {
        Self {
            provider: AiProvider::Anthropic,
            providers: AiProviders::default(),
            effort: Effort::Medium,
            fallback: true,
            keep_conversations: true,
            mcp_writes: false,
            legacy: LegacyAi::default(),
        }
    }
}

impl AiSettings {
    /// The selected provider's model and address.
    pub fn selected(&self) -> &ProviderEntry {
        self.providers.entry(self.provider)
    }

    pub fn selected_mut(&mut self) -> &mut ProviderEntry {
        self.providers.entry_mut(self.provider)
    }

    /// 4 -> 5: the model and address of a schema-4 file belong to Anthropic, its only provider.
    pub(super) fn adopt_legacy(&mut self) {
        let LegacyAi { model, base_url } = std::mem::take(&mut self.legacy);
        if let Some(model) = model {
            self.providers.anthropic.model = model;
        }
        if let Some(base_url) = base_url {
            self.providers.anthropic.base_url = base_url;
        }
    }

    /// Forgets the old top-level fields of a file that is at the current version already.
    pub(super) fn drop_legacy(&mut self) {
        self.legacy = LegacyAi::default();
    }

    /// Puts back what a hand-edited file made unusable, for each provider: an empty Anthropic
    /// model, an address the API key must not be sent to.
    pub fn normalize(&mut self) {
        for provider in AiProvider::ALL {
            let entry = self.providers.entry_mut(provider);
            if provider == AiProvider::Anthropic && entry.model.trim().is_empty() {
                entry.model = provider.default_model().to_owned();
            }
            if crate::ai::base_url_problem(provider, &entry.base_url).is_some() {
                entry.base_url = provider.default_base_url().to_owned();
            }
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Settings {
    pub schema_version: u32,
    pub appearance: Appearance,
    pub editor: EditorSettings,
    pub results: Results,
    pub query: QuerySettings,
    pub navigation: Navigation,
    pub ai: AiSettings,
    /// Development / Test / Production plus the user's own (see [`connection`]).
    pub connection_types: Vec<connection::ConnectionType>,
    /// Keyboard shortcut overrides: action id (`workspace.close_tab`) to its
    /// chords in gpui's keystroke form (`secondary-shift-k`). An absent id
    /// keeps the default chords; an empty list unbinds the action. Ids the app
    /// does not know (a newer or older build wrote them) are kept and ignored.
    pub shortcuts: BTreeMap<String, Vec<String>>,
}

impl Default for Settings {
    fn default() -> Self {
        Self {
            schema_version: SCHEMA_VERSION,
            appearance: Appearance::default(),
            editor: EditorSettings::default(),
            results: Results::default(),
            query: QuerySettings::default(),
            navigation: Navigation::default(),
            ai: AiSettings::default(),
            connection_types: connection::ConnectionType::builtins(),
            shortcuts: BTreeMap::new(),
        }
    }
}

impl Settings {
    #[cfg(test)]
    /// Query timeout as a `Duration`; `None` when unlimited.
    pub fn query_timeout(&self) -> Option<std::time::Duration> {
        (self.query.query_timeout_secs > 0)
            .then(|| std::time::Duration::from_secs(u64::from(self.query.query_timeout_secs)))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn defaults_round_trip_through_json() {
        let settings = Settings::default();
        let json = serde_json::to_string_pretty(&settings).unwrap();
        let back: Settings = serde_json::from_str(&json).unwrap();
        assert_eq!(back, settings);
        assert_eq!(back.schema_version, SCHEMA_VERSION);
    }

    #[test]
    fn modified_values_round_trip() {
        let mut settings = Settings::default();
        settings.results.null_text = "∅".into();
        settings.results.page_size = 1000;
        settings.editor.run_scope = RunScope::Current;
        settings.navigation.restore_tabs = RestoreTabs::Pinned;
        settings.appearance.tab_group_by = TabGroupBy::Database;
        let back: Settings =
            serde_json::from_str(&serde_json::to_string(&settings).unwrap()).unwrap();
        assert_eq!(back, settings);
    }

    #[test]
    fn missing_and_unknown_fields_fall_back_to_defaults() {
        let json = r#"{
            "schema_version": 1,
            "future_section": {"anything": [1, 2, 3]},
            "results": {"null_text": "-", "mystery": true},
            "editor": {"soft_wrap": true}
        }"#;
        let settings: Settings = serde_json::from_str(json).unwrap();
        assert_eq!(settings.results.null_text, "-");
        assert!(settings.editor.soft_wrap);
        // Everything not mentioned keeps its default.
        assert_eq!(settings.results.page_size, Results::default().page_size);
        assert_eq!(settings.editor.tab_size, EditorSettings::default().tab_size);
        assert_eq!(settings.navigation, Navigation::default());
        let empty: Settings = serde_json::from_str("{}").unwrap();
        assert_eq!(empty, Settings::default());
    }

    fn assert_serde_names_match<C: Choice + Serialize + std::fmt::Debug>() {
        for (id, _) in C::OPTIONS {
            let value = C::from_id(id).unwrap_or_else(|| panic!("{id} has no variant"));
            assert_eq!(value.id(), *id);
            assert_eq!(serde_json::to_value(value).unwrap(), serde_json::json!(id));
        }
        assert!(C::from_id("no-such-option").is_none());
    }

    #[test]
    fn choice_ids_match_their_serde_names() {
        assert_serde_names_match::<ConnectionSort>();
        assert_serde_names_match::<TabGroupBy>();
        assert_serde_names_match::<RunScope>();
        assert_serde_names_match::<OnScriptError>();
        assert_serde_names_match::<KeywordCase>();
        assert_serde_names_match::<FormatterChoice>();
        assert_serde_names_match::<BooleanDisplay>();
        assert_serde_names_match::<DateTimeFormat>();
        assert_serde_names_match::<BinaryDisplay>();
        assert_serde_names_match::<MongoJsonMode>();
        assert_serde_names_match::<CopyDefault>();
        assert_serde_names_match::<TableOpenAction>();
        assert_serde_names_match::<RestoreTabs>();
        assert_serde_names_match::<Effort>();
        assert_serde_names_match::<AiProvider>();
    }

    #[test]
    fn the_ai_section_defaults_and_round_trips_without_a_key() {
        let defaults = AiSettings::default();
        assert_eq!(defaults.provider, AiProvider::Anthropic);
        assert_eq!(defaults.selected().model, "claude-opus-5-5");
        assert_eq!(defaults.effort, Effort::Medium);
        assert!(defaults.fallback);
        assert_eq!(defaults.selected().base_url, "https://api.anthropic.com");
        // The new providers start with their address and no model; a compatible server has neither.
        let providers = &defaults.providers;
        assert_eq!(providers.openai.base_url, "https://api.openai.com/v1");
        assert_eq!(providers.ollama.base_url, "http://localhost:11434/v1");
        assert_eq!(providers.openai_compatible, ProviderEntry::default());
        assert!([&providers.openai, &providers.ollama]
            .iter()
            .all(|entry| entry.model.is_empty()));
        let mut settings = Settings::default();
        settings.ai.effort = Effort::XHigh;
        settings.ai.fallback = false;
        settings.ai.provider = AiProvider::Ollama;
        settings.ai.selected_mut().model = "qwen3:8b".into();
        settings.ai.providers.anthropic.model = "claude-haiku-5-5".into();
        let json = serde_json::to_string(&settings).unwrap();
        assert!(json.contains(r#""effort":"xhigh""#), "{json}");
        assert!(json.contains(r#""provider":"ollama""#), "{json}");
        assert!(!json.to_lowercase().contains("api_key") && !json.contains("sk-"));
        let back: Settings = serde_json::from_str(&json).unwrap();
        assert_eq!(back.ai, settings.ai);
        assert_eq!(back, settings);
    }

    #[test]
    fn model_and_address_are_kept_per_provider() {
        let mut ai = AiSettings::default();
        ai.selected_mut().model = "claude-haiku-5-5".into();
        ai.provider = AiProvider::OpenAi;
        assert_eq!(
            ai.selected().model,
            "",
            "OpenAI does not inherit Anthropic's model"
        );
        assert_eq!(ai.selected().base_url, "https://api.openai.com/v1");
        ai.selected_mut().model = "gpt-x".into();
        ai.provider = AiProvider::Anthropic;
        assert_eq!(ai.selected().model, "claude-haiku-5-5");
        for provider in AiProvider::ALL {
            ai.provider = provider;
            assert_eq!(ai.selected(), ai.providers.entry(provider));
        }
    }

    #[test]
    fn the_key_rule_follows_the_engine_flavor() {
        assert!(AiProvider::Anthropic.requires_key());
        assert!(AiProvider::OpenAi.requires_key());
        assert!(!AiProvider::OpenAiCompatible.requires_key());
        assert!(!AiProvider::Ollama.requires_key());
        for provider in AiProvider::ALL {
            if let Some(flavor) = provider.flavor() {
                assert_eq!(provider.requires_key(), flavor.requires_key());
                assert_eq!(
                    Some(provider.default_base_url()).filter(|url| !url.is_empty()),
                    flavor.default_base_url()
                );
            }
        }
    }

    #[test]
    fn a_hand_edited_ai_section_is_repaired_on_load() {
        let mut ai = AiSettings::default();
        ai.providers.anthropic.model = "  ".into();
        ai.providers.anthropic.base_url = "http://api.example.com".into();
        ai.providers.openai.base_url = "ftp://example.com".into();
        ai.providers.openai_compatible.base_url = "http://api.example.com".into();
        ai.providers.ollama.base_url = String::new();
        ai.normalize();
        assert_eq!(ai, AiSettings::default());
        let mut local = AiSettings::default();
        local.providers.anthropic.base_url = "http://localhost:8080".into();
        local.providers.openai_compatible.base_url = "http://127.0.0.1:1234/v1".into();
        local.providers.openai.model = "gpt-x".into();
        local.normalize();
        assert_eq!(local.providers.anthropic.base_url, "http://localhost:8080");
        assert_eq!(
            local.providers.openai_compatible.base_url,
            "http://127.0.0.1:1234/v1"
        );
        assert_eq!(local.providers.openai.model, "gpt-x");
        // Only Anthropic has a model to fall back to; the others are left for the user to choose.
        local.providers.openai.model = " ".into();
        local.normalize();
        assert_eq!(local.providers.openai.model, " ");
    }

    #[test]
    fn timeouts_zero_means_none() {
        let mut settings = Settings::default();
        assert_eq!(settings.query_timeout(), None);
        settings.query.query_timeout_secs = 30;
        assert_eq!(
            settings.query_timeout(),
            Some(std::time::Duration::from_secs(30))
        );
    }
}
