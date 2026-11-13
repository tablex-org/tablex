use crate::cell_detail::CellDetailState;
use engine_driver_api::sql::SchemaObjectRef;
use crate::grid::CellPosition;
use crate::grid::GridState;
use crate::grid_arrow::ArrowGridSource;
use crate::grid_source::GridContentSource;
use crate::mongo_result::MongoResultState;
use crate::query::{PaginationState, QueryState, DEFAULT_OUTPUT_LIMIT};
pub use crate::query_mode::{QueryMode, QueryModeExt};
use std::collections::{HashMap, HashSet};
use std::sync::Arc;

pub use crate::panes::{StripSettings, TabGroupKey};
use crate::panes::{Edge, PaneDrop, PaneId, PaneTree, SplitId};
pub use crate::sidebar_preferences::TabGroupBy;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TabKind {
    Welcome,
    Query,
    Connection,
    DriverManager,
    /// Application settings (singleton, like the Driver Manager).
    Settings,
    /// Whole-schema ER diagram (connection, database and optional schema
    /// live in the tab's identity fields).
    Diagram,
    /// Read-only source of one schema object (function, trigger, sequence,
    /// ...): the connection, database and schema are in the tab's identity
    /// fields, the object in [`Tab::source`]. The text is fetched, never kept.
    Source,
    /// A data import into a table of a connection (R4a): the connection, database and schema are in the
    /// tab's identity fields; the wizard itself is an `ImportView` entity of the app. Never persisted: it
    /// holds a file and a running load that a restart does not resume.
    Import,
    /// A data transfer between tables, a query and files (R4b): the source's connection, database and schema are in
    /// the tab's identity fields; the wizard is a `TransferView` entity of the app. Never persisted: it holds a
    /// running transfer that a restart does not resume.
    Transfer,
    /// A SQL file run (R5): the connection, database and schema it runs in are in the tab's identity fields; the
    /// wizard is a `FileRunView` entity of the app. Never persisted: it holds a file and a running script that a
    /// restart does not resume.
    SqlFile,
    /// A backup with the server's native tool (R6): the connection, database and schema it backs up are in the tab's
    /// identity fields; the wizard is a `BackupView` entity of the app. Never persisted: it holds a running tool that a
    /// restart does not resume.
    Backup,
    /// A restore of a backup (R6); like [`Self::Backup`].
    Restore,
    /// The users and privileges of a connection (R7), one tab per connection: the connection is in the tab's identity
    /// fields; the editor is an `AccountsView` entity of the app. Never persisted: it holds a read of the server and edits
    /// that must be reviewed, which a restart does not resume.
    Accounts,
    /// The sessions running on a connection's server (one tab per connection): the connection is in the tab's identity fields; the
    /// list is a `ProcessListView` entity of the app. Never persisted: it is a live read of the server.
    ProcessList,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Tab {
    pub id: u64,
    pub kind: TabKind,
    pub title: String,
    /// Legacy Swift tab identity, when this tab came from persisted state.
    pub source_id: Option<String>,
    /// Saved connection UUID this tab runs against (empty = no connection).
    pub connection_id: String,
    /// Database selected for this tab (empty = connection default).
    pub database: String,
    pub schema: Option<String>,
    pub collection: String,
    pub query_mode: Option<QueryMode>,
    pub is_pinned: bool,
    pub sql: String,
    pub output_limit: i64,
    /// `sqlTable` tabs: WHERE editor buffer (without the keyword). This is
    /// only the draft the user is typing; queries use [`Self::applied_where`].
    pub table_where: String,
    /// `sqlTable` tabs: ORDER BY editor buffer (without the keyword); a draft
    /// like [`Self::table_where`], queries use [`Self::applied_order_by`].
    pub table_order_by: String,
    /// `sqlTable` tabs: WHERE fragment of the query that is applied. Updated
    /// only on Run/Enter/Clear/sort, so paging and refresh keep re-running
    /// the query the user actually executed.
    pub applied_where: String,
    /// `sqlTable` tabs: applied ORDER BY fragment (see [`Self::applied_where`]).
    pub applied_order_by: String,
    /// `sqlTable` tabs: structured filter builder rules (persisted per tab).
    /// Applying them writes the generated fragment into [`Self::table_where`].
    pub table_filter: crate::sql_filter::SqlFilterState,
    /// `sqlTable` tabs: names of result columns hidden in the grid (display
    /// only; the query still selects them). Persisted per tab.
    pub table_hidden_columns: Vec<String>,
    /// The result columns pinned to the left of the grid (name and occurrence
    /// among same-named columns), in pin order (display only, like the hidden
    /// columns). Persisted per tab; a script run keeps its own per result tab
    /// (`ScriptResult::pinned`).
    pub pinned_columns: Vec<crate::pinned_column::PinnedColumn>,
    /// `mongoAggregate` tabs: the pipeline being edited (both editor forms,
    /// options, saved identity). `None` for every other tab.
    pub aggregate: Option<crate::aggregate::AggregateDraft>,
    /// The `.sql` file this tab's text is bound to (Open SQL File, Save As), if any.
    pub file: Option<SqlFile>,
    /// `Source` tabs: the object whose source the tab shows.
    pub source: Option<SchemaObjectRef>,
}

/// A tab's binding to a `.sql` file on disk.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SqlFile {
    pub path: std::path::PathBuf,
    /// The text as last read from or written to the file; the tab is unsaved while its text
    /// differs.
    pub saved: Arc<str>,
}

impl Tab {
    /// Whether the tab is bound to a file and its text is not what the file holds. A length
    /// check first, so a tab being typed in costs nothing; a clean one compares its bytes.
    pub fn has_unsaved_file(&self) -> bool {
        self.file
            .as_ref()
            .is_some_and(|file| file.saved.len() != self.sql.len() || *file.saved != *self.sql)
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct TabMetadata {
    pub source_id: Option<String>,
    pub connection_id: String,
    pub database: String,
    pub schema: Option<String>,
    pub collection: String,
    pub query_mode: Option<QueryMode>,
    pub is_pinned: bool,
}

impl Tab {
    fn new(id: u64, kind: TabKind, title: String) -> Self {
        Self {
            id,
            kind,
            title,
            source_id: None,
            connection_id: String::new(),
            database: String::new(),
            schema: None,
            collection: String::new(),
            query_mode: None,
            is_pinned: false,
            sql: String::new(),
            output_limit: DEFAULT_OUTPUT_LIMIT,
            table_where: String::new(),
            table_order_by: String::new(),
            applied_where: String::new(),
            applied_order_by: String::new(),
            table_filter: Default::default(),
            table_hidden_columns: Vec::new(),
            pinned_columns: Vec::new(),
            aggregate: None,
            file: None,
            source: None,
        }
    }
}

/// See [`TabRuntime::applied_rows`].
#[derive(Debug)]
pub struct AppliedRows {
    grid: GridState<GridContentSource>,
    /// Rows whose change was committed.
    rows: HashSet<usize>,
    /// Rows whose change MAY be committed (the driver could not tell): the
    /// change stays staged.
    unknown: HashSet<usize>,
}

/// A database sort of a custom SQL result: the statement on screen is
/// `sorted`, the unsorted statement it came from is `base`.
#[derive(Debug)]
pub struct ContextSqlSort {
    pub base: String,
    pub sorted: String,
    /// Column the statement is sorted by, and whether it is descending.
    pub column: String,
    pub descending: bool,
}

/// Per-tab runtime state that never touches GPUI entities.
#[derive(Debug)]
pub struct TabRuntime {
    pub query: QueryState,
    pub pagination: PaginationState,
    pub grid: GridState<GridContentSource>,
    pub driver_page_id: Option<u64>,
    /// Replaced through [`Self::set_structure_columns`] only, which tells the caches keyed on `structure_generation`; tests that
    /// write it directly opt out of that.
    pub structure_columns: Vec<crate::catalog::CatalogColumn>,
    /// Bumped by [`Self::set_structure_columns`]: which catalog `structure_columns` holds, which its address and length do not
    /// say (a catalog that is replaced by another of the same size may land at the address of the one before).
    pub structure_generation: u64,
    pub structure_error: Option<String>,
    /// A catalog load for `structure_columns` is in flight.
    pub structure_loading: bool,
    pub pending_edits: Vec<PendingCellEdit>,
    pub pending_sql_rows: Vec<PendingSqlRow>,
    pub sql_apply_running: bool,
    pending_cell_values: HashMap<CellPosition, Arc<str>>,
    pub column_width_overrides: HashMap<usize, (String, f32)>,
    pub pending_deleted_rows: HashSet<usize>,
    /// Rows a partial Apply committed while other changes stay staged: they
    /// show the values from before the commit until the page is reloaded.
    pub applied_rows: Option<AppliedRows>,
    pub mongo: MongoResultState,
    pub cell_detail: CellDetailState,
    pub columns_panel: crate::columns_panel::ColumnsPanelState,
    /// Refresh cadence in seconds; `None` means refresh is off.
    pub auto_refresh_seconds: Option<u64>,
    pub last_query_ms: Option<u64>,
    pub last_executed_query: Option<String>,
    /// The SQL statement of the run in flight. It becomes `last_executed_query`
    /// (what paging re-sends) only when its result is shown, so a refused,
    /// failed or cancelled run never replaces the statement on screen.
    pub pending_sql: Option<String>,
    pub context_sql_sort: Option<ContextSqlSort>,
    /// `sqlTable` tabs: bar UI state (validation error, expand popovers).
    pub sql_table: SqlTableUi,
    /// Redis key tabs: what the key is (type, TTL, length) and its string value.
    pub redis: crate::redis::TabState,
    /// The plan of the last Explain run on this tab (shown by the Plan presentation).
    pub plan: Option<crate::plan_view::PlanView>,
    /// The last MongoDB shell run of a script tab: its Output, text result, error and the
    /// cursor "More" continues. `None` while the grid shows a find.
    pub shell: Option<crate::mongo_shell::ShellView>,
    /// The script run in flight (several statements, one at a time).
    pub script_run: Option<crate::script_run::ScriptRun>,
    /// The last script run: its result tabs and Statistics. Cleared by the next run typed in
    /// the editor; paging and Refresh of a result tab keep it.
    pub script: Option<crate::script_run::ScriptView>,
    /// The single-statement run in flight came from the editor: its result replaces the script's.
    pub editor_run_pending: bool,
    /// What the user typed for `:name` / `${name}` parameters, offered again by the next run.
    pub param_values: HashMap<String, crate::sql_params::Value>,
}

/// Transient UI state of a `sqlTable` tab's WHERE / ORDER BY bar.
#[derive(Debug, Default, Clone, PartialEq)]
pub struct SqlTableUi {
    /// Fragment validation failure (nothing was run).
    pub error: Option<String>,
    pub where_expanded: bool,
    pub order_expanded: bool,
    pub preview_open: bool,
    /// Filter-builder popover is open.
    pub filter_open: bool,
    /// Last Apply failure shown inside the builder (nothing was applied).
    pub filter_error: Option<String>,
    /// Columns picker popover is open.
    pub columns_open: bool,
    /// Columns picker search text.
    pub column_search: String,
}

/// A row's primary key as column -> wire value: every key column, read from
/// the grid at the moment a change was staged. The staged change is addressed
/// by this key, never by its row index in whichever grid is shown later.
pub type RowKey = serde_json::Map<String, serde_json::Value>;

#[derive(Debug, Clone, PartialEq)]
pub struct PendingCellEdit {
    pub row: usize,
    pub column: usize,
    pub original_text: Arc<str>,
    /// The cell was NULL before the edit: `original_text` is then only the
    /// grid's placeholder, and typing it is a change.
    pub original_is_null: bool,
    pub display_text: Arc<str>,
    pub request: crate::staged_sql::UpdateCellRequest,
    /// The whole primary key of the edited row (the driver route sends it).
    pub key: RowKey,
}

#[derive(Debug, Clone, PartialEq)]
pub enum PendingSqlRow {
    Insert(crate::staged_sql::InsertRowRequest),
    Delete {
        row: usize,
        request: crate::staged_sql::DeleteRowRequest,
        /// The whole primary key of the deleted row (the driver route sends it).
        key: RowKey,
    },
}

impl Default for TabRuntime {
    fn default() -> Self {
        Self {
            query: QueryState::default(),
            pagination: PaginationState::default(),
            grid: GridState::new(GridContentSource::Arrow(ArrowGridSource::new(Vec::new()))),
            driver_page_id: None,
            structure_columns: Vec::new(),
            structure_generation: 0,
            structure_error: None,
            structure_loading: false,
            pending_edits: Vec::new(),
            pending_sql_rows: Vec::new(),
            sql_apply_running: false,
            pending_cell_values: HashMap::new(),
            column_width_overrides: HashMap::new(),
            pending_deleted_rows: HashSet::new(),
            applied_rows: None,
            mongo: MongoResultState::new(),
            cell_detail: CellDetailState::default(),
            columns_panel: Default::default(),
            auto_refresh_seconds: None,
            last_query_ms: None,
            last_executed_query: None,
            pending_sql: None,
            context_sql_sort: None,
            sql_table: SqlTableUi::default(),
            redis: crate::redis::TabState::default(),
            plan: None,
            shell: None,
            script_run: None,
            script: None,
            editor_run_pending: false,
            param_values: HashMap::new(),
        }
    }
}

impl TabRuntime {
    pub fn new() -> Self {
        Self::default()
    }

    /// The catalog the table's columns are read from has been replaced (an empty one: it could not be read).
    pub fn set_structure_columns(&mut self, columns: Vec<crate::catalog::CatalogColumn>) {
        self.structure_columns = columns;
        self.structure_generation += 1;
    }

    /// The in-flight SQL run's result is on screen: its statement is now the
    /// executed one (its page is committed by the pagination setters).
    pub fn commit_sql_run(&mut self) {
        if let Some(sql) = self.pending_sql.take() {
            self.last_executed_query = Some(sql);
        }
    }

    /// The in-flight SQL run ended without a result (failed or cancelled): the
    /// statement and page on screen stay.
    pub fn abandon_sql_run(&mut self) {
        self.pending_sql = None;
        self.editor_run_pending = false;
        self.script_run = None;
        self.pagination.abandon_page();
    }

    /// Cancels the live run, if any, and forgets its statement and page.
    pub fn cancel_sql_run(&mut self) -> Option<crate::query::QueryRunId> {
        let run = self.query.cancel()?;
        self.abandon_sql_run();
        Some(run)
    }

    /// Whether the tab has run a query in this session (or holds a result):
    /// New Query tabs show only their editor until then. Derived from state
    /// every run path already sets (status leaves Idle on `begin`, including
    /// failures), so Run, Run Selected and shortcuts all reveal results.
    pub fn has_run(&self) -> bool {
        !matches!(self.query.status(), crate::query::QueryStatus::Idle)
            || self.driver_page_id.is_some()
            || self.last_executed_query.is_some()
    }

    /// Whether the tab holds anything the user has staged but not committed
    /// (or is committing right now): cell edits, staged SQL inserts/deletes,
    /// Mongo mutations, or an in-flight apply. The single source of truth for
    /// close confirmations, the run gate and sort/refresh guards.
    pub fn has_staged_changes(&self) -> bool {
        self.has_staged_sql_changes() || self.mongo.has_pending_mutations()
    }

    /// Staged SQL cell edits, inserts and deletes, or an Apply in flight: all
    /// of them are tied to the rows of the grid on screen.
    pub fn has_staged_sql_changes(&self) -> bool {
        !self.pending_edits.is_empty()
            || !self.pending_sql_rows.is_empty()
            || !self.pending_deleted_rows.is_empty()
            || self.sql_apply_running
    }

    /// Marks display rows of the grid on screen after a partial Apply: `rows`
    /// were committed (their cells still show the old values), `unknown` may
    /// have been (a driver that lost the answer for them).
    pub fn mark_rows_applied(
        &mut self,
        rows: impl IntoIterator<Item = usize>,
        unknown: impl IntoIterator<Item = usize>,
    ) {
        let grid = self.grid.clone();
        match &mut self.applied_rows {
            Some(applied) if applied.grid.same_page(&grid) => {
                applied.rows.extend(rows);
                applied.unknown.extend(unknown);
            }
            slot => {
                *slot = Some(AppliedRows {
                    grid,
                    rows: rows.into_iter().collect(),
                    unknown: unknown.into_iter().collect(),
                })
            }
        }
    }

    /// Whether row `row` was committed by a partial Apply and the grid on
    /// screen still shows its old values. A reloaded page is another grid, so
    /// the mark ends with it; the check is a pointer comparison and a lookup.
    pub fn row_is_applied(&self, row: usize) -> bool {
        self.applied_rows
            .as_ref()
            .is_some_and(|applied| applied.rows.contains(&row) && applied.grid.same_page(&self.grid))
    }

    /// Whether the driver could not say if row `row`'s staged change was
    /// committed (same lifetime as [`Self::row_is_applied`]).
    pub fn row_is_unknown(&self, row: usize) -> bool {
        self.applied_rows
            .as_ref()
            .is_some_and(|applied| applied.unknown.contains(&row) && applied.grid.same_page(&self.grid))
    }

    pub fn pending_cell_text(&self, position: CellPosition) -> Option<&Arc<str>> {
        self.pending_cell_values.get(&position)
    }

    pub fn set_pending_cell_text(&mut self, position: CellPosition, text: Arc<str>) {
        self.pending_cell_values.insert(position, text);
    }

    pub fn remove_pending_cell_text(&mut self, position: CellPosition) {
        self.pending_cell_values.remove(&position);
    }

    pub fn clear_pending_cell_texts(&mut self) {
        self.pending_cell_values.clear();
    }
}

#[derive(Debug)]
pub struct WorkspaceState {
    /// Physical tab order of the whole workspace; each pane shows its own
    /// tabs as the subsequence of this list. Invariant (restored by
    /// `normalize`): pinned tabs first, then unpinned tabs with equal group
    /// keys contiguous (groups of one connection together, connections in
    /// order of first appearance, ungrouped tabs last) when grouping is on, so
    /// it holds inside every pane too.
    tabs: Vec<Tab>,
    /// Which pane holds each tab, the active tab per pane, the focused pane
    /// (its active tab is the workspace's active tab), collapsed groups per
    /// pane and the split ratios. Its per-pane tab order mirrors `tabs`
    /// after every `normalize`.
    panes: PaneTree,
    next_tab_id: u64,
    runtimes: std::collections::HashMap<u64, TabRuntime>,
    /// Strip settings of panes with none of their own yet: new workspaces and
    /// sessions that predate per-pane settings (the saved preferences). Every
    /// pane has its own `group_by` / rows setting (see `panes::StripSettings`).
    strip_defaults: StripSettings,
    /// The tab each pane last showed for every group (`None` = the pinned /
    /// unbound "Other" section), so a chip click returns to it. Refreshed by
    /// `normalize`; entries of closed tabs are validated on use.
    group_memory: HashMap<(PaneId, Option<TabGroupKey>), u64>,
    /// The group each pane's second row shows: the group of the last active
    /// tab that is not global. Pinned / global tabs keep it as it was.
    shown_group: HashMap<PaneId, Option<TabGroupKey>>,
    /// Cached bar layout per pane (chips + visible tabs); rebuilt only when
    /// `dirty`.
    layouts: Vec<(PaneId, Vec<TabBarItem>)>,
    dirty: bool,
    /// Bumped by every tab-list or tab-field change; the session saver compares it.
    revision: u64,
    /// Rows per page of a new tab (the "default page size" setting).
    default_output_limit: i64,
    /// Page size of the connections that override the default, by connection
    /// id (`Settings::for_connection`); new tabs of those connections use it.
    connection_output_limits: HashMap<String, i64>,
    /// New data tabs open beside the active tab (a setting).
    open_next_to_active: bool,
}

/// One slot of the rendered tab bar, in visual order.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TabBarItem {
    Tab(u64),
    /// Group chip. `count` is the number of tabs in the group (visible or
    /// hidden); a collapsed group renders its chip only.
    Group {
        key: TabGroupKey,
        count: usize,
        collapsed: bool,
    },
}

/// One chip of the grouped-rows strip's top row.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RowChip<'a> {
    /// `None` is the "Other" chip: pinned and connection-less tabs.
    pub group: Option<&'a TabGroupKey>,
    pub count: usize,
}

/// Both rows of one pane's grouped-rows strip.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TabRows<'a> {
    pub chips: Vec<RowChip<'a>>,
    /// Index into `chips` of the group the tabs row shows.
    pub active: Option<usize>,
    /// The tabs row in bar order: pinned and global tabs, then the shown
    /// group's tabs.
    pub tabs: Vec<u64>,
    /// How many leading entries of `tabs` are pinned / global tabs.
    pub always: usize,
}

impl TabRows<'_> {
    /// The chip row only appears once there is something to choose between.
    pub fn chip_row(&self) -> bool {
        self.chips.len() >= 2
    }
}

/// Where a tab sits in the grouped-rows strip.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Slot<'a> {
    /// Pinned or global: always in the tabs row.
    Always,
    /// Under a group chip (`None` = "Other").
    Chip(Option<&'a TabGroupKey>),
}

/// Tabs that belong to no connection by nature (the Welcome page, the Driver
/// Manager) and pinned tabs are "global": the grouped-rows strip keeps them at
/// the start of the tabs row whatever group is shown. Connection-less query
/// tabs stay in the "Other" chip.
fn is_global_tab(tab: &Tab) -> bool {
    tab.is_pinned || matches!(tab.kind, TabKind::Welcome | TabKind::DriverManager | TabKind::Settings)
}

/// Where a dragged tab is being dropped.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TabDropTarget {
    Before(u64),
    After(u64),
    /// Onto a group chip: becomes the first tab of that group.
    GroupStart(PaneId, TabGroupKey),
    /// Empty bar space after the last tab of a pane.
    End(PaneId),
}

/// Outcome of asking whether a drop is possible.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TabMovePlan {
    /// Insert at this index of the list without the dragged tab.
    Move(usize),
    /// Legal, but the tab would not change position.
    NoChange,
    /// Pinned/unpinned boundary or group key mismatch.
    Refused,
}

/// Which tabs a bulk-close entry affects. Pinned tabs act on the pinned
/// region and regular tabs on the regular region, so regular-tab commands
/// never close pinned tabs.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TabCloseScope {
    Others,
    Left,
    Right,
    All,
    Group,
}

/// The persisted form of `tab`; `None` for tabs the session leaves out (nothing to restore).
fn session_tab(tab: &Tab) -> Option<crate::gpui_session::SessionTab> {
    use crate::gpui_session::SessionTab;
    let kind = match tab.kind {
        TabKind::Welcome => "welcome",
        TabKind::Query => "query",
        TabKind::Connection => "connection",
        TabKind::Diagram => "diagram",
        // A source tab without its object has nothing to show.
        TabKind::Source if tab.source.is_none() => return None,
        TabKind::Source => "source",
        // Singleton utility tab; reopened on demand.
        TabKind::DriverManager | TabKind::Settings => return None,
        // A wizard over a file and a load in flight: nothing to restore.
        TabKind::Import | TabKind::Transfer | TabKind::SqlFile | TabKind::Backup | TabKind::Restore | TabKind::Accounts | TabKind::ProcessList => return None,
    };
    Some(SessionTab {
        kind: kind.into(),
        title: tab.title.clone(),
        connection_id: tab.connection_id.clone(),
        database: tab.database.clone(),
        schema: tab.schema.clone(),
        collection: tab.collection.clone(),
        query_mode: tab.query_mode.clone(),
        is_pinned: tab.is_pinned,
        sql: tab.sql.clone(),
        output_limit: tab.output_limit,
        // The applied query is what persists; the draft only when
        // it differs so relaunch restores unsent typing too.
        table_where: tab.applied_where.clone(),
        table_order_by: tab.applied_order_by.clone(),
        table_where_draft: (tab.table_where != tab.applied_where)
            .then(|| tab.table_where.clone()),
        table_order_by_draft: (tab.table_order_by != tab.applied_order_by)
            .then(|| tab.table_order_by.clone()),
        table_filter_rules: tab.table_filter.rules.clone(),
        table_filter_where: tab.table_filter.generated_where.clone(),
        table_hidden_columns: tab.table_hidden_columns.clone(),
        pinned_columns: tab.pinned_columns.clone(),
        // Only the object: the text is fetched again when the tab opens.
        source: tab.source.clone(),
        // A path that is not UTF-8 cannot go into the JSON: that tab is saved unbound.
        file_path: tab
            .file
            .as_ref()
            .and_then(|file| file.path.to_str())
            .map(str::to_owned),
        // The text the file held, kept only while the tab differs from it.
        file_saved: tab
            .file
            .as_ref()
            .filter(|_| tab.has_unsaved_file())
            .map(|file| file.saved.to_string()),
        aggregate: tab.aggregate.as_ref().map(|draft| {
            crate::gpui_session::SessionAggregate {
                mode: draft.mode,
                preview: draft.preview,
                text: draft.text.clone(),
                options: draft.stored_options(),
                name: draft.name.clone(),
                saved_id: draft.saved_id.clone(),
            }
        }),
    })
}

/// The tab kind a saved tab restores as; `None` for one that cannot be restored.
fn tab_kind_of(saved: &crate::gpui_session::SessionTab) -> Option<TabKind> {
    match saved.kind.as_str() {
        "welcome" => Some(TabKind::Welcome),
        "connection" => Some(TabKind::Connection),
        "diagram" => Some(TabKind::Diagram),
        "source" if saved.source.is_some() => Some(TabKind::Source),
        "query" => Some(TabKind::Query),
        _ => None,
    }
}

/// The tab a saved tab restores as (`kind` from [`tab_kind_of`]).
fn tab_of(id: u64, kind: TabKind, saved: &crate::gpui_session::SessionTab) -> Tab {
    let mut tab = Tab::new(id, kind, saved.title.clone());
    tab.connection_id = saved.connection_id.clone();
    tab.database = saved.database.clone();
    tab.schema = saved.schema.clone();
    tab.collection = saved.collection.clone();
    tab.query_mode = saved.query_mode.clone();
    tab.is_pinned = saved.is_pinned;
    tab.sql = saved.sql.clone();
    tab.output_limit = saved.output_limit;
    tab.applied_where = saved.table_where.clone();
    tab.applied_order_by = saved.table_order_by.clone();
    tab.table_where = saved
        .table_where_draft
        .clone()
        .unwrap_or_else(|| saved.table_where.clone());
    tab.table_order_by = saved
        .table_order_by_draft
        .clone()
        .unwrap_or_else(|| saved.table_order_by.clone());
    tab.table_filter = crate::sql_filter::SqlFilterState::from_saved(
        saved.table_filter_rules.clone(),
        saved.table_filter_where.clone(),
    );
    tab.table_hidden_columns = saved.table_hidden_columns.clone();
    tab.pinned_columns = saved.pinned_columns.clone();
    tab.source = saved.source.clone();
    tab.file = saved.file_path.as_ref().map(|path| SqlFile {
        path: path.into(),
        saved: saved.file_saved.as_deref().unwrap_or(&saved.sql).into(),
    });
    tab.aggregate = saved.aggregate.as_ref().map(|aggregate| {
        crate::aggregate::AggregateDraft::restore(
            &aggregate.text,
            aggregate.options.clone(),
            aggregate.mode,
            aggregate.preview,
            aggregate.name.clone(),
            aggregate.saved_id.clone(),
        )
    });
    tab
}

impl Default for WorkspaceState {
    fn default() -> Self {
        Self::new()
    }
}

/// Tab title of an Aggregate tab: `orders · Untitled` / `orders · Top sellers`.
pub fn aggregate_tab_title(collection: &str, draft: &crate::aggregate::AggregateDraft) -> String {
    format!("{collection} · {}", draft.title())
}

impl WorkspaceState {
    pub fn new() -> Self {
        let mut state = Self {
            tabs: Vec::new(),
            panes: PaneTree::single(Vec::new(), None),
            next_tab_id: 1,
            runtimes: std::collections::HashMap::new(),
            strip_defaults: StripSettings::default(),
            group_memory: HashMap::new(),
            shown_group: HashMap::new(),
            layouts: Vec::new(),
            dirty: true,
            revision: 0,
            default_output_limit: DEFAULT_OUTPUT_LIMIT,
            connection_output_limits: HashMap::new(),
            open_next_to_active: false,
        };
        state.open_welcome_tab();
        state.normalize();
        state
    }

    fn touch(&mut self) {
        self.dirty = true;
        self.revision = self.revision.wrapping_add(1);
    }

    /// Changes whenever the tab list or any tab field may have changed.
    pub fn revision(&self) -> u64 {
        self.revision
    }

    /// Captures what must survive a relaunch.
    pub fn snapshot(&self) -> crate::gpui_session::GpuiSession {
        use crate::gpui_session::{GpuiSession, SessionGroup};
        let tabs = self
            .tabs
            .iter()
            .filter_map(session_tab)
            .collect::<Vec<_>>();
        let persisted: Vec<u64> = self
            .tabs
            .iter()
            .filter(|tab| !matches!(tab.kind, TabKind::DriverManager | TabKind::Settings))
            .map(|tab| tab.id)
            .collect();
        // Index within the persisted list (driver tabs are skipped).
        let index_of = |id: u64| persisted.iter().position(|persisted| *persisted == id);
        let active = self
            .panes
            .focused_pane()
            .and_then(|pane| pane.active())
            .and_then(index_of);
        // Legacy readers know one list: the union over the panes.
        let mut collapsed_groups: Vec<_> = self
            .panes
            .panes()
            .flat_map(|pane| pane.collapsed())
            .map(|key| SessionGroup {
                connection_id: key.connection_id.clone(),
                database: key.database.clone(),
            })
            .collect();
        collapsed_groups
            .sort_by(|a, b| (&a.connection_id, &a.database).cmp(&(&b.connection_id, &b.database)));
        collapsed_groups.dedup();
        GpuiSession {
            tabs,
            active,
            collapsed_groups,
            // One pane is the pre-panes format (no key): its strip settings
            // live in the preferences, which the pane's changes also update.
            panes: (self.panes.pane_count() > 1).then(|| self.panes.to_spec(&index_of)),
            ..Default::default()
        }
    }

    /// Replaces every tab with the saved ones (`keep` drops tabs that no
    /// longer apply, e.g. their connection was deleted). Fresh tab ids and
    /// runtimes are allocated; group-by must already be configured so the
    /// restored order and collapsed groups are interpreted correctly.
    pub fn restore_snapshot(
        &mut self,
        session: &crate::gpui_session::GpuiSession,
        keep: impl Fn(&crate::gpui_session::SessionTab) -> bool,
    ) {
        self.tabs.clear();
        self.runtimes.clear();
        // Id of each saved tab (`None` when it was dropped), by session index.
        let mut restored: Vec<Option<u64>> = Vec::with_capacity(session.tabs.len());
        for saved in &session.tabs {
            if !keep(saved) {
                restored.push(None);
                continue;
            }
            let Some(kind) = tab_kind_of(saved) else {
                restored.push(None);
                continue;
            };
            let id = self.next_tab_id;
            self.next_tab_id += 1;
            let tab = tab_of(id, kind, saved);
            self.tabs.push(tab);
            self.runtimes.insert(id, TabRuntime::new());
            restored.push(Some(id));
        }
        self.panes = PaneTree::restore_with(
            session.panes.as_ref(),
            &restored,
            session.active,
            self.strip_defaults,
        );
        if session.panes.is_none() {
            // Sessions that predate panes: the one list applies to the one pane.
            let pane = self.panes.focused();
            for group in &session.collapsed_groups {
                let key = TabGroupKey {
                    connection_id: group.connection_id.clone(),
                    database: group.database.clone(),
                };
                self.panes.set_group_collapsed(pane, &key, true);
            }
        }
        self.touch();
        self.normalize();
    }

    /// What "Reopen Closed Tab" needs to recreate `id`, in the session's form; `None` for tabs
    /// the session leaves out (see [`session_tab`]).
    pub fn closed_snapshot(&self, id: u64) -> Option<crate::gpui_session::SessionTab> {
        self.tab(id).and_then(session_tab)
    }

    /// The open tab `saved` would duplicate, if any: one with the same file (a file is in one tab, or
    /// saving either overwrites the other), the Welcome tab, or a tab of the same connection,
    /// database, schema and object (and query mode, for data tabs). Plain SQL tabs may repeat.
    pub fn open_twin(&self, saved: &crate::gpui_session::SessionTab) -> Option<u64> {
        let kind = tab_kind_of(saved)?;
        let data = saved.query_mode.is_table_data() || saved.query_mode.is_mongo_find() || saved.query_mode.is_redis_key();
        self.tabs
            .iter()
            .find(|tab| {
                if let Some(path) = &saved.file_path {
                    return tab.file.as_ref().is_some_and(|file| file.path == std::path::Path::new(path));
                }
                let same_place = tab.connection_id == saved.connection_id && tab.database == saved.database && tab.schema == saved.schema;
                tab.kind == kind
                    && match kind {
                        TabKind::Welcome => true,
                        TabKind::Connection => tab.connection_id == saved.connection_id,
                        TabKind::Diagram => same_place,
                        TabKind::Source => tab.connection_id == saved.connection_id && tab.source == saved.source,
                        TabKind::Query => data && same_place && tab.collection == saved.collection && tab.query_mode == saved.query_mode,
                        _ => false,
                    }
            })
            .map(|tab| tab.id)
    }

    /// Opens a saved tab in the focused pane and activates it (a fresh id and runtime, like
    /// [`Self::restore_snapshot`]). `None` when the saved tab cannot be restored.
    pub fn reopen_tab(&mut self, saved: &crate::gpui_session::SessionTab) -> Option<u64> {
        let kind = tab_kind_of(saved)?;
        let id = self.next_tab_id;
        self.next_tab_id += 1;
        let tab = tab_of(id, kind, saved);
        let mut runtime = TabRuntime::new();
        runtime.pagination = PaginationState::new(tab.output_limit);
        self.tabs.push(tab);
        self.runtimes.insert(id, runtime);
        let pane = self.panes.focused();
        self.panes.add_tab(pane, id, None);
        self.touch();
        self.normalize();
        Some(id)
    }

    pub fn tabs(&self) -> &[Tab] {
        &self.tabs
    }

    /// The tab the user works in: the focused pane's active tab.
    pub fn active_tab(&self) -> Option<&Tab> {
        self.active_tab_id()
            .and_then(|id| self.tabs.iter().find(|tab| tab.id == id))
    }

    fn active_tab_id(&self) -> Option<u64> {
        self.panes.focused_pane().and_then(|pane| pane.active())
    }

    /// Mutable access marks the bar layout stale: callers may change the
    /// connection, database or pin state, so the next `normalize` re-derives
    /// ordering and groups.
    pub fn active_tab_mut(&mut self) -> Option<&mut Tab> {
        let active = self.active_tab_id()?;
        self.touch();
        self.tabs.iter_mut().find(|tab| tab.id == active)
    }

    pub fn tab_mut(&mut self, tab_id: u64) -> Option<&mut Tab> {
        self.touch();
        self.tabs.iter_mut().find(|tab| tab.id == tab_id)
    }

    pub fn tab(&self, tab_id: u64) -> Option<&Tab> {
        self.tabs.iter().find(|tab| tab.id == tab_id)
    }

    pub fn runtime(&self, tab_id: u64) -> Option<&TabRuntime> {
        self.runtimes.get(&tab_id)
    }

    pub fn runtime_mut(&mut self, tab_id: u64) -> Option<&mut TabRuntime> {
        self.runtimes.get_mut(&tab_id)
    }

    /// See [`TabRuntime::has_staged_changes`]; `false` for unknown tabs.
    pub fn tab_has_staged_changes(&self, tab_id: u64) -> bool {
        self.runtime(tab_id)
            .is_some_and(|runtime| runtime.has_staged_changes())
    }

    pub fn auto_refresh_seconds(&self, tab_id: u64) -> Option<u64> {
        self.runtime(tab_id)
            .and_then(|runtime| runtime.auto_refresh_seconds)
    }

    pub fn set_auto_refresh_seconds(&mut self, tab_id: u64, seconds: Option<u64>) -> bool {
        let Some(runtime) = self.runtime_mut(tab_id) else {
            return false;
        };
        runtime.auto_refresh_seconds = seconds;
        true
    }

    #[cfg(test)]
    pub fn open_tab(&mut self, title: impl Into<String>) -> u64 {
        self.open_query_tab(title)
    }

    pub fn open_welcome_tab(&mut self) -> u64 {
        self.open_tab_with_kind(TabKind::Welcome, "Welcome")
    }

    pub fn open_driver_manager_tab(&mut self) -> u64 {
        if let Some(tab) = self
            .tabs
            .iter()
            .find(|tab| tab.kind == TabKind::DriverManager)
        {
            let id = tab.id;
            self.activate_tab(id);
            return id;
        }
        self.open_tab_with_kind(TabKind::DriverManager, "Driver Manager")
    }

    pub fn open_settings_tab(&mut self) -> u64 {
        if let Some(tab) = self.tabs.iter().find(|tab| tab.kind == TabKind::Settings) {
            let id = tab.id;
            self.activate_tab(id);
            return id;
        }
        self.open_tab_with_kind(TabKind::Settings, "Settings")
    }

    /// "Open data tabs next to the active tab": a new data tab sits right
    /// after `anchor` (the tab that was active before it opened) instead of at
    /// the end. `normalize` still applies pinning and grouping afterwards.
    fn tuck_after(&mut self, id: u64, anchor: Option<u64>) {
        if !self.open_next_to_active {
            return;
        }
        let Some(anchor) = anchor.filter(|anchor| *anchor != id) else {
            return;
        };
        let Some(from) = self.tabs.iter().position(|tab| tab.id == id) else {
            return;
        };
        let tab = self.tabs.remove(from);
        let at = self
            .tabs
            .iter()
            .position(|tab| tab.id == anchor)
            .map_or(self.tabs.len(), |at| at + 1);
        self.tabs.insert(at, tab);
        self.touch();
    }

    pub fn set_open_next_to_active(&mut self, enabled: bool) {
        self.open_next_to_active = enabled;
    }

    /// Page size of tabs opened from now on (the "default page size" setting).
    pub fn set_default_output_limit(&mut self, limit: i64) {
        self.default_output_limit = limit.max(1);
    }

    /// Per-connection page sizes of tabs opened from now on.
    pub fn set_connection_output_limits(&mut self, limits: HashMap<String, i64>) {
        self.connection_output_limits = limits;
    }

    /// A tab that was just bound to a connection starts with the page size of
    /// that connection.
    fn adopt_connection_output_limit(&mut self, id: u64) {
        let Some(limit) = self
            .tab(id)
            .and_then(|tab| self.connection_output_limits.get(&tab.connection_id))
            .copied()
        else {
            return;
        };
        if let Some(tab) = self.tab_mut(id) {
            tab.output_limit = limit;
        }
        if let Some(runtime) = self.runtimes.get_mut(&id) {
            runtime.pagination = PaginationState::new(limit);
        }
    }

    pub fn open_query_tab(&mut self, title: impl Into<String>) -> u64 {
        let id = self.open_tab_with_kind(TabKind::Query, title);
        self.normalize();
        id
    }

    /// Test helper: a query tab that has already run once, for tests that
    /// inject a result grid directly instead of executing a query.
    #[cfg(test)]
    pub fn open_ran_query_tab(&mut self, title: impl Into<String>) -> u64 {
        let id = self.open_query_tab(title);
        if let Some(runtime) = self.runtime_mut(id) {
            runtime.last_executed_query = Some(String::new());
        }
        id
    }

    pub fn open_query_tab_with_metadata(
        &mut self,
        title: impl Into<String>,
        metadata: TabMetadata,
    ) -> u64 {
        let anchor = self.active_tab().map(|tab| tab.id);
        let data_tab = metadata
            .query_mode
            .as_ref()
            .is_some_and(|mode| {
                mode.is_sql_table() || *mode == QueryMode::MongoFind || mode.is_redis_key()
            });
        let id = self.open_query_tab(title);
        if data_tab {
            self.tuck_after(id, anchor);
        }
        if let Some(tab) = self.active_tab_mut() {
            tab.source_id = metadata.source_id;
            tab.connection_id = metadata.connection_id;
            tab.database = metadata.database;
            tab.schema = metadata.schema;
            tab.collection = metadata.collection;
            tab.query_mode = metadata.query_mode;
            tab.is_pinned = metadata.is_pinned;
        }
        self.adopt_connection_output_limit(id);
        self.normalize();
        id
    }

    pub fn open_or_activate_connection_tab(
        &mut self,
        connection_id: impl Into<String>,
        title: impl Into<String>,
    ) -> u64 {
        let connection_id = connection_id.into();
        if let Some(tab) = self
            .tabs
            .iter()
            .find(|tab| tab.kind == TabKind::Connection && tab.connection_id == connection_id)
        {
            let id = tab.id;
            self.activate_tab(id);
            return id;
        }

        let id = self.open_tab_with_kind(TabKind::Connection, title);
        if let Some(tab) = self.active_tab_mut() {
            tab.connection_id = connection_id;
        }
        self.normalize();
        id
    }

    /// Opens (or activates) the whole-schema diagram tab of `database` /
    /// `schema` on a connection. Titled after the schema, else the database.
    pub fn open_or_activate_diagram_tab(
        &mut self,
        connection_id: &str,
        database: &str,
        schema: Option<&str>,
    ) -> u64 {
        let schema = schema.filter(|schema| !schema.is_empty());
        if let Some(tab) = self.tabs.iter().find(|tab| {
            tab.kind == TabKind::Diagram
                && tab.connection_id == connection_id
                && tab.database == database
                && tab.schema.as_deref() == schema
        }) {
            let id = tab.id;
            self.activate_tab(id);
            return id;
        }
        let title = format!("{} diagram", schema.unwrap_or(database));
        let id = self.open_tab_with_kind(TabKind::Diagram, title);
        if let Some(tab) = self.active_tab_mut() {
            tab.connection_id = connection_id.to_string();
            tab.database = database.to_string();
            tab.schema = schema.map(str::to_string);
        }
        self.normalize();
        id
    }

    /// Opens (or activates) the source tab of `source` on a connection.
    pub fn open_or_activate_source_tab(
        &mut self,
        connection_id: &str,
        source: SchemaObjectRef,
        title: &str,
    ) -> u64 {
        if let Some(tab) = self.tabs.iter().find(|tab| {
            tab.kind == TabKind::Source
                && tab.connection_id == connection_id
                && tab.source.as_ref() == Some(&source)
        }) {
            let id = tab.id;
            self.activate_tab(id);
            return id;
        }
        let id = self.open_tab_with_kind(TabKind::Source, title.to_string());
        if let Some(tab) = self.active_tab_mut() {
            tab.connection_id = connection_id.to_string();
            tab.database = source.database.clone().unwrap_or_default();
            tab.schema = source.schema.clone();
            tab.source = Some(source);
        }
        self.normalize();
        id
    }

    /// Opens a tab for importing data into a table of `schema` in `database` (a new one every time: two files
    /// can be loaded side by side).
    pub fn open_import_tab(&mut self, connection_id: &str, database: &str, schema: Option<&str>, title: &str) -> u64 {
        let id = self.open_tab_with_kind(TabKind::Import, title.to_string());
        if let Some(tab) = self.active_tab_mut() {
            tab.connection_id = connection_id.to_string();
            tab.database = database.to_string();
            tab.schema = schema.map(str::to_string);
        }
        self.normalize();
        id
    }

    /// Opens a tab for transferring data out of the namespace (`schema` in `database`) of a connection (a new one
    /// every time: two transfers can be set up side by side).
    pub fn open_transfer_tab(&mut self, connection_id: &str, database: &str, schema: Option<&str>, title: &str) -> u64 {
        let id = self.open_tab_with_kind(TabKind::Transfer, title.to_string());
        if let Some(tab) = self.active_tab_mut() {
            tab.connection_id = connection_id.to_string();
            tab.database = database.to_string();
            tab.schema = schema.map(str::to_string);
        }
        self.normalize();
        id
    }

    /// Opens a tab for running a SQL file on a namespace (`schema` in `database`) of a connection (a new one every
    /// time: two files can be set up side by side).
    pub fn open_file_run_tab(&mut self, connection_id: &str, database: &str, schema: Option<&str>, title: &str) -> u64 {
        let id = self.open_tab_with_kind(TabKind::SqlFile, title.to_string());
        if let Some(tab) = self.active_tab_mut() {
            tab.connection_id = connection_id.to_string();
            tab.database = database.to_string();
            tab.schema = schema.map(str::to_string);
        }
        self.normalize();
        id
    }

    /// Opens a tab for backing up (or, with `restore`, restoring into) the namespace (`schema` in `database`) of a connection (a new one
    /// every time: two can be set up side by side).
    pub fn open_backup_tab(&mut self, restore: bool, connection_id: &str, database: &str, schema: Option<&str>, title: &str) -> u64 {
        let kind = if restore { TabKind::Restore } else { TabKind::Backup };
        let id = self.open_tab_with_kind(kind, title.to_string());
        if let Some(tab) = self.active_tab_mut() {
            tab.connection_id = connection_id.to_string();
            tab.database = database.to_string();
            tab.schema = schema.map(str::to_string);
        }
        self.normalize();
        id
    }

    /// Opens the Users & Privileges tab of a connection.
    pub fn open_accounts_tab(&mut self, connection_id: &str, title: &str) -> u64 {
        let id = self.open_tab_with_kind(TabKind::Accounts, title.to_string());
        if let Some(tab) = self.active_tab_mut() {
            tab.connection_id = connection_id.to_string();
        }
        self.normalize();
        id
    }

    /// Opens the Process List tab of a connection.
    pub fn open_process_list_tab(&mut self, connection_id: &str, title: &str) -> u64 {
        let id = self.open_tab_with_kind(TabKind::ProcessList, title.to_string());
        if let Some(tab) = self.active_tab_mut() {
            tab.connection_id = connection_id.to_string();
        }
        self.normalize();
        id
    }

    pub fn open_or_activate_collection_tab(
        &mut self,
        connection_id: &str,
        database: &str,
        collection: &str,
    ) -> (u64, bool) {
        if let Some(tab) = self.tabs.iter().find(|tab| {
            tab.kind == TabKind::Query
                && tab.connection_id == connection_id
                && tab.database == database
                && tab.collection == collection
                && tab.query_mode.is_mongo_find()
        }) {
            let id = tab.id;
            self.activate_tab(id);
            return (id, false);
        }

        let anchor = self.active_tab().map(|tab| tab.id);
        let id = self.open_query_tab(collection);
        self.tuck_after(id, anchor);
        if let Some(tab) = self.active_tab_mut() {
            tab.connection_id = connection_id.to_string();
            tab.database = database.to_string();
            tab.collection = collection.to_string();
            tab.query_mode = Some(QueryMode::MongoFind);
            tab.sql = "{}".into();
        }
        self.adopt_connection_output_limit(id);
        self.normalize();
        (id, true)
    }

    /// Opens (or activates) the value tab of one Redis key. `key_raw` is the
    /// base64 key, `database` the db index. `true` when the tab is new.
    pub fn open_or_activate_redis_key_tab(
        &mut self,
        connection_id: &str,
        database: &str,
        key_raw: &str,
        title: &str,
    ) -> (u64, bool) {
        if let Some(tab) = self.tabs.iter().find(|tab| {
            tab.kind == TabKind::Query
                && tab.connection_id == connection_id
                && tab.database == database
                && tab.collection == key_raw
                && tab.query_mode.is_redis_key()
        }) {
            let id = tab.id;
            self.activate_tab(id);
            return (id, false);
        }
        let id = self.open_query_tab_with_metadata(
            title,
            TabMetadata {
                connection_id: connection_id.to_string(),
                database: database.to_string(),
                collection: key_raw.to_string(),
                query_mode: Some(QueryMode::RedisKey),
                ..Default::default()
            },
        );
        (id, true)
    }

    /// Opens a new Aggregate tab for a collection. Several may exist per
    /// collection (each is its own pipeline), so this never reuses a tab.
    pub fn open_aggregate_tab(
        &mut self,
        connection_id: &str,
        database: &str,
        collection: &str,
        draft: crate::aggregate::AggregateDraft,
    ) -> u64 {
        let id = self.open_query_tab(aggregate_tab_title(collection, &draft));
        if let Some(tab) = self.active_tab_mut() {
            tab.connection_id = connection_id.to_string();
            tab.database = database.to_string();
            tab.collection = collection.to_string();
            tab.query_mode = Some(QueryMode::MongoAggregate);
            tab.aggregate = Some(draft);
        }
        self.adopt_connection_output_limit(id);
        self.normalize();
        id
    }

    fn open_tab_with_kind(&mut self, kind: TabKind, title: impl Into<String>) -> u64 {
        let id = self.next_tab_id;
        self.next_tab_id += 1;
        let mut tab = Tab::new(id, kind, title.into());
        tab.output_limit = self.default_output_limit;
        self.tabs.push(tab);
        let mut runtime = TabRuntime::new();
        runtime.pagination = PaginationState::new(self.default_output_limit);
        self.runtimes.insert(id, runtime);
        // New tabs open in the focused pane.
        let pane = self.panes.focused();
        self.panes.add_tab(pane, id, None);
        self.touch();
        id
    }

    pub fn close_tab(&mut self, id: u64) -> bool {
        let Some(index) = self.tabs.iter().position(|tab| tab.id == id) else {
            return false;
        };
        let pane = self.panes.pane_of(id);
        let was_active = pane
            .and_then(|pane| self.panes.pane(pane))
            .is_some_and(|pane| pane.active() == Some(id));
        self.tabs.remove(index);
        self.runtimes.remove(&id);
        let removal = self.panes.remove_tab(id);
        self.touch();
        if let (true, Some(pane), Some(removal)) = (was_active, pane, removal) {
            if !removal.pane_removed {
                if let Some(next) = self.nearest_in_pane(pane, index) {
                    self.panes.set_active(pane, next);
                }
            }
        }
        self.normalize();
        true
    }

    /// The tab of `pane` to show after a tab left position `index` of the
    /// list: the nearest one that is currently drawn (so closing does not
    /// silently expand a collapsed group), else the nearest one at all.
    fn nearest_in_pane(&self, pane: PaneId, index: usize) -> Option<u64> {
        let index = index.min(self.tabs.len());
        let in_pane = |tab: &&Tab| self.panes.pane_of(tab.id) == Some(pane);
        let visible = |tab: &&Tab| in_pane(tab) && !self.is_hidden(tab);
        self.tabs[..index]
            .iter()
            .rev()
            .find(visible)
            .or_else(|| self.tabs[index..].iter().find(visible))
            .or_else(|| self.tabs[..index].iter().rev().find(in_pane))
            .or_else(|| self.tabs.iter().find(in_pane))
            .map(|tab| tab.id)
    }

    /// Activates a tab (and focuses its pane).
    pub fn activate_tab(&mut self, id: u64) -> bool {
        if self.panes.activate(id) {
            // Activating a hidden tab expands its group.
            self.touch();
            self.normalize();
            return true;
        }
        self.tabs.iter().any(|tab| tab.id == id)
    }

    // ---- tab bar: grouping, pinning, ordering --------------------------

    /// Strip settings applied to every pane now and to panes restored without
    /// settings of their own (startup, preference reload).
    pub fn set_strip_defaults(&mut self, defaults: StripSettings) {
        self.strip_defaults = defaults;
        self.panes.set_all_strips(defaults);
        self.touch();
        self.normalize();
    }

    pub fn strip_of(&self, pane: PaneId) -> StripSettings {
        self.panes.pane(pane).map(|pane| pane.strip()).unwrap_or(self.strip_defaults)
    }

    /// "Group Tabs By" of one pane.
    pub fn group_by_of(&self, pane: PaneId) -> TabGroupBy {
        self.strip_of(pane).group_by
    }

    /// The focused pane's grouping mode.
    pub fn group_by(&self) -> TabGroupBy {
        self.group_by_of(self.panes.focused())
    }

    /// Changes the focused pane's grouping mode.
    pub fn set_group_by(&mut self, group_by: TabGroupBy) {
        self.set_pane_group_by(self.panes.focused(), group_by);
    }

    /// Changes one pane's grouping mode (other panes keep theirs). Collapsed
    /// state is per mode, so the pane's resets.
    pub fn set_pane_group_by(&mut self, pane: PaneId, group_by: TabGroupBy) {
        let strip = self.strip_of(pane);
        if strip.group_by == group_by
            || !self.panes.set_strip(pane, StripSettings { group_by, ..strip })
        {
            return;
        }
        self.panes.clear_collapsed(pane);
        self.touch();
        self.normalize();
    }

    /// True when the pane draws two rows: the option is on and its tabs are
    /// grouped (without groups there is nothing to put in the top row).
    pub fn tab_rows_of(&self, pane: PaneId) -> bool {
        let strip = self.strip_of(pane);
        strip.rows && strip.group_by != TabGroupBy::None
    }

    #[cfg(test)]
    /// The focused pane's two-row setting (see `tab_rows_of`).
    pub fn tab_rows(&self) -> bool {
        self.tab_rows_of(self.panes.focused())
    }

    pub fn set_tab_rows(&mut self, rows: bool) {
        self.set_pane_tab_rows(self.panes.focused(), rows);
    }

    pub fn set_pane_tab_rows(&mut self, pane: PaneId, rows: bool) {
        let strip = self.strip_of(pane);
        if self.panes.set_strip(pane, StripSettings { rows, ..strip }) {
            self.touch();
            self.normalize();
        }
    }

    /// The grouping mode of the pane holding `tab` (groups are per pane).
    fn mode_of(&self, tab: &Tab) -> TabGroupBy {
        self.panes
            .pane_of(tab.id)
            .map_or(self.strip_defaults.group_by, |pane| self.group_by_of(pane))
    }

    fn key_parts_with(tab: &Tab, mode: TabGroupBy) -> Option<(&str, &str)> {
        if tab.is_pinned || tab.connection_id.is_empty() {
            return None;
        }
        match mode {
            TabGroupBy::None => None,
            TabGroupBy::Connection => Some((tab.connection_id.as_str(), "")),
            TabGroupBy::Database => Some((tab.connection_id.as_str(), tab.database.as_str())),
        }
    }

    fn key_parts<'a>(&self, tab: &'a Tab) -> Option<(&'a str, &'a str)> {
        Self::key_parts_with(tab, self.mode_of(tab))
    }

    /// The group an unpinned, connection-bound tab belongs to, if grouping is on.
    pub fn group_key(&self, tab: &Tab) -> Option<TabGroupKey> {
        self.key_parts(tab)
            .map(|(connection_id, database)| TabGroupKey {
                connection_id: connection_id.to_string(),
                database: database.to_string(),
            })
    }

    /// True when the tab belongs to a Chrome-style group (grouping on,
    /// unpinned, bound to a connection).
    pub fn is_grouped(&self, tab: &Tab) -> bool {
        self.key_parts(tab).is_some()
    }

    #[cfg(test)]
    pub fn is_group_collapsed(&self, key: &TabGroupKey) -> bool {
        self.panes
            .focused_pane()
            .is_some_and(|pane| pane.collapsed().contains(key))
    }

    /// True when the tab's group is collapsed in the pane that holds it.
    fn is_hidden(&self, tab: &Tab) -> bool {
        let Some(pane) = self.panes.pane_of(tab.id) else {
            return false;
        };
        let strip = self.strip_of(pane);
        let Some((connection_id, database)) = Self::key_parts_with(tab, strip.group_by)
            .filter(|_| !strip.rows)
        else {
            return false;
        };
        self.panes.pane(pane).is_some_and(|pane| {
            pane.collapsed()
                .iter()
                .any(|key| key.connection_id == connection_id && key.database == database)
        })
    }

    /// True when the tab is currently drawn in the bar (its group is expanded).
    #[cfg(test)]
    pub fn is_tab_visible(&self, id: u64) -> bool {
        self.tab(id).is_some_and(|tab| !self.is_hidden(tab))
    }

    /// Restores the ordering invariant and rebuilds the cached bar layout.
    /// O(n) and a no-op unless something changed since the last call.
    pub fn normalize(&mut self) {
        if !self.dirty {
            return;
        }
        self.dirty = false;
        // (region, connection rank, group rank): pinned first; groups of one
        // connection sit together (databases in order of first appearance),
        // connections in order of first appearance; ungrouped tabs last.
        let sort_keys: Vec<(u8, usize, usize)> = {
            let mut connections: HashMap<&str, usize> = HashMap::new();
            let mut groups: HashMap<(&str, &str), usize> = HashMap::new();
            self.tabs
                .iter()
                .map(|tab| {
                    let mode = self.mode_of(tab);
                    if tab.is_pinned {
                        (0, 0, 0)
                    } else if let Some(parts) = Self::key_parts_with(tab, mode) {
                        let next = connections.len();
                        let connection = *connections.entry(parts.0).or_insert(next);
                        let next = groups.len();
                        (1, connection, *groups.entry(parts).or_insert(next))
                    } else if mode != TabGroupBy::None {
                        (1, usize::MAX, 0)
                    } else {
                        (1, 0, 0)
                    }
                })
                .collect()
        };
        if !sort_keys.windows(2).all(|pair| pair[0] <= pair[1]) {
            let mut order: Vec<usize> = (0..self.tabs.len()).collect();
            order.sort_by_key(|&index| sort_keys[index]); // stable
            let mut old: Vec<Option<Tab>> = self.tabs.drain(..).map(Some).collect();
            self.tabs = order
                .into_iter()
                .filter_map(|index| old[index].take())
                .collect();
        }
        // Per pane: mirror the tab order, keep the active tab's group
        // expanded, drop collapsed groups that no longer exist and rebuild
        // the strip layout.
        let mut members: HashMap<PaneId, Vec<usize>> = HashMap::new();
        for pane in self.panes.panes() {
            members.insert(pane.id(), Vec::with_capacity(pane.tabs().len()));
        }
        {
            let owner: HashMap<u64, PaneId> = self
                .panes
                .panes()
                .flat_map(|pane| pane.tabs().iter().map(|tab| (*tab, pane.id())))
                .collect();
            for (index, tab) in self.tabs.iter().enumerate() {
                if let Some(list) = owner.get(&tab.id).and_then(|pane| members.get_mut(pane)) {
                    list.push(index);
                }
            }
        }
        let pane_ids: Vec<PaneId> = self.panes.panes().map(|pane| pane.id()).collect();
        let mut layouts = Vec::with_capacity(pane_ids.len());
        for pane in pane_ids {
            let rows = self.strip_of(pane).rows;
            let indices = members.remove(&pane).unwrap_or_default();
            let order: Vec<u64> = indices.iter().map(|index| self.tabs[*index].id).collect();
            self.panes.set_order(pane, order);
            if let Some(key) = self
                .panes
                .pane(pane)
                .and_then(|pane| pane.active())
                .and_then(|active| self.tab(active))
                .and_then(|tab| self.group_key(tab))
            {
                self.panes.set_group_collapsed(pane, &key, false);
            }
            let collapsed_keys: Vec<TabGroupKey> = self
                .panes
                .pane(pane)
                .map(|pane| pane.collapsed().to_vec())
                .unwrap_or_default();
            let mut layout = Vec::with_capacity(indices.len() + 4);
            let mut live_groups: HashSet<TabGroupKey> = HashSet::new();
            let mut at = 0;
            while at < indices.len() {
                let Some(parts) = self.key_parts(&self.tabs[indices[at]]) else {
                    layout.push(TabBarItem::Tab(self.tabs[indices[at]].id));
                    at += 1;
                    continue;
                };
                let mut end = at;
                while end < indices.len()
                    && self.key_parts(&self.tabs[indices[end]]) == Some(parts)
                {
                    end += 1;
                }
                let key = TabGroupKey {
                    connection_id: parts.0.to_string(),
                    database: parts.1.to_string(),
                };
                let collapsed = !rows && collapsed_keys.contains(&key);
                layout.push(TabBarItem::Group {
                    key: key.clone(),
                    count: end - at,
                    collapsed,
                });
                if !collapsed {
                    layout.extend(
                        indices[at..end]
                            .iter()
                            .map(|index| TabBarItem::Tab(self.tabs[*index].id)),
                    );
                }
                live_groups.insert(key);
                at = end;
            }
            self.panes
                .retain_collapsed(pane, |key| live_groups.contains(key));
            if let Some(active) = self.panes.pane(pane).and_then(|pane| pane.active()) {
                if let Some(tab) = self.tab(active).filter(|tab| !is_global_tab(tab)) {
                    let group = self.group_key(tab);
                    self.group_memory.insert((pane, group.clone()), active);
                    self.shown_group.insert(pane, group);
                }
            }
            layouts.push((pane, layout));
        }
        self.group_memory
            .retain(|(pane, _), _| layouts.iter().any(|(id, _)| id == pane));
        self.shown_group
            .retain(|pane, _| layouts.iter().any(|(id, _)| id == pane));
        self.layouts = layouts;
    }

    /// Each tab of a pane layout with the slot it sits in. Allocation free.
    fn layout_members<'a>(
        &'a self,
        layout: &'a [TabBarItem],
    ) -> impl Iterator<Item = (u64, Slot<'a>)> + 'a {
        let mut current: Option<&'a TabGroupKey> = None;
        layout.iter().filter_map(move |item| match item {
            TabBarItem::Group { key, .. } => {
                current = Some(key);
                None
            }
            TabBarItem::Tab(id) => {
                let tab = self.tab(*id)?;
                let slot = if self.is_grouped(tab) {
                    Slot::Chip(current)
                } else if is_global_tab(tab) {
                    Slot::Always
                } else {
                    Slot::Chip(None)
                };
                Some((*id, slot))
            }
        })
    }

    /// The two rows of a pane's grouped-rows strip. Chips: a chip per group
    /// present, plus an "Other" chip (last) for the remaining connection-less
    /// tabs. Tabs row: the pane's pinned and global tabs first (always
    /// visible, never in a chip), then the tabs of the shown group.
    /// Meaningful when `tab_rows()` is on; call `normalize` first.
    pub fn pane_rows(&self, pane: PaneId) -> TabRows<'_> {
        let layout = self.pane_layout(pane);
        let mut chips: Vec<RowChip> = layout
            .iter()
            .filter_map(|item| match item {
                TabBarItem::Group { key, count, .. } => Some(RowChip {
                    group: Some(key),
                    count: *count,
                }),
                TabBarItem::Tab(_) => None,
            })
            .collect();
        let other = self
            .layout_members(layout)
            .filter(|(_, slot)| *slot == Slot::Chip(None))
            .count();
        if other > 0 {
            chips.push(RowChip {
                group: None,
                count: other,
            });
        }
        let active = self
            .shown_group
            .get(&pane)
            .and_then(|shown| chips.iter().position(|chip| chip.group == shown.as_ref()))
            .or_else(|| (!chips.is_empty()).then_some(0));
        let shown = active.map(|index| chips[index].group);
        let always = self
            .layout_members(layout)
            .filter(|(_, slot)| *slot == Slot::Always)
            .count();
        let tabs = self
            .layout_members(layout)
            .filter(|(_, slot)| *slot == Slot::Always)
            .chain(
                self.layout_members(layout)
                    .filter(|(_, slot)| shown.is_some_and(|group| *slot == Slot::Chip(group))),
            )
            .map(|(id, _)| id)
            .collect();
        TabRows {
            chips,
            active,
            tabs,
            always,
        }
    }

    /// Whether the pane draws a chip row above its tabs row: it is in rows
    /// mode and either has two or more groups or sits beside such a pane.
    /// Alignment rule: among the panes directly side by side in one row-axis
    /// split, if any rows-mode pane shows a chip row, every rows-mode sibling
    /// shows one too (even with a single chip or none), so their tab rows and
    /// content start at the same y. Panes that are not in rows mode keep one
    /// row, column splits never align, and nested splits decide among
    /// themselves. Purely visual: no pane's settings change.
    pub fn pane_chip_row(&self, pane: PaneId) -> bool {
        if !self.tab_rows_of(pane) {
            return false;
        }
        self.pane_rows(pane).chip_row()
            || self.panes.row_siblings(pane).into_iter().any(|sibling| {
                self.tab_rows_of(sibling) && self.pane_rows(sibling).chip_row()
            })
    }

    /// Whether `tab` belongs to `group` of a pane's chips (`None` = "Other").
    fn in_chip(&self, tab: &Tab, group: Option<&TabGroupKey>) -> bool {
        match group {
            Some(key) => {
                self.key_parts(tab) == Some((key.connection_id.as_str(), key.database.as_str()))
            }
            None => !self.is_grouped(tab) && !is_global_tab(tab),
        }
    }

    /// The pane's tabs inside a chip's group (never pinned / global ones).
    pub fn group_tab_ids(&self, pane: PaneId, group: Option<&TabGroupKey>) -> Vec<u64> {
        let Some(pane) = self.panes.pane(pane) else {
            return Vec::new();
        };
        pane.tabs()
            .iter()
            .copied()
            .filter(|id| self.tab(*id).is_some_and(|tab| self.in_chip(tab, group)))
            .collect()
    }

    /// The tab a click on a group chip activates: the group's last active tab
    /// in the pane, else its first.
    pub fn group_target(&self, pane: PaneId, group: Option<&TabGroupKey>) -> Option<u64> {
        let pane_tabs = self.panes.pane(pane)?.tabs();
        self.group_memory
            .get(&(pane, group.cloned()))
            .copied()
            .filter(|id| {
                pane_tabs.contains(id) && self.tab(*id).is_some_and(|tab| self.in_chip(tab, group))
            })
            .or_else(|| {
                pane_tabs
                    .iter()
                    .copied()
                    .find(|id| self.tab(*id).is_some_and(|tab| self.in_chip(tab, group)))
            })
    }

    /// The tab of the next (`delta` 1) or previous (-1) group chip of a pane,
    /// wrapping around; `None` with fewer than two chips.
    pub fn adjacent_group_target(&self, pane: PaneId, delta: isize) -> Option<u64> {
        let rows = self.pane_rows(pane);
        let len = rows.chips.len();
        if len < 2 {
            return None;
        }
        let from = rows.active?;
        let next = (from as isize + delta).rem_euclid(len as isize) as usize;
        self.group_target(pane, rows.chips[next].group)
    }

    #[cfg(test)]
    /// The cached bar layout of the focused pane. Call `normalize` first if
    /// tabs were mutated through `tab_mut` / `active_tab_mut`.
    pub fn layout(&self) -> &[TabBarItem] {
        self.pane_layout(self.panes.focused())
    }

    /// The cached bar layout of one pane.
    pub fn pane_layout(&self, pane: PaneId) -> &[TabBarItem] {
        self.layouts
            .iter()
            .find(|(id, _)| *id == pane)
            .map_or(&[], |(_, layout)| layout.as_slice())
    }

    #[cfg(test)]
    /// Collapses or expands a group in the focused pane.
    pub fn toggle_group_collapsed(&mut self, key: &TabGroupKey) -> bool {
        self.toggle_group_collapsed_in(self.panes.focused(), key)
    }

    /// Collapses or expands a group of one pane's strip. Collapsing the
    /// group that holds the pane's active tab first activates the nearest
    /// visible tab of that pane outside it; when there is none the request
    /// is refused and `false` is returned.
    pub fn toggle_group_collapsed_in(&mut self, pane: PaneId, key: &TabGroupKey) -> bool {
        let collapsed = self
            .panes
            .pane(pane)
            .is_some_and(|pane| pane.collapsed().contains(key));
        if collapsed {
            self.panes.set_group_collapsed(pane, key, false);
            self.touch();
            self.normalize();
            return true;
        }
        if let Some(active) = self.panes.pane(pane).and_then(|pane| pane.active()) {
            let in_group = self
                .tab(active)
                .and_then(|tab| self.group_key(tab))
                .is_some_and(|active_key| active_key == *key);
            if in_group {
                let Some(replacement) = self.nearest_tab_outside_group(pane, active, key) else {
                    return false;
                };
                self.panes.set_active(pane, replacement);
            }
        }
        self.panes.set_group_collapsed(pane, key, true);
        self.touch();
        self.normalize();
        true
    }

    /// Nearest tab of `pane` outside `key`'s group, preferring one that is
    /// visible. When every other tab sits in a collapsed group, the nearest
    /// hidden one is used: activating it expands its group (the active tab's
    /// group is always expanded), so collapsing the last expanded group
    /// still takes effect.
    fn nearest_tab_outside_group(&self, pane: PaneId, from: u64, key: &TabGroupKey) -> Option<u64> {
        let index = self.tabs.iter().position(|tab| tab.id == from)?;
        let nearest = |accept: &dyn Fn(&Tab) -> bool| {
            let in_pane = |tab: &&Tab| {
                self.panes.pane_of(tab.id) == Some(pane) && accept(tab)
            };
            self.tabs[index..]
                .iter()
                .find(in_pane)
                .or_else(|| self.tabs[..index].iter().rev().find(in_pane))
                .map(|tab| tab.id)
        };
        let outside = |tab: &Tab| self.group_key(tab).as_ref() != Some(key);
        nearest(&|tab| outside(tab) && !self.is_hidden(tab)).or_else(|| nearest(&outside))
    }

    /// Pins or unpins a tab. Pinned tabs go to the end of the pinned block;
    /// unpinned ones return right after it (or to their group when grouping).
    pub fn set_pinned(&mut self, id: u64, pinned: bool) -> bool {
        let Some(index) = self.tabs.iter().position(|tab| tab.id == id) else {
            return false;
        };
        if self.tabs[index].is_pinned == pinned {
            return false;
        }
        let grouped_pane = self.mode_of(&self.tabs[index]) != TabGroupBy::None;
        let mut tab = self.tabs.remove(index);
        tab.is_pinned = pinned;
        let pinned_count = self.tabs.iter().filter(|other| other.is_pinned).count();
        if pinned || !grouped_pane {
            self.tabs.insert(pinned_count, tab);
        } else {
            // Grouping places the tab at the end of its group on normalize.
            self.tabs.push(tab);
        }
        self.touch();
        self.normalize();
        true
    }

    /// The pane a drop on `target` lands in.
    fn target_pane(&self, target: &TabDropTarget) -> Option<PaneId> {
        match target {
            TabDropTarget::Before(anchor) | TabDropTarget::After(anchor) => {
                self.panes.pane_of(*anchor)
            }
            TabDropTarget::GroupStart(pane, _) | TabDropTarget::End(pane) => Some(*pane),
        }
    }

    /// Decides what dropping `dragged` on `target` would do. A drop onto
    /// another pane's strip is judged by the same pinned / group rules as a
    /// reorder (the tab is placed next to the anchor), except for the empty
    /// tail, which accepts any tab: `normalize` files it under pinned or its
    /// group.
    pub fn plan_move(&self, dragged: u64, target: &TabDropTarget) -> TabMovePlan {
        let Some(from) = self.tabs.iter().position(|tab| tab.id == dragged) else {
            return TabMovePlan::Refused;
        };
        let Some(dest) = self.target_pane(target) else {
            return TabMovePlan::Refused;
        };
        let same_pane = self.panes.pane_of(dragged) == Some(dest);
        let source = &self.tabs[from];
        // Groups are per pane: the tab is judged by the destination's mode.
        let mode = self.group_by_of(dest);
        let without = |position: usize| {
            if position > from {
                position - 1
            } else {
                position
            }
        };
        let new_index = match target {
            TabDropTarget::Before(anchor) | TabDropTarget::After(anchor) => {
                if *anchor == dragged {
                    return TabMovePlan::NoChange;
                }
                let Some(at) = self.tabs.iter().position(|tab| tab.id == *anchor) else {
                    return TabMovePlan::Refused;
                };
                let other = &self.tabs[at];
                if source.is_pinned != other.is_pinned
                    || (!source.is_pinned && Self::key_parts_with(source, mode) != Self::key_parts_with(other, mode))
                {
                    return TabMovePlan::Refused;
                }
                without(at) + usize::from(matches!(target, TabDropTarget::After(_)))
            }
            TabDropTarget::GroupStart(_, key) => {
                let in_key = |tab: &Tab| {
                    Self::key_parts_with(tab, mode)
                        == Some((key.connection_id.as_str(), key.database.as_str()))
                };
                if source.is_pinned || !in_key(source) {
                    return TabMovePlan::Refused;
                }
                let first = self.tabs.iter().position(|tab| {
                    tab.id != dragged && self.panes.pane_of(tab.id) == Some(dest) && in_key(tab)
                });
                match first {
                    Some(at) => without(at),
                    None if same_pane => return TabMovePlan::NoChange,
                    None => self.tabs.len() - 1,
                }
            }
            TabDropTarget::End(_) => {
                if same_pane && (source.is_pinned || self.key_parts(source).is_some()) {
                    return TabMovePlan::Refused;
                }
                self.tabs.len() - 1
            }
        };
        if same_pane && new_index == from {
            TabMovePlan::NoChange
        } else {
            TabMovePlan::Move(new_index)
        }
    }

    /// Applies a legal drop on a tab strip (reorder, or move into another
    /// pane next to the anchor). Returns whether anything changed.
    pub fn move_tab(&mut self, dragged: u64, target: &TabDropTarget) -> bool {
        let TabMovePlan::Move(new_index) = self.plan_move(dragged, target) else {
            return false;
        };
        let (Some(from), Some(dest), Some(source)) = (
            self.tabs.iter().position(|tab| tab.id == dragged),
            self.target_pane(target),
            self.panes.pane_of(dragged),
        ) else {
            return false;
        };
        let source_active = self.panes.pane(source).and_then(|pane| pane.active());
        let tab = self.tabs.remove(from);
        self.tabs.insert(new_index.min(self.tabs.len()), tab);
        if dest != source {
            self.panes.move_tab(
                dragged,
                PaneDrop::Tabs {
                    pane: dest,
                    index: None,
                },
            );
            self.repair_active(source, source_active == Some(dragged), from);
        }
        self.touch();
        self.normalize();
        true
    }

    /// After a tab left `pane` from list position `index` while being its
    /// active tab, shows the nearest visible neighbour instead of whatever
    /// the tree picked by its own order.
    fn repair_active(&mut self, pane: PaneId, was_active: bool, index: usize) {
        if !was_active || self.panes.pane(pane).is_none() {
            return;
        }
        if let Some(next) = self.nearest_in_pane(pane, index) {
            self.panes.set_active(pane, next);
        }
    }

    /// Applies a drop on a pane's content area: into its tab list (end) or
    /// into a new pane beside it. Returns whether anything changed; dropping
    /// on the pane the tab is already in is a no-op, so is splitting a pane
    /// off its only tab.
    pub fn drop_tab(&mut self, dragged: u64, drop: PaneDrop) -> bool {
        let (Some(source), Some(from)) = (
            self.panes.pane_of(dragged),
            self.tabs.iter().position(|tab| tab.id == dragged),
        ) else {
            return false;
        };
        if matches!(drop, PaneDrop::Tabs { pane, .. } if pane == source) {
            return false;
        }
        let source_active = self.panes.pane(source).and_then(|pane| pane.active());
        if !self.panes.move_tab(dragged, drop) {
            return false;
        }
        if matches!(drop, PaneDrop::Tabs { .. }) {
            // Appended: last in the list, so last in the pane (`normalize`
            // still files pinned tabs and group members where they belong).
            let tab = self.tabs.remove(from);
            self.tabs.push(tab);
        }
        self.repair_active(source, source_active == Some(dragged), from);
        self.touch();
        self.normalize();
        true
    }

    /// Whether dropping `dragged` on a pane's content would change anything.
    pub fn can_drop_tab(&self, dragged: u64, drop: PaneDrop) -> bool {
        let Some(source) = self.panes.pane_of(dragged) else {
            return false;
        };
        match drop {
            PaneDrop::Tabs { pane, .. } => pane != source && self.panes.pane(pane).is_some(),
            PaneDrop::Split { pane, .. } => {
                pane != source
                    || self
                        .panes
                        .pane(pane)
                        .is_some_and(|pane| pane.tabs().len() > 1)
            }
        }
    }

    /// Context menu "Split Right / Down / Left / Up": the tab moves into a
    /// new pane beside its own.
    pub fn split_tab(&mut self, tab: u64, edge: Edge) -> bool {
        let Some(pane) = self.panes.pane_of(tab) else {
            return false;
        };
        self.drop_tab(tab, PaneDrop::Split { pane, edge })
    }

    /// Context menu "Move to Other Pane": appends the tab to the next pane.
    pub fn move_tab_to_other_pane(&mut self, tab: u64) -> bool {
        let Some(other) = self
            .panes
            .pane_of(tab)
            .and_then(|pane| self.panes.other_pane(pane))
        else {
            return false;
        };
        self.drop_tab(
            tab,
            PaneDrop::Tabs {
                pane: other,
                index: None,
            },
        )
    }

    // ---- panes -------------------------------------------------------------

    pub fn panes(&self) -> &PaneTree {
        &self.panes
    }

    pub fn focused_pane(&self) -> PaneId {
        self.panes.focused()
    }

    pub fn pane_of(&self, tab: u64) -> Option<PaneId> {
        self.panes.pane_of(tab)
    }

    /// The pane's active tab.
    pub fn pane_active_tab(&self, pane: PaneId) -> Option<&Tab> {
        self.panes
            .pane(pane)
            .and_then(|pane| pane.active())
            .and_then(|id| self.tab(id))
    }

    /// Focuses a pane (its active tab becomes the workspace's active tab).
    pub fn focus_pane(&mut self, pane: PaneId) -> bool {
        let changed = self.panes.focus(pane);
        if changed {
            self.revision = self.revision.wrapping_add(1);
        }
        changed
    }

    /// Moves the focus `step` panes through visual order, wrapping.
    pub fn focus_next_pane(&mut self, step: isize) -> bool {
        let changed = self.panes.focus_next(step);
        if changed {
            self.revision = self.revision.wrapping_add(1);
        }
        changed
    }

    /// "Close Pane": its tabs join the neighbouring pane; nothing is closed.
    pub fn close_pane(&mut self, pane: PaneId) -> bool {
        if self.panes.close_pane(pane).is_none() {
            return false;
        }
        self.touch();
        self.normalize();
        true
    }

    /// Maximizes the pane, or restores the layout when it already is.
    pub fn toggle_maximize_pane(&mut self, pane: PaneId) -> bool {
        let changed = self.panes.toggle_maximize(pane);
        if changed {
            self.revision = self.revision.wrapping_add(1);
        }
        changed
    }

    /// Stores the shares a splitter drag produced (persisted with the session).
    pub fn set_split_ratios(&mut self, split: SplitId, ratios: &[f32]) -> bool {
        let changed = self.panes.set_ratios(split, ratios);
        if changed {
            self.revision = self.revision.wrapping_add(1);
        }
        changed
    }

    /// Ids a bulk-close entry would close for the tab `anchor` (only tabs of
    /// the anchor's pane).
    pub fn close_targets(&self, anchor: u64, scope: TabCloseScope) -> Vec<u64> {
        let Some(index) = self.tabs.iter().position(|tab| tab.id == anchor) else {
            return Vec::new();
        };
        let pinned = self.tabs[index].is_pinned;
        let pane = self.panes.pane_of(anchor);
        let in_region =
            |tab: &&Tab| tab.is_pinned == pinned && self.panes.pane_of(tab.id) == pane;
        let key = self.group_key(&self.tabs[index]);
        self.tabs
            .iter()
            .enumerate()
            .filter(|(position, tab)| {
                in_region(tab)
                    && match scope {
                        TabCloseScope::Others => tab.id != anchor,
                        TabCloseScope::Left => *position < index,
                        TabCloseScope::Right => *position > index,
                        TabCloseScope::All => true,
                        TabCloseScope::Group => key.is_some() && self.group_key(tab) == key,
                    }
            })
            .map(|(_, tab)| tab.id)
            .collect()
    }

    /// Opens a copy of a query tab right after it (same connection, target
    /// and text; fresh runtime, never pinned).
    pub fn duplicate_tab(&mut self, id: u64) -> Option<u64> {
        let index = self.tabs.iter().position(|tab| tab.id == id)?;
        if self.tabs[index].kind != TabKind::Query {
            return None;
        }
        let new_id = self.next_tab_id;
        self.next_tab_id += 1;
        let mut copy = self.tabs[index].clone();
        copy.id = new_id;
        copy.source_id = None;
        copy.is_pinned = false;
        self.tabs.insert(index + 1, copy);
        self.runtimes.insert(new_id, TabRuntime::new());
        if let Some(pane) = self.panes.pane_of(id) {
            self.panes.add_tab(pane, new_id, None);
        }
        self.touch();
        self.normalize();
        Some(new_id)
    }

    /// Renames a tab; blank titles are rejected.
    pub fn rename_tab(&mut self, id: u64, title: &str) -> bool {
        let title = title.trim();
        if title.is_empty() {
            return false;
        }
        let Some(tab) = self.tab_mut(id) else {
            return false;
        };
        tab.title = title.chars().take(120).collect();
        true
    }
}

#[cfg(test)]
mod tests {
    use super::{
        Edge, PaneDrop, PaneId, RowChip, TabBarItem, TabCloseScope, TabDropTarget, TabGroupBy, TabGroupKey, TabKind, TabMetadata,
        StripSettings, TabMovePlan, WorkspaceState,
    };
    use crate::query_mode::QueryMode;
    use crate::grid::GridState;
    use crate::grid_arrow::ArrowGridSource;
    use crate::query::QueryStatus;
    use arrow_array::{ArrayRef, Int64Array, RecordBatch};
    use arrow_schema::{DataType, Field, Schema};
    use std::sync::Arc;

    fn batch_with_rows(rows: &[i64]) -> Arc<RecordBatch> {
        let schema = Arc::new(Schema::new(vec![Field::new("id", DataType::Int64, false)]));
        let values: ArrayRef = Arc::new(Int64Array::from_iter_values(rows.iter().copied()));
        Arc::new(RecordBatch::try_new(schema, vec![values]).expect("valid batch"))
    }

    #[test]
    fn opening_and_closing_tabs_preserves_a_valid_active_tab() {
        let mut workspace = WorkspaceState::default();
        let query_id = workspace.open_tab("Query 1");
        let welcome_id = workspace.tabs()[0].id;

        assert_eq!(workspace.active_tab().map(|tab| tab.id), Some(query_id));
        assert!(workspace.close_tab(query_id));
        assert_eq!(workspace.active_tab().map(|tab| tab.id), Some(welcome_id));
        assert!(!workspace.close_tab(query_id));
        assert!(workspace.runtime(query_id).is_none(), "runtime dropped");
    }

    #[test]
    fn connection_tabs_are_deduplicated_by_saved_connection_id() {
        let mut workspace = WorkspaceState::new();
        let first = workspace.open_or_activate_connection_tab("conn-1", "Postgres");
        let second = workspace.open_or_activate_connection_tab("conn-1", "Renamed");

        assert_eq!(first, second);
        assert_eq!(workspace.tabs().len(), 2, "welcome plus one connection tab");
        assert_eq!(workspace.active_tab().map(|tab| tab.id), Some(first));
        assert_eq!(
            workspace.active_tab().map(|tab| tab.kind),
            Some(TabKind::Connection)
        );
    }

    #[test]
    fn schema_diagram_tabs_are_deduplicated_titled_and_survive_a_session_round_trip() {
        let mut workspace = WorkspaceState::new();
        let schema = workspace.open_or_activate_diagram_tab("conn-1", "warehouse", Some("public"));
        let again = workspace.open_or_activate_diagram_tab("conn-1", "warehouse", Some("public"));
        let database = workspace.open_or_activate_diagram_tab("conn-1", "main", None);
        let other_schema = workspace.open_or_activate_diagram_tab("conn-1", "warehouse", Some("sales"));
        assert_eq!(schema, again, "same target reuses the tab");
        assert_eq!(
            [schema, database, other_schema].iter().collect::<std::collections::HashSet<_>>().len(),
            3
        );
        let title = |id| workspace.tab(id).unwrap().title.clone();
        assert_eq!(title(schema), "public diagram");
        assert_eq!(title(database), "main diagram", "no schema: titled after the database");

        let session = workspace.snapshot();
        let mut restored = WorkspaceState::new();
        restored.restore_snapshot(&session, |_| true);
        let diagrams: Vec<_> = restored
            .tabs()
            .iter()
            .filter(|tab| tab.kind == TabKind::Diagram)
            .map(|tab| (tab.title.clone(), tab.connection_id.clone(), tab.database.clone(), tab.schema.clone()))
            .collect();
        assert_eq!(
            diagrams,
            [
                ("public diagram".to_string(), "conn-1".to_string(), "warehouse".to_string(), Some("public".to_string())),
                ("main diagram".to_string(), "conn-1".to_string(), "main".to_string(), None),
                ("sales diagram".to_string(), "conn-1".to_string(), "warehouse".to_string(), Some("sales".to_string())),
            ]
        );
    }

    #[test]
    fn driver_manager_tab_is_singleton() {
        let mut workspace = WorkspaceState::new();
        let first = workspace.open_driver_manager_tab();
        let second = workspace.open_driver_manager_tab();

        assert_eq!(first, second);
        assert_eq!(workspace.tabs().len(), 2);
        assert_eq!(
            workspace.active_tab().map(|tab| tab.kind),
            Some(TabKind::DriverManager)
        );
    }

    #[test]
    fn settings_tab_is_a_singleton_that_is_not_persisted() {
        let mut workspace = WorkspaceState::new();
        let first = workspace.open_settings_tab();
        let second = workspace.open_settings_tab();
        assert_eq!(first, second);
        assert_eq!(workspace.active_tab().map(|tab| tab.kind), Some(TabKind::Settings));
        let session = workspace.snapshot();
        assert!(session.tabs.iter().all(|tab| tab.kind != "settings"));
        assert_eq!(session.tabs.len(), workspace.tabs().len() - 1, "only the Welcome tab persists");
    }

    #[test]
    fn data_tabs_open_next_to_the_active_tab_only_when_asked() {
        let table = |workspace: &mut WorkspaceState, name: &str| {
            workspace.open_query_tab_with_metadata(
                name,
                TabMetadata {
                    connection_id: "c".into(),
                    collection: name.into(),
                    query_mode: Some(QueryMode::SqlTable),
                    ..Default::default()
                },
            )
        };
        let order = |workspace: &WorkspaceState| {
            workspace.tabs().iter().map(|tab| tab.title.clone()).collect::<Vec<_>>()
        };
        let mut workspace = WorkspaceState::new();
        let a = table(&mut workspace, "a");
        table(&mut workspace, "b");
        assert_eq!(order(&workspace), ["Welcome", "a", "b"], "the end by default");

        workspace.set_open_next_to_active(true);
        workspace.activate_tab(a);
        table(&mut workspace, "c");
        assert_eq!(order(&workspace), ["Welcome", "a", "c", "b"], "right after the active tab");
        // A plain query tab is not a data tab.
        workspace.activate_tab(a);
        workspace.open_query_tab("scratch");
        assert_eq!(order(&workspace), ["Welcome", "a", "c", "b", "scratch"]);
    }

    #[test]
    fn collection_tabs_are_deduplicated_by_connection_database_and_collection() {
        let mut workspace = WorkspaceState::new();
        let (first, created) =
            workspace.open_or_activate_collection_tab("conn-1", "payments", "invoices");
        let (second, reused) =
            workspace.open_or_activate_collection_tab("conn-1", "payments", "invoices");

        assert!(created);
        assert!(!reused);
        assert_eq!(first, second);
        let tab = workspace.active_tab().expect("collection tab");
        assert_eq!(tab.kind, TabKind::Query);
        assert_eq!(tab.query_mode, Some(QueryMode::MongoFind));
        let runtime = workspace.runtime(first).expect("collection runtime");
        assert_eq!(runtime.mongo.filter, "{}");
        assert_eq!(runtime.mongo.sort, r#"{"_id":1}"#);
        assert_eq!(tab.sql, "{}");
        assert_eq!(tab.database, "payments");
        assert_eq!(tab.collection, "invoices");
    }

    #[test]
    fn different_connection_ids_get_separate_connection_tabs() {
        let mut workspace = WorkspaceState::new();
        let first = workspace.open_or_activate_connection_tab("conn-1", "Postgres");
        let second = workspace.open_or_activate_connection_tab("conn-2", "SQLite");

        assert_ne!(first, second);
        assert_eq!(workspace.tabs().len(), 3);
    }

    #[test]
    fn activating_unknown_tab_does_not_change_selection() {
        let mut workspace = WorkspaceState::default();
        let current_id = workspace.active_tab().expect("welcome tab").id;

        assert!(!workspace.activate_tab(999));
        assert_eq!(workspace.active_tab().map(|tab| tab.id), Some(current_id));
    }

    #[test]
    fn tabs_carry_independent_sql_and_limits() {
        let mut workspace = WorkspaceState::default();
        let first = workspace.open_tab("One");
        let second = workspace.open_tab("Two");

        workspace.active_tab_mut().unwrap().sql = "SELECT 1".into();
        workspace.active_tab_mut().unwrap().output_limit = 50;
        workspace.activate_tab(first);
        workspace.active_tab_mut().unwrap().sql = "SELECT 2".into();

        let first_tab = workspace.tabs().iter().find(|t| t.id == first).unwrap();
        let second_tab = workspace.tabs().iter().find(|t| t.id == second).unwrap();
        assert_eq!(first_tab.sql, "SELECT 2");
        assert_eq!(first_tab.output_limit, 200);
        assert_eq!(second_tab.sql, "SELECT 1");
        assert_eq!(second_tab.output_limit, 50);
    }

    #[test]
    fn restored_query_tabs_keep_legacy_metadata() {
        let mut workspace = WorkspaceState::new();
        let id = workspace.open_query_tab_with_metadata(
            "Orders",
            TabMetadata {
                source_id: Some("legacy-tab".into()),
                connection_id: "conn-1".into(),
                database: "app".into(),
                schema: Some("public".into()),
                collection: "orders".into(),
                query_mode: Some(QueryMode::PostgresTable),
                is_pinned: true,
            },
        );

        let tab = workspace.tabs().iter().find(|tab| tab.id == id).unwrap();
        assert_eq!(tab.source_id.as_deref(), Some("legacy-tab"));
        assert_eq!(tab.schema.as_deref(), Some("public"));
        assert_eq!(tab.collection, "orders");
        assert_eq!(tab.query_mode, Some(QueryMode::PostgresTable));
        assert!(tab.is_pinned);
    }

    #[test]
    fn tab_runtimes_keep_queries_and_grids_isolated() {
        let mut workspace = WorkspaceState::default();
        let first = workspace.open_tab("One");
        let second = workspace.open_tab("Two");

        let run_one = workspace.runtime_mut(first).unwrap().query.begin().unwrap();
        workspace
            .runtime_mut(first)
            .unwrap()
            .query
            .complete(run_one, 7);
        workspace.runtime_mut(first).unwrap().grid =
            GridState::new(crate::grid_source::GridContentSource::Arrow(
                ArrowGridSource::new(vec![batch_with_rows(&[1, 2])]),
            ));
        let run_two = workspace
            .runtime_mut(second)
            .unwrap()
            .query
            .begin()
            .unwrap();
        workspace
            .runtime_mut(second)
            .unwrap()
            .query
            .fail(run_two, "boom");

        assert_eq!(
            workspace.runtime(first).unwrap().query.status(),
            &QueryStatus::Ready { rows: 7 }
        );
        assert_eq!(workspace.runtime(first).unwrap().grid.row_count(), 2);
        assert_eq!(
            workspace.runtime(second).unwrap().query.status(),
            &QueryStatus::Failed("boom".into())
        );
        assert_eq!(workspace.runtime(second).unwrap().grid.row_count(), 0);
    }

    #[test]
    fn stale_result_cannot_overwrite_the_live_tabs_state() {
        let mut workspace = WorkspaceState::default();
        let tab = workspace.open_tab("Q");

        // Run starts, is cancelled, and a new run goes live.
        let stale = workspace.runtime_mut(tab).unwrap().query.begin().unwrap();
        workspace.runtime_mut(tab).unwrap().query.cancel().unwrap();
        let live = workspace.runtime_mut(tab).unwrap().query.begin().unwrap();

        let stale_applied = workspace
            .runtime_mut(tab)
            .unwrap()
            .query
            .complete(stale, 999);
        assert!(!stale_applied, "stale completion must be rejected");
        assert_eq!(
            workspace.runtime(tab).unwrap().query.status(),
            &QueryStatus::Running { run: live }
        );
    }

    #[test]
    fn auto_refresh_configuration_is_isolated_per_tab_and_removed_on_close() {
        let mut workspace = WorkspaceState::default();
        let first = workspace.open_tab("One");
        let second = workspace.open_tab("Two");

        assert!(workspace.set_auto_refresh_seconds(first, Some(5)));
        assert_eq!(workspace.auto_refresh_seconds(first), Some(5));
        assert_eq!(workspace.auto_refresh_seconds(second), None);

        assert!(workspace.close_tab(first));
        assert_eq!(workspace.auto_refresh_seconds(first), None);
    }

    fn bound(workspace: &mut WorkspaceState, title: &str, conn: &str, db: &str) -> u64 {
        workspace.open_query_tab_with_metadata(
            title,
            TabMetadata {
                connection_id: conn.into(),
                database: db.into(),
                ..Default::default()
            },
        )
    }

    fn ids(workspace: &WorkspaceState) -> Vec<u64> {
        workspace.tabs().iter().map(|tab| tab.id).collect()
    }

    fn key(conn: &str, db: &str) -> TabGroupKey {
        TabGroupKey {
            connection_id: conn.into(),
            database: db.into(),
        }
    }

    #[test]
    fn pinned_tabs_sit_at_the_start_and_unpin_returns_after_them() {
        let mut workspace = WorkspaceState::new();
        let welcome = workspace.tabs()[0].id;
        let a = workspace.open_tab("a");
        let b = workspace.open_tab("b");
        let c = workspace.open_tab("c");

        assert!(workspace.set_pinned(c, true));
        assert_eq!(ids(&workspace), vec![c, welcome, a, b], "pinned goes first");
        assert!(workspace.set_pinned(a, true));
        assert_eq!(
            ids(&workspace),
            vec![c, a, welcome, b],
            "after existing pins"
        );
        assert!(workspace.set_pinned(c, false));
        assert_eq!(
            ids(&workspace),
            vec![a, c, welcome, b],
            "unpin lands after the pins"
        );
        assert!(!workspace.set_pinned(c, false), "already unpinned");
    }

    #[test]
    fn restored_pinned_tabs_are_ordered_first() {
        let mut workspace = WorkspaceState::new();
        let welcome = workspace.tabs()[0].id;
        let plain = workspace.open_tab("plain");
        let pinned = workspace.open_query_tab_with_metadata(
            "pinned",
            TabMetadata {
                is_pinned: true,
                ..Default::default()
            },
        );
        assert_eq!(ids(&workspace), vec![pinned, welcome, plain]);
    }

    #[test]
    fn move_plan_reorders_before_and_after_and_reports_noop() {
        let mut workspace = WorkspaceState::new();
        let w = workspace.tabs()[0].id;
        let a = workspace.open_tab("a");
        let b = workspace.open_tab("b");
        let c = workspace.open_tab("c");

        assert_eq!(
            workspace.plan_move(c, &TabDropTarget::Before(w)),
            TabMovePlan::Move(0)
        );
        assert!(workspace.move_tab(c, &TabDropTarget::Before(w)));
        assert_eq!(ids(&workspace), vec![c, w, a, b]);
        assert!(workspace.move_tab(c, &TabDropTarget::After(b)));
        assert_eq!(ids(&workspace), vec![w, a, b, c]);
        assert_eq!(
            workspace.plan_move(a, &TabDropTarget::Before(b)),
            TabMovePlan::NoChange,
            "dropping into the current slot changes nothing"
        );
        assert_eq!(
            workspace.plan_move(a, &TabDropTarget::After(a)),
            TabMovePlan::NoChange
        );
        assert!(!workspace.move_tab(a, &TabDropTarget::Before(b)));
        assert!(workspace.move_tab(a, &TabDropTarget::End(workspace.focused_pane())));
        assert_eq!(ids(&workspace), vec![w, b, c, a]);
    }

    #[test]
    fn pinned_and_unpinned_tabs_only_reorder_within_their_region() {
        let mut workspace = WorkspaceState::new();
        let w = workspace.tabs()[0].id;
        let a = workspace.open_tab("a");
        let b = workspace.open_tab("b");
        workspace.set_pinned(a, true);
        workspace.set_pinned(b, true);
        assert_eq!(ids(&workspace), vec![a, b, w]);

        assert_eq!(
            workspace.plan_move(w, &TabDropTarget::Before(a)),
            TabMovePlan::Refused
        );
        assert_eq!(
            workspace.plan_move(a, &TabDropTarget::After(w)),
            TabMovePlan::Refused
        );
        assert_eq!(
            workspace.plan_move(a, &TabDropTarget::End(workspace.focused_pane())),
            TabMovePlan::Refused
        );
        assert_eq!(
            workspace.plan_move(a, &TabDropTarget::After(b)),
            TabMovePlan::Move(1)
        );
        assert!(workspace.move_tab(a, &TabDropTarget::After(b)));
        assert_eq!(ids(&workspace), vec![b, a, w]);
        assert!(!workspace.move_tab(w, &TabDropTarget::Before(b)));
        assert_eq!(
            ids(&workspace),
            vec![b, a, w],
            "refused drop leaves order intact"
        );
    }

    #[test]
    fn grouping_keeps_keys_contiguous_and_puts_unbound_tabs_last() {
        let mut workspace = WorkspaceState::new();
        let welcome = workspace.tabs()[0].id;
        let p1 = bound(&mut workspace, "p1", "pg", "app");
        let s1 = bound(&mut workspace, "s1", "sq", "main");
        let p2 = bound(&mut workspace, "p2", "pg", "other");
        let loose = workspace.open_tab("loose");
        assert_eq!(
            ids(&workspace),
            vec![welcome, p1, s1, p2, loose],
            "no grouping yet"
        );

        workspace.set_group_by(TabGroupBy::Connection);
        assert_eq!(ids(&workspace), vec![p1, p2, s1, welcome, loose]);
        assert_eq!(
            workspace.layout(),
            &[
                TabBarItem::Group {
                    key: key("pg", ""),
                    count: 2,
                    collapsed: false
                },
                TabBarItem::Tab(p1),
                TabBarItem::Tab(p2),
                TabBarItem::Group {
                    key: key("sq", ""),
                    count: 1,
                    collapsed: false
                },
                TabBarItem::Tab(s1),
                TabBarItem::Tab(welcome),
                TabBarItem::Tab(loose),
            ]
        );

        workspace.set_group_by(TabGroupBy::Database);
        assert_eq!(
            ids(&workspace),
            vec![p1, p2, s1, welcome, loose],
            "one group per database"
        );
        assert_eq!(
            workspace
                .layout()
                .iter()
                .filter(|item| matches!(item, TabBarItem::Group { .. }))
                .count(),
            3
        );

        // New tabs open at the end of their group.
        let p3 = bound(&mut workspace, "p3", "pg", "app");
        assert_eq!(ids(&workspace), vec![p1, p3, p2, s1, welcome, loose]);

        workspace.set_group_by(TabGroupBy::None);
        assert!(workspace
            .layout()
            .iter()
            .all(|item| matches!(item, TabBarItem::Tab(_))));
    }

    #[test]
    fn pinned_tabs_are_never_grouped() {
        let mut workspace = WorkspaceState::new();
        let p1 = bound(&mut workspace, "p1", "pg", "app");
        let p2 = bound(&mut workspace, "p2", "pg", "app");
        workspace.set_group_by(TabGroupBy::Connection);
        workspace.set_pinned(p2, true);
        assert_eq!(workspace.tabs()[0].id, p2);
        assert_eq!(
            workspace.layout()[0],
            TabBarItem::Tab(p2),
            "no chip before pinned tabs"
        );
        assert!(matches!(
            workspace.layout()[1],
            TabBarItem::Group { count: 1, .. }
        ));
        // Unpinning returns it to the end of its group.
        workspace.set_pinned(p2, false);
        assert_eq!(ids(&workspace)[..2], [p1, p2]);
    }

    #[test]
    fn group_drops_only_accept_matching_keys() {
        let mut workspace = WorkspaceState::new();
        let welcome = workspace.tabs()[0].id;
        let p1 = bound(&mut workspace, "p1", "pg", "app");
        let p2 = bound(&mut workspace, "p2", "pg", "app");
        let s1 = bound(&mut workspace, "s1", "sq", "main");
        workspace.set_group_by(TabGroupBy::Connection);
        assert_eq!(ids(&workspace), vec![p1, p2, s1, welcome]);

        assert_eq!(
            workspace.plan_move(s1, &TabDropTarget::Before(p1)),
            TabMovePlan::Refused,
            "sqlite tab cannot join the postgres group"
        );
        assert_eq!(
            workspace.plan_move(s1, &TabDropTarget::GroupStart(workspace.focused_pane(), key("pg", ""))),
            TabMovePlan::Refused
        );
        assert_eq!(
            workspace.plan_move(welcome, &TabDropTarget::After(p2)),
            TabMovePlan::Refused
        );
        assert_eq!(
            workspace.plan_move(p1, &TabDropTarget::After(s1)),
            TabMovePlan::Refused
        );
        assert_eq!(
            workspace.plan_move(p1, &TabDropTarget::End(workspace.focused_pane())),
            TabMovePlan::Refused
        );
        assert_eq!(
            workspace.plan_move(welcome, &TabDropTarget::End(workspace.focused_pane())),
            TabMovePlan::NoChange
        );

        assert_eq!(
            workspace.plan_move(p2, &TabDropTarget::GroupStart(workspace.focused_pane(), key("pg", ""))),
            TabMovePlan::Move(0)
        );
        assert!(workspace.move_tab(p2, &TabDropTarget::GroupStart(workspace.focused_pane(), key("pg", ""))));
        assert_eq!(ids(&workspace), vec![p2, p1, s1, welcome]);
    }

    #[test]
    fn collapsing_hides_tabs_and_activation_or_opening_reexpands() {
        let mut workspace = WorkspaceState::new();
        let welcome = workspace.tabs()[0].id;
        let p1 = bound(&mut workspace, "p1", "pg", "app");
        let p2 = bound(&mut workspace, "p2", "pg", "app");
        workspace.set_group_by(TabGroupBy::Connection);
        let pg = key("pg", "");

        // The group holding the active tab hands activation to a tab outside it.
        assert!(workspace.toggle_group_collapsed(&pg));
        assert_eq!(workspace.active_tab().map(|tab| tab.id), Some(welcome));
        assert_eq!(
            workspace.layout(),
            &[
                TabBarItem::Group {
                    key: pg.clone(),
                    count: 2,
                    collapsed: true
                },
                TabBarItem::Tab(welcome),
            ]
        );
        assert!(!workspace.is_tab_visible(p1));

        assert!(workspace.activate_tab(p1), "activating a hidden tab");
        assert!(!workspace.is_group_collapsed(&pg), "expands its group");
        assert_eq!(workspace.layout().len(), 4);

        assert!(workspace.toggle_group_collapsed(&pg));
        let p3 = bound(&mut workspace, "p3", "pg", "app");
        assert!(
            !workspace.is_group_collapsed(&pg),
            "a new tab expands its group"
        );
        assert_eq!(ids(&workspace), vec![p1, p2, p3, welcome]);
    }

    #[test]
    fn collapsing_the_only_visible_group_is_refused() {
        let mut workspace = WorkspaceState::new();
        let welcome = workspace.tabs()[0].id;
        workspace.close_tab(welcome);
        let p1 = bound(&mut workspace, "p1", "pg", "app");
        workspace.set_group_by(TabGroupBy::Connection);
        assert!(
            !workspace.toggle_group_collapsed(&key("pg", "")),
            "nothing else to activate"
        );
        assert_eq!(workspace.active_tab().map(|tab| tab.id), Some(p1));
    }

    #[test]
    fn closing_the_active_tab_prefers_visible_neighbours() {
        let mut workspace = WorkspaceState::new();
        let welcome = workspace.tabs()[0].id;
        let p1 = bound(&mut workspace, "p1", "pg", "app");
        let s1 = bound(&mut workspace, "s1", "sq", "main");
        workspace.set_group_by(TabGroupBy::Connection);
        workspace.activate_tab(s1);
        workspace.toggle_group_collapsed(&key("pg", ""));
        assert!(!workspace.is_tab_visible(p1));
        workspace.close_tab(s1);
        assert_eq!(
            workspace.active_tab().map(|tab| tab.id),
            Some(welcome),
            "does not silently expand the collapsed group"
        );
    }

    #[test]
    fn close_targets_stay_inside_the_clicked_tabs_region_and_never_touch_pinned() {
        let mut workspace = WorkspaceState::new();
        let w = workspace.tabs()[0].id;
        let a = workspace.open_tab("a");
        let b = workspace.open_tab("b");
        let c = workspace.open_tab("c");
        workspace.set_pinned(a, true);
        workspace.set_pinned(c, true);
        assert_eq!(ids(&workspace), vec![a, c, w, b]);

        assert_eq!(workspace.close_targets(w, TabCloseScope::Others), vec![b]);
        assert_eq!(
            workspace.close_targets(w, TabCloseScope::Left),
            Vec::<u64>::new()
        );
        assert_eq!(workspace.close_targets(w, TabCloseScope::Right), vec![b]);
        assert_eq!(workspace.close_targets(w, TabCloseScope::All), vec![w, b]);
        // A pinned tab acts on the pinned region only.
        assert_eq!(workspace.close_targets(a, TabCloseScope::Others), vec![c]);
        assert_eq!(workspace.close_targets(a, TabCloseScope::Right), vec![c]);
        assert_eq!(workspace.close_targets(c, TabCloseScope::Left), vec![a]);
        assert_eq!(workspace.close_targets(a, TabCloseScope::All), vec![a, c]);
    }

    #[test]
    fn close_group_targets_only_the_groups_unpinned_tabs() {
        let mut workspace = WorkspaceState::new();
        let p1 = bound(&mut workspace, "p1", "pg", "app");
        let p2 = bound(&mut workspace, "p2", "pg", "app");
        let s1 = bound(&mut workspace, "s1", "sq", "main");
        assert!(
            workspace.close_targets(p1, TabCloseScope::Group).is_empty(),
            "grouping off"
        );
        workspace.set_group_by(TabGroupBy::Connection);
        assert_eq!(
            workspace.close_targets(p1, TabCloseScope::Group),
            vec![p1, p2]
        );
        assert_eq!(workspace.close_targets(s1, TabCloseScope::Group), vec![s1]);
        workspace.set_pinned(p2, true);
        assert_eq!(workspace.close_targets(p1, TabCloseScope::Group), vec![p1]);
    }

    #[test]
    fn duplicate_copies_the_query_tab_after_it_and_rename_trims() {
        let mut workspace = WorkspaceState::new();
        let welcome = workspace.tabs()[0].id;
        let q = workspace.open_tab("Report");
        let last = workspace.open_tab("Last");
        workspace.tab_mut(q).unwrap().sql = "SELECT 1".into();
        assert_eq!(
            workspace.duplicate_tab(welcome),
            None,
            "only query tabs duplicate"
        );
        let copy = workspace.duplicate_tab(q).expect("duplicate");
        assert_eq!(ids(&workspace), vec![welcome, q, copy, last]);
        assert_eq!(workspace.tab(copy).unwrap().sql, "SELECT 1");
        assert_eq!(workspace.active_tab().map(|tab| tab.id), Some(copy));
        assert!(workspace.runtime(copy).is_some());

        assert!(!workspace.rename_tab(q, "   "));
        assert!(workspace.rename_tab(q, "  Orders  "));
        assert_eq!(workspace.tab(q).unwrap().title, "Orders");
    }

    // ---- panes -----------------------------------------------------------

    impl WorkspaceState {
        fn is_hidden_for_test(&self, id: u64) -> bool {
            self.tab(id).is_some_and(|tab| self.is_hidden(tab))
        }
    }

    use crate::panes::Node;

    fn pane_tabs(workspace: &WorkspaceState) -> Vec<Vec<u64>> {
        workspace
            .panes()
            .panes()
            .map(|pane| pane.tabs().to_vec())
            .collect()
    }

    /// `[welcome q1 q2 q3]` split into `[welcome q1] | [q2 q3]`.
    fn two_panes() -> (WorkspaceState, [u64; 4], PaneId, PaneId) {
        let mut workspace = WorkspaceState::new();
        let welcome = workspace.tabs()[0].id;
        let (q1, q2, q3) = (
            workspace.open_tab("q1"),
            workspace.open_tab("q2"),
            workspace.open_tab("q3"),
        );
        assert!(workspace.split_tab(q2, Edge::Right));
        let left = workspace.pane_of(welcome).unwrap();
        let right = workspace.pane_of(q2).unwrap();
        assert!(workspace.drop_tab(
            q3,
            PaneDrop::Tabs {
                pane: right,
                index: None
            }
        ));
        assert_eq!(pane_tabs(&workspace), vec![vec![welcome, q1], vec![q2, q3]]);
        (workspace, [welcome, q1, q2, q3], left, right)
    }

    #[test]
    fn new_tabs_open_in_the_focused_pane_and_activation_moves_the_focus() {
        let (mut workspace, [welcome, q1, q2, q3], left, right) = two_panes();
        assert_eq!(workspace.focused_pane(), right);
        let fresh = workspace.open_tab("fresh");
        assert_eq!(workspace.pane_of(fresh), Some(right));
        assert!(workspace.activate_tab(q1));
        assert_eq!(workspace.focused_pane(), left, "activating focuses the tab's pane");
        let next = workspace.open_tab("next");
        assert_eq!(workspace.pane_of(next), Some(left));
        assert_eq!(workspace.active_tab().map(|tab| tab.id), Some(next));
        assert_eq!(
            workspace.pane_active_tab(right).map(|tab| tab.id),
            Some(fresh),
            "the other pane keeps its own active tab"
        );
        let _ = (welcome, q2, q3);
    }

    #[test]
    fn closing_picks_the_neighbour_inside_the_same_pane_and_removes_empty_panes() {
        let (mut workspace, [welcome, q1, q2, q3], _left, right) = two_panes();
        // q3 is active in the right pane: closing it shows q2, not a left tab.
        assert!(workspace.close_tab(q3));
        assert_eq!(workspace.pane_active_tab(right).map(|tab| tab.id), Some(q2));
        assert_eq!(workspace.active_tab().map(|tab| tab.id), Some(q2));
        // The last tab of the right pane takes the pane with it; focus goes left.
        assert!(workspace.close_tab(q2));
        assert_eq!(workspace.panes().pane_count(), 1);
        assert_eq!(pane_tabs(&workspace), vec![vec![welcome, q1]]);
        assert_eq!(workspace.active_tab().map(|tab| tab.id), Some(q1));
    }

    #[test]
    fn strip_drops_across_panes_follow_the_pinned_and_group_rules() {
        let (mut workspace, [welcome, q1, q2, _q3], left, right) = two_panes();
        // Before q2 in the right pane: allowed, the tab changes pane.
        let before = TabDropTarget::Before(q2);
        assert!(matches!(workspace.plan_move(q1, &before), TabMovePlan::Move(_)));
        assert!(workspace.move_tab(q1, &before));
        assert_eq!(workspace.pane_of(q1), Some(right));
        assert_eq!(pane_tabs(&workspace)[1][..2], [q1, q2], "inserted before the anchor");
        assert_eq!(workspace.focused_pane(), right);
        assert_eq!(workspace.active_tab().map(|tab| tab.id), Some(q1));
        // A pinned tab does not drop between unpinned ones of another pane.
        workspace.set_pinned(welcome, true);
        assert_eq!(
            workspace.plan_move(welcome, &TabDropTarget::Before(q2)),
            TabMovePlan::Refused
        );
        // The tail of another pane accepts any tab; the pinned tab is filed first
        // and the emptied source pane goes away.
        assert!(workspace.move_tab(welcome, &TabDropTarget::End(right)));
        assert_eq!(workspace.panes().pane_count(), 1);
        assert_eq!(pane_tabs(&workspace)[0][0], welcome);
        let _ = left;
    }

    #[test]
    fn content_drops_split_append_and_repair_the_source_active_tab() {
        let (mut workspace, [welcome, q1, q2, q3], left, right) = two_panes();
        assert!(workspace.activate_tab(q1));
        // Same pane: nothing to do; splitting a lone tab: refused.
        assert!(!workspace.can_drop_tab(q1, PaneDrop::Tabs { pane: left, index: None }));
        assert!(workspace.can_drop_tab(q1, PaneDrop::Split { pane: left, edge: Edge::Bottom }));
        assert!(!workspace.drop_tab(q1, PaneDrop::Tabs { pane: left, index: None }));
        // q1 (the left pane's active tab) goes to the right pane's end.
        assert!(workspace.drop_tab(q1, PaneDrop::Tabs { pane: right, index: None }));
        assert_eq!(pane_tabs(&workspace), vec![vec![welcome], vec![q2, q3, q1]]);
        assert_eq!(workspace.pane_active_tab(left).map(|tab| tab.id), Some(welcome));
        // Now split q3 down: new pane below the right one.
        assert!(workspace.split_tab(q3, Edge::Bottom));
        assert_eq!(pane_tabs(&workspace), vec![vec![welcome], vec![q2, q1], vec![q3]]);
        // A lone tab cannot be split off its own pane.
        assert!(!workspace.split_tab(welcome, Edge::Right));
        // Move to Other Pane appends to the next pane (wrapping).
        assert!(workspace.move_tab_to_other_pane(q3));
        assert_eq!(workspace.panes().pane_count(), 2);
    }

    #[test]
    fn groups_collapse_per_pane_and_bulk_close_stays_in_the_pane() {
        let mut workspace = WorkspaceState::new();
        workspace.set_group_by(TabGroupBy::Connection);
        let welcome = workspace.tabs()[0].id;
        let a1 = bound(&mut workspace, "a1", "pg", "");
        let a2 = bound(&mut workspace, "a2", "pg", "");
        let a3 = bound(&mut workspace, "a3", "pg", "");
        // Left: [a1 a2 welcome], right: [a3].
        assert!(workspace.split_tab(a3, Edge::Right));
        let (left, right) = (
            workspace.pane_of(welcome).unwrap(),
            workspace.pane_of(a3).unwrap(),
        );
        assert_eq!(workspace.pane_active_tab(left).map(|tab| tab.id), Some(a2));
        // Collapsing the left pane's group hands its active tab to `welcome`.
        assert!(workspace.toggle_group_collapsed_in(left, &key("pg", "")));
        assert_eq!(workspace.pane_active_tab(left).map(|tab| tab.id), Some(welcome));
        assert!(workspace.is_hidden_for_test(a1), "a1 hidden in the left pane");
        assert!(!workspace.is_hidden_for_test(a3), "a3 stays drawn in the right pane");
        assert!(workspace.panes().pane(right).unwrap().collapsed().is_empty());
        assert!(matches!(
            workspace.pane_layout(left),
            [TabBarItem::Group { collapsed: true, count: 2, .. }, TabBarItem::Tab(id)] if *id == welcome
        ));
        assert!(matches!(
            workspace.pane_layout(right),
            [TabBarItem::Group { collapsed: false, count: 1, .. }, TabBarItem::Tab(id)] if *id == a3
        ));
        // Bulk close never reaches into the other pane.
        assert!(workspace.close_targets(a3, TabCloseScope::Others).is_empty());
        let mut all = workspace.close_targets(welcome, TabCloseScope::All);
        all.sort_unstable();
        assert_eq!(all, vec![welcome, a1, a2]);
        // Regrouping resets the pane that was regrouped, not the other one.
        workspace.set_pane_group_by(left, TabGroupBy::None);
        assert!(workspace.panes().pane(left).unwrap().collapsed().is_empty());
        assert_eq!(workspace.group_by_of(right), TabGroupBy::Connection);
    }

    #[test]
    fn a_tab_stays_bound_to_its_file_across_a_relaunch() {
        let mut workspace = WorkspaceState::new();
        let clean = workspace.open_query_tab("a.sql");
        let edited = workspace.open_query_tab("b.sql");
        for (id, path) in [(clean, "/tmp/a.sql"), (edited, "/tmp/b.sql")] {
            let tab = workspace.tab_mut(id).unwrap();
            tab.sql = "select 1".into();
            tab.file = Some(super::SqlFile { path: path.into(), saved: "select 1".into() });
        }
        workspace.tab_mut(edited).unwrap().sql = "select 1 -- more".into();
        assert!(!workspace.tab(clean).unwrap().has_unsaved_file());
        assert!(workspace.tab(edited).unwrap().has_unsaved_file());

        let session = workspace.snapshot();
        // Only the unsaved tab carries the text its file holds.
        let saved_of = |title: &str| {
            session.tabs.iter().find(|tab| tab.title == title).unwrap().file_saved.clone()
        };
        assert_eq!(saved_of("a.sql"), None);
        assert_eq!(saved_of("b.sql").as_deref(), Some("select 1"));
        let json = serde_json::to_string(&session).unwrap();
        let loaded: crate::gpui_session::GpuiSession = serde_json::from_str(&json).unwrap();

        let mut restored = WorkspaceState::new();
        restored.restore_snapshot(&loaded, |_| true);
        let tab = |title: &str| restored.tabs().iter().find(|tab| tab.title == title).unwrap().clone();
        let (a, b) = (tab("a.sql"), tab("b.sql"));
        assert_eq!(a.file.as_ref().unwrap().path, std::path::Path::new("/tmp/a.sql"));
        assert!(!a.has_unsaved_file());
        assert_eq!(b.file.as_ref().unwrap().path, std::path::Path::new("/tmp/b.sql"));
        assert!(b.has_unsaved_file(), "still unsaved after the relaunch");
        assert_eq!(&*b.file.as_ref().unwrap().saved, "select 1");

        // Older session files have no file keys: the tabs come back unbound.
        let old: crate::gpui_session::GpuiSession =
            serde_json::from_str(r#"{"tabs":[{"kind":"query","title":"Q","sql":"select 1"}]}"#).unwrap();
        let mut restored = WorkspaceState::new();
        restored.restore_snapshot(&old, |_| true);
        assert!(restored.tabs().iter().all(|tab| tab.file.is_none()));
    }

    #[test]
    fn snapshots_round_trip_panes_ratios_collapsed_groups_and_focus() {
        let (mut workspace, [welcome, q1, _, _], left, _right) = two_panes();
        let Node::Split(split) = workspace.panes().root() else { panic!("split root") };
        let split_id = split.id();
        assert!(workspace.set_split_ratios(split_id, &[0.35, 0.65]));
        workspace.set_pane_group_by(left, TabGroupBy::Connection);
        workspace.set_pane_group_by(_right, TabGroupBy::Database);
        workspace.set_pane_tab_rows(_right, true);
        workspace.tab_mut(q1).unwrap().connection_id = "pg".into();
        workspace.normalize();
        assert!(workspace.toggle_group_collapsed_in(left, &key("pg", "")));
        workspace.activate_tab(welcome);
        let session = workspace.snapshot();
        assert!(session.panes.is_some());
        let json = serde_json::to_string(&session).unwrap();
        let loaded: crate::gpui_session::GpuiSession = serde_json::from_str(&json).unwrap();
        assert_eq!(loaded, session);

        let mut restored = WorkspaceState::new();
        restored.restore_snapshot(&loaded, |_| true);
        // Each pane comes back with its own strip settings.
        let strips: Vec<_> = restored.panes().panes().map(|pane| pane.strip()).collect();
        assert_eq!(
            strips,
            vec![
                StripSettings { group_by: TabGroupBy::Connection, rows: false },
                StripSettings { group_by: TabGroupBy::Database, rows: true },
            ]
        );
        let describe = |workspace: &WorkspaceState| -> Vec<(Vec<String>, Vec<TabGroupKey>)> {
            workspace
                .panes()
                .panes()
                .map(|pane| {
                    let titles = pane
                        .tabs()
                        .iter()
                        .map(|id| workspace.tab(*id).unwrap().title.clone())
                        .collect();
                    (titles, pane.collapsed().to_vec())
                })
                .collect()
        };
        assert_eq!(describe(&restored), describe(&workspace));
        assert_eq!(describe(&restored)[0].1, vec![key("pg", "")]);
        let Node::Split(split) = restored.panes().root() else { panic!("split root") };
        assert!((split.ratios()[0] - 0.35).abs() < 0.001);
        assert_eq!(
            restored.active_tab().map(|tab| tab.title.clone()),
            workspace.active_tab().map(|tab| tab.title.clone())
        );
        assert_eq!(restored.focused_pane(), restored.panes().panes().next().unwrap().id());
    }

    #[test]
    fn sessions_from_before_panes_load_into_one_pane_with_the_legacy_fields() {
        let session = crate::gpui_session::GpuiSession {
            tabs: vec![
                crate::gpui_session::SessionTab {
                    kind: "welcome".into(),
                    title: "Welcome".into(),
                    ..Default::default()
                },
                crate::gpui_session::SessionTab {
                    title: "Old".into(),
                    connection_id: "pg".into(),
                    ..Default::default()
                },
            ],
            active: Some(1),
            collapsed_groups: vec![crate::gpui_session::SessionGroup {
                connection_id: "pg".into(),
                database: String::new(),
            }],
            panes: None,
            ..Default::default()
        };
        let mut workspace = WorkspaceState::new();
        workspace.set_group_by(TabGroupBy::Connection);
        workspace.restore_snapshot(&session, |_| true);
        assert_eq!(workspace.panes().pane_count(), 1);
        assert_eq!(workspace.tabs().len(), 2);
        assert_eq!(workspace.active_tab().map(|tab| tab.title.as_str()), Some("Old"));
        // The active tab's group is expanded even though the file collapsed it.
        assert!(!workspace.is_group_collapsed(&key("pg", "")));
        // A single pane writes the pre-panes format.
        assert!(workspace.snapshot().panes.is_none());
    }
    // ---- grouped rows ----------------------------------------------------

    #[test]
    fn rows_need_grouping_ignore_collapse_and_keep_global_tabs_out_of_chips() {
        let mut workspace = WorkspaceState::new();
        let welcome = workspace.tabs()[0].id;
        let pinned = bound(&mut workspace, "pin", "pg", "d");
        let (p1, p2) = (
            bound(&mut workspace, "p1", "pg", "d"),
            bound(&mut workspace, "p2", "pg", "d"),
        );
        let s1 = bound(&mut workspace, "s1", "sq", "d");
        workspace.set_pinned(pinned, true);
        workspace.set_group_by(TabGroupBy::Connection);
        let pane = workspace.focused_pane();

        // Not effective until switched on, nor without grouping.
        assert!(!workspace.tab_rows());
        workspace.set_tab_rows(true);
        assert!(workspace.tab_rows());
        workspace.set_group_by(TabGroupBy::None);
        assert!(!workspace.tab_rows(), "no groups, no second row");
        workspace.set_group_by(TabGroupBy::Connection);
        assert!(workspace.tab_rows());

        // Chips are the groups only: the pinned and Welcome tabs are global,
        // so there is no "Other" chip. s1 (sqlite) was the last active tab.
        let rows = workspace.pane_rows(pane);
        assert_eq!(
            rows.chips,
            vec![
                RowChip { group: Some(&key("pg", "")), count: 2 },
                RowChip { group: Some(&key("sq", "")), count: 1 },
            ]
        );
        assert!(rows.chip_row());
        assert_eq!(rows.active, Some(1));
        assert_eq!(rows.tabs, vec![pinned, welcome, s1], "global tabs lead the row");

        // A pinned / global tab becoming active keeps the shown group.
        workspace.activate_tab(welcome);
        let rows = workspace.pane_rows(pane);
        assert_eq!(rows.active, Some(1));
        assert_eq!(rows.tabs, vec![pinned, welcome, s1]);
        workspace.activate_tab(pinned);
        assert_eq!(workspace.pane_rows(pane).active, Some(1));

        // Collapsing is meaningless here: a collapsed group still lists its tabs.
        workspace.set_tab_rows(false);
        assert!(workspace.toggle_group_collapsed(&key("pg", "")));
        assert!(!workspace.is_tab_visible(p1));
        workspace.set_tab_rows(true);
        assert!(workspace.is_tab_visible(p1) && workspace.is_tab_visible(p2));
        workspace.activate_tab(p1);
        assert_eq!(workspace.pane_rows(pane).tabs, vec![pinned, welcome, p1, p2]);
    }

    #[test]
    fn one_group_has_no_chip_row_and_other_only_holds_loose_query_tabs() {
        let mut workspace = WorkspaceState::new();
        let welcome = workspace.tabs()[0].id;
        let p1 = bound(&mut workspace, "p1", "pg", "d");
        workspace.set_group_by(TabGroupBy::Connection);
        workspace.set_tab_rows(true);
        let pane = workspace.focused_pane();

        let rows = workspace.pane_rows(pane);
        assert_eq!(rows.chips.len(), 1);
        assert!(!rows.chip_row(), "a single group has nothing to choose");
        assert_eq!(rows.tabs, vec![welcome, p1], "everything in the one row");

        // A second group brings the chips back, a loose query tab "Other".
        let s1 = bound(&mut workspace, "s1", "sq", "d");
        assert!(workspace.pane_rows(pane).chip_row());
        let loose = workspace.open_tab("loose");
        let rows = workspace.pane_rows(pane);
        assert_eq!(rows.chips.len(), 3);
        assert_eq!(rows.chips[2], RowChip { group: None, count: 1 });
        assert_eq!(rows.active, Some(2));
        assert_eq!(rows.tabs, vec![welcome, loose]);
        workspace.close_tab(s1);
        workspace.close_tab(loose);
        assert!(!workspace.pane_rows(pane).chip_row());
    }

    #[test]
    fn group_targets_return_to_the_last_active_tab_and_cycle_with_wraparound() {
        let mut workspace = WorkspaceState::new();
        let welcome = workspace.tabs()[0].id;
        let (p1, p2, p3) = (
            bound(&mut workspace, "p1", "pg", "d"),
            bound(&mut workspace, "p2", "pg", "d"),
            bound(&mut workspace, "p3", "pg", "d"),
        );
        let s1 = bound(&mut workspace, "s1", "sq", "d");
        let loose = workspace.open_tab("loose");
        workspace.set_group_by(TabGroupBy::Connection);
        workspace.set_tab_rows(true);
        let pane = workspace.focused_pane();
        let (pg, sq) = (key("pg", ""), key("sq", ""));

        workspace.activate_tab(p2);
        workspace.activate_tab(s1);
        assert_eq!(workspace.group_target(pane, Some(&pg)), Some(p2), "remembered");
        assert_eq!(workspace.group_target(pane, Some(&sq)), Some(s1));
        assert_eq!(workspace.group_target(pane, None), Some(loose));
        assert_eq!(workspace.group_target(pane, Some(&key("missing", ""))), None);
        assert_eq!(workspace.group_tab_ids(pane, Some(&pg)), vec![p1, p2, p3]);
        assert_eq!(workspace.group_tab_ids(pane, None), vec![loose]);
        assert!(!workspace.group_tab_ids(pane, None).contains(&welcome));

        // The remembered tab closing falls back to the group's first tab.
        workspace.close_tab(p2);
        assert_eq!(workspace.group_target(pane, Some(&pg)), Some(p1));
        workspace.close_tab(p1);
        assert_eq!(workspace.group_target(pane, Some(&pg)), Some(p3));

        // Chips: pg, sq, Other. sqlite shown -> next = Other -> pg -> sq.
        assert_eq!(workspace.adjacent_group_target(pane, 1), Some(loose));
        workspace.activate_tab(loose);
        assert_eq!(workspace.adjacent_group_target(pane, 1), Some(p3), "wraps");
        assert_eq!(workspace.adjacent_group_target(pane, -1), Some(s1));
        workspace.close_tab(s1);
        workspace.close_tab(loose);
        assert_eq!(
            workspace.adjacent_group_target(pane, 1),
            None,
            "a single chip has nothing to step to"
        );
    }

    #[test]
    fn each_pane_has_its_own_rows_and_group_memory() {
        let mut workspace = WorkspaceState::new();
        let (a1, a2) = (
            bound(&mut workspace, "a1", "pg", "d"),
            bound(&mut workspace, "a2", "pg", "d"),
        );
        let b1 = bound(&mut workspace, "b1", "sq", "d");
        let b2 = bound(&mut workspace, "b2", "sq", "d");
        workspace.set_group_by(TabGroupBy::Connection);
        workspace.set_tab_rows(true);
        assert!(workspace.split_tab(b1, Edge::Right));
        let left = workspace.pane_of(a1).unwrap();
        let right = workspace.pane_of(b1).unwrap();

        // Left holds welcome + pg tabs + b2; right holds b1 only.
        let left_rows = workspace.pane_rows(left);
        assert_eq!(left_rows.chips.len(), 2, "pg and sq (b2)");
        let right_rows = workspace.pane_rows(right);
        assert_eq!(right_rows.chips.len(), 1);
        assert!(!right_rows.chip_row());
        assert_eq!(right_rows.tabs, vec![b1]);

        workspace.activate_tab(a1);
        workspace.activate_tab(a2);
        workspace.activate_tab(b1);
        assert_eq!(workspace.group_target(left, Some(&key("pg", ""))), Some(a2));
        assert_eq!(workspace.group_target(left, Some(&key("sq", ""))), Some(b2));
        assert_eq!(workspace.group_target(right, Some(&key("pg", ""))), None);
    }
    #[test]
    fn database_groups_of_one_connection_sit_together_and_cross_pane_drops_merge_by_key() {
        let mut workspace = WorkspaceState::new();
        let m1 = bound(&mut workspace, "m1", "mongo", "shop");
        let t1 = bound(&mut workspace, "t1", "pg", "trans");
        let m2 = bound(&mut workspace, "m2", "mongo", "scripts");
        let m3 = bound(&mut workspace, "m3", "mongo", "shop");
        workspace.set_group_by(TabGroupBy::Database);
        let pane = workspace.focused_pane();
        // Same connection adjacent (databases by first appearance), then the
        // next connection, unbound last.
        assert_eq!(&ids(&workspace)[..4], &[m1, m3, m2, t1]);
        let chips: Vec<_> = workspace
            .layout()
            .iter()
            .filter_map(|item| match item {
                TabBarItem::Group { key, .. } => Some((key.connection_id.clone(), key.database.clone())),
                TabBarItem::Tab(_) => None,
            })
            .collect();
        assert_eq!(
            chips,
            vec![
                ("mongo".to_string(), "shop".to_string()),
                ("mongo".to_string(), "scripts".to_string()),
                ("pg".to_string(), "trans".to_string()),
            ]
        );

        // Move m2 and t1 to a split, then back into the first pane: every
        // group keeps one chip per key, and the tabs rejoin their group.
        assert!(workspace.split_tab(m2, Edge::Right));
        let right = workspace.pane_of(m2).unwrap();
        assert!(workspace.drop_tab(t1, PaneDrop::Tabs { pane: right, index: None }));
        workspace.set_group_by(TabGroupBy::Connection);
        workspace.set_group_by(TabGroupBy::Database);
        assert!(workspace.drop_tab(m2, PaneDrop::Tabs { pane, index: None }));
        assert!(workspace.drop_tab(t1, PaneDrop::Tabs { pane, index: None }));
        assert_eq!(workspace.panes().pane_count(), 1);
        let keys: Vec<TabGroupKey> = workspace
            .pane_layout(workspace.focused_pane())
            .iter()
            .filter_map(|item| match item {
                TabBarItem::Group { key, .. } => Some(key.clone()),
                TabBarItem::Tab(_) => None,
            })
            .collect();
        assert_eq!(keys, vec![key("mongo", "shop"), key("mongo", "scripts"), key("pg", "trans")]);

        // Connection mode merges the databases of one connection into one group.
        workspace.set_group_by(TabGroupBy::Connection);
        let chips = workspace
            .layout()
            .iter()
            .filter(|item| matches!(item, TabBarItem::Group { .. }))
            .count();
        assert_eq!(chips, 2);
    }
    #[test]
    fn strips_are_per_pane_splits_inherit_and_groups_follow_each_panes_mode() {
        let mut workspace = WorkspaceState::new();
        let a1 = bound(&mut workspace, "a1", "mongo", "shop");
        let a2 = bound(&mut workspace, "a2", "mongo", "scripts");
        let b1 = bound(&mut workspace, "b1", "pg", "trans");
        workspace.set_group_by(TabGroupBy::Connection);
        workspace.set_tab_rows(true);
        let left = workspace.focused_pane();
        // The split inherits the source pane's settings.
        assert!(workspace.split_tab(b1, Edge::Right));
        let right = workspace.pane_of(b1).unwrap();
        let both = StripSettings { group_by: TabGroupBy::Connection, rows: true };
        assert_eq!((workspace.strip_of(left), workspace.strip_of(right)), (both, both));

        // Changing one pane leaves the other alone.
        workspace.set_pane_group_by(right, TabGroupBy::None);
        assert_eq!(workspace.strip_of(left), both);
        assert_eq!(workspace.group_by_of(right), TabGroupBy::None);
        assert!(!workspace.tab_rows_of(right), "no grouping, no rows");
        assert!(workspace.tab_rows_of(left));
        workspace.set_pane_group_by(right, TabGroupBy::Connection);
        workspace.set_pane_tab_rows(right, false);
        assert!(workspace.tab_rows_of(left) && !workspace.tab_rows_of(right));

        // Groups are evaluated per pane: left by connection (one mongo chip),
        // after moving a2 right and regrouping that pane by database.
        workspace.set_pane_group_by(right, TabGroupBy::Database);
        assert!(workspace.drop_tab(a2, PaneDrop::Tabs { pane: right, index: None }));
        let chips = |workspace: &WorkspaceState, pane| -> Vec<TabGroupKey> {
            workspace
                .pane_layout(pane)
                .iter()
                .filter_map(|item| match item {
                    TabBarItem::Group { key, .. } => Some(key.clone()),
                    TabBarItem::Tab(_) => None,
                })
                .collect()
        };
        assert_eq!(chips(&workspace, left), vec![key("mongo", "")]);
        assert_eq!(chips(&workspace, right), vec![key("mongo", "scripts"), key("pg", "trans")]);
        // a2 is judged by the destination's mode when dragged around.
        assert_eq!(workspace.plan_move(a1, &TabDropTarget::GroupStart(right, key("mongo", "scripts"))), TabMovePlan::Refused);
        assert!(matches!(
            workspace.plan_move(a1, &TabDropTarget::End(right)),
            TabMovePlan::Move(_)
        ));
    }

    #[test]
    fn old_sessions_give_every_pane_the_saved_preferences_as_its_settings() {
        let mut workspace = WorkspaceState::new();
        let q1 = bound(&mut workspace, "q1", "pg", "d");
        let q2 = bound(&mut workspace, "q2", "pg", "d");
        let _ = q1;
        assert!(workspace.split_tab(q2, Edge::Right));
        let right = workspace.pane_of(q2).unwrap();
        workspace.set_pane_group_by(right, TabGroupBy::Database);
        let session = workspace.snapshot();
        let mut value = serde_json::to_value(&session).unwrap();
        fn strip_strips(value: &mut serde_json::Value) {
            match value {
                serde_json::Value::Object(map) => {
                    map.remove("strip");
                    map.values_mut().for_each(strip_strips);
                }
                serde_json::Value::Array(items) => items.iter_mut().for_each(strip_strips),
                _ => {}
            }
        }
        strip_strips(&mut value);
        let old: crate::gpui_session::GpuiSession = serde_json::from_value(value).unwrap();

        let defaults = StripSettings { group_by: TabGroupBy::Connection, rows: true };
        let mut restored = WorkspaceState::new();
        restored.set_strip_defaults(defaults);
        restored.restore_snapshot(&old, |_| true);
        assert_eq!(restored.panes().pane_count(), 2);
        assert!(restored.panes().panes().all(|pane| pane.strip() == defaults));

        // The current format round-trips each pane's own settings instead.
        let mut current = WorkspaceState::new();
        current.set_strip_defaults(defaults);
        current.restore_snapshot(&session, |_| true);
        let strips: Vec<_> = current.panes().panes().map(|pane| pane.strip().group_by).collect();
        assert_eq!(strips, vec![TabGroupBy::None, TabGroupBy::Database]);

        // A session without any layout (one pane) also takes the defaults.
        let single = crate::gpui_session::GpuiSession::default();
        let mut fresh = WorkspaceState::new();
        fresh.set_strip_defaults(defaults);
        fresh.restore_snapshot(&single, |_| true);
        assert_eq!(fresh.strip_of(fresh.focused_pane()), defaults);
    }
    #[test]
    fn chip_rows_align_among_rows_mode_siblings_in_a_side_by_side_split() {
        let mut workspace = WorkspaceState::new();
        let a1 = bound(&mut workspace, "a1", "pg", "d");
        let _a2 = bound(&mut workspace, "a2", "sq", "d");
        let b1 = bound(&mut workspace, "b1", "pg", "d");
        workspace.set_group_by(TabGroupBy::Connection);
        workspace.set_tab_rows(true);
        let left = workspace.focused_pane();
        assert!(workspace.split_tab(b1, Edge::Right));
        let right = workspace.pane_of(b1).unwrap();
        // Left: pg + sq (two chips); right: one group.
        assert!(workspace.pane_rows(left).chip_row());
        assert!(!workspace.pane_rows(right).chip_row());
        assert!(workspace.pane_chip_row(left));
        assert!(workspace.pane_chip_row(right), "aligned beside a chip row");
        let before = (workspace.strip_of(left), workspace.strip_of(right));

        // Ungrouped or not in rows mode: excluded, one row.
        workspace.set_pane_group_by(right, TabGroupBy::None);
        assert!(!workspace.pane_chip_row(right));
        workspace.set_pane_group_by(right, TabGroupBy::Connection);
        workspace.set_pane_tab_rows(right, false);
        assert!(!workspace.pane_chip_row(right));
        workspace.set_pane_tab_rows(right, true);
        assert_eq!((workspace.strip_of(left), workspace.strip_of(right)), before);

        // Without a two-group neighbour nobody shows a chip row.
        workspace.close_tab(a1);
        assert!(!workspace.pane_rows(left).chip_row(), "left is down to one group");
        assert!(!workspace.pane_chip_row(left) && !workspace.pane_chip_row(right));

        // Stacked (column) panes never align.
        let mut stacked = WorkspaceState::new();
        let s1 = bound(&mut stacked, "s1", "pg", "d");
        let _s2 = bound(&mut stacked, "s2", "sq", "d");
        let s3 = bound(&mut stacked, "s3", "pg", "d");
        stacked.set_group_by(TabGroupBy::Connection);
        stacked.set_tab_rows(true);
        let top = stacked.focused_pane();
        assert!(stacked.split_tab(s3, Edge::Bottom));
        let bottom = stacked.pane_of(s3).unwrap();
        let _ = s1;
        assert!(stacked.pane_chip_row(top));
        assert!(!stacked.pane_chip_row(bottom), "column splits decide per pane");
    }
}
