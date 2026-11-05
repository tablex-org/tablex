//! What AppRoot tells a structure editor: may this table / schema be edited, and how.

use std::rc::Rc;
use std::sync::Arc;

use super::apply::sqlite_is_in_memory;
use crate::app::results::diagram::data::ContextEntry;
use crate::app::AppRoot;
use crate::connections::ConnectionKind;
use crate::sql_ddl::{dialect_for, Change, DdlDialect, TableId};

/// What names a table in a change: the connection kind (dialect), the editor's
/// database and schema.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(in crate::app) struct EditScope {
    pub(in crate::app) kind: ConnectionKind,
    pub(in crate::app) database: String,
    /// The schema new tables go to; `None` for engines without one.
    pub(in crate::app) schema: Option<String>,
}

impl EditScope {
    pub(in crate::app) fn dialect(&self) -> &'static dyn DdlDialect {
        // Only SQL kinds get a scope.
        dialect_for(self.kind).expect("a SQL connection kind")
    }

    /// The table as a change names it: dialects that qualify with the
    /// database (MySQL, MariaDB, ClickHouse) get it as the schema.
    pub(in crate::app) fn table_id(&self, database: &str, schema: Option<&str>, table: &str) -> TableId {
        let schema = schema.filter(|schema| !schema.is_empty()).map(str::to_owned);
        let schema = if self.dialect().schema_is_database() {
            schema.or_else(|| Some(database.to_owned()).filter(|database| !database.is_empty()))
        } else {
            schema
        };
        TableId { schema, name: table.to_owned() }
    }
}

/// What AppRoot tells the editor about structure editing (handed over on every
/// render; the editor never reads settings itself).
#[derive(Debug, Clone)]
pub(in crate::app) struct EditContext {
    pub(in crate::app) scope: EditScope,
    /// The connection as the confirmations name it ("Production · shop").
    pub(in crate::app) subject: String,
    /// The connection asks for an explicit confirmation before it executes
    /// anything (confirm-execute, or a Production type).
    pub(in crate::app) confirm_execute: bool,
    /// Why editing is off (read-only connection, no driver); `None`: editable.
    pub(in crate::app) disabled: Option<String>,
    /// `sql_mode` of the server when an apply has read it (MySQL / MariaDB): the SQL is
    /// generated and checked for that mode. Unknown until then (the default mode is assumed;
    /// an apply regenerates the SQL for the real one and refuses if it differs).
    pub(in crate::app) sql_mode: Option<String>,
    /// The table names of the whole namespace (not only the ones the editor shows), when known.
    pub(in crate::app) catalog: Option<Arc<Vec<String>>>,
    /// Why the table names could not be loaded (`catalog` is `None` and the navigator's load failed).
    pub(in crate::app) catalog_error: Option<String>,
}

impl PartialEq for EditContext {
    fn eq(&self, other: &Self) -> bool {
        self.scope == other.scope
            && self.subject == other.subject
            && self.confirm_execute == other.confirm_execute
            && self.disabled == other.disabled
            && self.sql_mode == other.sql_mode
            && self.catalog_error == other.catalog_error
            // By identity first: the list can be long and AppRoot reuses the same one while it is unchanged.
            && match (&self.catalog, &other.catalog) {
                (Some(a), Some(b)) => Arc::ptr_eq(a, b) || a == b,
                (None, None) => true,
                _ => false,
            }
    }
}

impl EditContext {
    /// The dialect of the connection, for the server's `sql_mode` when it is known.
    pub(in crate::app) fn dialect(&self) -> Box<dyn crate::sql_ddl::DdlDialect> {
        let mode = self.sql_mode.as_deref().unwrap_or("");
        crate::sql_ddl::dialect_in_mode(self.scope.kind, mode).expect("a SQL connection kind")
    }
}

/// What Apply hands to AppRoot: the change-set and the exact SQL the user reviewed.
#[derive(Debug, Clone)]
pub(in crate::app) struct ApplyRequest {
    pub(in crate::app) changes: Vec<Change>,
    pub(in crate::app) script: String,
    /// The user ticked the confirmation naming the connection (shown when the connection asks for
    /// one). Apply checks it against what the connection asks *now*: a connection whose type became
    /// Production while the dialog was open is not applied on a review that never asked.
    pub(in crate::app) confirmed: bool,
}

impl AppRoot {
    /// What the editor of `connection` / `database` / `schema` may do to the connection's structure.
    pub(in crate::app) fn structure_edit_context(
        &self,
        connection: &str,
        database: &str,
        schema: Option<&str>,
    ) -> Option<EditContext> {
        let saved = self.saved_connections.iter().find(|saved| saved.matches_id(connection))?;
        // Capture-only (`TABLEX_SEED_DIAGRAM=dialect=postgres`): the review of another dialect
        // on the fixture connection (nothing can be applied there).
        let seeded = self
            .scenario_ui
            .service
            .is_some()
            .then(|| std::env::var("TABLEX_SEED_DIAGRAM").ok())
            .flatten()
            .and_then(|spec| spec.split(',').find_map(|token| token.strip_prefix("dialect=").map(str::to_owned)))
            .and_then(|name| match name.as_str() {
                "postgres" => Some(ConnectionKind::Postgres),
                "mysql" => Some(ConnectionKind::Mysql),
                "mariadb" => Some(ConnectionKind::Mariadb),
                "sqlite" => Some(ConnectionKind::Sqlite),
                "clickhouse" => Some(ConnectionKind::Clickhouse),
                _ => None,
            });
        let kind = seeded.unwrap_or(saved.kind);
        dialect_for(kind)?;
        let effective = self.effective_for(connection);
        let subject = self.connection_subject(connection, &effective);
        let confirm_execute = effective.confirm_execute || effective.connection_type.as_ref().is_some_and(|ty| ty.is_production());
        // Capture-only (`TABLEX_SEED_DIAGRAM=readonly`): the fixture connection looks read-only.
        let seeded_read_only = self.scenario_ui.service.is_some()
            && std::env::var("TABLEX_SEED_DIAGRAM").is_ok_and(|spec| spec.split(',').any(|token| token == "readonly"));
        let disabled = self
            .read_only_connection_message(connection, "Structure editing is")
            .or_else(|| {
                (saved.kind == ConnectionKind::Sqlite && sqlite_is_in_memory(&saved.uri)).then(|| {
                    "Structure editing is off for an in-memory SQLite database: the change would run on a second connection that sees a different, empty database."
                        .to_owned()
                })
            })
            .or_else(|| {
                seeded_read_only.then(|| {
                    "Structure editing is blocked: this connection is read-only (Production). Change it in Settings \u{203A} Query & Connections \u{203A} Connection types."
                        .to_owned()
                })
            });
        Some(EditContext {
            scope: EditScope { kind, database: database.to_owned(), schema: schema.map(str::to_owned) },
            subject,
            confirm_execute,
            disabled,
            sql_mode: self.diagram.sql_modes.get(connection).cloned(),
            catalog: self.structure_catalog_names(connection, database, schema),
            catalog_error: self.structure_catalog_error(connection, database, schema),
        })
    }

    /// [`Self::structure_edit_context`], computed once per frame and namespace: the Properties toolbar and
    /// the edit mode's sync both ask on every frame, for every tab, and the answer reads settings and
    /// formats strings. A repeated ask is a pointer copy.
    pub(in crate::app) fn structure_edit_context_this_frame(&self, connection: &str, database: &str, schema: Option<&str>) -> Option<Rc<EditContext>> {
        let frame = crate::app::frame_count();
        let found = |entry: &ContextEntry| entry.connection == connection && entry.database == database && entry.schema.as_deref() == schema;
        {
            let mut memo = self.diagram.context_memo.borrow_mut();
            if memo.frame != frame {
                memo.frame = frame;
                memo.entries.clear();
            } else if let Some(entry) = memo.entries.iter().find(|entry| found(entry)) {
                return entry.context.clone();
            }
        }
        let context = self.structure_edit_context(connection, database, schema).map(Rc::new);
        self.diagram.context_memo.borrow_mut().entries.push(ContextEntry {
            connection: connection.to_owned(),
            database: database.to_owned(),
            schema: schema.map(str::to_owned),
            context: context.clone(),
        });
        context
    }

    /// The navigator's Tables group of a namespace, whose children are its table names.
    fn structure_catalog_group(connection: &str, database: &str, schema: Option<&str>) -> crate::navigator::NavigatorNodeId {
        crate::navigator::object_group_id(connection, database, schema.unwrap_or(""), crate::navigator::ObjectGroup::Tables)
    }

    fn structure_catalog_error(&self, connection: &str, database: &str, schema: Option<&str>) -> Option<String> {
        match self.navigator.load_state(&Self::structure_catalog_group(connection, database, schema)) {
            Some(crate::navigator::NodeLoadState::Failed(error)) => Some(error.clone()),
            _ => None,
        }
    }

    /// Whether nothing has asked the navigator for the table names of a namespace yet (it is neither
    /// loading, loaded nor failed): the only state in which asking again does anything.
    pub(in crate::app) fn structure_catalog_unrequested(&self, connection: &str, database: &str, schema: Option<&str>) -> bool {
        self.navigator.load_state(&Self::structure_catalog_group(connection, database, schema)).is_none()
    }

    /// Asks the navigator to load the table names of a namespace when it has not (its own loading
    /// path, in the background: the answer reaches `structure_edit_context` as `catalog`). `retry`:
    /// also starts over after a failure. A request that cannot be made is a failure the lists show
    /// (with Retry), not a load that never ends.
    pub(in crate::app) fn load_structure_catalog(&mut self, connection: &str, database: &str, schema: Option<&str>, retry: bool, cx: &mut gpui::Context<Self>) {
        #[cfg(test)]
        crate::perf_counters::bump(&crate::perf_counters::LOAD_STRUCTURE_CATALOG);
        let group = Self::structure_catalog_group(connection, database, schema);
        if retry {
            self.navigator.reset_node(&group);
        }
        match self.navigator.begin_background_load(&group) {
            Some(request) => self.ensure_connected_then_load(request, cx),
            // Defensive: an object group always has a request, so this is not reached today. If a
            // group ever has none, the lists would say "Loading..." for ever without it.
            None if self.structure_catalog_unrequested(connection, database, schema) => {
                self.navigator.fail_load(&group, "The table names of this schema cannot be loaded.".to_owned());
                cx.notify();
            }
            None => {}
        }
    }

    /// The table names of the namespace the navigator has loaded (so a new or renamed
    /// table cannot take the name of one the diagram does not show). Rebuilt only
    /// when the loaded list changed.
    pub(in crate::app) fn structure_catalog_names(&self, connection: &str, database: &str, schema: Option<&str>) -> Option<Arc<Vec<String>>> {
        use crate::navigator::{object_group_id, NavigatorNodeId, ObjectGroup};
        let group = object_group_id(connection, database, schema.unwrap_or(""), ObjectGroup::Tables);
        let Some(children) = self.navigator.loaded_children(&group) else {
            // Reset or failed: what was kept for the group is gone with its names.
            self.diagram.catalog_memo.borrow_mut().remove(&group);
            return None;
        };
        fn names(child: &NavigatorNodeId) -> Option<&str> {
            match child {
                NavigatorNodeId::Object { object, .. } => Some(object.as_str()),
                _ => None,
            }
        }
        // A sequence the navigator has loaded (MariaDB's are rows of its table catalog) shares the
        // names of the tables.
        let sequences = self
            .navigator
            .loaded_children(&object_group_id(connection, database, schema.unwrap_or(""), ObjectGroup::Sequences))
            .unwrap_or_default();
        let all = || children.iter().chain(sequences);
        let key = (
            children.len() + sequences.len(),
            all().next().and_then(names).map(str::to_owned),
            all().last().and_then(names).map(str::to_owned),
        );
        let mut memo = self.diagram.catalog_memo.borrow_mut();
        if let Some((memo_key, list)) = memo.get(&group) {
            if *memo_key == key {
                return Some(Arc::clone(list));
            }
        }
        let list: Arc<Vec<String>> = Arc::new(all().filter_map(names).map(str::to_owned).collect());
        memo.insert(group, (key, Arc::clone(&list)));
        // Lists of groups the navigator no longer holds (a connection closed, a tree reset) go.
        if memo.len() > 8 {
            memo.retain(|group, _| self.navigator.loaded_children(group).is_some());
        }
        Some(list)
    }
}
