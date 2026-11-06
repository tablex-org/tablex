use super::connection_menu::CopyInfo;
use super::*;
use crate::connections::ConnectionKind;
use crate::navigator::NavigatorNodeId;
use crate::sessions::ConnectionStatus;
use gpui_component::menu::PopupMenu;
use gpui_component::scroll::ScrollableMask;
use gpui_component::select::{Select, SelectEvent, SelectState};

/// Display name of the menu's connection, for confirmations.
#[allow(dead_code)]
fn connection_label(this: &AppRoot, connection_id: &str) -> String {
    this.saved_connections
        .iter()
        .find(|saved| saved.matches_id(connection_id))
        .map(|saved| saved.name.clone())
        .unwrap_or_else(|| "Connection".into())
}

fn menu_icon(glyph: navigator_ui::SidebarGlyph) -> Icon {
    Icon::new(glyph).size(px(12.0))
}

fn history_menu_contents(
    submenu: PopupMenu,
    root: gpui::WeakEntity<AppRoot>,
    connection_id: String,
    loading: bool,
    entries: Vec<crate::saved_sql::SavedSqlEntry>,
    error: Option<String>,
) -> PopupMenu {
    if loading {
        return submenu.item(PopupMenuItem::new("Loading…").disabled(true));
    }
    if let Some(error) = error {
        return submenu.item(PopupMenuItem::new(format!("Error: {error}")).disabled(true));
    }
    if entries.is_empty() {
        return submenu.item(PopupMenuItem::new("No Saved Queries").disabled(true));
    }
    entries.into_iter().take(10).fold(submenu, |menu, entry| {
        let root = root.clone();
        let id = connection_id.clone();
        menu.item(
            PopupMenuItem::new(entry.title.clone())
                .icon(menu_icon(navigator_ui::SidebarGlyph::History))
                .on_click(move |_, _, app| {
                    let entry = entry.clone();
                    let _ = root.update(app, |this, cx| {
                        if let Err(error) = this.open_saved_sql_entry(&entry, &id, cx) {
                            this.sidebar.notice =
                                Some(format!("Could not open saved SQL: {error}"));
                            cx.notify();
                        }
                    });
                }),
        )
    })
}

/// Local draft for the Visible Object Filter dialog. Retained in an entity
/// while the dialog is open; nothing reaches persistence until Apply calls
/// the real product accessors, so Cancel, Escape, and overlay dismissal can
/// never persist.
pub(super) struct VisibleFilterDialogState {
    pub(super) explicit: bool,
    pub(super) selected: HashSet<String>,
    pub(super) show_system: bool,
    pub(super) schemas: HashMap<String, Option<Vec<String>>>,
    pub(super) schema_database: Option<String>,
    pub(super) schema_available: Arc<[String]>,
    pub(super) schema_loading: bool,
    pub(super) schema_error: Option<String>,
    pub(super) schema_subscription: Option<gpui::Subscription>,
    /// Mirror of the live enumeration cache. AppRoot pushes updates here on
    /// completion/cancel so the dialog builder — which runs during Root
    /// render and must never touch AppRoot, even for reads — still observes
    /// async results without close/reopen.
    pub(super) available: Arc<[String]>,
    pub(super) loading: bool,
    pub(super) error: Option<String>,
}

impl VisibleFilterDialogState {
    pub(super) fn init(current: crate::sidebar_preferences::VisibleObjectFilter) -> Self {
        Self {
            explicit: current.databases.is_some(),
            selected: current.databases.unwrap_or_default().into_iter().collect(),
            show_system: current.show_system,
            schemas: current.schemas,
            schema_database: None,
            schema_available: Arc::from(Vec::<String>::new()),
            schema_loading: false,
            schema_error: None,
            schema_subscription: None,
            available: Arc::from(Vec::<String>::new()),
            loading: false,
            error: None,
        }
    }

    pub(super) fn sync_enumeration(
        &mut self,
        available: Vec<String>,
        loading: bool,
        error: Option<String>,
    ) {
        self.available = Arc::from(available);
        self.loading = loading;
        self.error = error;
    }

    pub(super) fn checked(&self, name: &str) -> bool {
        !self.explicit || self.selected.contains(name)
    }

    fn materialize(&mut self, available: &[String]) {
        if !self.explicit {
            self.selected = available.iter().cloned().collect();
            self.explicit = true;
        }
    }

    pub(super) fn set_checked(&mut self, name: &str, checked: bool, available: &[String]) {
        if !checked {
            self.materialize(available);
            self.selected.remove(name);
        } else if self.explicit {
            self.selected.insert(name.to_string());
        }
    }

    pub(super) fn select_all(&mut self, available: &[String]) {
        self.selected = available.iter().cloned().collect();
        self.explicit = true;
    }

    pub(super) fn select_filtered(&mut self, displayed: Vec<String>) {
        self.selected = displayed.into_iter().collect();
        self.explicit = true;
    }

    pub(super) fn clear(&mut self) {
        self.selected.clear();
        self.explicit = true;
    }

    pub(super) fn show_all(&mut self) {
        self.selected.clear();
        self.explicit = false;
        self.show_system = true;
        self.schemas.clear();
    }

    pub(super) fn sync_schemas(
        &mut self,
        names: Vec<String>,
        loading: bool,
        error: Option<String>,
    ) {
        self.schema_available = Arc::from(names);
        self.schema_loading = loading;
        self.schema_error = error;
    }

    pub(super) fn schema_checked(&self, name: &str) -> bool {
        self.schema_database
            .as_ref()
            .and_then(|db| self.schemas.get(db))
            .and_then(Option::as_ref)
            .is_none_or(|selected| selected.iter().any(|item| item == name))
    }

    pub(super) fn set_schema_checked(&mut self, name: &str, checked: bool) {
        let Some(db) = self.schema_database.clone() else {
            return;
        };
        let selected = self.schemas.entry(db).or_insert(None);
        if selected.is_none() && !checked {
            *selected = Some(self.schema_available.iter().cloned().collect());
        }
        if let Some(selected) = selected {
            if checked {
                if !selected.iter().any(|item| item == name) {
                    selected.push(name.into());
                }
            } else {
                selected.retain(|item| item != name);
            }
        }
    }
}

/// Search- and system-scoped display names, reusing the product draft rules
/// so the dialog lists exactly what the bulk actions act on.
pub(super) fn displayed_indices(
    available: &[String],
    search: &str,
    show_system: bool,
) -> Vec<usize> {
    let matcher = if search.is_empty() {
        None
    } else {
        match regex::RegexBuilder::new(&regex::escape(search))
            .case_insensitive(true)
            .build()
        {
            Ok(matcher) => Some(matcher),
            Err(_) => return Vec::new(),
        }
    };
    available
        .iter()
        .enumerate()
        .filter(|(_, name)| show_system || !crate::sidebar_preferences::is_system_database(name))
        .filter(|(_, name)| {
            matcher
                .as_ref()
                .is_none_or(|matcher| matcher.is_match(name))
        })
        .map(|(index, _)| index)
        .collect()
}

pub(super) fn parse_filter_patterns(text: &str) -> Vec<String> {
    text.lines()
        .map(str::trim)
        .filter(|line| !line.is_empty())
        .map(str::to_string)
        .collect()
}

impl AppRoot {
    pub(super) fn sync_saved_history_menu(
        &mut self,
        connection_id: &str,
        epoch: u64,
        cx: &mut Context<Self>,
    ) {
        let Some((id, registered_epoch, submenu, window_handle)) = self.history_popup.clone()
        else {
            return;
        };
        if id != connection_id || registered_epoch != epoch {
            return;
        }
        let (loading, entries, error) = self.saved_query_history(connection_id);
        let entries = entries.map(|entries| entries.to_vec()).unwrap_or_default();
        let error = error.map(str::to_string);
        let root = cx.entity().downgrade();
        let _ = cx.update_window(window_handle, move |_, window, app| {
            let _ = submenu.update(app, |menu, cx| {
                menu.rebuild(window, cx, move |menu, _, _| {
                    history_menu_contents(menu, root, id, loading, entries, error)
                });
            });
        });
    }
    /// Copies the connection display name. Clipboard writes happen only on
    /// success; failures surface as a sidebar notice. Safe in UI scenarios:
    /// only the display name (never secrets or URLs) reaches the clipboard.
    pub(super) fn copy_connection_name(&mut self, connection_id: &str, cx: &mut Context<Self>) {
        let Some(saved) = self
            .saved_connections
            .iter()
            .find(|saved| saved.matches_id(connection_id))
        else {
            self.sidebar.notice = Some("Could not copy name: saved connection not found".into());
            self.report_error(Area::Connect, Some(connection_id), "Could not copy name: saved connection not found");
            cx.notify();
            return;
        };
        cx.write_to_clipboard(gpui::ClipboardItem::new_string(saved.name.clone()));
    }

    /// Cancels a pending connect by dropping the connecting status.
    /// Completed sessions use Disconnect; this only clears Connecting/Failed.
    pub(super) fn cancel_pending_connect(&mut self, connection_id: &str, cx: &mut Context<Self>) {
        if self.scenario_ui.service.is_some() {
            return;
        }
        if matches!(
            self.sessions.status(connection_id),
            ConnectionStatus::Connecting | ConnectionStatus::Failed(_)
        ) {
            // Only the shared session is in question: the tabs' own sessions keep their transactions.
            self.release_dead_session(connection_id, cx);
        }
    }

    /// Native save-name prompt for the active SQL draft. The product layer
    /// snapshots editor text and writes the file off the UI executor.
    pub(super) fn open_save_sql_dialog(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        // Silent no-op outside an SQL query tab: Cmd-S must never hijack
        // keystrokes from unrelated inputs or pop spurious notices.
        let tab = self.workspace.active_tab();
        let Some(tab) = tab else {
            return;
        };
        if tab.kind != crate::workspace::TabKind::Query
            || tab.query_mode.is_mongo_find()
            || tab.query_mode.is_table_data()
        {
            return;
        }
        let mongo_script = tab.query_mode.is_mongo_script();
        let input = cx.new(|cx| {
            InputState::new(window, cx)
                .placeholder("Script name")
                .default_value(tab.title.clone())
        });
        let root = cx.entity().downgrade();
        window.open_alert_dialog(cx, move |dialog, _, _| {
            let input = input.clone();
            let root = root.clone();
            dialog
                .title(if mongo_script {
                    "Save Mongo Script"
                } else {
                    "Save SQL Script"
                })
                .child(Input::new(&input))
                .button_props(gpui_component::dialog::DialogButtonProps::default().ok_text("Save"))
                .confirm()
                .on_ok(move |_, window, cx| {
                    let name = input.read(cx).value().to_string();
                    if name.trim().is_empty() || name.trim() != name {
                        window.open_alert_dialog(cx, move |dialog, _, _| {
                            dialog
                                .title("Invalid script name")
                                .child("Enter a name without leading or trailing whitespace.")
                        });
                        return false;
                    }
                    let _ = root.update(cx, |this, cx| {
                        if let Err(error) = this.save_active_sql_as(&name, cx) {
                            this.sidebar.notice =
                                Some(format!("Could not save SQL script: {error}"));
                            cx.notify();
                        }
                    });
                    true
                })
        });
    }

    /// Create Database. A kind with a SQL dialect gets the name dialog and then the same reviewed
    /// change-set the navigator's "New Database" runs (the SQL is shown, a Production connection
    /// asks for its confirmation, a read-only one refuses). A kind without one (MongoDB) creates
    /// through the driver's `create_database` mutation. Cancelling anywhere sends nothing to the
    /// server.
    pub(super) fn open_create_database_dialog(
        &mut self,
        connection_id: &str,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if !self.can_create_database(connection_id) {
            self.sidebar.notice = Some("Create Database is unavailable for this connection".into());
            cx.notify();
            return;
        }
        let reviewed = self
            .saved_connections
            .iter()
            .find(|saved| saved.matches_id(connection_id))
            .is_some_and(|saved| crate::sql_ddl::dialect_for(saved.kind).is_some());
        if reviewed {
            self.open_new_database_dialog(connection_id, window, cx);
        } else {
            self.open_driver_create_database_dialog(connection_id, window, cx);
        }
    }

    /// The name dialog of a Create Database that goes to the driver.
    fn open_driver_create_database_dialog(
        &mut self,
        connection_id: &str,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let input = cx.new(|cx| InputState::new(window, cx).placeholder("Database name"));
        let root = cx.entity().downgrade();
        let connection = connection_id.to_string();
        let focus_input = input.clone();
        window.open_alert_dialog(cx, move |dialog, _, _| {
            let input = input.clone();
            let root = root.clone();
            let connection = connection.clone();
            dialog
                .title("Create Database")
                .child(
                    div()
                        .flex()
                        .flex_col()
                        .gap(px(8.0))
                        .child(
                            div()
                                .debug_selector(|| "driver-create-db-input".into())
                                .child(Input::new(&input)),
                        )
                        .child(
                            div()
                                .text_size(px(10.0))
                                .text_color(theme::text_muted())
                                .child("Names must not be empty or padded with whitespace."),
                        ),
                )
                .button_props(
                    gpui_component::dialog::DialogButtonProps::default().ok_text("Create"),
                )
                .confirm()
                .on_ok(move |_, window, cx| {
                    let name = input.read(cx).value().to_string();
                    if let Some(message) = driver_database_name_error(&name) {
                        window.open_alert_dialog(cx, move |dialog, _, _| {
                            dialog.title("Invalid database name").child(message)
                        });
                        return false;
                    }
                    // This dialog goes before a confirmation opens: one dialog at a time.
                    window.close_dialog(cx);
                    let _ = root.update(cx, |this, cx| {
                        this.request_driver_create_database(&connection, name, window, cx)
                    });
                    false
                })
        });
        focus_input.update(cx, |state, cx| state.focus(window, cx));
    }

    /// A name typed for a Create Database through the driver: refused on a read-only connection,
    /// confirmed first when the connection asks for it (Production), then sent.
    fn request_driver_create_database(
        &mut self,
        connection: &str,
        name: String,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if let Some(message) = self.read_only_connection_message(connection, "Creating a database is") {
            self.sidebar.notice = Some(message);
            return cx.notify();
        }
        let effective = self.effective_for(connection);
        let asks = effective.confirm_execute
            || effective.connection_type.as_ref().is_some_and(|ty| ty.is_production());
        if !asks {
            return self.create_database_on_driver(connection, &name, cx);
        }
        let subject = self.connection_subject(connection, &effective);
        let root = cx.entity().downgrade();
        let connection = connection.to_string();
        window.open_alert_dialog(cx, move |dialog, _, _| {
            let (root, connection, name) = (root.clone(), connection.clone(), name.clone());
            dialog
                .title("Create this database?")
                .child(
                    div()
                        .debug_selector(|| "driver-create-db-confirm".into())
                        .child(format!("The database {} will be created on {subject}.", name)),
                )
                .button_props(
                    gpui_component::dialog::DialogButtonProps::default().ok_text("Create"),
                )
                .confirm()
                .on_ok(move |_, _, cx| {
                    let _ = root.update(cx, |this, cx| {
                        this.create_database_on_driver(&connection, &name, cx)
                    });
                    true
                })
        });
    }

    /// Sends the `create_database` mutation; the failure reaches the status and the error log, the
    /// success the sidebar (see `apply_mongo_admin_effects`).
    pub(super) fn create_database_on_driver(&mut self, connection: &str, name: &str, cx: &mut Context<Self>) {
        self.run_mongo_admin(
            connection,
            name,
            crate::mongo_admin::MongoAdminOp::CreateDatabase,
            Box::new(|this, result, cx| {
                if let Err(error) = result {
                    this.sidebar.notice = Some(error);
                    cx.notify();
                }
            }),
            cx,
        );
    }

    /// Move to New Group: prompts for a group name, creates it, then moves
    /// the connection. Cancel creates nothing.
    pub(super) fn open_move_to_new_group_dialog(
        &mut self,
        connection_id: &str,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if !self
            .saved_connections
            .iter()
            .any(|saved| saved.matches_id(connection_id))
        {
            self.sidebar.notice = Some("Could not move connection: not found".into());
            self.report_error(Area::Connect, Some(connection_id), "Could not move connection: not found");
            cx.notify();
            return;
        }
        let input = cx.new(|cx| InputState::new(window, cx).placeholder("Group name"));
        let root = cx.entity().downgrade();
        let connection = connection_id.to_string();
        window.open_alert_dialog(cx, move |dialog, _, _| {
            let input = input.clone();
            let root = root.clone();
            let connection = connection.clone();
            dialog
                .title("Move to New Group")
                .child(Input::new(&input))
                .button_props(gpui_component::dialog::DialogButtonProps::default().ok_text("Move"))
                .confirm()
                .on_ok(move |_, window, cx| {
                    let name = input.read(cx).value().to_string();
                    if name.trim().is_empty() {
                        window.open_alert_dialog(cx, move |dialog, _, _| {
                            dialog
                                .title("Invalid group name")
                                .child("Enter a non-empty group name.")
                        });
                        return false;
                    }
                    let _ = root.update(cx, |this, cx| {
                        match this.create_sidebar_group(name.trim(), None, cx) {
                            Ok(group) => {
                                if let Err(error) = this.move_sidebar_connection(
                                    &connection,
                                    Some(&group),
                                    usize::MAX,
                                    cx,
                                ) {
                                    this.sidebar.notice =
                                        Some(format!("Group created, but move failed: {error}"));
                                    cx.notify();
                                }
                            }
                            Err(error) => {
                                this.sidebar.notice =
                                    Some(format!("Could not create group: {error}"));
                                cx.notify();
                            }
                        }
                    });
                    true
                })
        });
    }

    /// Pushes the live enumeration cache into the open filter dialog's
    /// draft mirror. Called after every finish/cancel path so async server
    /// results land in place. Event-time only, never during render.
    pub(super) fn sync_visible_filter_dialog(
        &mut self,
        connection_id: &str,
        cx: &mut Context<Self>,
    ) {
        if let Some((open_id, draft)) = self.visible_filter_dialog.clone() {
            if open_id == connection_id {
                let available = self
                    .visible_filter_names(connection_id)
                    .map(|names| names.to_vec())
                    .unwrap_or_default();
                let loading = self.visible_filter_loading(connection_id);
                let error = self.visible_filter_error(connection_id).map(str::to_string);
                draft.update(cx, |state, cx| {
                    state.sync_enumeration(available, loading, error);
                    cx.notify();
                });
                if let Some((select_id, select, handle)) = self.visible_filter_schema_select.clone()
                {
                    if select_id == connection_id {
                        let names = self
                            .visible_filter_names(connection_id)
                            .unwrap_or(&[])
                            .to_vec();
                        let _ = cx.update_window(handle, move |_, window, app| {
                            select.update(app, |select, cx| select.set_items(names, window, cx));
                        });
                    }
                }
            }
        }
    }

    pub(super) fn sync_visible_filter_schema_dialog(
        &mut self,
        connection_id: &str,
        database: &str,
        cx: &mut Context<Self>,
    ) {
        let Some((open_id, draft)) = self.visible_filter_dialog.clone() else {
            return;
        };
        if open_id != connection_id || draft.read(cx).schema_database.as_deref() != Some(database) {
            return;
        }
        let (loading, names, error) = self.visible_filter_schema_state(connection_id, database);
        let names = names.unwrap_or(&[]).to_vec();
        let error = error.map(str::to_string);
        draft.update(cx, |state, cx| {
            state.sync_schemas(names, loading, error);
            cx.notify();
        });
    }

    /// Visible Object Filter dialog: requests a fresh server-side enumeration
    /// and edits a local draft (per-item checkboxes, search, bulk selection,
    /// show-system, patterns) until explicit Apply persists through the real
    /// product accessors. Cancel, Escape, and overlay dismissal persist
    /// nothing. The dialog builder reads only its own entities; AppRoot
    /// pushes enumeration updates into the draft mirror, so async completion
    /// lands without close/reopen.
    pub(super) fn open_visible_filter_dialog(
        &mut self,
        connection_id: &str,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(saved_kind) = self
            .saved_connections
            .iter()
            .find(|saved| saved.matches_id(connection_id) && !saved.disabled)
            .map(|saved| saved.kind)
        else {
            self.sidebar.notice = Some("Visible Object Filter is unavailable".into());
            cx.notify();
            return;
        };
        // Fire-and-forget refresh; the dialog below observes the live cache.
        let _ = self.request_visible_filter_names(connection_id, cx);
        let current = self
            .navigator
            .preferences()
            .visible_filters
            .get(connection_id)
            .cloned()
            .unwrap_or_else(|| crate::sidebar_preferences::VisibleObjectFilter {
                // A connection without a filter of its own follows the
                // "show system objects" setting.
                show_system: self.settings.navigation.show_system_objects,
                ..Default::default()
            });
        let draft = cx.new(|_| VisibleFilterDialogState::init(current.clone()));
        let schema_select = (saved_kind == ConnectionKind::Postgres).then(|| {
            cx.new(|cx| SelectState::new(Vec::<String>::new(), None, window, cx).searchable(true))
        });
        if let Some(select) = &schema_select {
            let draft_for_event = draft.clone();
            let connection_for_event = connection_id.to_string();
            let subscription = cx.subscribe(
                select,
                move |this, _, event: &SelectEvent<Vec<String>>, cx| {
                    let SelectEvent::Confirm(Some(database)) = event else {
                        return;
                    };
                    draft_for_event.update(cx, |state, cx| {
                        state.schema_database = Some(database.clone());
                        state.sync_schemas(Vec::new(), true, None);
                        cx.notify();
                    });
                    if let Err(error) =
                        this.request_visible_filter_schemas(&connection_for_event, database, cx)
                    {
                        draft_for_event.update(cx, |state, cx| {
                            state.sync_schemas(Vec::new(), false, Some(error));
                            cx.notify();
                        });
                    }
                },
            );
            draft.update(cx, |state, _| {
                state.schema_subscription = Some(subscription)
            });
            self.visible_filter_schema_select =
                Some((connection_id.into(), select.clone(), window.window_handle()));
        }
        let search = cx.new(|cx| InputState::new(window, cx).placeholder("Search databases"));
        let patterns = cx.new(|cx| {
            TextareaState::new(window, cx)
                .auto_grow(2, 5)
                .placeholder("Regex patterns, one per line (optional)")
                .default_value(current.patterns.join("\n"))
        });
        self.visible_filter_dialog = Some((connection_id.to_string(), draft.clone()));
        // Seed the draft mirror from the current cache; completions push
        // through sync_visible_filter_dialog below.
        self.sync_visible_filter_dialog(connection_id, cx);
        let root = cx.entity().downgrade();
        let connection = connection_id.to_string();
        let sqlite = saved_kind == ConnectionKind::Sqlite;
        // Where the two lists are scrolled to; the dialog is rebuilt on every render.
        let (list_scroll, schema_scroll) = (UniformListScrollHandle::new(), UniformListScrollHandle::new());
        cx.notify();
        window.open_dialog(cx, move |dialog, _, cx| {
            // Live snapshot on every render: enumeration completion, search
            // text, and draft compose here, so async results appear in place.
            // This `cx` is the builder-provided App context; nothing from the
            // opener's Context is captured. Reads are immutable: the builder
            // runs during Root render, where a mutable AppRoot borrow would
            // panic.
            let query = search.read(cx).value().to_string();
            let draft_view = draft.read(cx);
            let available = draft_view.available.clone();
            let loading = draft_view.loading;
            let error = draft_view.error.clone();
            let draft_show_system = draft_view.show_system;
            let schema_names = draft_view.schema_available.clone();
            let schema_db = draft_view.schema_database.clone();
            let schema_status = if draft_view.schema_loading {
                "Loading schemas…".to_string()
            } else if let Some(error) = &draft_view.schema_error {
                format!("Could not load schemas: {error}")
            } else if schema_db.is_some() {
                format!("{} schemas", schema_names.len())
            } else {
                "Choose a database to filter schemas".to_string()
            };
            let shown = displayed_indices(&available, &query, draft_show_system);
            let status_line = if loading {
                "Loading database list from the server…".to_string()
            } else if let Some(error) = &error {
                format!("Could not load databases: {error}")
            } else if available.is_empty() {
                "No databases enumerated yet.".to_string()
            } else if !draft_view.explicit {
                format!(
                    "{} databases enumerated; showing all objects.",
                    available.len()
                )
            } else {
                format!(
                    "{} databases enumerated; {} explicitly visible.",
                    available.len(),
                    draft_view.selected.len()
                )
            };
            let sqlite_note = sqlite.then_some({
                "SQLite connections expose a single file database; server schemas do not apply."
            });
            // Per-button clones: the dialog builder is Fn, so each handler
            // moves only its own snapshot. Checkboxes are stateless renders
            // inside the virtualized list; no per-row entities are created.
            let draft_all = draft.clone();
            let available_all = available.clone();
            let draft_filtered = draft.clone();
            let available_filtered = available.clone();
            let search_filtered = search.clone();
            let draft_clear = draft.clone();
            let draft_show = draft.clone();
            let draft_system = draft.clone();
            let draft_apply = draft.clone();
            let root_apply = root.clone();
            let connection_apply = connection.clone();
            let patterns_apply = patterns.clone();
            let patterns_filtered = patterns.clone();
            let patterns_clear = patterns.clone();
            let patterns_show = patterns.clone();
            let root_close = root.clone();
            let list_draft = draft.clone();
            let list_available = available.clone();
            let list = uniform_list("visible-filter-list", shown.len(), move |range, _, cx| {
                range
                    .map(|index| {
                        let name = list_available[shown[index]].clone();
                        let row_name = name.clone();
                        let row_draft = list_draft.clone();
                        let row_available = list_available.clone();
                        let checked = row_draft.read(cx).checked(&name);
                        // Full-row click target toggling the draft: the
                        // Checkbox keeps native box semantics while the row
                        // gives a larger hit area. No per-row entities.
                        let toggle_draft = row_draft.clone();
                        let toggle_name = name.clone();
                        let toggle_available = row_available.clone();
                        div()
                            .id((
                                gpui::ElementId::Name("visible-filter-row".into()),
                                name.clone(),
                            ))
                            .h(px(28.0))
                            .w_full()
                            .flex()
                            .items_center()
                            .gap(px(8.0))
                            .px(px(8.0))
                            .debug_selector({
                                let row_name = row_name.clone();
                                move || format!("visible-filter-row-{row_name}")
                            })
                            .on_click(move |_, _, cx| {
                                toggle_draft.update(cx, |state, cx| {
                                    let next = !state.checked(&toggle_name);
                                    state.set_checked(&toggle_name, next, &toggle_available);
                                    cx.notify();
                                });
                            })
                            .child(
                                Checkbox::new((
                                    gpui::ElementId::Name("visible-filter-check".into()),
                                    name.clone(),
                                ))
                                .checked(checked)
                                .label(name.clone())
                                .on_click(
                                    move |checked: &bool, _, cx| {
                                        let requested = *checked;
                                        row_draft.update(cx, |state, cx| {
                                            state.set_checked(&name, requested, &row_available);
                                            cx.notify();
                                        });
                                    },
                                ),
                            )
                            .into_any_element()
                    })
                    .collect::<Vec<_>>()
            })
            .h(px(220.0))
            .w_full()
            .track_scroll(&list_scroll);
            let schema_list_draft = draft.clone();
            let schema_list_names = schema_names.clone();
            let schema_list = uniform_list(
                "visible-schema-list",
                schema_names.len(),
                move |range, _, cx| {
                    range
                        .map(|index| {
                            let name = schema_list_names[index].clone();
                            let checked = schema_list_draft.read(cx).schema_checked(&name);
                            let row_draft = schema_list_draft.clone();
                            div()
                                .h(px(27.0))
                                .px(px(8.0))
                                .debug_selector({
                                    let name = name.clone();
                                    move || format!("visible-schema-row-{name}")
                                })
                                .child(
                                    Checkbox::new((
                                        gpui::ElementId::Name("visible-schema-check".into()),
                                        name.clone(),
                                    ))
                                    .checked(checked)
                                    .label(name.clone())
                                    .on_click(
                                        move |checked: &bool, _, cx| {
                                            let checked = *checked;
                                            row_draft.update(cx, |state, cx| {
                                                state.set_schema_checked(&name, checked);
                                                cx.notify();
                                            });
                                        },
                                    ),
                                )
                                .into_any_element()
                        })
                        .collect::<Vec<_>>()
                },
            )
            .h(px(100.0))
            .w_full()
            .track_scroll(&schema_scroll);
            dialog
                .w(px(480.0))
                .title("Visible Object Filter")
                .child(
                    div()
                        .flex()
                        .flex_col()
                        .gap(px(8.0))
                        .child(
                            div()
                                .text_size(px(11.0))
                                .text_color(theme::text_muted())
                                .child(status_line),
                        )
                        .children(sqlite_note.iter().map(|note| {
                            div()
                                .text_size(px(10.0))
                                .text_color(theme::text_muted())
                                .child(*note)
                        }))
                        .child(Input::new(&search))
                        .child(
                            div()
                                .h(px(220.0))
                                .w_full()
                                .flex_shrink_0()
                                .border_1()
                                .border_color(theme::border())
                                .debug_selector(|| "visible-filter-list".into())
                                .relative()
                                .child(list)
                                // The body of the dialog is a `Scrollable`, whose `ScrollBounce`
                                // takes every wheel event over it while the dialog is taller than
                                // the window. The mask scrolls the list first and hands the wheel
                                // back to the dialog at the list's end.
                                .child(ScrollableMask::new(gpui::Axis::Vertical, &list_scroll)),
                        )
                        .child(
                            div()
                                .flex()
                                .flex_row()
                                .flex_wrap()
                                .flex_shrink_0()
                                .gap(px(6.0))
                                .child(
                                    Button::new("visible-filter-select-all")
                                        .label("Select all")
                                        .small()
                                        .debug_selector(|| "visible-filter-select-all".into())
                                        .on_click(move |_, _, cx| {
                                            draft_all.update(cx, |state, cx| {
                                                state.select_all(&available_all);
                                                cx.notify();
                                            });
                                        }),
                                )
                                .child(
                                    Button::new("visible-filter-select-filtered")
                                        .label("Select filtered")
                                        .small()
                                        .debug_selector(|| "visible-filter-select-filtered".into())
                                        .on_click(move |_, window, cx| {
                                            patterns_filtered.update(cx, |state, cx| {
                                                state.set_value("", window, cx)
                                            });
                                            let query =
                                                search_filtered.read(cx).value().to_string();
                                            draft_filtered.update(cx, |state, cx| {
                                                let shown = displayed_indices(
                                                    &available_filtered,
                                                    &query,
                                                    state.show_system,
                                                );
                                                state.select_filtered(
                                                    shown
                                                        .into_iter()
                                                        .map(|index| {
                                                            available_filtered[index].clone()
                                                        })
                                                        .collect(),
                                                );
                                                cx.notify();
                                            });
                                        }),
                                )
                                .child(
                                    Button::new("visible-filter-clear")
                                        .label("Clear")
                                        .small()
                                        .debug_selector(|| "visible-filter-clear".into())
                                        .on_click(move |_, window, cx| {
                                            patterns_clear.update(cx, |state, cx| {
                                                state.set_value("", window, cx)
                                            });
                                            draft_clear.update(cx, |state, cx| {
                                                state.clear();
                                                cx.notify();
                                            });
                                        }),
                                )
                                .child(
                                    div()
                                        .flex()
                                        .items_center()
                                        .debug_selector(|| "visible-filter-show-system".into())
                                        .child(
                                            Checkbox::new("visible-filter-show-system-box")
                                                .checked(draft_show_system)
                                                .label("Show system databases")
                                                .on_click(move |checked: &bool, _, cx| {
                                                    let requested = *checked;
                                                    draft_system.update(cx, |state, cx| {
                                                        state.show_system = requested;
                                                        cx.notify();
                                                    });
                                                }),
                                        ),
                                ),
                        )
                        .child(Textarea::new(&patterns).h(px(70.0)))
                        .when_some(schema_select.clone(), |this, select| {
                            this.child(
                                div()
                                    .flex()
                                    .flex_col()
                                    .gap(px(6.0))
                                    .child(Select::new(&select).placeholder("Database for schemas"))
                                    .child(
                                        div()
                                            .text_size(px(10.0))
                                            .text_color(theme::text_muted())
                                            .child(schema_status),
                                    )
                                    .child(
                                        div()
                                            .relative()
                                            .debug_selector(|| "visible-schema-list".into())
                                            .child(schema_list)
                                            .child(ScrollableMask::new(gpui::Axis::Vertical, &schema_scroll)),
                                    ),
                            )
                        }),
                )
                .footer(
                    gpui_component::dialog::DialogFooter::new()
                        .child(
                            Button::new("visible-filter-show-all")
                                .label("Show All")
                                .compact()
                                .ghost()
                                .debug_selector(|| "visible-filter-show-all".into())
                                .on_click(move |_, window, cx| {
                                    patterns_show
                                        .update(cx, |state, cx| state.set_value("", window, cx));
                                    draft_show.update(cx, |state, cx| {
                                        state.show_all();
                                        cx.notify();
                                    });
                                }),
                        )
                        .child(
                            Button::new("visible-filter-cancel")
                                .label("Cancel")
                                .compact()
                                .ghost()
                                .debug_selector(|| "visible-filter-cancel".into())
                                .on_click(move |_, window, cx| {
                                    window.close_dialog(cx);
                                }),
                        )
                        .child(
                            Button::new("visible-filter-apply")
                                .label("Apply")
                                .compact()
                                .primary()
                                .debug_selector(|| "visible-filter-apply".into())
                                .on_click(move |_, window, cx| {
                                    let outcome = root_apply.update(cx, |this, cx| {
                                        let state = draft_apply.read(cx);
                                        let mut selected: Vec<String> =
                                            state.selected.iter().cloned().collect();
                                        selected.sort();
                                        let patterns_text =
                                            patterns_apply.read(cx).value().to_string();
                                        let patterns = parse_filter_patterns(&patterns_text);
                                        let filter =
                                            crate::sidebar_preferences::VisibleObjectFilter {
                                                databases: state.explicit.then_some(selected),
                                                patterns,
                                                schemas: state.schemas.clone(),
                                                show_system: state.show_system,
                                            };
                                        this.apply_visible_filter(&connection_apply, filter, cx)
                                    });
                                    match outcome {
                                        Ok(Ok(())) => window.close_dialog(cx),
                                        Ok(Err(error)) => {
                                            let _ = root_apply.update(cx, |this, cx| {
                                                this.report_error(Area::Navigator, Some(&connection_apply), format!("Could not apply visible filter: {error}"));
                                                this.sidebar.notice = Some(format!(
                                                    "Could not apply visible filter: {error}"
                                                ));
                                                cx.notify();
                                            });
                                        }
                                        Err(_) => {}
                                    }
                                }),
                        ),
                )
                .on_close(move |_, _, cx| {
                    let _ = root_close.update(cx, |this, cx| {
                        this.visible_filter_dialog = None;
                        this.visible_filter_schema_select = None;
                        cx.notify();
                    });
                })
        });
    }
}

/// Builds the exact approved connection menu. Extra legacy actions
/// (pin, hide, copy/export config) stay reachable from the sidebar
/// Import/Export toolbar and settings menu, never silently dropped.
#[allow(clippy::too_many_arguments)]
pub(super) fn build_connection_menu(
    menu: PopupMenu,
    menu_root: gpui::WeakEntity<AppRoot>,
    connection_id: String,
    connected: bool,
    connecting: bool,
    can_connect: bool,
    _can_disconnect: bool,
    can_refresh: bool,
    can_query: bool,
    window: &mut Window,
    cx: &mut Context<PopupMenu>,
) -> PopupMenu {
    // Snapshot product state once so the menu labels/disabled states are
    // truthful at open time.
    //
    // Items that carry a real keybinding (Copy Name, Refresh, Edit) dispatch
    // through their gpui action so the hint and the keypress share one path.
    // All other items use plain on_click handlers with no action attached, so
    // a click can never double-fire through both paths.
    let snapshot = menu_root
        .update(cx, |this, _| {
            let can_create = this.can_create_database(&connection_id);
            let (loading, entries, error) = this.saved_query_history(&connection_id);
            (
                can_create,
                loading,
                entries.map(|entries| entries.to_vec()),
                error.map(str::to_string),
                this.backup_supported(&connection_id),
                this.accounts_supported(&connection_id),
                this.process_list_supported(&connection_id),
            )
        })
        .unwrap_or_default();
    let (can_create, history_loading, history_entries, history_error, can_backup, can_accounts, can_process_list) = snapshot;

    // First action reflects live state: Cancel while connecting, Close when
    // connected, Connect otherwise. Plain on_click with the captured id: menu
    // clicks never dispatch through row listeners, so exactly one connection
    // acts no matter how many rows are mounted.
    let cancel_root = menu_root.clone();
    let cancel_id = connection_id.clone();
    let close_root = menu_root.clone();
    let close_id = connection_id.clone();
    let connect_root = menu_root.clone();
    let connect_id = connection_id.clone();
    let menu = if connecting {
        menu.item(
            PopupMenuItem::new("Cancel")
                .icon(menu_icon(navigator_ui::SidebarGlyph::Collapse))
                .action(Box::new(CancelConnectionConnect))
                .on_click(move |_, _, app| {
                    let _ = cancel_root.update(app, |this, cx| {
                        this.cancel_pending_connect(&cancel_id, cx);
                    });
                }),
        )
    } else if connected {
        menu.item(
            PopupMenuItem::new("Close Connection")
                .icon(menu_icon(navigator_ui::SidebarGlyph::Connected))
                .action(Box::new(DisconnectConnection))
                .on_click(move |_, window, app| {
                    let _ = close_root.update(app, |this, cx| {
                        this.request_disconnect(&close_id, window, cx);
                    });
                }),
        )
    } else {
        menu.item(
            PopupMenuItem::new("Connect")
                .icon(menu_icon(navigator_ui::SidebarGlyph::Connected))
                .action(Box::new(ConnectConnection))
                .disabled(!can_connect)
                .on_click(move |_, _, app| {
                    let _ = connect_root.update(app, |this, cx| {
                        if let Some(row) = this
                            .navigator
                            .visible_index(&NavigatorNodeId::Connection {
                                connection: connect_id.clone(),
                            })
                            .and_then(|index| this.navigator.visible_row_at(index))
                        {
                            this.handle_navigator_click(&row, cx);
                        }
                    });
                }),
        )
    };
    // Copy Name carries the real Cmd-C action for its hint; the click handler
    // above it wins on click, while keypresses land on the sidebar-level
    // selection-aware listener (never inside text inputs).
    let name_root = menu_root.clone();
    let name_id = connection_id.clone();
    let menu = menu.separator().item(
        PopupMenuItem::new("Copy Name")
            .icon(menu_icon(navigator_ui::SidebarGlyph::Copy))
            .action(Box::new(CopyConnectionName))
            .on_click(move |_, _, app| {
                let _ = name_root.update(app, |this, cx| {
                    this.copy_connection_name(&name_id, cx);
                });
            }),
    );
    let info_root = menu_root.clone();
    let info_id = connection_id.clone();
    let menu = menu.submenu("Copy Connection Info", window, cx, move |submenu, _, _| {
        let mut built = submenu;
        for (label, item, action) in [
            (
                "Connection URL",
                CopyInfo::ConnectionUrl,
                Box::new(CopyConnectionUrl) as Box<dyn gpui::Action>,
            ),
            (
                "Connection URL (with password)",
                CopyInfo::ConnectionUrlWithPassword,
                Box::new(CopyConnectionUrlWithPassword) as Box<dyn gpui::Action>,
            ),
            (
                "Host:Port",
                CopyInfo::HostPort,
                Box::new(CopyConnectionHostPort) as Box<dyn gpui::Action>,
            ),
        ] {
            let root = info_root.clone();
            let id = info_id.clone();
            built = built.item(
                PopupMenuItem::new(label)
                    .icon(menu_icon(navigator_ui::SidebarGlyph::Info))
                    .action(action)
                    .on_click(move |_, _, app| {
                        let _ = root.update(app, |this, cx| {
                            this.copy_connection_info(&id, item, cx);
                        });
                    }),
            );
        }
        built
    });

    // New Query then Query History submenu (real saved SQL files only).
    let query_root = menu_root.clone();
    let query_id = connection_id.clone();
    let menu = menu.separator().item(
        PopupMenuItem::new("New Query")
            .icon(Icon::new(IconName::SquareTerminal).size(px(12.0)))
            .action(Box::new(NewConnectionQuery))
            .disabled(!can_query)
            .on_click(move |_, _, app| {
                let _ = query_root.update(app, |this, cx| {
                    let id = NavigatorNodeId::Connection {
                        connection: query_id.clone(),
                    };
                    if this.navigator.action_target(&id).is_some() {
                        this.new_query_for_sidebar_node(&id, cx);
                    }
                });
            }),
    );
    let history_root = menu_root.clone();
    let history_id = connection_id.clone();
    let history_epoch = menu_root
        .update(cx, |this, _| {
            this.sessions.generation(&history_id).unwrap_or(0)
        })
        .unwrap_or(0);
    let history_window = window.window_handle();
    // Request a refresh on open; the submenu renders the cached snapshot and
    // loading/error/empty states truthfully. The fetch runs off the UI
    // executor, so opening the menu never blocks on filesystem I/O.
    let _ = history_root.update(cx, |this, cx| {
        let _ = this.request_saved_query_history(&history_id, cx);
    });
    let menu = menu.submenu("Query History", window, cx, move |submenu, _, cx| {
        let weak_submenu = cx.entity().downgrade();
        let _ = history_root.update(cx, |this, _| {
            this.history_popup = Some((
                history_id.clone(),
                history_epoch,
                weak_submenu,
                history_window,
            ));
        });
        history_menu_contents(
            submenu,
            history_root.clone(),
            history_id.clone(),
            history_loading,
            history_entries.clone().unwrap_or_default(),
            history_error.clone(),
        )
    });

    // Create Database with truthful capability gating.
    let create_root = menu_root.clone();
    let create_id = connection_id.clone();
    let create_label = if can_create {
        "Create Database".to_string()
    } else {
        "Create Database (unavailable)".to_string()
    };
    let menu = menu.item(
        PopupMenuItem::new(create_label)
            .icon(menu_icon(navigator_ui::SidebarGlyph::Database))
            .action(Box::new(CreateDatabase))
            .disabled(!can_create)
            .on_click(move |_, window, app| {
                let _ = create_root.update(app, |this, cx| {
                    this.open_create_database_dialog(&create_id, window, cx);
                });
            }),
    );

    // Run SQL File: a script of any size, streamed from disk on a session of its own.
    let run_file_root = menu_root.clone();
    let run_file_id = connection_id.clone();
    let menu = menu.item(
        PopupMenuItem::new("Run SQL File\u{2026}")
            .icon(Icon::empty().path("icons/play.svg").size(px(12.0)))
            .on_click(move |_, window, app| {
                let _ = run_file_root.update(app, |this, cx| {
                    let target = this.file_run_target_of_connection(&run_file_id);
                    this.open_file_run(super::file_run::FileRunInit { target, path: None }, window, cx);
                });
            }),
    );

    // Backup and Restore with the server's native tools (PostgreSQL, MySQL, MariaDB).
    let menu = if can_backup {
        [(super::backup::Mode::Backup, "Backup\u{2026}", "icons/download.svg"), (super::backup::Mode::Restore, "Restore\u{2026}", "icons/upload.svg")].into_iter().fold(menu, |menu, (mode, label, icon)| {
            let (root, id) = (menu_root.clone(), connection_id.clone());
            menu.item(PopupMenuItem::new(label).icon(Icon::empty().path(icon).size(px(12.0))).on_click(move |_, window, app| {
                let _ = root.update(app, |this, cx| this.open_backup_for_connection(mode, &id, window, cx));
            }))
        })
    } else {
        menu
    };

    // Users & Privileges of the server (PostgreSQL, MySQL, MariaDB): hidden for the others, like Backup.
    let menu = if can_accounts {
        let (root, id) = (menu_root.clone(), connection_id.clone());
        menu.item(super::mongo_admin_view::entry_item(
            super::mongo_admin_view::Entry {
                selector: "connection-menu-accounts",
                label: "Users & Privileges\u{2026}",
                icon: Icon::empty().path("icons/key-round.svg").size(px(12.0)),
                disabled: None,
                destructive: false,
            },
            move |window, app| {
                let _ = root.update(app, |this, cx| this.open_accounts(&id, window, cx));
            },
        ))
    } else {
        menu
    };

    // Process List of the server (PostgreSQL, MySQL, MariaDB, ClickHouse): hidden for the others.
    let menu = if can_process_list {
        let (root, id) = (menu_root.clone(), connection_id.clone());
        menu.item(super::mongo_admin_view::entry_item(
            super::mongo_admin_view::Entry {
                selector: "connection-menu-process-list",
                label: "Process List\u{2026}",
                icon: Icon::empty().path("icons/list-checks.svg").size(px(12.0)),
                disabled: None,
                destructive: false,
            },
            move |window, app| {
                let _ = root.update(app, |this, cx| this.open_process_list(&id, window, cx));
            },
        ))
    } else {
        menu
    };

    // Move to New Group, Refresh (F5), Visible Object Filter, Edit (Cmd-E),
    // Duplicate Connection.
    let move_root = menu_root.clone();
    let move_id = connection_id.clone();
    let menu = menu.separator().item(
        PopupMenuItem::new("Move to New Group")
            .icon(menu_icon(navigator_ui::SidebarGlyph::MoveToGroup))
            .action(Box::new(MoveConnectionToNewGroup))
            .on_click(move |_, window, app| {
                let _ = move_root.update(app, |this, cx| {
                    this.open_move_to_new_group_dialog(&move_id, window, cx);
                });
            }),
    );
    // Refresh (F5) and Edit (Cmd-E) carry their real actions for hints;
    // clicks run the captured-id handler, keypresses the sidebar-level one.
    let refresh_root = menu_root.clone();
    let refresh_id = connection_id.clone();
    let menu = menu.item(
        PopupMenuItem::new("Refresh")
            .icon(menu_icon(navigator_ui::SidebarGlyph::Refresh))
            .action(Box::new(RefreshConnection))
            .disabled(!can_refresh)
            .on_click(move |_, _, app| {
                let _ = refresh_root.update(app, |this, cx| {
                    if let Some(row) = this
                        .navigator
                        .visible_index(&NavigatorNodeId::Connection {
                            connection: refresh_id.clone(),
                        })
                        .and_then(|index| this.navigator.visible_row_at(index))
                    {
                        this.refresh_connection(&row, cx);
                    }
                });
            }),
    );
    let filter_root = menu_root.clone();
    let filter_id = connection_id.clone();
    let menu = menu.item(
        PopupMenuItem::new("Visible Object Filter")
            .icon(menu_icon(navigator_ui::SidebarGlyph::Filter))
            .action(Box::new(OpenVisibleFilter))
            .on_click(move |_, window, app| {
                let _ = filter_root.update(app, |this, cx| {
                    this.open_visible_filter_dialog(&filter_id, window, cx);
                });
            }),
    );
    let edit_root = menu_root.clone();
    let edit_id = connection_id.clone();
    let menu = menu.item(
        PopupMenuItem::new("Edit Connection")
            .icon(menu_icon(navigator_ui::SidebarGlyph::Edit))
            .action(Box::new(EditConnection))
            .on_click(move |_, window, app| {
                let _ = edit_root.update(app, |this, cx| {
                    let saved = this
                        .saved_connections
                        .iter()
                        .find(|connection| connection.matches_id(&edit_id))
                        .cloned();
                    this.open_connection_dialog(saved, window, cx);
                });
            }),
    );
    let duplicate_root = menu_root.clone();
    let duplicate_id = connection_id.clone();
    let menu = menu.item(
        PopupMenuItem::new("Duplicate Connection")
            .icon(menu_icon(navigator_ui::SidebarGlyph::Copy))
            .action(Box::new(DuplicateConnection))
            .on_click(move |_, _, app| {
                let _ = duplicate_root.update(app, |this, cx| {
                    let _ = this.duplicate_sidebar_connection(&duplicate_id, cx);
                    cx.notify();
                });
            }),
    );

    // Delete Connection: the pinned PopupMenu renders ElementItem inside the
    // standard item chrome (hover, keyboard, dismissal intact); only the
    // label carries the danger color. Confirmation stays the destructive gate.
    menu.separator().item(
        PopupMenuItem::element(|_, _| div().text_color(theme::danger()).child("Delete Connection"))
            .icon(menu_icon(navigator_ui::SidebarGlyph::Trash))
            .action(Box::new(DeleteConnection))
            .on_click(move |_, window, app| {
                let root = menu_root.clone();
                let id = connection_id.clone();
                let _ = root.update(app, |this, cx| {
                    let node = NavigatorNodeId::Connection {
                        connection: id.clone(),
                    };
                    if this.navigator.action_target(&node).is_none() {
                        return;
                    }
                    if this.navigator.connection_multi_mode()
                        && this.navigator.selected_ids().contains(&node)
                    {
                        this.confirm_delete_selected_connections(window, cx);
                    } else {
                        this.delete_connection_with_confirmation(id, window, cx);
                    }
                });
            }),
    )
}

/// Why `name` cannot name a database created through the driver; the driver refuses what else is
/// wrong with it and says so.
fn driver_database_name_error(name: &str) -> Option<&'static str> {
    if name.is_empty() || name.trim() != name {
        return Some("Database name is required and must not have leading or trailing whitespace.");
    }
    name.contains('\0').then_some("Database name must not contain NUL bytes.")
}
