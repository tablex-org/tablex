//! The names of the sidebar's rows as text, and the keyboard commands of its tree.
//!
//! * Copy Name / Copy Qualified Name (context menu, Cmd+C, Cmd+Shift+C): one name per line for a
//!   multi-selection.
//! * Dragging table, view, column, schema and collection rows onto a query editor inserts their
//!   names at the drop point, comma-separated for a multi-selection.
//! * F2 / Delete / F4 on the focused tree: rename, delete and open the selected object through the
//!   flows the context menus use (a confirmation or the review dialog always comes first).
//!
//! Qualification and quoting. SQL names are always quoted by the connection's dialect (the
//! advertised one, else the kind's), so a reserved word or a mixed-case name is safe. *Copy Name*
//! is the bare name as the tree shows it, unquoted. *Copy Qualified Name* is the whole path as a
//! statement would spell it: `"schema"."table"` (`` `database`.`table` `` where the dialect's schema
//! is the database), `"schema"."table"."column"` for a column, `database.collection` for a MongoDB
//! collection. A drop on an editor qualifies a table unless it is in the editor's own namespace (same connection,
//! database and schema), so a statement stays short where it can; a column is its bare name, a
//! schema is its quoted name, a collection is its name (`database.collection` from another database).

use super::object_actions::ObjectMenuItem;
use super::*;
use crate::sql_ddl::{dialect_for, NamespaceKind};
use crate::sql_table::{qualified_table, SqlDialect};

/// How a row's name is spelled.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum NameStyle {
    /// The name as the tree shows it.
    Plain,
    /// The full path to it, quoted for a statement.
    Qualified,
}

/// The default namespace of the editor a name is dropped into.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct EditorScope {
    pub(super) connection: String,
    pub(super) database: String,
    pub(super) schema: Option<String>,
}

/// A name that cannot be spelled safely is an `Err` with the reason; a row without a name is `Ok(None)`.
type Named = Result<Option<String>, String>;

impl AppRoot {
    /// Whether dragging the row `id` onto a query editor inserts a name: tables, views, columns and
    /// schemas of a SQL connection, collections of a MongoDB one.
    pub(super) fn object_drag_supported(&self, id: &NavigatorNodeId) -> bool {
        match id {
            NavigatorNodeId::Object { .. } | NavigatorNodeId::Column { .. } | NavigatorNodeId::Schema { .. } => true,
            NavigatorNodeId::Provider { .. } => self.mongo_node_target(id).is_some_and(|target| target.collection.is_some()),
            _ => false,
        }
    }

    /// `id`'s dialect: the advertised one, else the kind's.
    fn name_dialect(&self, connection: &str) -> Option<SqlDialect> {
        self.sql_dialect_for_connection(connection)
    }

    /// The namespace a table of `connection` is qualified with: its schema, else (where the dialect's
    /// schema is the database: MySQL, MariaDB, ClickHouse, SQLite) its database.
    fn name_namespace(&self, connection: &str, database: &str, schema: &str) -> Option<String> {
        let kind = self.saved_connections.iter().find(|saved| saved.matches_id(connection))?.kind;
        Some(schema)
            .filter(|schema| !schema.is_empty())
            .or_else(|| dialect_for(kind).filter(|ddl| ddl.schema_is_database()).and(Some(database)).filter(|database| !database.is_empty()))
            .map(str::to_owned)
    }

    /// The text of the row `id`, as `style` says. `Ok(None)`: the row has no name to give.
    pub(super) fn navigator_name_text(&self, id: &NavigatorNodeId, style: NameStyle) -> Named {
        let qualified = style == NameStyle::Qualified;
        let quote_one = |connection: &str, name: &str| -> Result<String, String> {
            let dialect = self.name_dialect(connection).ok_or("the connection has no SQL dialect")?;
            dialect.checked_quote(name).map_err(|error| error.to_string())
        };
        let table = |connection: &str, database: &str, schema: &str, object: &str| -> Result<String, String> {
            let dialect = self.name_dialect(connection).ok_or("the connection has no SQL dialect")?;
            let namespace = self.name_namespace(connection, database, schema);
            qualified_table(dialect, namespace.as_deref(), object).map_err(|error| error.to_string())
        };
        let text = match id {
            NavigatorNodeId::Connection { connection } => self.saved_connections.iter().find(|saved| saved.matches_id(connection)).map(|saved| saved.name.clone()),
            NavigatorNodeId::Group { id } => self.navigator.preferences().groups.iter().find(|group| &group.id == id).map(|group| group.name.clone()),
            NavigatorNodeId::Database { connection, database } if qualified => Some(quote_one(connection, database)?),
            NavigatorNodeId::Database { database, .. } => Some(database.clone()),
            NavigatorNodeId::Schema { connection, schema, .. } if qualified => Some(quote_one(connection, schema)?),
            NavigatorNodeId::Schema { schema, .. } => Some(schema.clone()),
            NavigatorNodeId::Object { connection, database, schema, object } if qualified => Some(table(connection, database, schema, object)?),
            NavigatorNodeId::Object { object, .. } => Some(object.clone()),
            NavigatorNodeId::Column { connection, database, schema, object, column } if qualified => {
                Some(format!("{}.{}", table(connection, database, schema, object)?, quote_one(connection, column)?))
            }
            NavigatorNodeId::Column { column, .. } => Some(column.clone()),
            NavigatorNodeId::SchemaObject { connection, database, schema, object, .. } => {
                let name = self.navigator.schema_object(id).map_or_else(|| object.clone(), |found| found.name.clone());
                if qualified {
                    Some(table(connection, database, schema, &name)?)
                } else {
                    Some(name)
                }
            }
            NavigatorNodeId::Provider { .. } => {
                if let Some(target) = self.mongo_node_target(id) {
                    Some(match target.collection {
                        Some(collection) if qualified => format!("{}.{collection}", target.database),
                        Some(collection) => collection,
                        None => target.database,
                    })
                } else {
                    use super::redis::RedisTarget;
                    match self.redis_node_target(id) {
                        Some(RedisTarget::Key { display, .. }) => Some(display),
                        Some(RedisTarget::Namespace { prefix, .. }) => Some(prefix),
                        Some(RedisTarget::Database { db, .. }) => Some(db),
                        None => None,
                    }
                }
            }
            NavigatorNodeId::TableItem { .. } => self.navigator.label(id).map(str::to_owned),
            NavigatorNodeId::ObjectGroup { .. } | NavigatorNodeId::TableFolder { .. } => None,
        };
        Ok(text)
    }

    /// What dropping the row `id` on an editor of `scope` inserts (see the module docs).
    pub(super) fn editor_drop_text(&self, id: &NavigatorNodeId, scope: &EditorScope) -> Named {
        match id {
            NavigatorNodeId::Object { connection, database, schema, object } => {
                let dialect = self.name_dialect(connection).ok_or("the connection has no SQL dialect")?;
                let namespace = self.name_namespace(connection, database, schema);
                // The editor's own namespace: its schema, or its database where that is the dialect's schema.
                let scope_namespace = self.name_namespace(&scope.connection, &scope.database, scope.schema.as_deref().unwrap_or_default());
                let same_namespace = scope.connection == *connection && scope.database == *database && scope_namespace == namespace;
                let name = qualified_table(dialect, if same_namespace { None } else { namespace.as_deref() }, object);
                Ok(Some(name.map_err(|error| error.to_string())?))
            }
            NavigatorNodeId::Column { connection, column, .. } => {
                let dialect = self.name_dialect(connection).ok_or("the connection has no SQL dialect")?;
                Ok(Some(dialect.checked_quote(column).map_err(|error| error.to_string())?))
            }
            NavigatorNodeId::Schema { .. } => self.navigator_name_text(id, NameStyle::Qualified),
            NavigatorNodeId::Provider { .. } => {
                let Some(target) = self.mongo_node_target(id) else { return Ok(None) };
                Ok(target.collection.map(|collection| {
                    if scope.connection == target.connection && scope.database == target.database {
                        collection
                    } else {
                        format!("{}.{collection}", target.database)
                    }
                }))
            }
            _ => Ok(None),
        }
    }

    /// The rows an action of the context menu on `id` is about: the selection when `id` is part of a
    /// multi-selection, else `id` alone (a right click on an unselected row acts on that row).
    pub(super) fn rows_for_name_action(&self, id: &NavigatorNodeId) -> Vec<NavigatorNodeId> {
        if self.navigator.selected_ids().len() > 1 && self.navigator.selected_ids().contains(id) {
            self.navigator.selection_in_visible_order()
        } else {
            vec![id.clone()]
        }
    }

    /// Puts the names of `ids` on the clipboard, one per line. A name that cannot be spelled safely
    /// is reported in a notice and left out.
    pub(super) fn copy_navigator_names(&mut self, ids: &[NavigatorNodeId], style: NameStyle, cx: &mut Context<Self>) -> bool {
        let mut lines = Vec::new();
        let mut failure = None;
        for id in ids {
            match self.navigator_name_text(id, style) {
                Ok(Some(text)) => lines.push(text),
                Ok(None) => {}
                Err(error) => failure = Some(error),
            }
        }
        if let Some(error) = failure {
            self.sidebar.notice = Some(t!("sidebar.name_failed", error = error).into_owned());
            cx.notify();
        }
        if lines.is_empty() {
            return false;
        }
        cx.write_to_clipboard(gpui::ClipboardItem::new_string(lines.join("\n")));
        true
    }

    /// Inserts the names of `ids` into the query editor of `tab_id`, at the point `at` of the window
    /// when the editor can tell the character there, else at its caret.
    pub(super) fn drop_objects_on_editor(&mut self, tab_id: u64, ids: &[NavigatorNodeId], at: gpui::Point<gpui::Pixels>, window: &mut Window, cx: &mut Context<Self>) {
        let Some(tab) = self.workspace.tab(tab_id) else { return };
        let scope = EditorScope { connection: tab.connection_id.clone(), database: tab.database.clone(), schema: tab.schema.clone() };
        let mut names = Vec::new();
        for id in ids {
            match self.editor_drop_text(id, &scope) {
                Ok(Some(name)) => names.push(name),
                Ok(None) => {}
                Err(error) => {
                    self.sidebar.notice = Some(t!("sidebar.name_failed", error = error).into_owned());
                    cx.notify();
                    return;
                }
            }
        }
        if names.is_empty() {
            return;
        }
        let text = names.join(", ");
        let editor = self.editor_for_tab(tab_id, window, cx);
        editor.update(cx, |editor, cx| {
            // `character_index_for_point` answers in UTF-16 units, `None` outside the drawn text.
            match gpui::EntityInputHandler::character_index_for_point(editor, at, window, cx) {
                Some(index) => gpui::EntityInputHandler::replace_text_in_range(editor, Some(index..index), &text, window, cx),
                None => editor.insert(text, window, cx),
            }
            editor.focus(window, cx);
        });
        // The editor reports its own change; a silent insert still reaches the tab.
        let text = editor.read(cx).value().to_string();
        if let Some(tab) = self.workspace.tab_mut(tab_id) {
            tab.sql = text;
        }
        cx.notify();
    }

    // ---- keyboard on the tree ----------------------------------------------------------------

    /// The one row F2 / Delete / F4 act on: the focused row, when it is the only selected one.
    fn single_selected_row(&self) -> Option<NavigatorRow> {
        let id = self.navigator.selected().cloned()?;
        (self.navigator.selected_ids().len() <= 1).then_some(())?;
        self.navigator.visible_index(&id).and_then(|index| self.navigator.visible_row_at(index))
    }

    /// F2: rename the selected row through its own flow. A connection opens its edit dialog (the name
    /// is its first field); a group its rename dialog; a table, view or collection its rename flow
    /// (a table or view is reviewed before anything runs); a Redis key its rename form.
    pub(super) fn rename_selected_sidebar_node(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(row) = self.single_selected_row() else { return };
        if self.navigator.action_target(&row.id).is_none() {
            return;
        }
        match &row.id {
            NavigatorNodeId::Connection { connection } => {
                let saved = self.saved_connections.iter().find(|saved| saved.matches_id(connection)).cloned();
                self.open_connection_dialog(saved, window, cx);
            }
            NavigatorNodeId::Group { id } => self.open_rename_sidebar_group_dialog(id.clone(), row.label.clone(), window, cx),
            NavigatorNodeId::Object { .. } => {
                if let Some(item) = self.object_menu_items(&row.id).into_iter().find(|item| matches!(item, ObjectMenuItem::Rename(_))) {
                    self.run_object_menu_item(item, &row.id, window, cx);
                }
            }
            NavigatorNodeId::Provider { .. } => {
                if let Some(target) = self.mongo_node_target(&row.id) {
                    if target.collection.is_some() && self.mongo_admin_caps(&target.connection).collection {
                        self.open_mongo_rename_dialog(target, window, cx);
                    }
                } else if let Some(super::redis::RedisTarget::Key { connection, db, key_raw, display, .. }) = self.redis_node_target(&row.id) {
                    self.open_redis_rename_form(&connection, &db, &key_raw, &display, window, cx);
                }
            }
            _ => {}
        }
    }

    /// Delete: delete the selected row(s) through the flow of the menu entry, which always asks first
    /// (a confirmation, the typed name of MongoDB's drop, the review dialog of a table or view).
    /// Only connections are deleted in a batch; any other multi-selection does nothing.
    pub(super) fn delete_selected_sidebar_node(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.navigator.connection_multi_mode() {
            self.confirm_delete_selected_connections(window, cx);
            return;
        }
        let Some(row) = self.single_selected_row() else { return };
        if self.navigator.action_target(&row.id).is_none() {
            return;
        }
        match &row.id {
            NavigatorNodeId::Connection { connection } => self.delete_connection_with_confirmation(connection.clone(), window, cx),
            NavigatorNodeId::Group { id } => self.confirm_delete_sidebar_group(id.clone(), window, cx),
            NavigatorNodeId::Object { .. } => {
                if let Some(item) = self.object_menu_items(&row.id).into_iter().find(|item| matches!(item, ObjectMenuItem::Drop(_))) {
                    self.run_object_menu_item(item, &row.id, window, cx);
                }
            }
            NavigatorNodeId::Provider { .. } => {
                if let Some(target) = self.mongo_node_target(&row.id) {
                    self.open_mongo_drop_dialog(target, window, cx);
                } else if let Some(super::redis::RedisTarget::Key { connection, db, key_raw, display, .. }) = self.redis_node_target(&row.id) {
                    self.confirm_delete_redis_key_named(&connection, &db, &key_raw, &display, window, cx);
                }
            }
            _ => {}
        }
    }

    /// F4: open the selected table, view, function, collection or Redis key, as Open does.
    pub(super) fn open_selected_sidebar_node(&mut self, cx: &mut Context<Self>) {
        let Some(row) = self.single_selected_row() else { return };
        if row.disabled || self.navigator.action_target(&row.id).is_none() {
            return;
        }
        match &row.id {
            NavigatorNodeId::Object { .. } | NavigatorNodeId::SchemaObject { .. } => self.handle_navigator_open(&row, cx),
            NavigatorNodeId::Provider { .. } => {
                if let Some(target) = self.mongo_node_target(&row.id) {
                    if target.collection.is_some() {
                        self.open_mongo_collection_data(&target, false, cx);
                    }
                } else {
                    self.open_redis_key_node(&row.id, cx);
                }
            }
            _ => {}
        }
    }
}

/// The Copy Name / Copy Qualified Name entries (`styles`) of a row's menu.
pub(super) fn append_copy_name_items(menu: gpui_component::menu::PopupMenu, root: gpui::WeakEntity<AppRoot>, id: &NavigatorNodeId, styles: &[NameStyle]) -> gpui_component::menu::PopupMenu {
    styles.iter().fold(menu, |menu, &style| {
        let (label, selector) = match style {
            NameStyle::Plain => (t!("menu.copy_name"), "sidebar-menu-copy-name"),
            NameStyle::Qualified => (t!("sidebar.copy_qualified_name"), "sidebar-menu-copy-qualified-name"),
        };
        let (root, id) = (root.clone(), id.clone());
        menu.item(
            PopupMenuItem::element(move |_, _| div().id(selector).debug_selector(move || selector.to_string()).child(label.clone())).on_click(move |_, _, app| {
                let _ = root.update(app, |this, cx| {
                    let ids = this.rows_for_name_action(&id);
                    this.copy_navigator_names(&ids, style, cx);
                });
            }),
        )
    })
}

/// The kinds that can create a database, known without asking the driver: a SQL kind whose dialect
/// creates databases, and MongoDB (its driver's `create_database` mutation).
pub(super) fn kind_creates_databases(kind: ConnectionKind) -> bool {
    match dialect_for(kind) {
        Some(ddl) => ddl.object_capabilities().create_namespace.contains(&NamespaceKind::Database),
        None => kind == ConnectionKind::Mongodb,
    }
}
