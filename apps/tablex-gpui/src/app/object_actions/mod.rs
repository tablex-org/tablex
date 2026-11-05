//! The navigator's object actions: New table, Drop, Rename and Truncate of a table, view or
//! materialized view, New schema (PostgreSQL) and New database (MySQL, MariaDB, ClickHouse).
//!
//! Every action is a reviewed change-set of the structure editor, not a statement of its own:
//! the menu entry (or the name dialog / new-table form behind it) builds typed
//! [`Change`](crate::sql_ddl::Change)s, an [`ObjectActionHost`] holds them in the shared
//! `EditSession`, and the shared review dialog shows the SQL the dialect generates, the warnings,
//! the acknowledgements (data loss, a Production connection) and the options that change the SQL
//! (CASCADE, RESTART IDENTITY). Apply runs on the dedicated session of `structure_edit::apply`,
//! never on a SQL tab's: a transaction a tab has open is neither joined nor ended. The read-only
//! and connection-type checks are `structure_edit_context`'s, the same ones the Properties editor
//! and the diagram obey.
//!
//! After an apply only the navigator group it changed reloads (in place: its expansion and the
//! rest of the tree stay), the new or renamed object is revealed and selected, and the open table
//! tabs follow: a dropped table's tabs close, a renamed one's point at the new name, a truncated
//! one's reload. A tab with staged edits, a pending structure draft or an open transaction is never
//! closed behind the user's back: it stays and the next load says what is missing.

mod host;
mod menu;
mod name_dialog;
mod table_form;

pub(in crate::app) use host::{ObjectActionEvent, ObjectActionHost};
#[cfg(all(test, feature = "bench"))]
pub(in crate::app) use table_form::NewTableForm;
pub(in crate::app) use menu::{append_object_items, ObjectKind, ObjectMenuItem};

use std::collections::HashSet;

use super::structure_edit::{self, apply::TableOrigin, ApplyTarget, EditContext};
use super::*;
use crate::sql_ddl::{Change, NamespaceKind, TableId};
use engine_driver_api::sql::SchemaObjectKind;

impl AppRoot {
    /// The object a navigator row stands for, as an apply names it (`object` is empty for a
    /// namespace). A database stands for its default schema.
    pub(in crate::app) fn object_origin(&self, id: &NavigatorNodeId) -> Option<TableOrigin> {
        let origin = |connection: &String, database: &String, schema: &str, object: &str| TableOrigin {
            connection: connection.clone(),
            database: database.clone(),
            schema: schema.to_owned(),
            object: object.to_owned(),
        };
        Some(match id {
            NavigatorNodeId::Object { connection, database, schema, object } => origin(connection, database, schema, object),
            NavigatorNodeId::Schema { connection, database, schema } | NavigatorNodeId::ObjectGroup { connection, database, schema, .. } => {
                origin(connection, database, schema, "")
            }
            NavigatorNodeId::Database { connection, database } => {
                origin(connection, database, &self.default_schema_of_database(connection, database), "")
            }
            _ => return None,
        })
    }

    /// The new-table form of a namespace row (tests and captures fill it in).
    #[cfg(all(test, feature = "bench"))]
    pub(in crate::app) fn open_new_table_form_for_test(&mut self, id: &NavigatorNodeId, window: &mut Window, cx: &mut Context<Self>) -> Option<gpui::Entity<NewTableForm>> {
        let origin = self.object_origin(id)?;
        self.open_new_table_dialog(origin, window, cx)
    }

    /// New database on `connection`, from the connection's own menu: the database it is reached
    /// through is the first one the navigator lists (the apply session connects there).
    pub(in crate::app) fn open_new_database_dialog(&mut self, connection: &str, window: &mut Window, cx: &mut Context<Self>) {
        let database = self
            .navigator
            .loaded_children(&NavigatorNodeId::Connection { connection: connection.to_owned() })
            .into_iter()
            .flatten()
            .find_map(|child| match child {
                NavigatorNodeId::Database { database, .. } => Some(database.clone()),
                _ => None,
            })
            .unwrap_or_default();
        let origin = TableOrigin { connection: connection.to_owned(), database, schema: String::new(), object: String::new() };
        self.open_name_dialog(name_dialog::NameFlow::Namespace(NamespaceKind::Database), origin, window, cx);
    }

    /// A menu entry was chosen on the row `id`.
    pub(in crate::app) fn run_object_menu_item(&mut self, item: ObjectMenuItem, id: &NavigatorNodeId, window: &mut Window, cx: &mut Context<Self>) {
        let Some(origin) = self.object_origin(id) else { return };
        match item {
            ObjectMenuItem::NewTable => {
                self.open_new_table_dialog(origin, window, cx);
            }
            // A table row loads into that table; a namespace row into a new one.
            ObjectMenuItem::ImportData => self.open_import_data(origin, window, cx),
            // A table row opens the wizard on that table (and the tables selected beside it); a schema or a database on all of its tables.
            ObjectMenuItem::TransferData | ObjectMenuItem::ExportData => {
                if let Some(init) = self.transfer_init_for_node(id, item == ObjectMenuItem::ExportData) {
                    self.open_data_transfer(init, window, cx);
                }
            }
            ObjectMenuItem::RunSqlFile => {
                let target = super::data_transfer::Namespace { connection: origin.connection, database: origin.database, schema: origin.schema };
                self.open_file_run(super::file_run::FileRunInit { target, path: None }, window, cx);
            }
            ObjectMenuItem::Backup => self.open_backup_for_node(super::backup::Mode::Backup, id, window, cx),
            ObjectMenuItem::Restore => self.open_backup_for_node(super::backup::Mode::Restore, id, window, cx),
            ObjectMenuItem::NewNamespace(kind) => self.open_name_dialog(name_dialog::NameFlow::Namespace(kind), origin, window, cx),
            ObjectMenuItem::Rename(kind) => self.open_name_dialog(name_dialog::NameFlow::Rename(kind), origin, window, cx),
            ObjectMenuItem::Drop(kind) => {
                self.open_object_review(
                origin,
                move |context, origin| {
                    let table = object_table(context, origin);
                    vec![match kind {
                        ObjectKind::Table => Change::DropTable { table, cascade: false },
                        view => Change::DropView { table, materialized: view == ObjectKind::MaterializedView, cascade: false },
                    }]
                },
                window,
                cx,
                );
            }
            ObjectMenuItem::Truncate => {
                self.open_object_review(
                origin,
                |context, origin| vec![Change::TruncateTable { table: object_table(context, origin), restart_identity: false, cascade: false }],
                window,
                cx,
                );
            }
        }
    }

    /// What the structure editor allows on `origin`'s connection and namespace, or why the action
    /// cannot even be reviewed.
    fn object_action_context(&self, origin: &TableOrigin) -> Result<EditContext, String> {
        let schema = Some(origin.schema.as_str()).filter(|schema| !schema.is_empty());
        self.structure_edit_context(&origin.connection, &origin.database, schema)
            .ok_or_else(|| "This connection has no SQL structure to change.".to_owned())
    }

    /// Opens the review dialog of the change-set `changes` builds. The host is the app's one
    /// pending object action: a dialog that was closed unapplied is replaced. `false`: no dialog was
    /// opened (the connection cannot be edited, another dialog is open), and `changes` was not asked for.
    pub(in crate::app) fn open_object_review(
        &mut self,
        origin: TableOrigin,
        changes: impl FnOnce(&EditContext, &TableOrigin) -> Vec<Change>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> bool {
        let context = match self.object_action_context(&origin) {
            Ok(context) => context,
            Err(message) => {
                self.sidebar.notice = Some(message);
                cx.notify();
                return false;
            }
        };
        let changes = changes(&context, &origin);
        let host = cx.new(|_| ObjectActionHost::new(origin, context, changes));
        cx.subscribe(&host, |this: &mut Self, host, event: &ObjectActionEvent, cx| {
            let target = ApplyTarget::Object { edit: host.entity_id().as_u64() };
            match event {
                ObjectActionEvent::Apply(request) => this.apply_structure_changes(target, (**request).clone(), cx),
                ObjectActionEvent::CancelApply => this.cancel_structure_apply(target, cx),
            }
        })
        .detach();
        self.object_action = Some(host.clone());
        let opened = host.update(cx, |host, cx| structure_edit::open_review(host, window, cx));
        if !opened {
            self.object_action = None;
        }
        opened
    }

    /// The server's `sql_mode` was learned (an apply refused to run SQL that differs from the SQL
    /// reviewed): the open dialog regenerates its SQL for it.
    pub(in crate::app) fn object_action_context_changed(&mut self, cx: &mut Context<Self>) {
        let Some(host) = self.object_action.clone() else { return };
        let origin = host.read(cx).origin();
        if let Ok(context) = self.object_action_context(&origin) {
            host.update(cx, |host, cx| host.set_context(context, cx));
        }
    }

    /// An apply asked from the dialog of `edit` ended.
    pub(in crate::app) fn object_action_answer(&mut self, edit: u64, done: &[usize], partial: &[usize], report: String, failed: bool, cx: &mut Context<Self>) {
        let found = self.object_action.clone().filter(|entity| entity.entity_id().as_u64() == edit);
        let created = !failed && !done.is_empty();
        if let Some(host) = found {
            host.update(cx, |host, cx| host.apply_finished(done, partial, report, failed, cx));
        }
        // The table an import was waiting for exists now.
        if created {
            self.import_table_created(edit, cx);
            self.transfer_tables_created(edit, done, cx);
        }
    }

    /// The names the namespace of `origin` already holds, as far as the navigator has loaded them:
    /// tables, views and materialized views share one namespace.
    pub(in crate::app) fn loaded_object_names(&self, origin: &TableOrigin) -> HashSet<String> {
        [ObjectGroup::Tables, ObjectGroup::Views, ObjectGroup::MaterializedViews]
            .into_iter()
            .filter_map(|group| {
                self.navigator.loaded_children(&navigator::object_group_id(&origin.connection, &origin.database, &origin.schema, group))
            })
            .flatten()
            .filter_map(|child| match child {
                NavigatorNodeId::Object { object, .. } => Some(object.clone()),
                _ => None,
            })
            .collect()
    }

    // ---- after an apply ----------------------------------------------------------------------

    /// The change-set `changes` ran on the object `origin` stands for: reload only the group it
    /// changed, reveal what it made, and bring the open tabs in line.
    pub(in crate::app) fn refresh_after_object_action(&mut self, origin: &TableOrigin, changes: &[Change], cx: &mut Context<Self>) {
        let TableOrigin { connection, database, schema, .. } = origin;
        let group = |group| navigator::object_group_id(connection, database, schema, group);
        let object = |name: &str| NavigatorNodeId::Object {
            connection: connection.clone(),
            database: database.clone(),
            schema: schema.clone(),
            object: name.to_owned(),
        };
        let path_to = |group: ObjectGroup, name: &str| {
            vec![
                NavigatorNodeId::Connection { connection: connection.clone() },
                NavigatorNodeId::Database { connection: connection.clone(), database: database.clone() },
                NavigatorNodeId::Schema { connection: connection.clone(), database: database.clone(), schema: schema.clone() },
                group_node(connection, database, schema, group),
                object(name),
            ]
        };
        for change in changes {
            match change {
                Change::CreateTable { table, .. } => {
                    self.reload_navigator_node(&group(ObjectGroup::Tables), cx);
                    self.invalidate_table_properties(&NavigatorNodeId::Schema { connection: connection.clone(), database: database.clone(), schema: schema.clone() });
                    self.reveal_path_in_sidebar(path_to(ObjectGroup::Tables, &table.name), cx);
                }
                Change::DropTable { table, cascade, .. } | Change::DropView { table, cascade, .. } => {
                    let kind = match change {
                        Change::DropView { materialized: true, .. } => ObjectKind::MaterializedView,
                        Change::DropView { .. } => ObjectKind::View,
                        _ => ObjectKind::Table,
                    };
                    let list = group(kind.group());
                    self.reload_navigator_node(&list, cx);
                    if *cascade {
                        self.reload_dependent_groups(origin, kind, cx);
                    }
                    self.invalidate_table_properties(&object(&table.name));
                    let kept = self.close_tabs_of_object(origin, &table.name, cx);
                    self.navigator.select_visible(&list, false, false);
                    self.sidebar.notice = (kept > 0).then(|| {
                        format!("{} open tab(s) showing {} have unsaved work and stay open; close them when you are done.", kept, table.name)
                    });
                }
                Change::RenameTable { table, to } | Change::RenameView { table, to, .. } => {
                    let kind = match change {
                        Change::RenameView { materialized: true, .. } => ObjectKind::MaterializedView,
                        Change::RenameView { .. } => ObjectKind::View,
                        _ => ObjectKind::Table,
                    };
                    self.reload_navigator_node(&group(kind.group()), cx);
                    self.invalidate_table_properties(&object(&table.name));
                    self.retarget_tabs_of_object(origin, &table.name, to, cx);
                    self.reveal_path_in_sidebar(path_to(kind.group(), to), cx);
                }
                Change::TruncateTable { table, cascade, .. } => {
                    let truncated = TableOrigin { object: table.name.clone(), ..origin.clone() };
                    self.refresh_table_tabs_after_apply(&truncated, cx);
                    if *cascade {
                        // CASCADE emptied tables the dialog does not name: every table tab of the schema reloads.
                        for name in self.table_tab_objects(origin) {
                            let emptied = TableOrigin { object: name, ..origin.clone() };
                            self.refresh_table_tabs_after_apply(&emptied, cx);
                        }
                    }
                }
                Change::CreateNamespace { kind, name } => self.refresh_after_namespace_created(origin, *kind, name, cx),
                _ => {}
            }
        }
        // The server's users and privileges changed: the tabs that list them read again, once for the whole change-set.
        if changes.iter().any(|change| matches!(change, Change::Account(_))) {
            self.accounts_after_apply(connection, cx);
        }
        self.refresh_schema_diagrams_of(origin, cx);
        cx.notify();
    }

    /// CASCADE dropped what depends on the object: the views (and materialized views) of the schema
    /// that are loaded list again. A group never loaded loads fresh when it is opened.
    fn reload_dependent_groups(&mut self, origin: &TableOrigin, dropped: ObjectKind, cx: &mut Context<Self>) {
        let TableOrigin { connection, database, schema, .. } = origin;
        for group in [ObjectGroup::Tables, ObjectGroup::Views, ObjectGroup::MaterializedViews] {
            if group != dropped.group() {
                self.reload_navigator_node(&group_node(connection, database, schema, group), cx);
            }
        }
    }

    /// The names of the tables that have an open table-data tab in the namespace of `origin`.
    fn table_tab_objects(&self, origin: &TableOrigin) -> HashSet<String> {
        self.workspace
            .tabs()
            .iter()
            .filter(|tab| {
                tab.query_mode.is_table_data()
                    && tab.connection_id == origin.connection
                    && tab.database == origin.database
                    && tab.schema.as_deref().unwrap_or_default() == origin.schema
            })
            .map(|tab| tab.collection.clone())
            .collect()
    }

    /// The whole-schema diagrams open on the namespace reload: a table may be gone, renamed or new.
    fn refresh_schema_diagrams_of(&mut self, origin: &TableOrigin, cx: &mut Context<Self>) {
        if self.scenario_ui.service.is_some() {
            return;
        }
        let diagrams: Vec<u64> = self
            .workspace
            .tabs()
            .iter()
            .filter(|tab| {
                tab.kind == TabKind::Diagram
                    && tab.connection_id == origin.connection
                    && tab.database == origin.database
                    && tab.schema.as_deref().unwrap_or_default() == origin.schema
            })
            .map(|tab| tab.id)
            .collect();
        for id in diagrams {
            self.refresh_schema_diagram(id, cx);
        }
    }

    /// Loads the children of `id` again in place (nothing happens for a node never loaded: its
    /// first expansion loads it).
    pub(in crate::app) fn reload_navigator_node(&mut self, id: &NavigatorNodeId, cx: &mut Context<Self>) {
        if self.scenario_ui.service.is_some() {
            return;
        }
        if let Some(request) = self.navigator.reload_children(id) {
            self.navigator_ui.generation = self.navigator.generation();
            self.ensure_connected_then_load(request, cx);
        }
    }

    /// The Visible Object Filter of a connection is a whitelist of databases (and hides the system
    /// ones unless asked): a database created behind it would stay hidden and could not be revealed.
    /// Its name joins the whitelist (and system databases are shown when it is one). A connection
    /// without a filter needs nothing.
    pub(in crate::app) fn make_created_database_visible(&mut self, connection: &str, name: &str, cx: &mut Context<Self>) {
        let Some(filter) = self.navigator.preferences().visible_filters.get(connection) else { return };
        let needs_selection = filter.databases.as_ref().is_some_and(|names| !names.iter().any(|existing| existing == name));
        let needs_system = !filter.show_system && crate::sidebar_preferences::is_system_database(name);
        if !needs_selection && !needs_system {
            return;
        }
        let result = self.change_sidebar_preferences(
            |prefs| {
                let filter = prefs.visible_filters.get_mut(connection).ok_or("Visible filter missing")?;
                if let Some(names) = filter.databases.as_mut().filter(|names| !names.iter().any(|existing| existing == name)) {
                    names.push(name.to_owned());
                }
                if needs_system {
                    filter.show_system = true;
                }
                Ok(())
            },
            cx,
        );
        self.report_visibility_not_saved(connection, "Database", result);
    }

    /// The same for a schema created in `database` behind that database's schema whitelist.
    fn make_created_schema_visible(&mut self, connection: &str, database: &str, name: &str, cx: &mut Context<Self>) {
        let hidden = self
            .navigator
            .preferences()
            .visible_filters
            .get(connection)
            .and_then(|filter| filter.schemas.get(database))
            .is_some_and(|names| names.as_ref().is_some_and(|names| !names.iter().any(|existing| existing == name)));
        if !hidden {
            return;
        }
        let result = self.change_sidebar_preferences(
            |prefs| {
                let names = prefs
                    .visible_filters
                    .get_mut(connection)
                    .and_then(|filter| filter.schemas.get_mut(database))
                    .and_then(Option::as_mut)
                    .ok_or("Visible filter missing")?;
                names.push(name.to_owned());
                Ok(())
            },
            cx,
        );
        self.report_visibility_not_saved(connection, "Schema", result);
    }

    fn report_visibility_not_saved<E: std::fmt::Display>(&mut self, connection: &str, what: &str, result: Result<(), E>) {
        if let Err(error) = result {
            let message = format!("{what} created, but visibility could not be saved: {error}");
            self.report_warning(Area::Admin, Some(connection), &message);
            self.sidebar.notice = Some(message);
        }
    }

    /// A schema (PostgreSQL) shows up in its database, a database (the others) in its connection.
    fn refresh_after_namespace_created(&mut self, origin: &TableOrigin, kind: NamespaceKind, name: &str, cx: &mut Context<Self>) {
        let TableOrigin { connection, database, .. } = origin;
        let connection_node = NavigatorNodeId::Connection { connection: connection.clone() };
        let (parent, created) = match kind {
            NamespaceKind::Schema => (
                NavigatorNodeId::Database { connection: connection.clone(), database: database.clone() },
                NavigatorNodeId::Schema { connection: connection.clone(), database: database.clone(), schema: name.to_owned() },
            ),
            NamespaceKind::Database => (connection_node.clone(), NavigatorNodeId::Database { connection: connection.clone(), database: name.to_owned() }),
        };
        match kind {
            NamespaceKind::Database => self.make_created_database_visible(connection, name, cx),
            NamespaceKind::Schema => self.make_created_schema_visible(connection, database, name, cx),
        }
        self.reload_navigator_node(&parent, cx);
        let mut path = vec![connection_node];
        if kind == NamespaceKind::Schema {
            path.push(parent);
        }
        path.push(created);
        self.reveal_path_in_sidebar(path, cx);
    }

    /// The table-data (and materialized view source) tabs that show `object` of `origin`'s namespace.
    fn tabs_of_object(&self, origin: &TableOrigin, object: &str) -> Vec<u64> {
        self.workspace
            .tabs()
            .iter()
            .filter(|tab| {
                let same_place = tab.connection_id == origin.connection
                    && tab.database == origin.database
                    && tab.schema.as_deref().unwrap_or_default() == origin.schema;
                let table_data = tab.query_mode.is_table_data() && tab.collection == object;
                let source = tab.kind == TabKind::Source
                    && tab.source.as_ref().is_some_and(|source| source.name == object && source.kind == SchemaObjectKind::MaterializedView);
                same_place && (table_data || source)
            })
            .map(|tab| tab.id)
            .collect()
    }

    /// Whether closing or retargeting the tab would lose something the user has not saved: staged
    /// row edits, a structure draft of its Properties, statements in an open transaction.
    fn tab_has_unsaved_work(&self, tab_id: u64, cx: &gpui::App) -> bool {
        self.workspace.tab_has_staged_changes(tab_id)
            || self.structure_draft_count(tab_id, cx) > 0
            || !self.open_transaction_tabs([tab_id]).is_empty()
    }

    /// Closes the tabs showing the dropped `object`; those with unsaved work stay. Returns how many stayed.
    fn close_tabs_of_object(&mut self, origin: &TableOrigin, object: &str, cx: &mut Context<Self>) -> usize {
        let (stay, close): (Vec<u64>, Vec<u64>) = self.tabs_of_object(origin, object).into_iter().partition(|id| self.tab_has_unsaved_work(*id, cx));
        if !close.is_empty() {
            self.close_tabs_now(&close, None, cx);
        }
        stay.len()
    }

    /// Points the table-data tabs of `from` at `to` (title, statement, columns, rows); a source tab
    /// of a materialized view shows a definition that no longer matches its name, so it closes.
    fn retarget_tabs_of_object(&mut self, origin: &TableOrigin, from: &str, to: &str, cx: &mut Context<Self>) {
        let dialect = self.sql_dialect_for_connection(&origin.connection).unwrap_or(crate::sql_table::SqlDialect::POSTGRES);
        let (data, sources): (Vec<u64>, Vec<u64>) = self
            .tabs_of_object(origin, from)
            .into_iter()
            .filter(|id| !self.tab_has_unsaved_work(*id, cx))
            .partition(|id| self.workspace.tab(*id).is_some_and(|tab| tab.kind != TabKind::Source));
        for id in data {
            if let Some(tab) = self.workspace.tab_mut(id) {
                if tab.title == from {
                    tab.title = to.to_owned();
                }
                tab.collection = to.to_owned();
                tab.sql = crate::sql_table::base_select(dialect, Some(&origin.schema), to).unwrap_or_default();
            }
        }
        if !sources.is_empty() {
            self.close_tabs_now(&sources, None, cx);
        }
        let renamed = TableOrigin { object: to.to_owned(), ..origin.clone() };
        self.refresh_table_tabs_after_apply(&renamed, cx);
    }
}

fn group_node(connection: &str, database: &str, schema: &str, group: ObjectGroup) -> NavigatorNodeId {
    navigator::object_group_id(connection, database, schema, group)
}

/// The table or view `origin` names, as a change names it.
fn object_table(context: &EditContext, origin: &TableOrigin) -> TableId {
    context.scope.table_id(&origin.database, Some(origin.schema.as_str()).filter(|schema| !schema.is_empty()), &origin.object)
}
