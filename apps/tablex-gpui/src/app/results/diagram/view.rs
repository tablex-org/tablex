//! GPUI rendering + interactions of the diagram presentation. Owner: DG.
//!
//! One `DiagramView` entity per SQL table tab: it owns the layout, the
//! viewport (pan/zoom), the selection and every drag, so moving an entity or
//! panning re-renders only this view, never the whole `AppRoot`. Entities are
//! absolutely positioned boxes (<= 41); all relation lines and their markers
//! are painted by ONE `canvas()`, the minimap by one more small `canvas()`.
//! Hit-testing (select, drag, hover, double-click, minimap, relation picking)
//! happens once on the area through `DiagramLayout::hit_test`: there are no
//! per-entity or per-column listeners.
//!
//! Saved state: the view loads the diagram's saved document (the entity
//! positions the user chose and the D2 annotations) off the UI thread when it
//! learns its identity (`set_persistence`), re-applies the positions whenever
//! the layout is rebuilt, and saves the document (debounced, off the UI
//! thread) after every edit. Re-layout forgets the positions, not the
//! annotations.
//!
//! D2 (annotate): `annotate` holds the editing (logical relationships, notes,
//! colors, groups, multi-select, undo / redo) and `panels` the side dock
//! (suggestions, layers, inspector). Groups are elements behind the lines
//! canvas, notes elements above the entities; neither adds a listener per item.

use super::super::super::*;
use super::data::DiagramSnapshot;
use super::annotations::{Annotations, GroupIndex, Rgb, TableKey};
use super::export::{ExportFormat, ExportRequest};
use super::history::Edit;
use super::infer::Suggestion;
use super::join_sql;
use super::layout::{
    hit_route, AttributeMode, DiagramLayout, DiagramPoint, DiagramRect, Metrics, Minimap, RoutedRelation, Viewport,
};
use super::model::{
    contains_ci, ColumnState, DiagramColumn, DiagramEntity, DiagramGraph, EntityId, EntityState, RelationOrigin,
    TableRef, TypeGlyph,
};
use super::overlay::TableState;
use crate::app::structure_edit::{ApplyRequest, EditContext, EditSession};
use super::notation::{route_directions, EndMark, RelationNotation};
use super::store::{DiagramKey, LayoutStore, SavedDiagram, StoreError};
use crate::app::actions::{ClearDiagramSelection, DiagramRedo, DiagramUndo, FocusDiagramSearch};
use crate::columns_panel::Presentation;
use crate::error_log::Area;
use crate::sql_table::SqlDialect;
use gpui::{
    canvas, point, size, Bounds, EventEmitter, FocusHandle, MouseButton, MouseMoveEvent, MouseUpEvent,
    PathBuilder, PinchEvent, ScrollEvent, Subscription, Task,
};
use gpui_component::input::{Escape, InputEvent};
use std::cell::Cell;
use std::rc::Rc;

mod annotate;
mod changes;
mod inspector;
mod panels;
mod structure;

pub(in crate::app) use annotate::Panel;
// D3 builds the editable inspector from these rows; the tests read them.
#[cfg(test)]
pub(in crate::app) use panels::{inspector_columns, KeyKind};

/// Box geometry shared with the layout/routing code.
pub(in crate::app) const METRICS: Metrics = Metrics {
    header_height: 30.0,
    row_height: 20.0,
    separator_height: 6.0,
    min_width: 180.0,
    max_width: 380.0,
    char_width: 9.0,
    column_gap: 110.0,
    row_gap: 28.0,
};
/// Width of a row's glyph slot, marker slot, gaps and padding (diagram units).
const ROW_CHROME: f32 = 62.0;
const TOOLBAR_HEIGHT: f32 = 34.0;
/// Widths (px) the toolbar never gives up: the zoom field, so "100%" always
/// shows whole, and the search field, so its placeholder reads.
const ZOOM_FIELD_WIDTH: f32 = 58.0;
const SEARCH_WIDTH: f32 = 180.0;
// What each toolbar control needs, measured at 100% (`toolbar_level`).
/// Padding, the zoom group, a separator, search, Export.
const TOOLBAR_FIXED: f32 = 542.0;
const TOOLBAR_OVERFLOW: f32 = 32.0;
const TOOLBAR_NOTATION: f32 = 109.0;
const TOOLBAR_SEPARATOR: f32 = 17.0;
const TOOLBAR_MODE_BASE: f32 = 40.0;
const TOOLBAR_MODE_PER_CHAR: f32 = 7.5;
const TOOLBAR_UNDO_REDO: f32 = 69.0;
const TOOLBAR_TOGGLES: f32 = 112.0;
const TOOLBAR_SUGGESTION_COUNT: f32 = 15.0;
/// The "Changes (N)" button (icon, label, count).
const TOOLBAR_CHANGES: f32 = 112.0;
const TOOLBAR_SEARCH_COUNT: f32 = 36.0;
/// The schema diagram's table count at the right end.
const TOOLBAR_STATUS: f32 = 80.0;
const FIT_PADDING: f32 = 32.0;
/// Zoom factor of the toolbar buttons.
const ZOOM_STEP: f32 = 1.25;
/// Presets of the zoom menu, in percent (all inside both kinds' ranges).
const ZOOM_PRESETS: [u32; 5] = [50, 75, 100, 150, 200];
/// Wheel/pinch sensitivity: factor per scrolled pixel.
const WHEEL_ZOOM_PER_PX: f32 = 0.0025;
/// Below this zoom rows show the column name only; below `OVERVIEW_ZOOM`
/// boxes are drawn without rows.
const DETAIL_ZOOM: f32 = 0.75;
const OVERVIEW_ZOOM: f32 = 0.35;
const LINE_WIDTH: f32 = 1.25;
const LINE_WIDTH_HIGHLIGHT: f32 = 2.0;
/// Overview in the bottom-right corner of the drawing area.
const MINIMAP_WIDTH: f32 = 180.0;
const MINIMAP_HEIGHT: f32 = 120.0;
const MINIMAP_MARGIN: f32 = 12.0;
const MINIMAP_PADDING: f32 = 6.0;
// Benchmarks switch the minimap off to measure what it costs.
#[cfg(test)]
thread_local! {
    pub(in crate::app) static MINIMAP_ENABLED: std::cell::Cell<bool> = const { std::cell::Cell::new(true) };
}
/// The minimap is drawn this opaque until the pointer is over it, so it
/// never hides the drawing behind it for good.
const MINIMAP_IDLE_OPACITY: f32 = 0.55;
// Benchmarks switch the D2 toolbar group off to measure what it costs.
#[cfg(test)]
thread_local! {
    pub(in crate::app) static ANNOTATE_TOOLBAR_ENABLED: std::cell::Cell<bool> = const { std::cell::Cell::new(true) };
}
/// Screen distance within which a click picks a relation line.
const RELATION_PICK: f32 = 6.0;
/// Quiet time after the last entity drag before the positions are written.
pub(in crate::app) const SAVE_DEBOUNCE: std::time::Duration = std::time::Duration::from_millis(500);

/// Routes per painted path (see `lines_canvas`).
const BATCH: usize = 150;
/// Dashed routes per painted path: a dash is several triangles, so a path
/// holds fewer routes before it nears GPUI's 16-bit vertex index limit.
const DASHED_BATCH: usize = 12;
/// Dash and gap of logical (user-declared) relationships and of the link
/// preview, in pixels.
const DASH: [f32; 2] = [7.0, 4.0];

/// Lines and arrow heads of one colour, painted as two paths.
struct Batch {
    lines: PathBuilder,
    arrows: PathBuilder,
    width: f32,
    count: usize,
    dashed: bool,
}

impl Batch {
    fn new(width: f32) -> Self {
        Self::build(width, false)
    }

    fn dashed(width: f32) -> Self {
        Self::build(width, true)
    }

    fn build(width: f32, dashed: bool) -> Self {
        let lines = PathBuilder::stroke(px(width));
        let lines = if dashed { lines.dash_array(&[px(DASH[0]), px(DASH[1])]) } else { lines };
        Self { lines, arrows: PathBuilder::fill(), width, count: 0, dashed }
    }

    /// Whether the path is full enough to paint (see `BATCH`).
    fn full(&self) -> bool {
        self.count >= if self.dashed { DASHED_BATCH } else { BATCH }
    }

    /// Paints what was collected and starts over.
    fn flush(&mut self, window: &mut Window, color: gpui::Rgba) {
        if self.count == 0 {
            return;
        }
        let fresh = Self::build(self.width, self.dashed);
        let done = std::mem::replace(self, fresh);
        if let Ok(path) = done.lines.build() {
            window.paint_path(path, color);
        }
        if let Ok(path) = done.arrows.build() {
            window.paint_path(path, color);
        }
    }
}

pub(in crate::app) enum DiagramEvent {
    /// Double-click on an entity: open that table in a tab.
    OpenTable(TableRef),
    /// Refresh button: drop the cache and refetch every table.
    Refresh,
    /// Export menu: ask for a file and write `request` in `format`.
    Export { format: ExportFormat, file_name: String, request: Box<ExportRequest> },
    /// Saved positions could not be read or written (shown in the error log).
    PersistFailed(String),
    /// Apply in the review dialog: run these changes on the connection.
    Apply(Box<ApplyRequest>),
    /// Cancel in the progress of a running apply.
    CancelApply,
}

#[derive(Debug, Clone, Copy)]
pub(in crate::app) enum Drag {
    Pan { last: Point<gpui::Pixels> },
    /// Moving an entity, or the whole selection when it is part of it.
    Entity { id: EntityId, last: Point<gpui::Pixels> },
    /// Dragging the viewport rectangle of the minimap; the mapping is frozen
    /// at the press so the content does not shift under the pointer.
    Minimap(Minimap),
    /// From a column row to another table's column: a logical relationship.
    Link { table: EntityId, column: usize, pointer: Point<gpui::Pixels> },
    Note { id: u64, last: Point<gpui::Pixels> },
    NoteResize { id: u64, last: Point<gpui::Pixels> },
    /// Moving a group's entities together.
    Group { id: u64, last: Point<gpui::Pixels> },
    /// Rubber band on empty canvas (diagram space).
    Marquee { origin: DiagramPoint, to: DiagramPoint },
}

/// What the view shows: one table and its neighbours, or a whole schema
/// (no focus entity, layered layout, progressive loading).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub(in crate::app) enum DiagramKind {
    #[default]
    Table,
    Schema,
}

pub(in crate::app) struct DiagramView {
    pub(in crate::app) kind: DiagramKind,
    pub(in crate::app) snapshot: Option<DiagramSnapshot>,
    pub(in crate::app) layout: DiagramLayout,
    pub(in crate::app) routes: Arc<Vec<RoutedRelation>>,
    pub(in crate::app) mode: AttributeMode,
    pub(in crate::app) viewport: Viewport,
    /// Bounds of the drawing area, measured by the lines canvas each frame.
    pub(in crate::app) area: Rc<Cell<Bounds<gpui::Pixels>>>,
    /// Fit as soon as the area has been measured (first layout, Re-layout).
    pub(in crate::app) needs_fit: bool,
    /// Keep fitting the whole graph whenever the layout or the area changes,
    /// until the user pans, zooms or drags (Fit / Re-layout re-arm it).
    pub(in crate::app) auto_fit: bool,
    /// Area size the last Fit was computed for.
    fitted_size: gpui::Size<gpui::Pixels>,
    /// The primary selection (last clicked): drives the highlight, the
    /// inspector and the minimap. Always part of `selection`.
    pub(in crate::app) selected: Option<EntityId>,
    /// Every selected entity (shift-click, rubber band), ascending.
    pub(in crate::app) selection: Vec<EntityId>,
    pub(in crate::app) selected_note: Option<u64>,
    pub(in crate::app) selected_group: Option<u64>,
    /// Index into `graph.relations`.
    pub(in crate::app) selected_relation: Option<usize>,
    pub(in crate::app) hovered: Option<EntityId>,
    /// The column row under the pointer (shows the link handle).
    hover_row: Option<(EntityId, usize)>,
    pub(in crate::app) drag: Option<Drag>,
    /// Some entity was placed by hand (dragged or restored from the store).
    pub(in crate::app) moved: bool,
    /// `pinned[i]`: entity `i` was placed by the user; reflows leave it where
    /// it is and only the others follow the automatic layout.
    pub(in crate::app) pinned: Vec<bool>,
    /// `linked[i]`: entity `i` shares a relation with the highlighted one;
    /// valid for `linked_key` only.
    linked: Vec<bool>,
    linked_key: Option<(Option<EntityId>, u64)>,
    /// The editable zoom field; created by the first render (it needs a window).
    zoom_input: Option<ZoomInput>,
    pub(in crate::app) notation: RelationNotation,
    focus_handle: FocusHandle,
    pub(in crate::app) search: Search,
    /// Quoting of the connection, for Copy JOIN SQL.
    pub(in crate::app) dialect: SqlDialect,
    /// Entity rectangles as the minimap canvas paints them; refreshed with
    /// the routes whenever the layout changes.
    rects: Arc<Vec<DiagramRect>>,
    persist: Option<Persist>,
    /// Debounce timer of the next save (replaced by every edit).
    save_task: Option<Task<()>>,
    load_task: Option<Task<()>>,
    /// Last export result, shown bottom-left: `(text, is_error)`.
    pub(in crate::app) notice: Option<(String, bool)>,
    /// The document changed since the last save.
    doc_dirty: bool,
    /// Writes a pending edit when the app quits.
    _quit: Subscription,
    // ---- D2 ----
    /// The graph as the catalog declares it; `snapshot.graph` is this plus the
    /// logical relationships.
    declared: Option<Arc<DiagramGraph>>,
    pub(in crate::app) annotations: Arc<Annotations>,
    /// Groups of the current graph: members and what they hide.
    pub(in crate::app) groups: GroupIndex,
    /// Header color per entity (empty while no entity has one).
    entity_colors: Vec<Option<Rgb>>,
    /// Candidate foreign keys, best first.
    pub(in crate::app) suggestions: Arc<Vec<Suggestion>>,
    pub(in crate::app) panel: Option<Panel>,
    /// The pointer is over the minimap (it is drawn opaque then).
    minimap_hover: bool,
    /// What the running drag started from, for its undo step.
    drag_start: Option<annotate::DragStart>,
    note_edit: Option<annotate::NoteEdit>,
    group_rename: Option<annotate::GroupRename>,
    /// Diagram position of the last right click ("Add note" puts the note there).
    context_at: DiagramPoint,
    /// Capture-only: the entity to select once the diagram has loaded.
    seed_select: Option<String>,
    /// Width of the toolbar, measured by a canvas in it each frame.
    toolbar_width: Rc<Cell<f32>>,
    // ---- D3 ----
    /// The pending structure changes of this diagram (`snapshot.graph` shows them applied), their
    /// undo history (moves and annotation edits share it) and the review dialog's progress.
    pub(in crate::app) session: EditSession<Edit>,
    /// State of each entity of the effective graph (empty without a draft).
    entity_states: Vec<TableState>,
    /// What AppRoot allows (`None`: this diagram is not editable at all).
    pub(in crate::app) edit: Option<EditContext>,
    editor: structure::Editor,
    /// Capture-only: stage the sample pending edits / open the review dialog once loaded.
    seed_draft: bool,
    /// Capture-only: the seeded draft started from an older catalog (`stale` token).
    seed_stale: bool,
    seed_changes: bool,
    /// Capture-only: the column to select in the Inspector once its table is selected.
    seed_column: Option<String>,
    /// The changes an apply was running (saved before it starts, cleared when it ends): after
    /// a crash the ones already in the catalog are dropped from the draft on load.
    applying_marker: Vec<crate::sql_ddl::Change>,
}

/// Where this diagram's positions live and what is known about them.
struct Persist {
    store: Arc<LayoutStore>,
    key: DiagramKey,
    /// What is on disk (kept in sync by saves) once loaded.
    saved: Option<SavedDiagram>,
    /// A file from a newer build is never overwritten.
    writable: bool,
}

/// The toolbar search: table and column names, Enter cycles the hits.
pub(in crate::app) struct Search {
    state: Option<SearchInput>,
    /// Trimmed, lower-case; empty = no search.
    pub(in crate::app) query: String,
    pub(in crate::app) matches: Arc<Vec<EntityId>>,
    /// `hit[i]`: entity `i` is in `matches` (empty without a search): O(1)
    /// per box for the renderer and the minimap.
    hit: Arc<Vec<bool>>,
    /// Index into `matches` of the entity the view was centred on.
    pub(in crate::app) current: Option<usize>,
}

struct SearchInput {
    state: gpui::Entity<InputState>,
    _subscription: Subscription,
}

/// The zoom field and what it currently shows.
struct ZoomInput {
    state: gpui::Entity<InputState>,
    /// Percent the field text was last set to: an idle view never rewrites it.
    shown: Option<u32>,
    _subscription: gpui::Subscription,
}

impl EventEmitter<DiagramEvent> for DiagramView {}


/// Zoom factor typed in the zoom field: `120` and `120%` are percents, `1.2x`
/// is a factor. `None` for anything else (including zero and negatives).
pub(in crate::app) fn parse_zoom(text: &str) -> Option<f32> {
    let text = text.trim();
    let (number, scale) = match text.strip_suffix(['x', 'X']) {
        Some(factor) => (factor, 1.0),
        None => (text.strip_suffix('%').unwrap_or(text), 0.01),
    };
    let value: f32 = number.trim().parse().ok()?;
    (value.is_finite() && value > 0.0).then_some(value * scale)
}

/// Whole percent shown for a zoom factor.
fn zoom_percent(zoom: f32) -> u32 {
    (zoom * 100.0).round() as u32
}

/// Columns an entity box shows in `mode`, in order.
fn visible_columns(
    entity: &DiagramEntity,
    mode: AttributeMode,
) -> impl Iterator<Item = (usize, &DiagramColumn)> {
    entity
        .columns
        .iter()
        .enumerate()
        .filter(move |(_, column)| mode.shows(column))
}

/// Fit that never magnifies: small diagrams stay at 100%, centered. When the
/// content cannot fit even at the minimum zoom, the view starts at the
/// focus entity (table diagrams) or the top-left corner (schema diagrams)
/// instead of the middle of nowhere.
fn fit_viewport(
    bounds: DiagramRect,
    focus: Option<DiagramRect>,
    width: f32,
    height: f32,
    min_zoom: f32,
) -> Viewport {
    let fitted = Viewport::fit(bounds, width, height, FIT_PADDING, min_zoom);
    let needed = ((width - 2.0 * FIT_PADDING) / bounds.width.max(1.0))
        .min((height - 2.0 * FIT_PADDING) / bounds.height.max(1.0));
    if needed < min_zoom {
        let zoom = min_zoom;
        let origin = match focus {
            Some(focus) => DiagramPoint {
                x: focus.x + focus.width / 2.0 - width / 2.0 / zoom,
                y: focus.y + focus.height / 2.0 - height / 2.0 / zoom,
            },
            None => DiagramPoint { x: bounds.x - FIT_PADDING / zoom, y: bounds.y - FIT_PADDING / zoom },
        };
        return Viewport { zoom, origin, min_zoom };
    }
    if fitted.zoom <= 1.0 {
        return fitted;
    }
    Viewport {
        zoom: 1.0,
        min_zoom,
        origin: DiagramPoint {
            x: bounds.x + bounds.width / 2.0 - width / 2.0,
            y: bounds.y + bounds.height / 2.0 - height / 2.0,
        },
    }
}

impl DiagramView {
    pub(in crate::app) fn new(kind: DiagramKind, cx: &mut Context<Self>) -> Self {
        let min_zoom = match kind {
            DiagramKind::Table => Viewport::MIN_ZOOM,
            DiagramKind::Schema => Viewport::SCHEMA_MIN_ZOOM,
        };
        Self {
            kind,
            snapshot: None,
            layout: DiagramLayout::default(),
            routes: Arc::default(),
            mode: AttributeMode::All,
            viewport: Viewport { origin: DiagramPoint::default(), zoom: 1.0, min_zoom },
            area: Rc::default(),
            needs_fit: true,
            auto_fit: true,
            fitted_size: Default::default(),
            selected: None,
            selection: Vec::new(),
            selected_note: None,
            selected_group: None,
            selected_relation: None,
            hovered: None,
            hover_row: None,
            drag: None,
            moved: false,
            pinned: Vec::new(),
            linked: Vec::new(),
            linked_key: None,
            zoom_input: None,
            notation: RelationNotation::default(),
            focus_handle: cx.focus_handle(),
            search: Search {
                state: None,
                query: String::new(),
                matches: Arc::default(),
                hit: Arc::default(),
                current: None,
            },
            dialect: SqlDialect::POSTGRES,
            rects: Arc::default(),
            persist: None,
            save_task: None,
            load_task: None,
            notice: None,
            doc_dirty: false,
            _quit: cx.on_app_quit(|this, _| {
                this.flush_blocking();
                async {}
            }),
            declared: None,
            annotations: Arc::default(),
            groups: GroupIndex::default(),
            entity_colors: Vec::new(),
            suggestions: Arc::default(),
            panel: None,
            minimap_hover: false,
            drag_start: None,
            note_edit: None,
            group_rename: None,
            context_at: DiagramPoint::default(),
            seed_select: None,
            toolbar_width: Rc::default(),
            session: EditSession::default(),
            entity_states: Vec::new(),
            edit: None,
            editor: structure::Editor::default(),
            seed_draft: false,
            seed_stale: false,
            seed_changes: false,
            seed_column: None,
            applying_marker: Vec::new(),
        }
    }

    fn graph(&self) -> Option<&DiagramGraph> {
        self.snapshot.as_ref().map(|snapshot| snapshot.graph.as_ref())
    }

    /// Automatic positions for the current kind.
    fn auto_layout(&self, graph: &DiagramGraph) -> DiagramLayout {
        match self.kind {
            DiagramKind::Table => DiagramLayout::auto(graph, self.mode, &METRICS),
            DiagramKind::Schema => DiagramLayout::auto_schema(graph, self.mode, &METRICS),
        }
    }

    /// New data from `data`. A changed entity set is laid out from scratch
    /// (and fitted). Otherwise state changes (a table finished loading) reflow
    /// the boxes: the ones the user placed stay, the others follow the
    /// automatic layout for their new sizes and never overlap.
    pub(in crate::app) fn set_snapshot(&mut self, snapshot: Option<DiagramSnapshot>, cx: &mut Context<Self>) {
        let unchanged = match (&self.snapshot, &snapshot) {
            (None, None) => true,
            (Some(old), Some(new)) => {
                old.generation == new.generation
                    && old.error == new.error
                    && old.progress == new.progress
                    && old.truncated == new.truncated
            }
            _ => false,
        };
        if unchanged {
            return;
        }
        self.declared = snapshot.as_ref().map(|snapshot| snapshot.graph.clone());
        self.snapshot = snapshot;
        self.rebuild_effective();
        // The effective graph: the pending structure changes may add tables.
        let entities = self.snapshot.as_ref().map_or(0, |snapshot| snapshot.graph.entities.len());
        let rebuild = entities != self.layout.rects.len();
        self.linked_key = None;
        match self.snapshot.as_ref().map(|snapshot| snapshot.graph.clone()) {
            Some(graph) if entities > 0 => {
                if rebuild {
                    self.layout = self.auto_layout(&graph);
                    self.pinned = vec![false; entities];
                    self.moved = false;
                    self.apply_saved_positions();
                    self.needs_fit = true;
                    self.clear_selection_state();
                    self.hovered = None;
                } else {
                    let schema = self.kind == DiagramKind::Schema;
                    self.layout.reflow(&graph, self.mode, &METRICS, schema, &self.pinned);
                }
                self.refresh_derived();
                // Boxes grow as tables load: until the user takes over, the
                // view follows the whole graph.
                self.needs_fit |= self.auto_fit;
            }
            _ => {
                self.layout = DiagramLayout::default();
                self.pinned.clear();
                self.routes = Arc::default();
                self.rects = Arc::default();
                self.groups = GroupIndex::default();
                self.suggestions = Arc::default();
            }
        }
        self.refresh_search();
        self.apply_seed_draft(cx);
        self.apply_seed_column();
        self.reconcile(cx);
        cx.notify();
    }

    /// Routes and minimap rectangles for the current layout. Entities hidden
    /// by a group, and the lines touching them, are left out.
    fn refresh_geometry(&mut self) {
        if let Some(graph) = self.graph() {
            let mut routes = self.layout.route(graph, self.mode, &METRICS);
            if self.groups.hidden.iter().any(|&hidden| hidden) {
                routes.retain(|route| {
                    graph
                        .relations
                        .get(route.relation)
                        .is_some_and(|r| self.groups.shown(r.child.0) && self.groups.shown(r.parent.0))
                });
            }
            self.routes = Arc::new(routes);
        }
        let mut rects = self.layout.rects.clone();
        for (index, rect) in rects.iter_mut().enumerate() {
            if !self.groups.shown(index) {
                (rect.width, rect.height) = (0.0, 0.0);
            }
        }
        self.rects = Arc::new(rects);
    }

    /// Re-layout button: automatic positions again (forgetting the saved
    /// positions, not the annotations), then fit. One undo step.
    pub(in crate::app) fn relayout(&mut self, cx: &mut Context<Self>) {
        if let Some(graph) = self.snapshot.as_ref().map(|snapshot| snapshot.graph.clone()) {
            let before = self.layout.rects.clone();
            self.layout = self.auto_layout(&graph);
            self.pinned = vec![false; self.layout.rects.len()];
            self.moved = false;
            self.record_moves(&before);
            self.refresh_geometry();
            self.needs_fit = true;
            self.auto_fit = true;
            self.forget_saved_positions(cx);
        }
        cx.notify();
    }

    pub(in crate::app) fn set_mode(&mut self, mode: AttributeMode, cx: &mut Context<Self>) {
        if self.mode == mode {
            return;
        }
        self.mode = mode;
        if let Some(graph) = self.snapshot.as_ref().map(|snapshot| snapshot.graph.clone()) {
            let schema = self.kind == DiagramKind::Schema;
            self.layout.reflow(&graph, mode, &METRICS, schema, &self.pinned);
            self.refresh_geometry();
            self.needs_fit |= self.auto_fit;
        }
        cx.notify();
    }

    pub(in crate::app) fn toggle_notation(&mut self, cx: &mut Context<Self>) {
        self.notation = match self.notation {
            RelationNotation::Plain => RelationNotation::CrowsFoot,
            RelationNotation::CrowsFoot => RelationNotation::Plain,
        };
        cx.notify();
    }

    pub(in crate::app) fn fit(&mut self, cx: &mut Context<Self>) {
        self.needs_fit = true;
        self.auto_fit = true;
        cx.notify();
    }

    /// Screen position (relative to the area) of a window position.
    fn local(&self, position: Point<gpui::Pixels>) -> DiagramPoint {
        let origin = self.area.get().origin;
        DiagramPoint {
            x: f32::from(position.x - origin.x),
            y: f32::from(position.y - origin.y),
        }
    }

    fn area_center(&self) -> DiagramPoint {
        let size = self.area.get().size;
        DiagramPoint { x: f32::from(size.width) / 2.0, y: f32::from(size.height) / 2.0 }
    }

    pub(in crate::app) fn zoom_by(&mut self, factor: f32, anchor: Option<DiagramPoint>, cx: &mut Context<Self>) {
        let anchor = anchor.unwrap_or_else(|| self.area_center());
        self.auto_fit = false;
        self.viewport.zoom_at(factor, anchor);
        cx.notify();
    }

    /// Zooms to `target` around the area center, clamped to the viewport range.
    pub(in crate::app) fn set_zoom(&mut self, target: f32, cx: &mut Context<Self>) {
        self.zoom_by(target / self.viewport.zoom, None, cx);
    }

    /// Text of the zoom field (tests).
    #[cfg(test)]
    pub(in crate::app) fn zoom_text(&self, cx: &gpui::App) -> String {
        self.zoom_input.as_ref().map(|input| input.state.read(cx).value().to_string()).unwrap_or_default()
    }

    /// Creates the zoom field on first use.
    fn ensure_zoom_input(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.zoom_input.is_some() {
            return;
        }
        let state = cx.new(|cx| InputState::new(window, cx));
        let subscription = cx.subscribe_in(&state, window, |this, state, event: &InputEvent, window, cx| match event {
            InputEvent::Focus => state.update(cx, |state, cx| state.select_all(window, cx)),
            InputEvent::PressEnter { .. } => {
                this.commit_zoom_text(window, cx);
                window.blur(cx);
            }
            InputEvent::Blur => this.commit_zoom_text(window, cx),
            InputEvent::Change => {}
        });
        self.zoom_input = Some(ZoomInput { state, shown: None, _subscription: subscription });
    }

    /// Shows the current zoom in the field. Only touches the field when the
    /// percentage changed (or `force`), and never while the user types.
    fn sync_zoom_text(&mut self, force: bool, window: &mut Window, cx: &mut Context<Self>) {
        let percent = zoom_percent(self.viewport.zoom);
        let Some(input) = self.zoom_input.as_mut() else {
            return;
        };
        if !force && (input.shown == Some(percent) || input.state.read(cx).focus_handle(cx).is_focused(window)) {
            return;
        }
        input.shown = Some(percent);
        input.state.update(cx, |state, cx| state.set_value(format!("{percent}%"), window, cx));
    }

    /// Enter / blur: applies the typed zoom; invalid text just reverts.
    fn commit_zoom_text(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let typed = self.zoom_input.as_ref().and_then(|input| parse_zoom(&input.state.read(cx).value()));
        if let Some(target) = typed {
            let target = target.clamp(self.viewport.min_zoom, Viewport::MAX_ZOOM);
            if zoom_percent(target) != zoom_percent(self.viewport.zoom) {
                self.set_zoom(target, cx);
            }
        }
        self.sync_zoom_text(true, window, cx);
    }

    /// A click on the drawing area ends the edit like a click elsewhere in a
    /// form (GPUI does not move focus off a field for a click on plain content).
    fn release_zoom_focus(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.zoom_input.as_ref().is_some_and(|input| input.state.read(cx).focus_handle(cx).is_focused(window)) {
            window.blur(cx);
        }
    }

    /// Escape: drops the typed text.
    fn cancel_zoom_text(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.sync_zoom_text(true, window, cx);
        window.blur(cx);
    }

    /// The entity under `local` (area coordinates); hidden ones are not there.
    fn entity_at(&self, local: DiagramPoint) -> Option<EntityId> {
        self.layout.hit_test_where(self.viewport.to_diagram(local), |i| self.groups.shown(i))
    }

    fn on_mouse_down(&mut self, event: &gpui::MouseDownEvent, window: &mut Window, cx: &mut Context<Self>) {
        window.focus(&self.focus_handle, cx);
        let local = self.local(event.position);
        if let Some(map) = self.minimap_hit(local) {
            self.drag = Some(Drag::Minimap(map));
            self.pan_from_minimap(map, local);
            cx.notify();
            return;
        }
        let at = self.viewport.to_diagram(local);
        if !self.press_note(at, event, window, cx) {
            match self.entity_at(local) {
                Some(id) => self.press_entity(id, at, event, cx),
                None => self.press_background(at, event, cx),
            }
        }
        cx.notify();
    }

    fn on_mouse_move(&mut self, event: &MouseMoveEvent, cx: &mut Context<Self>) {
        let Some(drag) = self.drag else {
            self.hover(event.position, cx);
            return;
        };
        if event.pressed_button != Some(MouseButton::Left) {
            self.end_drag(cx);
            return;
        }
        let zoom = self.viewport.zoom;
        let moved_by = |last: Point<gpui::Pixels>| {
            let delta = event.position - last;
            (f32::from(delta.x) / zoom, f32::from(delta.y) / zoom)
        };
        match drag {
            Drag::Pan { last } => {
                let delta = event.position - last;
                self.viewport.pan(f32::from(delta.x), f32::from(delta.y));
                self.auto_fit = false;
                self.drag = Some(Drag::Pan { last: event.position });
            }
            Drag::Entity { id, last } => {
                let (dx, dy) = moved_by(last);
                self.move_selection(id, dx, dy);
                self.drag = Some(Drag::Entity { id, last: event.position });
            }
            Drag::Group { id, last } => {
                let (dx, dy) = moved_by(last);
                self.move_group(id, dx, dy);
                self.drag = Some(Drag::Group { id, last: event.position });
            }
            Drag::Note { id, last } => {
                let (dx, dy) = moved_by(last);
                self.move_note(id, dx, dy);
                self.drag = Some(Drag::Note { id, last: event.position });
            }
            Drag::NoteResize { id, last } => {
                let (dx, dy) = moved_by(last);
                self.resize_note(id, dx, dy);
                self.drag = Some(Drag::NoteResize { id, last: event.position });
            }
            Drag::Link { table, column, .. } => {
                self.hover_row = self.row_under(event.position);
                self.drag = Some(Drag::Link { table, column, pointer: event.position });
            }
            Drag::Marquee { origin, .. } => {
                let to = self.viewport.to_diagram(self.local(event.position));
                self.drag = Some(Drag::Marquee { origin, to });
                self.apply_marquee(origin, to);
            }
            Drag::Minimap(map) => {
                let local = self.local(event.position);
                self.pan_from_minimap(map, local);
            }
        }
        cx.notify();
    }

    fn on_mouse_up(&mut self, _: &MouseUpEvent, cx: &mut Context<Self>) {
        self.end_drag(cx);
    }

    /// Ends the current drag: a moved entity, note or group becomes one undo
    /// step and starts the debounced save; a link drag creates the logical
    /// relationship it ended on.
    fn end_drag(&mut self, cx: &mut Context<Self>) {
        let Some(drag) = self.drag.take() else {
            return;
        };
        match drag {
            Drag::Entity { .. } | Drag::Group { .. } => self.finish_move(cx),
            Drag::Note { .. } | Drag::NoteResize { .. } => self.finish_annotation_drag(cx),
            Drag::Link { table, column, pointer } => self.finish_link(table, column, pointer, cx),
            Drag::Pan { .. } | Drag::Minimap(_) | Drag::Marquee { .. } => {}
        }
        cx.notify();
    }

    fn on_scroll(&mut self, event: &ScrollEvent, cx: &mut Context<Self>) {
        let delta = event.delta.pixel_delta(px(16.0));
        if event.modifiers.platform || event.modifiers.control {
            let factor = (1.0 - f32::from(delta.y) * WHEEL_ZOOM_PER_PX).clamp(0.5, 2.0);
            let anchor = self.local(event.position);
            self.zoom_by(factor, Some(anchor), cx);
        } else {
            self.viewport.pan(f32::from(delta.x), f32::from(delta.y));
            self.auto_fit = false;
            cx.notify();
        }
    }

    fn on_pinch(&mut self, event: &PinchEvent, cx: &mut Context<Self>) {
        let anchor = self.local(event.position);
        self.zoom_by((1.0 + event.delta).clamp(0.5, 2.0), Some(anchor), cx);
    }

    /// Entity highlighted with its relationships: the dragged one, else the
    /// hovered one, else the selected one.
    fn highlighted(&self) -> Option<EntityId> {
        match self.drag {
            Some(Drag::Entity { id, .. }) => Some(id),
            _ => self.hovered.or(self.selected),
        }
    }

    fn toolbar(&self, cx: &mut Context<Self>) -> Div {
        let icon_button = |id: &'static str, path: &'static str, tooltip: &'static str| {
            Button::new(id)
                .icon(Icon::empty().path(path).size(px(14.0)))
                .with_size(Size::Small)
                .compact()
                .ghost()
                .tooltip(tooltip)
                .debug_selector(move || id.into())
        };
        let mode = self.mode;
        let level = self.toolbar_level();
        let measure = {
            let width = self.toolbar_width.clone();
            let entity = cx.entity_id();
            canvas(
                move |bounds, _, cx| {
                    let measured = f32::from(bounds.size.width);
                    // A resized toolbar re-renders the view (which controls collapse).
                    if (width.replace(measured) - measured).abs() > 0.5 {
                        cx.notify(entity);
                    }
                },
                |_, _, _, _| {},
            )
            .absolute()
            .size_full()
        };
        div()
            .debug_selector(|| "diagram-toolbar".into())
            .relative()
            .h(px(TOOLBAR_HEIGHT))
            .w_full()
            .flex_shrink_0()
            .flex()
            .items_center()
            .gap(px(4.0))
            .px(px(8.0))
            .overflow_hidden()
            .bg(theme::grid_header())
            .border_b_1()
            .border_color(theme::border())
            .child(measure)
            .child(icon_button("diagram-zoom-out", "icons/minus.svg", "Zoom out").on_click(
                cx.listener(|this, _, _, cx| this.zoom_by(1.0 / ZOOM_STEP, None, cx)),
            ))
            .child(
                div()
                    .debug_selector(|| "diagram-zoom-label".into())
                    .w(px(ZOOM_FIELD_WIDTH))
                    .flex_shrink_0()
                    .on_action(cx.listener(|this, _: &Escape, window, cx| this.cancel_zoom_text(window, cx)))
                    .children(
                        self.zoom_input
                            .as_ref()
                            .map(|input| Input::new(&input.state).with_size(Size::Small).cleanable(false)),
                    ),
            )
            .child(
                Button::new("diagram-zoom-presets")
                    .dropdown_caret(true)
                    .with_size(Size::Small)
                    .compact()
                    .ghost()
                    .tooltip("Zoom presets")
                    .debug_selector(|| "diagram-zoom-presets".into())
                    .dropdown_menu({
                        let view = cx.entity().downgrade();
                        let current = zoom_percent(self.viewport.zoom);
                        move |menu, _, _| {
                            let menu = ZOOM_PRESETS.into_iter().fold(menu, |menu, percent| {
                                let view = view.clone();
                                menu.item(
                                    PopupMenuItem::new(format!("{percent}%")).checked(current == percent).on_click(
                                        move |_, _, app| {
                                            let _ = view.update(app, |this, cx| this.set_zoom(percent as f32 / 100.0, cx));
                                        },
                                    ),
                                )
                            });
                            let view = view.clone();
                            menu.separator().item(PopupMenuItem::new("Fit").on_click(move |_, _, app| {
                                let _ = view.update(app, |this, cx| this.fit(cx));
                            }))
                        }
                    }),
            )
            .child(icon_button("diagram-zoom-in", "icons/plus.svg", "Zoom in").on_click(
                cx.listener(|this, _, _, cx| this.zoom_by(ZOOM_STEP, None, cx)),
            ))
            .child(icon_button("diagram-fit", "icons/maximize.svg", "Fit to view").on_click(
                cx.listener(|this, _, _, cx| this.fit(cx)),
            ))
            .child(
                icon_button("diagram-relayout", "icons/layout-dashboard.svg", "Re-layout")
                    .on_click(cx.listener(|this, _, _, cx| this.relayout(cx))),
            )
            .child(
                icon_button("diagram-refresh", "icons/refresh-cw.svg", "Refresh relationships")
                    .on_click(cx.listener(|_, _, _, cx| cx.emit(DiagramEvent::Refresh))),
            )
            .children((level < 2).then(|| div().w(px(1.0)).h(px(16.0)).mx(px(4.0)).bg(theme::border())))
            .children((level < 2).then(|| {
                Button::new("diagram-mode")
                    .label(mode.label())
                    .dropdown_caret(true)
                    .with_size(Size::Small)
                    .compact()
                    .ghost()
                    .tooltip("Visible columns")
                    .debug_selector(|| "diagram-mode".into())
                    .dropdown_menu({
                        let view = cx.entity().downgrade();
                        move |menu, _, _| {
                            AttributeMode::ALL.into_iter().fold(menu, |menu, choice| {
                                let view = view.clone();
                                menu.item(PopupMenuItem::new(choice.label()).checked(mode == choice).on_click(
                                    move |_, _, app| {
                                        let _ = view.update(app, |this, cx| this.set_mode(choice, cx));
                                    },
                                ))
                            })
                        }
                    })
            }))
            .children((level < 1).then(|| {
                Button::new("diagram-notation")
                    .label("Crow's foot")
                    .with_size(Size::Small)
                    .compact()
                    .ghost()
                    .selected(self.notation == RelationNotation::CrowsFoot)
                    .tooltip("Crow's foot relationship ends (off: plain arrows)")
                    .debug_selector(|| "diagram-notation".into())
                    .on_click(cx.listener(|this, _, _, cx| this.toggle_notation(cx)))
            }))
            .child(div().w(px(1.0)).h(px(16.0)).mx(px(4.0)).bg(theme::border()))
            .child(
                div()
                    .debug_selector(|| "diagram-search".into())
                    .w(px(SEARCH_WIDTH))
                    .flex_shrink_0()
                    .on_action(cx.listener(|this, _: &Escape, window, cx| {
                        this.clear_search(window, cx);
                        window.focus(&this.focus_handle, cx);
                    }))
                    .children(self.search.state.as_ref().map(|input| {
                        Input::new(&input.state)
                            .with_size(Size::Small)
                            .prefix(Icon::new(IconName::Search).size(px(13.0)).text_color(theme::text_muted()))
                    })),
            )
            .children((!self.search.query.is_empty()).then(|| {
                let text = match self.search.current {
                    Some(current) => format!("{}/{}", current + 1, self.search.matches.len()),
                    None => "0".to_string(),
                };
                div()
                    .debug_selector(|| "diagram-search-count".into())
                    .min_w(px(32.0))
                    .flex_shrink_0()
                    .text_size(px(11.0))
                    .text_color(theme::text_muted())
                    .child(text)
            }))
            .child(
                Button::new("diagram-export")
                    .icon(Icon::empty().path("icons/download.svg").size(px(14.0)))
                    .label("Export")
                    .dropdown_caret(true)
                    .with_size(Size::Small)
                    .compact()
                    .ghost()
                    .tooltip("Export the diagram")
                    .debug_selector(|| "diagram-export".into())
                    .dropdown_menu({
                        let view = cx.entity().downgrade();
                        move |menu, _, _| {
                            ExportFormat::ALL.into_iter().fold(menu, |menu, format| {
                                let view = view.clone();
                                menu.item(PopupMenuItem::new(format.label()).on_click(move |_, _, app| {
                                    let _ = view.update(app, |this, cx| this.export(format, cx));
                                }))
                            })
                        }
                    }),
            )
            .children((self.supports_structure() && level < 5).then(|| self.changes_button(cx)))
            .children(Self::annotate_toolbar_enabled().then(|| self.annotate_toolbar(level, cx)))
    }

    /// How many of the lower-priority toolbar controls have moved into the
    /// overflow menu (0: all shown). Decided from the measured toolbar width
    /// with fixed estimates of what each control needs; the order is
    /// 1 Crow's foot, 2 Visible columns, 3 Undo / Redo, 4 the three panel
    /// toggles, 5 Changes (N) (the last to go: pending edits stay one click
    /// away). The zoom field, search, Export and the zoom / fit /
    /// re-layout / refresh buttons never collapse (and never shrink).
    pub(in crate::app) fn toolbar_level(&self) -> usize {
        let width = self.toolbar_width.get();
        if width <= 1.0 {
            return 0;
        }
        let mode = TOOLBAR_MODE_BASE + TOOLBAR_MODE_PER_CHAR * self.mode.label().chars().count() as f32;
        let fixed = TOOLBAR_FIXED
            + if self.search.query.is_empty() { 0.0 } else { TOOLBAR_SEARCH_COUNT }
            + if self.kind == DiagramKind::Schema { TOOLBAR_STATUS } else { 0.0 };
        let toggles = TOOLBAR_TOGGLES + if self.suggestions.is_empty() { 0.0 } else { TOOLBAR_SUGGESTION_COUNT };
        let changes = if self.supports_structure() { TOOLBAR_CHANGES } else { 0.0 };
        let needed = |level: usize| {
            fixed
                + if level < 5 { changes } else { 0.0 }
                + if level > 0 { TOOLBAR_OVERFLOW } else { 0.0 }
                + if level < 1 { TOOLBAR_NOTATION } else { 0.0 }
                + if level < 2 { TOOLBAR_SEPARATOR + mode } else { 0.0 }
                + if level < 3 { TOOLBAR_UNDO_REDO } else { 0.0 }
                + if level < 4 { toggles } else { 0.0 }
        };
        let deepest = if self.supports_structure() { 5 } else { 4 };
        (0..deepest).find(|&level| needed(level) <= width).unwrap_or(deepest)
    }

    /// The D2 toolbar group (benchmarks can switch it off).
    fn annotate_toolbar_enabled() -> bool {
        #[cfg(test)]
        return ANNOTATE_TOOLBAR_ENABLED.with(std::cell::Cell::get);
        #[cfg(not(test))]
        true
    }

    /// The single canvas: measures the area and paints every on-screen
    /// relation line with its FK dot and parent arrow. Lines and arrow heads
    /// are batched into one path per colour (and per `BATCH` routes, which
    /// keeps a path under GPUI's 16-bit vertex index limit). Logical
    /// relationships are dashed; a running link drag adds its preview line.
    fn lines_canvas(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let area = self.area.clone();
        let entity = cx.entity_id();
        let routes = self.routes.clone();
        let graph = self.snapshot.as_ref().map(|snapshot| snapshot.graph.clone());
        let viewport = self.viewport;
        let highlight = self.highlighted();
        let notation = self.notation;
        let selected_relation = self.selected_relation;
        let link = match self.drag {
            Some(Drag::Link { table, column, pointer }) => self.row_anchor(table, column).map(|start| (start, pointer)),
            _ => None,
        };
        canvas(
            move |bounds, _, cx| {
                // A resized area re-renders the view (pending Fit, culling).
                if area.replace(bounds).size != bounds.size {
                    cx.notify(entity);
                }
            },
            move |bounds, _, window, _| {
                let Some(graph) = graph else {
                    return;
                };
                let zoom = viewport.zoom;
                let to_window = |p: DiagramPoint| {
                    point(
                        bounds.origin.x + px((p.x - viewport.origin.x) * zoom),
                        bounds.origin.y + px((p.y - viewport.origin.y) * zoom),
                    )
                };
                // Visible part of the diagram (diagram units).
                let (x0, y0) = (viewport.origin.x, viewport.origin.y);
                let x1 = x0 + f32::from(bounds.size.width) / zoom;
                let y1 = y0 + f32::from(bounds.size.height) / zoom;
                let unit = zoom.max(0.75);
                // Solid, solid highlighted, dashed, dashed highlighted. A
                // route's line goes to the first or third, its end marks to
                // the first or second.
                const NORMAL: usize = 0;
                const ACTIVE: usize = 1;
                const DASHED_OFFSET: usize = 2;
                let mut batches = [
                    Batch::new(LINE_WIDTH),
                    Batch::new(LINE_WIDTH_HIGHLIGHT),
                    Batch::dashed(LINE_WIDTH),
                    Batch::dashed(LINE_WIDTH_HIGHLIGHT),
                ];
                let colors = [theme::text_faint(), theme::accent(), theme::text_faint(), theme::accent()];
                // Rings of crow's foot ends are painted last, over the lines
                // (paths are flushed after the quads of their route).
                let mut rings: Vec<(Point<gpui::Pixels>, f32, gpui::Rgba)> = Vec::new();
                for routed in routes.iter() {
                    let Some(relation) = graph.relations.get(routed.relation) else {
                        continue;
                    };
                    let points = &routed.points;
                    let (Some(&first), Some(&last)) = (points.first(), points.last()) else {
                        continue;
                    };
                    let (min_x, max_x) = points.iter().fold((f32::MAX, f32::MIN), |(lo, hi), p| (lo.min(p.x), hi.max(p.x)));
                    let (min_y, max_y) = points.iter().fold((f32::MAX, f32::MIN), |(lo, hi), p| (lo.min(p.y), hi.max(p.y)));
                    if max_x < x0 || min_x > x1 || max_y < y0 || min_y > y1 {
                        continue;
                    }
                    let is_active = selected_relation == Some(routed.relation)
                        || highlight.is_some_and(|id| relation.child == id || relation.parent == id);
                    let marks = if is_active { ACTIVE } else { NORMAL };
                    let lines = if matches!(relation.origin, RelationOrigin::Logical(_)) { marks + DASHED_OFFSET } else { marks };
                    for (index, p) in points.iter().enumerate() {
                        if index == 0 {
                            batches[lines].lines.move_to(to_window(*p));
                        } else {
                            batches[lines].lines.line_to(to_window(*p));
                        }
                    }
                    let color = colors[marks];
                    match (notation, route_directions(points)) {
                        (RelationNotation::CrowsFoot, Some((child_dir, parent_dir))) => {
                            // Marker size follows `unit` on screen like the
                            // plain dot / arrow do.
                            let scale = unit / zoom;
                            for (mark, tip, direction) in [
                                (EndMark::child(relation), first, child_dir),
                                (EndMark::parent(relation), last, parent_dir),
                            ] {
                                let shape = mark.shape(tip, direction, scale);
                                for (a, b) in shape.lines() {
                                    batches[marks].lines.move_to(to_window(*a));
                                    batches[marks].lines.line_to(to_window(*b));
                                }
                                if let Some((center, radius)) = shape.ring {
                                    rings.push((to_window(center), radius * zoom, color));
                                }
                            }
                        }
                        _ => {
                            // FK end: dot.
                            let radius = px(3.5 * unit);
                            window.paint_quad(gpui::quad(
                                Bounds::centered_at(to_window(first), size(radius * 2.0, radius * 2.0)),
                                radius,
                                color,
                                0.0,
                                gpui::transparent_black(),
                                gpui::BorderStyle::default(),
                            ));
                            // Parent end: arrow head pointing into the box.
                            let before = points[points.len().saturating_sub(2)];
                            let direction = if last.x >= before.x { 1.0 } else { -1.0 };
                            let base = to_window(DiagramPoint { x: last.x - direction * 8.0 * unit / zoom, y: last.y });
                            let half = px(4.0 * unit);
                            batches[marks].arrows.move_to(to_window(last));
                            batches[marks].arrows.line_to(point(base.x, base.y - half));
                            batches[marks].arrows.line_to(point(base.x, base.y + half));
                            batches[marks].arrows.close();
                        }
                    }
                    batches[lines].count += 1;
                    batches[marks].count += usize::from(lines != marks);
                    for slot in [lines, marks] {
                        if batches[slot].full() {
                            batches[slot].flush(window, colors[slot]);
                        }
                    }
                }
                for (batch, color) in batches.iter_mut().zip(colors) {
                    batch.flush(window, color);
                }
                for (center, radius, color) in rings {
                    let radius = px(radius);
                    window.paint_quad(gpui::quad(
                        Bounds::centered_at(center, size(radius * 2.0, radius * 2.0)),
                        radius,
                        theme::background(),
                        px(LINE_WIDTH),
                        color,
                        gpui::BorderStyle::default(),
                    ));
                }
                // The link being dragged: from the column to the pointer.
                if let Some((start, pointer)) = link {
                    let mut preview = PathBuilder::stroke(px(LINE_WIDTH_HIGHLIGHT)).dash_array(&[px(DASH[0]), px(DASH[1])]);
                    preview.move_to(to_window(start));
                    preview.line_to(pointer);
                    if let Ok(path) = preview.build() {
                        window.paint_path(path, theme::accent());
                    }
                }
            },
        )
        .absolute()
        .size_full()
    }

    fn glyph(glyph: TypeGlyph, z: f32) -> AnyElement {
        let slot = || div().w(px(26.0 * z)).flex().items_center().justify_center().flex_shrink_0();
        let text = |label: &'static str, color: gpui::Rgba| {
            slot().text_size(px(9.0 * z)).font_weight(gpui::FontWeight::SEMIBOLD).text_color(color).child(label)
        };
        let icon = |path: &'static str, color: gpui::Rgba| {
            slot().child(Icon::empty().path(path).size(px(11.0 * z)).text_color(color))
        };
        match glyph {
            TypeGlyph::Numeric => text("123", theme::type_number()),
            TypeGlyph::Text => text("A-Z", theme::type_text()),
            TypeGlyph::Binary => text("01", theme::type_object()),
            TypeGlyph::Temporal => icon("icons/calendar-clock.svg", theme::type_object()),
            TypeGlyph::Boolean => icon("icons/check.svg", theme::type_object()),
            TypeGlyph::Json => icon("icons/braces.svg", theme::type_object()),
            TypeGlyph::Other => icon("icons/square-dashed.svg", theme::text_faint()),
        }
        .into_any_element()
    }

    /// Name color of a column: a pending edit is tinted.
    fn column_color(state: ColumnState) -> gpui::Rgba {
        match state {
            ColumnState::Existing => theme::text(),
            ColumnState::Added => theme::success(),
            ColumnState::Changed => theme::warning(),
            ColumnState::Dropped => theme::danger(),
        }
    }

    /// `width` is the box width in diagram units: the name may use all of the row
    /// but the glyph/marker/padding, the type text gets what remains and is
    /// truncated first.
    fn column_row(entity: usize, index: usize, column: &DiagramColumn, width: f32, z: f32, hit: bool, mark: RowMark) -> Div {
        if z < DETAIL_ZOOM {
            // Zoomed out: the name only (a quarter of the elements per row).
            return div()
                .debug_selector(move || format!("diagram-row-{entity}-{index}"))
                .h(px(METRICS.row_height * z))
                .w_full()
                .px(px(6.0 * z))
                .flex()
                .items_center()
                .text_size(px(11.5 * z))
                .text_color(theme::text())
                .when(hit, |this| this.bg(theme::find_match()))
                .when(mark == RowMark::Target, |this| this.bg(theme::accent_surface()))
                .when(column.primary_key, |this| this.font_weight(gpui::FontWeight::BOLD))
                .text_color(Self::column_color(column.pending))
                .when(column.pending == ColumnState::Dropped, |this| this.line_through())
                .truncate()
                .child(column.name.clone());
        }
        let marker = div().w(px(12.0 * z)).flex_shrink_0().flex().items_center().justify_center();
        let marker = if column.primary_key {
            marker.child(
                Icon::empty().path("icons/key-round.svg").size(px(11.0 * z)).text_color(theme::warning()),
            )
        } else if column.foreign_key {
            marker.child(Icon::empty().path("icons/link.svg").size(px(10.0 * z)).text_color(theme::text_faint()))
        } else {
            marker
        };
        div()
            .debug_selector(move || format!("diagram-row-{entity}-{index}"))
            .relative()
            .h(px(METRICS.row_height * z))
            .w_full()
            .flex()
            .items_center()
            .gap(px(4.0 * z))
            .px(px(6.0 * z))
            .text_size(px(11.5 * z))
            .when(hit, |this| this.bg(theme::find_match()))
            .when(mark == RowMark::Target, |this| this.bg(theme::accent_surface()))
            .child(Self::glyph(column.glyph, z))
            .child(
                div()
                    .flex_shrink_0()
                    .max_w(px((width - ROW_CHROME).max(24.0) * z))
                    .truncate()
                    .text_color(Self::column_color(column.pending))
                    .when(column.pending == ColumnState::Dropped, |this| this.line_through())
                    .when(column.primary_key, |this| this.font_weight(gpui::FontWeight::BOLD))
                    .child(column.name.clone()),
            )
            .child(
                div()
                    .flex_1()
                    .min_w_0()
                    .truncate()
                    .text_right()
                    .text_size(px(10.0 * z))
                    .text_color(theme::text_faint())
                    .child(column.data_type.clone()),
            )
            .child(marker)
            // The drag handle of a logical relationship: drag from here onto
            // a column of another table.
            .when(mark == RowMark::Handle, |this| {
                this.child(
                    div()
                        .debug_selector(move || format!("diagram-link-{entity}-{index}"))
                        .absolute()
                        .right(px(2.0 * z))
                        .top(px((METRICS.row_height * z - 10.0 * z) / 2.0))
                        .size(px(10.0 * z))
                        .rounded_full()
                        .border_1()
                        .border_color(theme::accent())
                        .bg(theme::background()),
                )
            })
    }

    /// The "pending change" badge in an entity header.
    fn pending_badge(index: usize, state: TableState, z: f32) -> Div {
        let (label, color) = match state {
            TableState::New => ("NEW", theme::success()),
            TableState::Dropped => ("DROP", theme::danger()),
            _ => ("EDITED", theme::warning()),
        };
        div()
            .debug_selector(move || format!("diagram-pending-{index}"))
            .flex_shrink_0()
            .px(px(4.0 * z))
            .rounded(px(3.0 * z))
            .border_1()
            .border_color(color)
            .text_size(px(8.5 * z))
            .font_weight(gpui::FontWeight::SEMIBOLD)
            .text_color(color)
            .child(label)
    }

    fn entity_element(
        &self,
        index: usize,
        entity: &DiagramEntity,
        rect: DiagramRect,
        related: bool,
        active: bool,
        found: SearchHit,
    ) -> AnyElement {
        let z = self.viewport.zoom;
        let selected = self.selection.contains(&EntityId(index));
        let tint = self.entity_colors.get(index).copied().flatten();
        let link_target = match self.drag {
            Some(Drag::Link { .. }) => self.hover_row.filter(|(id, _)| id.0 == index).map(|(_, row)| row),
            _ => None,
        };
        let handle_row = match self.drag {
            None => self.hover_row.filter(|(id, _)| id.0 == index).map(|(_, row)| row),
            _ => None,
        };
        let border = if found != SearchHit::None {
            theme::warning()
        } else if selected || active {
            theme::accent()
        } else if related {
            theme::accent_soft()
        } else {
            theme::border()
        };
        let (left, top) = (
            (rect.x - self.viewport.origin.x) * z,
            (rect.y - self.viewport.origin.y) * z,
        );
        let columns: Vec<(usize, &DiagramColumn)> = visible_columns(entity, self.mode).collect();
        let pk_rows = columns.iter().take_while(|(_, column)| column.primary_key).count();
        let status_icon = match &entity.state {
            EntityState::Loading => Some(("icons/loader-circle.svg", theme::text_faint())),
            EntityState::Failed(_) => Some(("icons/triangle-alert.svg", theme::danger())),
            EntityState::Ready => None,
        };
        let failure = match &entity.state {
            EntityState::Failed(error) => Some(error.clone()),
            _ => None,
        };
        let header = div()
            .h(px(METRICS.header_height * z))
            .w_full()
            .flex_shrink_0()
            .flex()
            .items_center()
            .gap(px(6.0 * z))
            .px(px(8.0 * z))
            .bg(match found {
                SearchHit::Current => theme::find_match_current(),
                SearchHit::Match => theme::find_match(),
                SearchHit::None => match tint {
                    Some(color) => entity_tint(color),
                    None if entity.focus || selected => theme::accent_surface(),
                    None => theme::grid_header(),
                },
            })
            .border_b_1()
            .border_color(theme::border())
            .child(
                Icon::empty()
                    .path("icons/table-2.svg")
                    .size(px(13.0 * z))
                    .text_color(if entity.focus { theme::accent() } else { theme::text_muted() })
                    .flex_shrink_0(),
            )
            .child(
                div()
                    .flex_1()
                    .min_w_0()
                    .truncate()
                    .text_size(px(12.5 * z))
                    .font_weight(gpui::FontWeight::SEMIBOLD)
                    .text_color(if self.table_state(index) == TableState::Dropped { theme::danger() } else { theme::text() })
                    .when(self.table_state(index) == TableState::Dropped, |this| this.line_through())
                    .child(entity.label.clone()),
            )
            .children(self.table_state(index).pending().then(|| Self::pending_badge(index, self.table_state(index), z)))
            .children(status_icon.map(|(path, color)| {
                let icon = div()
                    .id(("diagram-entity-status", index))
                    .flex_shrink_0()
                    .child(Icon::empty().path(path).size(px(12.0 * z)).text_color(color));
                match failure.clone() {
                    Some(error) => icon
                        .tooltip(move |window, cx| {
                            gpui_component::tooltip::Tooltip::new(error.clone()).build(window, cx)
                        })
                        .into_any_element(),
                    None => icon.into_any_element(),
                }
            }));
        let mut body = div().w_full().flex().flex_col();
        let rows = if z < OVERVIEW_ZOOM { &columns[..0] } else { &columns[..] };
        for (row, (column_index, column)) in rows.iter().enumerate() {
            if row == pk_rows && pk_rows > 0 {
                body = body.child(
                    div().h(px(METRICS.separator_height * z)).w_full().flex().items_center().child(
                        div().h(px(1.0)).w_full().bg(theme::border()),
                    ),
                );
            }
            let hit = !self.search.query.is_empty() && contains_ci(&column.name, &self.search.query);
            let mark = if link_target == Some(*column_index) {
                RowMark::Target
            } else if handle_row == Some(*column_index) {
                RowMark::Handle
            } else {
                RowMark::None
            };
            body = body.child(Self::column_row(index, *column_index, column, rect.width, z, hit, mark));
        }
        div()
            .absolute()
            .left(px(left))
            .top(px(top))
            .w(px(rect.width * z))
            .h(px(rect.height * z))
            .overflow_hidden()
            .rounded(px(4.0 * z))
            .bg(theme::surface())
            .debug_selector(move || format!("diagram-entity-{index}"))
            .child(header)
            .child(body)
            // The outline is an overlay so rows keep the exact offsets the
            // routing assumes (header, rows, separator; no border inset).
            .child(
                div()
                    .absolute()
                    .size_full()
                    .rounded(px(4.0 * z))
                    .border_1()
                    .border_color(border),
            )
            .into_any_element()
    }
}


/// What a column row shows besides its text.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum RowMark {
    None,
    /// The pointer is over the row: the link handle is offered.
    Handle,
    /// A link drag is over the row: it would reference this column.
    Target,
}

/// Header color of a colored entity: the color, translucent over the box.
fn entity_tint(color: Rgb) -> gpui::Rgba {
    let (r, g, b) = color.channels();
    gpui::Rgba { r: f32::from(r) / 255.0, g: f32::from(g) / 255.0, b: f32::from(b) / 255.0, a: 0.38 }
}

/// What a context-menu "Copy JOIN SQL" joins.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(in crate::app) enum JoinTarget {
    /// One relation (index into `graph.relations`).
    Relation(usize),
    /// Every relation of one entity.
    Entity(EntityId),
}

// ---- minimap ---------------------------------------------------------------------

impl DiagramView {
    /// Whether the whole drawing is inside the viewport already.
    pub(in crate::app) fn content_fits(&self) -> bool {
        let size = self.area.get().size;
        let visible = self.viewport.visible_rect(f32::from(size.width), f32::from(size.height));
        self.content_bounds().is_inside(&visible, 1.0)
    }

    /// The minimap's rectangle in area coordinates. `None` when there is
    /// nothing to show, when the whole drawing is in view (an overview of
    /// what is all visible only covers it; a running minimap drag keeps it),
    /// or when the area is too small to spare the corner.
    ///
    /// Where it stays it is drawn translucent (`MINIMAP_IDLE_OPACITY`) until
    /// the pointer is over it, so the corner remains readable behind it.
    pub(in crate::app) fn minimap_rect(&self) -> Option<DiagramRect> {
        #[cfg(test)]
        if !MINIMAP_ENABLED.with(std::cell::Cell::get) {
            return None;
        }
        let size = self.area.get().size;
        let (width, height) = (f32::from(size.width), f32::from(size.height));
        let fits = width >= 2.0 * (MINIMAP_WIDTH + MINIMAP_MARGIN) && height >= 2.0 * (MINIMAP_HEIGHT + MINIMAP_MARGIN);
        let needed = matches!(self.drag, Some(Drag::Minimap(_))) || !self.content_fits();
        (fits && needed && !self.layout.rects.is_empty()).then_some(DiagramRect {
            x: width - MINIMAP_MARGIN - MINIMAP_WIDTH,
            y: height - MINIMAP_MARGIN - MINIMAP_HEIGHT,
            width: MINIMAP_WIDTH,
            height: MINIMAP_HEIGHT,
        })
    }

    /// The mapping the minimap uses now (frozen while it is being dragged).
    fn current_minimap(&self) -> Option<Minimap> {
        if let Some(Drag::Minimap(map)) = self.drag {
            return Some(map);
        }
        self.minimap_rect()?;
        let size = self.area.get().size;
        let viewport = self.viewport.visible_rect(f32::from(size.width), f32::from(size.height));
        Some(Minimap::new(self.content_bounds(), viewport, MINIMAP_WIDTH, MINIMAP_HEIGHT, MINIMAP_PADDING))
    }

    /// The mapping when `local` (area coordinates) is over the minimap.
    fn minimap_hit(&self, local: DiagramPoint) -> Option<Minimap> {
        let rect = self.minimap_rect()?;
        let inside = local.x >= rect.x && local.x < rect.x + rect.width && local.y >= rect.y && local.y < rect.y + rect.height;
        inside.then(|| self.current_minimap()).flatten()
    }

    /// Centres the viewport on the diagram point under `local`.
    fn pan_from_minimap(&mut self, map: Minimap, local: DiagramPoint) {
        let Some(rect) = self.minimap_rect() else {
            return;
        };
        let target = map.to_diagram(DiagramPoint { x: local.x - rect.x, y: local.y - rect.y });
        // The centre stays inside the content, so the view cannot be dragged
        // into empty space.
        let content = self.content_bounds();
        let target = DiagramPoint {
            x: target.x.clamp(content.x, content.x + content.width),
            y: target.y.clamp(content.y, content.y + content.height),
        };
        let size = self.area.get().size;
        self.viewport.center_on(target, f32::from(size.width), f32::from(size.height));
        self.auto_fit = false;
    }

    /// The overview, painted over the entities: boxes, the search hits and
    /// the selection highlighted, and the viewport rectangle.
    fn minimap_canvas(&self) -> Option<impl IntoElement> {
        let map = self.current_minimap()?;
        let rects = self.rects.clone();
        let area_size = self.area.get().size;
        let viewport = self.viewport.visible_rect(f32::from(area_size.width), f32::from(area_size.height));
        let selected = self.selected;
        let hits = self.search.hit.clone();
        let opacity = self.minimap_opacity();
        let faded = move |color: gpui::Rgba| gpui::Rgba { a: color.a * opacity, ..color };
        Some(
            div()
                .debug_selector(|| "diagram-minimap".into())
                .absolute()
                .right(px(MINIMAP_MARGIN))
                .bottom(px(MINIMAP_MARGIN))
                .w(px(MINIMAP_WIDTH))
                .h(px(MINIMAP_HEIGHT))
                .child(
            canvas(
                |_, _, _| (),
                move |bounds, _, window, _| {
                    let to_window = |p: DiagramPoint| {
                        let m = map.to_map(p);
                        bounds.origin + point(px(m.x), px(m.y))
                    };
                    window.paint_quad(gpui::quad(
                        bounds,
                        px(4.0),
                        faded(theme::chrome()),
                        px(1.0),
                        faded(theme::border_strong()),
                        gpui::BorderStyle::default(),
                    ));
                    let scale = map.scale();
                    let fill = |index: usize| {
                        faded(if selected == Some(EntityId(index)) {
                            theme::accent()
                        } else if hits.get(index).copied().unwrap_or(false) {
                            theme::warning()
                        } else {
                            theme::text_faint()
                        })
                    };
                    for (index, rect) in rects.iter().enumerate().filter(|(_, rect)| rect.width > 0.0) {
                        window.paint_quad(gpui::fill(
                            Bounds::new(
                                to_window(DiagramPoint { x: rect.x, y: rect.y }),
                                size((rect.width * scale).max(1.5).into(), (rect.height * scale).max(1.5).into()),
                            ),
                            fill(index),
                        ));
                    }
                    window.paint_quad(gpui::quad(
                        Bounds::new(
                            to_window(DiagramPoint { x: viewport.x, y: viewport.y }),
                            size((viewport.width * scale).into(), (viewport.height * scale).into()),
                        ),
                        px(1.0),
                        gpui::transparent_black(),
                        px(1.5),
                        faded(theme::accent()),
                        gpui::BorderStyle::default(),
                    ));
                },
            )
            .size_full(),
                ),
        )
    }

    /// How opaque the minimap is drawn: translucent until the pointer is
    /// over it (or it is being dragged), so what lies behind stays readable.
    pub(in crate::app) fn minimap_opacity(&self) -> f32 {
        if self.minimap_hover || matches!(self.drag, Some(Drag::Minimap(_))) {
            1.0
        } else {
            MINIMAP_IDLE_OPACITY
        }
    }
}

// ---- search ------------------------------------------------------------------------

impl DiagramView {
    /// Creates the search field on first use.
    fn ensure_search_input(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.search.state.is_some() {
            return;
        }
        let state = cx.new(|cx| InputState::new(window, cx).placeholder("Find table or column"));
        let _subscription = cx.subscribe_in(&state, window, |this, state, event: &InputEvent, _, cx| match event {
            InputEvent::Change => {
                let text = state.read(cx).value().to_string();
                this.set_query(&text, cx);
            }
            InputEvent::PressEnter { .. } => this.next_match(cx),
            InputEvent::Focus | InputEvent::Blur => {}
        });
        self.search.state = Some(SearchInput { state, _subscription });
    }

    /// Re-runs the search after the graph changed.
    fn refresh_search(&mut self) {
        if self.search.query.is_empty() {
            return;
        }
        let entities = self.graph().map_or(0, |graph| graph.entities.len());
        let matches = self.graph().map_or_else(Vec::new, |graph| graph.search(&self.search.query));
        let mut hit = vec![false; entities];
        for id in &matches {
            hit[id.0] = true;
        }
        self.search.current = self.search.current.filter(|&index| index < matches.len());
        self.search.hit = Arc::new(hit);
        self.search.matches = Arc::new(matches);
    }

    /// Typed text: highlights the hits and centres the first one.
    pub(in crate::app) fn set_query(&mut self, text: &str, cx: &mut Context<Self>) {
        let query = text.trim().to_lowercase();
        if query == self.search.query {
            return;
        }
        self.search.query = query;
        self.search.matches = Arc::default();
        self.search.hit = Arc::default();
        self.search.current = None;
        self.refresh_search();
        if let Some(&first) = self.search.matches.first() {
            self.search.current = Some(0);
            self.center_on_entity(first);
        }
        cx.notify();
    }

    /// Enter: the next hit (wrapping), centred.
    pub(in crate::app) fn next_match(&mut self, cx: &mut Context<Self>) {
        let hits = self.search.matches.clone();
        if hits.is_empty() {
            return;
        }
        let next = self.search.current.map_or(0, |current| (current + 1) % hits.len());
        self.search.current = Some(next);
        self.center_on_entity(hits[next]);
        cx.notify();
    }

    /// Pans (and zooms in to a readable scale) so `id` is centred.
    fn center_on_entity(&mut self, id: EntityId) {
        let Some(rect) = self.layout.rects.get(id.0).copied() else {
            return;
        };
        let size = self.area.get().size;
        self.viewport.zoom = self.viewport.zoom.max(DETAIL_ZOOM);
        self.viewport.center_on(rect.center(), f32::from(size.width), f32::from(size.height));
        self.auto_fit = false;
    }

    /// Secondary-F: focuses the search field with its text selected.
    pub(in crate::app) fn focus_search(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.ensure_search_input(window, cx);
        if let Some(input) = &self.search.state {
            input.state.update(cx, |state, cx| {
                state.focus(window, cx);
                state.select_all(window, cx);
            });
        }
    }

    /// Clears the search text and its highlights.
    pub(in crate::app) fn clear_search(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if let Some(input) = &self.search.state {
            input.state.update(cx, |state, cx| state.set_value("", window, cx));
        }
        self.set_query("", cx);
    }

    /// Escape: drops the selection and the search, focus back on the diagram.
    pub(in crate::app) fn clear_selection(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.clear_selection_state();
        self.clear_search(window, cx);
        window.focus(&self.focus_handle, cx);
        cx.notify();
    }

    /// Whether the search field has keyboard focus (tests).
    #[cfg(test)]
    pub(in crate::app) fn search_focused(&self, window: &Window, cx: &gpui::App) -> bool {
        self.search.state.as_ref().is_some_and(|input| input.state.read(cx).focus_handle(cx).is_focused(window))
    }

    /// Whether an entity is at `local` (area coordinates; tests).
    #[cfg(test)]
    pub(in crate::app) fn entity_at_for_test(&self, local: DiagramPoint) -> bool {
        self.entity_at(local).is_some()
    }

    /// The document has been read (or there was none): edits are saved (tests).
    #[cfg(test)]
    pub(in crate::app) fn persisting(&self) -> bool {
        self.persist.as_ref().is_some_and(|persist| self.load_task.is_some() && persist.writable)
    }

    /// Length of the search field's text (tests).
    #[cfg(test)]
    pub(in crate::app) fn search_text_len(&self) -> usize {
        self.search.query.chars().count()
    }

    /// Text of the search field (tests).
    #[cfg(test)]
    pub(in crate::app) fn search_text(&self, cx: &gpui::App) -> String {
        self.search.state.as_ref().map(|input| input.state.read(cx).value().to_string()).unwrap_or_default()
    }

    /// Hit entities and the one the view was last centred on.
    fn search_state_of(&self, id: EntityId) -> SearchHit {
        match self.search.current.and_then(|current| self.search.matches.get(current)) {
            Some(&current) if current == id => SearchHit::Current,
            _ if self.search.hit.get(id.0).copied().unwrap_or(false) => SearchHit::Match,
            _ => SearchHit::None,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum SearchHit {
    None,
    Match,
    Current,
}

// ---- saved document ------------------------------------------------------------------

impl DiagramView {
    /// Tells the view which diagram it shows and where its document is kept
    /// (`None`: nowhere, e.g. tests and scenarios). The saved positions and
    /// annotations are read off the UI thread and applied when they arrive.
    pub(in crate::app) fn set_persistence(
        &mut self,
        target: Option<(Arc<LayoutStore>, DiagramKey)>,
        cx: &mut Context<Self>,
    ) {
        if self.persist.as_ref().map(|p| &p.key) == target.as_ref().map(|(_, key)| key) {
            return;
        }
        self.save_task = None;
        self.load_task = None;
        self.doc_dirty = false;
        // Another diagram: nothing of the previous one carries over.
        if self.persist.is_some() {
            self.reset_annotations();
        }
        let Some((store, key)) = target else {
            self.persist = None;
            return;
        };
        self.persist = Some(Persist { store: store.clone(), key: key.clone(), saved: None, writable: true });
        self.load_task = Some(cx.spawn(async move |this, cx| {
            let wanted = key.clone();
            let result = cx.background_executor().spawn(async move { store.load(&key) }).await;
            let _ = this.update(cx, |this, cx| this.finish_load(&wanted, result, cx));
        }));
    }

    fn finish_load(&mut self, key: &DiagramKey, result: Result<Option<SavedDiagram>, StoreError>, cx: &mut Context<Self>) {
        let Some(persist) = self.persist.as_mut().filter(|persist| persist.key == *key) else {
            return;
        };
        match result {
            Ok(saved) => persist.saved = saved,
            Err(error) => {
                // A newer build's file must survive; a corrupt one is replaced
                // by the next save.
                persist.writable = !matches!(error, StoreError::Newer(_));
                cx.emit(DiagramEvent::PersistFailed(format!("Could not read the saved diagram layout: {error}")));
            }
        }
        // Edits made before the file arrived win over it.
        let annotations = persist.saved.as_ref().map(SavedDiagram::annotations);
        let draft = persist.saved.as_ref().map(SavedDiagram::draft).filter(|draft| !draft.is_empty());
        let persist_marker = persist.saved.as_ref().map(|saved| saved.applying.clone()).unwrap_or_default();
        let untouched = self.session.history.undo_len() == 0;
        let memory = self.layout_memory();
        let mut rebuild = false;
        if let Some(annotations) = annotations.filter(|_| untouched && self.annotations.is_empty()) {
            self.annotations = Arc::new(annotations);
            rebuild = true;
        }
        if let Some(draft) = draft.filter(|_| untouched && self.session.draft.is_empty()) {
            self.session.draft = Arc::new(draft);
            self.editor.revision += 1;
            rebuild = true;
        }
        if untouched && self.applying_marker.is_empty() {
            self.applying_marker = persist_marker;
        }
        if rebuild {
            self.rebuild_effective();
            self.restore_layout(memory);
        }
        self.reconcile(cx);
        let placed = self.apply_saved_positions();
        if !placed {
            self.reflow_all();
        }
        self.refresh_derived();
        self.needs_fit |= self.auto_fit;
        cx.notify();
    }

    /// Moves the entities the user placed to their saved positions (entries
    /// of tables that are gone are ignored) and pins them; the others reflow
    /// around them. `true` when any moved.
    fn apply_saved_positions(&mut self) -> bool {
        let (Some(persist), Some(snapshot)) = (&self.persist, &self.snapshot) else {
            return false;
        };
        let Some(saved) = &persist.saved else {
            return false;
        };
        let applied = saved.apply_entities(&snapshot.graph, &mut self.layout);
        if applied.is_empty() {
            return false;
        }
        self.pinned.resize(self.layout.rects.len(), false);
        for index in applied {
            self.pinned[index] = true;
        }
        self.moved = true;
        self.reflow_all();
        true
    }

    /// Reflows the boxes around the pinned ones.
    fn reflow_all(&mut self) {
        if let Some(graph) = self.snapshot.as_ref().map(|snapshot| snapshot.graph.clone()) {
            let schema = self.kind == DiagramKind::Schema;
            self.layout.reflow(&graph, self.mode, &METRICS, schema, &self.pinned);
        }
    }

    /// Starts (or restarts) the debounce that writes the document.
    fn schedule_save(&mut self, cx: &mut Context<Self>) {
        if !self.persist.as_ref().is_some_and(|persist| persist.writable) {
            return;
        }
        self.save_task = Some(cx.spawn(async move |this, cx| {
            cx.background_executor().timer(SAVE_DEBOUNCE).await;
            let _ = this.update(cx, |this, cx| this.save_document(cx));
        }));
    }

    /// The document as it is now: the positions the user placed and the
    /// annotations, over what was saved (fields of newer builds survive).
    fn document(&self) -> Option<SavedDiagram> {
        let (persist, snapshot) = (self.persist.as_ref()?, self.snapshot.as_ref()?);
        let base = persist
            .saved
            .clone()
            .unwrap_or_else(|| SavedDiagram { key: persist.key.clone(), ..SavedDiagram::default() });
        Some(
            base.with_document(&snapshot.graph, &self.layout, &self.pinned, &self.annotations, &self.session.draft)
                .with_applying(&self.applying_marker),
        )
    }

    /// Snapshots the document (small, on the UI thread) and writes it on the
    /// background executor. A document with nothing left in it removes the file.
    fn save_document(&mut self, cx: &mut Context<Self>) {
        if !std::mem::take(&mut self.doc_dirty) {
            return;
        }
        let Some(diagram) = self.document() else {
            return;
        };
        let Some(persist) = self.persist.as_mut().filter(|persist| persist.writable) else {
            return;
        };
        persist.saved = (!diagram.is_blank()).then(|| diagram.clone());
        let (store, key) = (persist.store.clone(), persist.key.clone());
        cx.spawn(async move |this, cx| {
            let result = cx
                .background_executor()
                .spawn(async move { if diagram.is_blank() { store.forget(&key) } else { store.save(&key, &diagram) } })
                .await;
            if let Err(error) = result {
                let _ = this.update(cx, |_, cx| {
                    cx.emit(DiagramEvent::PersistFailed(format!("Could not save the diagram layout: {error}")));
                });
            }
        })
        .detach();
    }

    /// Writes an edit that the debounce has not saved yet, right now. Only for
    /// the moments the view cannot wait (the tab closes, the app quits); the
    /// file is a few KB and errors have nobody left to read them.
    fn flush_blocking(&mut self) {
        if !std::mem::take(&mut self.doc_dirty) {
            return;
        }
        let Some(diagram) = self.document() else {
            return;
        };
        let Some(persist) = self.persist.as_ref().filter(|persist| persist.writable) else {
            return;
        };
        let _ = if diagram.is_blank() {
            persist.store.forget(&persist.key)
        } else {
            persist.store.save(&persist.key, &diagram)
        };
    }

    /// Writes the document right now after structure changes were applied,
    /// with the tables the database renamed under their new names (the diagram
    /// is rebuilt from this file once the refreshed catalog arrives).
    fn save_after_apply(&mut self, renames: &[(TableKey, TableKey)]) {
        self.save_task = None;
        self.doc_dirty = false;
        let Some(mut diagram) = self.document() else {
            return;
        };
        for (from, to) in renames {
            diagram.rename_table(from, to);
        }
        let Some(persist) = self.persist.as_mut().filter(|persist| persist.writable) else {
            return;
        };
        persist.saved = (!diagram.is_blank()).then(|| diagram.clone());
        let _ = if diagram.is_blank() { persist.store.forget(&persist.key) } else { persist.store.save(&persist.key, &diagram) };
    }

    /// Re-layout: the saved positions go away with the manual layout; the
    /// annotations stay (a document with nothing else left removes the file).
    fn forget_saved_positions(&mut self, cx: &mut Context<Self>) {
        self.save_task = None;
        self.doc_dirty = true;
        self.save_document(cx);
    }
}

impl Drop for DiagramView {
    fn drop(&mut self) {
        self.flush_blocking();
    }
}

// ---- export, JOIN SQL, key actions --------------------------------------------------------

impl DiagramView {
    /// File-name stem for exports: the focus table, else the schema.
    fn export_stem(&self) -> String {
        let Some(graph) = self.graph() else {
            return "diagram".into();
        };
        match graph.entities.iter().find(|entity| entity.focus).or(graph.entities.first()) {
            Some(entity) if entity.focus => format!("{}-diagram", entity.table.table),
            Some(entity) => format!(
                "{}-diagram",
                entity.table.schema.as_deref().filter(|schema| !schema.is_empty()).unwrap_or(&entity.table.database)
            ),
            None => "diagram".into(),
        }
    }

    /// Export menu: hands the AppRoot everything it needs to ask for a file
    /// and write it (off the UI thread).
    pub(in crate::app) fn export(&mut self, format: ExportFormat, cx: &mut Context<Self>) {
        let Some(graph) = self.snapshot.as_ref().map(|snapshot| snapshot.graph.clone()) else {
            return;
        };
        let request = ExportRequest {
            graph,
            layout: self.layout.clone(),
            mode: self.mode,
            notation: self.notation,
            metrics: METRICS,
            annotations: self.annotations.clone(),
        };
        cx.emit(DiagramEvent::Export {
            format,
            file_name: format.file_name(&self.export_stem()),
            request: Box::new(request),
        });
    }

    /// The JOIN statement for `target`, `None` while its columns are unknown.
    pub(in crate::app) fn join_sql(&self, target: JoinTarget) -> Option<String> {
        let graph = self.graph()?;
        match target {
            JoinTarget::Relation(index) => join_sql::relation_join(graph, index, self.dialect),
            JoinTarget::Entity(id) => join_sql::entity_join(graph, id, self.dialect),
        }
    }

    pub(in crate::app) fn copy_join_sql(&mut self, target: JoinTarget, cx: &mut Context<Self>) {
        self.notice = Some(match self.join_sql(target) {
            Some(sql) => {
                cx.write_to_clipboard(gpui::ClipboardItem::new_string(sql));
                ("Copied JOIN SQL".to_string(), false)
            }
            None => ("No known columns to join on".to_string(), true),
        });
        cx.notify();
    }
}

impl DiagramView {
    /// Recomputes `linked` when the highlighted entity or the graph changed.
    fn refresh_linked(&mut self, highlight: Option<EntityId>) {
        let generation = self.snapshot.as_ref().map_or(0, |snapshot| snapshot.generation);
        if self.linked_key == Some((highlight, generation)) {
            return;
        }
        self.linked_key = Some((highlight, generation));
        self.linked.clear();
        let (Some(id), Some(snapshot)) = (highlight, self.snapshot.as_ref()) else {
            return;
        };
        self.linked.resize(snapshot.graph.entities.len(), false);
        for relation in &snapshot.graph.relations {
            if relation.child == id {
                self.linked[relation.parent.0] = true;
            } else if relation.parent == id {
                self.linked[relation.child.0] = true;
            }
        }
    }

    /// Info on the right of the toolbar: table count and, while a schema
    /// diagram is still describing tables, `loaded/total`.
    fn toolbar_status(&self) -> Div {
        let Some(snapshot) = self.snapshot.as_ref().filter(|_| self.kind == DiagramKind::Schema) else {
            return div();
        };
        let tables = snapshot.graph.entities.len();
        div()
            .ml_auto()
            .min_w_0()
            .overflow_hidden()
            .flex()
            .items_center()
            .gap(px(10.0))
            .text_size(px(11.0))
            .text_color(theme::text_muted())
            .children(snapshot.progress.map(|(loaded, total)| {
                div()
                    .debug_selector(|| "diagram-progress".into())
                    .flex()
                    .items_center()
                    .gap(px(4.0))
                    .child(Icon::empty().path("icons/loader-circle.svg").size(px(12.0)))
                    .child(format!("Describing tables {loaded}/{total}"))
            }))
            .child(
                div()
                    .debug_selector(|| "diagram-table-count".into())
                    .child(match snapshot.truncated {
                        Some(all) => format!("{tables} of {all} tables"),
                        None => format!("{tables} {}", if tables == 1 { "table" } else { "tables" }),
                    }),
            )
    }
}

impl Render for DiagramView {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        self.ensure_zoom_input(window, cx);
        let area = self.area.get();
        // A resized area re-fits too while the user has not taken over.
        if self.auto_fit && area.size != self.fitted_size {
            self.needs_fit = true;
        }
        if self.needs_fit && area.size.width > px(1.0) && !self.layout.rects.is_empty() {
            let focus = self
                .graph()
                .and_then(|graph| graph.entities.iter().position(|entity| entity.focus))
                .and_then(|index| self.layout.rects.get(index).copied());
            self.viewport = fit_viewport(
                self.content_bounds(),
                focus,
                f32::from(area.size.width),
                f32::from(area.size.height),
                self.viewport.min_zoom,
            );
            self.needs_fit = false;
            self.fitted_size = area.size;
        }
        self.ensure_search_input(window, cx);
        self.sync_zoom_text(false, window, cx);
        if self.seed_changes && self.pending_count() > 0 {
            // Capture-only: the review dialog, once the sample edits are staged.
            self.seed_changes = false;
            cx.defer_in(window, |this, window, cx| this.open_changes(window, cx));
        }
        let highlight = self.highlighted();
        self.refresh_linked(highlight);
        let mut entities = Vec::new();
        if let Some(graph) = self.snapshot.as_ref().map(|snapshot| snapshot.graph.clone()) {
            // Before the area is measured, assume a typical window.
            let (width, height) = match (f32::from(area.size.width), f32::from(area.size.height)) {
                (w, h) if w > 1.0 && h > 1.0 => (w, h),
                _ => (1600.0, 1000.0),
            };
            for id in self.layout.visible(&self.viewport, width, height).filter(|id| self.groups.shown(id.0)) {
                let (Some(entity), Some(&rect)) = (graph.entities.get(id.0), self.layout.rects.get(id.0)) else {
                    continue;
                };
                entities.push(self.entity_element(
                    id.0,
                    entity,
                    rect,
                    self.linked.get(id.0).copied().unwrap_or(false),
                    highlight == Some(id),
                    self.search_state_of(id),
                ));
            }
        }
        let note = self.snapshot.as_ref().and_then(|snapshot| {
            let schema = self.kind == DiagramKind::Schema;
            if let Some(error) = &snapshot.error {
                let what = if schema { "schema" } else { "table" };
                return Some((format!("Could not load the {what}: {error}"), true));
            }
            let entities = &snapshot.graph.entities;
            if schema {
                return match (entities.is_empty(), snapshot.progress) {
                    (true, None) => Some(("No tables".to_string(), false)),
                    _ if snapshot.graph.relations.is_empty() && snapshot.progress.is_none() => {
                        Some(("No foreign keys in this schema".to_string(), false))
                    }
                    _ => None,
                };
            }
            let alone = entities.len() == 1 && snapshot.graph.relations.is_empty();
            let ready = entities.first().is_some_and(|e| e.state == EntityState::Ready);
            (alone && ready).then(|| ("No foreign keys".to_string(), false))
        });
        let note = self.notice.clone().or(note);
        let minimap = self.minimap_canvas();
        let waiting = self.snapshot.is_none();
        let weak = cx.entity().downgrade();
        let cursor_pan = matches!(self.drag, Some(Drag::Pan { .. }));
        let over_entity = self.hovered.is_some() || matches!(self.drag, Some(Drag::Entity { .. } | Drag::Group { .. }));
        // Before the area is measured, assume a typical window.
        let (width, height) = match (f32::from(area.size.width), f32::from(area.size.height)) {
            (w, h) if w > 1.0 && h > 1.0 => (w, h),
            _ => (1600.0, 1000.0),
        };
        let groups = self.group_elements(width, height);
        let notes = self.note_elements(width, height, cx);
        let marquee = self.marquee_element();
        let relation_bar = self.relation_bar(cx);
        let dock = self.dock(window, cx);
        div()
            .debug_selector(|| "diagram-view".into())
            .key_context("diagram")
            .track_focus(&self.focus_handle)
            .on_action(cx.listener(|this, _: &FocusDiagramSearch, window, cx| this.focus_search(window, cx)))
            .on_action(cx.listener(|this, _: &ClearDiagramSelection, window, cx| this.clear_selection(window, cx)))
            .on_action(cx.listener(|this, _: &DiagramUndo, _, cx| this.undo(cx)))
            .on_action(cx.listener(|this, _: &DiagramRedo, _, cx| this.redo(cx)))
            .size_full()
            .flex()
            .flex_col()
            .bg(theme::background())
            .child(self.toolbar(cx).child(self.toolbar_status()))
            .child(
                div()
                    .flex_1()
                    .min_h_0()
                    .w_full()
                    .flex()
                    .child(
                        div()
                            .id("diagram-surface")
                            .debug_selector(|| "diagram-area".into())
                            .relative()
                            .flex_1()
                            .min_w_0()
                            .h_full()
                            .overflow_hidden()
                            .when(cursor_pan, |this| this.cursor_grabbing())
                            .when(!cursor_pan && over_entity, |this| this.cursor_move())
                            .when(!cursor_pan && !over_entity, |this| this.cursor_grab())
                            .on_mouse_down(MouseButton::Left, cx.listener(|this, event, window, cx| {
                                this.release_zoom_focus(window, cx);
                                this.on_mouse_down(event, window, cx)
                            }))
                            .on_mouse_move(cx.listener(|this, event, _, cx| this.on_mouse_move(event, cx)))
                            .on_mouse_up(MouseButton::Left, cx.listener(|this, event, _, cx| this.on_mouse_up(event, cx)))
                            .on_mouse_up_out(MouseButton::Left, cx.listener(|this, event, _, cx| this.on_mouse_up(event, cx)))
                            .on_scroll(cx.listener(|this, event, _, cx| this.on_scroll(event, cx)))
                            .on_pinch(cx.listener(|this, event, _, cx| this.on_pinch(event, cx)))
                            .children(groups)
                            .child(self.lines_canvas(cx))
                            .children(entities)
                            .children(notes)
                            .children(marquee)
                            .children(relation_bar)
                            .children(minimap)
                            .when(waiting, |this| {
                                this.child(
                                    div()
                                        .debug_selector(|| "diagram-loading".into())
                                        .absolute()
                                        .size_full()
                                        .flex()
                                        .items_center()
                                        .justify_center()
                                        .text_size(px(12.0))
                                        .text_color(theme::text_muted())
                                        .child("Loading diagram..."),
                                )
                            })
                            .children(note.map(|(text, error)| {
                                div()
                                    .debug_selector(|| "diagram-note".into())
                                    .absolute()
                                    .left(px(12.0))
                                    .bottom(px(10.0))
                                    .px(px(8.0))
                                    .py(px(4.0))
                                    .rounded(px(4.0))
                                    .bg(theme::chrome())
                                    .border_1()
                                    .border_color(theme::border())
                                    .text_size(px(11.0))
                                    .text_color(if error { theme::danger() } else { theme::text_muted() })
                                    .child(text)
                            }))
                            .context_menu(move |menu, window, cx| {
                                let Some(view) = weak.upgrade() else {
                                    return menu;
                                };
                                let position = window.mouse_position();
                                let target = view.update(cx, |view, cx| view.context_target(position, cx));
                                Self::build_context_menu(&view, target, menu, window, cx)
                            }),
                    )
                    .children(dock),
            )
    }
}

impl AppRoot {
    /// The rail's Diagram button is offered for SQL table tabs only
    /// (PostgreSQL, SQLite, MySQL, MariaDB, ClickHouse): never Mongo, never a
    /// free-form query or editor tab.
    pub(in crate::app) fn diagram_available(&self, tab_id: u64) -> bool {
        let Some(tab) = self.workspace.tab(tab_id) else {
            return false;
        };
        tab.query_mode.is_sql_table()
            && !tab.collection.is_empty()
            && self
                .saved_connections
                .iter()
                .find(|saved| saved.matches_id(&tab.connection_id))
                .is_some_and(|saved| {
                    matches!(
                        saved.kind,
                        ConnectionKind::Postgres
                            | ConnectionKind::Sqlite
                            | ConnectionKind::Mysql
                            | ConnectionKind::Mariadb
                            | ConnectionKind::Clickhouse
                    )
                })
    }

    pub(in crate::app) fn diagram_presentation_active(&self, tab_id: u64) -> bool {
        self.workspace
            .runtime(tab_id)
            .is_some_and(|runtime| runtime.columns_panel.presentation == Presentation::Diagram)
    }

    /// Rail "Diagram": replaces the grid area with the ER diagram and starts
    /// (or reuses) the background load of the table's relationships.
    pub(in crate::app) fn show_diagram_presentation(&mut self, tab_id: u64, cx: &mut Context<Self>) {
        if !self.diagram_available(tab_id) {
            return;
        }
        if let Some(runtime) = self.workspace.runtime_mut(tab_id) {
            runtime.columns_panel.presentation = Presentation::Diagram;
        }
        self.request_diagram(tab_id, cx);
        cx.notify();
    }

    /// Scenario seeding: the Diagram presentation without a click (no
    /// `cx` is available while the root is being built; the first render
    /// starts the load).
    pub(in crate::app) fn show_diagram_presentation_for_scenario(&mut self, tab_id: u64) {
        if let Some(runtime) = self.workspace.runtime_mut(tab_id) {
            runtime.columns_panel.presentation = Presentation::Diagram;
        }
    }

    /// Whether the sidebar offers "View Diagram" for `id`: database and
    /// schema nodes of SQL connections.
    pub(in crate::app) fn diagram_node_supported(&self, id: &NavigatorNodeId) -> bool {
        let (NavigatorNodeId::Database { connection, .. } | NavigatorNodeId::Schema { connection, .. }) = id
        else {
            return false;
        };
        self.saved_connections
            .iter()
            .find(|saved| saved.matches_id(connection))
            .is_some_and(|saved| {
                matches!(
                    saved.kind,
                    ConnectionKind::Postgres
                        | ConnectionKind::Sqlite
                        | ConnectionKind::Mysql
                        | ConnectionKind::Mariadb
                        | ConnectionKind::Clickhouse
                )
            })
    }

    /// Sidebar / palette "View Diagram": a schema node diagrams that schema;
    /// a database node its only schema, else `public` (else the first) of
    /// the loaded ones, else the engine's default (PostgreSQL `public`; the
    /// schema-less engines name the schema after the database).
    pub(in crate::app) fn open_diagram_for_node(&mut self, id: &NavigatorNodeId, cx: &mut Context<Self>) {
        if !self.diagram_node_supported(id) {
            return;
        }
        match id {
            NavigatorNodeId::Schema { connection, database, schema } => {
                self.open_schema_diagram(connection, database, Some(schema), cx);
            }
            NavigatorNodeId::Database { connection, database } => {
                let schema = self.default_schema_of_database(connection, database);
                self.open_schema_diagram(connection, database, Some(&schema), cx);
            }
            _ => {}
        }
    }

    /// The schema a database node stands for when one is needed: its only loaded schema, else
    /// `public` (else the first) of the loaded ones, else the engine's default (PostgreSQL
    /// `public`; the schema-less engines name the schema after the database).
    pub(in crate::app) fn default_schema_of_database(&self, connection: &str, database: &str) -> String {
        let id = NavigatorNodeId::Database { connection: connection.to_owned(), database: database.to_owned() };
        let mut loaded: Vec<&str> = self
            .navigator
            .loaded_children(&id)
            .into_iter()
            .flatten()
            .filter_map(|child| match child {
                NavigatorNodeId::Schema { schema, .. } => Some(schema.as_str()),
                _ => None,
            })
            .collect();
        loaded.sort_unstable();
        let postgres = self
            .saved_connections
            .iter()
            .find(|saved| saved.matches_id(connection))
            .is_some_and(|saved| saved.kind == ConnectionKind::Postgres);
        match loaded.as_slice() {
            [] if postgres => "public",
            [] => database,
            [only] => only,
            several if several.contains(&"public") => "public",
            [first, ..] => first,
        }
        .to_string()
    }

    /// Palette "View Diagram": the selected sidebar database, schema or
    /// object's schema; with nothing usable selected, the active table tab's.
    pub(in crate::app) fn open_diagram_for_selection(&mut self, cx: &mut Context<Self>) {
        match self.navigator.selected().cloned() {
            Some(id @ (NavigatorNodeId::Database { .. } | NavigatorNodeId::Schema { .. })) => {
                self.open_diagram_for_node(&id, cx);
                return;
            }
            Some(NavigatorNodeId::Object { connection, database, schema, .. }) => {
                let id = NavigatorNodeId::Schema { connection, database, schema };
                if self.diagram_node_supported(&id) {
                    self.open_diagram_for_node(&id, cx);
                    return;
                }
            }
            _ => {}
        }
        let Some(tab) = self.workspace.active_tab().filter(|tab| self.diagram_available(tab.id)) else {
            return;
        };
        let (connection, database, schema) = (tab.connection_id.clone(), tab.database.clone(), tab.schema.clone());
        self.open_schema_diagram(&connection, &database, schema.as_deref(), cx);
    }

    /// Opens (or activates) the whole-schema diagram tab of a database or
    /// schema node; the first render of the tab starts the load.
    pub(in crate::app) fn open_schema_diagram(
        &mut self,
        connection: &str,
        database: &str,
        schema: Option<&str>,
        cx: &mut Context<Self>,
    ) {
        self.workspace.open_or_activate_diagram_tab(connection, database, schema);
        cx.notify();
    }

    /// The diagram presentation of a table tab.
    pub(in crate::app) fn render_diagram(&mut self, tab_id: u64, cx: &mut Context<Self>) -> Div {
        self.render_diagram_kind(tab_id, DiagramKind::Table, cx)
    }

    /// Feeds the latest graph snapshot (table or whole-schema) to the tab's
    /// view entity (created on first use) and renders it.
    pub(in crate::app) fn render_diagram_kind(
        &mut self,
        tab_id: u64,
        kind: DiagramKind,
        cx: &mut Context<Self>,
    ) -> Div {
        // Views of closed tabs are dropped here (a handful of entries).
        let tabs = self.workspace.tabs();
        self.diagram_views.retain(|id, _| tabs.iter().any(|tab| tab.id == *id));
        let snapshot = match kind {
            DiagramKind::Table => {
                self.request_diagram(tab_id, cx);
                self.diagram_graph(tab_id)
            }
            DiagramKind::Schema => {
                self.request_schema_diagram(tab_id, cx);
                self.schema_diagram_graph(tab_id)
            }
        };
        let view = match self.diagram_views.get(&tab_id) {
            Some(view) => view.clone(),
            None => {
                let view = cx.new(|cx| DiagramView::new(kind, cx));
                cx.subscribe(&view, move |this: &mut Self, _, event: &DiagramEvent, cx| match event {
                    DiagramEvent::OpenTable(table) => this.open_diagram_table(tab_id, table, cx),
                    DiagramEvent::Refresh => match kind {
                        DiagramKind::Table => this.refresh_diagram(tab_id, cx),
                        DiagramKind::Schema => this.refresh_schema_diagram(tab_id, cx),
                    },
                    DiagramEvent::Export { format, file_name, request } => {
                        this.export_diagram(tab_id, *format, file_name.clone(), (**request).clone(), cx)
                    }
                    DiagramEvent::PersistFailed(message) => {
                        let connection = this.workspace.tab(tab_id).map(|tab| tab.connection_id.clone());
                        this.report_warning(Area::Diagram, connection.as_deref(), message);
                    }
                    DiagramEvent::Apply(_) | DiagramEvent::CancelApply => this.handle_diagram_apply(tab_id, kind, event, cx),
                })
                .detach();
                self.diagram_views.insert(tab_id, view.clone());
                // Capture-only seeding of the scenario binary.
                if self.scenario_ui.service.is_some() {
                    if let Ok(spec) = std::env::var("TABLEX_SEED_DIAGRAM") {
                        view.update(cx, |view, cx| view.seed_for_scenario(&spec, cx));
                    }
                }
                view
            }
        };
        let identity = self.workspace.tab(tab_id).map(|tab| {
            let key = DiagramKey {
                connection: tab.connection_id.clone(),
                database: tab.database.clone(),
                schema: tab.schema.clone().filter(|schema| !schema.is_empty()),
                table: (kind == DiagramKind::Table).then(|| tab.collection.clone()),
            };
            (key, tab.connection_id.clone())
        });
        let dialect = identity
            .as_ref()
            .and_then(|(_, connection)| self.sql_dialect_for_connection(connection))
            .unwrap_or(SqlDialect::POSTGRES);
        let edit = identity.as_ref().and_then(|(key, connection)| {
            self.structure_edit_context(connection, &key.database, key.schema.as_deref())
        });
        let persistence = self.diagram_layouts.clone().zip(identity.map(|(key, _)| key));
        view.update(cx, |view, cx| {
            view.dialect = dialect;
            view.set_edit_context(edit, cx);
            view.set_persistence(persistence, cx);
            view.set_snapshot(snapshot, cx)
        });
        div().size_full().child(view)
    }

    /// Export menu: asks for a file, then renders and writes the diagram off
    /// the UI thread. The result is shown in the diagram and, on failure,
    /// logged.
    pub(in crate::app) fn export_diagram(
        &mut self,
        tab_id: u64,
        format: ExportFormat,
        file_name: String,
        request: ExportRequest,
        cx: &mut Context<Self>,
    ) {
        let directory = crate::platform::save_dialog_dir();
        let prompt = cx.prompt_for_new_path(&directory, Some(&file_name));
        let connection = self.workspace.tab(tab_id).map(|tab| tab.connection_id.clone());
        cx.spawn(async move |this, cx| {
            let path = match prompt.await {
                Ok(Ok(Some(path))) => path,
                Ok(Ok(None)) => return,
                _ => {
                    let _ = this.update(cx, |this, cx| {
                        this.show_diagram_notice(tab_id, "Could not open the save dialog".into(), true, cx)
                    });
                    return;
                }
            };
            let shown = path.display().to_string();
            let result = cx
                .background_executor()
                .spawn(async move { request.write(format, &path) })
                .await;
            let _ = this.update(cx, |this, cx| match result {
                Ok(()) => this.show_diagram_notice(tab_id, format!("Exported {} to {shown}", format.label()), false, cx),
                Err(error) => {
                    let message = format!("Could not export the diagram to {shown}: {error}");
                    this.report_error(Area::Export, connection.as_deref(), &message);
                    this.show_diagram_notice(tab_id, message, true, cx);
                }
            });
        })
        .detach();
    }

    fn show_diagram_notice(&mut self, tab_id: u64, text: String, error: bool, cx: &mut Context<Self>) {
        if let Some(view) = self.diagram_views.get(&tab_id) {
            view.update(cx, |view, cx| {
                view.notice = Some((text, error));
                cx.notify();
            });
        }
    }

    /// Cmd/Ctrl+F while the tab shows a diagram: focus its search field.
    /// `false` when the tab shows no diagram.
    pub(in crate::app) fn focus_diagram_search(&mut self, tab_id: u64, window: &mut Window, cx: &mut Context<Self>) -> bool {
        let shown = self
            .workspace
            .tab(tab_id)
            .is_some_and(|tab| tab.kind == crate::workspace::TabKind::Diagram)
            || self.diagram_presentation_active(tab_id);
        let Some(view) = self.diagram_views.get(&tab_id).filter(|_| shown) else {
            return false;
        };
        view.update(cx, |view, cx| view.focus_search(window, cx));
        true
    }

    /// Double-click on a related entity: opens (or activates) that table via
    /// the same helper the sidebar and the palette use.
    fn open_diagram_table(&mut self, tab_id: u64, table: &TableRef, cx: &mut Context<Self>) {
        let Some(tab) = self.workspace.tab(tab_id) else {
            return;
        };
        let connection = tab.connection_id.clone();
        let schema = table.schema.clone().or_else(|| tab.schema.clone()).unwrap_or_default();
        let database = if table.database.is_empty() { tab.database.clone() } else { table.database.clone() };
        self.open_sql_object_tab(&table.table, &connection, &database, &schema, &table.table, cx);
    }
}
