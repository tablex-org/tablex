//! The "New view / function / index / ..." entries of the navigator: they open a new SQL tab on the
//! row's connection, database and schema with the dialect's template (see `create_templates`) and
//! select its first placeholder. Nothing runs; a read-only connection refuses when the statement is
//! run, as it does for any SQL tab. New collection of a MongoDB database is the same, as a script tab.

use super::super::*;
use super::create_templates::{self, CreateKind, Level, Template};
use crate::connections::ConnectionKind;
use crate::sql_ddl::dialect_for;

impl CreateKind {
    pub(in crate::app) fn label(self) -> std::borrow::Cow<'static, str> {
        match self {
            Self::View => t!("menu.new_view"),
            Self::MaterializedView => t!("menu.new_materialized_view"),
            Self::Function => t!("menu.new_function"),
            Self::Procedure => t!("menu.new_procedure"),
            Self::Sequence => t!("menu.new_sequence"),
            Self::Trigger => t!("menu.new_trigger"),
            Self::Type => t!("menu.new_type"),
            Self::Column => t!("menu.new_column"),
            Self::Index => t!("menu.new_index"),
            Self::Constraint => t!("menu.new_constraint"),
            Self::ForeignKey => t!("menu.new_foreign_key"),
        }
    }

    pub(in crate::app) fn icon(self) -> Icon {
        let path = match self {
            Self::View | Self::MaterializedView => "icons/eye.svg",
            Self::Function | Self::Procedure => "icons/play.svg",
            Self::Trigger => "icons/zap.svg",
            Self::Column => "icons/columns-3.svg",
            Self::Index => "icons/list-ordered.svg",
            Self::Constraint => "icons/key-round.svg",
            Self::ForeignKey => "icons/link.svg",
            Self::Sequence | Self::Type => "icons/circle-plus.svg",
        };
        Icon::empty().path(path).size(px(12.0))
    }
}

impl AppRoot {
    /// The kind of the SQL connection a row belongs to (`None`: not SQL).
    fn sql_kind_of(&self, connection: &str) -> Option<ConnectionKind> {
        self.saved_connections.iter().find(|saved| saved.matches_id(connection)).map(|saved| saved.kind).filter(|kind| dialect_for(*kind).is_some())
    }

    /// The "New ..." entries the row `id` offers, by what its connection's dialect can create: a
    /// schema or database row all of the schema's, a folder the one of what it lists, a table row
    /// and its folders those of a table.
    pub(in crate::app) fn create_menu_kinds(&self, id: &NavigatorNodeId) -> Vec<CreateKind> {
        let connection = match id {
            NavigatorNodeId::Database { connection, .. }
            | NavigatorNodeId::Schema { connection, .. }
            | NavigatorNodeId::ObjectGroup { connection, .. }
            | NavigatorNodeId::Object { connection, .. }
            | NavigatorNodeId::TableFolder { connection, .. } => connection,
            _ => return Vec::new(),
        };
        let Some(kind) = self.sql_kind_of(connection) else { return Vec::new() };
        match id {
            NavigatorNodeId::Database { .. } | NavigatorNodeId::Schema { .. } => create_templates::offered(kind, Level::Namespace),
            NavigatorNodeId::ObjectGroup { group, .. } => create_templates::for_group(kind, *group),
            NavigatorNodeId::TableFolder { folder, .. } => create_templates::for_table_folder(kind, *folder).into_iter().collect(),
            NavigatorNodeId::Object { .. } if matches!(super::ObjectKind::of(self.navigator.object_kind(id)), Some(super::ObjectKind::Table)) => {
                create_templates::offered(kind, Level::Table)
            }
            _ => Vec::new(),
        }
    }

    /// Opens a new SQL tab on the row `id` with the template of `create`, its first placeholder
    /// selected. A name the dialect cannot quote is reported and no tab opens.
    pub(in crate::app) fn open_create_template(&mut self, create: CreateKind, id: &NavigatorNodeId, cx: &mut Context<Self>) {
        if !self.create_menu_kinds(id).contains(&create) {
            return;
        }
        let Some(origin) = self.object_origin(id) else { return };
        let Some(kind) = self.sql_kind_of(&origin.connection) else { return };
        let dialect = self.sql_dialect_for_connection(&origin.connection).unwrap_or(crate::sql_table::SqlDialect::for_kind(kind));
        // MySQL, MariaDB and ClickHouse qualify with the database.
        let namespace = Some(origin.schema.as_str())
            .filter(|schema| !schema.is_empty())
            .or_else(|| dialect_for(kind).filter(|ddl| ddl.schema_is_database()).and(Some(origin.database.as_str())).filter(|database| !database.is_empty()));
        let table = Some(origin.object.as_str()).filter(|object| !object.is_empty());
        match create_templates::template(kind, dialect, create, namespace, table) {
            Some(Ok(template)) => {
                let metadata = TabMetadata {
                    connection_id: origin.connection.clone(),
                    database: origin.database.clone(),
                    schema: Some(origin.schema.clone()).filter(|schema| !schema.is_empty()),
                    ..Default::default()
                };
                let tab = self.workspace.open_query_tab_with_metadata("Query", metadata);
                self.fill_template_tab(tab, template);
                self.notify_chrome(cx);
            }
            Some(Err(error)) => {
                let message = format!("Could not write the template: {error}");
                self.report_error(Area::Query, Some(&origin.connection), message.clone());
                self.sidebar.notice = Some(message);
                cx.notify();
            }
            None => {}
        }
    }

    /// New collection on a MongoDB database row: a script tab with `createCollection`.
    pub(in crate::app) fn open_mongo_collection_template(&mut self, connection: &str, database: &str, cx: &mut Context<Self>) {
        let tab = self.open_mongo_script_tab(connection, database, "");
        self.fill_template_tab(tab, create_templates::mongo_new_collection());
        self.notify_chrome(cx);
    }

    /// The tab holds the template; its editor selects the placeholder when it is created.
    fn fill_template_tab(&mut self, tab: u64, template: Template) {
        if let Some(tab) = self.workspace.tab_mut(tab) {
            tab.sql = template.text;
        }
        self.query_editors.pending_selection.insert(tab, template.select);
    }
}
