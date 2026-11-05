use super::super::*;

#[derive(Default)]
pub(in crate::app) struct DetailUiState {
    pub(in crate::app) edit: Option<CellDetailEdit>,
    pub(in crate::app) error: Option<String>,
    /// Formatted / canonical / raw presentation of JSON-like values.
    pub(in crate::app) value_mode: crate::cell_value::ValueMode,
    /// Active convert preview, scoped to the tab and cell it was chosen for.
    pub(in crate::app) convert: Option<(u64, CellPosition, crate::cell_value::ConvertKind)>,
    /// Hex / Text / Image view of a binary value (see `binary_view`).
    pub(in crate::app) binary: super::binary_view::BinaryViewState,
}

/// Plain text values use a textarea; JSON-like values use the code editor so
/// they get tree-sitter highlighting while being edited.
#[derive(Clone)]
pub(in crate::app) enum CellDetailEditor {
    Text(gpui::Entity<TextareaState>),
    Code(gpui::Entity<EditorState>),
}

impl CellDetailEditor {
    pub(in crate::app) fn value(&self, cx: &gpui::App) -> gpui::SharedString {
        match self {
            Self::Text(state) => state.read(cx).value(),
            Self::Code(state) => state.read(cx).value(),
        }
    }

    #[cfg(test)]
    pub(in crate::app) fn set_value(&self, text: &str, window: &mut Window, cx: &mut gpui::App) {
        match self {
            Self::Text(state) => state.update(cx, |state, cx| {
                state.set_value(text.to_string(), window, cx)
            }),
            Self::Code(state) => state.update(cx, |state, cx| {
                state.set_value(text.to_string(), window, cx)
            }),
        }
    }

    fn focus(&self, window: &mut Window, cx: &mut gpui::App) {
        match self {
            Self::Text(state) => state.update(cx, |state, cx| state.focus(window, cx)),
            Self::Code(state) => state.update(cx, |state, cx| state.focus(window, cx)),
        }
    }
}

pub(in crate::app) struct CellDetailEdit {
    pub(in crate::app) position: CellPosition,
    pub(in crate::app) editor: CellDetailEditor,
    pub(in crate::app) original_text: gpui::SharedString,
    /// Text the editor was seeded with (JSON is edited pretty-printed while
    /// `original_text` stays the stored value); an unchanged seed is a no-op.
    pub(in crate::app) seed_text: gpui::SharedString,
    pub(in crate::app) payload: InlineEditPayload,
}

impl AppRoot {
    pub(in crate::app) fn open_cell_editor(
        &mut self,
        tab_id: u64,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(range) = self.view(tab_id).grid_ui.selection.borrow().range else {
            return;
        };
        let start = range.normalized().start;
        self.begin_inline_edit(tab_id, start.row, start.column, window, cx);
    }

    pub(in crate::app) fn select_grid_cell(&mut self, tab_id: u64, position: CellPosition) {
        self.view(tab_id)
            .grid_ui
            .selection
            .borrow_mut()
            .select(position);
        self.refresh_cell_detail_target(tab_id, position);
    }

    /// Points an open right panel at `position` without touching the grid
    /// selection (the Properties view uses this to sync the Column tab).
    pub(in crate::app) fn refresh_cell_detail_target(
        &mut self,
        tab_id: u64,
        position: CellPosition,
    ) {
        let Some(tab) = self.workspace.tab(tab_id).cloned() else {
            return;
        };
        let detail_open = self
            .workspace
            .runtime(tab.id)
            .is_some_and(|runtime| runtime.cell_detail.open);
        if detail_open {
            let detail = self.build_cell_detail(&tab, position);
            let column = self.build_column_detail(&tab, position);
            let load_row = self.workspace.runtime(tab.id).is_some_and(|runtime| {
                runtime
                    .cell_detail
                    .opened_tabs
                    .contains(&CellDetailTab::Row)
            });
            let cached_row = if load_row {
                self.workspace.runtime(tab.id).and_then(|runtime| {
                    (runtime
                        .cell_detail
                        .target
                        .is_some_and(|target| target.row == position.row))
                    .then_some(runtime.cell_detail.row.clone())
                    .flatten()
                })
            } else {
                None
            };
            let row = cached_row.or_else(|| {
                if load_row {
                    self.build_cell_detail_row(&tab, position.row)
                } else {
                    None
                }
            });
            if let Some(runtime) = self.workspace.runtime_mut(tab.id) {
                runtime.cell_detail.show(position);
                runtime.cell_detail.detail = detail;
                runtime.cell_detail.set_row(row);
                runtime.cell_detail.column = column;
            }
        }
    }

    pub(in crate::app) fn open_cell_detail_tab(
        &mut self,
        tab_id: u64,
        tab_kind: CellDetailTab,
        cx: &mut Context<Self>,
    ) {
        self.open_cell_detail_tab_at(tab_id, tab_kind, None, cx);
    }

    /// Opens `tab_kind`; `target` overrides the grid selection as the cell the
    /// panel describes (Properties view: a column without changing the grid).
    pub(in crate::app) fn open_cell_detail_tab_at(
        &mut self,
        tab_id: u64,
        tab_kind: CellDetailTab,
        target_override: Option<CellPosition>,
        cx: &mut Context<Self>,
    ) {
        let already_open = self.workspace.runtime(tab_id).is_some_and(|runtime| {
            runtime.cell_detail.open && runtime.cell_detail.opened_tabs.contains(&tab_kind)
        });
        if already_open {
            if let Some(runtime) = self.workspace.runtime_mut(tab_id) {
                runtime.cell_detail.tab = tab_kind;
            }
            self.notify_tab(tab_id, cx);
            return;
        }
        // The panel opens even without a usable target (no grid selection, or
        // the cell is gone): its tabs then show an empty state and fill in as
        // soon as a cell (or, in Properties, a column row) is selected.
        let Some(tab) = self.workspace.tab(tab_id).cloned() else {
            return;
        };
        let target = target_override
            .or_else(|| {
                self.view(tab_id)
                    .grid_ui
                    .selection
                    .borrow()
                    .range
                    .map(|range| range.normalized().start)
            })
            .or_else(|| {
                self.workspace
                    .runtime(tab_id)
                    .and_then(|runtime| runtime.cell_detail.target)
            })
            .or_else(|| {
                // Properties -> Columns: a selected column row is the target
                // of the Column tab (row 0 supplies the sample cell).
                (tab_kind == CellDetailTab::Column)
                    .then(|| {
                        self.workspace
                            .runtime(tab_id)
                            .filter(|runtime| {
                                runtime.columns_panel.presentation
                                    == crate::columns_panel::Presentation::Columns
                            })
                            .and_then(|runtime| runtime.columns_panel.selected)
                    })
                    .flatten()
                    .map(|column| CellPosition { row: 0, column })
            })
            .filter(|target| {
                target.row < self.grid(tab_id).row_count()
                    && target.column < self.display_column_count(tab_id)
            });
        let built = target.and_then(|target| {
            let detail = self.build_cell_detail(&tab, target)?;
            let column = self.build_column_detail(&tab, target);
            (tab_kind != CellDetailTab::Column || column.is_some())
                .then_some((target, detail, column))
        });
        let Some((target, detail, column)) = built else {
            if let Some(runtime) = self.workspace.runtime_mut(tab_id) {
                runtime.cell_detail.open_tab(tab_kind);
            }
            self.notify_tab(tab_id, cx);
            return;
        };
        // `show` clears every derived part, so rebuild all parts that any open
        // tab needs; otherwise opening one tab would blank the others.
        let load_row = tab_kind == CellDetailTab::Row
            || self.workspace.runtime(tab_id).is_some_and(|runtime| {
                runtime
                    .cell_detail
                    .opened_tabs
                    .contains(&CellDetailTab::Row)
            });
        let cached_row = if load_row {
            self.workspace.runtime(tab_id).and_then(|runtime| {
                (runtime
                    .cell_detail
                    .target
                    .is_some_and(|current| current.row == target.row))
                .then_some(runtime.cell_detail.row.clone())
                .flatten()
            })
        } else {
            None
        };
        let row = cached_row.or_else(|| {
            if load_row {
                self.build_cell_detail_row(&tab, target.row)
            } else {
                None
            }
        });
        if let Some(runtime) = self.workspace.runtime_mut(tab_id) {
            runtime.cell_detail.show(target);
            runtime.cell_detail.open_tab(tab_kind);
            runtime.cell_detail.detail = Some(detail);
            runtime.cell_detail.column = column;
            runtime.cell_detail.set_row(row);
        }
        self.notify_tab(tab_id, cx);
    }

    /// Rail buttons toggle: an inactive tab opens/focuses, the
    /// active tab closes.
    pub(in crate::app) fn toggle_cell_detail_tab(
        &mut self,
        tab_id: u64,
        tab_kind: CellDetailTab,
        cx: &mut Context<Self>,
    ) {
        let is_active = self
            .workspace
            .runtime(tab_id)
            .is_some_and(|runtime| runtime.cell_detail.open && runtime.cell_detail.tab == tab_kind);
        if is_active {
            self.close_cell_detail_tab(tab_id, tab_kind, cx);
        } else {
            self.open_cell_detail_tab(tab_id, tab_kind, cx);
        }
    }

    /// Drops the tab's in-progress detail edit and the error line that went
    /// with it. Another tab's unsaved edit is untouched (each view owns its own).
    fn discard_detail_edit_of_tab(&mut self, tab_id: u64) {
        let detail_ui = &mut self.view_mut(tab_id).detail_ui;
        detail_ui.edit = None;
        detail_ui.error = None;
    }

    /// Closes one panel tab; the panel itself closes with its last tab.
    pub(in crate::app) fn close_cell_detail_tab(
        &mut self,
        tab_id: u64,
        tab_kind: CellDetailTab,
        cx: &mut Context<Self>,
    ) {
        if let Some(runtime) = self.workspace.runtime_mut(tab_id) {
            runtime.cell_detail.close_tab(tab_kind);
        }
        if tab_kind == CellDetailTab::Details || !self.cell_detail_open(tab_id) {
            self.discard_detail_edit_of_tab(tab_id);
        }
        self.notify_tab(tab_id, cx);
    }

    pub(in crate::app) fn close_other_cell_detail_tabs(
        &mut self,
        tab_id: u64,
        keep: CellDetailTab,
        cx: &mut Context<Self>,
    ) {
        if let Some(runtime) = self.workspace.runtime_mut(tab_id) {
            runtime.cell_detail.close_other_tabs(keep);
        }
        if keep != CellDetailTab::Details {
            self.discard_detail_edit_of_tab(tab_id);
        }
        self.notify_tab(tab_id, cx);
    }

    pub(in crate::app) fn set_cell_detail_row_view(
        &mut self,
        tab_id: u64,
        view: crate::cell_detail::RowView,
        cx: &mut Context<Self>,
    ) {
        if let Some(runtime) = self.workspace.runtime_mut(tab_id) {
            runtime.cell_detail.set_row_view(view);
        }
        self.notify_tab(tab_id, cx);
    }

    fn cell_detail_open(&self, tab_id: u64) -> bool {
        self.workspace
            .runtime(tab_id)
            .is_some_and(|runtime| runtime.cell_detail.open)
    }

    pub(in crate::app) fn toggle_cell_detail_row_path(
        &mut self,
        tab_id: u64,
        path: &str,
        cx: &mut Context<Self>,
    ) {
        if let Some(runtime) = self.workspace.runtime_mut(tab_id) {
            runtime.cell_detail.toggle_row_path(path);
        }
        self.notify_tab(tab_id, cx);
    }

    pub(in crate::app) fn close_cell_detail_panel(&mut self, tab_id: u64, cx: &mut Context<Self>) {
        if let Some(runtime) = self.workspace.runtime_mut(tab_id) {
            runtime.cell_detail.close();
        }
        self.discard_detail_edit_of_tab(tab_id);
        self.notify_tab(tab_id, cx);
    }

    pub(in crate::app) fn set_cell_detail_placement(
        &mut self,
        tab_id: u64,
        placement: CellDetailPlacement,
        cx: &mut Context<Self>,
    ) {
        if let Some(runtime) = self.workspace.runtime_mut(tab_id) {
            runtime.cell_detail.set_placement(placement);
        }
        self.notify_tab(tab_id, cx);
    }

    pub(in crate::app) fn begin_cell_detail_popup_drag(
        &mut self,
        tab_id: u64,
        kind: crate::cell_detail::PopupDragKind,
        mouse: gpui::Point<gpui::Pixels>,
        viewport: gpui::Size<gpui::Pixels>,
        cx: &mut Context<Self>,
    ) {
        let area = self.cell_detail_popup_area(tab_id, viewport);
        let shown_at = self.workspace.runtime(tab_id).map(|runtime| {
            let geometry = runtime.cell_detail.popup.clamped(area);
            self.cell_detail_popup_position(tab_id, &geometry, area)
        });
        if let (Some(shown_at), Some(runtime)) = (shown_at, self.workspace.runtime_mut(tab_id)) {
            runtime.cell_detail.begin_popup_drag(
                kind,
                (f32::from(mouse.x), f32::from(mouse.y)),
                area,
                shown_at,
            );
        }
        self.notify_tab(tab_id, cx);
    }

    pub(in crate::app) fn update_cell_detail_popup_drag(
        &mut self,
        tab_id: u64,
        mouse: gpui::Point<gpui::Pixels>,
        viewport: gpui::Size<gpui::Pixels>,
        cx: &mut Context<Self>,
    ) {
        let area = self.cell_detail_popup_area(tab_id, viewport);
        if let Some(runtime) = self.workspace.runtime_mut(tab_id) {
            if runtime
                .cell_detail
                .update_popup_drag((f32::from(mouse.x), f32::from(mouse.y)), area)
            {
                self.notify_tab(tab_id, cx);
            }
        }
    }

    pub(in crate::app) fn end_cell_detail_popup_drag(
        &mut self,
        tab_id: u64,
        cx: &mut Context<Self>,
    ) {
        if let Some(runtime) = self.workspace.runtime_mut(tab_id) {
            runtime.cell_detail.end_popup_drag();
        }
        self.notify_tab(tab_id, cx);
    }

    pub(in crate::app) fn tab_cell_detail(
        &self,
        tab_id: u64,
    ) -> Option<&crate::cell_detail::CellDetailState> {
        self.workspace
            .runtime(tab_id)
            .map(|runtime| &runtime.cell_detail)
    }

    pub(in crate::app) fn selected_cell_detail(&self, tab_id: u64) -> Option<&CellDetail> {
        let state = &self.workspace.runtime(tab_id)?.cell_detail;
        state.open.then_some(state.detail.as_ref()).flatten()
    }

    pub(in crate::app) fn refresh_cell_detail(&mut self, tab_id: u64) {
        let Some(tab) = self
            .workspace
            .tabs()
            .iter()
            .find(|tab| tab.id == tab_id)
            .cloned()
        else {
            return;
        };
        let target = self
            .workspace
            .runtime(tab_id)
            .and_then(|runtime| runtime.cell_detail.target);
        let Some(target) = target else {
            return;
        };
        let column = self.build_column_detail(&tab, target);
        let Some(detail) = self.build_cell_detail(&tab, target) else {
            return;
        };
        let row = self
            .workspace
            .runtime(tab_id)
            .is_some_and(|runtime| {
                runtime
                    .cell_detail
                    .opened_tabs
                    .contains(&CellDetailTab::Row)
            })
            .then(|| self.build_cell_detail_row(&tab, target.row))
            .flatten();
        if let Some(runtime) = self.workspace.runtime_mut(tab_id) {
            runtime.cell_detail.detail = Some(detail);
            runtime.cell_detail.set_row(row);
            runtime.cell_detail.column = column;
        }
    }

    pub(in crate::app) fn build_column_detail(
        &self,
        tab: &Tab,
        position: CellPosition,
    ) -> Option<CellDetailColumn> {
        let source_column = self.source_column_for_display(tab.id, position.column)?;
        let name = self.grid(tab.id).column_name(source_column)?.to_string();
        let metadata = self.workspace.runtime(tab.id).and_then(|runtime| {
            runtime
                .structure_columns
                .iter()
                .find(|column| column.name == name)
        });
        let mongo_type = (tab.query_mode.is_mongo_find())
            .then(|| self.mongo_row_fields(tab.id, position.row).ok())
            .flatten()
            .and_then(|fields| {
                fields
                    .fields
                    .iter()
                    .find(|field| field.name == name)
                    .map(|field| field.native_type.clone())
            });
        // Every Mongo collection has `_id` as its primary key.
        let mongo_id = tab.query_mode.is_mongo_find() && name == "_id";
        Some(CellDetailColumn {
            source_column,
            name,
            type_name: mongo_type
                .or_else(|| self.resolved_column_type(tab.id, source_column))
                .unwrap_or_else(|| "unknown".into()),
            nullable: metadata.map(|column| column.nullable),
            primary_key: metadata
                .map(|column| column.primary_key)
                .or_else(|| mongo_id.then_some(true)),
            is_generated: metadata.and_then(|column| column.is_generated),
            auto_increment: metadata.and_then(|column| column.auto_increment),
            default_expression: metadata.and_then(|column| column.default_expression.clone()),
        })
    }

    pub(in crate::app) fn build_cell_detail_row(
        &self,
        tab: &Tab,
        row: usize,
    ) -> Option<CellDetailRow> {
        let grid = self.grid(tab.id);
        if row >= grid.row_count() {
            return None;
        }

        if tab.query_mode.is_mongo_find() {
            if let Ok(row_fields) = self.mongo_row_fields(tab.id, row) {
                let base_document = serde_json::Value::Object(
                    row_fields
                        .fields
                        .iter()
                        .map(|field| (field.name.clone(), field.value.clone()))
                        .collect(),
                );
                let document = self
                    .workspace
                    .runtime(tab.id)
                    .map(|runtime| {
                        crate::mongo_result::apply_staged_updates(
                            &base_document,
                            row,
                            &runtime.mongo.mutations,
                        )
                    })
                    .unwrap_or(base_document);
                let fields = row_fields
                    .fields
                    .iter()
                    .map(|field| {
                        let value = document
                            .get(&field.name)
                            .cloned()
                            .unwrap_or(serde_json::Value::Null);
                        CellDetailRowField {
                            name: field.name.clone(),
                            type_name: field.native_type.clone(),
                            is_null: value.is_null(),
                            value: mongo_json_display_value(&value),
                        }
                    })
                    .collect();
                let json: std::sync::Arc<str> = serde_json::to_string_pretty(&document)
                    .unwrap_or_default()
                    .into();
                return Some(CellDetailRow {
                    row_number: row + 1,
                    fields,
                    json,
                    document: std::sync::Arc::new(document),
                });
            }
        }

        let mut fields = Vec::with_capacity(grid.column_count());
        let mut document = serde_json::Map::new();
        let mut members = Vec::with_capacity(grid.column_count());
        // `SELECT a.id, b.id` has two `id` columns: the tree and the JSON text
        // key them `id`, `id_2`, so none is lost (the field list keeps the
        // real names).
        let (member_names, _) = super::copy::unique_column_names(
            (0..grid.column_count()).map(|column| grid.column_name(column).unwrap_or_default()),
        );
        for (source_column, member_name) in member_names.iter().enumerate() {
            let name = grid.column_name(source_column)?.to_string();
            let visible_columns = &self.view(tab.id).grid_ui.visible_columns;
            let display_column = if visible_columns.is_empty() {
                Some(source_column)
            } else {
                visible_columns
                    .iter()
                    .position(|column| *column == source_column)
            };
            let position = CellPosition {
                row,
                column: source_column,
            };
            let pending = display_column
                .and_then(|column| self.pending_cell_text(tab.id, CellPosition { row, column }));
            let value = pending
                .as_ref()
                .map(|text| text.to_string())
                .or_else(|| grid.full_cell_text(position))
                .or_else(|| grid.cell_text(position).map(|text| text.to_string()))?;
            let is_null = match display_column {
                Some(column) if pending.is_some() => {
                    self.pending_is_null(tab.id, CellPosition { row, column })
                }
                _ => grid.cell_is_null(position),
            };
            let type_name = self
                .resolved_column_type(tab.id, source_column)
                .unwrap_or_else(|| "text".into());
            let cell = row_json_cell(&value, &type_name, is_null);
            members.push(format!(
                "{}:{}",
                serde_json::to_string(member_name).unwrap_or_default(),
                cell.literal
            ));
            document.insert(member_name.clone(), cell.value);
            fields.push(CellDetailRowField {
                name,
                type_name,
                value,
                is_null,
            });
        }
        let document = serde_json::Value::Object(document);
        // The text is assembled from the cells' own JSON (numbers and nested
        // JSON exactly as stored) and only re-indented.
        let json = crate::json_text::pretty(&format!("{{{}}}", members.join(",")))
            .or_else(|| serde_json::to_string_pretty(&document).ok())
            .unwrap_or_default();
        Some(CellDetailRow {
            row_number: row + 1,
            fields,
            json: json.into(),
            document: std::sync::Arc::new(document),
        })
    }

    pub(in crate::app) fn build_cell_detail(
        &self,
        tab: &Tab,
        position: CellPosition,
    ) -> Option<CellDetail> {
        self.build_cell_detail_with_row(tab, position, None)
    }

    pub(in crate::app) fn build_cell_detail_with_row(
        &self,
        tab: &Tab,
        position: CellPosition,
        inspected_row: Option<&engine_driver_api::RowFields>,
    ) -> Option<CellDetail> {
        let grid = self.grid(tab.id);
        let source_column = self.source_column_for_display(tab.id, position.column)?;
        let column_name = grid.column_name(source_column)?.to_string();
        let source_position = CellPosition {
            row: position.row,
            column: source_column,
        };
        let structure_column = self.workspace.runtime(tab.id).and_then(|runtime| {
            runtime
                .structure_columns
                .iter()
                .find(|column| column.name == column_name)
        });
        let mut type_name = self
            .resolved_column_type(tab.id, source_column)
            .unwrap_or_else(|| "text".into());
        // A large binary value is not turned into its whole hex text per
        // selection: the panel reads the bytes from the page when it draws.
        let large_binary = self.pending_cell_text(tab.id, position).is_none()
            && self
                .binary_cell_len(tab, source_column, source_position)
                .is_some_and(|len| len > crate::binary_value::TEXT_EDIT_CAP_BYTES);
        let mut value = self
            .pending_cell_text(tab.id, position)
            .map(|text| text.to_string())
            .or_else(|| {
                if large_binary {
                    None
                } else {
                    grid.full_cell_text(source_position)
                }
            })
            .or_else(|| grid.cell_text(source_position).map(|text| text.to_string()))?;
        let mut is_null = grid.cell_is_null(source_position);
        let mut mongo_canonical = None::<String>;
        let mut nullable = structure_column
            .map(|column| column.nullable)
            .unwrap_or(true);
        let mut editable = if tab.query_mode.is_mongo_find() {
            column_name != "_id"
        } else {
            self.sql_cell_editable(tab.id, position.row)
        };
        if let Some(pending_is_null) = self.workspace.runtime(tab.id).and_then(|runtime| {
            runtime
                .pending_edits
                .iter()
                .find(|pending| pending.row == position.row && pending.column == position.column)
                .map(|pending| matches!(&pending.request.value, MutationValue::Null))
        }) {
            is_null = pending_is_null;
        }

        if tab.query_mode.is_mongo_find() {
            let fetched_row = if inspected_row.is_none() {
                self.mongo_row_fields(tab.id, position.row).ok()
            } else {
                None
            };
            if let Some(row_fields) = inspected_row.or(fetched_row.as_deref()) {
                let refusal = mongo_edit_refusal(row_fields, &column_name);
                // A field a staged `$unset` removes is missing, whatever the page holds.
                let unset = self.workspace.runtime(tab.id).is_some_and(|runtime| {
                    runtime.mongo.mutations.is_unset(position.row, &column_name)
                });
                if let Some(field) = row_fields
                    .fields
                    .iter()
                    .find(|field| field.name == column_name)
                    .filter(|_| !unset)
                {
                    type_name = field.native_type.clone();
                    if self.pending_cell_text(tab.id, position).is_none() {
                        value = mongo_detail_value(field);
                    }
                    is_null = field.value.is_null();
                    nullable = column_name != "_id";
                    editable = field.editable && refusal.is_none();
                    if matches!(field.native_type.as_str(), "document" | "array") {
                        mongo_canonical = serde_json::to_string_pretty(&field.value).ok();
                    }
                } else {
                    // The document has no such field: not a NULL and not an
                    // empty string. Writing a value creates it.
                    type_name = "missing".into();
                    if unset || self.pending_cell_text(tab.id, position).is_none() {
                        value = String::new();
                    }
                    is_null = false;
                    editable = refusal.is_none();
                }
            }
            if let Some(runtime) = self.workspace.runtime(tab.id) {
                // The latest staged change of the field wins: a `$set` of NULL
                // is NULL, a later `$unset` makes the field missing (not NULL).
                match runtime.mongo.mutations.latest_field_change(position.row, &column_name) {
                    Some(crate::mongo_result::PendingMongoMutation::Update { value, .. }) => {
                        is_null = value.is_null();
                    }
                    Some(crate::mongo_result::PendingMongoMutation::Unset { .. }) => is_null = false,
                    _ => {}
                }
            }
        }

        // Server-text and ClickHouse composite columns, non-UTF-8 text and
        // geometry are shown but are not the value to write back.
        let read_only_reason = (!tab.query_mode.is_mongo_find())
            .then(|| {
                self.sql_column_read_only_reason(tab.id, source_column)
                    .or_else(|| {
                        (!is_null && self.pending_cell_text(tab.id, position).is_none())
                            .then(|| {
                                self.sql_cell_read_only_reason(tab.id, position.row, source_column)
                            })
                            .flatten()
                    })
            })
            .flatten();
        let mut detail = cell_detail::build_cell_detail(
            position,
            source_column,
            column_name,
            type_name,
            value,
            is_null,
            nullable,
            editable && read_only_reason.is_none(),
            String::new(),
        );
        detail.read_only_reason = read_only_reason;
        // A Mongo document / array is shown in shell syntax (the Value box and
        // the editor seed) and its canonical Extended JSON is the other mode.
        if let Some(canonical) = mongo_canonical {
            detail.formatted_json = (cell_detail::value_length(&canonical)
                <= cell_detail::MAX_JSON_FORMAT_LENGTH)
                .then_some(canonical);
        }
        detail.cell_class = self
            .cell_class(tab.id, position.row, source_column)
            .map(crate::grid_arrow::CellClass::label);
        detail.binary = self.binary_detail(tab, position, source_column, detail.is_null);
        detail.binary_form =
            self.binary_column_form(tab, position.row, source_column, grid.cell_bytes(source_position).is_some());
        if let Some(binary) = &detail.binary {
            detail.length = binary.len;
        }
        Some(detail)
    }

    /// Whether the tab's cells are the byte values the panel previews: SQL
    /// results (Arrow `Binary`); a document store delivers binary values as
    /// text and Redis has its own element editor.
    fn binary_cells_apply(&self, tab: &Tab) -> bool {
        !tab.query_mode.is_mongo() && !tab.query_mode.is_redis()
    }

    /// Size in bytes of the binary value of the cell at SOURCE `position`,
    /// without reading it: real bytes, or PostgreSQL's `\x` + hex text.
    fn binary_cell_len(&self, tab: &Tab, source_column: usize, position: CellPosition) -> Option<usize> {
        if !self.binary_cells_apply(tab) {
            return None;
        }
        let grid = self.grid(tab.id);
        if let Some(bytes) = grid.cell_bytes(position) {
            return Some(bytes.len());
        }
        if grid.native_type(source_column) == Some("bytea") {
            return Some(grid.cell_str(position)?.strip_prefix("\\x")?.len() / 2);
        }
        None
    }

    /// How a binary column's values are written back as text: real bytes
    /// (`0x…`), or PostgreSQL's `bytea`, which arrives as the server's `\x…`
    /// text. `None` for a column that holds no bytes, and for one whose values
    /// are text that is not valid UTF-8 (delivered as bytes, but not binary
    /// data: writing `0x…` would store that literal text). `page_bytes`: this
    /// cell holds real bytes; `row` is a display row.
    pub(in crate::app) fn binary_column_form(
        &self,
        tab: &Tab,
        row: usize,
        source_column: usize,
        page_bytes: bool,
    ) -> Option<crate::binary_value::HexForm> {
        use crate::binary_value::HexForm;
        if !self.binary_cells_apply(tab) {
            return None;
        }
        let grid = self.grid(tab.id);
        if grid
            .native_type(source_column)
            .is_some_and(|native| native.ends_with("(invalid utf-8)"))
            || self.cell_class(tab.id, row, source_column)
                == Some(crate::grid_arrow::CellClass::InvalidUtf8Text)
        {
            return None;
        }
        if page_bytes || grid.column_type(source_column) == Some("binary") {
            Some(HexForm::ZeroX)
        } else if grid.native_type(source_column) == Some("bytea") {
            Some(HexForm::PgBytea)
        } else {
            None
        }
    }

    /// The binary view of the cell at DISPLAY `position`, when it holds bytes
    /// (a staged edit counts: it is what Apply writes). Reads the page
    /// directly: a value within the preview cap is decoded at most once here,
    /// a larger PostgreSQL `bytea` is only measured.
    fn binary_detail(
        &self,
        tab: &Tab,
        position: CellPosition,
        source_column: usize,
        is_null: bool,
    ) -> Option<crate::cell_detail::BinaryDetail> {
        use crate::binary_value::{detect_image, image_dimensions, HexForm, ImageKind, PREVIEW_CAP_BYTES};
        if is_null || !self.binary_cells_apply(tab) {
            return None;
        }
        let grid = self.grid(tab.id);
        let source = CellPosition {
            row: position.row,
            column: source_column,
        };
        let page = grid.cell_bytes(source);
        // Text that is not valid UTF-8 is viewable (it has bytes) but never written back.
        let loadable = self.binary_column_form(tab, position.row, source_column, page.is_some());
        let form = if page.is_some() {
            HexForm::ZeroX
        } else {
            loadable?
        };
        let (owned, len): (Option<Arc<[u8]>>, usize) =
            match (self.pending_cell_text(tab.id, position), page, form) {
                (Some(text), _, _) => {
                    let bytes: Arc<[u8]> = crate::binary_value::from_hex_form(&text, form)?.into();
                    let len = bytes.len();
                    (Some(bytes), len)
                }
                (None, Some(bytes), _) => (None, bytes.len()),
                (None, None, HexForm::PgBytea) => {
                    let text = grid.cell_str(source)?;
                    let len = text.strip_prefix("\\x")?.len() / 2;
                    let decoded = if len <= PREVIEW_CAP_BYTES {
                        Some(Arc::<[u8]>::from(crate::binary_value::from_hex_form(text, form)?))
                    } else {
                        None
                    };
                    (decoded, len)
                }
                (None, None, HexForm::ZeroX) => return None,
            };
        let bytes: Option<&[u8]> = owned.as_deref().or(page);
        let image = bytes
            .filter(|bytes| bytes.len() <= PREVIEW_CAP_BYTES)
            .and_then(detect_image);
        let picture = image.zip(bytes).map(|(kind, bytes)| {
            let format = match kind {
                ImageKind::Png => gpui::ImageFormat::Png,
                ImageKind::Jpeg => gpui::ImageFormat::Jpeg,
                ImageKind::Gif => gpui::ImageFormat::Gif,
                ImageKind::Webp => gpui::ImageFormat::Webp,
                ImageKind::Bmp => gpui::ImageFormat::Bmp,
            };
            Arc::new(gpui::Image::from_bytes(format, bytes.to_vec()))
        });
        Some(crate::cell_detail::BinaryDetail {
            len,
            dimensions: image.zip(bytes).and_then(|(kind, bytes)| image_dimensions(bytes, kind)),
            image,
            picture,
            owned,
            form,
        })
    }

    pub(in crate::app) fn single_sql_primary_key(
        &self,
        tab_id: u64,
    ) -> Result<&crate::catalog::CatalogColumn, SqlEditError> {
        let runtime = self
            .workspace
            .runtime(tab_id)
            .ok_or(SqlEditError::StructureUnavailable)?;
        let mut keys = runtime
            .structure_columns
            .iter()
            .filter(|column| column.primary_key);
        let key = keys.next().ok_or(SqlEditError::NoPrimaryKey)?;
        if keys.next().is_some() {
            return Err(SqlEditError::CompositeKey);
        }
        Ok(key)
    }

    fn sql_cell_editable(&self, tab_id: u64, row: usize) -> bool {
        let Ok(primary_key) = self.sql_edit_key(tab_id) else {
            return false;
        };
        let grid = self.grid(tab_id);
        let Some(key_column) = (0..grid.column_count())
            .find(|column| grid.column_name(*column) == Some(primary_key.name.as_str()))
        else {
            return false;
        };
        if grid.cell_is_null(CellPosition {
            row,
            column: key_column,
        }) {
            return false;
        }
        let Some(key_text) = grid.full_cell_text(CellPosition {
            row,
            column: key_column,
        }) else {
            return false;
        };
        self.key_cell_value(tab_id, row, key_column, &key_text, &primary_key.data_type)
            .is_ok()
    }

    pub(in crate::app) fn cell_detail_payload(
        &self,
        tab: &Tab,
        detail: &CellDetail,
    ) -> Option<InlineEditPayload> {
        self.cell_detail_payload_with_row(tab, detail, None)
    }

    pub(in crate::app) fn cell_detail_payload_with_row(
        &self,
        tab: &Tab,
        detail: &CellDetail,
        inspected_row: Option<&engine_driver_api::RowFields>,
    ) -> Option<InlineEditPayload> {
        if tab.query_mode.is_mongo_find() {
            let fetched_row = if inspected_row.is_none() {
                self.mongo_row_fields(tab.id, detail.position.row).ok()
            } else {
                None
            };
            let row_fields = inspected_row.or(fetched_row.as_deref())?;
            let unset = self.workspace.runtime(tab.id).is_some_and(|runtime| {
                runtime.mongo.mutations.is_unset(detail.position.row, &detail.column_name)
            });
            let field = row_fields
                .fields
                .iter()
                .find(|field| field.name == detail.column_name)
                .filter(|_| !unset);
            let writable = mongo_edit_refusal(row_fields, &detail.column_name).is_none()
                && field.is_none_or(|field| field.editable);
            return Some(if writable {
                InlineEditPayload::Mongo {
                    native_type: field.map_or("missing", |field| field.native_type.as_str()).into(),
                    identity: row_fields.mutation_identity.clone(),
                    database: tab.database.clone(),
                    collection: tab.collection.clone(),
                    field: detail.column_name.clone(),
                }
            } else {
                InlineEditPayload::ReadOnly
            });
        }

        let primary_key = self.sql_edit_key(tab.id).ok()?.clone();
        let grid = self.grid(tab.id);
        let key_column = (0..grid.column_count())
            .find(|column| grid.column_name(*column) == Some(primary_key.name.as_str()))?;
        let key_text = grid.full_cell_text(CellPosition {
            row: detail.position.row,
            column: key_column,
        })?;
        let key_value = self
            .key_cell_value(
                tab.id,
                detail.position.row,
                key_column,
                &key_text,
                &primary_key.data_type,
            )
            .ok()?;
        let key = self.sql_row_key(tab.id, detail.position.row).ok()?;
        Some(InlineEditPayload::Sql {
            primary_key,
            key_value,
            key,
            column_name: detail.column_name.clone(),
            data_type: self.sql_edit_data_type(tab.id, detail.source_column, detail.position.row),
        })
    }

    pub(in crate::app) fn open_cell_detail_editor(
        &mut self,
        tab_id: u64,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(tab) = self.workspace.tab(tab_id).cloned() else {
            return;
        };
        let Some(detail) = self.selected_cell_detail(tab_id).cloned() else {
            return;
        };
        if self
            .view(tab_id)
            .detail_ui
            .edit
            .as_ref()
            .is_some_and(|edit| edit.position == detail.position)
        {
            return;
        }
        if self.refuse_read_only_edit(tab_id, cx) {
            return;
        }
        if detail.binary.as_ref().is_some_and(|binary| !binary.text_editable()) {
            self.view_mut(tab_id).detail_ui.error = Some(format!(
                "This value is {}: too large to edit as text. Use Load from file.",
                crate::binary_value::format_size(detail.length)
            ));
            self.notify_tab(tab_id, cx);
            return;
        }
        let Some(payload) = self.cell_detail_payload(&tab, &detail) else {
            return;
        };
        if matches!(payload, InlineEditPayload::ReadOnly) {
            if tab.query_mode.is_mongo_find() {
                // Never silent: say why the row or field cannot be edited.
                let refusal = self
                    .mongo_row_fields(tab.id, detail.position.row)
                    .ok()
                    .and_then(|row| mongo_edit_refusal(&row, &detail.column_name));
                if let Some(refusal) = refusal {
                    self.view_mut(tab_id).detail_ui.error = Some(refusal.to_string());
                    self.notify_tab(tab_id, cx);
                }
            }
            return;
        }
        // A SQL value is edited as STORED: a pretty-printed copy would rewrite
        // numbers, whitespace and repeated keys the user never touched. A NULL
        // opens empty (committing it unchanged changes nothing).
        let original_text = gpui::SharedString::from(detail.value.to_string());
        let stored_seed = if detail.is_null && !tab.query_mode.is_mongo_find() {
            gpui::SharedString::default()
        } else {
            original_text.clone()
        };
        let (editor, seed_text) = match detail.formatted_json.as_ref() {
            Some(_) => {
                // The text as stored / as shell syntax: a pretty-printed copy
                // would rewrite what the user never touched.
                let seed = stored_seed;
                let editor = cx.new(|cx| {
                    EditorState::new(window, cx)
                        .language("json")
                        .line_number(false)
                        .folding(false)
                        .soft_wrap(true)
                        .default_value(seed.clone())
                });
                (CellDetailEditor::Code(editor), seed)
            }
            None => {
                let editor = cx.new(|cx| {
                    TextareaState::new(window, cx)
                        .auto_grow(3, 12)
                        .soft_wrap(true)
                        .default_value(stored_seed.clone())
                });
                (CellDetailEditor::Text(editor), stored_seed)
            }
        };
        editor.focus(window, cx);
        let detail_ui = &mut self.view_mut(tab_id).detail_ui;
        detail_ui.edit = Some(CellDetailEdit {
            position: detail.position,
            editor,
            original_text,
            seed_text,
            payload,
        });
        detail_ui.error = None;
        self.notify_tab(tab_id, cx);
    }

    pub(in crate::app) fn copy_cell_detail_value(&mut self, tab_id: u64, cx: &mut Context<Self>) {
        if self
            .selected_cell_detail(tab_id)
            .is_some_and(|detail| detail.binary.is_some())
        {
            // The whole value as `0x` + hex, not the panel's shortened text.
            self.copy_cell_detail_binary(tab_id, cx);
            return;
        }
        if let Some(detail) = self.selected_cell_detail(tab_id) {
            cx.write_to_clipboard(gpui::ClipboardItem::new_string(detail.value.to_string()));
        }
    }

    pub(in crate::app) fn copy_cell_detail_column_name(
        &mut self,
        tab_id: u64,
        cx: &mut Context<Self>,
    ) {
        if let Some(detail) = self.selected_cell_detail(tab_id) {
            cx.write_to_clipboard(gpui::ClipboardItem::new_string(detail.column_name.clone()));
        }
    }

    /// `"col" = 'value'` / `IS NULL` for SQL tabs, `{ "field": value }` for Mongo.
    pub(in crate::app) fn copy_cell_detail_sql_condition(
        &mut self,
        tab_id: u64,
        cx: &mut Context<Self>,
    ) {
        let Some(tab) = self.workspace.tab(tab_id) else {
            return;
        };
        let Some(detail) = self.selected_cell_detail(tab_id).cloned() else {
            return;
        };
        let condition = if tab.query_mode.is_mongo_find() {
            self.mongo_row_fields(tab.id, detail.position.row)
                .ok()
                .and_then(|row| {
                    row.fields
                        .iter()
                        .find(|field| field.name == detail.column_name)
                        .cloned()
                })
                .map(|field| crate::cell_value::mongo_condition(&field.name, &field.value))
                .ok_or_else(|| "The document field is unavailable.".to_string())
        } else if detail.binary.as_ref().is_some_and(|binary| !binary.text_editable()) {
            Err("This binary value is too large for a SQL condition.".to_string())
        } else {
            let row = detail.position.row;
            let cell = self.copy_cell(
                tab.id,
                true,
                (row, detail.source_column),
                &detail.value,
                &self.sql_edit_data_type(tab.id, detail.source_column, row),
                detail.is_null,
            );
            super::copy::sql_condition(self.sql_copy_target(tab), &detail.column_name, &cell)
        };
        match condition {
            Ok(text) => {
                cx.write_to_clipboard(gpui::ClipboardItem::new_string(text));
                self.view_mut(tab_id).detail_ui.error = None;
            }
            Err(message) => self.view_mut(tab_id).detail_ui.error = Some(message),
        }
        self.notify_tab(tab_id, cx);
    }

    pub(in crate::app) fn cancel_cell_detail_editor(
        &mut self,
        tab_id: u64,
        cx: &mut Context<Self>,
    ) {
        self.discard_detail_edit_of_tab(tab_id);
        self.notify_tab(tab_id, cx);
    }

    pub(in crate::app) fn commit_cell_detail_editor(
        &mut self,
        tab_id: u64,
        cx: &mut Context<Self>,
    ) {
        let Some(edit) = self.view_mut(tab_id).detail_ui.edit.take() else {
            return;
        };
        let text = edit.editor.value(cx).to_string();
        if text == edit.seed_text {
            self.view_mut(tab_id).detail_ui.error = None;
            self.notify_tab(tab_id, cx);
            return;
        }
        let outcome = match &edit.payload {
            InlineEditPayload::Sql { .. } => self
                .try_stage_sql_edit(
                    tab_id,
                    edit.position,
                    &edit.payload,
                    edit.original_text.as_ref(),
                    &text,
                )
                .map_err(|error| match error {
                    SqlEditError::NotSqlCell => "Value is invalid for this column.".into(),
                    error => error.to_string(),
                }),
            InlineEditPayload::Mongo { .. } => {
                match build_inline_mongo_update(&edit.payload, edit.position.row, &text) {
                    Ok(mutation) => {
                        self.stage_mongo_mutation(tab_id, mutation, cx);
                        Ok(())
                    }
                    Err(error) => Err(error.to_string()),
                }
            }
            InlineEditPayload::ReadOnly => Err("Value is invalid for this column.".to_string()),
        };
        match outcome {
            Ok(()) => {
                self.refresh_cell_detail(tab_id);
                self.view_mut(tab_id).detail_ui.error = None;
            }
            Err(message) => {
                let detail_ui = &mut self.view_mut(tab_id).detail_ui;
                detail_ui.error = Some(message);
                detail_ui.edit = Some(edit);
            }
        }
        self.notify_tab(tab_id, cx);
    }

    pub(in crate::app) fn set_cell_detail_null(
        &mut self,
        tab_id: u64,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(tab) = self.workspace.tab(tab_id).cloned() else {
            return;
        };
        let Some(detail) = self.selected_cell_detail(tab_id).cloned() else {
            return;
        };
        if self.refuse_read_only_edit(tab_id, cx) {
            return;
        }
        if self
            .view(tab_id)
            .detail_ui
            .edit
            .as_ref()
            .is_some_and(|edit| edit.position != detail.position)
        {
            self.view_mut(tab_id).detail_ui.error =
                Some("Finish or cancel the active cell editor first.".into());
            self.notify_tab(tab_id, cx);
            return;
        }
        let Some(payload) = self
            .view(tab_id)
            .detail_ui
            .edit
            .as_ref()
            .map(|edit| edit.payload.clone())
            .or_else(|| self.cell_detail_payload(&tab, &detail))
        else {
            return;
        };
        if !detail.nullable
            || !detail.editable
            || matches!(&payload, InlineEditPayload::ReadOnly)
            || matches!(&payload, InlineEditPayload::Sql { primary_key, column_name, .. } if primary_key.name == *column_name)
        {
            self.view_mut(tab_id).detail_ui.error = Some("This cell cannot be set to NULL.".into());
            self.notify_tab(tab_id, cx);
            return;
        }
        self.view_mut(tab_id).detail_ui.edit = None;
        self.stage_cell_detail_null(tab_id, detail, payload, cx);
    }

    pub(in crate::app) fn stage_cell_detail_null(
        &mut self,
        tab_id: u64,
        detail: CellDetail,
        payload: InlineEditPayload,
        cx: &mut Context<Self>,
    ) {
        let blocked = self.workspace.runtime(tab_id).and_then(|runtime| {
            if runtime.sql_apply_running {
                Some("Wait for SQL Apply to finish.")
            } else if !runtime.query.can_run() {
                Some("Wait for the running query to finish.")
            } else if runtime.pending_deleted_rows.contains(&detail.position.row) {
                Some("This row is staged for deletion.")
            } else {
                None
            }
        });
        if let Some(message) = blocked {
            self.view_mut(tab_id).detail_ui.error = Some(message.into());
            self.notify_tab(tab_id, cx);
            return;
        }
        // The grid must still show the row whose key the payload carries.
        if let InlineEditPayload::Sql { key, .. } = &payload {
            if self.sql_row_key(tab_id, detail.position.row).as_ref() != Ok(key) {
                self.view_mut(tab_id).detail_ui.error = Some(SqlEditError::StaleRow.to_string());
                self.notify_tab(tab_id, cx);
                return;
            }
        }
        if !detail.nullable
            || !detail.editable
            || matches!(&payload, InlineEditPayload::ReadOnly)
            || matches!(&payload, InlineEditPayload::Sql { primary_key, column_name, .. } if primary_key.name == *column_name)
        {
            self.view_mut(tab_id).detail_ui.error = Some("This cell cannot be set to NULL.".into());
            self.notify_tab(tab_id, cx);
            return;
        }
        match payload {
            InlineEditPayload::Sql {
                primary_key,
                key_value,
                key,
                column_name,
                ..
            } => {
                if let Some(runtime) = self.workspace.runtime_mut(tab_id) {
                    let position = detail.position;
                    let (original_text, original_is_null) = runtime
                        .pending_edits
                        .iter()
                        .find(|pending| {
                            pending.row == position.row && pending.column == position.column
                        })
                        .map(|pending| (pending.original_text.clone(), pending.original_is_null))
                        .unwrap_or_else(|| {
                            (
                                Arc::<str>::from(detail.value.as_ref()),
                                runtime.grid.cell_is_null(CellPosition {
                                    row: position.row,
                                    column: detail.source_column,
                                }),
                            )
                        });
                    let request = crate::staged_sql::UpdateCellRequest {
                        database: String::new(),
                        schema: "public".into(),
                        table: String::new(),
                        key_column: primary_key.name,
                        key_value,
                        column: column_name,
                        value: MutationValue::Null,
                    };
                    let display_text = Arc::<str>::from("NULL");
                    if let Some(pending) = runtime.pending_edits.iter_mut().find(|pending| {
                        pending.row == position.row && pending.column == position.column
                    }) {
                        pending.display_text = display_text.clone();
                        pending.request = request;
                        pending.key = key;
                    } else {
                        runtime
                            .pending_edits
                            .push(crate::workspace::PendingCellEdit {
                                row: position.row,
                                column: position.column,
                                original_text,
                                original_is_null,
                                display_text: display_text.clone(),
                                request,
                                key,
                            });
                    }
                    runtime.set_pending_cell_text(position, display_text);
                }
            }
            InlineEditPayload::Mongo {
                identity,
                database,
                collection,
                field,
                ..
            } => {
                self.stage_mongo_mutation(
                    tab_id,
                    PendingMongoMutation::Update {
                        request: MutationRequest {
                            operation_id: crate::mongo_result::STAGED_OPERATION_ID,
                            kind: "update".into(),
                            database: Some(database),
                            object: Some(collection),
                            identity,
                            payload: serde_json::json!({"$set": {field.clone(): serde_json::Value::Null}}),
                        },
                        row: detail.position.row,
                        field,
                        value: serde_json::Value::Null,
                        display_text: "NULL".into(),
                    },
                    cx,
                );
            }
            InlineEditPayload::ReadOnly => {}
        }
        self.refresh_cell_detail(tab_id);
        self.view_mut(tab_id).detail_ui.error = None;
        self.notify_tab(tab_id, cx);
    }
}
