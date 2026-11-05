use super::*;
use crate::sidebar_preferences::{ConnectionGroup, ConnectionSort};

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum SearchScope {
    #[default]
    All,
    Connections,
    Databases,
    Objects,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct SearchOptions {
    pub query: String,
    pub regex: bool,
    pub scope: SearchScope,
    pub connected_only: bool,
    pub show_hidden: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NavigatorAction {
    NewQuery,
    NewGroup,
    RenameGroup,
    MoveGroup,
    DeleteGroup,
    MoveConnection,
    HideConnection,
    DuplicateConnection,
    CopyConfig,
    ExportConfig,
    Connect,
    Disconnect,
    Refresh,
    Pin,
    CopyUri,
    Edit,
    Delete,
    Open,
    CopySql,
    /// Opens the source of a function, trigger, materialized view, ... in a read-only tab.
    ViewSource,
}

/// Kinds with a query editor to open from the sidebar (SQL or script).
fn has_query_editor(kind: crate::connections::ConnectionKind) -> bool {
    super::sql_driver::is_sql_kind(kind)
        || matches!(
            kind,
            crate::connections::ConnectionKind::Mongodb | crate::connections::ConnectionKind::Redis
        )
}

impl NavigatorState {
    pub fn reveal_connection_group(&mut self, connection: &str) {
        let mut group = self
            .preferences
            .connections
            .iter()
            .find(|item| item.id == connection)
            .and_then(|item| item.group.as_deref());
        let mut changed = false;
        while let Some(id) = group {
            changed |= self.collapsed_groups.remove(id);
            group = self
                .preferences
                .groups
                .iter()
                .find(|entry| entry.id == id)
                .and_then(|entry| entry.parent.as_deref());
        }
        if changed {
            self.rebuild_visible();
        }
    }

    pub fn visible_index(&self, id: &NavigatorNodeId) -> Option<usize> {
        self.visible_lookup.get(id).copied()
    }

    pub fn connection(&self, id: &str) -> Option<&SavedConnection> {
        self.connection_lookup
            .get(id)
            .and_then(|index| self.connections.get(*index))
    }

    /// The databases the sidebar lists under `connection` (SQL databases and
    /// provider nodes of kind `database`), in load order, honouring the visible
    /// object filter; empty while the connection's children are not loaded.
    /// Reads the cache only: it never loads.
    pub fn connection_databases(&self, connection: &str) -> Vec<(NavigatorNodeId, String)> {
        let root = NavigatorNodeId::Connection {
            connection: connection.to_string(),
        };
        let Some(Some(children)) = self.children.get(&root) else {
            return Vec::new();
        };
        children
            .iter()
            .filter(|child| match child {
                NavigatorNodeId::Database { .. } => true,
                NavigatorNodeId::Provider { .. } => self
                    .provider_nodes
                    .get(*child)
                    .is_some_and(|node| node.kind == "database"),
                _ => false,
            })
            .filter(|child| self.visible_filter_allows(child))
            .map(|child| {
                let label = self
                    .labels
                    .get(child)
                    .cloned()
                    .unwrap_or_else(|| default_label(child));
                (child.clone(), label)
            })
            .collect()
    }

    pub fn find_provider_child(
        &self,
        parent: &NavigatorNodeId,
        kind: &str,
        database: &str,
        collection: Option<&str>,
    ) -> Option<NavigatorNodeId> {
        self.children
            .get(parent)?
            .as_ref()?
            .iter()
            .find(|id| {
                self.provider_nodes.get(*id).is_some_and(|node| {
                    node.kind == kind
                        && node
                            .payload
                            .get("database")
                            .and_then(serde_json::Value::as_str)
                            == Some(database)
                        && collection.is_none_or(|name| {
                            node.payload
                                .get("collection")
                                .and_then(serde_json::Value::as_str)
                                == Some(name)
                        })
                })
            })
            .cloned()
    }

    pub fn has_provider_next_page(&self, parent: &NavigatorNodeId) -> bool {
        self.next_cursors.contains_key(parent)
    }
    pub fn preferences(&self) -> &SidebarPreferences {
        &self.preferences
    }

    pub fn search_options(&self) -> &SearchOptions {
        &self.search
    }

    pub fn visible_row_count(&self) -> usize {
        self.visible.len()
    }

    pub fn selected_ids(&self) -> &HashSet<NavigatorNodeId> {
        &self.selected_ids
    }

    pub fn exit_multi_selection(&mut self) -> bool {
        if !self.selection_mode {
            return false;
        }
        self.selection_mode = false;
        self.selected_ids.clear();
        if let Some(id) = &self.selected {
            self.selected_ids.insert(id.clone());
        }
        self.anchor = self.selected.clone();
        true
    }

    pub fn select_all_connections(&mut self) {
        self.selected_ids = self
            .visible
            .iter()
            .filter(|row| matches!(row.id, NavigatorNodeId::Connection { .. }))
            .map(|row| row.id.clone())
            .collect();
        self.selected = self
            .visible
            .iter()
            .find(|row| self.selected_ids.contains(&row.id))
            .map(|row| row.id.clone());
        self.anchor = self.selected.clone();
        self.selection_mode = !self.selected_ids.is_empty();
    }

    pub fn clear_multi_selection(&mut self) {
        self.selected_ids.clear();
        self.selected = None;
        self.anchor = None;
        self.selection_mode = false;
    }

    pub fn selected_connection_ids(&self) -> Option<Vec<String>> {
        if self.selected_ids.is_empty() {
            return None;
        }
        let mut ids = Vec::with_capacity(self.selected_ids.len());
        for row in &self.visible {
            if !self.selected_ids.contains(&row.id) {
                continue;
            }
            let NavigatorNodeId::Connection { connection } = &row.id else {
                return None;
            };
            ids.push(connection.clone());
        }
        (ids.len() == self.selected_ids.len()).then_some(ids)
    }

    pub fn selected_connection_count(&self) -> usize {
        if self.selected_ids.iter().all(|id| {
            matches!(id, NavigatorNodeId::Connection { .. }) && self.visible_lookup.contains_key(id)
        }) {
            self.selected_ids.len()
        } else {
            0
        }
    }

    pub fn connection_multi_mode(&self) -> bool {
        self.selection_mode && self.selected_connection_count() > 0
    }

    pub fn selection_in_visible_order(&self) -> Vec<NavigatorNodeId> {
        self.visible
            .iter()
            .filter(|row| self.selected_ids.contains(&row.id))
            .map(|row| row.id.clone())
            .collect()
    }

    pub fn set_search(&mut self, options: SearchOptions) -> Result<(), regex::Error> {
        // An invalid pattern is an explicit "no matches" state, not a
        // reason to keep showing the previous result.
        let compiled = if options.regex && !options.query.is_empty() {
            regex::RegexBuilder::new(&options.query)
                .case_insensitive(true)
                .build()
                .map(Some)
        } else {
            Ok(None)
        };
        let (matcher, error) = match compiled {
            Ok(matcher) => (matcher, None),
            Err(error) => (None, Some(error)),
        };
        self.matcher = matcher;
        self.regex_invalid = error.is_some();
        if options.query.is_empty() && options.scope == SearchScope::All {
            self.search_collapsed.clear();
        }
        self.search = options;
        self.rebuild_visible();
        error.map_or(Ok(()), Err)
    }

    pub fn set_connected_ids(&mut self, ids: HashSet<String>) {
        self.connected_ids = ids;
        self.rebuild_visible();
    }

    /// The assistant dock's open state and width. Only these two fields: a full
    /// `set_preferences` would validate and compile every visible-filter pattern again.
    pub fn set_assistant_layout(&mut self, open: bool, width: f32) {
        self.preferences.set_assistant_open(open);
        self.preferences.set_assistant_width(width);
    }

    pub fn set_preferences(&mut self, preferences: SidebarPreferences) -> Result<(), String> {
        preferences.validate()?;
        let matchers = preferences
            .visible_filters
            .iter()
            .map(|(connection, filter)| {
                let compiled = filter
                    .patterns
                    .iter()
                    .map(|pattern| {
                        regex::RegexBuilder::new(pattern)
                            .case_insensitive(true)
                            .build()
                            .map_err(|_| "Invalid visible database pattern".to_string())
                    })
                    .collect::<Result<Vec<_>, _>>()?;
                Ok((connection.clone(), compiled))
            })
            .collect::<Result<HashMap<_, _>, String>>()?;
        let names = preferences
            .visible_filters
            .iter()
            .filter_map(|(connection, filter)| {
                filter.databases.as_ref().map(|names| {
                    (
                        connection.clone(),
                        names.iter().cloned().collect::<HashSet<_>>(),
                    )
                })
            })
            .collect();
        let schemas = preferences
            .visible_filters
            .iter()
            .map(|(connection, filter)| {
                (
                    connection.clone(),
                    filter
                        .schemas
                        .iter()
                        .filter_map(|(database, names)| {
                            names.as_ref().map(|names| {
                                (
                                    database.clone(),
                                    names.iter().cloned().collect::<HashSet<_>>(),
                                )
                            })
                        })
                        .collect::<HashMap<_, _>>(),
                )
            })
            .collect();
        let ids: HashSet<_> = preferences
            .groups
            .iter()
            .map(|group| group.id.as_str())
            .collect();
        self.collapsed_groups.retain(|id| ids.contains(id.as_str()));
        self.preferences = preferences;
        self.visible_filter_matchers = matchers;
        self.visible_filter_names = names;
        self.visible_schema_names = schemas;
        // The visible-object filter decides which databases are listed.
        self.catalog_revision += 1;
        self.rebuild_visible();
        Ok(())
    }

    pub fn select_visible(&mut self, id: &NavigatorNodeId, additive: bool, range: bool) -> bool {
        let Some(index) = self.visible_index(id) else {
            return false;
        };
        if range {
            self.selection_mode = true;
            let anchor = self
                .anchor
                .as_ref()
                .and_then(|anchor| self.visible_index(anchor))
                .unwrap_or(index);
            self.selected_ids = self.visible[anchor.min(index)..=anchor.max(index)]
                .iter()
                .map(|row| row.id.clone())
                .collect();
            if self.anchor.is_none() {
                self.anchor = Some(id.clone());
            }
        } else if additive {
            self.selection_mode = true;
            if !self.selected_ids.insert(id.clone()) {
                self.selected_ids.remove(id);
            }
            self.anchor = Some(id.clone());
        } else {
            self.selection_mode = false;
            self.selected_ids.clear();
            self.selected_ids.insert(id.clone());
            self.anchor = Some(id.clone());
        }
        self.selected = if self.selected_ids.contains(id) {
            Some(id.clone())
        } else {
            self.visible
                .iter()
                .find(|row| self.selected_ids.contains(&row.id))
                .map(|row| row.id.clone())
        };
        true
    }

    pub fn move_focus(&mut self, direction: isize, extend: bool) -> bool {
        if self.visible.is_empty() {
            return false;
        }
        let current = self
            .selected
            .as_ref()
            .and_then(|id| self.visible_index(id))
            .unwrap_or(0);
        let next = current
            .saturating_add_signed(direction)
            .min(self.visible.len() - 1);
        let id = self.visible[next].id.clone();
        self.select_visible(&id, false, extend)
    }

    pub fn action_target(&self, id: &NavigatorNodeId) -> Option<NavigatorNodeId> {
        self.visible_lookup.contains_key(id).then(|| id.clone())
    }

    pub fn supports_action(
        &self,
        id: &NavigatorNodeId,
        connected: bool,
        action: NavigatorAction,
    ) -> bool {
        if !self.visible_lookup.contains_key(id) {
            return false;
        }
        match action {
            NavigatorAction::Connect | NavigatorAction::Disconnect | NavigatorAction::Refresh => {
                let NavigatorNodeId::Connection { connection } = id else {
                    return false;
                };
                self.connection(connection).is_some_and(|saved| {
                    !saved.disabled
                        && match action {
                            NavigatorAction::Connect => !connected,
                            NavigatorAction::Disconnect => connected,
                            NavigatorAction::Refresh => true,
                            _ => unreachable!(),
                        }
                })
            }
            NavigatorAction::NewQuery => match id {
                NavigatorNodeId::Connection { connection } => self
                    .connection(connection)
                    .is_some_and(|saved| !saved.disabled && has_query_editor(saved.kind)),
                NavigatorNodeId::Database { .. }
                | NavigatorNodeId::Schema { .. }
                | NavigatorNodeId::Object { .. } => true,
                NavigatorNodeId::Provider { .. } => {
                    self.provider_collection_can_open(id) || self.provider_database_can_query(id)
                }
                _ => false,
            },
            NavigatorAction::Open => {
                matches!(
                    id,
                    NavigatorNodeId::Object { .. } | NavigatorNodeId::SchemaObject { .. }
                ) || self.provider_collection_can_open(id)
            }
            _ => self.available_actions(id, connected).contains(&action),
        }
    }

    fn provider_collection_can_open(&self, id: &NavigatorNodeId) -> bool {
        self.provider_node(id).is_some_and(|provider| {
            provider.kind == "collection"
                && provider.actions.iter().any(|action| action == "open")
                && provider
                    .payload
                    .get("database")
                    .and_then(serde_json::Value::as_str)
                    .is_some()
                && provider
                    .payload
                    .get("collection")
                    .and_then(serde_json::Value::as_str)
                    .is_some()
        })
    }

    fn provider_database_can_query(&self, id: &NavigatorNodeId) -> bool {
        let NavigatorNodeId::Provider { connection, .. } = id else {
            return false;
        };
        self.connection(connection).is_some_and(|saved| {
            !saved.disabled
                && matches!(
                    saved.kind,
                    crate::connections::ConnectionKind::Mongodb
                        | crate::connections::ConnectionKind::Redis
                )
        }) && self.provider_node(id).is_some_and(|node| {
            node.kind == "database"
                && node
                    .payload
                    .get("database")
                    .and_then(serde_json::Value::as_str)
                    .is_some_and(|name| !name.is_empty())
        })
    }

    pub fn available_actions(&self, id: &NavigatorNodeId, connected: bool) -> Vec<NavigatorAction> {
        if self.action_target(id).is_none() {
            return Vec::new();
        }
        match id {
            NavigatorNodeId::Connection { connection } => {
                let Some(saved) = self.connection(connection) else {
                    return Vec::new();
                };
                let mut actions = if saved.disabled {
                    Vec::new()
                } else if connected {
                    vec![NavigatorAction::Disconnect, NavigatorAction::Refresh]
                } else {
                    vec![NavigatorAction::Connect, NavigatorAction::Refresh]
                };
                actions.extend([
                    NavigatorAction::Pin,
                    NavigatorAction::CopyUri,
                    NavigatorAction::Edit,
                    NavigatorAction::Delete,
                    NavigatorAction::MoveConnection,
                    NavigatorAction::HideConnection,
                    NavigatorAction::DuplicateConnection,
                    NavigatorAction::CopyConfig,
                    NavigatorAction::ExportConfig,
                ]);
                if !saved.disabled && has_query_editor(saved.kind) {
                    actions.push(NavigatorAction::NewQuery);
                }
                actions
            }
            NavigatorNodeId::Group { .. } => vec![
                NavigatorAction::NewGroup,
                NavigatorAction::RenameGroup,
                NavigatorAction::MoveGroup,
                NavigatorAction::DeleteGroup,
            ],
            NavigatorNodeId::Database { .. } | NavigatorNodeId::Schema { .. } => {
                vec![NavigatorAction::NewQuery]
            }
            NavigatorNodeId::Object { .. } => {
                let mut actions = vec![
                    NavigatorAction::Open,
                    NavigatorAction::CopySql,
                    NavigatorAction::NewQuery,
                ];
                // A materialized view has a definition worth reading.
                if matches!(
                    self.object_kind(id),
                    Some(KindOfObject::MaterializedView | KindOfObject::Sequence)
                ) {
                    actions.push(NavigatorAction::ViewSource);
                }
                actions
            }
            NavigatorNodeId::SchemaObject { .. } => {
                vec![NavigatorAction::Open, NavigatorAction::ViewSource]
            }
            NavigatorNodeId::Provider { .. } => {
                if self.provider_collection_can_open(id) {
                    vec![NavigatorAction::Open, NavigatorAction::NewQuery]
                } else if self.provider_database_can_query(id) {
                    vec![NavigatorAction::NewQuery]
                } else {
                    Vec::new()
                }
            }
            _ => Vec::new(),
        }
    }

    pub(super) fn rebuild_visible(&mut self) {
        let mut rows = Vec::new();
        let mut visited = HashSet::new();
        let displays: HashMap<_, _> = self
            .preferences
            .connections
            .iter()
            .map(|item| (item.id.as_str(), item))
            .collect();
        let orders: HashMap<_, _> = self
            .preferences
            .connections
            .iter()
            .filter_map(|item| {
                uuid::Uuid::parse_str(&item.id)
                    .ok()
                    .map(|id| (id, item.order))
            })
            .collect();
        let mut buckets: HashMap<Option<&str>, Vec<&SavedConnection>> = HashMap::new();
        for connection in &self.connections {
            let id = connection.id.to_string();
            let display = displays.get(id.as_str()).copied();
            if (!self.search.show_hidden && display.is_some_and(|item| item.hidden))
                || (self.search.connected_only && !self.connected_ids.contains(&id))
            {
                continue;
            }
            buckets
                .entry(display.and_then(|item| item.group.as_deref()))
                .or_default()
                .push(connection);
        }
        for bucket in buckets.values_mut() {
            match self.preferences.sort {
                ConnectionSort::Manual => {
                    bucket.sort_by_key(|conn| orders.get(&conn.id).copied().unwrap_or(usize::MAX))
                }
                ConnectionSort::Ascending => {
                    bucket.sort_by_cached_key(|conn| conn.name.to_lowercase())
                }
                ConnectionSort::Descending => {
                    bucket.sort_by_cached_key(|conn| std::cmp::Reverse(conn.name.to_lowercase()))
                }
            }
        }
        let mut groups: HashMap<Option<&str>, Vec<&ConnectionGroup>> = HashMap::new();
        for group in &self.preferences.groups {
            groups
                .entry(group.parent.as_deref())
                .or_default()
                .push(group);
        }
        self.append_group(None, 0, &mut rows, &mut visited, &groups, &buckets, &orders);
        self.visible = rows;
        self.visible_lookup = self
            .visible
            .iter()
            .enumerate()
            .map(|(index, row)| (row.id.clone(), index))
            .collect();
        self.selected_ids
            .retain(|id| self.visible_lookup.contains_key(id));
        if self.selected_ids.is_empty() {
            self.selection_mode = false;
        }
        if self
            .selected
            .as_ref()
            .is_some_and(|id| !self.visible_lookup.contains_key(id))
        {
            self.selected = self
                .visible
                .iter()
                .find(|row| self.selected_ids.contains(&row.id))
                .map(|row| row.id.clone());
        }
        if self
            .anchor
            .as_ref()
            .is_some_and(|id| !self.visible_lookup.contains_key(id))
        {
            self.anchor = self.selected.clone();
        }
    }

    #[allow(clippy::too_many_arguments)] // each parameter is one input of the operation; a parameter struct would only rename the list
    fn append_group(
        &self,
        parent: Option<&str>,
        depth: usize,
        rows: &mut Vec<NavigatorRow>,
        visited: &mut HashSet<NavigatorNodeId>,
        groups: &HashMap<Option<&str>, Vec<&ConnectionGroup>>,
        buckets: &HashMap<Option<&str>, Vec<&SavedConnection>>,
        orders: &HashMap<uuid::Uuid, usize>,
    ) {
        enum Entry<'a> {
            Group(&'a ConnectionGroup),
            Connection(&'a SavedConnection),
        }
        let mut entries: Vec<(usize, Entry<'_>)> = groups
            .get(&parent)
            .into_iter()
            .flatten()
            .map(|group| (group.order, Entry::Group(group)))
            .collect();
        if let Some(connections) = buckets.get(&parent) {
            let mut slots = connections
                .iter()
                .map(|conn| orders.get(&conn.id).copied().unwrap_or(usize::MAX))
                .collect::<Vec<_>>();
            slots.sort_unstable();
            entries.extend(
                slots.into_iter().zip(
                    connections
                        .iter()
                        .map(|connection| Entry::Connection(connection)),
                ),
            );
        }
        entries.sort_by_key(|(order, _)| *order);
        for (_, entry) in entries {
            let Entry::Group(group) = entry else {
                if let Entry::Connection(connection) = entry {
                    self.append_connection(connection, depth, rows, visited);
                }
                continue;
            };
            let id = NavigatorNodeId::Group {
                id: group.id.clone(),
            };
            let start = rows.len();
            rows.push(NavigatorRow {
                id: id.clone(),
                label: group.name.clone(),
                depth,
                expandable: true,
                expanded: true,
                load: None,
                disabled: false,
            });
            if (self.search_active() && !self.search_collapsed.contains(&id))
                || (!self.search_active() && !self.collapsed_groups.contains(&group.id))
            {
                self.append_group(
                    Some(&group.id),
                    depth + 1,
                    rows,
                    visited,
                    groups,
                    buckets,
                    orders,
                );
            }
            let matched =
                self.match_label(&group.name).is_some() && self.search.scope == SearchScope::All;
            if self.search_active()
                && !matched
                && rows.len() == start + 1
                && !self.search_collapsed.contains(&id)
            {
                rows.truncate(start);
            } else {
                rows[start].expanded = if self.search_active() {
                    !self.search_collapsed.contains(&id)
                } else {
                    !self.collapsed_groups.contains(&group.id)
                };
            }
        }
    }

    fn append_connection(
        &self,
        connection: &SavedConnection,
        depth: usize,
        rows: &mut Vec<NavigatorRow>,
        visited: &mut HashSet<NavigatorNodeId>,
    ) {
        let id = NavigatorNodeId::Connection {
            connection: connection.id.to_string(),
        };
        let start = rows.len();
        rows.push(NavigatorRow {
            id: id.clone(),
            label: connection.name.clone(),
            depth,
            expandable: true,
            expanded: self.expanded.contains(&id),
            load: self.load_states.get(&id).cloned(),
            disabled: connection.disabled,
        });
        if !self.search_collapsed.contains(&id)
            && (self.expanded.contains(&id) || self.search_active())
        {
            self.append_loaded_children(
                &id,
                depth + 1,
                rows,
                visited,
                !self.search.query.trim().is_empty() && self.matches(&id, &connection.name),
            );
        }
        if self.search_active()
            && !self.matches(&id, &connection.name)
            && rows.len() == start + 1
            && !self.search_collapsed.contains(&id)
        {
            rows.truncate(start);
        } else {
            let expanded = rows.len() > start + 1;
            rows[start].expanded =
                !self.search_collapsed.contains(&id) && (self.expanded.contains(&id) || expanded);
        }
    }

    fn append_loaded_children(
        &self,
        parent: &NavigatorNodeId,
        depth: usize,
        rows: &mut Vec<NavigatorRow>,
        visited: &mut HashSet<NavigatorNodeId>,
        inherited: bool,
    ) {
        if !visited.insert(parent.clone()) {
            return;
        }
        if let Some(Some(children)) = self.children.get(parent) {
            for child in children {
                if !self.visible_filter_allows(child) {
                    continue;
                }
                let start = rows.len();
                let mut label = self
                    .labels
                    .get(child)
                    .cloned()
                    .unwrap_or_else(|| default_label(child));
                // A Redis database under a key filter says so.
                if let Some(filter) = self
                    .redis_filters
                    .get(child)
                    .filter(|filter| filter.narrows())
                {
                    label = format!("{label} \u{00B7} {}", filter.summary());
                }
                let expandable = self.is_expandable(child);
                let matched = self.matches(child, &label);
                rows.push(NavigatorRow {
                    id: child.clone(),
                    label,
                    depth,
                    expandable,
                    expanded: expandable && self.expanded.contains(child),
                    load: self.load_states.get(child).cloned(),
                    disabled: false,
                });
                if expandable
                    && !self.search_collapsed.contains(child)
                    && (self.expanded.contains(child) || self.search_active())
                {
                    self.append_loaded_children(
                        child,
                        depth + 1,
                        rows,
                        visited,
                        inherited || (!self.search.query.trim().is_empty() && matched),
                    );
                }
                if self.search_active()
                    && !matched
                    && !inherited
                    && rows.len() == start + 1
                    && !self.search_collapsed.contains(child)
                {
                    rows.truncate(start);
                } else {
                    let expanded = rows.len() > start + 1;
                    rows[start].expanded = expandable
                        && !self.search_collapsed.contains(child)
                        && (self.expanded.contains(child) || expanded);
                }
            }
        }
        if !self.search_active() && self.next_cursors.contains_key(parent) {
            rows.push(NavigatorRow {
                id: self.load_more_id(parent),
                label: "Load More".into(),
                depth,
                expandable: false,
                expanded: false,
                load: self.load_states.get(parent).cloned(),
                disabled: false,
            });
        }
        visited.remove(parent);
    }

    fn visible_filter_allows(&self, id: &NavigatorNodeId) -> bool {
        let (connection, database, schema) = match id {
            NavigatorNodeId::Database {
                connection,
                database,
            } => (connection, database.as_str(), None),
            NavigatorNodeId::Schema {
                connection,
                database,
                schema,
            } => (connection, database.as_str(), Some(schema.as_str())),
            NavigatorNodeId::Provider { connection, node } => {
                let Some(provider) = self.provider_nodes.get(id) else {
                    return true;
                };
                if provider.kind != "database" {
                    return true;
                }
                let database = provider
                    .payload
                    .get("database")
                    .and_then(serde_json::Value::as_str)
                    .or_else(|| self.labels.get(id).map(String::as_str))
                    .unwrap_or(node);
                (connection, database, None)
            }
            _ => return true,
        };
        let Some(filter) = self.preferences.visible_filters.get(connection) else {
            return !(self.hide_system_by_default
                && schema.is_none()
                && crate::sidebar_preferences::is_system_database(database));
        };
        if let Some(schema) = schema {
            return self
                .visible_schema_names
                .get(connection)
                .and_then(|by_db| by_db.get(database))
                .is_none_or(|names| names.contains(schema));
        }
        if !filter.show_system && crate::sidebar_preferences::is_system_database(database) {
            return false;
        }
        filter.databases.as_ref().is_none_or(|_| {
            self.visible_filter_names
                .get(connection)
                .is_some_and(|names| names.contains(database))
                || self
                    .visible_filter_matchers
                    .get(connection)
                    .is_some_and(|patterns| {
                        patterns.iter().any(|pattern| pattern.is_match(database))
                    })
        })
    }

    pub(super) fn search_active(&self) -> bool {
        !self.search.query.is_empty() || self.search.scope != SearchScope::All
    }

    fn matches(&self, id: &NavigatorNodeId, label: &str) -> bool {
        let scope = match id {
            NavigatorNodeId::Connection { .. } => SearchScope::Connections,
            NavigatorNodeId::Database { .. } => SearchScope::Databases,
            NavigatorNodeId::Object { .. }
            | NavigatorNodeId::SchemaObject { .. }
            | NavigatorNodeId::Column { .. } => SearchScope::Objects,
            NavigatorNodeId::Provider { .. } => {
                match self.provider_node(id).map(|node| node.kind.as_str()) {
                    Some("database") => SearchScope::Databases,
                    _ => SearchScope::Objects,
                }
            }
            _ => SearchScope::All,
        };
        (self.search.scope == SearchScope::All || scope == self.search.scope)
            && self.match_label(label).is_some()
    }

    fn match_label(&self, label: &str) -> Option<u8> {
        let query = self.search.query.trim();
        if query.is_empty() {
            return Some(0);
        }
        if self.regex_invalid {
            return None;
        }
        if let Some(regex) = &self.matcher {
            return regex.is_match(label).then_some(95);
        }
        let label_lower = label.to_lowercase();
        let query_lower = query.to_lowercase();
        if label_lower == query_lower {
            return Some(100);
        }
        if label_lower.starts_with(&query_lower) {
            return Some(90);
        }
        if label_lower.contains(&query_lower) {
            return Some(70);
        }
        let mut chars = query_lower.chars();
        let mut next = chars.next();
        for character in label_lower.chars() {
            if next == Some(character) {
                next = chars.next();
            }
        }
        (next.is_none()).then_some(40)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::sidebar_preferences::VisibleObjectFilter;

    /// The assistant dock's open state and width are two fields; storing them must not validate
    /// and compile every visible-filter pattern of every connection again (it runs at every toggle
    /// and resize, on the UI thread).
    #[test]
    fn the_assistant_layout_is_stored_without_compiling_the_visible_filters_again() {
        let mut state = NavigatorState::new(Vec::new());
        let mut preferences = SidebarPreferences::default();
        preferences
            .set_visible_filter(
                "conn",
                VisibleObjectFilter {
                    databases: Some(Vec::new()),
                    patterns: vec!["^app_.*".into()],
                    ..VisibleObjectFilter::default()
                },
            )
            .unwrap();
        state.set_preferences(preferences).unwrap();
        let compiled =
            |state: &NavigatorState| state.visible_filter_matchers["conn"][0].as_str().as_ptr();
        let before = compiled(&state);

        state.set_assistant_layout(true, 512.0);
        assert!(state.preferences().assistant_open);
        assert_eq!(state.preferences().resolve_assistant_width(), 512.0);
        assert_eq!(compiled(&state), before, "the pattern was compiled again");
        // A width that is not a number is refused, as in the preferences.
        state.set_assistant_layout(false, f32::NAN);
        assert!(!state.preferences().assistant_open);
        assert_eq!(state.preferences().resolve_assistant_width(), 512.0);

        // Control: a full replacement does compile them again.
        let same = state.preferences().clone();
        state.set_preferences(same).unwrap();
        assert_ne!(
            compiled(&state),
            before,
            "the control does not detect a rebuild"
        );
    }
}
