use super::super::*;
use super::detail::CellDetailEditor;
use gpui_component::button::{ButtonCustomVariant, ButtonGroup};

/// The convert preview applies only to the tab and cell it was chosen for
/// (and only while its kind is still offered for the cell's value).
pub(in crate::app) fn scoped_convert(
    convert: Option<(u64, CellPosition, crate::cell_value::ConvertKind)>,
    tab_id: u64,
    position: CellPosition,
    options: &[crate::cell_value::ConvertKind],
) -> Option<crate::cell_value::ConvertKind> {
    convert
        .filter(|(convert_tab, convert_position, kind)| {
            *convert_tab == tab_id && *convert_position == position && options.contains(kind)
        })
        .map(|(_, _, kind)| kind)
}

impl AppRoot {
    pub(in crate::app) fn render_cell_detail_column(
        &self,
        column: Option<&CellDetailColumn>,
    ) -> AnyElement {
        let Some(column) = column else {
            return cell_detail_empty_state("cell-detail-column-panel", "Select a column");
        };
        let rows = column.property_rows();
        div()
            .id("cell-detail-column-panel")
            .debug_selector(|| "cell-detail-column-panel".into())
            .flex_1()
            .min_h_0()
            .overflow_y_scrollbar()
            .p(px(12.0))
            .flex()
            .flex_col()
            .gap(px(6.0))
            .text_size(px(11.0))
            .children(rows.into_iter().map(|(name, value)| {
                property_row(name, 88.0, theme::text(), value, None)
            }))
            .into_any_element()
    }
    pub(in crate::app) fn render_cell_detail_rail(
        &self,
        tab_id: u64,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let active_tab = self
            .tab_cell_detail(tab_id)
            .and_then(|state| state.open.then_some(state.tab));
        let open_tabs: Vec<CellDetailTab> = self
            .tab_cell_detail(tab_id)
            .filter(|state| state.open)
            .map(|state| state.opened_tabs.clone())
            .unwrap_or_default();
        let button = |selector: &'static str,
                      path: &'static str,
                      label: &'static str,
                      tab: CellDetailTab,
                      cell_height: f32| {
            let active = active_tab == Some(tab);
            let open = open_tabs.contains(&tab);
            let control = Button::new((selector, tab_id))
                .icon(Icon::empty().path(path).size(px(16.0)))
                .with_size(Size::Small)
                .compact()
                .custom(chrome_button_variant(open, cx))
                .rounded(px(4.0))
                .w(px(28.0))
                .h(px(28.0))
                .selected(open)
                .tooltip(label)
                .on_click(cx.listener(move |this, _, _, cx| {
                    this.toggle_cell_detail_tab(tab_id, tab, cx);
                }));
            div()
                .relative()
                .w_full()
                .h(px(cell_height))
                .flex()
                .items_center()
                .justify_center()
                .flex_shrink_0()
                .when(active, |this| {
                    this.child(
                        div()
                            .absolute()
                            .left_0()
                            .top(px((cell_height - 20.0) / 2.0))
                            .w(px(2.0))
                            .h(px(20.0))
                            .rounded_r(px(1.0))
                            .bg(theme::accent()),
                    )
                })
                .child(div().debug_selector(move || selector.into()).child(control))
        };

        div()
            .id(("cell-detail-rail", tab_id))
            .debug_selector(|| "cell-detail-rail".into())
            .w(px(36.0))
            .h_full()
            .flex()
            .flex_col()
            .items_center()
            .flex_shrink_0()
            .bg(theme::chrome())
            .border_l_1()
            .border_color(theme::border())
            .child(
                div()
                    .debug_selector(|| "cell-detail-rail-header".into())
                    .w_full()
                    .h(px(theme::RESULT_GRID_HEADER_HEIGHT))
                    .flex()
                    .flex_col()
                    .flex_shrink_0()
                    .child(button(
                        "cell-detail-rail-details",
                        "icons/frame.svg",
                        "Cell details",
                        CellDetailTab::Details,
                        theme::RESULT_GRID_HEADER_HEIGHT - 1.0,
                    ))
                    .child(div().w_full().h(px(1.0)).flex_shrink_0().bg(theme::border())),
            )
            .child(
                div()
                    .w_full()
                    .flex()
                    .flex_col()
                    .items_center()
                    .gap(px(2.0))
                    .pt(px(6.0))
                    .child(button(
                        "cell-detail-rail-column",
                        "icons/columns-3.svg",
                        "Column details",
                        CellDetailTab::Column,
                        32.0,
                    ))
                    .child(button(
                        "cell-detail-rail-row",
                        "icons/list-checks.svg",
                        "Row details",
                        CellDetailTab::Row,
                        32.0,
                    )),
            )
            .into_any_element()
    }

    pub(in crate::app) fn render_cell_detail_row(
        &self,
        tab_id: u64,
        state: &crate::cell_detail::CellDetailState,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let Some(row) = state.row.as_ref() else {
            return div()
                .debug_selector(|| "cell-detail-row-panel".into())
                .flex_1()
                .items_center()
                .justify_center()
                .text_color(theme::text_muted())
                .child("Select a row to inspect")
                .into_any_element();
        };
        // Only the visible range is materialized; the flattened rows are
        // cached in the state and rebuilt on expansion/row changes.
        let rows = state.row_tree.clone();
        let tree_list = uniform_list(
            ("cell-detail-row-tree", tab_id),
            rows.len(),
            cx.processor(move |this, range: Range<usize>, _window, cx| {
                let runtime = this.workspace.runtime(tab_id);
                range
                    .map(|index| {
                        let tree_row = &rows[index];
                        let expanded = tree_row.container
                            && runtime.is_some_and(|runtime| {
                                runtime.cell_detail.row_expanded.contains(&tree_row.path)
                            });
                        let indent = tree_row.depth as f32 * ROW_TREE_INDENT;
                        let mut element = div()
                            .id(("cell-detail-tree-row", index))
                            .debug_selector(|| "cell-detail-tree-row".into())
                            .h(px(ROW_TREE_HEIGHT))
                            .w_full()
                            .flex()
                            .items_center()
                            .gap(px(6.0))
                            .child(
                                div()
                                    .w(px(ROW_TREE_KEY_WIDTH.max(indent + 62.0)))
                                    .flex_shrink_0()
                                    .flex()
                                    .items_center()
                                    .pl(px(indent))
                                    .child(
                                        div()
                                            .w(px(16.0))
                                            .flex_shrink_0()
                                            .text_color(theme::text_muted())
                                            .when(tree_row.container, |this| {
                                                this.child(
                                                    Icon::new(if expanded {
                                                        IconName::ChevronDown
                                                    } else {
                                                        IconName::ChevronRight
                                                    })
                                                    .size(px(12.0)),
                                                )
                                            }),
                                    )
                                    .child(
                                        div()
                                            .flex_1()
                                            .min_w(px(0.0))
                                            .truncate()
                                            .text_color(theme::accent())
                                            .child(tree_row.key.clone()),
                                    ),
                            )
                            .child(
                                div()
                                    .flex_1()
                                    .min_w(px(0.0))
                                    .truncate()
                                    .text_color(row_tree_value_color(
                                        &tree_row.type_name,
                                        tree_row.container,
                                    ))
                                    .child(tree_row.value.clone()),
                            )
                            .child(
                                div()
                                    .w(px(ROW_TREE_TYPE_WIDTH))
                                    .flex_shrink_0()
                                    .truncate()
                                    .text_right()
                                    .text_size(px(10.0))
                                    .text_color(row_tree_type_color(&tree_row.type_name))
                                    .child(tree_row.type_name.to_string()),
                            );
                        if tree_row.container {
                            let path = tree_row.path.clone();
                            element = element
                                .cursor_pointer()
                                .hover(|style| style.bg(theme::hover()))
                                .on_click(cx.listener(move |this, _, _, cx| {
                                    this.toggle_cell_detail_row_path(tab_id, &path, cx);
                                }));
                        }
                        element.into_any_element()
                    })
                    .collect()
            }),
        )
        .flex_1()
        .min_h_0()
        .track_scroll(&self.view(tab_id).cell_detail_row_scroll);

        let is_mongo = self
            .workspace
            .tab(tab_id)
            .is_some_and(|tab| tab.query_mode.is_mongo_find());
        let view = state.row_view;
        let root = cx.entity().downgrade();
        let view_toggle = {
            use crate::cell_detail::RowView;
            const OPTIONS: [(RowView, &str, &str); 2] = [
                (RowView::Tree, "Tree", "cell-detail-row-view-tree"),
                (RowView::Json, "JSON", "cell-detail-row-view-json"),
            ];
            ButtonGroup::new(("cell-detail-row-view", tab_id))
                .outline()
                .compact()
                .with_size(Size::XSmall)
                .children(OPTIONS.iter().enumerate().map(|(index, (option, label, selector))| {
                    Button::new(("cell-detail-row-view-option", index))
                        .label(*label)
                        .debug_selector(|| (*selector).into())
                        .selected(view == *option)
                }))
                .on_click({
                    let root = root.clone();
                    move |selected, _, app| {
                        if let Some((option, _, _)) =
                            selected.first().and_then(|index| OPTIONS.get(*index))
                        {
                            let option = *option;
                            let _ = root.update(app, |this, cx| {
                                this.set_cell_detail_row_view(tab_id, option, cx);
                            });
                        }
                    }
                })
        };
        let body: AnyElement = match view {
            crate::cell_detail::RowView::Tree => tree_list.into_any_element(),
            crate::cell_detail::RowView::Json => match self.view(tab_id).json_views.cell_json.as_ref() {
                // Official code editor (tree-sitter JSON highlighting),
                // read-only and unwrapped: virtualized, scrolls horizontally.
                Some(viewer) => div()
                    .id("cell-detail-json-panel")
                    .debug_selector(|| "cell-detail-json-panel".into())
                    .flex_1()
                    .min_h_0()
                    .child(
                        Editor::new(&viewer.state)
                            .readonly(true)
                            .appearance(false)
                            .bordered(false)
                            .h_full(),
                    )
                    .into_any_element(),
                None => div().flex_1().into_any_element(),
            },
        };
        div()
            .id("cell-detail-row-panel")
            .debug_selector(|| "cell-detail-row-panel".into())
            .size_full()
            .flex()
            .flex_col()
            .py(px(8.0))
            .font_family("Menlo")
            .text_size(px(12.0))
            .child(
                div()
                    .flex_shrink_0()
                    .flex()
                    .items_center()
                    .gap(px(6.0))
                    .px(px(12.0))
                    .pb(px(6.0))
                    .font_family("Inter")
                    .text_size(px(11.0))
                    .text_color(theme::text_muted())
                    .child(
                        div()
                            .flex_1()
                            .min_w_0()
                            .truncate()
                            .child(format!("Row {} · read-only", row.row_number)),
                    )
                    .when(
                        is_mongo && view == crate::cell_detail::RowView::Json,
                        |this| {
                            this.child(div().debug_selector(|| "cell-detail-json-mode".into()).child(
                                super::json_view::ejson_mode_toggle(
                                    ("cell-detail-json-mode-toggle", tab_id),
                                    state.row_json_mode,
                                    root.clone(),
                                    move |this, mode| {
                                        if let Some(runtime) = this.workspace.runtime_mut(tab_id) {
                                            runtime.cell_detail.row_json_mode = mode;
                                        }
                                    },
                                ),
                            ))
                        },
                    )
                    .child(view_toggle),
            )
            .child(
                div()
                    .flex_1()
                    .min_h_0()
                    .flex()
                    .flex_col()
                    .when(view == crate::cell_detail::RowView::Tree, |this| this.px(px(12.0)))
                    .child(body),
            )
            .into_any_element()
    }

    /// One header tab: label button + close button. The close
    /// button owns a reserved 16x16 slot (no layout shift), sits at least 6px
    /// from the label, and shows on the active tab and on hover; compact
    /// (icon-only) tabs show it on hover only so a click on the icon can never
    /// hit it. Middle-click closes; right-click offers Close / Close Others.
    fn render_cell_detail_tab(
        &self,
        tab_id: u64,
        tab: CellDetailTab,
        active: bool,
        compact: bool,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let (selector, close_selector, icon, label) = match tab {
            CellDetailTab::Details => (
                "cell-detail-details-tab",
                "cell-detail-details-tab-close",
                "icons/frame.svg",
                "Value",
            ),
            CellDetailTab::Column => (
                "cell-detail-column-tab",
                "cell-detail-column-tab-close",
                "icons/columns-3.svg",
                "Column",
            ),
            CellDetailTab::Row => (
                "cell-detail-row-tab",
                "cell-detail-row-tab-close",
                "icons/list-checks.svg",
                "Row",
            ),
        };
        let color = if active {
            theme::text()
        } else {
            theme::text_muted()
        };
        let close = Button::new((close_selector, tab_id))
            .icon(Icon::new(IconName::Close).size(px(10.0)))
            .with_size(Size::XSmall)
            .compact()
            .custom(chrome_button_variant(false, cx))
            .rounded(px(3.0))
            .w(px(16.0))
            .h(px(16.0))
            .tooltip("Close tab")
            .on_click(cx.listener(move |this, _, _, cx| {
                this.close_cell_detail_tab(tab_id, tab, cx);
            }));
        let show_close_at_rest = active && !compact;
        let root = cx.entity().downgrade();
        div()
            .id(("cell-detail-tab", tab as usize))
            .relative()
            .group(selector)
            .debug_selector(move || selector.into())
            .h_full()
            .flex()
            .min_w(px(32.0))
            .items_center()
            .pl(px(2.0))
            .pr(px(2.0))
            .text_size(px(12.0))
            .text_color(color)
            .when(active, |this| this.bg(theme::background()))
            .when(!active, |this| this.hover(|style| style.bg(theme::hover())))
            .on_mouse_down(
                gpui::MouseButton::Middle,
                cx.listener(move |this, _, _, cx| {
                    this.close_cell_detail_tab(tab_id, tab, cx);
                }),
            )
            .child(
                div()
                    .id((selector, tab_id))
                    .debug_selector(move || format!("{selector}-body"))
                    .flex()
                    .flex_1()
                    .min_w(px(28.0))
                    .h_full()
                    .items_center()
                    .justify_center()
                    .gap(px(4.0))
                    .px(px(5.0))
                    .cursor_pointer()
                    .on_click(cx.listener(move |this, _, _, cx| {
                        this.open_cell_detail_tab(tab_id, tab, cx);
                    }))
                    .when(compact, |this| {
                        this.tooltip(move |window, cx| {
                            gpui_component::tooltip::Tooltip::new(label).build(window, cx)
                        })
                    })
                    .child(
                        Icon::empty()
                            .path(icon)
                            .size(px(13.0))
                            .text_color(if active { theme::accent() } else { color })
                            .flex_shrink_0(),
                    )
                    .when(!compact, |this| this.child(div().truncate().child(label))),
            )
            .child(
                div()
                    .debug_selector(move || close_selector.into())
                    .w(px(16.0))
                    .h(px(16.0))
                    .flex_shrink_0()
                    // Icon-only tabs have no room for a reserved slot: the
                    // close button overlays the tab's right edge on hover only.
                    .when(compact, |this| this.absolute().right(px(0.0)).top(px(9.0)))
                    .when(!compact, |this| this.ml(px(1.0)))
                    .when(!show_close_at_rest, |this| {
                        this.invisible().group_hover(selector, |style| style.visible())
                    })
                    .child(close),
            )
            .context_menu(move |menu, _, _| {
                let close_root = root.clone();
                let others_root = root.clone();
                menu.item(PopupMenuItem::new("Close").on_click(move |_, _, app| {
                    let _ = close_root.update(app, |this, cx| this.close_cell_detail_tab(tab_id, tab, cx));
                }))
                .item(PopupMenuItem::new("Close Others").on_click(move |_, _, app| {
                    let _ = others_root
                        .update(app, |this, cx| this.close_other_cell_detail_tabs(tab_id, tab, cx));
                }))
            })
            .into_any_element()
    }

    /// The rectangle the popup must stay inside: the GRID area only. Its top is
    /// the grid header's top, its bottom the footer's top edge, and it lies
    /// between the left rail (plus the columns panel when open) and the right
    /// rail, so rails, panel and footer stay visible and clickable and the
    /// SQL editor is never covered. The whole window until the results area
    /// was measured.
    pub(in crate::app) fn cell_detail_popup_area(
        &self,
        tab_id: u64,
        viewport: gpui::Size<gpui::Pixels>,
    ) -> crate::cell_detail::PopupArea {
        let Some(bounds) = self.view(tab_id).results_bounds.get() else {
            return (0.0, 0.0, f32::from(viewport.width), f32::from(viewport.height));
        };
        (
            f32::from(bounds.left()) + RAIL_WIDTH,
            f32::from(bounds.top()),
            f32::from(bounds.right()) - RAIL_WIDTH,
            f32::from(bounds.bottom()),
        )
    }

    /// Window position of the popup: the stored one once the user moved it,
    /// otherwise the default that avoids the selected cell. Shared by rendering
    /// and drag start so a drag begins exactly where the popup is drawn.
    pub(in crate::app) fn cell_detail_popup_position(
        &self,
        tab_id: u64,
        geometry: &crate::cell_detail::PopupGeometry,
        area: crate::cell_detail::PopupArea,
    ) -> (f32, f32) {
        // The area starts at the grid's left edge; the selected cell's x comes
        // from the grid layout and its horizontal scroll.
        let grid_left = area.0;
        let view = self.view(tab_id);
        let avoid = view
            .grid_ui
            .selection
            .borrow()
            .range
            .map(|range| range.normalized().start.column)
            .filter(|column| *column < view.grid_ui.layout.widths.len())
            .map(|column| {
                let layout = &view.grid_ui.layout;
                let left = grid_left
                    + layout.gutter_width
                    + layout.offset(column)
                    + f32::from(view.grid_horizontal_scroll.offset().x);
                (left, left + layout.widths[column])
            });
        geometry.resolved_position_avoiding(area, avoid)
    }

    /// Floating, non-modal panel. gpui-component 0.6.6 only offers modal
    /// dialogs/sheets and anchored popovers that dismiss on outside click, none
    /// of which keep the grid interactive or can be dragged, so this composes
    /// gpui's own `deferred` + `anchored` primitives (window coordinates,
    /// snapped to the window) with a header drag handle and a corner resize
    /// grip. A transparent full-window overlay exists only while dragging so
    /// mouse moves are not lost when the pointer leaves the handle.
    pub(in crate::app) fn render_cell_detail_popup(
        &self,
        tab_id: u64,
        state: &crate::cell_detail::CellDetailState,
        detail: Option<&CellDetail>,
        viewport: gpui::Size<gpui::Pixels>,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let area = self.cell_detail_popup_area(tab_id, viewport);
        let geometry = state.popup.clamped(area);
        let (x, y) = self.cell_detail_popup_position(tab_id, &geometry, area);
        let panel = self.render_cell_detail_panel(tab_id, state, detail, cx);
        let grip = div()
            .debug_selector(|| "cell-detail-popup-resize".into())
            .absolute()
            .right_0()
            .bottom_0()
            .w(px(16.0))
            .h(px(16.0))
            .cursor_nwse_resize()
            .child(
                Icon::empty()
                    .path("icons/maximize.svg")
                    .size(px(10.0))
                    .text_color(theme::text_faint())
                    .absolute()
                    .right(px(3.0))
                    .bottom(px(3.0)),
            )
            .on_mouse_down(
                gpui::MouseButton::Left,
                cx.listener(move |this, event: &gpui::MouseDownEvent, window, cx| {
                    this.begin_cell_detail_popup_drag(
                        tab_id,
                        crate::cell_detail::PopupDragKind::Resize,
                        event.position,
                        window.viewport_size(),
                        cx,
                    );
                }),
            );
        let popup = div()
            .id("cell-detail-popup")
            .debug_selector(|| "cell-detail-popup".into())
            // The popup floats over the grid in a deferred layer; without
            // occluding, wheel/click events also reach the grid behind it
            // (scrolling the popup scrolled the table). `occlude` blocks all
            // mouse interaction, including scroll, for hitboxes behind it.
            .occlude()
            .relative()
            .w(px(geometry.size.0))
            .h(px(geometry.size.1))
            .rounded(px(8.0))
            .border_1()
            .border_color(theme::border_strong())
            // Visible elevation over the grid: 1px border plus a soft, dark
            // drop shadow (offset down, wide blur).
            .shadow(vec![
                gpui::BoxShadow {
                    color: gpui::hsla(0.0, 0.0, 0.0, 0.55),
                    offset: gpui::point(px(0.0), px(10.0)),
                    blur_radius: px(28.0),
                    spread_radius: px(0.0),
                    inset: false,
                },
                gpui::BoxShadow {
                    color: gpui::hsla(0.0, 0.0, 0.0, 0.35),
                    offset: gpui::point(px(0.0), px(2.0)),
                    blur_radius: px(6.0),
                    spread_radius: px(0.0),
                    inset: false,
                },
            ])
            .overflow_hidden()
            .bg(theme::background())
            .child(panel)
            .child(grip);
        let overlay = state.popup_drag.is_some().then(|| {
            gpui::deferred(
                gpui::anchored().position(gpui::point(px(0.0), px(0.0))).child(
                    div()
                        .debug_selector(|| "cell-detail-popup-drag-overlay".into())
                        .w(viewport.width)
                        .h(viewport.height)
                        .on_mouse_move(cx.listener(
                            move |this, event: &gpui::MouseMoveEvent, window, cx| {
                                this.update_cell_detail_popup_drag(
                                    tab_id,
                                    event.position,
                                    window.viewport_size(),
                                    cx,
                                );
                            },
                        ))
                        .on_mouse_up(
                            gpui::MouseButton::Left,
                            cx.listener(move |this, _, _, cx| this.end_cell_detail_popup_drag(tab_id, cx)),
                        )
                        .on_mouse_up_out(
                            gpui::MouseButton::Left,
                            cx.listener(move |this, _, _, cx| this.end_cell_detail_popup_drag(tab_id, cx)),
                        ),
                ),
            )
            .with_priority(2)
        });
        div()
            .children(overlay)
            .child(
                gpui::deferred(
                    gpui::anchored()
                        .position(gpui::point(px(x), px(y)))
                        .snap_to_window()
                        .child(popup),
                )
                .with_priority(1),
            )
            .into_any_element()
    }

    pub(in crate::app) fn render_cell_detail_panel(
        &self,
        tab_id: u64,
        state: &crate::cell_detail::CellDetailState,
        detail: Option<&CellDetail>,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        // Without a target the Details body is an empty state; the shared
        // header/chrome code below still needs a value to read from.
        let has_detail = detail.is_some();
        let placeholder;
        let detail = match detail {
            Some(detail) => detail,
            None => {
                placeholder = CellDetail::placeholder();
                &placeholder
            }
        };
        let panel_is_bottom = state.placement == CellDetailPlacement::Bottom;
        let panel_is_popup = state.placement == CellDetailPlacement::Popup;
        let header_height = if panel_is_popup {
            34.0
        } else if panel_is_bottom {
            32.0
        } else {
            theme::RESULT_GRID_HEADER_HEIGHT
        };
        let body_padding = if panel_is_bottom { 6.0 } else { 12.0 };
        let body_gap = if panel_is_bottom { 6.0 } else { 10.0 };
        let value_min_height = if panel_is_bottom { 40.0 } else { 64.0 };
        let is_details = state.tab == CellDetailTab::Details;
        let editing = is_details
            && self
                .view(tab_id)
                .detail_ui
                .edit
                .as_ref()
                .is_some_and(|edit| edit.position == detail.position);

        let placement_root = cx.entity().downgrade();
        let current_placement = state.placement;
        // Icon passed as a child, not `.icon()`: gpui-component 0.7.0 sizes an
        // icon-only Button as a fixed square (`size_6`), which clips the
        // dropdown caret. With content it uses padded sizing that fits both.
        let placement = Button::new(("cell-detail-placement", tab_id))
            .child(placement_icon(current_placement).size(px(13.0)))
            // XSmall keeps icon + caret narrow so the three labelled tabs still
            // fit the default-width header.
            .with_size(Size::XSmall)
            .compact()
            .custom(chrome_button_variant(false, cx))
            .rounded(px(4.0))
            .dropdown_caret(true)
            .tooltip("Panel placement")
            .dropdown_menu_with_anchor(gpui::Anchor::TopRight, move |menu, _, _| {
                let mut menu = menu.min_w(px(160.0));
                for (option, label) in [
                    (CellDetailPlacement::Right, "Side (right)"),
                    (CellDetailPlacement::Bottom, "Bottom"),
                    (CellDetailPlacement::Popup, "Popup"),
                ] {
                    let root = placement_root.clone();
                    menu = menu.item(
                        PopupMenuItem::new(label)
                            .checked(current_placement == option)
                            .on_click(move |_, _, app| {
                                let _ = root.update(app, |this, cx| {
                                    this.set_cell_detail_placement(tab_id, option, cx);
                                });
                            }),
                    );
                }
                menu
            });
        let close = Button::new(("cell-detail-close", tab_id))
            .icon(Icon::new(IconName::Close).size(px(14.0)))
            .with_size(Size::Small)
            .compact()
            .custom(chrome_button_variant(false, cx))
            .rounded(px(4.0))
            .tooltip("Close panel")
            .on_click(cx.listener(move |this, _, _, cx| {
                this.close_cell_detail_panel(tab_id, cx);
            }));

        let wrap = |selector: &'static str, button: Button| {
            div()
                .debug_selector(move || selector.into())
                .flex_shrink_0()
                .child(button)
        };
        // Labels stay unless the panel is too narrow to fit them.
        let measured = self.view(tab_id).cell_detail_width.get();
        let compact_tabs = state.placement == CellDetailPlacement::Right
            && crate::cell_detail::tabs_are_compact(if measured > 0.0 {
                measured
            } else {
                state.right_width
            });
        let tab_strip = div()
            .debug_selector(|| "cell-detail-tab-strip".into())
            .flex()
            .min_w_0()
            .h_full()
            .items_center()
            .overflow_hidden()
            .children(state.opened_tabs.iter().map(|tab| {
                self.render_cell_detail_tab(tab_id, *tab, *tab == state.tab, compact_tabs, cx)
            }));
        let header = div()
            .debug_selector(|| "cell-detail-header".into())
            .h(px(header_height))
            .flex()
            .items_center()
            .gap(px(0.0))
            .flex_shrink_0()
            .pl(px(0.0))
            .pr(px(4.0))
            .border_b_1()
            .border_color(theme::border())
            .bg(theme::chrome())
            .child(tab_strip)
            // Spacer between tabs and controls; in popup mode it is the
            // title-bar drag handle.
            .child(
                div()
                    .debug_selector(|| "cell-detail-drag-handle".into())
                    .flex_1()
                    .min_w(px(8.0))
                    .h_full()
                    .when(panel_is_popup, |this| {
                        this.cursor_grab().on_mouse_down(
                            gpui::MouseButton::Left,
                            cx.listener(move |this, event: &gpui::MouseDownEvent, window, cx| {
                                this.begin_cell_detail_popup_drag(
                                    tab_id,
                                    crate::cell_detail::PopupDragKind::Move,
                                    event.position,
                                    window.viewport_size(),
                                    cx,
                                );
                            }),
                        )
                    }),
            )
            .child(
                div()
                    .debug_selector(|| "cell-detail-placement".into())
                    .flex_shrink_0()
                    .child(placement),
            )
            .child(wrap("cell-detail-close", close));

        let metadata_label_width = if panel_is_bottom { 44.0 } else { 64.0 };
        let metadata_row = |selector: &'static str,
                            value_selector: &'static str,
                            label: &'static str,
                            value: String,
                            color: gpui::Rgba| {
            property_row(label, metadata_label_width, color, value, Some(value_selector))
                .debug_selector(move || selector.into())
        };
        let row_row = metadata_row(
            "cell-detail-row",
            "cell-detail-row-value",
            "Row",
            detail.row_number.to_string(),
            theme::text(),
        );
        let length_row = metadata_row(
            "cell-detail-length",
            "cell-detail-length-value",
            "Length",
            detail.length.to_string(),
            theme::text(),
        );
        let null_row = metadata_row(
            "cell-detail-null",
            "cell-detail-null-value",
            "NULL",
            detail.is_null.to_string(),
            if detail.is_null {
                theme::warning()
            } else {
                theme::text_muted()
            },
        );
        let comment_row = (!detail.comment.is_empty()).then(|| {
            metadata_row(
                "cell-detail-comment",
                "cell-detail-comment-value",
                "Comment",
                detail.comment.clone(),
                theme::text(),
            )
        });
        let metadata = div()
            .debug_selector(|| "cell-detail-metadata".into())
            .w_full()
            .flex()
            .flex_col()
            .gap(px(4.0))
            .flex_shrink_0()
            .text_size(px(11.0));
        // The column name leads the Details body (replaces the old header
        // suffix and the "Column" metadata row), with its type next to it.
        let column_title = div()
            .debug_selector(|| "cell-detail-column".into())
            .flex_shrink_0()
            .flex()
            .items_baseline()
            .gap(px(8.0))
            .min_w_0()
            .child(
                div()
                    .debug_selector(|| "cell-detail-column-value".into())
                    .min_w_0()
                    .truncate()
                    .text_size(px(13.0))
                    .font_weight(gpui::FontWeight::SEMIBOLD)
                    .text_color(theme::text())
                    .child(detail.column_name.clone()),
            )
            .child(
                div()
                    .flex_shrink_0()
                    .text_size(px(11.0))
                    .text_color(grid_column_type_color(&detail.type_name))
                    .child(detail.type_name.clone()),
            )
            .children(detail.cell_class.map(|class| {
                div()
                    .debug_selector(|| "cell-detail-cell-class".into())
                    .flex_shrink_0()
                    .text_size(px(11.0))
                    .text_color(theme::text_muted())
                    .child(class)
            }));
        let metadata = if panel_is_bottom {
            metadata.child(
                div()
                    .flex()
                    .gap(px(12.0))
                    .child(row_row.flex_1())
                    .child(length_row.flex_1())
                    .child(null_row.flex_1()),
            )
        } else {
            metadata
                .child(
                    div()
                        .flex()
                        .gap(px(12.0))
                        .child(row_row.flex_1())
                        .child(length_row.flex_1()),
                )
                .child(null_row)
        }
        .children(comment_row);

        let is_mongo = self
            .workspace
            .tab(tab_id)
            .is_some_and(|tab| tab.query_mode.is_mongo_find());
        let json_like = detail.formatted_json.is_some();
        let mode = self.view(tab_id).detail_ui.value_mode;
        let binary = detail.binary.as_ref().filter(|_| !editing);
        let convert_options = if editing || detail.is_null || detail.binary.is_some() {
            Vec::new()
        } else {
            crate::cell_value::convert_options(&detail.value)
        };
        let active_convert = scoped_convert(
            self.view(tab_id).detail_ui.convert,
            tab_id,
            detail.position,
            &convert_options,
        );
        let convert_preview = active_convert
            .and_then(|kind| crate::cell_value::convert_preview(kind, &detail.value).map(|text| (kind, text)));

        let panel_width = match self.view(tab_id).cell_detail_width.get() {
            measured if measured > 0.0 => measured,
            _ => state.right_width,
        };
        let value_box_child = if editing {
            let editor = self
                .view(tab_id)
                .detail_ui
                .edit
                .as_ref()
                .expect("cell detail editor should exist while editing")
                .editor
                .clone();
            let is_text = matches!(editor, CellDetailEditor::Text(_));
            div()
                .size_full()
                .capture_key_down(cx.listener(move |this, event: &gpui::KeyDownEvent, _, cx| {
                    if event.is_held {
                        return;
                    }
                    if event.keystroke.key == "escape" {
                        cx.stop_propagation();
                        this.cancel_cell_detail_editor(tab_id, cx);
                    } else if event.keystroke.key == "enter"
                        && !event.keystroke.modifiers.shift
                        && is_text
                    {
                        cx.stop_propagation();
                        this.commit_cell_detail_editor(tab_id, cx);
                    }
                }))
                .child(match &editor {
                    CellDetailEditor::Text(state) => {
                        Textarea::new(state).appearance(true).h_full().into_any_element()
                    }
                    CellDetailEditor::Code(state) => {
                        Editor::new(state).appearance(true).h_full().into_any_element()
                    }
                })
                .into_any_element()
        } else if let Some(binary) = binary {
            self.render_binary_value(tab_id, detail, binary, panel_width, cx)
        } else if let (true, Some(viewer)) =
            (json_like, self.view(tab_id).json_views.cell_value.as_ref())
        {
            // Official code editor: tree-sitter JSON highlighting, read-only.
            div()
                .debug_selector(|| "cell-detail-json-value".into())
                .size_full()
                .child(Editor::new(&viewer.state).readonly(true).appearance(false).bordered(false).h_full())
                .into_any_element()
        } else {
            div()
                .size_full()
                .overflow_y_scrollbar()
                .child(
                    div()
                        .child(
                            // A NULL reads muted, as in the grid, so it never
                            // passes for the text `NULL`.
                            div()
                                .text_color(super::grid::grid_cell_text_color(detail.is_null))
                                .child(detail.value_preview.clone()),
                        )
                        .when(detail.value_preview_truncated, |this| {
                            this.child(
                                div()
                                    .pt(px(6.0))
                                    .font_family("Inter")
                                    .text_size(px(10.0))
                                    .text_color(theme::text_muted())
                                    .child("Preview truncated; edit to inspect the full value."),
                            )
                        })
                        .when(detail.json_unformatted, |this| {
                            this.child(
                                div()
                                    .debug_selector(|| "cell-detail-json-unformatted".into())
                                    .pt(px(6.0))
                                    .font_family("Inter")
                                    .text_size(px(10.0))
                                    .text_color(theme::text_muted())
                                    .child(format!(
                                        "Too large to format ({} characters): shown as stored.",
                                        detail.length
                                    )),
                            )
                        })
                        .children(detail.read_only_reason.map(|reason| {
                            div()
                                .debug_selector(|| "cell-detail-read-only-reason".into())
                                .pt(px(6.0))
                                .font_family("Inter")
                                .text_size(px(10.0))
                                .text_color(theme::text_muted())
                                .child(reason)
                        })),
                )
                .into_any_element()
        };
        let value_body = div()
            .debug_selector(|| "cell-detail-value-box".into())
            .flex()
            .flex_col()
            .flex_1()
            .min_h(px(value_min_height))
            .min_w(px(0.0))
            .p(px(if json_like || editing || binary.is_some() { 2.0 } else { 8.0 }))
            .bg(theme::surface())
            .border_1()
            .border_color(theme::border())
            .rounded(px(4.0))
            .overflow_hidden()
            .text_size(px(12.0))
            .font_family("Menlo")
            .child(value_box_child);

        let root = cx.entity().downgrade();
        let mode_options: Vec<(crate::cell_value::ValueMode, &'static str)> = if is_mongo {
            vec![
                (crate::cell_value::ValueMode::Formatted, "Formatted"),
                (crate::cell_value::ValueMode::Canonical, "Canonical"),
                (crate::cell_value::ValueMode::Raw, "Raw"),
            ]
        } else {
            vec![
                (crate::cell_value::ValueMode::Formatted, "Formatted JSON"),
                (crate::cell_value::ValueMode::Raw, "Raw"),
            ]
        };
        let mode_toggle = ButtonGroup::new(("cell-detail-value-mode", tab_id))
            .outline()
            .compact()
            .with_size(Size::XSmall)
            .children(mode_options.iter().enumerate().map(|(index, (option, label))| {
                Button::new(("cell-detail-value-mode-option", index))
                    .label(*label)
                    .selected(mode == *option)
            }))
            .on_click({
                let root = root.clone();
                let options = mode_options.clone();
                move |selected, _, app| {
                    if let Some((option, _)) = selected.first().and_then(|index| options.get(*index)) {
                        let option = *option;
                        let _ = root.update(app, |this, cx| {
                            this.view_mut(tab_id).detail_ui.value_mode = option;
                            this.notify_tab(tab_id, cx);
                        });
                    }
                }
            });
        let convert_menu = Button::new(("cell-detail-convert", tab_id))
            .icon(Icon::empty().path("icons/arrow-left-right.svg").size(px(12.0)))
            .label("Convert preview")
            .with_size(Size::XSmall)
            .compact()
            .ghost()
            .dropdown_caret(true)
            // Right-aligned to the trigger so the menu grows leftwards and
            // stays inside the panel (the trigger sits at the panel's right edge).
            .dropdown_menu_with_anchor(gpui::Anchor::TopRight, {
                let root = root.clone();
                let options = convert_options.clone();
                let position = detail.position;
                move |menu, _, _| {
                    let mut menu = menu.min_w(px(220.0)).item(
                        PopupMenuItem::new("Original value")
                            .checked(active_convert.is_none())
                            .on_click({
                                let root = root.clone();
                                move |_, _, app| {
                                    let _ = root.update(app, |this, cx| {
                                        this.view_mut(tab_id).detail_ui.convert = None;
                                        this.notify_tab(tab_id, cx);
                                    });
                                }
                            }),
                    );
                    for kind in options.iter().copied() {
                        let root = root.clone();
                        menu = menu.item(
                            PopupMenuItem::new(kind.label())
                                .checked(active_convert == Some(kind))
                                .on_click(move |_, _, app| {
                                    let _ = root.update(app, |this, cx| {
                                        this.view_mut(tab_id).detail_ui.convert =
                                            Some((tab_id, position, kind));
                                        this.notify_tab(tab_id, cx);
                                    });
                                }),
                        );
                    }
                    menu
                }
            });
        let icon_action = |selector: &'static str,
                           path: &'static str,
                           label: &'static str,
                           cx: &mut Context<Self>| {
            Button::new((selector, tab_id))
                .icon(Icon::empty().path(path).size(px(13.0)))
                .with_size(Size::XSmall)
                .compact()
                .custom(chrome_button_variant(false, cx))
                .rounded(px(4.0))
                .tooltip(label)
        };
        let value_header = div()
            .debug_selector(|| "cell-detail-value-header".into())
            .flex_shrink_0()
            .flex()
            .items_center()
            .gap(px(4.0))
            .min_h(px(22.0))
            .child(
                div()
                    .debug_selector(|| "cell-detail-value-label".into())
                    .flex_1()
                    .min_w_0()
                    .text_size(px(11.0))
                    .text_color(theme::text_muted())
                    .child("Value"),
            )
            .when(json_like && !editing, |this| {
                this.child(div().debug_selector(|| "cell-detail-value-mode".into()).child(mode_toggle))
            })
            .when(
                !editing && detail.binary.is_none() && detail.binary_form.is_some() && detail.editable,
                |this| {
                    this.child(div().debug_selector(|| "cell-detail-binary-load".into()).child(
                        super::binary_view::binary_icon_button(
                            "cell-detail-binary-load",
                            tab_id,
                            "icons/upload.svg",
                            "Load from file",
                            chrome_button_variant(false, cx),
                        )
                        .on_click(cx.listener(move |this, _, _, cx| this.load_cell_binary(tab_id, cx))),
                    ))
                },
            )
            .when_some(binary, |this, binary| {
                let variant = chrome_button_variant(false, cx);
                this.child(
                    div()
                        .debug_selector(|| "cell-detail-binary-modes".into())
                        .child(self.render_binary_mode_switch(tab_id, detail, binary, cx)),
                )
                .child(div().debug_selector(|| "cell-detail-binary-save".into()).child(
                    super::binary_view::binary_icon_button(
                        "cell-detail-binary-save",
                        tab_id,
                        "icons/download.svg",
                        "Save to file",
                        variant,
                    )
                    .on_click(cx.listener(move |this, _, _, cx| this.save_cell_binary(tab_id, cx))),
                ))
                .when(detail.editable && detail.binary_form.is_some(), |this| {
                    this.child(div().debug_selector(|| "cell-detail-binary-load".into()).child(
                        super::binary_view::binary_icon_button(
                            "cell-detail-binary-load",
                            tab_id,
                            "icons/upload.svg",
                            "Load from file",
                            variant,
                        )
                        .on_click(cx.listener(move |this, _, _, cx| this.load_cell_binary(tab_id, cx))),
                    ))
                })
            })
            .when(!convert_options.is_empty(), |this| {
                this.child(div().debug_selector(|| "cell-detail-convert".into()).child(convert_menu))
            })
            .when(
                !editing
                    && detail.editable
                    && binary.is_none_or(crate::cell_detail::BinaryDetail::text_editable),
                |this| {
                this.child(div().debug_selector(|| "cell-detail-edit".into()).child(
                    icon_action("cell-detail-edit", "icons/pencil.svg", "Edit value", cx).on_click(
                        cx.listener(move |this, _, window, cx| {
                            this.open_cell_detail_editor(tab_id, window, cx);
                        }),
                    ),
                ))
                },
            )
            .when(!editing, |this| {
                this.child(div().debug_selector(|| "cell-detail-copy-value".into()).child(
                    icon_action("cell-detail-copy-value", "icons/copy.svg", "Copy value", cx)
                        .on_click(cx.listener(move |this, _, _, cx| {
                            this.copy_cell_detail_value(tab_id, cx);
                        })),
                ))
            });
        let editing_controls = div()
            .flex()
            .justify_end()
            .gap(px(4.0))
            .flex_shrink_0()
            .child(
                div().debug_selector(|| "cell-detail-cancel".into()).child(
                    Button::new(("cell-detail-cancel", tab_id))
                        .label("Cancel")
                        .with_size(Size::Small)
                        .compact()
                        .outline()
                        .on_click(cx.listener(move |this, _, _, cx| {
                            this.cancel_cell_detail_editor(tab_id, cx);
                        })),
                ),
            )
            .child(
                div().debug_selector(|| "cell-detail-confirm".into()).child(
                    Button::new(("cell-detail-confirm", tab_id))
                        .label("Confirm")
                        .with_size(Size::Small)
                        .compact()
                        .primary()
                        .on_click(cx.listener(move |this, _, _, cx| {
                            this.commit_cell_detail_editor(tab_id, cx);
                        })),
                ),
            );
        let value = div()
            .debug_selector(|| "cell-detail-value".into())
            .flex()
            .flex_col()
            .flex_1()
            .min_h(px(value_min_height))
            .gap(px(4.0))
            .child(value_header)
            .child(value_body)
            .when_some(binary, |this, binary| {
                this.child(self.render_binary_subbar(tab_id, detail, binary, cx))
            })
            .when_some(convert_preview, |this, (kind, text)| {
                this.child(
                    div()
                        .debug_selector(|| "cell-detail-convert-preview".into())
                        .flex_shrink_0()
                        .px(px(8.0))
                        .py(px(4.0))
                        .rounded(px(4.0))
                        .bg(theme::accent_surface())
                        .text_size(px(11.0))
                        .child(
                            div()
                                .text_color(theme::text_muted())
                                .child(format!("Preview · {}", kind.label())),
                        )
                        .child(
                            div()
                                .font_family("Menlo")
                                .text_color(theme::text())
                                .whitespace_normal()
                                .child(text),
                        ),
                )
            })
            .when(editing, |this| this.child(editing_controls));

        let action_row = |selector: &'static str, path: &'static str, label: &'static str| {
            div()
                .id((selector, tab_id))
                .debug_selector(move || selector.into())
                .h(px(26.0))
                .px(px(8.0))
                .flex()
                .items_center()
                .gap(px(8.0))
                .rounded(px(4.0))
                .cursor_pointer()
                .text_size(px(12.0))
                .text_color(theme::text())
                .hover(|style| style.bg(theme::hover()))
                .child(
                    Icon::empty()
                        .path(path)
                        .size(px(13.0))
                        .text_color(theme::text_muted())
                        .flex_shrink_0(),
                )
                .child(div().truncate().child(label))
        };
        let actions = div()
            .debug_selector(|| "cell-detail-actions".into())
            .flex_shrink_0()
            .w_full()
            .flex()
            .when(panel_is_bottom, |this| this.flex_row().justify_end().gap(px(4.0)))
            .when(!panel_is_bottom, |this| this.flex_col())
            .p(px(4.0))
            .border_t_1()
            .border_color(theme::border())
            .when(
                !editing && detail.nullable && detail.editable && !detail.is_null,
                |this| {
                    this.child(
                        action_row("cell-detail-set-null", "icons/eraser.svg", "Set NULL")
                            .on_click(cx.listener(move |this, _, window, cx| {
                                this.set_cell_detail_null(tab_id, window, cx);
                            })),
                    )
                },
            )
            .child(
                action_row("cell-detail-copy-column", "icons/copy.svg", "Copy Column Name")
                    .on_click(cx.listener(move |this, _, _, cx| {
                        this.copy_cell_detail_column_name(tab_id, cx);
                    })),
            )
            .child(
                action_row(
                    "cell-detail-copy-sql-condition",
                    "icons/filter.svg",
                    "Copy SQL Condition",
                )
                .on_click(cx.listener(move |this, _, _, cx| {
                    this.copy_cell_detail_sql_condition(tab_id, cx);
                })),
            );

        let body_content = match state.tab {
            CellDetailTab::Details if !has_detail => {
                cell_detail_empty_state("cell-detail-empty", "Select a cell in the grid")
            }
            CellDetailTab::Details => div()
                .w_full()
                .h_full()
                .flex()
                .flex_col()
                .min_h_0()
                .child(
                    div()
                        .debug_selector(|| "cell-detail-body-content".into())
                        .w_full()
                        .flex()
                        .flex_col()
                        .flex_1()
                        .min_h_0()
                        .gap(px(body_gap))
                        .p(px(body_padding))
                        .px(px(12.0))
                        .child(column_title)
                        .child(metadata)
                        .child(value)
                        .when_some(self.detail_message(tab_id), |this, error| {
                            this.child(
                                div()
                                    .debug_selector(|| "cell-detail-error".into())
                                    .flex_shrink_0()
                                    .text_size(px(10.0))
                                    .text_color(theme::danger())
                                    .child(error),
                            )
                        }),
                )
                .child(actions)
                .into_any_element(),
            CellDetailTab::Column => self.render_cell_detail_column(state.column.as_ref()),
            CellDetailTab::Row if state.target.is_none() => {
                cell_detail_empty_state("cell-detail-empty", "Select a cell in the grid")
            }
            CellDetailTab::Row => self.render_cell_detail_row(tab_id, state, cx),
                    };
        let body = div().flex_1().min_h_0().child(body_content);
        // Measures the rendered panel width (layout-driven: window size, left
        // panel, resizing) once per frame in prepaint; a refresh is requested
        // only when that flips the compact-tab threshold.
        let width_cell = self.view(tab_id).cell_detail_width.clone();
        let stored_width = state.right_width;
        let width_probe = gpui::canvas(
            move |bounds, window, _| {
                let width = f32::from(bounds.size.width);
                // Before the first measurement the stored width was used.
                let previous = match width_cell.replace(width) {
                    previous if previous > 0.0 => previous,
                    _ => stored_width,
                };
                if crate::cell_detail::tabs_are_compact(previous)
                    != crate::cell_detail::tabs_are_compact(width)
                {
                    window.refresh();
                }
            },
            |_, _, _, _| {},
        )
        .absolute()
        .size_full();

        div()
            .id(("cell-detail-panel", tab_id))
            .debug_selector(|| "cell-detail-panel".into())
            .h_full()
            .min_h_0()
            .w_full()
            .flex()
            .flex_col()
            .overflow_hidden()
            .bg(theme::background())
            .when(!panel_is_bottom && !panel_is_popup, |this| this.border_l_1())
            .when(panel_is_bottom, |this| this.border_t_1())
            .border_color(theme::border())
            .child(width_probe)
            .child(header)
            .child(body)
            .into_any_element()
    }
}

/// Width of the left/right rails flanking the results area.
const RAIL_WIDTH: f32 = 36.0;
const ROW_TREE_HEIGHT: f32 = 24.0;
const ROW_TREE_INDENT: f32 = 14.0;
const ROW_TREE_KEY_WIDTH: f32 = 100.0;
const ROW_TREE_TYPE_WIDTH: f32 = 62.0;

/// Type badge tint: reuses the grid palette, plus `document` (Mongo/JSON
/// object) which the grid palette does not name explicitly.
fn row_tree_type_color(type_name: &str) -> gpui::Rgba {
    if type_name == "document" {
        return theme::type_object();
    }
    grid_column_type_color(type_name)
}

pub(in crate::app) fn row_tree_value_color(type_name: &str, container: bool) -> gpui::Rgba {
    if container {
        return theme::text_muted();
    }
    match type_name {
        "null" => theme::text_faint(),
        "string" => theme::success(),
        "boolean" => theme::accent(),
        _ => match row_tree_type_color(type_name) {
            color if color == theme::type_number() => theme::warning(),
            _ => theme::text(),
        },
    }
}

fn placement_icon(placement: CellDetailPlacement) -> Icon {
    match placement {
        CellDetailPlacement::Right => Icon::new(IconName::PanelRight),
        CellDetailPlacement::Bottom => Icon::new(IconName::PanelBottom),
        CellDetailPlacement::Popup => Icon::empty().path("icons/maximize.svg"),
    }
}

/// Ghost-style chrome button colors for the cell-detail rail and header.
/// `Button::custom` is the official themed-variant hook in gpui-component.
pub(super) fn chrome_button_variant(active: bool, cx: &gpui::App) -> ButtonCustomVariant {
    let (bg, fg) = if active {
        (theme::accent_surface(), theme::accent())
    } else {
        (gpui::rgba(0x00000000), theme::text_muted())
    };
    ButtonCustomVariant::new(cx)
        .color(bg.into())
        .foreground(fg.into())
        .hover(if active {
            theme::accent_surface().into()
        } else {
            theme::hover().into()
        })
        .active(if active {
            theme::accent_surface().into()
        } else {
            theme::selected().into()
        })
}

/// Muted, centered placeholder for a panel tab with nothing to show yet.
fn cell_detail_empty_state(selector: &'static str, message: &'static str) -> AnyElement {
    div()
        .debug_selector(move || selector.into())
        .size_full()
        .flex()
        .items_center()
        .justify_center()
        .p(px(12.0))
        .text_size(px(12.0))
        .text_color(theme::text_muted())
        .child(message)
        .into_any_element()
}

/// One row of a compact properties table: fixed-width muted label + value.
fn property_row(
    label: &'static str,
    label_width: f32,
    value_color: gpui::Rgba,
    value: impl Into<gpui::SharedString>,
    value_selector: Option<&'static str>,
) -> Div {
    div()
        .flex()
        .items_start()
        .gap(px(8.0))
        .min_w(px(0.0))
        .child(
            div()
                .w(px(label_width))
                .flex_shrink_0()
                .whitespace_nowrap()
                .text_size(px(11.0))
                .text_color(theme::text_muted())
                .child(label),
        )
        .child(
            div()
                .flex_1()
                .min_w(px(0.0))
                .whitespace_normal()
                .text_color(value_color)
                .when_some(value_selector, |this, selector| {
                    this.debug_selector(move || selector.into())
                })
                .child(value.into()),
        )
}
