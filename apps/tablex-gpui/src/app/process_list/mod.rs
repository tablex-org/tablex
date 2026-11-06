//! Process List: the sessions running on a server, and a way to cancel a query or kill a session. "Process List" in a connection's
//! menu and in the command palette opens one tab per connection.
//!
//! * `engine`: per kind of server, as data: the query that lists the sessions, the columns, and the statements of the actions
//!   (PostgreSQL, MySQL, MariaDB, ClickHouse; MongoDB's `currentOp` / `killOp` and Redis' `CLIENT LIST` / `CLIENT KILL` are one more
//!   entry each);
//! * `session`: the session of the tab's own it reads and acts through, off the UI thread;
//! * `view`: the tab.
//!
//! Safety. Reading is harmless; acting is not, so an action goes through a dialog that shows the statement, a read-only connection
//! (Settings › Connection types) cannot act, and a Production connection (or one that confirms executions) wants its explicit
//! confirmation ticked first, the same gates as the structure editor's.

mod engine;
mod session;
mod view;

use std::collections::HashMap;

use gpui::{Context, Entity, Window};

use super::*;
use engine::ProcessEngine;
#[cfg(test)]
pub(in crate::app) use session::{InjectedSession, ProcessSession};
use session::SessionSource;
pub(in crate::app) use view::ProcessListView;
#[cfg(test)]
pub(in crate::app) use view::Snapshot;

/// What a tab says, and what a refresh gets, while its connection is closed.
const CLOSED: &str = "The connection is closed. Open it, then refresh.";

/// The entity of each Process List tab.
pub(in crate::app) type ProcessListViews = HashMap<u64, Entity<ProcessListView>>;

/// What the connection's settings say about acting on a session.
pub(super) struct ActionGate {
    /// Why acting is refused (a read-only connection); `None`: allowed.
    pub(super) read_only: Option<String>,
    /// The connection wants its explicit confirmation before it executes anything (Production, or confirm-execute).
    pub(super) needs_ack: bool,
    /// "Production · shop": the connection as the dialog names it.
    pub(super) subject: String,
}

/// An [`ActionGate`] and the connection's name as read, kept by the tab until what they were read from changes (the render asks every
/// frame; reading them builds strings).
pub(super) struct GateCache {
    pub(super) gate: ActionGate,
    /// `None`: the connection is not in the saved list.
    name: Option<String>,
    /// The connection profiles and the settings the gate was read from.
    policy: u64,
    settings: std::sync::Arc<crate::settings::Settings>,
}

impl GateCache {
    /// The connection's saved name; `None`: it is not in the saved list.
    pub(super) fn name(&self) -> Option<&str> {
        self.name.as_deref()
    }
}

impl AppRoot {
    /// Whether the connection has sessions to list (PostgreSQL, MySQL, MariaDB, ClickHouse).
    pub(in crate::app) fn process_list_supported(&self, connection: &str) -> bool {
        self.saved_connections.iter().find(|saved| saved.matches_id(connection)).is_some_and(|saved| ProcessEngine::of(saved.kind).is_some())
    }

    /// What `connection`'s settings allow: read-only refuses, Production asks for its confirmation.
    pub(super) fn process_list_action_gate(&self, connection: &str) -> ActionGate {
        let effective = self.effective_for(connection);
        let production = effective.connection_type.as_ref().is_some_and(|ty| ty.is_production());
        ActionGate {
            read_only: self.read_only_connection_message(connection, "Cancelling queries and killing sessions are"),
            needs_ack: effective.confirm_execute || production,
            subject: self.connection_subject(connection, &effective),
        }
    }

    /// The gate and name of `connection` as they are now.
    pub(super) fn process_list_gate_cache(&self, connection: &str) -> GateCache {
        #[cfg(test)]
        self.process_list_gate_reads.set(self.process_list_gate_reads.get() + 1);
        GateCache {
            gate: self.process_list_action_gate(connection),
            name: self.saved_connections.iter().find(|saved| saved.matches_id(connection)).map(|saved| saved.name.clone()),
            policy: self.connection_policy.revision(),
            settings: std::sync::Arc::clone(&self.settings),
        }
    }

    /// `cache` still says what the connection's profile, the settings and the saved list say (no allocation).
    pub(super) fn process_list_gate_current(&self, connection: &str, cache: &GateCache) -> bool {
        cache.policy == self.connection_policy.revision()
            && std::sync::Arc::ptr_eq(&cache.settings, &self.settings)
            && self.saved_connections.iter().find(|saved| saved.matches_id(connection)).map(|saved| saved.name.as_str()) == cache.name.as_deref()
    }

    /// The tab is the one shown in a pane.
    pub(super) fn process_list_shown(&self, tab_id: u64) -> bool {
        self.workspace.panes().panes().any(|pane| pane.active() == Some(tab_id))
    }

    /// The connection is open (a fixture's has no driver session to look for).
    fn process_list_connected(&self, connection: &str) -> bool {
        self.scenario_ui.service.is_some() || self.driver_runtime.sessions.contains_key(connection)
    }

    /// Where the tab of `connection` gets its session: a second one of the connection (never a SQL tab's). A connection that is not open
    /// has none, so nothing is read or run through a tab after Disconnect.
    pub(super) fn process_list_source(&self, connection: &str) -> Result<SessionSource, String> {
        if !self.process_list_connected(connection) {
            return Err(CLOSED.to_owned());
        }
        #[cfg(test)]
        if let Some(make) = &self.process_list_injected {
            return Ok(SessionSource::Injected(std::sync::Arc::clone(make)));
        }
        let saved = self.saved_connections.iter().find(|saved| saved.matches_id(connection)).cloned().ok_or("This connection no longer exists.")?;
        self.secondary_session_spec(&saved, connection).map(|spec| SessionSource::Driver(Box::new(spec)))
    }

    /// "Process List" of a connection: its tab (one per connection), read from the server.
    pub(in crate::app) fn open_process_list(&mut self, connection: &str, window: &mut Window, cx: &mut Context<Self>) {
        let engine = self.saved_connections.iter().find(|saved| saved.matches_id(connection)).map(|saved| ProcessEngine::of(saved.kind));
        let engine = match engine {
            None => return self.process_list_notice("This connection no longer exists.", cx),
            Some(None) => return self.process_list_notice("Process List covers PostgreSQL, MySQL, MariaDB and ClickHouse.", cx),
            Some(Some(engine)) => engine,
        };
        if !self.process_list_connected(connection) {
            return self.process_list_notice("Open the connection first: its sessions are read from the server.", cx);
        }
        let existing = self.workspace.tabs().iter().find(|tab| tab.kind == TabKind::ProcessList && tab.connection_id == connection).map(|tab| tab.id);
        if let Some(tab_id) = existing {
            return self.activate_tab_ui(tab_id, cx);
        }
        let tab_id = self.workspace.open_process_list_tab(connection, "Process List");
        let root = cx.entity().downgrade();
        let connection_id = connection.to_owned();
        let view = cx.new(|cx| ProcessListView::new(root, tab_id, connection_id.clone(), engine, window, cx));
        self.process_lists.insert(tab_id, view.clone());
        self.activate_tab_ui(tab_id, cx);
        let source = self.process_list_source(&connection_id);
        view.update(cx, |view, cx| view.start(source, cx));
        cx.notify();
    }

    /// The connection's session is going (Disconnect, a lost or replaced session): each of its tabs lets go of its own session and stops
    /// its clock, and says so.
    pub(super) fn process_list_connection_released(&mut self, connection: &str, cx: &mut Context<Self>) {
        for view in self.process_list_views_of(connection) {
            view.update(cx, |view, cx| view.connection_released(cx));
        }
    }

    /// The connection is open again: its tabs read again and take their clock back.
    pub(super) fn process_list_connection_reopened(&mut self, connection: &str, cx: &mut Context<Self>) {
        for view in self.process_list_views_of(connection) {
            // Handed over: the view cannot ask the app while the app is being updated.
            let source = self.process_list_source(connection);
            view.update(cx, |view, cx| view.connection_reopened(source, cx));
        }
    }

    fn process_list_views_of(&self, connection: &str) -> Vec<Entity<ProcessListView>> {
        self.workspace
            .tabs()
            .iter()
            .filter(|tab| tab.kind == TabKind::ProcessList && tab.connection_id == connection)
            .filter_map(|tab| self.process_lists.get(&tab.id).cloned())
            .collect()
    }

    fn process_list_notice(&mut self, text: &str, cx: &mut Context<Self>) {
        self.sidebar.notice = Some(text.to_owned());
        cx.notify();
    }

    /// The command palette's: the active tab's connection, else the one selected in the navigator.
    pub(in crate::app) fn open_process_list_from_context(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        match self.accounts_connection_from_context() {
            Some(connection) => self.open_process_list(&connection, window, cx),
            None => self.process_list_notice("Select a connection in the navigator first.", cx),
        }
    }
}
