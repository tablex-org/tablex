use crate::grid::CellPosition;
use crate::mongo_result::MongoTreeRow;
use std::borrow::Cow;
use std::collections::HashSet;
use std::sync::Arc;

pub const MAX_VALUE_PREVIEW_LENGTH: usize = 12_000;
pub const MAX_JSON_FORMAT_LENGTH: usize = 50_000;
pub const DEFAULT_RIGHT_WIDTH: f32 = 320.0;
pub const DEFAULT_BOTTOM_HEIGHT: f32 = 180.0;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CellDetailPlacement {
    Right,
    Bottom,
    /// Floating, non-modal panel over the results area.
    Popup,
}

/// Below this rendered panel width the tab strip drops labels (icon-only).
pub const COMPACT_TABS_BELOW: f32 = 300.0;

pub fn tabs_are_compact(rendered_width: f32) -> bool {
    rendered_width < COMPACT_TABS_BELOW
}

pub const POPUP_DEFAULT_SIZE: (f32, f32) = (520.0, 420.0);
pub const POPUP_MIN_SIZE: (f32, f32) = (320.0, 240.0);

/// Window-space rectangle the popup must stay inside (the grid area, never the
/// rails or footer): `(left, top, right, bottom)`.
pub type PopupArea = (f32, f32, f32, f32);

/// Geometry of the floating panel in window coordinates. `position` is
/// `None` until the popup is first shown (a default is derived from the
/// window size then) or after it was never dragged.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct PopupGeometry {
    pub position: Option<(f32, f32)>,
    pub size: (f32, f32),
}

impl Default for PopupGeometry {
    fn default() -> Self {
        Self {
            position: None,
            size: POPUP_DEFAULT_SIZE,
        }
    }
}

impl PopupGeometry {
    /// Keeps the whole popup inside `area`. A popup larger than the area
    /// shrinks to fit. [`POPUP_MIN_SIZE`] only applies while the area is at
    /// least that big: in a smaller area the popup yields and takes the whole
    /// area, so it never spills over the footer or rails.
    pub fn clamped(mut self, area: PopupArea) -> Self {
        let (left, top, right, bottom) = area;
        let (width, height) = ((right - left).max(0.0), (bottom - top).max(0.0));
        self.size.0 = self.size.0.clamp(POPUP_MIN_SIZE.0.min(width), width);
        self.size.1 = self.size.1.clamp(POPUP_MIN_SIZE.1.min(height), height);
        if let Some((x, y)) = self.position {
            self.position = Some((
                x.clamp(left, (right - self.size.0).max(left)),
                y.clamp(top, (bottom - self.size.1).max(top)),
            ));
        }
        self
    }

    /// Where the popup sits before the user moves it: near the top-right of
    /// `area`.
    pub fn resolved_position(&self, area: PopupArea) -> (f32, f32) {
        let (left, top, right, bottom) = area;
        self.position.unwrap_or((
            (right - self.size.0 - 16.0).max(left),
            (top + 16.0).min((bottom - self.size.1).max(top)),
        ))
    }

    /// Like [`Self::resolved_position`], but an unmoved popup is placed on
    /// whichever side of `area` overlaps the selected cell (`avoid` = its
    /// window x range) the least, so it covers the selection whenever the area
    /// leaves any alternative.
    pub fn resolved_position_avoiding(
        &self,
        area: PopupArea,
        avoid: Option<(f32, f32)>,
    ) -> (f32, f32) {
        let Some((cell_left, cell_right)) = avoid.filter(|_| self.position.is_none()) else {
            return self.resolved_position(area);
        };
        let (left_edge, _, right_edge, _) = area;
        let (_, y) = self.resolved_position(area);
        let margin = 16.0;
        let max_x = (right_edge - self.size.0).max(left_edge);
        let right = (right_edge - self.size.0 - margin).clamp(left_edge, max_x);
        let left = (left_edge + margin).clamp(left_edge, max_x);
        let overlap = |x: f32| ((x + self.size.0).min(cell_right) - x.max(cell_left)).max(0.0);
        // Ties (both clear or equally covered) prefer the right side.
        let x = if overlap(left) < overlap(right) {
            left
        } else {
            right
        };
        (x, y)
    }
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub enum PopupDragKind {
    Move,
    Resize,
}

/// An in-progress popup drag (transient; never persisted meaningfully).
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct PopupDrag {
    pub kind: PopupDragKind,
    pub start_mouse: (f32, f32),
    pub start: PopupGeometry,
    pub start_position: (f32, f32),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CellDetailTab {
    Details,
    Column,
    Row,
}

/// How the Row tab presents the selected row.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum RowView {
    #[default]
    Tree,
    Json,
}

/// What the value panel knows about a binary cell (BLOB, bytea, BINARY): its
/// size and, for an image, the decoded-by-GPUI picture. The bytes themselves
/// are read from the page when drawn (nothing is copied per selection unless
/// the cell holds an image or a staged edit).
#[derive(Debug, Clone)]
pub struct BinaryDetail {
    /// Size of the value in bytes.
    pub len: usize,
    /// Bytes the panel owns: those of a staged edit (`0x…` typed or loaded from
    /// a file) or decoded from a PostgreSQL `bytea` text; `None` reads the page.
    pub owned: Option<Arc<[u8]>>,
    /// How the value is written as text (`0x…`, or PostgreSQL's `\x…`).
    pub form: crate::binary_value::HexForm,
    pub image: Option<crate::binary_value::ImageKind>,
    pub dimensions: Option<(u32, u32)>,
    /// The picture, when the bytes are an image within the preview cap.
    pub picture: Option<Arc<gpui::Image>>,
}

impl BinaryDetail {
    /// True when the value is within the preview cap.
    pub fn previewable(&self) -> bool {
        self.len <= crate::binary_value::PREVIEW_CAP_BYTES
    }

    /// True when the value may be edited as `0x…` text.
    pub fn text_editable(&self) -> bool {
        self.len <= crate::binary_value::TEXT_EDIT_CAP_BYTES
    }
}

impl PartialEq for BinaryDetail {
    fn eq(&self, other: &Self) -> bool {
        let same_picture = match (&self.picture, &other.picture) {
            (Some(a), Some(b)) => Arc::ptr_eq(a, b),
            (None, None) => true,
            _ => false,
        };
        self.len == other.len
            && self.owned == other.owned
            && self.form == other.form
            && self.image == other.image
            && self.dimensions == other.dimensions
            && same_picture
    }
}

impl Eq for BinaryDetail {}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CellDetail {
    pub position: CellPosition,
    pub source_column: usize,
    pub column_name: String,
    pub row_number: usize,
    pub type_name: String,
    pub comment: String,
    pub value: Arc<str>,
    pub value_preview: String,
    pub value_preview_truncated: bool,
    pub is_null: bool,
    pub nullable: bool,
    pub length: usize,
    pub formatted_json: Option<String>,
    /// JSON-looking text above [`MAX_JSON_FORMAT_LENGTH`]: shown as stored,
    /// not formatted (the pane says so).
    pub json_unformatted: bool,
    pub editable: bool,
    /// Why the value cannot be edited, when the reason is the value itself.
    pub read_only_reason: Option<&'static str>,
    /// The storage class of this cell in a SQLite column of several
    /// (`INTEGER`, `TEXT`, ...): `1` and `'1'` read the same in the grid.
    pub cell_class: Option<&'static str>,
    /// Set for a binary value: the panel then offers Hex, Text and Image views.
    pub binary: Option<BinaryDetail>,
    /// Set when the column holds binary values (also on a NULL cell, which can
    /// be given a value from a file): how they are written as text.
    pub binary_form: Option<crate::binary_value::HexForm>,
}

impl CellDetail {
    /// Inert stand-in for the panel chrome when nothing is selected; never
    /// shown (the tab bodies render an empty state instead).
    pub fn placeholder() -> Self {
        Self {
            position: CellPosition { row: 0, column: 0 },
            source_column: 0,
            column_name: String::new(),
            row_number: 0,
            type_name: String::new(),
            comment: String::new(),
            value: Arc::from(""),
            value_preview: String::new(),
            value_preview_truncated: false,
            is_null: false,
            nullable: false,
            length: 0,
            formatted_json: None,
            json_unformatted: false,
            editable: false,
            read_only_reason: None,
            cell_class: None,
            binary: None,
            binary_form: None,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CellDetailRowField {
    pub name: String,
    pub type_name: String,
    pub value: String,
    pub is_null: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CellDetailRow {
    pub row_number: usize,
    pub fields: Vec<CellDetailRowField>,
    pub json: Arc<str>,
    /// Structured form of the row (Mongo Extended JSON or SQL values with
    /// JSON columns parsed) used to build the Row tab tree lazily.
    pub document: Arc<serde_json::Value>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CellDetailColumn {
    pub source_column: usize,
    pub name: String,
    pub type_name: String,
    pub nullable: Option<bool>,
    pub primary_key: Option<bool>,
    pub is_generated: Option<bool>,
    pub auto_increment: Option<bool>,
    pub default_expression: Option<String>,
}

impl CellDetailColumn {
    /// Rows of the Column tab. Only known properties are listed: engines that
    /// do not report a property (Mongo has no nullability, generated or
    /// auto-increment columns) omit its row instead of printing "Unknown".
    pub fn property_rows(&self) -> Vec<(&'static str, String)> {
        let yes_no =
            |value: Option<bool>| value.map(|value| if value { "Yes" } else { "No" }.to_string());
        [
            Some(("Name", self.name.clone())),
            Some(("Type", self.type_name.clone())),
            yes_no(self.nullable).map(|value| ("Nullable", value)),
            yes_no(self.primary_key).map(|value| ("Primary key", value)),
            yes_no(self.is_generated).map(|value| ("Generated", value)),
            yes_no(self.auto_increment).map(|value| ("Auto increment", value)),
            self.default_expression
                .clone()
                .map(|value| ("Default", value)),
        ]
        .into_iter()
        .flatten()
        .collect()
    }
}

#[derive(Debug, Clone)]
pub struct CellDetailState {
    pub open: bool,
    pub placement: CellDetailPlacement,
    pub tab: CellDetailTab,
    pub right_width: f32,
    pub bottom_height: f32,
    pub target: Option<CellPosition>,
    pub detail: Option<CellDetail>,
    pub opened_tabs: Vec<CellDetailTab>,
    pub row: Option<CellDetailRow>,
    pub column: Option<CellDetailColumn>,
    /// Expanded node paths of the Row tab tree; reset when the row changes.
    pub row_expanded: HashSet<String>,
    /// Flattened visible rows of the Row tab tree. Only populated while the
    /// Row tab is open, and recomputed only when the row or expansion changes.
    pub row_tree: Arc<Vec<MongoTreeRow>>,
    /// Tree or JSON presentation of the Row tab; kept across row changes.
    pub row_view: RowView,
    /// Shell-syntax / canonical Extended JSON in the Row tab's JSON view (Mongo).
    pub row_json_mode: crate::cell_value::EjsonMode,
    pub popup: PopupGeometry,
    pub popup_drag: Option<PopupDrag>,
}

impl Default for CellDetailState {
    fn default() -> Self {
        Self {
            open: false,
            placement: CellDetailPlacement::Right,
            tab: CellDetailTab::Details,
            right_width: DEFAULT_RIGHT_WIDTH,
            bottom_height: DEFAULT_BOTTOM_HEIGHT,
            target: None,
            detail: None,
            opened_tabs: Vec::new(),
            row: None,
            column: None,
            row_expanded: HashSet::new(),
            row_tree: Arc::default(),
            row_view: RowView::default(),
            row_json_mode: Default::default(),
            popup: PopupGeometry::default(),
            popup_drag: None,
        }
    }
}

impl CellDetailState {
    pub fn show(&mut self, target: CellPosition) {
        if self.target.map(|previous| previous.row) != Some(target.row) {
            self.row_expanded.clear();
        }
        self.target = Some(target);
        self.open = !self.opened_tabs.is_empty();
        self.detail = None;
        self.column = None;
        self.set_row(None);
    }

    pub fn open_tab(&mut self, tab: CellDetailTab) {
        if !self.opened_tabs.contains(&tab) {
            self.opened_tabs.push(tab);
        }
        self.tab = tab;
        self.open = true;
        self.rebuild_row_views();
    }

    /// Closes one tab: an active tab hands focus to
    /// its neighbor, and closing the last tab closes the whole panel.
    pub fn close_tab(&mut self, tab: CellDetailTab) {
        let Some(index) = self.opened_tabs.iter().position(|opened| *opened == tab) else {
            return;
        };
        self.opened_tabs.remove(index);
        if self.opened_tabs.is_empty() {
            self.close();
            return;
        }
        if self.tab == tab {
            self.tab = self.opened_tabs[index.min(self.opened_tabs.len() - 1)];
        }
        self.rebuild_row_views();
    }

    /// "Close Others": keeps only `tab`, which becomes the active tab.
    pub fn close_other_tabs(&mut self, tab: CellDetailTab) {
        if !self.opened_tabs.contains(&tab) {
            return;
        }
        self.opened_tabs.retain(|opened| *opened == tab);
        self.tab = tab;
        self.rebuild_row_views();
    }

    pub fn close(&mut self) {
        self.open = false;
        self.target = None;
        self.tab = CellDetailTab::Details;
        self.detail = None;
        self.opened_tabs.clear();
        self.column = None;
        self.row_expanded.clear();
        self.set_row(None);
    }

    /// Replaces the inspected row and rebuilds the derived Row/JSON views.
    /// Expansion state is kept (it is reset by [`Self::show`] when the target
    /// row changes) so data refreshes of the same row do not collapse the tree.
    pub fn set_row(&mut self, row: Option<CellDetailRow>) {
        self.row = row;
        self.rebuild_row_views();
    }

    /// Switches the Row tab between tree and JSON; the tree is only kept
    /// flattened while it is the visible view.
    pub fn set_row_view(&mut self, view: RowView) {
        if self.row_view != view {
            self.row_view = view;
            self.rebuild_row_tree();
        }
    }

    pub fn toggle_row_path(&mut self, path: &str) {
        if !self.row_expanded.remove(path) {
            self.row_expanded.insert(path.to_string());
        }
        self.rebuild_row_tree();
    }

    fn rebuild_row_views(&mut self) {
        self.rebuild_row_tree();
    }

    fn rebuild_row_tree(&mut self) {
        self.row_tree = match &self.row {
            Some(row)
                if self.row_view == RowView::Tree
                    && self.opened_tabs.contains(&CellDetailTab::Row) =>
            {
                let mut rows = crate::mongo_result::flatten_tree(&row.document, &self.row_expanded);
                for tree_row in rows.iter_mut().filter(|tree_row| tree_row.depth == 0) {
                    if let Some(field) = row.fields.iter().find(|field| field.name == tree_row.key)
                    {
                        tree_row.type_name = Cow::Owned(field.type_name.clone());
                    }
                }
                Arc::new(rows)
            }
            _ => Arc::default(),
        };
    }

    #[cfg(test)]
    /// Right -> Bottom -> Popup -> Right.
    pub fn cycle_placement(&mut self) {
        self.placement = match self.placement {
            CellDetailPlacement::Right => CellDetailPlacement::Bottom,
            CellDetailPlacement::Bottom => CellDetailPlacement::Popup,
            CellDetailPlacement::Popup => CellDetailPlacement::Right,
        };
        self.popup_drag = None;
    }

    pub fn set_placement(&mut self, placement: CellDetailPlacement) {
        self.placement = placement;
        self.popup_drag = None;
    }

    pub fn begin_popup_drag(
        &mut self,
        kind: PopupDragKind,
        mouse: (f32, f32),
        area: PopupArea,
        shown_at: (f32, f32),
    ) {
        let start = self.popup.clamped(area);
        self.popup_drag = Some(PopupDrag {
            kind,
            start_mouse: mouse,
            start,
            start_position: shown_at,
        });
    }

    /// Applies a mouse move to the active drag, clamped to the window.
    /// Returns whether a drag was active.
    pub fn update_popup_drag(&mut self, mouse: (f32, f32), area: PopupArea) -> bool {
        let Some(drag) = self.popup_drag else {
            return false;
        };
        let (dx, dy) = (mouse.0 - drag.start_mouse.0, mouse.1 - drag.start_mouse.1);
        let mut geometry = drag.start;
        match drag.kind {
            PopupDragKind::Move => {
                geometry.position = Some((drag.start_position.0 + dx, drag.start_position.1 + dy));
            }
            PopupDragKind::Resize => {
                geometry.position = Some(drag.start_position);
                geometry.size = (drag.start.size.0 + dx, drag.start.size.1 + dy);
            }
        }
        self.popup = geometry.clamped(area);
        true
    }

    pub fn end_popup_drag(&mut self) {
        self.popup_drag = None;
    }

    pub fn panel_size(&self) -> f32 {
        match self.placement {
            CellDetailPlacement::Right | CellDetailPlacement::Popup => self.right_width,
            CellDetailPlacement::Bottom => self.bottom_height,
        }
    }

    pub fn set_panel_size(&mut self, size: f32) {
        match self.placement {
            CellDetailPlacement::Right => self.right_width = size,
            CellDetailPlacement::Bottom => self.bottom_height = size,
            // The popup has its own geometry; docked sizes are untouched.
            CellDetailPlacement::Popup => {}
        }
    }
}

pub fn value_length(value: &str) -> usize {
    value.encode_utf16().count()
}

pub fn preview_value(value: &str) -> (Cow<'_, str>, bool) {
    if value_length(value) <= MAX_VALUE_PREVIEW_LENGTH {
        return (Cow::Borrowed(value), false);
    }

    let mut units = 0;
    let mut end = 0;
    for (index, character) in value.char_indices() {
        let next_units = units + character.len_utf16();
        if next_units > MAX_VALUE_PREVIEW_LENGTH {
            break;
        }
        units = next_units;
        end = index + character.len_utf8();
    }

    let mut preview = String::with_capacity(end + '…'.len_utf8());
    preview.push_str(&value[..end]);
    preview.push('…');
    (Cow::Owned(preview), true)
}

/// Whether `value` of a column of `type_name` is JSON by type or by shape.
fn looks_like_json(value: &str, type_name: &str) -> bool {
    let base_type = type_name
        .trim()
        .to_ascii_lowercase()
        .split([':', '(', ' ', '\t'])
        .next()
        .unwrap_or_default()
        .to_string();
    let trimmed = value.trim_start();
    base_type == "json"
        || base_type == "jsonb"
        || trimmed.starts_with('{')
        || trimmed.starts_with('[')
}

/// `value` re-indented when it is JSON, every token as stored (numbers such as
/// `1.10` or `12345678901234567890` and the strings are copied byte for byte).
pub fn format_json_value(value: &str, type_name: &str) -> Option<String> {
    if value_length(value) > MAX_JSON_FORMAT_LENGTH || !looks_like_json(value, type_name) {
        return None;
    }
    crate::json_text::pretty(value)
}

#[allow(clippy::too_many_arguments)] // each parameter is one input of the operation; a parameter struct would only rename the list
pub fn build_cell_detail(
    position: CellPosition,
    source_column: usize,
    column_name: impl Into<String>,
    type_name: impl Into<String>,
    value: impl Into<String>,
    is_null: bool,
    nullable: bool,
    editable: bool,
    comment: impl Into<String>,
) -> CellDetail {
    let value: Arc<str> = Arc::from(value.into());
    let type_name = type_name.into();
    let (value_preview, value_preview_truncated) = preview_value(&value);
    CellDetail {
        position,
        source_column,
        column_name: column_name.into(),
        row_number: position.row + 1,
        type_name: type_name.clone(),
        comment: comment.into(),
        value: value.clone(),
        value_preview: value_preview.into_owned(),
        value_preview_truncated,
        is_null,
        nullable,
        length: if is_null { 0 } else { value_length(&value) },
        formatted_json: format_json_value(&value, &type_name),
        json_unformatted: !is_null
            && value_length(&value) > MAX_JSON_FORMAT_LENGTH
            && looks_like_json(&value, &type_name),
        editable,
        read_only_reason: None,
        cell_class: None,
        binary: None,
        binary_form: None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn value_length_counts_utf16_code_units() {
        assert_eq!(value_length("😀"), 2);
        assert_eq!(value_length("PAY"), 3);
    }

    #[test]
    fn preview_truncates_at_a_character_boundary() {
        let value = "😀".repeat(MAX_VALUE_PREVIEW_LENGTH);
        let (preview, truncated) = preview_value(&value);

        assert!(truncated);
        assert!(preview.ends_with('…'));
        assert!(preview.is_char_boundary(preview.len() - '…'.len_utf8()));
        assert!(value_length(&preview) <= MAX_VALUE_PREVIEW_LENGTH + 1);
    }

    #[test]
    fn formats_json_objects_without_formatting_plain_text() {
        assert_eq!(
            format_json_value(r#"{"active":true}"#, "json").as_deref(),
            Some("{\n  \"active\": true\n}")
        );
        assert!(format_json_value("PAY", "text").is_none());
    }

    #[test]
    fn cell_detail_state_defaults_to_closed_right_details() {
        let state = CellDetailState::default();

        assert!(!state.open);
        assert_eq!(state.placement, CellDetailPlacement::Right);
        assert_eq!(state.tab, CellDetailTab::Details);
        assert!(state.opened_tabs.is_empty());
        assert!(state.target.is_none());
        assert!(state.detail.is_none());
    }

    #[test]
    fn opens_each_panel_tab_once_and_keeps_active_tab_when_selection_changes() {
        let mut state = CellDetailState::default();
        let first = CellPosition { row: 0, column: 0 };
        let second = CellPosition { row: 1, column: 0 };

        state.show(first);
        assert!(!state.open);
        assert!(state.opened_tabs.is_empty());
        state.open_tab(CellDetailTab::Details);
        state.open_tab(CellDetailTab::Row);
        state.open_tab(CellDetailTab::Column);
        state.open_tab(CellDetailTab::Column);

        assert_eq!(state.opened_tabs.len(), 3);
        assert_eq!(state.tab, CellDetailTab::Column);

        state.show(second);

        assert_eq!(state.target, Some(second));
        assert_eq!(state.tab, CellDetailTab::Column);
        assert_eq!(state.opened_tabs.len(), 3);
    }

    #[test]
    fn selection_refresh_does_not_reopen_a_closed_panel() {
        let mut state = CellDetailState::default();
        state.open_tab(CellDetailTab::Details);
        state.show(CellPosition { row: 0, column: 0 });
        state.close();

        state.show(CellPosition { row: 1, column: 0 });

        assert!(!state.open);
        assert!(state.opened_tabs.is_empty());
        assert_eq!(state.target, Some(CellPosition { row: 1, column: 0 }));
    }

    fn sample_row(row_number: usize) -> CellDetailRow {
        let document = serde_json::json!({"a": {"b": 1}, "list": [1, 2]});
        CellDetailRow {
            row_number,
            fields: vec![CellDetailRowField {
                name: "a".into(),
                type_name: "jsonb".into(),
                value: "{1 fields}".into(),
                is_null: false,
            }],
            json: serde_json::to_string_pretty(&document).unwrap().into(),
            document: Arc::new(document),
        }
    }

    #[test]
    fn row_tree_is_built_only_while_the_row_tab_is_open() {
        let mut state = CellDetailState::default();
        state.open_tab(CellDetailTab::Details);
        state.show(CellPosition { row: 0, column: 0 });
        state.set_row(Some(sample_row(1)));
        assert!(state.row_tree.is_empty());

        state.open_tab(CellDetailTab::Row);
        assert_eq!(state.row_tree.len(), 2);
        assert_eq!(state.row_tree[0].type_name, "jsonb");
    }

    #[test]
    fn placement_cycles_right_bottom_popup_and_survives_closing() {
        let mut state = CellDetailState::default();
        assert_eq!(state.placement, CellDetailPlacement::Right);
        for expected in [
            CellDetailPlacement::Bottom,
            CellDetailPlacement::Popup,
            CellDetailPlacement::Right,
        ] {
            state.cycle_placement();
            assert_eq!(state.placement, expected);
        }
        state.set_placement(CellDetailPlacement::Popup);
        state.open_tab(CellDetailTab::Details);
        state.close();
        assert_eq!(
            state.placement,
            CellDetailPlacement::Popup,
            "placement persists"
        );
        // Docked sizes are not disturbed by popup resizes.
        let right = state.right_width;
        state.set_panel_size(999.0);
        assert_eq!(state.right_width, right);
    }

    #[test]
    fn unmoved_popup_avoids_the_selected_cell_and_stays_in_the_area() {
        let geometry = PopupGeometry::default();
        let area = (300.0, 200.0, 1060.0, 800.0);
        let right_x = area.2 - geometry.size.0 - 16.0;
        // Selection at the far left: the right side is clear.
        let (x, y) = geometry.resolved_position_avoiding(area, Some((320.0, 400.0)));
        assert_eq!(x, right_x);
        assert!(y >= area.1, "never above the results area");
        // Selection at the far right: the left side is clear.
        let (x, _) = geometry.resolved_position_avoiding(area, Some((980.0, 1050.0)));
        assert_eq!(x, area.0 + 16.0);
        // Selection near the middle covers either side: least overlap wins.
        let (x, _) = geometry.resolved_position_avoiding(area, Some((500.0, 660.0)));
        assert_eq!(x, right_x);
        // A user-placed popup is never moved; no selection keeps the default.
        let placed = PopupGeometry {
            position: Some((5.0, 6.0)),
            ..geometry
        };
        assert_eq!(
            placed.resolved_position_avoiding(area, Some((320.0, 400.0))),
            (5.0, 6.0)
        );
        assert_eq!(
            geometry.resolved_position_avoiding(area, None),
            geometry.resolved_position(area)
        );
    }

    #[test]
    fn popup_shrinks_to_a_small_area_and_keeps_valid_positions() {
        let area = (300.0, 200.0, 800.0, 560.0); // 500 x 360
        let clamped = PopupGeometry::default().clamped(area);
        assert_eq!(clamped.size, (500.0, 360.0), "520x420 shrinks to the area");
        let (x, y) = clamped.resolved_position(area);
        assert!(x >= area.0 && x + clamped.size.0 <= area.2);
        assert!(y >= area.1 && y + clamped.size.1 <= area.3);

        // Smaller than the minimum: the popup yields and fills the area.
        let tiny = PopupGeometry::default().clamped((0.0, 0.0, 200.0, 100.0));
        assert_eq!(tiny.size, (200.0, 100.0));
        let (x, y) = tiny.resolved_position((0.0, 0.0, 200.0, 100.0));
        assert_eq!((x, y), (0.0, 0.0));

        // A user position that is still valid survives; an invalid one is pulled in.
        let valid = PopupGeometry {
            position: Some((350.0, 250.0)),
            size: (400.0, 300.0),
        };
        assert_eq!(valid.clamped(area).position, Some((350.0, 250.0)));
        let outside = PopupGeometry {
            position: Some((10.0, 10.0)),
            size: (400.0, 300.0),
        };
        assert_eq!(outside.clamped(area).position, Some((300.0, 200.0)));
    }

    #[test]
    fn popup_drag_moves_and_resizes_inside_the_area() {
        let area = (100.0, 150.0, 1000.0, 700.0);
        let mut state = CellDetailState::default();
        state.set_placement(CellDetailPlacement::Popup);
        let (x, y) = state.popup.resolved_position(area);

        state.begin_popup_drag(PopupDragKind::Move, (600.0, 200.0), area, (x, y));
        assert!(state.update_popup_drag((580.0, 230.0), area));
        assert_eq!(state.popup.position, Some((x - 20.0, y + 30.0)));
        // Dragging far outside pins the popup to the area edges, not the window.
        state.update_popup_drag((-5000.0, 9000.0), area);
        let (w, h) = state.popup.size;
        assert_eq!(state.popup.position, Some((area.0, area.3 - h)));
        state.end_popup_drag();
        assert!(!state.update_popup_drag((0.0, 0.0), area));

        state.begin_popup_drag(PopupDragKind::Resize, (500.0, 500.0), area, (x, y));
        state.update_popup_drag((400.0, 300.0), area);
        assert_eq!(
            state.popup.size,
            (
                (w - 100.0).max(POPUP_MIN_SIZE.0),
                (h - 200.0).max(POPUP_MIN_SIZE.1)
            )
        );
        state.update_popup_drag((9000.0, 9000.0), area);
        let (x, y) = state.popup.position.unwrap();
        assert!(x + state.popup.size.0 <= area.2 && y + state.popup.size.1 <= area.3);
    }

    #[test]
    fn tabs_go_compact_below_the_threshold_only() {
        assert!(tabs_are_compact(190.0));
        assert!(tabs_are_compact(299.9));
        assert!(!tabs_are_compact(300.0));
        assert!(!tabs_are_compact(320.0));
    }

    #[test]
    fn column_properties_hide_unknown_values() {
        let mut column = CellDetailColumn {
            source_column: 0,
            name: "_id".into(),
            type_name: "object_id".into(),
            nullable: None,
            primary_key: Some(true),
            is_generated: None,
            auto_increment: None,
            default_expression: None,
        };
        assert_eq!(
            column.property_rows(),
            vec![
                ("Name", "_id".to_string()),
                ("Type", "object_id".to_string()),
                ("Primary key", "Yes".to_string()),
            ]
        );
        column.primary_key = None;
        assert_eq!(column.property_rows().len(), 2, "Mongo: just Name and Type");

        column.nullable = Some(false);
        column.is_generated = Some(false);
        column.default_expression = Some("now()".into());
        let labels: Vec<_> = column
            .property_rows()
            .iter()
            .map(|(label, _)| *label)
            .collect();
        assert_eq!(labels, ["Name", "Type", "Nullable", "Generated", "Default"]);
    }

    #[test]
    fn close_others_keeps_one_tab_active() {
        let mut state = CellDetailState::default();
        state.open_tab(CellDetailTab::Details);
        state.open_tab(CellDetailTab::Column);
        state.open_tab(CellDetailTab::Row);
        state.close_other_tabs(CellDetailTab::Column);
        assert_eq!(state.opened_tabs, vec![CellDetailTab::Column]);
        assert_eq!(state.tab, CellDetailTab::Column);
        assert!(state.open);
    }

    #[test]
    fn row_view_persists_across_rows_and_only_the_visible_view_is_built() {
        let mut state = CellDetailState::default();
        state.open_tab(CellDetailTab::Row);
        state.show(CellPosition { row: 0, column: 0 });
        state.set_row(Some(sample_row(1)));
        assert_eq!(state.row_view, RowView::Tree);
        assert_eq!(state.row_tree.len(), 2);

        state.set_row_view(RowView::Json);
        assert!(
            state.row_tree.is_empty(),
            "tree is dropped while JSON is shown"
        );
        state.show(CellPosition { row: 1, column: 0 });
        state.set_row(Some(sample_row(2)));
        assert_eq!(state.row_view, RowView::Json);
        assert!(state.row_tree.is_empty());

        state.set_row_view(RowView::Tree);
        assert_eq!(state.row_tree.len(), 2);
    }

    #[test]
    fn row_expansion_toggles_and_resets_when_the_row_changes() {
        let mut state = CellDetailState::default();
        state.open_tab(CellDetailTab::Row);
        state.show(CellPosition { row: 0, column: 0 });
        state.set_row(Some(sample_row(1)));

        state.toggle_row_path("/a");
        assert_eq!(state.row_tree.len(), 3);
        // Refreshing the same row keeps the expansion.
        state.show(CellPosition { row: 0, column: 1 });
        state.set_row(Some(sample_row(1)));
        assert_eq!(state.row_tree.len(), 3);
        state.toggle_row_path("/a");
        assert_eq!(state.row_tree.len(), 2);

        state.toggle_row_path("/list");
        state.show(CellPosition { row: 1, column: 1 });
        state.set_row(Some(sample_row(2)));
        assert!(state.row_expanded.is_empty());
        assert_eq!(state.row_tree.len(), 2);
    }

    #[test]
    fn closing_tabs_activates_the_neighbor_and_the_last_tab_closes_the_panel() {
        let mut state = CellDetailState::default();
        state.open_tab(CellDetailTab::Details);
        state.open_tab(CellDetailTab::Row);
        state.open_tab(CellDetailTab::Column);
        assert_eq!(state.tab, CellDetailTab::Column);

        state.close_tab(CellDetailTab::Row);
        assert_eq!(state.tab, CellDetailTab::Column);
        assert_eq!(
            state.opened_tabs,
            vec![CellDetailTab::Details, CellDetailTab::Column]
        );
        state.close_tab(CellDetailTab::Column);
        assert_eq!(state.tab, CellDetailTab::Details);
        assert!(state.open);
        state.close_tab(CellDetailTab::Details);
        assert!(!state.open);
        assert!(state.opened_tabs.is_empty());
    }

    #[test]
    fn null_details_have_no_value_length() {
        let detail = build_cell_detail(
            CellPosition { row: 0, column: 0 },
            0,
            "name",
            "text",
            "NULL",
            true,
            true,
            false,
            "",
        );

        assert_eq!(detail.length, 0);
    }
}
