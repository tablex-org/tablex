//! AppRoot side of the MongoDB sidebar administration: capability gating,
//! running admin operations (driver session or scenario fixture), and the
//! sidebar / tab updates that follow a successful operation.
//!
//! Tab policy after a successful operation:
//! * rename: every open data tab (`mongoFind`) of the collection is retargeted
//!   to the new name (title too when it still showed the old name) and the
//!   active one re-runs; script tabs keep their text.
//! * drop collection / drop database: data tabs of the dropped object are
//!   closed (nothing left to show); script tabs are kept because they hold
//!   user-written text, and the notice says how many were kept.
use super::*;
use crate::mongo_admin::{MongoAdminCaps, MongoAdminOp, MongoIndexInfo};

pub(super) type AdminDone =
    Box<dyn FnOnce(&mut AppRoot, Result<u64, String>, &mut Context<AppRoot>) + 'static>;
pub(super) type IndexesDone = Box<
    dyn FnOnce(&mut AppRoot, Result<Vec<MongoIndexInfo>, String>, &mut Context<AppRoot>) + 'static,
>;

/// A MongoDB database or collection row of the sidebar.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct MongoNodeTarget {
    pub(super) id: NavigatorNodeId,
    pub(super) connection: String,
    pub(super) database: String,
    /// `None` for a database node.
    pub(super) collection: Option<String>,
}

impl AppRoot {
    /// Resolves a sidebar row to a MongoDB database/collection target.
    pub(super) fn mongo_node_target(&self, id: &NavigatorNodeId) -> Option<MongoNodeTarget> {
        let NavigatorNodeId::Provider { connection, .. } = id else {
            return None;
        };
        if !self.saved_connections.iter().any(|saved| {
            saved.matches_id(connection)
                && saved.kind == crate::connections::ConnectionKind::Mongodb
        }) {
            return None;
        }
        let provider = self.navigator.provider_node(id)?;
        let text = |key: &str| {
            provider
                .payload
                .get(key)
                .and_then(serde_json::Value::as_str)
                .map(str::to_string)
        };
        let database = text("database")?;
        match provider.kind.as_str() {
            "database" => Some(MongoNodeTarget {
                id: id.clone(),
                connection: connection.clone(),
                database,
                collection: None,
            }),
            "collection" => Some(MongoNodeTarget {
                id: id.clone(),
                connection: connection.clone(),
                database,
                collection: Some(text("collection")?),
            }),
            _ => None,
        }
    }

    /// Admin groups the live driver advertises; all false while disconnected.
    /// Reads the capabilities cached at connect time, so a busy session lock
    /// (a running query) never greys the menu out.
    pub(super) fn mongo_admin_caps(&self, connection: &str) -> MongoAdminCaps {
        if !self.sessions.is_connected(connection) {
            return MongoAdminCaps::default();
        }
        if let Some(service) = self.scenario_ui.service.as_ref() {
            return service.fixture().mongo_admin_caps();
        }
        if let Some(capabilities) = self.driver_runtime.capabilities.get(connection) {
            return MongoAdminCaps::from_capabilities(capabilities);
        }
        MongoAdminCaps::default()
    }

    /// Whether the connected driver can run `mongo_aggregate` (the scenario
    /// fixture always can). Read from the capabilities cached at connect time.
    pub(super) fn mongo_supports_aggregate(&self, connection: &str) -> bool {
        if !self.sessions.is_connected(connection) {
            return false;
        }
        if self.scenario_ui.service.is_some() {
            return true;
        }
        self.driver_runtime
            .capabilities
            .get(connection)
            .is_some_and(|capabilities| capabilities.has(Capability::QueryMongoAggregate))
    }

    /// Whether the connected driver can run `query.mongo_script`, read from
    /// cached capabilities (never the session lock). The scenario fixture
    /// has no driver session, so it reports false here as before.
    pub(super) fn mongo_supports_script(&self, connection: &str) -> bool {
        self.driver_runtime.sessions.contains_key(connection)
            && self
                .driver_capabilities(connection, "mongodb")
                .is_some_and(|capabilities| capabilities.has(Capability::QueryMongoScript))
    }

    /// "New Aggregation" on a collection row.
    pub(super) fn new_aggregation_for_target(
        &mut self,
        target: &MongoNodeTarget,
        cx: &mut Context<Self>,
    ) {
        let Some(collection) = target.collection.as_deref() else {
            return;
        };
        if !self.mongo_supports_aggregate(&target.connection) {
            self.sidebar.notice =
                Some("Not supported by this MongoDB driver version.".into());
            cx.notify();
            return;
        }
        self.open_aggregate_tab(
            &target.connection,
            &target.database,
            collection,
            crate::aggregate::AggregateDraft::new(),
            cx,
        );
    }

    pub(super) fn mongo_default_database(&self, connection: &str) -> Option<String> {
        self.navigator
            .preferences()
            .default_database(connection)
            .map(str::to_string)
    }

    /// Persists (or clears, with `None`) the sidebar default database.
    pub(super) fn set_mongo_default_database(
        &mut self,
        connection: &str,
        database: Option<&str>,
        cx: &mut Context<Self>,
    ) -> Result<(), String> {
        let connection_id = connection.to_string();
        let database = database.map(str::to_string);
        let notice = match &database {
            Some(name) => format!("Default database set to \"{name}\""),
            None => "Default database cleared".to_string(),
        };
        self.change_sidebar_preferences(
            move |prefs| prefs.set_default_database(&connection_id, database.as_deref()),
            cx,
        )?;
        self.sidebar.notice = Some(notice);
        cx.notify();
        Ok(())
    }

    pub(super) fn copy_mongo_node_name(&mut self, id: &NavigatorNodeId, cx: &mut Context<Self>) {
        let Some(target) = self.mongo_node_target(id) else {
            return;
        };
        let name = target.collection.unwrap_or(target.database);
        cx.write_to_clipboard(gpui::ClipboardItem::new_string(name));
    }

    /// View Data (reuse an open tab) or Open in New Data Tab (always a new one).
    pub(super) fn open_mongo_collection_data(
        &mut self,
        target: &MongoNodeTarget,
        new_tab: bool,
        cx: &mut Context<Self>,
    ) {
        let Some(collection) = target.collection.as_deref() else {
            return;
        };
        let created = if new_tab {
            self.workspace.open_query_tab(collection);
            if let Some(tab) = self.workspace.active_tab_mut() {
                tab.connection_id = target.connection.clone();
                tab.database = target.database.clone();
                tab.collection = collection.to_string();
                tab.query_mode = Some(QueryMode::MongoFind);
                tab.sql = "{}".into();
            }
            self.workspace.normalize();
            true
        } else {
            self.workspace
                .open_or_activate_collection_tab(&target.connection, &target.database, collection)
                .1
        };
        if created {
            self.run_active_tab_query(cx);
        }
        cx.notify();
    }

    /// Reloads one database node's collections without touching any other
    /// node's expansion (`reset_node` only clears that subtree).
    pub(super) fn refresh_mongo_database_node(
        &mut self,
        connection: &str,
        database: &str,
        cx: &mut Context<Self>,
    ) {
        let root = NavigatorNodeId::Connection {
            connection: connection.to_string(),
        };
        let Some(id) = self
            .navigator
            .find_provider_child(&root, "database", database, None)
        else {
            return;
        };
        self.navigator.reset_node(&id);
        if let Some(row) = self
            .navigator
            .visible_index(&id)
            .and_then(|index| self.navigator.visible_row_at(index))
        {
            self.handle_navigator_disclosure(&row, cx);
        }
        self.navigator_ui.generation = self.navigator.generation();
        cx.notify();
    }

    /// Lists the connection's databases again (a database was created): the connection node starts
    /// over and loads, like Refresh on it.
    fn refresh_mongo_databases(&mut self, connection: &str, cx: &mut Context<Self>) {
        let root = NavigatorNodeId::Connection {
            connection: connection.to_string(),
        };
        self.navigator.reset_node(&root);
        if let Some(row) = self
            .navigator
            .visible_index(&root)
            .and_then(|index| self.navigator.visible_row_at(index))
        {
            self.handle_navigator_disclosure(&row, cx);
        }
        self.navigator_ui.generation = self.navigator.generation();
        cx.notify();
    }

    /// Sidebar Refresh for a Mongo node: databases reload themselves, a
    /// collection reloads its database.
    pub(super) fn refresh_mongo_node(&mut self, target: &MongoNodeTarget, cx: &mut Context<Self>) {
        self.refresh_mongo_database_node(&target.connection, &target.database, cx);
    }

    /// Runs one admin mutation. In scenario mode the fixture answers
    /// synchronously; otherwise the driver session runs it off the UI thread.
    /// `done` always runs once, after the sidebar/tab effects of a success.
    pub(super) fn run_mongo_admin(
        &mut self,
        connection: &str,
        database: &str,
        op: MongoAdminOp,
        done: AdminDone,
        cx: &mut Context<Self>,
    ) {
        let connection = connection.to_string();
        let database = database.to_string();
        if let Some(message) = self.read_only_connection_message(&connection, "Administration is") {
            self.finish_mongo_admin(&connection, &database, &op, Err(message), done, cx);
            return;
        }
        if let Some(service) = self.scenario_ui.service.as_mut() {
            let result = service
                .fixture_mut()
                .apply_mongo_admin(&connection, &database, &op);
            self.finish_mongo_admin(&connection, &database, &op, result, done, cx);
            return;
        }
        let Some(session) = self.driver_runtime.sessions.get(&connection).cloned() else {
            let result = Err("The MongoDB connection is not available".to_string());
            self.finish_mongo_admin(&connection, &database, &op, result, done, cx);
            return;
        };
        // Host-allocated (lock-free) id; the operation stays registered until
        // the driver answered.
        let operation = match session.begin_operation() {
            Ok(operation) => operation,
            Err(error) => {
                self.finish_mongo_admin(&connection, &database, &op, Err(error.to_string()), done, cx);
                return;
            }
        };
        let request = op.mutation(&database, operation.id());
        let work = cx.background_executor().spawn(async move {
            smol::unblock(move || {
                // Lock-free: a slow admin command never queues the navigator
                // behind the session lock.
                session
                    .mutate(&operation, request)
                    .map(|result| (result.affected_rows, result.invalidated_page_ids))
                    .map_err(|error| error.to_string())
            })
            .await
        });
        cx.spawn(async move |this, cx| {
            let result = work.await;
            let _ = this.update(cx, |this, cx| {
                let result = result.map(|(affected, invalidated)| {
                    this.invalidate_driver_pages(&connection, &invalidated, None, cx);
                    affected
                });
                this.finish_mongo_admin(&connection, &database, &op, result, done, cx);
            });
        })
        .detach();
    }

    fn finish_mongo_admin(
        &mut self,
        connection: &str,
        database: &str,
        op: &MongoAdminOp,
        result: Result<u64, String>,
        done: AdminDone,
        cx: &mut Context<Self>,
    ) {
        // Failures stay in the dialog that started the operation; only a
        // success changes the sidebar and tabs.
        let result = result.map_err(|error| {
            format!(
                "{}: {}",
                admin_failure_title(op),
                crate::sessions::redact_secrets(&error)
            )
        });
        if let Err(error) = &result {
            self.report_error(Area::Admin, Some(connection), error);
        }
        if let Ok(affected) = &result {
            self.apply_mongo_admin_effects(connection, database, op, *affected, cx);
        }
        done(self, result, cx);
        cx.notify();
    }

    /// Lists a collection's indexes (driver `mongo_list_indexes`).
    pub(super) fn list_mongo_indexes(
        &mut self,
        connection: &str,
        database: &str,
        collection: &str,
        done: IndexesDone,
        cx: &mut Context<Self>,
    ) {
        let connection_key = connection.to_string();
        let done: IndexesDone = Box::new(move |this, result, cx| {
            if let Err(error) = &result {
                this.report_error(Area::Admin, Some(&connection_key), error);
            }
            done(this, result, cx)
        });
        if let Some(service) = self.scenario_ui.service.as_ref() {
            let result = service
                .fixture()
                .list_mongo_indexes(connection, database, collection);
            done(self, result, cx);
            return;
        }
        let Some(session) = self.driver_runtime.sessions.get(connection).cloned() else {
            done(
                self,
                Err("The MongoDB connection is not available".into()),
                cx,
            );
            return;
        };
        let operation = match session.begin_operation() {
            Ok(operation) => operation,
            Err(error) => {
                done(self, Err(error.to_string()), cx);
                return;
            }
        };
        let request = crate::mongo_admin::list_indexes_request(
            database,
            collection,
            operation.id(),
        );
        let work = cx.background_executor().spawn(async move {
            smol::unblock(move || -> Result<Vec<MongoIndexInfo>, String> {
                // Keeps the host-allocated operation registered for the call.
                let _operation = operation;
                let request_json = serde_json::to_string(&request).map_err(|e| e.to_string())?;
                let mut session = session
                    .lock()
                    .map_err(|_| "Driver session is unavailable".to_string())?;
                let page = session
                    .open_query(&request_json)
                    .map_err(|error| error.to_string())?;
                let rows = (0..page.row_count)
                    .map(|row| {
                        session
                            .inspect_row(page.page_id, row)
                            .map_err(|error| error.to_string())
                    })
                    .collect::<Result<Vec<_>, _>>();
                let _ = session.release_page(page.page_id);
                Ok(rows?
                    .iter()
                    .filter_map(|row| MongoIndexInfo::from_fields(&row.fields))
                    .collect())
            })
            .await
        });
        cx.spawn(async move |this, cx| {
            let result = work
                .await
                .map_err(|error| crate::sessions::redact_secrets(&error));
            let _ = this.update(cx, |this, cx| {
                done(this, result, cx);
                cx.notify();
            });
        })
        .detach();
    }

    fn apply_mongo_admin_effects(
        &mut self,
        connection: &str,
        database: &str,
        op: &MongoAdminOp,
        affected: u64,
        cx: &mut Context<Self>,
    ) {
        match op {
            MongoAdminOp::DropCollection { collection } => {
                let kept = self.close_mongo_data_tabs(connection, database, Some(collection), cx);
                self.refresh_mongo_database_node(connection, database, cx);
                self.sidebar.notice = Some(drop_notice(
                    &format!("Collection \"{collection}\" dropped"),
                    kept,
                ));
            }
            MongoAdminOp::RenameCollection { from, to } => {
                self.retarget_mongo_data_tabs(connection, database, from, to);
                self.refresh_mongo_database_node(connection, database, cx);
                self.sidebar.notice = Some(format!("Collection \"{from}\" renamed to \"{to}\""));
            }
            MongoAdminOp::CloneCollection { from, to, .. } => {
                self.refresh_mongo_database_node(connection, database, cx);
                self.sidebar.notice = Some(format!(
                    "Collection \"{from}\" cloned to \"{to}\" ({affected} documents)"
                ));
            }
            MongoAdminOp::CreateIndex { collection, index } => {
                self.sidebar.notice = Some(format!(
                    "Index \"{}\" created on \"{collection}\"",
                    index.effective_name()
                ));
            }
            MongoAdminOp::DropIndex { collection, name } => {
                self.sidebar.notice =
                    Some(format!("Index \"{name}\" dropped from \"{collection}\""));
            }
            MongoAdminOp::CreateDatabase => {
                self.sidebar.notice = Some(format!("Database \"{database}\" created"));
                self.make_created_database_visible(connection, database, cx);
                self.refresh_mongo_databases(connection, cx);
            }
            MongoAdminOp::DropDatabase => {
                let kept = self.close_mongo_data_tabs(connection, database, None, cx);
                let root = NavigatorNodeId::Connection {
                    connection: connection.to_string(),
                };
                if let Some(id) = self
                    .navigator
                    .find_provider_child(&root, "database", database, None)
                {
                    self.navigator.remove_node(&id);
                    self.navigator_ui.generation = self.navigator.generation();
                }
                if self.mongo_default_database(connection).as_deref() == Some(database) {
                    let _ = self.set_mongo_default_database(connection, None, cx);
                }
                self.sidebar.notice = Some(drop_notice(
                    &format!("Database \"{database}\" dropped"),
                    kept,
                ));
            }
        }
        // The driver invalidates every cached page of the connection after a
        // mutation, so the visible Mongo grid of this connection (the only
        // tab that can be re-run from here) loads a fresh page.
        let reload = self.workspace.active_tab().is_some_and(|tab| {
            tab.kind == TabKind::Query
                && tab.query_mode.is_mongo_find()
                && tab.connection_id == connection
        });
        if reload {
            self.run_active_tab_query(cx);
        }
        cx.notify();
    }

    /// Mongo data tabs of `database` (and `collection` when given).
    fn mongo_data_tab_ids(
        &self,
        connection: &str,
        database: &str,
        collection: Option<&str>,
    ) -> Vec<u64> {
        self.workspace
            .tabs()
            .iter()
            .filter(|tab| {
                tab.kind == TabKind::Query
                    && tab.query_mode.is_mongo_find()
                    && tab.connection_id == connection
                    && tab.database == database
                    && collection.is_none_or(|name| tab.collection == name)
            })
            .map(|tab| tab.id)
            .collect()
    }

    /// Closes the data tabs of a dropped object; returns how many script tabs
    /// of the same scope were kept.
    fn close_mongo_data_tabs(
        &mut self,
        connection: &str,
        database: &str,
        collection: Option<&str>,
        cx: &mut Context<Self>,
    ) -> usize {
        let kept = self
            .workspace
            .tabs()
            .iter()
            .filter(|tab| {
                tab.kind == TabKind::Query
                    && tab.query_mode.is_mongo_script()
                    && tab.connection_id == connection
                    && tab.database == database
                    && collection.is_none_or(|name| tab.collection == name)
            })
            .count();
        for id in self.mongo_data_tab_ids(connection, database, collection) {
            self.close_tab(id, cx);
        }
        kept
    }

    fn retarget_mongo_data_tabs(&mut self, connection: &str, database: &str, from: &str, to: &str) {
        let ids = self.mongo_data_tab_ids(connection, database, Some(from));
        for id in &ids {
            if let Some(tab) = self.workspace.tab_mut(*id) {
                tab.collection = to.to_string();
                if tab.title == from {
                    tab.title = to.to_string();
                }
            }
        }
        if !ids.is_empty() {
            self.workspace.normalize();
        }
    }
}

fn admin_failure_title(op: &MongoAdminOp) -> &'static str {
    match op {
        MongoAdminOp::DropCollection { .. } => "Could not drop collection",
        MongoAdminOp::RenameCollection { .. } => "Could not rename collection",
        MongoAdminOp::CloneCollection { .. } => "Could not clone collection",
        MongoAdminOp::CreateIndex { .. } => "Could not create index",
        MongoAdminOp::DropIndex { .. } => "Could not drop index",
        MongoAdminOp::DropDatabase => "Could not drop database",
        MongoAdminOp::CreateDatabase => "Could not create database",
    }
}

fn drop_notice(base: &str, kept_script_tabs: usize) -> String {
    match kept_script_tabs {
        0 => base.to_string(),
        1 => format!("{base}; 1 script tab was kept"),
        count => format!("{base}; {count} script tabs were kept"),
    }
}
