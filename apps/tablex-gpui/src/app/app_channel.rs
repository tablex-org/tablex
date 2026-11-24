//! The app's end of the local channel from `tablex-mcp` (see `engine_connect::app_channel`): a
//! request is read and answered on background threads, handed to the UI through a channel, and
//! done here on the UI thread with the same code the sidebar and the Run button use, so every
//! rule of the app (confirmations, production and read-only connection types, smart commit)
//! applies. Nothing here runs a statement on its own.

// Windows has no app channel: only the (unix) tests reach most of this when testing.
#![cfg_attr(all(test, not(unix)), allow(dead_code))]

use super::*;
use engine_connect::app_channel::{BindError, Handler, Listener, Reply, Request};
use std::path::Path;

/// How long a connection thread waits for the UI to take a request before it answers "no".
const UI_WAIT: Duration = Duration::from_secs(30);

type Pending = (Request, std::sync::mpsc::Sender<Reply>);

/// The listening socket and the task that carries requests to the UI.
#[derive(Default)]
pub(super) struct ChannelUi {
    listener: Option<Listener>,
    _task: Option<Task<()>>,
    _quit: Option<gpui::Subscription>,
}

impl ChannelUi {
    /// Stops listening and removes the socket file.
    pub(super) fn stop(&mut self) {
        self.listener.take();
        self._task.take();
    }
}

impl AppRoot {
    /// Starts listening on the socket in the app data dir. A second tablex leaves the first one's
    /// socket alone (and says so in the Error Log); a platform without the channel says nothing.
    #[cfg(not(test))]
    pub(super) fn start_app_channel(&mut self, cx: &mut Context<Self>) {
        let Some(path) = engine_connect::app_channel::socket_path() else {
            return;
        };
        match self.listen_on_app_channel(&path, cx) {
            Ok(()) | Err(BindError::Unsupported) => {}
            Err(error) => self.report_warning(
                Area::App,
                None,
                format!("The channel for AI agents is off: {error}"),
            ),
        }
    }

    /// Listens on `path`; requests are done by [`Self::handle_channel_request`].
    pub(super) fn listen_on_app_channel(
        &mut self,
        path: &Path,
        cx: &mut Context<Self>,
    ) -> Result<(), BindError> {
        let (requests, inbox) = smol::channel::unbounded::<Pending>();
        // Runs on a connection thread: never the UI thread, so a request that waits for the UI
        // blocks nothing but its own connection.
        let handler: Handler = Arc::new(move |request| {
            let (reply_to, reply) = std::sync::mpsc::channel();
            if requests.send_blocking((request, reply_to)).is_err() {
                return Reply::error("tablex is closing");
            }
            reply
                .recv_timeout(UI_WAIT)
                .unwrap_or_else(|_| Reply::error("tablex did not answer in time"))
        });
        self.app_channel.listener = Some(Listener::bind(path, handler)?);
        self.app_channel._task = Some(cx.spawn(async move |this, cx| {
            while let Ok((request, reply_to)) = inbox.recv().await {
                match this.update(cx, |this, cx| this.handle_channel_request(request, cx)) {
                    Ok(reply) => {
                        let _ = reply_to.send(reply);
                    }
                    Err(_) => {
                        let _ = reply_to.send(Reply::error("tablex is closing"));
                        break;
                    }
                }
            }
        }));
        // The socket file goes with a clean exit.
        self.app_channel._quit = Some(cx.on_app_quit(|this, _| {
            this.app_channel.stop();
            async {}
        }));
        Ok(())
    }

    /// One request, on the UI thread. The reply says whether it was taken up: a table that is
    /// still loading in the sidebar, or a query that is running, is not waited for.
    pub(super) fn handle_channel_request(&mut self, request: Request, cx: &mut Context<Self>) -> Reply {
        let result = match request {
            // The channel answers pings itself.
            Request::Ping { .. } => Ok(()),
            Request::ConnectionsChanged {} => self.channel_reload_connections(cx),
            Request::OpenObject {
                connection_id,
                database,
                schema,
                object,
            } => self.channel_open_object(&connection_id, database, schema, object, cx),
            Request::ShowQuery {
                connection_id,
                database,
                text,
                run,
            } => self.channel_show_query(&connection_id, database, text, run, cx),
        };
        match result {
            Ok(()) => Reply::ok(),
            Err(message) => Reply::error(message),
        }
    }

    fn channel_reload_connections(&mut self, cx: &mut Context<Self>) -> Result<(), String> {
        if self.scenario_ui.service.is_some() {
            return Err("this tablex is a UI scenario with no saved connections".into());
        }
        if self.refresh_saved_connections(cx) {
            Ok(())
        } else {
            Err("the saved connections could not be read (see the Error Log)".into())
        }
    }

    /// The saved connection, usable for a tab.
    fn channel_connection(&self, connection_id: &str) -> Result<SavedConnection, String> {
        let saved = self
            .saved_connections
            .iter()
            .find(|saved| saved.matches_id(connection_id))
            .ok_or_else(|| format!("no saved connection `{connection_id}` in tablex"))?;
        if saved.disabled {
            return Err(format!("the connection `{}` is disabled", saved.name));
        }
        Ok(saved.clone())
    }

    fn channel_open_object(
        &mut self,
        connection_id: &str,
        database: Option<String>,
        schema: Option<String>,
        object: String,
        cx: &mut Context<Self>,
    ) -> Result<(), String> {
        let saved = self.channel_connection(connection_id)?;
        let id = saved.id.to_string();
        let Some(database) = database.filter(|database| !database.is_empty()) else {
            return Err("`database` is required to open an object".into());
        };
        let connection = NavigatorNodeId::Connection { connection: id.clone() };
        let (path, mongo) = if saved.kind == ConnectionKind::Mongodb {
            (vec![connection], Some((database, object)))
        } else if navigator::sql_driver::is_sql_kind(saved.kind) {
            // A driver without schemas has one schema named like its database.
            let schema = match schema.filter(|schema| !schema.is_empty()) {
                Some(schema) => schema,
                None if !self.sql_supports_schemas(&id) => database.clone(),
                None => return Err("this connection has schemas: `schema` is required".into()),
            };
            (
                vec![
                    connection,
                    NavigatorNodeId::Database {
                        connection: id.clone(),
                        database: database.clone(),
                    },
                    NavigatorNodeId::Schema {
                        connection: id.clone(),
                        database: database.clone(),
                        schema: schema.clone(),
                    },
                    NavigatorNodeId::ObjectGroup {
                        connection: id.clone(),
                        database: database.clone(),
                        schema: schema.clone(),
                        group: ObjectGroup::Tables,
                    },
                    NavigatorNodeId::Object {
                        connection: id,
                        database,
                        schema,
                        object,
                    },
                ],
                None,
            )
        } else {
            return Err(format!(
                "a {:?} connection has no tables or collections to open",
                saved.kind
            ));
        };
        self.locate_and_open_in_sidebar(path, mongo, cx)?;
        self.bring_window_to_front(cx);
        Ok(())
    }

    fn channel_show_query(
        &mut self,
        connection_id: &str,
        database: Option<String>,
        text: String,
        run: bool,
        cx: &mut Context<Self>,
    ) -> Result<(), String> {
        let saved = self.channel_connection(connection_id)?;
        let id = saved.id.to_string();
        let database = database.unwrap_or_default();
        let tab_id = match saved.kind {
            ConnectionKind::Mongodb => {
                self.open_mongo_script_tab(&id, &database, "");
                self.workspace.active_tab().map(|tab| tab.id)
            }
            ConnectionKind::Redis => {
                let target = NavigatorNodeId::Connection { connection: id.clone() };
                if database.is_empty() {
                    self.open_redis_console_for_node(&target)
                        .then(|| self.workspace.active_tab().map(|tab| tab.id))
                        .flatten()
                } else {
                    Some(self.open_redis_console_tab(&id, &database))
                }
            }
            _ => Some(self.workspace.open_query_tab_with_metadata(
                "Query",
                TabMetadata {
                    connection_id: id.clone(),
                    database,
                    ..Default::default()
                },
            )),
        };
        let Some(tab_id) = tab_id else {
            return Err("could not open a query tab for this connection".into());
        };
        if let Some(tab) = self.workspace.tab_mut(tab_id) {
            tab.sql = text;
        }
        self.notify_chrome(cx);
        self.bring_window_to_front(cx);
        if run {
            // The Run button's own path: the app's rules decide whether it asks first or refuses.
            self.run_tab_query_explicit(tab_id, cx);
        }
        Ok(())
    }

    /// The app and its window to the front (once the current update has finished: the window
    /// cannot be updated while this entity is).
    fn bring_window_to_front(&self, cx: &mut Context<Self>) {
        cx.activate(true);
        if let Some(window) = self.driver_runtime.window {
            cx.defer(move |cx| {
                let _ = window.update(cx, |_, window, _| window.activate_window());
            });
        }
    }
}
