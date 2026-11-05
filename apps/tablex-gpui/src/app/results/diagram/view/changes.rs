//! The diagram's side of the review dialog (`structure_edit`): the "Changes" button, and what the
//! diagram adds around Apply, the answer of AppRoot and the layout bookkeeping after an apply.
//!
//! Apply hands the change-set to AppRoot (`DiagramEvent::Apply`), which runs it on the connection
//! and answers with [`DiagramView::apply_finished`].

use super::*;
use crate::app::structure_edit::{self, EditContext, NotSent, ReviewHost, Wording};
use crate::sql_ddl::Change;

impl ReviewHost for DiagramView {
    type Edit = Edit;
    const WORDING: Wording = Wording::DIAGRAM;

    fn session(&self) -> &structure_edit::EditSession<Edit> {
        &self.session
    }

    fn session_mut(&mut self) -> &mut structure_edit::EditSession<Edit> {
        &mut self.session
    }

    fn edit_context(&self) -> Option<&EditContext> {
        self.edit.as_ref()
    }

    fn before_review(&mut self, cx: &mut Context<Self>) {
        self.commit_editor_inputs(cx);
    }

    fn discard_change(&mut self, index: usize, cx: &mut Context<Self>) {
        DiagramView::discard_change(self, index, cx);
    }

    fn discard_all_changes(&mut self, cx: &mut Context<Self>) {
        DiagramView::discard_all_changes(self, cx);
    }

    fn request_apply(&mut self, reviewed: Vec<Change>, script: String, cx: &mut Context<Self>) {
        DiagramView::request_apply(self, reviewed, script, cx);
    }

    fn cancel_apply(&mut self, cx: &mut Context<Self>) {
        DiagramView::cancel_apply(self, cx);
    }

    fn show_notice(&mut self, text: String, failed: bool, cx: &mut Context<Self>) {
        self.notice = Some((text, failed));
        cx.notify();
    }
}

impl DiagramView {
    /// What the toolbar button and its overflow entry say: `Changes (N)`.
    pub(in crate::app) fn changes_label(&self) -> String {
        match self.pending_count() {
            0 => "Changes".to_string(),
            pending => format!("Changes ({pending})"),
        }
    }

    /// The toolbar button.
    pub(super) fn changes_button(&self, cx: &mut Context<Self>) -> Button {
        let pending = self.pending_count();
        Button::new("diagram-changes")
            .icon(Icon::empty().path("icons/pencil-ruler.svg").size(px(14.0)))
            .label(self.changes_label())
            .with_size(Size::Small)
            .compact()
            .ghost()
            .selected(pending > 0)
            .flex_shrink_0()
            .tooltip(if pending > 0 {
                "Review and apply the pending structure changes"
            } else {
                "No pending structure changes: edit a table in the Inspector, or right click > New table"
            })
            .debug_selector(|| "diagram-changes".into())
            .on_click(cx.listener(|this, _, window, cx| this.open_changes(window, cx)))
    }

    /// Opens the review dialog.
    pub(in crate::app) fn open_changes(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        structure_edit::open_review(self, window, cx);
    }

    /// An Apply is running (the dialog waits for AppRoot's answer).
    #[cfg(test)]
    pub(in crate::app) fn review_applying(&self) -> bool {
        self.session.applying()
    }

    /// Apply: checks everything again, then hands the change-set (the very one that was
    /// reviewed) to AppRoot, which runs it on a dedicated session of the connection.
    pub(in crate::app) fn request_apply(&mut self, reviewed: Vec<Change>, script: String, cx: &mut Context<Self>) {
        let Some(context) = self.edit.as_ref() else { return };
        let request = match self.session.check_apply(reviewed, script, context, &Wording::DIAGRAM) {
            Ok(request) => request,
            Err(NotSent::Ignored) => return,
            Err(NotSent::Refused(_)) => return cx.notify(),
        };
        // Saved before anything runs: after a crash the changes of an interrupted apply that are in
        // the catalog are dropped from the draft instead of being applied twice.
        self.applying_marker = request.changes.clone();
        self.doc_dirty = true;
        self.flush_blocking();
        self.session.apply_started(&request);
        cx.emit(DiagramEvent::Apply(Box::new(request)));
        cx.notify();
    }

    /// The Cancel button of the running apply.
    pub(in crate::app) fn cancel_apply(&mut self, cx: &mut Context<Self>) {
        if self.session.begin_cancel() {
            cx.emit(DiagramEvent::CancelApply);
            cx.notify();
        }
    }

    /// Opens the confirmation of "Drop table...": the table is dropped when the
    /// change-set is applied, not now.
    pub(in crate::app) fn request_drop_table(&mut self, index: usize, window: &mut Window, cx: &mut Context<Self>) {
        let Some(label) = self.graph().and_then(|g| g.entities.get(index)).map(|e| e.label.clone()) else { return };
        if !self.editable() {
            return;
        }
        if !self.drop_needs_confirmation(index) {
            self.drop_table(index, cx);
            return;
        }
        let view = cx.entity();
        window.open_alert_dialog(cx, move |dialog, _, _| {
            let view = view.clone();
            dialog
                .title(format!("Drop table {label}?"))
                .child(
                    div()
                        .debug_selector(|| "drop-table-confirm".into())
                        .flex()
                        .flex_col()
                        .gap(px(6.0))
                        .child(format!(
                            "Drop table {label} is added to the pending changes. The table and every row in it are removed from the database when you apply them."
                        ))
                        .child(div().text_color(theme::text_muted()).child(
                            "To only hide the table from this diagram, use Remove from diagram instead.",
                        )),
                )
                .button_props(gpui_component::dialog::DialogButtonProps::default().ok_text("Drop table"))
                .confirm()
                .on_ok(move |_, _, cx| {
                    view.update(cx, |this, cx| this.drop_table(index, cx));
                    true
                })
        });
    }

    /// AppRoot ran the change-set. `done` are the indices of the changes that
    /// are applied for good; they leave the draft and what depends on them
    /// follows (renamed tables keep their place, colors and groups; a logical
    /// relationship that became a foreign key is dropped). The text is the
    /// report of what ran.
    pub(in crate::app) fn apply_finished(&mut self, done: &[usize], partial: &[usize], report: String, failed: bool, cx: &mut Context<Self>) {
        let completed = self.session.apply_finished(done, partial, &report, failed);
        // The apply is over: the marker goes (written at once, with whatever the apply changed).
        self.applying_marker.clear();
        if completed.is_empty() {
            self.save_after_apply(&[]);
        } else {
            self.forget_applied(&completed);
        }
        self.notice = Some((report.lines().next().unwrap_or_default().to_owned(), failed));
        cx.notify();
        if let Some(window) = self.session.dialog_to_close(failed) {
            structure_edit::close_dialog_later(window, cx);
        }
    }

    fn forget_applied(&mut self, completed: &[Change]) {
        let scope = self.edit.as_ref().map(|edit| edit.scope.clone());
        let graph = self.graph().cloned();
        let key_of_table = |table: &crate::sql_ddl::TableId| -> Option<TableKey> {
            let (scope, graph) = (scope.as_ref()?, graph.as_ref()?);
            graph.entities.iter().find(|e| scope.id(&e.table) == *table).map(|e| TableKey::of(&e.table))
        };
        let mut annotations = (*self.annotations).clone();
        let mut renames: Vec<(TableKey, TableKey)> = Vec::new();
        for change in completed {
            match change {
                Change::RenameTable { table, to } => {
                    if let Some(from) = key_of_table(table) {
                        let target = TableKey { schema: from.schema.clone(), table: to.clone() };
                        annotations.rename_table(&from, &target);
                        renames.push((from, target));
                    }
                }
                Change::RenameColumn { table, from, to } => {
                    if let Some(key) = key_of_table(table) {
                        annotations.rename_column(&key, from, to);
                    }
                }
                Change::AddForeignKey { table, foreign_key } => {
                    // The foreign key exists now: the logical relationship it
                    // promoted is redundant.
                    let (child, parent) = (key_of_table(table), key_of_table(&foreign_key.ref_table));
                    annotations.relations.retain(|r| {
                        !(Some(r.child.key()) == child
                            && Some(r.parent.key()) == parent
                            && r.child.columns == foreign_key.columns
                            && r.parent.columns == foreign_key.ref_columns)
                    });
                }
                _ => {}
            }
        }
        self.annotations = Arc::new(annotations);
        self.session.forget_applied(completed);
        self.editor.revision += 1;
        // Written at once, under the new names: the refresh that follows rebuilds
        // the diagram from this file's positions.
        self.save_after_apply(&renames);
    }
}
