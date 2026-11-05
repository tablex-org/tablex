//! Record (form) view of the current row ("Switch to record mode"):
//! the grid's area shows ONE row as a vertical list of fields (column, type,
//! value) instead of the table. The footer button or Tab (in the grid) switches
//! it on and off; Previous / Next (and PageUp / PageDown) move to the
//! neighbouring row.
//!
//! The record view has no state of its own beyond "on": the current row and
//! field are the grid's selection, so the grid, the Details panel and the
//! footer statistics follow the record (and back again). Editing is the grid's
//! inline editor in the value cell: the same staged-edit model, the same
//! Apply / Discard, read-only wherever the grid is. The field list is
//! virtualized (a wide table paints the visible fields only) and clicks are
//! hit-tested once at the list, like the grid body.

use super::super::*;

const FIELD_HEIGHT: f32 = 28.0;
const NAME_WIDTH: f32 = 200.0;
const TYPE_WIDTH: f32 = 110.0;
/// Width a floating (multi-line) value editor is given.
const FLOATING_EDITOR_WIDTH: f32 = 360.0;

/// Why the record view is unavailable for a result, as the button's tooltip says it.
pub(in crate::app) const RECORD_UNAVAILABLE_MONGO: &str =
    "Record view is not available for MongoDB results (use the Tree or JSON view)";
pub(in crate::app) const RECORD_UNAVAILABLE_REDIS: &str =
    "Record view is not available for Redis results";

impl AppRoot {
    /// `Ok` when the record view applies to the tab's result, else why not.
    pub(in crate::app) fn record_view_support(&self, tab_id: u64) -> Result<(), &'static str> {
        let Some(tab) = self.workspace.tab(tab_id) else {
            return Err("No result");
        };
        if tab.query_mode.is_mongo() {
            return Err(RECORD_UNAVAILABLE_MONGO);
        }
        if tab.query_mode.is_redis() {
            return Err(RECORD_UNAVAILABLE_REDIS);
        }
        if self.grid(tab_id).column_count() == 0 {
            return Err("No result to show as a record");
        }
        Ok(())
    }

    /// True while the grid area shows the record view instead of the table.
    pub(in crate::app) fn record_view_active(&self, tab_id: u64) -> bool {
        self.view(tab_id).grid_ui.record
            && self.record_view_support(tab_id).is_ok()
            && !self.columns_presentation_active(tab_id)
            && !self.chart_presentation_active(tab_id)
            && !self.diagram_presentation_active(tab_id)
            && !self.plan_presentation_active(tab_id)
    }

    /// The row the record view shows: the selection's row, else the first.
    pub(in crate::app) fn record_row(&self, tab_id: u64) -> usize {
        let last = self.grid(tab_id).row_count().saturating_sub(1);
        self.view(tab_id)
            .grid_ui
            .selection
            .borrow()
            .range
            .map_or(0, |range| range.end.row)
            .min(last)
    }

    /// The field (display column) that is selected in the record view.
    fn record_field(&self, tab_id: u64) -> usize {
        let last = self.display_column_count(tab_id).saturating_sub(1);
        self.view(tab_id)
            .grid_ui
            .selection
            .borrow()
            .range
            .map_or(0, |range| range.end.column)
            .min(last)
    }

    /// Switches between the table and the record view of the current row.
    pub(in crate::app) fn toggle_record_view(&mut self, tab_id: u64, cx: &mut Context<Self>) {
        if self.record_view_support(tab_id).is_err() {
            return;
        }
        // A chart shown instead of the table gives way to the record.
        if self.chart_presentation_active(tab_id) {
            self.show_grid_presentation(tab_id, cx);
        }
        // A cell editor open in the grid belongs to the view being left.
        self.commit_inline_edit(tab_id, InlineEditExit::FocusLoss, None, cx);
        let turning_on = !self.view(tab_id).grid_ui.record;
        self.view_mut(tab_id).grid_ui.record = turning_on;
        if turning_on {
            if self.grid(tab_id).row_count() > 0
                && self.view(tab_id).grid_ui.selection.borrow().range.is_none()
            {
                self.select_grid_cell(tab_id, CellPosition { row: 0, column: 0 });
            }
        } else if self.grid(tab_id).row_count() > 0 {
            // Back in the table the current cell is in view.
            let row = self.record_row(tab_id);
            let column = self.record_field(tab_id);
            let view = self.view_mut(tab_id);
            view.grid_scroll
                .scroll_to_item(row, gpui::ScrollStrategy::Nearest);
            view.scroll_to_column(column);
        }
        self.notify_tab(tab_id, cx);
    }

    /// Moves the record to `row` (clamped), on the same field.
    pub(in crate::app) fn record_go_to_row(&mut self, tab_id: u64, row: usize, cx: &mut Context<Self>) {
        let rows = self.grid(tab_id).row_count();
        if rows == 0 {
            return;
        }
        // Leaving a row commits the value being typed (or says why it cannot).
        self.commit_inline_edit(tab_id, InlineEditExit::FocusLoss, None, cx);
        let position = CellPosition {
            row: row.min(rows - 1),
            column: self.record_field(tab_id),
        };
        self.select_grid_cell(tab_id, position);
        self.view(tab_id)
            .grid_scroll
            .scroll_to_item(position.row, gpui::ScrollStrategy::Nearest);
        self.notify_tab(tab_id, cx);
    }

    pub(in crate::app) fn record_step(&mut self, tab_id: u64, forward: bool, cx: &mut Context<Self>) {
        let row = self.record_row(tab_id);
        let target = if forward {
            row.saturating_add(1)
        } else {
            row.saturating_sub(1)
        };
        self.record_go_to_row(tab_id, target, cx);
    }

    /// Selects field `field` of the current row.
    fn record_select_field(&mut self, tab_id: u64, field: usize, cx: &mut Context<Self>) {
        if self.grid(tab_id).row_count() == 0 || field >= self.display_column_count(tab_id) {
            return;
        }
        let position = CellPosition {
            row: self.record_row(tab_id),
            column: field,
        };
        self.select_grid_cell(tab_id, position);
        self.view(tab_id)
            .record_scroll
            .scroll_to_item(field, gpui::ScrollStrategy::Nearest);
        self.notify_tab(tab_id, cx);
    }

    /// The field under a window-space point, from the painted list's bounds.
    fn record_field_at(&self, tab_id: u64, point: gpui::Point<gpui::Pixels>) -> Option<usize> {
        let (bounds, offset) = {
            let state = self.view(tab_id).record_scroll.0.borrow();
            (state.base_handle.bounds(), state.base_handle.offset())
        };
        if !bounds.contains(&point) {
            return None;
        }
        let y = f32::from(point.y - bounds.origin.y - offset.y);
        let field = (y / FIELD_HEIGHT) as usize;
        (y >= 0.0 && field < self.display_column_count(tab_id)).then_some(field)
    }

    pub(in crate::app) fn render_record_pane(
        &mut self,
        tab_id: u64,
        cx: &mut Context<Self>,
    ) -> Div {
        self.sync_column_type_labels(tab_id);
        let grid_focus = self.view_mut(tab_id).grid_focus(cx);
        let rows = self.grid(tab_id).row_count();
        let fields = self.display_column_count(tab_id);
        let row = self.record_row(tab_id);
        let view = self.view(tab_id);

        let step = |id: &'static str, forward: bool, enabled: bool, cx: &mut Context<Self>| {
            Button::new((id, tab_id))
                .small()
                .icon(
                    Icon::new(IconName::ChevronRight)
                        .size(px(14.0))
                        .when(!forward, |icon| icon.rotate(gpui::percentage(0.5))),
                )
                .compact()
                .ghost()
                .tooltip(if forward { "Next row (Page Down)" } else { "Previous row (Page Up)" })
                .debug_selector(move || id.into())
                .disabled(!enabled)
                .on_click(cx.listener(move |this, _, _, cx| this.record_step(tab_id, forward, cx)))
        };
        let bar = div()
            .debug_selector(|| "record-bar".into())
            .h(px(theme::RESULT_GRID_HEADER_HEIGHT))
            .flex_shrink_0()
            .px(px(8.0))
            .flex()
            .items_center()
            .gap(px(6.0))
            .bg(theme::grid_header())
            .border_b_1()
            .border_color(theme::border())
            .text_size(px(12.0))
            .child(step("record-prev", false, row > 0, cx))
            .child(step("record-next", true, row + 1 < rows, cx))
            .child(
                div()
                    .debug_selector(|| "record-position".into())
                    .text_color(theme::text())
                    .child(if rows == 0 {
                        "No rows".to_string()
                    } else {
                        format!("Row {} of {rows}", row + 1)
                    }),
            )
            .child(
                div()
                    .flex_1()
                    .min_w_0()
                    .truncate()
                    .text_color(theme::text_muted())
                    .child(format!("{fields} {}", if fields == 1 { "field" } else { "fields" })),
            )
            .child(
                Button::new(("record-to-table", tab_id))
                    .small()
                    .label("Table")
                    .compact()
                    .ghost()
                    .tooltip("Back to the table (Tab)")
                    .debug_selector(|| "record-to-table".into())
                    .on_click(cx.listener(move |this, _, _, cx| this.toggle_record_view(tab_id, cx))),
            );

        let header = div()
            .h(px(FIELD_HEIGHT))
            .flex_shrink_0()
            .flex()
            .items_center()
            .border_b_1()
            .border_color(theme::border())
            .bg(theme::grid_header())
            .text_size(px(11.0))
            .text_color(theme::text_muted())
            .child(div().w(px(NAME_WIDTH)).px(px(10.0)).flex_shrink_0().child("Column"))
            .child(div().w(px(TYPE_WIDTH)).px(px(8.0)).flex_shrink_0().child("Type"))
            .child(div().flex_1().min_w_0().px(px(8.0)).child("Value"));

        let list = uniform_list(
            ("record-fields", tab_id),
            if rows == 0 { 0 } else { fields },
            cx.processor(move |this, range: Range<usize>, _window, cx| {
                range
                    .map(|field| this.render_record_field(tab_id, row, field, cx))
                    .collect::<Vec<_>>()
            }),
        )
        .flex_1()
        .min_h_0()
        .track_scroll(&view.record_scroll);

        let body = div()
            .id(("record-body", tab_id))
            .debug_selector(|| "record-body".into())
            .relative()
            .flex_1()
            .min_h_0()
            .flex()
            .on_mouse_down(
                MouseButton::Left,
                cx.listener(move |this, event: &MouseDownEvent, window, cx| {
                    let Some(field) = this.record_field_at(tab_id, event.position) else {
                        return;
                    };
                    let row = this.record_row(tab_id);
                    let position = CellPosition { row, column: field };
                    if this
                        .view(tab_id)
                        .grid_ui
                        .inline_edit
                        .as_ref()
                        .is_some_and(|edit| edit.position == position)
                    {
                        return;
                    }
                    this.view_mut(tab_id).grid_focus(cx).focus(window, cx);
                    this.record_select_field(tab_id, field, cx);
                    if event.click_count >= 2 {
                        this.begin_inline_edit(tab_id, row, field, window, cx);
                        if this
                            .view(tab_id)
                            .grid_ui
                            .inline_edit
                            .as_ref()
                            .is_some_and(|edit| edit.position == position)
                        {
                            // The focusable pane must not take the editor's focus back.
                            window.prevent_default();
                        }
                    }
                }),
            )
            .child(list)
            .child(Scrollbar::vertical(&view.record_scroll));

        div()
            .flex()
            .flex_col()
            .flex_1()
            .min_h_0()
            .min_w_0()
            .overflow_hidden()
            .bg(theme::background())
            .child(
                div()
                    .id(("record-view", tab_id))
                    .debug_selector(|| "record-view".into())
                    .key_context("grid")
                    .track_focus(&grid_focus)
                    .flex()
                    .flex_col()
                    .flex_1()
                    .min_h_0()
                    .min_w_0()
                    .on_key_down(cx.listener(move |this, event: &gpui::KeyDownEvent, _, cx| {
                        if this.view(tab_id).grid_ui.inline_edit.is_some() {
                            return;
                        }
                        let field = this.record_field(tab_id);
                        let row = this.record_row(tab_id);
                        match event.keystroke.key.as_str() {
                            "up" => this.record_select_field(tab_id, field.saturating_sub(1), cx),
                            "down" => this.record_select_field(tab_id, field + 1, cx),
                            "pageup" => this.record_go_to_row(tab_id, row.saturating_sub(1), cx),
                            "pagedown" => this.record_go_to_row(tab_id, row + 1, cx),
                            _ => return,
                        }
                        cx.stop_propagation();
                    }))
                    .on_action(cx.listener(move |this, _: &CopyGridSelection, _, cx| {
                        this.copy_grid_selection(tab_id, cx);
                    }))
                    .on_action(cx.listener(move |this, _: &gpui_component::input::Copy, _, cx| {
                        this.copy_grid_selection(tab_id, cx);
                    }))
                    .on_action(cx.listener(move |this, _: &EditGridCell, window, cx| {
                        this.open_cell_editor(tab_id, window, cx);
                    }))
                    .on_action(cx.listener(move |this, _: &CancelCellEdit, window, cx| {
                        this.cancel_inline_edit(tab_id, window, cx);
                    }))
                    .on_action(cx.listener(move |this, _: &RefreshGridResults, window, cx| {
                        let view = this.view(tab_id);
                        if view.grid_ui.inline_edit.is_none() && view.detail_ui.edit.is_none() {
                            this.refresh_tab_results(tab_id, window, cx);
                        }
                    }))
                    .on_action(cx.listener(move |this, _: &ToggleRecordView, _, cx| {
                        // Tab belongs to a cell editor that is open (it indents or moves focus).
                        if this.view(tab_id).grid_ui.inline_edit.is_some() {
                            cx.propagate();
                            return;
                        }
                        this.toggle_record_view(tab_id, cx);
                    }))
                    .child(bar)
                    .child(header)
                    .child(if rows == 0 {
                        div()
                            .debug_selector(|| "record-empty".into())
                            .flex_1()
                            .flex()
                            .items_center()
                            .justify_center()
                            .text_size(px(12.0))
                            .text_color(theme::text_muted())
                            .child("No rows to show")
                            .into_any_element()
                    } else {
                        body.into_any_element()
                    }),
            )
    }

    /// One field of the record: column name, type and the value (or its editor).
    fn render_record_field(
        &self,
        tab_id: u64,
        row: usize,
        field: usize,
        cx: &mut Context<Self>,
    ) -> gpui::Stateful<Div> {
        let view = self.view(tab_id);
        let grid = self.grid(tab_id);
        let display = &self.grid_display;
        let runtime = self.workspace.runtime(tab_id);
        let source_column = view.source_column_for_display(field, grid.column_count());
        let position = CellPosition { row, column: field };
        let name = source_column
            .and_then(|source| grid.column_name(source))
            .unwrap_or_default()
            .to_string();
        let column_type = source_column.map(|source| view.header_column_type(source));
        let selected = view
            .grid_ui
            .selection
            .borrow()
            .range
            .is_some_and(|range| range.end == position);
        let editing = view
            .grid_ui
            .inline_edit
            .as_ref()
            .is_some_and(|edit| edit.position == position);
        let pending = runtime
            .and_then(|runtime| runtime.pending_cell_text(position))
            .map(super::grid::pending_cell_preview);
        let (text, is_null) = match (&pending, source_column) {
            (Some(text), _) => (text.clone(), false),
            (None, Some(source)) => display.cell_text(
                grid,
                CellPosition {
                    row,
                    column: source,
                },
            ),
            (None, None) => (gpui::SharedString::default(), false),
        };
        let staged_null = pending.is_some()
            && runtime.is_some_and(|runtime| {
                runtime.pending_edits.iter().any(|edit| {
                    edit.row == row
                        && edit.column == field
                        && matches!(edit.request.value, MutationValue::Null)
                })
            });
        let pending_delete = runtime.is_some_and(|runtime| runtime.pending_deleted_rows.contains(&row));

        let mut value = div()
            .id(("record-value", field))
            .debug_selector(move || format!("record-value-{field}"))
            .relative()
            .flex_1()
            .min_w_0()
            .h_full()
            .px(px(8.0))
            .flex()
            .items_center()
            .overflow_hidden()
            .whitespace_nowrap()
            .text_ellipsis();
        if editing {
            value = self.inline_editor_cell(value, tab_id, field, FLOATING_EDITOR_WIDTH, cx);
        } else if staged_null {
            value = value.text_color(super::grid::grid_cell_text_color(true)).child(display.null_text.clone());
        } else {
            value = value
                .text_color(super::grid::grid_cell_text_color(is_null))
                .child(text);
        }
        let invalid = editing
            && view
                .grid_ui
                .inline_edit
                .as_ref()
                .is_some_and(|edit| edit.error.is_some());
        div()
            .id(("record-field", field))
            .debug_selector(move || format!("record-field-{field}"))
            .w_full()
            .h(px(FIELD_HEIGHT))
            .flex()
            .items_center()
            .border_b_1()
            .border_color(theme::border())
            .text_size(px(display.font_size))
            .when_some(display.font_family.clone(), |this, family| this.font_family(family))
            .when(field % 2 == 1, |this| this.bg(theme::grid_alt_row()))
            .when(pending_delete, |this| {
                this.bg(gpui::Rgba {
                    a: 0.12,
                    ..theme::danger()
                })
            })
            .when(pending.is_some() && !editing, |this| this.bg(theme::pending_cell()))
            .when(selected && !editing, |this| {
                this.bg(theme::accent_surface())
                    .border_1()
                    .border_color(theme::accent())
            })
            .when(editing, |this| {
                this.border_1().border_color(if invalid {
                    theme::danger()
                } else {
                    theme::accent()
                })
            })
            .when(!selected && !editing, |this| {
                this.hover(|style| style.bg(theme::hover()))
            })
            .child(
                div()
                    .w(px(NAME_WIDTH))
                    .px(px(10.0))
                    .flex_shrink_0()
                    .truncate()
                    .font_weight(gpui::FontWeight::MEDIUM)
                    .text_color(theme::text())
                    .child(name),
            )
            .child(
                div()
                    .w(px(TYPE_WIDTH))
                    .px(px(8.0))
                    .flex_shrink_0()
                    .truncate()
                    .text_size(px(11.0))
                    .text_color(
                        column_type
                            .as_ref()
                            .map_or(theme::text_muted(), |label| grid_column_type_color(label)),
                    )
                    .child(column_type.unwrap_or_default()),
            )
            .child(value)
    }
}
