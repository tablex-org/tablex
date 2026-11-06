use super::object_actions::append_object_items;
use super::*;
use crate::connection_import::ImportSource;
use crate::navigator::NavigatorAction;
use crate::sidebar_preferences::ConnectionSort;
use gpui::img;
use gpui::Rgba;
use gpui::Stateful;

/// Lucide glyphs vendored under `assets/icons` for the sidebar.
/// Implemented against the pinned `gpui-component` icon contract
/// (`IconNamed -> Icon::new -> app AssetSource`) with no new dependency:
/// every path below must stay registered in `assets.rs`.
#[derive(Clone, Copy)]
pub(super) enum SidebarGlyph {
    Database,
    Table,
    View,
    Refresh,
    Locate,
    NewGroup,
    Transfer,
    Filter,
    SortManual,
    SortAsc,
    SortDesc,
    Connected,
    Collapse,
    MoveToGroup,
    Trash,
    SelectAll,
    Upload,
    Download,
    CollapseAll,
    Sliders,
    Copy,
    Edit,
    History,
    Info,
    CopyPlus,
    PencilRuler,
    TableProperties,
}

impl gpui_component::IconNamed for SidebarGlyph {
    fn path(self) -> gpui::SharedString {
        match self {
            Self::Database => "icons/database.svg",
            Self::Table => "icons/table.svg",
            Self::View => "icons/eye.svg",
            Self::Refresh => "icons/refresh-cw.svg",
            Self::Locate => "icons/locate-fixed.svg",
            Self::NewGroup => "icons/folder-plus.svg",
            Self::Transfer => "icons/arrow-down-up.svg",
            Self::Filter => "icons/filter.svg",
            Self::SortManual => "icons/list-ordered.svg",
            Self::SortAsc => "icons/arrow-down-a-z.svg",
            Self::SortDesc => "icons/arrow-up-z-a.svg",
            Self::Connected => "icons/plug.svg",
            Self::MoveToGroup => "icons/folder-input.svg",
            Self::Trash => "icons/trash.svg",
            Self::Copy => "icons/copy.svg",
            Self::Edit => "icons/pencil.svg",
            Self::History => "icons/list-ordered.svg",
            Self::Info => "icons/eye.svg",
            Self::CopyPlus => "icons/copy-plus.svg",
            Self::PencilRuler => "icons/pencil-ruler.svg",
            Self::TableProperties => "icons/table-properties.svg",
            Self::SelectAll => "icons/list-checks.svg",
            Self::Collapse => "icons/chevrons-left.svg",
            Self::Upload => "icons/upload.svg",
            Self::Download => "icons/download.svg",
            // Active-connection filter button shapes as SVG: a green dot
            // while any connection is live, an outlined circle otherwise.
            // The dot fill matches theme::success().
            Self::CollapseAll => "icons/chevrons-down-up.svg",
            Self::Sliders => "icons/sliders-horizontal.svg",
            // Vector `.*` glyph (dot + asterisk paths, no <text>): Button
            // has no text-child API, so 10px mono type is unreachable; the
            // glyph renders at the 12px icon box in currentColor instead.
        }
        .into()
    }
}

/// Wraps a sidebar element with a ui-scenarios prepaint bounds probe so
/// the capture runner can click real measured geometry instead of guessed
/// coordinates. Probes emit only when bounds change (tracked in
/// `ScenarioUiState.sidebar_bounds`); without the feature this is a
/// pass-through with zero cost.
#[cfg(feature = "ui-scenarios")]
fn sidebar_bounds_probe(
    element: impl IntoElement,
    target: &'static str,
    key: String,
    label: String,
    node: String,
    root: gpui::WeakEntity<AppRoot>,
) -> Stateful<Div> {
    let id = key.clone();
    div()
        .on_children_prepainted(move |children, window, app| {
            let [bounds] = &children[..] else {
                return;
            };
            let quad = (
                f32::from(bounds.origin.x),
                f32::from(bounds.origin.y),
                f32::from(bounds.size.width),
                f32::from(bounds.size.height),
            );
            let _ = root.update(app, |this, _| {
                let cache = &mut this.scenario_ui.sidebar_bounds;
                // Identity rides along: recycled uniform_list slots reuse
                // geometry, so a label change must re-emit even when the
                // quad is unchanged.
                let fingerprint = (quad, label.clone());
                if cache.get(&key).is_some_and(|cached| *cached == fingerprint) {
                    return;
                }
                cache.insert(key.clone(), fingerprint);
                crate::ui_scenario::emit_sidebar_bounds(
                    target, &key, &label, &node, *bounds, window,
                );
            });
        })
        .child(element)
        .id(id)
}

#[cfg(not(feature = "ui-scenarios"))]
fn sidebar_bounds_probe(
    element: impl IntoElement,
    _target: &'static str,
    key: String,
    _label: String,
    _node: String,
    _root: gpui::WeakEntity<AppRoot>,
) -> Stateful<Div> {
    div().child(element).id(key)
}

/// Native drag payload for sidebar reorder/reparent.
#[derive(Clone)]
pub(super) struct SidebarDrag {
    pub(super) id: NavigatorNodeId,
    pub(super) ids: Option<Arc<[NavigatorNodeId]>>,
}

struct SidebarDragGhost {
    label: String,
}

impl Render for SidebarDragGhost {
    fn render(&mut self, _window: &mut Window, _cx: &mut Context<Self>) -> impl IntoElement {
        div()
            .px(px(10.0))
            .py(px(4.0))
            .rounded(px(6.0))
            .bg(theme::surface())
            .border_1()
            .border_color(theme::border())
            .text_size(px(12.0))
            .text_color(theme::text())
            .child(format!("Move {}", self.label))
    }
}

/// Parses `#RRGGBB` display colors (sidebar preference overrides and stored
/// connection colors) without allocating.
pub(super) fn parse_hex_color(hex: &str) -> Option<Rgba> {
    let hex = hex.strip_prefix('#')?;
    if hex.len() != 6 {
        return None;
    }
    let rgb = u32::from_str_radix(hex, 16).ok()?;
    Some(gpui::rgb(rgb))
}

/// Applies an alpha level over an opaque base color.
pub(super) fn tinted(base: Rgba, alpha: f32) -> Rgba {
    let clamp = alpha.clamp(0.0, 1.0);
    gpui::Rgba {
        r: base.r,
        g: base.g,
        b: base.b,
        a: clamp,
    }
}

/// Owning connection id for a row, so every descendant renders on its
/// connection's tinted subtree background.
pub(super) fn row_connection_id(id: &NavigatorNodeId) -> Option<&str> {
    match id {
        NavigatorNodeId::Group { .. } => None,
        NavigatorNodeId::Connection { connection }
        | NavigatorNodeId::Database { connection, .. }
        | NavigatorNodeId::Schema { connection, .. }
        | NavigatorNodeId::ObjectGroup { connection, .. }
        | NavigatorNodeId::Object { connection, .. }
        | NavigatorNodeId::SchemaObject { connection, .. }
        | NavigatorNodeId::Column { connection, .. }
        | NavigatorNodeId::Provider { connection, .. } => Some(connection),
    }
}

pub(super) fn connection_brand_asset(
    kind: crate::connections::ConnectionKind,
) -> Option<&'static str> {
    match kind {
        crate::connections::ConnectionKind::Postgres => Some("icons/postgres.svg"),
        crate::connections::ConnectionKind::Sqlite => Some("icons/sqlite.svg"),
        crate::connections::ConnectionKind::Mongodb => Some("icons/mongodb.svg"),
        crate::connections::ConnectionKind::Clickhouse => Some("icons/clickhouse.svg"),
        crate::connections::ConnectionKind::Redis => Some("icons/redis.svg"),
        // The dolphin alone: the full MySQL logo's wordmark is a smudge at icon size.
        crate::connections::ConnectionKind::Mysql => Some("icons/mysql-mark.svg"),
        crate::connections::ConnectionKind::Mariadb => Some("icons/mariadb.svg"),
    }
}

/// (glyph element, icon color): per-kind row icon mapping.
/// Database yellow, schema sky, tables/collections green, views purple,
/// GridFS buckets cyan; groups stay amber folders.
fn row_glyph(
    row: &NavigatorRow,
    provider_kind: Option<&str>,
    provider_icon: Option<&str>,
    object_kind: Option<KindOfObject>,
) -> AnyElement {
    let icon = |glyph: SidebarGlyph, color: Rgba| {
        Icon::new(glyph)
            .size(px(theme::SIDEBAR_ICON_SIZE))
            .text_color(color)
            .into_any_element()
    };
    let component = |name: IconName, color: Rgba| {
        Icon::new(name)
            .size(px(theme::SIDEBAR_ICON_SIZE))
            .text_color(color)
            .into_any_element()
    };
    match &row.id {
        // Connections render brand art at the call site; this arm only
        // satisfies exhaustiveness for direct callers.
        NavigatorNodeId::Connection { .. } => component(IconName::Building2, theme::text_muted()),
        NavigatorNodeId::Group { .. } => {
            let name = if row.expanded {
                IconName::FolderOpen
            } else {
                IconName::FolderClosed
            };
            component(name, theme::icon_folder())
        }
        NavigatorNodeId::Database { .. } => icon(SidebarGlyph::Database, theme::icon_database()),
        NavigatorNodeId::Schema { .. } => component(IconName::FolderOpen, theme::icon_schema()),
        NavigatorNodeId::ObjectGroup { group, .. } | NavigatorNodeId::SchemaObject { group, .. } => {
            match group {
                ObjectGroup::Tables => icon(SidebarGlyph::Table, theme::icon_table()),
                ObjectGroup::Views | ObjectGroup::MaterializedViews => {
                    icon(SidebarGlyph::View, theme::icon_view())
                }
                other => {
                    let (path, color) = match other {
                        ObjectGroup::Functions => ("icons/braces.svg", theme::icon_schema()),
                        ObjectGroup::Procedures => ("icons/square-terminal.svg", theme::icon_schema()),
                        ObjectGroup::Sequences => ("icons/hash.svg", theme::icon_bucket()),
                        ObjectGroup::Triggers => ("icons/crosshair.svg", theme::icon_folder()),
                        ObjectGroup::Types => ("icons/type.svg", theme::icon_collection()),
                        _ => ("icons/calendar-clock.svg", theme::icon_bucket()),
                    };
                    Icon::empty()
                        .path(path)
                        .size(px(theme::SIDEBAR_ICON_SIZE))
                        .text_color(color)
                        .into_any_element()
                }
            }
        }
        NavigatorNodeId::Object { .. } => match object_kind {
            Some(kind) if kind.is_view() => icon(SidebarGlyph::View, theme::icon_view()),
            Some(KindOfObject::Sequence) => Icon::empty()
                .path("icons/hash.svg")
                .size(px(theme::SIDEBAR_ICON_SIZE))
                .text_color(theme::icon_bucket())
                .into_any_element(),
            _ => icon(SidebarGlyph::Table, theme::icon_table()),
        },
        NavigatorNodeId::Column { .. } => component(IconName::Frame, theme::text_muted()),
        NavigatorNodeId::Provider { .. } => match provider_kind {
            Some("database") => icon(SidebarGlyph::Database, theme::icon_database()),
            Some("collection") => icon(SidebarGlyph::Table, theme::icon_collection()),
            Some(aggregate::saved::GROUP_KIND) => {
                component(IconName::FolderClosed, theme::icon_folder())
            }
            Some(aggregate::saved::ITEM_KIND) => Icon::empty()
                .path("icons/list-ordered.svg")
                .size(px(theme::SIDEBAR_ICON_SIZE))
                .text_color(theme::icon_schema())
                .into_any_element(),
            Some("bucket") | Some("gridfs") => icon(SidebarGlyph::Table, theme::icon_bucket()),
            // Redis: a key wears the color of its type, a namespace is a folder.
            Some(crate::navigator::redis_tree::KEY) => Icon::empty()
                .path("icons/key-round.svg")
                .size(px(theme::SIDEBAR_ICON_SIZE))
                .text_color(super::redis::type_color(
                    provider_icon.and_then(crate::redis::KeyType::parse),
                ))
                .into_any_element(),
            Some(crate::navigator::redis_tree::NAMESPACE) => {
                let name = if row.expanded { IconName::FolderOpen } else { IconName::FolderClosed };
                component(name, theme::icon_folder())
            }
            _ => component(IconName::File, theme::text_muted()),
        },
    }
}

/// Maps a GPUI `LoadRequest::Provider` into the driver ABI's
/// `NavigatorRequest` JSON shape.
pub(super) fn navigator_request_from_load(request: &LoadRequest) -> engine_driver_api::NavigatorRequest {
    match request {
        LoadRequest::Provider {
            connection: _,
            parent,
            payload,
            cursor,
            limit,
        } => engine_driver_api::NavigatorRequest {
            parent: parent.clone(),
            database: payload
                .as_ref()
                .and_then(|p| p.get("database"))
                .and_then(|v| v.as_str())
                .map(String::from),
            payload: payload.clone(),
            cursor: cursor.clone(),
            limit: *limit,
        },
        _ => engine_driver_api::NavigatorRequest {
            parent: None,
            database: None,
            payload: None,
            cursor: None,
            limit: 0,
        },
    }
}

/// One navigator call on a driver session. The session lock is held for the
/// call only; run this on a blocking thread, never the UI thread.
pub(super) fn list_driver_nav_nodes(
    session: &engine_driver_host::SharedDriverSession,
    request: &engine_driver_api::NavigatorRequest,
) -> Result<engine_driver_api::NavigatorPage, DriverQueryError> {
    let json = serde_json::to_string(request).map_err(|error| {
        engine_driver_host::LoaderError::MalformedResponse(format!(
            "invalid navigator request: {error}"
        ))
    })?;
    Ok(session
        .lock()
        .map_err(|_| DriverQueryError::SessionPoisoned)?
        .list_nav_nodes(&json)?)
}

#[derive(Default)]
pub(super) struct NavigatorUiState {
    pub(super) pending_loads: HashMap<String, Vec<(LoadRequest, u64)>>,
    /// Bumped whenever visible navigator rows change; captured by load tasks.
    pub(super) generation: u64,
    /// Row currently hovered by a native sidebar drag for drop highlighting.
    pub(super) drop_target: Option<NavigatorNodeId>,
    /// The row whose failure popover is open (at most one).
    pub(super) problem_open: Option<NavigatorNodeId>,
}

impl AppRoot {
    /// The sidebar's connection-state dot color (also used by tab group chips).
    pub(super) fn connection_status_color(&self, connection: &str) -> Rgba {
        match self.sessions.status(connection) {
            ConnectionStatus::Disconnected => theme::text_faint(),
            ConnectionStatus::Connecting => theme::warning(),
            ConnectionStatus::Connected => theme::success(),
            ConnectionStatus::Failed(_) => theme::danger(),
        }
    }

    /// Plain single left click on a row:
    /// connection rows select and toggle expansion (connecting first when
    /// needed); table/view rows only select (their chevron still expands);
    /// everything else keeps `handle_navigator_click`. Modifier clicks and
    /// double clicks are routed elsewhere, so a double click never toggles a
    /// second time.
    pub(super) fn handle_navigator_row_click(&mut self, row: &NavigatorRow, cx: &mut Context<Self>) {
        match &row.id {
            NavigatorNodeId::Connection { .. } if !row.disabled => {
                self.navigator_ui.drop_target = None;
                if !self.navigator.select_visible(&row.id, false, false) {
                    return;
                }
                self.handle_navigator_disclosure(row, cx);
                self.notify_chrome(cx);
            }
            NavigatorNodeId::Object { .. } | NavigatorNodeId::SchemaObject { .. } => {
                self.navigator_ui.drop_target = None;
                self.navigator.select_visible(&row.id, false, false);
                // "Single click opens" (Settings › Navigation).
                if self.settings.navigation.single_click_opens {
                    self.handle_navigator_open(row, cx);
                }
                self.notify_chrome(cx);
            }
            _ => self.handle_navigator_click(row, cx),
        }
    }

    /// Handles a click on a navigator row: selection always, expansion for
    /// branches. Expanding dispatches the lazy load (connecting first when
    /// the connection has no live driver session yet).
    pub(super) fn handle_navigator_click(&mut self, row: &NavigatorRow, cx: &mut Context<Self>) {
        self.navigator_ui.drop_target = None;
        if !self.navigator.select_visible(&row.id, false, false) {
            return;
        }

        if row.disabled {
            self.notify_chrome(cx);
            return;
        }

        if let Some(parent) = self.navigator.load_more_parent(&row.id) {
            if let Some(request) = self.navigator.load_more(&parent) {
                self.ensure_connected_then_load(request, cx);
            }
            self.navigator_ui.generation = self.navigator.generation();
            self.notify_chrome(cx);
            return;
        }

        if let NavigatorNodeId::Connection { connection } = &row.id {
            self.open_connection_target(connection, cx);
        } else if row.expandable {
            self.handle_navigator_disclosure(row, cx);
        } else if self.open_redis_key_node(&row.id, cx) {
            // A Redis key opens its value tab.
        } else if let NavigatorNodeId::Provider { connection, .. } = &row.id {
            let can_open = self
                .navigator
                .actions(&row.id)
                .iter()
                .any(|action| action == "open");
            if can_open {
                if let Some(provider) = self.navigator.provider_node(&row.id).cloned() {
                    if provider.kind == aggregate::saved::ITEM_KIND {
                        if let Some(id) = provider
                            .payload
                            .get("aggregate_id")
                            .and_then(serde_json::Value::as_str)
                        {
                            self.open_saved_aggregate(connection, id, cx);
                        }
                    } else if provider.kind == "collection" {
                        let database = provider
                            .payload
                            .get("database")
                            .and_then(serde_json::Value::as_str);
                        let collection = provider
                            .payload
                            .get("collection")
                            .and_then(serde_json::Value::as_str);
                        if let (Some(database), Some(collection)) = (database, collection) {
                            self.open_collection_target(connection, database, collection, cx);
                        }
                    }
                }
            }
        }
        self.notify_chrome(cx);
    }

    pub(super) fn handle_navigator_disclosure(
        &mut self,
        row: &NavigatorRow,
        cx: &mut Context<Self>,
    ) {
        if row.disabled || !row.expandable {
            return;
        }
        if self.navigator.action_target(&row.id).is_none() {
            return;
        }
        let outcome = self.navigator.toggle_expansion(&row.id);
        self.navigator_ui.generation = self.navigator.generation();
        if let Some(request) = outcome.request {
            self.ensure_connected_then_load(request, cx);
        }
    }

    /// Double-click on an object opens (or reuses) its table-data tab.
    pub(super) fn handle_navigator_open(&mut self, row: &NavigatorRow, cx: &mut Context<Self>) {
        if !self.navigator.select_visible(&row.id, false, false) {
            return;
        }
        if let NavigatorNodeId::Object {
            connection,
            database,
            schema,
            object,
        } = &row.id
        {
            self.open_sql_object_tab(&row.label, connection, database, schema, object, cx);
            // What the object opens as is a setting: its data (the default),
            // the columns view, or the ER diagram of the table.
            if let Some(tab_id) = self.workspace.active_tab().map(|tab| tab.id) {
                match self.settings.navigation.table_double_click {
                    crate::settings::TableOpenAction::Data => {}
                    crate::settings::TableOpenAction::Properties => {
                        self.show_columns_presentation(tab_id, cx);
                    }
                    crate::settings::TableOpenAction::Diagram => {
                        self.show_diagram_presentation(tab_id, cx);
                    }
                }
            }
        } else if matches!(row.id, NavigatorNodeId::SchemaObject { .. }) {
            // A function, trigger, sequence, ... has no data: its source opens.
            self.open_schema_object_source(&row.id, &row.label, cx);
        } else if matches!(row.id, NavigatorNodeId::Provider { .. }) {
            self.open_redis_key_node(&row.id, cx);
        }
    }

    /// Opens (or reuses) the table-data tab of a loaded SQL table / view.
    /// Shared by the sidebar double-click and the command palette.
    pub(super) fn open_sql_object_tab(
        &mut self,
        label: &str,
        connection: &str,
        database: &str,
        schema: &str,
        object: &str,
        cx: &mut Context<Self>,
    ) {
        let id = NavigatorNodeId::Object {
            connection: connection.to_string(),
            database: database.to_string(),
            schema: schema.to_string(),
            object: object.to_string(),
        };
        if self.navigator.preview_sql(&id).is_none() {
            return;
        }
        // Table/view data opens as a table-data tab (WHERE / ORDER BY
        // bar, no SQL editor); New Query keeps the editor.
        let (_, created) = self.open_sql_table_tab(label, connection, database, schema, object);
        if self.sessions.is_connected(connection) {
            let tab_id = self
                .workspace
                .active_tab()
                .map(|tab| tab.id)
                .unwrap_or_default();
            if tab_id != 0 {
                self.load_table_structure(
                    tab_id,
                    connection,
                    database.to_string(),
                    schema.to_string(),
                    object.to_string(),
                    cx,
                );
            }
        }
        if created {
            self.run_active_tab_query(cx);
        }
        self.notify_chrome(cx);
    }

    /// Opens (or reuses) the Mongo collection tab and runs it when new.
    pub(super) fn open_collection_target(
        &mut self,
        connection: &str,
        database: &str,
        collection: &str,
        cx: &mut Context<Self>,
    ) {
        let (_, created) = self
            .workspace
            .open_or_activate_collection_tab(connection, database, collection);
        if created {
            self.run_active_tab_query(cx);
        }
        self.notify_chrome(cx);
    }

    /// Sidebar click on a connection row: opens its tab and connects.
    pub(super) fn open_connection_target(&mut self, connection: &str, cx: &mut Context<Self>) {
        let Some(saved) = self
            .saved_connections
            .iter()
            .find(|conn| conn.matches_id(connection))
            .cloned()
        else {
            self.notify_chrome(cx);
            return;
        };
        self.workspace
            .open_or_activate_connection_tab(connection.to_string(), saved.name);
        if !self.sessions.is_connected(connection) {
            self.ensure_connected(connection.to_string(), None, cx);
        }
        self.notify_chrome(cx);
    }

    /// Loads the catalog columns of the table behind `tab_id` (types, keys,
    /// defaults: what editing and the Columns presentation read) through the
    /// route that serves `connection`.
    pub(super) fn load_table_structure(
        &mut self,
        tab_id: u64,
        connection: &str,
        database: String,
        schema: String,
        object: String,
        cx: &mut Context<Self>,
    ) {
        if self.scenario_ui.service.is_some() {
            return;
        }
        let supports_schemas = self.sql_supports_schemas(connection);
        let connection = connection.to_string();
        let Some(session) = self.driver_runtime.sessions.get(&connection).cloned() else {
            return;
        };
        let load = LoadRequest::Columns {
            connection: connection.clone(),
            database,
            schema,
            object,
        };
        let Some(request) = navigator::sql_driver::catalog_request(&load, supports_schemas, None)
        else {
            return;
        };
        let Some(runtime) = self.workspace.runtime_mut(tab_id) else {
            return;
        };
        runtime.structure_loading = true;
        let task = cx.spawn(async move |this, cx| {
            let result = cx
                .background_executor()
                .spawn(async move {
                    smol::unblock(move || {
                        navigator::sql_driver::collect_pages(request, false, |request| {
                            list_driver_nav_nodes(&session, request)
                        })
                    })
                    .await
                })
                .await
                .map(|nodes| navigator::sql_driver::catalog_columns(&nodes))
                .map_err(|error| error.to_string());
            let _ = this.update(cx, |this, cx| {
                if let Err(error) = &result {
                    this.report_error(Area::Navigator, Some(&connection), error);
                    this.note_navigator_connection_lost(&connection, error);
                }
                if let Some(runtime) = this.workspace.runtime_mut(tab_id) {
                    runtime.structure_loading = false;
                    match result {
                        Ok(columns) => {
                            runtime.set_structure_columns(columns);
                            runtime.structure_error = None;
                        }
                        Err(error) => {
                            runtime.set_structure_columns(Vec::new());
                            runtime.structure_error = Some(error);
                        }
                    }
                }
                // The "still loading" notice is answered by the load itself.
                if let Some(view) = this.tab_views.get_mut(&tab_id) {
                    if view.detail_ui.error.as_deref()
                        == Some(SqlEditError::StructureLoading.to_string().as_str())
                    {
                        view.detail_ui.error = None;
                    }
                }
                this.notify_tab(tab_id, cx);
            });
        });
        task.detach();
    }

    /// Loads the structure of a table-data tab that has none: a tab restored
    /// from the saved session never went through the sidebar, and a failed
    /// load leaves nothing behind. Editing needs the table's keys and types.
    /// A no-op while a load is in flight or when the connection is not open.
    pub(super) fn ensure_table_structure(&mut self, tab_id: u64, cx: &mut Context<Self>) {
        let Some(tab) = self.workspace.tab(tab_id) else {
            return;
        };
        if !tab.query_mode.is_table_data() || tab.connection_id.is_empty() {
            return;
        }
        // Only behind rows on screen: a failed or running query shows none.
        if self.workspace.runtime(tab_id).is_none_or(|runtime| {
            runtime.structure_loading
                || !runtime.structure_columns.is_empty()
                || !matches!(runtime.query.status(), crate::query::QueryStatus::Ready { .. })
        }) {
            return;
        }
        let (connection, database, schema, object) = (
            tab.connection_id.clone(),
            tab.database.clone(),
            tab.schema.clone().unwrap_or_default(),
            tab.collection.clone(),
        );
        self.load_table_structure(tab_id, &connection, database, schema, object, cx);
    }

    /// Ensures the connection behind `request` has a live driver session,
    /// then dispatches the catalog load. The load captures the navigator
    /// generation so a reset tree rejects stale results.
    pub(super) fn ensure_connected_then_load(
        &mut self,
        request: LoadRequest,
        cx: &mut Context<Self>,
    ) {
        let connection = request.connection_id();
        self.ensure_connected(connection, Some(request), cx);
    }

    pub(super) fn drop_sidebar_drag(
        &mut self,
        drag: &SidebarDrag,
        target: &NavigatorNodeId,
        cx: &mut Context<Self>,
    ) {
        self.navigator_ui.drop_target = None;
        if let Err(error) = self.move_sidebar_drag_roots(&drag.id, drag.ids.as_deref(), target, cx)
        {
            self.report_error(Area::Navigator, None, &error);
            self.sidebar.notice = Some(error);
        }
        self.notify_chrome(cx);
    }

    /// Parent group and mixed sibling position for a drop onto a connection
    /// row: insert before the target among its group's groups+connections.
    #[cfg(test)]
    pub(super) fn sidebar_drop_sibling(
        &self,
        target_connection: &str,
        dragged: &NavigatorNodeId,
    ) -> (Option<String>, usize) {
        let preferences = self.navigator.preferences();
        let group = preferences
            .connections
            .iter()
            .find(|item| item.id == target_connection)
            .and_then(|item| item.group.clone());
        let placements: HashMap<_, _> = preferences
            .connections
            .iter()
            .map(|item| (item.id.as_str(), item))
            .collect();
        let mut siblings: Vec<(usize, NavigatorNodeId)> = preferences
            .groups
            .iter()
            .filter(|item| item.parent == group)
            .map(|item| {
                (
                    item.order,
                    NavigatorNodeId::Group {
                        id: item.id.clone(),
                    },
                )
            })
            .chain(self.navigator.connections().iter().filter_map(|saved| {
                let id = saved.id.to_string();
                let placement = placements.get(id.as_str());
                (placement.and_then(|item| item.group.as_deref()) == group.as_deref()).then(|| {
                    (
                        placement.map(|item| item.order).unwrap_or(usize::MAX),
                        NavigatorNodeId::Connection { connection: id },
                    )
                })
            }))
            .collect();
        siblings.sort_by_key(|(order, _)| *order);
        let target = NavigatorNodeId::Connection {
            connection: target_connection.into(),
        };
        let old_position = siblings.iter().position(|(_, id)| id == dragged);
        siblings.retain(|(_, id)| id != dragged);
        let position = if dragged == &target {
            old_position.unwrap_or(siblings.len())
        } else {
            siblings
                .iter()
                .position(|(_, id)| id == &target)
                .unwrap_or(siblings.len())
        };
        (group, position)
    }

    pub(super) fn open_scoped_or_generic_query(&mut self, cx: &mut Context<Self>) {
        let selected = self.navigator.selected().cloned();
        let handled = selected
            .as_ref()
            .is_some_and(|id| self.new_query_for_sidebar_node(id, cx));
        if !handled {
            self.workspace.open_query_tab("Query");
            self.notify_chrome(cx);
        }
    }

    pub(super) fn open_new_sidebar_group_dialog(
        &mut self,
        parent: Option<String>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let input = cx.new(|cx| InputState::new(window, cx).placeholder("Group name"));
        let root = cx.entity().downgrade();
        window.open_alert_dialog(cx, move |dialog, _, _| {
            let root = root.clone();
            let input = input.clone();
            let parent = parent.clone();
            dialog
                .title("New Group")
                .child(Input::new(&input))
                .button_props(
                    gpui_component::dialog::DialogButtonProps::default().ok_text("Create"),
                )
                .confirm()
                .on_ok(move |_, _, cx| {
                    let name = input.read(cx).value().to_string();
                    let _ = root.update(cx, |this, cx| {
                        this.create_sidebar_group(&name, parent.as_deref(), cx)
                    });
                    true
                })
        });
    }

    pub(super) fn open_rename_sidebar_group_dialog(
        &mut self,
        id: String,
        current: String,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let input = cx.new(|cx| InputState::new(window, cx).placeholder("Group name"));
        input.update(cx, |state, cx| {
            state.set_value(current.as_str(), window, cx);
        });
        let root = cx.entity().downgrade();
        window.open_alert_dialog(cx, move |dialog, _, _| {
            let root = root.clone();
            let input = input.clone();
            let id = id.clone();
            dialog
                .title("Rename Group")
                .child(Input::new(&input))
                .button_props(
                    gpui_component::dialog::DialogButtonProps::default().ok_text("Rename"),
                )
                .confirm()
                .on_ok(move |_, _, cx| {
                    let name = input.read(cx).value().to_string();
                    let _ = root.update(cx, |this, cx| this.rename_sidebar_group(&id, &name, cx));
                    true
                })
        });
    }

    pub(super) fn render_navigator(
        &mut self,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let root = cx.entity().downgrade();
        let search_input = self.sidebar_search_input(window, cx);
        let search = self.navigator.search_options().clone();
        let sort = self.navigator.preferences().sort;
        let export_ids: Vec<String> = self
            .navigator
            .selection_in_visible_order()
            .iter()
            .filter_map(|id| match id {
                NavigatorNodeId::Connection { connection } => Some(connection.clone()),
                _ => None,
            })
            .collect();
        let homogeneous_selection = export_ids.len() == self.navigator.selected_ids().len();
        let export_ids = if homogeneous_selection {
            export_ids
        } else {
            Vec::new()
        };

        // New Connection / New Query / Data Transfer live
        // in the global window toolbar (see shell.rs), not in the sidebar.
        // The sidebar starts with the Import/Export row below.
        let transfer_root = root.clone();
        let transfer_export_ids = export_ids.clone();
        // Sidebar toolbar: 36px tall, 12px side padding, 1px gap, small medium muted text,
        // bottom border, muted/20 background. Collapse/group/refresh are
        // 20px; hide is 24px. The trigger shrinks with ellipsis so the row
        // survives the 180px minimum width.
        let mut toolbar_bg = theme::surface();
        toolbar_bg.a = 0.2;
        let toolbar_icon = |id: &'static str| Button::new(id).xsmall().ghost();
        let toolbar_glyph = px(12.0);
        let probe_toolbar_root = root.clone();
        let probe_btn = |button: AnyElement, key: &'static str| {
            sidebar_bounds_probe(
                button,
                "sidebar-button",
                key.into(),
                String::new(),
                String::new(),
                probe_toolbar_root.clone(),
            )
        };
        let normal_transfer = div()
            .h(px(36.0))
            .flex()
            .items_center()
            .gap(px(1.0))
            .px(px(12.0))
            .border_b_1()
            .border_color(theme::border())
            .bg(toolbar_bg)
            .text_size(px(12.0))
            .font_weight(gpui::FontWeight::MEDIUM)
            .text_color(theme::text_muted())
            // Layout: shrinkable trigger, flexible spacer, fixed action
            // buttons. The wrapper is the flex item that shrinks; the button
            // fills it so the label (already min-w-0 + ellipsis) truncates
            // instead of pushing collapse/group/refresh/hide past the pane
            // edge. No auto margins: the spacer owns the surplus exactly once.
            .child(
                div().flex_grow_0().flex_shrink_1().min_w_0().child(probe_btn(
                Button::new("sidebar-import-export")
                    .debug_selector(|| "sidebar-import-export".into())
                    .icon(Icon::new(SidebarGlyph::Transfer))
                    .label("Import / Export")
                    .xsmall()
                    .compact()
                    .ghost()
                    .h(px(28.0))
                    .dropdown_caret(true)
                    .tooltip("Import or export connection configurations")
                    .dropdown_menu(move |menu, _, _| {
                        let import_root = transfer_root.clone();
                        let tablex_root = transfer_root.clone();
                        let export_root = transfer_root.clone();
                        let export_ids = transfer_export_ids.clone();
                        // Menu order, labels, icons and separators.
                        let menu = menu.min_w(px(192.0)).max_w(px(264.0));
                        // Element items carry a debug selector so tests click
                        // the real rows; they render like plain items.
                        let import_item = |selector: &'static str,
                                           label: &'static str,
                                           source: ImportSource| {
                            let root = import_root.clone();
                            PopupMenuItem::element(move |_, _| {
                                div()
                                    .debug_selector(move || selector.to_string())
                                    .child(label)
                            })
                            .icon(Icon::new(SidebarGlyph::Download).size(px(14.0)))
                            .on_click(move |_, window, app| {
                                let _ = root.update(app, |this, cx| {
                                    this.import_foreign_connections(source, window, cx);
                                });
                            })
                        };
                        menu
                            .item(import_item("import-menu-dbx", "Import DBX Config", ImportSource::Dbx))
                            .item(import_item("import-menu-navicat", "Import Navicat NCX", ImportSource::Navicat))
                            .item(import_item("import-menu-dbeaver", "Import DBeaver", ImportSource::Dbeaver))
                            .item(import_item("import-menu-datagrip", "Import DataGrip", ImportSource::DataGrip))
                            .separator()
                            .item(
                                PopupMenuItem::new("Export Connections")
                                    .icon(Icon::new(SidebarGlyph::Upload).size(px(14.0)))
                                    .disabled(export_ids.is_empty())
                                    .on_click(move |_, window, app| {
                                        let _ = export_root.update(app, |this, cx| {
                                            this.export_sidebar_config_file(
                                                &export_ids,
                                                window,
                                                cx,
                                            );
                                        });
                                    }),
                            )
                            .separator()
                            .item(
                                PopupMenuItem::new("Import TableX file…")
                                    .icon(Icon::new(SidebarGlyph::Download).size(px(14.0)))
                                    .on_click(move |_, window, app| {
                                        let _ = tablex_root.update(app, |this, cx| {
                                            this.import_sidebar_config_file(window, cx);
                                        });
                                    }),
                            )
                    })
                    .into_any_element(),
                "import-export",
            )))
            // Flexible spacer: absorbs all surplus
            // so the action buttons dock right; collapses to zero first at
            // 180px, then the trigger wrapper above starts truncating.
            .child(div().flex_1())
            .child(probe_btn(
                toolbar_icon("sidebar-collapse-all")
                    .debug_selector(|| "sidebar-collapse-all".into())
                    .icon(Icon::new(SidebarGlyph::CollapseAll).size(toolbar_glyph))
                    .tooltip("Collapse all")
                    .on_click(cx.listener(|this, _, _, cx| {
                        this.collapse_navigator(cx);
                    }))
                    .into_any_element(),
                "collapse-all",
            ))
            .child(probe_btn(
                toolbar_icon("sidebar-new-group")
                    .debug_selector(|| "sidebar-new-group".into())
                    .icon(Icon::new(SidebarGlyph::NewGroup).size(toolbar_glyph))
                    .tooltip("New group")
                    .on_click(cx.listener(|this, _, window, cx| {
                        if this.navigator.connection_multi_mode() {
                            this.open_create_selected_group_dialog(window, cx);
                        } else {
                            this.open_new_sidebar_group_dialog(None, window, cx);
                        }
                    }))
                    .into_any_element(),
                "new-group",
            ))
            .child(probe_btn(
                toolbar_icon("sidebar-refresh")
                    .debug_selector(|| "sidebar-refresh".into())
                    .icon(Icon::new(SidebarGlyph::Refresh).size(toolbar_glyph))
                    .tooltip("Refresh connections")
                    .on_click(cx.listener(|this, _, _, cx| {
                        this.refresh_saved_connections(cx);
                    }))
                    .into_any_element(),
                "refresh",
            ))
            .child(probe_btn(
                Button::new("sidebar-hide")
                    .debug_selector(|| "sidebar-hide".into())
                    .icon(Icon::new(SidebarGlyph::Collapse))
                    .small()
                    .compact()
                    .ghost()
                    .tooltip("Hide sidebar")
                    .on_click(cx.listener(|this, _, _, cx| {
                        this.shell.sidebar_collapsed = true;
                        this.notify_chrome(cx);
                    }))
                    .into_any_element(),
                "hide",
            ));

        // Multiselect toolbar: when several connections are selected the
        // Import/Export row is replaced by a count plus group/move/delete/
        // select-all/exit actions wired to the real batch controllers.
        let multi_snapshot: Option<Vec<String>> = if self.navigator.connection_multi_mode() {
            self.selected_sidebar_connection_ids().ok()
        } else {
            None
        };
        let toolbar_transfer = if let Some(snapshot) = multi_snapshot {
            let count = snapshot.len();
            let group_ids = snapshot.clone();
            let move_menu_root = root.clone();
            let move_ids = snapshot.clone();
            let delete_ids = snapshot.clone();
            let groups: Vec<(String, String)> = self
                .navigator
                .preferences()
                .groups
                .iter()
                .map(|group| (group.id.clone(), group.name.clone()))
                .collect();
            let multi_button =
                |id: &'static str| Button::new(id).w(px(24.0)).small().compact().ghost();
            let probe_root = root.clone();
            let probe = |button: AnyElement, key: &'static str| {
                sidebar_bounds_probe(
                    button,
                    "sidebar-multi",
                    key.into(),
                    format!("{count} selected"),
                    String::new(),
                    probe_root.clone(),
                )
            };
            div()
                .h(px(30.0))
                .flex()
                .items_center()
                .gap(px(1.0))
                .px(px(4.0))
                .border_b_1()
                .border_color(theme::border())
                .child(
                    div()
                        .text_size(px(12.0))
                        .text_color(theme::text_muted())
                        .child("Connections"),
                )
                .child(
                    div()
                        .ml(px(6.0))
                        .text_size(px(11.0))
                        .text_color(theme::text_faint())
                        .child(format!("{count} selected")),
                )
                .child(div().flex_1())
                .child(probe(
                    multi_button("sidebar-multi-group")
                        .debug_selector(|| "sidebar-multi-group".into())
                        .icon(Icon::new(SidebarGlyph::NewGroup).size(toolbar_glyph))
                        .tooltip("Group selected connections")
                        .on_click(cx.listener(move |this, _, window, cx| {
                            if this.selected_sidebar_snapshot_matches(&group_ids) {
                                this.open_create_selected_group_dialog(window, cx);
                            }
                        }))
                        .into_any_element(),
                    "multi-group",
                ))
                .child({
                    let move_root = move_menu_root.clone();
                    probe(
                        multi_button("sidebar-multi-move")
                            .debug_selector(|| "sidebar-multi-move".into())
                            .icon(Icon::new(SidebarGlyph::MoveToGroup).size(toolbar_glyph))
                            .tooltip("Move selected to group")
                            .dropdown_menu(move |menu, _, _| {
                                let ungroup_root = move_root.clone();
                                let ungroup_ids = move_ids.clone();
                                let mut built = menu.item(
                                    PopupMenuItem::new("Ungrouped").on_click(move |_, _, app| {
                                        let _ = ungroup_root.update(app, |this, cx| {
                                            if this.selected_sidebar_snapshot_matches(&ungroup_ids)
                                            {
                                                let _ = this
                                                    .move_selected_sidebar_connections(None, cx);
                                            }
                                        });
                                    }),
                                );
                                for (group_id, name) in groups.clone() {
                                    let item_root = move_root.clone();
                                    let item_ids = move_ids.clone();
                                    built = built.item(PopupMenuItem::new(name).on_click(
                                        move |_, _, app| {
                                            let _ = item_root.update(app, |this, cx| {
                                                if this.selected_sidebar_snapshot_matches(&item_ids)
                                                {
                                                    let _ = this.move_selected_sidebar_connections(
                                                        Some(&group_id),
                                                        cx,
                                                    );
                                                }
                                            });
                                        },
                                    ));
                                }
                                built
                            })
                            .into_any_element(),
                        "multi-move",
                    )
                })
                .child(probe(
                    multi_button("sidebar-multi-delete")
                        .debug_selector(|| "sidebar-multi-delete".into())
                        .icon(Icon::new(SidebarGlyph::Trash).size(toolbar_glyph))
                        .tooltip(format!("Delete {count} selected connections"))
                        .on_click(cx.listener(move |this, _, window, cx| {
                            if this.selected_sidebar_snapshot_matches(&delete_ids) {
                                this.confirm_delete_selected_connections(window, cx);
                            }
                        }))
                        .into_any_element(),
                    "multi-delete",
                ))
                .child(probe(
                    multi_button("sidebar-multi-select-all")
                        .debug_selector(|| "sidebar-multi-select-all".into())
                        .icon(Icon::new(SidebarGlyph::SelectAll).size(toolbar_glyph))
                        .tooltip("Select all visible connections")
                        .on_click(cx.listener(|this, _, _, cx| {
                            this.navigator.select_all_connections();
                            this.notify_chrome(cx);
                        }))
                        .into_any_element(),
                    "multi-select-all",
                ))
                .child(probe(
                    multi_button("sidebar-multi-exit")
                        .debug_selector(|| "sidebar-multi-exit".into())
                        .icon(Icon::new(IconName::Close).size(toolbar_glyph))
                        .tooltip("Exit selection")
                        .on_click(cx.listener(|this, _, _, cx| {
                            this.navigator.clear_multi_selection();
                            this.notify_chrome(cx);
                        }))
                        .into_any_element(),
                    "multi-exit",
                ))
                .into_any_element()
        } else {
            normal_transfer.into_any_element()
        };

        // Search row: 24px field with a magnifier (spinner while a remote
        // search runs) prefix; inside-right: the component's cleanable x
        // (non-empty only), the
        // `.*` regex text toggle and the Local-search switch. Locate, list
        // options and connected-only are 24px bordered buttons outside.
        let regex_invalid = search.regex && self.sidebar.search_error.is_some();
        let searching = self.sidebar.remote.loading;
        let local_search = self.navigator.preferences().local_search();
        // No inline error block; the invalid state is the danger colors
        // plus a one-line message in the `.*` tooltip.
        let regex_tip: gpui::SharedString = match (&self.sidebar.search_error, regex_invalid) {
            (Some(message), true) => message.clone().into(),
            _ => "Match sidebar names, comments, and aliases with regular expressions.".into(),
        };
        let switch_tip = if search.regex {
            "Local search is unavailable in regular expression mode."
        } else {
            "Search only currently loaded sidebar objects without remote requests."
        };
        // Official ghost Button: `.selected()` carries the armed state through
        // the theme; only the invalid-pattern text is overridden (danger).
        let regex_button = Button::new("sidebar-regex-toggle")
            .debug_selector(|| "sidebar-regex".into())
            .label(".*")
            .ghost()
            .xsmall()
            .compact()
            .font_family("Menlo")
            .text_size(px(11.0))
            .selected(search.regex)
            .toggled(search.regex)
            .when(regex_invalid, |this| this.text_color(theme::danger()))
            .accessibility_label("Regular expression search")
            .tooltip(regex_tip)
            .on_click(cx.listener(|this, _, _, cx| {
                let mut options = this.navigator.search_options().clone();
                options.regex = !options.regex;
                this.update_sidebar_search(options, cx);
            }));
        let search_bar = div()
            .flex()
            .items_center()
            .gap(px(4.0))
            .px(px(8.0))
            .py(px(4.0))
            .border_b_1()
            .border_color(theme::border())
            .child(
                div()
                    .flex_1()
                    .min_w_0()
                    .when(regex_invalid, |this| {
                        this.debug_selector(|| "sidebar-search-invalid".into())
                    })
                    .child(
                        Input::new(&search_input)
                            .small()
                            .cleanable(true)
                            .h(px(24.0))
                            .text_size(px(12.0))
                            .when(regex_invalid, |input| {
                                input.focus_bordered(false).border_color(theme::danger())
                            })
                            .prefix(if searching {
                                gpui_component::spinner::Spinner::new()
                                    .with_size(Size::XSmall)
                                    .color(theme::text_muted().into())
                                    .into_any_element()
                            } else {
                                Icon::new(IconName::Search)
                                    .size(px(12.0))
                                    .text_color(theme::text_muted())
                                    .into_any_element()
                            })
                            .suffix(
                                div()
                                    .debug_selector(|| "sidebar-search-suffix".into())
                                    .flex()
                                    .items_center()
                                    .gap(px(2.0))
                                    .child(probe_btn(
                                        regex_button.into_any_element(),
                                        "regex",
                                    ))
                                    .child(
                                        div()
                                            .debug_selector(|| "sidebar-global-search".into())
                                            // The component's own disabled style is
                                            // subtle; dim the whole switch so the
                                            // unavailable state is obvious.
                                            .when(search.regex, |this| {
                                                this.opacity(0.4)
                                                    .cursor_not_allowed()
                                                    .child(
                                                        div()
                                                            .debug_selector(|| {
                                                                "sidebar-global-search-dimmed".into()
                                                            })
                                                            .size_0(),
                                                    )
                                            })
                                            .child(
                                            Switch::new("sidebar-global-search")
                                                .small()
                                                .checked(local_search)
                                                .disabled(search.regex)
                                                .accessibility_label("Local search")
                                                .tooltip(switch_tip)
                                                .on_click(cx.listener(|this, checked: &bool, _, cx| {
                                                    this.set_sidebar_local_search(*checked, cx);
                                                })),
                                        ),
                                    ),
                            ),
                    ),
            )
            .child(probe_btn(
                Button::new("sidebar-locate")
                    .debug_selector(|| "sidebar-locate".into())
                    .icon(Icon::new(SidebarGlyph::Locate))
                    .w(px(24.0))
                    .small()
                    .compact()
                    .secondary()
                    .outline()
                    .tooltip("Locate in sidebar")
                    .on_click(cx.listener(|this, _, _, cx| {
                        this.locate_active_sidebar_tab(cx);
                    }))
                    .into_any_element(),
                "locate",
            ))
            .child({
                let has_scope_filter = search.scope != navigator::SearchScope::All;
                let settings_active =
                    has_scope_filter || sort != ConnectionSort::Manual || search.show_hidden;
                // Legacy per-connection actions that left the connection menu
                // act on the selected connection; snapshot here because the
                // dropdown builder is Fn and cannot borrow cx mutably.
                let settings_selected: Option<String> = match self.navigator.selected().cloned() {
                    Some(NavigatorNodeId::Connection { connection }) => Some(connection),
                    _ => None,
                };
                // Single-selection copy target for the settings menu; the
                // helper also backs the model tests.
                let settings_copy_target = self.sidebar_single_config_copy_target();
                let settings_root = root.clone();
                sidebar_bounds_probe(
                    Button::new("sidebar-settings")
                        .debug_selector(|| "sidebar-settings".into())
                        .icon(Icon::new(SidebarGlyph::Sliders).size(px(12.0)))
                        .w(px(24.0))
                        .small()
                        .compact()
                        .secondary()
                        .outline()
                        .selected(settings_active)
                        .tooltip("Sort connections / Filter by type")
                        .dropdown_menu(move |menu, _, _| {
                            let scope_root = settings_root.clone();
                            let sort_root = settings_root.clone();
                            let hidden_root = settings_root.clone();
                            let mut built = menu.label("Sort connections");
                            for (value, label, glyph) in [
                                (ConnectionSort::Manual, "Manual", SidebarGlyph::SortManual),
                                (
                                    ConnectionSort::Ascending,
                                    "Ascending",
                                    SidebarGlyph::SortAsc,
                                ),
                                (
                                    ConnectionSort::Descending,
                                    "Descending",
                                    SidebarGlyph::SortDesc,
                                ),
                            ] {
                                let item_root = sort_root.clone();
                                let checked = sort == value;
                                built = built.item(
                                    PopupMenuItem::new(label)
                                        .icon(Icon::new(glyph).size(px(12.0)))
                                        .checked(checked)
                                        .on_click(move |_, _, app| {
                                            let _ = item_root.update(app, |this, cx| {
                                                if this.sort_sidebar_connections(value, cx).is_err()
                                                {
                                                    // Error surfaces on the sidebar banner.
                                                }
                                            });
                                        }),
                                );
                            }
                            built = built.separator().label("Filter by type");
                            for (scope, label) in [
                                (navigator::SearchScope::All, "All"),
                                (navigator::SearchScope::Connections, "Connections"),
                                (navigator::SearchScope::Databases, "Databases"),
                                (navigator::SearchScope::Objects, "Objects"),
                            ] {
                                let item_root = scope_root.clone();
                                let checked = search.scope == scope;
                                built = built.item(
                                    PopupMenuItem::new(label).checked(checked).on_click(
                                        move |_, _, app| {
                                            let _ = item_root.update(app, |this, cx| {
                                                let mut options =
                                                    this.navigator.search_options().clone();
                                                options.scope = scope;
                                                this.update_sidebar_search(options, cx);
                                            });
                                        },
                                    ),
                                );
                            }
                            built = built.separator().item(
                                PopupMenuItem::new("Show hidden connections")
                                    .checked(search.show_hidden)
                                    .on_click(move |_, _, app| {
                                        let _ = hidden_root.update(app, |this, cx| {
                                            let mut options =
                                                this.navigator.search_options().clone();
                                            options.show_hidden = !options.show_hidden;
                                            this.update_sidebar_search(options, cx);
                                        });
                                    }),
                            );
                            if has_scope_filter {
                                let item_root = scope_root.clone();
                                built = built.item(PopupMenuItem::new("Clear filter").on_click(
                                    move |_, _, app| {
                                        let _ = item_root.update(app, |this, cx| {
                                            let mut options =
                                                this.navigator.search_options().clone();
                                            options.scope = navigator::SearchScope::All;
                                            this.update_sidebar_search(options, cx);
                                        });
                                    },
                                ));
                            }
                            // Legacy per-connection actions that left the
                            // connection menu stay reachable here, acting on
                            // the selected connection.
                            let selected_id = settings_selected.clone();
                            built = built.separator().label("Selected connection");
                            {
                                let item_root = settings_root.clone();
                                let id = selected_id.clone();
                                built = built.item(
                                    PopupMenuItem::new("Toggle Pin")
                                        .disabled(id.is_none())
                                        .on_click(move |_, _, app| {
                                            let _ = item_root.update(app, |this, cx| {
                                                if let Some(id) = id.clone() {
                                                    this.toggle_connection_pin(&id, cx);
                                                }
                                            });
                                        }),
                                );
                            }
                            {
                                let item_root = settings_root.clone();
                                let id = selected_id.clone();
                                built = built.item(
                                    PopupMenuItem::new("Hide / Unhide")
                                        .disabled(id.is_none())
                                        .on_click(move |_, _, app| {
                                            let _ = item_root.update(app, |this, cx| {
                                                if let Some(id) = id.clone() {
                                                    let hidden = this
                                                        .navigator
                                                        .preferences()
                                                        .connections
                                                        .iter()
                                                        .find(|item| item.id == id)
                                                        .is_some_and(|item| item.hidden);
                                                    let _ = this.set_sidebar_connection_hidden(
                                                        &id, !hidden, cx,
                                                    );
                                                }
                                            });
                                        }),
                                );
                            }
                            {
                                let item_root = settings_root.clone();
                                let id = settings_copy_target.clone();
                                built = built.item(
                                    PopupMenuItem::new("Copy Connection Config")
                                        .disabled(id.is_none())
                                        .on_click(move |_, _, app| {
                                            let _ = item_root.update(app, |this, cx| {
                                                if let Some(id) = id.clone() {
                                                    this.copy_sidebar_connection_config(&id, cx);
                                                }
                                            });
                                        }),
                                );
                            }
                            {
                                let item_root = settings_root.clone();
                                let id = selected_id.clone();
                                built = built.item(
                                    PopupMenuItem::new("Export Connection Config…")
                                        .disabled(id.is_none())
                                        .on_click(move |_, window, app| {
                                            let _ = item_root.update(app, |this, cx| {
                                                if let Some(id) = id.clone() {
                                                    this.export_sidebar_config_file(
                                                        &[id],
                                                        window,
                                                        cx,
                                                    );
                                                }
                                            });
                                        }),
                                );
                            }
                            // Paste lives here (not in the Import/Export
                            // dropdown) so the menu order stays exact
                            // with no clipboard action dropped.
                            {
                                let item_root = settings_root.clone();
                                built = built.item(
                                    PopupMenuItem::new("Paste Connection Config")
                                        .on_click(move |_, window, app| {
                                            let _ = item_root.update(app, |this, cx| {
                                                this.paste_sidebar_connection_config(window, cx);
                                            });
                                        }),
                                );
                            }
                            built
                        })
                        .into_any_element(),
                    "sidebar-button",
                    "settings".into(),
                    String::new(),
                    String::new(),
                    root.clone(),
                )
            })
            .child(probe_btn(
                // Active-connection filter button: 24px bordered button with
                // a centered green dot while any connection is live (a ring
                // otherwise); pressed = accent text, tint and border.
                {
                    let live = self.sessions.connected().next().is_some();
                    let pressed = search.connected_only;
                    // Official outline Button (same family as locate/settings);
                    // `.selected()` paints the pressed state from the theme.
                    Button::new("sidebar-connected-only")
                        .debug_selector(|| "sidebar-connected-only".into())
                        .w(px(24.0))
                        .small()
                        .compact()
                        .secondary()
                        .outline()
                        .selected(pressed)
                        .toggled(pressed)
                        .accessibility_label("Show active connections only")
                        .tooltip("Show active connections only")
                        .child(if live {
                            div()
                                .debug_selector(|| "sidebar-connected-dot".into())
                                .flex_shrink_0()
                                .w(px(6.0))
                                .h(px(6.0))
                                .rounded_full()
                                .bg(theme::success())
                                .into_any_element()
                        } else {
                            div()
                                .flex_shrink_0()
                                .w(px(12.0))
                                .h(px(12.0))
                                .rounded_full()
                                .border_1()
                                .border_color(if pressed {
                                    theme::accent()
                                } else {
                                    theme::text_muted()
                                })
                                .into_any_element()
                        })
                        .on_click(cx.listener(|this, _, _, cx| {
                            let mut options = this.navigator.search_options().clone();
                            options.connected_only = !options.connected_only;
                            this.update_sidebar_search(options, cx);
                        }))
                        .into_any_element()
                },
                "connected-only",
            ));

        let menu_root = root.clone();
        let selected_drag_ids: Option<Arc<[NavigatorNodeId]>> =
            (self.navigator.selected_ids().len() > 1).then(|| {
                Arc::from(
                    self.navigator
                        .selection_in_visible_order()
                        .into_boxed_slice(),
                )
            });
        let list = uniform_list(
            "navigator-tree",
            self.navigator.visible_row_count(),
            cx.processor(move |this, range: Range<usize>, _window, cx| {
                range
                    .map(|index| {
                        // Owned per-row handle: inner `move` menu/drag closures
                        // clone from this instead of the render-scope binding.
                        let menu_root = menu_root.clone();
                        let row = this.navigator.visible_row_at(index);
                        let Some(row) = row else {
                            return div().h(px(theme::SIDEBAR_ROW_HEIGHT)).into_any_element();
                        };
                        let row_for_disclosure = row.clone();
                        let row_for_disclosure_click = row_for_disclosure.clone();
                        let row_id = row.id.clone();
                        let provider_kind = this
                            .navigator
                            .provider_node(&row.id)
                            .map(|node| node.kind.as_str());
                        let provider_icon = this
                            .navigator
                            .provider_node(&row.id)
                            .and_then(|node| node.icon.as_deref());
                        let object_kind = this.navigator.object_kind(&row.id);
                        let tint = row_connection_id(&row.id)
                            .and_then(|id| this.sidebar_color(id))
                            .and_then(parse_hex_color);
                        let selected = this.navigator.selected_ids().contains(&row.id);
                        let row_bg = if selected {
                            tint.map(|color| tinted(color, 0.22))
                                .unwrap_or_else(theme::selected)
                        } else {
                            tint.map(|color| tinted(color, 0.08)).unwrap_or(gpui::Rgba {
                                r: 0.0,
                                g: 0.0,
                                b: 0.0,
                                a: 0.0,
                            })
                        };
                        let row_hover = tint
                            .map(|color| tinted(color, 0.16))
                            .unwrap_or_else(theme::hover);
                        let mut disclosure = div()
                            .id(("nav-disclosure", index))
                            .debug_selector(move || format!("nav-disclosure-{index}"))
                            .w(px(16.0))
                            .flex_shrink_0()
                            .flex()
                            .items_center()
                            .justify_center();
                        if row.expandable {
                            let chevron = if row.expanded {
                                IconName::ChevronDown
                            } else {
                                IconName::ChevronRight
                            };
                            disclosure = disclosure.child(
                                Icon::new(chevron)
                                    .size(px(12.0))
                                    .text_color(theme::text_muted()),
                            );
                            disclosure = disclosure.on_click(cx.listener(move |this, _, _, cx| {
                                this.handle_navigator_disclosure(&row_for_disclosure_click, cx);
                                cx.stop_propagation();
                                this.notify_chrome(cx);
                            }));
                        }
                        // A disabled connection shows no status.
                        let status = match &row.id {
                            NavigatorNodeId::Connection { connection } if !row.disabled => {
                                this.sessions.status(connection)
                            }
                            _ => ConnectionStatus::Disconnected,
                        };
                        let row_icon = match &row.id {
                            NavigatorNodeId::Connection { connection } => {
                                let kind = this
                                    .navigator
                                    .connection(connection)
                                    .map(|saved| saved.kind);
                                let icon = kind
                                    .and_then(connection_brand_asset)
                                    .map(|asset| {
                                        img(asset)
                                            .w(px(theme::SIDEBAR_ICON_SIZE))
                                            .h(px(theme::SIDEBAR_ICON_SIZE))
                                            .into_any_element()
                                    })
                                    .unwrap_or_else(|| {
                                        Icon::new(IconName::Building2)
                                            .size(px(theme::SIDEBAR_ICON_SIZE))
                                            .text_color(theme::text_muted())
                                            .into_any_element()
                                    });
                                navigator_status::icon_with_status(
                                    icon,
                                    &status,
                                    navigator_status::over_sidebar(row_bg),
                                    navigator_status::over_sidebar(row_hover),
                                    index,
                                )
                            }
                            _ => row_glyph(&row, provider_kind, provider_icon, object_kind),
                        };
                        let mut element = div()
                            .id(("nav-row", index))
                            .group(navigator_status::ROW_GROUP)
                            .debug_selector(move || format!("sidebar-row-{index}"))
                            .w_full()
                            .h(px(theme::SIDEBAR_ROW_HEIGHT))
                            .flex()
                            .items_center()
                            .pl(px(8.0 + row.depth as f32 * theme::SIDEBAR_INDENT))
                            .text_size(px(12.0))
                            .text_color(theme::text())
                            .bg(row_bg)
                            .child(disclosure)
                            .child(row_icon)
                            .child(div().w(px(6.0)).flex_shrink_0())
                            .child(
                                div()
                                    .flex_1()
                                    .min_w_0()
                                    .overflow_hidden()
                                    .whitespace_nowrap()
                                    .text_ellipsis()
                                    .child(row.label.clone()),
                            );
                        element = element.hover(move |style| style.bg(row_hover));
                        // A Redis key shows its type at the right edge.
                        if provider_kind == Some(crate::navigator::redis_tree::KEY) {
                            if let Some(type_name) = this
                                .navigator
                                .provider_node(&row.id)
                                .and_then(|node| node.subtitle.clone())
                            {
                                element = element.child(
                                    div()
                                        .flex_shrink_0()
                                        .px(px(8.0))
                                        .text_size(px(10.0))
                                        .text_color(theme::text_faint())
                                        .child(type_name),
                                );
                            }
                        }
                        if this.navigator_ui.drop_target.as_ref() == Some(&row.id) {
                            element = element.border_1().border_color(theme::accent());
                        }
                        if row.disabled {
                            element = element.text_color(theme::text_faint());
                        }
                        if matches!(row.load, Some(crate::navigator::NodeLoadState::Loading)) {
                            element = element.child(
                                div()
                                    .pl(px(6.0))
                                    .text_color(theme::text_muted())
                                    .child("…"),
                            );
                        }
                        if let NavigatorNodeId::Connection { connection } = &row.id {
                            if let Some(via) = this.via_ssh(connection) {
                                element = element.child(connection_policy::info_badge(
                                    this.route_badge(connection),
                                    via,
                                    format!("sidebar-ssh-badge-{index}"),
                                ));
                            }
                            if let Some(ty) = this.connection_type_of(connection) {
                                element = element.child(connection_policy::type_badge(
                                    ty,
                                    format!("sidebar-type-badge-{index}"),
                                ));
                            }
                        }
                        // The failure marker closes the row, after the badges.
                        if let Some(problem) = navigator_status::row_problem(&row, &status) {
                            element =
                                element.child(this.render_row_problem(index, &row, problem, cx));
                        }
                        let row_for_click = row.clone();
                        let base = element.on_click(cx.listener(
                            move |this, event: &gpui::ClickEvent, window, cx| {
                                let modifiers = event.modifiers();
                                if modifiers.secondary(cx) || modifiers.shift {
                                    this.navigator.select_visible(
                                        &row_for_click.id,
                                        modifiers.secondary(cx),
                                        modifiers.shift,
                                    );
                                    this.sidebar.focus.focus(window, cx);
                                    this.notify_chrome(cx);
                                    return;
                                }
                                if event.click_count() >= 2 {
                                    this.handle_navigator_open(&row_for_click, cx);
                                } else {
                                    this.handle_navigator_row_click(&row_for_click, cx);
                                }
                                this.sidebar.focus.focus(window, cx);
                            },
                        ));
                        // Measured-bounds probe: wraps the stateful row so the
                        // capture runner clicks real layout geometry. Events
                        // bubble unchanged; drag/menu chains continue below.
                        let mut base = sidebar_bounds_probe(
                            base,
                            "sidebar-row",
                            format!("row-{index}"),
                            row.label.clone(),
                            format!("{:?}", row.id),
                            menu_root.clone(),
                        );
                        let connected = match &row_id {
                            NavigatorNodeId::Connection { connection } => matches!(
                                this.sessions.status(connection),
                                ConnectionStatus::Connected
                            ),
                            _ => false,
                        };
                        let can_query = this.navigator.supports_action(
                            &row_id,
                            connected,
                            NavigatorAction::NewQuery,
                        );
                        let can_open = this.navigator.supports_action(
                            &row_id,
                            connected,
                            NavigatorAction::Open,
                        );

                        if matches!(
                            &row_id,
                            NavigatorNodeId::Group { .. } | NavigatorNodeId::Connection { .. }
                        ) {
                            let drag = SidebarDrag {
                                id: row_id.clone(),
                                ids: if this.navigator.selected_ids().contains(&row_id) {
                                    selected_drag_ids.clone()
                                } else {
                                    None
                                },
                            };
                            let ghost_label = row.label.clone();
                            base = base.on_drag(drag, move |_, _, _, cx| {
                                cx.new(|_| SidebarDragGhost {
                                    label: ghost_label.clone(),
                                })
                            });
                            let drop_root = menu_root.clone();
                            let drop_target = row_id.clone();
                            let hover_target = drop_target.clone();
                            base = base
                                .on_drag_move(cx.listener(
                                    move |this, _: &gpui::DragMoveEvent<SidebarDrag>, _, cx| {
                                        this.navigator_ui.drop_target = Some(hover_target.clone());
                                        this.notify_chrome(cx);
                                    },
                                ))
                                .on_drop(move |drag: &SidebarDrag, _, app| {
                                    let _ = drop_root.update(app, |this, cx| {
                                        this.drop_sidebar_drag(drag, &drop_target, cx);
                                    });
                                });
                        }

                        // Group rows: subgroup creation, rename, move, delete.
                        if let NavigatorNodeId::Group { id } = &row_id {
                            let row_group = id.clone();
                            let rename_group = id.clone();
                            let rename_label = row.label.clone();
                            let delete_group = id.clone();
                            return base
                                .context_menu(move |menu, window, cx| {
                                    let groups: Vec<(String, String)> = menu_root
                                        .update(cx, |this, _| {
                                            this.navigator
                                                .preferences()
                                                .groups
                                                .iter()
                                                .map(|group| (group.id.clone(), group.name.clone()))
                                                .collect()
                                        })
                                        .unwrap_or_default();
                                    let row_group = row_group.clone();
                                    let rename_group = rename_group.clone();
                                    let rename_label = rename_label.clone();
                                    let delete_group = delete_group.clone();
                                    let groups = groups.clone();
                                    let new_root = menu_root.clone();
                                    let select_root = menu_root.clone();
                                    let select_group = row_group.clone();
                                    let rename_root = menu_root.clone();
                                    let delete_root = menu_root.clone();
                                    let move_root = menu_root.clone();
                                    let parent = row_group.clone();
                                    menu.item(PopupMenuItem::new("New Subgroup").on_click(
                                        move |_, window, app| {
                                            let _ = new_root.update(app, |this, cx| {
                                                this.open_new_sidebar_group_dialog(
                                                    Some(parent.clone()),
                                                    window,
                                                    cx,
                                                );
                                            });
                                        },
                                    ))
                                    .item(PopupMenuItem::new("Select group connections").on_click(
                                        move |_, _, app| {
                                            let _ = select_root.update(app, |this, cx| {
                                                if this
                                                    .navigator
                                                    .action_target(&NavigatorNodeId::Group {
                                                        id: select_group.clone(),
                                                    })
                                                    .is_some()
                                                {
                                                    let _ = this.toggle_sidebar_group_connections(
                                                        &select_group,
                                                        cx,
                                                    );
                                                }
                                            });
                                        },
                                    ))
                                    .item(PopupMenuItem::new("Rename Group").on_click(
                                        move |_, window, app| {
                                            let _ = rename_root.update(app, |this, cx| {
                                                this.open_rename_sidebar_group_dialog(
                                                    rename_group.clone(),
                                                    rename_label.clone(),
                                                    window,
                                                    cx,
                                                );
                                            });
                                        },
                                    ))
                                    .submenu("Move to Group", window, cx, move |submenu, _, _| {
                                        let ungroup_root = move_root.clone();
                                        let moved = row_group.clone();
                                        let mut built =
                                            submenu.item(PopupMenuItem::new("Ungrouped").on_click(
                                                move |_, _, app| {
                                                    let _ = ungroup_root.update(app, |this, cx| {
                                                        let target = NavigatorNodeId::Group {
                                                            id: moved.clone(),
                                                        };
                                                        if this
                                                            .navigator
                                                            .action_target(&target)
                                                            .is_some()
                                                        {
                                                            let _ = this.move_sidebar_group(
                                                                &moved,
                                                                None,
                                                                usize::MAX,
                                                                cx,
                                                            );
                                                        }
                                                    });
                                                },
                                            ));
                                        for (group_id, name) in groups.clone() {
                                            if group_id == row_group {
                                                continue;
                                            }
                                            let item_root = move_root.clone();
                                            let moved = row_group.clone();
                                            built = built.item(PopupMenuItem::new(name).on_click(
                                                move |_, _, app| {
                                                    let _ = item_root.update(app, |this, cx| {
                                                        let target = NavigatorNodeId::Group {
                                                            id: moved.clone(),
                                                        };
                                                        if this
                                                            .navigator
                                                            .action_target(&target)
                                                            .is_some()
                                                        {
                                                            let _ = this.move_sidebar_group(
                                                                &moved,
                                                                Some(&group_id),
                                                                usize::MAX,
                                                                cx,
                                                            );
                                                        }
                                                    });
                                                },
                                            ));
                                        }
                                        built
                                    })
                                    .item(
                                        PopupMenuItem::new("Delete Group").on_click(
                                            move |_, window, app| {
                                                let _ = delete_root.update(app, |this, cx| {
                                                    this.confirm_delete_sidebar_group(
                                                        delete_group.clone(),
                                                        window,
                                                        cx,
                                                    );
                                                });
                                            },
                                        ),
                                    )
                                })
                                .into_any_element();
                        }

                        // Functions, triggers, sequences, types, events: View Source.
                        if let NavigatorNodeId::SchemaObject { .. } = &row_id {
                            let (source_root, source_row) = (menu_root.clone(), row.clone());
                            return base
                                .context_menu(move |menu, _, _| {
                                    let (source_root, source_row) =
                                        (source_root.clone(), source_row.clone());
                                    menu.item(PopupMenuItem::new("View Source").on_click(
                                        move |_, _, app| {
                                            let _ = source_root.update(app, |this, cx| {
                                                this.handle_navigator_open(&source_row, cx);
                                            });
                                        },
                                    ))
                                })
                                .into_any_element();
                        }

                        // The Tables group: New Table.
                        if let NavigatorNodeId::ObjectGroup { group, .. } = &row_id {
                            // The other groups have no action: no (empty) menu.
                            if *group != ObjectGroup::Tables {
                                return base.into_any_element();
                            }
                            let (group_root, group_id) = (menu_root.clone(), row_id.clone());
                            return base
                                .context_menu(move |menu, window, cx| {
                                    append_object_items(menu, group_root.clone(), &group_id, false, window, cx)
                                })
                                .into_any_element();
                        }

                        if !matches!(
                            row_id,
                            NavigatorNodeId::Connection { .. }
                                | NavigatorNodeId::Object { .. }
                                | NavigatorNodeId::Database { .. }
                                | NavigatorNodeId::Schema { .. }
                                | NavigatorNodeId::Provider { .. }
                        ) {
                            return base.into_any_element();
                        }

                        if !matches!(row_id, NavigatorNodeId::Connection { .. }) {
                            let query_id = row_id.clone();
                            let query_root = menu_root.clone();
                            // 'static builder: clone owned captures per call.
                            let new_query_item = move || {
                                let query_root = query_root.clone();
                                let query_id = query_id.clone();
                                PopupMenuItem::new("New Query")
                                    .disabled(!can_query)
                                    .on_click(move |_, _, app| {
                                        let _ = query_root.update(app, |this, cx| {
                                            let id = query_id.clone();
                                            if this.navigator.action_target(&id).is_some()
                                                && this
                                                    .navigator
                                                    .available_actions(&id, false)
                                                    .contains(&NavigatorAction::NewQuery)
                                            {
                                                this.new_query_for_sidebar_node(&id, cx);
                                            }
                                        });
                                    })
                            };
                            if let NavigatorNodeId::Object { .. } = &row_id {
                                let row_for_open = row.clone();
                                let row_for_copy = row.clone();
                                let (ask_root, ask_id) = (menu_root.clone(), row_id.clone());
                                // A materialized view or a sequence also has a definition to read.
                                let view_source = matches!(
                                    object_kind,
                                    Some(KindOfObject::MaterializedView | KindOfObject::Sequence)
                                )
                                    .then(|| (menu_root.clone(), row.clone()));
                                return base
                                    .on_action(cx.listener(
                                        move |this, _: &OpenNavigatorNode, _, cx| {
                                            this.handle_navigator_open(&row_for_open, cx);
                                        },
                                    ))
                                    .on_action(cx.listener(
                                        move |this, _: &CopyNavigatorSql, _, cx| {
                                            this.copy_navigator_sql(&row_for_copy, cx);
                                        },
                                    ))
                                    .context_menu(move |menu, window, cx| {
                                        let (ask_root, ask_id) = (ask_root.clone(), ask_id.clone());
                                        let menu = menu
                                            .menu("Open", Box::new(OpenNavigatorNode))
                                            .menu("Copy SQL", Box::new(CopyNavigatorSql));
                                        let menu = match view_source.clone() {
                                            Some((source_root, source_row)) => menu.item(
                                                PopupMenuItem::new("View Source").on_click(
                                                    move |_, _, app| {
                                                        let _ = source_root.update(app, |this, cx| {
                                                            this.open_schema_object_source(
                                                                &source_row.id,
                                                                &source_row.label,
                                                                cx,
                                                            );
                                                        });
                                                    },
                                                ),
                                            ),
                                            None => menu,
                                        };
                                        let menu = menu
                                            .separator()
                                            .item(new_query_item())
                                            .item(
                                                PopupMenuItem::new(
                                                    "Ask the assistant about this table",
                                                )
                                                .on_click({
                                                    let (ask_root, ask_id) = (ask_root.clone(), ask_id.clone());
                                                    move |_, window, app| {
                                                        let _ = ask_root.update(app, |this, cx| {
                                                            this.assistant_table_ask(
                                                                &ask_id, window, cx,
                                                            )
                                                        });
                                                    }
                                                }),
                                            );
                                        append_object_items(menu, ask_root, &ask_id, true, window, cx)
                                    })
                                    .into_any_element();
                            }
                            if let NavigatorNodeId::Provider { .. } = &row_id {
                                // Redis database / namespace / key rows.
                                if let Some(target) = this.redis_node_target(&row_id) {
                                    let target_root = menu_root.clone();
                                    return base
                                        .context_menu(move |menu, _, _| {
                                            super::redis::build_redis_node_menu(
                                                menu,
                                                target_root.clone(),
                                                target.clone(),
                                            )
                                        })
                                        .into_any_element();
                                }
                                // MongoDB database / collection rows: context menu
                                // with admin actions gated on driver capabilities.
                                if let Some((connection, database, aggregate_id)) =
                                    this.saved_aggregation_target(&row_id)
                                {
                                    let menu_root = menu_root.clone();
                                    return base
                                        .context_menu(move |menu, _, _| {
                                            super::aggregate::saved::build_saved_aggregate_menu(
                                                menu,
                                                menu_root.clone(),
                                                connection.clone(),
                                                database.clone(),
                                                aggregate_id.clone(),
                                            )
                                        })
                                        .into_any_element();
                                }
                                if let Some(target) = this.mongo_node_target(&row_id) {
                                    let target_root = menu_root.clone();
                                    return base
                                        .context_menu(move |menu, _, cx| {
                                            super::mongo_admin_view::build_mongo_node_menu(
                                                menu,
                                                target_root.clone(),
                                                target.clone(),
                                                cx,
                                            )
                                        })
                                        .into_any_element();
                                }
                                if !can_open && !can_query {
                                    return base.into_any_element();
                                }
                                let open_id = row_id.clone();
                                let open_root = menu_root.clone();
                                return base
                                    .context_menu(move |menu, _, _| {
                                        let open_id = open_id.clone();
                                        let open_root = open_root.clone();
                                        let mut built = menu;
                                        if can_open {
                                            built =
                                                built.item(PopupMenuItem::new("Open").on_click(
                                                    move |_, _, app| {
                                                        let _ =
                                                            open_root.update(app, |this, cx| {
                                                                let id = open_id.clone();
                                                                if let Some(index) = this
                                                                    .navigator
                                                                    .visible_index(&id)
                                                                {
                                                                    if let Some(live) = this
                                                                        .navigator
                                                                        .visible_row_at(index)
                                                                    {
                                                                        this.handle_navigator_click(
                                                                        &live, cx,
                                                                    );
                                                                    }
                                                                }
                                                            });
                                                    },
                                                ));
                                        }
                                        built.item(new_query_item())
                                    })
                                    .into_any_element();
                            }
                            let diagram_id = this.diagram_node_supported(&row_id).then(|| row_id.clone());
                            let diagram_root = menu_root.clone();
                            let object_id = row_id.clone();
                            return base
                                .context_menu(move |menu, window, cx| {
                                    let menu = menu.item(new_query_item());
                                    let menu = match diagram_id.clone() {
                                        Some(id) => {
                                            let root = diagram_root.clone();
                                            menu.item(PopupMenuItem::new("View Diagram").on_click(move |_, _, app| {
                                                let _ = root.update(app, |this, cx| {
                                                    this.open_diagram_for_node(&id, cx);
                                                });
                                            }))
                                        }
                                        None => menu,
                                    };
                                    append_object_items(menu, diagram_root.clone(), &object_id, true, window, cx)
                                })
                                .into_any_element();
                        }
                        let NavigatorNodeId::Connection { connection } = &row_id else {
                            return base.into_any_element();
                        };
                        let connection_id = connection.clone();
                        // Pin/hide/copy-config/export-config left this menu for the
                        // sidebar Import/Export toolbar and settings menu; the
                        // approved connection menu owns the rest (see
                        // connection_menu_view::build_connection_menu).
                        let can_connect = this.navigator.supports_action(
                            &row_id,
                            connected,
                            NavigatorAction::Connect,
                        );
                        let can_refresh = this.navigator.supports_action(
                            &row_id,
                            connected,
                            NavigatorAction::Refresh,
                        );
                        let row_for_refresh = row_for_disclosure.clone();
                        let row_for_connect = row_for_disclosure.clone();
                        let connection_for_disconnect = connection_id.clone();
                        let connection_for_pin = connection_id.clone();
                        let connection_for_copy = connection_id.clone();
                        let connection_for_edit = connection_id.clone();
                        let connection_for_delete = connection_id.clone();
                        let base = base
                            .on_action(cx.listener(move |this, _: &ConnectConnection, _, cx| {
                                let NavigatorNodeId::Connection { connection } =
                                    &row_for_connect.id
                                else {
                                    return;
                                };
                                if this.navigator.supports_action(
                                    &row_for_connect.id,
                                    matches!(
                                        this.sessions.status(connection),
                                        ConnectionStatus::Connected
                                    ),
                                    NavigatorAction::Connect,
                                ) {
                                    if let Some(row) = this
                                        .navigator
                                        .visible_index(&row_for_connect.id)
                                        .and_then(|index| this.navigator.visible_row_at(index))
                                    {
                                        this.handle_navigator_click(&row, cx);
                                    }
                                }
                            }))
                            .on_action(cx.listener(move |this, _: &DisconnectConnection, window, cx| {
                                let id = NavigatorNodeId::Connection {
                                    connection: connection_for_disconnect.clone(),
                                };
                                if this.navigator.supports_action(
                                    &id,
                                    matches!(
                                        this.sessions.status(&connection_for_disconnect),
                                        ConnectionStatus::Connected
                                    ),
                                    NavigatorAction::Disconnect,
                                ) {
                                    this.request_disconnect(&connection_for_disconnect, window, cx);
                                }
                            }))
                            .on_action(cx.listener(move |this, _: &RefreshConnection, _, cx| {
                                let NavigatorNodeId::Connection { connection } =
                                    &row_for_refresh.id
                                else {
                                    return;
                                };
                                if this.navigator.supports_action(
                                    &row_for_refresh.id,
                                    matches!(
                                        this.sessions.status(connection),
                                        ConnectionStatus::Connected
                                    ),
                                    NavigatorAction::Refresh,
                                ) {
                                    if let Some(row) = this
                                        .navigator
                                        .visible_index(&row_for_refresh.id)
                                        .and_then(|index| this.navigator.visible_row_at(index))
                                    {
                                        this.refresh_connection(&row, cx);
                                    }
                                }
                            }))
                            .on_action(cx.listener(move |this, _: &TogglePinConnection, _, cx| {
                                this.toggle_connection_pin(&connection_for_pin, cx);
                            }))
                            .on_action(cx.listener(move |this, _: &CopyConnectionString, _, cx| {
                                this.copy_connection_string(&connection_for_copy, cx);
                            }))
                            .on_action(cx.listener(move |this, _: &EditConnection, window, cx| {
                                let saved = this
                                    .saved_connections
                                    .iter()
                                    .find(|connection| connection.matches_id(&connection_for_edit))
                                    .cloned();
                                this.open_connection_dialog(saved, window, cx);
                            }))
                            .on_action(cx.listener(
                                move |this, _: &DeleteConnection, window, cx| {
                                    let id = NavigatorNodeId::Connection {
                                        connection: connection_for_delete.clone(),
                                    };
                                    if this.navigator.action_target(&id).is_none() {
                                        return;
                                    }
                                    if this.navigator.connection_multi_mode()
                                        && this.navigator.selected_ids().contains(&id)
                                    {
                                        this.confirm_delete_selected_connections(window, cx);
                                    } else {
                                        this.delete_connection_with_confirmation(
                                            connection_for_delete.clone(),
                                            window,
                                            cx,
                                        );
                                    }
                                },
                            ));
                        base.context_menu(move |menu, window, cx| {
                            let batch = menu_root
                                .update(cx, |this, _| {
                                    let id = NavigatorNodeId::Connection {
                                        connection: connection_id.clone(),
                                    };
                                    if this.navigator.selected_ids().contains(&id) {
                                        this.selected_sidebar_connection_ids().ok()
                                    } else {
                                        None
                                    }
                                })
                                .unwrap_or_default();
                            if let Some(ids) = batch {
                                return sidebar_batch_ui::append_selected_connection_menu(
                                    menu,
                                    menu_root.clone(),
                                    ids,
                                    window,
                                    cx,
                                );
                            }
                            // Approved connection menu (see connection_menu_view):
                            // extra legacy actions stay in the Import/Export
                            // toolbar and settings menu, never in this menu.
                            let live = menu_root
                                .update(cx, |this, _| {
                                    let id = NavigatorNodeId::Connection {
                                        connection: connection_id.clone(),
                                    };
                                    let status = this.sessions.status(&connection_id);
                                    (
                                        matches!(status, ConnectionStatus::Connected),
                                        matches!(status, ConnectionStatus::Connecting),
                                        this.navigator.supports_action(
                                            &id,
                                            connected,
                                            NavigatorAction::Connect,
                                        ),
                                        this.navigator.supports_action(
                                            &id,
                                            connected,
                                            NavigatorAction::Disconnect,
                                        ),
                                        this.navigator.supports_action(
                                            &id,
                                            connected,
                                            NavigatorAction::Refresh,
                                        ),
                                        this.navigator.supports_action(
                                            &id,
                                            connected,
                                            NavigatorAction::NewQuery,
                                        ) && (!matches!(status, ConnectionStatus::Connected)
                                            || this.saved_connections.iter().find(|saved| saved.matches_id(&connection_id))
                                                .is_none_or(|saved| saved.kind != ConnectionKind::Mongodb)
                                            || this.mongo_supports_script(&connection_id)),
                                    )
                                })
                                .unwrap_or((
                                    connected,
                                    false,
                                    can_connect,
                                    false,
                                    can_refresh,
                                    can_query,
                                ));
                            connection_menu_view::build_connection_menu(
                                menu,
                                menu_root.clone(),
                                connection_id.clone(),
                                live.0,
                                live.1,
                                live.2,
                                live.3,
                                live.4,
                                live.5,
                                window,
                                cx,
                            )
                        })
                        .into_any_element()
                    })
                    .collect::<Vec<_>>()
            }),
        )
        .flex_1()
        .min_h_0()
        .track_scroll(&self.sidebar.scroll);

        div()
            .flex()
            .flex_col()
            .size_full()
            .bg(theme::sidebar())
            .track_focus(&self.sidebar.focus)
            // A `.sql` file dragged in from the OS runs on the row it is dropped on (the container listens, not the rows).
            .on_drag_move(cx.listener(|this, event: &gpui::DragMoveEvent<gpui::ExternalPaths>, _, cx| this.file_drag_over_navigator(event, cx)))
            .on_file_drop_exit(cx.listener(|this, _, _, cx| this.file_drag_left(cx)))
            .on_drop(cx.listener(|this, paths: &gpui::ExternalPaths, window, cx| this.drop_files_on_navigator(paths, window, cx)))
            // Scopes the sidebar Cmd-C/Cmd-E/F5 bindings: editor and grid
            // inputs never match this context, so text-input Cmd-C is safe.
            .key_context("sidebar")
            // Selection-aware keyboard paths for the connection menu
            // shortcuts. The exact-handle focus gate keeps text inputs inside
            // the sidebar (search, dialogs) from being hijacked; menu clicks
            // use captured ids instead, so right-clicking a non-selected row
            // still acts on that row.
            .on_action(cx.listener(|this, _: &CopyConnectionName, window, cx| {
                if !this.sidebar.focus.is_focused(window) {
                    return;
                }
                match this.navigator.selected().cloned() {
                    Some(NavigatorNodeId::Connection { connection }) => {
                        this.copy_connection_name(&connection, cx);
                    }
                    // MongoDB database / collection rows copy their own name.
                    Some(id @ NavigatorNodeId::Provider { .. }) => {
                        this.copy_mongo_node_name(&id, cx);
                    }
                    _ => {}
                }
            }))
            .on_action(cx.listener(|this, _: &EditConnection, window, cx| {
                if !this.sidebar.focus.is_focused(window) {
                    return;
                }
                if let Some(NavigatorNodeId::Connection { connection }) =
                    this.navigator.selected().cloned()
                {
                    let saved = this
                        .saved_connections
                        .iter()
                        .find(|saved| saved.matches_id(&connection))
                        .cloned();
                    this.open_connection_dialog(saved, window, cx);
                }
            }))
            .on_action(cx.listener(|this, _: &RefreshConnection, window, cx| {
                if !this.sidebar.focus.is_focused(window) {
                    return;
                }
                let Some(NavigatorNodeId::Connection { connection }) =
                    this.navigator.selected().cloned()
                else {
                    return;
                };
                let id = NavigatorNodeId::Connection {
                    connection: connection.clone(),
                };
                if !this.navigator.supports_action(
                    &id,
                    matches!(
                        this.sessions.status(&connection),
                        ConnectionStatus::Connected
                    ),
                    NavigatorAction::Refresh,
                ) {
                    return;
                }
                if let Some(row) = this
                    .navigator
                    .visible_index(&id)
                    .and_then(|index| this.navigator.visible_row_at(index))
                {
                    this.refresh_connection(&row, cx);
                }
            }))
            .on_key_down(cx.listener(|this, event: &gpui::KeyDownEvent, window, cx| {
                if this.handle_sidebar_key(event, window, cx) {
                    cx.stop_propagation();
                }
            }))
            .child(toolbar_transfer)
            .child(search_bar)
            .children(
                self.sidebar
                    .error
                    .iter()
                    .map(|error| (error, theme::danger()))
                    .chain(
                        self.sidebar
                            .locate_error
                            .iter()
                            .map(|error| (error, theme::danger())),
                    )
                    .chain(
                        self.sidebar
                            .notice
                            .iter()
                            .map(|notice| (notice, theme::text_muted())),
                    )
                    .chain(
                        self.remote_truncated_notice()
                            .iter()
                            .map(|notice| (notice, theme::text_muted())),
                    )
                    .map(|(text, color)| {
                        div()
                            .px(px(10.0))
                            .py(px(4.0))
                            .text_size(px(11.0))
                            .text_color(color)
                            .child(text.clone())
                    }),
            )
            .child(list)
            .into_any_element()
    }

    pub(super) fn apply_navigator_load(
        &mut self,
        request: &LoadRequest,
        generation: u64,
        children: Result<Vec<navigator::LoadedChild>, String>,
        cx: &mut Context<Self>,
    ) {
        let children = children.map_err(|error| crate::sessions::redact_secrets(&error));
        if let Err(error) = &children {
            self.report_error(Area::Navigator, Some(&request.connection_id()), error);
            self.note_navigator_connection_lost(&request.connection_id(), error);
        }
        self.navigator.apply_load(request, generation, children);
        self.navigator_ui.generation = self.navigator.generation();
        self.continue_sidebar_locate(cx);
        self.pump_remote_search(cx);
        self.continue_schema_diagrams(cx);
    }

    /// Loads one level of a driver session's catalog: the typed SQL tree for
    /// SQL kinds, provider nodes for the rest (MongoDB).
    pub(super) fn dispatch_provider_load(&mut self, request: LoadRequest, cx: &mut Context<Self>) {
        if self.scenario_ui.service.is_some() {
            return;
        }
        if !matches!(request, LoadRequest::Provider { .. }) {
            self.dispatch_sql_driver_load(request, cx);
            return;
        }
        let connection = request.connection_id();
        let generation = match self.navigator.load_generation(&request) {
            Some(generation) => generation,
            None => return,
        };
        // Collections list their saved aggregations from local storage.
        if let Some(page) = self.local_aggregation_page(&request) {
            self.navigator.apply_provider_page(&request, generation, Ok(page));
            self.navigator_ui.generation = self.navigator.generation();
            cx.notify();
            return;
        }
        let Some(session) = self.driver_runtime.sessions.get(&connection).cloned() else {
            self.report_error(Area::Navigator, Some(&connection), "driver session is not connected");
            self.navigator.apply_provider_page(
                &request,
                generation,
                Err("driver session is not connected".into()),
            );
            self.navigator_ui.generation = self.navigator.generation();
            cx.notify();
            return;
        };
        let driver_request = navigator_request_from_load(&request);
        let task = cx.background_executor().spawn(async move {
            smol::unblock(move || list_driver_nav_nodes(&session, &driver_request)).await
        });
        let task = cx.spawn(async move |this, cx| {
            let result = task.await.map_err(|error| error.to_string());
            let _ = this.update(cx, |this, cx| {
                let loaded = result.is_ok();
                if let Err(error) = &result {
                    this.report_error(Area::Navigator, Some(&request.connection_id()), error);
                    this.note_navigator_connection_lost(&request.connection_id(), error);
                }
                this.navigator.apply_provider_page(
                    &request,
                    generation,
                    result.map_err(|error| crate::sessions::redact_secrets(&error)),
                );
                this.navigator_ui.generation = this.navigator.generation();
                if loaded {
                    this.after_provider_collections_loaded(&request, cx);
                }
                this.continue_sidebar_locate(cx);
                cx.notify();
            });
        });
        task.detach();
    }

    /// Loads one level of a SQL driver's catalog into the typed SQL tree. The session lock is taken per driver call on
    /// a blocking thread (never the UI thread), and the driver answers from its
    /// own metadata connection, so a running query does not delay it.
    fn dispatch_sql_driver_load(&mut self, request: LoadRequest, cx: &mut Context<Self>) {
        let connection = request.connection_id();
        let Some(generation) = self.navigator.load_generation(&request) else {
            return;
        };
        let Some(session) = self.driver_runtime.sessions.get(&connection).cloned() else {
            self.apply_navigator_load(
                &request,
                generation,
                Err("driver session is not connected".into()),
                cx,
            );
            cx.notify();
            return;
        };
        let filter = match request {
            LoadRequest::Objects { .. } => self.group_filter_for(&request),
            _ => None,
        };
        let supports_schemas = self.sql_supports_schemas(&connection);
        let Some(driver_request) =
            navigator::sql_driver::catalog_request(&request, supports_schemas, filter.as_deref())
        else {
            // A driver without a schema level: its one schema is the database.
            let children = match &request {
                LoadRequest::Schemas {
                    connection,
                    database,
                } => vec![navigator::schema_child(connection, database, database)],
                _ => Vec::new(),
            };
            self.apply_navigator_load(&request, generation, Ok(children), cx);
            cx.notify();
            return;
        };
        let single_page = filter.is_some();
        let task = cx.background_executor().spawn(async move {
            smol::unblock(move || {
                navigator::sql_driver::collect_pages(driver_request, single_page, |request| {
                    list_driver_nav_nodes(&session, request)
                })
            })
            .await
        });
        cx.spawn(async move |this, cx| {
            let result = task.await;
            let _ = this.update(cx, |this, cx| {
                let children = result
                    .map(|nodes| navigator::sql_driver::catalog_children(&request, &nodes))
                    .map_err(|error| error.to_string());
                this.apply_navigator_load(&request, generation, children, cx);
                cx.notify();
            });
        })
        .detach();
    }

    /// Whether the SQL driver behind `connection` has a schema level
    /// (its advertised dialect; PostgreSQL's when it advertises none).
    pub(super) fn sql_supports_schemas(&self, connection: &str) -> bool {
        let Some(kind) = self
            .saved_connections
            .iter()
            .find(|saved| saved.matches_id(connection))
            .map(|saved| saved.kind)
        else {
            return false;
        };
        self.driver_capabilities(connection, kind.driver_id())
            .and_then(|capabilities| capabilities.sql_dialect.as_ref())
            .map_or(kind == crate::connections::ConnectionKind::Postgres, |dialect| {
                dialect.supports_schemas
            })
    }

    /// A navigator call said the driver session is gone (`*.not_connected`):
    /// the connection flips to its reconnect state, like a failed query does.
    /// The dead session is released by the next connect attempt.
    pub(super) fn note_navigator_connection_lost(&mut self, connection: &str, error: &str) {
        if !(crate::sessions::is_connection_lost_error(error)
            || self.tunnel_failure(connection).is_some())
            || !self.driver_runtime.sessions.contains_key(connection)
        {
            return;
        }
        if self.mark_connection_lost(connection) {
            self.sync_connected_ids();
        }
    }

    pub(super) fn refresh_connection(&mut self, row: &NavigatorRow, cx: &mut Context<Self>) {
        if self.scenario_ui.service.is_some() || self.navigator.action_target(&row.id).is_none() {
            return;
        }
        self.navigator.reset_node(&row.id);
        self.invalidate_table_properties(&row.id);
        self.handle_navigator_disclosure(row, cx);
    }

    pub(super) fn collapse_navigator(&mut self, cx: &mut Context<Self>) {
        self.navigator.collapse_all();
        self.notify_chrome(cx);
    }

    pub(super) fn copy_navigator_sql(&self, row: &NavigatorRow, cx: &mut Context<Self>) {
        let Some(sql) = self.navigator.preview_sql(&row.id) else {
            return;
        };
        cx.write_to_clipboard(gpui::ClipboardItem::new_string(sql));
    }
}
