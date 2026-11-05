use super::*;
use crate::navigator::{NavigatorAction, SearchOptions};
use crate::sidebar_preferences::{ConnectionSort, SidebarPreferences};
use std::path::PathBuf;

pub(super) struct SidebarState {
    pub(super) path: Option<PathBuf>,
    pub(super) error: Option<String>,
    writes_blocked: bool,
    pub(super) search_error: Option<String>,
    pub(super) notice: Option<String>,
    pub(super) locate_error: Option<String>,
    pending_locate: Option<PendingLocate>,
    pub(super) search_input: Option<gpui::Entity<InputState>>,
    pub(super) remote: super::remote_search::RemoteSearch,
    pub(super) scroll: UniformListScrollHandle,
    pub(super) focus: FocusHandle,
    colors: HashMap<String, String>,
    pub(super) color_overrides: HashMap<String, String>,
}

impl SidebarState {
    pub(super) fn new(
        scenario: bool,
        navigator: &mut NavigatorState,
        cx: &mut Context<AppRoot>,
    ) -> Self {
        let mut state = Self {
            path: None,
            error: None,
            writes_blocked: false,
            search_error: None,
            notice: None,
            locate_error: None,
            pending_locate: None,
            search_input: None,
            remote: Default::default(),
            scroll: UniformListScrollHandle::new(),
            focus: cx.focus_handle(),
            colors: HashMap::new(),
            color_overrides: HashMap::new(),
        };
        if scenario {
            // Deterministic screenshot seed (capture only): distinct
            // per-connection colors so the connection-tree capture shows
            // tinted subtrees without touching real stored
            // connections or preferences. Enabled only when the capture
            // launcher sets TABLEX_SEED_CONNECTION_TREE=1; unit tests never
            // see it, so async locate/disclosure tests keep a collapsed tree.
            // The seed applies as a FALLBACK after the isolated prefs file
            // loads below, so explicit file colors always win.
            let seed_colors = std::env::var("TABLEX_SEED_CONNECTION_TREE").as_deref() == Ok("1");
            // Isolated preference file for native interaction proofs
            // (group/drop/hide/sort in the capture scenario). The launcher
            // sets TABLEX_SIDEBAR_PREFS_PATH to a temp-dir file; anything
            // else is refused so scenario runs can never touch the user's
            // real preferences. Without it, mutations report unavailable.
            if let Some(path) = scenario_prefs_path() {
                state.path = Some(path.clone());
                match SidebarPreferences::load(&path) {
                    Ok(preferences) => {
                        if let Err(error) = navigator.set_preferences(preferences) {
                            state.error = Some(error);
                            state.writes_blocked = true;
                        }
                    }
                    Err(error) => {
                        state.error = Some(format!("Could not read sidebar preferences: {error}"));
                        state.writes_blocked = true;
                    }
                }
                state.refresh_overrides(navigator.preferences());
            } else if std::env::var_os("TABLEX_SIDEBAR_PREFS_PATH").is_some() {
                state.notice =
                    Some("Scenario preference path must be an absolute temp-dir file".into());
            }
            if seed_colors {
                state.seed_scenario_colors_as_fallback();
            }
            return state;
        }
        match SidebarPreferences::default_path() {
            Ok(path) => {
                state.path = Some(path.clone());
                match SidebarPreferences::load(&path) {
                    Ok(preferences) => {
                        if let Err(error) = navigator.set_preferences(preferences) {
                            state.error = Some(error);
                            state.writes_blocked = true;
                        }
                    }
                    Err(error) => {
                        state.error = Some(format!("Could not read sidebar preferences: {error}"));
                        state.writes_blocked = true;
                    }
                }
            }
            Err(error) => {
                state.error = Some(format!("Could not locate sidebar preferences: {error}"));
                state.writes_blocked = true;
            }
        }
        state.refresh_overrides(navigator.preferences());
        state.reload_colors();
        state
    }

    pub(super) fn reload_colors(&mut self) {
        match engine_storage::connection::list_connections() {
            Ok(records) => {
                self.colors = records
                    .into_iter()
                    .filter_map(|record| {
                        uuid::Uuid::parse_str(&record.id)
                            .ok()
                            .map(|id| (id.to_string(), record.color_hex))
                    })
                    .collect();
            }
            Err(error) => self.notice = Some(format!("Could not read connection colors: {error}")),
        }
    }

    pub(super) fn refresh_overrides(&mut self, preferences: &SidebarPreferences) {
        self.color_overrides = preferences
            .connections
            .iter()
            .filter_map(|display| {
                display
                    .color
                    .as_ref()
                    .map(|color| (display.id.clone(), color.clone()))
            })
            .collect();
    }

    #[cfg(test)]
    pub(super) fn seed_scenario_colors(&mut self) {
        self.color_overrides = [
            (
                crate::ui_fixture::MONGO_ID.to_string(),
                "#34C759".to_string(),
            ),
            (
                crate::ui_fixture::POSTGRES_ID.to_string(),
                "#336791".to_string(),
            ),
            (
                crate::ui_fixture::SQLITE_ID.to_string(),
                "#0F80CC".to_string(),
            ),
            (
                crate::ui_fixture::CLICKHOUSE_ID.to_string(),
                "#FFCC01".to_string(),
            ),
        ]
        .into_iter()
        .collect();
    }

    /// Capture-only fallback: fills seeded display colors without
    /// overwriting explicit file colors loaded from preferences.
    pub(super) fn seed_scenario_colors_as_fallback(&mut self) {
        for (id, color) in [
            (
                crate::ui_fixture::MONGO_ID.to_string(),
                "#34C759".to_string(),
            ),
            (
                crate::ui_fixture::POSTGRES_ID.to_string(),
                "#336791".to_string(),
            ),
            (
                crate::ui_fixture::SQLITE_ID.to_string(),
                "#0F80CC".to_string(),
            ),
            (
                crate::ui_fixture::CLICKHOUSE_ID.to_string(),
                "#FFCC01".to_string(),
            ),
        ] {
            self.color_overrides.entry(id).or_insert(color);
        }
    }

    pub(super) fn color(&self, id: &str) -> Option<&str> {
        self.color_overrides
            .get(id)
            .or_else(|| self.colors.get(id))
            .map(String::as_str)
    }
}

/// Isolated preference file for scenario interaction proofs. Returns the
/// launcher-provided path only when it is an absolute temp-dir file, so
/// scenario runs can never touch real user preferences.
pub(super) fn scenario_prefs_path() -> Option<std::path::PathBuf> {
    std::env::var("TABLEX_SIDEBAR_PREFS_PATH")
        .ok()
        .and_then(|raw| scenario_prefs_path_from(&raw))
}

pub(super) fn scenario_prefs_path_from(raw: &str) -> Option<std::path::PathBuf> {
    let path = std::path::PathBuf::from(raw);
    if !path.is_absolute() || !path.starts_with(std::env::temp_dir()) || path.file_name().is_none()
    {
        return None;
    }
    let temp = std::fs::canonicalize(std::env::temp_dir()).ok()?;
    let parent = std::fs::canonicalize(path.parent()?).ok()?;
    if !parent.starts_with(&temp) || (parent == temp && path.parent()? != std::env::temp_dir()) {
        return None;
    }
    match std::fs::symlink_metadata(&path) {
        Ok(metadata) if !metadata.file_type().is_file() => None,
        Err(error) if error.kind() != std::io::ErrorKind::NotFound => None,
        _ => Some(path),
    }
}

struct PendingLocate {
    /// Active tab this locate belongs to (dropped when it changes); `None`
    /// for node reveals that do not depend on the active tab.
    tab_id: Option<u64>,
    path: Vec<NavigatorNodeId>,
    mongo: Option<(String, String)>,
    pages: u8,
    /// Expand the target itself once it is selected (palette reveal).
    expand_target: bool,
}

impl AppRoot {
    pub(super) fn locate_active_sidebar_tab(&mut self, cx: &mut Context<Self>) {
        self.sidebar.pending_locate = None;
        self.sidebar.locate_error = None;
        let Some(tab) = self.workspace.active_tab() else {
            return;
        };
        if tab.connection_id.is_empty()
            || !self
                .saved_connections
                .iter()
                .any(|saved| saved.matches_id(&tab.connection_id))
        {
            self.sidebar.locate_error = Some("Active tab has no saved connection".into());
            self.notify_chrome(cx);
            return;
        }
        let connection = tab.connection_id.clone();
        let mut path = vec![NavigatorNodeId::Connection {
            connection: connection.clone(),
        }];
        let mongo = if tab.query_mode.is_mongo_find() {
            Some((tab.database.clone(), tab.collection.clone()))
        } else {
            None
        };
        if tab.query_mode.is_mongo_find()
            && (tab.database.is_empty() || tab.collection.is_empty())
        {
            self.sidebar.locate_error =
                Some("MongoDB tab has no database and collection target".into());
            self.notify_chrome(cx);
            return;
        }
        if mongo.is_none() && !tab.database.is_empty() {
            path.push(NavigatorNodeId::Database {
                connection: connection.clone(),
                database: tab.database.clone(),
            });
            if let Some(schema) = tab.schema.as_ref() {
                path.push(NavigatorNodeId::Schema {
                    connection: connection.clone(),
                    database: tab.database.clone(),
                    schema: schema.clone(),
                });
                if !tab.collection.is_empty() {
                    path.push(NavigatorNodeId::ObjectGroup {
                        connection: connection.clone(),
                        database: tab.database.clone(),
                        schema: schema.clone(),
                        group: ObjectGroup::Tables,
                    });
                    path.push(NavigatorNodeId::Object {
                        connection,
                        database: tab.database.clone(),
                        schema: schema.clone(),
                        object: tab.collection.clone(),
                    });
                }
            } else if !tab.collection.is_empty() {
                self.sidebar.locate_error =
                    Some("Cannot locate SQL object without a schema".into());
                self.notify_chrome(cx);
                return;
            }
        }
        self.sidebar.pending_locate = Some(PendingLocate {
            tab_id: Some(tab.id),
            path,
            mongo,
            pages: 0,
            expand_target: false,
        });
        self.continue_sidebar_locate(cx);
    }

    /// Command-palette navigation: reveals a database / schema (or Mongo
    /// database) node in the sidebar through the same load path as "Reveal
    /// Active Tab": connects when needed, expands the ancestors and the node
    /// itself, selects it and scrolls it into view.
    pub(super) fn reveal_node_in_sidebar(&mut self, id: &NavigatorNodeId, cx: &mut Context<Self>) {
        self.sidebar.pending_locate = None;
        self.sidebar.locate_error = None;
        let (connection, path, mongo) = match id {
            NavigatorNodeId::Database { connection, .. } => (
                connection,
                vec![NavigatorNodeId::Connection { connection: connection.clone() }, id.clone()],
                None,
            ),
            NavigatorNodeId::Schema {
                connection,
                database,
                ..
            } => (
                connection,
                vec![
                    NavigatorNodeId::Connection { connection: connection.clone() },
                    NavigatorNodeId::Database {
                        connection: connection.clone(),
                        database: database.clone(),
                    },
                    id.clone(),
                ],
                None,
            ),
            NavigatorNodeId::Provider { connection, .. } => {
                let database = self
                    .navigator
                    .provider_node(id)
                    .filter(|provider| provider.kind == "database")
                    .and_then(|provider| {
                        provider.payload.get("database")?.as_str().map(str::to_string)
                    });
                let Some(database) = database else {
                    return;
                };
                (
                    connection,
                    vec![NavigatorNodeId::Connection { connection: connection.clone() }],
                    // Empty collection: stop at the database node.
                    Some((database, String::new())),
                )
            }
            _ => return,
        };
        if !self.saved_connections.iter().any(|saved| saved.matches_id(connection)) {
            return;
        }
        self.shell.sidebar_collapsed = false;
        self.sidebar.pending_locate = Some(PendingLocate {
            tab_id: None,
            path,
            mongo,
            pages: 0,
            expand_target: true,
        });
        self.continue_sidebar_locate(cx);
    }

    /// Reveals and selects the last node of `path` (the nodes from the connection down): what an
    /// object action created or renamed. Collapsed ancestors are expanded and a group that is
    /// loading is waited for, as for "Reveal Active Tab"; a target that never shows leaves the
    /// locate error in the sidebar.
    pub(super) fn reveal_path_in_sidebar(&mut self, path: Vec<NavigatorNodeId>, cx: &mut Context<Self>) {
        self.sidebar.pending_locate = None;
        self.sidebar.locate_error = None;
        if path.is_empty() {
            return;
        }
        self.sidebar.pending_locate = Some(PendingLocate {
            tab_id: None,
            path,
            mongo: None,
            pages: 0,
            expand_target: false,
        });
        self.continue_sidebar_locate(cx);
    }

    pub(super) fn continue_sidebar_locate(&mut self, cx: &mut Context<Self>) {
        let Some(mut pending) = self.sidebar.pending_locate.take() else {
            return;
        };
        if pending
            .tab_id
            .is_some_and(|id| self.workspace.active_tab().map(|tab| tab.id) != Some(id))
        {
            return;
        }
        let connection = match &pending.path[0] {
            NavigatorNodeId::Connection { connection } => connection.clone(),
            _ => return,
        };
        self.navigator.reveal_connection_group(&connection);
        if pending.mongo.is_some() {
            self.continue_mongo_sidebar_locate(pending, cx);
            return;
        }
        let mut index = 0;
        while index < pending.path.len() {
            if self.navigator.visible_index(&pending.path[index]).is_some() {
                index += 1;
                continue;
            }
            if index == 0 {
                self.sidebar.locate_error =
                    Some("Connection is hidden or filtered from the sidebar".into());
                self.notify_chrome(cx);
                return;
            }
            let parent = &pending.path[index - 1];
            let Some(row) = self
                .navigator
                .visible_index(parent)
                .and_then(|index| self.navigator.visible_row_at(index))
            else {
                self.sidebar.locate_error =
                    Some("Clear sidebar search to locate the active tab".into());
                self.notify_chrome(cx);
                return;
            };
            if let Some(crate::navigator::NodeLoadState::Failed(error)) = row.load.as_ref() {
                self.sidebar.locate_error = Some(format!("Could not locate tab: {error}"));
                self.notify_chrome(cx);
                return;
            }
            if !row.expanded {
                let request = self.navigator.toggle_expansion(parent).request;
                self.sidebar.pending_locate = Some(pending);
                if let Some(request) = request {
                    self.ensure_connected_then_load(request, cx);
                } else {
                    self.continue_sidebar_locate(cx);
                }
                return;
            }
            if matches!(row.load, Some(crate::navigator::NodeLoadState::Loading)) {
                self.sidebar.pending_locate = Some(pending);
                return;
            }
            // A table-like target is in the next of the groups that hold tables, views
            // and materialized views.
            let next_group = match parent {
                NavigatorNodeId::ObjectGroup { connection, group, .. } => {
                    let kind = self.navigator.connection(connection).map(|saved| saved.kind);
                    group.next_with_data(kind).map(|next| (connection, next))
                }
                _ => None,
            };
            if let (
                Some((connection, group)),
                NavigatorNodeId::ObjectGroup { database, schema, .. },
            ) = (next_group, parent)
            {
                pending.path[index - 1] = NavigatorNodeId::ObjectGroup {
                    connection: connection.clone(),
                    database: database.clone(),
                    schema: schema.clone(),
                    group,
                };
                self.sidebar.pending_locate = Some(pending);
                self.continue_sidebar_locate(cx);
                return;
            }
            self.sidebar.locate_error = Some(
                "Target is not in the loaded catalog; refresh the connection and retry".into(),
            );
            self.notify_chrome(cx);
            return;
        }
        if let Some(target) = pending.path.last() {
            self.select_sidebar_locate_target(target, pending.expand_target, cx);
        }
    }

    fn select_sidebar_locate_target(
        &mut self,
        id: &NavigatorNodeId,
        expand: bool,
        cx: &mut Context<Self>,
    ) {
        if self.navigator.select_visible(id, false, false) {
            if let Some(index) = self.navigator.visible_index(id) {
                self.sidebar
                    .scroll
                    .scroll_to_item(index, gpui::ScrollStrategy::Nearest);
            }
            let collapsed = expand
                && self
                    .navigator
                    .visible_index(id)
                    .and_then(|index| self.navigator.visible_row_at(index))
                    .is_some_and(|row| !row.expanded);
            if collapsed {
                if let Some(request) = self.navigator.toggle_expansion(id).request {
                    self.ensure_connected_then_load(request, cx);
                }
            }
            self.notify_chrome(cx);
        } else {
            self.sidebar.locate_error =
                Some("Target is hidden by the current sidebar filter".into());
            self.notify_chrome(cx);
        }
    }

    fn continue_mongo_sidebar_locate(
        &mut self,
        mut pending: PendingLocate,
        cx: &mut Context<Self>,
    ) {
        let (database, collection) = pending.mongo.clone().expect("Mongo locate target");
        let mut parent = pending.path[0].clone();
        for (kind, collection_name) in [
            ("database", None),
            ("collection", Some(collection.as_str())),
        ] {
            // Database-only reveal (palette): stop at the database node.
            if collection_name.is_some_and(str::is_empty) {
                break;
            }
            let Some(row) = self
                .navigator
                .visible_index(&parent)
                .and_then(|index| self.navigator.visible_row_at(index))
            else {
                self.sidebar.locate_error =
                    Some("Target is hidden by the current sidebar filter".into());
                self.notify_chrome(cx);
                return;
            };
            if let Some(crate::navigator::NodeLoadState::Failed(error)) = row.load.as_ref() {
                self.sidebar.locate_error = Some(format!("Could not locate tab: {error}"));
                self.notify_chrome(cx);
                return;
            }
            if !row.expanded {
                let request = self.navigator.toggle_expansion(&parent).request;
                if let Some(request) = request {
                    self.sidebar.pending_locate = Some(pending);
                    self.ensure_connected_then_load(request, cx);
                } else if self
                    .navigator
                    .visible_index(&parent)
                    .and_then(|index| self.navigator.visible_row_at(index))
                    .is_some_and(|row| row.expanded)
                {
                    self.sidebar.pending_locate = Some(pending);
                    self.continue_sidebar_locate(cx);
                } else {
                    self.sidebar.locate_error = Some("Provider node cannot be expanded".into());
                    self.notify_chrome(cx);
                }
                return;
            }
            if matches!(row.load, Some(crate::navigator::NodeLoadState::Loading)) {
                self.sidebar.pending_locate = Some(pending);
                return;
            }
            if let Some(child) =
                self.navigator
                    .find_provider_child(&parent, kind, &database, collection_name)
            {
                parent = child;
                continue;
            }
            if self.navigator.has_provider_next_page(&parent) && pending.pages < 32 {
                pending.pages += 1;
                let request = self.navigator.load_more(&parent);
                self.sidebar.pending_locate = Some(pending);
                if let Some(request) = request {
                    self.ensure_connected_then_load(request, cx);
                }
                return;
            }
            self.sidebar.locate_error = Some(if pending.pages >= 32 {
                "Locate stopped after 32 provider pages; narrow the target or browse manually"
                    .into()
            } else {
                "Target is not in the loaded provider catalog".into()
            });
            self.notify_chrome(cx);
            return;
        }
        self.select_sidebar_locate_target(&parent, pending.expand_target, cx);
    }

    /// The record a duplicate is stored as, with its new id: a copy of the connection that starts
    /// without credentials, with the tunnel's secrets gone and with agent access `off`.
    pub(super) fn duplicated_record(
        record: &engine_storage::connection::SavedConnectionRecord,
    ) -> Result<(String, engine_storage::connection::SavedConnectionRecord), String> {
        let id = uuid::Uuid::new_v4().to_string();
        let copy = crate::connections::duplicate_record(record, &id, current_time())?;
        Ok((id, copy))
    }

    pub(super) fn duplicate_sidebar_connection(
        &mut self,
        id: &str,
        cx: &mut Context<Self>,
    ) -> Result<String, String> {
        let result = (|| {
            if self.scenario_ui.service.is_some() {
                return Err("Cannot duplicate connections in a UI scenario".into());
            }
            let storage_id = crate::connections::storage_connection_id(id);
            let record = engine_storage::connection::get_connection(&storage_id)
                .map_err(|error| format!("Could not read connection: {error}"))?
                .ok_or("Saved connection not found")?;
            let (id, copy) = Self::duplicated_record(&record)?;
            engine_storage::connection::upsert_connection(&copy)
                .map_err(|error| format!("Could not duplicate connection: {error}"))?;
            self.refresh_saved_connections(cx);
            if self.sidebar.notice.is_none() {
                self.sidebar.notice = Some("Duplicated redacted connection; SSH/proxy disabled and credentials omitted. Review options before connecting".into());
            }
            Ok::<_, String>(id)
        })();
        if let Err(error) = &result {
            self.report_error(Area::Connect, None, error);
            self.sidebar.notice = Some(error.clone());
            cx.notify();
        }
        result
    }

    pub(super) fn sidebar_color(&self, id: &str) -> Option<&str> {
        self.sidebar.color(id)
    }

    #[cfg(test)]
    pub(super) fn reload_sidebar_preferences(
        &mut self,
        cx: &mut Context<Self>,
    ) -> Result<(), String> {
        let result = (|| {
            let path = self
                .sidebar
                .path
                .as_ref()
                .ok_or("Sidebar preferences path is unavailable")?;
            let preferences = SidebarPreferences::load(path)
                .map_err(|error| format!("Could not read sidebar preferences: {error}"))?;
            self.navigator.set_preferences(preferences)
        })();
        self.sidebar.error = result.as_ref().err().cloned();
        if result.is_ok() {
            self.sidebar.refresh_overrides(self.navigator.preferences());
        }
        self.sidebar.writes_blocked = result.is_err();
        if result.is_ok() {
            self.sidebar.notice = None;
        }
        cx.notify();
        result
    }

    pub(super) fn change_sidebar_preferences(
        &mut self,
        change: impl FnOnce(&mut SidebarPreferences) -> Result<(), String>,
        cx: &mut Context<Self>,
    ) -> Result<(), String> {
        if self.sidebar.writes_blocked {
            return Err(self
                .sidebar
                .error
                .clone()
                .unwrap_or_else(|| "Resolve the sidebar preferences error before saving".into()));
        }
        let result = (|| {
            let path = self
                .sidebar
                .path
                .as_ref()
                .ok_or("Sidebar preferences path is unavailable")?;
            let mut candidate = self.navigator.preferences().clone();
            change(&mut candidate)?;
            candidate
                .save(path)
                .map_err(|error| format!("Could not save sidebar preferences: {error}"))?;
            self.navigator.set_preferences(candidate)
        })();
        self.sidebar.error = result.as_ref().err().cloned();
        if result.is_ok() {
            self.sidebar.refresh_overrides(self.navigator.preferences());
            // Sort, search scope and tab-bar defaults are also settings.
            self.mirror_sidebar_into_settings(cx);
        }
        cx.notify();
        result
    }

    /// Persists the drag-end sidebar width (persisted on mouse-up only).
    /// Without a preference file (plain scenario runs) the width still
    /// applies in memory and nothing is reported.
    pub(super) fn persist_sidebar_width(&mut self, width: f32, cx: &mut Context<Self>) {
        if self.sidebar.path.is_none() {
            return;
        }
        let _ = self.change_sidebar_preferences(
            |prefs| {
                prefs.set_sidebar_width(width);
                Ok(())
            },
            cx,
        );
    }

    pub(super) fn create_sidebar_group(
        &mut self,
        name: &str,
        parent: Option<&str>,
        cx: &mut Context<Self>,
    ) -> Result<String, String> {
        let id = uuid::Uuid::new_v4().to_string();
        self.change_sidebar_preferences(|prefs| prefs.add_group(&id, name, parent), cx)?;
        Ok(id)
    }

    pub(super) fn rename_sidebar_group(
        &mut self,
        id: &str,
        name: &str,
        cx: &mut Context<Self>,
    ) -> Result<(), String> {
        self.change_sidebar_preferences(|prefs| prefs.rename_group(id, name), cx)
    }

    pub(super) fn move_sidebar_group(
        &mut self,
        id: &str,
        parent: Option<&str>,
        position: usize,
        cx: &mut Context<Self>,
    ) -> Result<(), String> {
        self.change_sidebar_preferences(|prefs| prefs.move_group(id, parent, position), cx)
    }

    pub(super) fn delete_sidebar_group(
        &mut self,
        id: &str,
        cx: &mut Context<Self>,
    ) -> Result<(), String> {
        self.change_sidebar_preferences(|prefs| prefs.remove_group(id), cx)
    }

    pub(super) fn confirm_delete_sidebar_group(
        &mut self,
        id: String,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(group) = self
            .navigator
            .preferences()
            .groups
            .iter()
            .find(|group| group.id == id)
        else {
            return;
        };
        let title = group.name.clone();
        let root = cx.entity().downgrade();
        window.open_alert_dialog(cx, move |dialog, _, _| {
            let root = root.clone();
            let id = id.clone();
            dialog
                .title("Delete Group")
                .child(format!(
                    "Delete {title}? Its subgroups and connections will move to the parent group."
                ))
                .button_props(
                    gpui_component::dialog::DialogButtonProps::default().ok_text("Delete"),
                )
                .confirm()
                .on_ok(move |_, _, cx| {
                    root.update(cx, |this, cx| this.delete_sidebar_group(&id, cx).is_ok())
                        .unwrap_or(false)
                })
        });
    }

    pub(super) fn move_sidebar_connection(
        &mut self,
        id: &str,
        group: Option<&str>,
        position: usize,
        cx: &mut Context<Self>,
    ) -> Result<(), String> {
        if !self
            .saved_connections
            .iter()
            .any(|connection| connection.matches_id(id))
        {
            return Err("Saved connection not found".into());
        }
        self.change_sidebar_preferences(|prefs| prefs.move_connection(id, group, position), cx)
    }

    pub(super) fn set_sidebar_connection_hidden(
        &mut self,
        id: &str,
        hidden: bool,
        cx: &mut Context<Self>,
    ) -> Result<(), String> {
        if !self
            .saved_connections
            .iter()
            .any(|connection| connection.matches_id(id))
        {
            return Err("Saved connection not found".into());
        }
        self.change_sidebar_preferences(|prefs| prefs.set_hidden(id, hidden), cx)
    }

    #[cfg(test)]
    pub(super) fn set_sidebar_connection_color(
        &mut self,
        id: &str,
        color: Option<&str>,
        cx: &mut Context<Self>,
    ) -> Result<(), String> {
        if !self
            .saved_connections
            .iter()
            .any(|connection| connection.matches_id(id))
        {
            return Err("Saved connection not found".into());
        }
        self.change_sidebar_preferences(|prefs| prefs.set_color(id, color), cx)
    }

    pub(super) fn sort_sidebar_connections(
        &mut self,
        sort: ConnectionSort,
        cx: &mut Context<Self>,
    ) -> Result<(), String> {
        self.change_sidebar_preferences(
            |prefs| {
                prefs.sort = sort;
                Ok(())
            },
            cx,
        )
    }

    pub(super) fn update_sidebar_search(&mut self, options: SearchOptions, cx: &mut Context<Self>) {
        self.sidebar.search_error = self
            .navigator
            .set_search(options)
            .err()
            .map(|error| concise_regex_error(&error.to_string()));
        self.schedule_remote_search(cx);
        self.notify_chrome(cx);
    }

    pub(super) fn sidebar_search_input(
        &mut self,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> gpui::Entity<InputState> {
        if let Some(input) = &self.sidebar.search_input {
            return input.clone();
        }
        let input = cx.new(|cx| InputState::new(window, cx).placeholder("Search..."));
        let observed = input.clone();
        cx.subscribe(
            &input,
            move |this: &mut Self, _, event: &gpui_component::input::InputEvent, cx| {
                if matches!(event, gpui_component::input::InputEvent::Change) {
                    let mut options = this.navigator.search_options().clone();
                    options.query = observed.read(cx).value().to_string();
                    this.update_sidebar_search(options, cx);
                }
            },
        )
        .detach();
        self.sidebar.search_input = Some(input.clone());
        input
    }

    pub(super) fn sync_sidebar_connections(&mut self, cx: &mut Context<Self>) {
        self.sync_connected_ids();
        cx.notify();
    }

    /// Context-free half of [`Self::sync_sidebar_connections`] for callers
    /// that already notify (query-result handlers).
    pub(super) fn sync_connected_ids(&mut self) {
        let ids = self.sessions.connected().map(str::to_string).collect();
        self.navigator.set_connected_ids(ids);
    }

    pub(super) fn handle_sidebar_key(
        &mut self,
        event: &gpui::KeyDownEvent,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> bool {
        if !self.sidebar.focus.is_focused(window) {
            return false;
        }
        let key = event.keystroke.key.as_str();
        let modifiers = event.keystroke.modifiers;
        // Off macOS Ctrl is the primary modifier (`secondary()` below).
        if modifiers.alt || (cfg!(target_os = "macos") && modifiers.control) {
            return false;
        }
        if modifiers.secondary(cx) && key == "a" {
            self.navigator.select_all_connections();
            self.notify_chrome(cx);
            return true;
        }
        if modifiers.secondary(cx) {
            return false;
        }
        let changed = match key {
            "escape" => self.navigator.exit_multi_selection(),
            "up" => self.navigator.move_focus(-1, modifiers.shift),
            "down" => self.navigator.move_focus(1, modifiers.shift),
            "home" => self.navigator.move_focus(isize::MIN, modifiers.shift),
            "end" => self.navigator.move_focus(isize::MAX, modifiers.shift),
            "left" => {
                let Some(id) = self.navigator.selected().cloned() else {
                    return false;
                };
                let Some(index) = self.navigator.visible_index(&id) else {
                    return false;
                };
                let Some(row) = self.navigator.visible_row_at(index) else {
                    return false;
                };
                if row.expanded {
                    self.handle_navigator_disclosure(&row, cx);
                    true
                } else if row.depth > 0 {
                    self.navigator.visible_rows()[..index]
                        .iter()
                        .rev()
                        .find(|candidate| candidate.depth < row.depth)
                        .is_some_and(|ancestor| {
                            self.navigator.select_visible(&ancestor.id, false, false)
                        })
                } else {
                    false
                }
            }
            "right" => {
                let Some(id) = self.navigator.selected().cloned() else {
                    return false;
                };
                let Some(index) = self.navigator.visible_index(&id) else {
                    return false;
                };
                let Some(row) = self.navigator.visible_row_at(index) else {
                    return false;
                };
                if row.expandable && !row.expanded {
                    self.handle_navigator_disclosure(&row, cx);
                    true
                } else if self
                    .navigator
                    .visible_row_at(index + 1)
                    .is_some_and(|child| child.depth > row.depth)
                {
                    self.navigator.move_focus(1, false)
                } else {
                    false
                }
            }
            "enter" => {
                let Some(id) = self.navigator.selected().cloned() else {
                    return false;
                };
                let Some(row) = self
                    .navigator
                    .visible_index(&id)
                    .and_then(|index| self.navigator.visible_row_at(index))
                else {
                    return false;
                };
                match &id {
                    NavigatorNodeId::Object { .. } => self.handle_navigator_open(&row, cx),
                    NavigatorNodeId::Group { .. } => self.handle_navigator_disclosure(&row, cx),
                    _ => self.handle_navigator_click(&row, cx),
                }
                true
            }
            _ => return false,
        };
        if changed {
            if let Some(id) = self.navigator.selected() {
                if let Some(index) = self
                    .navigator
                    .visible_rows()
                    .iter()
                    .position(|row| &row.id == id)
                {
                    self.sidebar
                        .scroll
                        .scroll_to_item(index, gpui::ScrollStrategy::Nearest);
                }
            }
            self.notify_chrome(cx);
        }
        changed
    }

    pub(super) fn new_query_for_sidebar_node(
        &mut self,
        id: &NavigatorNodeId,
        cx: &mut Context<Self>,
    ) -> bool {
        if self.navigator.action_target(id).is_none()
            || !self
                .navigator
                .available_actions(id, false)
                .contains(&NavigatorAction::NewQuery)
        {
            return false;
        }
        self.open_new_query_for_node(id, cx)
    }

    /// Opens the New Query tab scoped to a loaded navigator node. Split from
    /// the visible-row gate above so the command palette (whose targets may
    /// sit in collapsed branches) shares the exact same tab construction.
    pub(super) fn open_new_query_for_node(
        &mut self,
        id: &NavigatorNodeId,
        cx: &mut Context<Self>,
    ) -> bool {
        // A Redis connection or database opens the command console.
        let redis_connection = match id {
            NavigatorNodeId::Connection { connection } | NavigatorNodeId::Provider { connection, .. } => {
                self.saved_connections
                    .iter()
                    .any(|saved| saved.matches_id(connection) && saved.kind == crate::connections::ConnectionKind::Redis)
            }
            _ => false,
        };
        if redis_connection {
            if !self.open_redis_console_for_node(id) {
                return false;
            }
            self.notify_chrome(cx);
            return true;
        }
        match id {
            NavigatorNodeId::Provider { connection, .. } => {
                if !self.saved_connections.iter().any(|saved| {
                    saved.matches_id(connection)
                        && saved.kind == crate::connections::ConnectionKind::Mongodb
                }) {
                    return false;
                }
                let Some(provider) = self.navigator.provider_node(id) else {
                    return false;
                };
                let Some(database) = provider
                    .payload
                    .get("database")
                    .and_then(serde_json::Value::as_str)
                    .map(str::to_string)
                else {
                    return false;
                };
                let collection = provider
                    .payload
                    .get("collection")
                    .and_then(serde_json::Value::as_str)
                    .unwrap_or_default()
                    .to_string();
                self.open_mongo_script_tab(connection, &database, &collection);
            }
            NavigatorNodeId::Connection { connection }
            | NavigatorNodeId::Database { connection, .. }
            | NavigatorNodeId::Schema { connection, .. }
            | NavigatorNodeId::Object { connection, .. } => {
                if self.saved_connections.iter().any(|saved| {
                    saved.matches_id(connection)
                        && saved.kind == crate::connections::ConnectionKind::Mongodb
                }) {
                    let database = match id {
                        NavigatorNodeId::Database { database, .. } => database.clone(),
                        // "Set as Default Database" (sidebar preference) wins
                        // over the database in the connection URI path.
                        _ => self
                            .mongo_default_database(connection)
                            .or_else(|| {
                                self.saved_connections
                                    .iter()
                                    .find(|saved| saved.matches_id(connection))
                                    .and_then(|saved| {
                                        crate::mongo_script::default_database(&saved.uri)
                                            .ok()
                                            .flatten()
                                    })
                            })
                            .unwrap_or_default(),
                    };
                    self.open_mongo_script_tab(connection, &database, "");
                    self.notify_chrome(cx);
                    return true;
                }
                let metadata = TabMetadata {
                    connection_id: connection.clone(),
                    database: match id {
                        NavigatorNodeId::Database { database, .. }
                        | NavigatorNodeId::Schema { database, .. }
                        | NavigatorNodeId::Object { database, .. } => database.clone(),
                        _ => String::new(),
                    },
                    schema: match id {
                        NavigatorNodeId::Schema { schema, .. }
                        | NavigatorNodeId::Object { schema, .. } => Some(schema.clone()),
                        _ => None,
                    },
                    collection: match id {
                        NavigatorNodeId::Object { object, .. } => object.clone(),
                        _ => String::new(),
                    },
                    ..Default::default()
                };
                self.workspace
                    .open_query_tab_with_metadata("Query", metadata);
                if let Some(sql) = self.navigator.preview_sql(id) {
                    if let Some(tab) = self.workspace.active_tab_mut() {
                        tab.sql = sql;
                    }
                }
            }
            _ => return false,
        }
        self.notify_chrome(cx);
        true
    }

    pub(super) fn open_mongo_script_tab(&mut self, connection: &str, database: &str, collection: &str) {
        let id = self.workspace.open_query_tab_with_metadata(
            "Mongo Script",
            TabMetadata {
                connection_id: connection.into(),
                database: database.into(),
                collection: collection.into(),
                query_mode: Some(QueryMode::MongoScript),
                ..Default::default()
            },
        );
        if !collection.is_empty() {
            if let Some(tab) = self.workspace.tab_mut(id) {
                tab.sql = crate::mongo_script::collection_template(collection);
            }
        }
    }
}

/// One-line message for a regex compile error: the `error: ...` line of the
/// regex crate's multi-line report (or its first line), e.g.
/// "Invalid regular expression: unclosed group".
pub(super) fn concise_regex_error(error: &str) -> String {
    let detail = error
        .lines()
        .map(str::trim)
        .find_map(|line| line.strip_prefix("error:").map(str::trim))
        .or_else(|| error.lines().map(str::trim).find(|line| !line.is_empty()))
        .unwrap_or("invalid pattern");
    format!("Invalid regular expression: {detail}")
}

#[cfg(test)]
mod regex_error_tests {
    use super::concise_regex_error;

    #[test]
    fn regex_errors_are_reduced_to_one_line() {
        let pattern = String::from("(");
        let error = regex::Regex::new(&pattern).unwrap_err().to_string();
        assert!(error.lines().count() > 1, "the raw error is multi-line");
        assert_eq!(
            concise_regex_error(&error),
            "Invalid regular expression: unclosed group"
        );
        assert_eq!(
            concise_regex_error("compiled program too big"),
            "Invalid regular expression: compiled program too big"
        );
    }
}
