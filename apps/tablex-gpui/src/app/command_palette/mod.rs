//! Spotlight-style command palette (Cmd+K / Cmd+P).
//!
//! UI is the stock `gpui-component` pieces: a `Dialog` (overlay, Escape and
//! click-outside dismissal, focus trap) hosting a searchable `List`
//! (`ListState` + `ListDelegate`: virtualized uniform rows, sections, arrow /
//! Enter navigation). `gpui_component::command::Command` was evaluated and
//! rejected: it filters with a per-item `to_lowercase()` substring test (no
//! ranking, no match highlighting) and re-measures every row whenever a custom
//! row child is used. Only the matcher/index below is custom, because nothing
//! in the pinned components ranks 50k items (see `matcher.rs`).
//!
//! The index is a snapshot of data the app already holds in memory (tabs,
//! saved connections, the navigator's *loaded* catalog, cached saved
//! scripts / pipelines). Building it never loads anything, never touches a
//! driver session, and the large catalog segment is rebuilt only when the
//! navigator's catalog revision (or the connection list) changed.

mod commands;
mod delegate;
mod index;
mod matcher;

pub(super) use commands::StaticAction;
use super::tab_bar::TabIcon;
use super::*;
use crate::connections::ConnectionKind;
use gpui_component::list::{List, ListState};
use index::{actions_segment, Command, Index, Kind, Segment, SegmentBuilder};
use std::collections::VecDeque;

use delegate::{Activation, PaletteDelegate, Recent};
use gpui_component::list::ListDelegate as _;

/// Segment positions inside [`Index::segments`] (idle view addresses them
/// directly instead of scanning).
const SEG_ACTIONS: usize = 0;
const SEG_TABS: usize = 1;
const SEG_CONNECTIONS: usize = 2;
// Followed by the catalog (3) and saved scripts / pipelines (4) segments,
// which are only ever ranked, never listed by position.

const RECENT_LIMIT: usize = 10;
const DIALOG_WIDTH: f32 = 640.0;

/// The catalog segment and the sources it was built from.
struct CatalogCache {
    revision: u64,
    connections: u64,
    segment: Arc<Segment>,
}

#[derive(Default)]
pub(super) struct PaletteState {
    list: Option<gpui::Entity<ListState<PaletteDelegate>>>,
    /// Newest first, in memory only.
    recent: VecDeque<Recent>,
    catalog: Option<CatalogCache>,
}

/// Registers the app-level `OpenCommandPalette` handler. Like Cmd+F it has to
/// work with focus anywhere (including inside the palette's own dialog, which
/// is not an `AppRoot` descendant), so it finds the window's `AppRoot`.
pub(super) fn register_open_action(cx: &mut gpui::App) {
    cx.on_action(|_: &OpenCommandPalette, cx: &mut gpui::App| {
        let Some(window) = cx.active_window() else {
            return;
        };
        // The window is leased while its action is dispatched.
        cx.defer(move |cx| {
            let _ = window.update(cx, |root, window, cx| {
                let app = root
                    .downcast::<gpui_component::Root>()
                    .ok()
                    .and_then(|root| root.read(cx).view().clone().downcast::<AppRoot>().ok());
                if let Some(app) = app {
                    app.update(cx, |app, cx| app.toggle_command_palette(window, cx));
                }
            });
        });
    });
}

impl AppRoot {
    /// Cmd+K / Cmd+P: opens the palette, or closes it when it already has
    /// focus. Never stacks on top of another dialog.
    pub(super) fn toggle_command_palette(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        // A dialog that was just dismissed leaves its input focused until the
        // close animation ends, so require the dialog itself to still exist.
        let open = window.has_active_dialog(cx)
            && self.palette.list.as_ref().is_some_and(|list| {
                list.read(cx).focus_handle(cx).contains_focused(window, cx)
            });
        if open {
            window.close_dialog(cx);
        } else if !window.has_active_dialog(cx) {
            self.open_command_palette(window, cx);
        }
    }

    fn open_command_palette(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let index = self.palette_index();
        let recent: Vec<Recent> = self.palette.recent.iter().cloned().collect();
        let list = match self.palette.list.clone() {
            Some(list) => list,
            None => {
                let root = cx.entity().downgrade();
                let list = cx.new(|cx| {
                    ListState::new(PaletteDelegate::new(root), window, cx).searchable(true)
                });
                self.palette.list = Some(list.clone());
                list
            }
        };
        list.update(cx, |state, cx| {
            state.delegate_mut().load(index, recent);
            state.set_query("", window, cx);
            state.set_selected_index(Some(gpui_component::IndexPath::default()), window, cx);
        });
        let dialog_list = list.clone();
        window.open_dialog(cx, move |dialog, _, _| {
            let list = dialog_list.clone();
            dialog
                .p_0()
                .w(px(DIALOG_WIDTH))
                .close_button(false)
                .content(move |content, _, cx| {
                    // Fit the rows (fixed heights, no measuring) up to the cap.
                    let height = list.read(cx).delegate().panel_height();
                    let action_list = list.clone();
                    content.child(
                        div()
                            .debug_selector(|| "palette-list".into())
                            .w_full()
                            .h(px(height))
                            .on_action(move |_: &PaletteNewQuery, window, cx| {
                                action_list.update(cx, |state, cx| {
                                    let selected = state.selected_index();
                                    let delegate = state.delegate_mut();
                                    delegate.set_selected_index(selected, window, cx);
                                    delegate.activate(Activation::NewQuery, window, cx);
                                });
                            })
                            .child(
                            List::new(&list)
                                .search_placeholder("Search tabs, tables, connections, actions…"),
                        ),
                    )
                })
        });
        list.update(cx, |state, cx| state.focus(window, cx));
    }

    /// Records a chosen item as recent and performs it.
    pub(in crate::app::command_palette) fn activate_palette_item(
        &mut self,
        item: Recent,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        // Open tabs already have their own section; a closed tab is stale.
        if item.kind != Kind::Tab {
            self.palette.recent.retain(|known| known.command != item.command);
            self.palette.recent.push_front(item.clone());
            self.palette.recent.truncate(RECENT_LIMIT);
        }
        self.run_palette_command(item.command, window, cx);
    }

    fn run_palette_command(&mut self, command: Command, window: &mut Window, cx: &mut Context<Self>) {
        match command {
            Command::ActivateTab(id) => self.activate_tab_ui(id, cx),
            Command::OpenConnection(connection) => self.open_connection_target(&connection, cx),
            Command::Node(node) => self.open_palette_node(&node, cx),
            Command::OpenSavedSql { connection, path } => {
                let entry = self
                    .saved_sql_ui
                    .entries(&connection)
                    .and_then(|entries| entries.iter().find(|entry| entry.path == path))
                    .cloned();
                if let Some(entry) = entry {
                    if let Err(error) = self.open_saved_sql_entry(&entry, &connection, cx) {
                        self.sidebar.notice = Some(format!("Could not open saved SQL: {error}"));
                        cx.notify();
                    }
                }
            }
            Command::OpenSavedPipeline { connection, id } => {
                self.open_saved_aggregate(&connection, &id, cx)
            }
            Command::Run(action) => action.run(self, window, cx),
            Command::Diagram(node) => self.open_diagram_for_node(&node, cx),
            Command::NewQuery(node) => {
                self.open_new_query_for_node(&node, cx);
            }
        }
    }

    /// table / view -> data tab, Mongo collection -> collection tab,
    /// database / schema -> revealed in the sidebar (Shift+Enter: scoped query).
    fn open_palette_node(&mut self, node: &NavigatorNodeId, cx: &mut Context<Self>) {
        match node {
            NavigatorNodeId::Object {
                connection,
                database,
                schema,
                object,
            } => self.open_sql_object_tab(object, connection, database, schema, object, cx),
            NavigatorNodeId::Database { .. } | NavigatorNodeId::Schema { .. } => {
                self.reveal_node_in_sidebar(node, cx);
            }
            NavigatorNodeId::Provider { connection, .. } => {
                let Some(provider) = self.navigator.provider_node(node) else {
                    return;
                };
                let text = |key: &str| {
                    provider
                        .payload
                        .get(key)
                        .and_then(serde_json::Value::as_str)
                        .map(str::to_string)
                };
                match provider.kind.as_str() {
                    "collection" => {
                        if let (Some(database), Some(collection)) =
                            (text("database"), text("collection"))
                        {
                            self.open_collection_target(connection, &database, &collection, cx);
                        }
                    }
                    "database" => self.reveal_node_in_sidebar(node, cx),
                    _ => {}
                }
            }
            _ => {}
        }
    }

    // ---- index ---------------------------------------------------------------

    /// Snapshot of everything searchable. Tabs, connections and scripts are
    /// small and rebuilt per open; the catalog is reused until the navigator's
    /// catalog revision or the connection list changes.
    fn palette_index(&mut self) -> Arc<Index> {
        let revision = self.navigator.catalog_revision();
        let connections = self.connections_signature();
        let catalog = match &self.palette.catalog {
            Some(cache) if cache.revision == revision && cache.connections == connections => {
                cache.segment.clone()
            }
            _ => {
                let segment = Arc::new(self.palette_catalog_segment());
                self.palette.catalog = Some(CatalogCache {
                    revision,
                    connections,
                    segment: segment.clone(),
                });
                segment
            }
        };
        // Order must match the `SEG_*` positions.
        Arc::new(Index {
            segments: vec![
                Arc::new(actions_segment(
                    &self.settings.shortcuts,
                    self.assistant_editor_asks(),
                    self.explain_offered(),
                    self.workspace.active_tab().is_some_and(super::assistant::is_sql_editor),
                )),
                Arc::new(self.palette_tabs_segment()),
                Arc::new(self.palette_connections_segment()),
                catalog,
                Arc::new(self.palette_scripts_segment()),
            ],
        })
    }

    /// The titles of the palette's action entries as they would be listed now (tests).
    #[cfg(test)]
    pub(in crate::app) fn palette_action_titles(&mut self) -> Vec<String> {
        self.palette_index().segments[SEG_ACTIONS]
            .entries
            .iter()
            .map(|entry| entry.title.to_string())
            .collect()
    }

    fn connections_signature(&self) -> u64 {
        use std::hash::{Hash, Hasher};
        let mut hasher = std::collections::hash_map::DefaultHasher::new();
        for saved in &self.saved_connections {
            saved.id.hash(&mut hasher);
            saved.name.hash(&mut hasher);
            (saved.kind as u8).hash(&mut hasher);
        }
        hasher.finish()
    }

    fn palette_connection_name(&self, id: &str) -> &str {
        self.saved_connections
            .iter()
            .find(|saved| saved.matches_id(id))
            .map_or("", |saved| saved.name.as_str())
    }

    fn palette_tabs_segment(&self) -> Segment {
        let mut builder = SegmentBuilder::new();
        for tab in self.workspace.tabs() {
            let name = self.palette_connection_name(&tab.connection_id);
            let detail = join_detail(name, &tab.database);
            let scope = builder.scope(&detail, "", &tab.connection_id, &tab.database, "");
            builder.push(
                Kind::Tab,
                self.tab_icon(tab),
                scope,
                &tab.title,
                Some(Command::ActivateTab(tab.id)),
            );
        }
        builder.finish()
    }

    fn palette_connections_segment(&self) -> Segment {
        let mut builder = SegmentBuilder::new();
        for saved in self.saved_connections.iter().filter(|saved| !saved.disabled) {
            let id = saved.id.to_string();
            let scope = builder.scope(kind_label(saved.kind), "", &id, "", "");
            let icon = super::navigator_ui::connection_brand_asset(saved.kind)
                .map_or(TabIcon::Plain("icons/building-2.svg"), TabIcon::Brand);
            builder.push(
                Kind::Connection,
                icon,
                scope,
                &saved.name,
                Some(Command::OpenConnection(id)),
            );
        }
        builder.finish()
    }

    /// Databases, schemas, tables, views and Mongo collections that are
    /// already loaded in the navigator.
    fn palette_catalog_segment(&self) -> Segment {
        let names: HashMap<String, (&str, ConnectionKind)> = self
            .saved_connections
            .iter()
            .map(|saved| (saved.id.to_string(), (saved.name.as_str(), saved.kind)))
            .collect();
        let mut builder = SegmentBuilder::new();
        let mut object_scope: Option<(&str, &str, &str, u32)> = None;
        self.navigator.visit_catalog(|id, label, object_kind, provider| {
            let Some(connection) = node_connection_id(id) else {
                return;
            };
            let Some(&(name, kind)) = names.get(connection) else {
                return;
            };
            match id {
                NavigatorNodeId::Database { database, .. } => {
                    let scope = builder.scope(name, "", connection, "", "");
                    builder.push(Kind::Database, icon_of(Kind::Database), scope, database, None);
                }
                NavigatorNodeId::Schema {
                    database, schema, ..
                } => {
                    let detail = join_detail(name, database);
                    let scope = builder.scope(&detail, "", connection, database, "");
                    builder.push(Kind::Schema, icon_of(Kind::Schema), scope, schema, None);
                }
                NavigatorNodeId::Object {
                    database, schema, ..
                } => {
                    // Objects arrive grouped by schema: build the detail text
                    // once per schema, not once per object.
                    let scope = match object_scope {
                        Some((c, d, s, scope)) if c == connection && d == database && s == schema => {
                            scope
                        }
                        _ => {
                            let place = if schema.is_empty() {
                                database.clone()
                            } else {
                                format!("{database}.{schema}")
                            };
                            let detail = join_detail(name, &place);
                            let scope = builder.scope(&detail, "", connection, database, schema);
                            object_scope = Some((connection, database, schema, scope));
                            scope
                        }
                    };
                    let object = match object_kind {
                        Some(kind) if kind.is_view() => Kind::View,
                        _ => Kind::Table,
                    };
                    builder.push(object, icon_of(object), scope, label, None);
                }
                NavigatorNodeId::Provider { .. } => {
                    if !matches!(kind, ConnectionKind::Mongodb | ConnectionKind::Redis) {
                        return;
                    }
                    let Some(provider) = provider else {
                        return;
                    };
                    // Redis: its databases only (a key list can hold millions).
                    if kind == ConnectionKind::Redis && provider.kind != "database" {
                        return;
                    }
                    let database = provider
                        .payload
                        .get("database")
                        .and_then(serde_json::Value::as_str)
                        .unwrap_or_default();
                    let (entry_kind, detail) = if provider.kind == "collection" {
                        if database.is_empty() {
                            return;
                        }
                        (Kind::Collection, join_detail(name, database))
                    } else {
                        (Kind::Database, name.to_string())
                    };
                    let scope = builder.scope(&detail, "", connection, database, "");
                    builder.push(
                        entry_kind,
                        icon_of(entry_kind),
                        scope,
                        label,
                        Some(Command::Node(id.clone())),
                    );
                }
                _ => {}
            }
        });
        builder.finish()
    }

    /// Saved SQL / Mongo scripts and saved pipelines that are cached.
    fn palette_scripts_segment(&self) -> Segment {
        let mut builder = SegmentBuilder::new();
        for saved in &self.saved_connections {
            let id = saved.id.to_string();
            for entry in self.saved_sql_ui.entries(&id).unwrap_or_default() {
                let place = match &entry.schema {
                    Some(schema) if !schema.is_empty() => format!("{}.{schema}", entry.database),
                    _ => entry.database.clone(),
                };
                let detail = join_detail(&saved.name, &place);
                let scope = builder.scope(&detail, "", &id, &entry.database, "");
                builder.push(
                    Kind::Script,
                    icon_of(Kind::Script),
                    scope,
                    &entry.title,
                    Some(Command::OpenSavedSql {
                        connection: id.clone(),
                        path: entry.path.clone(),
                    }),
                );
            }
        }
        let mut keys: Vec<_> = self.saved_aggregates.keys().collect();
        keys.sort();
        for key in keys {
            let (connection, database) = key;
            let name = self.palette_connection_name(connection);
            for pipeline in &self.saved_aggregates[key] {
                let place = if pipeline.collection.is_empty() {
                    database.clone()
                } else {
                    format!("{database}.{}", pipeline.collection)
                };
                let detail = join_detail(name, &place);
                let scope = builder.scope(&detail, "", connection, database, "");
                builder.push(
                    Kind::Pipeline,
                    icon_of(Kind::Pipeline),
                    scope,
                    &pipeline.name,
                    Some(Command::OpenSavedPipeline {
                        connection: connection.clone(),
                        id: pipeline.id.clone(),
                    }),
                );
            }
        }
        builder.finish()
    }
}

fn node_connection_id(id: &NavigatorNodeId) -> Option<&str> {
    super::navigator_ui::row_connection_id(id)
}

/// "connection · rest", dropping an empty side.
fn join_detail(connection: &str, rest: &str) -> String {
    match (connection.is_empty(), rest.is_empty()) {
        (false, false) => format!("{connection} · {rest}"),
        (false, true) => connection.to_string(),
        _ => rest.to_string(),
    }
}

fn kind_label(kind: ConnectionKind) -> &'static str {
    match kind {
        ConnectionKind::Postgres => "PostgreSQL",
        ConnectionKind::Sqlite => "SQLite",
        ConnectionKind::Mongodb => "MongoDB",
        ConnectionKind::Mysql => "MySQL",
        ConnectionKind::Mariadb => "MariaDB",
        ConnectionKind::Redis => "Redis",
        ConnectionKind::Clickhouse => "ClickHouse",
    }
}

/// Icons match the sidebar tree rows (see `navigator_ui::row_glyph`).
fn icon_of(kind: Kind) -> TabIcon {
    match kind {
        Kind::Database => TabIcon::Colored("icons/database.svg", theme::icon_database()),
        Kind::Schema => TabIcon::Colored("icons/folder-open.svg", theme::icon_schema()),
        Kind::Table => TabIcon::Colored("icons/table.svg", theme::icon_table()),
        Kind::View => TabIcon::Colored("icons/eye.svg", theme::icon_view()),
        Kind::Collection => TabIcon::Colored("icons/table.svg", theme::icon_collection()),
        Kind::Pipeline => TabIcon::Colored("icons/list-ordered.svg", theme::icon_schema()),
        Kind::Script => TabIcon::Plain("icons/square-terminal.svg"),
        Kind::Connection => TabIcon::Plain("icons/building-2.svg"),
        Kind::Tab => TabIcon::Plain("icons/layout-dashboard.svg"),
        Kind::Action => TabIcon::Plain("icons/square-terminal.svg"),
    }
}

#[cfg(test)]
impl AppRoot {
    /// `"Kind: title"` of every indexed entry plus the catalog segment's
    /// identity (pointer), for index-content and caching assertions.
    pub(super) fn palette_index_for_test(&mut self) -> (Vec<String>, usize) {
        let index = self.palette_index();
        let entries = index
            .segments
            .iter()
            .flat_map(|segment| segment.entries.iter())
            .map(|entry| format!("{}: {}", entry.kind.badge(), entry.title))
            .collect();
        (entries, Arc::as_ptr(&index.segments[3]) as usize)
    }

    /// `(section title, row titles)` currently shown by the open palette.
    pub(super) fn palette_snapshot_for_test(
        &self,
        cx: &gpui::App,
    ) -> Vec<(&'static str, Vec<String>)> {
        self.palette
            .list
            .as_ref()
            .expect("palette list exists after the first open")
            .read(cx)
            .delegate()
            .snapshot()
    }
}
