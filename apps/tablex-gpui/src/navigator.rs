//! Lazy-loading navigator tree state. Pure model, no GPUI types: the shell
//! flattens only expanded branches into visible rows, tracks per-node load
//! state so async loads never double-fire, and lets failures be retried.

use std::collections::{HashMap, HashSet};

use crate::connections::{ConnectionKind, SavedConnection};
use crate::sidebar_preferences::SidebarPreferences;
pub use engine_driver_api::sql::{SchemaObjectKind, SchemaObjectRef};
mod pins;
mod sidebar_view;
pub mod redis_tree;
pub mod schema_columns;
pub mod sql_driver;
pub use sidebar_view::{folder_key, NavigatorAction, SearchOptions, SearchScope};

/// Fully-qualified identity of one node in the navigator tree. Two tables
/// named "users" in different schemas must never collide, so every id carries
/// its full path. Serializable for the remembered tree state; the driver section is never saved.
#[derive(Debug, Clone, PartialEq, Eq, Hash, serde::Serialize, serde::Deserialize)]
pub enum NavigatorNodeId {
    Group {
        id: String,
    },
    /// A section of the "group by database type" tree (render-time only): the driver id of its type.
    #[serde(skip)]
    Driver {
        driver: String,
    },
    /// The Bookmarks section at the top of the tree (render-time only).
    #[serde(skip)]
    BookmarkSection,
    /// One bookmark of that section (render-time only): the id of the bookmark in the preferences.
    #[serde(skip)]
    Bookmark {
        id: String,
    },
    Connection {
        connection: String,
    },
    Database {
        connection: String,
        database: String,
    },
    Schema {
        connection: String,
        database: String,
        schema: String,
    },
    ObjectGroup {
        connection: String,
        database: String,
        schema: String,
        group: ObjectGroup,
    },
    Object {
        connection: String,
        database: String,
        schema: String,
        object: String,
    },
    /// A function, procedure, sequence, trigger, type or event of a schema:
    /// source text instead of columns. `object` is the driver's node id, which is
    /// unique within the group (a function's carries its argument types, a trigger's
    /// its table); what the driver needs to find it again is a
    /// [`SchemaObjectRef`] kept beside the tree (`NavigatorState::schema_object`).
    SchemaObject {
        connection: String,
        database: String,
        schema: String,
        group: ObjectGroup,
        object: String,
    },
    Column {
        connection: String,
        database: String,
        schema: String,
        object: String,
        column: String,
    },
    /// A folder under a table or view (Columns, Indexes, ...): lazily loaded when opened.
    TableFolder {
        connection: String,
        database: String,
        schema: String,
        object: String,
        folder: TableFolder,
    },
    /// An index, constraint, foreign key or trigger of a table. `item` is unique within its
    /// folder (the item's name, suffixed when a driver lists a name twice); what the row shows
    /// beside the name and its definition are kept beside the tree ([`NavigatorState::table_item`]).
    TableItem {
        connection: String,
        database: String,
        schema: String,
        object: String,
        folder: TableFolder,
        item: String,
    },
    Provider {
        connection: String,
        node: String,
    },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, serde::Serialize, serde::Deserialize)]
pub enum ObjectGroup {
    Tables,
    Views,
    MaterializedViews,
    Functions,
    Procedures,
    Sequences,
    Triggers,
    Types,
    Events,
}

impl ObjectGroup {
    pub const ALL: [Self; 9] = [
        Self::Tables,
        Self::Views,
        Self::MaterializedViews,
        Self::Functions,
        Self::Procedures,
        Self::Sequences,
        Self::Triggers,
        Self::Types,
        Self::Events,
    ];

    pub fn title(self) -> &'static str {
        match self {
            Self::Tables => "Tables",
            Self::Views => "Views",
            Self::MaterializedViews => "Materialized Views",
            Self::Functions => "Functions",
            Self::Procedures => "Procedures",
            Self::Sequences => "Sequences",
            Self::Triggers => "Triggers",
            Self::Types => "Types",
            Self::Events => "Events",
        }
    }

    /// The driver's name for what the group lists (`payload.kind` of its
    /// catalog request); `None` for tables and views, which the plain listing
    /// answers.
    pub fn schema_object_kind(self) -> Option<SchemaObjectKind> {
        match self {
            Self::Tables | Self::Views => None,
            Self::MaterializedViews => Some(SchemaObjectKind::MaterializedView),
            Self::Functions => Some(SchemaObjectKind::Function),
            Self::Procedures => Some(SchemaObjectKind::Procedure),
            Self::Sequences => Some(SchemaObjectKind::Sequence),
            Self::Triggers => Some(SchemaObjectKind::Trigger),
            Self::Types => Some(SchemaObjectKind::Type),
            Self::Events => Some(SchemaObjectKind::Event),
        }
    }

    /// Whether the group's objects have columns and data (tables, views,
    /// materialized views) rather than source text.
    pub fn is_table_like(self) -> bool {
        matches!(self, Self::Tables | Self::Views | Self::MaterializedViews)
    }

    /// Whether the group's rows open as data (`SELECT` from them) for a database kind:
    /// the table-like groups, and MariaDB's sequences (a row of its table catalog).
    pub fn has_data(self, kind: Option<ConnectionKind>) -> bool {
        self.is_table_like() || (self == Self::Sequences && kind == Some(ConnectionKind::Mariadb))
    }

    /// The group after this one among those that hold objects with data, for a database kind:
    /// where to look next for a table that is not in this group.
    pub fn next_with_data(self, kind: Option<ConnectionKind>) -> Option<ObjectGroup> {
        Self::for_kind(kind)
            .iter()
            .copied()
            .filter(|group| group.has_data(kind))
            .skip_while(|group| *group != self)
            .nth(1)
    }

    /// The groups a schema shows, in order, for a database kind: tables and
    /// views everywhere, then what the dialect has beside them.
    pub fn for_kind(kind: Option<ConnectionKind>) -> &'static [ObjectGroup] {
        use ObjectGroup::*;
        match kind {
            Some(ConnectionKind::Postgres) => &[
                Tables, Views, MaterializedViews, Functions, Procedures, Sequences, Triggers, Types,
            ],
            Some(ConnectionKind::Mysql) => &[Tables, Views, Functions, Procedures, Triggers, Events],
            Some(ConnectionKind::Mariadb) => {
                &[Tables, Views, Functions, Procedures, Sequences, Triggers, Events]
            }
            Some(ConnectionKind::Sqlite) => &[Tables, Views, Triggers],
            // DuckDB's navigator lists tables and views; it has no triggers or routines to browse.
            _ => &[Tables, Views],
        }
    }
}

/// The folders under a table or view. A folder is always shown where
/// its dialect has the kind (see [`Self::for_kind`]) and, like an [`ObjectGroup`], expands to
/// nothing when it is empty. Everything but Columns is the driver's `describe_object` answer.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, serde::Serialize, serde::Deserialize)]
pub enum TableFolder {
    Columns,
    Indexes,
    /// Primary key, unique and check constraints (SQLite: primary key and unique).
    Constraints,
    ForeignKeys,
    Triggers,
}

impl TableFolder {
    pub fn title(self) -> &'static str {
        match self {
            Self::Columns => "Columns",
            Self::Indexes => "Indexes",
            Self::Constraints => "Constraints",
            Self::ForeignKeys => "Foreign Keys",
            Self::Triggers => "Triggers",
        }
    }

    /// The folders a table (`view`: a view, a materialized view or a sequence, which have
    /// columns only) shows for a database kind, in order. Kinds without them (MongoDB, Redis)
    /// have no SQL tables: nothing.
    pub fn for_kind(kind: Option<ConnectionKind>, view: bool) -> &'static [TableFolder] {
        use TableFolder::*;
        match kind {
            Some(
                ConnectionKind::Postgres
                | ConnectionKind::Mysql
                | ConnectionKind::Mariadb
                | ConnectionKind::Sqlite
                | ConnectionKind::Mssql,
            ) if !view => &[Columns, Indexes, Constraints, ForeignKeys, Triggers],
            // Primary key, unique and check constraints and foreign keys; DuckDB has no triggers.
            Some(ConnectionKind::Duckdb) if !view => &[Columns, Indexes, Constraints, ForeignKeys],
            // Data-skipping indexes; no foreign keys, constraints or triggers to list.
            Some(ConnectionKind::Clickhouse) if !view => &[Columns, Indexes],
            Some(
                ConnectionKind::Postgres
                | ConnectionKind::Mysql
                | ConnectionKind::Mariadb
                | ConnectionKind::Sqlite
                | ConnectionKind::Clickhouse
                | ConnectionKind::Duckdb
                | ConnectionKind::Mssql,
            ) => &[Columns],
            _ => &[],
        }
    }
}

/// What a lazy node is waiting for. Only `Loading` blocks re-dispatch.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum NodeLoadState {
    Loading,
    Loaded,
    Failed(String),
}

/// One visible navigator row: identity, display label, and depth in the tree.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NavigatorRow {
    pub id: NavigatorNodeId,
    pub label: String,
    pub depth: usize,
    /// `Some` when the row is an expandable branch (has lazily-loaded children).
    pub expandable: bool,
    pub expanded: bool,
    pub load: Option<NodeLoadState>,
    /// Persisted connection color/type metadata for connection rows.
    pub disabled: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LoadRequest {
    Databases {
        connection: String,
    },
    Schemas {
        connection: String,
        database: String,
    },
    Objects {
        connection: String,
        database: String,
        schema: String,
        group: ObjectGroup,
    },
    Columns {
        connection: String,
        database: String,
        schema: String,
        object: String,
    },
    /// The Indexes, Constraints, Foreign Keys or Triggers folder of a table (never Columns).
    TableFolder {
        connection: String,
        database: String,
        schema: String,
        object: String,
        folder: TableFolder,
    },
    Provider {
        connection: String,
        parent: Option<String>,
        payload: Option<serde_json::Value>,
        cursor: Option<String>,
        limit: u64,
    },
}

pub const PROVIDER_PAGE_SIZE: u64 = 200;
/// Open nodes kept per connection in the session file.
pub const TREE_STATE_LIMIT: usize = 200;
/// Per-call cap of a server-side filtered object listing.
pub const REMOTE_FILTER_LIMIT: usize = 500;
/// Databases per connection a remote search looks into (the rest are counted, not searched).
pub const SEARCH_DATABASE_LIMIT: usize = 50;
const LOAD_MORE_PREFIX: &str = "__tablex_load_more__:";

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProviderNode {
    pub kind: String,
    pub subtitle: Option<String>,
    pub icon: Option<String>,
    pub has_children: bool,
    pub payload: serde_json::Value,
    pub actions: Vec<String>,
}

/// Result of one remote-search planning step (filtered reloads or restore).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct FilterStep {
    /// Loads the caller must dispatch now.
    pub requests: Vec<LoadRequest>,
    /// Crawl loads currently in flight (including the ones just started).
    pub in_flight: usize,
    /// Unloaded nodes left behind because the budget was exhausted.
    pub remaining: usize,
}

impl FilterStep {
    fn started(&self) -> usize {
        self.requests.len()
    }

    /// Nothing left to load or wait for.
    pub fn finished(&self) -> bool {
        self.in_flight == 0 && self.remaining == 0
    }
}

#[derive(Debug, Default)]
pub struct NavigatorState {
    connections: Vec<SavedConnection>,
    preferences: SidebarPreferences,
    search: SearchOptions,
    matcher: Option<regex::Regex>,
    /// The regex query does not compile: nothing matches.
    regex_invalid: bool,
    visible_filter_matchers: HashMap<String, Vec<regex::Regex>>,
    visible_filter_names: HashMap<String, HashSet<String>>,
    visible_schema_names: HashMap<String, HashMap<String, HashSet<String>>>,
    /// The include / exclude patterns of single folders, compiled (see `SidebarPreferences::folder_filters`).
    folder_filters: HashMap<crate::sidebar_preferences::FolderKey, crate::sidebar_preferences::CompiledFolderFilter>,
    connected_ids: HashSet<String>,
    visible: Vec<NavigatorRow>,
    visible_lookup: HashMap<NavigatorNodeId, usize>,
    connection_lookup: HashMap<String, usize>,
    selected_ids: HashSet<NavigatorNodeId>,
    selection_mode: bool,
    anchor: Option<NavigatorNodeId>,
    expanded: std::collections::HashSet<NavigatorNodeId>,
    collapsed_groups: HashSet<String>,
    /// Settings: connections are listed under a section per database type.
    group_by_driver: bool,
    /// The sections (driver ids) the user collapsed.
    collapsed_drivers: HashSet<String>,
    /// The Bookmarks section is collapsed.
    bookmarks_collapsed: bool,
    /// The open nodes of connections whose tree is not (fully) loaded: what the last session left
    /// open and what a closed session had. Applied step by step as the nodes load, then forgotten.
    remembered_tree: HashMap<String, Vec<NavigatorNodeId>>,
    /// Hash of the open nodes the last rebuild saw, and the count of its changes (the session saves
    /// the tree state when this moves).
    expanded_signature: u64,
    tree_revision: u64,
    search_collapsed: HashSet<NavigatorNodeId>,
    selected: Option<NavigatorNodeId>,
    load_states: HashMap<NavigatorNodeId, NodeLoadState>,
    /// Children keyed by parent id. `None` = not yet loaded.
    children: HashMap<NavigatorNodeId, Option<Vec<NavigatorNodeId>>>,
    /// Labels for non-connection rows, keyed by id.
    labels: HashMap<NavigatorNodeId, String>,
    /// Kinds for object rows (table vs view) so icons and SQL generation match.
    object_kinds: HashMap<NavigatorNodeId, KindOfObject>,
    /// What the driver needs to find each schema object again (its source).
    schema_objects: HashMap<NavigatorNodeId, SchemaObjectRef>,
    /// The object kinds each connected driver advertises (`navigator_kinds`): a schema shows
    /// only the groups its driver can list. A connection without an entry shows its dialect's.
    driver_kinds: HashMap<String, HashSet<String>>,
    /// Connections whose driver cannot describe a table (no `metadata.table_properties`): their
    /// tables show the Columns folder only. A connection without an entry shows every folder.
    no_table_describe: HashSet<String>,
    /// What each index, constraint, foreign key and trigger row shows and offers.
    table_items: HashMap<NavigatorNodeId, TableItemInfo>,
    provider_nodes: HashMap<NavigatorNodeId, ProviderNode>,
    /// Connections that list only the database their address names (connection id -> database).
    only_databases: HashMap<String, String>,
    /// Database nodes the connected user holds no CONNECT privilege on: shown, never opened.
    denied_databases: HashSet<NavigatorNodeId>,
    /// Redis: the key filter and grouping of each database node.
    redis_filters: HashMap<NavigatorNodeId, crate::redis::KeyFilter>,
    provider_connections: HashSet<String>,
    next_cursors: HashMap<NavigatorNodeId, String>,
    load_generations: HashMap<NavigatorNodeId, u64>,
    /// Object groups whose loaded children came from a server-side name
    /// filter (remote search), with that filter.
    search_filters: HashMap<NavigatorNodeId, String>,
    /// Filter attached to an in-flight group load; promoted to
    /// `search_filters` when the load succeeds.
    pending_filters: HashMap<NavigatorNodeId, String>,
    /// Filtered groups whose result hit the per-call limit.
    truncated_groups: HashSet<NavigatorNodeId>,
    /// Remote search: databases of each connection it left out because of
    /// [`SEARCH_DATABASE_LIMIT`].
    unsearched_databases: HashMap<String, usize>,
    /// Databases per connection a remote search looks into; `0` = [`SEARCH_DATABASE_LIMIT`].
    search_database_limit: usize,
    /// Nodes started again by [`Self::reload_children`], with the load generation of that reload:
    /// when that load's answer lands, the rows it no longer lists go. (Any other load - a remote
    /// search narrowing a group included, even one that took the reload's place - keeps the state
    /// of the rows it hides: they come back with their expansion when the search is cleared.)
    reloading: HashMap<NavigatorNodeId, u64>,
    /// Monotonic request token used for per-parent stale-response checks.
    generation: u64,
    /// Bumped whenever the set of loaded catalog nodes (labels, object kinds,
    /// provider nodes) changes; lets the command palette rebuild its index
    /// only when the catalog actually changed (expansion/search do not).
    catalog_revision: u64,
    /// Rows pinned to the top of their siblings, with the order they were pinned in (see
    /// [`pins`]); connections pin through their saved record instead.
    node_pins: HashMap<NavigatorNodeId, u64>,
    /// The connections that have a pinned row.
    pinned_connections: HashSet<String>,
    next_pin: u64,
    /// Settings: system databases are hidden for connections without a
    /// visible-objects filter of their own.
    pub(crate) hide_system_by_default: bool,
    /// Settings: rows per provider page; `0` = [`PROVIDER_PAGE_SIZE`].
    provider_page_size: u64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum KindOfObject {
    Table,
    View,
    MaterializedView,
    /// A MariaDB sequence: a row of the catalog's tables that can be selected from.
    Sequence,
}

impl KindOfObject {
    /// A view or a materialized view: read-only, no keys of its own.
    pub fn is_view(self) -> bool {
        matches!(self, Self::View | Self::MaterializedView)
    }
}

/// What a [`NavigatorNodeId::TableItem`] row says beside its name, and the driver's definition of it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TableItemInfo {
    pub detail: String,
    pub definition: Option<String>,
}

pub struct ExpansionOutcome {
    /// The load the expansion requires, if the node was not already loading.
    pub request: Option<LoadRequest>,
}

/// Saved connection a node belongs to (`None` for sidebar groups).
fn node_connection(id: &NavigatorNodeId) -> Option<&str> {
    match id {
        NavigatorNodeId::Group { .. } | NavigatorNodeId::Driver { .. } | NavigatorNodeId::BookmarkSection | NavigatorNodeId::Bookmark { .. } => None,
        NavigatorNodeId::Connection { connection }
        | NavigatorNodeId::Database { connection, .. }
        | NavigatorNodeId::Schema { connection, .. }
        | NavigatorNodeId::ObjectGroup { connection, .. }
        | NavigatorNodeId::Object { connection, .. }
        | NavigatorNodeId::SchemaObject { connection, .. }
        | NavigatorNodeId::Column { connection, .. }
        | NavigatorNodeId::TableFolder { connection, .. }
        | NavigatorNodeId::TableItem { connection, .. }
        | NavigatorNodeId::Provider { connection, .. } => Some(connection),
    }
}

impl NavigatorState {
    /// Rows per provider page (the "navigator page size" setting).
    pub(crate) fn provider_page_size(&self) -> u64 {
        if self.provider_page_size == 0 {
            PROVIDER_PAGE_SIZE
        } else {
            self.provider_page_size
        }
    }

    /// Settings: list the connections under a section per database type (user groups stay inside).
    pub fn set_group_by_driver(&mut self, on: bool) {
        if self.group_by_driver != on {
            self.group_by_driver = on;
            self.rebuild_visible();
        }
    }

    /// The open nodes of `connection`'s loaded tree, parents before children, at most
    /// [`TREE_STATE_LIMIT`] of them.
    fn expanded_nodes(&self, connection: &str) -> Vec<NavigatorNodeId> {
        let root = NavigatorNodeId::Connection {
            connection: connection.to_string(),
        };
        let mut found = Vec::new();
        let mut stack = vec![&root];
        while let Some(id) = stack.pop() {
            if found.len() >= TREE_STATE_LIMIT {
                break;
            }
            if self.expanded.contains(id) {
                found.push(id.clone());
            }
            if let Some(Some(children)) = self.children.get(id) {
                stack.extend(children.iter().rev());
            }
        }
        found
    }

    /// Keeps what is open under `connection` to open it again when the connection reloads (its
    /// tree is about to be reset: a disconnect, a reconnect). Nothing open keeps what was kept.
    pub fn remember_tree(&mut self, connection: &str) {
        let nodes = self.expanded_nodes(connection);
        if !nodes.is_empty() {
            self.remembered_tree.insert(connection.to_string(), nodes);
            self.tree_revision += 1;
        }
    }

    /// What the last session had open (replaces what was remembered). Connections that no longer
    /// exist and nodes of other connections are ignored.
    pub fn set_remembered_tree(&mut self, trees: Vec<(String, Vec<NavigatorNodeId>)>) {
        self.remembered_tree = trees
            .into_iter()
            .filter(|(connection, _)| self.connection(connection).is_some())
            .map(|(connection, nodes)| {
                let nodes = nodes
                    .into_iter()
                    .filter(|node| node_connection(node) == Some(connection.as_str()))
                    .take(TREE_STATE_LIMIT)
                    .collect::<Vec<_>>();
                (connection, nodes)
            })
            .filter(|(_, nodes)| !nodes.is_empty())
            .collect();
        self.tree_revision += 1;
    }

    /// The open nodes of every connection (its loaded tree, plus what is still waiting to open),
    /// in saved-connection order: what the session file keeps.
    pub fn tree_snapshot(&self) -> Vec<(String, Vec<NavigatorNodeId>)> {
        self.connections
            .iter()
            .filter_map(|saved| {
                let id = saved.id.to_string();
                let mut nodes = self.expanded_nodes(&id);
                for node in self.remembered_tree.get(&id).into_iter().flatten() {
                    if !nodes.contains(node) {
                        nodes.push(node.clone());
                    }
                }
                nodes.truncate(TREE_STATE_LIMIT);
                (!nodes.is_empty()).then_some((id, nodes))
            })
            .collect()
    }

    /// Moves whenever the open nodes change (the session saves the tree state then).
    pub fn tree_revision(&self) -> u64 {
        self.tree_revision
    }

    /// Whether the node is open (its children, when loaded, are listed under it).
    pub fn is_expanded(&self, id: &NavigatorNodeId) -> bool {
        self.expanded.contains(id)
    }

    /// The ids of the connections in the group `group` or a group below it.
    pub fn group_member_ids(&self, group: &str) -> Vec<String> {
        let prefs = &self.preferences;
        self.connections
            .iter()
            .filter_map(|saved| {
                let id = saved.id.to_string();
                let mut parent = prefs
                    .connections
                    .iter()
                    .find(|item| item.id == id)
                    .and_then(|item| item.group.as_deref());
                while let Some(current) = parent {
                    if current == group {
                        return Some(id);
                    }
                    parent = prefs
                        .groups
                        .iter()
                        .find(|item| item.id == current)
                        .and_then(|item| item.parent.as_deref());
                }
                None
            })
            .collect()
    }

    /// Whether remembered nodes of `connection` still wait to open.
    pub fn has_remembered_tree(&self, connection: &str) -> bool {
        self.remembered_tree.contains_key(connection)
    }

    /// Opens the remembered nodes of `connection` that exist now, parents first, and returns the
    /// loads that opening them needs. A node that is not loaded yet waits for the next call while
    /// anything of the connection is still loading; once nothing is, the rest is gone from the
    /// server and is forgotten. Never connects: it only walks what the tree holds.
    pub fn resume_tree(&mut self, connection: &str) -> Vec<LoadRequest> {
        let mut requests = Vec::new();
        if self.search_active() {
            return requests;
        }
        let Some(mut pending) = self.remembered_tree.remove(connection) else {
            return requests;
        };
        self.tree_revision += 1;
        loop {
            let mut progressed = false;
            let mut waiting = Vec::new();
            for id in pending {
                if self.expanded.contains(&id) {
                    progressed = true;
                    continue;
                }
                let exists = match &id {
                    NavigatorNodeId::Connection { connection } => self.connection(connection).is_some(),
                    other => self.labels.contains_key(other),
                };
                if !exists {
                    waiting.push(id);
                    continue;
                }
                progressed = true;
                requests.extend(self.toggle_expansion(&id).request);
            }
            pending = waiting;
            if !progressed || pending.is_empty() {
                break;
            }
        }
        let loading = self.load_states.iter().any(|(id, state)| {
            *state == NodeLoadState::Loading && node_connection(id) == Some(connection)
        });
        if loading && !pending.is_empty() {
            self.remembered_tree.insert(connection.to_string(), pending);
        }
        requests
    }

    /// Applies the navigation settings; the tree is rebuilt only when the
    /// system-object default changed.
    pub fn set_settings_defaults(&mut self, hide_system: bool, page_size: u64) {
        self.provider_page_size = page_size;
        if self.hide_system_by_default != hide_system {
            self.hide_system_by_default = hide_system;
            self.catalog_revision += 1;
            self.rebuild_visible();
        }
    }

    pub fn new(connections: Vec<SavedConnection>) -> Self {
        let mut state = Self {
            connections,
            ..Default::default()
        };
        state.rebuild();
        state
    }

    /// Applies a changed saved-connection list (add / edit / delete) without
    /// disturbing connections that were not affected: their expansion, loaded
    /// children, labels, cursors and selection all survive.
    ///
    /// * removed connection: every piece of state under it is dropped;
    /// * edited connection whose endpoint/credentials (`uri`) or database type
    ///   changed: only that connection's subtree is reset (it must reconnect
    ///   and reload); label-only edits (rename, pin, ...) keep everything;
    /// * new connection: starts collapsed.
    ///
    /// If the selected node lived under a removed connection the nearest
    /// surviving sibling is selected (or nothing); under a reset one, the
    /// connection row itself.
    pub fn set_connections(&mut self, connections: Vec<SavedConnection>) {
        let affected: Vec<(String, bool)> = self
            .connections
            .iter()
            .filter_map(|old| {
                let id = old.id.to_string();
                match connections.iter().find(|new| new.id == old.id) {
                    None => Some((id, true)),
                    Some(new) if new.uri != old.uri || new.kind != old.kind => Some((id, false)),
                    Some(_) => None,
                }
            })
            .collect();
        // Old order is only needed to pick a replacement selection.
        let old_selected = self.selected.clone().filter(|selected| {
            node_connection(selected)
                .is_some_and(|connection| affected.iter().any(|(id, _)| id == connection))
        });
        let old_rows = old_selected.is_some().then(|| self.visible.clone());

        self.connections = connections;
        self.provider_connections.retain(|connection| {
            self.connections
                .iter()
                .any(|saved| saved.matches_id(connection))
        });
        for (connection, _) in &affected {
            self.purge_connection_state(connection);
        }
        self.connection_lookup = self
            .connections
            .iter()
            .enumerate()
            .map(|(index, connection)| (connection.id.to_string(), index))
            .collect();
        // Cached rows must notice the change; per-node load tokens of the
        // untouched connections stay valid so their in-flight loads land.
        self.generation += 1;
        self.rebuild_visible();

        let (Some(old_selected), Some(old_rows)) = (old_selected, old_rows) else {
            return;
        };
        if self.selected.is_some() {
            return;
        }
        let Some(connection) = node_connection(&old_selected) else {
            return;
        };
        let removed = affected
            .iter()
            .any(|(id, removed)| id == connection && *removed);
        let replacement = if removed {
            let first = old_rows
                .iter()
                .position(|row| node_connection(&row.id) == Some(connection));
            let last = old_rows
                .iter()
                .rposition(|row| node_connection(&row.id) == Some(connection));
            match (first, last) {
                (Some(first), Some(last)) => {
                    let depth = old_rows[first].depth;
                    old_rows[last + 1..]
                        .iter()
                        .find(|row| row.depth <= depth && self.visible_lookup.contains_key(&row.id))
                        .or_else(|| {
                            old_rows[..first].iter().rev().find(|row| {
                                row.depth <= depth && self.visible_lookup.contains_key(&row.id)
                            })
                        })
                        .map(|row| row.id.clone())
                }
                _ => None,
            }
        } else {
            let id = NavigatorNodeId::Connection {
                connection: connection.to_string(),
            };
            self.visible_lookup.contains_key(&id).then_some(id)
        };
        if let Some(id) = replacement {
            self.select(id);
        }
    }

    /// Drops every piece of per-node state under `connection` (including the
    /// connection row's own expansion and load state).
    fn purge_connection_state(&mut self, connection: &str) {
        self.remembered_tree.remove(connection);
        let under = |id: &NavigatorNodeId| node_connection(id) == Some(connection);
        self.catalog_revision += 1;
        self.expanded.retain(|id| !under(id));
        self.search_collapsed.retain(|id| !under(id));
        self.load_states.retain(|id, _| !under(id));
        self.children.retain(|id, _| !under(id));
        self.labels.retain(|id, _| !under(id));
        self.object_kinds.retain(|id, _| !under(id));
        self.schema_objects.retain(|id, _| !under(id));
        self.table_items.retain(|id, _| !under(id));
        self.driver_kinds.remove(connection);
        self.no_table_describe.remove(connection);
        self.provider_nodes.retain(|id, _| !under(id));
        self.next_cursors.retain(|id, _| !under(id));
        self.load_generations.retain(|id, _| !under(id));
        self.search_filters.retain(|id, _| !under(id));
        self.pending_filters.retain(|id, _| !under(id));
        self.truncated_groups.retain(|id| !under(id));
        self.reloading.retain(|id, _| !under(id));
        self.selected_ids.retain(|id| !under(id));
        if self.anchor.as_ref().is_some_and(under) {
            self.anchor = None;
        }
        if self.selected.as_ref().is_some_and(under) {
            self.selected = None;
        }
    }

    /// The database each connection lists alone (`Show only the connection's database`); a
    /// connection that is not in the map lists every database.
    pub fn set_database_scopes(&mut self, scopes: HashMap<String, String>) {
        if self.only_databases != scopes {
            self.only_databases = scopes;
            self.catalog_revision += 1;
            self.rebuild_visible();
        }
    }

    /// The databases loaded under `connection` that it lists and the user may open, in load order
    /// (the one a connection-level action starts in, and the ones a picker offers).
    pub fn openable_databases(&self, connection: &str) -> impl Iterator<Item = &str> {
        let root = NavigatorNodeId::Connection { connection: connection.to_owned() };
        self.children
            .get(&root)
            .into_iter()
            .flatten()
            .flatten()
            .filter(|id| !self.scoped_out(id) && !self.denied_databases.contains(*id))
            .filter_map(|id| match id {
                NavigatorNodeId::Database { database, .. } => Some(database.as_str()),
                _ => None,
            })
    }

    /// A database node the connected user may not connect to (the driver said so).
    pub fn database_denied(&self, id: &NavigatorNodeId) -> bool {
        self.denied_databases.contains(id)
    }

    #[cfg(test)]
    pub fn connections(&self) -> &[SavedConnection] {
        &self.connections
    }

    /// Records the `navigator_kinds` the driver of `connection` advertises (`None` forgets
    /// them). Schemas expanded afterwards show only the groups it can list: an older driver
    /// that does not know functions or triggers would answer them with a listing of every
    /// table, which is thrown away.
    pub fn set_driver_kinds(&mut self, connection: &str, kinds: Option<Vec<String>>) {
        match kinds.filter(|kinds| !kinds.is_empty()) {
            Some(kinds) => {
                self.driver_kinds.insert(connection.to_string(), kinds.into_iter().collect());
            }
            None => {
                self.driver_kinds.remove(connection);
            }
        }
    }

    /// Records whether the driver of `connection` can describe a table (the source of the Indexes,
    /// Constraints, Foreign Keys and Triggers folders). Tables opened afterwards show those
    /// folders only when it can.
    pub fn set_table_describe(&mut self, connection: &str, describes: bool) {
        if describes {
            self.no_table_describe.remove(connection);
        } else {
            self.no_table_describe.insert(connection.to_string());
        }
    }

    /// The folders a table of `connection` shows now.
    fn table_folders(&self, connection: &str, object: &NavigatorNodeId) -> &'static [TableFolder] {
        let kind = self.connection(connection).map(|saved| saved.kind);
        let view = self
            .object_kinds
            .get(object)
            .is_some_and(|kind| *kind != KindOfObject::Table);
        let folders = TableFolder::for_kind(kind, view);
        if self.no_table_describe.contains(connection) {
            &folders[..folders.len().min(1)]
        } else {
            folders
        }
    }

    /// A table lists its folders at once (no load); each folder loads when it is opened.
    fn open_object(&mut self, id: &NavigatorNodeId) {
        let NavigatorNodeId::Object { connection, .. } = id else {
            return;
        };
        let folders: Vec<NavigatorNodeId> = self
            .table_folders(connection, id)
            .iter()
            .filter_map(|folder| id.table_folder(*folder))
            .collect();
        for (folder, node) in self.table_folders(connection, id).iter().zip(&folders) {
            self.labels.insert(node.clone(), folder.title().to_string());
        }
        self.children.insert(id.clone(), Some(folders));
        self.load_states.insert(id.clone(), NodeLoadState::Loaded);
    }

    /// Whether a schema of `connection` shows `group`: tables and views always, the rest when
    /// the driver advertises its kind (or has not been asked).
    fn driver_lists(&self, connection: &str, group: ObjectGroup) -> bool {
        let Some(kind) = group.schema_object_kind().filter(|_| group != ObjectGroup::Views) else {
            return true;
        };
        self.driver_kinds
            .get(connection)
            .is_none_or(|kinds| kinds.contains(kind.as_str()))
    }

    pub fn selected(&self) -> Option<&NavigatorNodeId> {
        self.selected.as_ref()
    }

    pub fn select(&mut self, id: NavigatorNodeId) {
        if self.selected.as_ref() != Some(&id) {
            self.selection_mode = false;
            self.selected_ids.clear();
            self.selected_ids.insert(id.clone());
            self.anchor = Some(id.clone());
            self.selected = Some(id);
        }
    }

    pub fn generation(&self) -> u64 {
        self.generation
    }

    /// Changes whenever the loaded catalog nodes change (see the field docs).
    pub fn catalog_revision(&self) -> u64 {
        self.catalog_revision
    }

    /// Visits every loaded catalog node below a saved connection in tree
    /// order (connections in saved order, children in load order): SQL
    /// databases / schemas / tables / views and provider (Mongo) databases and
    /// collections. Columns, object-group folders, load-more placeholders and
    /// saved-pipeline folders are skipped. Pure in-memory; never loads.
    /// With `routines`, the loaded functions and procedures of the schemas too (what "Go to
    /// object" searches).
    pub fn visit_catalog_with<'a>(
        &'a self,
        routines: bool,
        mut visit: impl FnMut(&'a NavigatorNodeId, &'a str, Option<KindOfObject>, Option<&'a ProviderNode>),
    ) {
        let mut stack: Vec<&NavigatorNodeId> = Vec::new();
        let roots: Vec<NavigatorNodeId> = self
            .connections
            .iter()
            .map(|saved| NavigatorNodeId::Connection {
                connection: saved.id.to_string(),
            })
            .collect();
        for root in &roots {
            let Some(Some(children)) = self.children.get(root) else {
                continue;
            };
            stack.extend(children.iter().rev());
            while let Some(id) = stack.pop() {
                if self.scoped_out(id) {
                    continue;
                }
                let label = self.labels.get(id).map(String::as_str);
                match id {
                    NavigatorNodeId::Database { .. }
                    | NavigatorNodeId::Schema { .. }
                    | NavigatorNodeId::Object { .. } => {
                        if let Some(label) = label {
                            visit(id, label, self.object_kinds.get(id).copied(), None);
                        }
                    }
                    NavigatorNodeId::SchemaObject { group, .. }
                        if routines && matches!(group, ObjectGroup::Functions | ObjectGroup::Procedures) =>
                    {
                        if let Some(label) = label {
                            visit(id, label, None, None);
                        }
                    }
                    NavigatorNodeId::Provider { node, .. } => {
                        let Some(provider) = self.provider_nodes.get(id) else {
                            continue;
                        };
                        if node.starts_with(LOAD_MORE_PREFIX)
                            || provider.payload.get("group").is_some()
                            || !matches!(provider.kind.as_str(), "database" | "collection")
                        {
                            continue;
                        }
                        if let Some(label) = label {
                            visit(id, label, None, Some(provider));
                        }
                    }
                    _ => {}
                }
                if !matches!(id, NavigatorNodeId::Object { .. } | NavigatorNodeId::Column { .. }) {
                    if let Some(Some(children)) = self.children.get(id) {
                        stack.extend(children.iter().rev());
                    }
                }
            }
        }
    }

    pub fn load_generation(&self, request: &LoadRequest) -> Option<u64> {
        self.load_generations.get(&request.parent_id()).copied()
    }

    /// The generation of the last load of the children of `id` (it changes when they load again).
    pub fn node_load_generation(&self, id: &NavigatorNodeId) -> Option<u64> {
        self.load_generations.get(id).copied()
    }

    /// Flattens expanded branches into the rows the list should render.
    pub fn visible_rows(&self) -> Vec<NavigatorRow> {
        self.visible.clone()
    }

    /// One visible row by index, without materializing the whole vector.
    pub fn visible_row_at(&self, index: usize) -> Option<NavigatorRow> {
        self.visible.get(index).cloned()
    }

    /// Expands or collapses a node. Expanding an unloaded node returns the
    /// load to dispatch; re-expanding a `Loading` node is a no-op.
    pub fn toggle_expansion(&mut self, id: &NavigatorNodeId) -> ExpansionOutcome {
        if self.search_active()
            && self
                .visible
                .iter()
                .any(|row| &row.id == id && (row.expanded || self.search_collapsed.contains(id)))
        {
            if !self.search_collapsed.insert(id.clone()) {
                self.search_collapsed.remove(id);
            }
            self.rebuild_visible();
            return ExpansionOutcome {
                request: None,
            };
        }
        if matches!(id, NavigatorNodeId::BookmarkSection) {
            self.bookmarks_collapsed = !self.bookmarks_collapsed;
            self.rebuild_visible();
            return ExpansionOutcome { request: None };
        }
        if let NavigatorNodeId::Driver { driver } = id {
            if !self.collapsed_drivers.insert(driver.clone()) {
                self.collapsed_drivers.remove(driver);
            }
            self.rebuild_visible();
            return ExpansionOutcome {
                request: None,
            };
        }
        if let NavigatorNodeId::Group { id } = id {
            if self.preferences.groups.iter().any(|group| group.id == *id) {
                if !self.collapsed_groups.insert(id.clone()) {
                    self.collapsed_groups.remove(id);
                }
                self.rebuild_visible();
            }
            return ExpansionOutcome {
                request: None,
            };
        }
        // A database the user may not connect to never opens: the load could only fail.
        if self.denied_databases.contains(id) {
            return ExpansionOutcome {
                request: None,
            };
        }
        let failed = matches!(self.load_states.get(id), Some(NodeLoadState::Failed(_)));
        if failed {
            self.reset_node(id);
        }
        if self.expanded.contains(id) && !failed {
            self.expanded.remove(id);
            self.rebuild_visible();
            return ExpansionOutcome {
                request: None,
            };
        }
        if !self.is_expandable(id) {
            return ExpansionOutcome {
                request: None,
            };
        }
        let already_loading = matches!(
            self.load_states.get(id),
            Some(NodeLoadState::Loading) | Some(NodeLoadState::Loaded)
        );
        self.expanded.insert(id.clone());

        if matches!(id, NavigatorNodeId::Schema { .. }) {
            self.open_schema(id);
            self.rebuild_visible();
            return ExpansionOutcome {
                request: None,
            };
        }
        if matches!(id, NavigatorNodeId::Object { .. }) {
            self.open_object(id);
            self.rebuild_visible();
            return ExpansionOutcome {
                request: None,
            };
        }

        if already_loading {
            self.rebuild_visible();
            return ExpansionOutcome {
                request: None,
            };
        }
        self.load_states.insert(id.clone(), NodeLoadState::Loading);
        self.generation += 1;
        self.load_generations.insert(id.clone(), self.generation);
        let request = self
            .provider_load_request(id, None)
            .or_else(|| id.load_request());
        self.rebuild_visible();
        ExpansionOutcome {
            request,
        }
    }

    /// A schema lists the object groups its driver can list, at once (no load).
    fn open_schema(&mut self, id: &NavigatorNodeId) {
        let NavigatorNodeId::Schema {
            connection,
            database,
            schema,
        } = id
        else {
            return;
        };
        let kind = self.connection(connection).map(|saved| saved.kind);
        let groups: Vec<LoadedChild> = ObjectGroup::for_kind(kind)
            .iter()
            .filter(|group| self.driver_lists(connection, **group))
            .map(|group| object_group_child(connection, database, schema, *group))
            .collect();
        let child_ids = groups.iter().map(|child| child.id.clone()).collect();
        for child in groups {
            self.labels.insert(child.id.clone(), child.label);
        }
        self.children.insert(id.clone(), Some(child_ids));
        self.load_states.insert(id.clone(), NodeLoadState::Loaded);
    }

    /// Applies a load result. `generation` guards against applying a result
    /// after the tree was reset; per-node `Loading` state guards against
    /// applying after a retry was dispatched.
    pub fn apply_load(
        &mut self,
        request: &LoadRequest,
        generation: u64,
        result: Result<Vec<LoadedChild>, String>,
    ) {
        let parent = request.parent_id();
        if self.load_generations.get(&parent).copied() != Some(generation) {
            return;
        }
        let expected_state = self
            .load_states
            .get(&parent)
            .map(|state| matches!(state, NodeLoadState::Loading))
            .unwrap_or(false);
        if !expected_state {
            return;
        }
        match result {
            Ok(children) => {
                match self.pending_filters.remove(&parent) {
                    Some(filter) => {
                        if children.len() >= REMOTE_FILTER_LIMIT {
                            self.truncated_groups.insert(parent.clone());
                        } else {
                            self.truncated_groups.remove(&parent);
                        }
                        self.search_filters.insert(parent.clone(), filter);
                    }
                    None => {
                        self.search_filters.remove(&parent);
                        self.truncated_groups.remove(&parent);
                    }
                }
                let child_ids: Vec<NavigatorNodeId> =
                    children.iter().map(|child| child.id.clone()).collect();
                self.catalog_revision += 1;
                // A reload: what is not listed any more goes with everything below it.
                let reloaded = self.reloading.remove(&parent) == Some(generation);
                if let Some(Some(old)) = self.children.get(&parent).filter(|_| reloaded) {
                    let kept: HashSet<&NavigatorNodeId> = child_ids.iter().collect();
                    let gone: Vec<NavigatorNodeId> =
                        old.iter().filter(|id| !kept.contains(id)).cloned().collect();
                    for id in gone {
                        self.forget_subtree(&id);
                    }
                }
                for child in children {
                    if child.no_connect {
                        self.denied_databases.insert(child.id.clone());
                    } else if !self.denied_databases.is_empty() {
                        self.denied_databases.remove(&child.id);
                    }
                    self.labels.insert(child.id.clone(), child.label);
                    if let Some(kind) = child.object_kind {
                        self.object_kinds.insert(child.id.clone(), kind);
                    }
                    if let Some(item) = child.item {
                        self.table_items.insert(child.id.clone(), item);
                    }
                    if let Some(reference) = child.schema_object {
                        self.schema_objects.insert(child.id, reference);
                    }
                }
                self.children.insert(parent.clone(), Some(child_ids));
                self.load_states
                    .insert(parent.clone(), NodeLoadState::Loaded);
            }
            Err(error) => {
                self.reloading.remove(&parent);
                self.pending_filters.remove(&parent);
                self.children.insert(parent.clone(), None);
                self.load_states
                    .insert(parent.clone(), NodeLoadState::Failed(error));
            }
        }
        self.rebuild_visible();
    }

    /// A driver session of `connection` is live: its tree starts over. Driver
    /// connections that are not SQL list provider nodes (MongoDB); a SQL
    /// driver fills the typed tree (see [`sql_driver`]).
    pub fn enable_provider(&mut self, connection: &str) {
        if !self
            .connection(connection)
            .is_some_and(|saved| sql_driver::is_sql_kind(saved.kind))
        {
            self.provider_connections.insert(connection.to_string());
        }
        self.reset_node(&NavigatorNodeId::Connection {
            connection: connection.to_string(),
        });
        self.rebuild_visible();
    }

    pub fn load_more(&mut self, parent: &NavigatorNodeId) -> Option<LoadRequest> {
        if matches!(self.load_states.get(parent), Some(NodeLoadState::Loading)) {
            return None;
        }
        let cursor = self.next_cursors.get(parent)?.clone();
        let request = self.provider_load_request(parent, Some(cursor))?;
        self.load_states
            .insert(parent.clone(), NodeLoadState::Loading);
        self.generation += 1;
        self.load_generations
            .insert(parent.clone(), self.generation);
        // Only the load markers of the parent and its `Load More` row change:
        // flattening a 100k-key list again to show them would cost O(rows).
        if !self.refresh_load_markers(parent) {
            self.rebuild_visible();
        }
        Some(request)
    }

    /// Copies the load state of `parent` onto its row and its `Load More` row.
    /// `false` when the rows are not both on screen as plain rows (the caller
    /// then flattens the tree).
    fn refresh_load_markers(&mut self, parent: &NavigatorNodeId) -> bool {
        if self.search_active() || !self.is_redis_database(parent) {
            return false;
        }
        let marker = self.load_more_id(parent);
        let (Some(&parent_at), Some(&marker_at)) =
            (self.visible_lookup.get(parent), self.visible_lookup.get(&marker))
        else {
            return false;
        };
        let state = self.load_states.get(parent).cloned();
        self.visible[parent_at].load = state.clone();
        self.visible[marker_at].load = state;
        true
    }

    /// Whether `id` is a "Load More" placeholder row (its label is drawn in the interface language).
    pub fn is_load_more(id: &NavigatorNodeId) -> bool {
        matches!(id, NavigatorNodeId::Provider { node, .. } if node.starts_with(LOAD_MORE_PREFIX))
    }

    pub fn load_more_parent(&self, id: &NavigatorNodeId) -> Option<NavigatorNodeId> {
        let NavigatorNodeId::Provider { connection, node } = id else {
            return None;
        };
        let node = node.strip_prefix(LOAD_MORE_PREFIX)?;
        if node == "root" {
            return Some(NavigatorNodeId::Connection {
                connection: connection.clone(),
            });
        }
        Some(NavigatorNodeId::Provider {
            connection: connection.clone(),
            node: node.into(),
        })
    }

    pub fn apply_provider_page(
        &mut self,
        request: &LoadRequest,
        generation: u64,
        result: Result<engine_driver_api::NavigatorPage, String>,
    ) {
        let parent = request.parent_id();
        if self.load_generations.get(&parent).copied() != Some(generation) {
            return;
        }
        if !matches!(self.load_states.get(&parent), Some(NodeLoadState::Loading)) {
            return;
        }
        if let LoadRequest::Provider {
            cursor: Some(cursor),
            ..
        } = request
        {
            if self.next_cursors.get(&parent) != Some(cursor) {
                return;
            }
        }
        match result {
            Ok(page) => {
                let append = matches!(
                    request,
                    LoadRequest::Provider {
                        cursor: Some(_),
                        ..
                    }
                );
                let children = page
                    .nodes
                    .into_iter()
                    .map(|node| provider_child(&request.connection_id(), node))
                    .collect();
                let added = self.store_provider_children(&parent, children, append);
                if let Some(cursor) = page.next_cursor {
                    self.next_cursors.insert(parent.clone(), cursor);
                } else {
                    self.next_cursors.remove(&parent);
                }
                self.load_states.insert(parent.clone(), NodeLoadState::Loaded);
                // A page of keys appended to a long flat list adds rows at its
                // end: splice them in instead of flattening the whole tree again
                // (that is O(all rows) per page).
                if append && added.is_some_and(|added| self.append_visible_rows(&parent, &added)) {
                    return;
                }
            }
            Err(error) => {
                self.load_states
                    .insert(parent, NodeLoadState::Failed(error));
            }
        }
        self.rebuild_visible();
    }

    pub fn actions(&self, id: &NavigatorNodeId) -> &[String] {
        self.provider_nodes
            .get(id)
            .map(|node| node.actions.as_slice())
            .unwrap_or_default()
    }

    pub fn provider_node(&self, id: &NavigatorNodeId) -> Option<&ProviderNode> {
        self.provider_nodes.get(id)
    }

    /// Loaded provider nodes of `connection` that are collections of
    /// `database` (payload carries both `database` and `collection`, and no
    /// `group` marker), with the collection name.
    pub fn provider_collection_nodes(
        &self,
        connection: &str,
        database: &str,
    ) -> Vec<(NavigatorNodeId, String)> {
        self.provider_nodes
            .iter()
            .filter_map(|(id, node)| {
                let NavigatorNodeId::Provider {
                    connection: owner, ..
                } = id
                else {
                    return None;
                };
                if owner != connection
                    || node.kind != "collection"
                    || node.payload.get("database").and_then(|value| value.as_str())
                        != Some(database)
                    || node.payload.get("group").is_some()
                {
                    return None;
                }
                let collection = node.payload.get("collection")?.as_str()?;
                Some((id.clone(), collection.to_string()))
            })
            .collect()
    }

    /// Children of `id` as loaded so far (`None` while never loaded). Read-only
    /// view of the cache for editor completion.
    pub fn loaded_children(&self, id: &NavigatorNodeId) -> Option<&[NavigatorNodeId]> {
        self.children.get(id)?.as_deref()
    }

    /// The object groups of `connection` that are loaded (or loading) and that `wanted` accepts: what a
    /// soft refresh may reload without loading anything that was not listed yet. Only those are cloned.
    pub fn loaded_object_groups(&self, connection: &str, mut wanted: impl FnMut(&NavigatorNodeId) -> bool) -> Vec<NavigatorNodeId> {
        self.load_states
            .iter()
            .filter(|(id, state)| {
                matches!(id, NavigatorNodeId::ObjectGroup { connection: owner, .. } if owner == connection)
                    && matches!(state, NodeLoadState::Loaded | NodeLoadState::Loading)
                    && wanted(id)
            })
            .map(|(id, _)| id.clone())
            .collect()
    }

    /// Where the lazy load of `id` stands (`None`: never requested).
    pub fn load_state(&self, id: &NavigatorNodeId) -> Option<&NodeLoadState> {
        self.load_states.get(id)
    }

    /// Starts loading the children of `id` without expanding it (editor
    /// completion warming a table's columns); the result lands through the
    /// normal `apply_load` / `apply_provider_page` path. `None` when the node
    /// is already loading, loaded or failed, or has nothing to load.
    pub fn begin_background_load(&mut self, id: &NavigatorNodeId) -> Option<LoadRequest> {
        if self.load_states.contains_key(id) {
            return None;
        }
        let request = self.provider_load_request(id, None).or_else(|| id.load_request())?;
        self.load_states.insert(id.clone(), NodeLoadState::Loading);
        self.generation += 1;
        self.load_generations.insert(id.clone(), self.generation);
        Some(request)
    }

    /// Loads the children of `id` again where they are: what is shown stays, with its expansion
    /// and selection, until the answer replaces the list (rows that are gone leave, new ones
    /// arrive). The way an object action refreshes the one group it changed instead of resetting
    /// (and collapsing) it. A node whose children were never loaded returns `None`: its first
    /// expansion loads them.
    pub fn reload_children(&mut self, id: &NavigatorNodeId) -> Option<LoadRequest> {
        let request = self.begin_reload(id)?;
        self.rebuild_visible();
        Some(request)
    }

    /// [`Self::reload_children`] for several nodes: the sidebar is rebuilt once, not once per node.
    pub fn reload_children_of(&mut self, ids: &[NavigatorNodeId]) -> Vec<LoadRequest> {
        let requests: Vec<LoadRequest> = ids.iter().filter_map(|id| self.begin_reload(id)).collect();
        if !requests.is_empty() {
            self.rebuild_visible();
        }
        requests
    }

    fn begin_reload(&mut self, id: &NavigatorNodeId) -> Option<LoadRequest> {
        if !matches!(self.load_states.get(id), Some(NodeLoadState::Loaded | NodeLoadState::Loading)) {
            return None;
        }
        let request = self.provider_load_request(id, None).or_else(|| id.load_request())?;
        // A load in flight is superseded: its answer carries an older generation. The group keeps
        // the remote-search filter it is about to show (the search load in flight, else the one it
        // shows), so the reload lists what the user is looking at.
        let filter = self
            .pending_filters
            .get(id)
            .or_else(|| self.search_filters.get(id))
            .filter(|filter| !filter.is_empty())
            .cloned();
        self.begin_group_load(id, filter.as_deref());
        self.reloading.insert(id.clone(), self.generation);
        Some(request)
    }

    /// Records that the children of `id` cannot be loaded and nothing is loading them (a request that
    /// could not be made): the node shows the failure, and a retry starts over. A defensive
    /// safeguard: every object group has a load request today, so [`Self::begin_background_load`]
    /// returns `None` for one only while it is loading, loaded or failed, never for lack of a request.
    pub fn fail_load(&mut self, id: &NavigatorNodeId, error: String) {
        self.reloading.remove(id);
        self.children.insert(id.clone(), None);
        self.load_states.insert(id.clone(), NodeLoadState::Failed(error));
        self.rebuild_visible();
    }

    /// Whether a provider node's children were loaded (and are shown when it
    /// is expanded).
    pub fn has_loaded_children(&self, id: &NavigatorNodeId) -> bool {
        matches!(self.children.get(id), Some(Some(_)))
    }

    /// Replaces the loaded children of `parent` in place: surviving children
    /// keep their expansion and loaded subtree, vanished ones (and their
    /// subtrees) are removed. Unlike `reset_node` nothing collapses.
    pub fn replace_provider_children(
        &mut self,
        parent: &NavigatorNodeId,
        nodes: Vec<engine_driver_api::NavigatorNode>,
    ) {
        let (NavigatorNodeId::Provider { connection, .. }
        | NavigatorNodeId::Connection { connection }) = parent
        else {
            return;
        };
        let connection = connection.clone();
        let incoming: Vec<LoadedChild> = nodes
            .into_iter()
            .map(|node| provider_child(&connection, node))
            .collect();
        let keep: HashSet<NavigatorNodeId> = incoming.iter().map(|child| child.id.clone()).collect();
        let old = self
            .children
            .get(parent)
            .and_then(Option::as_ref)
            .cloned()
            .unwrap_or_default();
        for gone in old.into_iter().filter(|id| !keep.contains(id)) {
            self.remove_node(&gone);
        }
        self.store_provider_children(parent, incoming, false);
        self.load_states
            .insert(parent.clone(), NodeLoadState::Loaded);
        self.rebuild_visible();
    }

    /// Sets whether a loaded provider node can be expanded. Returns whether
    /// the flag changed (a collection is expandable only while it has saved
    /// aggregations).
    pub fn set_provider_has_children(&mut self, id: &NavigatorNodeId, has_children: bool) -> bool {
        let Some(node) = self.provider_nodes.get_mut(id) else {
            return false;
        };
        if node.has_children == has_children {
            return false;
        }
        node.has_children = has_children;
        if !has_children {
            self.expanded.remove(id);
        }
        self.rebuild_visible();
        true
    }

    pub fn object_kind(&self, id: &NavigatorNodeId) -> Option<KindOfObject> {
        self.object_kinds.get(id).copied()
    }

    /// What an index, constraint, foreign key or trigger row shows beside its name.
    pub fn table_item(&self, id: &NavigatorNodeId) -> Option<&TableItemInfo> {
        self.table_items.get(id)
    }

    /// The name a loaded non-connection row shows.
    pub fn label(&self, id: &NavigatorNodeId) -> Option<&str> {
        self.labels.get(id).map(String::as_str)
    }

    /// The driver's reference to a loaded function, sequence, trigger, ... row.
    pub fn schema_object(&self, id: &NavigatorNodeId) -> Option<&SchemaObjectRef> {
        self.schema_objects.get(id)
    }

    #[cfg(test)]
    fn insert_provider_children(&mut self, parent: &NavigatorNodeId, children: Vec<LoadedChild>) {
        let _ = self.store_provider_children(parent, children, false);
        self.load_states
            .insert(parent.clone(), NodeLoadState::Loaded);
        self.rebuild_visible();
    }

    /// Stores `children` under `parent` (after the loaded ones when `append`).
    /// Returns the ids that were added, or `None` when the caller cannot tell
    /// the visible rows apart without flattening the tree again (a grouped
    /// key tree).
    fn store_provider_children(
        &mut self,
        parent: &NavigatorNodeId,
        children: Vec<LoadedChild>,
        append: bool,
    ) -> Option<Vec<NavigatorNodeId>> {
        if self.groups_redis_keys(parent) {
            self.store_grouped_keys(parent, children, append);
            return None;
        }
        // The loaded list is taken, not cloned: it can hold 100k keys.
        let mut ids = if append {
            self.children
                .get_mut(parent)
                .and_then(Option::take)
                .unwrap_or_default()
        } else {
            Vec::with_capacity(children.len())
        };
        let redis_keys = self.is_redis_database(parent);
        let mut added = Vec::with_capacity(children.len());
        // A provider id is registered with its node: a repeat of a loaded id
        // (SCAN may return a key twice) is found without scanning `ids`.
        let mut page: HashSet<NavigatorNodeId> = HashSet::with_capacity(children.len());
        self.catalog_revision += 1;
        for child in children {
            let repeated = (append && self.provider_nodes.contains_key(&child.id))
                || !page.insert(child.id.clone());
            if repeated {
                continue;
            }
            let mut provider = child.provider;
            // The full key name (the tree shows the last segment only).
            if let Some(map) = provider
                .as_mut()
                .filter(|node| redis_keys && node.kind == redis_tree::KEY)
                .and_then(|node| node.payload.as_object_mut())
            {
                map.insert("key_display".into(), child.label.clone().into());
            }
            self.labels.insert(child.id.clone(), child.label);
            if let Some(provider) = provider {
                self.provider_nodes.insert(child.id.clone(), provider);
            }
            added.push(child.id.clone());
            ids.push(child.id);
        }
        self.children.insert(parent.clone(), Some(ids));
        Some(added)
    }

    fn is_expandable(&self, id: &NavigatorNodeId) -> bool {
        if matches!(id, NavigatorNodeId::Provider { node, .. } if node.starts_with(LOAD_MORE_PREFIX))
        {
            return false;
        }
        self.provider_nodes
            .get(id)
            .map(|node| node.has_children)
            .unwrap_or_else(|| id.is_expandable())
    }

    fn provider_load_request(
        &self,
        id: &NavigatorNodeId,
        cursor: Option<String>,
    ) -> Option<LoadRequest> {
        match id {
            NavigatorNodeId::Connection { connection }
                if self.provider_connections.contains(connection) =>
            {
                Some(LoadRequest::Provider {
                    connection: connection.clone(),
                    parent: None,
                    payload: None,
                    cursor,
                    limit: self.provider_page_size(),
                })
            }
            NavigatorNodeId::Provider { connection, node } => {
                let metadata = self.provider_nodes.get(id)?;
                let mut payload = metadata.payload.clone();
                if let Some(filter) = self.redis_filters.get(id) {
                    filter.apply_to(&mut payload);
                }
                Some(LoadRequest::Provider {
                    connection: connection.clone(),
                    parent: Some(node.clone()),
                    payload: Some(payload),
                    cursor,
                    limit: self.provider_page_size(),
                })
            }
            _ => None,
        }
    }

    fn load_more_id(&self, parent: &NavigatorNodeId) -> NavigatorNodeId {
        let (connection, node) = match parent {
            NavigatorNodeId::Connection { connection } => (connection.clone(), "root".into()),
            NavigatorNodeId::Provider { connection, node } => (connection.clone(), node.clone()),
            _ => unreachable!("load-more placeholders only belong to provider parents"),
        };
        NavigatorNodeId::Provider {
            connection,
            node: format!("{LOAD_MORE_PREFIX}{node}"),
        }
    }

    /// Catalog loads currently in flight (crawl and user-initiated).
    pub fn loading_count(&self) -> usize {
        self.load_states
            .values()
            .filter(|state| matches!(state, NodeLoadState::Loading))
            .count()
    }

    /// Remote search planning, for the `connected` connections only (a
    /// disconnected one is never connected for a search):
    /// - SQL: the levels down to each object group (databases, schemas) that
    ///   are not loaded yet are loaded, expanded or not, and every table-like
    ///   group is (re)loaded with the server-side name `filter`;
    /// - MongoDB: the database list, then the collection list of each database
    ///   (the driver has no name filter, the tree matches them locally).
    ///
    /// At most [`SEARCH_DATABASE_LIMIT`] databases per connection are looked
    /// into ([`Self::unsearched_database_count`] counts the others); hidden
    /// connections and databases and those the user may not open are left
    /// alone. Nothing is expanded: levels loaded for the search stay cached
    /// and collapsed, and the search reveals the matches below them. Groups
    /// already loaded with this filter, loading, or failed are not requested
    /// again.
    pub fn filter_step(
        &mut self,
        connected: &HashSet<String>,
        filter: &str,
        budget: usize,
    ) -> FilterStep {
        let mut step = FilterStep::default();
        let before = self.generation;
        self.unsearched_databases.clear();
        if self.search.scope != SearchScope::Connections {
            let targets: Vec<(String, ConnectionKind)> = self
                .connections
                .iter()
                .map(|saved| (saved.id.to_string(), saved.kind))
                .filter(|(id, kind)| {
                    connected.contains(id)
                        && if *kind == ConnectionKind::Mongodb {
                            self.provider_connections.contains(id)
                        } else {
                            sql_driver::is_sql_kind(*kind)
                        }
                        && !self.connection(id).is_some_and(|saved| saved.disabled)
                        && (self.search.show_hidden
                            || !self.preferences.connections.iter().any(|item| item.id == *id && item.hidden))
                })
                .collect();
            for (connection, kind) in targets {
                self.search_connection(&connection, kind, filter, budget, &mut step);
            }
        }
        if self.generation != before {
            self.rebuild_visible();
        }
        step
    }

    fn search_connection(
        &mut self,
        connection: &str,
        kind: ConnectionKind,
        filter: &str,
        budget: usize,
        step: &mut FilterStep,
    ) {
        let root = NavigatorNodeId::Connection { connection: connection.to_string() };
        let Some(databases) = self.search_children(&root, budget, step) else {
            return;
        };
        if self.search.scope == SearchScope::Databases {
            return;
        }
        let limit = if self.search_database_limit == 0 {
            SEARCH_DATABASE_LIMIT
        } else {
            self.search_database_limit
        };
        let mut searched = 0;
        for database in databases {
            if !self.visible_filter_allows(&database) || self.denied_databases.contains(&database) {
                continue;
            }
            if searched >= limit {
                *self.unsearched_databases.entry(connection.to_string()).or_default() += 1;
                continue;
            }
            searched += 1;
            if kind == ConnectionKind::Mongodb {
                self.search_children(&database, budget, step);
                continue;
            }
            let Some(schemas) = self.search_children(&database, budget, step) else {
                continue;
            };
            for schema in schemas {
                if !self.visible_filter_allows(&schema) {
                    continue;
                }
                let Some(groups) = self.search_children(&schema, budget, step) else {
                    continue;
                };
                for group in groups {
                    self.search_group(&group, kind, filter, budget, step);
                }
            }
        }
    }

    /// Reloads one object group with the server-side `filter`.
    fn search_group(
        &mut self,
        group: &NavigatorNodeId,
        kind: ConnectionKind,
        filter: &str,
        budget: usize,
        step: &mut FilterStep,
    ) {
        let NavigatorNodeId::ObjectGroup { group: object_group, .. } = group else {
            return;
        };
        // Functions, triggers and the rest are searched among what is loaded,
        // never loaded for the search: a collapsed group costs no query.
        if !object_group.has_data(Some(kind)) {
            return;
        }
        match self.load_states.get(group) {
            Some(NodeLoadState::Loading) => {
                step.in_flight += 1;
                return;
            }
            Some(NodeLoadState::Failed(_)) => return,
            Some(NodeLoadState::Loaded)
                if self.search_filters.get(group).map(String::as_str) == Some(filter) =>
            {
                return;
            }
            _ => {}
        }
        if step.started() >= budget {
            step.remaining += 1;
            return;
        }
        if let Some(request) = group.load_request() {
            self.begin_group_load(group, Some(filter));
            step.requests.push(request);
            step.in_flight += 1;
        }
    }

    /// The loaded children of `id` for a search. A node not loaded yet is
    /// requested (within `budget`; a schema lists its groups at once) and has
    /// none until the answer lands; a loading one is waited for, a failed one
    /// is left alone.
    fn search_children(
        &mut self,
        id: &NavigatorNodeId,
        budget: usize,
        step: &mut FilterStep,
    ) -> Option<Vec<NavigatorNodeId>> {
        match self.load_states.get(id) {
            Some(NodeLoadState::Loaded) if !self.next_cursors.contains_key(id) => {
                return self.children.get(id).cloned().flatten();
            }
            Some(NodeLoadState::Loaded) => {
                // A provider listing that has more pages: a search needs all of it.
                if step.started() >= budget {
                    step.remaining += 1;
                } else if let Some(request) = self.load_more(id) {
                    step.requests.push(request);
                    step.in_flight += 1;
                }
                return None;
            }
            Some(NodeLoadState::Loading) => {
                step.in_flight += 1;
                return None;
            }
            Some(NodeLoadState::Failed(_)) => return None,
            None => {}
        }
        if matches!(id, NavigatorNodeId::Schema { .. }) {
            self.open_schema(id);
            self.generation += 1;
            return self.children.get(id).cloned().flatten();
        }
        if step.started() >= budget {
            step.remaining += 1;
            return None;
        }
        if let Some(request) = self.begin_background_load(id) {
            step.requests.push(request);
            step.in_flight += 1;
        }
        None
    }

    /// Databases the last remote search left out (over the per-connection limit).
    pub fn unsearched_database_count(&self) -> usize {
        self.unsearched_databases.values().sum()
    }

    #[cfg(test)]
    pub(crate) fn set_search_database_limit(&mut self, limit: usize) {
        self.search_database_limit = limit;
    }

    /// Undoes remote search once the query is cleared (or Local search is
    /// turned back on): expanded filtered groups reload unfiltered; collapsed
    /// ones simply go back to unloaded so their next expansion loads the full
    /// listing.
    pub fn restore_step(&mut self, budget: usize) -> FilterStep {
        let mut step = FilterStep::default();
        self.unsearched_databases.clear();
        let mut groups: Vec<NavigatorNodeId> = self
            .search_filters
            .keys()
            .chain(self.pending_filters.keys())
            .cloned()
            .collect();
        groups.sort_by_key(|group| format!("{group:?}"));
        groups.dedup();
        for group in groups {
            if matches!(self.load_states.get(&group), Some(NodeLoadState::Loading)) {
                // A filtered load is still in flight: it is superseded below
                // once it lands.
                step.in_flight += 1;
                continue;
            }
            self.search_filters.remove(&group);
            self.pending_filters.remove(&group);
            self.truncated_groups.remove(&group);
            if self.expanded.contains(&group) {
                if step.started() >= budget {
                    // Keep it tracked so the next pass reloads it.
                    step.remaining += 1;
                    self.search_filters.insert(group, String::new());
                    continue;
                }
                if let Some(request) = group.load_request() {
                    self.begin_group_load(&group, None);
                    step.requests.push(request);
                    step.in_flight += 1;
                }
            } else {
                self.reset_node(&group);
            }
        }
        if !step.requests.is_empty() {
            self.rebuild_visible();
        }
        step
    }

    fn begin_group_load(&mut self, group: &NavigatorNodeId, filter: Option<&str>) {
        self.load_states
            .insert(group.clone(), NodeLoadState::Loading);
        self.generation += 1;
        self.load_generations
            .insert(group.clone(), self.generation);
        match filter {
            Some(filter) => {
                self.pending_filters
                    .insert(group.clone(), filter.to_string());
            }
            None => {
                self.pending_filters.remove(group);
            }
        }
    }

    /// Attaches the active remote-search filter to a user-initiated group
    /// load (expanding a group while a remote search is active).
    pub fn note_group_filter(&mut self, request: &LoadRequest, filter: &str) {
        let parent = request.parent_id();
        if matches!(parent, NavigatorNodeId::ObjectGroup { .. })
            && !self.pending_filters.contains_key(&parent)
        {
            self.pending_filters.insert(parent, filter.to_string());
        }
    }

    /// The server-side filter the group load in `request` was started with.
    pub fn request_filter(&self, request: &LoadRequest) -> Option<String> {
        self.pending_filters.get(&request.parent_id()).cloned()
    }

    /// Groups currently showing a filtered listing that hit the limit.
    pub fn truncated_group_count(&self) -> usize {
        self.truncated_groups.len()
    }

    /// Whether any group holds (or is loading) a filtered listing.
    pub fn has_filtered_groups(&self) -> bool {
        !self.search_filters.is_empty() || !self.pending_filters.is_empty()
    }

    /// Clears loaded children so the next expansion reloads (retry path).
    pub fn reset_node(&mut self, id: &NavigatorNodeId) {
        let mut pending = self.children.remove(id).flatten().unwrap_or_default();
        self.catalog_revision += 1;
        while let Some(node) = pending.pop() {
            if let Some(Some(children)) = self.children.remove(&node) {
                pending.extend(children);
            }
            self.load_states.remove(&node);
            self.load_generations.remove(&node);
            self.next_cursors.remove(&node);
            self.labels.remove(&node);
            self.object_kinds.remove(&node);
            self.schema_objects.remove(&node);
            self.table_items.remove(&node);
            self.provider_nodes.remove(&node);
            self.expanded.remove(&node);
            if self.selected.as_ref() == Some(&node) {
                self.selected = None;
            }
        }
        self.reloading.remove(id);
        self.load_states.remove(id);
        self.load_generations.remove(id);
        self.next_cursors.remove(id);
        self.expanded.remove(id);
        self.search_filters.remove(id);
        self.pending_filters.remove(id);
        self.truncated_groups.remove(id);
        self.generation += 1;
        self.rebuild_visible();
    }

    /// Forgets the state of `id` and of everything loaded below it (not its place in its parent's list).
    fn forget_subtree(&mut self, id: &NavigatorNodeId) {
        let mut pending = vec![id.clone()];
        while let Some(node) = pending.pop() {
            if let Some(Some(children)) = self.children.remove(&node) {
                pending.extend(children);
            }
            self.reloading.remove(&node);
            self.load_states.remove(&node);
            self.load_generations.remove(&node);
            self.next_cursors.remove(&node);
            self.labels.remove(&node);
            self.object_kinds.remove(&node);
            self.schema_objects.remove(&node);
            self.table_items.remove(&node);
            self.provider_nodes.remove(&node);
            self.expanded.remove(&node);
            self.search_filters.remove(&node);
            self.pending_filters.remove(&node);
            self.truncated_groups.remove(&node);
            self.selected_ids.remove(&node);
            if self.selected.as_ref() == Some(&node) {
                self.selected = None;
            }
            if self.anchor.as_ref() == Some(&node) {
                self.anchor = None;
            }
        }
    }

    /// Removes one loaded node and its loaded subtree from its parent's child
    /// list. Siblings, their expansion and the parent's paging cursor stay
    /// untouched (unlike `reset_node`, which collapses and reloads). Used
    /// after the server confirmed a drop, so no reload is needed.
    pub fn remove_node(&mut self, id: &NavigatorNodeId) -> bool {
        let mut removed = false;
        for children in self.children.values_mut().flatten() {
            if let Some(position) = children.iter().position(|child| child == id) {
                children.remove(position);
                removed = true;
                break;
            }
        }
        if !removed {
            return false;
        }
        let mut pending = vec![id.clone()];
        self.catalog_revision += 1;
        while let Some(node) = pending.pop() {
            if let Some(Some(children)) = self.children.remove(&node) {
                pending.extend(children);
            }
            self.load_states.remove(&node);
            self.load_generations.remove(&node);
            self.next_cursors.remove(&node);
            self.labels.remove(&node);
            self.object_kinds.remove(&node);
            self.schema_objects.remove(&node);
            self.table_items.remove(&node);
            self.provider_nodes.remove(&node);
            self.expanded.remove(&node);
            self.search_filters.remove(&node);
            self.pending_filters.remove(&node);
            self.truncated_groups.remove(&node);
            self.selected_ids.remove(&node);
            if self.selected.as_ref() == Some(&node) {
                self.selected = None;
            }
        }
        self.generation += 1;
        self.rebuild_visible();
        true
    }

    pub fn collapse_all(&mut self) {
        if self.search_active() {
            self.search_collapsed.extend(
                self.visible
                    .iter()
                    .filter(|row| row.expandable)
                    .map(|row| row.id.clone()),
            );
            self.rebuild_visible();
            return;
        }
        self.expanded.clear();
        self.collapsed_groups = self
            .preferences
            .groups
            .iter()
            .map(|group| group.id.clone())
            .collect();
        self.rebuild_visible();
    }

    #[cfg(test)]
    /// The object row the user activated, if the id refers to one.
    /// Returns (id, connection, database, object).
    pub fn object_selection(&self) -> Option<(&NavigatorNodeId, &str, &str, &str)> {
        let id = self.selected.as_ref()?;
        if let NavigatorNodeId::Object {
            connection,
            database,
            object,
            ..
        } = id
        {
            Some((id, connection.as_str(), database.as_str(), object.as_str()))
        } else {
            None
        }
    }

    /// SQL preview for a selected object with quoted identifiers.
    pub fn preview_sql(&self, id: &NavigatorNodeId) -> Option<String> {
        if let NavigatorNodeId::Object {
            connection,
            schema,
            object,
            ..
        } = id
        {
            let backtick = |name: &str| format!("`{}`", name.replace('`', "``"));
            let quote: &dyn Fn(&str) -> String = match self.connection(connection).map(|c| c.kind) {
                Some(ConnectionKind::Mysql | ConnectionKind::Mariadb | ConnectionKind::Clickhouse) => {
                    &backtick
                }
                _ => &quote_identifier,
            };
            let mssql = self.connection(connection).map(|c| c.kind) == Some(ConnectionKind::Mssql);
            Some(if mssql {
                format!("SELECT TOP 200 *\nFROM {}.{};", quote(schema), quote(object))
            } else {
                format!("SELECT *\nFROM {}.{}\nLIMIT 200;", quote(schema), quote(object))
            })
        } else {
            None
        }
    }

    fn rebuild(&mut self) {
        self.connection_lookup = self
            .connections
            .iter()
            .enumerate()
            .map(|(index, connection)| (connection.id.to_string(), index))
            .collect();
        self.load_generations.clear();
        self.generation += 1;
        self.rebuild_visible();
    }
}

/// One child produced by a completed load.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LoadedChild {
    pub id: NavigatorNodeId,
    pub label: String,
    pub object_kind: Option<KindOfObject>,
    pub provider: Option<ProviderNode>,
    /// A schema object's reference for the driver (see [`NavigatorNodeId::SchemaObject`]).
    pub schema_object: Option<SchemaObjectRef>,
    /// A database the connected user may not connect to (the driver said so).
    pub no_connect: bool,
    /// An index, constraint, foreign key or trigger row's extras.
    pub item: Option<TableItemInfo>,
}

/// Builds child ids from catalog data for each load type.
pub fn database_child(connection: &str, database: &str) -> LoadedChild {
    LoadedChild {
        id: NavigatorNodeId::Database {
            connection: connection.to_string(),
            database: database.to_string(),
        },
        label: database.to_string(),
        object_kind: None,
        provider: None,
        schema_object: None,
        no_connect: false,
        item: None,
    }
}

pub fn schema_child(connection: &str, database: &str, schema: &str) -> LoadedChild {
    LoadedChild {
        id: NavigatorNodeId::Schema {
            connection: connection.to_string(),
            database: database.to_string(),
            schema: schema.to_string(),
        },
        label: schema.to_string(),
        object_kind: None,
        provider: None,
        schema_object: None,
        no_connect: false,
        item: None,
    }
}

pub fn object_group_child(
    connection: &str,
    database: &str,
    schema: &str,
    group: ObjectGroup,
) -> LoadedChild {
    LoadedChild {
        id: object_group_id(connection, database, schema, group),
        label: group.title().to_string(),
        object_kind: None,
        provider: None,
        schema_object: None,
        no_connect: false,
        item: None,
    }
}

pub fn object_child(
    connection: &str,
    database: &str,
    schema: &str,
    object: &str,
    kind: KindOfObject,
) -> LoadedChild {
    LoadedChild {
        id: NavigatorNodeId::Object {
            connection: connection.to_string(),
            database: database.to_string(),
            schema: schema.to_string(),
            object: object.to_string(),
        },
        label: object.to_string(),
        object_kind: Some(kind),
        provider: None,
        schema_object: None,
        no_connect: false,
        item: None,
    }
}

/// A function, procedure, sequence, trigger, type or event row of `group`. Its
/// identity is the driver's node id `key`, unique within the group (see
/// [`NavigatorNodeId::SchemaObject`]); `label` is what the row says.
pub fn schema_object_child(
    connection: &str,
    database: &str,
    schema: &str,
    group: ObjectGroup,
    key: &str,
    label: &str,
    reference: SchemaObjectRef,
) -> LoadedChild {
    LoadedChild {
        id: schema_object_id(connection, database, schema, group, key),
        label: label.to_string(),
        object_kind: None,
        provider: None,
        schema_object: Some(reference),
        no_connect: false,
        item: None,
    }
}

pub fn schema_object_id(
    connection: &str,
    database: &str,
    schema: &str,
    group: ObjectGroup,
    label: &str,
) -> NavigatorNodeId {
    NavigatorNodeId::SchemaObject {
        connection: connection.to_string(),
        database: database.to_string(),
        schema: schema.to_string(),
        group,
        object: label.to_string(),
    }
}

pub fn column_child(
    connection: &str,
    database: &str,
    schema: &str,
    object: &str,
    column: &str,
) -> LoadedChild {
    LoadedChild {
        id: NavigatorNodeId::Column {
            connection: connection.to_string(),
            database: database.to_string(),
            schema: schema.to_string(),
            object: object.to_string(),
            column: column.to_string(),
        },
        label: column.to_string(),
        object_kind: None,
        provider: None,
        schema_object: None,
        no_connect: false,
        item: None,
    }
}

/// An index, constraint, foreign key or trigger row of the table folder `parent`. `item` is unique
/// within the folder; `name` is what the row says.
pub fn table_item_child(
    parent: &NavigatorNodeId,
    item: &str,
    name: &str,
    info: TableItemInfo,
) -> Option<LoadedChild> {
    let NavigatorNodeId::TableFolder { connection, database, schema, object, folder } = parent else {
        return None;
    };
    Some(LoadedChild {
        id: NavigatorNodeId::TableItem {
            connection: connection.clone(),
            database: database.clone(),
            schema: schema.clone(),
            object: object.clone(),
            folder: *folder,
            item: item.to_string(),
        },
        label: name.to_string(),
        object_kind: None,
        provider: None,
        schema_object: None,
        no_connect: false,
        item: Some(info),
    })
}

pub fn provider_child(connection: &str, node: engine_driver_api::NavigatorNode) -> LoadedChild {
    LoadedChild {
        id: NavigatorNodeId::Provider {
            connection: connection.to_string(),
            node: node.id,
        },
        label: node.label,
        object_kind: None,
        schema_object: None,
        provider: Some(ProviderNode {
            kind: node.kind,
            subtitle: node.subtitle,
            icon: node.icon,
            has_children: node.has_children,
            payload: node.payload,
            actions: node.actions,
        }),
        no_connect: false,
        item: None,
    }
}

pub fn object_group_id(
    connection: &str,
    database: &str,
    schema: &str,
    group: ObjectGroup,
) -> NavigatorNodeId {
    NavigatorNodeId::ObjectGroup {
        connection: connection.to_string(),
        database: database.to_string(),
        schema: schema.to_string(),
        group,
    }
}

impl NavigatorNodeId {
    pub fn is_expandable(&self) -> bool {
        !matches!(
            self,
            NavigatorNodeId::Column { .. }
                | NavigatorNodeId::SchemaObject { .. }
                | NavigatorNodeId::TableItem { .. }
        )
    }

    /// `folder` of the table or view this id names (`None` for any other node).
    pub fn table_folder(&self, folder: TableFolder) -> Option<NavigatorNodeId> {
        let NavigatorNodeId::Object { connection, database, schema, object } = self else {
            return None;
        };
        Some(NavigatorNodeId::TableFolder {
            connection: connection.clone(),
            database: database.clone(),
            schema: schema.clone(),
            object: object.clone(),
            folder,
        })
    }
    fn load_request(&self) -> Option<LoadRequest> {
        match self {
            NavigatorNodeId::Group { .. } | NavigatorNodeId::Driver { .. } | NavigatorNodeId::BookmarkSection | NavigatorNodeId::Bookmark { .. } => None,
            NavigatorNodeId::Connection { connection } => Some(LoadRequest::Databases {
                connection: connection.clone(),
            }),
            NavigatorNodeId::Database {
                connection,
                database,
            } => Some(LoadRequest::Schemas {
                connection: connection.clone(),
                database: database.clone(),
            }),
            NavigatorNodeId::Schema {
                connection,
                database,
                schema,
            } => Some(LoadRequest::Objects {
                connection: connection.clone(),
                database: database.clone(),
                schema: schema.clone(),
                group: ObjectGroup::Tables,
            }),
            NavigatorNodeId::ObjectGroup {
                connection,
                database,
                schema,
                group,
            } => Some(LoadRequest::Objects {
                connection: connection.clone(),
                database: database.clone(),
                schema: schema.clone(),
                group: *group,
            }),
            NavigatorNodeId::TableFolder {
                connection,
                database,
                schema,
                object,
                folder: TableFolder::Columns,
            } => Some(LoadRequest::Columns {
                connection: connection.clone(),
                database: database.clone(),
                schema: schema.clone(),
                object: object.clone(),
            }),
            NavigatorNodeId::TableFolder {
                connection,
                database,
                schema,
                object,
                folder,
            } => Some(LoadRequest::TableFolder {
                connection: connection.clone(),
                database: database.clone(),
                schema: schema.clone(),
                object: object.clone(),
                folder: *folder,
            }),
            // A table lists its folders at once ([`NavigatorState::open_object`]).
            NavigatorNodeId::Object { .. }
            | NavigatorNodeId::Column { .. }
            | NavigatorNodeId::TableItem { .. }
            | NavigatorNodeId::SchemaObject { .. }
            | NavigatorNodeId::Provider { .. } => None,
        }
    }
}

impl LoadRequest {
    pub fn connection_id(&self) -> String {
        match self {
            LoadRequest::Databases { connection } => connection.clone(),
            LoadRequest::Schemas { connection, .. } => connection.clone(),
            LoadRequest::Objects { connection, .. } => connection.clone(),
            LoadRequest::Columns { connection, .. } => connection.clone(),
            LoadRequest::TableFolder { connection, .. } => connection.clone(),
            LoadRequest::Provider { connection, .. } => connection.clone(),
        }
    }

    pub fn parent_id(&self) -> NavigatorNodeId {
        match self {
            LoadRequest::Databases { connection } => NavigatorNodeId::Connection {
                connection: connection.clone(),
            },
            LoadRequest::Schemas {
                connection,
                database,
            } => NavigatorNodeId::Database {
                connection: connection.clone(),
                database: database.clone(),
            },
            LoadRequest::Objects {
                connection,
                database,
                schema,
                group,
            } => NavigatorNodeId::ObjectGroup {
                connection: connection.clone(),
                database: database.clone(),
                schema: schema.clone(),
                group: *group,
            },
            LoadRequest::Columns {
                connection,
                database,
                schema,
                object,
            } => NavigatorNodeId::TableFolder {
                connection: connection.clone(),
                database: database.clone(),
                schema: schema.clone(),
                object: object.clone(),
                folder: TableFolder::Columns,
            },
            LoadRequest::TableFolder {
                connection,
                database,
                schema,
                object,
                folder,
            } => NavigatorNodeId::TableFolder {
                connection: connection.clone(),
                database: database.clone(),
                schema: schema.clone(),
                object: object.clone(),
                folder: *folder,
            },
            LoadRequest::Provider {
                connection, parent, ..
            } => parent.as_ref().map_or_else(
                || NavigatorNodeId::Connection {
                    connection: connection.clone(),
                },
                |node| NavigatorNodeId::Provider {
                    connection: connection.clone(),
                    node: node.clone(),
                },
            ),
        }
    }
}

/// The name of a database type, as the "group by database type" sections show it.
pub fn driver_title(kind: ConnectionKind) -> &'static str {
    match kind {
        ConnectionKind::Postgres => "PostgreSQL",
        ConnectionKind::Sqlite => "SQLite",
        ConnectionKind::Mongodb => "MongoDB",
        ConnectionKind::Mysql => "MySQL",
        ConnectionKind::Mariadb => "MariaDB",
        ConnectionKind::Redis => "Redis",
        ConnectionKind::Clickhouse => "ClickHouse",
        ConnectionKind::Duckdb => "DuckDB",
        ConnectionKind::Mssql => "SQL Server",
    }
}

/// The order of the database-type sections.
const DRIVER_SECTIONS: [ConnectionKind; 9] = [
    ConnectionKind::Postgres,
    ConnectionKind::Mysql,
    ConnectionKind::Mariadb,
    ConnectionKind::Mssql,
    ConnectionKind::Sqlite,
    ConnectionKind::Duckdb,
    ConnectionKind::Clickhouse,
    ConnectionKind::Mongodb,
    ConnectionKind::Redis,
];

pub fn quote_identifier(value: &str) -> String {
    format!("\"{}\"", value.replace('"', "\"\""))
}

fn default_label(id: &NavigatorNodeId) -> String {
    match id {
        NavigatorNodeId::Group { id } => id.clone(),
        NavigatorNodeId::Driver { driver } => driver.clone(),
        NavigatorNodeId::BookmarkSection => "Bookmarks".into(),
        NavigatorNodeId::Bookmark { id } => id.clone(),
        NavigatorNodeId::Connection { connection } => connection.clone(),
        NavigatorNodeId::Database { database, .. } => database.clone(),
        NavigatorNodeId::Schema { schema, .. } => schema.clone(),
        NavigatorNodeId::ObjectGroup { group, .. } => group.title().to_string(),
        NavigatorNodeId::Object { object, .. } | NavigatorNodeId::SchemaObject { object, .. } => {
            object.clone()
        }
        NavigatorNodeId::Column { column, .. } => column.clone(),
        NavigatorNodeId::TableFolder { folder, .. } => folder.title().to_string(),
        NavigatorNodeId::TableItem { item, .. } => item.clone(),
        NavigatorNodeId::Provider { node, .. } if node.starts_with(LOAD_MORE_PREFIX) => {
            "Load More".into()
        }
        NavigatorNodeId::Provider { node, .. } => node.clone(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::connections::{ConnectionKind, SavedConnection};
    use crate::sidebar_preferences::{ConnectionSort, SidebarPreferences};
    use super::sidebar_view::FolderFilterCount;

    fn sqlite_conn(id: &str, name: &str) -> SavedConnection {
        SavedConnection {
            id: uuid::Uuid::parse_str(id).unwrap(),
            name: name.to_string(),
            uri: "sqlite:///tmp/app.db".to_string(),
            kind: ConnectionKind::Sqlite,
            save_password: false,
            is_pinned: false,
            disabled: false,
            ssh_tunnel: false,
        }
    }

    /// A driver connection that lists provider nodes (MongoDB); SQL drivers
    /// fill the typed tree instead.
    fn mongo_conn(id: &str, name: &str) -> SavedConnection {
        SavedConnection {
            uri: "mongodb://localhost/app".to_string(),
            kind: ConnectionKind::Mongodb,
            ..sqlite_conn(id, name)
        }
    }

    const CONN: &str = "11111111-1111-1111-1111-111111111111";

    /// Opens a table and its Columns folder, answering the folder's load with `columns`.
    fn open_columns(state: &mut NavigatorState, table: &NavigatorNodeId, columns: Vec<LoadedChild>) -> NavigatorNodeId {
        state.toggle_expansion(table);
        let folder = table.table_folder(TableFolder::Columns).expect("a table");
        let request = state.toggle_expansion(&folder).request.expect("the folder loads");
        state.apply_load(&request, state.generation(), Ok(columns));
        folder
    }

    #[test]
    fn search_reveals_loaded_descendant_path_without_dispatch_or_changing_expansion() {
        let mut state = NavigatorState::new(vec![sqlite_conn(CONN, "Workspace")]);
        let root = NavigatorNodeId::Connection {
            connection: CONN.into(),
        };
        let request = state.toggle_expansion(&root).request.unwrap();
        state.apply_load(
            &request,
            state.generation(),
            Ok(vec![database_child(CONN, "analytics")]),
        );
        state.toggle_expansion(&root);
        let previous_generation = state.generation();
        state
            .set_search(SearchOptions {
                query: "anltcs".into(),
                ..Default::default()
            })
            .unwrap();
        let rows = state.visible_rows();
        assert_eq!(
            rows.iter()
                .map(|row| row.label.as_str())
                .collect::<Vec<_>>(),
            ["Workspace", "analytics"]
        );
        assert_eq!(state.generation(), previous_generation);
        state.set_search(SearchOptions::default()).unwrap();
        assert_eq!(state.visible_rows().len(), 1);
        assert_eq!(state.visible_row_count(), 1);
    }

    #[test]
    fn a_background_load_that_cannot_start_is_a_failure_until_reset() {
        let mut state = NavigatorState::new(vec![sqlite_conn(CONN, "Workspace")]);
        let group = object_group_id(CONN, "app", "public", ObjectGroup::Tables);
        assert!(state.load_state(&group).is_none());
        state.fail_load(&group, "cannot load".into());
        assert_eq!(state.load_state(&group), Some(&NodeLoadState::Failed("cannot load".into())));
        assert!(state.begin_background_load(&group).is_none(), "a failed group is not asked again by itself");
        state.reset_node(&group);
        assert!(state.begin_background_load(&group).is_some(), "Retry starts over");
    }

    fn connected(ids: &[&str]) -> HashSet<String> {
        ids.iter().map(|id| id.to_string()).collect()
    }

    /// Expands connection -> database -> schema and loads both object groups,
    /// so the tree looks like a user had opened `Workspace / app / public`.
    fn expanded_schema(state: &mut NavigatorState) -> (NavigatorNodeId, NavigatorNodeId) {
        let root = NavigatorNodeId::Connection { connection: CONN.into() };
        let request = state.toggle_expansion(&root).request.unwrap();
        state.apply_load(&request, state.generation(), Ok(vec![database_child(CONN, "app")]));
        let database = NavigatorNodeId::Database { connection: CONN.into(), database: "app".into() };
        let request = state.toggle_expansion(&database).request.unwrap();
        state.apply_load(&request, state.generation(), Ok(vec![schema_child(CONN, "app", "public")]));
        let schema = NavigatorNodeId::Schema {
            connection: CONN.into(),
            database: "app".into(),
            schema: "public".into(),
        };
        state.toggle_expansion(&schema);
        let tables = object_group_id(CONN, "app", "public", ObjectGroup::Tables);
        let views = object_group_id(CONN, "app", "public", ObjectGroup::Views);
        for (group, names) in [(&tables, ["orders", "customers"]), (&views, ["order_totals", "v_x"])] {
            let request = state.toggle_expansion(group).request.unwrap();
            let children = names
                .iter()
                .map(|name| object_child(CONN, "app", "public", name, KindOfObject::Table))
                .collect();
            state.apply_load(&request, state.generation(), Ok(children));
        }
        (tables, views)
    }

    #[test]
    fn filter_step_reloads_every_group_under_expanded_schemas_with_the_filter() {
        let mut state = NavigatorState::new(vec![sqlite_conn(CONN, "Workspace")]);
        let (tables, views) = expanded_schema(&mut state);
        let live = connected(&[CONN]);

        let step = state.filter_step(&live, "ord", 4);
        assert_eq!(step.requests.len(), 2, "{step:?}");
        for request in &step.requests {
            assert_eq!(state.request_filter(request).as_deref(), Some("ord"));
        }
        // In flight: not requested again.
        assert!(state.filter_step(&live, "ord", 4).requests.is_empty());

        // The filtered listings replace the children and the filter sticks.
        for request in &step.requests {
            let generation = state.load_generation(request).unwrap();
            let children = match request.parent_id() {
                id if id == tables => vec![object_child(CONN, "app", "public", "orders", KindOfObject::Table)],
                _ => vec![object_child(CONN, "app", "public", "order_totals", KindOfObject::View)],
            };
            state.apply_load(request, generation, Ok(children));
        }
        assert!(state.has_filtered_groups());
        assert!(state.filter_step(&live, "ord", 4).finished(), "already filtered with this filter");
        // A different query reloads them again.
        assert_eq!(state.filter_step(&live, "cust", 4).requests.len(), 2);
        let _ = views;
    }

    #[test]
    fn a_remote_search_that_hides_a_table_keeps_its_expansion_and_columns_for_when_it_is_cleared() {
        let mut state = NavigatorState::new(vec![sqlite_conn(CONN, "Workspace")]);
        let (tables, _) = expanded_schema(&mut state);
        let orders = object_child(CONN, "app", "public", "orders", KindOfObject::Table);
        let customers = object_child(CONN, "app", "public", "customers", KindOfObject::Table);
        // `orders` is open with its columns.
        let orders_columns = open_columns(&mut state, &orders.id, vec![column_child(CONN, "app", "public", "orders", "id")]);
        assert!(state.has_loaded_children(&orders_columns));

        // The search lists `customers` only: `orders` is hidden, not forgotten.
        let live = connected(&[CONN]);
        let step = state.filter_step(&live, "cust", 4);
        let request = step.requests.iter().find(|request| request.parent_id() == tables).unwrap();
        state.apply_load(request, state.load_generation(request).unwrap(), Ok(vec![customers.clone()]));
        assert!(state.has_loaded_children(&orders_columns), "the hidden table keeps its loaded columns");
        // Cleared: the group lists everything again and `orders` is still open with them.
        let restore = state.restore_step(4);
        let request = restore.requests.iter().find(|request| request.parent_id() == tables).unwrap();
        state.apply_load(request, state.load_generation(request).unwrap(), Ok(vec![orders.clone(), customers]));
        assert!(state.has_loaded_children(&orders_columns));
        let row = state.visible_rows().into_iter().find(|row| row.id == orders.id).unwrap();
        assert!(row.expanded, "and expanded");
    }

    #[test]
    fn a_reload_forgets_the_rows_that_are_gone_keeps_the_rest_and_the_active_search_filter() {
        let mut state = NavigatorState::new(vec![sqlite_conn(CONN, "Workspace")]);
        let (tables, _) = expanded_schema(&mut state);
        let orders = object_child(CONN, "app", "public", "orders", KindOfObject::Table);
        let customers = object_child(CONN, "app", "public", "customers", KindOfObject::Table);
        for table in [&orders, &customers] {
            open_columns(&mut state, &table.id, vec![column_child(CONN, "app", "public", "x", "id")]);
        }
        state.select(customers.id.clone());
        let request = state.reload_children(&tables).expect("a loaded group reloads");
        assert_eq!(state.load_state(&tables), Some(&NodeLoadState::Loading));
        assert!(state.has_loaded_children(&orders.id), "what is listed stays while it loads");
        // `customers` is dropped on the server: it goes, with its columns and the selection; `orders` stays open.
        state.apply_load(&request, state.load_generation(&request).unwrap(), Ok(vec![orders.clone()]));
        assert!(!state.has_loaded_children(&customers.id));
        assert!(state.has_loaded_children(&orders.id));
        assert_eq!(state.selected(), None);
        // A group showing a remote search is reloaded with the same filter.
        let live = connected(&[CONN]);
        let step = state.filter_step(&live, "ord", 4);
        let filtered = step.requests.iter().find(|request| request.parent_id() == tables).unwrap();
        state.apply_load(filtered, state.load_generation(filtered).unwrap(), Ok(vec![orders]));
        let request = state.reload_children(&tables).unwrap();
        assert_eq!(state.request_filter(&request).as_deref(), Some("ord"));
        // A node never loaded has nothing to reload.
        let views = object_group_id(CONN, "app", "public", ObjectGroup::Triggers);
        assert!(state.reload_children(&views).is_none());
    }

    #[test]
    fn a_reload_taken_over_by_a_search_load_keeps_the_rows_the_search_hides() {
        let mut state = NavigatorState::new(vec![sqlite_conn(CONN, "Workspace")]);
        let (tables, _) = expanded_schema(&mut state);
        let orders = object_child(CONN, "app", "public", "orders", KindOfObject::Table);
        let customers = object_child(CONN, "app", "public", "customers", KindOfObject::Table);
        let orders_columns = open_columns(&mut state, &orders.id, vec![column_child(CONN, "app", "public", "orders", "id")]);

        // A reload starts; before it answers, a remote search takes the group's load over.
        let reload = state.reload_children(&tables).unwrap();
        let reload_generation = state.load_generation(&reload).unwrap();
        state.begin_group_load(&tables, Some("cust"));
        // The reload's own answer is stale now and changes nothing.
        state.apply_load(&reload, reload_generation, Ok(vec![]));
        assert_eq!(state.load_state(&tables), Some(&NodeLoadState::Loading));
        // The search answers with `customers` only. `orders` is hidden by it, not gone: it keeps its
        // columns and its expansion for when the search is cleared.
        let search = state.load_generation(&reload).unwrap();
        assert_ne!(search, reload_generation);
        state.apply_load(&reload, search, Ok(vec![customers.clone()]));
        assert_eq!(state.load_state(&tables), Some(&NodeLoadState::Loaded));
        assert!(state.has_loaded_children(&orders_columns), "the hidden table keeps its columns");
        assert!(state.reloading.is_empty(), "the answer of the load that took over ends the reload");
        let restore = state.restore_step(4);
        let request = restore.requests.iter().find(|request| request.parent_id() == tables).unwrap();
        state.apply_load(request, state.load_generation(request).unwrap(), Ok(vec![orders.clone(), customers]));
        assert!(state.visible_rows().into_iter().find(|row| row.id == orders.id).unwrap().expanded);
    }

    #[test]
    fn a_reload_asks_for_the_search_filter_in_flight_before_the_one_shown() {
        let mut state = NavigatorState::new(vec![sqlite_conn(CONN, "Workspace")]);
        let (tables, _) = expanded_schema(&mut state);
        let live = connected(&[CONN]);
        // "ord" is shown; the user types "cust" and that load is in flight.
        let step = state.filter_step(&live, "ord", 4);
        let ord = step.requests.iter().find(|request| request.parent_id() == tables).unwrap();
        state.apply_load(ord, state.load_generation(ord).unwrap(), Ok(vec![]));
        state.begin_group_load(&tables, Some("cust"));
        let request = state.reload_children(&tables).unwrap();
        assert_eq!(state.request_filter(&request).as_deref(), Some("cust"));
        // With none in flight it is the filter shown.
        state.apply_load(&request, state.load_generation(&request).unwrap(), Ok(vec![]));
        let request = state.reload_children(&tables).unwrap();
        assert_eq!(state.request_filter(&request).as_deref(), Some("cust"));
    }

    #[test]
    fn filter_step_reaches_collapsed_containers_of_connected_connections_only() {
        let other = "22222222-2222-2222-2222-222222222222";
        let mut state = NavigatorState::new(vec![sqlite_conn(CONN, "One"), sqlite_conn(other, "Two")]);
        expanded_schema(&mut state);
        // Collapse the whole connection: its groups are still search targets.
        let root = NavigatorNodeId::Connection { connection: CONN.into() };
        state.toggle_expansion(&root);
        // Not connected -> untouched, whether loaded or not.
        assert!(state.filter_step(&connected(&[]), "x", 4).finished());
        // `Two` is connected but never loaded: its database list is requested, nothing more.
        let step = state.filter_step(&connected(&[other]), "x", 4);
        assert!(matches!(step.requests.as_slice(), [LoadRequest::Databases { connection }] if connection == other));
        assert!(state.visible_rows().iter().all(|row| !row.expanded), "nothing was expanded");
        // `One`: the groups under the collapsed connection are reloaded with the filter.
        let step = state.filter_step(&connected(&[CONN]), "x", 1);
        assert_eq!(step.requests.len(), 1, "the budget caps started loads");
        assert_eq!(step.remaining, 1);
        assert!(!state.visible_rows()[0].expanded);
    }

    #[test]
    fn filter_step_loads_the_levels_above_the_groups_without_expanding_them() {
        let mut state = NavigatorState::new(vec![sqlite_conn(CONN, "Workspace")]);
        let live = connected(&[CONN]);
        let step = state.filter_step(&live, "ord", 4);
        assert!(matches!(step.requests.as_slice(), [LoadRequest::Databases { .. }]));
        assert_eq!(state.filter_step(&live, "ord", 4).requests.len(), 0, "in flight: not asked again");
        let request = &step.requests[0];
        state.apply_load(request, state.load_generation(request).unwrap(), Ok(vec![database_child(CONN, "app")]));

        let step = state.filter_step(&live, "ord", 4);
        assert!(matches!(step.requests.as_slice(), [LoadRequest::Schemas { database, .. }] if database == "app"));
        let request = &step.requests[0];
        state.apply_load(request, state.load_generation(request).unwrap(), Ok(vec![schema_child(CONN, "app", "public")]));

        // A schema lists its groups at once; both table-like groups load with the filter.
        let step = state.filter_step(&live, "ord", 4);
        assert_eq!(step.requests.len(), 2, "{step:?}");
        for request in &step.requests {
            assert_eq!(state.request_filter(request).as_deref(), Some("ord"));
        }
        // Everything stays collapsed: only the connection row is shown without a search.
        assert_eq!(state.visible_rows().len(), 1);
        assert!(!state.visible_rows()[0].expanded);
        // The search reveals the loaded path to a match.
        let tables = object_group_id(CONN, "app", "public", ObjectGroup::Tables);
        let request = step.requests.iter().find(|request| request.parent_id() == tables).unwrap();
        state.apply_load(
            request,
            state.load_generation(request).unwrap(),
            Ok(vec![object_child(CONN, "app", "public", "orders", KindOfObject::Table)]),
        );
        let request = step.requests.iter().find(|request| request.parent_id() != tables).unwrap();
        state.apply_load(request, state.load_generation(request).unwrap(), Ok(vec![]));
        state
            .set_search(SearchOptions { query: "ord".into(), ..Default::default() })
            .unwrap();
        let labels: Vec<_> = state.visible_rows().into_iter().map(|row| row.label).collect();
        assert_eq!(labels, ["Workspace", "app", "public", "Tables", "orders"]);
        // Cleared: the tree is as before, the filtered group is unloaded again.
        state.set_search(SearchOptions::default()).unwrap();
        assert_eq!(state.visible_rows().len(), 1);
        state.restore_step(4);
        assert!(!state.has_filtered_groups());
    }

    #[test]
    fn filter_step_searches_a_limited_number_of_databases_and_counts_the_rest() {
        let mut state = NavigatorState::new(vec![sqlite_conn(CONN, "Workspace")]);
        let live = connected(&[CONN]);
        let step = state.filter_step(&live, "ord", 100);
        let request = &step.requests[0];
        let names: Vec<String> = (0..SEARCH_DATABASE_LIMIT + 7).map(|index| format!("db{index:02}")).collect();
        let children = names.iter().map(|name| database_child(CONN, name)).collect();
        state.apply_load(request, state.load_generation(request).unwrap(), Ok(children));

        let step = state.filter_step(&live, "ord", 1000);
        assert_eq!(step.requests.len(), SEARCH_DATABASE_LIMIT);
        assert!(step.requests.iter().all(|request| matches!(
            request,
            LoadRequest::Schemas { database, .. } if database.as_str() < "db50"
        )));
        assert_eq!(state.unsearched_database_count(), 7);
        state.restore_step(4);
        assert_eq!(state.unsearched_database_count(), 0);
    }

    #[test]
    fn filter_step_leaves_hidden_denied_and_disabled_alone() {
        let mut state = NavigatorState::new(vec![sqlite_conn(CONN, "Workspace")]);
        let live = connected(&[CONN]);
        let step = state.filter_step(&live, "ord", 100);
        let request = &step.requests[0];
        let mut denied = database_child(CONN, "locked");
        denied.no_connect = true;
        state.apply_load(
            request,
            state.load_generation(request).unwrap(),
            Ok(vec![database_child(CONN, "app"), denied, database_child(CONN, "admin")]),
        );
        // `admin` is a system database: hidden by default.
        state.hide_system_by_default = true;
        let step = state.filter_step(&live, "ord", 100);
        assert!(matches!(step.requests.as_slice(), [LoadRequest::Schemas { database, .. }] if database == "app"));

        // A scope that cannot match below databases stops at the database list.
        let mut state = NavigatorState::new(vec![sqlite_conn(CONN, "Workspace")]);
        state.set_search(SearchOptions { query: "ord".into(), scope: SearchScope::Connections, ..Default::default() }).unwrap();
        assert!(state.filter_step(&live, "ord", 100).finished());
        state.set_search(SearchOptions { query: "ord".into(), scope: SearchScope::Databases, ..Default::default() }).unwrap();
        let step = state.filter_step(&live, "ord", 100);
        let request = &step.requests[0];
        state.apply_load(request, state.load_generation(request).unwrap(), Ok(vec![database_child(CONN, "app")]));
        assert!(state.filter_step(&live, "ord", 100).finished(), "databases only");

        // A hidden connection is not searched.
        let mut state = NavigatorState::new(vec![sqlite_conn(CONN, "Workspace")]);
        let mut preferences = SidebarPreferences::default();
        preferences.set_hidden(CONN, true).unwrap();
        state.set_preferences(preferences).unwrap();
        assert!(state.filter_step(&live, "ord", 100).finished());
    }

    #[test]
    fn filter_step_lists_the_collections_of_every_mongodb_database() {
        let mut state = NavigatorState::new(vec![mongo_conn(CONN, "Mongo")]);
        let live = connected(&[CONN]);
        // Not enabled as a provider connection yet: nothing to ask.
        assert!(state.filter_step(&live, "keys", 4).finished());
        state.enable_provider(CONN);
        let step = state.filter_step(&live, "keys", 4);
        let [LoadRequest::Provider { parent: None, .. }] = step.requests.as_slice() else {
            panic!("{step:?}");
        };
        let request = &step.requests[0];
        let page = |nodes: Vec<(&str, &str)>| engine_driver_api::NavigatorPage {
            nodes: nodes
                .into_iter()
                .map(|(id, kind)| engine_driver_api::NavigatorNode {
                    id: id.into(),
                    kind: kind.into(),
                    label: id.into(),
                    subtitle: None,
                    icon: None,
                    has_children: kind == "database",
                    payload: serde_json::json!({"database": id, "collection": id}),
                    actions: Vec::new(),
                })
                .collect(),
            next_cursor: None,
        };
        state.apply_provider_page(request, state.load_generation(request).unwrap(), Ok(page(vec![("a", "database"), ("b", "database")])));
        let step = state.filter_step(&live, "keys", 1);
        assert_eq!(step.requests.len(), 1, "the budget caps the listings");
        assert_eq!(step.remaining, 1);
        let request = &step.requests[0];
        state.apply_provider_page(request, state.load_generation(request).unwrap(), Ok(page(vec![("api-keys", "collection")])));
        let step = state.filter_step(&live, "keys", 1);
        assert_eq!(step.requests.len(), 1);
        let request = &step.requests[0];
        state.apply_provider_page(request, state.load_generation(request).unwrap(), Ok(page(vec![("logs", "collection")])));
        assert!(state.filter_step(&live, "keys", 1).finished());
        assert!(!state.has_filtered_groups(), "a listing is never filtered on the server");

        state
            .set_search(SearchOptions { query: "api-keys".into(), ..Default::default() })
            .unwrap();
        let labels: Vec<_> = state.visible_rows().into_iter().map(|row| row.label).collect();
        assert_eq!(labels, ["Mongo", "a", "api-keys"]);
        state.set_search(SearchOptions::default()).unwrap();
        assert_eq!(state.visible_rows().len(), 1, "still collapsed");
    }

    #[test]
    fn restore_reloads_expanded_groups_and_unloads_collapsed_ones() {
        let mut state = NavigatorState::new(vec![sqlite_conn(CONN, "Workspace")]);
        let (tables, views) = expanded_schema(&mut state);
        let live = connected(&[CONN]);
        let step = state.filter_step(&live, "ord", 4);
        for request in &step.requests {
            let generation = state.load_generation(request).unwrap();
            state.apply_load(request, generation, Ok(vec![]));
        }
        // The user collapses the Views group while the filter is active.
        state.toggle_expansion(&views);
        let restore = state.restore_step(4);
        assert_eq!(restore.requests.len(), 1, "only the expanded group reloads: {restore:?}");
        assert_eq!(restore.requests[0].parent_id(), tables);
        assert!(state.request_filter(&restore.requests[0]).is_none(), "unfiltered reload");
        // The collapsed group went back to unloaded: expanding loads the full listing.
        assert!(matches!(state.toggle_expansion(&views).request, Some(LoadRequest::Objects { .. })));
        assert!(!state.has_filtered_groups());
    }

    #[test]
    fn a_filtered_load_that_hits_the_limit_is_reported_as_truncated() {
        let mut state = NavigatorState::new(vec![sqlite_conn(CONN, "Workspace")]);
        let (tables, _) = expanded_schema(&mut state);
        let step = state.filter_step(&connected(&[CONN]), "o", 4);
        let request = step.requests.iter().find(|r| r.parent_id() == tables).unwrap().clone();
        let generation = state.load_generation(&request).unwrap();
        let many = (0..REMOTE_FILTER_LIMIT)
            .map(|index| object_child(CONN, "app", "public", &format!("o{index}"), KindOfObject::Table))
            .collect();
        state.apply_load(&request, generation, Ok(many));
        assert_eq!(state.truncated_group_count(), 1);
        state.restore_step(4);
        assert_eq!(state.truncated_group_count(), 0);
    }

    #[test]
    fn persisted_visible_filter_hides_loaded_and_later_pages_without_losing_cursors_or_cache() {
        let mut state = NavigatorState::new(vec![sqlite_conn(CONN, "Workspace")]);
        let mut prefs = SidebarPreferences::default();
        prefs
            .set_visible_filter(
                CONN,
                crate::sidebar_preferences::VisibleObjectFilter {
                    databases: Some(vec!["analytics".into()]),
                    ..Default::default()
                },
            )
            .unwrap();
        state.set_preferences(prefs.clone()).unwrap();
        let root = NavigatorNodeId::Connection {
            connection: CONN.into(),
        };
        let request = state.toggle_expansion(&root).request.unwrap();
        state.apply_load(
            &request,
            state.generation(),
            Ok(vec![
                database_child(CONN, "hidden"),
                database_child(CONN, "analytics"),
            ]),
        );
        assert_eq!(
            state
                .visible_rows()
                .iter()
                .map(|r| r.label.as_str())
                .collect::<Vec<_>>(),
            ["Workspace", "analytics"]
        );
        state
            .set_search(SearchOptions {
                query: "hidden".into(),
                ..Default::default()
            })
            .unwrap();
        assert_eq!(state.visible_row_count(), 0);
        state.set_search(SearchOptions::default()).unwrap();
        state.toggle_expansion(&root);
        state.toggle_expansion(&root);
        assert_eq!(state.visible_row_count(), 2);
        prefs.show_all_objects(CONN);
        state.set_preferences(prefs).unwrap();
        assert_eq!(
            state
                .visible_rows()
                .iter()
                .map(|r| r.label.as_str())
                .collect::<Vec<_>>(),
            ["Workspace", "hidden", "analytics"]
        );
    }

    #[test]
    fn provider_filter_keeps_load_more_and_hidden_page_nodes_for_show_all() {
        let mut state = NavigatorState::new(vec![mongo_conn(CONN, "Mongo")]);
        state.enable_provider(CONN);
        let mut prefs = SidebarPreferences::default();
        prefs
            .set_visible_filter(
                CONN,
                crate::sidebar_preferences::VisibleObjectFilter {
                    databases: Some(vec!["visible".into()]),
                    ..Default::default()
                },
            )
            .unwrap();
        state.set_preferences(prefs.clone()).unwrap();
        let root = NavigatorNodeId::Connection {
            connection: CONN.into(),
        };
        let request = state.toggle_expansion(&root).request.unwrap();
        let node = |name: &str| engine_driver_api::NavigatorNode {
            id: name.into(),
            kind: "database".into(),
            label: name.into(),
            subtitle: None,
            icon: None,
            has_children: true,
            payload: serde_json::json!({"database":name}),
            actions: vec![],
        };
        state.apply_provider_page(
            &request,
            state.generation(),
            Ok(engine_driver_api::NavigatorPage {
                nodes: vec![node("hidden")],
                next_cursor: Some("next".into()),
            }),
        );
        assert_eq!(
            state
                .visible_rows()
                .iter()
                .map(|r| r.label.as_str())
                .collect::<Vec<_>>(),
            ["Mongo", "Load More"]
        );
        let more = state.load_more(&root).unwrap();
        state.apply_provider_page(
            &more,
            state.generation(),
            Ok(engine_driver_api::NavigatorPage {
                nodes: vec![node("visible")],
                next_cursor: None,
            }),
        );
        assert_eq!(
            state
                .visible_rows()
                .iter()
                .map(|r| r.label.as_str())
                .collect::<Vec<_>>(),
            ["Mongo", "visible"]
        );
        prefs.show_all_objects(CONN);
        state.set_preferences(prefs).unwrap();
        assert_eq!(
            state
                .visible_rows()
                .iter()
                .map(|r| r.label.as_str())
                .collect::<Vec<_>>(),
            ["Mongo", "hidden", "visible"]
        );
    }

    #[test]
    fn schema_filter_applies_only_to_its_configured_database() {
        let mut state = NavigatorState::new(vec![sqlite_conn(CONN, "SQL")]);
        let root = NavigatorNodeId::Connection {
            connection: CONN.into(),
        };
        let load = state.toggle_expansion(&root).request.unwrap();
        state.apply_load(
            &load,
            state.generation(),
            Ok(vec![
                database_child(CONN, "app"),
                database_child(CONN, "other"),
            ]),
        );
        for db in ["app", "other"] {
            let id = NavigatorNodeId::Database {
                connection: CONN.into(),
                database: db.into(),
            };
            let load = state.toggle_expansion(&id).request.unwrap();
            state.apply_load(
                &load,
                state.generation(),
                Ok(vec![
                    schema_child(CONN, db, "public"),
                    schema_child(CONN, db, "private"),
                ]),
            );
        }
        let mut prefs = SidebarPreferences::default();
        prefs
            .set_visible_filter(
                CONN,
                crate::sidebar_preferences::VisibleObjectFilter {
                    schemas: HashMap::from([("app".into(), Some(vec!["public".into()]))]),
                    ..Default::default()
                },
            )
            .unwrap();
        state.set_preferences(prefs).unwrap();
        let labels = state
            .visible_rows()
            .iter()
            .map(|row| row.label.clone())
            .collect::<Vec<_>>();
        assert_eq!(
            labels,
            ["SQL", "app", "public", "other", "public", "private"]
        );
    }

    #[test]
    fn regex_errors_are_reported_and_filter_scopes_prune_selection() {
        let mut state = NavigatorState::new(vec![sqlite_conn(CONN, "Local")]);
        let root = NavigatorNodeId::Connection {
            connection: CONN.into(),
        };
        state.select_visible(&root, false, false);
        assert!(state
            .set_search(SearchOptions {
                query: "[".into(),
                regex: true,
                ..Default::default()
            })
            .is_err());
        assert_eq!(state.visible_row_count(), 0, "an invalid pattern matches nothing");
        state
            .set_search(SearchOptions {
                query: "^Loc".into(),
                regex: true,
                scope: SearchScope::Connections,
                ..Default::default()
            })
            .unwrap();
        assert_eq!(state.visible_row_count(), 1);
        state
            .set_search(SearchOptions {
                query: "none".into(),
                ..Default::default()
            })
            .unwrap();
        assert_eq!(state.visible_row_count(), 0);
        assert!(state.selected().is_none());
        assert!(state.selected_ids().is_empty());
    }

    #[test]
    fn type_only_scope_does_not_preserve_unrelated_loaded_descendants() {
        let mut state = NavigatorState::new(vec![sqlite_conn(CONN, "Local")]);
        let root = NavigatorNodeId::Connection {
            connection: CONN.into(),
        };
        let request = state.toggle_expansion(&root).request.unwrap();
        state.apply_load(
            &request,
            state.generation(),
            Ok(vec![database_child(CONN, "main")]),
        );
        state
            .set_search(SearchOptions {
                scope: SearchScope::Connections,
                ..Default::default()
            })
            .unwrap();
        assert_eq!(state.visible_row_count(), 1);
        state
            .set_search(SearchOptions {
                scope: SearchScope::Databases,
                ..Default::default()
            })
            .unwrap();
        assert_eq!(
            state
                .visible_rows()
                .iter()
                .map(|row| row.label.as_str())
                .collect::<Vec<_>>(),
            ["Local", "main"]
        );
    }

    #[test]
    fn search_explicit_collapse_and_collapse_all_hide_matches_then_clear_restores_normal_tree() {
        let mut state = NavigatorState::new(vec![sqlite_conn(CONN, "Local")]);
        let root = NavigatorNodeId::Connection {
            connection: CONN.into(),
        };
        let request = state.toggle_expansion(&root).request.unwrap();
        state.apply_load(
            &request,
            state.generation(),
            Ok(vec![database_child(CONN, "main")]),
        );
        state.toggle_expansion(&root);
        state
            .set_search(SearchOptions {
                query: "main".into(),
                ..Default::default()
            })
            .unwrap();
        assert_eq!(state.visible_row_count(), 2);
        state.toggle_expansion(&root);
        assert_eq!(state.visible_row_count(), 1);
        assert!(!state.visible_rows()[0].expanded);
        state.toggle_expansion(&root);
        assert_eq!(state.visible_row_count(), 2);
        state.collapse_all();
        assert_eq!(state.visible_row_count(), 1);
        state.set_search(SearchOptions::default()).unwrap();
        assert_eq!(state.visible_row_count(), 1);
        assert!(!state.visible_rows()[0].expanded);
        state
            .set_search(SearchOptions {
                query: "main".into(),
                ..Default::default()
            })
            .unwrap();
        assert_eq!(state.visible_row_count(), 2);
    }

    #[test]
    fn scoped_search_group_and_connection_collapses_are_explicit_not_forced_open() {
        let mut state = NavigatorState::new(vec![sqlite_conn(CONN, "Local")]);
        let mut prefs = SidebarPreferences::default();
        prefs.add_group("work", "Work", None).unwrap();
        prefs.move_connection(CONN, Some("work"), 0).unwrap();
        state.set_preferences(prefs).unwrap();
        let root = NavigatorNodeId::Connection {
            connection: CONN.into(),
        };
        let request = state.toggle_expansion(&root).request.unwrap();
        state.apply_load(
            &request,
            state.generation(),
            Ok(vec![database_child(CONN, "main")]),
        );
        state
            .set_search(SearchOptions {
                scope: SearchScope::Databases,
                ..Default::default()
            })
            .unwrap();
        assert_eq!(state.visible_row_count(), 3);
        let group = NavigatorNodeId::Group { id: "work".into() };
        state.toggle_expansion(&group);
        assert_eq!(state.visible_row_count(), 1);
        assert!(!state.visible_rows()[0].expanded);
        state.toggle_expansion(&group);
        state.toggle_expansion(&root);
        assert_eq!(state.visible_row_count(), 2);
        assert!(!state.visible_rows()[1].expanded);
        state.set_search(SearchOptions::default()).unwrap();
        assert_eq!(state.visible_row_count(), 3);
    }

    #[test]
    fn connected_only_manual_and_sorted_groups_keep_stable_order() {
        let second = "22222222-2222-2222-2222-222222222222";
        let mut state = NavigatorState::new(vec![
            sqlite_conn(CONN, "zebra"),
            sqlite_conn(second, "Alpha"),
        ]);
        let mut prefs = SidebarPreferences::default();
        prefs.add_group("work", "Work", None).unwrap();
        prefs.move_connection(CONN, Some("work"), 0).unwrap();
        prefs.move_connection(second, Some("work"), 1).unwrap();
        state.set_preferences(prefs.clone()).unwrap();
        assert_eq!(
            state
                .visible_rows()
                .iter()
                .map(|row| row.label.as_str())
                .collect::<Vec<_>>(),
            ["Work", "zebra", "Alpha"]
        );
        prefs.sort = ConnectionSort::Ascending;
        state.set_preferences(prefs).unwrap();
        assert_eq!(
            state
                .visible_rows()
                .iter()
                .map(|row| row.label.as_str())
                .collect::<Vec<_>>(),
            ["Work", "Alpha", "zebra"]
        );
        state.set_connected_ids(HashSet::from([second.to_string()]));
        state
            .set_search(SearchOptions {
                connected_only: true,
                ..Default::default()
            })
            .unwrap();
        assert_eq!(
            state
                .visible_rows()
                .iter()
                .map(|row| row.label.as_str())
                .collect::<Vec<_>>(),
            ["Work", "Alpha"]
        );
    }

    #[test]
    fn root_group_can_be_reordered_between_connections() {
        let other = "22222222-2222-2222-2222-222222222222";
        let mut state =
            NavigatorState::new(vec![sqlite_conn(CONN, "One"), sqlite_conn(other, "Two")]);
        let mut prefs = SidebarPreferences::default();
        prefs.add_group("middle", "Middle", None).unwrap();
        prefs.move_connection(CONN, None, 0).unwrap();
        prefs.move_connection(other, None, 2).unwrap();
        prefs.move_group("middle", None, 1).unwrap();
        state.set_preferences(prefs.clone()).unwrap();
        assert_eq!(
            state
                .visible_rows()
                .iter()
                .map(|row| row.label.as_str())
                .collect::<Vec<_>>(),
            ["One", "Middle", "Two"]
        );
        prefs.sort = ConnectionSort::Descending;
        state.set_preferences(prefs).unwrap();
        assert_eq!(
            state
                .visible_rows()
                .iter()
                .map(|row| row.label.as_str())
                .collect::<Vec<_>>(),
            ["Two", "Middle", "One"]
        );
    }

    #[test]
    fn hidden_connection_and_collapsed_nested_groups_rebuild_the_cached_rows() {
        let other = "22222222-2222-2222-2222-222222222222";
        let mut state =
            NavigatorState::new(vec![sqlite_conn(CONN, "One"), sqlite_conn(other, "Two")]);
        let mut preferences = SidebarPreferences::default();
        preferences.add_group("parent", "Parent", None).unwrap();
        preferences
            .add_group("nested", "Nested", Some("parent"))
            .unwrap();
        preferences
            .move_connection(CONN, Some("nested"), 0)
            .unwrap();
        preferences.set_hidden(other, true).unwrap();
        state.set_preferences(preferences).unwrap();
        assert_eq!(state.visible_row_count(), 3);
        let nested = NavigatorNodeId::Group {
            id: "nested".into(),
        };
        assert!(state.toggle_expansion(&nested).request.is_none());
        assert_eq!(state.visible_row_count(), 2);
        state.collapse_all();
        assert_eq!(state.visible_row_count(), 1);
        assert_eq!(state.visible_row_at(0).unwrap().label, "Parent");
        state.set_connections(vec![
            sqlite_conn(CONN, "One renamed"),
            sqlite_conn(other, "Two"),
        ]);
        assert_eq!(
            state.visible_row_count(),
            1,
            "group collapse survives connection refresh"
        );
    }

    #[test]
    fn show_hidden_is_temporary_and_does_not_change_persisted_visibility() {
        let mut state = NavigatorState::new(vec![sqlite_conn(CONN, "Hidden")]);
        let mut prefs = SidebarPreferences::default();
        prefs.set_hidden(CONN, true).unwrap();
        state.set_preferences(prefs.clone()).unwrap();
        assert_eq!(state.visible_row_count(), 0);
        state
            .set_search(SearchOptions {
                show_hidden: true,
                ..Default::default()
            })
            .unwrap();
        assert_eq!(state.visible_rows()[0].label, "Hidden");
        assert!(state.preferences().connections[0].hidden);
        let mut restarted = NavigatorState::new(vec![sqlite_conn(CONN, "Hidden")]);
        restarted.set_preferences(prefs).unwrap();
        assert_eq!(restarted.visible_row_count(), 0);
        restarted
            .set_search(SearchOptions {
                show_hidden: true,
                ..Default::default()
            })
            .unwrap();
        assert_eq!(restarted.visible_row_count(), 1);
    }

    #[test]
    fn stale_provider_result_cannot_resurrect_selected_removed_connection() {
        let mut state = NavigatorState::new(vec![mongo_conn(CONN, "Local")]);
        state.enable_provider(CONN);
        let root = NavigatorNodeId::Connection {
            connection: CONN.into(),
        };
        let request = state.toggle_expansion(&root).request.unwrap();
        let generation = state.load_generation(&request).unwrap();
        state.set_connections(Vec::new());
        state.apply_provider_page(
            &request,
            generation,
            Ok(engine_driver_api::NavigatorPage {
                nodes: vec![provider_node(
                    "collection:users",
                    "collection",
                    "users",
                    false,
                    serde_json::json!({"database":"app","collection":"users"}),
                )],
                next_cursor: None,
            }),
        );
        assert_eq!(state.visible_row_count(), 0);
        assert!(state.selected().is_none());
        assert!(state.action_target(&root).is_none());
    }

    #[test]
    fn multi_range_keyboard_and_deletion_prune_ids() {
        let second = "22222222-2222-2222-2222-222222222222";
        let third = "33333333-3333-3333-3333-333333333333";
        let mut state = NavigatorState::new(vec![
            sqlite_conn(CONN, "A"),
            sqlite_conn(second, "B"),
            sqlite_conn(third, "C"),
        ]);
        let first_id = state.visible_rows()[0].id.clone();
        let last_id = state.visible_rows()[2].id.clone();
        assert!(state.select_visible(&first_id, false, false));
        assert!(state.select_visible(&last_id, false, true));
        assert_eq!(state.selected_ids().len(), 3);
        assert!(state.move_focus(-1, false));
        assert_eq!(state.selected_ids().len(), 1);
        assert!(state.select_visible(&last_id, true, false));
        assert_eq!(state.selected_ids().len(), 2);
        state.set_connections(vec![sqlite_conn(CONN, "A")]);
        assert!(state.selected_ids().iter().all(|id| *id == first_id));
        assert!(!state.select_visible(&last_id, false, false));
    }

    #[test]
    fn connection_multi_mode_counts_only_explicit_homogeneous_visible_selection() {
        let second = "22222222-2222-2222-2222-222222222222";
        let mut state = NavigatorState::new(vec![sqlite_conn(CONN, "A"), sqlite_conn(second, "B")]);
        let first = NavigatorNodeId::Connection {
            connection: CONN.into(),
        };
        let last = NavigatorNodeId::Connection {
            connection: second.into(),
        };
        state.select_visible(&first, false, false);
        assert!(!state.connection_multi_mode());
        state.select_visible(&last, true, false);
        assert_eq!(state.selected_connection_ids().unwrap(), vec![CONN, second]);
        assert_eq!(state.selected_connection_count(), 2);
        assert!(state.connection_multi_mode());
        state.select_all_connections();
        assert_eq!(state.selected_connection_count(), 2);
        let mut prefs = SidebarPreferences::default();
        prefs.add_group("g", "G", None).unwrap();
        state.set_preferences(prefs).unwrap();
        state.select_visible(&NavigatorNodeId::Group { id: "g".into() }, true, false);
        assert!(
            state.selected_connection_ids().is_none(),
            "mixed selection cannot batch"
        );
        assert!(!state.connection_multi_mode());
        state.clear_multi_selection();
        assert!(state.selected_ids().is_empty());
        assert!(!state.connection_multi_mode());
        state.select_all_connections();
        state
            .set_search(SearchOptions {
                query: "nomatch".into(),
                ..Default::default()
            })
            .unwrap();
        assert_eq!(state.selected_connection_count(), 0);
        assert!(!state.connection_multi_mode());
    }

    #[test]
    fn connection_refresh_keeps_selection_for_surviving_id_only() {
        let other = "22222222-2222-2222-2222-222222222222";
        let mut state = NavigatorState::new(vec![sqlite_conn(CONN, "A"), sqlite_conn(other, "B")]);
        let first = state.visible_rows()[0].id.clone();
        let second = state.visible_rows()[1].id.clone();
        state.select_visible(&first, false, false);
        state.select_visible(&second, true, false);
        state.set_connections(vec![sqlite_conn(CONN, "A renamed")]);
        assert_eq!(state.selected_ids(), &HashSet::from([first.clone()]));
        assert_eq!(state.selected(), Some(&first));
    }

    const CONN_B: &str = "22222222-2222-2222-2222-222222222222";
    const CONN_C: &str = "33333333-3333-3333-3333-333333333333";

    /// Expands `conn -> app -> public` and loads the Tables group, like a
    /// user who opened that path.
    fn expand_tree(state: &mut NavigatorState, conn: &str) {
        let root = NavigatorNodeId::Connection { connection: conn.into() };
        let request = state.toggle_expansion(&root).request.unwrap();
        state.apply_load(&request, state.generation(), Ok(vec![database_child(conn, "app")]));
        let database = NavigatorNodeId::Database { connection: conn.into(), database: "app".into() };
        let request = state.toggle_expansion(&database).request.unwrap();
        state.apply_load(&request, state.generation(), Ok(vec![schema_child(conn, "app", "public")]));
        let schema = NavigatorNodeId::Schema {
            connection: conn.into(),
            database: "app".into(),
            schema: "public".into(),
        };
        state.toggle_expansion(&schema);
        let tables = object_group_id(conn, "app", "public", ObjectGroup::Tables);
        let request = state.toggle_expansion(&tables).request.unwrap();
        state.apply_load(
            &request,
            state.generation(),
            Ok(vec![object_child(conn, "app", "public", "orders", KindOfObject::Table)]),
        );
    }

    fn rows_of(state: &NavigatorState, conn: &str) -> Vec<(NavigatorNodeId, bool)> {
        state
            .visible_rows()
            .into_iter()
            .filter(|row| node_connection(&row.id) == Some(conn))
            .map(|row| (row.id, row.expanded))
            .collect()
    }

    fn three_expanded() -> NavigatorState {
        let mut state = NavigatorState::new(vec![
            sqlite_conn(CONN, "A"),
            sqlite_conn(CONN_B, "B"),
            sqlite_conn(CONN_C, "C"),
        ]);
        for conn in [CONN, CONN_B, CONN_C] {
            expand_tree(&mut state, conn);
        }
        state
    }

    fn state_keys_for(state: &NavigatorState, conn: &str) -> usize {
        let under = |id: &NavigatorNodeId| node_connection(id) == Some(conn);
        state.expanded.iter().filter(|id| under(id)).count()
            + state.load_states.keys().filter(|id| under(id)).count()
            + state.children.keys().filter(|id| under(id)).count()
            + state.labels.keys().filter(|id| under(id)).count()
            + state.object_kinds.keys().filter(|id| under(id)).count()
            + state.load_generations.keys().filter(|id| under(id)).count()
    }

    #[test]
    fn deleting_one_connection_keeps_the_others_expanded_and_loaded() {
        let mut state = three_expanded();
        let (a_rows, c_rows) = (rows_of(&state, CONN), rows_of(&state, CONN_C));
        assert!(a_rows.len() > 4, "A shows db, schema, group and object rows");
        assert!(state_keys_for(&state, CONN_B) > 0);
        let tokens = state.load_generations.clone();

        state.set_connections(vec![sqlite_conn(CONN, "A"), sqlite_conn(CONN_C, "C")]);

        assert_eq!(rows_of(&state, CONN), a_rows, "A subtree unchanged");
        assert_eq!(rows_of(&state, CONN_C), c_rows, "C subtree unchanged");
        assert!(rows_of(&state, CONN_B).is_empty());
        assert_eq!(state_keys_for(&state, CONN_B), 0, "B leaves no state behind");
        // Loaded children and their in-flight tokens survive: nothing reloads.
        for (id, token) in &tokens {
            if node_connection(id) != Some(CONN_B) {
                assert_eq!(state.load_generations.get(id), Some(token));
            }
        }
        let database = NavigatorNodeId::Database { connection: CONN.into(), database: "app".into() };
        assert_eq!(state.load_states.get(&database), Some(&NodeLoadState::Loaded));
        let root = NavigatorNodeId::Connection { connection: CONN.into() };
        assert!(state.toggle_expansion(&root).request.is_none(), "collapse, no load");
        assert!(state.toggle_expansion(&root).request.is_none(), "re-expand uses cached children");
        assert_eq!(rows_of(&state, CONN), a_rows);
    }

    #[test]
    fn renaming_a_connection_keeps_its_expansion_and_children() {
        let mut state = three_expanded();
        let before = rows_of(&state, CONN_B);
        let a_before = rows_of(&state, CONN);
        let mut renamed = sqlite_conn(CONN_B, "B renamed");
        renamed.is_pinned = true;
        state.set_connections(vec![sqlite_conn(CONN, "A"), renamed, sqlite_conn(CONN_C, "C")]);
        assert_eq!(rows_of(&state, CONN_B), before);
        assert!(state.visible_rows().iter().any(|row| row.label == "B renamed"));
        assert_eq!(rows_of(&state, CONN), a_before);
    }

    #[test]
    fn endpoint_or_type_edit_resets_only_that_connections_subtree() {
        for change_kind in [false, true] {
            let mut state = three_expanded();
            let (a_rows, c_rows) = (rows_of(&state, CONN), rows_of(&state, CONN_C));
            let mut edited = sqlite_conn(CONN_B, "B");
            if change_kind {
                edited.kind = ConnectionKind::Postgres;
            } else {
                edited.uri = "sqlite:///tmp/other.db".into();
            }
            state.set_connections(vec![sqlite_conn(CONN, "A"), edited, sqlite_conn(CONN_C, "C")]);
            assert_eq!(rows_of(&state, CONN), a_rows);
            assert_eq!(rows_of(&state, CONN_C), c_rows);
            let b_root = NavigatorNodeId::Connection { connection: CONN_B.into() };
            assert_eq!(rows_of(&state, CONN_B), vec![(b_root.clone(), false)], "B collapsed");
            assert_eq!(state_keys_for(&state, CONN_B), 0);
            assert!(
                state.toggle_expansion(&b_root).request.is_some(),
                "B reloads on next expansion"
            );
        }
    }

    #[test]
    fn adding_a_connection_keeps_the_others_and_starts_collapsed() {
        let mut state = NavigatorState::new(vec![sqlite_conn(CONN, "A"), sqlite_conn(CONN_C, "C")]);
        expand_tree(&mut state, CONN);
        expand_tree(&mut state, CONN_C);
        let (a_rows, c_rows) = (rows_of(&state, CONN), rows_of(&state, CONN_C));
        state.set_connections(vec![
            sqlite_conn(CONN, "A"),
            sqlite_conn(CONN_C, "C"),
            sqlite_conn(CONN_B, "B"),
        ]);
        assert_eq!(rows_of(&state, CONN), a_rows);
        assert_eq!(rows_of(&state, CONN_C), c_rows);
        let b_root = NavigatorNodeId::Connection { connection: CONN_B.into() };
        assert_eq!(rows_of(&state, CONN_B), vec![(b_root, false)]);
    }

    #[test]
    fn group_collapse_state_survives_connection_changes() {
        let mut state = NavigatorState::new(vec![sqlite_conn(CONN, "A"), sqlite_conn(CONN_B, "B")]);
        state.collapsed_groups.insert("g1".into());
        state.set_connections(vec![sqlite_conn(CONN, "A")]);
        assert!(state.collapsed_groups.contains("g1"));
    }

    #[test]
    fn selection_moves_to_the_nearest_sibling_when_its_connection_is_deleted() {
        // Middle connection deleted: the next sibling is selected.
        let mut state = three_expanded();
        let deep = object_child(CONN_B, "app", "public", "orders", KindOfObject::Table).id;
        assert!(state.select_visible(&deep, false, false));
        state.set_connections(vec![sqlite_conn(CONN, "A"), sqlite_conn(CONN_C, "C")]);
        assert_eq!(
            state.selected(),
            Some(&NavigatorNodeId::Connection { connection: CONN_C.into() })
        );
        // Last connection deleted: falls back to the previous sibling, not
        // to the deepest row of its expanded subtree.
        let mut state = three_expanded();
        let deep = object_child(CONN_C, "app", "public", "orders", KindOfObject::Table).id;
        assert!(state.select_visible(&deep, false, false));
        state.set_connections(vec![sqlite_conn(CONN, "A"), sqlite_conn(CONN_B, "B")]);
        assert_eq!(
            state.selected(),
            Some(&NavigatorNodeId::Connection { connection: CONN_B.into() })
        );
        // A surviving selection is untouched.
        let mut state = three_expanded();
        let keep = object_child(CONN, "app", "public", "orders", KindOfObject::Table).id;
        assert!(state.select_visible(&keep, false, false));
        state.set_connections(vec![sqlite_conn(CONN, "A"), sqlite_conn(CONN_C, "C")]);
        assert_eq!(state.selected(), Some(&keep));
        // Deleting the only connection leaves nothing selected.
        let mut state = NavigatorState::new(vec![sqlite_conn(CONN, "A")]);
        let root = state.visible_rows()[0].id.clone();
        state.select_visible(&root, false, false);
        state.set_connections(vec![]);
        assert_eq!(state.selected(), None);
        // Editing the endpoint under the selected node selects the connection row.
        let mut state = three_expanded();
        let deep = object_child(CONN_B, "app", "public", "orders", KindOfObject::Table).id;
        state.select_visible(&deep, false, false);
        let mut edited = sqlite_conn(CONN_B, "B");
        edited.uri = "sqlite:///tmp/other.db".into();
        state.set_connections(vec![sqlite_conn(CONN, "A"), edited, sqlite_conn(CONN_C, "C")]);
        assert_eq!(
            state.selected(),
            Some(&NavigatorNodeId::Connection { connection: CONN_B.into() })
        );
    }

    #[test]
    fn action_target_is_full_identity_and_only_exposes_implemented_capabilities() {
        let mut state = NavigatorState::new(vec![sqlite_conn(CONN, "Local")]);
        let root = state.visible_rows()[0].id.clone();
        assert_eq!(
            state.available_actions(&root, false),
            vec![
                NavigatorAction::Connect,
                NavigatorAction::Refresh,
                NavigatorAction::Pin,
                NavigatorAction::CopyUri,
                NavigatorAction::Edit,
                NavigatorAction::Delete,
                NavigatorAction::MoveConnection,
                NavigatorAction::HideConnection,
                NavigatorAction::DuplicateConnection,
                NavigatorAction::CopyConfig,
                NavigatorAction::ExportConfig,
                NavigatorAction::NewQuery,
            ]
        );
        assert!(state
            .available_actions(&root, true)
            .contains(&NavigatorAction::Disconnect));
        assert_eq!(state.action_target(&root), Some(root.clone()));
        let nonexistent = object_child(CONN, "main", "public", "users", KindOfObject::Table).id;
        assert_eq!(state.action_target(&nonexistent), None);
        assert!(state.available_actions(&nonexistent, true).is_empty());
        state.set_connections(vec![]);
        assert_eq!(state.action_target(&root), None);
    }

    #[test]
    fn collapsed_tree_shows_only_connections() {
        let state = NavigatorState::new(vec![sqlite_conn(CONN, "Local DB")]);

        let rows = state.visible_rows();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].label, "Local DB");
        assert!(rows[0].expandable);
        assert!(!rows[0].expanded);
    }

    #[test]
    fn cached_identity_index_is_pruned_on_filter_hide_and_connection_reset() {
        let mut state = NavigatorState::new(vec![sqlite_conn(CONN, "Visible")]);
        let id = NavigatorNodeId::Connection {
            connection: CONN.into(),
        };
        assert_eq!(state.visible_lookup.len(), 1);
        assert_eq!(state.connection_lookup.len(), 1);
        assert_eq!(state.visible_index(&id), Some(0));
        state
            .set_search(SearchOptions {
                query: "missing".into(),
                ..Default::default()
            })
            .unwrap();
        assert!(state.visible_lookup.is_empty());
        assert!(state.action_target(&id).is_none());
        state.set_search(SearchOptions::default()).unwrap();
        let mut prefs = SidebarPreferences::default();
        prefs.set_hidden(CONN, true).unwrap();
        state.set_preferences(prefs).unwrap();
        assert!(state.visible_lookup.is_empty());
        state.set_connections(Vec::new());
        assert!(state.connection_lookup.is_empty());
        assert!(state.available_actions(&id, false).is_empty());
    }

    #[cfg(feature = "ui")]
    fn measure_large_sidebar(count: usize) {
        let connections = (1..=count)
            .map(|i| SavedConnection {
                id: uuid::Uuid::from_u128(i as u128),
                name: format!("Connection {i}"),
                uri: "sqlite:///tmp/test.db".into(),
                kind: ConnectionKind::Sqlite,
                save_password: false,
                is_pinned: false,
                disabled: false,
                ssh_tunnel: false,
            })
            .collect();
        let state = NavigatorState::new(connections);
        assert_eq!(state.visible_row_count(), count);
        assert_eq!(state.visible_lookup.len(), count);
        assert_eq!(state.connection_lookup.len(), count);
        let start = std::time::Instant::now();
        let before = crate::grid_bench::allocation_count();
        for row in &state.visible {
            std::hint::black_box(state.visible_index(&row.id));
            std::hint::black_box(state.supports_action(&row.id, false, NavigatorAction::NewQuery));
        }
        let allocations = crate::grid_bench::allocation_count() - before;
        let elapsed = start.elapsed();
        println!("sidebar {count} indexed action lookups: {elapsed:?}, {allocations} allocations");
        assert_eq!(
            allocations, 0,
            "per-row capability/index queries must allocate nothing"
        );
        let before_rows = crate::grid_bench::allocation_count();
        for index in 0..count {
            std::hint::black_box(state.visible_row_at(index));
        }
        let row_allocations = crate::grid_bench::allocation_count() - before_rows;
        println!("sidebar {count} virtual row clones: {row_allocations} allocations");
        assert!(
            row_allocations <= count as u64 * 3,
            "virtual row lookup must allocate only its own label and id"
        );
        assert!(state.action_target(&state.visible[0].id).is_some());
        assert!(state.action_target(&state.visible[count - 1].id).is_some());
    }

    #[cfg(feature = "ui")]
    #[test]
    #[ignore = "allocation counter requires --test-threads=1"]
    fn large_sidebar_10k_lookup() {
        measure_large_sidebar(10_000);
    }

    #[cfg(feature = "ui")]
    #[test]
    #[ignore = "allocation counter requires --test-threads=1"]
    fn large_sidebar_100k_lookup() {
        measure_large_sidebar(100_000);
    }

    #[test]
    fn typed_capability_checks_use_live_cached_identity_and_never_offer_disabled_connect() {
        let mut conn = sqlite_conn(CONN, "Unsupported");
        conn.disabled = true;
        let mut state = NavigatorState::new(vec![conn]);
        let id = NavigatorNodeId::Connection {
            connection: CONN.into(),
        };
        assert!(!state.supports_action(&id, false, NavigatorAction::Connect));
        assert!(!state.supports_action(&id, false, NavigatorAction::Refresh));
        assert!(state.supports_action(&id, false, NavigatorAction::Edit));
        state
            .set_search(SearchOptions {
                query: "no match".into(),
                ..Default::default()
            })
            .unwrap();
        assert!(!state.supports_action(&id, false, NavigatorAction::Edit));
    }

    #[test]
    fn expansion_dispatches_one_load_and_blocks_duplicates() {
        let mut state = NavigatorState::new(vec![sqlite_conn(CONN, "Local DB")]);
        let conn_id = NavigatorNodeId::Connection {
            connection: CONN.to_string(),
        };

        let first = state.toggle_expansion(&conn_id);
        assert_eq!(
            first.request,
            Some(LoadRequest::Databases {
                connection: CONN.to_string()
            })
        );
        assert!(matches!(
            state.visible_rows()[0].load,
            Some(NodeLoadState::Loading)
        ));

        // Expanding a node that is already loading must not re-dispatch.
        state.expanded.remove(&conn_id);
        let duplicate = state.toggle_expansion(&conn_id);
        assert_eq!(duplicate.request, None, "duplicate load blocked");
    }

    #[test]
    fn applying_a_load_flattens_children_with_depth() {
        let mut state = NavigatorState::new(vec![sqlite_conn(CONN, "Local DB")]);
        let conn_id = NavigatorNodeId::Connection {
            connection: CONN.to_string(),
        };
        let outcome = state.toggle_expansion(&conn_id);
        let generation = state.generation();
        let request = outcome.request.expect("load dispatched");

        state.apply_load(&request, generation, Ok(vec![database_child(CONN, "main")]));

        let rows = state.visible_rows();
        assert_eq!(rows.len(), 2);
        assert!(
            rows[0].expanded,
            "visible children require an expanded parent indicator"
        );
        assert_eq!(rows[1].label, "main");
        assert_eq!(rows[1].depth, 1);
        // The loaded parent reports Loaded; the child has not been touched yet.
        assert!(matches!(rows[0].load, Some(NodeLoadState::Loaded)));
        assert_eq!(rows[1].load, None);
        assert!(rows[1].expandable);
        assert!(!rows[1].expanded);
        state.toggle_expansion(&conn_id);
        assert_eq!(state.visible_rows().len(), 1);
        assert!(!state.visible_rows()[0].expanded);
    }

    #[test]
    fn expanding_a_schema_creates_object_groups_before_object_loads() {
        let mut state = NavigatorState::new(vec![sqlite_conn(CONN, "Local DB")]);
        let schema = NavigatorNodeId::Schema {
            connection: CONN.to_string(),
            database: "main".to_string(),
            schema: "public".to_string(),
        };

        let outcome = state.toggle_expansion(&schema);

        assert_eq!(outcome.request, None);
        // SQLite: tables, views and triggers, nothing loaded yet.
        let groups: Vec<_> = state.children.get(&schema).unwrap().as_ref().unwrap().clone();
        assert_eq!(
            groups,
            [ObjectGroup::Tables, ObjectGroup::Views, ObjectGroup::Triggers]
                .map(|group| object_group_id(CONN, "main", "public", group))
        );
        assert!(groups.iter().all(|group| state.load_state(group).is_none()));
        assert!(matches!(
            state.load_states.get(&schema),
            Some(NodeLoadState::Loaded)
        ));
    }

    #[test]
    fn each_dialect_shows_the_groups_it_has() {
        use ObjectGroup::*;
        let titles = |kind| {
            ObjectGroup::for_kind(Some(kind)).iter().map(|group| group.title()).collect::<Vec<_>>()
        };
        assert_eq!(
            titles(ConnectionKind::Postgres),
            ["Tables", "Views", "Materialized Views", "Functions", "Procedures", "Sequences", "Triggers", "Types"]
        );
        assert_eq!(
            titles(ConnectionKind::Mysql),
            ["Tables", "Views", "Functions", "Procedures", "Triggers", "Events"]
        );
        assert_eq!(
            titles(ConnectionKind::Mariadb),
            ["Tables", "Views", "Functions", "Procedures", "Sequences", "Triggers", "Events"]
        );
        assert_eq!(titles(ConnectionKind::Sqlite), ["Tables", "Views", "Triggers"]);
        assert_eq!(titles(ConnectionKind::Duckdb), ["Tables", "Views"]);
        assert_eq!(
            crate::navigator::TableFolder::for_kind(Some(ConnectionKind::Duckdb), false),
            [TableFolder::Columns, TableFolder::Indexes, TableFolder::Constraints, TableFolder::ForeignKeys],
            "no triggers"
        );
        assert_eq!(crate::navigator::TableFolder::for_kind(Some(ConnectionKind::Duckdb), true), [TableFolder::Columns]);
        assert_eq!(driver_title(ConnectionKind::Duckdb), "DuckDB");
        assert!(crate::navigator::sql_driver::is_sql_kind(ConnectionKind::Duckdb));
        assert_eq!(titles(ConnectionKind::Clickhouse), ["Tables", "Views"]);
        assert_eq!(ObjectGroup::for_kind(None), [Tables, Views]);
        // Only the table-like groups have columns; every other group asks for its own kind.
        assert!([Tables, Views, MaterializedViews].iter().all(|group| group.is_table_like()));
        assert!([Functions, Procedures, Sequences, Triggers, Types, Events].iter().all(|group| !group.is_table_like()));
        assert_eq!(Tables.schema_object_kind(), None);
        assert_eq!(Functions.schema_object_kind(), Some(SchemaObjectKind::Function));
    }

    /// A Postgres tree opened down to the schema, so every group exists.
    fn postgres_schema(state: &mut NavigatorState) -> NavigatorNodeId {
        let root = NavigatorNodeId::Connection { connection: CONN.into() };
        let request = state.toggle_expansion(&root).request.unwrap();
        state.apply_load(&request, state.generation(), Ok(vec![database_child(CONN, "app")]));
        let database = NavigatorNodeId::Database { connection: CONN.into(), database: "app".into() };
        let request = state.toggle_expansion(&database).request.unwrap();
        state.apply_load(&request, state.generation(), Ok(vec![schema_child(CONN, "app", "public")]));
        let schema = NavigatorNodeId::Schema {
            connection: CONN.to_string(),
            database: "app".to_string(),
            schema: "public".to_string(),
        };
        state.toggle_expansion(&schema);
        schema
    }

    fn postgres_conn() -> SavedConnection {
        SavedConnection {
            uri: "postgres://localhost/app".to_string(),
            kind: ConnectionKind::Postgres,
            ..sqlite_conn(CONN, "Warehouse")
        }
    }

    fn function_reference(arguments: &str) -> SchemaObjectRef {
        SchemaObjectRef {
            database: Some("app".into()),
            schema: Some("public".into()),
            kind: SchemaObjectKind::Function,
            name: "add".into(),
            arguments: Some(arguments.into()),
            table: None,
            identity: None,
        }
    }

    #[test]
    fn expanding_a_schema_loads_no_group_and_a_group_loads_only_when_expanded() {
        let mut state = NavigatorState::new(vec![postgres_conn()]);
        let schema = postgres_schema(&mut state);
        assert_eq!(state.loading_count(), 0, "a schema with eight groups starts no load");
        let functions = object_group_id(CONN, "app", "public", ObjectGroup::Functions);
        assert!(state.load_state(&functions).is_none());
        let outcome = state.toggle_expansion(&functions);
        assert_eq!(state.loading_count(), 1, "only the expanded group loads");
        let request = outcome.request.expect("the group's own load");
        assert_eq!(
            request,
            LoadRequest::Objects {
                connection: CONN.into(),
                database: "app".into(),
                schema: "public".into(),
                group: ObjectGroup::Functions,
            }
        );
        // Overloads of one name are two rows, each with the driver's reference.
        let (ints, texts) = (function_reference("integer, integer"), function_reference("text, text"));
        let children = vec![
            schema_object_child(CONN, "app", "public", ObjectGroup::Functions, "function:app:public:add(integer, integer)", "add(integer, integer)", ints.clone()),
            schema_object_child(CONN, "app", "public", ObjectGroup::Functions, "function:app:public:add(text, text)", "add(text, text)", texts.clone()),
        ];
        let ids: Vec<_> = children.iter().map(|child| child.id.clone()).collect();
        state.apply_load(&request, state.generation(), Ok(children));
        assert_eq!(state.schema_object(&ids[0]), Some(&ints));
        assert_eq!(state.schema_object(&ids[1]), Some(&texts));
        let labels: Vec<_> = state.visible_rows().iter().map(|row| row.label.clone()).collect();
        assert!(labels.contains(&"add(integer, integer)".to_string()) && labels.contains(&"add(text, text)".to_string()), "{labels:?}");
        // A source row is a leaf, and the search matches it by its label.
        let row = state.visible_rows().into_iter().find(|row| row.id == ids[0]).unwrap();
        assert!(!row.expandable);
        assert!(state.toggle_expansion(&ids[0]).request.is_none());
        state
            .set_search(SearchOptions { query: "text, text".into(), ..Default::default() })
            .unwrap();
        let found: Vec<_> = state.visible_rows().iter().map(|row| row.label.clone()).collect();
        assert!(found.contains(&"add(text, text)".to_string()), "{found:?}");
        assert!(!found.contains(&"add(integer, integer)".to_string()), "{found:?}");
        state.set_search(SearchOptions::default()).unwrap();
        // Reloading the group drops what it held.
        state.reset_node(&functions);
        assert!(state.schema_object(&ids[0]).is_none());
        let _ = schema;
    }

    fn mariadb_conn() -> SavedConnection {
        SavedConnection {
            uri: "mariadb://localhost/app".to_string(),
            kind: ConnectionKind::Mariadb,
            ..sqlite_conn(CONN, "Shop")
        }
    }

    #[test]
    fn a_driver_that_does_not_advertise_a_kind_does_not_get_its_group() {
        let titles = |state: &NavigatorState| {
            state.children[&NavigatorNodeId::Schema {
                connection: CONN.into(),
                database: "app".into(),
                schema: "public".into(),
            }]
            .as_ref()
            .unwrap()
            .iter()
            .filter_map(|id| state.labels.get(id).cloned())
            .collect::<Vec<_>>()
        };
        let kinds = |names: &[&str]| Some(names.iter().map(|name| name.to_string()).collect::<Vec<_>>());
        // An older PostgreSQL driver: tables, views and materialized views only.
        let mut state = NavigatorState::new(vec![postgres_conn()]);
        state.set_driver_kinds(CONN, kinds(&["database", "schema", "table", "view", "materialized_view", "column"]));
        postgres_schema(&mut state);
        assert_eq!(titles(&state), ["Tables", "Views", "Materialized Views"]);
        // A driver that advertises no object kind at all: tables and views, and the schema
        // issues no listing of its own.
        let mut state = NavigatorState::new(vec![postgres_conn()]);
        state.set_driver_kinds(CONN, kinds(&["database", "table", "column"]));
        postgres_schema(&mut state);
        assert_eq!(titles(&state), ["Tables", "Views"]);
        assert_eq!(state.loading_count(), 0);
        // A current driver gets every group of its dialect.
        let mut state = NavigatorState::new(vec![postgres_conn()]);
        state.set_driver_kinds(
            CONN,
            kinds(&["table", "view", "materialized_view", "function", "procedure", "sequence", "trigger", "type"]),
        );
        postgres_schema(&mut state);
        assert_eq!(titles(&state).len(), 8);
        // Forgetting the kinds falls back to the dialect's groups; an empty list is "unknown".
        let mut state = NavigatorState::new(vec![postgres_conn()]);
        state.set_driver_kinds(CONN, kinds(&["table"]));
        state.set_driver_kinds(CONN, Some(Vec::new()));
        postgres_schema(&mut state);
        assert_eq!(titles(&state).len(), 8);
    }

    #[test]
    fn mariadb_sequences_open_as_data_and_are_found_where_tables_are() {
        use ObjectGroup::*;
        let mariadb = Some(ConnectionKind::Mariadb);
        assert!(Sequences.has_data(mariadb));
        assert!(!Sequences.has_data(Some(ConnectionKind::Postgres)), "a PostgreSQL sequence has no columns to open");
        assert!(!Functions.has_data(mariadb));
        // Where to look next for a table the Tables group does not hold.
        assert_eq!(Tables.next_with_data(mariadb), Some(Views));
        assert_eq!(Views.next_with_data(mariadb), Some(Sequences));
        assert_eq!(Sequences.next_with_data(mariadb), None);
        let postgres = Some(ConnectionKind::Postgres);
        assert_eq!(Views.next_with_data(postgres), Some(MaterializedViews));
        assert_eq!(MaterializedViews.next_with_data(postgres), None);
        assert_eq!(Views.next_with_data(Some(ConnectionKind::Mysql)), None);
        // A remote search reloads the sequence group of MariaDB (its rows were tables).
        let mut state = NavigatorState::new(vec![mariadb_conn()]);
        let root = NavigatorNodeId::Connection { connection: CONN.into() };
        let request = state.toggle_expansion(&root).request.unwrap();
        state.apply_load(&request, state.generation(), Ok(vec![database_child(CONN, "app")]));
        let database = NavigatorNodeId::Database { connection: CONN.into(), database: "app".into() };
        let request = state.toggle_expansion(&database).request.unwrap();
        state.apply_load(&request, state.generation(), Ok(vec![schema_child(CONN, "app", "app")]));
        state.toggle_expansion(&NavigatorNodeId::Schema {
            connection: CONN.into(),
            database: "app".into(),
            schema: "app".into(),
        });
        let step = state.filter_step(&connected(&[CONN]), "ord", 100);
        let groups: Vec<_> = step
            .requests
            .iter()
            .filter_map(|request| match request {
                LoadRequest::Objects { group, .. } => Some(*group),
                _ => None,
            })
            .collect();
        assert_eq!(groups, [Tables, Views, Sequences]);
    }

    #[test]
    fn a_remote_search_reloads_the_table_like_groups_and_never_the_others() {
        let mut state = NavigatorState::new(vec![postgres_conn()]);
        postgres_schema(&mut state);
        let step = state.filter_step(&connected(&[CONN]), "ord", 100);
        let groups: Vec<_> = step
            .requests
            .iter()
            .map(|request| match request {
                LoadRequest::Objects { group, .. } => *group,
                other => panic!("{other:?}"),
            })
            .collect();
        assert_eq!(groups, [ObjectGroup::Tables, ObjectGroup::Views, ObjectGroup::MaterializedViews]);
    }

    #[test]
    fn failed_loads_can_be_retried() {
        let mut state = NavigatorState::new(vec![sqlite_conn(CONN, "Local DB")]);
        let conn_id = NavigatorNodeId::Connection {
            connection: CONN.to_string(),
        };
        let outcome = state.toggle_expansion(&conn_id);
        let generation = state.generation();
        let request = outcome.request.expect("load dispatched");

        state.apply_load(&request, generation, Err("connect failed".into()));

        let rows = state.visible_rows();
        assert!(matches!(
            rows[0].load,
            Some(NodeLoadState::Failed(ref e)) if e == "connect failed"
        ));

        // The failed expanded row retries on its next click.
        let retry = state.toggle_expansion(&conn_id);
        assert!(retry.request.is_some(), "retry dispatches after failure");
    }

    #[test]
    fn stale_generation_results_are_ignored() {
        let mut state = NavigatorState::new(vec![sqlite_conn(CONN, "Local DB")]);
        let conn_id = NavigatorNodeId::Connection {
            connection: CONN.to_string(),
        };
        let outcome = state.toggle_expansion(&conn_id);
        let request = outcome.request.expect("load dispatched");

        // Tree changes (connections replaced) before the load lands.
        state.set_connections(vec![sqlite_conn(CONN, "Renamed")]);
        let stale_generation = 0;
        state.apply_load(
            &request,
            stale_generation,
            Ok(vec![database_child(CONN, "main")]),
        );

        let rows = state.visible_rows();
        assert!(
            !matches!(rows[0].load, Some(NodeLoadState::Loaded)),
            "stale result must not apply"
        );
    }

    #[test]
    fn concurrent_sibling_loads_apply_with_independent_tokens() {
        let mut state = NavigatorState::new(vec![sqlite_conn(CONN, "Local DB")]);
        let first = NavigatorNodeId::Connection {
            connection: CONN.to_string(),
        };
        let second_connection = "22222222-2222-2222-2222-222222222222";
        state.set_connections(vec![
            sqlite_conn(CONN, "Local DB"),
            sqlite_conn(second_connection, "Other DB"),
        ]);
        let second = NavigatorNodeId::Connection {
            connection: second_connection.to_string(),
        };

        let first_request = state.toggle_expansion(&first).request.expect("first load");
        let first_generation = state.load_generation(&first_request).unwrap();
        let second_request = state
            .toggle_expansion(&second)
            .request
            .expect("second load");
        let second_generation = state.load_generation(&second_request).unwrap();

        state.apply_load(
            &first_request,
            first_generation,
            Ok(vec![database_child(CONN, "main")]),
        );
        state.apply_load(
            &second_request,
            second_generation,
            Ok(vec![database_child(second_connection, "other")]),
        );

        assert!(matches!(
            state.load_states.get(&first),
            Some(NodeLoadState::Loaded)
        ));
        assert!(matches!(
            state.load_states.get(&second),
            Some(NodeLoadState::Loaded)
        ));
    }

    #[test]
    fn resetting_one_node_does_not_invalidate_sibling_load() {
        let second_connection = "22222222-2222-2222-2222-222222222222";
        let mut state = NavigatorState::new(vec![
            sqlite_conn(CONN, "Local DB"),
            sqlite_conn(second_connection, "Other DB"),
        ]);
        let first = NavigatorNodeId::Connection {
            connection: CONN.to_string(),
        };
        let second = NavigatorNodeId::Connection {
            connection: second_connection.to_string(),
        };
        let request = state
            .toggle_expansion(&second)
            .request
            .expect("sibling load");
        let generation = state.generation();

        state.reset_node(&first);
        state.apply_load(
            &request,
            generation,
            Ok(vec![database_child(second_connection, "other")]),
        );

        assert!(matches!(
            state.load_states.get(&second),
            Some(NodeLoadState::Loaded)
        ));
    }

    #[test]
    fn response_from_before_reset_cannot_win_retry() {
        let mut state = NavigatorState::new(vec![sqlite_conn(CONN, "Local DB")]);
        let parent = NavigatorNodeId::Connection {
            connection: CONN.to_string(),
        };
        let stale_request = state
            .toggle_expansion(&parent)
            .request
            .expect("initial load");
        let stale_generation = state.generation();

        state.reset_node(&parent);
        let retry_request = state.toggle_expansion(&parent).request.expect("retry load");
        let retry_generation = state.generation();
        state.apply_load(
            &stale_request,
            stale_generation,
            Ok(vec![database_child(CONN, "stale")]),
        );
        state.apply_load(
            &retry_request,
            retry_generation,
            Ok(vec![database_child(CONN, "fresh")]),
        );

        let children = state.children.get(&parent).unwrap().as_ref().unwrap();
        assert!(matches!(
            &children[0],
            NavigatorNodeId::Database { database, .. } if database == "fresh"
        ));
    }

    #[test]
    fn replacing_connections_clears_loading_state_only_for_changed_endpoints() {
        let mut state = NavigatorState::new(vec![sqlite_conn(CONN, "Local DB")]);
        let parent = NavigatorNodeId::Connection {
            connection: CONN.to_string(),
        };
        state.toggle_expansion(&parent);

        // A rename keeps the in-flight load (and its token) so it can land.
        state.set_connections(vec![sqlite_conn(CONN, "Renamed")]);
        assert!(state.expanded.contains(&parent));
        assert!(state.load_states.contains_key(&parent));
        assert!(state.load_generations.contains_key(&parent));

        // A new endpoint drops it; the old response can no longer apply.
        let mut moved = sqlite_conn(CONN, "Renamed");
        moved.uri = "sqlite:///tmp/elsewhere.db".into();
        state.set_connections(vec![moved]);
        assert!(!state.expanded.contains(&parent));
        assert!(!state.load_states.contains_key(&parent));
        assert!(!state.load_generations.contains_key(&parent));
    }

    #[test]
    fn replacing_connections_prunes_deleted_provider_connections() {
        let mut state = NavigatorState::new(vec![mongo_conn(CONN, "Local DB")]);
        state.enable_provider(CONN);

        state.set_connections(Vec::new());

        assert!(!state.provider_connections.contains(CONN));
    }

    #[test]
    fn enabling_provider_rearms_a_pending_connection_load() {
        let mut state = NavigatorState::new(vec![mongo_conn(CONN, "Local DB")]);
        let parent = NavigatorNodeId::Connection {
            connection: CONN.to_string(),
        };
        let pending = state
            .toggle_expansion(&parent)
            .request
            .expect("connection load should be pending");

        state.enable_provider(CONN);

        assert!(state.load_generation(&pending).is_none());
        let retry = state
            .toggle_expansion(&pending.parent_id())
            .request
            .expect("provider load should be rearmed");
        assert!(matches!(retry, LoadRequest::Provider { parent: None, .. }));
    }

    #[test]
    fn a_sql_driver_connection_keeps_the_typed_tree_when_its_session_starts() {
        let mut state = NavigatorState::new(vec![sqlite_conn(CONN, "Local DB")]);
        let parent = NavigatorNodeId::Connection {
            connection: CONN.to_string(),
        };
        let pending = state.toggle_expansion(&parent).request.expect("pending load");

        state.enable_provider(CONN);

        // The tree starts over, and the retried load is the catalog's own
        // databases request, not a provider page.
        assert!(state.load_generation(&pending).is_none());
        let retry = state.toggle_expansion(&parent).request.expect("rearmed");
        assert!(matches!(retry, LoadRequest::Databases { .. }), "{retry:?}");
        assert!(
            !state.provider_connections.contains(CONN),
            "SQL connections are never provider connections"
        );
    }

    #[test]
    fn resetting_parent_invalidates_descendant_loads() {
        let mut state = NavigatorState::new(vec![sqlite_conn(CONN, "Local DB")]);
        let parent = NavigatorNodeId::Connection {
            connection: CONN.to_string(),
        };
        let child = database_child(CONN, "main");
        let child_id = child.id.clone();
        state
            .children
            .insert(parent.clone(), Some(vec![child_id.clone()]));
        state.children.insert(child_id.clone(), Some(Vec::new()));
        state
            .load_states
            .insert(child_id.clone(), NodeLoadState::Loading);
        state.generation += 1;
        state
            .load_generations
            .insert(child_id.clone(), state.generation);

        state.reset_node(&parent);

        assert!(!state.children.contains_key(&child_id));
        assert!(!state.load_states.contains_key(&child_id));
        assert!(!state.load_generations.contains_key(&child_id));
    }

    #[test]
    fn retrying_failed_provider_node_clears_descendants_and_keeps_payload() {
        let mut state = NavigatorState::new(vec![mongo_conn(CONN, "Local DB")]);
        state.enable_provider(CONN);
        let parent = provider_child(
            CONN,
            engine_driver_api::NavigatorNode {
                id: "database:app".into(),
                kind: "database".into(),
                label: "app".into(),
                subtitle: None,
                icon: None,
                has_children: true,
                payload: serde_json::json!({"database": "app"}),
                actions: Vec::new(),
            },
        );
        let parent_id = parent.id.clone();
        let root = NavigatorNodeId::Connection {
            connection: CONN.to_string(),
        };
        state.store_provider_children(&root, vec![parent], false);
        let child = provider_child(
            CONN,
            engine_driver_api::NavigatorNode {
                id: "collection:users".into(),
                kind: "collection".into(),
                label: "users".into(),
                subtitle: None,
                icon: None,
                has_children: true,
                payload: serde_json::json!({"collection": "users"}),
                actions: Vec::new(),
            },
        );
        let child_id = child.id.clone();
        state.store_provider_children(&parent_id, vec![child], false);
        state
            .load_states
            .insert(parent_id.clone(), NodeLoadState::Failed("failed".into()));

        let request = state
            .toggle_expansion(&parent_id)
            .request
            .expect("provider retry");

        assert!(!state.provider_nodes.contains_key(&child_id));
        assert!(matches!(
            request,
            LoadRequest::Provider { payload: Some(payload), .. }
                if payload == serde_json::json!({"database": "app"})
        ));
    }

    #[test]
    fn duplicate_object_names_in_different_schemas_stay_distinct() {
        let public_users = object_child(CONN, "app", "public", "users", KindOfObject::Table);
        let sales_users = object_child(CONN, "app", "sales", "users", KindOfObject::Table);

        assert_ne!(public_users.id, sales_users.id);
        assert_eq!(public_users.label, sales_users.label);
    }

    #[test]
    fn preview_sql_quotes_identifiers() {
        let state = NavigatorState::new(vec![sqlite_conn(CONN, "Local DB")]);
        let id = NavigatorNodeId::Object {
            connection: CONN.to_string(),
            database: "app".to_string(),
            schema: "public".to_string(),
            object: "weird \"name\"".to_string(),
        };

        let sql = state.preview_sql(&id).expect("preview sql");
        assert_eq!(
            sql,
            "SELECT *\nFROM \"public\".\"weird \"\"name\"\"\"\nLIMIT 200;"
        );
    }

    #[test]
    fn object_selection_reports_the_full_path() {
        let mut state = NavigatorState::new(vec![sqlite_conn(CONN, "Local DB")]);
        let id = NavigatorNodeId::Object {
            connection: CONN.to_string(),
            database: "app".to_string(),
            schema: "public".to_string(),
            object: "users".to_string(),
        };
        state.select(id);

        let (_, connection, database, object) = state.object_selection().expect("selection");
        assert_eq!(connection, CONN);
        assert_eq!(database, "app");
        assert_eq!(object, "users");
    }

    fn provider_node(
        id: &str,
        kind: &str,
        label: &str,
        has_children: bool,
        payload: serde_json::Value,
    ) -> engine_driver_api::NavigatorNode {
        engine_driver_api::NavigatorNode {
            id: id.into(),
            kind: kind.into(),
            label: label.into(),
            subtitle: Some(format!("{kind} subtitle")),
            icon: Some(kind.into()),
            has_children,
            payload,
            actions: vec!["refresh".into(), "open".into()],
        }
    }

    #[test]
    fn provider_nodes_preserve_opaque_metadata_and_stable_ids() {
        let node = provider_node(
            "collection:invoices",
            "collection",
            "invoices",
            true,
            serde_json::json!({"opaque": [1, {"token": "abc"}]}),
        );

        let child = provider_child(CONN, node.clone());
        assert_eq!(
            child.id,
            NavigatorNodeId::Provider {
                connection: CONN.into(),
                node: "collection:invoices".into(),
            }
        );
        assert_eq!(child.provider.as_ref().unwrap().payload, node.payload);
        assert_eq!(child.provider.as_ref().unwrap().actions, node.actions);
        assert_eq!(child.provider.as_ref().unwrap().icon, node.icon);
        assert_eq!(child.provider.as_ref().unwrap().subtitle, node.subtitle);
    }

    #[test]
    fn provider_child_load_round_trips_parent_payload() {
        let mut state = NavigatorState::new(vec![sqlite_conn(CONN, "Local DB")]);
        let node = provider_node(
            "database:app",
            "database",
            "app",
            true,
            serde_json::json!({"driver_private": "payload"}),
        );
        let child = provider_child(CONN, node.clone());
        let id = child.id.clone();
        state.insert_provider_children(
            &NavigatorNodeId::Connection {
                connection: CONN.into(),
            },
            vec![child],
        );

        assert_eq!(
            state.provider_node(&id).expect("provider metadata").kind,
            "database"
        );

        let outcome = state.toggle_expansion(&id);
        assert_eq!(
            outcome.request,
            Some(LoadRequest::Provider {
                connection: CONN.into(),
                parent: Some(node.id),
                payload: Some(node.payload),
                cursor: None,
                limit: PROVIDER_PAGE_SIZE,
            })
        );
    }

    #[test]
    fn provider_pages_append_with_cursor_without_duplicate_ids() {
        let mut state = NavigatorState::new(vec![mongo_conn(CONN, "Local DB")]);
        let parent = NavigatorNodeId::Connection {
            connection: CONN.into(),
        };
        state.enable_provider(CONN);
        let first = state.toggle_expansion(&parent);
        let first_request = first.request.expect("provider root request");
        let generation = state.generation();
        state.apply_provider_page(
            &first_request,
            generation,
            Ok(engine_driver_api::NavigatorPage {
                nodes: vec![provider_node(
                    "database:app",
                    "database",
                    "app",
                    false,
                    serde_json::json!({"page": 1}),
                )],
                next_cursor: Some("next-1".into()),
            }),
        );

        let more = state.load_more(&parent).expect("next page request");
        assert!(matches!(
            &more,
            LoadRequest::Provider { cursor: Some(cursor), .. } if cursor == "next-1"
        ));
        let placeholder = state.visible_rows().pop().expect("load more row");
        assert_eq!(placeholder.label, "Load More");
        assert_eq!(
            state.load_more_parent(&placeholder.id),
            Some(parent.clone())
        );
        let generation = state.generation();
        state.apply_provider_page(
            &more,
            generation,
            Ok(engine_driver_api::NavigatorPage {
                nodes: vec![
                    provider_node(
                        "database:app",
                        "database",
                        "app renamed",
                        false,
                        serde_json::json!({"page": 2}),
                    ),
                    provider_node(
                        "database:audit",
                        "database",
                        "audit",
                        false,
                        serde_json::json!({"page": 2}),
                    ),
                ],
                next_cursor: None,
            }),
        );

        let children = state.children.get(&parent).unwrap().as_ref().unwrap();
        assert_eq!(children.len(), 2);
        assert_eq!(state.actions(&children[0]), &["refresh", "open"]);
        assert!(state.load_more(&parent).is_none());
    }

    #[test]
    fn remove_node_drops_one_provider_subtree_and_keeps_sibling_expansion() {
        let mut state = NavigatorState::new(vec![mongo_conn(CONN, "Local DB")]);
        let root = NavigatorNodeId::Connection {
            connection: CONN.into(),
        };
        state.enable_provider(CONN);
        let request = state.toggle_expansion(&root).request.unwrap();
        let generation = state.generation();
        let database = |name: &str| {
            provider_node(
                &format!("database:{name}"),
                "database",
                name,
                true,
                serde_json::json!({"database": name}),
            )
        };
        state.apply_provider_page(
            &request,
            generation,
            Ok(engine_driver_api::NavigatorPage {
                nodes: vec![database("a"), database("b")],
                next_cursor: None,
            }),
        );
        let ids: Vec<_> = ["a", "b"]
            .into_iter()
            .map(|name| NavigatorNodeId::Provider {
                connection: CONN.into(),
                node: format!("database:{name}"),
            })
            .collect();
        for id in &ids {
            let request = state.toggle_expansion(id).request.unwrap();
            let generation = state.generation();
            state.apply_provider_page(
                &request,
                generation,
                Ok(engine_driver_api::NavigatorPage {
                    nodes: vec![provider_node(
                        &format!("{}:collection:c", match id {
                            NavigatorNodeId::Provider { node, .. } => node.as_str(),
                            _ => unreachable!(),
                        }),
                        "collection",
                        "c",
                        false,
                        serde_json::json!({"collection": "c"}),
                    )],
                    next_cursor: None,
                }),
            );
        }
        state.select(ids[0].clone());
        assert_eq!(state.visible_rows().len(), 5, "root, a, a/c, b, b/c");

        assert!(state.remove_node(&ids[0]));
        let rows = state.visible_rows();
        assert_eq!(rows.len(), 3, "root, b and its collection: {rows:?}");
        assert!(rows.iter().any(|row| row.id == ids[1] && row.expanded));
        assert!(state.provider_node(&ids[0]).is_none());
        assert_eq!(state.selected(), None, "a removed selection is cleared");
        assert!(!state.remove_node(&ids[0]), "already gone");
    }

    #[test]
    fn mongodb_provider_hierarchy_has_no_manufactured_schema() {
        let database = provider_child(
            CONN,
            provider_node(
                "database:app",
                "database",
                "app",
                true,
                serde_json::json!({"database": "app"}),
            ),
        );
        let collection = provider_child(
            CONN,
            provider_node(
                "collection:invoices",
                "collection",
                "invoices",
                false,
                serde_json::json!({"database": "app", "collection": "invoices"}),
            ),
        );

        assert!(matches!(database.id, NavigatorNodeId::Provider { .. }));
        assert!(matches!(collection.id, NavigatorNodeId::Provider { .. }));
    }

    fn folder_filter(key: crate::sidebar_preferences::FolderKey, include: &[&str], exclude: &[&str]) -> crate::sidebar_preferences::FolderFilter {
        crate::sidebar_preferences::FolderFilter {
            key,
            include: include.iter().map(|pattern| (*pattern).to_owned()).collect(),
            exclude: exclude.iter().map(|pattern| (*pattern).to_owned()).collect(),
        }
    }

    fn labels(state: &NavigatorState) -> Vec<String> {
        state.visible_rows().iter().map(|row| row.label.clone()).collect()
    }

    #[test]
    fn a_folder_filter_narrows_its_own_folder_only_and_counts_what_it_leaves() {
        let mut state = NavigatorState::new(vec![postgres_conn()]);
        postgres_schema(&mut state);
        let tables = object_group_id(CONN, "app", "public", ObjectGroup::Tables);
        let views = object_group_id(CONN, "app", "public", ObjectGroup::Views);
        for (group, names, kind) in [
            (&tables, ["orders", "Orders_old", "users", "order_items"], KindOfObject::Table),
            (&views, ["order_totals", "orders_old_v", "user_names", "x"], KindOfObject::View),
        ] {
            let request = state.toggle_expansion(group).request.unwrap();
            state.apply_load(&request, state.generation(), Ok(names.iter().map(|name| object_child(CONN, "app", "public", name, kind)).collect()));
        }
        let all = labels(&state);
        assert_eq!(all.len(), 3 + 1 + 4 + 1 + 4 + 6, "{all:?}");
        let key = folder_key(&tables).unwrap();
        let mut prefs = SidebarPreferences::default();
        // Include and exclude, wildcards and case do not matter: `Orders_old` is an `*_OLD` too.
        prefs.set_folder_filter(folder_filter(key.clone(), &["ORDER*", "user?"], &["*_old"])).unwrap();
        state.set_preferences(prefs.clone()).unwrap();
        let rows = labels(&state);
        let shown: Vec<_> = rows.iter().filter(|label| matches!(label.as_str(), "orders" | "Orders_old" | "users" | "order_items")).collect();
        assert_eq!(shown, ["orders", "users", "order_items"], "{rows:?}");
        // `user?` is exactly one character after `user`: `users` matches, `user_names` (a view) is another folder.
        for view in ["order_totals", "orders_old_v", "user_names", "x"] {
            assert!(rows.iter().any(|label| label == view), "the Views folder is not filtered: {rows:?}");
        }
        assert_eq!(state.folder_filter_note(&tables), Some(Some(FolderFilterCount { shown: 3, total: 4 })));
        assert_eq!(state.folder_filter_note(&views), None, "another folder has no filter");
        // The filter leaves the load alone: the folder is still loaded, expanded and has all its children.
        assert_eq!(state.loaded_children(&tables).map(<[_]>::len), Some(4));
        // Clearing brings everything back.
        prefs.clear_folder_filter(&key);
        state.set_preferences(prefs).unwrap();
        assert_eq!(labels(&state), all);
        assert_eq!(state.folder_filter_note(&tables), None);
    }

    #[test]
    fn a_filter_on_the_databases_of_a_connection_keeps_load_more_working() {
        let mut state = NavigatorState::new(vec![mongo_conn(CONN, "Mongo")]);
        state.enable_provider(CONN);
        let root = NavigatorNodeId::Connection { connection: CONN.into() };
        let mut prefs = SidebarPreferences::default();
        prefs.set_folder_filter(folder_filter(folder_key(&root).unwrap(), &["vis*"], &[])).unwrap();
        state.set_preferences(prefs).unwrap();
        let request = state.toggle_expansion(&root).request.unwrap();
        let node = |name: &str| engine_driver_api::NavigatorNode {
            id: name.into(),
            kind: "database".into(),
            label: name.into(),
            subtitle: None,
            icon: None,
            has_children: true,
            payload: serde_json::json!({"database":name}),
            actions: vec![],
        };
        state.apply_provider_page(&request, state.generation(), Ok(engine_driver_api::NavigatorPage { nodes: vec![node("hidden")], next_cursor: Some("next".into()) }));
        assert_eq!(labels(&state), ["Mongo", "Load More"]);
        let more = state.load_more(&root).expect("the next page is still asked for");
        state.apply_provider_page(&more, state.generation(), Ok(engine_driver_api::NavigatorPage { nodes: vec![node("visible_db")], next_cursor: None }));
        assert_eq!(labels(&state), ["Mongo", "visible_db"]);
        assert_eq!(state.folder_filter_note(&root), Some(Some(FolderFilterCount { shown: 1, total: 2 })));
    }

    #[test]
    fn replace_provider_children_keeps_surviving_expansion_and_drops_the_rest() {
        let mut state = three_expanded();
        let connection = "mongo-conn".to_string();
        let node = |id: &str, kind: &str, has_children: bool| engine_driver_api::NavigatorNode {
            id: id.into(),
            kind: kind.into(),
            label: id.into(),
            subtitle: None,
            icon: None,
            has_children,
            payload: serde_json::json!({"database": "db", "collection": "c"}),
            actions: vec![],
        };
        let parent = NavigatorNodeId::Provider { connection: connection.clone(), node: "c".into() };
        let group = |name: &str| NavigatorNodeId::Provider { connection: connection.clone(), node: name.into() };
        state.insert_provider_children(&parent, vec![provider_child(&connection, node("g1", "aggregations", true)), provider_child(&connection, node("g2", "aggregations", true))]);
        state.expanded.insert(group("g1"));
        assert!(state.has_loaded_children(&parent));
        state.replace_provider_children(&parent, vec![node("g1", "aggregations", true), node("g3", "aggregations", true)]);
        let ids = state.children[&parent].clone().unwrap();
        assert_eq!(ids, vec![group("g1"), group("g3")]);
        assert!(state.expanded.contains(&group("g1")), "a surviving child keeps its expansion");
        assert!(state.provider_node(&group("g2")).is_none(), "a vanished child is forgotten");
    }

    #[test]
    fn set_provider_has_children_reports_changes_and_collapses() {
        let mut state = NavigatorState::new(vec![]);
        let id = NavigatorNodeId::Provider { connection: "c".into(), node: "n".into() };
        assert!(!state.set_provider_has_children(&id, true), "unknown node: nothing to change");
        state.insert_provider_children(
            &NavigatorNodeId::Connection { connection: "c".into() },
            vec![provider_child("c", engine_driver_api::NavigatorNode {
                id: "n".into(), kind: "collection".into(), label: "n".into(), subtitle: None, icon: None,
                has_children: false, payload: serde_json::json!({"database": "d", "collection": "n"}), actions: vec![],
            })],
        );
        assert_eq!(state.provider_collection_nodes("c", "d"), vec![(id.clone(), "n".to_string())]);
        assert!(state.provider_collection_nodes("c", "other").is_empty());
        assert!(state.set_provider_has_children(&id, true));
        assert!(!state.set_provider_has_children(&id, true));
        state.expanded.insert(id.clone());
        assert!(state.set_provider_has_children(&id, false));
        assert!(!state.expanded.contains(&id));
    }

    /// The connection `CONN` opened and its databases loaded.
    fn databases_loaded(state: &mut NavigatorState, databases: Vec<LoadedChild>) -> NavigatorNodeId {
        let root = NavigatorNodeId::Connection { connection: CONN.into() };
        let request = state.toggle_expansion(&root).request.unwrap();
        state.apply_load(&request, state.generation(), Ok(databases));
        root
    }

    fn database_labels(state: &NavigatorState) -> Vec<String> {
        state
            .visible_rows()
            .into_iter()
            .filter(|row| matches!(row.id, NavigatorNodeId::Database { .. }))
            .map(|row| row.label)
            .collect()
    }

    fn catalog_databases(state: &NavigatorState) -> Vec<String> {
        let mut seen = Vec::new();
        state.visit_catalog_with(false, |id, label, _, _| {
            if matches!(id, NavigatorNodeId::Database { .. }) {
                seen.push(label.to_owned());
            }
        });
        seen
    }

    #[test]
    fn a_connection_that_lists_one_database_shows_only_that_one_everywhere() {
        let mut state = NavigatorState::new(vec![postgres_conn()]);
        let root = databases_loaded(
            &mut state,
            vec![database_child(CONN, "app"), database_child(CONN, "reports"), database_child(CONN, "warehouse")],
        );
        // Nothing asked for: every database, as before.
        assert_eq!(database_labels(&state), ["app", "reports", "warehouse"]);
        let names = |state: &NavigatorState| state.connection_databases(CONN).into_iter().map(|(_, name)| name).collect::<Vec<_>>();
        assert_eq!(names(&state), ["app", "reports", "warehouse"]);
        assert_eq!(state.openable_databases(CONN).collect::<Vec<_>>(), ["app", "reports", "warehouse"]);
        assert_eq!(catalog_databases(&state), ["app", "reports", "warehouse"]);

        let revision = state.catalog_revision();
        state.set_database_scopes(HashMap::from([(CONN.to_owned(), "reports".to_owned())]));
        assert_eq!(database_labels(&state), ["reports"], "the tree");
        assert_eq!(names(&state), ["reports"], "the pickers");
        assert_eq!(state.openable_databases(CONN).collect::<Vec<_>>(), ["reports"], "the first-database choices");
        assert_eq!(catalog_databases(&state), ["reports"], "the search index");
        assert!(state.catalog_revision() > revision, "the search index is rebuilt");
        assert!(state.visible_rows().iter().any(|row| row.id == root), "the connection itself stays");

        // The same scope again changes nothing; another connection is not affected.
        let revision = state.catalog_revision();
        state.set_database_scopes(HashMap::from([(CONN.to_owned(), "reports".to_owned())]));
        assert_eq!(state.catalog_revision(), revision);
        state.set_database_scopes(HashMap::from([("another".to_owned(), "app".to_owned())]));
        assert_eq!(database_labels(&state), ["app", "reports", "warehouse"]);

        // Show all again.
        state.set_database_scopes(HashMap::from([(CONN.to_owned(), "app".to_owned())]));
        state.set_database_scopes(HashMap::new());
        assert_eq!(database_labels(&state), ["app", "reports", "warehouse"]);
    }

    #[test]
    fn a_database_without_connect_is_listed_but_never_opens() {
        let mut state = NavigatorState::new(vec![postgres_conn()]);
        let mut closed = database_child(CONN, "reports");
        closed.no_connect = true;
        databases_loaded(&mut state, vec![database_child(CONN, "app"), closed]);
        let app = NavigatorNodeId::Database { connection: CONN.into(), database: "app".into() };
        let reports = NavigatorNodeId::Database { connection: CONN.into(), database: "reports".into() };
        assert_eq!(database_labels(&state), ["app", "reports"], "listed");
        assert!(state.database_denied(&reports) && !state.database_denied(&app));
        assert_eq!(state.openable_databases(CONN).collect::<Vec<_>>(), ["app"], "not offered as a place to start in");

        let outcome = state.toggle_expansion(&reports);
        assert!(outcome.request.is_none(), "no load is started");
        assert!(state.load_state(&reports).is_none() && !state.visible_rows().iter().any(|row| row.id == reports && row.expanded));
        assert!(state.toggle_expansion(&app).request.is_some(), "a database the user may open loads");

        // The privilege was granted and the connection refreshed: the same database opens.
        state.reset_node(&NavigatorNodeId::Connection { connection: CONN.into() });
        databases_loaded(&mut state, vec![database_child(CONN, "app"), database_child(CONN, "reports")]);
        assert!(!state.database_denied(&reports));
        assert!(state.toggle_expansion(&reports).request.is_some());
    }

    #[test]
    fn a_failed_database_shows_the_failure_and_a_retry_does_not_hide_it() {
        let mut state = NavigatorState::new(vec![postgres_conn()]);
        databases_loaded(&mut state, vec![database_child(CONN, "reports")]);
        let reports = NavigatorNodeId::Database { connection: CONN.into(), database: "reports".into() };
        state.fail_load(&reports, "permission denied for database reports".into());
        let row = state.visible_rows().into_iter().find(|row| row.id == reports).unwrap();
        assert_eq!(row.load, Some(NodeLoadState::Failed("permission denied for database reports".into())), "the row carries it at once");
    }

    #[test]
    fn pinned_connections_lead_whatever_the_sort_and_keep_the_sort_among_themselves() {
        let ids = [
            "11111111-1111-1111-1111-111111111111",
            "22222222-2222-2222-2222-222222222222",
            "33333333-3333-3333-3333-333333333333",
            "44444444-4444-4444-4444-444444444444",
        ];
        let mut connections: Vec<_> = ["delta", "alpha", "charlie", "bravo"]
            .iter()
            .zip(ids)
            .map(|(name, id)| sqlite_conn(id, name))
            .collect();
        connections[0].is_pinned = true; // delta
        connections[3].is_pinned = true; // bravo
        let mut state = NavigatorState::new(connections);
        let labels = |state: &NavigatorState| -> Vec<String> {
            state.visible_rows().into_iter().map(|row| row.label).collect()
        };
        // Manual: the stored order within each part.
        assert_eq!(labels(&state), ["delta", "bravo", "alpha", "charlie"]);
        let mut prefs = SidebarPreferences {
            sort: ConnectionSort::Ascending,
            ..Default::default()
        };
        state.set_preferences(prefs.clone()).unwrap();
        assert_eq!(labels(&state), ["bravo", "delta", "alpha", "charlie"]);
        prefs.sort = ConnectionSort::Descending;
        state.set_preferences(prefs).unwrap();
        assert_eq!(labels(&state), ["delta", "bravo", "charlie", "alpha"]);
    }

    #[test]
    fn pinned_children_lead_in_pin_order_per_place_and_unpin_restores_the_listing() {
        let mut state = NavigatorState::new(vec![sqlite_conn(CONN, "Workspace")]);
        let root = NavigatorNodeId::Connection { connection: CONN.into() };
        let request = state.toggle_expansion(&root).request.unwrap();
        state.apply_load(
            &request,
            state.generation(),
            Ok(["a", "b", "c", "d"].map(|name| database_child(CONN, name)).to_vec()),
        );
        let labels = |state: &NavigatorState| -> Vec<String> {
            state.visible_rows().into_iter().skip(1).map(|row| row.label).collect()
        };
        let database = |name: &str| NavigatorNodeId::Database { connection: CONN.into(), database: name.into() };
        assert_eq!(labels(&state), ["a", "b", "c", "d"]);
        assert!(state.set_node_pinned(&database("c"), true));
        assert!(state.set_node_pinned(&database("b"), true));
        assert!(!state.set_node_pinned(&database("b"), true), "pinned already: it keeps its place");
        assert_eq!(labels(&state), ["c", "b", "a", "d"]);
        assert!(state.is_pinned(&database("c")) && !state.is_pinned(&database("a")));
        // The same name on another connection is another row.
        let other = NavigatorNodeId::Database { connection: "other".into(), database: "c".into() };
        assert!(!state.is_pinned(&other));
        assert!(state.set_node_pinned(&database("c"), false));
        assert_eq!(labels(&state), ["b", "a", "c", "d"]);
        assert!(!state.set_node_pinned(&database("c"), false), "nothing to unpin");
        assert!(state.set_node_pinned(&database("b"), false));
        assert_eq!(labels(&state), ["a", "b", "c", "d"]);
        // Stored pins come back in their order; a pin of a row that is not listed changes nothing.
        state.set_node_pins([database("d"), database("gone"), database("a")]);
        assert_eq!(labels(&state), ["d", "a", "b", "c"]);
    }

    #[test]
    fn a_pin_key_names_the_exact_place_and_reads_back() {
        let object = NavigatorNodeId::Object {
            connection: CONN.into(),
            database: "app".into(),
            schema: "public".into(),
            object: "orders".into(),
        };
        let key = object.pin_key().unwrap();
        assert_eq!(NavigatorNodeId::from_pin_key(CONN, &key), Some(object.clone()));
        // Another connection reads the same key as its own row.
        assert_ne!(NavigatorNodeId::from_pin_key("other", &key), Some(object));
        for id in [
            NavigatorNodeId::Database { connection: CONN.into(), database: "a/b".into() },
            NavigatorNodeId::Schema { connection: CONN.into(), database: "d".into(), schema: "s\"q".into() },
            NavigatorNodeId::Provider { connection: CONN.into(), node: "collection:d:c".into() },
        ] {
            assert_eq!(NavigatorNodeId::from_pin_key(CONN, &id.pin_key().unwrap()), Some(id));
        }
        assert_eq!(NavigatorNodeId::Connection { connection: CONN.into() }.pin_key(), None);
    }

    /// `orders` of `app.public`, listed under Tables of a connection of `kind`.
    fn table_of(kind: ConnectionKind) -> (NavigatorState, NavigatorNodeId) {
        let mut state = NavigatorState::new(vec![SavedConnection { kind, ..sqlite_conn(CONN, "Workspace") }]);
        expanded_schema(&mut state);
        let orders = object_child(CONN, "app", "public", "orders", KindOfObject::Table).id;
        (state, orders)
    }

    fn folder_titles(state: &NavigatorState, table: &NavigatorNodeId) -> Vec<&'static str> {
        state
            .loaded_children(table)
            .expect("the table is open")
            .iter()
            .map(|id| match id {
                NavigatorNodeId::TableFolder { folder, .. } => folder.title(),
                other => panic!("{other:?} is not a folder"),
            })
            .collect()
    }

    fn index_item(folder: &NavigatorNodeId, name: &str, columns: &str, definition: Option<&str>) -> LoadedChild {
        let info = TableItemInfo { detail: columns.into(), definition: definition.map(str::to_owned) };
        table_item_child(folder, name, name, info).expect("a folder")
    }

    #[test]
    fn a_table_lists_the_folders_of_its_dialect_at_once_and_loads_none_until_one_opens() {
        let all = ["Columns", "Indexes", "Constraints", "Foreign Keys", "Triggers"];
        let matrix: [(ConnectionKind, &[&str]); 4] = [
            (ConnectionKind::Postgres, &all),
            (ConnectionKind::Mysql, &all),
            (ConnectionKind::Sqlite, &all),
            (ConnectionKind::Clickhouse, &["Columns", "Indexes"]),
        ];
        for (kind, expected) in matrix {
            let (mut state, orders) = table_of(kind);
            assert!(state.loaded_children(&orders).is_none(), "{kind:?}: nothing is listed before the table opens");
            assert!(state.toggle_expansion(&orders).request.is_none(), "{kind:?}: opening a table asks the driver nothing");
            assert_eq!(folder_titles(&state, &orders), expected, "{kind:?}");
            assert_eq!(state.loading_count(), 0, "{kind:?}");
            for folder in state.loaded_children(&orders).unwrap() {
                assert!(state.load_state(folder).is_none(), "{kind:?}: {folder:?} is not loaded");
            }
        }
        // A view, a materialized view or a sequence has columns only; MongoDB and Redis have no SQL tables.
        for kind in [KindOfObject::View, KindOfObject::MaterializedView, KindOfObject::Sequence] {
            let (mut state, orders) = table_of(ConnectionKind::Postgres);
            state.object_kinds.insert(orders.clone(), kind);
            state.toggle_expansion(&orders);
            assert_eq!(folder_titles(&state, &orders), ["Columns"], "{kind:?}");
        }
        for kind in [ConnectionKind::Mongodb, ConnectionKind::Redis] {
            assert!(TableFolder::for_kind(Some(kind), false).is_empty(), "{kind:?}");
        }
        // A driver that cannot describe a table has the Columns folder only.
        let (mut state, orders) = table_of(ConnectionKind::Postgres);
        state.set_table_describe(CONN, false);
        state.toggle_expansion(&orders);
        assert_eq!(folder_titles(&state, &orders), ["Columns"]);
    }

    #[test]
    fn a_folder_loads_when_opened_is_cached_and_a_refresh_loads_it_again() {
        let (mut state, orders) = table_of(ConnectionKind::Postgres);
        state.toggle_expansion(&orders);
        let indexes = orders.table_folder(TableFolder::Indexes).unwrap();
        let columns = orders.table_folder(TableFolder::Columns).unwrap();

        // Columns keep their own load; the other folders ask for the table's description.
        let request = state.toggle_expansion(&columns).request.unwrap();
        assert!(matches!(request, LoadRequest::Columns { .. }));
        assert_eq!(request.parent_id(), columns);
        let request = state.toggle_expansion(&indexes).request.unwrap();
        assert_eq!(
            request,
            LoadRequest::TableFolder {
                connection: CONN.into(),
                database: "app".into(),
                schema: "public".into(),
                object: "orders".into(),
                folder: TableFolder::Indexes,
            }
        );
        assert_eq!(request.parent_id(), indexes);
        assert_eq!(state.load_state(&indexes), Some(&NodeLoadState::Loading));
        let constraints = orders.table_folder(TableFolder::Constraints).unwrap();
        assert!(state.load_state(&constraints).is_none(), "the other folders wait for their own opening");

        let pkey = index_item(&indexes, "orders_pkey", "(id) \u{00B7} primary", Some("CREATE UNIQUE INDEX orders_pkey ..."));
        let item = pkey.id.clone();
        state.apply_load(&request, state.generation(), Ok(vec![pkey]));
        let rows = state.visible_rows();
        let at = rows.iter().position(|row| row.id == orders).unwrap();
        let below: Vec<(&str, usize)> = rows[at + 1..]
            .iter()
            .take_while(|row| row.depth > rows[at].depth)
            .map(|row| (row.label.as_str(), row.depth - rows[at].depth))
            .collect();
        assert_eq!(
            below,
            [("Columns", 1), ("Indexes", 1), ("orders_pkey", 2), ("Constraints", 1), ("Foreign Keys", 1), ("Triggers", 1)]
        );
        assert_eq!(state.table_item(&item).unwrap().definition.as_deref(), Some("CREATE UNIQUE INDEX orders_pkey ..."));
        assert!(!NavigatorNodeId::is_expandable(&item), "an index has nothing below it");

        // Collapsing and opening again does not ask again.
        state.toggle_expansion(&indexes);
        assert!(state.toggle_expansion(&indexes).request.is_none());
        assert_eq!(state.loaded_children(&indexes).unwrap(), std::slice::from_ref(&item));

        // A refresh of the table forgets the folders and what they listed; opening it again reloads.
        state.reset_node(&orders);
        assert!(state.table_item(&item).is_none());
        assert!(state.load_state(&indexes).is_none() && state.loaded_children(&indexes).is_none());
        state.toggle_expansion(&orders);
        assert!(state.toggle_expansion(&indexes).request.is_some());
    }

    #[test]
    fn the_search_matches_folder_rows_by_their_names_not_their_titles() {
        let (mut state, orders) = table_of(ConnectionKind::Postgres);
        state.toggle_expansion(&orders);
        state.toggle_expansion(&orders);
        let indexes = orders.table_folder(TableFolder::Indexes).unwrap();
        let request = state.toggle_expansion(&indexes).request.unwrap();
        let children = vec![index_item(&indexes, "orders_pkey", "(id)", None), index_item(&indexes, "orders_name_idx", "(name)", None)];
        state.apply_load(&request, state.generation(), Ok(children));
        state.toggle_expansion(&indexes);
        let labels = |state: &NavigatorState| state.visible_rows().into_iter().map(|row| row.label).collect::<Vec<_>>();

        // A loaded leaf is found with the path to it, like a column.
        state.set_search(SearchOptions { query: "name_idx".into(), ..Default::default() }).unwrap();
        let found = labels(&state);
        assert!(found.ends_with(&["orders".to_string(), "Indexes".into(), "orders_name_idx".into()]), "{found:?}");
        assert!(!found.contains(&"orders_pkey".to_string()));
        // A folder's title is not a name: "Index" finds no row below the table.
        state.set_search(SearchOptions { query: "Indexes".into(), ..Default::default() }).unwrap();
        assert!(labels(&state).is_empty(), "{:?}", labels(&state));
        // Matching the table itself keeps its folders and what they list.
        state.set_search(SearchOptions { query: "orders".into(), ..Default::default() }).unwrap();
        assert!(labels(&state).contains(&"Indexes".to_string()));
        // Cleared, the tree is as it was: the table is collapsed.
        state.set_search(SearchOptions::default()).unwrap();
        assert!(!labels(&state).contains(&"orders_pkey".to_string()));
    }

    // ---- tree state, database-type sections ------------------------------------------------

    fn labels_with_depth(state: &NavigatorState) -> Vec<(usize, String)> {
        state.visible_rows().into_iter().map(|row| (row.depth, row.label)).collect()
    }

    #[test]
    fn group_by_driver_lists_a_section_per_type_with_the_user_groups_inside() {
        let mut state = NavigatorState::new(vec![
            sqlite_conn(CONN, "lite one"),
            mongo_conn(CONN_B, "docs"),
            sqlite_conn(CONN_C, "lite two"),
        ]);
        let mut prefs = SidebarPreferences::default();
        prefs.add_group("work", "Work", None).unwrap();
        prefs.add_group("empty", "Empty", None).unwrap();
        prefs.move_connection(CONN, Some("work"), 0).unwrap();
        prefs.move_connection(CONN_B, Some("work"), 1).unwrap();
        state.set_preferences(prefs).unwrap();
        // Off: the groups at the top, as saved.
        assert_eq!(
            labels_with_depth(&state),
            [(0, "Work".into()), (1, "lite one".into()), (1, "docs".into()), (0, "Empty".into()), (0, "lite two".into())]
        );
        state.set_group_by_driver(true);
        // A section per type in a fixed order; a group shows inside the sections it has a
        // connection for and nowhere else (Empty has none).
        assert_eq!(
            labels_with_depth(&state),
            [
                (0, "SQLite".into()),
                (1, "Work".into()),
                (2, "lite one".into()),
                (1, "lite two".into()),
                (0, "MongoDB".into()),
                (1, "Work".into()),
                (2, "docs".into()),
            ]
        );
        // A section collapses and opens again with its rows.
        let sqlite = NavigatorNodeId::Driver { driver: "sqlite".into() };
        state.toggle_expansion(&sqlite);
        assert_eq!(
            labels_with_depth(&state),
            [(0, "SQLite".into()), (0, "MongoDB".into()), (1, "Work".into()), (2, "docs".into())]
        );
        state.toggle_expansion(&sqlite);
        assert_eq!(state.visible_rows().len(), 7);
        state.set_group_by_driver(false);
        assert_eq!(state.visible_rows().len(), 5);
    }

    #[test]
    fn open_nodes_are_listed_parents_first_capped_and_kept_for_the_connection_that_closed() {
        let mut state = three_expanded();
        let snapshot = state.tree_snapshot();
        assert_eq!(snapshot.len(), 3, "one entry per connection with open nodes");
        let (conn, nodes) = &snapshot[0];
        assert_eq!(conn, CONN);
        // connection, database, schema, Tables group: parents before children.
        assert_eq!(nodes.len(), 4);
        assert!(matches!(nodes[0], NavigatorNodeId::Connection { .. }));
        assert!(matches!(nodes[3], NavigatorNodeId::ObjectGroup { .. }));
        // The remembered form survives a JSON round trip (the session file keeps it that way).
        let json = serde_json::to_value(nodes).unwrap();
        let back: Vec<NavigatorNodeId> = serde_json::from_value(json).unwrap();
        assert_eq!(&back, nodes);

        // The tree is reset (a disconnect): what was open is remembered, the rows collapse.
        state.remember_tree(CONN);
        state.reset_node(&NavigatorNodeId::Connection { connection: CONN.into() });
        assert!(rows_of(&state, CONN).iter().all(|(_, expanded)| !expanded));
        assert_eq!(state.tree_snapshot()[0].1, *nodes, "still saved while it is closed");
        // A connection that no longer exists keeps nothing.
        state.set_connections(vec![sqlite_conn(CONN_B, "B"), sqlite_conn(CONN_C, "C")]);
        assert!(!state.has_remembered_tree(CONN));
        assert!(state.tree_snapshot().iter().all(|(id, _)| id != CONN));
    }

    #[test]
    fn remembered_nodes_open_as_their_parents_load_and_the_gone_ones_are_forgotten() {
        let mut state = three_expanded();
        let nodes = state.tree_snapshot().remove(0).1;
        state.reset_node(&NavigatorNodeId::Connection { connection: CONN.into() });
        state.set_remembered_tree(vec![(CONN.into(), nodes)]);
        assert!(state.has_remembered_tree(CONN));

        // The connection opens first: its databases are requested, nothing deeper exists yet.
        let requests = state.resume_tree(CONN);
        assert_eq!(requests, [LoadRequest::Databases { connection: CONN.into() }]);
        assert!(state.has_remembered_tree(CONN), "the rest waits for the databases");
        state.apply_load(&requests[0], state.generation(), Ok(vec![database_child(CONN, "app")]));
        let requests = state.resume_tree(CONN);
        assert_eq!(requests, [LoadRequest::Schemas { connection: CONN.into(), database: "app".into() }]);
        state.apply_load(&requests[0], state.generation(), Ok(vec![schema_child(CONN, "app", "public")]));
        // The schema opens at once, then the Tables group asks for its tables.
        let requests = state.resume_tree(CONN);
        assert!(matches!(requests.as_slice(), [LoadRequest::Objects { group: ObjectGroup::Tables, .. }]));
        state.apply_load(
            &requests[0],
            state.generation(),
            Ok(vec![object_child(CONN, "app", "public", "orders", KindOfObject::Table)]),
        );
        assert!(!state.has_remembered_tree(CONN), "everything is open again");
        assert!(state.visible_rows().iter().any(|row| row.label == "orders"));

        // A node the server no longer has is forgotten once nothing is loading.
        state.reset_node(&NavigatorNodeId::Connection { connection: CONN.into() });
        let gone = NavigatorNodeId::Database { connection: CONN.into(), database: "dropped".into() };
        state.set_remembered_tree(vec![(
            CONN.into(),
            vec![NavigatorNodeId::Connection { connection: CONN.into() }, gone],
        )]);
        let requests = state.resume_tree(CONN);
        state.apply_load(&requests[0], state.generation(), Ok(vec![database_child(CONN, "app")]));
        assert!(state.resume_tree(CONN).is_empty());
        assert!(!state.has_remembered_tree(CONN));
    }

    #[test]
    fn at_most_a_bounded_number_of_nodes_is_kept_per_connection() {
        let mut state = NavigatorState::new(vec![sqlite_conn(CONN, "A")]);
        let nodes: Vec<NavigatorNodeId> = (0..TREE_STATE_LIMIT + 50)
            .map(|index| NavigatorNodeId::Database { connection: CONN.into(), database: format!("db{index}") })
            .collect();
        state.set_remembered_tree(vec![(CONN.into(), nodes), ("not-a-connection".into(), Vec::new())]);
        assert_eq!(state.tree_snapshot()[0].1.len(), TREE_STATE_LIMIT);
        assert!(!state.has_remembered_tree("not-a-connection"));
    }
}
