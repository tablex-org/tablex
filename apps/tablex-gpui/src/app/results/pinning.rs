//! Pinned result columns: the header menu's Pin / Unpin and the "freeze first
//! column" toggle, which keep columns at the left of the grid while the
//! rest scrolls horizontally.
//!
//! The pinned columns (name plus occurrence among same-named columns, see
//! `pinned_column.rs`) are stored per tab (`Tab::pinned_columns`,
//! persisted in the session) and, for a script run, per result tab
//! (`ScriptResult::pinned`). Like hidden columns they only change the
//! display->source map: `pin_to_front` (in `derived_columns`) puts the pinned
//! columns first, in pin order, and the grid draws that leading block in the
//! sticky element. Staged edits are keyed by display column, so pinning is
//! refused while changes are staged or a cell is being edited.

use super::super::*;
use crate::pinned_column::PinnedColumn;

/// Why a column cannot be pinned now, as the menu says it.
pub(in crate::app) const PIN_LOCKED_HINT: &str = "Apply or discard staged changes first";

impl AppRoot {
    /// Whether pinning applies to the tab's result at all (a MongoDB grid and
    /// the Redis element lists keep their own column handling).
    pub(in crate::app) fn pinning_available(&self, tab_id: u64) -> bool {
        self.workspace
            .tab(tab_id)
            .is_some_and(|tab| !tab.query_mode.is_mongo() && !tab.query_mode.is_redis())
            && self.grid(tab_id).column_count() > 0
    }

    /// True while a pin change would break the staged edits' display columns.
    pub(in crate::app) fn pinning_locked(&self, tab_id: u64) -> bool {
        self.workspace.tab_has_staged_changes(tab_id)
            || self.view(tab_id).grid_ui.inline_edit.is_some()
            || self.view(tab_id).detail_ui.edit.is_some()
    }

    /// Pins the column shown at `display_column` after the ones already pinned.
    pub(in crate::app) fn pin_column(&mut self, tab_id: u64, display_column: usize, cx: &mut Context<Self>) {
        if self.pinning_locked(tab_id) || !self.pinning_available(tab_id) {
            return;
        }
        let Some(pin) = self.pin_at(tab_id, display_column) else {
            return;
        };
        let mut pins = self.pinned_columns(tab_id).to_vec();
        if !pins.contains(&pin) {
            pins.push(pin);
            self.set_pinned(tab_id, pins, cx);
        }
    }

    /// Unpins the column shown at `display_column`; it returns to its place
    /// among the unpinned columns.
    pub(in crate::app) fn unpin_column(&mut self, tab_id: u64, display_column: usize, cx: &mut Context<Self>) {
        if self.pinning_locked(tab_id) {
            return;
        }
        let Some(pin) = self.pin_at(tab_id, display_column) else {
            return;
        };
        let mut pins = self.pinned_columns(tab_id).to_vec();
        if let Some(at) = pins.iter().position(|pinned| *pinned == pin) {
            pins.remove(at);
            self.set_pinned(tab_id, pins, cx);
        }
    }

    /// The source index of the result's first shown column (what "freeze
    /// first column" pins; a hidden one cannot be frozen).
    fn first_shown_source(&self, tab_id: u64) -> Option<usize> {
        match &self.view(tab_id).grid_ui.visible_columns {
            visible if visible.is_empty() => Some(0),
            visible => visible.iter().min().copied(),
        }
    }

    /// Whether the result's first column is pinned.
    pub(in crate::app) fn first_column_frozen(&self, tab_id: u64) -> bool {
        let grid = self.grid(tab_id);
        let Some(source) = self.first_shown_source(tab_id) else {
            return false;
        };
        let Some(name) = grid.column_name(source) else {
            return false;
        };
        let occurrence = occurrence_of(grid, source, name);
        self.pinned_columns(tab_id).iter().any(|pin| pin.matches(name, occurrence))
    }

    /// "Freeze first column": pins the result's first column in front
    /// of the others, or unpins it when it is pinned already.
    pub(in crate::app) fn toggle_freeze_first_column(&mut self, tab_id: u64, cx: &mut Context<Self>) {
        if self.pinning_locked(tab_id) || !self.pinning_available(tab_id) {
            return;
        }
        let Some(first) = self.first_shown_source(tab_id).and_then(|source| self.pin_of(tab_id, source)) else {
            return;
        };
        let mut pins = self.pinned_columns(tab_id).to_vec();
        match pins.iter().position(|pin| *pin == first) {
            Some(at) => {
                pins.remove(at);
            }
            None => pins.insert(0, first),
        }
        self.set_pinned(tab_id, pins, cx);
    }

    pub(in crate::app) fn unpin_all_columns(&mut self, tab_id: u64, cx: &mut Context<Self>) {
        if self.pinning_locked(tab_id) || self.pinned_columns(tab_id).is_empty() {
            return;
        }
        self.set_pinned(tab_id, Vec::new(), cx);
    }

    /// The pin identity of the column shown at `display_column`.
    fn pin_at(&self, tab_id: u64, display_column: usize) -> Option<PinnedColumn> {
        let source = self.source_column_for_display(tab_id, display_column)?;
        self.pin_of(tab_id, source)
    }

    /// The pin identity of source column `source` of the tab's result.
    fn pin_of(&self, tab_id: u64, source: usize) -> Option<PinnedColumn> {
        let grid = self.grid(tab_id);
        let name = grid.column_name(source)?;
        Some(PinnedColumn::new(name, occurrence_of(grid, source, name)))
    }

    /// Stores the pins (on the shown script result, else on the tab) and
    /// re-orders the grid, keeping the selected cell on the same column.
    fn set_pinned(&mut self, tab_id: u64, pins: Vec<PinnedColumn>, cx: &mut Context<Self>) {
        let script_result = self
            .workspace
            .runtime(tab_id)
            .and_then(|runtime| runtime.script.as_ref())
            .and_then(|script| match script.shown {
                crate::script_run::ScriptTab::Result(index) => Some(index),
                crate::script_run::ScriptTab::Statistics => None,
            });
        match script_result {
            Some(index) => {
                let Some(result) = self
                    .workspace
                    .runtime_mut(tab_id)
                    .and_then(|runtime| runtime.script.as_mut())
                    .and_then(|script| script.results.get_mut(index))
                else {
                    return;
                };
                result.pinned = pins;
            }
            None => {
                let Some(tab) = self.workspace.tab_mut(tab_id) else {
                    return;
                };
                tab.pinned_columns = pins;
            }
        }
        self.reorder_columns(tab_id);
        self.notify_tab(tab_id, cx);
    }

    /// Rebuilds the display order after the pinned set changed. The selected
    /// cell (or its column, for a range) follows its source column to the new
    /// display position; the scroll goes back to the left so the change is seen.
    fn reorder_columns(&mut self, tab_id: u64) {
        let selected = self
            .view(tab_id)
            .grid_ui
            .selection
            .borrow()
            .range
            .map(|range| range.end);
        let source = selected.and_then(|cell| self.source_column_for_display(tab_id, cell.column));
        let Some(derived) = self.derived_columns(tab_id) else {
            return;
        };
        let view = self.view_mut(tab_id);
        view.grid_ui.visible_columns = derived.visible_columns;
        view.grid_ui.pinned = derived.pinned;
        view.grid_ui.layout = derived.layout;
        view.grid_ui.context_cell = None;
        let at = |view: &super::TabView, source: usize| {
            view.grid_ui
                .visible_columns
                .iter()
                .position(|candidate| *candidate == source)
        };
        let target = selected
            .zip(source)
            .and_then(|(cell, source)| at(view, source).map(|column| CellPosition { row: cell.row, column }));
        match target {
            Some(position) => view.grid_ui.selection.borrow_mut().select(position),
            None => view.grid_ui.selection.borrow_mut().clear(),
        }
        view.grid_horizontal_scroll
            .set_offset(gpui::point(px(0.0), view.grid_horizontal_scroll.offset().y));
        if let Some(position) = target {
            self.refresh_cell_detail_target(tab_id, position);
        }
    }
}

/// How many columns before `source` share its `name` (its occurrence among
/// the same-named columns, in source order). Run on a menu action or once per
/// frame for the first column only, never per cell.
fn occurrence_of(grid: &GridState<GridContentSource>, source: usize, name: &str) -> usize {
    (0..source).filter(|column| grid.column_name(*column) == Some(name)).count()
}
