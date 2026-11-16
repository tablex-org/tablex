//! "Connect as Another User...": a server's databases may belong to different users, and a
//! connection has one. The action opens the New Connection dialog for the same server with the
//! database chosen and no user, so the next one is a separate connection (saved the normal way).
//!
//! It is offered on a database row (context menu) and on the failure marker of a database that
//! could not be opened for want of rights, and a database the connected user holds no CONNECT
//! privilege on is shown dimmed and opens nothing: the same panel is what a click on it shows.

use super::*;
use crate::navigator::NavigatorNodeId;
use crate::settings::connection::supports_database_scope;
use gpui::SharedString;

/// What the dialog is opened with besides the source connection.
pub(super) struct AnotherUser {
    /// The database the new connection names (and shows alone).
    pub(super) database: String,
    /// The sidebar group of the source: the new connection is placed in it once saved.
    pub(super) group: Option<String>,
}

/// Whether `message`, the failure of opening a database, says the server refused the user (its
/// credentials, or the database to it) rather than being unreachable: the case another user fixes.
pub(super) fn is_denied_database(message: &str) -> bool {
    let lower = message.to_ascii_lowercase();
    crate::connections::is_auth_failure(message)
        || lower.contains("permission denied for database")
        || lower.contains("no pg_hba.conf entry")
}

/// The failure of a database the user may not connect to, as the sidebar shows it.
fn no_connect_failure(database: &str, user: &str) -> String {
    format!("Permission denied for database {database}: no CONNECT privilege for {user}.")
}

impl AppRoot {
    /// The user of a connection's saved address, for the messages (`this user` when it names none).
    fn connection_user(&self, connection: &str) -> String {
        self.saved_connections
            .iter()
            .find(|saved| saved.matches_id(connection))
            .and_then(|saved| url::Url::parse(&saved.uri).ok())
            .map(|url| crate::connection_form::decode_uri_component(url.username()))
            .filter(|user| !user.is_empty())
            .unwrap_or_else(|| "this user".to_owned())
    }

    /// The tooltip of a database row the connected user may not connect to.
    pub(super) fn no_connect_tooltip(&self, connection: &str) -> SharedString {
        format!("No CONNECT privilege for {}", self.connection_user(connection)).into()
    }

    /// A click on a database the user may not connect to: no connect is attempted; the row shows
    /// why, with the way out ("Connect as Another User...") in the panel it opens.
    pub(super) fn show_no_connect(&mut self, id: &NavigatorNodeId, cx: &mut Context<Self>) {
        let NavigatorNodeId::Database { connection, database } = id else {
            return;
        };
        let failure = no_connect_failure(database, &self.connection_user(connection));
        self.navigator.fail_load(id, failure);
        self.navigator_ui.problem_open = Some(id.clone());
        self.navigator_ui.generation = self.navigator.generation();
        self.notify_chrome(cx);
    }

    /// Whether the failure panel of row `id` offers "Connect as Another User...": a database of a
    /// kind with databases that the server refused to this user.
    pub(super) fn offers_another_user(&self, id: &NavigatorNodeId, message: &str) -> bool {
        let NavigatorNodeId::Database { connection, .. } = id else {
            return false;
        };
        self.navigator
            .connection(connection)
            .is_some_and(|saved| !saved.disabled && supports_database_scope(saved.kind))
            && is_denied_database(message)
    }

    /// Opens the New Connection dialog for the server of `connection`, on `database`.
    pub(super) fn connect_as_another_user(
        &mut self,
        connection: &str,
        database: &str,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(saved) = self
            .saved_connections
            .iter()
            .find(|saved| saved.matches_id(connection) && !saved.disabled && supports_database_scope(saved.kind))
            .cloned()
        else {
            return;
        };
        let group = self
            .navigator
            .preferences()
            .connections
            .iter()
            .find(|item| saved.matches_id(&item.id))
            .and_then(|item| item.group.clone());
        let another = AnotherUser { database: database.to_owned(), group };
        self.navigator_ui.problem_open = None;
        self.open_connection_dialog_for(Some(saved), Some(another), false, window, cx);
    }

    /// Puts the connection `id` in the sidebar group `group` (the source's of "Connect as Another
    /// User"); a group that is gone, or preferences that cannot be written, leave it where a new
    /// connection goes, with a warning.
    pub(super) fn place_connection_in_group(&mut self, id: &str, group: &str, cx: &mut Context<Self>) {
        let result = self.change_sidebar_preferences(|prefs| prefs.move_connection(id, Some(group), usize::MAX), cx);
        if let Err(error) = result {
            self.report_warning(Area::Connect, Some(id), format!("The new connection could not be placed in its group: {error}"));
        }
    }
}
