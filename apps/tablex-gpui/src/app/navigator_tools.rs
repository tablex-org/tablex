//! Sidebar connection tools: Reconnect, Disconnect all (everywhere or in a group), what a
//! double-click on a connection does, opening the first level of a connection made by hand, and
//! reopening the nodes of the tree that were open (after a restart, a disconnect, a reconnect).
//!
//! Each of them goes through the paths the single actions already use: a disconnect asks about
//! open transactions exactly like Disconnect on one connection, a reconnect releases the session
//! like an automatic reconnect does and connects through `ensure_connected` (so the password and
//! missing-driver prompts, the tunnel and the re-run of the open data tabs are the usual ones).

use super::*;
use crate::settings::ConnectionDoubleClick;

/// Which connected connections "Disconnect all" reaches.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) enum DisconnectScope {
    /// Every connected connection.
    All,
    /// The connected connections of a group and of the groups below it.
    Group(String),
}

impl AppRoot {
    /// The connected connections `scope` reaches, in saved order.
    pub(super) fn connected_in_scope(&self, scope: &DisconnectScope) -> Vec<String> {
        let members: Option<HashSet<String>> = match scope {
            DisconnectScope::All => None,
            DisconnectScope::Group(group) => Some(self.navigator.group_member_ids(group).into_iter().collect()),
        };
        self.saved_connections
            .iter()
            .map(|saved| saved.id.to_string())
            .filter(|id| members.as_ref().is_none_or(|members| members.contains(id)))
            .filter(|id| matches!(self.sessions.status(id), ConnectionStatus::Connected))
            .collect()
    }

    /// Disconnect all / in a group: asks first (once) when a tab of any of them has a transaction
    /// open, as Disconnect on one connection does, then disconnects each through the normal
    /// per-connection path. Returns how many connections it was asked for.
    pub(super) fn request_disconnect_all(
        &mut self,
        scope: DisconnectScope,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> usize {
        let ids = self.connected_in_scope(&scope);
        if ids.is_empty() {
            return 0;
        }
        let count = ids.len();
        let tabs = self.tabs_of_connections(&ids);
        let later = ids.clone();
        let asked = self.guard_transactions(tabs, "Disconnecting ends their sessions.", window, cx, move |this, _, cx| {
            for id in &later {
                this.disconnect_connection(id, cx);
            }
        });
        if !asked {
            for id in &ids {
                self.disconnect_connection(id, cx);
            }
        }
        count
    }

    /// Reconnect is offered on a connection with a live session that nothing is replacing.
    pub(super) fn can_reconnect(&self, connection: &str) -> bool {
        matches!(self.sessions.status(connection), ConnectionStatus::Connected)
            && self
                .saved_connections
                .iter()
                .any(|saved| saved.matches_id(connection) && !saved.disabled)
    }

    /// Reconnect from the user: asks first when a tab of the connection has a transaction open
    /// (the session goes), then drops the session and connects again.
    pub(super) fn request_reconnect(&mut self, connection: &str, window: &mut Window, cx: &mut Context<Self>) {
        if !self.can_reconnect(connection) {
            return;
        }
        let tabs = self.tabs_of_connections(&[connection.to_owned()]);
        let id = connection.to_owned();
        let asked = self.guard_transactions(tabs, "Reconnecting ends their sessions.", window, cx, move |this, _, cx| {
            this.reconnect_connection(&id, cx)
        });
        if !asked {
            self.reconnect_connection(connection, cx);
        }
    }

    /// Drops the connection's session and connects again. The tree keeps what was open (it comes
    /// back as the nodes load), the typed secrets stay, and the data tabs of the connection run
    /// again on the new session like after an automatic reconnect.
    pub(super) fn reconnect_connection(&mut self, connection: &str, cx: &mut Context<Self>) {
        if !self.can_reconnect(connection) {
            return;
        }
        self.navigator.remember_tree(connection);
        self.connection_transactions_closed(connection);
        self.release_connection(connection, true, cx);
        self.ensure_connected(connection.to_string(), None, cx);
    }

    /// The connection to reconnect from the palette: the selected connection of the sidebar, else
    /// the one of the active tab.
    pub(super) fn reconnect_target(&self) -> Option<String> {
        let selected = match self.navigator.selected() {
            Some(NavigatorNodeId::Connection { connection }) => Some(connection.clone()),
            _ => None,
        };
        selected
            .into_iter()
            .chain(self.workspace.active_tab().map(|tab| tab.connection_id.clone()))
            .find(|id| self.can_reconnect(id))
    }

    /// A connect the user asked for (Connect, a double-click, the palette): the connection opens its
    /// first level once connected when "Expand on connect" is on.
    pub(super) fn note_connect_by_hand(&mut self, connection: &str) {
        self.navigator_ui.expand_after_connect.insert(connection.to_string());
    }

    /// A connection's driver session just came up: opens its first level (a connect by hand with
    /// "Expand on connect" on) and starts reopening the nodes the tree had open.
    pub(super) fn connection_came_up(&mut self, connection: &str, cx: &mut Context<Self>) {
        self.welcome_note_connected(connection, cx);
        let by_hand = self.navigator_ui.expand_after_connect.remove(connection);
        if by_hand && self.settings.navigation.expand_on_connect {
            let id = NavigatorNodeId::Connection { connection: connection.to_string() };
            if !self.navigator.is_expanded(&id) {
                if let Some(request) = self.navigator.toggle_expansion(&id).request {
                    self.navigator_ui.generation = self.navigator.generation();
                    self.ensure_connected_then_load(request, cx);
                }
            }
        }
        self.continue_tree_restore(connection, cx);
    }

    /// Reopens the remembered nodes of a connected connection that have loaded by now (never
    /// connects: a connection without a session keeps its nodes remembered).
    pub(super) fn continue_tree_restore(&mut self, connection: &str, cx: &mut Context<Self>) {
        if !self.navigator.has_remembered_tree(connection) || !self.sessions.is_connected(connection) {
            return;
        }
        let requests = self.navigator.resume_tree(connection);
        self.navigator_ui.generation = self.navigator.generation();
        for request in requests {
            self.ensure_connected_then_load(request, cx);
        }
        cx.notify();
    }

    /// Double-click on a connection row, as the "Double-click a connection" setting says. `false`:
    /// the setting is the default (expand or collapse), which the clicks of the row already did.
    pub(super) fn handle_connection_double_click(
        &mut self,
        row: &NavigatorRow,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> bool {
        let NavigatorNodeId::Connection { connection } = &row.id else {
            return false;
        };
        let mode = self.settings.navigation.connection_double_click;
        if row.disabled || mode == ConnectionDoubleClick::Expand {
            return false;
        }
        self.navigator.select_visible(&row.id, false, false);
        match mode {
            ConnectionDoubleClick::Expand => {}
            ConnectionDoubleClick::Connect => match self.sessions.status(connection) {
                ConnectionStatus::Connected => self.request_disconnect(connection, window, cx),
                ConnectionStatus::Connecting => {}
                _ => self.open_connection_target(connection, cx),
            },
            ConnectionDoubleClick::NewQuery => {
                self.new_query_for_sidebar_node(&row.id, cx);
            }
        }
        self.notify_chrome(cx);
        true
    }
}
