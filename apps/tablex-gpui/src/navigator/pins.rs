//! Rows pinned to the top of their siblings.
//!
//! A pin belongs to the row's exact place (its whole [`NavigatorNodeId`]), so `orders` pinned in
//! one database is not pinned in another, nor on another connection. Connections pin through
//! `SavedConnection::is_pinned` (the saved connection record); every other row is kept here, in
//! memory, with the order the rows were pinned in. The app loads the stored pins once, off the UI
//! thread, and writes each change as it is made: nothing here touches storage, and listing the
//! rows costs a hash lookup per child of a connection that has pins and nothing for the others.

use super::*;

/// The provider rows that can be pinned: the databases, schemas and tables of a driver whose
/// catalog is listed as provider nodes (MongoDB's collections included).
const PINNABLE_PROVIDER_KINDS: [&str; 6] =
    ["database", "schema", "collection", "table", "view", "materialized_view"];

impl NavigatorNodeId {
    /// What a stored pin keeps of this row within its connection (a JSON array of the kind and the
    /// names of its path); `None` for a row that cannot be pinned this way (a connection pins
    /// through its saved record).
    pub fn pin_key(&self) -> Option<String> {
        let parts: Vec<&str> = match self {
            Self::Database { database, .. } => vec!["database", database],
            Self::Schema { database, schema, .. } => vec!["schema", database, schema],
            Self::Object { database, schema, object, .. } => vec!["object", database, schema, object],
            Self::Provider { node, .. } => vec!["node", node],
            _ => return None,
        };
        serde_json::to_string(&parts).ok()
    }

    /// The row a stored pin of `connection` names; `None` for a key this build does not know.
    pub fn from_pin_key(connection: &str, key: &str) -> Option<Self> {
        let parts: Vec<String> = serde_json::from_str(key).ok()?;
        let connection = connection.to_string();
        match parts.as_slice() {
            [kind, database] if kind == "database" => {
                Some(Self::Database { connection, database: database.clone() })
            }
            [kind, node] if kind == "node" => Some(Self::Provider { connection, node: node.clone() }),
            [kind, database, schema] if kind == "schema" => Some(Self::Schema {
                connection,
                database: database.clone(),
                schema: schema.clone(),
            }),
            [kind, database, schema, object] if kind == "object" => Some(Self::Object {
                connection,
                database: database.clone(),
                schema: schema.clone(),
                object: object.clone(),
            }),
            _ => None,
        }
    }
}

impl NavigatorState {
    /// Whether the row's menu offers Pin: connections, databases, schemas, tables, views and
    /// collections.
    pub fn is_pinnable(&self, id: &NavigatorNodeId) -> bool {
        match id {
            NavigatorNodeId::Connection { .. }
            | NavigatorNodeId::Database { .. }
            | NavigatorNodeId::Schema { .. }
            | NavigatorNodeId::Object { .. } => true,
            NavigatorNodeId::Provider { .. } => self
                .provider_nodes
                .get(id)
                .is_some_and(|node| PINNABLE_PROVIDER_KINDS.contains(&node.kind.as_str())),
            _ => false,
        }
    }

    pub fn is_pinned(&self, id: &NavigatorNodeId) -> bool {
        match id {
            NavigatorNodeId::Connection { connection } => {
                self.connection(connection).is_some_and(|saved| saved.is_pinned)
            }
            _ => self.node_pins.contains_key(id),
        }
    }

    /// Replaces the pins of the other rows with `ids`, the first pinned first (a pin of a row that
    /// does not exist is kept and never listed).
    pub fn set_node_pins(&mut self, ids: impl IntoIterator<Item = NavigatorNodeId>) {
        self.node_pins.clear();
        self.next_pin = 0;
        for id in ids {
            if !matches!(id, NavigatorNodeId::Connection { .. }) {
                self.next_pin += 1;
                self.node_pins.entry(id).or_insert(self.next_pin);
            }
        }
        self.pins_changed();
    }

    /// Pins (after the others) or unpins one row that is not a connection; whether anything
    /// changed.
    pub fn set_node_pinned(&mut self, id: &NavigatorNodeId, pinned: bool) -> bool {
        if matches!(id, NavigatorNodeId::Connection { .. }) || !self.is_pinnable(id) {
            return false;
        }
        let changed = if pinned {
            // A row that is pinned keeps its place.
            let fresh = !self.node_pins.contains_key(id);
            if fresh {
                self.next_pin += 1;
                self.node_pins.insert(id.clone(), self.next_pin);
            }
            fresh
        } else {
            self.node_pins.remove(id).is_some()
        };
        if changed {
            self.pins_changed();
        }
        changed
    }

    fn pins_changed(&mut self) {
        self.pinned_connections = self
            .node_pins
            .keys()
            .filter_map(|id| node_connection(id).map(str::to_owned))
            .collect();
        self.generation += 1;
        self.rebuild_visible();
    }

    /// The pinned ones of `children` (the children of `parent`), in the order they were pinned;
    /// empty, without a lookup per child, when `parent`'s connection has no pins.
    pub(super) fn pinned_children<'a>(
        &self,
        parent: &NavigatorNodeId,
        children: &'a [NavigatorNodeId],
    ) -> Vec<&'a NavigatorNodeId> {
        if !node_connection(parent).is_some_and(|connection| self.pinned_connections.contains(connection)) {
            return Vec::new();
        }
        let mut pinned: Vec<(u64, &NavigatorNodeId)> = children
            .iter()
            .filter_map(|child| self.node_pins.get(child).map(|slot| (*slot, child)))
            .collect();
        pinned.sort_unstable_by_key(|(slot, _)| *slot);
        pinned.into_iter().map(|(_, child)| child).collect()
    }
}
