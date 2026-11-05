//! The entity behind the review dialog of one object action: the draft (one change, or the
//! `CreateTable` of the new-table form), the [`EditContext`] AppRoot handed over, and the shared
//! [`EditSession`] that checks Apply and records its answer. The dialog itself, the ticks that
//! change the SQL and the apply round trip are the structure editor's (`structure_edit`).

use std::sync::Arc;

use gpui::{Context, EventEmitter};

use super::super::structure_edit::{
    self, apply::TableOrigin, ApplyRequest, DraftEdit, EditContext, EditSession, NotSent, ReviewHost, ReviewOption, Wording,
};
use crate::sql_account::Password;
use crate::sql_ddl::draft::Draft;
use crate::sql_ddl::Change;

/// What the host asks AppRoot to do.
pub(in crate::app) enum ObjectActionEvent {
    /// Apply in the review dialog: run these changes on the connection.
    Apply(Box<ApplyRequest>),
    /// Cancel in the progress of a running apply.
    CancelApply,
}

impl EventEmitter<ObjectActionEvent> for ObjectActionHost {}

/// Wipes the passwords it holds when it is dropped.
struct WipeOnDrop(Vec<Password>);

impl Drop for WipeOnDrop {
    fn drop(&mut self) {
        self.0.iter().for_each(Password::wipe);
    }
}

pub(in crate::app) struct ObjectActionHost {
    origin: TableOrigin,
    context: EditContext,
    session: EditSession<DraftEdit>,
    /// A short status line (copied SQL, how an apply ended).
    notice: Option<(String, bool)>,
}

impl ObjectActionHost {
    pub(in crate::app) fn new(origin: TableOrigin, context: EditContext, changes: Vec<Change>) -> Self {
        let mut host = Self { origin, context, session: EditSession::default(), notice: None };
        host.session.set_draft(Arc::new(Draft::from_changes(changes)));
        host.session.stale = vec![None; host.session.draft.len()];
        host
    }

    /// The object the action started from.
    pub(in crate::app) fn origin(&self) -> TableOrigin {
        self.origin.clone()
    }

    /// The server's `sql_mode`, the connection's policy or the catalog changed under the dialog.
    pub(in crate::app) fn set_context(&mut self, context: EditContext, cx: &mut Context<Self>) {
        if self.context != context {
            self.context = context;
            cx.notify();
        }
    }

    /// The passwords the draft sets (one shared cell each: wiping one wipes every copy of it).
    fn passwords(&self) -> Vec<Password> {
        self.session
            .draft
            .changes
            .iter()
            .filter_map(|change| match change {
                Change::Account(account) => account.password().cloned(),
                _ => None,
            })
            .collect()
    }

    #[cfg(test)]
    pub(in crate::app) fn draft_changes(&self) -> &[Change] {
        &self.session.draft.changes
    }

    #[cfg(test)]
    pub(in crate::app) fn review_applying(&self) -> bool {
        self.session.applying()
    }

    #[cfg(test)]
    pub(in crate::app) fn review_report(&self) -> Option<(String, bool)> {
        self.session.report().cloned()
    }

    /// Installs `draft` (an edit of the dialog's own: a tick, a discard).
    fn set_draft(&mut self, draft: Arc<Draft>, cx: &mut Context<Self>) {
        self.session.set_draft(draft);
        self.session.stale = vec![None; self.session.draft.len()];
        cx.notify();
    }

    /// AppRoot ran the change-set: what ran leaves the draft, and the dialog closes after a run that
    /// succeeded.
    pub(in crate::app) fn apply_finished(&mut self, done: &[usize], partial: &[usize], report: String, failed: bool, cx: &mut Context<Self>) {
        // Whatever the answer, a password the apply carried is not kept: a retry asks for it again.
        let passwords = self.passwords();
        passwords.iter().for_each(Password::wipe);
        let completed = self.session.apply_finished(done, partial, &report, failed);
        if !completed.is_empty() {
            self.session.forget_applied(&completed);
            self.session.stale = vec![None; self.session.draft.len()];
        }
        if !passwords.is_empty() {
            // The dialog plans again: a change whose password is gone shows that it cannot be applied until it is typed again.
            let fresh = Arc::new((*self.session.draft).clone());
            self.set_draft(fresh, cx);
        }
        self.notice = Some((report.lines().next().unwrap_or_default().to_owned(), failed));
        cx.notify();
        if let Some(window) = self.session.dialog_to_close(failed) {
            structure_edit::close_dialog_later(window, cx);
        }
    }
}

impl ReviewHost for ObjectActionHost {
    type Edit = DraftEdit;
    const WORDING: Wording = Wording::OBJECT;

    fn session(&self) -> &EditSession<DraftEdit> {
        &self.session
    }

    fn session_mut(&mut self) -> &mut EditSession<DraftEdit> {
        &mut self.session
    }

    fn edit_context(&self) -> Option<&EditContext> {
        Some(&self.context)
    }

    fn before_review(&mut self, _: &mut Context<Self>) {}

    fn discard_change(&mut self, index: usize, cx: &mut Context<Self>) {
        if let Some(after) = self.session.discard(index) {
            self.set_draft(after, cx);
        }
    }

    fn discard_all_changes(&mut self, cx: &mut Context<Self>) {
        if let Some(after) = self.session.discard_all() {
            self.set_draft(after, cx);
        }
    }

    fn request_apply(&mut self, reviewed: Vec<Change>, script: String, cx: &mut Context<Self>) {
        let request = match self.session.check_apply(reviewed, script, &self.context, &Wording::OBJECT) {
            Ok(request) => request,
            Err(NotSent::Ignored) => return,
            Err(NotSent::Refused(_)) => return cx.notify(),
        };
        self.session.apply_started(&request);
        cx.emit(ObjectActionEvent::Apply(Box::new(request)));
        cx.notify();
    }

    fn cancel_apply(&mut self, cx: &mut Context<Self>) {
        if self.session.begin_cancel() {
            cx.emit(ObjectActionEvent::CancelApply);
            cx.notify();
        }
    }

    fn show_notice(&mut self, text: String, failed: bool, cx: &mut Context<Self>) {
        self.notice = Some((text, failed));
        cx.notify();
    }

    /// The passwords of the draft are wiped when the dialog closes, applied or not.
    fn dialog_guard(&self) -> Option<std::rc::Rc<dyn std::any::Any>> {
        let passwords = self.passwords();
        (!passwords.is_empty()).then(|| std::rc::Rc::new(WipeOnDrop(passwords)) as std::rc::Rc<dyn std::any::Any>)
    }

    /// CASCADE of a drop, RESTART IDENTITY and CASCADE of a truncate, where the dialect has them.
    fn options(&self) -> Vec<ReviewOption> {
        let caps = self.context.dialect().object_capabilities();
        let option = |key, label: String, checked| ReviewOption { key, label, checked };
        match self.session.draft.changes.as_slice() {
            [Change::DropTable { table, cascade }] | [Change::DropView { table, cascade, .. }] if caps.drop_cascade => {
                vec![option("cascade", format!("Also drop everything that depends on {table} (CASCADE)"), *cascade)]
            }
            [Change::TruncateTable { table, restart_identity, cascade }] if caps.truncate_options => vec![
                option("restart_identity", "Restart the identity (sequence) columns (RESTART IDENTITY)".to_owned(), *restart_identity),
                option("cascade", format!("Also empty the tables that reference {table} (CASCADE)"), *cascade),
            ],
            _ => Vec::new(),
        }
    }

    fn set_option(&mut self, key: &'static str, on: bool, cx: &mut Context<Self>) {
        let edited = self.session.edit(|draft| {
            for change in &mut draft.changes {
                match (key, change) {
                    ("cascade", Change::DropTable { cascade, .. } | Change::DropView { cascade, .. } | Change::TruncateTable { cascade, .. }) => {
                        *cascade = on
                    }
                    ("restart_identity", Change::TruncateTable { restart_identity, .. }) => *restart_identity = on,
                    _ => {}
                }
            }
        });
        if let Some(draft) = edited {
            self.set_draft(draft, cx);
        }
    }
}
