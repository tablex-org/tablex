//! GPUI adapter of the editor autocomplete core (`crate::completion`).
//!
//! A [`TabCompletion`] is installed as the `CompletionProvider` of one editor.
//! When the menu asks for items it snapshots a bounded window of the text,
//! then reads the tab, the navigator cache and the result columns of
//! [`AppRoot`] in a foreground task (after the keystroke's own update has
//! ended, so the root is never read while it is being updated). Only caches
//! that are already loaded are consulted: no I/O, no session lock.

use std::cell::{OnceCell, RefCell};
use std::rc::Rc;

use gpui::{App, Context, Task, WeakEntity, Window};
use gpui_component::input::{CompletionProvider, EditorState, Rope, RopeExt};
use lsp_types::{
    CompletionContext, CompletionItem, CompletionItemKind, CompletionResponse, CompletionTextEdit,
    InsertTextFormat, Range as LspRange, TextEdit,
};

use super::AppRoot;
use crate::completion::{
    self, Catalog, Column, Completions, Editor, Kind, MongoEditor, MongoRole, Relation, SqlEditor,
    SqlFlavor,
};
use crate::connections::ConnectionKind;
use crate::navigator::{NavigatorNodeId, NavigatorState, ObjectGroup};
use crate::workspace::{QueryModeExt, Tab};

/// Text kept around the cursor: statements and shell scripts that matter for
/// completion are far smaller; a huge script is cut at a line boundary.
const WINDOW_BEFORE: usize = 48 * 1024;
const WINDOW_AFTER: usize = 8 * 1024;

/// Which editor of a tab a provider serves.
#[derive(Clone, Copy, Debug)]
pub(super) enum Role {
    /// The query editor: SQL, or a Mongo shell script on Mongo tabs.
    Query,
    /// The `WHERE` / `ORDER BY` fragment editor of a SQL table-data tab.
    TableFragment,
    /// Expanded Mongo filter / sort (strict JSON) editors.
    Filter,
    Sort,
    /// The aggregation pipeline text editor.
    Pipeline,
    /// The body editor of one pipeline stage.
    Stage(u64),
}

pub(super) struct TabCompletion {
    root: WeakEntity<AppRoot>,
    tab: u64,
    role: Role,
}

impl TabCompletion {
    /// Provider for the `role` editor of `tab`, created from the root's context.
    pub(super) fn for_tab(cx: &gpui::Context<AppRoot>, tab: u64, role: Role) -> Self {
        Self {
            root: cx.entity().downgrade(),
            tab,
            role,
        }
    }
}

/// The "suggestions while typing" setting, published for the providers
/// (`AppRoot::apply_settings` sets it).
pub(super) struct CompletionPrefs {
    pub(super) auto_trigger: bool,
}

impl gpui::Global for CompletionPrefs {}

/// Builder hook: `EditorState::new(..).language(..).completion(provider)`.
pub(super) trait EditorCompletion: Sized {
    fn completion(self, provider: TabCompletion) -> Self;
}

impl EditorCompletion for EditorState {
    fn completion(mut self, provider: TabCompletion) -> Self {
        self.lsp_mut().completion_provider = Some(Rc::new(provider));
        self
    }
}

impl CompletionProvider for TabCompletion {
    fn completions(
        &self,
        text: &Rope,
        offset: usize,
        _: CompletionContext,
        _: &mut Window,
        cx: &mut App,
    ) -> Task<gpui::Result<CompletionResponse>> {
        let (base, window_text) = window_around(text, offset);
        let rope = text.clone();
        let (root, tab, role) = (self.root.clone(), self.tab, self.role);
        cx.spawn(async move |cx| {
            let (items, warm) = root
                .read_with(cx, |root, _| {
                    suggest(root, tab, role, &window_text, offset - base, base, &rope)
                })
                .ok()
                .flatten()
                .unwrap_or_default();
            if !warm.nodes.is_empty() {
                root.update(cx, |root, cx| root.warm_columns(warm, cx)).ok();
            }
            Ok(CompletionResponse::Array(items))
        })
    }

    /// Any single typed character (or a deletion) re-queries; the core decides
    /// whether there is anything to show, which also closes a stale menu.
    fn is_completion_trigger(&self, _: usize, new_text: &str, cx: &mut App) -> bool {
        // A global, not a read of the root: the editor may type into itself
        // from inside a root update.
        cx.try_global::<CompletionPrefs>()
            .is_none_or(|prefs| prefs.auto_trigger)
            && new_text.chars().nth(1).is_none()
    }
}

/// `(window start, window text)` around `offset`, cut at char boundaries and
/// starting on a line boundary when the document is cut.
fn window_around(text: &Rope, offset: usize) -> (usize, String) {
    let mut start = offset.saturating_sub(WINDOW_BEFORE);
    while !text.is_char_boundary(start) {
        start += 1;
    }
    let mut end = (offset + WINDOW_AFTER).min(text.len());
    while !text.is_char_boundary(end) {
        end -= 1;
    }
    let mut window = text.slice(start..end).to_string();
    if start > 0 {
        if let Some(newline) = window
            .find('\n')
            .filter(|&newline| start + newline < offset)
        {
            window.drain(..=newline);
            start += newline + 1;
        }
    }
    (start, window)
}

fn flavor_of(kind: Option<ConnectionKind>) -> SqlFlavor {
    match kind {
        Some(ConnectionKind::Sqlite) => SqlFlavor::Sqlite,
        Some(ConnectionKind::Mysql) => SqlFlavor::Mysql,
        Some(ConnectionKind::Mariadb) => SqlFlavor::Mariadb,
        Some(ConnectionKind::Clickhouse) => SqlFlavor::Clickhouse,
        _ => SqlFlavor::Postgres,
    }
}

/// Tables in scope whose columns are not cached yet.
#[derive(Default)]
struct Warm {
    connection: String,
    nodes: Vec<NavigatorNodeId>,
}

/// Most column loads one completion may start.
const MAX_WARM: usize = 4;

impl AppRoot {
    /// Loads the columns of tables the user is querying through the regular
    /// navigator load path (no sidebar expansion), so the next keystroke can
    /// complete them. Only for connections that are already connected.
    fn warm_columns(&mut self, warm: Warm, cx: &mut Context<Self>) {
        if !self.sessions.is_connected(&warm.connection) {
            return;
        }
        for node in warm.nodes.iter().take(MAX_WARM) {
            if let Some(request) = self.navigator.begin_background_load(node) {
                self.ensure_connected_then_load(request, cx);
            }
        }
    }
}

fn suggest(
    root: &AppRoot,
    tab_id: u64,
    role: Role,
    text: &str,
    cursor: usize,
    base: usize,
    rope: &Rope,
) -> Option<(Vec<CompletionItem>, Warm)> {
    let tab = root.workspace.tab(tab_id)?;
    let runtime = root.workspace.runtime(tab_id);
    let kind = root
        .saved_connections
        .iter()
        .find(|saved| saved.matches_id(&tab.connection_id))
        .map(|saved| saved.kind);
    if tab.query_mode.is_redis_command() && matches!(role, Role::Query) {
        // Command names, then the keys already loaded in the sidebar.
        let keys = root.navigator.redis_loaded_keys(&tab.connection_id, &tab.database);
        let (range, candidates) = crate::redis::complete(
            text,
            cursor,
            keys,
            root.settings.editor.autocomplete_max_items as usize,
        );
        let range = LspRange {
            start: rope.offset_to_position(base + range.start),
            end: rope.offset_to_position(base + range.end),
        };
        let items = candidates
            .into_iter()
            .map(|candidate| CompletionItem {
                filter_text: Some(candidate.label.clone()),
                detail: Some(candidate.detail).filter(|detail| !detail.is_empty()),
                kind: Some(match candidate.kind {
                    crate::redis::CandidateKind::Command => CompletionItemKind::FUNCTION,
                    crate::redis::CandidateKind::Key => CompletionItemKind::FIELD,
                }),
                text_edit: Some(CompletionTextEdit::Edit(TextEdit {
                    range,
                    new_text: candidate.label.clone(),
                })),
                insert_text_format: Some(InsertTextFormat::PLAIN_TEXT),
                label: candidate.label,
                ..Default::default()
            })
            .collect();
        return Some((items, Warm::default()));
    }
    let collection = Some(tab.collection.as_str()).filter(|name| !name.is_empty());
    let sql = |fragment_of| {
        let flavor = flavor_of(kind);
        let identifier_quote = root.sql_dialect_for_connection(&tab.connection_id).map_or(
            if matches!(
                flavor,
                SqlFlavor::Mysql | SqlFlavor::Mariadb | SqlFlavor::Clickhouse
            ) {
                '`'
            } else {
                '"'
            },
            |dialect| dialect.identifier_quote,
        );
        Editor::Sql(SqlEditor {
            flavor,
            identifier_quote,
            fragment_of,
        })
    };
    let mongo = |role, json| {
        Editor::Mongo(MongoEditor {
            role,
            collection,
            json,
        })
    };
    let editor = match role {
        Role::Query
            if tab.query_mode.is_mongo_script() || kind == Some(ConnectionKind::Mongodb) =>
        {
            mongo(MongoRole::Script, false)
        }
        Role::Query => sql(None),
        Role::TableFragment => sql(Some(Relation {
            namespace: tab.schema.as_deref().filter(|schema| !schema.is_empty()),
            name: tab.collection.as_str(),
            view: false,
        })),
        Role::Filter => mongo(MongoRole::Filter, true),
        Role::Sort => mongo(MongoRole::Sort, true),
        Role::Pipeline => mongo(MongoRole::Pipeline, false),
        Role::Stage(id) => {
            let draft = root.aggregate_draft(tab_id)?;
            let stage = draft.stages.iter().find(|stage| stage.id == id)?;
            mongo(MongoRole::Stage(stage.operator.as_str()), false)
        }
    };
    let catalog = NavigatorCatalog {
        nav: &root.navigator,
        connection: &tab.connection_id,
        database: &tab.database,
        default_namespace: default_namespace(kind, tab),
        structure: matches!(role, Role::TableFragment)
            .then(|| {
                runtime.map(|runtime| {
                    (
                        tab.collection.as_str(),
                        runtime.structure_columns.as_slice(),
                    )
                })
            })
            .flatten(),
        fields: runtime.map_or(&[], |runtime| runtime.mongo.known_columns.as_slice()),
        collections: OnceCell::new(),
        uncached: RefCell::new(Vec::new()),
    };
    let completions = completion::complete(&editor, text, cursor, &catalog);
    let items = to_lsp(completions, base, rope, &root.settings.editor);
    let warm = Warm {
        connection: tab.connection_id.clone(),
        nodes: catalog.uncached.take(),
    };
    Some((items, warm))
}

fn default_namespace(kind: Option<ConnectionKind>, tab: &Tab) -> Option<&str> {
    let schema = tab.schema.as_deref().filter(|schema| !schema.is_empty());
    let database = Some(tab.database.as_str()).filter(|database| !database.is_empty());
    match kind {
        Some(ConnectionKind::Sqlite) => schema.or(Some("main")),
        Some(ConnectionKind::Mysql | ConnectionKind::Mariadb | ConnectionKind::Clickhouse) => {
            schema.or(database)
        }
        Some(ConnectionKind::Mongodb | ConnectionKind::Redis) => None,
        _ => schema.or(Some("public")),
    }
}

fn lsp_kind(kind: Kind) -> CompletionItemKind {
    match kind {
        Kind::Snippet => CompletionItemKind::SNIPPET,
        Kind::Keyword => CompletionItemKind::KEYWORD,
        Kind::Function => CompletionItemKind::FUNCTION,
        Kind::Method => CompletionItemKind::METHOD,
        Kind::Table | Kind::Collection => CompletionItemKind::CLASS,
        Kind::View => CompletionItemKind::INTERFACE,
        Kind::Column | Kind::Field => CompletionItemKind::FIELD,
        Kind::Schema => CompletionItemKind::MODULE,
        Kind::Alias => CompletionItemKind::VARIABLE,
        Kind::Operator => CompletionItemKind::OPERATOR,
        Kind::Value => CompletionItemKind::VALUE,
    }
}

/// The menu replaces `text_edit.range` with `new_text`, highlights the first
/// `filter_text.len()` bytes of the label and shows `detail` beside it.
fn to_lsp(
    completions: Completions<'_>,
    base: usize,
    rope: &Rope,
    settings: &crate::settings::EditorSettings,
) -> Vec<CompletionItem> {
    let range = LspRange {
        start: rope.offset_to_position(base + completions.replace.start),
        end: rope.offset_to_position(base + completions.replace.end),
    };
    completions
        .items
        .into_iter()
        .take(settings.autocomplete_max_items as usize)
        .map(|item| CompletionItem {
            filter_text: Some(item.label[..item.matched].to_owned()),
            detail: Some(item.detail.into_owned()).filter(|detail| !detail.is_empty()),
            kind: Some(lsp_kind(item.kind)),
            text_edit: Some(CompletionTextEdit::Edit(TextEdit {
                range,
                new_text: {
                    let mut text = item.insert.into_owned();
                    if settings.autocomplete_insert_space
                        && matches!(item.kind, Kind::Keyword)
                        && !text.ends_with(' ')
                    {
                        text.push(' ');
                    }
                    text
                },
            })),
            insert_text_format: Some(InsertTextFormat::PLAIN_TEXT),
            label: item.label.into_owned(),
            ..Default::default()
        })
        .collect()
}

/// The navigator tree cache seen as a completion [`Catalog`].
///
/// Standard SQL connections (PostgreSQL, SQLite) store `Database > Schema >
/// ObjectGroup > Object > Column` nodes; driver-backed ones (MySQL, MariaDB)
/// store provider nodes of kind `database` / `table` / `view` / `column`.
/// Both are walked read-only; nodes that were never expanded simply yield
/// nothing.
struct NavigatorCatalog<'n> {
    nav: &'n NavigatorState,
    connection: &'n str,
    /// Tab database; empty = every loaded database.
    database: &'n str,
    default_namespace: Option<&'n str>,
    /// Columns (with types) of the table a table-data tab shows.
    structure: Option<(&'n str, &'n [crate::catalog::CatalogColumn])>,
    /// Observed Mongo fields (current result columns).
    fields: &'n [String],
    collections: OnceCell<Vec<(NavigatorNodeId, String)>>,
    /// Tables whose columns were asked for but are not cached.
    uncached: RefCell<Vec<NavigatorNodeId>>,
}

/// One table / view found in the cache.
struct Entry<'n> {
    namespace: &'n str,
    name: &'n str,
    view: bool,
    /// Node whose children are the columns.
    node: &'n NavigatorNodeId,
}

impl<'n> NavigatorCatalog<'n> {
    fn connection_node(&self) -> NavigatorNodeId {
        NavigatorNodeId::Connection {
            connection: self.connection.to_owned(),
        }
    }

    fn children(&self, id: &NavigatorNodeId) -> &'n [NavigatorNodeId] {
        self.nav.loaded_children(id).unwrap_or_default()
    }

    fn wants_database(&self, name: &str) -> bool {
        self.database.is_empty() || self.database == name
    }

    /// Provider payload string.
    fn payload<'a>(&'a self, id: &NavigatorNodeId, key: &str) -> Option<&'n str> {
        self.nav.provider_node(id)?.payload.get(key)?.as_str()
    }

    fn provider_kind(&self, id: &NavigatorNodeId) -> Option<&'n str> {
        self.nav.provider_node(id).map(|node| node.kind.as_str())
    }

    fn entries(&self, mut visit: impl FnMut(Entry<'n>)) {
        for database in self.children(&self.connection_node()) {
            match database {
                NavigatorNodeId::Database { database: name, .. } if self.wants_database(name) => {
                    for schema in self.children(database) {
                        let NavigatorNodeId::Schema {
                            connection,
                            database,
                            schema,
                            ..
                        } = schema
                        else {
                            continue;
                        };
                        for group in [
                            ObjectGroup::Tables,
                            ObjectGroup::Views,
                            ObjectGroup::MaterializedViews,
                            // MariaDB's sequences are objects that can be selected from.
                            ObjectGroup::Sequences,
                        ] {
                            let group_id = NavigatorNodeId::ObjectGroup {
                                connection: connection.clone(),
                                database: database.clone(),
                                schema: schema.clone(),
                                group,
                            };
                            for object in self.children(&group_id) {
                                if let NavigatorNodeId::Object { object: name, .. } = object {
                                    visit(Entry {
                                        namespace: schema,
                                        name,
                                        view: matches!(group, ObjectGroup::Views | ObjectGroup::MaterializedViews),
                                        node: object,
                                    });
                                }
                            }
                        }
                    }
                }
                NavigatorNodeId::Provider { .. }
                    if self.provider_kind(database) == Some("database") =>
                {
                    let Some(name) = self
                        .payload(database, "database")
                        .filter(|name| self.wants_database(name))
                    else {
                        continue;
                    };
                    for object in self.children(database) {
                        let view = match self.provider_kind(object) {
                            Some("table") => false,
                            Some("view") => true,
                            _ => continue,
                        };
                        if let Some(object_name) = self.payload(object, "object") {
                            visit(Entry {
                                namespace: name,
                                name: object_name,
                                view,
                                node: object,
                            });
                        }
                    }
                }
                _ => {}
            }
        }
    }

    fn is_default(&self, namespace: Option<&str>) -> bool {
        match (namespace, self.default_namespace) {
            (None, _) => true,
            (Some(namespace), Some(default)) => namespace.eq_ignore_ascii_case(default),
            (Some(_), None) => false,
        }
    }

    fn column_of(&self, node: &'n NavigatorNodeId) -> Option<Column<'n>> {
        match node {
            NavigatorNodeId::Column { column, .. } => Some(Column {
                name: column,
                data_type: None,
            }),
            NavigatorNodeId::Provider { .. } if self.provider_kind(node) == Some("column") => {
                let provider = self.nav.provider_node(node)?;
                let name = provider.payload.get("column")?.as_str()?;
                let data_type = provider
                    .payload
                    .get("details")?
                    .get("data_type")
                    .and_then(|ty| ty.as_str());
                Some(Column { name, data_type })
            }
            _ => None,
        }
    }
}

impl Catalog for NavigatorCatalog<'_> {
    fn relations<'a>(&'a self, visit: &mut dyn FnMut(Relation<'a>)) {
        self.entries(|entry| {
            visit(Relation {
                namespace: Some(entry.namespace),
                name: entry.name,
                view: entry.view,
            });
        });
    }

    fn namespaces<'a>(&'a self, visit: &mut dyn FnMut(&'a str)) {
        for database in self.children(&self.connection_node()) {
            match database {
                NavigatorNodeId::Database { database: name, .. } if self.wants_database(name) => {
                    for schema in self.children(database) {
                        if let NavigatorNodeId::Schema { schema, .. } = schema {
                            visit(schema);
                        }
                    }
                }
                NavigatorNodeId::Provider { .. } => {
                    if let Some(name) = self
                        .payload(database, "database")
                        .filter(|_| self.provider_kind(database) == Some("database"))
                    {
                        visit(name);
                    }
                }
                _ => {}
            }
        }
    }

    fn default_namespace(&self) -> Option<&str> {
        self.default_namespace
    }

    fn columns<'a>(
        &'a self,
        namespace: Option<&str>,
        relation: &str,
        visit: &mut dyn FnMut(Column<'a>),
    ) -> bool {
        if let Some((table, columns)) = self.structure {
            if table.eq_ignore_ascii_case(relation) && self.is_default(namespace) {
                for column in columns {
                    visit(Column {
                        name: &column.name,
                        data_type: Some(&column.data_type),
                    });
                }
                return true;
            }
        }
        let mut node = None;
        self.entries(|entry| {
            let same_namespace = match namespace {
                Some(namespace) => entry.namespace.eq_ignore_ascii_case(namespace),
                None => self.is_default(Some(entry.namespace)),
            };
            if node.is_none() && same_namespace && entry.name.eq_ignore_ascii_case(relation) {
                node = Some(entry.node);
            }
        });
        let Some(node) = node else { return false };
        let Some(children) = self.nav.loaded_children(node) else {
            let mut uncached = self.uncached.borrow_mut();
            if !uncached.contains(node) {
                uncached.push(node.clone());
            }
            return false;
        };
        children
            .iter()
            .filter_map(|child| self.column_of(child))
            .for_each(visit);
        true
    }

    fn collections<'a>(&'a self, visit: &mut dyn FnMut(&'a str)) {
        let collections = self.collections.get_or_init(|| {
            self.nav
                .provider_collection_nodes(self.connection, self.database)
        });
        collections.iter().for_each(|(_, name)| visit(name));
    }

    fn fields<'a>(&'a self, visit: &mut dyn FnMut(&'a str)) {
        self.fields.iter().for_each(|field| visit(field));
    }
}

#[cfg(test)]
mod tests {
    use std::time::Instant;

    use engine_driver_api::{NavigatorNode, NavigatorPage};
    use serde_json::json;

    use super::*;
    use crate::connections::SavedConnection;
    use crate::navigator::{self, LoadedChild};

    const CONN: &str = "11111111-1111-1111-1111-111111111111";

    fn nav(kind: ConnectionKind) -> NavigatorState {
        NavigatorState::new(vec![SavedConnection {
            id: uuid::Uuid::parse_str(CONN).unwrap(),
            name: "test".into(),
            uri: "postgres://h/app".into(),
            kind,
            save_password: false,
            is_pinned: false,
            disabled: false,
            ssh_tunnel: false,
        }])
    }

    /// Expands `id` and answers its load with `children`.
    fn expand(nav: &mut NavigatorState, id: &NavigatorNodeId, children: Vec<LoadedChild>) {
        let request = nav.toggle_expansion(id).request.expect("load request");
        let generation = nav.load_generation(&request).expect("generation");
        nav.apply_load(&request, generation, Ok(children));
    }

    fn postgres_nav(tables: usize) -> NavigatorState {
        let mut nav = nav(ConnectionKind::Postgres);
        let connection = NavigatorNodeId::Connection {
            connection: CONN.into(),
        };
        expand(
            &mut nav,
            &connection,
            vec![navigator::database_child(CONN, "app")],
        );
        let database = NavigatorNodeId::Database {
            connection: CONN.into(),
            database: "app".into(),
        };
        expand(
            &mut nav,
            &database,
            vec![navigator::schema_child(CONN, "app", "public")],
        );
        let schema = NavigatorNodeId::Schema {
            connection: CONN.into(),
            database: "app".into(),
            schema: "public".into(),
        };
        assert!(nav.toggle_expansion(&schema).request.is_none());
        let group = navigator::object_group_id(CONN, "app", "public", ObjectGroup::Tables);
        let objects = (0..tables)
            .map(|i| {
                navigator::object_child(
                    CONN,
                    "app",
                    "public",
                    &format!("table_{i:05}"),
                    navigator::KindOfObject::Table,
                )
            })
            .chain([navigator::object_child(
                CONN,
                "app",
                "public",
                "orders",
                navigator::KindOfObject::Table,
            )])
            .collect();
        expand(&mut nav, &group, objects);
        let orders = NavigatorNodeId::Object {
            connection: CONN.into(),
            database: "app".into(),
            schema: "public".into(),
            object: "orders".into(),
        };
        let columns = ["id", "total", "status"]
            .iter()
            .map(|column| navigator::column_child(CONN, "app", "public", "orders", column))
            .collect();
        expand(&mut nav, &orders, columns);
        nav
    }

    fn catalog<'n>(
        nav: &'n NavigatorState,
        database: &'n str,
        default: Option<&'n str>,
    ) -> NavigatorCatalog<'n> {
        NavigatorCatalog {
            nav,
            connection: CONN,
            database,
            default_namespace: default,
            structure: None,
            fields: &[],
            collections: OnceCell::new(),
            uncached: RefCell::new(Vec::new()),
        }
    }

    fn sql_editor(flavor: SqlFlavor) -> Editor<'static> {
        Editor::Sql(SqlEditor {
            flavor,
            identifier_quote: '"',
            fragment_of: None,
        })
    }

    fn labels(editor: &Editor<'_>, marked: &str, catalog: &NavigatorCatalog<'_>) -> Vec<String> {
        let cursor = marked.find('|').unwrap();
        let text = marked.replacen('|', "", 1);
        let completions = completion::complete(editor, &text, cursor, catalog);
        completions
            .items
            .iter()
            .map(|item| item.label.to_string())
            .collect()
    }

    #[test]
    fn a_mariadb_sequence_completes_like_a_table_and_a_postgres_one_does_not() {
        let tree = |kind, schema: &str, sequence: LoadedChild| {
            let mut nav = nav(kind);
            let connection = NavigatorNodeId::Connection { connection: CONN.into() };
            expand(&mut nav, &connection, vec![navigator::database_child(CONN, "app")]);
            let database = NavigatorNodeId::Database { connection: CONN.into(), database: "app".into() };
            expand(&mut nav, &database, vec![navigator::schema_child(CONN, "app", schema)]);
            let schema = NavigatorNodeId::Schema { connection: CONN.into(), database: "app".into(), schema: schema.into() };
            nav.toggle_expansion(&schema);
            let group = |group| navigator::object_group_id(CONN, "app", schema_name(&schema), group);
            fn schema_name(id: &NavigatorNodeId) -> &str {
                match id {
                    NavigatorNodeId::Schema { schema, .. } => schema,
                    _ => "",
                }
            }
            let sequences = group(ObjectGroup::Sequences);
            expand(&mut nav, &sequences, vec![sequence]);
            nav
        };
        let editor = sql_editor(SqlFlavor::Postgres);
        // MariaDB: the sequence is an object that can be selected from.
        let mariadb = tree(
            ConnectionKind::Mariadb,
            "app",
            navigator::object_child(CONN, "app", "app", "ticket_seq", navigator::KindOfObject::Sequence),
        );
        let found = labels(&editor, "select * from ticket|", &catalog(&mariadb, "app", Some("app")));
        assert_eq!(found, ["ticket_seq"]);
        // PostgreSQL: a sequence row has a source only, so it is no table name.
        let reference = navigator::SchemaObjectRef {
            database: Some("app".into()),
            schema: Some("public".into()),
            kind: navigator::SchemaObjectKind::Sequence,
            name: "ticket_seq".into(),
            arguments: None,
            table: None,
            identity: None,
        };
        let postgres = tree(
            ConnectionKind::Postgres,
            "public",
            navigator::schema_object_child(CONN, "app", "public", ObjectGroup::Sequences, "sequence:ticket_seq", "ticket_seq", reference),
        );
        assert!(labels(&editor, "select * from ticket|", &catalog(&postgres, "app", Some("public"))).is_empty());
    }

    #[test]
    fn standard_tree_yields_tables_and_cached_columns_only() {
        let nav = postgres_nav(3);
        let catalog = catalog(&nav, "app", Some("public"));
        let editor = sql_editor(SqlFlavor::Postgres);
        assert_eq!(labels(&editor, "select * from ord|", &catalog)[0], "orders");
        assert_eq!(
            labels(&editor, "select o.| from orders o", &catalog),
            ["id", "total", "status"]
        );
        // table_00001 was never expanded: no columns, but the table itself is known.
        assert!(labels(&editor, "select t.| from table_00001 t", &catalog).is_empty());
        assert!(labels(&editor, "select * from table_0000|", &catalog).len() >= 3);
        // The database filter hides other databases' tables.
        let other = self::catalog(&nav, "other", Some("public"));
        assert!(labels(&editor, "select * from ord|", &other).is_empty());
    }

    #[test]
    fn driver_provider_tree_is_read_for_mysql() {
        let mut nav = nav(ConnectionKind::Mysql);
        nav.enable_provider(CONN);
        let node = |id: &str, kind: &str, label: &str, payload: serde_json::Value| NavigatorNode {
            id: id.into(),
            kind: kind.into(),
            label: label.into(),
            subtitle: None,
            icon: None,
            has_children: true,
            payload,
            actions: Vec::new(),
        };
        let expand_provider =
            |nav: &mut NavigatorState, id: &NavigatorNodeId, nodes: Vec<NavigatorNode>| {
                let request = nav.toggle_expansion(id).request.expect("provider load");
                let generation = nav.load_generation(&request).unwrap();
                nav.apply_provider_page(
                    &request,
                    generation,
                    Ok(NavigatorPage {
                        nodes,
                        next_cursor: None,
                    }),
                );
            };
        let root = NavigatorNodeId::Connection {
            connection: CONN.into(),
        };
        expand_provider(
            &mut nav,
            &root,
            vec![node(
                "database:shop",
                "database",
                "shop",
                json!({"database": "shop"}),
            )],
        );
        let database = NavigatorNodeId::Provider {
            connection: CONN.into(),
            node: "database:shop".into(),
        };
        let object = json!({"database": "shop", "object": "customers"});
        expand_provider(
            &mut nav,
            &database,
            vec![
                node("table:shop:customers", "table", "customers", object),
                node(
                    "view:shop:vip",
                    "view",
                    "vip",
                    json!({"database": "shop", "object": "vip"}),
                ),
            ],
        );
        let table = NavigatorNodeId::Provider {
            connection: CONN.into(),
            node: "table:shop:customers".into(),
        };
        let column = |name: &str, ty: &str| {
            node(
                &format!("column:shop:customers:{name}"),
                "column",
                name,
                json!({"object": {"database": "shop", "object": "customers"}, "column": name, "details": {"name": name, "data_type": ty}}),
            )
        };
        expand_provider(
            &mut nav,
            &table,
            vec![column("id", "int"), column("email", "varchar(255)")],
        );

        let catalog = catalog(&nav, "shop", Some("shop"));
        let editor = Editor::Sql(SqlEditor {
            flavor: SqlFlavor::Mysql,
            identifier_quote: '`',
            fragment_of: None,
        });
        assert_eq!(
            labels(&editor, "select * from c|", &catalog)[0],
            "customers"
        );
        assert!(labels(&editor, "select * from v|", &catalog).contains(&"vip".to_string()));
        assert_eq!(
            labels(&editor, "select c.| from customers c", &catalog),
            ["id", "email"]
        );
        let typed = completion::complete(&editor, "select c.em from customers c", 11, &catalog);
        assert_eq!(typed.items[0].detail, "varchar(255)");
    }

    /// The whole path of a keystroke over a 10k-table navigator cache.
    #[test]
    fn navigator_cache_with_ten_thousand_tables_completes_fast() {
        let nav = postgres_nav(10_000);
        let catalog = catalog(&nav, "app", Some("public"));
        let editor = sql_editor(SqlFlavor::Postgres);
        for (marked, what) in [
            ("select * from |", "empty prefix"),
            ("select * from t|", "one letter"),
            ("select * from table_09|", "prefix"),
            ("select * from ords|", "orders by subsequence"),
            ("select o.| from orders o", "columns via alias"),
        ] {
            let cursor = marked.find('|').unwrap();
            let text = marked.replacen('|', "", 1);
            let run =
                || std::hint::black_box(completion::complete(&editor, &text, cursor, &catalog));
            (0..20).for_each(|_| drop(run()));
            let runs = 200;
            let start = Instant::now();
            (0..runs).for_each(|_| drop(run()));
            let micros = start.elapsed().as_secs_f64() * 1e6 / f64::from(runs);
            println!("nav-cache completion latency, 10k tables, {what:<22}: {micros:>8.1} us");
            // The 1 ms budget is for optimized builds; unoptimized runs under a
            // loaded test machine only guard against pathological slowdowns.
            let budget = if cfg!(debug_assertions) {
                250_000.0
            } else {
                1_000.0
            };
            assert!(micros < budget, "{what}: {micros} us");
        }
    }
}
