//! The state of one structure edit (no entity, no rendering): the pending change-set, its undo
//! history, which changes are stale, and how far the review dialog is.
//!
//! The host (the diagram, the Properties tab) owns an [`EditSession`] and keeps what is its
//! own around it: it installs the draft an edit produced (`set_draft`) after rebuilding whatever it
//! derives from it, and acts on the answers of `check_apply` / `apply_finished`.

use std::cell::{OnceCell, RefCell};
use std::rc::Rc;
use std::sync::{Arc, Weak};

use gpui::{AnyWindowHandle, SharedString};

use super::{ApplyRequest, EditContext, History};
use crate::sql_ddl::draft::Draft;
use crate::sql_ddl::schema::{LiveSchema, Lookup};
use crate::sql_ddl::{plan_against, plan_against_with, warnings, Change, ColumnSpec, Planned, Severity, Warning};

/// The changes of a draft as an undo step keeps them: every change behind its own `Arc`, shared with
/// the steps before and after it (an edit changes one or a few of them), so a step costs a pointer
/// per change and the changes it did not touch exist once.
#[derive(Debug, Clone, Default)]
pub(in crate::app) struct Snapshot(Arc<[Arc<Change>]>);

impl Snapshot {
    fn of(draft: &Draft) -> Self {
        Self(draft.changes.iter().map(|change| Arc::new(change.clone())).collect())
    }

    /// `draft` (an edit of the draft `self` holds), sharing the changes the edit left alone: the ones
    /// before the first and after the last change that differs. `None`: nothing differs.
    fn following(&self, draft: &Draft) -> Option<Self> {
        let (old, new) = (&self.0, &draft.changes);
        let common = old.len().min(new.len());
        let prefix = (0..common).take_while(|&i| *old[i] == new[i]).count();
        if prefix == old.len() && prefix == new.len() {
            return None;
        }
        let suffix = (0..common - prefix).take_while(|&i| *old[old.len() - 1 - i] == new[new.len() - 1 - i]).count();
        let edited = new[prefix..new.len() - suffix].iter().map(|change| Arc::new(change.clone()));
        Some(Self(old[..prefix].iter().cloned().chain(edited).chain(old[old.len() - suffix..].iter().cloned()).collect()))
    }

    fn to_draft(&self) -> Arc<Draft> {
        Arc::new(Draft { changes: self.0.iter().map(|change| (**change).clone()).collect() })
    }
}

impl PartialEq for Snapshot {
    fn eq(&self, other: &Self) -> bool {
        self.0.len() == other.0.len() && self.0.iter().zip(other.0.iter()).all(|(a, b)| Arc::ptr_eq(a, b) || a == b)
    }
}

/// One undoable step of the history of an [`EditSession`]. A host with other undoable edits (the
/// diagram: moves, annotations) wraps the draft's step in its own enum, so undo keeps one order.
pub(in crate::app) trait SessionEdit: Clone {
    /// The step of a draft edit.
    fn of_draft(before: Snapshot, after: Snapshot) -> Self;
    fn is_draft(&self) -> bool;
}

/// The step of a draft edit, for a host whose only undoable edits are the draft's.
#[derive(Debug, Clone, PartialEq)]
pub(in crate::app) struct DraftEdit {
    pub(in crate::app) before: Snapshot,
    pub(in crate::app) after: Snapshot,
}

impl SessionEdit for DraftEdit {
    fn of_draft(before: Snapshot, after: Snapshot) -> Self {
        Self { before, after }
    }

    fn is_draft(&self) -> bool {
        true
    }
}

/// How the dialog and the refusals speak of the place the edit is made from.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(in crate::app) struct Wording {
    /// What to do when a table changed under the draft ("refresh the diagram").
    pub(in crate::app) refresh: &'static str,
    /// The dialog's text when nothing is pending.
    pub(in crate::app) empty: &'static str,
}

impl Wording {
    pub(in crate::app) const DIAGRAM: Self = Self {
        refresh: "refresh the diagram",
        empty: "No pending changes. Edit a table in the Inspector, or right click the canvas > New table.",
    };
    /// The navigator's object actions (drop, rename, truncate, create).
    pub(in crate::app) const OBJECT: Self = Self { refresh: "refresh the navigator", empty: "Nothing is left to apply." };
    pub(in crate::app) const PROPERTIES: Self = Self {
        refresh: "refresh the table properties",
        empty: "No pending changes. Turn on Edit structure, then change a column or the primary key, or add a column.",
    };
}

/// The review dialog's state.
#[derive(Default)]
struct Review {
    window: Option<AnyWindowHandle>,
    /// The user ticked "I understand data will be lost".
    understood: bool,
    /// The user ticked the explicit confirmation naming the connection.
    confirmed: bool,
    applying: bool,
    /// Cancel was pressed for the running apply.
    cancelling: bool,
    /// The change-set Apply sent (the answer names changes by their index in it).
    sent: Vec<Change>,
    /// The last result: `(text, failed)`.
    report: Option<(String, bool)>,
}

/// A copy of what the dialog needs from the review state.
#[derive(Debug, Clone)]
pub(in crate::app) struct ReviewSnapshot {
    pub(in crate::app) understood: bool,
    pub(in crate::app) confirmed: bool,
    pub(in crate::app) applying: bool,
    pub(in crate::app) cancelling: bool,
    pub(in crate::app) report: Option<(String, bool)>,
}

/// What the dialog shows, read from the session on every render: the draft, the plan and the
/// warnings are shared with the session (nothing is copied per render).
pub(super) struct ReviewModel {
    pub(super) draft: Arc<Draft>,
    pub(super) planned: Arc<Planned>,
    pub(super) warnings: Arc<Vec<Warning>>,
    /// The plan's statements as one script (Copy SQL, and what Apply must find unchanged).
    pub(super) script: SharedString,
    pub(super) subject: String,
    pub(super) label: &'static str,
    pub(super) transactional: bool,
    /// The dialect has transactional DDL (`transactional` is off for a change-set that cannot use it).
    pub(super) ddl_transactional: bool,
    pub(super) needs_confirm: bool,
    pub(super) disabled: Option<String>,
    /// Per change: why it cannot be applied any more (its table changed since).
    pub(super) stale: Arc<Vec<Option<String>>>,
}

impl ReviewModel {
    pub(super) fn changes(&self) -> &[Change] {
        &self.draft.changes
    }

    pub(super) fn destructive(&self) -> bool {
        self.warnings.iter().any(|w| w.severity == Severity::Destructive)
    }

    pub(super) fn statements(&self) -> usize {
        self.planned.entries.iter().map(|entry| entry.statements.as_ref().map_or(0, Vec::len)).sum()
    }

    pub(super) fn any_stale(&self) -> bool {
        self.stale.iter().any(Option::is_some)
    }
}

/// Why [`EditSession::check_apply`] sent nothing.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(in crate::app) enum NotSent {
    /// Nothing to say: an apply is running already, or there is nothing to apply.
    Ignored,
    /// The dialog shows this (it is also the session's report).
    Refused(String),
}

/// The pending change-set of one editor.
pub(in crate::app) struct EditSession<E = DraftEdit> {
    /// The pending structure changes.
    pub(in crate::app) draft: Arc<Draft>,
    /// Undo / redo. A host with other undoable edits (the diagram: moves, annotations) records them
    /// here too, so undo keeps one order.
    pub(in crate::app) history: History<E>,
    /// Per pending change: why it can no longer be applied (its table changed since the draft was
    /// made), or `None`. Parallel to `draft.changes`.
    pub(in crate::app) stale: Vec<Option<String>>,
    /// Changes an apply half-ran: their starting point is re-read from the refreshed catalog.
    pub(in crate::app) partial: Vec<Change>,
    /// The catalog the host knows of its tables: the plan refuses a name the tables already use.
    /// A host that does not know it (the diagram) leaves it empty: the plan then only compares the
    /// names the change-set itself uses.
    live: Arc<LiveSchema>,
    /// Counts the catalogs `live` has been set to (the plan cache is keyed on it).
    live_revision: u64,
    /// The plan, warnings and script of the draft, for the catalog and dialect they were made for.
    plan_cache: RefCell<Option<Rc<Plan>>>,
    /// `stale` as the review model shares it: the same `Arc` for as long as `stale` is unchanged.
    stale_shared: RefCell<Option<Arc<Vec<Option<String>>>>>,
    /// The draft as an undo step keeps it, so an edit only has to look for what it changed.
    snapshot: RefCell<Option<(Weak<Draft>, Snapshot)>>,
    review: Review,
}

/// What the review dialog shows of a draft: computed once per draft, not on every render. It
/// remembers its draft by a `Weak`, which keeps the allocation (and so its address) from being
/// reused for another draft while the plan is cached.
struct Plan {
    draft: Weak<Draft>,
    live_revision: u64,
    sql_mode: Option<String>,
    kind: crate::connections::ConnectionKind,
    planned: Arc<Planned>,
    warnings: OnceCell<Arc<Vec<Warning>>>,
    script: OnceCell<SharedString>,
}

impl<E> Default for EditSession<E> {
    fn default() -> Self {
        Self {
            draft: Arc::default(),
            history: History::default(),
            stale: Vec::new(),
            partial: Vec::new(),
            live: Arc::default(),
            live_revision: 0,
            plan_cache: RefCell::new(None),
            stale_shared: RefCell::new(None),
            snapshot: RefCell::new(None),
            review: Review::default(),
        }
    }
}

impl<E: SessionEdit> EditSession<E> {
    pub(in crate::app) fn pending_count(&self) -> usize {
        self.draft.len()
    }

    /// A different document: draft, history and review start empty. (`stale` and `partial` are
    /// recomputed by the host when its catalog arrives.)
    pub(in crate::app) fn clear(&mut self) {
        self.draft = Arc::default();
        self.review = Review::default();
        self.history.clear();
    }

    // ---- the draft ----

    /// Runs `change` on the draft (it may push several changes: one undo step). When it changed
    /// anything the edit is recorded for undo and the new draft is returned; the caller finishes
    /// with [`Self::set_draft`] on it after its own bookkeeping.
    ///
    /// The draft is edited in place when nothing else holds it (no copy of the changes per edit); the
    /// undo step shares every change the edit left alone with the step before it. The edited draft is
    /// the session's draft from the moment `change` ran (`set_draft` of it resets the acknowledgements).
    pub(in crate::app) fn edit(&mut self, change: impl FnOnce(&mut Draft)) -> Option<Arc<Draft>> {
        let before = self.draft_snapshot();
        change(Arc::make_mut(&mut self.draft));
        let Some(after) = before.following(&self.draft) else {
            self.remember_snapshot(before);
            return None;
        };
        self.remember_snapshot(after.clone());
        self.history.push(E::of_draft(before, after));
        Some(Arc::clone(&self.draft))
    }

    /// The snapshot of the draft: the one the last edit made, else made now.
    fn draft_snapshot(&self) -> Snapshot {
        let cached = self.snapshot.borrow().as_ref().filter(|(draft, _)| std::ptr::eq(draft.as_ptr(), Arc::as_ptr(&self.draft))).map(|(_, snapshot)| snapshot.clone());
        cached.unwrap_or_else(|| Snapshot::of(&self.draft))
    }

    fn remember_snapshot(&self, snapshot: Snapshot) {
        *self.snapshot.borrow_mut() = Some((Arc::downgrade(&self.draft), snapshot));
    }

    /// The draft an undo step holds, now the session's draft: the caller finishes the undo / redo
    /// with [`Self::set_draft`] on it.
    pub(in crate::app) fn restore(&mut self, snapshot: &Snapshot) -> Arc<Draft> {
        self.draft = snapshot.to_draft();
        self.remember_snapshot(snapshot.clone());
        Arc::clone(&self.draft)
    }

    /// Replaces the draft (an edit, undo / redo, a loaded document).
    pub(in crate::app) fn set_draft(&mut self, draft: Arc<Draft>) {
        self.draft = draft;
        // The acknowledgements were for the change-set the dialog showed then.
        self.review.understood = false;
        self.review.confirmed = false;
    }

    /// Discards pending change `index` (and what only made sense with it), as an edit.
    pub(in crate::app) fn discard(&mut self, index: usize) -> Option<Arc<Draft>> {
        self.edit(|draft| {
            draft.discard(index);
        })
    }

    pub(in crate::app) fn discard_all(&mut self) -> Option<Arc<Draft>> {
        self.edit(|draft| draft.changes.clear())
    }

    /// The edit to undo, now kept for redo: the caller applies its `before` side.
    pub(in crate::app) fn undo(&mut self) -> Option<E> {
        let edit = self.history.take_undo()?;
        self.history.keep_undone(edit.clone());
        Some(edit)
    }

    /// The edit to redo, now kept for undo: the caller applies its `after` side.
    pub(in crate::app) fn redo(&mut self) -> Option<E> {
        let edit = self.history.take_redo()?;
        self.history.keep_done(edit.clone());
        Some(edit)
    }

    // ---- the review ----

    /// The dialog opens in `window`: its state starts empty.
    pub(in crate::app) fn begin_review(&mut self, window: AnyWindowHandle) {
        self.review = Review { window: Some(window), ..Review::default() };
    }

    pub(in crate::app) fn review_snapshot(&self) -> ReviewSnapshot {
        ReviewSnapshot {
            understood: self.review.understood,
            confirmed: self.review.confirmed,
            applying: self.review.applying,
            cancelling: self.review.cancelling,
            report: self.review.report.clone(),
        }
    }

    /// An Apply is running (the dialog waits for AppRoot's answer).
    #[cfg(test)]
    pub(in crate::app) fn applying(&self) -> bool {
        self.review.applying
    }

    /// The last report of the dialog: `(text, failed)`.
    #[cfg(test)]
    pub(in crate::app) fn report(&self) -> Option<&(String, bool)> {
        self.review.report.as_ref()
    }

    pub(in crate::app) fn set_understood(&mut self, understood: bool) {
        self.review.understood = understood;
    }

    pub(in crate::app) fn set_confirmed(&mut self, confirmed: bool) {
        self.review.confirmed = confirmed;
    }

    #[cfg(test)]
    /// The catalog the plan checks names against (see [`Self::live`]).
    pub(in crate::app) fn set_live(&mut self, live: LiveSchema) {
        self.set_live_shared(Arc::new(live));
    }

    /// [`Self::set_live`] for a host that keeps the catalog itself (it reads it on every rebuild).
    /// Call it when the catalog changed, not on every rebuild: it drops the cached plan.
    pub(in crate::app) fn set_live_shared(&mut self, live: Arc<LiveSchema>) {
        self.live = live;
        self.live_revision += 1;
    }

    /// The plan of the draft against the catalog, in the dialect of `context`: kept for as long as the
    /// draft, the catalog and the dialect stay what they were. `clashes` are the names the draft
    /// takes that its tables already use, when the host has just computed them (they are the plan's
    /// own inputs: it does not compute them again). They must come from the catalog this session
    /// plans against (the `Arc<LiveSchema>` given to [`Self::set_live_shared`]) and the dialect of
    /// `context`; debug builds check it.
    pub(in crate::app) fn planned(&self, context: &EditContext, clashes: Option<&[(usize, String)]>) -> Arc<Planned> {
        Arc::clone(&self.plan(context, clashes).planned)
    }

    fn plan(&self, context: &EditContext, clashes: Option<&[(usize, String)]>) -> Rc<Plan> {
        let draft_at = Arc::as_ptr(&self.draft);
        let hit = self.plan_cache.borrow().as_ref().filter(|plan| {
            std::ptr::eq(plan.draft.as_ptr(), draft_at) && plan.live_revision == self.live_revision && plan.sql_mode == context.sql_mode && plan.kind == context.scope.kind
        }).cloned();
        hit.unwrap_or_else(|| {
            let dialect = context.dialect();
            let changes = &self.draft.changes;
            let planned = match clashes {
                Some(clashes) => {
                    debug_assert_eq!(clashes, crate::sql_ddl::schema::name_clashes(changes, &self.live, dialect.name_rules()).as_slice(), "the clashes are not those of this session's catalog");
                    plan_against_with(&*dialect, changes, clashes)
                }
                None => plan_against(&*dialect, changes, &self.live),
            };
            let plan = Rc::new(Plan {
                draft: Arc::downgrade(&self.draft),
                live_revision: self.live_revision,
                sql_mode: context.sql_mode.clone(),
                kind: context.scope.kind,
                planned: Arc::new(planned),
                warnings: OnceCell::new(),
                script: OnceCell::new(),
            });
            *self.plan_cache.borrow_mut() = Some(Rc::clone(&plan));
            plan
        })
    }

    /// `stale` behind an `Arc` that stays the same while the marks do (the dialog compares it by pointer).
    fn stale_shared(&self) -> Arc<Vec<Option<String>>> {
        let mut shared = self.stale_shared.borrow_mut();
        match shared.as_ref() {
            Some(kept) if **kept == self.stale => Arc::clone(kept),
            _ => {
                let fresh = Arc::new(self.stale.clone());
                *shared = Some(Arc::clone(&fresh));
                fresh
            }
        }
    }

    /// What the dialog shows now (see [`Self::planned`] for what is kept).
    pub(super) fn review_model(&self, context: &EditContext) -> ReviewModel {
        let dialect = context.dialect();
        let plan = self.plan(context, None);
        let found = plan.warnings.get_or_init(|| Arc::new(warnings(&*dialect, &self.draft.changes)));
        let script = plan.script.get_or_init(|| plan.planned.script().into());
        ReviewModel {
            draft: Arc::clone(&self.draft),
            planned: Arc::clone(&plan.planned),
            warnings: Arc::clone(found),
            script: script.clone(),
            stale: self.stale_shared(),
            subject: context.subject.clone(),
            label: dialect.label(),
            transactional: plan.planned.transactional,
            ddl_transactional: dialect.transactional(),
            needs_confirm: context.confirm_execute,
            disabled: context.disabled.clone(),
        }
    }

    /// Apply: checks everything again; the request is the very change-set that was reviewed.
    /// A refusal is also the dialog's report.
    pub(in crate::app) fn check_apply(
        &mut self,
        reviewed: Vec<Change>,
        script: String,
        context: &EditContext,
        wording: &Wording,
    ) -> Result<ApplyRequest, NotSent> {
        if self.review.applying {
            return Err(NotSent::Ignored);
        }
        let model = self.review_model(context);
        let refuse = |this: &mut Self, message: String| {
            this.review.report = Some((message.clone(), true));
            Err(NotSent::Refused(message))
        };
        if let Some(reason) = &model.disabled {
            return refuse(self, reason.clone());
        }
        if reviewed != self.draft.changes {
            return refuse(self, "The pending changes are not the ones that were reviewed. Review them again.".into());
        }
        if reviewed.is_empty() {
            return Err(NotSent::Ignored);
        }
        if model.any_stale() {
            return refuse(
                self,
                format!(
                    "Some changes are stale (their table changed since the draft was made): {}, then discard or redo them.",
                    wording.refresh
                ),
            );
        }
        if !model.planned.executable() || *model.script != *script {
            return refuse(self, "The SQL differs from what was reviewed, or a change cannot be turned into SQL. Review the changes again.".into());
        }
        if model.destructive() && !self.review.understood {
            return refuse(self, "Confirm that data will be lost before applying.".into());
        }
        if model.needs_confirm && !self.review.confirmed {
            return refuse(self, format!("Confirm that {} may run the statements.", model.subject));
        }
        Ok(ApplyRequest { changes: reviewed, script, confirmed: self.review.confirmed })
    }

    /// The request passed [`Self::check_apply`] and goes to AppRoot.
    pub(in crate::app) fn apply_started(&mut self, request: &ApplyRequest) {
        self.review.applying = true;
        self.review.cancelling = false;
        self.review.report = None;
        self.review.sent = request.changes.clone();
    }

    /// The Cancel button of the running apply. `false`: nothing to cancel (or cancelled already).
    pub(in crate::app) fn begin_cancel(&mut self) -> bool {
        if !self.review.applying || self.review.cancelling {
            return false;
        }
        self.review.cancelling = true;
        true
    }

    /// AppRoot ran the change-set. `done` are the indices of the changes that are applied for good,
    /// `partial` the ones that half ran. Returns the completed changes; the caller removes them from
    /// the draft with [`Self::forget_applied`] once its own bookkeeping (renames) has used them.
    pub(in crate::app) fn apply_finished(&mut self, done: &[usize], partial: &[usize], report: &str, failed: bool) -> Vec<Change> {
        self.review.applying = false;
        self.review.cancelling = false;
        self.review.report = Some((report.to_owned(), failed));
        self.review.understood = false;
        self.review.confirmed = false;
        let sent = std::mem::take(&mut self.review.sent);
        self.partial = partial.iter().filter_map(|&i| sent.get(i).cloned()).collect();
        done.iter().filter_map(|&i| sent.get(i).cloned()).collect()
    }

    /// `completed` left the draft for good. What was edited meanwhile stays; undoing a draft edit
    /// would bring back DDL that already ran, so those steps go.
    pub(in crate::app) fn forget_applied(&mut self, completed: &[Change]) {
        self.draft = Arc::new(self.draft.without_changes(completed));
        self.history.retain(|edit| !edit.is_draft());
    }

    /// The changes an apply half-ran start from the catalog's column now: `draft` (the session's, being
    /// reconciled with a freshly loaded `live` catalog) is rewritten, and a change that now equals its
    /// target goes. Returns whether `draft` changed.
    pub(in crate::app) fn reconcile_partial(&mut self, draft: &mut Draft, live: &LiveSchema) -> bool {
        let mut changed = false;
        for half in std::mem::take(&mut self.partial) {
            let Change::AlterColumn { table, from, .. } = &half else { continue };
            let Some(at) = draft.changes.iter().position(|change| *change == half) else { continue };
            let original = crate::sql_ddl::schema::original_table(&draft.changes, table);
            let name = crate::sql_ddl::schema::original_column(&draft.changes, table, &from.name);
            let now = match live.get(&original) {
                Lookup::Present(table_live) => table_live.columns.iter().find(|column| column.name == name).cloned(),
                _ => None,
            };
            if let (Some(now), Change::AlterColumn { from, to, .. }) = (now, &mut draft.changes[at]) {
                *from = ColumnSpec { name: from.name.clone(), ..now };
                if from == to {
                    draft.changes.remove(at);
                }
                changed = true;
            }
        }
        changed
    }

    /// The window of the dialog, once an apply that succeeded asks for it to close.
    pub(in crate::app) fn dialog_to_close(&mut self, failed: bool) -> Option<AnyWindowHandle> {
        if failed {
            return None;
        }
        self.review.window.take()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::sql_ddl::plan;
    use crate::app::structure_edit::EditScope;
    use crate::connections::ConnectionKind;
    use crate::sql_ddl::{ColumnSpec, TableId};

    fn ctx(disabled: Option<&str>, confirm: bool) -> EditContext {
        EditContext {
            scope: EditScope { kind: ConnectionKind::Postgres, database: "db".into(), schema: Some("public".into()) },
            subject: "Production · shop".into(),
            confirm_execute: confirm,
            disabled: disabled.map(str::to_owned),
            sql_mode: None,
            catalog: None,
        catalog_error: None,
        }
    }

    fn table() -> TableId {
        TableId { schema: Some("public".into()), name: "orders".into() }
    }

    fn add(name: &str) -> impl FnOnce(&mut Draft) + '_ {
        move |draft| {
            draft.push(Change::AddColumn {
                table: table(),
                column: ColumnSpec { name: name.into(), data_type: "integer".into(), nullable: true, ..ColumnSpec::default() },
            })
        }
    }

    fn drop_column(draft: &mut Draft) {
        draft.push(Change::DropColumn { table: table(), column: "legacy".into() });
    }

    type PendingEdit = Box<dyn FnOnce(&mut Draft)>;

    /// A session holding `edits` applied one by one, installed like a host does.
    fn session(edits: Vec<PendingEdit>) -> EditSession {
        let mut session: EditSession = EditSession::default();
        for edit in edits {
            let after = session.edit(edit).expect("changes the draft");
            session.set_draft(after);
        }
        session.stale = vec![None; session.draft.len()];
        session
    }

    fn reviewed(session: &EditSession, context: &EditContext) -> (Vec<Change>, String) {
        let changes = session.draft.changes.clone();
        let script = plan(&*context.dialect(), &changes).script();
        (changes, script)
    }

    fn refusal(result: Result<ApplyRequest, NotSent>) -> String {
        match result {
            Err(NotSent::Refused(message)) => message,
            other => panic!("expected a refusal, got {other:?}"),
        }
    }

    #[test]
    fn an_edit_is_recorded_and_undo_redo_walk_the_history() {
        let mut session: EditSession = EditSession::default();
        assert_eq!(session.pending_count(), 0);
        let after = session.edit(add("a")).expect("a new column");
        assert_eq!(session.pending_count(), 1, "the edit is the session's draft; the host's set_draft finishes it");
        session.set_draft(after);
        let after = session.edit(add("b")).expect("another one");
        session.set_draft(after);
        assert_eq!(session.pending_count(), 2);
        assert_eq!(session.history.undo_len(), 2);

        let Some(DraftEdit { before, .. }) = session.undo() else { panic!("a draft edit") };
        let before = session.restore(&before);
        session.set_draft(before);
        assert_eq!(session.pending_count(), 1);
        assert!(session.history.can_redo());
        let Some(DraftEdit { after, .. }) = session.redo() else { panic!("a draft edit") };
        let after = session.restore(&after);
        session.set_draft(after);
        assert_eq!(session.pending_count(), 2);
        assert!(!session.history.can_redo());

        // A new edit drops what could be redone.
        let Some(DraftEdit { before, .. }) = session.undo() else { panic!("a draft edit") };
        let before = session.restore(&before);
        session.set_draft(before);
        let after = session.edit(add("c")).expect("a different column");
        session.set_draft(after);
        assert!(session.redo().is_none());
        assert!(session.undo().is_some() && session.undo().is_some() && session.undo().is_none());
    }

    /// Every draft after each of `edits`, for undo and redo to be compared against.
    fn walk(session: &mut EditSession, edits: Vec<PendingEdit>) -> Vec<Vec<Change>> {
        let mut seen = vec![session.draft.changes.clone()];
        for edit in edits {
            let after = session.edit(edit).expect("changes the draft");
            session.set_draft(after);
            seen.push(session.draft.changes.clone());
        }
        seen
    }

    fn undo_to(session: &mut EditSession) {
        let Some(DraftEdit { before, .. }) = session.undo() else { panic!("a draft edit") };
        let draft = session.restore(&before);
        session.set_draft(draft);
    }

    fn redo_to(session: &mut EditSession) {
        let Some(DraftEdit { after, .. }) = session.redo() else { panic!("a draft edit") };
        let draft = session.restore(&after);
        session.set_draft(draft);
    }

    fn alter_note(draft: &mut Draft) {
        let spec = |nullable| ColumnSpec { name: "note".into(), data_type: "text".into(), nullable, ..ColumnSpec::default() };
        draft.push(Change::AlterColumn { table: table(), from: spec(true), to: spec(false) });
    }

    #[test]
    fn undo_and_redo_restore_the_exact_changes_of_every_step() {
        let mut session: EditSession = EditSession::default();
        // Steps that share changes (an edit leaves most of them alone), one that drops the first pending one.
        let edits: Vec<PendingEdit> = vec![
            Box::new(add("a")),
            Box::new(add("b")),
            Box::new(drop_column),
            Box::new(alter_note),
            Box::new(add("c")),
            Box::new(|draft| {
                draft.discard(0);
            }),
        ];
        let seen = walk(&mut session, edits);
        for expected in seen.iter().rev().skip(1) {
            undo_to(&mut session);
            assert_eq!(&session.draft.changes, expected);
        }
        assert!(!session.history.can_undo());
        for expected in seen.iter().skip(1) {
            redo_to(&mut session);
            assert_eq!(&session.draft.changes, expected);
        }
        // Undo again after the redo, then a new edit and its undo.
        undo_to(&mut session);
        undo_to(&mut session);
        assert_eq!(session.draft.changes, seen[seen.len() - 3]);
        let after = session.edit(add("z")).expect("a new column");
        session.set_draft(after);
        undo_to(&mut session);
        assert_eq!(session.draft.changes, seen[seen.len() - 3]);
    }

    #[test]
    fn undo_after_an_apply_removed_changes_or_a_reload_replaced_the_draft_restores_the_exact_changes() {
        let mut session = session(vec![Box::new(add("a")), Box::new(add("b")), Box::new(add("c"))]);
        let applied = vec![session.draft.changes[0].clone()];
        session.forget_applied(&applied);
        assert_eq!(session.history.undo_len(), 0, "the steps from before the apply are gone");
        let rest = session.draft.changes.clone();
        let after = session.edit(add("d")).expect("a new column");
        session.set_draft(after);
        let with_d = session.draft.changes.clone();
        assert_eq!(with_d[..2], rest[..]);
        undo_to(&mut session);
        assert_eq!(session.draft.changes, rest);
        redo_to(&mut session);
        assert_eq!(session.draft.changes, with_d);

        // A reload replaces the draft behind the session's back (the host rewrites it with the catalog).
        let reloaded = Draft::from_changes(vec![session.draft.changes[1].clone(), session.draft.changes[2].clone()]);
        session.set_draft(Arc::new(reloaded.clone()));
        let after = session.edit(add("e")).expect("a new column");
        session.set_draft(after);
        let with_e = session.draft.changes.clone();
        assert_eq!(with_e[..2], reloaded.changes[..]);
        undo_to(&mut session);
        assert_eq!(session.draft.changes, reloaded.changes);
        redo_to(&mut session);
        assert_eq!(session.draft.changes, with_e);
    }

    #[test]
    fn an_edit_that_changes_nothing_records_nothing() {
        let mut session = session(vec![Box::new(add("a"))]);
        assert!(session.edit(|_| {}).is_none());
        // Adding the same column again folds into the pending one.
        assert!(session.edit(add("a")).is_none());
        assert_eq!(session.history.undo_len(), 1);
    }

    #[test]
    fn discarding_one_or_all_changes_is_an_undoable_edit() {
        let mut session = session(vec![Box::new(add("a")), Box::new(drop_column)]);
        assert_eq!(session.pending_count(), 2);
        let after = session.discard(0).expect("a pending change");
        session.set_draft(after);
        assert_eq!(session.draft.changes.len(), 1);
        assert!(matches!(session.draft.changes[0], Change::DropColumn { .. }));
        let after = session.discard_all().expect("one is left");
        session.set_draft(after);
        assert_eq!(session.pending_count(), 0);
        assert!(session.discard_all().is_none(), "nothing left to discard");
        assert_eq!(session.history.undo_len(), 4);
        let Some(DraftEdit { before, .. }) = session.undo() else { panic!("a draft edit") };
        let before = session.restore(&before);
        session.set_draft(before);
        assert_eq!(session.pending_count(), 1);
    }

    #[test]
    fn setting_the_draft_resets_the_acknowledgements() {
        let mut session = session(vec![Box::new(drop_column)]);
        session.set_understood(true);
        session.set_confirmed(true);
        let after = session.edit(add("a")).unwrap();
        session.set_draft(after);
        let snapshot = session.review_snapshot();
        assert!(!snapshot.understood && !snapshot.confirmed);
    }

    #[test]
    fn apply_is_refused_with_the_reason_the_dialog_shows() {
        let context = ctx(None, true);
        let mut session = session(vec![Box::new(add("a")), Box::new(drop_column)]);
        let (changes, script) = reviewed(&session, &context);

        let disabled = ctx(Some("Structure editing is blocked."), false);
        assert_eq!(refusal(session.check_apply(changes.clone(), script.clone(), &disabled, &Wording::DIAGRAM)), "Structure editing is blocked.");
        assert_eq!(session.report(), Some(&("Structure editing is blocked.".to_owned(), true)));

        assert_eq!(
            refusal(session.check_apply(changes[..1].to_vec(), script.clone(), &context, &Wording::DIAGRAM)),
            "The pending changes are not the ones that were reviewed. Review them again."
        );
        assert_eq!(
            refusal(session.check_apply(changes.clone(), "SELECT 1;".into(), &context, &Wording::DIAGRAM)),
            "The SQL differs from what was reviewed, or a change cannot be turned into SQL. Review the changes again."
        );
        assert_eq!(
            refusal(session.check_apply(changes.clone(), script.clone(), &context, &Wording::DIAGRAM)),
            "Confirm that data will be lost before applying."
        );
        session.set_understood(true);
        assert_eq!(
            refusal(session.check_apply(changes.clone(), script.clone(), &context, &Wording::DIAGRAM)),
            "Confirm that Production · shop may run the statements."
        );
        session.set_confirmed(true);
        let request = session.check_apply(changes.clone(), script.clone(), &context, &Wording::DIAGRAM).expect("everything is confirmed");
        assert_eq!((request.changes, request.script), (changes, script));
    }

    #[test]
    fn a_name_the_catalog_has_is_refused_by_the_review_plan_and_the_apply() {
        use crate::sql_ddl::schema::LiveTable;
        let context = ctx(None, false);
        let mut session = session(vec![Box::new(add("note"))]);
        // Without a catalog the name looks free.
        assert!(session.review_model(&context).planned.executable());
        let mut live = LiveSchema::new(false);
        live.present(
            table(),
            LiveTable { columns: vec![ColumnSpec { name: "note".into(), data_type: "text".into(), nullable: true, ..ColumnSpec::default() }], ..LiveTable::default() },
        );
        session.set_live(live);
        let model = session.review_model(&context);
        assert!(!model.planned.executable());
        assert_eq!(model.planned.first_error().map(|(_, error)| error.to_string()).as_deref(), Some("A column named \"note\" already exists in public.orders."));
        let (changes, script) = (session.draft.changes.clone(), model.planned.script());
        assert_eq!(
            refusal(session.check_apply(changes, script, &context, &Wording::DIAGRAM)),
            "The SQL differs from what was reviewed, or a change cannot be turned into SQL. Review the changes again."
        );
    }

    #[test]
    fn the_review_plan_is_computed_once_per_draft_catalog_and_dialect() {
        let context = ctx(None, false);
        let mut session = session(vec![Box::new(add("a"))]);
        let first = session.review_model(&context);
        let again = session.review_model(&context);
        assert!(Arc::ptr_eq(&first.planned, &again.planned) && Arc::ptr_eq(&first.warnings, &again.warnings), "the same draft: the same plan");
        // Another sql_mode quotes differently: a new plan.
        let moded = EditContext { sql_mode: Some("ANSI_QUOTES".into()), ..ctx(None, false) };
        assert!(!Arc::ptr_eq(&first.planned, &session.review_model(&moded).planned));
        // Another draft or another catalog: a new plan.
        let before = session.review_model(&context);
        let after = session.edit(add("b")).expect("a new column");
        session.set_draft(after);
        let edited = session.review_model(&context);
        assert!(!Arc::ptr_eq(&before.planned, &edited.planned) && edited.planned.entries.len() == 2);
        session.set_live(LiveSchema::new(false));
        assert!(!Arc::ptr_eq(&edited.planned, &session.review_model(&context).planned));
    }

    #[test]
    fn stale_changes_are_not_applied() {
        let context = ctx(None, false);
        let mut session = session(vec![Box::new(add("a"))]);
        session.stale = vec![Some("the column exists now".into())];
        let (changes, script) = reviewed(&session, &context);
        assert_eq!(
            refusal(session.check_apply(changes, script, &context, &Wording::DIAGRAM)),
            "Some changes are stale (their table changed since the draft was made): refresh the diagram, then discard or redo them."
        );
    }

    #[test]
    fn nothing_to_apply_and_a_running_apply_are_ignored() {
        let context = ctx(None, false);
        let mut empty: EditSession = EditSession::default();
        assert_eq!(empty.check_apply(Vec::new(), String::new(), &context, &Wording::DIAGRAM).err(), Some(NotSent::Ignored));
        assert!(empty.report().is_none(), "ignored, not reported");

        let mut session = session(vec![Box::new(add("a"))]);
        let (changes, script) = reviewed(&session, &context);
        let request = session.check_apply(changes.clone(), script.clone(), &context, &Wording::DIAGRAM).expect("ready");
        session.apply_started(&request);
        assert!(session.applying());
        assert_eq!(session.check_apply(changes, script, &context, &Wording::DIAGRAM).err(), Some(NotSent::Ignored));
    }

    #[test]
    fn cancel_works_once_while_an_apply_runs() {
        let context = ctx(None, false);
        let mut session = session(vec![Box::new(add("a"))]);
        assert!(!session.begin_cancel(), "nothing runs");
        let (changes, script) = reviewed(&session, &context);
        let request = session.check_apply(changes, script, &context, &Wording::DIAGRAM).unwrap();
        session.apply_started(&request);
        assert!(session.begin_cancel());
        assert!(session.review_snapshot().cancelling);
        assert!(!session.begin_cancel(), "cancelled already");
    }

    #[test]
    fn a_finished_apply_names_the_changes_that_ran() {
        let context = ctx(None, false);
        let mut session = session(vec![Box::new(add("a")), Box::new(add("b")), Box::new(add("c"))]);
        let (changes, script) = reviewed(&session, &context);
        let request = session.check_apply(changes.clone(), script, &context, &Wording::DIAGRAM).unwrap();
        session.apply_started(&request);
        assert!(session.report().is_none(), "the report of the last try is gone");
        // The user edits while the apply runs: a fourth change arrives.
        let after = session.edit(add("d")).unwrap();
        session.set_draft(after);

        let completed = session.apply_finished(&[0, 1], &[2], "Statement 3 of 3 failed: boom", true);
        assert_eq!(completed, changes[..2].to_vec());
        assert_eq!(session.partial, vec![changes[2].clone()]);
        let snapshot = session.review_snapshot();
        assert!(!snapshot.applying && !snapshot.cancelling);
        assert_eq!(snapshot.report, Some(("Statement 3 of 3 failed: boom".to_owned(), true)));
        assert!(session.dialog_to_close(true).is_none(), "a failed apply keeps the dialog");

        session.forget_applied(&completed);
        assert_eq!(session.draft.changes.len(), 2);
        assert_eq!(session.draft.changes[0], changes[2]);
        assert!(matches!(&session.draft.changes[1], Change::AddColumn { column, .. } if column.name == "d"));
        assert_eq!(session.history.undo_len(), 0, "undoing would bring back DDL that already ran");
    }

    #[test]
    fn a_failed_answer_without_changes_leaves_the_draft_alone() {
        let context = ctx(None, false);
        let mut session = session(vec![Box::new(add("a"))]);
        session.set_understood(true);
        let (changes, script) = reviewed(&session, &context);
        let request = session.check_apply(changes.clone(), script, &context, &Wording::DIAGRAM).unwrap();
        session.apply_started(&request);
        assert!(session.apply_finished(&[], &[], "boom", true).is_empty());
        assert_eq!(session.draft.changes, changes);
        assert!(!session.review_snapshot().understood, "the acknowledgements start over");
        assert!(session.partial.is_empty());
    }

    #[test]
    fn clear_starts_a_different_document() {
        let mut session = session(vec![Box::new(add("a"))]);
        session.set_understood(true);
        session.clear();
        assert_eq!(session.pending_count(), 0);
        assert!(!session.history.can_undo());
        assert!(!session.review_snapshot().understood);
    }
}
