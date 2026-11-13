use super::super::*;
use super::find::FindTarget;

/// Bytes of a staged value a cell lays out: a staged hex literal loaded from a
/// file can be megabytes long, and one text line that long is never shaped.
const PENDING_CELL_DISPLAY_BYTES: usize = 2048;

/// A staged value as its cell shows it: the whole text, or its first
/// [`PENDING_CELL_DISPLAY_BYTES`] bytes and `…`. Only a long value allocates.
pub(super) fn pending_cell_preview(text: &Arc<str>) -> gpui::SharedString {
    if text.len() <= PENDING_CELL_DISPLAY_BYTES {
        return gpui::SharedString::new(Arc::clone(text));
    }
    let mut cut = PENDING_CELL_DISPLAY_BYTES;
    while !text.is_char_boundary(cut) {
        cut -= 1;
    }
    gpui::SharedString::from(format!("{}…", &text[..cut]))
}

/// Largest share of the grid's width the sticky block (gutter and pinned
/// columns) may take; wider ones scroll like the other columns.
const STICKY_MAX_SHARE: f32 = 0.6;

/// What the header cells' right-click menu needs to offer Pin / Unpin.
#[derive(Clone)]
struct HeaderPinMenu {
    /// Pinning applies to this result.
    available: bool,
    /// Staged edits forbid changing the pinned set now.
    locked: bool,
    /// Number of pinned (leading) columns.
    pinned: usize,
    /// The result's first column is pinned ("Freeze first column" is on).
    first_frozen: bool,
    root: gpui::WeakEntity<AppRoot>,
}

impl HeaderPinMenu {
    /// Pin / Unpin this column, "Freeze first column", and Unpin all.
    fn build(
        &self,
        menu: gpui_component::menu::PopupMenu,
        tab_id: u64,
        column: usize,
    ) -> gpui_component::menu::PopupMenu {
        if !self.available {
            return menu;
        }
        let is_pinned = column < self.pinned;
        let item = |label: &'static str, checked: bool, action: fn(&mut AppRoot, u64, usize, &mut Context<AppRoot>)| {
            let root = self.root.clone();
            PopupMenuItem::new(label)
                .icon(cell_menu_icon("icons/pin.svg"))
                .checked(checked)
                .disabled(self.locked)
                .on_click(move |_, _, app| {
                    let _ = root.update(app, |this, cx| action(this, tab_id, column, cx));
                })
        };
        let mut menu = menu.item(if is_pinned {
            item("Unpin column", false, AppRoot::unpin_column)
        } else {
            item("Pin column", false, AppRoot::pin_column)
        });
        menu = menu.item(item(
            "Freeze first column",
            self.first_frozen,
            |this, tab_id, _, cx| this.toggle_freeze_first_column(tab_id, cx),
        ));
        if self.pinned > 0 {
            menu = menu.item(item("Unpin all columns", false, |this, tab_id, _, cx| {
                this.unpin_all_columns(tab_id, cx)
            }));
        }
        if self.locked {
            menu = menu.item(PopupMenuItem::new(super::pinning::PIN_LOCKED_HINT).disabled(true));
        }
        menu
    }
}

/// The line closing the sticky block, over the cells that scroll under it.
fn sticky_divider() -> Div {
    div()
        .absolute()
        .top_0()
        .bottom_0()
        .right_0()
        .w(px(1.0))
        .bg(theme::border_strong())
}

fn cell_menu_icon(path: &'static str) -> Icon {
    Icon::empty().path(path).size(px(12.0))
}

/// First key of a Mongo sort document and whether it sorts descending.
fn first_sort_key(sort: &str) -> Option<(String, bool)> {
    if sort.is_empty() {
        return None;
    }
    let document: serde_json::Map<String, serde_json::Value> = serde_json::from_str(sort).ok()?;
    let (key, direction) = document.into_iter().next()?;
    Some((key, direction.as_f64()? < 0.0))
}

/// The sort a result already has, resolved once per frame for the header
/// sort buttons.
pub(in crate::app) struct HeaderSort {
    /// Database sort: column name and whether it is descending.
    pub database: Option<(String, bool)>,
    /// Page-only sort: source column and whether it is descending.
    pub page: Option<(usize, bool)>,
}

/// What one header's sort button shows and its menu checks.
#[derive(Clone, Copy)]
pub(in crate::app) struct ColumnSort {
    /// Database sort on this column (`true` = descending).
    pub database: Option<bool>,
    /// Page-only sort on this column (`true` = descending).
    pub page: Option<bool>,
    pub can_database_sort: bool,
    pub menu_open: bool,
}

/// Entries of a header sort menu: the same sorts as the cell context menu,
/// run for this column, with a check on the sort the column has.
pub(in crate::app) fn sort_menu_items(
    tab_id: u64,
    column: usize,
    sort: ColumnSort,
    root: &gpui::WeakEntity<AppRoot>,
) -> Vec<PopupMenuItem> {
    let ColumnSort {
        database,
        page,
        can_database_sort,
        ..
    } = sort;
    // The same words and icons as the cell's right-click menu.
    let choice = |label: &'static str, icon: &'static str, descending: bool, database_sort: bool| {
        let checked = if database_sort { database } else { page } == Some(descending);
        let root = root.clone();
        PopupMenuItem::new(label)
            .icon(cell_menu_icon(icon))
            .checked(checked)
            .disabled(database_sort && !can_database_sort)
            .on_click(move |_, window, app| {
                let _ = root.update(app, |this, cx| {
                    if database_sort {
                        this.sort_column_database(tab_id, column, descending, window, cx);
                    } else {
                        this.sort_column_page(tab_id, column, descending, window, cx);
                    }
                });
            })
    };
    let root = root.clone();
    vec![
        choice("Sort Database Ascending", "icons/database.svg", false, true),
        choice("Sort Database Descending", "icons/database.svg", true, true),
        PopupMenuItem::separator(),
        choice("Sort Current Page Ascending", "icons/arrow-up.svg", false, false),
        choice("Sort Current Page Descending", "icons/arrow-down.svg", true, false),
        PopupMenuItem::separator(),
        PopupMenuItem::new("Clear Sort")
            .icon(cell_menu_icon("icons/eraser.svg"))
            .disabled(database.is_none() && page.is_none())
            .on_click(move |_, window, app| {
                let _ = root.update(app, |this, cx| this.clear_context_sort(tab_id, window, cx));
            }),
    ]
}

/// The sort button at the right end of a header's name row; its menu is
/// [`sort_menu_items`].
fn header_sort_button(
    tab_id: u64,
    column: usize,
    sort: ColumnSort,
    root: gpui::WeakEntity<AppRoot>,
) -> impl IntoElement {
    let (path, color, state) = match (sort.database, sort.page) {
        (Some(descending), _) => (
            if descending { "icons/arrow-up-z-a.svg" } else { "icons/arrow-down-a-z.svg" },
            theme::accent(),
            if descending { "database-desc" } else { "database-asc" },
        ),
        (None, Some(descending)) => (
            if descending { "icons/arrow-up-z-a.svg" } else { "icons/arrow-down-a-z.svg" },
            theme::text_muted(),
            if descending { "page-desc" } else { "page-asc" },
        ),
        (None, None) => ("icons/chevrons-up-down.svg", theme::text_faint(), "none"),
    };
    let open_root = root.clone();
    Button::new(("grid-header-sort", column))
        .icon(Icon::empty().path(path).size(px(12.0)).text_color(color))
        .ghost()
        .xsmall()
        .compact()
        .flex_shrink_0()
        .debug_selector(move || format!("grid-header-sort-{column}-{state}"))
        .when(sort.database.is_none() && sort.page.is_some(), |this| {
            this.tooltip("Sorted on the loaded rows only")
        })
        .dropdown_menu_with_anchor(gpui::Anchor::TopRight, move |menu, _, _| {
            // Built when the menu opens, so the checks follow the sort then.
            sort_menu_items(tab_id, column, sort, &root)
                .into_iter()
                .fold(menu.min_w(px(220.0)).check_side(gpui_component::Side::Right), |menu, item| menu.item(item))
        })
        .on_open_change(move |open, _, app| {
            let _ = open_root.update(app, |this, cx| {
                let ui = &mut this.view_mut(tab_id).grid_ui;
                if *open {
                    ui.sort_menu = Some(column);
                } else if ui.sort_menu == Some(column) {
                    ui.sort_menu = None;
                } else {
                    return;
                }
                this.notify_tab(tab_id, cx);
            });
        })
}

/// Which row actions the grid's context menu offers for a tab's result.
#[derive(Clone, Copy)]
struct GridRowActions {
    /// Pinning applies to this result, and whether staged edits forbid it now.
    can_pin: bool,
    pin_locked: bool,
    can_stage_row: bool,
    can_sql_database_sort: bool,
    can_sql_clone: bool,
}

impl AppRoot {
    pub(in crate::app) fn render_result_row(
        &self,
        tab_id: u64,
        row: usize,
        layout: &crate::grid::ColumnLayout,
        selection: &crate::grid::GridSelection,
        columns: &crate::grid::ColumnWindow,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        use crate::grid::{CellPosition, CellRange};

        // Per-row (not per-cell) lookups.
        let view = self.view(tab_id);
        let runtime = self.workspace.runtime(tab_id);
        let grid = runtime.map_or(&self.empty_grid, |runtime| &runtime.grid);
        let source_columns = grid.column_count();
        let pending_delete =
            runtime.is_some_and(|runtime| runtime.pending_deleted_rows.contains(&row));
        // Committed by a partial Apply: the cells still show the old values.
        let applied = runtime.is_some_and(|runtime| runtime.row_is_applied(row));
        // Possibly committed (the driver could not tell): still staged.
        let unknown = runtime.is_some_and(|runtime| runtime.row_is_unknown(row));
        let row_edit = view
            .grid_ui
            .inline_edit
            .as_ref()
            .filter(|edit| edit.position.row == row);
        // Settings the row reads once (plain fields; the font clone is a
        // refcount bump).
        let display = &self.grid_display;
        // Pinned columns drawn in the sticky block (see `sticky`).
        let frozen = view.grid_ui.frozen;
        // Culled rows keep the full content width so scrollbar and header
        // alignment do not move.
        let mut result = div()
            .when(frozen > 0, |this| this.relative())
            .w(px(columns.total_width))
            .h(px(theme::ROW_HEIGHT))
            .flex()
            .items_center()
            .text_size(px(display.font_size))
            .when_some(display.font_family.clone(), |this, family| {
                this.font_family(family)
            })
            .text_color(theme::text());
        if row % 2 == 1 {
            result = result.bg(theme::grid_alt_row());
        }
        if pending_delete {
            result = result.bg(gpui::Rgba {
                a: 0.12,
                ..theme::danger()
            });
        }
        if unknown {
            result = result.bg(gpui::Rgba {
                a: 0.28,
                ..theme::warning()
            });
        } else if applied {
            result = result.bg(gpui::Rgba {
                a: 0.12,
                ..theme::warning()
            });
        }
        result = result.border_b_1().border_color(theme::border());
        let gutter = div()
            .w(px(layout.gutter_width))
            .h_full()
            .px(px(8.0))
            .flex()
            .flex_shrink_0()
            .items_center()
            .bg(theme::grid_gutter())
            .child(
                div()
                    .debug_selector(move || format!("pending-delete-row-{row}"))
                    .w(px(if pending_delete || applied || unknown { 10.0 } else { 0.0 }))
                    .flex_shrink_0()
                    .overflow_hidden()
                    .text_color(if (applied || unknown) && !pending_delete {
                        theme::warning()
                    } else {
                        theme::danger()
                    })
                    .when(pending_delete && !unknown, |this| this.child("−"))
                    .when(unknown, |this| this.child("?"))
                    .when(applied && !unknown && !pending_delete, |this| this.child("↻")),
            )
            .when(display.show_row_numbers, |this| {
                this.child((row + 1).to_string())
            });
        // With pinned columns the gutter and those columns form the sticky
        // block, drawn last (on top of what scrolls under it); the row holds an
        // empty placeholder of the block's width so the scrolling cells keep
        // their places.
        let sticky_width = layout.gutter_width + layout.leading_width(frozen);
        let mut sticky_cells = None;
        if frozen > 0 {
            result = result.child(div().w(px(sticky_width)).h_full().flex_shrink_0());
            sticky_cells = Some(
                div()
                    .flex()
                    .items_center()
                    .size_full()
                    .child(gutter),
            );
        } else {
            result = result.child(gutter);
        }

        let active_range = selection.range.map(|range| range.normalized());
        let display_column_count = view.display_column_count(source_columns);
        // Find-bar highlights: this row's matches (a sub-slice of the sorted
        // match list, found by binary search) and the current match; both are
        // allocation-free and empty when no search is active.
        let find = &view.grid_ui.find;
        let find_row = find.matches.row_cells(row);
        let find_current = find.current_cell();
        let pinned_segment = crate::grid::ColumnSegment {
            columns: 0..frozen,
            spacer: 0.0,
        };
        for segment in std::iter::once(&pinned_segment)
            .filter(|segment| !segment.columns.is_empty())
            .chain(columns.iter())
        {
            if segment.spacer > 0.0 {
                result = result.child(div().w(px(segment.spacer)).h_full().flex_shrink_0());
            }
            for column in segment.columns.clone() {
                let Some(source_column) = view.source_column_for_display(column, source_columns)
                else {
                    continue;
                };
                let in_selection = active_range
                    .is_some_and(|range: CellRange| range.contains(CellPosition { row, column }));
                let position = CellPosition { row, column };
                let find_match = !find_row.is_empty()
                    && find_row
                        .binary_search_by_key(&column, |cell| cell.column)
                        .is_ok();
                let find_is_current = find_match && find_current == Some(position);
                let edit_info = row_edit
                    .filter(|edit| edit.position.column == column)
                    .map(|edit| (edit.floating, edit.read_only));
                let editing = edit_info.is_some();
                let floating_editing = edit_info.is_some_and(|(floating, _)| floating);
                let pending_text = runtime
                    .and_then(|runtime| runtime.pending_cell_text(position))
                    .map(pending_cell_preview);
                let is_pending = pending_text.is_some();
                let width = layout
                    .widths
                    .get(column)
                    .copied()
                    .unwrap_or(crate::grid::ColumnLayout::DEFAULT_WIDTH);
                let mut cell = div()
                    .id(("grid-cell", row * display_column_count + column))
                    .w(px(width))
                    .h_full()
                    .px(px(8.0))
                    .flex()
                    .flex_shrink_0()
                    .items_center()
                    .when(floating_editing, |this| this.relative())
                    .when(!floating_editing, |this| this.overflow_hidden())
                    .whitespace_nowrap()
                    .text_ellipsis()
                    .border_r_1()
                    .border_color(theme::border())
                    .text_color(theme::text())
                    .debug_selector(move || format!("grid-body-cell-{row}-{column}"));
                if editing {
                    cell = self.inline_editor_cell(
                        cell,
                        tab_id,
                        row * display_column_count + column,
                        width,
                        cx,
                    );
                } else if let Some(text) = pending_text {
                    // A staged NULL reads muted like a stored one; a staged
                    // string `NULL` keeps the normal text color. Only staged
                    // cells scan the (short) staged list.
                    let staged_null = runtime.is_some_and(|runtime| {
                        runtime.pending_edits.iter().any(|edit| {
                            edit.row == row
                                && edit.column == column
                                && matches!(edit.request.value, MutationValue::Null)
                        })
                    });
                    if staged_null {
                        cell = cell
                            .text_color(grid_cell_text_color(true))
                            .child(display.null_text.clone());
                    } else {
                        cell = cell.child(text);
                    }
                } else {
                    let position = CellPosition {
                        row,
                        column: source_column,
                    };
                    // Real NULLs read as a placeholder, not data — muted, so
                    // they stand apart from a literal 'NULL' string value.
                    let (text, is_null) = display.cell_text(grid, position);
                    if is_null {
                        cell = cell.text_color(grid_cell_text_color(true));
                    }
                    cell = cell.child(text);
                }
                // `text_right` is a no-op on a flex cell (the text node only
                // spans its content), so numeric cells switch to block layout:
                // the text then fills the cell width, aligns right and keeps
                // ellipsis truncation. Line height re-centers it vertically
                // (row height minus the bottom border). Numeric-ness comes from
                // the cached header label category, so native names such as
                // `bigint`/`numeric` align like Arrow/BSON numbers.
                if !editing && display.right_align_numbers && view.column_is_numeric(source_column)
                {
                    cell = cell
                        .block()
                        .text_right()
                        .line_height(px(theme::ROW_HEIGHT - 1.0));
                }
                if editing {
                    let invalid = view
                        .grid_ui
                        .inline_edit
                        .as_ref()
                        .is_some_and(|edit| edit.error.is_some());
                    cell = cell.border_1().border_color(if invalid {
                        theme::danger()
                    } else {
                        theme::accent()
                    });
                } else {
                    if is_pending {
                        cell = cell.bg(theme::pending_cell());
                    } else if find_is_current {
                        cell = cell.bg(theme::find_match_current());
                    } else if find_match {
                        cell = cell.bg(theme::find_match());
                    } else if in_selection {
                        cell = cell.bg(theme::accent_surface());
                    }
                    if in_selection || find_is_current {
                        cell = cell.border_1().border_color(theme::accent());
                    }
                }
                let hoverable = !in_selection
                    && !editing
                    && !is_pending
                    && !pending_delete
                    && !find_match;
                cell = cell.when(hoverable, |this| {
                    this.hover(|style| style.bg(theme::hover()))
                });
                if column < frozen {
                    sticky_cells = sticky_cells.map(|block| block.child(cell));
                } else {
                    result = result.child(cell);
                }
            }
        }
        if let Some(cells) = sticky_cells {
            // Opaque under the row's own (translucent) tint, so the columns
            // scrolling under the block do not show through.
            let tint = if unknown {
                Some(gpui::Rgba { a: 0.28, ..theme::warning() })
            } else if applied {
                Some(gpui::Rgba { a: 0.12, ..theme::warning() })
            } else if pending_delete {
                Some(gpui::Rgba { a: 0.12, ..theme::danger() })
            } else if row % 2 == 1 {
                Some(theme::grid_alt_row())
            } else {
                None
            };
            result = result.child(super::sticky::StickyLeft::new(
                &view.grid_horizontal_scroll,
                div()
                    .absolute()
                    .left_0()
                    .top_0()
                    .h_full()
                    .w(px(sticky_width))
                    .bg(theme::background())
                    .child(cells.when_some(tint, |this, tint| this.bg(tint)))
                    .child(sticky_divider()),
            ));
        }
        result
    }

    /// Puts the open inline editor into `cell` (a grid cell or a record view
    /// value of width `width`; `element_index` keys the floating editor).
    /// Returns `cell` unchanged when no editor is open.
    pub(in crate::app) fn inline_editor_cell(
        &self,
        mut cell: gpui::Stateful<Div>,
        tab_id: u64,
        element_index: usize,
        width: f32,
        cx: &mut Context<Self>,
    ) -> gpui::Stateful<Div> {
        let Some(edit) = self.view(tab_id).grid_ui.inline_edit.as_ref() else {
            return cell;
        };
        let (floating_editing, read_only) = (edit.floating, edit.read_only);
        if floating_editing {
            let editor_for_key = edit
                .editor
                .as_textarea()
                .expect("floating cell uses a textarea")
                .clone();
            let floating_editor = div()
                .id(("inline-cell-editor", element_index))
                .debug_selector(|| "inline-cell-editor-floating".into())
                .absolute()
                .left_0()
                .top_0()
                .w(px(width.max(128.0)))
                .bg(theme::surface())
                .border_1()
                .border_color(if read_only {
                    theme::border()
                } else {
                    theme::accent()
                })
                .rounded(px(4.0))
                .shadow_lg()
                .occlude()
                .capture_key_down(cx.listener(
                    move |this, event: &gpui::KeyDownEvent, window, cx| {
                        if event.is_held {
                            return;
                        }
                        if event.keystroke.key == "escape" {
                            cx.stop_propagation();
                            this.cancel_inline_edit(tab_id, window, cx);
                        } else if event.keystroke.key == "enter" {
                            cx.stop_propagation();
                            if !read_only && event.keystroke.modifiers.shift {
                                editor_for_key.update(cx, |state, cx| {
                                    state.replace("\n", window, cx);
                                });
                            } else {
                                this.commit_inline_edit(
                                    tab_id,
                                    InlineEditExit::Submit,
                                    Some(window),
                                    cx,
                                );
                            }
                        }
                    },
                ))
                .child(
                    Textarea::new(
                        edit.editor
                            .as_textarea()
                            .expect("floating cell uses a textarea"),
                    )
                    .appearance(false)
                    .bordered(false)
                    .disabled(read_only)
                    .text_size(px(12.0)),
                );
            cell = cell.child(gpui::deferred(floating_editor).with_priority(2));
        } else {
            cell = cell.on_mouse_down_out(cx.listener(move |this, _, _, cx| {
                this.commit_inline_edit(tab_id, InlineEditExit::FocusLoss, None, cx);
            }));
            cell = cell.child(
                gpui_component::input::Input::new(
                    edit.editor.as_input().expect("inline cell uses an input"),
                )
                .xsmall()
                .size_full()
                .p_0()
                .text_size(px(12.0))
                .appearance(false)
                .disabled(read_only),
            );
        }
        cell
    }

    /// Display cell of `tab_id`'s grid under a window-space point (see
    /// `TabView::cell_at`).
    pub(in crate::app) fn grid_cell_at(
        &self,
        tab_id: u64,
        point: gpui::Point<gpui::Pixels>,
    ) -> Option<crate::grid::CellPosition> {
        let grid = self.grid(tab_id);
        self.view(tab_id)
            .cell_at(point, grid.row_count(), grid.column_count())
    }

    /// Wrapper around the virtualized rows. Cell interaction is hoisted here
    /// so each frame allocates it once instead of once per cell: one left and
    /// one right mouse-down handler hit-test the cell, and the menu-scoped
    /// actions and the context menu are registered on this single element
    /// (menu actions bubble from the popup to its trigger, then ancestors).
    /// Keyboard actions still resolve on the focused `grid-scroll` ancestor.
    fn render_grid_body(
        &self,
        tab_id: u64,
        rows: impl IntoElement,
        content_width: f32,
        actions: GridRowActions,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        let GridRowActions {
            can_pin,
            pin_locked,
            can_stage_row,
            can_sql_database_sort,
            can_sql_clone,
        } = actions;
        let can_database_sort = can_stage_row || can_sql_database_sort;
        let menu_root = cx.entity().downgrade();
        let copy_default = self.grid_prefs.copy_default;
        let copy_root = menu_root.clone();
        let default_root = menu_root.clone();
        let generate_root = menu_root.clone();
        let export_root = menu_root.clone();
        let can_transfer = self.transfer_available(tab_id);
        let snapshot_root = menu_root.clone();
        let gate_root = menu_root;
        div()
            .id("grid-body")
            .flex()
            .flex_col()
            .flex_1()
            .min_h_0()
            .w(px(content_width))
            .on_mouse_down(
                MouseButton::Left,
                cx.listener(move |this, event: &MouseDownEvent, window, cx| {
                    let Some(position) = this.grid_cell_at(tab_id, event.position) else {
                        return;
                    };
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
                    if event.click_count >= 2 {
                        this.begin_inline_edit(tab_id, position.row, position.column, window, cx);
                        if this
                            .view(tab_id)
                            .grid_ui
                            .inline_edit
                            .as_ref()
                            .is_some_and(|edit| edit.position == position)
                        {
                            // Prevent the focusable grid parent from stealing editor focus.
                            window.prevent_default();
                        }
                    } else if event.modifiers.shift {
                        this.view(tab_id)
                            .grid_ui
                            .selection
                            .borrow_mut()
                            .extend_to(position);
                    } else {
                        this.select_grid_cell(tab_id, position);
                    }
                    this.notify_tab(tab_id, cx);
                }),
            )
            .on_mouse_down(
                MouseButton::Right,
                cx.listener(move |this, event: &MouseDownEvent, _, cx| {
                    // Runs before the deferred menu build. No target (gutter,
                    // empty area) leaves the menu empty, so it does not open.
                    let target = this.grid_cell_at(tab_id, event.position);
                    let grid_ui = &mut this.view_mut(tab_id).grid_ui;
                    match target {
                        Some(position) => {
                            grid_ui.context_cell = Some(position);
                            grid_ui.selection.borrow_mut().context_target(position);
                        }
                        None => grid_ui.context_cell = None,
                    }
                    this.notify_tab(tab_id, cx);
                }),
            )
            .on_action(cx.listener(move |this, _: &EditGridCell, window, cx| {
                if this.view(tab_id).grid_ui.inline_edit.is_some() {
                    return;
                }
                if let Some(position) = this.view(tab_id).grid_ui.context_cell {
                    this.select_grid_cell(tab_id, position);
                    this.open_cell_editor(tab_id, window, cx);
                }
            }))
            .on_action(cx.listener(move |this, _: &DeleteGridRow, window, cx| {
                if can_stage_row || can_sql_database_sort {
                    if let Some(position) = this.view(tab_id).grid_ui.context_cell {
                        this.select_grid_cell(tab_id, position);
                        this.confirm_delete_selected_row(tab_id, window, cx);
                    }
                }
            }))
            .on_action(cx.listener(move |this, _: &CloneGridRow, window, cx| {
                let Some(position) = this.view(tab_id).grid_ui.context_cell else {
                    return;
                };
                if this
                    .workspace
                    .tab(tab_id)
                    .is_some_and(|tab| tab.query_mode.is_mongo_find())
                {
                    this.select_grid_cell(tab_id, position);
                    this.open_mongo_duplicate_row_editor(tab_id, window, cx);
                } else if can_sql_database_sort {
                    this.clone_context_sql_row(tab_id, position.row, window, cx);
                }
            }))
            .on_action(cx.listener(move |this, _: &SortGridPageAscending, window, cx| {
                this.sort_context_page(tab_id, false, window, cx);
            }))
            .on_action(
                cx.listener(move |this, _: &SortGridDatabaseAscending, window, cx| {
                    this.sort_context_database(tab_id, false, window, cx);
                }),
            )
            .on_action(
                cx.listener(move |this, _: &SortGridDatabaseDescending, window, cx| {
                    this.sort_context_database(tab_id, true, window, cx);
                }),
            )
            .on_action(cx.listener(move |this, _: &SortGridPageDescending, window, cx| {
                this.sort_context_page(tab_id, true, window, cx);
            }))
            .on_action(cx.listener(move |this, _: &ClearGridSort, window, cx| {
                this.clear_context_sort(tab_id, window, cx);
            }))
            .on_action(cx.listener(move |this, _: &RefreshGridResults, window, cx| {
                let view = this.view(tab_id);
                if view.grid_ui.inline_edit.is_none() && view.detail_ui.edit.is_none() {
                    this.refresh_tab_results(tab_id, window, cx);
                }
            }))
            .on_action(cx.listener(move |this, _: &OpenGridCellDetails, _, cx| {
                this.open_context_cell_details(tab_id, cx);
            }))
            .on_action(cx.listener(move |this, _: &OpenGridColumnDetails, _, cx| {
                this.open_context_column_details(tab_id, cx);
            }))
            .on_action(cx.listener(move |this, _: &OpenGridRowDetails, _, cx| {
                this.open_context_row_details(tab_id, cx);
            }))
            .on_action(cx.listener(move |this, _: &TransposeGridRow, _, cx| {
                this.open_context_row_details(tab_id, cx);
            }))
            .on_action(cx.listener(move |this, _: &PinGridColumn, _, cx| {
                if let Some(position) = this.view(tab_id).grid_ui.context_cell {
                    this.pin_column(tab_id, position.column, cx);
                }
            }))
            .on_action(cx.listener(move |this, _: &UnpinGridColumn, _, cx| {
                if let Some(position) = this.view(tab_id).grid_ui.context_cell {
                    this.unpin_column(tab_id, position.column, cx);
                }
            }))
            .on_action(cx.listener(move |this, _: &ClearGridSelection, _, cx| {
                this.view(tab_id).grid_ui.selection.borrow_mut().clear();
                this.notify_tab(tab_id, cx);
            }))
            .on_action(cx.listener(move |this, _: &SetGridSelectionNull, window, cx| {
                this.set_context_selection_null(tab_id, window, cx);
            }))
            .on_action(cx.listener(move |this, _: &BulkEditGridSelection, window, cx| {
                this.open_context_bulk_edit(tab_id, window, cx);
            }))
            .child(rows)
            .context_menu(move |menu, window, cx| {
                // Only a right-click on a cell is a menu target.
                if !gate_root
                    .read_with(&*cx, |this, _| this.view(tab_id).grid_ui.context_cell.is_some())
                    .unwrap_or(false)
                {
                    return menu;
                }
                let snapshot_root = snapshot_root.clone();
                // Database sort stays visible but disabled when
                // the active result cannot run a table sort, so capability
                // state is honest instead of silently hiding the action.
                menu.menu_with_icon_and_disabled(
                    "Sort Database Ascending",
                    cell_menu_icon("icons/database.svg"),
                    Box::new(SortGridDatabaseAscending),
                    !can_database_sort,
                )
                .menu_with_icon_and_disabled(
                    "Sort Database Descending",
                    cell_menu_icon("icons/database.svg"),
                    Box::new(SortGridDatabaseDescending),
                    !can_database_sort,
                )
                .separator()
                .menu_with_icon(
                    "Sort Current Page Ascending",
                    cell_menu_icon("icons/arrow-up.svg"),
                    Box::new(SortGridPageAscending),
                )
                .menu_with_icon(
                    "Sort Current Page Descending",
                    cell_menu_icon("icons/arrow-down.svg"),
                    Box::new(SortGridPageDescending),
                )
                .menu_with_icon(
                    "Clear Sort",
                    cell_menu_icon("icons/eraser.svg"),
                    Box::new(ClearGridSort),
                )
                .separator()
                .menu_with_icon(
                    "Open Cell Details",
                    cell_menu_icon("icons/eye.svg"),
                    Box::new(OpenGridCellDetails),
                )
                .menu_with_icon(
                    "Open Column Details",
                    cell_menu_icon("icons/columns-3.svg"),
                    Box::new(OpenGridColumnDetails),
                )
                .menu_with_icon(
                    "Open Row Details",
                    cell_menu_icon("icons/rows-3.svg"),
                    Box::new(OpenGridRowDetails),
                )
                .separator()
                .when(can_pin, |menu| {
                    let pinned = gate_root
                        .read_with(&*cx, |this, _| {
                            let ui = &this.view(tab_id).grid_ui;
                            ui.context_cell.is_some_and(|cell| cell.column < ui.pinned)
                        })
                        .unwrap_or(false);
                    if pinned {
                        menu.menu_with_icon_and_disabled(
                            "Unpin Column",
                            cell_menu_icon("icons/pin.svg"),
                            Box::new(UnpinGridColumn),
                            pin_locked,
                        )
                    } else {
                        menu.menu_with_icon_and_disabled(
                            "Pin Column",
                            cell_menu_icon("icons/pin.svg"),
                            Box::new(PinGridColumn),
                            pin_locked,
                        )
                    }
                })
                .submenu_with_icon(
                    Some(cell_menu_icon("icons/copy.svg")),
                    "Copy",
                    window,
                    cx,
                    {
                        let copy_root = copy_root.clone();
                        let default_root = default_root.clone();
                        let dsv_root = copy_root.clone();
                        let sql_available = can_sql_database_sort;
                        move |submenu, window, cx| {
                            let submenu = crate::app::results::copy::CopyFormat::ALL.iter().fold(
                                submenu,
                                |submenu, (format, label)| {
                                    // SQL formats truly cannot work without
                                    // a connected, unambiguous table
                                    // result; keep them visible but
                                    // disabled instead of deleting them.
                                    if !sql_available
                                        && matches!(
                                            format,
                                            crate::app::results::copy::CopyFormat::SqlIn
                                                | crate::app::results::copy::CopyFormat::SqlInsert
                                                | crate::app::results::copy::CopyFormat::SqlInsertNoPk
                                                | crate::app::results::copy::CopyFormat::SqlUpdate
                                                | crate::app::results::copy::CopyFormat::SqlSelect
                                                | crate::app::results::copy::CopyFormat::SqlWhere
                                        )
                                    {
                                        return submenu.item(
                                            PopupMenuItem::new(*label)
                                                .icon(cell_menu_icon("icons/database.svg"))
                                                .disabled(true),
                                        );
                                    }
                                    let root = copy_root.clone();
                                    let format = *format;
                                    submenu.item(PopupMenuItem::new(*label).on_click(move |_, _, app| {
                                        let _ = root.update(app, |this, cx| this.copy_scope_to_clipboard(tab_id, crate::app::results::copy::CopyScope::Selected, format, cx));
                                    }))
                                },
                            );
                            let submenu = submenu.separator();
                            let submenu = [
                                ("Copy Cell", crate::app::results::copy::CopyScope::Cell),
                                ("Copy Selected Rows", crate::app::results::copy::CopyScope::Rows),
                                ("Copy All (Current Page)", crate::app::results::copy::CopyScope::Page),
                                ("Copy Column Names", crate::app::results::copy::CopyScope::ColumnNames),
                            ].into_iter().fold(submenu, |submenu, (label, scope)| {
                                let root = copy_root.clone();
                                submenu.item(PopupMenuItem::new(label).on_click(move |_, _, app| {
                                    let _ = root.update(app, |this, cx| this.copy_scope_to_clipboard(tab_id, scope, crate::app::results::copy::CopyFormat::Smart, cx));
                                }))
                            });
                            let submenu = submenu.separator().item(
                                PopupMenuItem::new("Configure DSV…")
                                    .icon(cell_menu_icon("icons/sliders-horizontal.svg"))
                                    .on_click({
                                        let root = dsv_root.clone();
                                        move |_, window, app| {
                                            let _ = root.update(app, |this, cx| this.open_copy_dsv_dialog(tab_id, window, cx));
                                        }
                                    }),
                            );
                            let default_root = default_root.clone();
                            let submenu = submenu.when(!sql_available, |submenu| {
                                submenu
                                    .separator()
                                    .label("SQL copy needs")
                                    .label("a connected table.")
                            });
                            submenu.separator().submenu_with_icon(
                                Some(cell_menu_icon("icons/settings-2.svg")),
                                "Configure Default",
                                window,
                                cx,
                                move |submenu, _, _| {
                                    std::iter::once(&(crate::app::results::copy::CopyFormat::Smart, "Smart"))
                                        .chain(crate::app::results::copy::CopyFormat::ALL.iter())
                                        .fold(submenu, |submenu, (format, label)| {
                                        let root = default_root.clone();
                                        let format = *format;
                                        submenu.item(PopupMenuItem::new(*label).checked(copy_default == format).on_click(move |_, _, app| {
                                            let _ = root.update(app, |this, cx| this.set_copy_default(tab_id, format, cx));
                                        }))
                                    })
                                },
                            )
                        }
                    },
                )
                .menu_with_icon(
                    "Set NULL",
                    cell_menu_icon("icons/circle-x.svg"),
                    Box::new(SetGridSelectionNull),
                )
                .menu_with_icon(
                    "Bulk Edit Selection",
                    cell_menu_icon("icons/pencil.svg"),
                    Box::new(BulkEditGridSelection),
                )
                .submenu_with_icon(
                    Some(cell_menu_icon("icons/wand-sparkles.svg")),
                    "Generate Value",
                    window,
                    cx,
                    {
                        let generate_root = generate_root.clone();
                        move |submenu, _, _| {
                            let submenu = [
                                ("Empty String", crate::app::results::context_menu::GenerateKind::EmptyString, "icons/type.svg"),
                                ("NULL", crate::app::results::context_menu::GenerateKind::Null, "icons/circle-x.svg"),
                                ("Current DateTime", crate::app::results::context_menu::GenerateKind::CurrentDateTime, "icons/calendar-clock.svg"),
                                ("Current Date", crate::app::results::context_menu::GenerateKind::CurrentDate, "icons/calendar-days.svg"),
                                ("UUID", crate::app::results::context_menu::GenerateKind::Uuid, "icons/key-round.svg"),
                            ].into_iter().fold(submenu, |submenu, (label, kind, icon)| {
                                let root = generate_root.clone();
                                submenu.item(PopupMenuItem::new(label).icon(cell_menu_icon(icon)).on_click(move |_, _, app| {
                                    let _ = root.update(app, |this, cx| {
                                        if let Err(error) = this.generate_context_selection(tab_id, kind, cx) {
                                            this.view_mut(tab_id).detail_ui.error = Some(error); this.notify_tab(tab_id, cx);
                                        }
                                    });
                                }))
                            });
                            submenu
                                .item(PopupMenuItem::new("Increment…").icon(cell_menu_icon("icons/hash.svg")).on_click({
                                    let root = generate_root.clone();
                                    move |_, window, app| {
                                        let _ = root.update(app, |this, cx| this.open_context_increment_dialog(tab_id, window, cx));
                                    }
                                }))
                                .separator()
                                .item(
                                    PopupMenuItem::new("Snowflake…")
                                        .icon(cell_menu_icon("icons/snowflake.svg"))
                                        .on_click({
                                            let root = generate_root.clone();
                                            move |_, window, app| {
                                                let _ = root.update(app, |this, cx| {
                                                    this.open_context_snowflake_dialog(tab_id, window, cx)
                                                });
                                            }
                                        }),
                                )
                        }
                    },
                )
                .menu_with_icon(
                    "Transpose Row",
                    cell_menu_icon("icons/arrow-left-right.svg"),
                    Box::new(TransposeGridRow),
                )
                .menu_with_icon(
                    "Clear Selection",
                    cell_menu_icon("icons/square-dashed.svg"),
                    Box::new(ClearGridSelection),
                )
                .separator()
                .item(
                    PopupMenuItem::new("Screenshot Selected Cells")
                        .icon(cell_menu_icon("icons/camera.svg"))
                        .on_click(move |_, window, app| {
                            let _ = snapshot_root.update(app, |this, cx| {
                                this.open_selected_snapshot_preview(tab_id, window, cx)
                            });
                        }),
                )
                .separator()
                .when(can_stage_row, |menu| {
                    menu.menu_with_icon(
                        "Clone as New Row",
                        cell_menu_icon("icons/circle-plus.svg"),
                        Box::new(CloneGridRow),
                    )
                })
                .when(can_sql_clone, |menu| {
                    menu.menu_with_icon(
                        "Clone as New Row",
                        cell_menu_icon("icons/circle-plus.svg"),
                        Box::new(CloneGridRow),
                    )
                })
                .when(can_sql_database_sort && !can_stage_row && !can_sql_clone, |menu| {
                    menu.item(
                        PopupMenuItem::new("Clone as New Row")
                            .icon(cell_menu_icon("icons/circle-plus.svg"))
                            .disabled(true),
                    )
                    .label("SQL clone needs complete")
                    .label("generated/default metadata.")
                })
                .when(can_stage_row || can_sql_database_sort, |menu| {
                    menu.when(!can_stage_row, |menu| menu.separator())
                        // Danger treatment keeps the standard item
                        // chrome (hover, keyboard, dismissal); only the
                        // label carries the danger color. Confirmation
                        // stays the destructive gate.
                        .item(
                            PopupMenuItem::element(|_, _| {
                                div().text_color(theme::danger()).child("Delete Row…")
                            })
                            .icon(cell_menu_icon("icons/trash.svg"))
                            .action(Box::new(DeleteGridRow)),
                        )
                })
                .separator()
                .submenu_with_icon(
                    Some(cell_menu_icon("icons/download.svg")),
                    "Export",
                    window,
                    cx,
                    {
                        let export_root = export_root.clone();
                        let sql_available = can_sql_database_sort;
                        move |submenu, _, _| {
                            let submenu = submenu.label("Selected Cells");
                            let submenu = crate::app::results::export::ContextExportFormat::ALL.iter().fold(
                                submenu,
                                |submenu, (format, label)| {
                                    let root = export_root.clone();
                                    let format = *format;
                                    submenu.item(
                                        PopupMenuItem::new(*label)
                                            .disabled(format == crate::app::results::export::ContextExportFormat::Sql && !sql_available)
                                            .on_click(move |_, window, app| {
                                                let _ = root.update(app, |this, cx| this.export_context_selection(tab_id, format, false, window, cx));
                                            }),
                                    )
                                },
                            );
                            let submenu = submenu.separator().label("Current Page");
                            let submenu = crate::app::results::export::ContextExportFormat::ALL.iter().fold(
                                submenu,
                                |submenu, (format, label)| {
                                    let root = export_root.clone();
                                    let format = *format;
                                    submenu.item(
                                        PopupMenuItem::new(format!("Page {label}"))
                                            .disabled(format == crate::app::results::export::ContextExportFormat::Sql && !sql_available)
                                            .on_click(move |_, window, app| {
                                                let _ = root.update(app, |this, cx| this.export_context_selection(tab_id, format, true, window, cx));
                                            }),
                                    )
                                },
                            );
                            let transfer_root = export_root.clone();
                            submenu
                                .when(!sql_available, |submenu| {
                                    submenu
                                        .separator()
                                        .label("SQL export needs")
                                        .label("a connected table.")
                                })
                                .when(can_transfer, |submenu| {
                                    submenu.separator().item(
                                        PopupMenuItem::new("Whole Result to Files or Tables\u{2026}")
                                            .icon(cell_menu_icon("icons/arrow-left-right.svg"))
                                            .on_click(move |_, window, app| {
                                                let _ = transfer_root.update(app, |this, cx| this.open_transfer_from_tab(tab_id, window, cx));
                                            }),
                                    )
                                })
                        }
                    },
                )
                .separator()
                .menu_with_icon(
                    "Refresh",
                    cell_menu_icon("icons/refresh-cw.svg"),
                    Box::new(RefreshGridResults),
                )
                .when(!can_database_sort, |menu| {
                    menu.label("Database sort needs")
                        .label("a connected table.")
                })
            })
    }

    fn render_result_empty_state(&self, tab_id: u64) -> AnyElement {
        use crate::query::QueryStatus;
        let status = self
            .workspace
            .runtime(tab_id)
            .map(|runtime| runtime.query.status());
        // A failed shell run says why in the strip above; the empty area does not say it again.
        let shell_failed = self
            .shell_view(tab_id)
            .is_some_and(|view| view.error.is_some());
        let message = match status {
            _ if shell_failed => "No result: the script failed".to_string(),
            Some(QueryStatus::Ready { .. }) => "No rows returned".to_string(),
            Some(QueryStatus::Running { .. }) => "Running query…".to_string(),
            Some(QueryStatus::Failed(error)) => {
                format!("Query failed: {}", crate::sessions::redact_secrets(error))
            }
            Some(QueryStatus::Cancelled) => "Query cancelled".to_string(),
            _ => "Run a query to see results".to_string(),
        };
        div()
            .debug_selector(|| "result-empty-state".into())
            .flex_1()
            .min_h_0()
            .min_w_0()
            .size_full()
            .flex()
            .items_center()
            .justify_center()
            .p(px(16.0))
            .text_size(px(12.0))
            .text_color(theme::text_muted())
            .child(
                div()
                    .w_full()
                    .text_center()
                    .whitespace_normal()
                    .child(message),
            )
            .into_any_element()
    }

    /// The sort the shown result has: a table-data tab's first ORDER BY term,
    /// a Mongo find's first sort key, a custom SQL result's database sort, and
    /// the page sort of the loaded rows.
    pub(in crate::app) fn header_sort(&self, tab_id: u64) -> HeaderSort {
        let tab = self.workspace.tab(tab_id);
        let runtime = self.workspace.runtime(tab_id);
        let database = match (tab, runtime) {
            (Some(tab), _) if tab.query_mode.is_table_data() => {
                crate::sql_table::primary_sort(&tab.applied_order_by)
            }
            (Some(tab), Some(runtime)) if tab.query_mode.is_mongo_find() => {
                first_sort_key(&runtime.mongo.sort)
            }
            (Some(tab), Some(runtime)) if tab.query_mode.is_sql_table() => runtime
                .context_sql_sort
                .as_ref()
                .map(|sort| (sort.column.clone(), sort.descending)),
            _ => None,
        };
        HeaderSort {
            database,
            page: self.grid(tab_id).page_sort(),
        }
    }

    /// The table grid (header, virtualized rows, scrollbars).
    fn render_grid_pane(&mut self, tab_id: u64, window: &Window, cx: &mut Context<Self>) -> Div {
        // Header type labels resolve once per result/catalog change.
        self.sync_column_type_labels(tab_id);
        // Re-run an open find when the page or visible columns changed.
        self.sync_find_with_grid(tab_id, cx);
        let grid_focus = self.view_mut(tab_id).grid_focus(cx);
        // Pinned columns stick to the left edge unless they would take most of
        // the grid (then they scroll with the rest, still left of it).
        let frozen = {
            let ui = &self.view(tab_id).grid_ui;
            let measured = f32::from(self.view(tab_id).grid_horizontal_scroll.bounds().size.width);
            let pane = if measured > 0.0 {
                measured
            } else {
                f32::from(window.viewport_size().width)
            };
            let sticky = ui.layout.gutter_width + ui.layout.leading_width(ui.pinned);
            if ui.pinned > 0 && sticky <= STICKY_MAX_SHARE * pane {
                ui.pinned
            } else {
                0
            }
        };
        self.view_mut(tab_id).grid_ui.frozen = frozen;
        let view = self.view(tab_id);
        let grid = self.grid(tab_id);
        let find = &view.grid_ui.find;
        let find_bar = self.render_grid_find_bar(tab_id, cx);
        let layout = &view.grid_ui.layout;
        let content_width = layout.content_width();
        let can_sql_database_sort = self.sql_sort_available(tab_id);
        let can_sql_clone = self.sql_clone_available(tab_id);
        let can_stage_row = self
            .workspace
            .tab(tab_id)
            .is_some_and(|tab| tab.query_mode.is_mongo_find());
        // Horizontal culling, computed once per frame. The offset is the live
        // scroll state; the viewport is the window width (never stale and
        // never narrower than the grid, so it only over-renders by the side
        // panels). A zero-sized window falls back to a 2560px viewport.
        let window_width = f32::from(window.viewport_size().width);
        let viewport_width = if window_width > 0.0 {
            window_width
        } else {
            2560.0
        };
        let scroll_left = (-f32::from(view.grid_horizontal_scroll.offset().x))
            .clamp(0.0, (content_width - viewport_width).max(0.0));
        let editing_column = view
            .grid_ui
            .inline_edit
            .as_ref()
            .map(|edit| edit.position.column);
        let columns = layout.column_window(scroll_left, viewport_width, editing_column, frozen);
        // Table-data tabs sort by clicking a header (ASC -> DESC -> default),
        // mirrored in ORDER BY; every header's sort button shows the active sort.
        let table_data = self
            .workspace
            .tab(tab_id)
            .is_some_and(|tab| tab.query_mode.is_table_data());
        let header_sort = self.header_sort(tab_id);
        let can_database_sort = can_stage_row || can_sql_database_sort;
        let sort_menu = view.grid_ui.sort_menu;
        let pinned_segment = crate::grid::ColumnSegment {
            columns: 0..frozen,
            spacer: 0.0,
        };
        let pin_menu = HeaderPinMenu {
            available: self.pinning_available(tab_id),
            locked: self.pinning_locked(tab_id),
            pinned: view.grid_ui.pinned,
            first_frozen: self.first_column_frozen(tab_id),
            root: cx.entity().downgrade(),
        };
        let gutter_header = div()
            .w(px(layout.gutter_width))
            .h_full()
            .flex_shrink_0()
            .flex()
            .items_center()
            .justify_center()
            .text_color(theme::text_faint())
            .child("#");
        let sticky_width = layout.gutter_width + layout.leading_width(frozen);
        let mut header_row = div()
            .id(("grid-header", tab_id))
            .h(px(theme::RESULT_GRID_HEADER_HEIGHT))
            .flex()
            .flex_shrink_0()
            .bg(theme::grid_header())
            .border_b_1()
            .border_color(theme::border())
            .text_size(px(11.0))
            .text_color(theme::text_muted());
        // Same split as the rows: gutter and pinned columns in the sticky block.
        let sticky_header = if frozen > 0 {
            header_row = header_row
                .relative()
                .child(div().w(px(sticky_width)).h_full().flex_shrink_0());
            Some(div().flex().size_full().child(gutter_header))
        } else {
            header_row = header_row.child(gutter_header);
            None
        };
        let (header, sticky_header) = std::iter::once(&pinned_segment)
            .filter(|segment| !segment.columns.is_empty())
            .chain(columns.iter())
            .fold(
            (header_row, sticky_header),
            |(header, sticky), segment| {
                let header = if segment.spacer > 0.0 {
                    header.child(div().w(px(segment.spacer)).h_full().flex_shrink_0())
                } else {
                    header
                };
                segment.columns.clone().fold((header, sticky), |(header, sticky), column| {
                    let Some(source_column) =
                        view.source_column_for_display(column, grid.column_count())
                    else {
                        return (header, sticky);
                    };
                    let width = layout
                        .widths
                        .get(column)
                        .copied()
                        .unwrap_or(crate::grid::ColumnLayout::DEFAULT_WIDTH);
                    let column_type = view.header_column_type(source_column);
                    let type_color = grid_column_type_color(&column_type);
                    let resize_handle = div()
                        .id(("grid-column-resize", column))
                        .absolute()
                        .top_0()
                        .right_0()
                        .bottom_0()
                        .w(px(8.0))
                        .occlude()
                        .cursor_col_resize()
                        .on_mouse_down(
                            MouseButton::Left,
                            cx.listener(move |this, event: &gpui::MouseDownEvent, _, _| {
                                this.view_mut(tab_id)
                                    .begin_column_resize(column, event.position.x);
                            }),
                        )
                        .on_mouse_up(
                            MouseButton::Left,
                            cx.listener(move |this, _, _, _| this.end_column_resize(tab_id)),
                        )
                        .on_mouse_up_out(
                            MouseButton::Left,
                            cx.listener(move |this, _, _, _| this.end_column_resize(tab_id)),
                        )
                        .on_drag(column, |_, _, _, cx| cx.new(|_| ColumnResizeGhost))
                        .on_drag_move(cx.listener(
                            move |this, event: &gpui::DragMoveEvent<usize>, _, cx| {
                                this.view_mut(tab_id)
                                    .update_column_resize(event.event.position.x);
                                this.notify_tab(tab_id, cx);
                            },
                        ));
                    let column_name = grid
                        .column_name(source_column)
                        .unwrap_or("")
                        .to_string();
                    let column_sort = ColumnSort {
                        database: header_sort
                            .database
                            .as_ref()
                            .filter(|(name, _)| *name == column_name)
                            .map(|(_, descending)| *descending),
                        page: header_sort
                            .page
                            .filter(|(sorted, _)| *sorted == source_column)
                            .map(|(_, descending)| descending),
                        can_database_sort,
                        menu_open: sort_menu == Some(column),
                    };
                    let header_cell = div()
                        .id(("grid-header-cell", column))
                        .relative()
                        .when(find.matches.is_header_match(column), |this| {
                            this.bg(if find.current_target() == Some(FindTarget::Column(column)) {
                                theme::find_match_current()
                            } else {
                                theme::find_match()
                            })
                        })
                        .when(table_data, |this| {
                            let sort_name = column_name.clone();
                            this.cursor_pointer().on_click(cx.listener(
                                move |this, _, window, cx| {
                                    this.sort_sql_table_by_header(tab_id, &sort_name, window, cx);
                                },
                            ))
                        })
                        .w(px(width))
                        .flex_shrink_0()
                        .px(px(8.0))
                        .flex()
                        .flex_col()
                        .justify_center()
                        .gap(px(2.0))
                        .overflow_hidden()
                        .whitespace_nowrap()
                        .text_ellipsis()
                        .border_r_1()
                        .border_color(theme::border())
                        .child(
                            div()
                                .flex()
                                .items_center()
                                .gap(px(4.0))
                                .child(
                                    div()
                                        .flex_1()
                                        .min_w_0()
                                        .overflow_hidden()
                                        .whitespace_nowrap()
                                        .text_ellipsis()
                                        .text_color(theme::text())
                                        .child(column_name.clone()),
                                )
                                .when(column < pin_menu.pinned, |this| {
                                    this.child(
                                        Icon::empty()
                                            .path("icons/pin.svg")
                                            .size(px(10.0))
                                            .text_color(theme::accent())
                                            .flex_shrink_0(),
                                    )
                                })
                                .child(header_sort_button(
                                    tab_id,
                                    column,
                                    column_sort,
                                    pin_menu.root.clone(),
                                )),
                        )
                        .child({
                            // Long native names (`timestamp(3) with time zone`)
                            // truncate and reveal the full name in a tooltip.
                            let long_type = column_type.len() > 10;
                            let tooltip_text = column_type.clone();
                            div()
                                .id(("grid-header-type", column))
                                .min_w_0()
                                .overflow_hidden()
                                .whitespace_nowrap()
                                .text_ellipsis()
                                .text_size(px(10.0))
                                .text_color(type_color)
                                .when(long_type, |this| {
                                    this.tooltip(move |window, cx| {
                                        gpui_component::tooltip::Tooltip::new(tooltip_text.clone())
                                            .build(window, cx)
                                    })
                                })
                                .child(column_type)
                        })
                        .child(resize_handle)
                        .debug_selector(move || format!("grid-header-cell-{column}"));
                    if column < frozen {
                        (header, sticky.map(|block| block.child(header_cell)))
                    } else {
                        (header.child(header_cell), sticky)
                    }
                })
            },
        );
        // One right-click handler and one menu for the whole header row (the
        // target column is hit-tested, like the body's cells).
        let header = header
            .on_mouse_down(
                MouseButton::Right,
                cx.listener(move |this, event: &MouseDownEvent, _, _| {
                    let source_columns = this.grid(tab_id).column_count();
                    let column = this.view(tab_id).header_column_at(event.position, source_columns);
                    this.view_mut(tab_id).grid_ui.context_header = column;
                }),
            )
            .context_menu({
                let pin_menu = pin_menu.clone();
                move |menu, _, cx| {
                    let target = pin_menu
                        .root
                        .read_with(&*cx, |this, _| this.view(tab_id).grid_ui.context_header)
                        .ok()
                        .flatten();
                    match target {
                        Some(column) => pin_menu.build(menu, tab_id, column),
                        None => menu,
                    }
                }
            });
        let header = match sticky_header {
            Some(cells) => header.child(super::sticky::StickyLeft::new(
                &view.grid_horizontal_scroll,
                div()
                    .absolute()
                    .left_0()
                    .top_0()
                    .h_full()
                    .w(px(sticky_width))
                    .bg(theme::grid_header())
                    .child(cells)
                    .child(sticky_divider()),
            )),
            None => header,
        };
        let mut rows = uniform_list(
            "query-results",
            grid.row_count(),
            cx.processor(move |this, range: Range<usize>, _window, cx| {
                let view = this.view(tab_id);
                let selection = view.grid_ui.selection.borrow().clone();
                range
                    .map(|row| {
                        this.render_result_row(
                            tab_id,
                            row,
                            &view.grid_ui.layout,
                            &selection,
                            &columns,
                            cx,
                        )
                    })
                    .collect::<Vec<_>>()
            }),
        )
        .w(px(content_width))
        .flex_1()
        .min_h_0()
        .track_scroll(&view.grid_scroll);
        rows.interactivity().base_style.overflow.x = Some(gpui::Overflow::Scroll);

        let body = self.render_grid_body(
            tab_id,
            rows,
            content_width,
            GridRowActions {
                can_pin: self.pinning_available(tab_id),
                pin_locked: self.pinning_locked(tab_id),
                can_stage_row,
                can_sql_database_sort,
                can_sql_clone,
            },
            cx,
        );
        let horizontal_scrollbar = div()
            .h(px(10.0))
            .flex_shrink_0()
            .bg(theme::background())
            .child(Scrollbar::horizontal(&view.grid_horizontal_scroll).mode(ScrollbarMode::Always));
        div()
            .flex()
            .flex_col()
            .flex_1()
            .min_h_0()
            .min_w_0()
            .overflow_hidden()
            .bg(theme::background())
            .relative()
            .child(
                div()
                    .id("grid-scroll")
                    .relative()
                    .flex_1()
                    .min_h_0()
                    .min_w_0()
                    .overflow_hidden()
                    .key_context("grid")
                    .track_focus(&grid_focus)
                    .debug_selector(|| "grid-scroll-area".into())
                    .on_key_down(cx.listener(move |this, event: &gpui::KeyDownEvent, _, cx| {
                        let view = this.view(tab_id);
                        if view.grid_ui.inline_edit.is_some() {
                            return;
                        }
                        let selection = view.grid_ui.selection.borrow();
                        let Some(range) = selection.range else {
                            return;
                        };
                        let current = range.end;
                        let next = match event.keystroke.key.as_str() {
                            "up" => current.row.checked_sub(1).map(|row| CellPosition {
                                row,
                                column: current.column,
                            }),
                            "down" => {
                                (current.row + 1 < this.grid(tab_id).row_count()).then_some(CellPosition {
                                    row: current.row + 1,
                                    column: current.column,
                                })
                            }
                            "left" => current.column.checked_sub(1).map(|column| CellPosition {
                                row: current.row,
                                column,
                            }),
                            "right" => (current.column + 1 < this.display_column_count(tab_id))
                                .then_some(CellPosition {
                                    row: current.row,
                                    column: current.column + 1,
                                }),
                            _ => return,
                        };
                        drop(selection);
                        let Some(next) = next else {
                            return;
                        };
                        let view = this.view_mut(tab_id);
                        let mut selection = view.grid_ui.selection.borrow_mut();
                        if event.keystroke.modifiers.shift {
                            selection.extend_to(next);
                        } else {
                            selection.select(next);
                        }
                        drop(selection);
                        view.grid_scroll
                            .scroll_to_item(next.row, gpui::ScrollStrategy::Nearest);
                        view.scroll_to_column(next.column);
                        cx.stop_propagation();
                        this.notify_tab(tab_id, cx);
                    }))
                    .on_action(cx.listener(move |this, _: &CopyGridSelection, _, cx| {
                        this.copy_grid_selection(tab_id, cx);
                    }))
                    // Edit > Copy (the OS copy action): an inline cell editor is
                    // a deeper context and keeps handling it for its own text.
                    .on_action(cx.listener(move |this, _: &gpui_component::input::Copy, _, cx| {
                        this.copy_grid_selection(tab_id, cx);
                    }))
                    .on_action(cx.listener(move |this, _: &EditGridCell, window, cx| {
                        this.open_cell_editor(tab_id, window, cx);
                    }))
                    .on_action(cx.listener(move |this, _: &CancelCellEdit, window, cx| {
                        this.cancel_inline_edit(tab_id, window, cx);
                    }))
                    .on_action(cx.listener(move |this, _: &ToggleRecordView, _, cx| {
                        // Tab belongs to a cell editor that is open (it indents or moves focus).
                        if this.view(tab_id).grid_ui.inline_edit.is_some() {
                            cx.propagate();
                            return;
                        }
                        this.toggle_record_view(tab_id, cx);
                    }))
                    .on_action(cx.listener(move |this, _: &RefreshGridResults, window, cx| {
                        let view = this.view(tab_id);
                        if view.grid_ui.inline_edit.is_none() && view.detail_ui.edit.is_none() {
                            this.refresh_tab_results(tab_id, window, cx);
                        }
                    }))
                    .child(
                        div()
                            .id("grid-h-scroll")
                            .h_full()
                            .overflow_x_scroll()
                            .overflow_y_scroll()
                            .track_scroll(&view.grid_horizontal_scroll)
                            .child(
                                div()
                                    .flex()
                                    .flex_col()
                                    .w(px(content_width))
                                    .h_full()
                                    .child(header)
                                    .child(body),
                            ),
                    )
                    .when(grid.row_count() == 0, |this| {
                        this.child(
                            div()
                                .absolute()
                                .top(px(theme::RESULT_GRID_HEADER_HEIGHT))
                                .bottom_0()
                                .left_0()
                                .right_0()
                                .child(self.render_result_empty_state(tab_id)),
                        )
                    }),
            )
            .child(horizontal_scrollbar)
            .when_some(find_bar, |this, bar| this.child(bar))
    }

    /// A Mongo find tab showing the document/JSON view instead of the table.
    pub(in crate::app) fn mongo_documents_view_active(&self, tab_id: u64) -> bool {
        self.workspace
            .tab(tab_id)
            .and_then(|tab| {
                (tab.query_mode.is_mongo_find()).then(|| {
                    self.workspace
                        .runtime(tab.id)
                        .map(|runtime| runtime.mongo.view)
                })
            })
            .flatten()
            .is_some_and(|view| view != crate::mongo_result::MongoResultView::Table)
    }

    /// Connection state of the tab for the results area, with whether it
    /// holds a result (banner over it) or not (full placeholder).
    pub(in crate::app) fn tab_result_connection(
        &self,
        tab_id: u64,
    ) -> Option<(String, super::super::connection_ui::ResultConnectionState, bool)> {
        let tab = self.workspace.tab(tab_id)?;
        let state = self.result_connection_state(tab)?;
        Some((tab.connection_id.clone(), state, self.tab_holds_results(tab.id)))
    }

    /// The reconnect card standing in for the results while the tab's
    /// connection is not usable and it has no result to keep showing.
    pub(in crate::app) fn render_connection_state_placeholder(
        &mut self,
        tab_id: u64,
        cx: &mut Context<Self>,
    ) -> Option<AnyElement> {
        let (connection, state, holds_results) = self.tab_result_connection(tab_id)?;
        (!holds_results)
            .then(|| self.render_result_connection_state(tab_id, &connection, state, cx))
    }

    /// The slim banner over a stale result, when the tab has one.
    pub(in crate::app) fn render_connection_state_banner(
        &mut self,
        tab_id: u64,
        cx: &mut Context<Self>,
    ) -> Option<AnyElement> {
        let (connection, state, holds_results) = self.tab_result_connection(tab_id)?;
        holds_results
            .then(|| self.render_result_connection_banner(tab_id, &connection, state, cx))
    }

    pub(in crate::app) fn render_results_body(
        &mut self,
        tab_id: u64,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        if self.redis_string_view_active(tab_id) {
            return self.render_redis_string(tab_id, window, cx);
        }
        if self.plan_presentation_active(tab_id) {
            return self.render_plan_view(tab_id, cx);
        }
        if self.shell_text_active(tab_id) {
            return self.render_shell_text(tab_id, cx);
        }
        if let Some(stats) = self
            .script_stats_active(tab_id)
            .then(|| self.render_script_stats(tab_id))
            .flatten()
        {
            return stats;
        }
        let grid = self.grid(tab_id);
        if grid.row_count() == 0 && grid.column_count() == 0 {
            return self.render_result_empty_state(tab_id);
        }
        if !self.columns_presentation_active(tab_id) && self.mongo_documents_view_active(tab_id) {
            self.view_mut(tab_id).grid_focus(cx);
            return self.render_mongo_documents(tab_id, cx);
        }
        let grid_pane = if self.diagram_presentation_active(tab_id) {
            self.render_diagram(tab_id, cx)
        } else if self.columns_presentation_active(tab_id) {
            self.render_columns_view(tab_id, cx)
        } else if self.chart_presentation_active(tab_id) {
            self.render_chart_pane(tab_id, cx)
        } else if self.record_view_active(tab_id) {
            self.render_record_pane(tab_id, cx)
        } else {
            self.render_grid_pane(tab_id, window, cx)
        };
        let rail = self.render_cell_detail_rail(tab_id, cx);
        // Layout: grid | panel | rail. The rail is always the far-right edge column.
        let with_rail = |content: AnyElement, rail: AnyElement| {
            div()
                .flex()
                .flex_1()
                .min_h_0()
                .min_w_0()
                .child(div().flex_1().min_h_0().min_w_0().child(content))
                .child(rail)
                .into_any_element()
        };
        let closed_layout = |grid_pane: Div, rail: AnyElement| {
            div()
                .flex()
                .flex_1()
                .min_h_0()
                .min_w_0()
                .child(grid_pane.flex_1().min_w_0())
                .child(rail)
                .into_any_element()
        };

        let Some(state) = self.tab_cell_detail(tab_id).cloned() else {
            return closed_layout(grid_pane, rail);
        };
        if !state.open {
            return closed_layout(grid_pane, rail);
        }
        // Open without a target (nothing selected yet): tabs show empty states.
        let detail = state.detail.as_ref();
        if state.placement == CellDetailPlacement::Popup {
            // The grid keeps the whole area; the panel floats above it.
            let popup =
                self.render_cell_detail_popup(tab_id, &state, detail, window.viewport_size(), cx);
            return div()
                .flex()
                .flex_1()
                .min_h_0()
                .min_w_0()
                .child(grid_pane.flex_1().min_w_0())
                .child(rail)
                .child(popup)
                .into_any_element();
        }
        let detail_panel = self.render_cell_detail_panel(tab_id, &state, detail, cx);
        let layout = match state.placement {
            CellDetailPlacement::Right => {
                let root = cx.entity().downgrade();
                gpui_component::resizable::h_resizable(("cell-detail-layout", tab_id))
                    .on_resize(move |resize_state, _, app| {
                        let Some(size) = resize_state.read(app).sizes().get(1).copied() else {
                            return;
                        };
                        let _ = root.update(app, |this, cx| {
                            if let Some(runtime) = this.workspace.runtime_mut(tab_id) {
                                runtime.cell_detail.set_panel_size(f32::from(size));
                            }
                            this.notify_tab(tab_id, cx);
                        });
                    })
                    .child(
                        gpui_component::resizable::resizable_panel()
                            .size_range(px(320.0)..gpui::Pixels::MAX)
                            .child(grid_pane),
                    )
                    .child(
                        gpui_component::resizable::resizable_panel()
                            .size(px(state.panel_size()))
                            .size_range(px(240.0)..px(640.0))
                            .child(detail_panel),
                    )
            }
            CellDetailPlacement::Popup => unreachable!("popup returns before the docked layouts"),
            CellDetailPlacement::Bottom => {
                let root = cx.entity().downgrade();
                gpui_component::resizable::v_resizable(("cell-detail-layout", tab_id))
                    .on_resize(move |resize_state, _, app| {
                        let Some(size) = resize_state.read(app).sizes().get(1).copied() else {
                            return;
                        };
                        let _ = root.update(app, |this, cx| {
                            if let Some(runtime) = this.workspace.runtime_mut(tab_id) {
                                runtime.cell_detail.set_panel_size(f32::from(size));
                            }
                            this.notify_tab(tab_id, cx);
                        });
                    })
                    .child(
                        gpui_component::resizable::resizable_panel()
                            .size_range(px(180.0)..gpui::Pixels::MAX)
                            .child(grid_pane),
                    )
                    .child(
                        gpui_component::resizable::resizable_panel()
                            .size(px(state.panel_size()))
                            .size_range(px(140.0)..px(420.0))
                            .child(detail_panel),
                    )
            }
        };
        with_rail(layout.into_any_element(), rail)
    }
}

/// Grid body text color: SQL NULLs are drawn muted so they read as
/// "no value" and differ from a literal `'NULL'` string, which keeps the
/// normal text color.
pub(in crate::app) fn grid_cell_text_color(is_null: bool) -> gpui::Rgba {
    if is_null {
        theme::text_faint()
    } else {
        theme::text()
    }
}

impl AppRoot {
    /// Copies the selected cells (the grid shortcut and Edit > Copy).
    pub(in crate::app) fn copy_grid_selection(&mut self, tab_id: u64, cx: &mut Context<Self>) {
        if self.view(tab_id).grid_ui.selection.borrow().range.is_some() {
            self.copy_format_to_clipboard(tab_id, super::copy::CopyFormat::Smart, cx);
        }
    }
}

#[cfg(test)]
mod null_color_tests {
    #[test]
    fn null_cells_are_muted_but_text_null_is_not() {
        assert_eq!(
            super::grid_cell_text_color(true),
            crate::theme::text_faint()
        );
        assert_eq!(super::grid_cell_text_color(false), crate::theme::text());
        assert_ne!(
            super::grid_cell_text_color(true),
            super::grid_cell_text_color(false)
        );
    }
}
