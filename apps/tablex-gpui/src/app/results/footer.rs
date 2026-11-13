use super::super::*;

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
            PopupMenuItem::new("Customize...")
                .disabled(!is_mongo)
                .on_click(move |_, window, app| {
                    let _ = root_customize.update(app, |this, cx| {
                        this.open_auto_refresh_custom_dialog(tab_id, window, cx);
                    });
                }),
        )
        .item(
            PopupMenuItem::new("Stop")
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
            PopupMenuItem::new(format!(
                "Refresh every {seconds} {}",
                if *seconds == 1 { "second" } else { "seconds" }
            ))
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
        "Frequent refresh increases server load"
    } else {
        "Automatic refresh requires a read-only driver query; unavailable for SQL"
    })
}

/// Compact status-row text of the auto-refresh state.
fn auto_refresh_label(seconds: Option<u64>) -> String {
    seconds.map_or_else(|| "Off".to_string(), |seconds| format!("{seconds}s"))
}

/// A page button whose page is loading: its icon spins. Kept out of the footer
/// render so the icon is only built while loading.
fn spinning_page_button(button: Button) -> Button {
    button
        .loading(true)
        .loading_icon(Icon::empty().path("icons/loader-circle.svg").size(px(14.0)))
}

impl AppRoot {
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
        let stats_label = self.selection_stats(tab_id).map(|stats| stats.label());
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
            .tooltip("Previous")
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
            .tooltip("Next")
            .debug_selector(|| "page-next".into())
            .when(paging == Some(true), spinning_page_button)
            .disabled(!pagination.has_next() || editing_document || query_running)
            .on_click(cx.listener(move |this, _, _, cx| {
                this.page_tab_query(tab_id, true, cx);
            }));

        let summary = format!("{} rows", self.grid(tab_id).row_count());
        let query_time = last_query_ms
            .map(|milliseconds| format!("{milliseconds} ms"))
            .unwrap_or_else(|| "—".to_string());
        let root = cx.entity().downgrade();
        let can_sql_export = self.sql_sort_available(tab_id);
        let refresh_button = DropdownButton::new(("result-refresh", tab_id))
            .small()
            .button(
                Button::new(("result-refresh-action", tab_id))
                    .small()
                    .debug_selector(|| "result-refresh".into())
                    .label("Refresh")
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
                        .label("Refresh")
                        .compact()
                        .ghost()
                        .disabled(true)
                        .tooltip("Run the script again with Run"),
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
            .label("Export")
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
                                PopupMenuItem::new("Whole Result to Files or Tables\u{2026}")
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
            .label(format!("Limit {limit}"))
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
            let commit = ui::primary_action_button("mongo-commit", tab_id, "Commit")
                .small()
                .debug_selector(|| "mongo-commit".into())
                .disabled(pending == 0 || commit_running)
                .on_click(cx.listener(move |this, _, window, cx| this.request_commit_mongo(tab_id, window, cx)));
            let rollback = ui::action_button("mongo-rollback", tab_id, "Rollback")
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
                    "Apply"
                } else {
                    "Apply All"
                },
            )
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
                if cancellable { "Cancel" } else { "Discard" },
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
            let add = ui::icon_button(("redis-add", tab_id), IconName::Plus, "Add")
                .small()
                .debug_selector(|| "redis-add".into())
                .on_click(cx.listener(move |this, _, window, cx| this.open_redis_add_dialog(tab_id, window, cx)));
            let edit = ui::icon_button(("redis-edit", tab_id), IconName::Frame, "Edit")
                .small()
                .icon(Icon::empty().path("icons/pencil.svg").size(px(14.0)))
                .debug_selector(|| "redis-edit".into())
                .disabled(!has_selection)
                .on_click(cx.listener(move |this, _, window, cx| this.open_redis_element_editor(tab_id, window, cx)));
            let delete = ui::icon_button(("redis-remove", tab_id), IconName::Delete, "Remove")
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
                .tooltip("Add Row")
                .disabled(commit_running)
                .dropdown_menu(move |menu, _, _| {
                    let root_new = root_add.clone();
                    let root_duplicate = root_add.clone();
                    menu.item(
                        PopupMenuItem::new("New Document").on_click(move |_, window, app| {
                            let _ = root_new.update(app, |this, cx| {
                                this.open_mongo_insert_row_editor_from_menu(tab_id, window, cx);
                            });
                        }),
                    )
                    .item(
                        PopupMenuItem::new("Duplicate Selected")
                            .disabled(!has_selection)
                            .on_click(move |_, window, app| {
                                let _ = root_duplicate.update(app, |this, cx| {
                                    this.open_mongo_duplicate_row_editor(tab_id, window, cx);
                                });
                            }),
                    )
                });
            let delete = ui::icon_button(("mongo-delete", tab_id), IconName::Delete, "Delete")
                .small()
                .debug_selector(|| "mongo-delete".into())
                .disabled(!can_delete || commit_running)
                .on_click(cx.listener(move |this, _, window, cx| {
                    this.confirm_delete_selected_row(tab_id, window, cx);
                }));
            let edit = ui::icon_button(("edit-cell", tab_id), IconName::Frame, "Edit")
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
            let edit = ui::icon_button(("edit-cell", tab_id), IconName::Frame, "Edit")
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
            let delete = ui::icon_button(("delete-row", tab_id), IconName::Delete, "Delete")
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
            let add = ui::icon_button(("sql-add-row", tab_id), IconName::Plus, "Insert Row")
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
            let undo = ui::icon_button(("mongo-undo", tab_id), IconName::Undo2, "Undo")
                .small()
                .debug_selector(|| "mongo-undo".into())
                .disabled(!can_undo || commit_running)
                .on_click(cx.listener(move |this, _, _, cx| this.undo_mongo_mutation(tab_id, cx)));
            let redo = ui::icon_button(("mongo-redo", tab_id), IconName::Redo2, "Redo")
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
                Ok(()) => "Record view (Tab)",
                Err(reason) => reason,
            })
            .debug_selector(|| "record-view-toggle".into())
            .on_click(cx.listener(move |this, _, _, cx| this.toggle_record_view(tab_id, cx)));
        let output_group = div()
            .debug_selector(|| "result-group-output".into())
            .flex()
            .items_center()
            .gap(px(4.0))
            .flex_shrink_0()
            .child(record)
            .child(export)
            // The limit of a shell result applies to the next Run, not to what is shown.
            .when(!shell_result, |this| this.child(limit_button));
        let action_content = div()
            .debug_selector(|| "result-action-content".into())
            .h_full()
            .w_full()
            .min_w(px(756.0))
            .flex()
            .items_center()
            .gap(px(6.0))
            .flex_shrink_0()
            .child(refresh)
            // No staging, paging or separators around groups a shell result does not have.
            .when(!shell_result, |this| {
                this.child(ui::separator())
                    .child(commit_group)
                    .child(ui::separator())
                    .child(row_group)
                    .when(is_mongo, |this| {
                        this.child(ui::separator()).child(history_group)
                    })
                    .child(ui::separator())
                    .child(pagination_group)
                    .child(ui::separator())
            })
            .child(div().flex_1().min_w(px(0.0)))
            .child(output_group);
        let action_strip = div()
            .id("result-action-strip")
            .debug_selector(|| "result-action-strip".into())
            .flex()
            .h(px(theme::RESULT_ACTION_HEIGHT))
            .flex_shrink_0()
            .overflow_x_scroll()
            .overflow_y_scroll()
            .track_scroll(&self.view(tab_id).result_action_scroll)
            .child(action_content);
        let status_error = match commit_status {
            crate::mongo_result::MongoCommitStatus::Running => Some("Committing...".to_string()),
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
                            Some(seconds) => format!("Auto refresh: every {seconds}s"),
                            None => "Auto refresh: off".to_string(),
                        })
                        .dropdown_menu(move |menu, _, _| {
                            auto_refresh_menu(menu, &root, tab_id, true, auto_refresh)
                        }),
                )
        });
        let status_row = div()
            .debug_selector(|| "result-status-row".into())
            .h(px(theme::RESULT_STATUS_HEIGHT))
            .px(px(12.0))
            .flex()
            .items_center()
            .gap(px(8.0))
            .flex_shrink_0()
            .border_t_1()
            .border_color(theme::border())
            .text_size(px(11.0))
            .text_color(theme::text_muted())
            .child(
                div()
                    .debug_selector(|| "result-status-summary".into())
                    .min_w(px(0.0))
                    .overflow_hidden()
                    .whitespace_nowrap()
                    .text_ellipsis()
                    .child(summary),
            )
            .child(div().flex_1().min_w(px(0.0)))
            .when_some(stats_label, |this, label| {
                this.child(
                    div()
                        .debug_selector(|| "result-status-selection-stats".into())
                        .flex_shrink_0()
                        .whitespace_nowrap()
                        .child(label),
                )
            })
            .child(
                div()
                    .debug_selector(|| "result-status-duration".into())
                    .flex_shrink_0()
                    .whitespace_nowrap()
                    .child(query_time),
            )
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
