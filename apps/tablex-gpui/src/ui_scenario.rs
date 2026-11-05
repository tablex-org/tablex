use crate::connections::SavedConnection;
use engine_storage::connection::SavedConnectionRecord;
use std::time::Duration;

pub const DEFAULT_DELAY_MS: u64 = 700;
pub const DEFAULT_TIMEOUT_DELAY_MS: u64 = 120;

#[cfg(feature = "ui-scenarios")]
pub fn emit_ui_state(state: serde_json::Value) {
    use std::io::Write;
    let event = serde_json::json!({"event": "ui-state", "phase": "render", "pid": std::process::id(), "state": state});
    let mut stdout = std::io::stdout().lock();
    let _ = writeln!(stdout, "{event}");
    let _ = stdout.flush();
}

#[cfg(feature = "ui-scenarios")]
pub fn emit_ready() {
    use std::io::Write;
    let event = serde_json::json!({"event": "ready", "phase": "render", "pid": std::process::id()});
    let mut stdout = std::io::stdout().lock();
    let _ = writeln!(stdout, "{event}");
    let _ = stdout.flush();
}

#[cfg(feature = "ui-scenarios")]
pub fn emit_ui_state_for_dialog(state: serde_json::Value, dialog_id: u64, window: &gpui::Window) {
    use std::io::Write;
    let frame = window.bounds();
    let event = serde_json::json!({
        "event": "ui-state",
        "phase": "render",
        "pid": std::process::id(),
        "dialog_id": dialog_id,
        "window_id": window.window_handle().window_id().as_u64(),
        "window_width": f32::from(frame.size.width),
        "window_height": f32::from(frame.size.height),
        "state": state,
    });
    let mut stdout = std::io::stdout().lock();
    let _ = writeln!(stdout, "{event}");
    let _ = stdout.flush();
}

#[cfg(feature = "ui-scenarios")]
pub fn emit_ui_dialog_closed(dialog_id: u64, window: &gpui::Window) {
    use std::io::Write;
    let event = serde_json::json!({
        "event": "ui-state",
        "phase": "closed",
        "pid": std::process::id(),
        "dialog_id": dialog_id,
        "window_id": window.window_handle().window_id().as_u64(),
        "closed": true,
    });
    let mut stdout = std::io::stdout().lock();
    let _ = writeln!(stdout, "{event}");
    let _ = stdout.flush();
}

#[cfg(feature = "ui-scenarios")]
pub fn emit_ui_bounds(
    target: &str,
    step: &str,
    dialog_id: u64,
    bounds: gpui::Bounds<gpui::Pixels>,
    enabled: bool,
    window: &gpui::Window,
) {
    use std::io::Write;
    let frame = window.bounds();
    let viewport = window.viewport_size();
    let chrome_height = (f32::from(frame.size.height) - f32::from(viewport.height)).max(0.0);
    let width = f32::from(bounds.size.width);
    let height = f32::from(bounds.size.height);
    let event = serde_json::json!({
        "event": "ui-bounds",
        "phase": "prepaint",
        "pid": std::process::id(),
        "target": target,
        "step": step,
        "dialog_id": dialog_id,
        "window_id": window.window_handle().window_id().as_u64(),
        "enabled": enabled,
        "visible": width > 0.0 && height > 0.0,
        "bounds": {
            "x": f32::from(bounds.origin.x),
            "y": f32::from(bounds.origin.y) + chrome_height,
            "width": width,
            "height": height,
        },
        "window_width": f32::from(frame.size.width),
        "window_height": f32::from(frame.size.height),
    });
    let mut stdout = std::io::stdout().lock();
    let _ = writeln!(stdout, "{event}");
    let _ = stdout.flush();
}

#[cfg(feature = "ui-scenarios")]
pub fn emit_sidebar_bounds(
    target: &str,
    key: &str,
    label: &str,
    node: &str,
    bounds: gpui::Bounds<gpui::Pixels>,
    window: &gpui::Window,
) {
    use std::io::Write;
    let frame = window.bounds();
    let viewport = window.viewport_size();
    let chrome_height = (f32::from(frame.size.height) - f32::from(viewport.height)).max(0.0);
    let width = f32::from(bounds.size.width);
    let height = f32::from(bounds.size.height);
    let event = serde_json::json!({
        "event": "ui-bounds",
        "phase": "prepaint",
        "pid": std::process::id(),
        "target": target,
        "key": key,
        "label": label,
        "node": node,
        "enabled": true,
        "visible": width > 0.0 && height > 0.0,
        "bounds": {
            "x": f32::from(bounds.origin.x),
            "y": f32::from(bounds.origin.y) + chrome_height,
            "width": width,
            "height": height,
        },
        "window_width": f32::from(frame.size.width),
        "window_height": f32::from(frame.size.height),
    });
    let mut stdout = std::io::stdout().lock();
    let _ = writeln!(stdout, "{event}");
    let _ = stdout.flush();
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ScenarioKind {
    NewConnection,
    ConnectionTree,
    /// The Connection tab of the Postgres fixture (`sample`: connected, several databases;
    /// `error`: failed; `disconnected`; `loading`: connecting).
    ConnectionOverview,
    ResultGrid,
    ResultFooter,
    MongoGrid,
    /// The Track table of a Chinook-like SQLite catalog (real FKs) for the
    /// ER diagram presentation.
    TableDiagram,
    /// The whole-database diagram tab of the same catalog.
    DatabaseDiagram,
    /// The `orders` table's Properties > Columns in structure edit mode with a rename, a type
    /// change, an added and a dropped column, an index and a foreign key pending
    /// (`TABLEX_SEED_PROPERTIES=review` opens the Changes dialog, `=type` leaves a Type cell open on
    /// its picker, `=indexes` / `=foreign-keys` open those sub-tabs).
    TablePropertiesEdit,
    /// The Driver Manager tab over a sample catalog (downloading, queued,
    /// update, disabled and developer rows).
    DriverManager,
    /// The same tab with the Tools section's `tablex-mcp` installed: its stable path as selectable
    /// text and the two copy buttons.
    DriverManagerTools,
    /// The "driver is not installed" dialog, mid-install, over the tab.
    MissingDriver,
    /// Two editor panes side by side: the orders grid (focused, left) and a
    /// SQL editor (right), 55/45.
    SplitPanes,
    /// `split-panes` with the drop-zone overlay of a tab dragged over the right
    /// edge of the right pane pinned on (a capture cannot hold a drag).
    SplitPanesDrop,
    /// Grouped-rows tab strip: connection chips on the top row, the active
    /// group's tabs on a second (two connections x several tabs).
    GroupedTabs,
    /// One pane, one group in rows mode: the group's chip leads the tabs row
    /// (after the pinned and global tabs) instead of taking a row.
    GroupedTabsLone,
    /// The Settings tab on the Editor page with pending (unapplied) edits:
    /// modified markers, per-setting reset and an enabled Apply.
    Settings,
    /// The Settings tab on the Shortcuts page with a changed chord, a removed
    /// one, a reset marker and a refused (conflicting) recording.
    SettingsShortcuts,
    /// The Edit Connection dialog of a Production connection, on its
    /// Overrides tab (one setting with its own value, the rest on "Use
    /// global"), over the grouped tab strip with the type colors.
    ConnectionOverrides,
    /// The Import selection dialog over the sidebar, listing a sample
    /// Navicat export (supported rows checked, unsupported ones disabled).
    ImportSelection,
    /// The Redis key browser: a connection with its keys grouped by `:`, a
    /// key filter, and (`TABLEX_SEED_REDIS=hash|string|console|confirm`) a
    /// value tab, the JSON string view, the command console with a result, or
    /// the unsafe-command confirmation.
    Redis,
    /// The assistant dock after a finished exchange: question, reasoning, one tool step, a
    /// Markdown answer with a list and a SQL block.
    Assistant,
    /// The assistant dock while an answer streams, with a tool step still running.
    AssistantStreaming,
    /// The assistant dock without an API key.
    AssistantNoKey,
    /// The assistant dock on a connection whose agent access is off: the first-use disclosure.
    AssistantAccessOff,
    /// The assistant dock after a provider error, with Retry.
    AssistantError,
    /// The assistant dock with an UPDATE waiting for the user's answer, on a development connection.
    AssistantApproval,
    /// The assistant dock with a `DELETE` without `WHERE` waiting, on a production connection.
    AssistantApprovalDangerous,
    /// The assistant dock with an UPDATE longer than the code box, not scrolled: the cue and the
    /// disabled Run.
    AssistantApprovalLong,
    /// The assistant's access chooser with the "Read and write" level.
    AssistantAccessWrite,
    /// The assistant's History list open on three stored conversations, one of them the open one.
    AssistantHistory,
    /// A stored conversation opened read-only: the connection's access is below the level it ran at.
    AssistantReadonly,
    /// The panel after "Explain" on a selection of the SQL editor: the composed message and an answer.
    AssistantEditorAction,
    /// Settings on the AI Assistant page.
    SettingsAi,
    /// Settings on the AI Assistant page with Ollama selected, a model chosen and an optional key.
    SettingsAiOllama,
}

impl ScenarioKind {
    pub fn name(self) -> &'static str {
        match self {
            Self::NewConnection => "new-connection",
            Self::ConnectionTree => "connection-tree",
            Self::ConnectionOverview => "connection-overview",
            Self::ResultGrid => "result-grid",
            Self::ResultFooter => "result-footer",
            Self::MongoGrid => "mongo-grid",
            Self::TableDiagram => "table-diagram",
            Self::DatabaseDiagram => "database-diagram",
            Self::TablePropertiesEdit => "table-properties-edit",
            Self::DriverManager => "driver-manager",
            Self::DriverManagerTools => "driver-manager-tools",
            Self::MissingDriver => "missing-driver",
            Self::SplitPanes => "split-panes",
            Self::SplitPanesDrop => "split-panes-drop",
            Self::GroupedTabs => "grouped-tabs",
            Self::GroupedTabsLone => "grouped-tabs-lone",
            Self::Settings => "settings",
            Self::SettingsShortcuts => "settings-shortcuts",
            Self::ConnectionOverrides => "connection-overrides",
            Self::ImportSelection => "import-selection",
            Self::Redis => "redis",
            Self::Assistant => "assistant",
            Self::AssistantStreaming => "assistant-streaming",
            Self::AssistantNoKey => "assistant-no-key",
            Self::AssistantAccessOff => "assistant-access-off",
            Self::AssistantError => "assistant-error",
            Self::AssistantApproval => "assistant-approval",
            Self::AssistantApprovalDangerous => "assistant-approval-dangerous",
            Self::AssistantApprovalLong => "assistant-approval-long",
            Self::AssistantAccessWrite => "assistant-access-write",
            Self::AssistantHistory => "assistant-history",
            Self::AssistantReadonly => "assistant-readonly",
            Self::AssistantEditorAction => "assistant-editor-action",
            Self::SettingsAi => "settings-ai",
            Self::SettingsAiOllama => "settings-ai-ollama",
        }
    }

    /// Scenarios backed by the Chinook-like diagram catalog.
    pub fn is_diagram(self) -> bool {
        matches!(self, Self::TableDiagram | Self::DatabaseDiagram)
    }

    fn parse(value: &str) -> Option<Self> {
        Some(match value {
            "new-connection" => Self::NewConnection,
            "connection-tree" => Self::ConnectionTree,
            "connection-overview" => Self::ConnectionOverview,
            "result-grid" => Self::ResultGrid,
            "result-footer" => Self::ResultFooter,
            "mongo-grid" => Self::MongoGrid,
            "table-diagram" => Self::TableDiagram,
            "database-diagram" => Self::DatabaseDiagram,
            "table-properties-edit" => Self::TablePropertiesEdit,
            "driver-manager" => Self::DriverManager,
            "driver-manager-tools" => Self::DriverManagerTools,
            "missing-driver" => Self::MissingDriver,
            "split-panes" => Self::SplitPanes,
            "split-panes-drop" => Self::SplitPanesDrop,
            "grouped-tabs" => Self::GroupedTabs,
            "grouped-tabs-lone" => Self::GroupedTabsLone,
            "settings" => Self::Settings,
            "settings-shortcuts" => Self::SettingsShortcuts,
            "connection-overrides" => Self::ConnectionOverrides,
            "import-selection" => Self::ImportSelection,
            "redis" => Self::Redis,
            "assistant" => Self::Assistant,
            "assistant-streaming" => Self::AssistantStreaming,
            "assistant-no-key" => Self::AssistantNoKey,
            "assistant-access-off" => Self::AssistantAccessOff,
            "assistant-error" => Self::AssistantError,
            "assistant-approval" => Self::AssistantApproval,
            "assistant-approval-dangerous" => Self::AssistantApprovalDangerous,
            "assistant-approval-long" => Self::AssistantApprovalLong,
            "assistant-access-write" => Self::AssistantAccessWrite,
            "assistant-history" => Self::AssistantHistory,
            "assistant-readonly" => Self::AssistantReadonly,
            "assistant-editor-action" => Self::AssistantEditorAction,
            "settings-ai" => Self::SettingsAi,
            "settings-ai-ollama" => Self::SettingsAiOllama,
            _ => return None,
        })
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum FixtureKind {
    Sample,
    Populated,
    PendingEdits,
    Empty,
    Loading,
    Error,
    Narrow,
    /// `result-grid` with the Postgres connection dropped: orders keeps its
    /// stale rows under the reconnect banner, a second empty Query tab shows
    /// the full placeholder.
    Disconnected,
    /// `connection-tree` with one connection per status (connected, connecting,
    /// failed, disconnected, behind SSH, selected) and a node whose load failed.
    States,
}

impl FixtureKind {
    pub fn name(self) -> &'static str {
        match self {
            Self::Sample => "sample",
            Self::Populated => "populated",
            Self::PendingEdits => "pending-edits",
            Self::Empty => "empty",
            Self::Loading => "loading",
            Self::Error => "error",
            Self::Narrow => "narrow",
            Self::Disconnected => "disconnected",
            Self::States => "states",
        }
    }

    fn parse(value: &str) -> Option<Self> {
        Some(match value {
            "sample" => Self::Sample,
            "populated" => Self::Populated,
            "pending-edits" => Self::PendingEdits,
            "empty" => Self::Empty,
            "loading" => Self::Loading,
            "error" => Self::Error,
            "narrow" => Self::Narrow,
            "disconnected" => Self::Disconnected,
            "states" => Self::States,
            _ => return None,
        })
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct UiScenario {
    pub kind: ScenarioKind,
    pub fixture: FixtureKind,
    pub delay: Duration,
    pub timeout_delay: Duration,
}

impl UiScenario {
    pub fn from_args<I, S>(args: I) -> Result<Option<Self>, String>
    where
        I: IntoIterator<Item = S>,
        S: Into<String>,
    {
        let args = args.into_iter().map(Into::into).skip(1).collect::<Vec<_>>();
        if args.is_empty() {
            return Ok(None);
        }

        let mut scenario = None;
        let mut fixture = None;
        let mut index = 0;
        while index < args.len() {
            let flag = &args[index];
            let value = args.get(index + 1).ok_or_else(|| {
                format!("missing value for {flag}; expected --ui-scenario new-connection --ui-fixture sample")
            })?;
            match flag.as_str() {
                "--ui-scenario" => {
                    if scenario.is_some() {
                        return Err("duplicate --ui-scenario flag".to_string());
                    }
                    let Some(kind) = ScenarioKind::parse(value) else {
                        return Err(format!(
                            "unsupported UI scenario '{value}'; expected new-connection, connection-tree, connection-overview, result-grid, result-footer, mongo-grid, table-diagram, database-diagram, table-properties-edit, driver-manager, driver-manager-tools, missing-driver, split-panes, split-panes-drop, grouped-tabs, grouped-tabs-lone, settings, settings-shortcuts, connection-overrides, import-selection, redis, assistant, assistant-streaming, assistant-no-key, assistant-access-off, assistant-error, assistant-approval, assistant-approval-dangerous, assistant-approval-long, assistant-access-write, assistant-history, assistant-readonly, assistant-editor-action, settings-ai, or settings-ai-ollama"
                        ));
                    };
                    scenario = Some(kind);
                }
                "--ui-fixture" => {
                    if fixture.is_some() {
                        return Err("duplicate --ui-fixture flag".to_string());
                    }
                    let Some(kind) = FixtureKind::parse(value) else {
                        return Err(format!(
                            "unknown UI fixture '{value}'; expected sample, populated, pending-edits, empty, loading, error, narrow, disconnected, or states"
                        ));
                    };
                    fixture = Some(kind);
                }
                _ if flag.starts_with('-') => return Err(format!("unknown flag '{flag}'")),
                _ => return Err(format!("unexpected argument '{flag}'")),
            }
            index += 2;
        }

        let kind = scenario.ok_or_else(|| "missing --ui-scenario".to_string())?;
        let fixture = fixture.ok_or_else(|| "missing --ui-fixture".to_string())?;
        Ok(Some(Self {
            kind,
            fixture,
            delay: Duration::from_millis(DEFAULT_DELAY_MS),
            timeout_delay: Duration::from_millis(DEFAULT_TIMEOUT_DELAY_MS),
        }))
    }

    pub fn with_environment(mut self) -> Result<Self, String> {
        self.delay = duration_from_env("TABLEX_UI_SCENARIO_DELAY_MS", self.delay)?;
        self.timeout_delay =
            duration_from_env("TABLEX_UI_SCENARIO_TIMEOUT_MS", self.timeout_delay)?;
        Ok(self)
    }

    pub fn name(&self) -> &'static str {
        self.kind.name()
    }

    pub fn fixture_name(&self) -> &'static str {
        self.fixture.name()
    }
}

pub fn ensure_feature_enabled(
    scenario: Option<&UiScenario>,
    feature_enabled: bool,
) -> Result<(), String> {
    if scenario.is_some() && !feature_enabled {
        return Err(
            "UI scenarios are disabled in this build; rebuild with --features ui-scenarios"
                .to_string(),
        );
    }
    Ok(())
}

fn duration_from_env(name: &str, default: Duration) -> Result<Duration, String> {
    let Some(value) = std::env::var_os(name) else {
        return Ok(default);
    };
    let value = value
        .to_str()
        .ok_or_else(|| format!("{name} must be an integer number of milliseconds"))?;
    let milliseconds = value
        .parse::<u64>()
        .map_err(|_| format!("{name} must be an integer number of milliseconds"))?;
    Ok(Duration::from_millis(milliseconds))
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum FixtureOutcome {
    Success,
    Error,
    Timeout,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct FixtureResponse {
    pub outcome: FixtureOutcome,
}

impl FixtureResponse {
    pub fn for_name(name: &str) -> Self {
        let name = name.to_ascii_lowercase();
        let outcome = if name.contains("fixture-timeout") {
            FixtureOutcome::Timeout
        } else if name.contains("fixture-error") {
            FixtureOutcome::Error
        } else {
            FixtureOutcome::Success
        };
        Self { outcome }
    }
}

#[derive(Debug)]
pub struct FixtureService {
    scenario: UiScenario,
    fixture: crate::ui_fixture::UiFixture,
    saved_connections: Vec<SavedConnection>,
    last_saved_record: Option<SavedConnectionRecord>,
    test_count: u64,
    save_count: u64,
}

impl FixtureService {
    pub fn new(scenario: UiScenario) -> Self {
        let mut fixture = crate::ui_fixture::UiFixture::new(scenario.fixture);
        if scenario.kind == ScenarioKind::Redis {
            fixture.add_redis();
        }
        Self {
            fixture,
            scenario,
            saved_connections: Vec::new(),
            last_saved_record: None,
            test_count: 0,
            save_count: 0,
        }
    }

    pub fn test(&mut self, name: &str) -> (FixtureResponse, Duration) {
        self.test_count += 1;
        self.response(name)
    }

    pub fn connect(&self, name: &str) -> (FixtureResponse, Duration) {
        self.response(name)
    }

    fn response(&self, name: &str) -> (FixtureResponse, Duration) {
        let response = FixtureResponse::for_name(name);
        let delay = match response.outcome {
            FixtureOutcome::Timeout => self.scenario.timeout_delay,
            FixtureOutcome::Success | FixtureOutcome::Error => self.scenario.delay,
        };
        (response, delay)
    }

    pub fn save(&mut self, record: SavedConnectionRecord) {
        let saved = SavedConnection::from_record(&record);
        self.last_saved_record = Some(record);
        if let Some(existing) = self
            .saved_connections
            .iter_mut()
            .find(|existing| existing.id == saved.id)
        {
            *existing = saved;
        } else {
            self.saved_connections.push(saved);
        }
        self.save_count += 1;
    }

    #[cfg(test)]
    /// Last record written by `save` (fixture stand-in for storage).
    pub fn last_saved_record(&self) -> Option<&SavedConnectionRecord> {
        self.last_saved_record.as_ref()
    }

    pub fn saved_connections(&self) -> &[SavedConnection] {
        if self.saved_connections.is_empty() {
            self.fixture.connections()
        } else {
            &self.saved_connections
        }
    }

    pub fn scenario(&self) -> &UiScenario {
        &self.scenario
    }

    pub fn fixture(&self) -> &crate::ui_fixture::UiFixture {
        &self.fixture
    }

    pub fn fixture_mut(&mut self) -> &mut crate::ui_fixture::UiFixture {
        &mut self.fixture
    }

    pub fn load(
        &self,
        request: &crate::navigator::LoadRequest,
    ) -> Result<crate::ui_fixture::FixtureLoad, String> {
        self.fixture.load(request)
    }

    pub fn query_page(
        &self,
        connection: &str,
        database: &str,
        schema: &str,
        object: &str,
        offset: u64,
        limit: i64,
    ) -> Result<crate::query::QueryResultBatch, String> {
        self.fixture
            .query_page(connection, database, schema, object, offset, limit)
    }

    /// `table` is `(connection, database, schema, object)`; `fragments` is
    /// `(where, order_by)`.
    pub fn query_sql_table_page(
        &self,
        table: (&str, &str, &str, &str),
        offset: u64,
        limit: i64,
        fragments: (&str, &str),
    ) -> Result<crate::query::QueryResultBatch, String> {
        self.fixture
            .query_sql_table_page(table, offset, limit, fragments)
    }

    #[allow(clippy::too_many_arguments)] // each parameter is one input of the operation; a parameter struct would only rename the list
    pub fn query_page_with_options(
        &self,
        connection: &str,
        database: &str,
        schema: &str,
        object: &str,
        offset: u64,
        limit: i64,
        filter: Option<&str>,
        sort: Option<&str>,
    ) -> Result<crate::query::QueryResultBatch, String> {
        self.fixture.query_page_with_options(
            connection, database, schema, object, offset, limit, filter, sort,
        )
    }

    #[allow(clippy::too_many_arguments)] // each parameter is one input of the operation; a parameter struct would only rename the list
    pub fn query_mongo_page(
        &mut self,
        connection: &str,
        database: &str,
        collection: &str,
        offset: u64,
        limit: i64,
        filter: Option<&str>,
        sort: Option<&str>,
    ) -> Result<crate::ui_fixture::FixtureMongoPage, String> {
        self.fixture.query_mongo_page(
            connection, database, collection, offset, limit, filter, sort,
        )
    }

    #[allow(clippy::too_many_arguments)] // each parameter is one input of the operation; a parameter struct would only rename the list
    pub fn query_mongo_aggregate(
        &mut self,
        connection: &str,
        database: &str,
        collection: &str,
        explain: bool,
        payload: &serde_json::Value,
        offset: u64,
        limit: i64,
    ) -> Result<crate::ui_fixture::FixtureMongoPage, String> {
        self.fixture.aggregate_mongo_page(
            connection, database, collection, explain, payload, offset, limit,
        )
    }

    pub fn apply_mongo_mutations(
        &mut self,
        connection: &str,
        database: &str,
        collection: &str,
        requests: &[engine_driver_api::MutationRequest],
    ) -> Result<usize, String> {
        self.fixture
            .apply_mongo_mutations(connection, database, collection, requests)
    }

    pub fn apply_sql_updates(
        &mut self,
        connection: &str,
        database: &str,
        schema: &str,
        object: &str,
        updates: &[crate::staged_sql::UpdateCellRequest],
    ) -> Result<usize, String> {
        self.fixture
            .apply_sql_updates(connection, database, schema, object, updates)
    }

    pub fn test_count(&self) -> u64 {
        self.test_count
    }

    pub fn save_count(&self) -> u64 {
        self.save_count
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_the_approved_new_connection_fixture() {
        let scenario = UiScenario::from_args([
            "tablex-gpui",
            "--ui-scenario",
            "new-connection",
            "--ui-fixture",
            "sample",
        ])
        .expect("scenario flags should parse")
        .expect("scenario should be enabled");

        assert_eq!(scenario.kind, ScenarioKind::NewConnection);
        assert_eq!(scenario.fixture, FixtureKind::Sample);
    }

    #[test]
    fn rejects_missing_fixture_and_unknown_flags() {
        assert!(UiScenario::from_args(["tablex-gpui", "--ui-scenario", "new-connection"]).is_err());
        assert!(UiScenario::from_args([
            "tablex-gpui",
            "--ui-scenario",
            "unknown",
            "--ui-fixture",
            "sample",
        ])
        .is_err());
        assert!(UiScenario::from_args(["tablex-gpui", "--ui-fixture", "sample"]).is_err());
        assert!(
            UiScenario::from_args(["tablex-gpui", "--ui-scenario", "new-connection", "--wat"])
                .is_err()
        );
    }

    #[test]
    fn parses_populated_tree_grid_and_footer_scenarios() {
        for (name, kind) in [
            ("connection-tree", ScenarioKind::ConnectionTree),
            ("connection-overview", ScenarioKind::ConnectionOverview),
            ("result-grid", ScenarioKind::ResultGrid),
            ("result-footer", ScenarioKind::ResultFooter),
            ("mongo-grid", ScenarioKind::MongoGrid),
            ("table-diagram", ScenarioKind::TableDiagram),
            ("database-diagram", ScenarioKind::DatabaseDiagram),
            ("table-properties-edit", ScenarioKind::TablePropertiesEdit),
            ("driver-manager", ScenarioKind::DriverManager),
            ("driver-manager-tools", ScenarioKind::DriverManagerTools),
            ("missing-driver", ScenarioKind::MissingDriver),
            ("split-panes", ScenarioKind::SplitPanes),
            ("split-panes-drop", ScenarioKind::SplitPanesDrop),
            ("grouped-tabs", ScenarioKind::GroupedTabs),
            ("grouped-tabs-lone", ScenarioKind::GroupedTabsLone),
            ("settings", ScenarioKind::Settings),
            ("settings-shortcuts", ScenarioKind::SettingsShortcuts),
            ("connection-overrides", ScenarioKind::ConnectionOverrides),
            ("import-selection", ScenarioKind::ImportSelection),
            ("assistant", ScenarioKind::Assistant),
            ("assistant-streaming", ScenarioKind::AssistantStreaming),
            ("assistant-no-key", ScenarioKind::AssistantNoKey),
            ("assistant-access-off", ScenarioKind::AssistantAccessOff),
            ("assistant-error", ScenarioKind::AssistantError),
            ("assistant-approval", ScenarioKind::AssistantApproval),
            (
                "assistant-approval-dangerous",
                ScenarioKind::AssistantApprovalDangerous,
            ),
            (
                "assistant-approval-long",
                ScenarioKind::AssistantApprovalLong,
            ),
            ("assistant-access-write", ScenarioKind::AssistantAccessWrite),
            ("assistant-history", ScenarioKind::AssistantHistory),
            ("assistant-readonly", ScenarioKind::AssistantReadonly),
            (
                "assistant-editor-action",
                ScenarioKind::AssistantEditorAction,
            ),
            ("settings-ai", ScenarioKind::SettingsAi),
            ("settings-ai-ollama", ScenarioKind::SettingsAiOllama),
        ] {
            let scenario = UiScenario::from_args([
                "tablex-gpui",
                "--ui-scenario",
                name,
                "--ui-fixture",
                "populated",
            ])
            .expect("scenario flags should parse")
            .expect("scenario should be enabled");
            assert_eq!(scenario.kind, kind);
            assert_eq!(scenario.name(), name);
            assert_eq!(scenario.fixture_name(), "populated");
        }
    }

    #[test]
    fn rejects_unknown_fixture_profiles() {
        assert!(UiScenario::from_args([
            "tablex-gpui",
            "--ui-scenario",
            "result-grid",
            "--ui-fixture",
            "not-a-fixture",
        ])
        .is_err());
    }

    #[test]
    fn rejects_scenarios_when_the_feature_is_not_enabled() {
        let scenario = UiScenario::from_args([
            "tablex-gpui",
            "--ui-scenario",
            "new-connection",
            "--ui-fixture",
            "sample",
        ])
        .unwrap();
        assert!(ensure_feature_enabled(scenario.as_ref(), false).is_err());
        assert!(ensure_feature_enabled(scenario.as_ref(), true).is_ok());
    }

    #[test]
    fn fixture_response_is_selected_by_name_without_exposing_form_values() {
        assert_eq!(
            FixtureResponse::for_name("sample").outcome,
            FixtureOutcome::Success
        );
        assert_eq!(
            FixtureResponse::for_name("fixture-error").outcome,
            FixtureOutcome::Error
        );
        assert_eq!(
            FixtureResponse::for_name("fixture-timeout").outcome,
            FixtureOutcome::Timeout
        );
    }

    #[test]
    fn fixture_uses_normal_and_timeout_delays() {
        let scenario = UiScenario::from_args([
            "tablex-gpui",
            "--ui-scenario",
            "new-connection",
            "--ui-fixture",
            "sample",
        ])
        .unwrap()
        .unwrap();
        let mut service = FixtureService::new(scenario);
        assert_eq!(service.test("normal").1, Duration::from_millis(700));
        assert_eq!(
            service.test("fixture-timeout").1,
            Duration::from_millis(120)
        );
        assert_eq!(service.test_count(), 2);
    }
}
