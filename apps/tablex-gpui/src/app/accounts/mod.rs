//! Users & Privileges (R7): PostgreSQL roles and MySQL / MariaDB accounts. "Users & Privileges" in a connection's menu and in the
//! command palette opens one tab per connection that lists them, shows the selected one's attributes, memberships and privileges,
//! and edits them as a draft.
//!
//! Nothing here runs a statement. Apply turns the draft into typed changes (`sql_account`) and opens the structure editor's review
//! dialog on them, so the SQL is shown (a password hidden), the read-only and Production gates of the connection apply, and the
//! statements run on a dedicated session, in one transaction where the server has transactional DDL. See `sql_account` for how a
//! password is kept out of every place that shows or logs SQL.
//!
//! * `view`, `panels`, `forms`: the tab, how it looks, and the New / Drop dialogs;
//! * `model`: the roles, the selection and the edits, without a window;
//! * `load`: reading the catalogs on a session of their own, off the UI thread.

mod forms;
mod load;
mod model;
mod panels;
mod view;

use std::collections::HashMap;

use gpui::{Context, Entity, Window};

use super::*;
use crate::sql_account::AccountEngine;
pub(in crate::app) use load::{CatalogSource, DriverCatalog};
#[cfg(test)]
pub(in crate::app) use load::{InjectedCatalog, LoadJob};
pub(in crate::app) use view::AccountsView;
#[cfg(test)]
pub(in crate::app) use view::{Panel, Snapshot};

/// Why the users of a connection cannot be read.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(in crate::app) enum AccountsError {
    /// The connection was removed.
    ConnectionGone,
    /// Its kind has no users and privileges here.
    Unsupported(ConnectionKind),
    /// The connection is not open: its users are read from the server.
    NotOpen,
    /// A second session of the connection could not be prepared (the connect layer's words).
    Session(String),
    /// The window the tab belongs to is closing.
    WindowClosing,
}

impl std::fmt::Display for AccountsError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::ConnectionGone => f.write_str("This connection no longer exists."),
            Self::Unsupported(ConnectionKind::Sqlite) => f.write_str("SQLite has no users or privileges: access is the permission of the database file."),
            Self::Unsupported(ConnectionKind::Clickhouse) => {
                f.write_str("Users & Privileges covers PostgreSQL, MySQL and MariaDB. ClickHouse users are managed with its own SQL (CREATE USER, GRANT).")
            }
            Self::Unsupported(_) => f.write_str("Users & Privileges covers PostgreSQL, MySQL and MariaDB."),
            Self::NotOpen => f.write_str("Open the connection first: its users are read from the server."),
            Self::Session(message) => f.write_str(message),
            Self::WindowClosing => f.write_str("The window is closing."),
        }
    }
}

impl std::error::Error for AccountsError {}

/// The entity of each Users & Privileges tab.
pub(in crate::app) type AccountsViews = HashMap<u64, Entity<AccountsView>>;

impl AppRoot {
    /// Whether the connection has users and privileges to edit (PostgreSQL, MySQL, MariaDB).
    pub(in crate::app) fn accounts_supported(&self, connection: &str) -> bool {
        self.saved_connections.iter().find(|saved| saved.matches_id(connection)).is_some_and(|saved| AccountEngine::of(saved.kind).is_some())
    }

    /// Why the connection's users and privileges cannot be opened, or what opening it needs.
    fn accounts_gate(&self, connection: &str) -> Result<(ConnectionKind, AccountEngine), AccountsError> {
        let saved = self.saved_connections.iter().find(|saved| saved.matches_id(connection)).ok_or(AccountsError::ConnectionGone)?;
        let engine = AccountEngine::of(saved.kind).ok_or(AccountsError::Unsupported(saved.kind))?;
        if self.scenario_ui.service.is_none() && !self.driver_runtime.sessions.contains_key(connection) {
            return Err(AccountsError::NotOpen);
        }
        Ok((saved.kind, engine))
    }

    /// "Users & Privileges" of a connection: its tab (one per connection), read from the server.
    pub(in crate::app) fn open_accounts(&mut self, connection: &str, window: &mut Window, cx: &mut Context<Self>) {
        let (kind, engine) = match self.accounts_gate(connection) {
            Ok(found) => found,
            Err(message) => {
                self.sidebar.notice = Some(message.to_string());
                return cx.notify();
            }
        };
        let existing = self.workspace.tabs().iter().find(|tab| tab.kind == TabKind::Accounts && tab.connection_id == connection).map(|tab| tab.id);
        if let Some(tab_id) = existing {
            return self.activate_tab_ui(tab_id, cx);
        }
        let tab_id = self.workspace.open_accounts_tab(connection, "Users & Privileges");
        let root = cx.entity().downgrade();
        let connection_id = connection.to_owned();
        let view = cx.new(|cx| AccountsView::new(root, tab_id, connection_id.clone(), kind, engine, window, cx));
        self.accounts.insert(tab_id, view.clone());
        self.activate_tab_ui(tab_id, cx);
        let source = self.accounts_catalog_source(&connection_id);
        view.update(cx, |view, cx| view.start(source, cx));
        cx.notify();
    }

    /// The connection the palette's command means: the active tab's, else the one of the row selected in the navigator.
    pub(in crate::app) fn accounts_connection_from_context(&self) -> Option<String> {
        let tab = self.workspace.active_tab().filter(|tab| !tab.connection_id.is_empty());
        if let Some(tab) = tab {
            return Some(tab.connection_id.clone());
        }
        match self.navigator.selected()? {
            NavigatorNodeId::Connection { connection }
            | NavigatorNodeId::Database { connection, .. }
            | NavigatorNodeId::Schema { connection, .. }
            | NavigatorNodeId::ObjectGroup { connection, .. }
            | NavigatorNodeId::Object { connection, .. }
            | NavigatorNodeId::SchemaObject { connection, .. }
            | NavigatorNodeId::Column { connection, .. }
            | NavigatorNodeId::Provider { connection, .. } => Some(connection.clone()),
            NavigatorNodeId::Group { .. } => None,
        }
    }

    /// The command palette's.
    pub(in crate::app) fn open_accounts_from_context(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        match self.accounts_connection_from_context() {
            Some(connection) => self.open_accounts(&connection, window, cx),
            None => {
                self.sidebar.notice = Some("Select a connection in the navigator first.".to_owned());
                cx.notify();
            }
        }
    }

    /// Where the tab of `connection` reads from: a second session of it (never a SQL tab's).
    pub(in crate::app) fn accounts_catalog_source(&self, connection: &str) -> Result<CatalogSource, AccountsError> {
        #[cfg(test)]
        if let Some(make) = &self.accounts_injected {
            return Ok(CatalogSource::Injected(std::sync::Arc::clone(make)));
        }
        let saved = self.saved_connections.iter().find(|saved| saved.matches_id(connection)).cloned().ok_or(AccountsError::ConnectionGone)?;
        self.secondary_session_spec(&saved, connection).map(|spec| CatalogSource::Driver(Box::new(spec))).map_err(AccountsError::Session)
    }

    /// An apply on `connection` ended: its Users & Privileges tabs read the server again.
    pub(in crate::app) fn accounts_after_apply(&mut self, connection: &str, cx: &mut Context<Self>) {
        let views: Vec<_> = self
            .workspace
            .tabs()
            .iter()
            .filter(|tab| tab.kind == TabKind::Accounts && tab.connection_id == connection)
            .filter_map(|tab| self.accounts.get(&tab.id).cloned())
            .collect();
        for view in views {
            let source = self.accounts_catalog_source(connection);
            view.update(cx, |view, cx| view.after_apply(source, cx));
        }
    }

    /// The tab has edits that were not applied (closing it asks first).
    pub(in crate::app) fn accounts_dirty(&self, tab_id: u64, cx: &gpui::App) -> bool {
        self.accounts.get(&tab_id).is_some_and(|view| view.read(cx).has_edits(cx))
    }
}
