//! Background fetch + cache of table properties for the diagram. Owner: DG-D.
//!
//! Contract for the view (DG):
//! - `AppRoot::request_diagram(tab_id, cx)` starts loading the focus table's
//!   properties and then its related tables (depth 1, at most `MAX_RELATED`,
//!   at most `MAX_CONCURRENT` fetches in flight), on the background executor
//!   — never I/O or a driver-session lock on the UI thread. It is idempotent:
//!   while the tab's diagram is loading or loaded for the same connection
//!   session and table it does nothing, so the view may call it whenever the
//!   diagram is shown. Each completed fetch updates the cache and notifies.
//! - `AppRoot::refresh_diagram(tab_id, cx)` drops the cache and refetches
//!   everything (the view's Refresh / Retry).
//! - `AppRoot::diagram_graph(tab_id) -> Option<DiagramSnapshot>` returns the
//!   current graph (rebuilt only when the cache generation changed) for the
//!   view to lay out and render. `None` until the focus table's properties
//!   arrived; a failed focus yields an empty graph with `error` set.
//! - Whole-schema diagrams (`request_schema_diagram` / `refresh_schema_diagram`
//!   / `schema_diagram_graph`): every table of one schema (listed from the
//!   navigator cache, else loaded through the navigator's own load path, at
//!   most `MAX_SCHEMA_TABLES`), described with the same bounded fetcher. The
//!   snapshot is republished at most every 100 ms while tables arrive and
//!   carries `progress` until the last one landed.
//!
//! Driver route: `describe_object` (same path as app/results/properties.rs). Cache key: connection +
//! `TableRef`, kept for the tab's lifetime; dropped when the tab closes.

use std::cell::RefCell;
use std::collections::hash_map::Entry;
use std::collections::{HashMap, VecDeque};
use std::sync::Arc;

use crate::catalog::TableProperties;
use engine_driver_api::sql as wire;
use gpui::{App, Context, Task};

use super::model::{DiagramGraph, RelatedProperties, TableRef};
use crate::app::AppRoot;
use crate::error_log::Area;
use crate::navigator::NavigatorNodeId;
use crate::workspace::{QueryModeExt, Tab};

pub(in crate::app) const MAX_RELATED: usize = 40;
pub(in crate::app) const MAX_CONCURRENT: usize = 4;
/// Most tables a whole-schema diagram describes.
pub(in crate::app) const MAX_SCHEMA_TABLES: usize = 300;
/// A schema diagram shows new results at most this often.
const PUBLISH_INTERVAL: std::time::Duration = std::time::Duration::from_millis(100);

/// The graph plus a generation the view uses to know when to re-layout.
#[derive(Debug, Clone)]
pub(in crate::app) struct DiagramSnapshot {
    pub(in crate::app) graph: Arc<DiagramGraph>,
    pub(in crate::app) generation: u64,
    /// Set when the focus table itself failed to load (a schema diagram: when
    /// its table list failed).
    pub(in crate::app) error: Option<String>,
    /// Schema diagrams still describing tables: `(loaded, total)`; `None` for
    /// table diagrams and once everything is loaded.
    pub(in crate::app) progress: Option<(usize, usize)>,
    /// Schema diagrams: the schema has more than `MAX_SCHEMA_TABLES` tables;
    /// holds the real count (only the first `MAX_SCHEMA_TABLES` by name are
    /// shown).
    pub(in crate::app) truncated: Option<usize>,
}

/// Why one table's properties could not be fetched. `Display` keeps the
/// driver's `driver rejected the request: <code>: <message>` text, which the
/// lost-connection check parses.
#[derive(Debug)]
enum DiagramFetchError {
    NotConnected,
    Driver(engine_driver_host::LoaderError),
    Request(serde_json::Error),
    Unreadable(serde_json::Error),
    /// Scenario fixture catalogs and test doubles.
    Fixture(String),
}

impl std::fmt::Display for DiagramFetchError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::NotConnected => f.write_str("The connection is not open"),
            Self::Driver(error) => error.fmt(f),
            Self::Request(error) => write!(f, "Could not build the table request: {error}"),
            Self::Unreadable(error) => write!(f, "Unreadable table properties: {error}"),
            Self::Fixture(message) => f.write_str(message),
        }
    }
}

impl From<engine_driver_host::LoaderError> for DiagramFetchError {
    fn from(error: engine_driver_host::LoaderError) -> Self {
        Self::Driver(error)
    }
}

#[cfg(test)]
pub(in crate::app) type InjectedFetch =
    dyn Fn(&TableRef) -> Result<TableProperties, String> + Send + Sync;

/// Where one tab's properties come from, resolved on the UI thread from the
/// connection's route; cloned into each background fetch.
#[derive(Clone)]
enum Source {
    Driver {
        session: Arc<engine_driver_host::SharedDriverSession>,
        /// A driver without `metadata.table_properties` is never called: its
        /// tables just have no relations.
        describes: bool,
    },
    Fixture {
        kind: crate::ui_scenario::FixtureKind,
        connection: String,
        chinook: bool,
    },
    #[cfg(test)]
    Injected(Arc<InjectedFetch>),
}

impl Source {
    fn fetch(&self, table: &TableRef, cx: &App) -> Task<Result<TableProperties, DiagramFetchError>> {
        let schema = table.schema.clone().unwrap_or_default();
        match self {
            Self::Driver { session, describes } => {
                if !describes {
                    return Task::ready(Ok(TableProperties::default()));
                }
                let request = serde_json::to_value(wire::SqlObjectRef {
                    database: Some(table.database.clone()).filter(|database| !database.is_empty()),
                    schema: Some(schema).filter(|schema| !schema.is_empty()),
                    object: table.table.clone(),
                })
                .map_err(DiagramFetchError::Request);
                let session = Arc::clone(session);
                cx.background_executor().spawn(async move {
                    smol::unblock(move || {
                        let described = session.describe_object(&request?)?;
                        serde_json::from_value::<TableProperties>(described)
                            .map_err(DiagramFetchError::Unreadable)
                    })
                    .await
                })
            }
            Self::Fixture { kind, connection, chinook } => {
                let result = if *chinook {
                    crate::ui_fixture_diagram::table_properties(&table.table)
                } else {
                    crate::ui_fixture_properties::table_properties(
                        *kind,
                        connection,
                        &schema,
                        &table.table,
                    )
                };
                Task::ready(result.map_err(DiagramFetchError::Fixture))
            }
            #[cfg(test)]
            Self::Injected(fetch) => {
                let (fetch, table) = (Arc::clone(fetch), table.clone());
                cx.background_executor().spawn(async move {
                    smol::unblock(move || fetch(&table).map_err(DiagramFetchError::Fixture)).await
                })
            }
        }
    }
}

/// The fetch schedule and cache every diagram kind shares: tables queued,
/// at most `MAX_CONCURRENT` in flight, results cached per table.
struct Schedule {
    source: Option<Source>,
    /// Every table fetched (or being fetched), keyed by table.
    cache: HashMap<TableRef, RelatedProperties>,
    queue: VecDeque<TableRef>,
    in_flight: usize,
    /// Completions of an older request are dropped.
    epoch: u64,
    /// Bumps on every completed fetch.
    generation: u64,
}

impl Schedule {
    fn new(
        source: Option<Source>,
        mut cache: HashMap<TableRef, RelatedProperties>,
        (epoch, generation): (u64, u64),
    ) -> Self {
        // A fetch of the previous request will never report back.
        cache.retain(|_, entry| !matches!(entry, RelatedProperties::Loading));
        Self {
            source,
            cache,
            queue: VecDeque::new(),
            in_flight: 0,
            epoch,
            generation,
        }
    }

    /// Queues `table` unless it is cached (or already queued).
    fn enqueue(&mut self, table: TableRef) {
        if let Entry::Vacant(slot) = self.cache.entry(table.clone()) {
            slot.insert(RelatedProperties::Loading);
            self.queue.push_back(table);
        }
    }

    /// The fetches to start now: queued tables up to `MAX_CONCURRENT` in flight.
    fn take_jobs(&mut self) -> Vec<TableRef> {
        let room = MAX_CONCURRENT.saturating_sub(self.in_flight);
        let jobs: Vec<TableRef> = self.queue.drain(..room.min(self.queue.len())).collect();
        self.in_flight += jobs.len();
        jobs
    }

    /// Stores a completed fetch. `false` when it belongs to an older request.
    fn store(
        &mut self,
        epoch: u64,
        table: TableRef,
        result: Result<Arc<TableProperties>, String>,
    ) -> bool {
        if epoch != self.epoch {
            return false;
        }
        self.in_flight = self.in_flight.saturating_sub(1);
        self.generation += 1;
        let entry = match result {
            Ok(properties) => RelatedProperties::Ready(properties),
            Err(message) => RelatedProperties::Failed(message),
        };
        self.cache.insert(table, entry);
        true
    }

    /// The connection is gone: the queued tables cannot load either.
    fn fail_queued(&mut self, message: &str) {
        for table in self.queue.drain(..) {
            self.cache.insert(table, RelatedProperties::Failed(message.to_string()));
        }
    }

    fn idle(&self) -> bool {
        self.queue.is_empty() && self.in_flight == 0
    }

    fn loaded(&self, table: &TableRef) -> bool {
        !matches!(self.cache.get(table), None | Some(RelatedProperties::Loading))
    }
}

/// A graph built for one generation.
struct Built {
    generation: u64,
    graph: Arc<DiagramGraph>,
    progress: Option<(usize, usize)>,
}

/// One tab's table diagram: the focus table and its foreign-key neighbours.
struct TabDiagram {
    connection: String,
    /// The engine session the cache was fetched through; a reconnect refetches.
    session: u64,
    focus: TableRef,
    sched: Schedule,
    memo: RefCell<Option<Built>>,
}

impl TabDiagram {
    /// Starts the schedule: the focus is fetched (or reused from `cache`),
    /// then its related tables.
    fn begin(
        connection: String,
        session: u64,
        focus: TableRef,
        source: Result<Source, DiagramFetchError>,
        mut cache: HashMap<TableRef, RelatedProperties>,
        stamp: (u64, u64),
    ) -> Self {
        if matches!(cache.get(&focus), Some(RelatedProperties::Failed(_))) {
            cache.remove(&focus);
        }
        let mut state = Self {
            connection,
            session,
            focus,
            sched: Schedule::new(None, cache, stamp),
            memo: RefCell::new(None),
        };
        match source {
            Err(error) => {
                state
                    .sched
                    .cache
                    .insert(state.focus.clone(), RelatedProperties::Failed(error.to_string()));
            }
            Ok(source) => {
                state.sched.source = Some(source);
                if state.sched.cache.contains_key(&state.focus) {
                    state.enqueue_related();
                } else {
                    state.sched.enqueue(state.focus.clone());
                }
            }
        }
        state
    }

    /// Whether this is the diagram of `tab` under `session` (no allocation:
    /// the view asks on every frame).
    fn matches(&self, tab: &Tab, session: u64) -> bool {
        self.session == session
            && self.connection == tab.connection_id
            && self.focus.table == tab.collection
            && self.focus.database == tab.database
            && self.focus.schema.as_deref() == tab.schema.as_deref().filter(|schema| !schema.is_empty())
    }

    /// Queues the not yet cached related tables of the (loaded) focus.
    fn enqueue_related(&mut self) {
        let Some(RelatedProperties::Ready(properties)) = self.sched.cache.get(&self.focus) else {
            return;
        };
        let properties = Arc::clone(properties);
        for table in DiagramGraph::related_tables(&self.focus, &properties, MAX_RELATED) {
            self.sched.enqueue(table);
        }
    }

    fn finish(
        &mut self,
        epoch: u64,
        table: TableRef,
        result: Result<Arc<TableProperties>, String>,
    ) -> bool {
        let is_focus = table == self.focus;
        if !self.sched.store(epoch, table, result) {
            return false;
        }
        if is_focus {
            self.enqueue_related();
        }
        true
    }

    fn snapshot(&self) -> Option<DiagramSnapshot> {
        let (properties, error) = match self.sched.cache.get(&self.focus)? {
            RelatedProperties::Loading => return None,
            RelatedProperties::Failed(message) => (None, Some(message.clone())),
            RelatedProperties::Ready(properties) => (Some(properties), None),
        };
        let mut memo = self.memo.borrow_mut();
        let generation = self.sched.generation;
        let built = match &*memo {
            Some(built) if built.generation == generation => Arc::clone(&built.graph),
            _ => {
                let graph = Arc::new(properties.map_or_else(DiagramGraph::default, |properties| {
                    DiagramGraph::build(&self.focus, properties, &|table| {
                        // A table the cap left out is not fetched.
                        self.sched.cache.get(table).cloned().unwrap_or_else(|| {
                            RelatedProperties::Failed(format!(
                                "Not loaded: more than {MAX_RELATED} related tables"
                            ))
                        })
                    })
                }));
                *memo = Some(Built { generation, graph: Arc::clone(&graph), progress: None });
                graph
            }
        };
        Some(DiagramSnapshot { graph: built, generation, error, progress: None, truncated: None })
    }
}

/// What a whole-schema diagram shows.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(in crate::app) struct SchemaDiagramTarget {
    pub(in crate::app) connection: String,
    pub(in crate::app) database: String,
    /// `None` for engines without a schema level.
    pub(in crate::app) schema: Option<String>,
}

/// Where a schema diagram stands with its table list.
enum TableList {
    /// The navigator is loading the schema's tables.
    Waiting,
    Ready,
    Failed(String),
}

/// One tab's whole-schema diagram.
struct SchemaDiagram {
    target: SchemaDiagramTarget,
    session: u64,
    list: TableList,
    tables: Vec<TableRef>,
    truncated: Option<usize>,
    sched: Schedule,
    /// The generation the view sees; advances at most every `PUBLISH_INTERVAL`
    /// so a burst of completions costs one rebuild.
    published: u64,
    last_publish: std::time::Instant,
    publish_timer: bool,
    memo: RefCell<Option<Built>>,
}

impl SchemaDiagram {
    fn matches(&self, tab: &Tab, session: u64) -> bool {
        self.session == session
            && self.target.connection == tab.connection_id
            && self.target.database == tab.database
            && self.target.schema.as_deref() == tab.schema.as_deref().filter(|schema| !schema.is_empty())
    }

    /// The table list arrived: queue the first `MAX_SCHEMA_TABLES` by name.
    fn set_tables(&mut self, mut names: Vec<String>) {
        names.sort();
        names.dedup();
        self.truncated = (names.len() > MAX_SCHEMA_TABLES).then_some(names.len());
        names.truncate(MAX_SCHEMA_TABLES);
        self.tables = names
            .into_iter()
            .map(|table| TableRef {
                database: self.target.database.clone(),
                schema: self.target.schema.clone(),
                table,
            })
            .collect();
        for table in self.tables.clone() {
            self.sched.enqueue(table);
        }
        self.list = TableList::Ready;
        self.sched.generation += 1;
        self.publish();
    }

    fn publish(&mut self) {
        self.published = self.sched.generation;
        self.last_publish = std::time::Instant::now();
    }

    fn complete(&self) -> bool {
        !matches!(self.list, TableList::Waiting) && self.sched.idle()
    }

    fn snapshot(&self) -> Option<DiagramSnapshot> {
        let error = match &self.list {
            TableList::Waiting => return None,
            TableList::Failed(message) => Some(message.clone()),
            TableList::Ready => None,
        };
        let mut memo = self.memo.borrow_mut();
        if memo.as_ref().is_none_or(|built| built.generation != self.published) {
            let entries: Vec<(TableRef, RelatedProperties)> = self
                .tables
                .iter()
                .map(|table| {
                    let entry = self.sched.cache.get(table).cloned();
                    (table.clone(), entry.unwrap_or(RelatedProperties::Loading))
                })
                .collect();
            let loaded = self.tables.iter().filter(|table| self.sched.loaded(table)).count();
            *memo = Some(Built {
                generation: self.published,
                graph: Arc::new(DiagramGraph::build_schema(&entries)),
                progress: (loaded < self.tables.len()).then_some((loaded, self.tables.len())),
            });
        }
        let built = memo.as_ref()?;
        Some(DiagramSnapshot {
            graph: Arc::clone(&built.graph),
            generation: self.published,
            error,
            progress: built.progress,
            truncated: self.truncated,
        })
    }
}

/// Which map of [`DiagramState`] a fetch belongs to.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Scope {
    Table,
    Schema,
}

/// `(frame, connection, database, schema)` and the edit context computed for it then.
/// The edit contexts of one frame, by namespace.
#[derive(Default)]
pub(in crate::app) struct ContextMemo {
    pub(in crate::app) frame: u64,
    pub(in crate::app) entries: Vec<ContextEntry>,
}

pub(in crate::app) struct ContextEntry {
    pub(in crate::app) connection: String,
    pub(in crate::app) database: String,
    pub(in crate::app) schema: Option<String>,
    pub(in crate::app) context: Option<std::rc::Rc<crate::app::structure_edit::EditContext>>,
}

/// What the navigator's table names of a namespace looked like when its list was last built.
pub(in crate::app) type CatalogKey = (usize, Option<String>, Option<String>);

/// The table names of each namespace the edit contexts have read, with the key they were built for.
pub(in crate::app) type CatalogNames = HashMap<NavigatorNodeId, (CatalogKey, Arc<Vec<String>>)>;

/// Per-tab diagram data, dropped with the tab.
#[derive(Default)]
pub(in crate::app) struct DiagramState {
    tabs: HashMap<u64, TabDiagram>,
    schemas: HashMap<u64, SchemaDiagram>,
    next_epoch: u64,
    /// Tests replace the engine / driver I/O with this.
    #[cfg(test)]
    pub(in crate::app) injected: Option<Arc<InjectedFetch>>,
    /// Tests replace the statements an Apply sends to the connection with this.
    #[cfg(test)]
    pub(in crate::app) injected_exec: Option<Arc<crate::app::structure_edit::apply::InjectedExec>>,
    /// The `sql_mode` the injected apply session reports (MySQL, MariaDB); none for a session that reads none.
    #[cfg(test)]
    pub(in crate::app) injected_sql_mode: Option<String>,
    /// The apply running on each connection (at most one).
    pub(in crate::app) applies: crate::app::structure_edit::apply::Applies,
    /// `sql_mode` of the MySQL / MariaDB connections an apply has read.
    pub(in crate::app) sql_modes: HashMap<String, String>,
    /// The edit context of each namespace, computed once per frame (the Properties toolbar and its
    /// sync both ask on every frame, for every tab).
    pub(in crate::app) context_memo: RefCell<ContextMemo>,
    /// The navigator's table names for the edit contexts, one list per namespace, rebuilt when its list changes.
    pub(in crate::app) catalog_memo: RefCell<CatalogNames>,
}

impl DiagramState {
    fn schedule_mut(&mut self, tab_id: u64, scope: Scope) -> Option<&mut Schedule> {
        match scope {
            Scope::Table => self.tabs.get_mut(&tab_id).map(|state| &mut state.sched),
            Scope::Schema => self.schemas.get_mut(&tab_id).map(|state| &mut state.sched),
        }
    }

    fn connection(&self, tab_id: u64, scope: Scope) -> Option<&str> {
        match scope {
            Scope::Table => self.tabs.get(&tab_id).map(|state| state.connection.as_str()),
            Scope::Schema => self.schemas.get(&tab_id).map(|state| state.target.connection.as_str()),
        }
    }

    fn next_epoch(&mut self) -> u64 {
        self.next_epoch += 1;
        self.next_epoch
    }
}

#[cfg(test)]
impl DiagramState {
    pub(in crate::app) fn tracks(&self, tab_id: u64) -> bool {
        self.tabs.contains_key(&tab_id) || self.schemas.contains_key(&tab_id)
    }
}

impl AppRoot {
    /// Starts loading the diagram of `tab_id` unless it is already loading or
    /// loaded for the same table and connection session.
    pub(in crate::app) fn request_diagram(&mut self, tab_id: u64, cx: &mut Context<Self>) {
        self.load_diagram(tab_id, false, cx);
    }

    /// Drops everything fetched for `tab_id` and fetches it again.
    pub(in crate::app) fn refresh_diagram(&mut self, tab_id: u64, cx: &mut Context<Self>) {
        self.load_diagram(tab_id, true, cx);
    }

    /// The graph to render, rebuilt only when a fetch completed since the last
    /// call (the same `Arc` otherwise).
    pub(in crate::app) fn diagram_graph(&self, tab_id: u64) -> Option<DiagramSnapshot> {
        self.diagram.tabs.get(&tab_id)?.snapshot()
    }

    /// Closing the tab drops its caches; fetches still in flight are ignored.
    pub(in crate::app) fn diagram_tab_removed(&mut self, tab_id: u64) {
        self.diagram.tabs.remove(&tab_id);
        self.diagram.schemas.remove(&tab_id);
    }

    /// The driver session `connection` currently runs on (0: none).
    fn diagram_session(&self, connection: &str) -> u64 {
        self.driver_runtime
            .connection_ids
            .get(connection)
            .copied()
            .unwrap_or(0)
    }

    fn load_diagram(&mut self, tab_id: u64, refresh: bool, cx: &mut Context<Self>) {
        let Some(tab) = self.workspace.tab(tab_id).filter(|tab| {
            tab.query_mode.is_sql_table() && !tab.connection_id.is_empty() && !tab.collection.is_empty()
        }) else {
            return;
        };
        let session = self.diagram_session(&tab.connection_id);
        if !refresh
            && self
                .diagram
                .tabs
                .get(&tab_id)
                .is_some_and(|state| state.matches(tab, session))
        {
            return;
        }
        let connection = tab.connection_id.clone();
        let focus = TableRef {
            database: tab.database.clone(),
            schema: tab.schema.clone().filter(|schema| !schema.is_empty()),
            table: tab.collection.clone(),
        };
        let previous = self.diagram.tabs.remove(&tab_id);
        // The generation keeps counting so the view relayouts; only another
        // table of the same session keeps the cache.
        let generation = previous.as_ref().map_or(0, |state| state.sched.generation);
        let cache = previous
            .filter(|state| !refresh && state.connection == connection && state.session == session)
            .map(|state| state.sched.cache)
            .unwrap_or_default();
        let epoch = self.diagram.next_epoch();
        let source = self.diagram_source(&connection);
        let state = TabDiagram::begin(connection, session, focus, source, cache, (epoch, generation + 1));
        self.diagram.tabs.insert(tab_id, state);
        self.pump_diagram(tab_id, Scope::Table, cx);
        cx.notify();
    }

    /// Starts the whole-schema diagram of `tab_id` (idempotent, like
    /// [`Self::request_diagram`]).
    pub(in crate::app) fn request_schema_diagram(&mut self, tab_id: u64, cx: &mut Context<Self>) {
        self.load_schema_diagram(tab_id, false, cx);
    }

    /// Drops the schema diagram's cache and describes every table again.
    pub(in crate::app) fn refresh_schema_diagram(&mut self, tab_id: u64, cx: &mut Context<Self>) {
        self.load_schema_diagram(tab_id, true, cx);
    }

    /// The whole-schema graph to render; `None` until the table list is known.
    pub(in crate::app) fn schema_diagram_graph(&self, tab_id: u64) -> Option<DiagramSnapshot> {
        self.diagram.schemas.get(&tab_id)?.snapshot()
    }

    /// What a schema-diagram tab shows: its connection, database and schema.
    fn schema_diagram_target(&self, tab_id: u64) -> Option<SchemaDiagramTarget> {
        let tab = self.workspace.tab(tab_id)?;
        (tab.kind == crate::workspace::TabKind::Diagram
            && !tab.connection_id.is_empty()
            && !tab.database.is_empty())
        .then(|| SchemaDiagramTarget {
            connection: tab.connection_id.clone(),
            database: tab.database.clone(),
            schema: tab.schema.clone().filter(|schema| !schema.is_empty()),
        })
    }

    fn load_schema_diagram(&mut self, tab_id: u64, refresh: bool, cx: &mut Context<Self>) {
        let Some(target) = self.schema_diagram_target(tab_id) else {
            return;
        };
        self.load_schema_diagram_for(tab_id, target, refresh, cx);
    }

    /// [`Self::request_schema_diagram`] for an explicit target.
    pub(in crate::app) fn load_schema_diagram_for(
        &mut self,
        tab_id: u64,
        target: SchemaDiagramTarget,
        refresh: bool,
        cx: &mut Context<Self>,
    ) {
        let session = self.diagram_session(&target.connection);
        if !refresh
            && self.diagram.schemas.get(&tab_id).is_some_and(|state| {
                state.session == session && state.target == target
            })
        {
            return;
        }
        let generation = self.diagram.schemas.remove(&tab_id).map_or(0, |state| state.sched.generation);
        let epoch = self.diagram.next_epoch();
        let source = self.diagram_source(&target.connection);
        let mut state = SchemaDiagram {
            target,
            session,
            list: TableList::Waiting,
            tables: Vec::new(),
            truncated: None,
            sched: Schedule::new(None, HashMap::new(), (epoch, generation + 1)),
            published: generation + 1,
            last_publish: std::time::Instant::now(),
            publish_timer: false,
            memo: RefCell::new(None),
        };
        match source {
            Ok(source) => state.sched.source = Some(source),
            Err(error) => state.list = TableList::Failed(error.to_string()),
        }
        self.diagram.schemas.insert(tab_id, state);
        self.resolve_schema_tables(tab_id, true, refresh, cx);
        cx.notify();
    }

    /// Takes the schema's table list from the navigator when it has it. With
    /// `load`, asks the navigator's own load path for it otherwise (`reload`
    /// also retries a failed one).
    fn resolve_schema_tables(&mut self, tab_id: u64, load: bool, reload: bool, cx: &mut Context<Self>) {
        use crate::navigator::{object_group_id, NodeLoadState, ObjectGroup};
        let Some(state) = self.diagram.schemas.get(&tab_id) else {
            return;
        };
        if !matches!(state.list, TableList::Waiting) {
            return;
        }
        let target = &state.target;
        let names = match self.scenario_ui.service.as_ref() {
            Some(service) if service.scenario().kind.is_diagram() =>
            {
                Ok(crate::ui_fixture_diagram::table_names())
            }
            _ => {
                let group = object_group_id(
                    &target.connection,
                    &target.database,
                    target.schema.as_deref().unwrap_or(""),
                    ObjectGroup::Tables,
                );
                // A group that is loading again (an object action reloads it in place) still holds the
                // old list: the diagram waits for the new one.
                let reloading = matches!(self.navigator.load_state(&group), Some(NodeLoadState::Loading));
                match self.navigator.loaded_children(&group).filter(|_| !reloading) {
                    Some(children) => Ok(children
                        .iter()
                        .filter_map(|child| match child {
                            NavigatorNodeId::Object { object, .. } => Some(object.clone()),
                            _ => None,
                        })
                        .collect()),
                    None => {
                        if reload && matches!(self.navigator.load_state(&group), Some(NodeLoadState::Failed(_))) {
                            self.navigator.reset_node(&group);
                        }
                        match self.navigator.load_state(&group) {
                            Some(NodeLoadState::Failed(message)) => Err(Some(message.clone())),
                            _ => {
                                if load {
                                    if let Some(request) = self.navigator.begin_background_load(&group) {
                                        self.ensure_connected_then_load(request, cx);
                                    }
                                }
                                Err(None)
                            }
                        }
                    }
                }
            }
        };
        let Some(state) = self.diagram.schemas.get_mut(&tab_id) else {
            return;
        };
        match names {
            Ok(names) => state.set_tables(names),
            Err(Some(message)) => {
                state.list = TableList::Failed(message);
                state.sched.generation += 1;
                state.publish();
            }
            Err(None) => return,
        }
        self.pump_diagram(tab_id, Scope::Schema, cx);
        cx.notify();
    }

    /// A navigator load finished: schema diagrams waiting for their table
    /// list may have it now.
    pub(in crate::app) fn continue_schema_diagrams(&mut self, cx: &mut Context<Self>) {
        let waiting: Vec<u64> = self
            .diagram
            .schemas
            .iter()
            .filter(|(_, state)| matches!(state.list, TableList::Waiting))
            .map(|(tab_id, _)| *tab_id)
            .collect();
        for tab_id in waiting {
            self.resolve_schema_tables(tab_id, false, false, cx);
        }
    }

    /// The route of `connection`, decided on the UI thread without touching
    /// a session lock.
    fn diagram_source(&self, connection: &str) -> Result<Source, DiagramFetchError> {
        #[cfg(test)]
        if let Some(fetch) = &self.diagram.injected {
            return Ok(Source::Injected(Arc::clone(fetch)));
        }
        if let Some(service) = self.scenario_ui.service.as_ref() {
            return Ok(Source::Fixture {
                kind: service.scenario().fixture,
                connection: connection.to_string(),
                chinook: service.scenario().kind.is_diagram(),
            });
        }
        let session = self
            .driver_runtime
            .sessions
            .get(connection)
            .cloned()
            .ok_or(DiagramFetchError::NotConnected)?;
        let driver_id = self
            .saved_connections
            .iter()
            .find(|saved| saved.matches_id(connection))
            .map(|saved| saved.kind.driver_id())
            .unwrap_or_default();
        let describes = self
            .driver_capabilities(connection, driver_id)
            .is_some_and(|capabilities| {
                capabilities.has(engine_driver_api::Capability::MetadataTableProperties)
            });
        Ok(Source::Driver { session, describes })
    }

    /// Starts the queued fetches that fit under `MAX_CONCURRENT`.
    fn pump_diagram(&mut self, tab_id: u64, scope: Scope, cx: &mut Context<Self>) {
        let Some(sched) = self.diagram.schedule_mut(tab_id, scope) else {
            return;
        };
        let (epoch, jobs) = (sched.epoch, sched.take_jobs());
        let Some(source) = sched.source.clone() else {
            return;
        };
        for table in jobs {
            let fetch = source.fetch(&table, cx);
            cx.spawn(async move |this, cx| {
                let result = fetch.await;
                let _ = this.update(cx, |this, cx| {
                    this.finish_diagram_fetch(tab_id, scope, epoch, table, result, cx);
                });
            })
            .detach();
        }
    }

    fn finish_diagram_fetch(
        &mut self,
        tab_id: u64,
        scope: Scope,
        epoch: u64,
        table: TableRef,
        result: Result<TableProperties, DiagramFetchError>,
        cx: &mut Context<Self>,
    ) {
        let result = result
            .map(Arc::new)
            .map_err(|error| crate::sessions::redact_secrets(&error.to_string()));
        let failure = result.as_ref().err().cloned();
        let accepted = match scope {
            Scope::Table => self
                .diagram
                .tabs
                .get_mut(&tab_id)
                .map(|state| state.finish(epoch, table.clone(), result)),
            Scope::Schema => self
                .diagram
                .schemas
                .get_mut(&tab_id)
                .map(|state| state.sched.store(epoch, table.clone(), result)),
        };
        if accepted != Some(true) {
            return;
        }
        if let Some(message) = failure {
            if crate::sessions::is_connection_lost_error(&message) {
                if let Some(sched) = self.diagram.schedule_mut(tab_id, scope) {
                    sched.fail_queued(&message);
                }
            }
            let connection = self.diagram.connection(tab_id, scope).map(str::to_string);
            self.note_connection_lost(tab_id, &message);
            self.report_error(
                Area::Diagram,
                connection.as_deref(),
                format!("Could not load table {} for the diagram: {message}", table.table),
            );
        }
        self.pump_diagram(tab_id, scope, cx);
        match scope {
            Scope::Table => cx.notify(),
            Scope::Schema => self.publish_schema_diagram(tab_id, cx),
        }
    }

    /// Shows what has arrived of a schema diagram: right away when it is done
    /// or the last update is older than `PUBLISH_INTERVAL`, else once the
    /// interval is over (one timer, not a poll).
    fn publish_schema_diagram(&mut self, tab_id: u64, cx: &mut Context<Self>) {
        let Some(state) = self.diagram.schemas.get_mut(&tab_id) else {
            return;
        };
        let age = state.last_publish.elapsed();
        if state.complete() || age >= PUBLISH_INTERVAL {
            state.publish();
            cx.notify();
        } else if !state.publish_timer {
            state.publish_timer = true;
            let (epoch, wait) = (state.sched.epoch, PUBLISH_INTERVAL - age);
            cx.spawn(async move |this, cx| {
                cx.background_executor().timer(wait).await;
                let _ = this.update(cx, |this, cx| {
                    let Some(state) = this.diagram.schemas.get_mut(&tab_id) else {
                        return;
                    };
                    state.publish_timer = false;
                    if state.sched.epoch == epoch {
                        state.publish();
                        cx.notify();
                    }
                });
            })
            .detach();
        }
    }
}
