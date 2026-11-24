//! Copy and Paste of tables between connections, in the tree.
//!
//! Copy (Cmd+C, or the row menu) on tables, views and materialized views of a SQL connection remembers them in an app-internal tree
//! clipboard (and puts their qualified names on the system clipboard as text). Paste (Cmd+V on the focused tree, or the row menu) on a
//! connection, database or schema opens the Data Transfer tab the usual way (`open_data_transfer`) with the copied tables as its
//! sources and that namespace as its target; the tables are created there when they are missing. Nothing runs until the user goes
//! through the wizard and starts the transfer.
//!
//! One transfer has one source namespace, so a copy keeps the tables of the namespace of the first one and says that it left the
//! others out. A MongoDB collection has no transfer to a table: it keeps copying its name only.

use super::data_transfer::view::TransferInit;
use super::data_transfer::Namespace;
use super::sidebar_names::NameStyle;
use super::*;
use crate::sql_ddl::dialect_for;

/// The tables of one namespace the tree remembers.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct TreeClip {
    pub(super) source: Namespace,
    pub(super) tables: Vec<String>,
}

impl AppRoot {
    /// Whether `id` is a table, view or materialized view of a connection that can be transferred (a SQL one).
    fn tree_copyable(&self, id: &NavigatorNodeId) -> bool {
        let NavigatorNodeId::Object { connection, .. } = id else { return false };
        object_actions::ObjectKind::of(self.navigator.object_kind(id)).is_some()
            && self.saved_connections.iter().find(|saved| saved.matches_id(connection)).is_some_and(|saved| dialect_for(saved.kind).is_some())
    }

    /// Cmd+C / Copy on `ids`: when they are all tables or views of a SQL connection, the tree clipboard takes them (those of the namespace
    /// of the first) and their qualified names go to the system clipboard. `false`: they are not, nothing was copied here.
    pub(super) fn copy_tree_objects(&mut self, ids: &[NavigatorNodeId], cx: &mut Context<Self>) -> bool {
        if ids.is_empty() || !ids.iter().all(|id| self.tree_copyable(id)) {
            return false;
        }
        let Some(origin) = self.object_origin(&ids[0]) else { return false };
        let source = Namespace { connection: origin.connection, database: origin.database, schema: origin.schema };
        let (kept, left_out): (Vec<&NavigatorNodeId>, Vec<&NavigatorNodeId>) = ids
            .iter()
            .partition(|id| self.object_origin(id).is_some_and(|other| other.connection == source.connection && other.database == source.database && other.schema == source.schema));
        let tables: Vec<String> = kept
            .iter()
            .filter_map(|id| match id {
                NavigatorNodeId::Object { object, .. } => Some(object.clone()),
                _ => None,
            })
            .collect();
        let kept: Vec<NavigatorNodeId> = kept.into_iter().cloned().collect();
        self.copy_navigator_names(&kept, NameStyle::Qualified, cx);
        self.navigator_ui.clipboard = Some(TreeClip { source, tables });
        if !left_out.is_empty() {
            self.sidebar.notice = Some(t!("sidebar.tree_copy_left_out", count = left_out.len()).into_owned());
        }
        cx.notify();
        true
    }

    /// The rows Cmd+C acts on: the selection, else the focused row.
    pub(super) fn tree_copy_selection(&self) -> Vec<NavigatorNodeId> {
        let selection = self.navigator.selection_in_visible_order();
        if selection.is_empty() {
            self.navigator.selected().cloned().into_iter().collect()
        } else {
            selection
        }
    }

    /// Whether the tree clipboard holds tables.
    pub(super) fn tree_clipboard_filled(&self) -> bool {
        self.navigator_ui.clipboard.as_ref().is_some_and(|clip| !clip.tables.is_empty())
    }

    /// The namespace a Paste on the row `id` writes to: a connection's first one the tree lists, a database's default schema, a schema, or
    /// the schema of a folder. `None`: the row is not a place tables can be pasted into.
    pub(super) fn paste_target(&self, id: &NavigatorNodeId) -> Option<Namespace> {
        let connection = match id {
            NavigatorNodeId::Connection { connection } | NavigatorNodeId::Database { connection, .. } | NavigatorNodeId::Schema { connection, .. } => connection,
            NavigatorNodeId::ObjectGroup { connection, group: ObjectGroup::Tables, .. } => connection,
            _ => return None,
        };
        let kind = self.saved_connections.iter().find(|saved| saved.matches_id(connection))?.kind;
        dialect_for(kind)?;
        match id {
            NavigatorNodeId::Connection { connection } => {
                let none = Namespace { connection: connection.clone(), database: String::new(), schema: String::new() };
                Some(self.transfer_namespaces(connection, &none).into_iter().next().unwrap_or(none))
            }
            _ => {
                let origin = self.object_origin(id)?;
                Some(Namespace { connection: origin.connection, database: origin.database, schema: origin.schema })
            }
        }
    }

    /// Paste on the row `id`: opens the Data Transfer tab with the copied tables as its sources and the row's namespace as its target.
    pub(super) fn paste_tree_objects(&mut self, id: &NavigatorNodeId, window: &mut Window, cx: &mut Context<Self>) {
        let Some(clip) = self.navigator_ui.clipboard.clone().filter(|clip| !clip.tables.is_empty()) else { return };
        let Some(target) = self.paste_target(id) else { return };
        // The wizard writes to the open connections only: a connection that is not open opens first.
        if self.scenario_ui.service.is_none() && !self.driver_runtime.sessions.contains_key(&target.connection) {
            self.ensure_connected(target.connection.clone(), None, cx);
            self.sidebar.notice = Some(t!("sidebar.paste_connecting").into_owned());
            return cx.notify();
        }
        let init = TransferInit { source: clip.source, tables: clip.tables, all: false, to_file: false, query: None };
        self.open_data_transfer_to(init, Some(target), window, cx);
    }
}
