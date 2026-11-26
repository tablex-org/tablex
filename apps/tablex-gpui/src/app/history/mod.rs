//! Query History: a tab across every connection that lists what was run (and what the MCP server was
//! asked), searched and filtered; and the recording of runs that feeds it.
//!
//! * `model`: rows, filters, paging, time ranges (no window);
//! * `view`: the tab;
//! * here: what the tab's actions do to the app (open, insert, run, ask the assistant), how a run is
//!   recorded (off the UI thread; failures and durations too; gated by Settings) and the start-up
//!   retention.
//!
//! The per-tab strip under a table's results (`workspace.rs`) reads the same table.

mod model;
mod view;

use std::path::PathBuf;

use gpui::{Context, Entity, Window};

use super::*;
use model::{Names, Row};
pub(in crate::app) use view::HistoryView;

/// The rows a delete of old runs takes at a time (the lock is let go between batches).
const PRUNE_BATCH: u32 = 1000;

/// What a run was and how it ended, ready for the history.
pub(in crate::app) struct Recorded {
    pub(in crate::app) connection: String,
    pub(in crate::app) database: String,
    pub(in crate::app) collection: String,
    pub(in crate::app) statement: String,
    pub(in crate::app) limit: i64,
    pub(in crate::app) rows: i64,
    pub(in crate::app) duration_ms: Option<i64>,
    /// `Some`: the run failed with this (already masked) error.
    pub(in crate::app) error: Option<String>,
    pub(in crate::app) kind: &'static str,
}

/// The app's part of the Query History tab.
pub(in crate::app) struct HistoryUi {
    pub(in crate::app) view: Option<Entity<HistoryView>>,
    /// The editor tab the user came from: where "Insert into editor" goes.
    pub(in crate::app) return_tab: Option<u64>,
    /// The connection of the tab the user came from.
    pub(in crate::app) current_connection: Option<String>,
    /// The audit log the MCP calls are read from.
    pub(in crate::app) audit_path: Option<PathBuf>,
}

impl Default for HistoryUi {
    fn default() -> Self {
        Self {
            view: None,
            return_tab: None,
            current_connection: None,
            // A test never reads the user's own log.
            audit_path: if cfg!(test) { None } else { engine_connect::platform::audit_log_path() },
        }
    }
}

impl AppRoot {
    /// The Query History tab: opened, or shown when it is open. `connection`: narrowed to its runs.
    pub(in crate::app) fn open_query_history(&mut self, connection: Option<String>, window: &mut Window, cx: &mut Context<Self>) {
        if let Some(tab) = self.workspace.active_tab().filter(|tab| tab.kind != TabKind::History) {
            if is_editor_tab(tab) {
                self.history.return_tab = Some(tab.id);
            }
            if !tab.connection_id.is_empty() {
                self.history.current_connection = Some(tab.connection_id.clone());
            }
        }
        self.workspace.open_history_tab(&t!("history.title"));
        if self.history.view.is_none() {
            let root = cx.entity().downgrade();
            self.history.view = Some(cx.new(|cx| HistoryView::new(root, window, cx)));
        }
        if let Some(view) = self.history.view.clone() {
            let current = self.history.current_connection.clone();
            let (names, audit) = self.history_read_context();
            view.update(cx, |view, cx| {
                view.set_context(current, names, audit);
                match connection {
                    Some(id) => view.show_connection(Some(id), cx),
                    None => view.refresh(cx),
                }
            });
        }
        self.notify_chrome(cx);
        cx.notify();
    }

    /// The tab closed: its rows go with it.
    pub(super) fn history_tab_closing(&mut self, tab_id: u64) {
        if self.workspace.tab(tab_id).is_some_and(|tab| tab.kind == TabKind::History) {
            self.history.view = None;
        }
    }

    /// What the tab reads from the app: the connections' names (ids in lowercase) and the audit log.
    pub(in crate::app) fn history_read_context(&self) -> (Names, Option<PathBuf>) {
        let names = self.saved_connections.iter().map(|saved| (saved.id.to_string().to_lowercase(), saved.name.clone())).collect();
        (names, self.history.audit_path.clone())
    }

    /// The assistant can take a question about a run of `connection` (it is set up, and allowed there).
    pub(in crate::app) fn history_ai_available(&self, connection: &str) -> bool {
        let Some(saved) = self.saved_connections.iter().find(|saved| saved.matches_id(connection)) else {
            return false;
        };
        let scope = engine_ai::Scope { connection_id: saved.id.to_string(), database: None, schema: None };
        matches!(self.assistant_status(Some(&scope)).readiness, assistant::Readiness::Ready)
    }

    // ---- what the actions do ---------------------------------------------------------------------------

    /// A new query tab on the run's connection and database, holding its statement.
    fn history_new_tab(&mut self, entry: &engine_storage::history::HistoryEntry, cx: &mut Context<Self>) -> Option<u64> {
        let Some(saved) = self.saved_connections.iter().find(|saved| saved.matches_id(&entry.conn_uuid)) else {
            self.sidebar.notice = Some(t!("history.connection_gone").into_owned());
            cx.notify();
            return None;
        };
        let query_mode = match saved.kind {
            ConnectionKind::Mongodb => Some(crate::workspace::QueryMode::MongoScript),
            ConnectionKind::Redis => Some(crate::workspace::QueryMode::RedisCommand),
            _ => None,
        };
        let metadata = TabMetadata {
            connection_id: saved.id.to_string(),
            database: entry.database.clone(),
            query_mode,
            ..Default::default()
        };
        let id = self.workspace.open_query_tab_with_metadata("Query", metadata);
        if let Some(tab) = self.workspace.tab_mut(id) {
            tab.sql = entry.filter.clone();
        }
        self.notify_chrome(cx);
        cx.notify();
        Some(id)
    }

    /// Open in new tab.
    pub(in crate::app) fn history_open_in_tab(&mut self, row: &Row, _window: &mut Window, cx: &mut Context<Self>) {
        if let Row::Sql(entry) = row {
            self.history_new_tab(entry, cx);
        }
    }

    /// Insert into the editor the user came from (the tab shown before the History tab), at its cursor;
    /// a new tab on the run's connection when there is none.
    pub(in crate::app) fn history_insert(&mut self, row: &Row, window: &mut Window, cx: &mut Context<Self>) {
        let Row::Sql(entry) = row else {
            return;
        };
        let target = self.history.return_tab.filter(|id| self.workspace.tab(*id).is_some_and(is_editor_tab));
        match target {
            Some(tab_id) => {
                self.history_insert_into_tab(tab_id, &entry.filter, window, cx);
                self.activate_tab_ui(tab_id, cx);
            }
            None => {
                self.history_new_tab(entry, cx);
            }
        }
    }

    /// `sql` at the cursor of `tab_id`'s editor (the editor reports its own change; the tab is made
    /// to have the text even if the insert was silent).
    pub(in crate::app) fn history_insert_into_tab(&mut self, tab_id: u64, sql: &str, window: &mut Window, cx: &mut Context<Self>) {
        let editor = self.editor_for_tab(tab_id, window, cx);
        editor.update(cx, |editor, cx| editor.insert(sql.to_owned(), window, cx));
        let text = editor.read(cx).value().to_string();
        if let Some(tab) = self.workspace.tab_mut(tab_id) {
            tab.sql = text;
        }
        cx.notify();
    }

    /// Run: a plain read goes at once; anything else asks first, with the statement in front of the
    /// user. The statement runs in a new tab on the run's connection and database.
    pub(in crate::app) fn history_run(&mut self, row: &Row, window: &mut Window, cx: &mut Context<Self>) {
        let Row::Sql(entry) = row else {
            return;
        };
        if !model::needs_confirmation(&entry.filter, row.kind()) {
            self.history_run_now(entry.clone(), cx);
            return;
        }
        let name = self
            .saved_connections
            .iter()
            .find(|saved| saved.matches_id(&entry.conn_uuid))
            .map_or_else(|| t!("history.unknown_connection").into_owned(), |saved| saved.name.clone());
        let preview: String = entry.filter.chars().take(600).collect();
        let root = cx.entity().downgrade();
        let entry = entry.clone();
        window.open_alert_dialog(cx, move |dialog, _, _| {
            let (root, entry) = (root.clone(), entry.clone());
            dialog
                .title(t!("history.run_title").into_owned())
                .child(
                    div()
                        .flex()
                        .flex_col()
                        .gap(px(8.0))
                        .child(div().debug_selector(|| "history-run-body".into()).text_size(px(12.0)).child(t!("history.run_body", connection = name.clone()).into_owned()))
                        .child(
                            div()
                                .debug_selector(|| "history-run-sql".into())
                                .max_h(px(160.0))
                                .overflow_hidden()
                                .px(px(8.0))
                                .py(px(6.0))
                                .rounded(px(4.0))
                                .border_1()
                                .border_color(theme::border())
                                .bg(theme::chrome())
                                .text_size(px(12.0))
                                .child(preview.clone()),
                        ),
                )
                .button_props(gpui_component::dialog::DialogButtonProps::default().ok_text(t!("history.run").into_owned()))
                .confirm()
                .on_ok(move |_, _, cx| {
                    let _ = root.update(cx, |root, cx| root.history_run_now(entry.clone(), cx));
                    true
                })
        });
    }

    fn history_run_now(&mut self, entry: engine_storage::history::HistoryEntry, cx: &mut Context<Self>) {
        if let Some(tab_id) = self.history_new_tab(&entry, cx) {
            self.run_tab_query_explicit(tab_id, cx);
        }
    }

    /// Analyze with AI.
    pub(in crate::app) fn history_analyze(&mut self, row: &Row, window: &mut Window, cx: &mut Context<Self>) {
        let Row::Sql(entry) = row else {
            return;
        };
        if !self.history_ai_available(&entry.conn_uuid) {
            return;
        }
        let error = entry.error.as_deref().filter(|_| entry.failed());
        self.assistant_history_ask(&entry.conn_uuid, &entry.database, &entry.filter, error, window, cx);
    }

    // ---- the strip under a table's results -------------------------------------------------------------

    /// A click on an entry of a tab's strip puts its statement in the tab's editor.
    pub(super) fn history_strip_insert(&mut self, tab_id: u64, entry_id: i64, window: &mut Window, cx: &mut Context<Self>) {
        let Some(entry) = self.view(tab_id).history.entries.iter().find(|entry| entry.id == entry_id).cloned() else {
            return;
        };
        self.view_mut(tab_id).history.selected = Some(entry_id);
        self.history_insert_into_tab(tab_id, &entry.filter, window, cx);
    }

    /// A double click, or Enter on the selected entry: runs its statement in the tab (the editor keeps its text).
    pub(super) fn history_strip_run(&mut self, tab_id: u64, entry_id: i64, cx: &mut Context<Self>) {
        let Some(entry) = self.view(tab_id).history.entries.iter().find(|entry| entry.id == entry_id).cloned() else {
            return;
        };
        self.view_mut(tab_id).history.selected = Some(entry_id);
        self.run_typed_sql(tab_id, entry.filter, true, cx);
    }

    // ---- recording -------------------------------------------------------------------------------------

    /// Puts a run in the history, off the UI thread (tests keep it in memory instead of touching the
    /// user's database). Nothing is recorded while Settings says so.
    pub(super) fn record_history(&mut self, run: Recorded, cx: &mut Context<Self>) {
        if !self.settings.query.history_record || run.statement.trim().is_empty() {
            return;
        }
        #[cfg(test)]
        {
            let _ = &cx;
            self.recorded_history.push(super::workspace::RecordedHistory {
                connection: run.connection,
                database: run.database,
                collection: run.collection,
                sql: run.statement,
                rows: run.rows,
                error: run.error,
                duration_ms: run.duration_ms,
                kind: run.kind.to_owned(),
            });
        }
        #[cfg(not(test))]
        cx.background_executor()
            .spawn(async move {
                let _ = write_history(&run);
            })
            .detach();
    }

    /// Deletes the runs older than the retention Settings give, at start, in batches off the UI thread.
    pub(super) fn prune_history_in_background(&mut self, cx: &mut Context<Self>) {
        let days = self.settings.query.history_keep_days;
        if days == 0 {
            return;
        }
        let cutoff = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs_f64()
            - f64::from(days) * 86_400.0;
        cx.background_executor()
            .spawn(async move {
                let _ = engine_storage::history::prune_older_than(cutoff, PRUNE_BATCH);
            })
            .detach();
    }
}

/// An editor a statement can be put in: a SQL editor, or a Mongo script or Redis console.
fn is_editor_tab(tab: &Tab) -> bool {
    assistant::is_sql_editor(tab) || tab.query_mode.is_mongo_script() || tab.query_mode.is_redis_command()
}

/// The history row of a recorded run.
pub(super) fn write_history(run: &Recorded) -> engine_storage::history::Result<()> {
    engine_storage::history::record(&engine_storage::history::Record {
        conn_uuid: &run.connection,
        database: &run.database,
        collection: &run.collection,
        filter: &run.statement,
        project: "",
        sort: "",
        skip: 0,
        limit: run.limit,
        max_time_ms: 0,
        result_count: run.rows,
        error: run.error.as_deref(),
        duration_ms: run.duration_ms,
        kind: run.kind,
    })
}

/// The longest an error is kept, in characters.
pub(in crate::app) fn clip_error(error: &str) -> String {
    crate::sessions::redact_secrets(error).chars().take(engine_storage::history::MAX_ERROR_CHARS).collect()
}

/// A row of the manager as it reads, for the tests that read a database of an earlier release.
#[cfg(test)]
pub(in crate::app) fn row_view_for_test(entry: engine_storage::history::HistoryEntry, names: &Names) -> model::RowView {
    model::RowView::new(Row::Sql(entry), names)
}

/// Whether running `statement` asks first, for the tests.
#[cfg(test)]
pub(in crate::app) fn needs_confirmation_for_test(statement: &str) -> bool {
    model::needs_confirmation(statement, engine_storage::history::classify_sql(statement))
}
