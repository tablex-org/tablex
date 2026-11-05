//! `.sql` files dragged in from the OS (gpui reports them as an `ExternalPaths` drag): dropped on a SQL
//! editor tab or on a navigator row they open in the editor, one file-backed tab each, bound to that tab's
//! connection and database or to that row's connection and namespace. A file already open in a tab activates that
//! tab. A file over the editor's size limit (`sql_file::MAX_BYTES`) goes to the Run SQL File wizard on the same
//! target instead, which streams it. Paths that are not `.sql` are named in a notification and skipped.
//!
//! Only the containers listen (the editor tab, the navigator); the row under the pointer is found from the list's
//! painted bounds and scroll offset, so no row carries a listener.

use std::path::{Path, PathBuf};

use gpui::{div, px, App, Context, DragMoveEvent, ExternalPaths, IntoElement, ParentElement, SharedString, Styled, Window};
use gpui_component::ActiveTheme;

use super::super::data_transfer::Namespace;
use super::super::sql_editor::DroppedFile;
use super::super::*;
use super::FileRunInit;
use crate::theme;

/// What the user is told when a drop holds nothing to run.
const NOT_SQL: &str = "Only .sql files can be opened. Drop a .sql file.";

pub(in crate::app) fn is_sql_file(path: &Path) -> bool {
    path.extension().is_some_and(|extension| extension.eq_ignore_ascii_case("sql"))
}

fn holds_sql(paths: &ExternalPaths) -> bool {
    paths.paths().iter().any(|path| is_sql_file(path))
}

/// The `.sql` files of a drop and how many other paths it held.
fn split_sql_files(paths: &[PathBuf]) -> (Vec<PathBuf>, usize) {
    let files: Vec<PathBuf> = paths.iter().filter(|path| is_sql_file(path)).cloned().collect();
    let skipped = paths.len() - files.len();
    (files, skipped)
}

/// The tint and border drawn over a container while a `.sql` file is dragged over it. Carries no listener, so it never
/// takes the drag; place it as an absolute child of a `relative()` container.
pub(in crate::app) fn highlight(label: impl Into<SharedString>, cx: &App) -> impl IntoElement {
    div()
        .debug_selector(|| "file-drop-highlight".into())
        .absolute()
        .top_0()
        .left_0()
        .size_full()
        .flex()
        .items_center()
        .justify_center()
        .rounded(px(4.0))
        .bg(cx.theme().drop_target)
        .border_2()
        .border_color(theme::accent())
        .child(div().px(px(10.0)).py(px(4.0)).rounded(px(4.0)).bg(theme::surface()).text_size(px(12.0)).text_color(theme::text()).child(label.into()))
}

impl AppRoot {
    /// The namespace a file dropped on the navigator row `id` runs in: a connection's first database, or the database,
    /// schema or table the row stands for. `None` for a group, a column or anything that is not in a SQL namespace.
    fn file_run_target_of_node(&self, id: &NavigatorNodeId) -> Option<Namespace> {
        match id {
            NavigatorNodeId::Connection { connection } => Some(self.file_run_target_of_connection(connection)),
            id => self.object_origin(id).map(|origin| Namespace { connection: origin.connection, database: origin.database, schema: origin.schema }),
        }
    }

    /// The navigator row under a window-space point, from the list's painted bounds plus its scroll offset (the scheme
    /// of `error_row_at`).
    fn navigator_row_at(&self, point: gpui::Point<gpui::Pixels>) -> Option<NavigatorNodeId> {
        let (bounds, offset) = {
            let state = self.sidebar.scroll.0.borrow();
            (state.base_handle.bounds(), state.base_handle.offset())
        };
        if !bounds.contains(&point) {
            return None;
        }
        let y = f32::from(point.y - bounds.origin.y - offset.y);
        if y < 0.0 {
            return None;
        }
        self.navigator.visible_row_at((y / theme::SIDEBAR_ROW_HEIGHT) as usize).map(|row| row.id)
    }

    /// A file drag moves over the editor tab `tab_id`: its highlight follows the pointer (`.sql` files only).
    pub(in crate::app) fn file_drag_over_tab(&mut self, tab_id: u64, event: &DragMoveEvent<ExternalPaths>, cx: &mut Context<Self>) {
        let over = event.bounds.contains(&event.event.position) && holds_sql(event.drag(cx));
        let next = over.then_some(tab_id);
        if self.file_drop_tab != next && (over || self.file_drop_tab == Some(tab_id)) {
            self.file_drop_tab = next;
            cx.notify();
        }
    }

    /// A file drag moves over the navigator: the row it would run on gets the drop border.
    pub(in crate::app) fn file_drag_over_navigator(&mut self, event: &DragMoveEvent<ExternalPaths>, cx: &mut Context<Self>) {
        let target = if holds_sql(event.drag(cx)) { self.navigator_row_at(event.event.position).filter(|id| self.file_run_target_of_node(id).is_some()) } else { None };
        if self.navigator_ui.drop_target != target {
            self.navigator_ui.drop_target = target;
            self.notify_chrome(cx);
        }
    }

    /// The drag left the window over the editor or the navigator: no highlight stays behind.
    pub(in crate::app) fn file_drag_left(&mut self, cx: &mut Context<Self>) {
        if self.file_drop_tab.take().is_some() | self.navigator_ui.drop_target.take().is_some() {
            self.notify_chrome(cx);
        }
    }

    /// Files dropped on the editor tab `tab_id`: they open beside it, on its connection and database.
    pub(in crate::app) fn drop_files_on_tab(&mut self, tab_id: u64, paths: &ExternalPaths, window: &mut Window, cx: &mut Context<Self>) {
        self.file_drag_left(cx);
        let metadata = self.workspace.tab(tab_id).map(|tab| self.metadata_beside(tab)).unwrap_or_default();
        let run = self.namespace_from_context(Some(tab_id));
        self.open_dropped_sql_files(paths.paths(), metadata, run, window, cx);
    }

    /// Files dropped on the navigator: they open on the row under the pointer.
    pub(in crate::app) fn drop_files_on_navigator(&mut self, paths: &ExternalPaths, window: &mut Window, cx: &mut Context<Self>) {
        self.file_drag_left(cx);
        let Some(target) = self.navigator_row_at(window.mouse_position()).and_then(|id| self.file_run_target_of_node(&id)) else {
            if paths.paths().iter().any(|path| is_sql_file(path)) {
                window.push_notification("Drop the file on a connection, a database or a schema.", cx);
            } else {
                window.push_notification(NOT_SQL, cx);
            }
            return;
        };
        let metadata = if self.is_sql_connection(&target.connection) {
            TabMetadata { connection_id: target.connection.clone(), database: target.database.clone(), schema: Some(target.schema.clone()).filter(|schema| !schema.is_empty()), ..Default::default() }
        } else {
            TabMetadata::default()
        };
        self.open_dropped_sql_files(paths.paths(), metadata, Some(target), window, cx);
    }

    /// One editor tab per `.sql` file of `paths`, bound by `metadata`; `run` is where a file too big for an editor runs.
    fn open_dropped_sql_files(&mut self, paths: &[PathBuf], metadata: TabMetadata, run: Option<Namespace>, window: &mut Window, cx: &mut Context<Self>) {
        let (files, skipped) = split_sql_files(paths);
        if files.is_empty() {
            window.push_notification(NOT_SQL, cx);
            return;
        }
        if skipped > 0 {
            window.push_notification(format!("{skipped} dropped file{} skipped: only .sql files can be opened.", if skipped == 1 { " was" } else { "s were" }), cx);
        }
        for path in files {
            self.open_sql_path_on(path, Some(DroppedFile { metadata: metadata.clone(), run: run.clone() }), window, cx);
        }
    }

    /// A dropped file is over the editor's limit: the Run SQL File wizard takes it (or says why it cannot).
    pub(in crate::app) fn run_too_large_dropped_file(&mut self, path: PathBuf, run: Option<Namespace>, window: &mut Window, cx: &mut Context<Self>) {
        let Some(target) = run else {
            window.push_notification(format!("{} is too big to open in an editor. Select a connection in the navigator to run it with Run SQL File.", path.display()), cx);
            return;
        };
        if let Err(error) = self.file_run_gate(&target.connection) {
            window.push_notification(format!("{} is too big to open in an editor, and cannot be run: {error}", path.display()), cx);
            return;
        }
        self.open_file_run(FileRunInit { target, path: Some(path) }, window, cx);
    }
}
