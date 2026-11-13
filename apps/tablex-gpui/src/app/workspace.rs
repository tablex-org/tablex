use super::*;

/// How many closed tabs "Reopen Closed Tab" walks back through; the oldest is dropped first.
const CLOSED_TABS_KEPT: usize = 20;

#[derive(Default)]
pub(super) struct WorkspaceEditors {
    /// Inactive editors stay alive with their cursor and undo history.
    pub(super) by_tab: HashMap<u64, gpui::Entity<EditorState>>,
}

#[cfg(test)]
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct RecordedHistory {
    pub(super) connection: String,
    pub(super) database: String,
    pub(super) collection: String,
    pub(super) sql: String,
    pub(super) rows: i64,
}

/// Restores Swift workspace SQL tabs whose connections this shell can drive.
/// Read-only: nothing is written back, and tabs for unsupported connections
/// (MongoDB, Redis, missing UUIDs) are skipped rather than migrated.
pub(super) fn restore_swift_sql_tabs(
    workspace: &mut WorkspaceState,
    connections: &[SavedConnection],
) {
    let Some(restored) = crate::session::restore_sql_tabs() else {
        return;
    };
    restore_tabs_into(workspace, connections, restored);
}

/// Opens `restored` tabs whose saved connection still exists (split out so
/// tests can restore without reading `~/Library`).
pub(super) fn restore_tabs_into(
    workspace: &mut WorkspaceState,
    connections: &[SavedConnection],
    restored: Vec<crate::session::RestoredTab>,
) {
    let mut active_tab = None;
    for tab in restored {
        let Some(saved) = connections
            .iter()
            .find(|conn| conn.matches_id(&tab.connection_id))
        else {
            continue; // Saved connection was deleted; leave the tab out.
        };
        if saved.disabled {
            continue; // Connection type this shell cannot drive (Mongo etc.).
        }
        let id = workspace.open_query_tab_with_metadata(
            tab.title.clone(),
            TabMetadata {
                source_id: Some(tab.source_id.clone()),
                connection_id: saved.id.to_string(),
                database: tab.database,
                schema: tab.schema,
                collection: tab.collection,
                query_mode: tab.query_mode,
                is_pinned: tab.is_pinned,
            },
        );
        if tab.active {
            active_tab = Some(id);
        }
        if let Some(workspace_tab) = workspace.active_tab_mut() {
            workspace_tab.sql = tab.sql;
            workspace_tab.output_limit = tab.output_limit;
            if workspace_tab.query_mode.is_table_data() {
                // Table-data tabs keep their statement in WHERE / ORDER BY.
                workspace_tab.applied_where = tab.where_clause.clone();
                workspace_tab.applied_order_by = tab.order_by.clone();
                workspace_tab.table_where = tab.where_clause;
                workspace_tab.table_order_by = tab.order_by;
                // A name the dialect cannot quote has no statement to inspect; the run says why.
                workspace_tab.sql = crate::sql_table::base_select(
                    crate::sql_table::SqlDialect::for_kind(saved.kind),
                    workspace_tab.schema.as_deref(),
                    &workspace_tab.collection,
                )
                .unwrap_or_default();
            }
        }
    }
    if let Some(id) = active_tab {
        workspace.activate_tab(id);
    }
}

#[derive(Default)]
pub(super) struct AutoRefreshState {
    pub(super) tasks: HashMap<u64, Task<()>>,
}

/// A run that starts once its connection opened (`epoch` is the connect
/// attempt it waits for; a different attempt supersedes it).
pub(super) struct PendingRun {
    pub(super) connection: String,
    pub(super) epoch: u64,
    pub(super) run: crate::query::QueryRunId,
    pub(super) kind: PendingRunKind,
}

pub(super) enum PendingRunKind {
    /// `sql` is a selection / current statement when `selection`, else the
    /// tab's whole text.
    MongoScript {
        sql: String,
        selection: bool,
    },
    Sql(SqlRun),
}

#[derive(Default)]
pub(super) struct QueryTaskState {
    pub(super) tasks: HashMap<u64, Task<()>>,
    /// One run per tab waiting for its driver connection to open.
    pub(super) pending_runs: HashMap<u64, PendingRun>,
    pub(super) started_at: HashMap<u64, (crate::query::QueryRunId, Instant)>,
    pub(super) registry: QueryTaskRegistry,
    /// The run a page button started, and its direction (`true` = next): the
    /// footer spins that button while the run is the tab's current one.
    pub(super) paging: HashMap<u64, (crate::query::QueryRunId, bool)>,
}

impl QueryTaskState {
    /// The direction of the page the tab is loading, if its running query
    /// came from a page button.
    pub(super) fn paging_direction(&self, tab_id: u64) -> Option<bool> {
        let (run, _) = self.started_at.get(&tab_id)?;
        let (paged_run, forward) = self.paging.get(&tab_id)?;
        (self.tasks.contains_key(&tab_id) && paged_run == run).then_some(*forward)
    }
}

impl AppRoot {
    pub(super) fn toggle_history(&mut self, tab_id: u64, cx: &mut Context<Self>) {
        if self.scenario_ui.service.is_some() {
            return;
        }
        let visible = !self.view(tab_id).history.visible;
        self.view_mut(tab_id).history.visible = visible;
        if !visible {
            cx.notify();
            return;
        }
        let entries = match self.workspace.tab(tab_id) {
            Some(tab) if !tab.collection.is_empty() => engine_storage::history::fetch_entries(
                &tab.connection_id,
                &tab.database,
                &tab.collection,
                50,
            )
            .unwrap_or_default(),
            _ => Vec::new(),
        };
        self.view_mut(tab_id).history.entries = entries;
        cx.notify();
    }

    pub(super) fn run_history_entry(
        &mut self,
        tab_id: u64,
        entry_id: i64,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(entry) = self
            .view(tab_id)
            .history
            .entries
            .iter()
            .find(|entry| entry.id == entry_id)
            .cloned()
        else {
            return;
        };
        if let Some(tab) = self.workspace.tab_mut(tab_id) {
            tab.sql = entry.filter.clone();
        }
        if let Some(editor) = self.query_editors.by_tab.get(&tab_id).cloned() {
            editor.update(cx, |state, cx| {
                state.set_value(entry.filter.clone(), window, cx);
            });
        }
        self.view_mut(tab_id).history.visible = false;
        self.run_tab_query_explicit(tab_id, cx);
    }

    pub(super) fn render_history_panel(&self, tab_id: u64, cx: &mut Context<Self>) -> AnyElement {
        let history = &self.view(tab_id).history;
        if !history.visible {
            return div().into_any_element();
        }
        let entries = history.entries.clone();
        let list = entries.into_iter().fold(
            div()
                .id("history-list")
                .flex()
                .flex_col()
                .gap(px(2.0))
                .overflow_y_scroll(),
            |container, entry| {
                let entry_id = entry.id;
                container.child(
                    div()
                        .id(("history-entry", entry_id as u64))
                        .p(px(8.0))
                        .bg(theme::surface())
                        .hover(|style| style.bg(theme::hover()))
                        .on_click(cx.listener(move |this, _, window, cx| {
                            this.run_history_entry(tab_id, entry_id, window, cx);
                        }))
                        .child(
                            div()
                                .text_size(px(11.0))
                                .text_color(theme::text())
                                .child(entry.filter),
                        )
                        .child(
                            div()
                                .text_size(px(10.0))
                                .text_color(theme::text_muted())
                                .child(format!("{} rows", entry.result_count)),
                        ),
                )
            },
        );
        div()
            .h(px(180.0))
            .px(px(12.0))
            .py(px(8.0))
            .flex()
            .flex_col()
            .gap(px(6.0))
            .border_t_1()
            .border_color(theme::border())
            .bg(theme::chrome())
            .child(
                div()
                    .text_size(px(11.0))
                    .text_color(theme::text_muted())
                    .child("Query History"),
            )
            .child(if history.entries.is_empty() {
                div()
                    .text_size(px(11.0))
                    .text_color(theme::text_muted())
                    .child("No saved queries for this table")
                    .into_any_element()
            } else {
                list.into_any_element()
            })
            .into_any_element()
    }

    pub(super) fn set_output_limit(&mut self, tab_id: u64, limit: i64, cx: &mut Context<Self>) {
        if self.refuse_while_editing_mongo_document(tab_id, cx) {
            return;
        }
        let is_mongo = self
            .workspace
            .tab(tab_id)
            .is_some_and(|tab| tab.query_mode.is_mongo());
        let effective = if is_mongo {
            limit.clamp(1, super::results::mongo::query::MONGO_PAGE_MAX)
        } else {
            limit
        };
        if let Some(tab) = self.workspace.tab_mut(tab_id) {
            tab.output_limit = effective;
        }
        if let Some(runtime) = self.workspace.runtime_mut(tab_id) {
            runtime.pagination = crate::query::PaginationState::new(effective);
        }
        // A shell result's script may write: the limit only applies to the next Run.
        if self.shell_view(tab_id).is_some() {
            cx.notify();
            return;
        }
        self.rerun_shown_result(tab_id, true, cx);
    }

    /// The editor entity for a tab, creating it on first use with SQL
    /// highlighting and the tab's current SQL text.
    pub(super) fn editor_for_tab(
        &mut self,
        tab_id: u64,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> gpui::Entity<EditorState> {
        if let Some(editor) = self.query_editors.by_tab.get(&tab_id) {
            return editor.clone();
        }
        let sql = self
            .workspace
            .tabs()
            .iter()
            .find(|tab| tab.id == tab_id)
            .map(|tab| tab.sql.clone())
            .unwrap_or_default();
        let language = self
            .workspace
            .tabs()
            .iter()
            .find(|tab| tab.id == tab_id)
            .map_or("sql", |tab| {
                if tab.query_mode.is_mongo_script() {
                    "javascript"
                } else if tab.query_mode.is_redis_command() {
                    "plaintext"
                } else {
                    "sql"
                }
            });
        let tab_id_for_events = tab_id;
        let completion = TabCompletion::for_tab(cx, tab_id, CompletionRole::Query);
        let editor_settings = self.settings.editor.clone();
        let editor = cx.new(|cx| {
            EditorState::new(window, cx)
                .language(language)
                .line_number(editor_settings.line_numbers)
                .soft_wrap(editor_settings.soft_wrap)
                .tab_size(super::settings_state::tab_size_of(&editor_settings))
                .auto_close(editor_settings.auto_close)
                .default_value(sql.clone())
                .completion(completion)
        });
        let editor_for_events = editor.clone();
        cx.subscribe(
            &editor,
            move |this: &mut Self,
                  _: gpui::Entity<EditorState>,
                  event: &gpui_component::input::InputEvent,
                  cx: &mut Context<Self>| {
                match event {
                    gpui_component::input::InputEvent::Change => {
                        let text = editor_for_events.read(cx).value().to_string();
                        // By id, not "the active tab": the editor of a tab in an
                        // unfocused pane saves its text too.
                        if let Some(tab) = this.workspace.tab_mut(tab_id_for_events) {
                            let was_unsaved = tab.has_unsaved_file();
                            tab.sql = text;
                            // The tab's unsaved dot appears and goes with the text.
                            if was_unsaved != tab.has_unsaved_file() {
                                cx.notify();
                            }
                            if let Some(runtime) = this.workspace.runtime_mut(tab_id_for_events) {
                                runtime.context_sql_sort = None;
                            }
                        }
                    }
                    // Cmd+Enter arrives as the secondary enter binding.
                    gpui_component::input::InputEvent::PressEnter {
                        secondary: true, ..
                    } => {
                        this.run_tab_query_explicit(tab_id_for_events, cx);
                    }
                    _ => {}
                }
            },
        )
        .detach();
        // gpui-component 0.8.0's search panel selects its field again whenever the session's
        // `anchor_offset` (where the nearest match is looked for) changes, and the first
        // character typed changes it: the second character then replaces the first. The anchor is
        // only read at the moment the query is set, so it is cleared once the panel has set it
        // (setting the unchanged query is what clears it). A flag read per editor notification.
        cx.observe(&editor, |_, editor, cx| {
            let anchored = {
                let session = editor.read(cx).search_session();
                (session.open && session.anchor_offset.is_some())
                    .then(|| (session.query.clone(), session.case_insensitive))
            };
            if let Some((query, case_insensitive)) = anchored {
                editor.update(cx, |state, cx| state.set_search_query(query, case_insensitive, cx));
            }
        })
        .detach();
        self.query_editors.by_tab.insert(tab_id, editor.clone());
        editor
    }

    /// Drops editor entities when their tab closes.
    pub(super) fn editor_for_tab_removed(&mut self, tab_id: u64) {
        self.cancel_format_of(tab_id);
        self.query_editors.by_tab.remove(&tab_id);
        self.script_toolbar.remove_tab(tab_id);
        self.sql_table_ui.remove_tab(tab_id);
        self.redis_ui.tab_removed(tab_id);
        self.mongo_ui.filter_editors.remove(&tab_id);
        self.mongo_ui.sort_editors.remove(&tab_id);
        self.mongo_ui.column_search_editors.remove(&tab_id);
        self.columns_filter_inputs.remove(&tab_id);
        self.connection_overviews.remove(&tab_id);
        self.properties_edits.remove(&tab_id);
        self.source_tabs.remove(&tab_id);
        self.imports.remove(&tab_id);
        self.transfers.remove(&tab_id);
        self.file_runs.remove(&tab_id);
        self.cancel_file_run_of_tab(tab_id);
        self.backups.remove(&tab_id);
        self.accounts.remove(&tab_id);
        self.process_lists.remove(&tab_id);
        self.cancel_backup_of_tab(tab_id);
        self.transfer_create.remove(&tab_id);
        self.mongo_ui.expanded_find_editors.remove(&tab_id);
        self.mongo_ui.expanded_sort_editors.remove(&tab_id);
        self.mongo_ui
            .filter_field_editors
            .retain(|(editor_tab_id, _), _| *editor_tab_id != tab_id);
        self.mongo_ui
            .filter_value_editors
            .retain(|(editor_tab_id, _), _| *editor_tab_id != tab_id);
    }

    pub(super) fn confirm_close_tab(
        &mut self,
        tab_id: u64,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        // An open transaction is asked about first; once it ended, closing goes on from the start
        // (the questions about files and staged changes follow).
        if self.guard_transactions(
            vec![tab_id],
            "Closing the tab ends its session.",
            window,
            cx,
            move |this, window, cx| this.confirm_close_tab(tab_id, window, cx),
        ) {
            return;
        }
        if self
            .workspace
            .tab(tab_id)
            .is_some_and(|tab| tab.kind == TabKind::Settings)
            && self.settings_dirty()
        {
            self.open_discard_settings_dialog(tab_id, window, cx);
            return;
        }
        // A file's own changes are asked about whatever the "confirm closing" setting says: they
        // are on disk only once saved.
        if self.workspace.tab(tab_id).is_some_and(Tab::has_unsaved_file) {
            self.open_unsaved_file_dialog(tab_id, window, cx);
            return;
        }
        if self.is_unsaved_script(tab_id) && self.settings.editor.confirm_unsaved_close {
            self.open_unsaved_script_dialog(tab_id, window, cx);
            return;
        }
        if self.transfer_running(tab_id, cx) {
            self.open_discard_dialog(
                "Cancel the transfer?".to_owned(),
                "The transfer is still running. Closing the tab cancels it; rows already committed stay in their tables.".to_owned(),
                move |this, _, cx| {
                    this.close_tab(tab_id, cx);
                    cx.notify();
                },
                window,
                cx,
            );
            return;
        }
        if self.file_run_running(tab_id, cx) {
            self.open_discard_dialog(
                "Cancel the run?".to_owned(),
                "The SQL file is still running. Closing the tab cancels it: a statement that is running is interrupted, an open transaction is rolled back, and statements already committed stay.".to_owned(),
                move |this, _, cx| {
                    this.close_tab(tab_id, cx);
                    cx.notify();
                },
                window,
                cx,
            );
            return;
        }
        if self.backup_running(tab_id, cx) {
            let restore = self.workspace.tab(tab_id).is_some_and(|tab| tab.kind == TabKind::Restore);
            let (title, body) = if restore {
                ("Cancel the restore?", "The restore is still running. Closing the tab cancels it: the tool is stopped, and what it had restored stays in the database.")
            } else {
                ("Cancel the backup?", "The backup is still running. Closing the tab cancels it: the tool is stopped and the unfinished file is removed.")
            };
            self.open_discard_dialog(
                title.to_owned(),
                body.to_owned(),
                move |this, _, cx| {
                    this.close_tab(tab_id, cx);
                    cx.notify();
                },
                window,
                cx,
            );
            return;
        }
        if self.import_running(tab_id, cx) {
            self.open_discard_dialog(
                "Cancel the import?".to_owned(),
                "The import is still loading. Closing the tab cancels it; rows already committed stay in the table.".to_owned(),
                move |this, _, cx| {
                    this.close_tab(tab_id, cx);
                    cx.notify();
                },
                window,
                cx,
            );
            return;
        }
        // An open document editor is text that was not saved: it counts as a staged change.
        let staged = self.workspace.tab_has_staged_changes(tab_id)
            || self.view(tab_id).mongo.document_edit.is_some();
        let drafts = self.structure_draft_count(tab_id, cx);
        if !staged && drafts == 0 {
            self.close_tab(tab_id, cx);
            cx.notify();
            return;
        }
        let (title, body) = match (staged, drafts) {
            (true, 0) => ("Discard pending changes?".to_owned(), "This tab has staged changes that have not been committed.".to_owned()),
            (false, drafts) => (
                format!("Discard {}?", structure_changes(drafts)),
                "The structure changes of this table have not been applied. Closing the tab drops them.".to_owned(),
            ),
            (true, drafts) => (
                "Discard pending changes?".to_owned(),
                format!("This tab has staged changes that have not been committed and {} that have not been applied.", structure_changes(drafts)),
            ),
        };
        self.open_discard_dialog(title, body, move |this, _, cx| {
            this.close_tab(tab_id, cx);
            cx.notify();
        }, window, cx);
    }

    /// Pending structure changes of the Properties edit mode of `tab_id`.
    pub(super) fn structure_draft_count(&self, tab_id: u64, cx: &gpui::App) -> usize {
        self.properties_edits.get(&tab_id).map_or(0, |edit| edit.read(cx).pending_count())
    }

    /// Pending structure changes of every tab.
    fn structure_draft_total(&self, cx: &gpui::App) -> usize {
        self.properties_edits.values().map(|edit| edit.read(cx).pending_count()).sum()
    }

    /// "Discard ...?" with Discard and Cancel; `discard` runs on Discard.
    pub(super) fn open_discard_dialog(
        &mut self,
        title: String,
        body: String,
        discard: impl Fn(&mut Self, &mut Window, &mut Context<Self>) + 'static,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let root = cx.entity().downgrade();
        let discard = std::rc::Rc::new(discard);
        window.open_alert_dialog(cx, move |dialog, _, _| {
            let (root, discard) = (root.clone(), discard.clone());
            dialog
                .width(px(440.0))
                .title(title.clone())
                .child(div().debug_selector(|| "discard-pending-body".into()).child(body.clone()))
                .button_props(gpui_component::dialog::DialogButtonProps::default().ok_text("Discard"))
                .confirm()
                .footer(
                    gpui_component::dialog::DialogFooter::new()
                        .child(
                            div()
                                .debug_selector(|| "discard-pending-cancel".into())
                                .child(gpui_component::dialog::DialogClose::new().trigger(|button| button.label("Cancel"))),
                        )
                        .child(
                            div().debug_selector(|| "discard-pending-confirm".into()).child(
                                gpui_component::dialog::DialogAction::new().child(Button::new("discard-pending").label("Discard").danger()),
                            ),
                        ),
                )
                .on_ok(move |_, window, cx| {
                    let _ = root.update(cx, |this, cx| discard(this, window, cx));
                    true
                })
        });
    }

    /// Closing the window asks first when a table's Properties has structure changes that were not
    /// applied. Installed once, on the first frame.
    pub(super) fn guard_window_close(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if std::mem::replace(&mut self.shell.close_guarded, true) {
            return;
        }
        // However the application ends (the window's close box, Quit with no window): the tools of running backups are stopped and cleaned up after.
        cx.on_app_quit(|this, cx| this.backups_stop_for_quit(cx)).detach();
        let root = cx.entity().downgrade();
        window.on_window_should_close(cx, move |window, cx| {
            root.update(cx, |this, cx| this.window_may_close(window, cx)).unwrap_or(true)
        });
    }

    /// The window's close check: open transactions are asked about first, then structure changes
    /// that were not applied. `true`: nothing to ask, the window closes. When a question is opened
    /// the answer is `false`, and whatever it leads to runs this whole check again, so that the
    /// next question is not skipped.
    pub(super) fn window_may_close(&mut self, window: &mut Window, cx: &mut Context<Self>) -> bool {
        if self.guard_app_exit("Closing the window ends their sessions.", window, cx, |this, window, cx| {
            if this.window_may_close(window, cx) {
                window.remove_window();
            }
        }) {
            return false;
        }
        // A backup or a restore that runs: asked, then stopped, and its private files and unfinished backup are gone before the window is.
        if self.guard_backups("Closing the window cancels it.", window, cx, |this, window, cx| {
            if this.window_may_close(window, cx) {
                window.remove_window();
            }
        }) {
            return false;
        }
        let drafts = self.structure_draft_total(cx);
        let (title, body) = match (drafts, self.document_edits_open()) {
            (0, 0) => return true,
            (0, _) => (
                "Discard the document edit?".to_owned(),
                "The edited document has not been saved. Closing the window drops it.".to_owned(),
            ),
            (drafts, _) => (
                format!("Discard {}?", structure_changes(drafts)),
                "They have not been applied. Closing the window drops them.".to_owned(),
            ),
        };
        self.open_discard_dialog(title, body, |_, window, _| window.remove_window(), window, cx);
        false
    }

    /// How many Mongo document editors hold text that was not saved.
    fn document_edits_open(&self) -> usize {
        self.tab_views
            .values()
            .filter(|view| view.mongo.document_edit.is_some())
            .count()
    }

    /// Quit: asks first when a table's Properties has structure changes that were not applied.
    /// `false`: nothing to ask, the caller quits.
    pub(super) fn request_quit(&mut self, window: &mut Window, cx: &mut Context<Self>) -> bool {
        // Open transactions first; quitting is asked for again once they ended.
        if self.guard_app_exit("Quitting ends their sessions.", window, cx, |this, window, cx| {
            if !this.request_quit(window, cx) {
                cx.quit();
            }
        }) {
            return true;
        }
        if self.guard_backups("Quitting cancels it.", window, cx, |this, window, cx| {
            if !this.request_quit(window, cx) {
                cx.quit();
            }
        }) {
            return true;
        }
        let drafts = self.structure_draft_total(cx);
        let (title, body) = match (drafts, self.document_edits_open()) {
            (0, 0) => return false,
            (0, _) => (
                "Discard the document edit?".to_owned(),
                "The edited document has not been saved. Quitting drops it.".to_owned(),
            ),
            (drafts, _) => (
                format!("Discard {}?", structure_changes(drafts)),
                "They have not been applied. Quitting drops them.".to_owned(),
            ),
        };
        self.open_discard_dialog(title, body, |_, _, cx| cx.quit(), window, cx);
        true
    }

    /// The user closes a tab: it keeps what "Reopen Closed Tab" needs to bring it back.
    pub(super) fn close_tab(&mut self, tab_id: u64, cx: &mut Context<Self>) {
        // Taken before anything is torn down: the unsaved text and staged filters are the tab's.
        if let Some(saved) = self.workspace.closed_snapshot(tab_id) {
            let closed = &mut self.shell.closed_tabs;
            if closed.len() == CLOSED_TABS_KEPT {
                closed.pop_front();
            }
            closed.push_back(saved);
        }
        self.close_tab_unremembered(tab_id, cx);
    }

    /// The app closes a tab the user did not (its object was dropped, its connection deleted, the
    /// start-up restore keeps only pinned ones): "Reopen Closed Tab" does not bring it back.
    pub(super) fn close_tab_unremembered(&mut self, tab_id: u64, cx: &mut Context<Self>) {
        self.auto_refresh.tasks.remove(&tab_id);
        self.query_tasks.started_at.remove(&tab_id);
        if self
            .shell
            .tab_bar
            .rename
            .as_ref()
            .is_some_and(|rename| rename.tab_id == tab_id)
        {
            self.shell.tab_bar.rename = None;
        }
        self.cancel_tab_query(tab_id, cx);
        self.release_driver_page(tab_id);
        // Its own session (manual commit mode) closes; the server rolls back what was open.
        self.tab_transaction_closed(tab_id);
        self.diagram_tab_removed(tab_id);
        if self
            .workspace
            .tab(tab_id)
            .is_some_and(|tab| tab.kind == TabKind::Settings)
        {
            // The draft goes with the tab.
            self.settings_ui.page = None;
            // The model list a closed Settings page asked for is not wanted any more.
            self.assistant_cancel_models_refresh();
        }
        self.settings_ui.saved_scripts.remove(&tab_id);
        self.workspace.close_tab(tab_id);
        self.editor_for_tab_removed(tab_id);
        self.aggregate_tab_removed(tab_id, cx);
        // The tab's whole result view (scroll, selection, find, editors) goes with it.
        self.tab_views.remove(&tab_id);
    }

    /// "Reopen Closed Tab": brings back the tab closed last, in the focused pane (or shows the open
    /// tab it would duplicate). A tab whose connection is gone is dropped and the one closed before
    /// it is tried. Nothing to reopen does nothing.
    pub(super) fn reopen_closed_tab(&mut self, cx: &mut Context<Self>) {
        while let Some(saved) = self.shell.closed_tabs.pop_back() {
            if !saved.connection_id.is_empty()
                && !self.saved_connections.iter().any(|connection| connection.matches_id(&saved.connection_id))
            {
                continue;
            }
            // Already open (the same file, Welcome, the same table...): that one, not a twin.
            if let Some(open) = self.workspace.open_twin(&saved) {
                self.activate_tab_ui(open, cx);
                return;
            }
            let Some(tab_id) = self.workspace.reopen_tab(&saved) else {
                continue;
            };
            // The data comes from the server again, never from the rows the tab held. SQL typed
            // in an editor is not run: reopening must not execute a statement.
            if self.workspace.tab(tab_id).is_some_and(session_persist::is_data_tab) {
                self.run_tab_query_explicit(tab_id, cx);
            } else if saved.kind == "source" {
                self.ensure_source_loaded(tab_id, cx);
            }
            cx.notify();
            return;
        }
    }

    pub(super) fn finish_query_task(&mut self, tab_id: u64, run: crate::query::QueryRunId) {
        if self.query_tasks.registry.clear(tab_id, run) {
            self.query_tasks.tasks.remove(&tab_id);
            self.driver_runtime.query_operations.remove(&tab_id);
        }
    }

    pub(super) fn finish_query_timing(&mut self, tab_id: u64, run: crate::query::QueryRunId) {
        let Some((started_run, started_at)) = self.query_tasks.started_at.remove(&tab_id) else {
            return;
        };
        if started_run != run {
            return;
        }
        if let Some(runtime) = self.workspace.runtime_mut(tab_id) {
            runtime.last_query_ms =
                Some(started_at.elapsed().as_millis().min(u64::MAX as u128) as u64);
        }
    }

    /// The connection of `connection` opened: starts every run queued for it.
    /// A run of an older connect attempt, or one that is no longer the tab's
    /// live run, is dropped.
    pub(super) fn resume_pending_runs(&mut self, connection: &str, cx: &mut Context<Self>) {
        self.resume_waiting_sources(connection, cx);
        let tabs: Vec<u64> = self
            .query_tasks
            .pending_runs
            .iter()
            .filter_map(|(id, pending)| (pending.connection == connection).then_some(*id))
            .collect();
        for tab_id in tabs {
            let Some(pending) = self.query_tasks.pending_runs.remove(&tab_id) else {
                continue;
            };
            if self.sessions.generation(connection) != Some(pending.epoch) {
                self.fail_pending_run(
                    tab_id,
                    &pending,
                    "Connection changed while connecting; run again",
                    cx,
                );
                continue;
            }
            let PendingRun { run, kind, .. } = pending;
            match kind {
                PendingRunKind::MongoScript { sql, selection } => {
                    self.resume_pending_mongo_script(tab_id, connection, run, (sql, selection), cx);
                }
                PendingRunKind::Sql(request) => self.resume_pending_sql(run, request, cx),
            }
        }
    }

    /// The connect attempt `epoch` of `connection` failed or was replaced:
    /// every run queued on it fails with `reason`.
    pub(super) fn discard_pending_runs(
        &mut self,
        connection: &str,
        epoch: u64,
        reason: &str,
        cx: &mut Context<Self>,
    ) {
        self.fail_waiting_sources(connection, epoch, reason, cx);
        let tabs: Vec<u64> = self
            .query_tasks
            .pending_runs
            .iter()
            .filter_map(|(id, queued)| {
                (queued.connection == connection && queued.epoch == epoch).then_some(*id)
            })
            .collect();
        for tab_id in tabs {
            if let Some(pending) = self.query_tasks.pending_runs.remove(&tab_id) {
                self.fail_pending_run(tab_id, &pending, reason, cx);
            }
        }
    }

    fn fail_pending_run(
        &mut self,
        tab_id: u64,
        pending: &PendingRun,
        reason: &str,
        cx: &mut Context<Self>,
    ) {
        match pending.kind {
            PendingRunKind::MongoScript { .. } => self.fail_mongo_script(tab_id, reason, cx),
            PendingRunKind::Sql(_) => self.fail_pending_sql(tab_id, pending.run, reason, cx),
        }
    }

    /// Records a successful SQL run in the shared query history. The SQL text
    /// lands in the legacy `filter` column, matching how Swift history entries
    /// are keyed per connection context. The write happens off the UI thread;
    /// tests keep the entries in memory instead of touching the user's history
    /// database.
    pub(super) fn record_query_history(&mut self, run: &SqlRun, rows: i64, cx: &mut Context<Self>) {
        #[cfg(test)]
        {
            let _ = &cx;
            self.recorded_history.push(RecordedHistory {
                connection: run.connection.clone(),
                database: run.database.clone(),
                collection: run.collection.clone(),
                sql: run.sql.clone(),
                rows,
            });
        }
        #[cfg(not(test))]
        {
            let run = run.clone();
            cx.background_executor()
                .spawn(async move {
                    let _ = engine_storage::history::save_entry(
                        &run.connection,
                        &run.database,
                        &run.collection,
                        &run.sql,
                        "",
                        "",
                        0,
                        run.limit,
                        0,
                        rows,
                    );
                })
                .detach();
        }
    }

    pub(super) fn cancel_tab_query(&mut self, tab_id: u64, cx: &mut Context<Self>) {
        // A MongoDB shell script waiting for its plugin is forgotten; a running one is stopped.
        self.stop_shell_run(tab_id, cx);
        if self.query_tasks.pending_runs.remove(&tab_id).is_some() {
            if let Some(runtime) = self.workspace.runtime_mut(tab_id) {
                runtime.cancel_sql_run();
            }
            cx.notify();
            return;
        }
        if self.scenario_ui.service.is_some() {
            if let Some(runtime) = self.workspace.runtime_mut(tab_id) {
                runtime.cancel_sql_run();
            }
            self.query_tasks.tasks.remove(&tab_id);
            self.query_tasks.registry.remove_tab(tab_id);
            self.query_tasks.started_at.remove(&tab_id);
            cx.notify();
            return;
        }
        if self.workspace.tab(tab_id).is_none() {
            self.query_tasks.tasks.remove(&tab_id);
            self.query_tasks.registry.remove_tab(tab_id);
            return;
        }
        let applying = self.workspace.runtime(tab_id).is_some_and(|runtime| {
            runtime.sql_apply_running
                || runtime.mongo.commit_status == crate::mongo_result::MongoCommitStatus::Running
        });
        // A script keeps what it produced: the statement running is cancelled, the rest skipped.
        let script = self.cancel_script(tab_id);
        let run = self
            .workspace
            .runtime_mut(tab_id)
            .and_then(crate::workspace::TabRuntime::cancel_sql_run);
        self.query_tasks.tasks.remove(&tab_id);
        self.query_tasks.registry.remove_tab(tab_id);
        let driver_operation = self.driver_runtime.query_operations.remove(&tab_id);

        let Some(_run) = run else {
            // A staged-changes Apply holds a driver operation but no query run;
            // cancelling it ends as `SqlApplyError::Cancelled`.
            if let (true, Some(operation)) = (applying, driver_operation) {
                cancel_driver_operation(operation, cx);
            }
            return;
        };
        // A run is cancelled through its host-allocated operation.
        if let Some(operation) = driver_operation {
            cancel_driver_operation(operation, cx);
        }
        self.tx_run_cancelled(tab_id, cx);
        if let Some(script) = script {
            self.land_script(tab_id, None, script, cx);
        }
    }

    /// Cancels the active tab's in-flight run without affecting other tabs.
    pub(super) fn cancel_active_tab_query(&mut self, cx: &mut Context<Self>) {
        let Some(tab_id) = self.workspace.active_tab().map(|tab| tab.id) else {
            return;
        };
        self.cancel_tab_query(tab_id, cx);
        cx.notify();
    }

    #[cfg(test)]
    pub(super) fn next_query_page(&mut self, cx: &mut Context<Self>) {
        if let Some(tab_id) = self.workspace.active_tab().map(|tab| tab.id) {
            self.page_tab_query(tab_id, true, cx);
        }
    }

    #[cfg(test)]
    pub(super) fn previous_query_page(&mut self, cx: &mut Context<Self>) {
        if let Some(tab_id) = self.workspace.active_tab().map(|tab| tab.id) {
            self.page_tab_query(tab_id, false, cx);
        }
    }

    /// Moves the tab's result one page. A SQL tab re-sends exactly the
    /// statement on screen (`last_executed_query`, never the editor text, which
    /// may hold anything typed since) for the neighbouring page; with no
    /// executed statement there is nothing to page. Mongo tabs and the scenario
    /// fixture keep their own query state and re-run through the common path.
    pub(super) fn page_tab_query(&mut self, tab_id: u64, forward: bool, cx: &mut Context<Self>) {
        // A page that is still loading: another click (or key) waits for it
        // instead of moving the page counter twice.
        if self.query_tasks.tasks.contains_key(&tab_id) {
            return;
        }
        self.page_tab_query_inner(tab_id, forward, cx);
        if let Some((run, _)) = self.query_tasks.started_at.get(&tab_id) {
            self.query_tasks.paging.insert(tab_id, (*run, forward));
        }
    }

    fn page_tab_query_inner(&mut self, tab_id: u64, forward: bool, cx: &mut Context<Self>) {
        if self.refuse_while_editing_mongo_document(tab_id, cx) {
            return;
        }
        let Some(tab) = self.workspace.tab(tab_id) else {
            return;
        };
        if tab.query_mode.is_mongo() || self.scenario_ui.service.is_some() {
            let moved = self
                .workspace
                .runtime_mut(tab_id)
                .map(|runtime| {
                    if forward {
                        runtime.pagination.next()
                    } else {
                        runtime.pagination.previous()
                    }
                })
                .unwrap_or(false);
            if moved {
                self.run_tab_query(tab_id, cx);
            }
            return;
        }
        let Some((sql, page)) = self.workspace.runtime(tab_id).and_then(|runtime| {
            let page = if forward {
                runtime.pagination.next_page()
            } else {
                runtime.pagination.previous_page()
            };
            let sql = runtime
                .last_executed_query
                .clone()
                .filter(|sql| !sql.is_empty());
            Some((sql?, page?))
        }) else {
            return;
        };
        if self.refuse_run_over_staged_changes(tab_id, cx) {
            return;
        }
        self.run_sql(tab_id, sql, Some(page), cx);
    }

    /// Re-runs the tab's result on screen: footer Refresh, a Limit change, the
    /// refresh after Apply, auto-refresh. A SQL editor tab re-sends the
    /// statement that produced the rows (`last_executed_query`), never the
    /// editor text, which may hold anything typed since; with nothing executed
    /// there is nothing to re-run. `restart` fetches the first page instead of
    /// the shown one. Table-data, Mongo and scenario tabs build their own
    /// statement and re-run through `run_tab_query`.
    pub(super) fn rerun_shown_result(
        &mut self,
        tab_id: u64,
        restart: bool,
        cx: &mut Context<Self>,
    ) {
        let Some(tab) = self.workspace.tab(tab_id) else {
            return;
        };
        // Whatever asks (Cmd+R, the context menu, an edit, auto refresh): the script behind a shell
        // result is only ever run again by Run, which asks again where the connection wants it.
        if self.shell_view(tab_id).is_some() {
            return;
        }
        if tab.query_mode.is_mongo()
            || tab.query_mode.is_table_data()
            || self.scenario_ui.service.is_some()
        {
            self.run_tab_query(tab_id, cx);
            return;
        }
        let Some((sql, page)) = self.workspace.runtime(tab_id).and_then(|runtime| {
            let sql = runtime
                .last_executed_query
                .clone()
                .filter(|sql| !sql.is_empty())?;
            Some((
                sql,
                if restart {
                    0
                } else {
                    runtime.pagination.page()
                },
            ))
        }) else {
            return;
        };
        if self.refuse_run_over_staged_changes(tab_id, cx) {
            return;
        }
        self.run_sql(tab_id, sql, Some(page), cx);
    }

    /// The unsorted statement behind the rows on screen: the base of an active
    /// context-menu sort, else the last executed statement.
    pub(super) fn shown_sql_base(&self, tab_id: u64) -> Option<String> {
        let runtime = self.workspace.runtime(tab_id)?;
        runtime
            .context_sql_sort
            .as_ref()
            .map(|sort| sort.base.clone())
            .or_else(|| runtime.last_executed_query.clone())
            .filter(|sql| !sql.is_empty())
    }

    /// A tab with staged SQL changes does not run another query over them
    /// (the new result would replace the rows they were staged on). `true`
    /// when the run was refused.
    pub(super) fn refuse_run_over_staged_changes(
        &mut self,
        tab_id: u64,
        cx: &mut Context<Self>,
    ) -> bool {
        let staged = self.workspace.tab(tab_id).is_some_and(|tab| {
            !tab.query_mode.is_mongo_find() && self.workspace.tab_has_staged_changes(tab.id)
        });
        if staged {
            self.view_mut(tab_id).detail_ui.error =
                Some("Apply or discard staged SQL changes before running another query.".into());
            cx.notify();
        }
        staged
    }

    /// Runs the active tab's SQL (Run button, palette, Cmd+Enter).
    pub(super) fn run_active_tab_query(&mut self, cx: &mut Context<Self>) {
        if let Some(tab_id) = self.workspace.active_tab().map(|tab| tab.id) {
            self.run_tab_query_explicit(tab_id, cx);
        }
    }

    /// Runs `tab_id` because the user asked for it (Run, Cmd+Enter, a history
    /// entry): a script tab runs its editor text, a shell script included.
    pub(super) fn run_tab_query_explicit(&mut self, tab_id: u64, cx: &mut Context<Self>) {
        self.run_tab_query_as(tab_id, true, cx);
    }

    /// Re-runs `tab_id`'s result: paging, Refresh, a Limit change, a staged
    /// edit's reload, a reconnect. A script tab re-runs only the read that is on
    /// screen, never the editor text and never a shell script.
    pub(super) fn run_tab_query(&mut self, tab_id: u64, cx: &mut Context<Self>) {
        self.run_tab_query_as(tab_id, false, cx);
    }

    /// Runs `tab_id`'s SQL. Falls back to the smoke demo when the tab has no
    /// connection bound (the Welcome/standalone tabs).
    fn run_tab_query_as(&mut self, tab_id: u64, explicit: bool, cx: &mut Context<Self>) {
        if self.refuse_while_editing_mongo_document(tab_id, cx)
            || self.refuse_run_over_staged_changes(tab_id, cx)
        {
            return;
        }
        // A Redis key tab re-reads the key it shows (page, refresh, after an edit).
        if self.scenario_ui.service.is_none() {
            if let Some(key_raw) = self
                .workspace
                .tab(tab_id)
                .filter(|tab| tab.query_mode.is_redis_key())
                .map(|tab| tab.collection.clone())
            {
                self.run_sql(tab_id, key_raw, None, cx);
                return;
            }
        }
        if self
            .workspace
            .tab(tab_id)
            .is_some_and(|tab| tab.query_mode.is_mongo_script())
        {
            if explicit {
                self.run_mongo_script_tab(tab_id, cx);
            } else {
                self.rerun_mongo_script_tab(tab_id, cx);
            }
            return;
        }
        // Paging / Refresh of an Aggregate tab re-run its pipeline; the Run
        // button itself goes through `run_aggregate` (validation, confirm).
        if self
            .workspace
            .tab(tab_id)
            .is_some_and(|tab| tab.query_mode.is_mongo_aggregate())
        {
            self.rerun_aggregate_page(tab_id, cx);
            return;
        }
        // `sqlTable` tabs build their statement from WHERE / ORDER BY; an
        // invalid fragment is reported in the bar and nothing runs.
        let table_sql = match self
            .workspace
            .tab(tab_id)
            .filter(|tab| tab.query_mode.is_table_data())
            .map(|tab| self.sql_table_statement(tab))
        {
            Some(Err(error)) => {
                if let Some(runtime) = self.workspace.runtime_mut(tab_id) {
                    runtime.sql_table.error = Some(error);
                }
                cx.notify();
                return;
            }
            Some(Ok(sql)) => {
                if let Some(runtime) = self.workspace.runtime_mut(tab_id) {
                    runtime.sql_table.error = None;
                }
                Some(sql)
            }
            None => None,
        };
        if self.scenario_ui.service.is_some() {
            self.run_scenario_query(tab_id, cx);
            return;
        }
        let Some(tab) = self.workspace.tab(tab_id) else {
            return;
        };
        let tab_connection = tab.connection_id.clone();
        let tab_sql = tab.sql.clone();
        let tab_database = tab.database.clone();
        let tab_collection = tab.collection.clone();

        if tab.query_mode.is_mongo_find() {
            let (tab_filter, tab_sort, tab_project) = self
                .workspace
                .runtime(tab_id)
                .map(|runtime| {
                    let filter = runtime
                        .mongo
                        .applied_filter
                        .as_ref()
                        .and_then(|filter| serde_json::to_string(filter).ok())
                        .unwrap_or_else(|| runtime.mongo.filter.clone());
                    (
                        filter,
                        runtime.mongo.sort.clone(),
                        runtime.mongo.project.clone(),
                    )
                })
                .unwrap_or_else(|| ("{}".into(), r#"{"_id":1}"#.into(), String::new()));
            self.run_active_mongo_query(
                tab_id,
                tab_connection,
                tab_database,
                tab_collection,
                tab_filter,
                tab_sort,
                tab_project,
                None,
                None,
                cx,
            );
            return;
        }

        // An empty editor has nothing to run.
        if table_sql.is_none() && tab_sql.trim().is_empty() {
            return;
        }
        // SQL the user typed, as opposed to a table-data tab's own statement.
        let typed_in_editor = table_sql.is_none();
        let sorted = self
            .workspace
            .runtime(tab_id)
            .and_then(|runtime| runtime.context_sql_sort.as_ref())
            .filter(|sort| sort.base == tab_sql)
            .map(|sort| sort.sorted.clone());
        let (sql, from_scope) = match (table_sql, sorted) {
            (Some(table_sql), _) => (table_sql, false),
            (None, Some(sorted)) => (sorted, false),
            (None, None) => (self.sql_in_run_scope(tab_id, tab_sql, cx), true),
        };
        // The text the user typed may have `:name` / `${name}` parameters: ask for them first.
        if typed_in_editor && from_scope && self.prompt_parameters(tab_id, &sql, AfterParams::Run, cx) {
            return;
        }
        self.run_typed_sql(tab_id, sql, typed_in_editor, cx);
    }

    /// `sql` is the editor's text with its parameters bound. Statements typed in the editor honour
    /// the connection type: refused on a read-only connection, confirmed first on a production
    /// one.
    pub(super) fn run_typed_sql(&mut self, tab_id: u64, sql: String, typed_in_editor: bool, cx: &mut Context<Self>) {
        if typed_in_editor && self.gate_editor_statement(tab_id, &sql, cx) {
            return;
        }
        if typed_in_editor {
            self.run_editor_sql(tab_id, sql, cx);
        } else {
            self.run_sql(tab_id, sql, None, cx);
        }
    }

    /// The part of the editor's script that Run executes (the "Run executes"
    /// setting). A selection wins unless the setting says the whole script;
    /// without one, the statement at the cursor or the whole script.
    pub(super) fn sql_in_run_scope(&self, tab_id: u64, script: String, cx: &gpui::App) -> String {
        use crate::settings::RunScope;
        let scope = self.settings.editor.run_scope;
        if scope == RunScope::All {
            return script;
        }
        let Some(editor) = self.query_editors.by_tab.get(&tab_id) else {
            return script;
        };
        let (selection, cursor) = {
            let state = editor.read(cx);
            (state.selected_value().to_string(), state.cursor())
        };
        if !selection.trim().is_empty() {
            return selection;
        }
        // A Redis console runs one command per line: the line at the cursor.
        if self
            .workspace
            .tab(tab_id)
            .is_some_and(|tab| tab.query_mode.is_redis_command())
        {
            return crate::redis::line_at(&script, cursor).unwrap_or(script);
        }
        match scope {
            RunScope::Current => crate::sql_script::statement_at(&script, cursor, &self.sql_lex(tab_id))
                .map_or(script.clone(), |range| script[range].to_string()),
            RunScope::Selection | RunScope::All => script,
        }
    }

    /// Starts `sql` on the tab's driver as a new run. `page` is the page to
    /// fetch; `None` (an editor run) keeps the shown page for the statement on
    /// screen and starts any other statement from its first page. The shown
    /// page and statement change only when the result lands.
    pub(super) fn run_sql(
        &mut self,
        tab_id: u64,
        sql: String,
        page: Option<u64>,
        cx: &mut Context<Self>,
    ) {
        self.start_sql_run(tab_id, sql, page, None, cx);
    }

    /// `run_sql`, or an Explain run (`explain`) of an EXPLAIN statement: the same driver route,
    /// session, cancel and failure handling, but one unpaged read that never touches the tab's
    /// result rows, pagination or last executed statement.
    pub(super) fn start_sql_run(
        &mut self,
        tab_id: u64,
        sql: String,
        page: Option<u64>,
        explain: Option<drivers::ExplainRun>,
        cx: &mut Context<Self>,
    ) {
        self.start_run(tab_id, sql, page, explain, None, cx);
    }

    /// `start_sql_run`, or the run of a script: `script` is its statements, `sql` the first of
    /// them. A script is one run of the tab that sends its statements one after the other (see
    /// `script_run`).
    pub(super) fn start_run(
        &mut self,
        tab_id: u64,
        sql: String,
        page: Option<u64>,
        explain: Option<drivers::ExplainRun>,
        script: Option<Vec<crate::script_run::PlannedStatement>>,
        cx: &mut Context<Self>,
    ) {
        let Some(tab) = self.workspace.tab(tab_id) else {
            return;
        };
        let tab_connection = tab.connection_id.clone();
        let tab_collection = tab.collection.clone();
        let database = tab.database.clone();
        let schema = tab.schema.clone();
        let limit = if explain.is_some() {
            drivers::EXPLAIN_ROW_LIMIT
        } else {
            tab.output_limit
        };
        let run_kind = explain.map_or_else(|| RunKind::of_tab(tab), RunKind::Explain);
        // The UI scenario answers Redis from its fixture server (after the gate).
        if self.scenario_ui.service.is_some() && tab.query_mode.is_redis() {
            let tab = tab.clone();
            self.run_scenario_redis(tab, false, cx);
            return;
        }
        // A dropped session keeps its dead handle until released; do it before
        // this run begins, because releasing cancels the connection's runs.
        if matches!(
            self.sessions.status(&tab_connection),
            ConnectionStatus::Failed(_)
        ) && self.driver_runtime.sessions.contains_key(&tab_connection)
        {
            self.release_dead_session(&tab_connection, cx);
        }
        let (run, offset) = {
            let Some(runtime) = self.workspace.runtime_mut(tab_id) else {
                return;
            };
            if runtime.mongo.has_pending_mutations() {
                return;
            }
            let Some(run) = runtime.query.begin() else {
                return;
            };
            // A different statement starts from its first page: a driver
            // refuses `offset > 0` for anything but a pageable read.
            let page = page.unwrap_or_else(|| {
                if runtime.last_executed_query.as_deref() == Some(sql.as_str()) {
                    runtime.pagination.page()
                } else {
                    0
                }
            });
            if explain.is_none() && script.is_none() {
                runtime.pagination.begin_page(page);
                runtime.pending_sql = Some(sql.clone());
            }
            // The run's state says what it ran and where ("Fix" sends this with the failure).
            runtime.query.describe(
                run,
                crate::query::RunContext {
                    sql: sql.clone(),
                    connection: tab_connection.clone(),
                    database: database.clone(),
                    schema,
                },
            );
            (run, if explain.is_some() || script.is_some() { 0 } else { runtime.pagination.offset_of(page) })
        };
        let reason = if tab_connection.is_empty() {
            Some("This tab has no connection; open a query from a connection in the sidebar")
        } else if !self
            .saved_connections
            .iter()
            .any(|saved| saved.matches_id(&tab_connection))
        {
            Some("Saved connection not found")
        } else {
            None
        };
        if let Some(reason) = reason {
            self.fail_pending_sql(tab_id, run, reason, cx);
            return;
        }
        // A read-only connection also has the driver refuse writes (its server-side read-only run),
        // behind the app's own check, for a driver that advertises it.
        let read_only = matches!(run_kind, RunKind::Sql | RunKind::Explain(_))
            && self.effective_for(&tab_connection).read_only
            && !self.explain_driver_lacks_read_only(tab_id);
        let request = SqlRun {
            tab_id,
            kind: run_kind,
            connection: tab_connection,
            sql,
            database,
            collection: tab_collection,
            limit,
            offset,
            confirmed: script.as_ref().and_then(|plan| plan.first()).is_some_and(|first| first.confirmed),
            read_only,
        };
        if let Some(plan) = script {
            self.begin_script(run, &request, plan);
        }
        // A connection that never opened, or whose session dropped, opens
        // first and runs once it is up. A session the computer slept under counts as dropped.
        self.reopen_if_stale(&request.connection);
        let needs_connect = !self
            .driver_runtime
            .sessions
            .contains_key(&request.connection)
            || matches!(
                self.sessions.status(&request.connection),
                ConnectionStatus::Failed(_)
            );
        if needs_connect {
            self.connect_then_run_sql(run, request, cx);
        } else {
            self.start_driver_sql_run(run, request, cx);
        }
    }

    pub(super) fn cancel_auto_refresh_for_connection(&mut self, connection_id: &str) {
        for tab_id in self
            .workspace
            .tabs()
            .iter()
            .filter(|tab| tab.connection_id == connection_id)
            .map(|tab| tab.id)
            .collect::<Vec<_>>()
        {
            self.auto_refresh.tasks.remove(&tab_id);
        }
    }

    fn tab_is_connected(&self, tab_id: u64) -> bool {
        let Some(tab) = self.workspace.tabs().iter().find(|tab| tab.id == tab_id) else {
            return false;
        };
        tab.connection_id.is_empty()
            || self
                .driver_runtime
                .sessions
                .contains_key(&tab.connection_id)
            || self.sessions.is_connected(&tab.connection_id)
    }

    pub(super) fn ensure_auto_refresh_task(&mut self, tab_id: u64, cx: &mut Context<Self>) {
        if self.auto_refresh.tasks.contains_key(&tab_id) {
            return;
        }
        let Some(seconds) = self.workspace.auto_refresh_seconds(tab_id) else {
            return;
        };
        let interval = crate::auto_refresh::interval_duration(seconds);
        let task = cx.spawn(async move |this, cx| loop {
            cx.background_executor().timer(interval).await;
            if this
                .update(cx, |this, cx| this.auto_refresh_tick(tab_id, cx))
                .is_err()
            {
                break;
            }
        });
        self.auto_refresh.tasks.insert(tab_id, task);
    }

    pub(super) fn ensure_auto_refresh_for_connection(
        &mut self,
        connection_id: &str,
        cx: &mut Context<Self>,
    ) {
        for tab_id in self
            .workspace
            .tabs()
            .iter()
            .filter(|tab| tab.connection_id == connection_id)
            .map(|tab| tab.id)
            .collect::<Vec<_>>()
        {
            self.ensure_auto_refresh_task(tab_id, cx);
        }
    }

    fn auto_refresh_tick(&mut self, tab_id: u64, cx: &mut Context<Self>) -> bool {
        let Some(_tab) = self.workspace.tabs().iter().find(|tab| tab.id == tab_id) else {
            self.auto_refresh.tasks.remove(&tab_id);
            return false;
        };
        let active = self.workspace.active_tab().map(|active| active.id) == Some(tab_id);
        let connected = self.tab_is_connected(tab_id);
        let view = self.view(tab_id);
        let draft_active = view.grid_ui.inline_edit.is_some()
            || view.detail_ui.edit.is_some()
            || view.mongo.document_edit.is_some();
        let read_only_query = self
            .workspace
            .tabs()
            .iter()
            .find(|tab| tab.id == tab_id)
            .is_some_and(|tab| tab.query_mode.is_mongo_find());
        let Some(runtime) = self.workspace.runtime(tab_id) else {
            return false;
        };
        let query_editor_draft = self
            .mongo_ui
            .filter_editors
            .get(&tab_id)
            .is_some_and(|editor| editor.read(cx).value().as_ref() != runtime.mongo.filter)
            || self
                .mongo_ui
                .sort_editors
                .get(&tab_id)
                .is_some_and(|editor| editor.read(cx).value().as_ref() != runtime.mongo.sort)
            || runtime.mongo.projection_draft_columns != runtime.mongo.projection_columns;
        let query_is_unchanged =
            runtime.last_executed_query.as_deref() == Some(mongo_query_signature(runtime).as_str());
        let eligible = crate::auto_refresh::is_eligible(
            runtime.auto_refresh_seconds,
            active,
            connected,
            !runtime.query.can_run(),
            runtime.has_staged_changes(),
            runtime.mongo.has_pending_mutations(),
            draft_active || query_editor_draft || !query_is_unchanged,
            read_only_query,
        );
        if eligible {
            self.rerun_shown_result(tab_id, false, cx);
        }
        true
    }

    pub(super) fn set_auto_refresh_interval(
        &mut self,
        tab_id: u64,
        seconds: Option<u64>,
        cx: &mut Context<Self>,
    ) {
        if !self.workspace.set_auto_refresh_seconds(tab_id, seconds) {
            return;
        }
        self.auto_refresh.tasks.remove(&tab_id);
        if seconds.is_some() {
            self.ensure_auto_refresh_task(tab_id, cx);
        }
        cx.notify();
    }

    pub(super) fn open_auto_refresh_custom_dialog(
        &mut self,
        tab_id: u64,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let input = cx.new(|cx| {
            InputState::new(window, cx)
                .placeholder("Seconds")
                .default_value(
                    self.workspace
                        .auto_refresh_seconds(tab_id)
                        .unwrap_or(5)
                        .to_string(),
                )
        });
        let root = cx.entity().downgrade();
        window.open_alert_dialog(cx, move |dialog, _, _| {
            let input = input.clone();
            let root = root.clone();
            dialog
                .width(px(360.0))
                .title("Customize auto refresh")
                .child(
                    div()
                        .flex()
                        .flex_col()
                        .gap(px(8.0))
                        .child(div().debug_selector(|| "auto-refresh-seconds".into()).child(Input::new(&input).appearance(true)))
                        .child(
                            div()
                                .text_size(px(10.0))
                                .text_color(theme::text_muted())
                                .child("Enter seconds from 1 to 86400. Frequent refresh increases server load."),
                        ),
                )
                .button_props(
                    gpui_component::dialog::DialogButtonProps::default().ok_text("Apply"),
                )
                .confirm()
                .footer(
                    gpui_component::dialog::DialogFooter::new()
                        .child(
                            div().debug_selector(|| "auto-refresh-custom-cancel".into()).child(
                                gpui_component::dialog::DialogClose::new()
                                    .trigger(|button| button.label("Cancel")),
                            ),
                        )
                        .child(
                            div().debug_selector(|| "auto-refresh-custom-apply".into()).child(
                                gpui_component::dialog::DialogAction::new().child(
                                    Button::new("apply").label("Apply").primary(),
                                ),
                            ),
                        ),
                )
                .on_ok(move |_, window, cx| {
                    let value = input.read(cx).value().to_string();
                    match crate::auto_refresh::parse_custom_seconds(&value) {
                        Ok(seconds) => {
                            let _ = root.update(cx, |this, cx| {
                                this.set_auto_refresh_interval(tab_id, Some(seconds), cx);
                            });
                            true
                        }
                        Err(error) => {
                            window.open_alert_dialog(cx, move |dialog, _, _| {
                                dialog
                                    .title("Invalid refresh interval")
                                    .child(error.to_string())
                            });
                            false
                        }
                    }
                })
        });
    }
}

/// "1 pending structure change", "3 pending structure changes".
fn structure_changes(count: usize) -> String {
    format!("{count} pending structure change{}", if count == 1 { "" } else { "s" })
}
