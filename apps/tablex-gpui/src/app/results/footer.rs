use super::super::*;
use gpui_component::ElementExt as _;

/// Everything the selection statistics depend on; the cache is reused until
/// one of these changes, so nothing is recomputed per frame.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(in crate::app) struct SelectionStatsKey {
    tab_id: u64,
    start: (usize, usize),
    end: (usize, usize),
    rows: usize,
    columns: usize,
    generation: u64,
    page_id: Option<u64>,
    pending: usize,
    mongo_pending: usize,
    /// Display->source column map: hiding / showing columns changes which
    /// source column a display position reads.
    display_map: u64,
}

/// What the footer's action strip draws, and measures to decide what fits. `Strip` is the room the strip has.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(in crate::app) enum FooterPart {
    Limit,
    Record,
    History,
    Rows,
    Refresh,
    Pagination,
    Commit,
    Export,
    More,
    Strip,
    /// The status row under the strip, and what it draws: the fetched count, the selection's numbers and the
    /// selection's "Selected:" label (which goes first when the row is short).
    Status,
    Fetched,
    Selection,
    Prefix,
}

impl FooterPart {
    /// What moves into the "..." menu when the strip is short, first to go first. Commit (Apply / Discard,
    /// Commit / Roll Back) and Export stay in the strip, and so does the menu's own button.
    const PEEL: [Self; 6] = [Self::Limit, Self::Record, Self::History, Self::Rows, Self::Refresh, Self::Pagination];
    /// The groups left of the spacer, in order.
    const LEFT: [Self; 5] = [Self::Refresh, Self::Commit, Self::Rows, Self::History, Self::Pagination];
    /// The groups of the right end, in order.
    const OUTPUT: [Self; 3] = [Self::Record, Self::Export, Self::Limit];
}

/// The painted widths of the strip and of each part of it. A part's width is known once it was drawn (a part
/// in the menu keeps the one it had), so the strip follows the language and the texts it is given instead of
/// a width written in the code.
#[derive(Default)]
pub(in crate::app) struct FooterFit {
    /// What the widths were measured for: another one measures again, drawing everything.
    key: u64,
    parts: [f32; 14],
}

pub(in crate::app) type SharedFooterFit = std::rc::Rc<std::cell::RefCell<FooterFit>>;

impl FooterFit {
    /// The status row's padding on each side, and the gaps of its children and of the selection's group.
    const STATUS_PAD: f32 = 12.0;
    const STATUS_GAP: f32 = 8.0;
    const PREFIX_GAP: f32 = 4.0;
    /// The gap between the strip's children, and inside the output group.
    const GAP: f32 = 6.0;
    const OUTPUT_GAP: f32 = 4.0;
    /// A separator is 1px wide (`ui::separator`).
    const SEPARATOR: f32 = 1.0;
    /// The strip's padding on each side: the controls do not touch the panel's edge.
    const PAD: f32 = 4.0;
    /// What the "..." button is assumed to need until it was drawn once: an icon button.
    const MORE: f32 = 28.0;

    fn width(&self, part: FooterPart) -> f32 {
        self.parts[part as usize]
    }

    /// Records a measure; whether it changed what is drawn.
    fn measured(fits: &SharedFooterFit, part: FooterPart, value: f32) -> bool {
        let mut fit = fits.borrow_mut();
        // The menu's button is assumed to be `MORE` wide until it was drawn: that is not a change.
        let known = if part == FooterPart::More && fit.parts[part as usize] <= 0.0 { Self::MORE } else { fit.parts[part as usize] };
        let changed = (known - value).abs() > 0.5;
        fit.parts[part as usize] = value;
        changed
    }

    /// What does not fit the strip, in the order it goes to the menu. Nothing until every part of the strip
    /// was drawn once: the first frame draws all of it, and measures.
    fn peeled(&self, present: impl Fn(FooterPart) -> bool, trailing_separator: bool) -> Vec<FooterPart> {
        let room = self.width(FooterPart::Strip) - 2.0 * Self::PAD;
        let known = room > 0.0
            && FooterPart::LEFT.iter().chain(&FooterPart::OUTPUT).all(|part| !present(*part) || self.width(*part) > 0.0);
        if room <= 0.0 || !known {
            return Vec::new();
        }
        let mut peeled = Vec::new();
        let mut next = FooterPart::PEEL.iter().filter(|part| present(**part));
        while self.needs(&present, &peeled, trailing_separator) > room {
            let Some(part) = next.next() else { break };
            peeled.push(*part);
        }
        peeled
    }

    /// Whether the selection has room for its "Selected:" label: what the fetched count leaves of the row
    /// holds the label and the numbers. Until all of it was drawn once the label is drawn, and measured.
    fn selection_label_fits(&self) -> bool {
        let parts = [FooterPart::Status, FooterPart::Fetched, FooterPart::Selection, FooterPart::Prefix];
        if parts.iter().any(|part| self.width(*part) <= 0.0) {
            return true;
        }
        let room = self.width(FooterPart::Status) - 2.0 * Self::STATUS_PAD - self.width(FooterPart::Fetched) - Self::STATUS_GAP;
        self.width(FooterPart::Selection) + self.width(FooterPart::Prefix) + Self::PREFIX_GAP <= room
    }

    /// The width the strip needs with `peeled` in the menu.
    fn needs(&self, present: &impl Fn(FooterPart) -> bool, peeled: &[FooterPart], trailing_separator: bool) -> f32 {
        let shown = |part: &&FooterPart| present(**part) && !peeled.contains(part);
        let left: Vec<_> = FooterPart::LEFT.iter().filter(shown).collect();
        let output: Vec<_> = FooterPart::OUTPUT.iter().filter(shown).collect();
        let separators = left.len().saturating_sub(1) + usize::from(trailing_separator && !left.is_empty());
        let more = (!peeled.is_empty()).then(|| if self.width(FooterPart::More) > 0.0 { self.width(FooterPart::More) } else { Self::MORE });
        // The left groups and their separators, the spacer, the output group and the menu's button.
        let children = left.len() + separators + 1 + usize::from(!output.is_empty()) + usize::from(more.is_some());
        let output_width = output.iter().map(|part| self.width(**part)).sum::<f32>() + Self::OUTPUT_GAP * output.len().saturating_sub(1) as f32;
        left.iter().map(|part| self.width(**part)).sum::<f32>()
            + separators as f32 * Self::SEPARATOR
            + output_width
            + more.unwrap_or(0.0)
            + Self::GAP * children.saturating_sub(1) as f32
    }
}

/// What the "..." menu of a short footer holds: the controls that no longer fit, with their state when the
/// strip was drawn.
struct FooterMenu {
    tab_id: u64,
    root: gpui::WeakEntity<AppRoot>,
    peeled: Vec<FooterPart>,
    is_mongo: bool,
    mongo_page: bool,
    is_redis_key: bool,
    shell_result: bool,
    auto_refresh: Option<u64>,
    limit: i64,
    editing_document: bool,
    commit_running: bool,
    has_selection: bool,
    can_delete: bool,
    sql_busy: bool,
    can_insert: bool,
    can_previous: bool,
    can_next: bool,
    can_undo: bool,
    can_redo: bool,
    record_on: bool,
    record_ok: bool,
}

/// A menu entry the tests can find by `selector`.
fn footer_entry(selector: &'static str, label: impl Into<String>) -> PopupMenuItem {
    let label: String = label.into();
    PopupMenuItem::element(move |_, _| div().id(selector).debug_selector(move || selector.to_string()).child(label.clone()))
}

impl FooterMenu {
    fn build(&self, menu: gpui_component::menu::PopupMenu, window: &mut Window, cx: &mut Context<gpui_component::menu::PopupMenu>) -> gpui_component::menu::PopupMenu {
        let (tab_id, root) = (self.tab_id, &self.root);
        let mut menu = menu;
        // In the order of the strip: Refresh, rows, history, paging, then the output group's.
        let order = [FooterPart::Refresh, FooterPart::Rows, FooterPart::History, FooterPart::Pagination, FooterPart::Record, FooterPart::Limit];
        let mut first = true;
        for part in order.into_iter().filter(|part| self.peeled.contains(part)) {
            if !std::mem::take(&mut first) {
                menu = menu.separator();
            }
            menu = match part {
                FooterPart::Refresh => {
                    let refresh = root.clone();
                    let menu = menu.item(
                        footer_entry("more-refresh", t!("grid.refresh").into_owned())
                            .disabled(self.editing_document || self.shell_result)
                            .on_click(move |_, window, app| {
                                let _ = refresh.update(app, |this, cx| this.refresh_tab_results(tab_id, window, cx));
                            }),
                    );
                    // A shell result is not run again by Refresh, and has no auto-refresh either.
                    if self.shell_result {
                        menu
                    } else {
                        let (root, is_mongo, auto_refresh) = (root.clone(), self.is_mongo, self.auto_refresh);
                        let label = auto_refresh.map_or_else(|| t!("grid.auto_refresh_off").into_owned(), |seconds| t!("grid.auto_refresh_every", seconds = seconds).into_owned());
                        menu.submenu(label, window, cx, move |submenu, _, _| auto_refresh_menu(submenu, &root, tab_id, is_mongo, auto_refresh))
                    }
                }
                FooterPart::Rows => self.rows(menu),
                FooterPart::History => {
                    let (undo, redo) = (root.clone(), root.clone());
                    menu.item(footer_entry("more-undo", t!("grid.undo").into_owned()).disabled(!self.can_undo || self.commit_running).on_click(
                        move |_, _, app| {
                            let _ = undo.update(app, |this, cx| this.undo_mongo_mutation(tab_id, cx));
                        },
                    ))
                    .item(footer_entry("more-redo", t!("grid.redo").into_owned()).disabled(!self.can_redo || self.commit_running).on_click(
                        move |_, _, app| {
                            let _ = redo.update(app, |this, cx| this.redo_mongo_mutation(tab_id, cx));
                        },
                    ))
                }
                FooterPart::Pagination => {
                    let (previous, next) = (root.clone(), root.clone());
                    menu.item(footer_entry("more-previous", t!("grid.previous").into_owned()).disabled(!self.can_previous).on_click(
                        move |_, _, app| {
                            let _ = previous.update(app, |this, cx| this.page_tab_query(tab_id, false, cx));
                        },
                    ))
                    .item(footer_entry("more-next", t!("grid.next").into_owned()).disabled(!self.can_next).on_click(move |_, _, app| {
                        let _ = next.update(app, |this, cx| this.page_tab_query(tab_id, true, cx));
                    }))
                }
                FooterPart::Record => {
                    let record = root.clone();
                    menu.item(footer_entry("more-record", t!("grid.record_view_tab").into_owned()).disabled(!self.record_ok).checked(self.record_on).on_click(
                        move |_, _, app| {
                            let _ = record.update(app, |this, cx| this.toggle_record_view(tab_id, cx));
                        },
                    ))
                }
                FooterPart::Limit => [50_i64, 100, 200, 500, 1000].into_iter().fold(menu, |menu, value| {
                    let root = root.clone();
                    let selector: &'static str = match value {
                        50 => "more-limit-50",
                        100 => "more-limit-100",
                        200 => "more-limit-200",
                        500 => "more-limit-500",
                        _ => "more-limit-1000",
                    };
                    menu.item(
                        footer_entry(selector, t!("grid.limit", count = value).into_owned())
                            .disabled(self.editing_document || (self.mongo_page && value > super::mongo::query::MONGO_PAGE_MAX))
                            .checked(self.limit == value)
                            .on_click(move |_, _, app| {
                                let _ = root.update(app, |this, cx| this.set_output_limit(tab_id, value, cx));
                            }),
                    )
                }),
                _ => menu,
            };
        }
        menu
    }

    /// Add, edit and delete of the rows (a document, a Redis element).
    fn rows(&self, menu: gpui_component::menu::PopupMenu) -> gpui_component::menu::PopupMenu {
        let (tab_id, root) = (self.tab_id, &self.root);
        let (edit, delete) = (root.clone(), root.clone());
        let busy = self.commit_running || self.sql_busy;
        if self.is_redis_key {
            let add = root.clone();
            return menu
                .item(footer_entry("more-add", t!("grid.add").into_owned()).on_click(move |_, window, app| {
                    let _ = add.update(app, |this, cx| this.open_redis_add_dialog(tab_id, window, cx));
                }))
                .item(footer_entry("more-edit", t!("grid.edit").into_owned()).disabled(!self.has_selection).on_click(move |_, window, app| {
                    let _ = edit.update(app, |this, cx| this.open_redis_element_editor(tab_id, window, cx));
                }))
                .item(footer_entry("more-delete", t!("grid.remove").into_owned()).disabled(!self.has_selection).on_click(move |_, window, app| {
                    let _ = delete.update(app, |this, cx| this.confirm_delete_redis_element(tab_id, window, cx));
                }));
        }
        let edit_item = footer_entry("more-edit", t!("grid.edit").into_owned()).disabled(!self.has_selection || busy).on_click(move |_, window, app| {
            let _ = edit.update(app, |this, cx| this.open_cell_editor(tab_id, window, cx));
        });
        let delete_item = footer_entry("more-delete", t!("grid.delete").into_owned())
            .disabled(if self.is_mongo { !self.can_delete || self.commit_running } else { !self.has_selection || busy })
            .on_click(move |_, window, app| {
                let _ = delete.update(app, |this, cx| this.confirm_delete_selected_row(tab_id, window, cx));
            });
        if self.is_mongo {
            let (new, duplicate) = (root.clone(), root.clone());
            menu.item(footer_entry("more-new-document", t!("grid.new_document").into_owned()).disabled(self.commit_running).on_click(
                move |_, window, app| {
                    let _ = new.update(app, |this, cx| this.open_mongo_insert_row_editor_from_menu(tab_id, window, cx));
                },
            ))
            .item(
                footer_entry("more-duplicate", t!("grid.duplicate_selected").into_owned())
                    .disabled(!self.has_selection || self.commit_running)
                    .on_click(move |_, window, app| {
                        let _ = duplicate.update(app, |this, cx| this.open_mongo_duplicate_row_editor(tab_id, window, cx));
                    }),
            )
            .item(edit_item)
            .item(delete_item)
        } else {
            let insert = root.clone();
            menu.item(footer_entry("more-insert", t!("grid.insert_row").into_owned()).disabled(!self.can_insert || self.sql_busy).on_click(
                move |_, window, app| {
                    let _ = insert.update(app, |this, cx| this.open_insert_row_editor(tab_id, window, cx));
                },
            ))
            .item(edit_item)
            .item(delete_item)
        }
    }
}

/// FNV-1a over the display->source map (a few dozen `usize`s at most).
fn display_map_signature(map: &[usize]) -> u64 {
    map.iter().fold(0xcbf2_9ce4_8422_2325, |hash, column| {
        (hash ^ *column as u64).wrapping_mul(0x0100_0000_01b3)
    })
}

/// The auto-refresh interval menu, shared by the Refresh split button and the
/// status-row control so both offer the same choices.
fn auto_refresh_menu(
    menu: gpui_component::menu::PopupMenu,
    root: &gpui::WeakEntity<AppRoot>,
    tab_id: u64,
    is_mongo: bool,
    auto_refresh: Option<u64>,
) -> gpui_component::menu::PopupMenu {
    let root_customize = root.clone();
    let root_stop = root.clone();
    let mut menu = menu
        .item(
            PopupMenuItem::new(t!("grid.customize"))
                .disabled(!is_mongo)
                .on_click(move |_, window, app| {
                    let _ = root_customize.update(app, |this, cx| {
                        this.open_auto_refresh_custom_dialog(tab_id, window, cx);
                    });
                }),
        )
        .item(
            PopupMenuItem::new(t!("grid.stop"))
                .disabled(auto_refresh.is_none())
                .on_click(move |_, _, app| {
                    let _ = root_stop.update(app, |this, cx| {
                        this.set_auto_refresh_interval(tab_id, None, cx);
                    });
                }),
        )
        .separator();
    for seconds in crate::auto_refresh::PRESET_INTERVALS {
        let root = root.clone();
        menu = menu.item(
            PopupMenuItem::new(if *seconds == 1 {
                t!("grid.refresh_every_one")
            } else {
                t!("grid.refresh_every_other", seconds = seconds)
            })
            .disabled(!is_mongo)
            .checked(auto_refresh == Some(*seconds))
            .on_click(move |_, _, app| {
                let _ = root.update(app, |this, cx| {
                    this.set_auto_refresh_interval(tab_id, Some(*seconds), cx);
                });
            }),
        );
    }
    menu.label(if is_mongo {
        t!("grid.frequent_refresh_increases_server_load")
    } else {
        t!("grid.automatic_refresh_requires_a_read_only_driver_qu")
    })
}

/// Compact status-row text of the auto-refresh state.
fn auto_refresh_label(seconds: Option<u64>) -> String {
    seconds.map_or_else(|| t!("grid.auto_refresh_off_short").into_owned(), |seconds| format!("{seconds}s"))
}

/// A page button whose page is loading: its icon spins. Kept out of the footer
/// render so the icon is only built while loading.
fn spinning_page_button(button: Button) -> Button {
    button
        .loading(true)
        .loading_icon(Icon::empty().path("icons/loader-circle.svg").size(px(14.0)))
}

impl AppRoot {
    /// A paint callback that records the width of `part` of the footer of `tab_id` and draws the results
    /// again when that changes what fits. A notification made while painting does not ask for a frame (the
    /// window is not dirty afterwards: a real window waits for the next input), so it is made from a
    /// next-frame callback, which does.
    fn footer_probe(&self, tab_id: u64, part: FooterPart, cx: &mut Context<Self>) -> impl FnOnce(gpui::Bounds<gpui::Pixels>, &mut Window, &mut gpui::App) + 'static {
        let (fit, root) = (self.view(tab_id).footer_fit.clone(), cx.entity().downgrade());
        move |bounds, window, _| {
            if FooterFit::measured(&fit, part, f32::from(bounds.size.width)) {
                window.on_next_frame(move |_, app| {
                    let _ = root.update(app, |this, cx| {
                        this.notify_tab(tab_id, cx);
                        this.notify_pane_views(cx);
                    });
                });
            }
        }
    }

    pub(in crate::app) fn selection_stats(&mut self, tab_id: u64) -> Option<crate::selection_stats::SelectionStats> {
        let view = self.view(tab_id);
        let range = view.grid_ui.selection.borrow().range?.normalized();
        let runtime = self.workspace.runtime(tab_id)?;
        let grid = &runtime.grid;
        let key = SelectionStatsKey {
            tab_id,
            start: (range.start.row, range.start.column),
            end: (range.end.row, range.end.column),
            rows: grid.row_count(),
            columns: grid.column_count(),
            generation: runtime.query.generation(),
            page_id: runtime.driver_page_id,
            pending: runtime.pending_edits.len(),
            mongo_pending: runtime.mongo.mutations.len(),
            display_map: display_map_signature(&view.grid_ui.visible_columns),
        };
        if let Some((cached_key, stats)) = view.grid_ui.selection_stats {
            if cached_key == key {
                return Some(stats);
            }
        }
        // How each selected column counts, from its RESOLVED type (a PostgreSQL
        // `numeric` or MySQL `DECIMAL` arrives as text and is exact; a float
        // type makes the result approximate); text-like columns never count,
        // whatever their content.
        let column_kinds: Vec<crate::selection_stats::StatColumn> = (range.start.column
            ..=range.end.column)
            .map(|display| {
                use crate::selection_stats::StatColumn;
                let Some(source) = view.source_column_for_display(display, grid.column_count())
                else {
                    return StatColumn::Skip;
                };
                let Some(type_name) = self.resolved_column_type(tab_id, source) else {
                    return StatColumn::Exact;
                };
                match super::column_type::value_kind(&type_name) {
                    super::column_type::ValueKind::Integer
                    | super::column_type::ValueKind::Decimal => StatColumn::Exact,
                    super::column_type::ValueKind::Float => StatColumn::Float,
                    _ => {
                        let lower = type_name.to_ascii_lowercase();
                        if ["char", "text", "string", "object_id", "objectid", "uuid", "json", "binary", "bytea", "blob"]
                            .iter()
                            .any(|kind| lower.contains(kind))
                        {
                            StatColumn::Skip
                        } else {
                            StatColumn::Exact
                        }
                    }
                }
            })
            .collect();
        let stats = crate::selection_stats::SelectionStats::compute(
            range.end.row - range.start.row + 1,
            range.end.column - range.start.column + 1,
            &column_kinds,
            |row, column| {
                let display = CellPosition {
                    row: range.start.row + row,
                    column: range.start.column + column,
                };
                let source = view.source_column_for_display(display.column, grid.column_count())?;
                let position = CellPosition {
                    row: display.row,
                    column: source,
                };
                match self.pending_cell_text(tab_id, display) {
                    Some(text) => Some(text.to_string()),
                    None => (!grid.cell_is_null(position))
                        .then(|| grid.cell_text(position).map(|text| text.to_string()))
                        .flatten(),
                }
            },
        );
        self.view_mut(tab_id).grid_ui.selection_stats = Some((key, stats));
        Some(stats)
    }

    pub(in crate::app) fn render_result_footer(
        &mut self,
        tab_id: u64,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let stats = self.selection_stats(tab_id);
        let Some(tab) = self.workspace.tab(tab_id) else {
            return div().into_any_element();
        };
        let is_mongo = tab.query_mode.is_mongo_find();
        let mongo_page = tab.query_mode.is_mongo();
        let is_redis_key = tab.query_mode.is_redis_key();
        let is_redis = tab.query_mode.is_redis();
        let (pagination, pending, can_undo, can_redo, commit_status, auto_refresh, last_query_ms) =
            self.workspace
                .runtime(tab_id)
                .map(|runtime| {
                    (
                        runtime.pagination.clone(),
                        runtime.mongo.mutations.len(),
                        runtime.mongo.mutations.can_undo(),
                        runtime.mongo.mutations.can_redo(),
                        runtime.mongo.commit_status.clone(),
                        runtime.auto_refresh_seconds,
                        runtime.last_query_ms,
                    )
                })
                .unwrap_or_default();
        let commit_running = matches!(
            &commit_status,
            crate::mongo_result::MongoCommitStatus::Running
        );
        // An open document editor holds the staged changes and the result still: the buttons that
        // would commit, roll back, undo, change rows or re-run wait for its Save or Cancel.
        let editing_document = self.view(tab_id).mongo.document_edit.is_some();
        let commit_running = commit_running || editing_document;
        // While the tab's query runs the page buttons wait for it; the one that
        // started it spins, so a click visibly took.
        let query_running = self.query_tasks.tasks.contains_key(&tab_id);
        let paging = self.query_tasks.paging_direction(tab_id);
        let previous = Button::new(("page-previous", tab_id))
            .small()
            .icon(
                Icon::new(IconName::ChevronRight)
                    .rotate(gpui::percentage(0.5))
                    .size(px(14.0)),
            )
            .compact()
            .ghost()
            .tooltip(t!("grid.previous"))
            .debug_selector(|| "page-previous".into())
            .when(paging == Some(false), spinning_page_button)
            .disabled(!pagination.has_previous() || editing_document || query_running)
            .on_click(cx.listener(move |this, _, _, cx| {
                this.page_tab_query(tab_id, false, cx);
            }));
        let next = Button::new(("page-next", tab_id))
            .small()
            .icon(Icon::new(IconName::ChevronRight).size(px(14.0)))
            .compact()
            .ghost()
            .tooltip(t!("grid.next"))
            .debug_selector(|| "page-next".into())
            .when(paging == Some(true), spinning_page_button)
            .disabled(!pagination.has_next() || editing_document || query_running)
            .on_click(cx.listener(move |this, _, _, cx| {
                this.page_tab_query(tab_id, true, cx);
            }));

        let summary = fetched_text(self.grid(tab_id).row_count(), pagination.has_next(), last_query_ms);
        let root = cx.entity().downgrade();
        let can_sql_export = self.sql_sort_available(tab_id);
        let refresh_button = DropdownButton::new(("result-refresh", tab_id))
            .small()
            .button(
                Button::new(("result-refresh-action", tab_id))
                    .small()
                    .debug_selector(|| "result-refresh".into())
                    .label(t!("grid.refresh"))
                    .compact()
                    .ghost()
                    .disabled(editing_document)
                    .on_click(
                        cx.listener(move |this, _, window, cx| {
                            this.refresh_tab_results(tab_id, window, cx)
                        }),
                    ),
            )
            .ghost()
            .dropdown_menu({
                let root = root.clone();
                move |menu, _, _| auto_refresh_menu(menu, &root, tab_id, is_mongo, auto_refresh)
            });
        // A shell result is read-only and its script may write: Refresh (and its auto-refresh menu)
        // would run it again, so only Run does.
        let shell_result = self.shell_view(tab_id).is_some();
        let refresh = if shell_result {
            div()
                .debug_selector(|| "result-group-refresh".into())
                .flex()
                .items_center()
                .flex_shrink_0()
                .child(
                    Button::new(("result-refresh-action", tab_id))
                        .small()
                        .debug_selector(|| "result-refresh".into())
                        .label(t!("grid.refresh"))
                        .compact()
                        .ghost()
                        .disabled(true)
                        .tooltip(t!("grid.run_the_script_again_with_run")),
                )
        } else {
            div()
                .debug_selector(|| "result-group-refresh".into())
                .flex()
                .items_center()
                .flex_shrink_0()
                .child(
                    div()
                        .debug_selector(|| "result-refresh-dropdown".into())
                        .child(refresh_button),
                )
        };
        let export = Button::new(("result-export", tab_id))
            .small()
            .debug_selector(|| "result-export".into())
            .label(t!("grid.export"))
            .dropdown_caret(true)
            .compact()
            .ghost()
            .disabled(self.grid(tab_id).row_count() == 0)
            .dropdown_menu({
                let root = cx.entity().downgrade();
                let can_transfer = self.transfer_available(tab_id);
                move |menu, _, _| {
                    let transfer_root = root.clone();
                    super::export::ContextExportFormat::ALL
                        .iter()
                        .filter(|(format, _)| {
                            can_sql_export || *format != super::export::ContextExportFormat::Sql
                        })
                        .fold(menu, |menu, (format, label)| {
                            let page_root = root.clone();
                            let selected_root = root.clone();
                            let format = *format;
                            menu.item(
                                PopupMenuItem::new(format!("Current Page {label}")).on_click(
                                    move |_, window, app| {
                                        let _ = page_root.update(app, |this, cx| {
                                            this.export_context_selection(tab_id, format, true, window, cx)
                                        });
                                    },
                                ),
                            )
                            .item(
                                PopupMenuItem::new(format!("Selected Cells {label}")).on_click(
                                    move |_, window, app| {
                                        let _ = selected_root.update(app, |this, cx| {
                                            this.export_context_selection(tab_id, format, false, window, cx)
                                        });
                                    },
                                ),
                            )
                        })
                        .when(can_transfer, |menu| {
                            menu.separator().item(
                                PopupMenuItem::new(t!("grid.whole_result_to_files_or_tables"))
                                    .icon(Icon::empty().path("icons/arrow-left-right.svg").size(px(12.0)))
                                    .on_click(move |_, window, app| {
                                        let _ = transfer_root.update(app, |this, cx| this.open_transfer_from_tab(tab_id, window, cx));
                                    }),
                            )
                        })
                }
            });
        let limit = tab.output_limit;
        let limit_button = Button::new(("result-limit", tab_id))
            .small()
            .debug_selector(|| "result-limit".into())
            .label(t!("grid.limit", count = limit))
            .dropdown_caret(true)
            .compact()
            .ghost()
            .disabled(editing_document)
            .dropdown_menu({
                let root = cx.entity().downgrade();
                move |menu, _, _| {
                    [50_i64, 100, 200, 500, 1000]
                        .into_iter()
                        .fold(menu, |menu, value| {
                            let root = root.clone();
                            menu.item(
                                PopupMenuItem::new(value.to_string())
                                    .disabled(
                                        mongo_page && value > super::mongo::query::MONGO_PAGE_MAX,
                                    )
                                    .checked(limit == value)
                                    .on_click(move |_, _, app| {
                                        let _ = root.update(app, |this, cx| {
                                            this.set_output_limit(tab_id, value, cx);
                                        });
                                    }),
                            )
                        })
                }
            });

        let has_selection = self.view(tab_id).grid_ui.selection.borrow().range.is_some();
        let can_delete = has_selection
            && self
                .workspace
                .runtime(tab_id)
                .and_then(|runtime| runtime.driver_page_id)
                .is_some();
        let pending_count = self
            .workspace
            .runtime(tab_id)
            .map(|runtime| runtime.pending_edits.len() + runtime.pending_sql_rows.len())
            .unwrap_or_default();
        let commit_group = if is_redis {
            // Redis edits are applied at once: nothing to commit.
            div().debug_selector(|| "result-group-commit".into()).into_any_element()
        } else if is_mongo {
            let commit = ui::primary_action_button("mongo-commit", tab_id, t!("grid.commit"))
                .small()
                .debug_selector(|| "mongo-commit".into())
                .disabled(pending == 0 || commit_running)
                .on_click(cx.listener(move |this, _, window, cx| this.request_commit_mongo(tab_id, window, cx)));
            let rollback = ui::action_button("mongo-rollback", tab_id, t!("grid.rollback"))
                .small()
                .debug_selector(|| "mongo-rollback".into())
                .disabled(pending == 0 || commit_running)
                .on_click(cx.listener(move |this, _, _, cx| this.rollback_mongo_mutations(tab_id, cx)));
            div()
                .debug_selector(|| "result-group-commit".into())
                .flex()
                .items_center()
                .gap(px(4.0))
                .flex_shrink_0()
                .child(commit)
                .child(rollback)
                .into_any_element()
        } else {
            let apply = ui::primary_action_button(
                "apply-edit",
                tab_id,
                if pending_count == 1 {
                    t!("grid.apply")
                } else {
                    t!("grid.apply_all")
                },
            )
            .debug_selector(|| "apply-edit".into())
            .disabled(
                pending_count == 0
                    || self
                        .workspace
                        .runtime(tab_id)
                        .is_some_and(|runtime| runtime.sql_apply_running),
            )
            .small()
            .on_click(
                cx.listener(move |this, _, window, cx| {
                    this.confirm_apply_sql_changes(tab_id, window, cx)
                }),
            );
            let applying = self
                .workspace
                .runtime(tab_id)
                .is_some_and(|runtime| runtime.sql_apply_running);
            // An Apply on a driver is a cancellable operation of the tab: its
            // Discard turns into Cancel for as long as it runs.
            let cancellable =
                applying && self.driver_runtime.query_operations.contains_key(&tab_id);
            let discard = ui::action_button(
                "discard-edit",
                tab_id,
                if cancellable { t!("common.cancel") } else { t!("grid.discard") },
            )
            .small()
            .debug_selector(|| "discard-edit".into())
            .disabled(pending_count == 0 || (applying && !cancellable))
            .on_click(cx.listener(move |this, _, _, cx| {
                if cancellable {
                    this.cancel_tab_query(tab_id, cx);
                    this.notify_tab(tab_id, cx);
                } else {
                    this.discard_pending_edit(tab_id, cx);
                }
            }));
            div()
                .debug_selector(|| "result-group-commit".into())
                .flex()
                .items_center()
                .gap(px(4.0))
                .flex_shrink_0()
                .child(apply)
                .child(discard)
                .into_any_element()
        };
        let row_group = if is_redis_key {
            let add = ui::icon_button(("redis-add", tab_id), IconName::Plus, t!("grid.add"))
                .small()
                .debug_selector(|| "redis-add".into())
                .on_click(cx.listener(move |this, _, window, cx| this.open_redis_add_dialog(tab_id, window, cx)));
            let edit = ui::icon_button(("redis-edit", tab_id), IconName::Frame, t!("grid.edit"))
                .small()
                .icon(Icon::empty().path("icons/pencil.svg").size(px(14.0)))
                .debug_selector(|| "redis-edit".into())
                .disabled(!has_selection)
                .on_click(cx.listener(move |this, _, window, cx| this.open_redis_element_editor(tab_id, window, cx)));
            let delete = ui::icon_button(("redis-remove", tab_id), IconName::Delete, t!("grid.remove"))
                .small()
                .debug_selector(|| "redis-remove".into())
                .disabled(!has_selection)
                .on_click(cx.listener(move |this, _, window, cx| {
                    this.confirm_delete_redis_element(tab_id, window, cx)
                }));
            div()
                .debug_selector(|| "result-group-row".into())
                .flex()
                .items_center()
                .gap(px(4.0))
                .flex_shrink_0()
                .child(add)
                .child(edit)
                .child(delete)
                .into_any_element()
        } else if is_redis {
            div().debug_selector(|| "result-group-row".into()).into_any_element()
        } else if is_mongo {
            let root_add = cx.entity().downgrade();
            let add = Button::new(("mongo-add-row", tab_id))
                .small()
                .debug_selector(|| "mongo-add-row".into())
                // Icon as child so the caret isn't clipped by icon-only square sizing.
                .child(Icon::new(IconName::Plus).size(px(14.0)))
                .dropdown_caret(true)
                .compact()
                .ghost()
                .tooltip(t!("grid.add_row"))
                .disabled(commit_running)
                .dropdown_menu(move |menu, _, _| {
                    let root_new = root_add.clone();
                    let root_duplicate = root_add.clone();
                    menu.item(
                        PopupMenuItem::new(t!("grid.new_document")).on_click(move |_, window, app| {
                            let _ = root_new.update(app, |this, cx| {
                                this.open_mongo_insert_row_editor_from_menu(tab_id, window, cx);
                            });
                        }),
                    )
                    .item(
                        PopupMenuItem::new(t!("grid.duplicate_selected"))
                            .disabled(!has_selection)
                            .on_click(move |_, window, app| {
                                let _ = root_duplicate.update(app, |this, cx| {
                                    this.open_mongo_duplicate_row_editor(tab_id, window, cx);
                                });
                            }),
                    )
                });
            let delete = ui::icon_button(("mongo-delete", tab_id), IconName::Delete, t!("grid.delete"))
                .small()
                .debug_selector(|| "mongo-delete".into())
                .disabled(!can_delete || commit_running)
                .on_click(cx.listener(move |this, _, window, cx| {
                    this.confirm_delete_selected_row(tab_id, window, cx);
                }));
            let edit = ui::icon_button(("edit-cell", tab_id), IconName::Frame, t!("grid.edit"))
                .small()
                .icon(Icon::empty().path("icons/pencil.svg").size(px(14.0)))
                .debug_selector(|| "edit-cell".into())
                .disabled(!has_selection || commit_running)
                .on_click(cx.listener(move |this, _, window, cx| {
                    this.open_cell_editor(tab_id, window, cx);
                }));
            div()
                .debug_selector(|| "result-group-row".into())
                .flex()
                .items_center()
                .gap(px(4.0))
                .flex_shrink_0()
                .child(add)
                .child(edit)
                .child(delete)
                .into_any_element()
        } else {
            let edit = ui::icon_button(("edit-cell", tab_id), IconName::Frame, t!("grid.edit"))
                .small()
                .icon(Icon::empty().path("icons/pencil.svg").size(px(14.0)))
                .debug_selector(|| "edit-cell".into())
                .disabled(
                    !has_selection
                        || self
                            .workspace
                            .runtime(tab_id)
                            .is_some_and(|runtime| runtime.sql_apply_running),
                )
                .on_click(cx.listener(move |this, _, window, cx| {
                    this.open_cell_editor(tab_id, window, cx);
                }));
            let delete = ui::icon_button(("delete-row", tab_id), IconName::Delete, t!("grid.delete"))
                .small()
                .debug_selector(|| "delete-row".into())
                .disabled(
                    !has_selection
                        || self
                            .workspace
                            .runtime(tab_id)
                            .is_some_and(|runtime| runtime.sql_apply_running),
                )
                .on_click(cx.listener(move |this, _, window, cx| {
                    this.confirm_delete_selected_row(tab_id, window, cx);
                }));
            // Insert lives in the footer (was the "Structure" block's Insert
            // Row button); it opens the same staged-insert dialog.
            let add = ui::icon_button(("sql-add-row", tab_id), IconName::Plus, t!("grid.insert_row"))
                .small()
                .debug_selector(|| "sql-add-row".into())
                .disabled(
                    tab.collection.is_empty()
                        || self
                            .workspace
                            .runtime(tab_id)
                            .is_some_and(|runtime| runtime.sql_apply_running),
                )
                .on_click(cx.listener(move |this, _, window, cx| {
                    this.open_insert_row_editor(tab_id, window, cx);
                }));
            div()
                .debug_selector(|| "result-group-row".into())
                .flex()
                .items_center()
                .gap(px(4.0))
                .flex_shrink_0()
                .child(add)
                .child(edit)
                .child(delete)
                .into_any_element()
        };
        let history_group = if is_mongo {
            let undo = ui::icon_button(("mongo-undo", tab_id), IconName::Undo2, t!("grid.undo"))
                .small()
                .debug_selector(|| "mongo-undo".into())
                .disabled(!can_undo || commit_running)
                .on_click(cx.listener(move |this, _, _, cx| this.undo_mongo_mutation(tab_id, cx)));
            let redo = ui::icon_button(("mongo-redo", tab_id), IconName::Redo2, t!("grid.redo"))
                .small()
                .debug_selector(|| "mongo-redo".into())
                .disabled(!can_redo || commit_running)
                .on_click(cx.listener(move |this, _, _, cx| this.redo_mongo_mutation(tab_id, cx)));
            div()
                .debug_selector(|| "result-group-history".into())
                .flex()
                .items_center()
                .gap(px(4.0))
                .flex_shrink_0()
                .child(undo)
                .child(redo)
                .into_any_element()
        } else {
            div()
                .debug_selector(|| "result-group-history".into())
                .flex_shrink_0()
                .into_any_element()
        };
        let pagination_group = div()
            .debug_selector(|| "result-group-pagination".into())
            .flex()
            .items_center()
            .gap(px(4.0))
            .flex_shrink_0()
            .child(previous)
            .child(next);
        // Record mode: the current row as a list of fields.
        let record_support = self.record_view_support(tab_id);
        let record = Button::new(("record-view-toggle", tab_id))
            .small()
            .icon(Icon::empty().path("icons/rows-3.svg").size(px(14.0)))
            .compact()
            .ghost()
            .selected(self.record_view_active(tab_id))
            .disabled(record_support.is_err())
            .tooltip(match record_support {
                Ok(()) => t!("grid.record_view_tab"),
                Err(reason) => reason.into(),
            })
            .debug_selector(|| "record-view-toggle".into())
            .on_click(cx.listener(move |this, _, _, cx| this.toggle_record_view(tab_id, cx)));
        // What fits the strip is measured from what was painted (see `FooterFit`); what does not is in
        // the "..." menu, the controls of the rows, paging, Refresh and Limit first.
        let (has_history, has_commit, has_rows) = (is_mongo && !shell_result, !shell_result && !is_redis, !shell_result && (is_redis_key || !is_redis));
        let present = |part: FooterPart| match part {
            FooterPart::Limit => !shell_result,
            FooterPart::History => has_history,
            FooterPart::Commit => has_commit,
            FooterPart::Rows => has_rows,
            FooterPart::Pagination => !shell_result,
            FooterPart::More | FooterPart::Strip | FooterPart::Status | FooterPart::Fetched | FooterPart::Selection | FooterPart::Prefix => false,
            FooterPart::Record | FooterPart::Refresh | FooterPart::Export => true,
        };
        let fit = self.view(tab_id).footer_fit.clone();
        let peeled = {
            let mut fit = fit.borrow_mut();
            let key = u64::from(crate::i18n::current() == crate::i18n::Locale::Vi)
                | (limit as u64) << 1
                | u64::from(is_mongo) << 20
                | u64::from(is_redis_key) << 21
                | u64::from(is_redis) << 22
                | u64::from(shell_result) << 23;
            if fit.key != key {
                let (strip, status) = (fit.width(FooterPart::Strip), fit.width(FooterPart::Status));
                *fit = FooterFit { key, ..FooterFit::default() };
                fit.parts[FooterPart::Strip as usize] = strip;
                fit.parts[FooterPart::Status as usize] = status;
            }
            fit.peeled(present, !shell_result)
        };
        let part = |part: FooterPart, element: AnyElement, cx: &mut Context<Self>| -> AnyElement {
            div()
                .flex()
                .flex_shrink_0()
                .child(element)
                .on_prepaint(self.footer_probe(tab_id, part, cx))
                .into_any_element()
        };
        let mut output = Vec::new();
        output.extend((!peeled.contains(&FooterPart::Record)).then(|| part(FooterPart::Record, record.into_any_element(), cx)));
        output.push(part(FooterPart::Export, export.into_any_element(), cx));
        // The limit of a shell result applies to the next Run, not to what is shown.
        output.extend((!shell_result && !peeled.contains(&FooterPart::Limit)).then(|| part(FooterPart::Limit, limit_button.into_any_element(), cx)));
        let output_group = div()
            .debug_selector(|| "result-group-output".into())
            .flex()
            .items_center()
            .gap(px(FooterFit::OUTPUT_GAP))
            .flex_shrink_0()
            .children(output);
        let mut left = Vec::new();
        for (kind, element) in [
            (FooterPart::Refresh, refresh.into_any_element()),
            (FooterPart::Commit, commit_group),
            (FooterPart::Rows, row_group),
            (FooterPart::History, history_group),
            (FooterPart::Pagination, pagination_group.into_any_element()),
        ] {
            if present(kind) && !peeled.contains(&kind) {
                left.push(part(kind, element, cx));
            }
        }
        // No staging, paging or separators around groups a shell result does not have.
        let separators = !shell_result && !left.is_empty();
        let mut content = div()
            .debug_selector(|| "result-action-content".into())
            .h_full()
            .w_full()
            .flex()
            .items_center()
            .gap(px(FooterFit::GAP))
            .flex_shrink_0();
        for (index, group) in left.into_iter().enumerate() {
            if index > 0 {
                content = content.child(ui::separator());
            }
            content = content.child(group);
        }
        let more = (!peeled.is_empty()).then(|| {
            let menu = FooterMenu {
                tab_id,
                root: cx.entity().downgrade(),
                peeled: peeled.clone(),
                is_mongo,
                mongo_page,
                is_redis_key,
                shell_result,
                auto_refresh,
                limit,
                editing_document,
                commit_running,
                has_selection,
                can_delete,
                sql_busy: self.workspace.runtime(tab_id).is_some_and(|runtime| runtime.sql_apply_running),
                can_insert: !tab.collection.is_empty(),
                can_previous: pagination.has_previous() && !editing_document && !query_running,
                can_next: pagination.has_next() && !editing_document && !query_running,
                can_undo,
                can_redo,
                record_on: self.record_view_active(tab_id),
                record_ok: self.record_view_support(tab_id).is_ok(),
            };
            let button = Button::new(("result-more", tab_id))
                .small()
                .debug_selector(|| "result-more".into())
                .icon(Icon::new(IconName::Ellipsis).size(px(12.0)))
                .compact()
                .ghost()
                .tooltip(t!("editor.more_actions"))
                .dropdown_menu(move |popup, window, cx| menu.build(popup, window, cx));
            part(FooterPart::More, button.into_any_element(), cx)
        });
        let content = content
            .when(separators, |this| this.child(ui::separator()))
            .child(div().flex_1().min_w(px(0.0)))
            .child(output_group)
            .children(more);
        let action_strip = div()
            .id("result-action-strip")
            .debug_selector(|| "result-action-strip".into())
            .flex()
            .h(px(theme::RESULT_ACTION_HEIGHT))
            .flex_shrink_0()
            .px(px(FooterFit::PAD))
            .overflow_hidden()
            .child(content)
            .on_prepaint(self.footer_probe(tab_id, FooterPart::Strip, cx));
        let status_error = match commit_status {
            crate::mongo_result::MongoCommitStatus::Running => Some(t!("grid.committing").into_owned()),
            crate::mongo_result::MongoCommitStatus::Partial { committed, error } => {
                Some(format!("{committed} committed: {error}"))
            }
            crate::mongo_result::MongoCommitStatus::Idle => None,
        }
        .or_else(|| {
            self.view(tab_id)
                .grid_ui
                .inline_edit
                .as_ref()
                .and_then(|edit| edit.error.clone())
        })
        .or_else(|| self.detail_message(tab_id));
        // Auto-refresh only exists for tabs with a read-only driver query
        // (Mongo find); elsewhere the control is not rendered at all.
        let auto_control = is_mongo.then(|| {
            let root = cx.entity().downgrade();
            div()
                .debug_selector(|| "result-status-auto".into())
                .flex_shrink_0()
                .child(
                    Button::new(("result-auto-refresh", tab_id))
                        .xsmall()
                        .ghost()
                        .compact()
                        .icon(Icon::empty().path("icons/refresh-cw.svg").size(px(12.0)))
                        .label(auto_refresh_label(auto_refresh))
                        .tooltip(match auto_refresh {
                            Some(seconds) => t!("grid.auto_refresh_every", seconds = seconds).into_owned(),
                            None => t!("grid.auto_refresh_off").into_owned(),
                        })
                        .dropdown_menu(move |menu, _, _| {
                            auto_refresh_menu(menu, &root, tab_id, true, auto_refresh)
                        }),
                )
        });
        // Only the numbers are in the monospace font, so they do not jitter; the words are in the UI font.
        let mono = gpui_component::theme::Theme::global(cx).mono_font_family.clone();
        let label_fits = self.view(tab_id).footer_fit.borrow().selection_label_fits();
        let selection = stats.map(|stats| {
            let counts = selection_text(stats.cells, stats.rows);
            let mut inner = div().flex().items_center().flex_shrink_0().gap(px(FooterFit::STATUS_GAP));
            if let Some(sums) = stats.sums_label() {
                inner = inner.child(div().font_family(mono.clone()).flex_shrink_0().whitespace_nowrap().child(sums));
            }
            let inner = inner
                .child(ui::numbers_mono(&counts, &mono))
                .on_prepaint(self.footer_probe(tab_id, FooterPart::Selection, cx));
            div()
                .debug_selector(|| "result-status-selection-stats".into())
                .flex()
                .items_center()
                .gap(px(FooterFit::PREFIX_GAP))
                .min_w(px(0.0))
                .overflow_hidden()
                .when(label_fits, |this| {
                    this.child(
                        div()
                            .debug_selector(|| "result-status-selected-label".into())
                            .flex_shrink_0()
                            .whitespace_nowrap()
                            .child(t!("grid.selected").into_owned())
                            .on_prepaint(self.footer_probe(tab_id, FooterPart::Prefix, cx)),
                    )
                })
                .child(inner)
        });
        // Part of the results box: its own background, one faint hairline above, the status line's type.
        let status_row = div()
            .debug_selector(|| "result-status-row".into())
            .h(px(theme::RESULT_STATUS_HEIGHT))
            .px(px(FooterFit::STATUS_PAD))
            .flex()
            .items_center()
            .gap(px(FooterFit::STATUS_GAP))
            .flex_shrink_0()
            .bg(theme::background())
            .border_t_1()
            .border_color(gpui::Hsla::from(theme::border()).opacity(0.6))
            .text_xs()
            .text_color(theme::text_muted())
            .on_prepaint(self.footer_probe(tab_id, FooterPart::Status, cx))
            .child(
                // The fetched count is never cut: the selection gives way first.
                div()
                    .debug_selector(|| "result-status-summary".into())
                    .flex_shrink_0()
                    .child(ui::numbers_mono(&summary, &mono))
                    .on_prepaint(self.footer_probe(tab_id, FooterPart::Fetched, cx)),
            )
            .child(div().flex_1().min_w(px(0.0)))
            .children(selection)
            .children(auto_control)
            .when_some(status_error, |this, error| {
                let tooltip_text = error.clone();
                this.child(
                    div()
                        .id("result-status-error")
                        .debug_selector(|| "result-status-error".into())
                        .tooltip(move |window, cx| {
                            gpui_component::tooltip::Tooltip::new(tooltip_text.clone())
                                .build(window, cx)
                        })
                        .min_w(px(0.0))
                        .max_w(px(320.0))
                        .overflow_hidden()
                        .whitespace_nowrap()
                        .text_ellipsis()
                        .text_color(theme::danger())
                        .child(error),
                )
            });
        div()
            .debug_selector(|| "result-action-footer".into())
            .h(px(theme::RESULT_FOOTER_HEIGHT))
            .flex()
            .flex_col()
            .flex_shrink_0()
            .overflow_hidden()
            .bg(theme::chrome())
            .child(action_strip)
            .child(status_row)
            .into_any_element()
    }
}

/// What the left of the status row says: the rows fetched (`200+` when the app knows more are there) and
/// how long the query took, when known.
fn fetched_text(rows: usize, more: bool, milliseconds: Option<u64>) -> String {
    let count = match (rows, more) {
        (_, true) => t!("grid.fetched_more", count = rows).into_owned(),
        (1, false) => t!("grid.fetched_one").into_owned(),
        (count, false) => t!("grid.fetched_other", count = count).into_owned(),
    };
    match milliseconds {
        Some(milliseconds) => format!("{count} \u{b7} {milliseconds} ms"),
        None => count,
    }
}

/// What the selection is, without the "Selected:" label: `16 cells \u{b7} 4 rows`.
fn selection_text(cells: usize, rows: usize) -> String {
    let cells = match cells {
        1 => t!("grid.cells_one").into_owned(),
        count => t!("grid.cells_other", count = count).into_owned(),
    };
    let rows = match rows {
        1 => t!("grid.rows_one").into_owned(),
        count => t!("grid.rows_other", count = count).into_owned(),
    };
    format!("{cells} \u{b7} {rows}")
}

#[cfg(test)]
mod fit_tests {
    use super::{FooterFit, FooterPart};

    fn measured(strip: f32) -> FooterFit {
        let mut fit = FooterFit::default();
        for (part, width) in [
            (FooterPart::Limit, 80.0),
            (FooterPart::Record, 24.0),
            (FooterPart::History, 56.0),
            (FooterPart::Rows, 84.0),
            (FooterPart::Refresh, 90.0),
            (FooterPart::Pagination, 56.0),
            (FooterPart::Commit, 150.0),
            (FooterPart::Export, 70.0),
            (FooterPart::More, 28.0),
            (FooterPart::Strip, strip),
        ] {
            fit.parts[part as usize] = width;
        }
        fit
    }

    #[test]
    fn the_selection_label_goes_when_the_row_has_no_room_for_it_after_the_fetched_count() {
        let mut fit = measured(0.0);
        assert!(fit.selection_label_fits(), "nothing measured yet: draw it, and measure");
        for (part, width) in [(FooterPart::Fetched, 120.0), (FooterPart::Selection, 100.0), (FooterPart::Prefix, 60.0)] {
            fit.parts[part as usize] = width;
        }
        // 24 padding + 120 fetched + 8 gap + 100 numbers + 60 label + 4 gap.
        fit.parts[FooterPart::Status as usize] = 316.0;
        assert!(fit.selection_label_fits());
        fit.parts[FooterPart::Status as usize] = 315.0;
        assert!(!fit.selection_label_fits(), "one pixel short: the label goes, the numbers stay");
    }

    #[test]
    fn what_does_not_fit_goes_to_the_menu_lowest_priority_first_and_the_primary_actions_stay() {
        let all = |_: FooterPart| true;
        assert!(measured(2000.0).peeled(all, true).is_empty(), "everything fits");
        assert!(FooterFit::default().peeled(all, true).is_empty(), "nothing is known before the first draw");
        let limit_first = measured(2000.0).needs(&all, &[], true) - 1.0;
        let mut fit = measured(limit_first);
        assert_eq!(fit.peeled(all, true), [FooterPart::Limit], "one short: Limit goes");
        fit.parts[FooterPart::Strip as usize] = 100.0;
        let peeled = fit.peeled(all, true);
        assert_eq!(peeled, FooterPart::PEEL, "too short for any of them: all go, in order");
        assert!(!peeled.contains(&FooterPart::Commit) && !peeled.contains(&FooterPart::Export));
        // A part the tab does not have is not counted.
        let no_history = |part: FooterPart| part != FooterPart::History;
        assert!(measured(2000.0).needs(&no_history, &[], true) < measured(2000.0).needs(&all, &[], true));
    }
}

#[cfg(test)]
mod status_text_tests {
    use super::{fetched_text, selection_text};
    use crate::i18n::{self, Locale};

    #[test]
    fn the_fetched_count_says_what_it_counts_and_the_time_what_it_is() {
        assert_eq!(fetched_text(19, false, Some(96)), "19 rows fetched \u{b7} 96 ms");
        assert_eq!(fetched_text(1, false, Some(0)), "1 row fetched \u{b7} 0 ms");
        assert_eq!(fetched_text(19, false, None), "19 rows fetched", "no time known, none made up");
        assert_eq!(fetched_text(200, true, Some(96)), "200+ rows fetched \u{b7} 96 ms", "more are there");
        assert_eq!(fetched_text(0, false, None), "0 rows fetched");
        i18n::set(Locale::Vi);
        assert_eq!(fetched_text(19, false, Some(96)), "\u{110}\u{e3} t\u{1ea3}i 19 d\u{f2}ng \u{b7} 96 ms");
        assert_eq!(fetched_text(200, true, Some(96)), "\u{110}\u{e3} t\u{1ea3}i 200+ d\u{f2}ng \u{b7} 96 ms");
        assert_eq!(fetched_text(1, false, None), "\u{110}\u{e3} t\u{1ea3}i 1 d\u{f2}ng");
        i18n::set(Locale::En);
    }

    #[test]
    fn the_selection_is_counted_in_cells_and_rows_under_a_label() {
        assert_eq!(t!("grid.selected"), "Selected:");
        assert_eq!(selection_text(16, 4), "16 cells \u{b7} 4 rows");
        assert_eq!(selection_text(1, 1), "1 cell \u{b7} 1 row", "one cell: the same label, singular");
        i18n::set(Locale::Vi);
        assert_eq!(t!("grid.selected"), "\u{110}ang ch\u{1ecd}n:");
        assert_eq!(selection_text(16, 4), "16 \u{f4} \u{b7} 4 d\u{f2}ng");
        assert_eq!(selection_text(1, 1), "1 \u{f4} \u{b7} 1 d\u{f2}ng");
        i18n::set(Locale::En);
    }
}
