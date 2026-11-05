//! D2 editing of the diagram view: logical relationships, relationship
//! suggestions, notes, entity colors, groups, multi-select and undo / redo.
//!
//! Every edit of the annotations goes through [`DiagramView::edit_annotations`]
//! (copy, change, record `(before, after)`, rebuild what depends on it, save);
//! moves are recorded from the positions at the press of a drag. Nothing here
//! adds a listener per item: presses are decoded once in the area's handler
//! (`on_mouse_down`) against the layout, the notes and the group frames.

use super::*;
use crate::app::results::diagram::annotations::{
    cardinality_label, group_frame, AnnotationError, Group, Note, RelationEnd, GROUP_TITLE, SWATCHES,
};
use crate::app::results::diagram::history::MoveStep;
use crate::app::results::diagram::infer;
use gpui::{Entity, Rgba, WeakEntity};
use gpui_component::input::{Escape, TextareaState};
use gpui_component::menu::PopupMenu;
use std::collections::HashMap;

/// Side of the note resize corner, screen pixels.
const NOTE_HANDLE: f32 = 16.0;
/// Width of the band at the right edge of a row that starts a link drag,
/// diagram units (the whole row does with Alt held).
const LINK_BAND: f32 = 18.0;

/// What the side dock shows.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(in crate::app) enum Panel {
    Suggestions,
    Layers,
    Inspector,
}

/// What a right click landed on.
#[derive(Debug, Clone, Copy, PartialEq)]
pub(in crate::app) enum ContextTarget {
    Entity(EntityId),
    /// Index into `graph.relations`.
    Relation(usize),
    Note(u64),
    Group(u64),
    Canvas,
}

/// What the running drag started from (for its undo step).
#[derive(Default)]
pub(super) struct DragStart {
    /// Entities moved by the drag with their positions at the press.
    positions: Vec<(usize, DiagramPoint)>,
    /// The annotations at the press (note drags).
    annotations: Option<Arc<Annotations>>,
    /// The selection a rubber band adds to.
    base: Vec<EntityId>,
    /// A plain press on an already selected entity of a multi-selection
    /// selects it alone on release unless the drag moved something.
    reduce_to: Option<EntityId>,
}

/// The inline editor of one note.
pub(super) struct NoteEdit {
    pub(super) id: u64,
    pub(super) state: Entity<TextareaState>,
    _subscription: Subscription,
}

/// The inline name editor of one group (Layers panel).
pub(super) struct GroupRename {
    pub(super) id: u64,
    pub(super) state: Entity<InputState>,
    _subscription: Subscription,
}

// ---- capture seed -----------------------------------------------------------------------

impl DiagramView {
    /// Capture-only (`TABLEX_SEED_DIAGRAM`, read by the scenario binary): a
    /// comma separated list. `annotated` adds a logical relationship, a note,
    /// colors and a group to the Chinook diagram; `suggestions`, `layers` and
    /// `inspector` open that panel; `select=<Table>` selects an entity once
    /// the diagram has loaded and `column=<Name>` a column of it in the Inspector; `draft` stages pending structure edits and
    /// `changes` opens the review dialog; `readonly` and `dialect=<postgres|mysql|
    /// mariadb|sqlite|clickhouse>` are read by AppRoot (the connection looks
    /// read-only / the review shows another dialect). (`legacy` is read by the fixture catalog.)
    pub(in crate::app) fn seed_for_scenario(&mut self, spec: &str, cx: &mut Context<Self>) {
        let tokens: Vec<&str> = spec.split(',').map(str::trim).collect();
        let key = |table: &str| TableKey { schema: Some("main".into()), table: table.into() };
        let end = |table: &str, column: &str| RelationEnd {
            schema: Some("main".into()),
            table: table.into(),
            columns: vec![column.into()],
        };
        if tokens.contains(&"annotated") {
            let mut annotations = Annotations::default();
            let _ = annotations.add_relation(end("Track", "AlbumId"), end("Album", "AlbumId"), true, false);
            let note = annotations.add_note(SEED_NOTE.0, SEED_NOTE.1);
            if let Some(note) = annotations.note_mut(note) {
                note.text = "Track.GenreId has no foreign key in production: check before relying on it.".into();
            }
            annotations.set_color(key("Track"), Some(SWATCHES[0].1));
            annotations.set_color(key("Invoice"), Some(SWATCHES[2].1));
            annotations.add_group("Catalog".into(), SWATCHES[1].1, vec![key("Album"), key("MediaType")]);
            self.annotations = Arc::new(annotations);
            self.rebuild_effective();
            self.refresh_derived();
        }
        self.panel = tokens.iter().find_map(|token| match *token {
            "suggestions" => Some(Panel::Suggestions),
            "layers" => Some(Panel::Layers),
            "inspector" => Some(Panel::Inspector),
            _ => None,
        });
        self.seed_select = tokens.iter().find_map(|token| token.strip_prefix("select=")).map(str::to_string);
        // D3: `draft` stages pending structure edits, `changes` opens the review dialog.
        self.seed_column = tokens.iter().find_map(|token| token.strip_prefix("column=")).map(str::to_string);
        self.seed_draft = tokens.contains(&"draft");
        self.seed_stale = tokens.contains(&"stale");
        self.seed_changes = tokens.contains(&"changes");
        cx.notify();
    }
}

/// Where the seeded note sits (diagram units: free space below the group).
const SEED_NOTE: (f32, f32) = (240.0, 690.0);

// ---- derived state --------------------------------------------------------------------

impl DiagramView {
    /// `snapshot.graph` = the catalog's graph plus the logical relationships
    /// that fit it. A selected logical relationship stays selected.
    pub(super) fn rebuild_effective(&mut self) {
        let keep = self.selected_logical_id();
        let (Some(declared), Some(snapshot)) = (self.declared.clone(), self.snapshot.as_mut()) else {
            return;
        };
        let base = if self.annotations.relations.is_empty() {
            declared
        } else {
            Arc::new(declared.with_logical(&self.annotations.relations))
        };
        // D3: the pending structure changes, applied.
        snapshot.graph = match self.edit.as_ref() {
            Some(edit) if !self.session.draft.is_empty() => {
                let (graph, states) = base.with_draft(&self.session.draft, &edit.scope);
                self.entity_states = states;
                Arc::new(graph)
            }
            _ => {
                self.entity_states.clear();
                base
            }
        };
        self.linked_key = None;
        self.selected_relation = keep.and_then(|id| self.logical_index(id));
    }

    fn selected_logical_id(&self) -> Option<u64> {
        let relation = self.graph()?.relations.get(self.selected_relation?)?;
        match relation.origin {
            RelationOrigin::Logical(id) => Some(id),
            RelationOrigin::Declared | RelationOrigin::Draft => None,
        }
    }

    /// Index into `graph.relations` of logical relationship `id`.
    pub(in crate::app) fn logical_index(&self, id: u64) -> Option<usize> {
        self.graph()?.relations.iter().position(|r| r.origin == RelationOrigin::Logical(id))
    }

    /// Everything computed from the graph and the annotations: group
    /// membership and visibility, entity colors, suggestions, the selection
    /// (without what vanished or is hidden now), routes and minimap boxes.
    pub(super) fn refresh_derived(&mut self) {
        let Some(graph) = self.snapshot.as_ref().map(|snapshot| snapshot.graph.clone()) else {
            return;
        };
        self.groups = GroupIndex::build(&graph, &self.annotations.groups);
        self.groups.hide(&graph, &self.annotations.removed);
        self.entity_colors = if self.annotations.colors.is_empty() {
            Vec::new()
        } else {
            graph.entities.iter().map(|e| self.annotations.colors.get(&TableKey::of(&e.table)).copied()).collect()
        };
        self.suggestions = Arc::new(infer::suggest(&graph, &self.annotations));
        let shown = |id: &EntityId| id.0 < graph.entities.len() && self.groups.shown(id.0);
        self.selection.retain(|id| shown(id));
        if !self.selected.is_some_and(|id| self.selection.contains(&id)) {
            self.selected = self.selection.last().copied();
        }
        if self.selected_note.is_some_and(|id| self.annotations.note(id).is_none()) {
            self.selected_note = None;
        }
        if self.selected_group.is_some_and(|id| self.annotations.group(id).is_none()) {
            self.selected_group = None;
        }
        if self.note_edit.as_ref().is_some_and(|edit| self.annotations.note(edit.id).is_none()) {
            self.note_edit = None;
        }
        if let Some(wanted) = self.seed_select.take() {
            match graph.entities.iter().position(|e| e.table.table == wanted) {
                Some(index) => self.select_only(EntityId(index)),
                None => self.seed_select = Some(wanted),
            }
        }
        self.refresh_geometry();
    }

    /// A different diagram: its annotations and history start empty.
    pub(super) fn reset_annotations(&mut self) {
        self.annotations = Arc::default();
        self.session.clear();
        self.entity_states.clear();
        self.editor = structure::Editor::default();
        self.note_edit = None;
        self.group_rename = None;
        self.rebuild_effective();
        self.refresh_derived();
    }

    /// Nothing selected: entities, note, group and relationship.
    pub(super) fn clear_selection_state(&mut self) {
        self.selected = None;
        self.selection.clear();
        self.selected_note = None;
        self.selected_group = None;
        self.selected_relation = None;
    }

    fn select_only(&mut self, id: EntityId) {
        self.clear_selection_state();
        self.selected = Some(id);
        self.selection.push(id);
    }

    /// Shift-click: adds `id` to the selection or takes it out.
    fn toggle_selected(&mut self, id: EntityId) {
        self.selected_note = None;
        self.selected_group = None;
        self.selected_relation = None;
        match self.selection.iter().position(|&s| s == id) {
            Some(at) => {
                self.selection.remove(at);
                if self.selected == Some(id) {
                    self.selected = self.selection.last().copied();
                }
            }
            None => {
                self.selection.push(id);
                self.selection.sort_unstable();
                self.selected = Some(id);
            }
        }
    }

    /// The whole drawing: shown entities, notes and group frames.
    pub(in crate::app) fn content_bounds(&self) -> DiagramRect {
        let boxes = self.layout.rects.iter().enumerate().filter(|(i, _)| self.groups.shown(*i)).map(|(_, r)| *r);
        let notes = self.annotations.notes.iter().map(Note::rect);
        let frames = (0..self.annotations.groups.len()).filter_map(|g| self.group_rect(g));
        boxes.chain(notes).chain(frames).reduce(|a, b| a.union(&b)).unwrap_or_default()
    }

    /// The frame of group `index` (into `annotations.groups`): its entities
    /// plus padding and a title bar, or the chip of a collapsed group. `None`
    /// for a hidden group.
    pub(in crate::app) fn group_rect(&self, index: usize) -> Option<DiagramRect> {
        let group = self.annotations.groups.get(index).filter(|g| !g.hidden)?;
        let union = self.layout.union_of(self.groups.members.get(index)?)?;
        Some(group_frame(union, group.collapsed))
    }

    /// The group whose title bar (the whole chip when collapsed) is at `at`.
    fn group_title_at(&self, at: DiagramPoint) -> Option<u64> {
        (0..self.annotations.groups.len()).rev().find_map(|index| {
            let group = &self.annotations.groups[index];
            let frame = self.group_rect(index)?;
            let title = if group.collapsed { frame } else { DiagramRect { height: GROUP_TITLE, ..frame } };
            title.contains(at).then_some(group.id)
        })
    }

    /// Where a link from `column` of `table` leaves the box.
    pub(super) fn row_anchor(&self, table: EntityId, column: usize) -> Option<DiagramPoint> {
        self.layout.row_anchor(self.graph()?, table, column, self.mode, &METRICS)
    }

    /// The column row under a window position, with its entity.
    pub(super) fn row_under(&self, position: Point<gpui::Pixels>) -> Option<(EntityId, usize)> {
        if self.viewport.zoom < OVERVIEW_ZOOM {
            return None;
        }
        let local = self.local(position);
        let id = self.entity_at(local)?;
        let row = self.layout.row_at(self.graph()?, id, self.viewport.to_diagram(local), self.mode, &METRICS)?;
        Some((id, row))
    }
}

// ---- edits and undo -------------------------------------------------------------------

impl DiagramView {
    pub(super) fn mark_dirty(&mut self, cx: &mut Context<Self>) {
        self.doc_dirty = true;
        self.schedule_save(cx);
        cx.notify();
    }

    /// Runs `change` on a copy of the annotations. When it changed anything
    /// the copy replaces them, the edit is recorded for undo, what depends on
    /// the annotations is rebuilt and the document is saved (debounced).
    pub(in crate::app) fn edit_annotations<R>(
        &mut self,
        cx: &mut Context<Self>,
        change: impl FnOnce(&mut Annotations) -> R,
    ) -> R {
        let before = self.annotations.clone();
        let mut next = (*before).clone();
        let result = change(&mut next);
        if next != *before {
            let after = Arc::new(next);
            self.session.history.push(Edit::Annotations { before, after: after.clone() });
            self.set_annotations(after, cx);
        }
        result
    }

    fn set_annotations(&mut self, annotations: Arc<Annotations>, cx: &mut Context<Self>) {
        let relations = annotations.relations != self.annotations.relations;
        self.annotations = annotations;
        if relations {
            // Boxes stay where they are: a new line must not reshuffle the
            // diagram under the pointer (the layout of a whole schema also
            // depends on its relations, but only when it is built).
            self.rebuild_effective();
        }
        self.refresh_derived();
        self.needs_fit |= self.auto_fit;
        self.mark_dirty(cx);
    }

    /// Records the move of every entity whose box is not at `before` any more
    /// (Re-layout); nothing when none moved.
    pub(super) fn record_moves(&mut self, before: &[DiagramRect]) {
        let Some(graph) = self.snapshot.as_ref().map(|snapshot| snapshot.graph.clone()) else {
            return;
        };
        let steps: Vec<MoveStep> = graph
            .entities
            .iter()
            .zip(before.iter().zip(&self.layout.rects))
            .filter(|(_, (from, to))| (from.x, from.y) != (to.x, to.y))
            .map(|(entity, (from, to))| MoveStep {
                table: TableKey::of(&entity.table),
                from: DiagramPoint { x: from.x, y: from.y },
                to: DiagramPoint { x: to.x, y: to.y },
            })
            .collect();
        if !steps.is_empty() {
            self.session.history.push(Edit::Move(steps));
        }
    }

    pub(in crate::app) fn undo(&mut self, cx: &mut Context<Self>) {
        let Some(edit) = self.session.undo() else {
            return;
        };
        self.apply_edit(&edit, true, cx);
    }

    pub(in crate::app) fn redo(&mut self, cx: &mut Context<Self>) {
        let Some(edit) = self.session.redo() else {
            return;
        };
        self.apply_edit(&edit, false, cx);
    }

    fn apply_edit(&mut self, edit: &Edit, undo: bool, cx: &mut Context<Self>) {
        match edit {
            Edit::Annotations { before, after } => {
                self.set_annotations(if undo { before.clone() } else { after.clone() }, cx);
            }
            Edit::Draft { before, after } => {
                let draft = self.session.restore(if undo { before } else { after });
                self.set_draft(draft, cx);
            }
            Edit::Move(steps) => {
                let Some(graph) = self.snapshot.as_ref().map(|snapshot| snapshot.graph.clone()) else {
                    return;
                };
                let index: HashMap<(Option<&str>, &str), usize> = graph
                    .entities
                    .iter()
                    .enumerate()
                    .map(|(i, e)| ((e.table.schema.as_deref(), e.table.table.as_str()), i))
                    .collect();
                self.pinned.resize(self.layout.rects.len(), false);
                for step in steps {
                    let Some(&i) = index.get(&(step.table.schema.as_deref(), step.table.table.as_str())) else {
                        continue;
                    };
                    let to = if undo { step.from } else { step.to };
                    if let Some(rect) = self.layout.rects.get_mut(i) {
                        (rect.x, rect.y) = (to.x, to.y);
                        self.pinned[i] = true;
                    }
                }
                self.moved = true;
                self.refresh_geometry();
                self.mark_dirty(cx);
            }
        }
    }
}

// ---- presses, hover, drags ------------------------------------------------------------

impl DiagramView {
    /// A press on a note: selects it and starts moving (or resizing, at the
    /// corner) it; a double-click edits its text. `false` when no note is there.
    pub(super) fn press_note(
        &mut self,
        at: DiagramPoint,
        event: &gpui::MouseDownEvent,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> bool {
        let Some((id, rect)) = self.annotations.notes.iter().rev().find(|n| n.rect().contains(at)).map(|n| (n.id, n.rect()))
        else {
            return false;
        };
        self.clear_selection_state();
        self.selected_note = Some(id);
        if event.click_count >= 2 {
            self.begin_note_edit(id, window, cx);
            // The tracked focus handle of the diagram would take the focus
            // from the editor on this very press.
            window.prevent_default();
            return true;
        }
        let handle = NOTE_HANDLE / self.viewport.zoom;
        let on_handle = at.x >= rect.right() - handle && at.y >= rect.bottom() - handle;
        self.drag_start = Some(DragStart { annotations: Some(self.annotations.clone()), ..DragStart::default() });
        self.drag = Some(if on_handle {
            Drag::NoteResize { id, last: event.position }
        } else {
            Drag::Note { id, last: event.position }
        });
        true
    }

    /// The row a link drag would start from at `at` on entity `id`.
    fn link_start(&self, id: EntityId, at: DiagramPoint, alt: bool) -> Option<usize> {
        if self.viewport.zoom < OVERVIEW_ZOOM {
            return None;
        }
        let rect = self.layout.rects.get(id.0)?;
        let row = self.layout.row_at(self.graph()?, id, at, self.mode, &METRICS)?;
        (alt || at.x >= rect.right() - LINK_BAND).then_some(row)
    }

    pub(super) fn press_entity(
        &mut self,
        id: EntityId,
        at: DiagramPoint,
        event: &gpui::MouseDownEvent,
        cx: &mut Context<Self>,
    ) {
        if event.click_count >= 2 {
            self.select_only(id);
            self.drag = None;
            if let Some(entity) = self.graph().and_then(|graph| graph.entities.get(id.0)) {
                if !entity.focus {
                    cx.emit(DiagramEvent::OpenTable(entity.table.clone()));
                }
            }
            return;
        }
        if let Some(column) = self.link_start(id, at, event.modifiers.alt) {
            self.select_only(id);
            self.hover_row = Some((id, column));
            self.drag = Some(Drag::Link { table: id, column, pointer: event.position });
            return;
        }
        let mut reduce_to = None;
        if event.modifiers.shift {
            self.toggle_selected(id);
        } else if self.selection.contains(&id) {
            reduce_to = (self.selection.len() > 1).then_some(id);
            self.selected = Some(id);
            self.selected_note = None;
            self.selected_group = None;
            self.selected_relation = None;
        } else {
            self.select_only(id);
        }
        if !self.selection.contains(&id) {
            self.drag = None;
            return;
        }
        let positions = self.selection.iter().filter_map(|s| Some((s.0, self.layout.rects.get(s.0).map(|r| DiagramPoint { x: r.x, y: r.y })?))).collect();
        self.drag_start = Some(DragStart { positions, reduce_to, ..DragStart::default() });
        self.drag = Some(Drag::Entity { id, last: event.position });
    }

    /// A press on empty canvas: a group's title bar starts moving the group;
    /// a relation line is selected; Shift starts a rubber band, a plain drag
    /// pans.
    pub(super) fn press_background(&mut self, at: DiagramPoint, event: &gpui::MouseDownEvent, cx: &mut Context<Self>) {
        let _ = cx;
        if let Some(id) = self.group_title_at(at) {
            if event.click_count >= 2 {
                self.toggle_group_collapsed(id, cx);
                return;
            }
            self.clear_selection_state();
            self.selected_group = Some(id);
            let members = self.group_members(id);
            let positions = members.iter().filter_map(|&i| Some((i, self.layout.rects.get(i).map(|r| DiagramPoint { x: r.x, y: r.y })?))).collect();
            self.drag_start = Some(DragStart { positions, ..DragStart::default() });
            self.drag = Some(Drag::Group { id, last: event.position });
            return;
        }
        let relation = hit_route(&self.routes, at, RELATION_PICK / self.viewport.zoom);
        let base = if event.modifiers.shift { self.selection.clone() } else { Vec::new() };
        if !event.modifiers.shift {
            self.clear_selection_state();
        }
        if let Some(relation) = relation {
            self.selected_relation = Some(relation);
        }
        if event.modifiers.shift {
            self.drag_start = Some(DragStart { base, ..DragStart::default() });
            self.drag = Some(Drag::Marquee { origin: at, to: at });
        } else {
            self.drag = Some(Drag::Pan { last: event.position });
        }
    }

    /// Pointer movement without a drag: hover highlight, link handle row and
    /// minimap opacity.
    pub(super) fn hover(&mut self, position: Point<gpui::Pixels>, cx: &mut Context<Self>) {
        let local = self.local(position);
        let hovered = self.entity_at(local);
        let row = self.row_under(position);
        let over_minimap = self.minimap_rect().is_some_and(|rect| rect.contains(local));
        if hovered != self.hovered || row != self.hover_row || over_minimap != self.minimap_hover {
            (self.hovered, self.hover_row, self.minimap_hover) = (hovered, row, over_minimap);
            cx.notify();
        }
    }

    /// Moves the selection (or just `id` when it is not selected).
    pub(super) fn move_selection(&mut self, id: EntityId, dx: f32, dy: f32) {
        let single = [id];
        let ids: &[EntityId] = if self.selection.contains(&id) { &self.selection } else { &single };
        self.pinned.resize(self.layout.rects.len(), false);
        for &moved in ids {
            self.layout.move_entity(moved, dx, dy);
            if let Some(pinned) = self.pinned.get_mut(moved.0) {
                *pinned = true;
            }
        }
        self.after_move();
    }

    pub(super) fn move_group(&mut self, id: u64, dx: f32, dy: f32) {
        self.pinned.resize(self.layout.rects.len(), false);
        for i in self.group_members(id) {
            self.layout.move_entity(EntityId(i), dx, dy);
            self.pinned[i] = true;
        }
        self.after_move();
    }

    fn after_move(&mut self) {
        self.moved = true;
        self.doc_dirty = true;
        self.auto_fit = false;
        if let Some(start) = &mut self.drag_start {
            start.reduce_to = None;
        }
        self.refresh_geometry();
    }

    /// Entity indices of group `id`.
    fn group_members(&self, id: u64) -> Vec<usize> {
        self.annotations
            .groups
            .iter()
            .position(|g| g.id == id)
            .and_then(|index| self.groups.members.get(index))
            .cloned()
            .unwrap_or_default()
    }

    pub(super) fn move_note(&mut self, id: u64, dx: f32, dy: f32) {
        if let Some(note) = Arc::make_mut(&mut self.annotations).note_mut(id) {
            note.x += dx;
            note.y += dy;
        }
        self.doc_dirty = true;
        self.auto_fit = false;
    }

    pub(super) fn resize_note(&mut self, id: u64, dx: f32, dy: f32) {
        if let Some(note) = Arc::make_mut(&mut self.annotations).note_mut(id) {
            let rect = note.rect();
            note.width = (rect.width + dx).max(Note::MIN_WIDTH);
            note.height = (rect.height + dy).max(Note::MIN_HEIGHT);
        }
        self.doc_dirty = true;
        self.auto_fit = false;
    }

    /// A move drag ended: one undo step for everything it moved.
    pub(super) fn finish_move(&mut self, cx: &mut Context<Self>) {
        let Some(start) = self.drag_start.take() else {
            return;
        };
        let Some(graph) = self.snapshot.as_ref().map(|snapshot| snapshot.graph.clone()) else {
            return;
        };
        let steps: Vec<MoveStep> = start
            .positions
            .iter()
            .filter_map(|&(i, from)| {
                let (entity, rect) = (graph.entities.get(i)?, self.layout.rects.get(i)?);
                ((rect.x, rect.y) != (from.x, from.y)).then(|| MoveStep {
                    table: TableKey::of(&entity.table),
                    from,
                    to: DiagramPoint { x: rect.x, y: rect.y },
                })
            })
            .collect();
        if steps.is_empty() {
            if let Some(id) = start.reduce_to {
                self.select_only(id);
            }
            return;
        }
        self.session.history.push(Edit::Move(steps));
        self.mark_dirty(cx);
    }

    /// A note drag ended: one undo step when it changed the note.
    pub(super) fn finish_annotation_drag(&mut self, cx: &mut Context<Self>) {
        let Some(before) = self.drag_start.take().and_then(|start| start.annotations) else {
            return;
        };
        if *before != *self.annotations {
            self.session.history.push(Edit::Annotations { before, after: self.annotations.clone() });
            self.mark_dirty(cx);
        }
    }

    /// A link drag ended at `pointer`: a logical relationship from the
    /// dragged column to the column under it, if there is one.
    pub(super) fn finish_link(&mut self, table: EntityId, column: usize, pointer: Point<gpui::Pixels>, cx: &mut Context<Self>) {
        let target = self.row_under(pointer).filter(|&(id, row)| (id, row) != (table, column));
        self.hover_row = None;
        if let Some((parent, parent_column)) = target {
            self.create_relation((table, column), (parent, parent_column), cx);
        }
    }

    /// Rubber band: the selection becomes what the band started from plus
    /// every shown entity it touches.
    pub(super) fn apply_marquee(&mut self, origin: DiagramPoint, to: DiagramPoint) {
        let band = DiagramRect {
            x: origin.x.min(to.x),
            y: origin.y.min(to.y),
            width: (origin.x - to.x).abs(),
            height: (origin.y - to.y).abs(),
        };
        let base = self.drag_start.as_ref().map(|start| start.base.clone()).unwrap_or_default();
        let mut selection = base;
        let touched: Vec<EntityId> = self
            .layout
            .rects
            .iter()
            .enumerate()
            .filter(|(i, r)| self.groups.shown(*i) && r.intersects(&band))
            .map(|(i, _)| EntityId(i))
            .collect();
        for id in &touched {
            if !selection.contains(id) {
                selection.push(*id);
            }
        }
        selection.sort_unstable();
        self.selected = touched.last().copied().or_else(|| selection.last().copied());
        self.selection = selection;
    }
}

// ---- logical relationships and suggestions ------------------------------------------

impl DiagramView {
    fn relation_end(&self, id: EntityId, column: usize) -> Option<RelationEnd> {
        let entity = self.graph()?.entities.get(id.0)?;
        Some(RelationEnd {
            schema: entity.table.schema.clone(),
            table: entity.table.table.clone(),
            columns: vec![entity.columns.get(column)?.name.clone()],
        })
    }

    /// Nullability and uniqueness of `column` as the catalog knows them: the
    /// defaults of a new logical relationship.
    fn column_cardinality(&self, id: EntityId, column: usize) -> (bool, bool) {
        let Some(entity) = self.graph().and_then(|graph| graph.entities.get(id.0)) else {
            return (true, false);
        };
        let optional = entity.columns.get(column).is_none_or(|c| c.nullable);
        let unique = entity.unique_keys.iter().any(|key| key.as_slice() == [column]);
        (optional, unique)
    }

    /// Declares `child.column` a reference to `parent.column` (a logical
    /// relationship: stored in the diagram file, never in the database),
    /// selects it and says what happened.
    pub(in crate::app) fn create_relation(
        &mut self,
        (child, child_column): (EntityId, usize),
        (parent, parent_column): (EntityId, usize),
        cx: &mut Context<Self>,
    ) {
        let (Some(from), Some(to)) = (self.relation_end(child, child_column), self.relation_end(parent, parent_column))
        else {
            return;
        };
        let declared = self.graph().is_some_and(|graph| {
            graph.relations.iter().any(|r| {
                r.child == child
                    && r.parent == parent
                    && r.child_columns == [child_column]
                    && r.parent_columns == [parent_column]
            })
        });
        let (optional, unique) = self.column_cardinality(child, child_column);
        let existing = self.annotations.relations.iter().find(|r| r.child == from && r.parent == to).map(|r| r.id);
        let added = if declared {
            Err(AnnotationError::DuplicateRelation)
        } else {
            self.edit_annotations(cx, |a| a.add_relation(from, to, optional, unique))
        };
        self.clear_selection_state();
        self.notice = Some(match added {
            Ok(id) => {
                self.selected_relation = self.logical_index(id);
                ("Added a logical relationship (kept in the diagram, not in the database)".to_string(), false)
            }
            Err(error) => {
                // Show the one that is there already.
                self.selected_relation = existing.and_then(|id| self.logical_index(id));
                (error.to_string(), true)
            }
        });
        cx.notify();
    }

    /// Edits the cardinality of logical relationship `id`.
    pub(in crate::app) fn set_relation_cardinality(&mut self, id: u64, optional: bool, unique: bool, cx: &mut Context<Self>) {
        self.edit_annotations(cx, |a| a.set_cardinality(id, optional, unique));
    }

    pub(in crate::app) fn delete_relation(&mut self, id: u64, cx: &mut Context<Self>) {
        self.edit_annotations(cx, |a| a.remove_relation(id));
        self.selected_relation = None;
    }

    /// Suggestion `index` becomes a logical relationship.
    pub(in crate::app) fn confirm_suggestion(&mut self, index: usize, cx: &mut Context<Self>) {
        let Some(suggestion) = self.suggestions.get(index).copied() else {
            return;
        };
        let ends = (
            self.relation_end(suggestion.child, suggestion.child_column),
            self.relation_end(suggestion.parent, suggestion.parent_column),
        );
        let (optional, unique) = self.column_cardinality(suggestion.child, suggestion.child_column);
        if let (Some(from), Some(to)) = ends {
            let _ = self.edit_annotations(cx, |a| a.add_relation(from, to, optional, unique));
        }
    }

    /// Every suggestion becomes a logical relationship (one undo step).
    pub(in crate::app) fn confirm_all_suggestions(&mut self, cx: &mut Context<Self>) {
        let suggestions = self.suggestions.clone();
        let adds: Vec<(RelationEnd, RelationEnd, bool, bool)> = suggestions
            .iter()
            .filter_map(|s| {
                let (optional, unique) = self.column_cardinality(s.child, s.child_column);
                Some((self.relation_end(s.child, s.child_column)?, self.relation_end(s.parent, s.parent_column)?, optional, unique))
            })
            .collect();
        self.edit_annotations(cx, |a| {
            for (from, to, optional, unique) in adds {
                let _ = a.add_relation(from, to, optional, unique);
            }
        });
    }

    pub(in crate::app) fn ignore_suggestion(&mut self, index: usize, cx: &mut Context<Self>) {
        let Some(link) = self.graph().zip(self.suggestions.get(index)).map(|(graph, s)| s.link(graph)) else {
            return;
        };
        self.edit_annotations(cx, |a| a.ignore(link));
    }

    pub(in crate::app) fn ignore_all_suggestions(&mut self, cx: &mut Context<Self>) {
        let (Some(graph), suggestions) = (self.graph(), self.suggestions.clone()) else {
            return;
        };
        let links: Vec<_> = suggestions.iter().map(|s| s.link(graph)).collect();
        self.edit_annotations(cx, |a| {
            for link in links {
                a.ignore(link);
            }
        });
    }

    /// Brings every dismissed suggestion back.
    pub(in crate::app) fn restore_ignored_suggestions(&mut self, cx: &mut Context<Self>) {
        self.edit_annotations(cx, |a| a.ignored.clear());
    }
}

// ---- notes ------------------------------------------------------------------------------

impl DiagramView {
    /// Adds a note at `at` and starts editing its text.
    pub(in crate::app) fn add_note(&mut self, at: DiagramPoint, window: &mut Window, cx: &mut Context<Self>) {
        let id = self.edit_annotations(cx, |a| a.add_note(at.x, at.y));
        self.clear_selection_state();
        self.selected_note = Some(id);
        self.begin_note_edit(id, window, cx);
    }

    /// Opens the inline editor of note `id`; leaving it (focus lost, Escape)
    /// stores the text as one undo step.
    pub(in crate::app) fn begin_note_edit(&mut self, id: u64, window: &mut Window, cx: &mut Context<Self>) {
        self.commit_note_edit(cx);
        let Some(text) = self.annotations.note(id).map(|note| note.text.clone()) else {
            return;
        };
        let state = cx.new(|cx| TextareaState::new(window, cx).placeholder("Note").default_value(text));
        let subscription = cx.subscribe_in(&state, window, |this, _, event: &InputEvent, _, cx| {
            if matches!(event, InputEvent::Blur) {
                this.commit_note_edit(cx);
            }
        });
        state.update(cx, |state, cx| state.focus(window, cx));
        self.note_edit = Some(NoteEdit { id, state, _subscription: subscription });
        cx.notify();
    }

    /// Stores the text being edited (if it changed) and closes the editor.
    pub(in crate::app) fn commit_note_edit(&mut self, cx: &mut Context<Self>) {
        let Some(edit) = self.note_edit.take() else {
            return;
        };
        let text = edit.state.read(cx).value().to_string();
        if self.annotations.note(edit.id).is_some_and(|note| note.text != text) {
            self.edit_annotations(cx, |a| {
                if let Some(note) = a.note_mut(edit.id) {
                    note.text = text;
                }
            });
        }
        cx.notify();
    }

    pub(in crate::app) fn delete_note(&mut self, id: u64, cx: &mut Context<Self>) {
        self.note_edit = None;
        self.edit_annotations(cx, |a| a.remove_note(id));
        self.selected_note = None;
    }

    /// The text in the open note editor (tests).
    #[cfg(test)]
    pub(in crate::app) fn note_edit_text(&self, cx: &gpui::App) -> Option<String> {
        self.note_edit.as_ref().map(|edit| edit.state.read(cx).value().to_string())
    }

    /// Whether a note's text is being edited.
    pub(in crate::app) fn note_edit_open(&self) -> bool {
        self.note_edit.is_some()
    }

    /// Escape in the note editor: keeps the text, gives the focus back.
    pub(super) fn end_note_edit(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.commit_note_edit(cx);
        window.focus(&self.focus_handle, cx);
    }
}

// ---- colors and groups -------------------------------------------------------------------

impl DiagramView {
    fn key_of(&self, id: EntityId) -> Option<TableKey> {
        self.graph()?.entities.get(id.0).map(|entity| TableKey::of(&entity.table))
    }

    /// Sets (or with `None` resets) the header color of `entities`.
    pub(in crate::app) fn set_entity_color(&mut self, entities: &[EntityId], color: Option<Rgb>, cx: &mut Context<Self>) {
        let keys: Vec<TableKey> = entities.iter().filter_map(|&id| self.key_of(id)).collect();
        self.edit_annotations(cx, |a| {
            for key in keys {
                a.set_color(key, color);
            }
        });
    }

    /// The header color of entity `id`.
    pub(in crate::app) fn entity_color(&self, id: EntityId) -> Option<Rgb> {
        self.entity_colors.get(id.0).copied().flatten()
    }

    /// A new group of `entities`.
    pub(in crate::app) fn group_entities(&mut self, entities: &[EntityId], cx: &mut Context<Self>) -> Option<u64> {
        let keys: Vec<TableKey> = entities.iter().filter_map(|&id| self.key_of(id)).collect();
        if keys.is_empty() {
            return None;
        }
        let color = SWATCHES[self.annotations.groups.len() % SWATCHES.len()].1;
        let name = format!("Group {}", self.annotations.groups.len() + 1);
        let id = self.edit_annotations(cx, |a| a.add_group(name, color, keys));
        self.clear_selection_state();
        self.selected_group = Some(id);
        Some(id)
    }

    pub(in crate::app) fn rename_group(&mut self, id: u64, name: String, cx: &mut Context<Self>) {
        let name = name.trim().to_string();
        if name.is_empty() {
            return;
        }
        self.edit_annotations(cx, |a| {
            if let Some(group) = a.group_mut(id) {
                group.name = name;
            }
        });
    }

    pub(in crate::app) fn set_group_color(&mut self, id: u64, color: Rgb, cx: &mut Context<Self>) {
        self.edit_annotations(cx, |a| {
            if let Some(group) = a.group_mut(id) {
                group.color = color.hex();
            }
        });
    }

    pub(in crate::app) fn toggle_group_hidden(&mut self, id: u64, cx: &mut Context<Self>) {
        self.edit_annotations(cx, |a| {
            if let Some(group) = a.group_mut(id) {
                group.hidden = !group.hidden;
            }
        });
    }

    pub(in crate::app) fn toggle_group_collapsed(&mut self, id: u64, cx: &mut Context<Self>) {
        self.edit_annotations(cx, |a| {
            if let Some(group) = a.group_mut(id) {
                group.collapsed = !group.collapsed;
            }
        });
    }

    /// Dissolves group `id`; its entities stay where they are.
    pub(in crate::app) fn ungroup(&mut self, id: u64, cx: &mut Context<Self>) {
        self.edit_annotations(cx, |a| a.remove_group(id));
        self.selected_group = None;
    }

    pub(in crate::app) fn leave_group(&mut self, entity: EntityId, cx: &mut Context<Self>) {
        if let Some(key) = self.key_of(entity) {
            self.edit_annotations(cx, |a| a.leave_group(&key));
        }
    }

    /// Starts renaming group `id` in the Layers panel.
    pub(in crate::app) fn begin_group_rename(&mut self, id: u64, window: &mut Window, cx: &mut Context<Self>) {
        self.commit_group_rename(cx);
        let Some(name) = self.annotations.group(id).map(|group| group.name.clone()) else {
            return;
        };
        let state = cx.new(|cx| InputState::new(window, cx).default_value(name));
        let subscription = cx.subscribe_in(&state, window, |this, _, event: &InputEvent, _, cx| {
            if matches!(event, InputEvent::PressEnter { .. } | InputEvent::Blur) {
                this.commit_group_rename(cx);
            }
        });
        state.update(cx, |state, cx| {
            state.focus(window, cx);
            state.select_all(window, cx);
        });
        self.group_rename = Some(GroupRename { id, state, _subscription: subscription });
        cx.notify();
    }

    pub(in crate::app) fn commit_group_rename(&mut self, cx: &mut Context<Self>) {
        let Some(rename) = self.group_rename.take() else {
            return;
        };
        let name = rename.state.read(cx).value().to_string();
        self.rename_group(rename.id, name, cx);
        cx.notify();
    }
}

// ---- context menu -------------------------------------------------------------------------

impl DiagramView {
    /// What a right click at `position` landed on. A right click on an entity
    /// selects it (unless it is part of the selection already, which a menu
    /// action then applies to as a whole).
    pub(in crate::app) fn context_target(&mut self, position: Point<gpui::Pixels>, cx: &mut Context<Self>) -> ContextTarget {
        let local = self.local(position);
        let at = self.viewport.to_diagram(local);
        self.context_at = at;
        if self.minimap_rect().is_some_and(|rect| rect.contains(local)) {
            return ContextTarget::Canvas;
        }
        if let Some(note) = self.annotations.notes.iter().rev().find(|n| n.rect().contains(at)).map(|n| n.id) {
            self.clear_selection_state();
            self.selected_note = Some(note);
            cx.notify();
            return ContextTarget::Note(note);
        }
        if let Some(id) = self.entity_at(local) {
            if !self.selection.contains(&id) {
                self.select_only(id);
            } else {
                self.selected = Some(id);
            }
            cx.notify();
            return ContextTarget::Entity(id);
        }
        if let Some(group) = self.group_title_at(at) {
            self.clear_selection_state();
            self.selected_group = Some(group);
            cx.notify();
            return ContextTarget::Group(group);
        }
        match hit_route(&self.routes, at, RELATION_PICK / self.viewport.zoom) {
            Some(relation) => {
                self.clear_selection_state();
                self.selected_relation = Some(relation);
                cx.notify();
                ContextTarget::Relation(relation)
            }
            None => ContextTarget::Canvas,
        }
    }

    /// The entities a menu action on `id` applies to: the selection when `id`
    /// is part of it, else `id`.
    fn action_entities(&self, id: EntityId) -> Vec<EntityId> {
        if self.selection.contains(&id) {
            self.selection.clone()
        } else {
            vec![id]
        }
    }

    /// The menu of a right click on `target`.
    pub(super) fn build_context_menu(
        view: &Entity<Self>,
        target: ContextTarget,
        menu: PopupMenu,
        window: &mut Window,
        cx: &mut Context<PopupMenu>,
    ) -> PopupMenu {
        let weak: WeakEntity<Self> = view.downgrade();
        let join = match target {
            ContextTarget::Entity(id) => Some(JoinTarget::Entity(id)),
            ContextTarget::Relation(index) => Some(JoinTarget::Relation(index)),
            _ => None,
        }
        .filter(|join| view.read(cx).join_sql(*join).is_some());
        let mut menu = menu;
        if let Some(join) = join {
            let weak = weak.clone();
            menu = menu.item(PopupMenuItem::new("Copy JOIN SQL").on_click(move |_, _, app| {
                let _ = weak.update(app, |view, cx| view.copy_join_sql(join, cx));
            }));
        }
        match target {
            ContextTarget::Entity(id) => Self::entity_menu(view, id, menu, window, cx),
            ContextTarget::Relation(index) => Self::relation_menu(view, index, menu, window, cx),
            ContextTarget::Note(id) => {
                let (edit, delete) = (weak.clone(), weak.clone());
                menu.item(PopupMenuItem::new("Edit note").on_click(move |_, window, app| {
                    let _ = edit.update(app, |view, cx| view.begin_note_edit(id, window, cx));
                }))
                .item(PopupMenuItem::new("Delete note").on_click(move |_, _, app| {
                    let _ = delete.update(app, |view, cx| view.delete_note(id, cx));
                }))
            }
            ContextTarget::Group(id) => Self::group_menu(view, id, menu, window, cx),
            ContextTarget::Canvas => {
                let (at, structure, editable, removed) = {
                    let view = view.read(cx);
                    (view.context_at, view.supports_structure(), view.editable(), view.annotations.removed.len())
                };
                let create = weak.clone();
                let restore = weak.clone();
                let mut menu = menu.item(PopupMenuItem::new("Add note").on_click(move |_, window, app| {
                    let _ = weak.update(app, |view, cx| view.add_note(at, window, cx));
                }));
                if structure {
                    menu = menu.item(PopupMenuItem::new("New table").disabled(!editable).on_click(move |_, window, app| {
                        let _ = create.update(app, |view, cx| view.create_table_at(at, window, cx));
                    }));
                }
                if removed > 0 {
                    menu = menu.item(
                        PopupMenuItem::new(format!("Show {removed} removed {}", if removed == 1 { "table" } else { "tables" }))
                            .on_click(move |_, _, app| {
                                let _ = restore.update(app, |view, cx| view.restore_removed(cx));
                            }),
                    );
                }
                menu
            }
        }
    }

    fn entity_menu(view: &Entity<Self>, id: EntityId, menu: PopupMenu, window: &mut Window, cx: &mut Context<PopupMenu>) -> PopupMenu {
        let weak = view.downgrade();
        let (current, grouped, selected) = {
            let view = view.read(cx);
            (view.entity_color(id), view.key_of(id).is_some_and(|key| view.annotations.group_of(&key).is_some()), view.action_entities(id).len())
        };
        let menu = menu.separator().submenu("Color", window, cx, {
            let weak = weak.clone();
            move |menu, _, _| {
                let menu = SWATCHES.iter().fold(menu, |menu, &(name, color)| {
                    let weak = weak.clone();
                    menu.item(swatch_item(name, color).checked(current == Some(color)).on_click(move |_, _, app| {
                        let _ = weak.update(app, |view, cx| {
                            let targets = view.action_entities(id);
                            view.set_entity_color(&targets, Some(color), cx)
                        });
                    }))
                });
                let weak = weak.clone();
                menu.separator().item(PopupMenuItem::new("Reset color").disabled(current.is_none()).on_click(move |_, _, app| {
                    let _ = weak.update(app, |view, cx| {
                        let targets = view.action_entities(id);
                        view.set_entity_color(&targets, None, cx)
                    });
                }))
            }
        });
        let group_weak = weak.clone();
        let menu = menu.item(
            PopupMenuItem::new(if selected > 1 { format!("Group {selected} selected tables") } else { "Group this table".to_string() })
                .on_click(move |_, _, app| {
                    let _ = group_weak.update(app, |view, cx| {
                        let targets = view.action_entities(id);
                        view.group_entities(&targets, cx);
                    });
                }),
        );
        let menu = if grouped {
            let weak = weak.clone();
            menu.item(PopupMenuItem::new("Remove from group").on_click(move |_, _, app| {
                let _ = weak.update(app, |view, cx| view.leave_group(id, cx));
            }))
        } else {
            menu
        };
        let (at, structure, editable, focus, dropped) = {
            let view = view.read(cx);
            (
                view.context_at,
                view.supports_structure(),
                view.editable(),
                view.graph().and_then(|g| g.entities.get(id.0)).is_some_and(|e| e.focus),
                view.table_state(id.0) == crate::app::results::diagram::overlay::TableState::Dropped,
            )
        };
        let (remove, drop_table) = (weak.clone(), weak.clone());
        let mut menu = menu
            .separator()
            .item(PopupMenuItem::new("Add note here").on_click(move |_, window, app| {
                let _ = weak.update(app, |view, cx| view.add_note(at, window, cx));
            }))
            .separator()
            .item(PopupMenuItem::new("Remove from diagram").disabled(focus).on_click(move |_, _, app| {
                let _ = remove.update(app, |view, cx| {
                    let targets = view.action_entities(id);
                    view.remove_from_diagram(&targets, cx)
                });
            }));
        if structure {
            menu = menu.item(PopupMenuItem::new("Drop table...").disabled(!editable || dropped).on_click(move |_, window, app| {
                let _ = drop_table.update(app, |view, cx| view.request_drop_table(id.0, window, cx));
            }));
        }
        menu
    }

    fn relation_menu(view: &Entity<Self>, index: usize, menu: PopupMenu, window: &mut Window, cx: &mut Context<PopupMenu>) -> PopupMenu {
        let weak = view.downgrade();
        let logical = {
            let view = view.read(cx);
            view.graph().and_then(|graph| graph.relations.get(index)).and_then(|relation| match relation.origin {
                RelationOrigin::Logical(id) => Some((id, relation.optional, relation.unique)),
                RelationOrigin::Declared | RelationOrigin::Draft => None,
            })
        };
        let Some((id, optional, unique)) = logical else {
            return menu;
        };
        let menu = menu.separator().submenu("Cardinality", window, cx, {
            let weak = weak.clone();
            move |menu, _, _| {
                [(false, false), (true, false), (false, true), (true, true)].into_iter().fold(menu, |menu, (opt, uniq)| {
                    let weak = weak.clone();
                    menu.item(PopupMenuItem::new(cardinality_label(opt, uniq)).checked((opt, uniq) == (optional, unique)).on_click(
                        move |_, _, app| {
                            let _ = weak.update(app, |view, cx| view.set_relation_cardinality(id, opt, uniq, cx));
                        },
                    ))
                })
            }
        });
        menu.item(PopupMenuItem::new("Delete logical relationship").on_click(move |_, _, app| {
            let _ = weak.update(app, |view, cx| view.delete_relation(id, cx));
        }))
    }

    fn group_menu(view: &Entity<Self>, id: u64, menu: PopupMenu, window: &mut Window, cx: &mut Context<PopupMenu>) -> PopupMenu {
        let weak = view.downgrade();
        let current = view.read(cx).annotations.group(id).map(Group::rgb);
        let (rename, hide, collapse, ungroup) = (weak.clone(), weak.clone(), weak.clone(), weak.clone());
        menu.item(PopupMenuItem::new("Rename").on_click(move |_, window, app| {
            let _ = rename.update(app, |view, cx| {
                view.panel = Some(Panel::Layers);
                view.begin_group_rename(id, window, cx);
            });
        }))
        .submenu("Color", window, cx, {
            let weak = weak.clone();
            move |menu, _, _| {
                SWATCHES.iter().fold(menu, |menu, &(name, color)| {
                    let weak = weak.clone();
                    menu.item(swatch_item(name, color).checked(current == Some(color)).on_click(move |_, _, app| {
                        let _ = weak.update(app, |view, cx| view.set_group_color(id, color, cx));
                    }))
                })
            }
        })
        .item(PopupMenuItem::new("Hide group").on_click(move |_, _, app| {
            let _ = hide.update(app, |view, cx| view.toggle_group_hidden(id, cx));
        }))
        .item(PopupMenuItem::new("Collapse / expand").on_click(move |_, _, app| {
            let _ = collapse.update(app, |view, cx| view.toggle_group_collapsed(id, cx));
        }))
        .separator()
        .item(PopupMenuItem::new("Ungroup").on_click(move |_, _, app| {
            let _ = ungroup.update(app, |view, cx| view.ungroup(id, cx));
        }))
    }
}

/// A color menu entry: a dot in the swatch's color and its name.
fn swatch_item(name: &'static str, color: Rgb) -> PopupMenuItem {
    let (r, g, b) = color.channels();
    let dot = gpui::rgb(u32::from(r) << 16 | u32::from(g) << 8 | u32::from(b));
    PopupMenuItem::element(move |_, _| {
        div().flex().items_center().gap(px(8.0)).child(div().size(px(10.0)).rounded_full().bg(dot)).child(name)
    })
}

// ---- drawing --------------------------------------------------------------------------------

/// `color` with the given alpha.
fn tinted(color: Rgb, alpha: f32) -> Rgba {
    let (r, g, b) = color.channels();
    Rgba { r: f32::from(r) / 255.0, g: f32::from(g) / 255.0, b: f32::from(b) / 255.0, a: alpha }
}

/// Fill, border and text of a sticky note: yellow paper in both themes.
fn note_palette() -> (Rgba, Rgba, Rgba) {
    if theme::mode().is_dark() {
        (gpui::rgb(0x4a4012), gpui::rgb(0x8c7a26), gpui::rgb(0xf3e9c0))
    } else {
        (gpui::rgb(0xfff5b8), gpui::rgb(0xe0c34e), gpui::rgb(0x3b3200))
    }
}

impl DiagramView {
    /// The group frames (behind the lines): a rounded translucent region with
    /// its title bar, or the chip of a collapsed group. Only those in view.
    pub(super) fn group_elements(&self, width: f32, height: f32) -> Vec<AnyElement> {
        let z = self.viewport.zoom;
        let visible = self.viewport.visible_rect(width, height);
        let mut elements = Vec::new();
        for (index, group) in self.annotations.groups.iter().enumerate() {
            let Some(frame) = self.group_rect(index).filter(|frame| frame.intersects(&visible)) else {
                continue;
            };
            let color = group.rgb();
            let selected = self.selected_group == Some(group.id);
            let members = self.groups.members.get(index).map_or(0, Vec::len);
            let id = group.id;
            let title = div()
                .h(px(if group.collapsed { frame.height } else { GROUP_TITLE } * z))
                .w_full()
                .flex()
                .items_center()
                .gap(px(6.0 * z))
                .px(px(10.0 * z))
                .child(div().size(px(8.0 * z)).rounded_full().bg(tinted(color, 1.0)).flex_shrink_0())
                .child(
                    div()
                        .flex_1()
                        .min_w_0()
                        .truncate()
                        .text_size(px(11.5 * z))
                        .font_weight(gpui::FontWeight::SEMIBOLD)
                        .text_color(theme::text())
                        .child(group.name.clone()),
                )
                .child(
                    div()
                        .flex_shrink_0()
                        .text_size(px(10.0 * z))
                        .text_color(theme::text_faint())
                        .child(if group.collapsed { format!("{members} tables (collapsed)") } else { members.to_string() }),
                );
            elements.push(
                div()
                    .debug_selector(move || format!("diagram-group-{id}"))
                    .absolute()
                    .left(px((frame.x - self.viewport.origin.x) * z))
                    .top(px((frame.y - self.viewport.origin.y) * z))
                    .w(px(frame.width * z))
                    .h(px(frame.height * z))
                    .rounded(px(10.0 * z))
                    .bg(tinted(color, if group.collapsed { 0.22 } else { 0.08 }))
                    .border_1()
                    .border_color(if selected { theme::accent() } else { tinted(color, 0.6) })
                    .child(title)
                    .into_any_element(),
            );
        }
        elements
    }

    /// The sticky notes in view, above the entities.
    pub(super) fn note_elements(&self, width: f32, height: f32, cx: &mut Context<Self>) -> Vec<AnyElement> {
        let z = self.viewport.zoom;
        let visible = self.viewport.visible_rect(width, height);
        let mut elements = Vec::new();
        for note in &self.annotations.notes {
            let rect = note.rect();
            if !rect.intersects(&visible) {
                continue;
            }
            let id = note.id;
            let selected = self.selected_note == Some(id);
            let editor = self.note_edit.as_ref().filter(|edit| edit.id == id);
            let body: AnyElement = match editor {
                Some(edit) => div()
                    .size_full()
                    // The area must not take the press for a click on the canvas.
                    .on_mouse_down(MouseButton::Left, |_, _, cx| cx.stop_propagation())
                    .on_action(cx.listener(|this, _: &Escape, window, cx| this.end_note_edit(window, cx)))
                    .child(gpui_component::input::Textarea::new(&edit.state).appearance(false).bordered(false).h_full())
                    .into_any_element(),
                None if note.text.is_empty() => {
                    div().text_color(theme::text_faint()).child("Double-click to edit").into_any_element()
                }
                None => div().child(note.text.clone()).into_any_element(),
            };
            elements.push(
                div()
                    .debug_selector(move || format!("diagram-note-{id}"))
                    .absolute()
                    .left(px((rect.x - self.viewport.origin.x) * z))
                    .top(px((rect.y - self.viewport.origin.y) * z))
                    .w(px(rect.width * z))
                    .h(px(rect.height * z))
                    .overflow_hidden()
                    .rounded(px(3.0 * z))
                    .bg(note_palette().0)
                    .border_1()
                    .border_color(if selected { theme::accent() } else { note_palette().1 })
                    .px(px(8.0 * z))
                    .py(px(6.0 * z))
                    .text_size(px(12.0 * z))
                    .text_color(note_palette().2)
                    .child(body)
                    .child(
                        div()
                            .absolute()
                            .right(px(3.0 * z))
                            .bottom(px(3.0 * z))
                            .size(px(9.0 * z))
                            .border_r_2()
                            .border_b_2()
                            .border_color(theme::text_faint()),
                    )
                    .into_any_element(),
            );
        }
        elements
    }

    /// The rubber band while dragging.
    pub(super) fn marquee_element(&self) -> Option<AnyElement> {
        let Some(Drag::Marquee { origin, to }) = self.drag else {
            return None;
        };
        let (a, b) = (self.viewport.to_screen(origin), self.viewport.to_screen(to));
        Some(
            div()
                .debug_selector(|| "diagram-marquee".into())
                .absolute()
                .left(px(a.x.min(b.x)))
                .top(px(a.y.min(b.y)))
                .w(px((a.x - b.x).abs()))
                .h(px((a.y - b.y).abs()))
                .bg(Rgba { a: 0.12, ..theme::accent() })
                .border_1()
                .border_color(theme::accent())
                .into_any_element(),
        )
    }

    /// The small editor next to a selected logical relationship: cardinality,
    /// optional, delete.
    pub(super) fn relation_bar(&self, cx: &mut Context<Self>) -> Option<AnyElement> {
        let index = self.selected_relation?;
        let relation = self.graph()?.relations.get(index)?;
        let RelationOrigin::Logical(id) = relation.origin else {
            return None;
        };
        let route = self.routes.iter().find(|route| route.relation == index)?;
        let (a, b) = (*route.points.get(1)?, *route.points.get(2)?);
        let at = self.viewport.to_screen(DiagramPoint { x: a.x, y: (a.y + b.y) / 2.0 });
        let area = self.area.get().size;
        let (width, height) = (f32::from(area.width), f32::from(area.height));
        let (optional, unique) = (relation.optional, relation.unique);
        let (left, top) = ((at.x + 10.0).min(width - 380.0).max(4.0), (at.y - 14.0).clamp(4.0, (height - 36.0).max(4.0)));
        let button = |key: &'static str, label: &'static str| {
            Button::new(key).label(label).with_size(Size::Small).compact().ghost().debug_selector(move || key.into())
        };
        Some(
            div()
                .debug_selector(|| "diagram-relation-bar".into())
                .absolute()
                .left(px(left))
                .top(px(top))
                .flex()
                .items_center()
                .gap(px(2.0))
                .px(px(6.0))
                .py(px(3.0))
                .rounded(px(6.0))
                .bg(theme::chrome())
                .border_1()
                .border_color(theme::border_strong())
                // Buttons here are not a click on the canvas.
                .on_mouse_down(MouseButton::Left, |_, _, cx| cx.stop_propagation())
                .child(div().mr(px(4.0)).text_size(px(11.0)).text_color(theme::text_muted()).child("Logical"))
                .child(
                    button("relation-many", "Many to one")
                        .selected(!unique)
                        .on_click(cx.listener(move |this, _, _, cx| this.set_relation_cardinality(id, optional, false, cx))),
                )
                .child(
                    button("relation-one", "One to one")
                        .selected(unique)
                        .on_click(cx.listener(move |this, _, _, cx| this.set_relation_cardinality(id, optional, true, cx))),
                )
                .child(
                    button("relation-optional", "Optional")
                        .selected(optional)
                        .on_click(cx.listener(move |this, _, _, cx| this.set_relation_cardinality(id, !optional, unique, cx))),
                )
                .child(
                    Button::new("relation-delete")
                        .icon(Icon::empty().path("icons/trash.svg").size(px(13.0)))
                        .with_size(Size::Small)
                        .compact()
                        .ghost()
                        .tooltip("Delete this logical relationship")
                        .debug_selector(|| "relation-delete".into())
                        .on_click(cx.listener(move |this, _, _, cx| this.delete_relation(id, cx))),
                )
                .into_any_element(),
        )
    }
}

/// Test helpers: the state the interaction tests assert on.
#[cfg(test)]
impl DiagramView {
    pub(in crate::app) fn group(&self, id: u64) -> Option<&Group> {
        self.annotations.group(id)
    }
}
