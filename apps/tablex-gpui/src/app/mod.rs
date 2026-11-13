use crate::cell_detail::{
    self, CellDetail, CellDetailColumn, CellDetailPlacement, CellDetailRow, CellDetailRowField,
    CellDetailTab,
};
use crate::connection_dashboard::{self, ConnectionTabState};
use crate::connection_dialog::ConnectionDialogInputs;
use crate::connection_form::ConnectionFormState;
use crate::connections::{ConnectionKind, SavedConnection};
use crate::error_log::Area;
#[cfg(test)]
use crate::error_log::Report;
use crate::grid::{CellPosition, GridState};
use crate::grid_arrow::ArrowGridSource;
use crate::grid_bson::BsonGridSource;
use crate::grid_source::GridContentSource;
use crate::mongo_result::PendingMongoMutation;
use crate::navigator::{
    self, KindOfObject, LoadRequest, NavigatorNodeId, NavigatorRow, NavigatorState, ObjectGroup,
};
use crate::query::QueryTaskRegistry;
use crate::sessions::{ConnectionSessions, ConnectionStatus};
use crate::staged_sql::{DeleteRowRequest, InsertRowRequest, MutationValue};
use crate::ui_scenario::{FixtureKind, FixtureOutcome, FixtureService, UiScenario};
use crate::workspace::{QueryMode, QueryModeExt, Tab, TabKind, TabMetadata, WorkspaceState};
use crate::{theme, ui};
use engine_driver_api::{Capability, MutationRequest, OperationId, QueryRequest};
use engine_export::{ExportDialect, ExportFormat, TableExporter};
use gpui::prelude::FluentBuilder;
use gpui::{
    div, px, uniform_list, AnyElement, AppContext, Context, Div, FocusHandle, Focusable,
    InteractiveElement, IntoElement, ListHorizontalSizingBehavior, MouseButton, MouseDownEvent,
    ParentElement, Point, Render, ScrollHandle, StatefulInteractiveElement, Styled, Task,
    UniformListScrollHandle, Window,
};
use gpui_component::button::{Button, ButtonVariants, DropdownButton};
use gpui_component::checkbox::Checkbox;
use gpui_component::input::{Editor, EditorState, Input, InputState, Textarea, TextareaState};
use gpui_component::menu::{ContextMenuExt, DropdownMenu, PopupMenuItem};
use gpui_component::popover::Popover;
use gpui_component::scroll::{ScrollableElement, Scrollbar, ScrollbarMode};
use gpui_component::switch::Switch;
use gpui_component::WindowExt;
use gpui_component::{Disableable, Icon, IconName, Selectable, Sizable, Size};
use std::collections::{HashMap, HashSet};
use std::io::Write as _;
use std::ops::Range;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::Arc;
use std::time::Duration;
use std::time::Instant;

mod actions;
mod aggregate;
mod assistant;
mod command_palette;
mod completion;
mod connection_menu;
mod connection_menu_view;
mod connection_overview;
mod connection_policy;
mod connection_ui;
mod data_import;
mod accounts;
mod backup;
mod file_run;
mod data_transfer;
mod driver_manager;
mod drivers;
mod error_log_view;
mod explain;
mod menus;
mod mongo_admin;
mod mongo_admin_view;
mod mongo_script;
mod mongo_shell;
mod navigator_status;
mod navigator_ui;
mod object_actions;
mod panes_ui;
mod params;
mod process_list;
mod recovery;
mod redis;
mod remote_search;
mod results;
mod scenario;
mod script_run;
mod script_toolbar;
mod session_persist;
mod settings_ai;
mod settings_format;
mod settings_state;
mod settings_view;
mod shell;
mod shortcuts;
mod sidebar;
mod sidebar_batch;
mod sidebar_batch_ui;
mod sidebar_config;
mod sidebar_import;
mod smart_commit;
mod source_tab;
mod sql_editor;
mod structure_edit;
mod tab_bar;
#[cfg(test)]
mod test_data;
mod transaction;
mod transaction_dialogs;
mod tunnel_ui;
mod updates;
mod workspace;

use actions::*;
use completion::{EditorCompletion, Role as CompletionRole, TabCompletion};
use connection_ui::current_time;
#[cfg(test)]
use connection_ui::{connection_dialog_height, connection_dialog_width, runtime_uri_for_form};
use params::AfterParams;
use drivers::{
    cancel_driver_operation, is_stale_driver_page, release_driver_page_async,
    superseded_driver_page, DriverMongoResult, DriverPageGuard, DriverQueryError,
    DriverQueryOperation, ExplainRun, ExplainRunError, PreparedDriverQuery, RunKind, SqlRun,
};
use results::column_picker::{column_picker_footer, columns_button_label, ColumnPicker};
use results::editing::{
    insert_values_from_json, mutation_value_from_cell, mutation_value_from_input, row_json_cell,
    row_value_to_json, InlineEditExit, InlineEditPayload, SqlEditError,
};
#[cfg(test)]
use results::editing::{stage_inline_sql_edit, try_stage_inline_sql_edit};
use results::grid_column_type_color;
#[cfg(test)]
use results::mongo::mongo_duplicate_text;
use results::mongo::{
    build_inline_mongo_update, mongo_columns_button_label, mongo_detail_value, mongo_edit_refusal,
    mongo_document_preview, mongo_filter_conjunction_label, mongo_filter_dropdown_button,
    mongo_filter_element_id, mongo_filter_mode_label, mongo_filter_type_label,
    mongo_json_display_value, mongo_json_object_is_valid, mongo_query_payload,
    mongo_query_signature, mongo_query_signature_from_parts, mongo_tree_value_color,
    rebuild_mongo_pending_overlay,
};
pub(crate) use shell::open_with_scenario;
use workspace::restore_swift_sql_tabs;

/// Incremented on every AppRoot render — sampled by the FPS timer to measure
/// event-driven frame rate. Idle app = no renders = 0 fps.
static FRAME_COUNT: AtomicU64 = AtomicU64::new(0);

/// Frames drawn so far: what a value computed once per frame is keyed on.
pub(in crate::app) fn frame_count() -> u64 {
    FRAME_COUNT.load(Ordering::Relaxed)
}

/// The schema select of the open visible-objects filter dialog: the connection it belongs to, the
/// select and its window.
type VisibleFilterSchemaSelect = (
    String,
    gpui::Entity<gpui_component::select::SelectState<Vec<String>>>,
    gpui::AnyWindowHandle,
);

pub struct AppRoot {
    /// Per-tab result view state (see `results::TabView`).
    tab_views: HashMap<u64, results::TabView>,
    /// Cached view of each pane (`panes_ui::PaneView`).
    pane_views: HashMap<crate::panes::PaneId, gpui::Entity<panes_ui::PaneView>>,
    /// Read-only stand-in for a tab without a view; never kept.
    detached_view: results::TabView,
    /// Result page of a tab without a runtime.
    empty_grid: GridState<GridContentSource>,
    grid_prefs: results::GridPrefs,
    /// The window's `Root`, notified for chrome-only changes (see
    /// `results::surface`). Captured on the first render.
    window_root: Option<gpui::WeakEntity<gpui_component::Root>>,
    workspace: WorkspaceState,
    query_tasks: workspace::QueryTaskState,
    auto_refresh: workspace::AutoRefreshState,
    columns_filter_inputs: HashMap<u64, gpui::Entity<InputState>>,
    /// The Connection tab's search, order and cached grid / database list, by tab.
    connection_overviews: HashMap<u64, connection_overview::ConnectionOverview>,
    /// ER diagram view entity per table tab (see `results::diagram`).
    diagram_views: HashMap<u64, gpui::Entity<results::diagram::view::DiagramView>>,
    /// Structure edit mode of a table tab's Properties (in memory; see `results::properties_edit`).
    properties_edits: HashMap<u64, gpui::Entity<results::properties_edit::PropertiesEdit>>,
    /// The navigator's object action whose review dialog is (or was last) open: drop, rename,
    /// truncate, new table, new schema / database (see `object_actions`).
    object_action: Option<gpui::Entity<object_actions::ObjectActionHost>>,
    /// The fetched text and read-only editor of each source tab (see `source_tab`).
    source_tabs: HashMap<u64, source_tab::SourceTabState>,
    /// The wizard of each import tab (see `data_import`).
    imports: data_import::ImportViews,
    /// The wizard of each SQL file run tab (see `file_run`).
    file_runs: file_run::FileRunViews,
    /// The SQL editor tab a `.sql` file is being dragged over (its drop highlight).
    file_drop_tab: Option<u64>,
    /// The cancel handle of each SQL file run in flight, kept from the moment its worker starts.
    file_run_controls: HashMap<u64, Arc<crate::app::structure_edit::apply::ApplyControl>>,
    /// The wizard of each backup and restore tab (see `backup`).
    backups: backup::BackupViews,
    /// The tab of each connection's Users & Privileges (R7).
    accounts: accounts::AccountsViews,
    /// Test double: where the Users & Privileges tabs read their catalogs from.
    #[cfg(test)]
    accounts_injected: Option<Arc<accounts::InjectedCatalog>>,
    /// The tab of each connection's Process List.
    process_lists: process_list::ProcessListViews,
    /// Test double: where the Process List tabs get their session from.
    #[cfg(test)]
    process_list_injected: Option<Arc<process_list::InjectedSession>>,
    /// Test probe: how often a Process List tab had to read its gate again.
    #[cfg(test)]
    process_list_gate_reads: std::cell::Cell<usize>,
    /// The cancel token of each backup or restore in flight, kept from the moment its worker starts.
    backup_controls: HashMap<u64, Arc<crate::backup::process::CancelToken>>,
    /// How many worker threads of backups and restores are alive (a quit waits for them to have cleaned up).
    backup_workers: Arc<std::sync::atomic::AtomicUsize>,
    /// The private directories of the runs that are alive (removed all at once when the application quits).
    backup_dirs: crate::backup::secrets::PrivateDirs,
    /// The wizards of the data-transfer tabs (R4b).
    transfers: data_transfer::TransferViews,
    /// The review of the tables a transfer creates, until it is answered.
    transfer_create: std::collections::HashMap<u64, data_transfer::TransferCreate>,
    /// The review of the table an import creates, until it is answered.
    import_create: Option<data_import::ImportCreate>,
    navigator: NavigatorState,
    sessions: ConnectionSessions,
    connection_ui: connection_ui::ConnectionUiState,
    filter_enumeration: connection_menu::FilterEnumerationState,
    saved_sql_ui: connection_menu::SavedSqlUiState,
    history_popup: Option<(
        String,
        u64,
        gpui::WeakEntity<gpui_component::menu::PopupMenu>,
        gpui::AnyWindowHandle,
    )>,
    /// Where imported connections and their passwords are saved; in memory in
    /// a UI scenario.
    import_store: Arc<dyn crate::connection_import::store::ConnectionImportStore>,
    /// The open import selection dialog, if any.
    import_selection: Option<gpui::Entity<sidebar_import::ImportSelection>>,
    visible_filter_dialog: Option<(
        String,
        gpui::Entity<connection_menu_view::VisibleFilterDialogState>,
    )>,
    visible_filter_schema_select: Option<VisibleFilterSchemaSelect>,
    navigator_ui: navigator_ui::NavigatorUiState,
    sidebar: sidebar::SidebarState,
    saved_connections: Vec<SavedConnection>,
    /// Connection type and setting overrides of the saved connections.
    connection_policy: connection_policy::ConnectionPolicy,
    query_editors: workspace::WorkspaceEditors,
    format_ui: sql_editor::FormatUi,
    mongo_ui: results::mongo::MongoUiState,
    aggregate_ui: aggregate::AggregateUi,
    script_toolbar: script_toolbar::ScriptToolbarUi,
    /// MongoDB shell scripts: the engine, scripts waiting for the plugin, runs in flight.
    mongo_shell: mongo_shell::ShellUi,
    /// Saved-aggregates store (opened on first use) and the per-database
    /// lists the sidebar group is built from.
    aggregate_store: aggregate::saved::StoreSlot,
    /// An implicit store failure is reported once, not on every sidebar load.
    aggregate_store_error_shown: bool,
    saved_aggregates: HashMap<(String, String), Vec<engine_storage::aggregate::SavedAggregate>>,
    sql_table_ui: results::sql_table::SqlTableUiState,
    /// Redis key tabs: string editors and the TTL countdown.
    redis_ui: redis::RedisUi,
    /// Table ER diagram data per tab (fetch schedule + cache).
    diagram: results::diagram::data::DiagramState,
    /// Where diagram entity positions are saved; `None` in scenarios and
    /// unit tests (they inject a temp dir when they test persistence).
    diagram_layouts: Option<Arc<results::diagram::store::LayoutStore>>,
    /// Runs recorded in the query history (tests only: the real store is the
    /// user's file).
    #[cfg(test)]
    recorded_history: Vec<workspace::RecordedHistory>,
    /// Live driver-backed sessions keyed by saved-connection UUID.
    driver_runtime: drivers::DriverRuntime,
    /// Wake detection, automatic reconnect-and-retry of reads, idle liveness checks.
    recovery: recovery::Recovery,
    /// Manual commit mode of SQL tabs: the open transactions and their Commit / Rollback.
    transactions: transaction::TransactionUi,
    /// SSH tunnels of the live connections, the secrets they were opened with and the dialogs
    /// that ask for host-key trust, passwords and one-time codes.
    tunnel_ui: tunnel_ui::TunnelUi,
    /// Loaded driver handles keyed by driver id ("mongodb"), shared across
    /// sessions that use the same driver kind.
    driver_ui: drivers::DriverManagerUi,
    scenario_ui: scenario::ScenarioUiState,
    /// Frames-per-second sampled every 500ms; displayed in the bottom bar.
    shell: shell::ShellState,
    /// Error Log model, persistence queue and pane state.
    error_log: error_log_view::ErrorLogUi,
    /// Command palette (Cmd+K / Cmd+P): list entity, recents, catalog cache.
    palette: command_palette::PaletteState,
    /// The applied settings: a cheap snapshot, replaced whole by
    /// `apply_settings` (see `settings`).
    settings: Arc<crate::settings::Settings>,
    /// Values the grid renderer reads, derived from `settings`.
    grid_display: settings_state::GridDisplay,
    settings_ui: settings_state::SettingsUi,
    /// The AI assistant: tool layer, API key, models, the chat (see `assistant`).
    assistant: assistant::AssistantUi,
    /// The update check and its notice (see `updates`).
    update_ui: updates::UpdateUi,
}

struct ColumnResizeGhost;

impl Render for ColumnResizeGhost {
    fn render(&mut self, _window: &mut Window, _cx: &mut Context<Self>) -> impl IntoElement {
        div()
    }
}

impl AppRoot {
    pub fn new(cx: &mut Context<Self>) -> Self {
        Self::new_with_scenario(None, cx)
    }

    pub fn new_for_scenario(scenario: UiScenario, cx: &mut Context<Self>) -> Self {
        Self::new_with_scenario(Some(scenario), cx)
    }

    /// A fixture-scenario app for tests: never the user's connections or files.
    #[cfg(test)]
    pub fn new_for_test(cx: &mut Context<Self>) -> Self {
        let scenario = UiScenario::from_args([
            "tablex-gpui",
            "--ui-scenario",
            "new-connection",
            "--ui-fixture",
            "sample",
        ])
        .ok()
        .flatten()
        .expect("the new-connection sample scenario parses");
        Self::new_for_scenario(scenario, cx)
    }

    /// A real-mode app for tests that need the real flows (driver sessions, the connection
    /// store), over a temporary data dir.
    #[cfg(test)]
    pub fn new_isolated(cx: &mut Context<Self>) -> Self {
        test_data::opt_in_real_mode();
        Self::new_with_scenario(None, cx)
    }

    fn new_with_scenario(scenario: Option<UiScenario>, cx: &mut Context<Self>) -> Self {
        #[cfg(test)]
        if scenario.is_none() {
            test_data::assert_real_mode_isolated();
        } else {
            test_data::isolate();
        }
        let scenario_enabled = scenario.is_some();
        let scenario_root = scenario_enabled.then(|| {
            sidebar::scenario_prefs_path()
                .and_then(|path| path.parent().map(std::path::Path::to_path_buf))
                .filter(|root| {
                    std::fs::canonicalize(root)
                        .ok()
                        .zip(std::fs::canonicalize(std::env::temp_dir()).ok())
                        .is_some_and(|(root, temp)| root != temp)
                })
                .unwrap_or_else(|| {
                    std::env::temp_dir()
                        .join(format!("tablex-scenario-sql-{}", uuid::Uuid::new_v4()))
                })
        });
        let ui_scenario = scenario.clone().map(FixtureService::new);
        let (saved_connections, connection_policy) = if let Some(service) = ui_scenario.as_ref() {
            (
                service.saved_connections().to_vec(),
                connection_policy::ConnectionPolicy::default(),
            )
        } else {
            let records = crate::connections::load_saved_records();
            (
                records.iter().map(SavedConnection::from_record).collect(),
                connection_policy::ConnectionPolicy::from_records(&records),
            )
        };
        let mut navigator = NavigatorState::new(saved_connections.clone());
        let sidebar = sidebar::SidebarState::new(scenario_enabled, &mut navigator, cx);
        let settings_path =
            settings_state::resolve_settings_path(scenario_enabled, sidebar.path.as_deref());
        let workspace = WorkspaceState::default();
        // A 0.1.0 store is moved into the app folder here, before anything opens the store.
        let driver_data_dir = if scenario_enabled {
            crate::drivers::DriverDataDir {
                path: std::path::PathBuf::from("__tablex_ui_scenario_driver_store_disabled__"),
                moved: None,
                warning: None,
            }
        } else {
            // Said once, at start (as `tablex-mcp` does): a relative value is not used.
            if std::env::var_os(crate::drivers::DRIVER_DATA_DIR_ENV).is_some_and(|v| !v.is_empty())
                && crate::drivers::driver_data_dir_override().is_none()
            {
                eprintln!(
                    "TableX: ignoring {}: it is not an absolute path",
                    crate::drivers::DRIVER_DATA_DIR_ENV
                );
            }
            crate::drivers::resolve_driver_data_dir()
        };
        let (driver_store_moved, driver_store_warning) =
            (driver_data_dir.moved, driver_data_dir.warning);
        let driver_store = Arc::new(engine_driver_host::InstalledDriverStore::new(
            driver_data_dir.path,
        ));
        let (mut tunnel_ui, tunnel_prompts) = tunnel_ui::TunnelUi::new(scenario_enabled);
        tunnel_ui.attach_agent(Arc::clone(&driver_store));
        let assistant = assistant::AssistantUi::production(
            tunnel_ui.agent.clone(),
            scenario_enabled || cfg!(test),
        );
        let mut app = Self {
            tab_views: HashMap::new(),
            pane_views: HashMap::new(),
            detached_view: results::TabView::default(),
            empty_grid: GridState::new(GridContentSource::Arrow(ArrowGridSource::new(Vec::new()))),
            grid_prefs: results::GridPrefs::new(scenario_root.as_deref()),
            window_root: None,
            workspace,
            query_tasks: workspace::QueryTaskState::default(),
            auto_refresh: workspace::AutoRefreshState::default(),
            columns_filter_inputs: HashMap::new(),
            connection_overviews: HashMap::new(),
            diagram_views: HashMap::new(),
            properties_edits: HashMap::new(),
            object_action: None,
            source_tabs: HashMap::new(),
            imports: HashMap::new(),
            file_runs: HashMap::new(),
            file_drop_tab: None,
            file_run_controls: HashMap::new(),
            backups: HashMap::new(),
            accounts: HashMap::new(),
            #[cfg(test)]
            accounts_injected: None,
            process_lists: HashMap::new(),
            #[cfg(test)]
            process_list_injected: None,
            #[cfg(test)]
            process_list_gate_reads: std::cell::Cell::new(0),
            backup_controls: HashMap::new(),
            backup_workers: Default::default(),
            backup_dirs: Default::default(),
            import_create: None,
            transfers: Default::default(),
            transfer_create: Default::default(),
            navigator,
            sessions: ConnectionSessions::default(),
            connection_ui: connection_ui::ConnectionUiState::default(),
            filter_enumeration: connection_menu::FilterEnumerationState::default(),
            saved_sql_ui: {
                let mut state = connection_menu::SavedSqlUiState::default();
                if let Some(base) = &scenario_root {
                    state.set_source(base.clone(), false);
                } else if let Some(base) = crate::saved_sql::default_projects_base() {
                    state.set_source(base, true);
                }
                state
            },
            history_popup: None,
            import_store: if scenario_enabled {
                Arc::new(crate::connection_import::store::MemoryImportStore::default())
            } else {
                Arc::new(crate::connection_import::store::SystemImportStore)
            },
            import_selection: None,
            visible_filter_dialog: None,
            visible_filter_schema_select: None,
            navigator_ui: navigator_ui::NavigatorUiState::default(),
            sidebar,
            saved_connections,
            connection_policy,
            query_editors: workspace::WorkspaceEditors::default(),
            format_ui: sql_editor::FormatUi::default(),
            mongo_ui: results::mongo::MongoUiState::default(),
            aggregate_ui: aggregate::AggregateUi::default(),
            script_toolbar: script_toolbar::ScriptToolbarUi::default(),
            mongo_shell: mongo_shell::ShellUi::default(),
            aggregate_store: Arc::new(std::sync::OnceLock::new()),
            aggregate_store_error_shown: false,
            saved_aggregates: HashMap::new(),
            sql_table_ui: results::sql_table::SqlTableUiState::default(),
            redis_ui: redis::RedisUi::default(),
            diagram: Default::default(),
            diagram_layouts: (!scenario_enabled && !cfg!(test))
                .then(results::diagram::store::LayoutStore::default_location)
                .flatten()
                .map(Arc::new),
            #[cfg(test)]
            recorded_history: Vec::new(),
            driver_runtime: drivers::DriverRuntime::default(),
            recovery: recovery::Recovery::default(),
            transactions: transaction::TransactionUi::default(),
            tunnel_ui,
            // The installed set is read off the UI thread (`start_driver_startup`).
            driver_ui: drivers::DriverManagerUi::new(driver_store),
            scenario_ui: scenario::ScenarioUiState::new(ui_scenario),
            shell: shell::ShellState::default(),
            error_log: error_log_view::ErrorLogUi::new(cx),
            palette: Default::default(),
            settings: Arc::new(crate::settings::Settings::default()),
            grid_display: settings_state::GridDisplay::from_settings(
                &crate::settings::Settings::default(),
            ),
            settings_ui: settings_state::SettingsUi::new(settings_path),
            assistant,
            update_ui: updates::UpdateUi::new(scenario_enabled || cfg!(test)),
        };
        // Every tab's cached results view is stale after a plain notification
        // of the root unless it was announced as chrome-only.
        cx.observe_self(|this, cx| this.on_root_notified(cx))
            .detach();
        app.tunnel_ui._quit = Some(tunnel_ui::listen_for_prompts(tunnel_prompts, cx));
        app.driver_runtime._quit = Some(cx.on_app_quit(|this, _| this.disconnect_sessions_for_quit()));
        app.init_assistant(cx);
        #[cfg(not(test))]
        if !scenario_enabled {
            app.start_recovery(cx);
        }
        #[cfg(feature = "ui-scenarios")]
        if scenario_enabled {
            app.seed_connected_tunnels();
        }
        // Tests build real (non-scenario) apps on a deterministic scheduler
        // and must not touch the user's driver store; they call
        // `start_driver_startup` themselves where it is the subject.
        #[cfg(not(test))]
        app.start_driver_startup(cx);
        #[cfg(test)]
        {
            app.driver_ui.installed_loaded = true;
        }
        app.init_error_log(
            crate::error_log::LogStore::resolve(scenario_root.as_deref()),
            cx,
        );
        app.report_startup_problems();
        if let Some(moved) = driver_store_moved {
            app.report_event(
                None,
                crate::error_log::Report::new(
                    crate::error_log::Severity::Info,
                    Area::Driver,
                    moved,
                ),
            );
        }
        if let Some(warning) = driver_store_warning {
            app.report_warning(Area::Driver, None, warning);
        }
        // Sidebar geometry: persisted width when valid, 260px default.
        // Applies to real and scenario constructions alike.
        app.shell.sidebar_width = app.navigator.preferences().resolve_sidebar_width();
        app.assistant.dock.width = app.navigator.preferences().resolve_assistant_width();
        app.assistant.dock.open = app.navigator.preferences().assistant_open;
        app.init_settings(cx);
        // Tests call `start_launch_update_check` themselves, with a fetcher of their own.
        #[cfg(not(test))]
        if !scenario_enabled {
            app.start_launch_update_check(cx);
        }
        if scenario_enabled {
            app.apply_scenario_theme(cx);
        }
        app.apply_tab_bar_preferences(cx);
        app.init_workspace_session(scenario_enabled, cx);
        if matches!(
            scenario.as_ref().map(|scenario| scenario.kind),
            Some(
                crate::ui_scenario::ScenarioKind::ResultGrid
                    | crate::ui_scenario::ScenarioKind::ResultFooter
                    | crate::ui_scenario::ScenarioKind::MongoGrid
                    | crate::ui_scenario::ScenarioKind::TableDiagram
                    | crate::ui_scenario::ScenarioKind::DatabaseDiagram
                    | crate::ui_scenario::ScenarioKind::TablePropertiesEdit
                    | crate::ui_scenario::ScenarioKind::SplitPanes
                    | crate::ui_scenario::ScenarioKind::SplitPanesDrop
                    | crate::ui_scenario::ScenarioKind::GroupedTabs
                    | crate::ui_scenario::ScenarioKind::GroupedTabsLone
                    | crate::ui_scenario::ScenarioKind::ConnectionOverrides
            )
        ) {
            app.seed_scenario_query();
        }
        match scenario.as_ref().map(|scenario| scenario.kind) {
            Some(crate::ui_scenario::ScenarioKind::DriverManager) => {
                app.seed_driver_manager_scenario(false);
            }
            Some(crate::ui_scenario::ScenarioKind::DriverManagerTools) => {
                app.seed_driver_manager_scenario(false);
                app.seed_driver_tools_fixture(true);
                app.seed_sql_formatter_fixture(driver_manager::FormatterSeed::Available);
                app.driver_ui.selected = None;
                app.driver_ui.model.set_filter(driver_manager::DriverFilter::Tools);
            }
            Some(crate::ui_scenario::ScenarioKind::MissingDriver) => {
                app.seed_driver_manager_scenario(true);
            }
            Some(crate::ui_scenario::ScenarioKind::Redis) => app.seed_redis_scenario(cx),
            Some(crate::ui_scenario::ScenarioKind::ConnectionOverview) => {
                app.seed_connection_overview_scenario();
            }
            Some(crate::ui_scenario::ScenarioKind::Settings) => app.seed_settings_scenario(),
            #[cfg(any(test, feature = "ui-scenarios"))]
            Some(crate::ui_scenario::ScenarioKind::SettingsAi) => app.seed_settings_ai_scenario(),
            #[cfg(any(test, feature = "ui-scenarios"))]
            Some(crate::ui_scenario::ScenarioKind::SettingsAiOllama) => {
                app.seed_settings_ai_ollama_scenario()
            }
            #[cfg(any(test, feature = "ui-scenarios"))]
            Some(
                kind @ (crate::ui_scenario::ScenarioKind::Assistant
                | crate::ui_scenario::ScenarioKind::AssistantStreaming
                | crate::ui_scenario::ScenarioKind::AssistantNoKey
                | crate::ui_scenario::ScenarioKind::AssistantAccessOff
                | crate::ui_scenario::ScenarioKind::AssistantError
                | crate::ui_scenario::ScenarioKind::AssistantApproval
                | crate::ui_scenario::ScenarioKind::AssistantApprovalDangerous
                | crate::ui_scenario::ScenarioKind::AssistantApprovalLong
                | crate::ui_scenario::ScenarioKind::AssistantAccessWrite
                | crate::ui_scenario::ScenarioKind::AssistantHistory
                | crate::ui_scenario::ScenarioKind::AssistantReadonly
                | crate::ui_scenario::ScenarioKind::AssistantEditorAction),
            ) => app.seed_assistant_scenario(kind, cx),
            Some(crate::ui_scenario::ScenarioKind::SettingsShortcuts) => {
                app.seed_settings_shortcuts_scenario();
            }
            _ => {}
        }
        if matches!(
            scenario.as_ref().map(|scenario| scenario.kind),
            Some(crate::ui_scenario::ScenarioKind::ConnectionTree)
        ) && std::env::var("TABLEX_SEED_CONNECTION_TREE").as_deref() == Ok("1")
        {
            // Capture-only deterministic tree (see sidebar.rs): unit tests
            // construct the same scenario without this env var and keep a
            // collapsed tree for async locate/disclosure coverage.
            app.seed_connection_tree_scenario();
        }
        if !scenario_enabled {
            cx.spawn(async move |this, cx| {
                let mut last = FRAME_COUNT.load(Ordering::Relaxed);
                loop {
                    cx.background_executor()
                        .timer(Duration::from_millis(500))
                        .await;
                    if this
                        .update(cx, |this, cx| {
                            let now = FRAME_COUNT.load(Ordering::Relaxed);
                            let fps = now.saturating_sub(last) * 2;
                            last = now;
                            if this.shell.fps_display != fps {
                                this.shell.fps_display = fps;
                                if this.settings.appearance.show_fps {
                                    this.notify_chrome(cx);
                                }
                            }
                        })
                        .is_err()
                    {
                        break;
                    }
                }
            })
            .detach();
        }
        app
    }
}

#[cfg(test)]
mod tests {
    use super::object_actions::ObjectActionHost;
    use super::results::mongo::mongo_value_from_input;
    use super::*;
    use gpui::{KeyBinding, ScrollDelta, ScrollEvent, TestAppContext};
    use gpui_component::WindowExt;

    include!("tests/aggregate.rs");
    include!("tests/completion.rs");
    include!("tests/auto_refresh.rs");
    include!("tests/connection_menu_view.rs");
    include!("tests/mongo.rs");
    include!("tests/mongo_values.rs");
    include!("tests/mongo_value_panel.rs");
    include!("tests/mongo_document_edit.rs");
    include!("tests/mongo_tree_copy.rs");
    include!("tests/mongo_tree_perf.rs");
    include!("tests/mongo_admin.rs");
    include!("tests/results.rs");
    include!("tests/value_correctness.rs");
    include!("tests/value_copy.rs");
    include!("tests/grid_render_perf.rs");
    include!("tests/grid_find.rs");
    include!("tests/tab_views.rs");
    include!("tests/script_toolbar.rs");
    include!("tests/mongo_shell.rs");
    include!("tests/connections.rs");
    include!("tests/connection_state.rs");
    include!("tests/connection_overview.rs");
    include!("tests/navigator.rs");
    include!("tests/sidebar.rs");
    include!("tests/navigator_status.rs");
    include!("tests/sidebar_batch.rs");
    include!("tests/sidebar_import.rs");
    include!("tests/editing.rs");
    include!("tests/cell_detail.rs");
    include!("tests/binary_view.rs");
    include!("tests/pinned_columns.rs");
    include!("tests/header_sort.rs");
    include!("tests/record_view.rs");
    include!("tests/chart.rs");
    include!("tests/sql_table.rs");
    include!("tests/sql_filter.rs");
    include!("tests/sql_columns.rs");
    include!("tests/properties.rs");
    include!("tests/properties_edit.rs");
    include!("tests/object_actions.rs");
    include!("tests/generate_sql.rs");
    include!("tests/data_import.rs");
    include!("tests/data_transfer.rs");
    include!("tests/file_run.rs");
    // The tools are stood in for by shell scripts.
    #[cfg(unix)]
    include!("tests/backup.rs");
    mod accounts_ui {
        use super::*;
        include!("tests/accounts.rs");
    }
    mod process_list_ui {
        use super::*;
        include!("tests/process_list.rs");
    }
    include!("tests/properties_perf.rs");
    include!("tests/diagram.rs");
    include!("tests/diagram_d2.rs");
    include!("tests/diagram_d3.rs");
    include!("tests/diagram_data.rs");
    include!("tests/tab_bar.rs");
    include!("tests/panes.rs");
    include!("tests/tab_rows.rs");
    include!("tests/editor_layout.rs");
    include!("tests/driver_contention.rs");
    include!("tests/sql_driver.rs");
    include!("tests/recovery.rs");
    include!("tests/script_run.rs");
    include!("tests/explain.rs");
    include!("tests/sql_editor.rs");
    include!("tests/driver_manager.rs");
    include!("tests/driver_manager_live.rs");
    include!("tests/sql_driver_edit.rs");
    include!("tests/sql_review_fixes.rs");
    include!("tests/sql_navigator.rs");
    include!("tests/schema_objects.rs");
    include!("tests/clickhouse.rs");
    include!("tests/redis.rs");
    include!("tests/error_log.rs");
    include!("tests/command_palette.rs");
    include!("tests/settings.rs");
    include!("tests/assistant_panel.rs");
    include!("tests/shortcuts.rs");
    include!("tests/reopen_tab.rs");
    include!("tests/updates.rs");
    include!("tests/connection_types.rs");
    include!("tests/connection_tls.rs");
    include!("tests/ssh_tunnel.rs");
    include!("tests/ssh_tunnel_live.rs");
    include!("tests/transactions.rs");
    include!("tests/transactions_smart.rs");

    /// GPUI `BenchContext` scroll benchmark; compiled only with the dev-only
    /// `bench` feature (see the file header for the run command).
    #[cfg(feature = "bench")]
    mod bench_context {
        use super::*;
        include!("tests/grid_bench_context.rs");
        include!("tests/properties_bench_context.rs");
        include!("tests/chart_bench_context.rs");
        include!("tests/ssh_tunnel_capture.rs");
        include!("tests/explain_capture.rs");
        include!("tests/sql_editor_capture.rs");
        include!("tests/shell_capture.rs");
        include!("tests/sql_formatter_capture.rs");
        include!("tests/script_run_capture.rs");
        include!("tests/p5_capture.rs");
        include!("tests/p6a_capture.rs");
        use crate::app::object_actions::{ObjectKind, ObjectMenuItem};
        include!("tests/p6b_capture.rs");
        include!("tests/r4a_capture.rs");
        include!("tests/r4b_capture.rs");
        include!("tests/r5_capture.rs");
        include!("tests/f1_capture.rs");
        include!("tests/f2_capture.rs");
        include!("tests/r6_capture.rs");
        include!("tests/r7_capture.rs");
        include!("tests/r8_capture.rs");
        include!("tests/mongo_document_edit_capture.rs");
        include!("tests/agent_access_capture.rs");
        include!("tests/transaction_capture.rs");
    }
}
