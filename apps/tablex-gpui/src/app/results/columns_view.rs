//! Left result-viewer rail with exclusive presentations:
//! `left rail | Grid or Columns | right panel | right rail`. The Columns
//! presentation replaces the grid area with a table of the result's columns.

use super::super::*;
use super::detail_view::chrome_button_variant;
use crate::columns_panel::{
    type_icon, ColumnItem, ColumnsKey, Presentation, PropertiesTab, CELL_PADDING,
};
use crate::app::actions::{PropertiesRedo, PropertiesUndo};
use gpui_component::input::Input;

pub(super) const ROW_HEIGHT: f32 = 26.0;
pub(super) const HEADER_ROW_HEIGHT: f32 = 24.0;
/// Toolbar row (title + filter) below the Properties tab strip.
pub(super) const COLUMNS_TOOLBAR_HEIGHT: f32 = 34.0;
/// Height of the underline tab strip inside the grid-header-height row.
const PROPERTIES_STRIP_HEIGHT: f32 = 30.0;
impl AppRoot {
    /// Runs before a shown tab's results render: while the panel is open, keeps
    /// the filter input and the (cached) column list current. The list is
    /// rebuilt only when the result/schema key changes and re-filtered only
    /// when the query text changed, never per frame.
    pub(in crate::app) fn sync_columns_panel(
        &mut self,
        tab_id: u64,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(runtime) = self.workspace.runtime(tab_id) else {
            return;
        };
        if runtime.columns_panel.presentation != Presentation::Columns {
            return;
        }
        if let std::collections::hash_map::Entry::Vacant(slot) =
            self.columns_filter_inputs.entry(tab_id)
        {
            let filter = runtime.columns_panel.filter.clone();
            let input = cx.new(|cx| {
                gpui_component::input::InputState::new(window, cx)
                    .placeholder("Filter fields")
                    .default_value(filter)
            });
            let watched = input.clone();
            cx.subscribe(
                &input,
                move |this: &mut Self,
                      _: gpui::Entity<gpui_component::input::InputState>,
                      event: &gpui_component::input::InputEvent,
                      cx: &mut Context<Self>| {
                    if !matches!(event, gpui_component::input::InputEvent::Change) {
                        return;
                    }
                    let value = watched.read(cx).value().to_string();
                    if let Some(runtime) = this.workspace.runtime_mut(tab_id) {
                        runtime.columns_panel.filter = value;
                    }
                    this.notify_tab(tab_id, cx);
                },
            )
            .detach();
            slot.insert(input);
        }

        // The input is the source of truth for the query (programmatic edits
        // do not emit Change events); comparing two short strings per frame.
        if let Some(input) = self.columns_filter_inputs.get(&tab_id) {
            let value = input.read(cx).value();
            if let Some(runtime) = self.workspace.runtime_mut(tab_id) {
                if runtime.columns_panel.filter != value.as_ref() {
                    runtime.columns_panel.filter = value.to_string();
                }
            }
        }
        // Fetches the table properties (once per result) and builds the active
        // list sub-tab's model, now that the filter text is current.
        self.sync_table_properties(tab_id, window, cx);
        self.sync_properties_edit(tab_id, window, cx);
        self.run_properties_edit_seed(tab_id, cx, window);
        let Some(runtime) = self.workspace.runtime(tab_id) else {
            return;
        };
        let key = ColumnsKey {
            generation: runtime.query.generation(),
            page_id: runtime.driver_page_id,
            columns: runtime.grid.column_count(),
            schema_columns: runtime.structure_columns.len(),
            visible: self.display_column_count(tab_id),
        };
        if runtime.columns_panel.key != Some(key) {
            let items = self.build_column_items(tab_id);
            // Text measurement runs once per item-list change, never per frame.
            let style = window.text_style();
            let system = window.text_system().clone();
            let measure = |text: &str, header: bool| -> f32 {
                if text.is_empty() {
                    return 0.0;
                }
                let size = px(if header { 11.0 } else { 12.0 });
                // The measuring style is the window default, slightly narrower
                // than the themed UI font the cells render with: pad by 10%.
                f32::from(
                    system
                        .layout_line(text, size, &[style.to_run(text.len())], None)
                        .width,
                ) * 1.1
            };
            if let Some(runtime) = self.workspace.runtime_mut(tab_id) {
                runtime.columns_panel.set_items(key, items);
                runtime.columns_panel.measure_widths(&measure);
            }
        } else if runtime.columns_panel.filter_is_stale() {
            if let Some(runtime) = self.workspace.runtime_mut(tab_id) {
                runtime.columns_panel.refilter();
            }
        }
    }

    /// One item per displayed grid column, with schema metadata (type,
    /// primary key, nullability) where the catalog knows the column.
    pub(in crate::app) fn build_column_items(&self, tab_id: u64) -> Vec<ColumnItem> {
        let Some(runtime) = self.workspace.runtime(tab_id) else {
            return Vec::new();
        };
        let grid = &runtime.grid;
        let is_mongo = self
            .workspace
            .tab(tab_id)
            .is_some_and(|tab| tab.query_mode.is_mongo_find());
        let is_sql_table = self
            .workspace
            .tab(tab_id)
            .is_some_and(|tab| tab.query_mode.is_table_data());
        let display_count = self.display_column_count(tab_id);
        let visible_columns = &self.view(tab_id).grid_ui.visible_columns;
        // (source column, display column when shown). SQL table tabs list every
        // column, hidden ones included; other results list what the grid shows.
        let columns: Vec<(usize, Option<usize>)> = if is_sql_table {
            (0..grid.column_count())
                .map(|source| {
                    let display = if visible_columns.is_empty() {
                        Some(source)
                    } else {
                        visible_columns
                            .iter()
                            .position(|visible| *visible == source)
                    };
                    (source, display)
                })
                .collect()
        } else {
            (0..display_count)
                .filter_map(|display| {
                    Some((self.source_column_for_display(tab_id, display)?, Some(display)))
                })
                .collect()
        };
        columns
            .into_iter()
            .filter_map(|(source, display)| {
                let name = grid.column_name(source)?.to_string();
                // Mongo has no catalog schema: Name and Type only.
                let schema = (!is_mongo)
                    .then(|| {
                        runtime
                            .structure_columns
                            .iter()
                            .find(|column| column.name == name)
                    })
                    .flatten();
                // Same resolver as the grid header and the cell detail.
                let type_name = self
                    .resolved_column_type(tab_id, source)
                    .unwrap_or_else(|| "unknown".into());
                Some(ColumnItem {
                    display_index: display.unwrap_or(display_count + source),
                    name,
                    // No engine exposes column comments/aliases yet.
                    label: String::new(),
                    type_name,
                    default: schema.and_then(|column| column.default_expression.clone()),
                    primary_key: schema.is_some_and(|column| column.primary_key),
                    nullable: schema.map(|column| column.nullable),
                    hidden: display.is_none(),
                })
            })
            .collect()
    }

    pub(in crate::app) fn columns_presentation_active(&self, tab_id: u64) -> bool {
        self.workspace
            .runtime(tab_id)
            .is_some_and(|runtime| runtime.columns_panel.presentation == Presentation::Columns)
    }

    /// Rail "Columns": replaces the grid area with the columns table. The
    /// selection starts at the grid's selected column, if any.
    pub(in crate::app) fn show_columns_presentation(&mut self, tab_id: u64, cx: &mut Context<Self>) {
        let selected_column = self
            .view(tab_id)
            .grid_ui
            .selection
            .borrow()
            .range
            .map(|range| range.normalized().start.column);
        if let Some(runtime) = self.workspace.runtime_mut(tab_id) {
            runtime.columns_panel.presentation = Presentation::Columns;
            if runtime.columns_panel.selected.is_none() {
                runtime.columns_panel.selected = selected_column;
            }
        }
        self.notify_tab(tab_id, cx);
    }

    /// Click / Enter / arrow keys: highlight the column row. Never leaves the
    /// Properties presentation and never selects or scrolls in the grid; an
    /// open Column tab in the right panel follows the row.
    pub(in crate::app) fn select_column_row(
        &mut self,
        tab_id: u64,
        display_column: usize,
        cx: &mut Context<Self>,
    ) {
        if display_column >= self.display_column_count(tab_id) {
            return;
        }
        let position = self.workspace.runtime_mut(tab_id).and_then(|runtime| {
            runtime.columns_panel.selected = Some(display_column);
            runtime.columns_panel.position_of(display_column)
        });
        if let Some(position) = position {
            self.view(tab_id)
                .columns_scroll
                .scroll_to_item(position, gpui::ScrollStrategy::Nearest);
        }
        // An open right panel follows the row (Column tab), without touching
        // the grid selection.
        let panel_open = self
            .workspace
            .runtime(tab_id)
            .is_some_and(|runtime| runtime.cell_detail.open);
        if panel_open && self.grid(tab_id).row_count() > 0 {
            let row = self
                .view(tab_id)
                .grid_ui
                .selection
                .borrow()
                .range
                .map_or(0, |range| range.normalized().start.row)
                .min(self.grid(tab_id).row_count() - 1);
            let target = CellPosition {
                row,
                column: display_column,
            };
            self.refresh_cell_detail_target(tab_id, target);
            self.open_cell_detail_tab_at(tab_id, CellDetailTab::Column, Some(target), cx);
        }
        self.notify_tab(tab_id, cx);
    }

    /// Left result-viewer toolbar (icon-only): the presentation
    /// group (Grid) in the header-aligned top cell, a thin separator, then the
    /// panel group (Columns). `grid_shown` is false in the Mongo document view,
    /// where Grid is unselected (clicking it switches back) and Columns is
    /// disabled because there is no grid to describe.
    pub(in crate::app) fn render_columns_rail(
        &self,
        tab_id: u64,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let columns_shown = self.columns_presentation_active(tab_id);
        let is_mongo = self
            .workspace
            .tab(tab_id)
            .is_some_and(|tab| tab.query_mode.is_mongo_find());
        let document_view = is_mongo
            && self.workspace.runtime(tab_id).is_some_and(|runtime| {
                runtime.mongo.view != crate::mongo_result::MongoResultView::Table
            });
        let diagram_shown = self.diagram_presentation_active(tab_id);
        let plan_shown = self.plan_presentation_active(tab_id);
        let chart_shown = self.chart_presentation_active(tab_id);
        // Exclusive presentations: Grid = table grid, Columns = columns table,
        // Diagram = ER diagram (SQL table tabs), Plan = the last Explain plan,
        // Chart = the result drawn as a chart.
        let grid_shown = !columns_shown && !diagram_shown && !plan_shown && !chart_shown && !document_view;
        // An Explain run can come first: the rail then has no grid to describe.
        let has_grid = self.grid(tab_id).column_count() > 0;
        let open = columns_shown;
        let button = |id: &'static str, path: &'static str, tooltip: &'static str, active: bool| {
            Button::new((id, tab_id))
                .icon(Icon::empty().path(path).size(px(16.0)))
                .with_size(Size::Small)
                .compact()
                .custom(chrome_button_variant(active, cx))
                .rounded(px(4.0))
                .w(px(28.0))
                .h(px(28.0))
                .selected(active)
                .tooltip(tooltip)
        };
        let grid_button = button("columns-rail-grid", "icons/table.svg", "Grid", grid_shown)
            .on_click(cx.listener(move |this, _, _, cx| this.show_grid_presentation(tab_id, cx)));
        let columns_button = button("properties-rail-toggle", "icons/list-ordered.svg", "Properties", open)
            .on_click(cx.listener(move |this, _, _, cx| this.show_columns_presentation(tab_id, cx)));
        let diagram_button = self.diagram_available(tab_id).then(|| {
            button("diagram-rail-button", "icons/network.svg", "Diagram", diagram_shown)
                .on_click(cx.listener(move |this, _, _, cx| this.show_diagram_presentation(tab_id, cx)))
        });
        let plan_button = self
            .workspace
            .runtime(tab_id)
            .is_some_and(|runtime| runtime.plan.is_some())
            .then(|| {
                button("plan-rail-button", "icons/network.svg", "Explain plan", plan_shown)
                    .on_click(cx.listener(move |this, _, _, cx| this.show_plan_presentation(tab_id, cx)))
            });
        let chart_support = self.chart_support(tab_id);
        let chart_button = button(
            "chart-rail-toggle",
            "icons/chart-column-big.svg",
            match chart_support {
                Ok(()) if chart_shown => "Back to the grid",
                Ok(()) => "Chart",
                Err(reason) => reason,
            },
            chart_shown,
        )
        .disabled(chart_support.is_err())
        .on_click(cx.listener(move |this, _, _, cx| this.toggle_chart_presentation(tab_id, cx)));
        let cell = |height: f32, active: bool, selector: &'static str, button: Button| {
            div()
                .relative()
                .w_full()
                .h(px(height))
                .flex()
                .items_center()
                .justify_center()
                .flex_shrink_0()
                .when(active, |this| {
                    this.child(
                        div()
                            .absolute()
                            .right_0()
                            .top(px((height - 20.0) / 2.0))
                            .w(px(2.0))
                            .h(px(20.0))
                            .rounded_l(px(1.0))
                            .bg(theme::accent()),
                    )
                })
                .child(div().debug_selector(move || selector.into()).child(button))
        };
        let header_cell = theme::RESULT_GRID_HEADER_HEIGHT - 1.0;
        div()
            .id(("columns-rail", tab_id))
            .debug_selector(|| "columns-rail".into())
            .w(px(36.0))
            .h_full()
            .flex()
            .flex_col()
            .items_center()
            .flex_shrink_0()
            .bg(theme::chrome())
            .border_r_1()
            .border_color(theme::border())
            .child(
                div()
                    .w_full()
                    .h(px(theme::RESULT_GRID_HEADER_HEIGHT))
                    .flex()
                    .flex_col()
                    .flex_shrink_0()
                    .child(cell(header_cell, grid_shown, "columns-rail-grid-button", grid_button))
                    // Separator between the presentation and panel groups.
                    .child(div().w_full().h(px(1.0)).flex_shrink_0().bg(theme::border())),
            )
            .child(
                div()
                    .w_full()
                    .pt(px(6.0))
                    .flex()
                    .flex_col()
                    .items_center()
                    .children(has_grid.then(|| cell(32.0, chart_shown, "chart-rail-button", chart_button)))
                    .children(has_grid.then(|| cell(32.0, open, "properties-rail-button", columns_button)))
                    .children(diagram_button.map(|button| {
                        cell(32.0, diagram_shown, "diagram-rail-toggle", button)
                    }))
                    .children(plan_button.map(|button| cell(32.0, plan_shown, "plan-rail-toggle", button))),
            )
            .into_any_element()
    }

    /// Rail "Grid" button: back to the grid presentation. Mongo also leaves
    /// the document view.
    pub(in crate::app) fn show_grid_presentation(&mut self, tab_id: u64, cx: &mut Context<Self>) {
        if let Some(runtime) = self.workspace.runtime_mut(tab_id) {
            runtime.columns_panel.presentation = Presentation::Grid;
        }
        let is_mongo = self
            .workspace
            .tabs()
            .iter()
            .any(|tab| tab.id == tab_id && tab.query_mode.is_mongo_find());
        if is_mongo {
            self.set_mongo_view(tab_id, crate::mongo_result::MongoResultView::Table, cx);
        }
        self.notify_tab(tab_id, cx);
    }

    /// The Properties presentation: fills the grid area with a horizontal
    /// sub-tab strip (same look as the right panel's tab strip, in the
    /// grid-header-height row) above the selected sub-tab's content.
    pub(in crate::app) fn render_columns_view(&mut self, tab_id: u64, cx: &mut Context<Self>) -> Div {
        let Some(runtime) = self.workspace.runtime(tab_id) else {
            return div();
        };
        let active = runtime.columns_panel.properties_tab;
        // Secondary (underline) tabs: text only, no box or borders; the
        // active tab gets a 2px accent underline flush with the strip's
        // bottom line.
        let sub_tabs = div()
            .debug_selector(|| "properties-tab-list".into())
            .h(px(PROPERTIES_STRIP_HEIGHT))
            .flex()
            .items_end()
            .gap(px(2.0))
            .min_w_0()
            .overflow_hidden()
            .children(runtime.columns_panel.props.visible_tabs().map(|tab| {
                let selector = tab.selector();
                let is_active = tab == active;
                div()
                    .id(("properties-tab", tab as usize))
                    .debug_selector(move || selector.into())
                    .relative()
                    .h_full()
                    .px(px(10.0))
                    .flex()
                    .items_center()
                    .text_size(px(12.0))
                    .whitespace_nowrap()
                    .cursor_pointer()
                    .text_color(if is_active { theme::text() } else { theme::text_muted() })
                    .when(is_active, |this| this.font_weight(gpui::FontWeight::MEDIUM))
                    .when(!is_active, |this| {
                        this.hover(|style| style.text_color(theme::text()))
                    })
                    .on_click(cx.listener(move |this, _, window, cx| {
                        this.select_properties_tab(tab_id, tab, window, cx);
                    }))
                    .child(tab.label())
                    .when(is_active, |this| {
                        this.child(
                            div()
                                .debug_selector(|| "properties-tab-underline".into())
                                .absolute()
                                .left(px(10.0))
                                .right(px(10.0))
                                .bottom(px(0.0))
                                .h(px(2.0))
                                .bg(theme::accent()),
                        )
                    })
            }));
        // The strip fills the bottom of the grid-header-height row so its
        // bottom line lines up with the grid header's.
        let strip_row = div()
            .debug_selector(|| "properties-tab-strip".into())
            .h(px(theme::RESULT_GRID_HEADER_HEIGHT))
            .w_full()
            .flex_shrink_0()
            .flex()
            .items_end()
            .px(px(2.0))
            .bg(theme::chrome())
            .border_b_1()
            .border_color(theme::border())
            .child(sub_tabs)
            .child(
                div()
                    .ml_auto()
                    .h(px(PROPERTIES_STRIP_HEIGHT))
                    .flex()
                    .items_center()
                    .pr(px(6.0))
                    .children(self.render_properties_refresh(tab_id, cx)),
            );
        let content = match active {
            PropertiesTab::Columns => self.render_columns_content(tab_id, cx),
            tab => self.render_properties_content(tab_id, tab, cx),
        };
        let status = self.render_properties_status(tab_id, cx);
        div()
            .debug_selector(|| "columns-view".into())
            .size_full()
            .flex()
            .flex_col()
            .bg(theme::background())
            .child(strip_row)
            .children(status)
            .child(content)
    }

    /// Columns sub-tab: toolbar row (title + compact filter) and a table whose
    /// columns are sized to their content, left-aligned; the area to the right
    /// of the table stays plain background.
    fn render_columns_content(
        &mut self,
        tab_id: u64,
        cx: &mut Context<Self>,
    ) -> gpui::Stateful<Div> {
        let columns_focus = self.view_mut(tab_id).columns_focus(cx);
        let Some(runtime) = self.workspace.runtime(tab_id) else {
            return div().id(("columns-content-empty", tab_id));
        };
        let panel = &runtime.columns_panel;
        let items = panel.items.clone();
        let filtered = panel.filtered.clone();
        let widths = panel.widths;
        let (has_nullable, has_key, has_default, has_comment) =
            (panel.has_nullable, panel.has_key, panel.has_default, panel.has_comment);
        let selected = panel.selected;
        let mut title = if filtered.len() == items.len() {
            format!("Columns ({})", items.len())
        } else {
            format!("Columns ({}/{})", filtered.len(), items.len())
        };
        // Structure edit mode: the one list draws the rows of the draft instead of the result's
        // columns (the other list below is only built, never drawn).
        let edit_body = self
            .properties_edit_render(tab_id, cx)
            .map(|state| self.edit_columns_body(tab_id, state, widths, cx));
        let editing = edit_body.is_some();
        if let Some(body) = &edit_body {
            title = body.title.clone();
        }
        let table_width = widths.total();
        let cell = |width: f32, header: bool| {
            div()
                .w(px(width))
                .h_full()
                .flex_shrink_0()
                .px(px(CELL_PADDING))
                .flex()
                .items_center()
                .overflow_hidden()
                .border_r_1()
                .border_color(theme::border())
                .when(header, |this| this.text_size(px(11.0)).text_color(theme::text_muted()))
        };
        let list = uniform_list(
            ("columns-view-list", tab_id),
            filtered.len(),
            cx.processor(move |_this, range: Range<usize>, _window, cx| {
                range
                    .map(|index| {
                        let item = &items[filtered[index]];
                        let display_index = item.display_index;
                        let is_selected = selected == Some(display_index);
                        div()
                            .id(("columns-view-row", index))
                            .debug_selector(|| "columns-view-row".into())
                            .h(px(ROW_HEIGHT))
                            // Only as wide as the table: hover/selection do not
                            // stretch across the whole area.
                            .w(px(table_width))
                            .flex()
                            .items_center()
                            .text_size(px(12.0))
                            .cursor_pointer()
                            .when(item.hidden, |this| this.opacity(0.55))
                            .when(is_selected, |this| this.bg(theme::accent_surface()))
                            .when(!is_selected, |this| this.hover(|style| style.bg(theme::hover())))
                            .border_b_1()
                            .border_color(theme::divider())
                            .on_click(cx.listener(move |this, _, window, cx| {
                                // Single or double click only selects the row.
                                this.view_mut(tab_id).columns_focus(cx).focus(window, cx);
                                this.select_column_row(tab_id, display_index, cx);
                            }))
                            .child(
                                cell(widths.name, false)
                                    .gap(px(6.0))
                                    .child(
                                        // A column hidden in the grid shows the
                                        // eye-off glyph in place of its type icon.
                                        Icon::empty()
                                            .path(if item.hidden {
                                                "icons/eye-off.svg"
                                            } else {
                                                type_icon(&item.type_name)
                                            })
                                            .size(px(12.0))
                                            .text_color(grid_column_type_color(&item.type_name))
                                            .flex_shrink_0(),
                                    )
                                    .child(
                                        div()
                                            .min_w_0()
                                            .truncate()
                                            .text_color(theme::text())
                                            .child(item.name.clone()),
                                    ),
                            )
                            .child(cell(widths.type_name, false).child(
                                div()
                                    .truncate()
                                    .text_color(grid_column_type_color(&item.type_name))
                                    .child(item.type_name.clone()),
                            ))
                            .when(has_nullable, |this| {
                                this.child(cell(widths.nullable, false).child(
                                    div()
                                        .text_color(theme::text_muted())
                                        .child(crate::columns_panel::nullable_text(item)),
                                ))
                            })
                            .when(has_key, |this| {
                                this.child(cell(widths.key, false).gap(px(4.0)).when(
                                    item.primary_key,
                                    |this| {
                                        this.child(
                                            Icon::empty()
                                                .path("icons/key-round.svg")
                                                .size(px(11.0))
                                                .text_color(theme::warning())
                                                .flex_shrink_0(),
                                        )
                                        .child(div().text_color(theme::text_muted()).child("PK"))
                                    },
                                ))
                            })
                            .when(has_default, |this| {
                                this.child(cell(widths.default, false).child(
                                    div()
                                        .truncate()
                                        .text_color(theme::text_muted())
                                        .child(item.default.clone().unwrap_or_default()),
                                ))
                            })
                            .when(has_comment, |this| {
                                this.child(cell(widths.comment, false).child(
                                    div()
                                        .truncate()
                                        .text_color(theme::text_muted())
                                        .child(item.label.clone()),
                                ))
                            })
                            .into_any_element()
                    })
                    .collect()
            }),
        )
        .track_scroll(&self.view(tab_id).columns_scroll)
        .flex_1()
        .min_h_0();
        let empty = runtime.columns_panel.filtered.is_empty();
        let filter_input = self.columns_filter_inputs.get(&tab_id).cloned();
        let edit_toolbar = self.render_properties_edit_toolbar(tab_id, crate::columns_panel::PropertiesTab::Columns, cx);
        let columns_header = div()
            .debug_selector(|| "columns-table-header".into())
            .h(px(HEADER_ROW_HEIGHT))
            .w(px(table_width))
            .flex_shrink_0()
            .flex()
            .items_center()
            .bg(theme::grid_header())
            .border_b_1()
            .border_color(theme::border())
            .child(cell(widths.name, true).child("Name"))
            .child(cell(widths.type_name, true).child("Type"))
            .when(has_nullable, |this| this.child(cell(widths.nullable, true).child("Nullable")))
            .when(has_key, |this| this.child(cell(widths.key, true).child("Key")))
            .when(has_default, |this| this.child(cell(widths.default, true).child("Default")))
            .when(has_comment, |this| this.child(cell(widths.comment, true).child("Comment")));
        let (header, edit_list, empty) = match edit_body {
            Some(body) => (body.header, Some(body.list), body.empty),
            None => (columns_header, None, empty),
        };

        div()
            .id(("columns-content", tab_id))
            .track_focus(&columns_focus)
            .when(editing, |this| {
                this.key_context("properties-edit")
                    .on_action(cx.listener(move |this, _: &PropertiesUndo, _, cx| this.with_properties_edit(tab_id, cx, |edit, cx| edit.undo(cx))))
                    .on_action(cx.listener(move |this, _: &PropertiesRedo, _, cx| this.with_properties_edit(tab_id, cx, |edit, cx| edit.redo(cx))))
            })
            .on_key_down(cx.listener(move |this, event: &gpui::KeyDownEvent, window, cx| {
                if editing {
                    let focused = this.view_mut(tab_id).columns_focus(cx).is_focused(window);
                    if this.properties_edit_key(tab_id, event.keystroke.key.as_str(), focused, window, cx) {
                        cx.stop_propagation();
                    }
                    return;
                }
                let delta = match event.keystroke.key.as_str() {
                    "down" => 1,
                    "up" => -1,
                    "enter" => {
                        let selected = this
                            .workspace
                            .runtime(tab_id)
                            .and_then(|runtime| runtime.columns_panel.selected);
                        if let Some(column) = selected {
                            this.select_column_row(tab_id, column, cx);
                            cx.stop_propagation();
                        }
                        return;
                    }
                    _ => return,
                };
                let target = this
                    .workspace
                    .runtime(tab_id)
                    .and_then(|runtime| runtime.columns_panel.step_selection(delta));
                if let Some(column) = target {
                    this.select_column_row(tab_id, column, cx);
                    cx.stop_propagation();
                }
            }))
            .debug_selector(|| "columns-content".into())
            .flex_1()
            .min_w_0()
            .min_h_0()
            .flex()
            .flex_col()
            .child(
                div()
                    .debug_selector(|| "columns-view-header".into())
                    .h(px(COLUMNS_TOOLBAR_HEIGHT))
                    .flex_shrink_0()
                    .flex()
                    .items_center()
                    .gap(px(12.0))
                    .px(px(12.0))
                    .border_b_1()
                    .border_color(theme::border())
                    .bg(theme::grid_header())
                    .child(
                        div()
                            .flex_shrink_0()
                            .whitespace_nowrap()
                            .text_size(px(12.0))
                            .text_color(theme::text())
                            .child(title),
                    )
                    .child(
                        div()
                            .debug_selector(|| "columns-view-filter".into())
                            .w(px(220.0))
                            .min_w(px(96.0))
            .flex_shrink(1.0)
                            .children(filter_input.map(|input| Input::new(&input).small().cleanable(true))),
                    )
                    .children(edit_toolbar),
            )
            .child(header)
            .child(if empty {
                div()
                    .flex_1()
                    .flex()
                    .items_center()
                    .justify_center()
                    .text_size(px(11.0))
                    .text_color(theme::text_muted())
                    .child("No matching fields")
                    .into_any_element()
            } else {
                edit_list.unwrap_or_else(|| list.into_any_element())
            })
    }

    /// `left rail | (grid or columns) | right panel | right rail`. Wraps the
    /// results body with the rail for every result that has columns, including
    /// the Mongo document view.
    pub(in crate::app) fn render_results(
        &mut self,
        tab_id: u64,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        if let Some(placeholder) = self.render_connection_state_placeholder(tab_id, cx) {
            self.view(tab_id).results_bounds.set(None);
            return placeholder;
        }
        let Some(banner) = self.render_connection_state_banner(tab_id, cx) else {
            return self.render_results_inner(tab_id, window, cx);
        };
        let results = self.render_results_inner(tab_id, window, cx);
        div()
            .flex()
            .flex_col()
            .flex_1()
            .min_h_0()
            .min_w_0()
            .child(banner)
            .child(div().flex().flex_1().min_h_0().min_w_0().child(results))
            .into_any_element()
    }

    fn render_results_inner(
        &mut self,
        tab_id: u64,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let body = self.render_results_body(tab_id, window, cx);
        let has_plan = self
            .workspace
            .runtime(tab_id)
            .is_some_and(|runtime| runtime.plan.is_some());
        if self.grid(tab_id).column_count() == 0 && !has_plan {
            self.view(tab_id).results_bounds.set(None);
            return body;
        }
        let rail = self.render_columns_rail(tab_id, cx);
        // Measures the results area (rail + presentation) for the popup clamp;
        // a refresh is requested only when the bounds actually changed.
        let bounds_cell = self.view(tab_id).results_bounds.clone();
        let bounds_probe = gpui::canvas(
            move |bounds, window, _| {
                let previous = bounds_cell.replace(Some(bounds));
                if previous != Some(bounds) {
                    window.refresh();
                }
            },
            |_, _, _, _| {},
        )
        .absolute()
        .size_full();
        div()
            .debug_selector(|| "results-with-columns-panel".into())
            .relative()
            .flex()
            .flex_1()
            .min_h_0()
            .min_w_0()
            .child(bounds_probe)
            .child(rail)
            .child(div().flex().flex_1().min_h_0().min_w_0().child(body))
            .into_any_element()
    }
}
