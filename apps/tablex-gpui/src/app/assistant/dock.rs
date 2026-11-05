//! The dock: where the panel sits, how it opens and closes, what it persists, and the one thing
//! it does to the editors (Insert).
//!
//! Closed, the dock costs nothing: its entity is not rendered, observes nothing and is not
//! updated; the shell lays out two panels, not three.

use super::panel::{AssistantPanel, PanelContext};
use super::*;
use crate::sidebar_preferences::{ASSISTANT_WIDTH_MAX, ASSISTANT_WIDTH_MIN};
use crate::workspace::{QueryMode, Tab, TabKind, TabMetadata};
use gpui::WeakFocusHandle;

#[derive(Default)]
pub(in crate::app) struct DockState {
    pub(in crate::app) open: bool,
    pub(in crate::app) width: f32,
    pub(in crate::app) panel: Option<Entity<AssistantPanel>>,
    /// Where the keyboard focus was when the dock opened; given back when it closes (and by
    /// Escape in the panel). Weak: the place may be gone by then.
    return_focus: Option<WeakFocusHandle>,
    /// The tab that was active when `return_focus` was taken: a place in another tab is not on
    /// screen any more, even while its handle lives.
    return_tab: Option<u64>,
    /// A write of the open state and width waits for its quiet time.
    save_task: Option<gpui::Task<()>>,
    /// The sizes of the split group around the main area, kept as an entity so they do not
    /// depend on where in the element tree the group is.
    split: Option<Entity<gpui_component::resizable::ResizableState>>,
    /// What the panel was last told about the active tab (compared with a hash, so the root's
    /// render allocates nothing while the tab did not change).
    tab_key: u64,
}

/// A change of the layout waits this long before it is written: a burst of toggles is one write.
const LAYOUT_SAVE_DEBOUNCE: Duration = Duration::from_millis(400);

impl DockState {
    pub(in crate::app) fn split_state(
        &mut self,
        cx: &mut gpui::App,
    ) -> Entity<gpui_component::resizable::ResizableState> {
        self.split
            .get_or_insert_with(|| cx.new(|_| gpui_component::resizable::ResizableState::default()))
            .clone()
    }
}

/// A tab whose editor takes SQL: a query tab that is not a Mongo, Redis or table-data tab.
pub(in crate::app) fn is_sql_editor(tab: &Tab) -> bool {
    tab.kind == TabKind::Query
        && !matches!(
            tab.query_mode,
            Some(
                QueryMode::MongoFind
                    | QueryMode::MongoScript
                    | QueryMode::MongoAggregate
                    | QueryMode::SqlTable
                    | QueryMode::RedisKey
                    | QueryMode::RedisCommand
                    | QueryMode::Unknown(_)
            )
        )
}

impl AppRoot {
    pub(in crate::app) fn toggle_assistant(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.assistant.dock.open {
            self.close_assistant(window, cx);
        } else {
            self.open_assistant(window, cx);
        }
    }

    /// Shows the dock and puts the keyboard in the composer.
    pub(in crate::app) fn open_assistant(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if !self.assistant.dock.open {
            self.assistant.dock.return_focus = window.focused(cx).map(|handle| handle.downgrade());
            self.assistant.dock.return_tab = self.workspace.active_tab().map(|tab| tab.id);
        }
        self.assistant.dock.open = true;
        self.sync_dock_split(cx);
        let panel = self.assistant_panel(window, cx);
        panel.update(cx, |panel, cx| {
            panel.attach(cx);
            panel.focus_composer(window, cx);
        });
        // The credential store may have been unreadable a moment ago.
        self.assistant_recheck_key(cx);
        self.assistant_sync(cx);
        // What was kept may have changed while the dock was closed (nothing reads it then).
        self.assistant_refresh_history(true, cx);
        self.persist_assistant_layout(cx);
        self.notify_chrome(cx);
    }

    /// Escape in the panel: stops a running answer; with nothing running it gives the keyboard
    /// back to where it was before the panel took it, or, when that place is not known (the
    /// dock was restored open) or is gone, to the active tab's editor.
    pub(in crate::app) fn assistant_escape(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let running = self
            .assistant
            .chat()
            .is_some_and(|chat| chat.read(cx).is_running());
        if running {
            self.assistant_stop(cx);
        } else if let Some(handle) = self.focus_target_outside_dock(cx) {
            window.focus(&handle, cx);
        }
    }

    /// Where the keyboard goes when it leaves the dock: where it was, if that still exists, else
    /// the active tab's query editor (when it has one).
    fn focus_target_outside_dock(&self, cx: &gpui::App) -> Option<gpui::FocusHandle> {
        let active = self.workspace.active_tab().map(|tab| tab.id);
        self.assistant
            .dock
            .return_focus
            .as_ref()
            .filter(|_| self.assistant.dock.return_tab == active)
            .and_then(WeakFocusHandle::upgrade)
            .or_else(|| {
                let tab = self.workspace.active_tab()?;
                let editor = self.query_editors.by_tab.get(&tab.id)?;
                Some(editor.focus_handle(cx))
            })
    }

    /// Hides the dock. Focus goes back to where it was if it was in the panel.
    pub(in crate::app) fn close_assistant(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if !self.assistant.dock.open {
            return;
        }
        self.assistant.dock.open = false;
        self.sync_dock_split(cx);
        let had_focus = self.assistant.dock.panel.as_ref().is_some_and(|panel| {
            panel.update(cx, |panel, cx| {
                let focused = panel.has_focus(window, cx);
                panel.detach();
                focused
            })
        });
        let back = self.focus_target_outside_dock(cx);
        self.assistant.dock.return_focus = None;
        if had_focus {
            if let Some(handle) = back {
                window.focus(&handle, cx);
            }
        }
        self.persist_assistant_layout(cx);
        self.notify_chrome(cx);
    }

    /// The panel entity, created on first use.
    pub(in crate::app) fn assistant_panel(
        &mut self,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Entity<AssistantPanel> {
        if let Some(panel) = &self.assistant.dock.panel {
            return panel.clone();
        }
        let chat = self.assistant_chat(cx);
        let root = cx.entity().downgrade();
        let panel = cx.new(|cx| AssistantPanel::new(root, chat, window, cx));
        self.assistant.dock.panel = Some(panel.clone());
        panel
    }

    /// The dock to put next to the main area, or `None` while it is closed.
    pub(in crate::app) fn render_assistant_dock(
        &mut self,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Option<Entity<AssistantPanel>> {
        if !self.assistant.dock.open {
            return None;
        }
        self.sync_dock_split(cx);
        let panel = self.assistant_panel(window, cx);
        if !panel.read(cx).is_attached() {
            // Restored open from the preferences: follow the chat from the first frame.
            panel.update(cx, |panel, cx| panel.attach(cx));
            self.assistant_refresh_history(true, cx);
        }
        // The active tab may have changed since the panel last heard (a change of tab does not
        // always notify the root). A hash of the tab's identity says so without allocating.
        let key = self.active_tab_key();
        if key != self.assistant.dock.tab_key {
            self.assistant.dock.tab_key = key;
            // ONE place re-scopes: the whole sync (the chat's scope, its status and whether it is
            // kept, the History of its connection, the panel's context), never a part of it.
            self.assistant_sync(cx);
        }
        #[cfg(any(test, feature = "ui-scenarios"))]
        if std::mem::take(&mut self.assistant.scenario_open_history) {
            panel.update(cx, |panel, cx| panel.open_history_for_scenario(cx));
        }
        Some(panel)
    }

    /// Puts the split group's state in step with the dock: opened, it gets a second panel of the
    /// saved width (rather than a share of the space); closed, the panel goes and the main area
    /// takes the width back.
    fn sync_dock_split(&mut self, cx: &mut Context<Self>) {
        let state = self.assistant.dock.split_state(cx);
        let (open, width) = (self.assistant.dock.open, self.assistant.dock.width);
        state.update(cx, |state, cx| {
            // One panel: the main area is laid out and the dock is new. (None: the first frame,
            // where the panel's own size is used.)
            if open && state.sizes().len() == 1 {
                state.insert_panel(Some(px(width)), None, cx);
            } else if !open && state.sizes().len() > 1 {
                state.remove_panel(1, cx);
            }
        });
    }

    /// The width the user dragged the dock to (mouse-up).
    pub(in crate::app) fn assistant_resized(&mut self, width: f32, cx: &mut Context<Self>) {
        if !width.is_finite() {
            return;
        }
        self.assistant.dock.width = width.clamp(ASSISTANT_WIDTH_MIN, ASSISTANT_WIDTH_MAX);
        self.persist_assistant_layout(cx);
    }

    /// What identifies the active tab for the assistant: its id and where it points.
    fn active_tab_key(&self) -> u64 {
        use std::hash::{Hash, Hasher};
        let mut hasher = std::collections::hash_map::DefaultHasher::new();
        if let Some(tab) = self.workspace.active_tab() {
            tab.id.hash(&mut hasher);
            tab.connection_id.hash(&mut hasher);
            tab.database.hash(&mut hasher);
            tab.schema.hash(&mut hasher);
        }
        hasher.finish()
    }

    /// Open / closed and the width, in the sidebar preferences file (nothing without one). The
    /// in-memory preferences change now; the file is written after a quiet moment, on a
    /// background thread, into whatever the file holds then.
    fn persist_assistant_layout(&mut self, cx: &mut Context<Self>) {
        let Some(path) = self.sidebar.path.clone() else {
            return;
        };
        let (open, width) = (self.assistant.dock.open, self.assistant.dock.width);
        self.navigator.set_assistant_layout(open, width);
        self.assistant.dock.save_task = Some(cx.spawn(async move |this, cx| {
            cx.background_executor().timer(LAYOUT_SAVE_DEBOUNCE).await;
            let (open, width) = this
                .update(cx, |this, _| {
                    (this.assistant.dock.open, this.assistant.dock.width)
                })
                .unwrap_or((open, width));
            let written = cx
                .background_executor()
                .spawn(async move {
                    crate::sidebar_preferences::SidebarPreferences::update_assistant_layout(
                        &path, open, width,
                    )
                })
                .await;
            let _ = this.update(cx, |this, cx| {
                this.assistant.dock.save_task = None;
                if let Err(error) = written {
                    this.report_warning(
                        Area::App,
                        None,
                        format!("Could not save the assistant panel layout: {error}"),
                    );
                    cx.notify();
                }
            });
        }));
    }

    /// The quit: a layout change still waiting for its quiet time is written now.
    pub(in crate::app) fn flush_assistant_layout(&mut self) {
        if self.assistant.dock.save_task.take().is_some() {
            if let Some(path) = &self.sidebar.path {
                let _ = crate::sidebar_preferences::SidebarPreferences::update_assistant_layout(
                    path,
                    self.assistant.dock.open,
                    self.assistant.dock.width,
                );
            }
        }
    }

    /// Where the data goes, for the disclosure: the selected provider's host (and port) from its
    /// base URL.
    fn assistant_destination(&self) -> String {
        let base = self.settings.ai.selected().base_url.trim();
        let Ok(url) = url::Url::parse(base) else {
            return base.to_owned();
        };
        let host = url.host_str().unwrap_or(base);
        match host {
            "api.anthropic.com" => return "Anthropic (api.anthropic.com)".to_owned(),
            "api.openai.com" => return "OpenAI (api.openai.com)".to_owned(),
            _ => {}
        }
        match url.port() {
            Some(port) => format!("{host}:{port}"),
            None => host.to_owned(),
        }
    }

    /// What the panel shows besides the chat; recomputed when the app changes under it.
    fn assistant_panel_context(&self, cx: &gpui::App) -> PanelContext {
        let chat = self.assistant.chat();
        let scope = chat.and_then(|chat| chat.read(cx).scope().cloned());
        let scope_id = scope
            .as_ref()
            .map(|scope| scope.connection_id.clone())
            .unwrap_or_default();
        let scope_label = scope.as_ref().map_or_else(String::new, |scope| {
            // The saved name now; the one the chat started with if the connection is gone.
            let name = self
                .connection_name(&scope.connection_id)
                .map(str::to_owned)
                .or_else(|| {
                    chat.map(|chat| chat.read(cx).scope_label().to_owned())
                        .filter(|label| !label.is_empty())
                })
                .unwrap_or_else(|| scope.connection_id.clone());
            let mut parts = vec![name];
            parts.extend(scope.database.clone());
            parts.extend(scope.schema.clone());
            parts.join(" \u{00B7} ")
        });
        let active_differs = self.assistant_scope_differs(cx).then(|| {
            self.assistant_active_scope()
                .0
                .map(|scope| {
                    self.connection_name(&scope.connection_id)
                        .unwrap_or("this tab's connection")
                        .to_owned()
                })
                .unwrap_or_else(|| "this tab".to_owned())
        });
        let history_available =
            self.assistant.store.is_some() && self.connection_name(&scope_id).is_some();
        let (history, history_connection) = self
            .assistant
            .history
            .rows_for(scope.as_ref().map(|scope| scope.connection_id.as_str()));
        PanelContext {
            scope_label,
            active_differs,
            offered: scope
                .map(|scope| self.assistant_offered_levels(&scope.connection_id))
                .unwrap_or_default(),
            destination: self.assistant_destination(),
            provider: self.settings.ai.provider.label().to_owned(),
            model: self.settings.ai.selected().model.clone(),
            access_busy: self.assistant.access_busy,
            access_error: self.assistant.access_error.clone(),
            scope_id: scope_id.into(),
            names: self
                .saved_connections
                .iter()
                .map(|saved| (saved.id.to_string(), saved.name.clone()))
                .collect(),
            history,
            history_connection,
            history_note: self.assistant.history.note.clone(),
            history_available,
            keep_conversations: self.settings.ai.keep_conversations,
        }
    }

    /// Hands the panel its context. Free while the dock is closed.
    pub(in crate::app) fn assistant_push_context(&mut self, cx: &mut Context<Self>) {
        if !self.assistant.dock.open {
            return;
        }
        let Some(panel) = self.assistant.dock.panel.clone() else {
            return;
        };
        let context = self.assistant_panel_context(cx);
        panel.update(cx, |panel, cx| panel.set_context(context, cx));
    }

    /// Insert on a SQL block: the SQL goes in at the cursor of the active SQL editor when that
    /// tab is on the chat's connection, else into a new query tab on it. Nothing is run.
    pub(in crate::app) fn assistant_insert_sql(
        &mut self,
        sql: &str,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(scope) = self
            .assistant
            .chat()
            .and_then(|chat| chat.read(cx).scope().cloned())
        else {
            return;
        };
        // The editor must be on the chat's connection *and* its database (and schema, where the
        // tab has one): SQL about one database does not land in an editor on another.
        let active = self
            .workspace
            .active_tab()
            .filter(|tab| {
                is_sql_editor(tab)
                    && tab.connection_id == scope.connection_id
                    && tab.database == scope.database.clone().unwrap_or_default()
                    && tab
                        .schema
                        .as_ref()
                        .is_none_or(|schema| scope.schema.as_ref() == Some(schema))
            })
            .map(|tab| tab.id);
        match active {
            Some(tab_id) => {
                let editor = self.editor_for_tab(tab_id, window, cx);
                editor.update(cx, |editor, cx| editor.insert(sql.to_owned(), window, cx));
                // The editor reports its own change; make sure the tab has the text even if the
                // insert was silent.
                let text = editor.read(cx).value().to_string();
                if let Some(tab) = self.workspace.tab_mut(tab_id) {
                    tab.sql = text;
                }
            }
            None => {
                let id = self.workspace.open_query_tab_with_metadata(
                    "Query",
                    TabMetadata {
                        connection_id: scope.connection_id,
                        database: scope.database.unwrap_or_default(),
                        schema: scope.schema,
                        ..Default::default()
                    },
                );
                if let Some(tab) = self.workspace.tab_mut(id) {
                    tab.sql = sql.to_owned();
                }
            }
        }
        self.notify_chrome(cx);
    }
}
