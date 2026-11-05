//! The SQL editor's text commands: Format SQL, and the `.sql` files a query tab can be bound to
//! (Open SQL File, Save, Save As, and the question when a tab with unsaved changes closes).
//!
//! Format SQL picks its text like Run does (the selection's statements, else everything), runs
//! the formatter on the background executor, and applies the result as one undo step only if the
//! editor still holds the text it was given. File reads and writes run on the background
//! executor too (`sql_file`).

use super::assistant::is_sql_editor;
use super::*;
use crate::sql_file::{self, SqlFileError};
use crate::sql_format::{guard::Verified, target_range, wire_dialect, Disagreement, FormatContext, FormatError};
use engine_driver_api::sql_format::Dialect;
use engine_driver_host::FormatCancel;
use crate::workspace::SqlFile;
use gpui::PathPromptOptions;
use std::path::{Path, PathBuf};

/// What happens after a save succeeded.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum AfterSave {
    Nothing,
    CloseTab,
}

/// A `.sql` file dropped in from the OS: where it opens in an editor, and where the Run SQL File
/// wizard takes it when it is too big for one.
pub(super) struct DroppedFile {
    pub(super) metadata: TabMetadata,
    pub(super) run: Option<super::data_transfer::Namespace>,
}

/// A tab's file name, falling back to a generic one.
fn file_name(path: &Path) -> String {
    path.file_name()
        .map_or_else(|| "query.sql".to_owned(), |name| name.to_string_lossy().into_owned())
}

/// A tab title as a file name: no path separators or characters Windows refuses.
fn suggested_file_name(title: &str) -> String {
    let base: String = title
        .chars()
        .map(|c| if c.is_alphanumeric() || matches!(c, ' ' | '-' | '_' | '.') { c } else { '_' })
        .collect();
    let base = base.trim();
    format!("{}.sql", if base.is_empty() { "query" } else { base })
}

impl AppRoot {
    /// The active tab when it is a SQL editor.
    fn active_sql_tab(&self) -> Option<u64> {
        self.workspace
            .active_tab()
            .filter(|tab| is_sql_editor(tab))
            .map(|tab| tab.id)
    }

    // ---- Format SQL --------------------------------------------------------------------

    /// What the formatters that run a process need to know about `tab_id`. A new format in the same
    /// tab stops the one still running there (its answer would be dropped anyway); another tab's
    /// format is its own.
    pub(super) fn format_context(&mut self, tab_id: u64) -> FormatContext {
        let cancel = FormatCancel::new();
        if let Some(running) = self.format_ui.running.insert(tab_id, cancel.clone()) {
            running.cancel();
        }
        FormatContext {
            dialect: self.explain_dialect_of(tab_id).map_or(Dialect::Ansi, wire_dialect),
            tools: self.driver_ui.tools.store.clone(),
            cancel,
        }
    }

    /// The editor of `tab_id` closed: a format of its text has nothing to come back to.
    pub(super) fn cancel_format_of(&mut self, tab_id: u64) {
        if let Some(cancel) = self.format_ui.running.remove(&tab_id) {
            cancel.cancel();
        }
    }

    /// Formats the selection's statements, else the whole editor; one undo step.
    pub(super) fn format_sql(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(tab_id) = self.active_sql_tab() else {
            return;
        };
        let Some(editor) = self.query_editors.by_tab.get(&tab_id).cloned() else {
            return;
        };
        let (text, selection, cursor) = {
            let state = editor.read(cx);
            (state.value().to_string(), state.selected_range(), state.cursor())
        };
        let lex = self.sql_lex(tab_id);
        let Some(target) = target_range(&text, selection.clone(), &lex) else {
            window.push_notification("There is no SQL to format", cx);
            return;
        };
        let context = self.format_context(tab_id);
        let formatter = self.settings.editor.sql_formatter(context);
        let region = text[target.clone()].to_owned();
        // The formatter may block on a process for up to its timeout: not on an executor thread.
        let job = cx
            .background_executor()
            .spawn(async move { smol::unblock(move || formatter.format(&region, &lex)).await });
        cx.spawn_in(window, async move |this, cx| {
            let outcome = job.await;
            let _ = this.update_in(cx, |this, window, cx| {
                let job = FormatJob { tab_id, text, target, selection, cursor };
                this.apply_format(job, outcome, window, cx);
            });
        })
        .detach();
    }

    fn apply_format(
        &mut self,
        job: FormatJob,
        outcome: Result<Verified, FormatError>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let FormatJob { tab_id, text, target, selection, cursor } = job;
        if !self.query_editors.by_tab.contains_key(&tab_id) {
            return;
        }
        let done = match outcome {
            Ok(done) => done,
            Err(FormatError::Empty) => {
                window.push_notification("There is no SQL to format", cx);
                return;
            }
            Err(error @ FormatError::Refused(_)) => {
                window.push_notification(format!("Format SQL left the text unchanged: {error}."), cx);
                return;
            }
            // A newer format or the closing editor stopped it: nothing to say.
            Err(FormatError::Cancelled) => return,
            Err(FormatError::Disagreement(disagreement)) => {
                let job = FormatJob { tab_id, text, target, selection, cursor };
                self.open_format_disagreement_dialog(job, *disagreement, window, cx);
                return;
            }
            Err(error) => {
                self.report_warning(Area::App, None, format!("Format SQL: {error}"));
                window.push_notification(format!("Format SQL failed: {}.", shortened(&error.to_string())), cx);
                return;
            }
        };
        self.finish_format(FormatJob { tab_id, text, target, selection, cursor }, done, window, cx);
    }

    /// Replaces the formatted text if the editor still holds what was formatted.
    fn finish_format(&mut self, job: FormatJob, done: Verified, window: &mut Window, cx: &mut Context<Self>) {
        let FormatJob { tab_id, text, target, selection, cursor } = job;
        let Some(editor) = self.query_editors.by_tab.get(&tab_id).cloned() else {
            return;
        };
        if editor.read(cx).value().as_ref() != text.as_str() {
            window.push_notification("The text changed while it was being formatted; nothing was applied", cx);
            return;
        }
        if text[target.clone()] == done.text {
            window.push_notification("Already formatted", cx);
            return;
        }
        let selecting = !selection.is_empty();
        let formatted_end = target.start + done.text.len();
        editor.update(cx, |state, cx| {
            state.set_selected_range(target.clone(), cx);
            state.replace(done.text.clone(), window, cx);
            if selecting {
                // The reformatted statements stay selected.
                state.set_selected_range(target.start..formatted_end, cx);
                return;
            }
            // The cursor stays in the same token; outside the formatted text it keeps its place
            // (shifted by what the formatting added before it).
            let at = if cursor < target.start {
                cursor
            } else if cursor > target.end {
                formatted_end + (cursor - target.end)
            } else {
                target.start + done.map_offset(cursor - target.start)
            };
            state.set_selected_range(at..at, cx);
        });
    }

    /// "SQL Formatter (sqruff) changed more than the layout": the output the guard cannot accept,
    /// with the first token that differs. Apply anyway puts it in (the user chose this formatter);
    /// Cancel is the default: Enter and Escape keep the text, and only a click on Apply anyway (a
    /// plain button, not the dialog's confirm action) applies it.
    fn open_format_disagreement_dialog(
        &mut self,
        job: FormatJob,
        disagreement: Disagreement,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let root = cx.entity().downgrade();
        let approval = Arc::new((job, disagreement));
        window.open_alert_dialog(cx, move |dialog, _, _| {
            let disagreement = &approval.1;
            let root = root.clone();
            let apply = Arc::clone(&approval);
            let first_difference = disagreement.first_difference();
            dialog
                .width(px(480.0))
                .title(format!("{} changed more than the layout", disagreement.source))
                .child(
                    div()
                        .debug_selector(|| "format-disagree-body".into())
                        .flex()
                        .flex_col()
                        .gap(px(6.0))
                        .child(format!("Its text is not the same statement as yours: {}.", disagreement.violation))
                        .children(first_difference)
                        .child("Apply it anyway, or keep your text?"),
                )
                .confirm()
                .footer(
                    gpui_component::dialog::DialogFooter::new()
                        .child(
                            div()
                                .debug_selector(|| "format-disagree-cancel".into())
                                .child(gpui_component::dialog::DialogClose::new().trigger(|button| button.label("Cancel").primary())),
                        )
                        .child(div().debug_selector(|| "format-disagree-apply".into()).child(
                            Button::new("format-disagree-apply").label("Apply anyway").danger().on_click(
                                move |_, window, cx| {
                                    window.close_dialog(cx);
                                    let (job, disagreement) = &*apply;
                                    let (job, done) = (job.clone(), Verified::unchecked(disagreement.formatted.clone()));
                                    let _ = root.update(cx, |this, cx| this.finish_format(job, done, window, cx));
                                },
                            ),
                        )),
                )
                // Enter is the dialog's confirm action: here it keeps the text, like Cancel.
                .on_ok(|_, _, _| true)
        });
    }

    // ---- Open ----------------------------------------------------------------------------

    /// Asks for a `.sql` file and opens it in a query tab.
    pub(super) fn open_sql_file(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let prompt = cx.prompt_for_paths(PathPromptOptions {
            files: true,
            directories: false,
            multiple: false,
            prompt: Some("Open SQL file".into()),
        });
        cx.spawn_in(window, async move |this, cx| {
            let path = match prompt.await {
                Ok(Ok(Some(paths))) => paths.into_iter().next(),
                Ok(Ok(None)) => None,
                _ => {
                    let _ = this.update_in(cx, |_, window, cx| {
                        window.push_notification("Could not open the file picker", cx);
                    });
                    None
                }
            };
            if let Some(path) = path {
                let _ = this.update_in(cx, |this, window, cx| this.open_sql_path(path, window, cx));
            }
        })
        .detach();
    }

    /// Opens `path` in the tab that already holds it, else in a new tab (or the empty tab you are
    /// on); the file is read off the UI thread.
    pub(super) fn open_sql_path(&mut self, path: PathBuf, window: &mut Window, cx: &mut Context<Self>) {
        self.open_sql_path_on(path, None, window, cx);
    }

    /// [`Self::open_sql_path`] for a file dropped in from the OS: the tab is bound to `dropped`'s
    /// metadata instead of the active tab's, and a file over the editor's limit goes to the Run SQL
    /// File wizard instead of being refused.
    pub(super) fn open_sql_path_on(&mut self, path: PathBuf, dropped: Option<DroppedFile>, window: &mut Window, cx: &mut Context<Self>) {
        let open = self
            .workspace
            .tabs()
            .iter()
            .find(|tab| tab.file.as_ref().is_some_and(|file| file.path == path))
            .map(|tab| tab.id);
        if let Some(tab_id) = open {
            self.workspace.activate_tab(tab_id);
            cx.notify();
            return;
        }
        let read_path = path.clone();
        let read = cx
            .background_executor()
            .spawn(async move { smol::unblock(move || sql_file::read(&read_path)).await });
        cx.spawn_in(window, async move |this, cx| {
            let outcome = read.await;
            let _ = this.update_in(cx, |this, window, cx| match (outcome, dropped) {
                (Ok(text), dropped) => this.show_opened_file(path, text, dropped.map(|dropped| dropped.metadata), window, cx),
                (Err(SqlFileError::TooLarge { .. }), Some(dropped)) => this.run_too_large_dropped_file(path, dropped.run, window, cx),
                (Err(error), _) => this.sql_file_failed(&error, window, cx),
            });
        })
        .detach();
    }

    /// `metadata` is where the file is bound (`None`: beside the active tab).
    fn show_opened_file(&mut self, path: PathBuf, text: String, metadata: Option<TabMetadata>, window: &mut Window, cx: &mut Context<Self>) {
        let reusable = self
            .workspace
            .active_tab()
            .filter(|tab| is_sql_editor(tab) && tab.file.is_none() && tab.sql.trim().is_empty())
            .filter(|tab| metadata.as_ref().is_none_or(|metadata| (&tab.connection_id, &tab.database, &tab.schema) == (&metadata.connection_id, &metadata.database, &metadata.schema)))
            .map(|tab| tab.id);
        let tab_id = reusable.unwrap_or_else(|| {
            let metadata = metadata.unwrap_or_else(|| self.metadata_for_opened_file());
            self.workspace.open_query_tab_with_metadata(file_name(&path), metadata)
        });
        let saved: Arc<str> = Arc::from(text.as_str());
        self.set_script_text(tab_id, text, window, cx);
        if let Some(tab) = self.workspace.tab_mut(tab_id) {
            tab.title = file_name(&path);
            tab.file = Some(SqlFile { path, saved });
        }
        self.workspace.activate_tab(tab_id);
        cx.notify();
    }

    /// The connection (and database) a file opens on: the active tab's, when it is a SQL one.
    fn metadata_for_opened_file(&self) -> TabMetadata {
        self.workspace.active_tab().map_or_else(TabMetadata::default, |tab| self.metadata_beside(tab))
    }

    /// Where a file opened beside `tab` is bound: its connection (and database and schema when it
    /// is a SQL editor); nothing when the connection is not a SQL one.
    pub(super) fn metadata_beside(&self, tab: &Tab) -> TabMetadata {
        let sql_connection = self.is_sql_connection(&tab.connection_id);
        match (sql_connection, is_sql_editor(tab)) {
            (false, _) => TabMetadata::default(),
            (true, false) => TabMetadata { connection_id: tab.connection_id.clone(), ..Default::default() },
            (true, true) => TabMetadata {
                connection_id: tab.connection_id.clone(),
                database: tab.database.clone(),
                schema: tab.schema.clone(),
                ..Default::default()
            },
        }
    }

    pub(super) fn is_sql_connection(&self, connection: &str) -> bool {
        self.saved_connections
            .iter()
            .find(|saved| saved.matches_id(connection))
            .is_some_and(|saved| super::explain::explain_dialect(saved.kind).is_some())
    }

    fn sql_file_failed(&mut self, error: &SqlFileError, window: &mut Window, cx: &mut Context<Self>) {
        self.report_error(Area::App, None, error.to_string());
        window.push_notification(error.to_string(), cx);
    }

    // ---- Save ----------------------------------------------------------------------------

    /// Cmd/Ctrl+S: a tab bound to a file writes it; any other keeps the Save Script flow.
    pub(super) fn save_active_sql(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let bound = self
            .active_sql_tab()
            .and_then(|id| self.workspace.tab(id)?.file.as_ref().map(|file| (id, file.path.clone())));
        match bound {
            Some((tab_id, path)) => self.write_sql_file(tab_id, path, AfterSave::Nothing, window, cx),
            None => self.open_save_sql_dialog(window, cx),
        }
    }

    /// Saves the file-bound tab `tab_id` (a no-op for any other).
    pub(super) fn save_sql_file(&mut self, tab_id: u64, after: AfterSave, window: &mut Window, cx: &mut Context<Self>) {
        let path = self.workspace.tab(tab_id).and_then(|tab| Some(tab.file.as_ref()?.path.clone()));
        if let Some(path) = path {
            self.write_sql_file(tab_id, path, after, window, cx);
        }
    }

    /// Save As: asks where, writes the tab's text there and binds the tab to that file.
    pub(super) fn save_sql_file_as(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(tab_id) = self.active_sql_tab() else {
            return;
        };
        let Some(tab) = self.workspace.tab(tab_id) else {
            return;
        };
        let (directory, suggested) = match &tab.file {
            Some(file) => (
                file.path.parent().map_or_else(crate::platform::save_dialog_dir, Path::to_path_buf),
                file_name(&file.path),
            ),
            None => (crate::platform::save_dialog_dir(), suggested_file_name(&tab.title)),
        };
        let prompt = cx.prompt_for_new_path(&directory, Some(&suggested));
        cx.spawn_in(window, async move |this, cx| {
            let path = match prompt.await {
                Ok(Ok(Some(path))) => path,
                Ok(Ok(None)) => return,
                _ => {
                    let _ = this.update_in(cx, |_, window, cx| {
                        window.push_notification("Could not open the save dialog", cx);
                    });
                    return;
                }
            };
            let _ = this.update_in(cx, |this, window, cx| {
                this.write_sql_file(tab_id, path, AfterSave::Nothing, window, cx);
            });
        })
        .detach();
    }

    /// Writes the tab's text to `path` off the UI thread, then binds the tab to it. A failure
    /// leaves the tab as it was.
    fn write_sql_file(
        &mut self,
        tab_id: u64,
        path: PathBuf,
        after: AfterSave,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let text = match self.query_editors.by_tab.get(&tab_id) {
            Some(editor) => editor.read(cx).value().to_string(),
            None => match self.workspace.tab(tab_id) {
                Some(tab) => tab.sql.clone(),
                None => return,
            },
        };
        let saved: Arc<str> = Arc::from(text);
        let (write_path, write_text) = (path.clone(), saved.clone());
        let write = cx
            .background_executor()
            .spawn(async move { smol::unblock(move || sql_file::write(&write_path, &write_text)).await });
        cx.spawn_in(window, async move |this, cx| {
            let outcome = write.await;
            let _ = this.update_in(cx, |this, window, cx| match outcome {
                Ok(()) => {
                    if let Some(tab) = this.workspace.tab_mut(tab_id) {
                        tab.title = file_name(&path);
                        tab.file = Some(SqlFile { path, saved });
                    }
                    if after == AfterSave::CloseTab {
                        this.close_tab(tab_id, cx);
                    }
                    cx.notify();
                }
                Err(error) => this.sql_file_failed(&error, window, cx),
            });
        })
        .detach();
    }

    // ---- Close ---------------------------------------------------------------------------

    /// "Save changes to <file>?": Save writes the file and closes the tab, Don't Save closes it,
    /// Cancel (or Escape) keeps it.
    pub(super) fn open_unsaved_file_dialog(&mut self, tab_id: u64, window: &mut Window, cx: &mut Context<Self>) {
        let Some(name) = self.workspace.tab(tab_id).map(|tab| tab.title.clone()) else {
            return;
        };
        let root = cx.entity().downgrade();
        window.open_alert_dialog(cx, move |dialog, _, _| {
            let (root_save, root_discard) = (root.clone(), root.clone());
            dialog
                .width(px(440.0))
                .title(format!("Save changes to {name}?"))
                .child(
                    div()
                        .debug_selector(|| "unsaved-file-body".into())
                        .child("Your changes will be lost if you do not save them."),
                )
                .confirm()
                .footer(
                    gpui_component::dialog::DialogFooter::new()
                        .child(
                            div()
                                .debug_selector(|| "unsaved-file-cancel".into())
                                .child(gpui_component::dialog::DialogClose::new().trigger(|button| button.label("Cancel"))),
                        )
                        .child(div().debug_selector(|| "unsaved-file-discard".into()).child(
                            Button::new("unsaved-file-discard").label("Don't Save").on_click(
                                move |_, window, cx| {
                                    window.close_dialog(cx);
                                    let _ = root_discard.update(cx, |this, cx| {
                                        this.close_tab(tab_id, cx);
                                        cx.notify();
                                    });
                                },
                            ),
                        ))
                        .child(div().debug_selector(|| "unsaved-file-save".into()).child(
                            gpui_component::dialog::DialogAction::new()
                                .child(Button::new("unsaved-file-save").label("Save").primary()),
                        )),
                )
                .on_ok(move |_, window, cx| {
                    let _ = root_save.update(cx, |this, cx| {
                        this.save_sql_file(tab_id, AfterSave::CloseTab, window, cx);
                    });
                    true
                })
        });
    }
}

/// The longest failure message a notification shows (the error log keeps all of it).
const NOTIFICATION_CHARS: usize = 300;

fn shortened(message: &str) -> String {
    match message.char_indices().nth(NOTIFICATION_CHARS) {
        Some((end, _)) => format!("{}\u{2026}", &message[..end]),
        None => message.to_owned(),
    }
}

/// What Format SQL is doing: the process each tab started, so a newer format in that tab or its editor
/// closing can stop it.
#[derive(Default)]
pub(super) struct FormatUi {
    running: std::collections::HashMap<u64, FormatCancel>,
    /// What Settings › Editor last found installed and on the PATH (see `settings_format`).
    pub(super) scan: Option<super::settings_format::FormatScan>,
    pub(super) scan_task: Option<gpui::Task<()>>,
}

#[cfg(test)]
impl AppRoot {
    pub(super) fn format_ui_scan_for_test(&self) -> Option<super::settings_format::FormatScan> {
        self.format_ui.scan.clone()
    }

    pub(super) fn format_ui_set_scan_for_test(&mut self, scan: super::settings_format::FormatScan) {
        self.format_ui.scan = Some(scan);
        self.format_ui.scan_task = None;
    }

    /// Forgets what the Settings page found, so its next look is the one a test waits for.
    pub(super) fn format_ui_reset_for_test(&mut self) {
        self.format_ui.scan = None;
        self.format_ui.scan_task = None;
    }
}

/// The text Format SQL was given, and where the cursor and selection were in it.
#[derive(Clone)]
struct FormatJob {
    tab_id: u64,
    text: String,
    target: Range<usize>,
    selection: Range<usize>,
    cursor: usize,
}
