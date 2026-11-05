//! D3 (structure) editing of the diagram view: the pending change-set of the
//! diagram and every edit that adds to it.
//!
//! Edits never touch the database. They go through
//! [`DiagramView::edit_draft`] (the [`EditSession`](crate::app::structure_edit::EditSession)
//! copies the [`Draft`], pushes a [`Change`] into it and records `(before, after)` for undo; the
//! view rebuilds the effective graph and saves the document), exactly like annotation edits go
//! through `edit_annotations`. The change-set is applied from the review dialog (`changes.rs`).

use super::*;
use crate::app::results::diagram::overlay::TableState;
use crate::sql_ddl::draft::Draft;
use crate::sql_ddl::schema::{already_applied, stale, LiveSchema};
use crate::sql_ddl::{Change, ColumnSpec, FkAction, ForeignKeySpec, PrimaryKeyRef, TableId};
use gpui::Entity;
use gpui_component::select::SelectState;

/// The inline editors of the inspector, created on first use.
pub(super) struct EditorInputs {
    pub(super) table: Entity<InputState>,
    pub(super) name: Entity<InputState>,
    pub(super) data_type: Entity<InputState>,
    pub(super) default: Entity<InputState>,
    pub(super) comment: Entity<InputState>,
    pub(super) index_name: Entity<InputState>,
    pub(super) fk_name: Entity<InputState>,
    _subscriptions: Vec<Subscription>,
}

/// The "add index" form.
#[derive(Default)]
pub(super) struct IndexForm {
    pub(super) columns: Vec<String>,
    pub(super) unique: bool,
}

/// The "add foreign key" form.
pub(super) struct FkForm {
    pub(super) columns: Vec<String>,
    /// Entity index of the referenced table.
    pub(super) parent: Option<usize>,
    pub(super) parent_columns: Vec<String>,
    pub(super) on_delete: Option<FkAction>,
    pub(super) select: Entity<SelectState<Vec<gpui::SharedString>>>,
    pub(super) on_delete_select: Entity<SelectState<Vec<gpui::SharedString>>>,
    /// Entity indices behind the select's rows.
    pub(super) choices: Vec<usize>,
    pub(super) _subscriptions: Vec<Subscription>,
}

/// What the inspector's inputs show: the table, the selected column and the
/// draft revision, so typing is never overwritten.
type Shown = (TableKey, Option<String>, u64);

#[derive(Default)]
pub(super) struct Editor {
    pub(super) inputs: Option<EditorInputs>,
    /// Index of the selected column in the selected entity.
    pub(super) column: Option<usize>,
    pub(super) shown: Option<Shown>,
    /// Bumped by every change of the draft.
    pub(super) revision: u64,
    pub(super) index_form: Option<IndexForm>,
    pub(super) fk_form: Option<FkForm>,
    /// The table the open forms are for.
    pub(super) forms_for: Option<TableKey>,
    /// Why the last edit was refused (shown in the inspector).
    pub(super) error: Option<String>,
}

/// What a diagram holds of one entity's layout while the entity list changes.
pub(super) struct LayoutMemory {
    keys: Vec<TableKey>,
    rects: Vec<DiagramRect>,
    pinned: Vec<bool>,
    selected: Option<TableKey>,
    selection: Vec<TableKey>,
}

impl DiagramView {
    // ---- context ----

    /// AppRoot's answer to "may this diagram be edited, and how".
    pub(in crate::app) fn set_edit_context(&mut self, context: Option<EditContext>, cx: &mut Context<Self>) {
        if self.edit == context {
            return;
        }
        let rescope = self.edit.as_ref().map(|c| &c.scope) != context.as_ref().map(|c| &c.scope);
        self.edit = context;
        if rescope && !self.session.draft.is_empty() {
            let memory = self.layout_memory();
            self.rebuild_effective();
            self.restore_layout(memory);
            self.refresh_derived();
        }
        cx.notify();
    }

    /// Structure edits are on: a SQL connection that may be written to.
    pub(in crate::app) fn editable(&self) -> bool {
        self.edit.as_ref().is_some_and(|edit| edit.disabled.is_none())
    }

    /// Why structure editing is off, when it is.
    pub(in crate::app) fn edit_disabled_reason(&self) -> Option<&str> {
        self.edit.as_ref().and_then(|edit| edit.disabled.as_deref())
    }

    /// The diagram supports structure changes at all (a SQL connection).
    pub(in crate::app) fn supports_structure(&self) -> bool {
        self.edit.is_some()
    }

    pub(in crate::app) fn pending_count(&self) -> usize {
        self.session.pending_count()
    }

    /// The state of entity `index` against the database.
    pub(in crate::app) fn table_state(&self, index: usize) -> TableState {
        self.entity_states.get(index).copied().unwrap_or_default()
    }

    /// The name the draft knows entity `index` by.
    pub(in crate::app) fn table_id(&self, index: usize) -> Option<TableId> {
        let scope = &self.edit.as_ref()?.scope;
        let entity = self.graph()?.entities.get(index)?;
        let original = scope.id(&entity.table);
        // A table renamed by the draft is known by its new name.
        Some(
            self.session.draft
                .changes
                .iter()
                .find_map(|change| match change {
                    Change::RenameTable { table, to } if *table == original => {
                        Some(TableId { schema: original.schema.clone(), name: to.clone() })
                    }
                    _ => None,
                })
                .unwrap_or(original),
        )
    }

    // ---- the draft ----

    /// Runs `change` on the draft (in place when nothing else holds it). When it changed anything
    /// the edit is recorded for undo and the effective graph is rebuilt. Returns whether it
    /// changed anything.
    pub(in crate::app) fn edit_draft(&mut self, cx: &mut Context<Self>, change: impl FnOnce(&mut Draft)) -> bool {
        if !self.editable() {
            return false;
        }
        let edited = self.session.edit(change);
        self.install_edit(edited, cx)
    }

    /// Installs the draft an [`EditSession`](crate::app::structure_edit::EditSession) edit produced.
    fn install_edit(&mut self, edited: Option<Arc<Draft>>, cx: &mut Context<Self>) -> bool {
        let Some(after) = edited else { return false };
        self.set_draft(after, cx);
        true
    }

    /// Replaces the draft (an edit, undo / redo, a loaded document).
    pub(in crate::app) fn set_draft(&mut self, draft: Arc<Draft>, cx: &mut Context<Self>) {
        let memory = self.layout_memory();
        self.session.set_draft(draft);
        self.editor.revision += 1;
        self.editor.error = None;
        self.rebuild_effective();
        self.restore_layout(memory);
        self.refresh_derived();
        self.refresh_search();
        self.refresh_stale();
        self.needs_fit |= self.auto_fit;
        self.mark_dirty(cx);
    }

    // ---- staleness ----

    /// What the declared graph says about its tables, for the staleness checks.
    fn declared_live_schema(&self) -> Option<LiveSchema> {
        let (edit, declared, snapshot) = (self.edit.as_ref()?, self.declared.as_ref()?, self.snapshot.as_ref()?);
        let complete = self.kind == DiagramKind::Schema
            && snapshot.progress.is_none()
            && snapshot.truncated.is_none()
            && snapshot.error.is_none()
            && declared.entities.iter().all(|e| e.state == EntityState::Ready);
        Some(declared.live_schema(&edit.scope, complete))
    }

    /// Marks every pending change that no longer starts from the catalog's structure.
    pub(super) fn refresh_stale(&mut self) {
        self.session.stale = vec![None; self.session.draft.len()];
        let Some(live) = self.declared_live_schema() else { return };
        for (index, reason) in stale(&self.session.draft.changes, &live) {
            self.session.stale[index] = Some(reason);
        }
    }

    /// Everything a freshly loaded catalog tells about the draft: the changes of an apply that
    /// was interrupted and are in the catalog already leave the draft; a change an apply half-ran
    /// starts from the catalog's column now; the rest is checked for staleness.
    pub(super) fn reconcile(&mut self, cx: &mut Context<Self>) {
        let ready = self.snapshot.as_ref().is_some_and(|s| s.error.is_none())
            && self.declared.as_ref().is_some_and(|g| !g.entities.is_empty() && g.entities.iter().all(|e| e.state == EntityState::Ready));
        let (Some(live), true) = (self.declared_live_schema(), ready) else {
            self.refresh_stale();
            return;
        };
        let mut draft = (*self.session.draft).clone();
        let mut changed = false;
        if !self.applying_marker.is_empty() {
            let marker = std::mem::take(&mut self.applying_marker);
            let applied: Vec<Change> = already_applied(&marker, &live).into_iter().map(|index| marker[index].clone()).collect();
            draft = draft.without_changes(&applied);
            changed = true;
        }
        changed |= self.session.reconcile_partial(&mut draft, &live);
        if changed {
            self.set_draft(Arc::new(draft), cx);
        } else {
            self.refresh_stale();
        }
    }

    pub(super) fn layout_memory(&self) -> Option<LayoutMemory> {
        let graph = self.graph()?;
        let keys: Vec<TableKey> = graph.entities.iter().map(|entity| TableKey::of(&entity.table)).collect();
        let key_of = |id: EntityId| keys.get(id.0).cloned();
        Some(LayoutMemory {
            rects: self.layout.rects.clone(),
            pinned: self.pinned.clone(),
            selected: self.selected.and_then(key_of),
            selection: self.selection.iter().filter_map(|&id| key_of(id)).collect(),
            keys,
        })
    }

    /// Re-lays out after the entities changed: boxes keep their places (the
    /// pinned ones exactly; the others follow the automatic layout), a created
    /// table is placed by the layout, the selection follows the tables.
    pub(super) fn restore_layout(&mut self, memory: Option<LayoutMemory>) {
        let (Some(memory), Some(graph)) = (memory, self.graph()) else {
            return;
        };
        let keys: Vec<TableKey> = graph.entities.iter().map(|entity| TableKey::of(&entity.table)).collect();
        if keys != memory.keys {
            // Which old entity each new one is: the same table, or (a table the
            // draft creates and renames) the one left over in the same order.
            let old: HashMap<&TableKey, usize> = memory.keys.iter().enumerate().map(|(index, key)| (key, index)).collect();
            let kept: Vec<Option<usize>> = keys.iter().map(|key| old.get(key).copied()).collect();
            let left_old: Vec<usize> = (0..memory.keys.len()).filter(|i| !kept.contains(&Some(*i))).collect();
            let left_new: Vec<usize> = (0..keys.len()).filter(|&i| kept[i].is_none()).collect();
            let mut from: Vec<Option<usize>> = kept;
            if left_old.len() == left_new.len() {
                for (&new, &old) in left_new.iter().zip(&left_old) {
                    from[new] = Some(old);
                }
            }
            self.layout.rects = from.iter().map(|old| old.map_or_else(DiagramRect::default, |at| memory.rects[at])).collect();
            self.pinned = from.iter().map(|old| old.is_some_and(|at| memory.pinned.get(at).copied().unwrap_or(false))).collect();
            let new_index = |key: &TableKey| {
                let at = memory.keys.iter().position(|k| k == key)?;
                from.iter().position(|old| *old == Some(at)).map(EntityId)
            };
            self.selected = memory.selected.as_ref().and_then(new_index);
            self.selection = memory.selection.iter().filter_map(new_index).collect();
            self.selection.sort_unstable();
            self.hovered = None;
            self.hover_row = None;
        }
        self.reflow_all();
    }

    // ---- tables ----

    /// A name for a new table or column that `taken` does not use.
    fn fresh_name(base: &str, taken: impl Fn(&str) -> bool) -> String {
        if !taken(base) {
            return base.to_owned();
        }
        (2..).map(|n| format!("{base}_{n}")).find(|name| !taken(name)).unwrap_or_else(|| base.to_owned())
    }

    /// Right click on empty canvas > New table: a draft table with an `id`
    /// primary key at the clicked place, selected and ready to be named.
    pub(in crate::app) fn create_table_at(&mut self, at: DiagramPoint, window: &mut Window, cx: &mut Context<Self>) {
        let Some(scope) = self.edit.as_ref().map(|edit| edit.scope.clone()).filter(|_| self.editable()) else {
            return;
        };
        let dialect = scope.dialect();
        let taken: Vec<TableId> =
            self.graph().map(|g| g.entities.iter().map(|e| scope.id(&e.table)).collect()).unwrap_or_default();
        let catalog = self.edit.as_ref().and_then(|edit| edit.catalog.clone());
        let name = Self::fresh_name("new_table", |name| {
            taken.iter().any(|t| t.name == name)
                || self.session.draft.touches(&scope.new_table(name))
                || catalog.as_ref().is_some_and(|names| names.iter().any(|n| n == name))
        });
        let table = scope.new_table(&name);
        let int = dialect.common_types().first().copied().unwrap_or("integer");
        let id = ColumnSpec { name: "id".into(), data_type: int.into(), nullable: false, ..ColumnSpec::default() };
        // The view stays where the user is looking: the new table appears under the pointer.
        self.auto_fit = false;
        self.needs_fit = false;
        self.edit_draft(cx, |draft| {
            draft.push(Change::CreateTable { table: table.clone(), columns: vec![id], primary_key: vec!["id".into()], comment: None });
        });
        let Some(index) = self.graph().and_then(|g| g.entities.iter().position(|e| e.table == scope.table_ref(&table))) else {
            return;
        };
        if let Some(rect) = self.layout.rects.get_mut(index) {
            (rect.x, rect.y) = (at.x, at.y);
            self.pinned.resize(self.layout.rects.len(), false);
            self.pinned[index] = true;
            self.moved = true;
            self.reflow_all();
            self.refresh_geometry();
        }
        self.clear_selection_state();
        self.selected = Some(EntityId(index));
        self.selection.push(EntityId(index));
        self.panel = Some(Panel::Inspector);
        self.editor.column = None;
        self.focus_table_name(window, cx);
        self.mark_dirty(cx);
    }

    /// Renames the table of entity `index`.
    pub(in crate::app) fn rename_table(&mut self, index: usize, name: &str, cx: &mut Context<Self>) {
        let (Some(table), name) = (self.table_id(index), name.trim()) else { return };
        if name.is_empty() || name == table.name {
            return;
        }
        // Against every table of the namespace the navigator knows, not only the ones the diagram shows.
        let in_catalog = self.edit.as_ref().and_then(|edit| edit.catalog.as_ref()).is_some_and(|names| names.iter().any(|n| n == name));
        let exists = in_catalog
            || self.graph().is_some_and(|g| {
                g.entities.iter().enumerate().any(|(i, e)| i != index && (e.label == name || e.table.table == name))
            });
        if exists {
            self.editor.error = Some(format!("There is a table named {name} already."));
            cx.notify();
            return;
        }
        self.edit_draft(cx, |draft| draft.push(Change::RenameTable { table, to: name.to_owned() }));
    }

    /// "Drop table…" was confirmed: the table is dropped by the change-set
    /// (a table the draft creates is just forgotten).
    pub(in crate::app) fn drop_table(&mut self, index: usize, cx: &mut Context<Self>) {
        let Some(table) = self.table_id(index) else { return };
        self.edit_draft(cx, |draft| draft.push(Change::DropTable { table, cascade: false }));
    }

    /// Whether dropping the table of entity `index` needs the user's confirmation
    /// (not for a table that only exists in the draft).
    pub(in crate::app) fn drop_needs_confirmation(&self, index: usize) -> bool {
        self.table_state(index) != TableState::New
    }

    /// Takes back a pending drop of the table of entity `index`.
    pub(in crate::app) fn restore_table(&mut self, index: usize, cx: &mut Context<Self>) {
        let Some(table) = self.table_id(index) else { return };
        let Some(at) = self.session.draft.changes.iter().position(|c| matches!(c, Change::DropTable { table: t, .. } if *t == table)) else {
            return;
        };
        self.discard_change(at, cx);
    }

    // ---- columns ----

    pub(in crate::app) fn add_column(&mut self, index: usize, window: &mut Window, cx: &mut Context<Self>) {
        let (Some(table), Some(scope)) = (self.table_id(index), self.edit.as_ref().map(|e| e.scope.clone())) else { return };
        let Some(entity) = self.graph().and_then(|g| g.entities.get(index)) else { return };
        let name = Self::fresh_name("new_column", |name| entity.columns.iter().any(|c| c.name == name));
        let data_type = scope.dialect().common_types().first().copied().unwrap_or("integer");
        let column = ColumnSpec { name, data_type: data_type.into(), nullable: true, ..ColumnSpec::default() };
        let position = entity.columns.len();
        self.edit_draft(cx, |draft| draft.push(Change::AddColumn { table, column }));
        self.select_column(Some(position), window, cx);
        self.focus_column_name(window, cx);
    }

    /// Drops column `column` of entity `index` (a column added by the draft is
    /// just forgotten).
    pub(in crate::app) fn drop_column(&mut self, index: usize, column: usize, cx: &mut Context<Self>) {
        let Some(table) = self.table_id(index) else { return };
        let Some(name) = self.graph().and_then(|g| g.entities.get(index)).and_then(|e| e.columns.get(column)).map(|c| c.name.clone())
        else {
            return;
        };
        let count = self.graph().and_then(|g| g.entities.get(index)).map_or(0, |e| e.columns.len());
        self.edit_draft(cx, |draft| draft.push(Change::DropColumn { table, column: name }));
        // A dropped column stays listed (marked, restorable); one the draft added is gone.
        if self.graph().and_then(|g| g.entities.get(index)).map_or(0, |e| e.columns.len()) < count {
            self.editor.column = None;
        }
    }

    /// Takes back the pending drop of column `column`.
    pub(in crate::app) fn restore_column(&mut self, index: usize, column: usize, cx: &mut Context<Self>) {
        let Some(table) = self.table_id(index) else { return };
        let Some(name) = self.graph().and_then(|g| g.entities.get(index)).and_then(|e| e.columns.get(column)).map(|c| c.name.clone())
        else {
            return;
        };
        let Some(at) = self
            .session
            .draft
            .changes
            .iter()
            .position(|c| matches!(c, Change::DropColumn { table: t, column: n } if *t == table && *n == name))
        else {
            return;
        };
        self.discard_change(at, cx);
    }

    /// Pushes the edit of column `column` of entity `index`: a rename and / or
    /// an alteration, one undo step. `edit` changes a copy of the column spec.
    pub(in crate::app) fn edit_column(
        &mut self,
        index: usize,
        column: usize,
        cx: &mut Context<Self>,
        edit: impl FnOnce(&mut ColumnSpec),
    ) {
        let Some(table) = self.table_id(index) else { return };
        let Some(from) = self.graph().and_then(|g| g.entities.get(index)).and_then(|e| e.column_spec(column)) else { return };
        let mut to = from.clone();
        edit(&mut to);
        if to == from {
            return;
        }
        if to.name != from.name {
            let taken = self
                .graph()
                .and_then(|g| g.entities.get(index))
                .is_some_and(|e| e.columns.iter().enumerate().any(|(i, c)| i != column && c.name == to.name));
            if to.name.trim().is_empty() || taken {
                self.editor.error = Some(if taken {
                    format!("There is a column named {} already.", to.name)
                } else {
                    "A column needs a name.".to_owned()
                });
                cx.notify();
                return;
            }
        }
        let renamed = to.name != from.name;
        // Specs carry the name the column has when the alteration runs.
        let before = ColumnSpec { name: to.name.clone(), ..from.clone() };
        self.edit_draft(cx, |draft| {
            if renamed {
                draft.push(Change::RenameColumn { table: table.clone(), from: from.name.clone(), to: to.name.clone() });
            }
            if before != to {
                draft.push(Change::AlterColumn { table, from: before, to });
            }
        });
    }

    /// Sets or clears the primary-key flag of column `column`.
    pub(in crate::app) fn set_primary_key(&mut self, index: usize, column: usize, on: bool, cx: &mut Context<Self>) {
        let Some(table) = self.table_id(index) else { return };
        let Some(entity) = self.graph().and_then(|g| g.entities.get(index)) else { return };
        let Some(target) = entity.columns.get(column) else { return };
        let current: Vec<String> = entity.columns.iter().filter(|c| c.primary_key).map(|c| c.name.clone()).collect();
        if target.primary_key == on {
            return;
        }
        let name = target.name.clone();
        let mut columns: Vec<String> = entity
            .columns
            .iter()
            .filter(|c| if c.name == name { on } else { c.primary_key })
            .map(|c| c.name.clone())
            .collect();
        columns.dedup();
        let previous = PrimaryKeyRef { name: entity.primary_key_name.clone(), columns: current };
        let previous = Some(previous).filter(|previous| !previous.columns.is_empty());
        self.edit_draft(cx, |draft| draft.push(Change::SetPrimaryKey { table, columns, previous }));
    }

    // ---- indexes and foreign keys ----

    pub(in crate::app) fn add_index(&mut self, index: usize, name: &str, columns: Vec<String>, unique: bool, cx: &mut Context<Self>) -> bool {
        let Some(table) = self.table_id(index) else { return false };
        let existing = self.graph().and_then(|g| g.entities.get(index));
        let taken = |name: &str| existing.is_some_and(|e| e.indexes.iter().any(|i| i.name == name));
        match structure_edit::specs::index_spec(name, columns, unique, taken) {
            Ok(spec) => self.edit_draft(cx, |draft| draft.push(Change::CreateIndex { table, index: spec })),
            Err(message) => {
                self.editor.error = Some(message);
                cx.notify();
                false
            }
        }
    }

    pub(in crate::app) fn drop_index(&mut self, index: usize, name: &str, cx: &mut Context<Self>) {
        let Some(table) = self.table_id(index) else { return };
        self.edit_draft(cx, |draft| draft.push(Change::DropIndex { table, name: name.to_owned() }));
    }

    pub(in crate::app) fn add_foreign_key(&mut self, index: usize, spec: ForeignKeySpec, cx: &mut Context<Self>) -> bool {
        let Some(table) = self.table_id(index) else { return false };
        if let Err(message) = structure_edit::specs::check_foreign_key(&spec) {
            self.editor.error = Some(message);
            cx.notify();
            return false;
        }
        self.edit_draft(cx, |draft| draft.push(Change::AddForeignKey { table, foreign_key: spec }))
    }

    /// Drops the real or pending foreign key behind relation `relation` of the graph.
    pub(in crate::app) fn drop_foreign_key(&mut self, relation: usize, cx: &mut Context<Self>) {
        let Some((child, name, origin)) =
            self.graph().and_then(|g| g.relations.get(relation)).map(|r| (r.child.0, r.name.clone(), r.origin))
        else {
            return;
        };
        if matches!(origin, RelationOrigin::Logical(_)) {
            return;
        }
        let Some(table) = self.table_id(child) else { return };
        self.edit_draft(cx, |draft| draft.push(Change::DropForeignKey { table, name }));
    }

    /// The `ForeignKeySpec` a relation of the graph stands for (a logical
    /// relationship being promoted to a real key).
    pub(in crate::app) fn foreign_key_of(&self, relation: usize) -> Option<(usize, ForeignKeySpec)> {
        let graph = self.graph()?;
        let relation = graph.relations.get(relation)?;
        let (child, parent) = (graph.entities.get(relation.child.0)?, graph.entities.get(relation.parent.0)?);
        let columns: Vec<String> = relation.child_columns.iter().map(|&c| child.columns[c].name.clone()).collect();
        let ref_columns: Vec<String> = relation.parent_columns.iter().map(|&c| parent.columns[c].name.clone()).collect();
        if columns.is_empty() || columns.len() != ref_columns.len() {
            return None;
        }
        let name = structure_edit::specs::default_foreign_key_name(&child.table.table, &columns, |_| false);
        Some((
            relation.child.0,
            ForeignKeySpec {
                name,
                columns,
                ref_table: self.table_id(relation.parent.0)?,
                ref_columns,
                on_delete: None,
                on_update: None,
            },
        ))
    }

    /// "Create foreign key" on a logical relationship: the change-set gets a
    /// real foreign key with the same columns (the logical one is dropped from
    /// the annotations once it has been applied).
    pub(in crate::app) fn promote_logical(&mut self, relation: usize, cx: &mut Context<Self>) -> bool {
        let Some((child, spec)) = self.foreign_key_of(relation) else { return false };
        self.add_foreign_key(child, spec, cx)
    }

    // ---- view only: remove from diagram ----

    /// "Remove from diagram": the table disappears from this view; it stays in
    /// the database (and in the other diagrams). The focus table of a table
    /// diagram cannot be removed.
    pub(in crate::app) fn remove_from_diagram(&mut self, entities: &[EntityId], cx: &mut Context<Self>) {
        let keys: Vec<TableKey> = entities
            .iter()
            .filter_map(|&id| self.graph()?.entities.get(id.0).filter(|e| !e.focus).map(|e| TableKey::of(&e.table)))
            .collect();
        if keys.is_empty() {
            return;
        }
        self.edit_annotations(cx, |annotations| annotations.remove_tables(&keys));
    }

    /// Brings every removed table back.
    pub(in crate::app) fn restore_removed(&mut self, cx: &mut Context<Self>) {
        self.edit_annotations(cx, |annotations| annotations.restore_removed());
    }

    // ---- the change list ----

    /// Discards one pending change (and what only made sense with it).
    pub(in crate::app) fn discard_change(&mut self, index: usize, cx: &mut Context<Self>) {
        if self.editable() {
            let edited = self.session.discard(index);
            self.install_edit(edited, cx);
        }
    }

    pub(in crate::app) fn discard_all_changes(&mut self, cx: &mut Context<Self>) {
        if self.editable() {
            let edited = self.session.discard_all();
            self.install_edit(edited, cx);
        }
    }

    /// Selects column `column` of the selected table in the inspector.
    pub(in crate::app) fn select_column(&mut self, column: Option<usize>, window: &mut Window, cx: &mut Context<Self>) {
        self.commit_editor_inputs(cx);
        self.editor.column = column;
        self.sync_editor_inputs(window, cx);
        cx.notify();
    }
}

// ---- the inspector's inputs ---------------------------------------------------------------

impl DiagramView {
    /// Creates the inspector's inputs on first use.
    pub(super) fn ensure_editor_inputs(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.editor.inputs.is_some() {
            return;
        }
        let make = |placeholder: &'static str, window: &mut Window, cx: &mut Context<Self>| {
            cx.new(|cx| InputState::new(window, cx).placeholder(placeholder))
        };
        let (table, name, data_type, default, comment) = (
            make("Table name", window, cx),
            make("Column name", window, cx),
            make("Type, e.g. varchar(255)", window, cx),
            make("Default (value or expression)", window, cx),
            make("Comment", window, cx),
        );
        let (index_name, fk_name) = (make("Index name", window, cx), make("Constraint name", window, cx));
        let mut subscriptions = Vec::new();
        for state in [&table, &name, &data_type, &default, &comment] {
            subscriptions.push(cx.subscribe_in(state, window, move |this, _, event: &InputEvent, window, cx| match event {
                InputEvent::PressEnter { .. } => {
                    this.commit_editor_inputs(cx);
                    window.focus(&this.focus_handle, cx);
                }
                InputEvent::Blur => this.commit_editor_inputs(cx),
                InputEvent::Focus | InputEvent::Change => {}
            }));
        }
        self.editor.inputs = Some(EditorInputs {
            table,
            name,
            data_type,
            default,
            comment,
            index_name,
            fk_name,
            _subscriptions: subscriptions,
        });
    }

    /// The identity of what the inputs should show now.
    fn editor_key(&self) -> Option<Shown> {
        let entity = self.graph()?.entities.get(self.selected?.0)?;
        let column = self.editor.column.and_then(|c| entity.columns.get(c)).map(|c| c.name.clone());
        Some((TableKey::of(&entity.table), column, self.editor.revision))
    }

    /// Shows the selected table and column in the inputs (never while the
    /// user types in one: only when the table, column or draft changed).
    pub(super) fn sync_editor_inputs(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(key) = self.editor_key() else {
            self.editor.shown = None;
            return;
        };
        if self.editor.shown.as_ref() == Some(&key) {
            return;
        }
        let (Some(inputs), Some(graph), Some(selected)) = (self.editor.inputs.as_ref(), self.graph(), self.selected) else {
            return;
        };
        let Some(entity) = graph.entities.get(selected.0) else { return };
        let column = self.editor.column.and_then(|c| entity.columns.get(c));
        // The name the table has (not the label, which may carry the schema).
        let table_name = self.table_id(selected.0).map_or_else(|| entity.label.clone(), |table| table.name);
        let texts = [
            (&inputs.table, table_name),
            (&inputs.name, column.map(|c| c.name.clone()).unwrap_or_default()),
            (&inputs.data_type, column.map(|c| c.data_type.clone()).unwrap_or_default()),
            (&inputs.default, column.and_then(|c| c.default.clone()).unwrap_or_default()),
            (&inputs.comment, column.and_then(|c| c.comment.clone()).unwrap_or_default()),
        ];
        for (state, text) in texts {
            state.update(cx, |state, cx| state.set_value(text, window, cx));
        }
        self.editor.shown = Some(key);
    }

    /// Stores what was typed in the inputs as draft changes (Enter, leaving a
    /// field, selecting another column, applying). A field equal to what the
    /// draft shows changes nothing.
    pub(in crate::app) fn commit_editor_inputs(&mut self, cx: &mut Context<Self>) {
        let Some((key, column_name, _)) = self.editor.shown.clone() else { return };
        let Some(inputs) = self.editor.inputs.as_ref() else { return };
        let (table, name, data_type, default, comment) = (
            inputs.table.read(cx).value().to_string(),
            inputs.name.read(cx).value().to_string(),
            inputs.data_type.read(cx).value().to_string(),
            inputs.default.read(cx).value().to_string(),
            inputs.comment.read(cx).value().to_string(),
        );
        let Some(graph) = self.graph() else { return };
        let Some(index) = graph.entities.iter().position(|e| TableKey::of(&e.table) == key) else { return };
        let column = column_name.as_deref().and_then(|name| graph.entities[index].columns.iter().position(|c| c.name == name));
        self.rename_table(index, &table, cx);
        if let Some(column) = column {
            // The inputs may be stale after the rename; the entity index is not.
            let nullable_untouched = |spec: &mut ColumnSpec| {
                spec.name = name.trim().to_owned();
                spec.data_type = data_type.trim().to_owned();
                spec.default = Some(default.trim().to_owned()).filter(|d| !d.is_empty());
                spec.comment = Some(comment.trim().to_owned()).filter(|c| !c.is_empty());
            };
            self.edit_column(index, column, cx, nullable_untouched);
        }
    }

    #[cfg(test)]
    pub(in crate::app) fn editor_debug(&self, cx: &gpui::App) -> String {
        format!(
            "column {:?} shown {:?} name {:?}",
            self.editor.column,
            self.editor.shown,
            self.editor.inputs.as_ref().map(|i| i.name.read(cx).value().to_string())
        )
    }

    /// The last report of the review dialog: `(text, failed)` (tests).
    #[cfg(test)]
    pub(in crate::app) fn review_report(&self) -> Option<(String, bool)> {
        self.session.report().cloned()
    }

    /// How many changes the saved "an apply is running" marker holds (tests).
    #[cfg(test)]
    pub(in crate::app) fn applying_marker_len(&self) -> usize {
        self.applying_marker.len()
    }

    #[cfg(test)]
    pub(in crate::app) fn editor_error(&self) -> Option<String> {
        self.editor.error.clone()
    }

    /// The referenced table chosen in the foreign key form (tests).
    #[cfg(test)]
    pub(in crate::app) fn editor_fk_parent(&self) -> Option<usize> {
        self.editor.fk_form.as_ref().and_then(|form| form.parent)
    }

    pub(super) fn focus_table_name(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.ensure_editor_inputs(window, cx);
        self.sync_editor_inputs(window, cx);
        if let Some(inputs) = &self.editor.inputs {
            inputs.table.update(cx, |state, cx| state.focus(window, cx));
        }
    }

    pub(super) fn focus_column_name(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.ensure_editor_inputs(window, cx);
        self.sync_editor_inputs(window, cx);
        if let Some(inputs) = &self.editor.inputs {
            inputs.name.update(cx, |state, cx| state.focus(window, cx));
        }
    }

    /// Whether any inspector input has the focus.
    pub(super) fn editor_input_focused(&self, window: &Window, cx: &gpui::App) -> bool {
        self.editor.inputs.as_ref().is_some_and(|inputs| {
            [&inputs.table, &inputs.name, &inputs.data_type, &inputs.default, &inputs.comment, &inputs.index_name, &inputs.fk_name]
                .into_iter()
                .any(|state| state.read(cx).focus_handle(cx).is_focused(window))
        })
    }
}

// ---- capture seed -------------------------------------------------------------------------

impl DiagramView {
    /// Capture-only (`column=<Name>`): selects that column of the selected table.
    pub(super) fn apply_seed_column(&mut self) {
        let Some(wanted) = self.seed_column.clone() else { return };
        let Some(entity) = self.selected.and_then(|id| self.graph()?.entities.get(id.0)) else { return };
        if let Some(position) = entity.columns.iter().position(|c| c.name == wanted) {
            self.seed_column = None;
            self.editor.column = Some(position);
        }
    }

    /// Capture-only (`TABLEX_SEED_DIAGRAM=draft`): stages sample edits of the
    /// Chinook catalog once it has loaded: a retyped and a renamed column, a
    /// new column, a dropped one and a new table with a foreign key.
    pub(super) fn apply_seed_draft(&mut self, cx: &mut Context<Self>) {
        let (Some(edit), Some(graph)) = (self.edit.clone(), self.graph().cloned()) else { return };
        if !self.seed_draft || graph.entities.is_empty() || graph.entities.iter().any(|e| e.state != EntityState::Ready) {
            return;
        }
        self.seed_draft = false;
        let scope = edit.scope;
        let find = |name: &str| graph.entities.iter().find(|e| e.table.table == name);
        let Some(track) = find("Track") else { return };
        let spec = |entity: &DiagramEntity, name: &str| entity.column_spec(entity.columns.iter().position(|c| c.name == name)?);
        let track_id = scope.id(&track.table);
        let mut draft = Draft::default();
        if let Some(mut from) = spec(track, "UnitPrice") {
            if self.seed_stale {
                // The draft was made when the column was narrower, and before a column of this name appeared:
                // the catalog has moved on, so the two changes are stale (and the rest of the sample is left out).
                from.data_type = "numeric(8, 2)".into();
                let to = ColumnSpec { comment: Some("List price in USD, before tax".into()), ..from.clone() };
                draft.push(Change::AlterColumn { table: track_id.clone(), from, to });
                draft.push(Change::AddColumn {
                    table: track_id.clone(),
                    column: ColumnSpec { name: "Name".into(), data_type: "text".into(), nullable: true, ..ColumnSpec::default() },
                });
                draft.push(Change::DropColumn { table: track_id, column: "Bytes".into() });
                self.set_draft(Arc::new(draft), cx);
                return;
            }
            let to = ColumnSpec { data_type: "numeric(12, 4)".into(), comment: Some("List price in USD, before tax".into()), ..from.clone() };
            draft.push(Change::AlterColumn { table: track_id.clone(), from, to });
        }
        draft.push(Change::RenameColumn { table: track_id.clone(), from: "Composer".into(), to: "Writer".into() });
        draft.push(Change::AddColumn {
            table: track_id.clone(),
            column: ColumnSpec { name: "BitRate".into(), data_type: "integer".into(), nullable: true, comment: Some("kbit/s".into()), ..ColumnSpec::default() },
        });
        draft.push(Change::DropColumn { table: track_id.clone(), column: "Bytes".into() });
        let review = scope.new_table("Review");
        draft.push(Change::CreateTable {
            table: review.clone(),
            columns: vec![
                ColumnSpec { name: "ReviewId".into(), data_type: "bigint".into(), nullable: false, ..ColumnSpec::default() },
                ColumnSpec { name: "TrackId".into(), data_type: "integer".into(), nullable: false, ..ColumnSpec::default() },
                ColumnSpec { name: "Rating".into(), data_type: "smallint".into(), nullable: false, default: Some("3".into()), ..ColumnSpec::default() },
                ColumnSpec { name: "Body".into(), data_type: "text".into(), nullable: true, ..ColumnSpec::default() },
            ],
            primary_key: vec!["ReviewId".into()],
            comment: None,
        });
        draft.push(Change::AddForeignKey {
            table: review,
            foreign_key: ForeignKeySpec {
                name: "FK_Review_TrackId".into(),
                columns: vec!["TrackId".into()],
                ref_table: track_id,
                ref_columns: vec!["TrackId".into()],
                on_delete: Some(FkAction::Cascade),
                on_update: None,
            },
        });
        self.set_draft(Arc::new(draft), cx);
    }
}
