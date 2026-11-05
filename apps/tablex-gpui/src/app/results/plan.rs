//! The Plan presentation of a SQL tab's results: the last Explain run as a flat, virtualized list
//! of plan nodes (indentation by depth, disclosure to collapse a subtree, a cost bar, estimated
//! and actual rows, time, loops), a detail pane for the selected node, and a read-only Raw view.
//!
//! The list is one `uniform_list` (only the rows in the viewport are built) with one mouse and one
//! key handler on the container: a click is mapped to a row from the painted list bounds, as in
//! the Error Log and the Driver Manager. The state (collapsed set, selection) lives in
//! `crate::plan_view::PlanView` on the tab's runtime; no row is an entity or owns a listener.

use super::super::*;
use crate::columns_panel::Presentation;
use crate::plan_view::{fmt_cost, fmt_ms, fmt_rows, PlanView};
use engine_plan::ExplainMode;
use gpui::{relative, ClipboardItem, MouseButton, MouseDownEvent};
use gpui_component::button::ButtonGroup;

/// Per-level indentation and the width of the disclosure box, in pixels.
const INDENT: f32 = 14.0;
const DISCLOSURE: f32 = 16.0;
const ROW_PADDING: f32 = 8.0;
const OBJECT_WIDTH: f32 = 110.0;
const COST_WIDTH: f32 = 112.0;
const NUMBER_WIDTH: f32 = 68.0;
const LOOPS_WIDTH: f32 = 54.0;
const DETAIL_WIDTH: f32 = 280.0;
const HEADER_ROW_HEIGHT: f32 = 28.0;

#[cfg(test)]
thread_local! {
    /// How many list rows were built (tests prove that only the viewport's rows are).
    pub(in crate::app) static PLAN_ROWS_BUILT: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
}

/// Read-only text of `Plan::raw`, refilled when another plan is shown.
pub(in crate::app) struct PlanRawEditor {
    plan_id: u64,
    state: gpui::Entity<EditorState>,
}

#[cfg(test)]
impl PlanRawEditor {
    pub(in crate::app) fn state_for_test(&self) -> gpui::Entity<EditorState> {
        self.state.clone()
    }
}

impl AppRoot {
    pub(in crate::app) fn plan_presentation_active(&self, tab_id: u64) -> bool {
        self.workspace.runtime(tab_id).is_some_and(|runtime| {
            runtime.columns_panel.presentation == Presentation::Plan && runtime.plan.is_some()
        })
    }

    /// Rail "Plan": the Explain plan replaces the grid area.
    pub(in crate::app) fn show_plan_presentation(&mut self, tab_id: u64, cx: &mut Context<Self>) {
        if let Some(runtime) = self.workspace.runtime_mut(tab_id) {
            if runtime.plan.is_some() {
                runtime.columns_panel.presentation = Presentation::Plan;
            }
        }
        self.notify_tab(tab_id, cx);
    }

    fn plan_of_mut(&mut self, tab_id: u64) -> Option<&mut PlanView> {
        self.workspace
            .runtime_mut(tab_id)
            .and_then(|runtime| runtime.plan.as_mut())
    }

    pub(in crate::app) fn select_plan_node(&mut self, tab_id: u64, node: usize, cx: &mut Context<Self>) {
        let Some(view) = self.plan_of_mut(tab_id) else {
            return;
        };
        view.select(node);
        let position = view.position_of(node);
        if let Some(position) = position {
            self.view(tab_id)
                .plan_scroll
                .scroll_to_item(position, gpui::ScrollStrategy::Nearest);
        }
        self.notify_tab(tab_id, cx);
    }

    /// Collapses or expands the subtree of `node`.
    pub(in crate::app) fn toggle_plan_node(&mut self, tab_id: u64, node: usize, cx: &mut Context<Self>) {
        if self.plan_of_mut(tab_id).is_some_and(|view| view.toggle(node)) {
            self.notify_tab(tab_id, cx);
        }
    }

    pub(in crate::app) fn set_plan_raw(&mut self, tab_id: u64, raw: bool, cx: &mut Context<Self>) {
        if let Some(view) = self.plan_of_mut(tab_id) {
            view.show_raw = raw;
        }
        self.notify_tab(tab_id, cx);
    }

    /// Copies the server's output exactly as received.
    pub(in crate::app) fn copy_plan_raw(&mut self, tab_id: u64, cx: &mut Context<Self>) {
        let raw = self
            .workspace
            .runtime(tab_id)
            .and_then(|runtime| runtime.plan.as_ref())
            .map(|view| view.plan.raw.clone());
        if let Some(raw) = raw {
            cx.write_to_clipboard(ClipboardItem::new_string(raw));
        }
    }

    /// Keyboard navigation of the node list: up / down move the selection, left collapses (or
    /// selects the parent), right expands (or enters the first child).
    fn plan_key(&mut self, tab_id: u64, key: &str, cx: &mut Context<Self>) -> bool {
        let Some(view) = self.plan_of_mut(tab_id) else {
            return false;
        };
        let target = match key {
            "down" => view.step_target(1),
            "up" => view.step_target(-1),
            "left" => match view.selected {
                Some(node) if view.has_children(node) && !view.is_collapsed(node) => {
                    view.toggle(node);
                    None
                }
                Some(node) => view.node(node).and_then(|node| node.parent),
                None => None,
            },
            "right" => match view.selected {
                Some(node) if view.is_collapsed(node) => {
                    view.toggle(node);
                    None
                }
                Some(node) if view.has_children(node) => Some(node + 1),
                _ => None,
            },
            _ => return false,
        };
        match target {
            Some(node) => self.select_plan_node(tab_id, node, cx),
            None => self.notify_tab(tab_id, cx),
        }
        true
    }

    /// Row (and whether the disclosure box) under a window-space point, from the painted list
    /// bounds and its scroll offset (the scheme of the Error Log).
    fn plan_row_at(&self, tab_id: u64, point: gpui::Point<gpui::Pixels>) -> Option<(usize, bool)> {
        let view = self.workspace.runtime(tab_id)?.plan.as_ref()?;
        let (bounds, offset) = {
            let state = self.view(tab_id).plan_scroll.0.borrow();
            (state.base_handle.bounds(), state.base_handle.offset())
        };
        if !bounds.contains(&point) {
            return None;
        }
        let y = f32::from(point.y - bounds.origin.y - offset.y);
        if y < 0.0 {
            return None;
        }
        let node = view.visible_node((y / theme::ROW_HEIGHT) as usize)?;
        let x = f32::from(point.x - bounds.origin.x);
        let depth = view.node(node)?.depth as f32;
        let start = ROW_PADDING + depth * INDENT;
        Some((node, x >= start && x < start + DISCLOSURE))
    }

    /// Creates / refills the Raw editor while the Raw view is on. Called before the tab's
    /// results render (it needs the `Window`); an unchanged plan costs one id comparison.
    pub(in crate::app) fn sync_plan_view(&mut self, tab_id: u64, window: &mut Window, cx: &mut Context<Self>) {
        let Some(view) = self
            .workspace
            .runtime(tab_id)
            .and_then(|runtime| runtime.plan.as_ref())
            .filter(|view| view.show_raw)
        else {
            return;
        };
        let current = self
            .tab_views
            .get(&tab_id)
            .and_then(|tab_view| tab_view.plan_raw.as_ref())
            .map(|editor| editor.plan_id);
        if current == Some(view.id) {
            return;
        }
        let (plan_id, raw) = (view.id, view.plan.raw.clone());
        match self.view_mut(tab_id).plan_raw.as_mut() {
            Some(editor) => {
                editor.plan_id = plan_id;
                let language = raw_language(&raw);
                editor.state.update(cx, |state, cx| {
                    state.set_highlighter(language, cx);
                    state.set_value(raw, window, cx);
                });
            }
            None => {
                let state = cx.new(|cx| {
                    EditorState::new(window, cx)
                        .language(raw_language(&raw))
                        .line_number(true)
                        .soft_wrap(false)
                        .default_value(raw)
                });
                self.view_mut(tab_id).plan_raw = Some(PlanRawEditor { plan_id, state });
            }
        }
    }

    /// The Plan presentation: summary header, then the node list with its detail pane, or the Raw
    /// text.
    pub(in crate::app) fn render_plan_view(&mut self, tab_id: u64, cx: &mut Context<Self>) -> AnyElement {
        let Some(view) = self
            .workspace
            .runtime(tab_id)
            .and_then(|runtime| runtime.plan.as_ref())
        else {
            return div().into_any_element();
        };
        let show_raw = view.show_raw;
        let header = self.render_plan_header(tab_id, view, cx);
        let body = if show_raw {
            self.render_plan_raw(tab_id)
        } else {
            self.render_plan_nodes(tab_id, cx)
        };
        div()
            .id(("plan-view", tab_id))
            .debug_selector(|| "plan-view".into())
            .size_full()
            .flex()
            .flex_col()
            .bg(theme::background())
            .child(header)
            .child(body)
            .into_any_element()
    }

    fn render_plan_header(&self, tab_id: u64, view: &PlanView, cx: &mut Context<Self>) -> AnyElement {
        let plan = &view.plan;
        let stat = |selector: &'static str, label: &'static str, value: String| {
            div()
                .debug_selector(move || selector.into())
                .flex()
                .items_center()
                .gap(px(4.0))
                .flex_shrink_0()
                .child(div().text_color(theme::text_muted()).child(label))
                .child(div().text_color(theme::text()).child(value))
        };
        let mode = match view.mode {
            ExplainMode::Estimate => "Estimate",
            ExplainMode::Analyze => "Analyze",
        };
        let raw = view.show_raw;
        let toggle = ButtonGroup::new(("plan-view-mode", tab_id))
            .outline()
            .compact()
            .with_size(Size::XSmall)
            .children([("plan-view-tree", "Plan", !raw), ("plan-view-raw", "Raw", raw)].map(
                |(selector, label, selected)| {
                    Button::new((selector, tab_id))
                        .debug_selector(move || selector.into())
                        .label(label)
                        .selected(selected)
                },
            ))
            .on_click({
                let root = cx.entity().downgrade();
                move |selected, _, app| {
                    let Some(index) = selected.first() else {
                        return;
                    };
                    let _ = root.update(app, |this, cx| this.set_plan_raw(tab_id, *index == 1, cx));
                }
            });
        let copy = Button::new(("plan-copy", tab_id))
            .debug_selector(|| "plan-copy".into())
            .icon(Icon::empty().path("icons/copy.svg").size(px(12.0)))
            .label("Copy")
            .with_size(Size::XSmall)
            .compact()
            .outline()
            .tooltip("Copy the raw plan")
            .on_click(cx.listener(move |this, _, _, cx| this.copy_plan_raw(tab_id, cx)));
        let summary = div()
            .debug_selector(|| "plan-summary".into())
            .flex_1()
            .min_w_0()
            .flex()
            .flex_wrap()
            .items_center()
            .gap_x(px(14.0))
            .child(
                div()
                    .id(("plan-title", tab_id))
                    .debug_selector(|| "plan-title".into())
                    // The statement that produced the plan, as sent to the server.
                    .tooltip({
                        let sql = view.sql.clone();
                        move |window, cx| gpui_component::tooltip::Tooltip::new(sql.clone()).build(window, cx)
                    })
                    .flex_shrink_0()
                    .text_color(theme::text())
                    .font_weight(gpui::FontWeight::MEDIUM)
                    .child(format!("{} {mode}", view.dialect.name())),
            )
            .children(plan.total_cost.map(|cost| stat("plan-total-cost", "Total cost", fmt_cost(cost))))
            .children(plan.planning_ms.map(|ms| stat("plan-planning-time", "Planning", format!("{} ms", fmt_ms(ms)))))
            .children(plan.execution_ms.map(|ms| stat("plan-execution-time", "Execution", format!("{} ms", fmt_ms(ms)))))
            .child(stat("plan-node-count", "Nodes", fmt_rows(plan.nodes.len() as f64)));
        let warnings = (!plan.warnings.is_empty()).then(|| {
            div()
                .debug_selector(|| "plan-warnings".into())
                .flex()
                .flex_col()
                .gap(px(2.0))
                .children(plan.warnings.iter().enumerate().map(|(index, warning)| {
                    div()
                        .debug_selector(move || format!("plan-warning-{index}"))
                        .flex()
                        .items_start()
                        .gap(px(6.0))
                        .text_color(theme::warning())
                        .child(
                            Icon::empty()
                                .path("icons/triangle-alert.svg")
                                .size(px(12.0))
                                .flex_shrink_0(),
                        )
                        .child(div().min_w_0().child(warning.clone()))
                }))
        });
        div()
            .debug_selector(|| "plan-header".into())
            .flex_shrink_0()
            .flex()
            .flex_col()
            .gap(px(4.0))
            .px(px(12.0))
            .py(px(6.0))
            .bg(theme::chrome())
            .border_b_1()
            .border_color(theme::border())
            .text_size(px(12.0))
            .child(
                div()
                    .flex()
                    .items_center()
                    .gap(px(10.0))
                    .child(summary)
                    .child(toggle)
                    .child(copy),
            )
            .children(warnings)
            .into_any_element()
    }

    fn render_plan_raw(&self, tab_id: u64) -> AnyElement {
        let editor = self
            .tab_views
            .get(&tab_id)
            .and_then(|view| view.plan_raw.as_ref())
            .map(|editor| editor.state.clone());
        match editor {
            Some(state) => div()
                .debug_selector(|| "plan-raw-editor".into())
                .flex_1()
                .min_h_0()
                .font_family("Menlo")
                .text_size(px(12.0))
                .child(Editor::new(&state).readonly(true).appearance(false).bordered(false).h_full())
                .into_any_element(),
            None => div().flex_1().into_any_element(),
        }
    }

    fn render_plan_nodes(&mut self, tab_id: u64, cx: &mut Context<Self>) -> AnyElement {
        let focus = self.view_mut(tab_id).plan_focus(cx);
        let Some(view) = self
            .workspace
            .runtime(tab_id)
            .and_then(|runtime| runtime.plan.as_ref())
        else {
            return div().into_any_element();
        };
        let columns = view.columns;
        let cell = |width: f32| div().w(px(width)).flex_shrink_0().px(px(6.0)).overflow_hidden().whitespace_nowrap();
        let header = div()
            .debug_selector(|| "plan-columns-header".into())
            .w_full()
            .h(px(HEADER_ROW_HEIGHT))
            .flex_shrink_0()
            .flex()
            .items_center()
            .bg(theme::grid_header())
            .border_b_1()
            .border_color(theme::border())
            .text_size(px(11.0))
            .text_color(theme::text_muted())
            .child(div().flex_1().min_w(px(140.0)).pl(px(ROW_PADDING + DISCLOSURE)).child("Operation"))
            .child(cell(OBJECT_WIDTH).child("Object"))
            .when(columns.cost, |this| this.child(cell(COST_WIDTH).child("Cost")))
            .child(cell(NUMBER_WIDTH).text_right().child("Est. rows"))
            .when(columns.actual_rows, |this| this.child(cell(NUMBER_WIDTH).text_right().child("Actual rows")))
            .when(columns.time, |this| this.child(cell(NUMBER_WIDTH).text_right().child("Time (ms)")))
            .when(columns.loops, |this| this.child(cell(LOOPS_WIDTH).text_right().child("Loops")));
        let rows = uniform_list(
            ("plan-rows", tab_id),
            view.visible_len(),
            cx.processor(move |this, range: Range<usize>, _window, _cx| {
                let Some(view) = this
                    .workspace
                    .runtime(tab_id)
                    .and_then(|runtime| runtime.plan.as_ref())
                else {
                    return Vec::new();
                };
                range
                    .filter_map(|position| plan_row(view, view.visible_node(position)?))
                    .collect()
            }),
        )
        .flex_1()
        .min_h_0()
        .track_scroll(&self.view(tab_id).plan_scroll);
        let list = div()
            .id(("plan-list", tab_id))
            .debug_selector(|| "plan-list".into())
            .key_context("PlanView")
            .track_focus(&focus)
            .flex_1()
            .min_w_0()
            .min_h_0()
            .flex()
            .flex_col()
            .on_key_down(cx.listener(move |this, event: &gpui::KeyDownEvent, _, cx| {
                if this.plan_key(tab_id, event.keystroke.key.as_str(), cx) {
                    cx.stop_propagation();
                }
            }))
            .child(header)
            .child(
                div()
                    .relative()
                    .flex_1()
                    .min_h_0()
                    .flex()
                    .flex_col()
                    .on_mouse_down(
                        MouseButton::Left,
                        cx.listener(move |this, event: &MouseDownEvent, window, cx| {
                            this.view_mut(tab_id).plan_focus(cx).focus(window, cx);
                            let Some((node, disclosure)) = this.plan_row_at(tab_id, event.position) else {
                                return;
                            };
                            this.select_plan_node(tab_id, node, cx);
                            if disclosure || event.click_count >= 2 {
                                this.toggle_plan_node(tab_id, node, cx);
                            }
                        }),
                    )
                    .child(if view.visible_len() == 0 {
                        div()
                            .flex_1()
                            .flex()
                            .items_center()
                            .justify_center()
                            .text_color(theme::text_muted())
                            .child("The plan has no nodes")
                            .into_any_element()
                    } else {
                        rows.into_any_element()
                    })
                    .child(Scrollbar::vertical(&self.view(tab_id).plan_scroll)),
            );
        div()
            .flex_1()
            .min_h_0()
            .flex()
            .child(list)
            .child(self.render_plan_detail(view))
            .into_any_element()
    }

    /// Every field of the selected node, then the server's own properties.
    fn render_plan_detail(&self, view: &PlanView) -> AnyElement {
        let pane = div()
            .debug_selector(|| "plan-detail".into())
            .w(px(DETAIL_WIDTH))
            .flex_shrink_0()
            .h_full()
            .flex()
            .flex_col()
            .border_l_1()
            .border_color(theme::border())
            .bg(theme::background())
            .text_size(px(12.0));
        let Some(node) = view.selected.and_then(|selected| view.node(selected)) else {
            return pane
                .items_center()
                .justify_center()
                .text_color(theme::text_muted())
                .child("Select a node to see its fields")
                .into_any_element();
        };
        let fields = PlanView::detail_fields(node);
        let typed = fields.len() - node.props.len();
        pane.child(
            div()
                .h(px(HEADER_ROW_HEIGHT))
                .flex_shrink_0()
                .px(px(10.0))
                .flex()
                .items_center()
                .bg(theme::grid_header())
                .border_b_1()
                .border_color(theme::border())
                .text_size(px(11.0))
                .text_color(theme::text_muted())
                .child("Node"),
        )
        .child(
            div()
                .id("plan-detail-scroll")
                .flex_1()
                .min_h_0()
                .overflow_y_scroll()
                .p(px(10.0))
                .flex()
                .flex_col()
                .gap(px(4.0))
                .children(fields.into_iter().enumerate().map(|(index, (label, value))| {
                    let misestimate = PlanView::is_misestimated(node)
                        && (label == "Est. rows" || label == "Actual rows");
                    div()
                        .debug_selector(move || {
                            if index < typed {
                                format!("plan-detail-field-{index}")
                            } else {
                                format!("plan-detail-prop-{}", index - typed)
                            }
                        })
                        .when(index == typed && typed > 0, |this| {
                            this.mt(px(6.0)).pt(px(6.0)).border_t_1().border_color(theme::divider())
                        })
                        .flex()
                        .items_start()
                        .gap(px(8.0))
                        .child(
                            div()
                                .w(px(96.0))
                                .flex_shrink_0()
                                .text_size(px(11.0))
                                .text_color(theme::text_muted())
                                .child(label),
                        )
                        .child(
                            div()
                                .flex_1()
                                .min_w_0()
                                .whitespace_normal()
                                .text_color(if misestimate { theme::warning() } else { theme::text() })
                                .child(value),
                        )
                })),
        )
        .into_any_element()
    }
}

/// Raw text above this size is shown as plain text: building the highlighter's tree for megabytes
/// of JSON would hold the UI thread.
const RAW_HIGHLIGHT_LIMIT: usize = 1024 * 1024;

/// The highlighter for the Raw view: JSON for JSON output (up to [`RAW_HIGHLIGHT_LIMIT`]), plain
/// text for MySQL's tree, SQLite's rows and anything large.
fn raw_language(raw: &str) -> &'static str {
    if raw.len() <= RAW_HIGHLIGHT_LIMIT && raw.trim_start().starts_with(['{', '[']) {
        "json"
    } else {
        "text"
    }
}

/// One row of the list. Reads only the node it shows.
fn plan_row(view: &PlanView, index: usize) -> Option<AnyElement> {
    #[cfg(test)]
    PLAN_ROWS_BUILT.with(|built| built.set(built.get() + 1));
    let node = view.node(index)?;
    let columns = view.columns;
    let selected = view.selected == Some(index);
    let misestimate = PlanView::is_misestimated(node);
    let number = |width: f32, value: Option<f64>, color: gpui::Rgba| {
        div()
            .w(px(width))
            .flex_shrink_0()
            .px(px(6.0))
            .text_right()
            .whitespace_nowrap()
            .text_color(color)
            .child(value.map(fmt_rows).unwrap_or_default())
    };
    let rows_color = if misestimate { theme::warning() } else { theme::text() };
    let disclosure = if view.has_children(index) {
        Icon::empty()
            .path(if view.is_collapsed(index) {
                "icons/chevron-right.svg"
            } else {
                "icons/chevron-down.svg"
            })
            .size(px(12.0))
            .text_color(theme::text_muted())
            .into_any_element()
    } else {
        div().into_any_element()
    };
    Some(
        div()
            .id(("plan-row", index))
            .debug_selector(move || format!("plan-row-{index}"))
            .relative()
            .w_full()
            .h(px(theme::ROW_HEIGHT))
            .flex()
            .items_center()
            .text_size(px(12.0))
            .border_b_1()
            .border_color(theme::divider())
            .when(misestimate, |this| this.bg(theme::warning().opacity(0.12)))
            .when(selected, |this| this.bg(theme::accent_surface()))
            .when(!selected && !misestimate, |this| this.hover(|style| style.bg(theme::hover())))
            .when(misestimate, |this| {
                this.child(
                    div()
                        .debug_selector(move || format!("plan-misestimate-{index}"))
                        .absolute()
                        .left_0()
                        .top_0()
                        .bottom_0()
                        .w(px(3.0))
                        .bg(theme::warning()),
                )
            })
            .child(
                div()
                    .flex_1()
                    .w_0()
                    .min_w(px(140.0))
                    .flex()
                    .items_center()
                    .overflow_hidden()
                    .pl(px(ROW_PADDING + node.depth as f32 * INDENT))
                    .child(
                        div()
                            .debug_selector(move || format!("plan-disclosure-{index}"))
                            .w(px(DISCLOSURE))
                            .flex_shrink_0()
                            .flex()
                            .items_center()
                            .child(disclosure),
                    )
                    // The wrapper takes what the disclosure leaves. In it the operation keeps its
                    // width while the detail beside it truncates first; the operation itself
                    // ends in an ellipsis only when the wrapper is too narrow for it alone.
                    .child(
                        div()
                            .flex_1()
                            .min_w_0()
                            .flex()
                            .items_center()
                            .child(
                                div()
                                    .flex_shrink_0()
                                    .max_w_full()
                                    .truncate()
                                    .text_color(theme::text())
                                    .font_weight(gpui::FontWeight::MEDIUM)
                                    .child(node.operation.clone()),
                            )
                            .children(node.detail.as_ref().map(|detail| {
                                div()
                                    .min_w_0()
                                    .truncate()
                                    .pl(px(8.0))
                                    .text_color(theme::text_faint())
                                    .child(detail.clone())
                            })),
                    ),
            )
            .child(
                div()
                    .w(px(OBJECT_WIDTH))
                    .flex_shrink_0()
                    .px(px(6.0))
                    .truncate()
                    .text_color(theme::text_muted())
                    .child(node.object.clone().unwrap_or_default()),
            )
            .when(columns.cost, |this| this.child(cost_cell(view, node)))
            .child(number(NUMBER_WIDTH, node.est_rows, rows_color))
            .when(columns.actual_rows, |this| {
                this.child(number(NUMBER_WIDTH, node.actual_rows, rows_color))
            })
            .when(columns.time, |this| {
                this.child(
                    div()
                        .w(px(NUMBER_WIDTH))
                        .flex_shrink_0()
                        .px(px(6.0))
                        .text_right()
                        .whitespace_nowrap()
                        .text_color(theme::text())
                        .child(node.actual_time_ms.map(fmt_ms).unwrap_or_default()),
                )
            })
            .when(columns.loops, |this| {
                this.child(number(LOOPS_WIDTH, node.loops.map(|loops| loops as f64), theme::text_muted()))
            })
            .into_any_element(),
    )
}

/// The cost bar: the node's own share of the plan's cost in the accent colour over its cumulative
/// share (the node and everything under it) in a lighter one, with the own share as a percentage.
fn cost_cell(view: &PlanView, node: &engine_plan::PlanNode) -> impl IntoElement {
    let (own, cumulative) = view.cost_shares(node).unwrap_or_default();
    let label = if own <= 0.0 {
        String::new()
    } else if own < 0.01 {
        "<1%".to_string()
    } else {
        format!("{:.0}%", own * 100.0)
    };
    div()
        .w(px(COST_WIDTH))
        .flex_shrink_0()
        .px(px(6.0))
        .flex()
        .items_center()
        .gap(px(6.0))
        .child(
            div()
                .relative()
                .w(px(56.0))
                .h(px(8.0))
                .flex_shrink_0()
                .rounded(px(2.0))
                .bg(theme::border())
                .child(
                    div()
                        .absolute()
                        .left_0()
                        .top_0()
                        .h_full()
                        .w(relative(cumulative))
                        .rounded(px(2.0))
                        .bg(theme::accent().opacity(0.4)),
                )
                .child(
                    div()
                        .absolute()
                        .left_0()
                        .top_0()
                        .h_full()
                        .w(relative(own))
                        .rounded(px(2.0))
                        .bg(theme::accent()),
                ),
        )
        .child(
            div()
                .text_size(px(11.0))
                .text_color(theme::text_muted())
                .whitespace_nowrap()
                .child(label),
        )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn raw_text_is_highlighted_only_when_it_is_json_and_small() {
        assert_eq!(raw_language("[\n {\"Plan\": {}}\n]"), "json");
        assert_eq!(raw_language("-> Limit: 10 row(s)"), "text");
        let big = format!("[\"{}\"]", "x".repeat(RAW_HIGHLIGHT_LIMIT));
        assert_eq!(raw_language(&big), "text", "megabytes of JSON stay plain text");
    }
}
