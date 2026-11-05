use super::super::*;

#[derive(Debug)]
pub(in crate::app) enum CloneSqlRow {
    Ready(InsertRowRequest),
    NeedsValues {
        request: InsertRowRequest,
        missing: Vec<String>,
    },
}

pub(in crate::app) fn finish_sql_clone_draft(
    mut request: InsertRowRequest,
    missing: &[String],
    json: &str,
    columns: &[crate::catalog::CatalogColumn],
) -> Result<InsertRowRequest, String> {
    let values = insert_values_from_json(json, columns)?;
    for name in missing {
        if !values
            .iter()
            .any(|(field, value)| field == name && !matches!(value, MutationValue::Null))
        {
            return Err(format!(
                "Enter a new non-NULL value for required primary key {name}"
            ));
        }
    }
    for (name, _) in &values {
        let column = columns
            .iter()
            .find(|column| &column.name == name)
            .ok_or("Clone column is not in the table")?;
        if column.is_generated != Some(false) || column.auto_increment != Some(false) {
            return Err(format!(
                "{name} is generated or has unknown insert metadata"
            ));
        }
    }
    for (name, _) in &request.values {
        if !values.iter().any(|(field, _)| field == name) {
            return Err(format!("Clone draft must retain source column {name}"));
        }
    }
    request.values = values;
    Ok(request)
}

const APPLY_ATOMIC_NOTICE: &str =
    "All staged SQL changes commit together or roll back together. Cancel preserves them.";
const APPLY_NON_ATOMIC_NOTICE: &str = "This database has no transactions: the changes are checked first but applied one after another, so a failure part-way can leave some of them applied. Cancel preserves them.";

/// What the Apply dialog promises about a batch on a `kind` connection.
pub(in crate::app) fn apply_notice(kind: ConnectionKind) -> &'static str {
    if kind.applies_batches_atomically() {
        APPLY_ATOMIC_NOTICE
    } else {
        APPLY_NON_ATOMIC_NOTICE
    }
}

/// A SQL row delete ready to be staged: the engine request (first key column)
/// plus the whole key of the row it was prepared on.
#[derive(Debug, Clone)]
pub(in crate::app) struct PreparedSqlDelete {
    pub(in crate::app) request: DeleteRowRequest,
    pub(in crate::app) key: crate::workspace::RowKey,
}

impl AppRoot {
    pub(in crate::app) fn sql_clone_available(&self, tab_id: u64) -> bool {
        let Some(tab) = self.workspace.tab(tab_id) else {
            return false;
        };
        let Some(runtime) = self.workspace.runtime(tab.id) else {
            return false;
        };
        self.sql_sort_available(tab_id)
            && !runtime.sql_apply_running
            && self.single_sql_primary_key(tab.id).is_ok()
            && runtime
                .structure_columns
                .iter()
                .all(|column| column.is_generated.is_some() && column.auto_increment.is_some())
            && runtime.grid.column_name(0).is_some_and(|column| {
                super::context_menu::sorted_sql_query(
                    self.sql_table_dialect(tab),
                    &tab.sql,
                    &self.sql_row_schema(tab),
                    &tab.collection,
                    column,
                    false,
                )
                .is_ok()
            })
    }
    pub(in crate::app) fn sql_row_schema(&self, tab: &Tab) -> String {
        tab.schema.clone().unwrap_or_else(|| {
            if tab.query_mode.is_sqlite_table()
                || self.saved_connections.iter().any(|saved| {
                    saved.matches_id(&tab.connection_id) && saved.kind == ConnectionKind::Sqlite
                })
            {
                "main".into()
            } else {
                "public".into()
            }
        })
    }
    /// What the Apply dialog of `tab_id` promises about its batch.
    pub(in crate::app) fn apply_dialog_notice(&self, tab_id: u64) -> &'static str {
        let connection = self
            .workspace
            .tab(tab_id)
            .map(|tab| tab.connection_id.as_str());
        connection
            .and_then(|connection| {
                self.saved_connections
                    .iter()
                    .find(|saved| saved.matches_id(connection))
            })
            .map_or(APPLY_ATOMIC_NOTICE, |saved| apply_notice(saved.kind))
    }

    pub(in crate::app) fn confirm_apply_sql_changes(
        &mut self,
        tab_id: u64,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(runtime) = self.workspace.runtime(tab_id) else {
            return;
        };
        if runtime.sql_apply_running {
            return;
        }
        let count = runtime.pending_edits.len() + runtime.pending_sql_rows.len();
        if count == 0 {
            return;
        }
        if self.refuse_read_only_edit(tab_id, cx) {
            return;
        }
        // A typed connection follows its type's "confirm data changes"; one
        // without a type keeps asking, as it always did.
        let effective = self.effective_for_tab(tab_id);
        if effective.connection_type.is_some() && !effective.confirm_data_change {
            self.apply_pending_edit(tab_id, cx);
            return;
        }
        let notice = self.apply_dialog_notice(tab_id);
        let summary = effective
            .confirm_data_change
            .then(|| self.data_change_summary_lines(tab_id));
        let root = cx.entity().downgrade();
        window.open_alert_dialog(cx, move |dialog, _, _| {
            let root = root.clone();
            dialog
                .title(format!("Apply {count} staged changes?"))
                .child(
                    div()
                        .flex()
                        .flex_col()
                        .gap(px(6.0))
                        .when_some(summary.clone(), |this, lines| {
                            this.child(
                                div()
                                    .debug_selector(|| "data-change-summary".into())
                                    .flex()
                                    .flex_col()
                                    .children(lines),
                            )
                        })
                        .child(notice),
                )
                .button_props(gpui_component::dialog::DialogButtonProps::default().ok_text("Apply"))
                .confirm()
                .on_ok(move |_, _, cx| {
                    let _ = root.update(cx, |this, cx| this.apply_pending_edit(tab_id, cx));
                    true
                })
        });
    }

    pub(in crate::app) fn prepare_sql_delete(
        &self,
        tab_id: u64,
        row: usize,
    ) -> Result<PreparedSqlDelete, SqlEditError> {
        let tab = self
            .workspace
            .tab(tab_id)
            .ok_or(SqlEditError::NoActiveTab)?;
        let grid = self.grid(tab_id);
        if !self.sql_sort_available(tab_id) || row >= grid.row_count() {
            return Err(SqlEditError::NotDeletable);
        }
        let runtime = self
            .workspace
            .runtime(tab.id)
            .ok_or(SqlEditError::StructureUnavailable)?;
        if runtime.sql_apply_running {
            return Err(SqlEditError::ApplyRunning);
        }
        if !runtime.query.can_run() {
            return Err(SqlEditError::QueryRunning);
        }
        let primary_key = self.sql_edit_key(tab.id)?;
        // Every key column, read now: the delete stays addressed by this key
        // whatever the grid shows by the time it is applied.
        let key = self.sql_row_key(tab.id, row)?;
        let missing = || SqlEditError::KeyValueMissing {
            row,
            column: primary_key.name.clone(),
        };
        let column = (0..grid.column_count())
            .find(|index| grid.column_name(*index) == Some(primary_key.name.as_str()))
            .ok_or_else(|| SqlEditError::KeyColumnMissing(primary_key.name.clone()))?;
        let text = grid
            .full_cell_text(CellPosition { row, column })
            .ok_or_else(missing)?;
        let key_value = self.key_cell_value(tab_id, row, column, &text, &primary_key.data_type)?;
        Ok(PreparedSqlDelete {
            request: DeleteRowRequest {
                database: tab.database.clone(),
                schema: self.sql_row_schema(tab),
                table: tab.collection.clone(),
                key_column: primary_key.name.clone(),
                key_value,
            },
            key,
        })
    }

    pub(in crate::app) fn prepare_sql_clone(
        &self,
        tab_id: u64,
        row: usize,
    ) -> Result<CloneSqlRow, String> {
        let tab = self.workspace.tab(tab_id).ok_or("No active SQL table")?;
        let grid = self.grid(tab_id);
        if !self.sql_sort_available(tab_id) || row >= grid.row_count() {
            return Err("SQL clone requires a connected table result".into());
        }
        let runtime = self
            .workspace
            .runtime(tab.id)
            .ok_or("Table structure unavailable")?;
        if runtime.sql_apply_running {
            return Err("Wait for SQL Apply to finish".into());
        }
        if runtime
            .structure_columns
            .iter()
            .filter(|column| column.primary_key)
            .count()
            != 1
        {
            return Err("SQL clone requires exactly one primary key".into());
        }
        if runtime.structure_columns.is_empty()
            || runtime
                .structure_columns
                .iter()
                .any(|column| column.is_generated.is_none() || column.auto_increment.is_none())
        {
            return Err(
                "Cannot safely clone SQL rows: catalog generated/default metadata is unavailable"
                    .into(),
            );
        }
        let column = grid.column_name(0).ok_or("Source column unavailable")?;
        super::context_menu::sorted_sql_query(
            self.sql_table_dialect(tab),
            &tab.sql,
            &self.sql_row_schema(tab),
            &tab.collection,
            column,
            false,
        )
        .map_err(|error| error.to_string())?;
        let mut values = Vec::with_capacity(runtime.structure_columns.len());
        let mut missing = Vec::new();
        for column in &runtime.structure_columns {
            if column.is_generated == Some(true) || column.auto_increment == Some(true) {
                continue;
            }
            if column.primary_key {
                missing.push(column.name.clone());
                continue;
            }
            let source_column = (0..grid.column_count())
                .find(|index| grid.column_name(*index) == Some(column.name.as_str()))
                .ok_or_else(|| format!("{} is missing from the current result", column.name))?;
            let position = CellPosition {
                row,
                column: source_column,
            };
            let visible_columns = &self.view(tab_id).grid_ui.visible_columns;
            let display_column = if visible_columns.is_empty() {
                Some(source_column)
            } else {
                visible_columns
                    .iter()
                    .position(|index| *index == source_column)
            };
            let pending = display_column.and_then(|index| {
                runtime
                    .pending_edits
                    .iter()
                    .find(|edit| edit.row == row && edit.column == index)
            });
            // NULL is the cell's null flag, never its text: a stored string
            // `null` is cloned as that string.
            let value = if let Some(pending) = pending {
                pending.request.value.clone()
            } else if grid.cell_is_null(position) {
                MutationValue::Null
            } else {
                let full = grid
                    .full_cell_text(position)
                    .ok_or("Full cell value unavailable")?;
                if let Some(reason) = self
                    .sql_column_read_only_reason(tab_id, source_column)
                    .or_else(|| self.sql_cell_read_only_reason(tab_id, row, source_column))
                {
                    return Err(format!("{}: {reason}", column.name));
                }
                mutation_value_from_cell(
                    &full,
                    &self.sql_edit_data_type(tab_id, source_column, row),
                )
                .map_err(|error| format!("{}: {error}", column.name))?
            };
            if matches!(value, MutationValue::Null) && !column.nullable {
                return Err(format!("{} is nonnullable", column.name));
            }
            values.push((column.name.clone(), value));
        }
        let request = InsertRowRequest {
            database: tab.database.clone(),
            schema: self.sql_row_schema(tab),
            table: tab.collection.clone(),
            values,
        };
        Ok(if missing.is_empty() {
            CloneSqlRow::Ready(request)
        } else {
            CloneSqlRow::NeedsValues { request, missing }
        })
    }

    pub(in crate::app) fn stage_sql_insert(
        &mut self,
        request: InsertRowRequest,
        tab_id: u64,
        cx: &mut Context<Self>,
    ) {
        if self.refuse_read_only_edit(tab_id, cx) {
            return;
        }
        if let Some(runtime) = self
            .workspace
            .runtime_mut(tab_id)
            .filter(|runtime| !runtime.sql_apply_running)
        {
            runtime
                .pending_sql_rows
                .push(crate::workspace::PendingSqlRow::Insert(request));
            self.notify_tab(tab_id, cx);
        }
    }

    pub(in crate::app) fn stage_sql_delete(
        &mut self,
        prepared: PreparedSqlDelete,
        row: usize,
        tab_id: u64,
        cx: &mut Context<Self>,
    ) {
        if self.refuse_read_only_edit(tab_id, cx) {
            return;
        }
        // The grid must still show the row the dialog was opened on, and no
        // query may be about to replace it.
        if self.sql_row_key(tab_id, row).as_ref() != Ok(&prepared.key) {
            self.view_mut(tab_id).detail_ui.error = Some(SqlEditError::StaleRow.to_string());
            self.notify_tab(tab_id, cx);
            return;
        }
        let PreparedSqlDelete { request, key } = prepared;
        if let Some(runtime) = self
            .workspace
            .runtime_mut(tab_id)
            .filter(|runtime| !runtime.sql_apply_running && runtime.query.can_run())
        {
            if runtime.pending_deleted_rows.insert(row) {
                runtime
                    .pending_sql_rows
                    .push(crate::workspace::PendingSqlRow::Delete { row, request, key });
                let removed = runtime
                    .pending_edits
                    .iter()
                    .filter(|edit| edit.row == row)
                    .map(|edit| CellPosition {
                        row,
                        column: edit.column,
                    })
                    .collect::<Vec<_>>();
                runtime.pending_edits.retain(|edit| edit.row != row);
                for position in removed {
                    runtime.remove_pending_cell_text(position);
                }
                self.notify_tab(tab_id, cx);
            }
        }
    }
    pub(in crate::app) fn confirm_delete_selected_row(
        &mut self,
        tab_id: u64,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(tab) = self.workspace.tab(tab_id).cloned() else {
            return;
        };
        let Some(row) = self
            .view(tab_id)
            .grid_ui
            .selection
            .borrow()
            .range
            .map(|range| range.normalized().start.row)
        else {
            return;
        };
        if tab.query_mode.is_mongo_find() {
            self.confirm_delete_mongo_row(tab, row, window, cx);
            return;
        }
        let request = match self.prepare_sql_delete(tab_id, row) {
            Ok(request) => request,
            Err(error) => {
                let error = error.to_string();
                window.open_alert_dialog(cx, move |dialog, _, _| {
                    dialog.title("Cannot stage deletion").child(error.clone())
                });
                return;
            }
        };
        let root = cx.entity().downgrade();
        window.open_alert_dialog(cx, move |dialog, _, _| {
            let root = root.clone();
            let request = request.clone();
            dialog
                .title(format!("Stage deletion of row {}?", row + 1))
                .child("The row is not deleted until you confirm Apply.")
                .button_props(
                    gpui_component::dialog::DialogButtonProps::default().ok_text("Delete"),
                )
                .confirm()
                .on_ok(move |_, _, cx| {
                    let request = request.clone();
                    let _ = root.update(cx, |this, cx| {
                        this.stage_sql_delete(request, row, tab_id, cx);
                    });
                    true
                })
        });
        self.notify_tab(tab_id, cx);
    }

    fn confirm_delete_mongo_row(
        &mut self,
        tab: Tab,
        row: usize,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Ok(row_fields) = self.mongo_row_fields(tab.id, row) else {
            return;
        };
        // A row the page has no `_id` for cannot be addressed: say so instead
        // of staging a delete that could hit nothing, or something else.
        if row_fields.mutation_identity.is_null() {
            let message = super::mongo::MongoEditRefusal::NoIdentity.to_string();
            window.open_alert_dialog(cx, move |dialog, _, _| {
                dialog.title("Cannot stage deletion").child(message.clone())
            });
            return;
        }
        let root = cx.entity().downgrade();
        let database = tab.database.clone();
        let collection = tab.collection.clone();
        let identity = row_fields.mutation_identity.clone();
        window.open_alert_dialog(cx, move |dialog, _, _| {
            let root = root.clone();
            let database = database.clone();
            let collection = collection.clone();
            let identity = identity.clone();
            dialog
                .title(format!("Delete row {}?", row + 1))
                .child("This stages a deletion. Undo or Rollback can restore the row until Commit.")
                .button_props(
                    gpui_component::dialog::DialogButtonProps::default().ok_text("Delete"),
                )
                .confirm()
                .footer(
                    gpui_component::dialog::DialogFooter::new()
                        .child(
                            gpui_component::dialog::DialogClose::new()
                                .trigger(|button| button.label("Cancel")),
                        )
                        .child(
                            div()
                                .debug_selector(|| "mongo-delete-confirm".into())
                                .child(
                                    gpui_component::dialog::DialogAction::new()
                                        .child(Button::new("delete").label("Delete").primary()),
                                ),
                        ),
                )
                .on_ok(move |_, _, cx| {
                    let request = MutationRequest {
                        operation_id: crate::mongo_result::STAGED_OPERATION_ID,
                        kind: "delete".into(),
                        database: Some(database.clone()),
                        object: Some(collection.clone()),
                        identity: identity.clone(),
                        payload: serde_json::json!({}),
                    };
                    let _ = root.update(cx, |this, cx| {
                        this.stage_mongo_mutation(
                            tab.id,
                            PendingMongoMutation::Delete { request, row },
                            cx,
                        );
                    });
                    true
                })
        });
        cx.notify();
    }

    pub(in crate::app) fn open_insert_row_editor(
        &mut self,
        tab_id: u64,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(tab) = self.workspace.tab(tab_id).cloned() else {
            return;
        };
        if tab.query_mode.is_mongo_find() {
            self.open_mongo_insert_row_editor(tab, window, cx);
            return;
        }
        if !self.sql_sort_available(tab_id) {
            window.open_alert_dialog(cx, |dialog, _, _| {
                dialog
                    .title("Insert unavailable")
                    .child("A connected SQL table result is required to stage a row insert.")
            });
            return;
        }
        if self
            .workspace
            .runtime(tab.id)
            .is_some_and(|runtime| runtime.sql_apply_running)
        {
            window.open_alert_dialog(cx, |dialog, _, _| {
                dialog
                    .title("Insert unavailable")
                    .child("Wait for SQL Apply to finish.")
            });
            return;
        }
        let Some(columns) = self
            .workspace
            .runtime(tab.id)
            .map(|runtime| runtime.structure_columns.clone())
        else {
            return;
        };
        if columns.is_empty() {
            return;
        }
        self.open_sql_insert_row_editor_with_default(tab, columns, "{}".into(), None, window, cx);
    }

    pub(in crate::app) fn open_sql_clone_draft_editor(
        &mut self,
        tab_id: u64,
        request: InsertRowRequest,
        missing: Vec<String>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(tab) = self.workspace.tab(tab_id).cloned() else {
            return;
        };
        let Some(columns) = self
            .workspace
            .runtime(tab.id)
            .map(|runtime| runtime.structure_columns.clone())
        else {
            return;
        };
        let values = request
            .values
            .iter()
            .map(|(name, value)| {
                let json = match value {
                    MutationValue::Null => serde_json::Value::Null,
                    MutationValue::Text(value) | MutationValue::Number(value) => {
                        serde_json::Value::String(value.clone())
                    }
                    MutationValue::Boolean(value) => serde_json::Value::Bool(*value),
                };
                (name.clone(), json)
            })
            .collect::<serde_json::Map<_, _>>();
        let Ok(default_json) = serde_json::to_string_pretty(&values) else {
            return;
        };
        self.open_sql_insert_row_editor_with_default(
            tab,
            columns,
            default_json,
            Some((request, missing)),
            window,
            cx,
        );
    }

    fn open_sql_insert_row_editor_with_default(
        &mut self,
        tab: Tab,
        columns: Vec<crate::catalog::CatalogColumn>,
        default_json: String,
        clone_draft: Option<(InsertRowRequest, Vec<String>)>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let input = cx.new(|cx| {
            EditorState::new(window, cx)
                .language("json")
                .line_number(true)
                .default_value(default_json)
        });
        let root = cx.entity().downgrade();
        let tab_id = tab.id;
        let schema = self.sql_row_schema(&tab);
        let error = std::rc::Rc::new(std::cell::RefCell::new(None::<String>));
        window.open_alert_dialog(cx, move |dialog, window, _| {
            let root = root.clone();
            let input = input.clone();
            let columns = columns.clone();
            let tab = tab.clone();
            let schema = schema.clone();
            let clone_draft = clone_draft.clone();
            let error = error.clone();
            let message = error.borrow().clone();
            dialog
                .width((window.viewport_size().width - px(48.0)).min(px(640.0)))
                .title(if clone_draft.is_some() {
                    format!("Clone as new row in {}", tab.collection)
                } else {
                    format!("Stage insert into {}", tab.collection)
                })
                .when_some(clone_draft.as_ref(), |dialog, (_, missing)| {
                    dialog.child(format!("Enter a new primary key: {}", missing.join(", ")))
                })
                .child(
                    div()
                        .debug_selector(|| "document-editor".into())
                        .h(px(
                            (f32::from(window.viewport_size().height) - 260.0).clamp(160.0, 440.0)
                        ))
                        .child(Editor::new(&input).appearance(true).h_full()),
                )
                .when_some(message, |dialog, message| {
                    dialog.child(div().text_color(theme::danger()).child(message))
                })
                .button_props(
                    gpui_component::dialog::DialogButtonProps::default().ok_text("Stage Insert"),
                )
                .confirm()
                .on_ok(move |_, window, cx| {
                    let text = input.read(cx).value().to_string();
                    let result = if let Some((template, missing)) = clone_draft.as_ref() {
                        finish_sql_clone_draft(template.clone(), missing, &text, &columns)
                    } else {
                        insert_values_from_json(&text, &columns)
                            .map(|values| InsertRowRequest {
                                database: tab.database.clone(),
                                schema: schema.clone(),
                                table: tab.collection.clone(),
                                values,
                            })
                            .map_err(String::from)
                    };
                    let request = match result {
                        Ok(request) => request,
                        Err(message) => {
                            *error.borrow_mut() = Some(message);
                            window.refresh();
                            return false;
                        }
                    };
                    let _ = root.update(cx, |this, cx| this.stage_sql_insert(request, tab_id, cx));
                    true
                })
        });
        self.notify_tab(tab_id, cx);
    }

    pub(in crate::app) fn open_mongo_insert_row_editor(
        &mut self,
        tab: Tab,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.open_mongo_insert_row_editor_with_default(tab, "{}".into(), window, cx);
    }

    pub(in crate::app) fn open_mongo_duplicate_row_editor(
        &mut self,
        tab_id: u64,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(tab) = self.workspace.tab(tab_id).cloned() else {
            return;
        };
        let Some(row) = self
            .view(tab_id)
            .grid_ui
            .selection
            .borrow()
            .range
            .map(|range| range.normalized().start.row)
        else {
            return;
        };
        let Ok(row_fields) = self.mongo_row_fields(tab.id, row) else {
            return;
        };
        let Some(default_value) = super::mongo::mongo_duplicate_text(&row_fields.fields) else {
            return;
        };
        self.open_mongo_insert_row_editor_with_default(tab, default_value, window, cx);
    }

    fn open_mongo_insert_row_editor_with_default(
        &mut self,
        tab: Tab,
        default_value: String,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let input = cx.new(|cx| {
            EditorState::new(window, cx)
                .language("json")
                .line_number(true)
                .default_value(default_value)
        });
        let root = cx.entity().downgrade();
        let validation_error = std::rc::Rc::new(std::cell::RefCell::new(None::<String>));
        window.open_alert_dialog(cx, move |dialog, window, _| {
            let root = root.clone();
            let input = input.clone();
            let tab = tab.clone();
            let error = validation_error.clone();
            let message = error.borrow().clone();
            dialog
                .width((window.viewport_size().width - px(48.0)).min(px(640.0)))
                .title(format!("Insert into {}", tab.collection))
                .child(
                    div()
                        .debug_selector(|| "document-editor".into())
                        .h(px(
                            (f32::from(window.viewport_size().height) - 260.0).clamp(160.0, 440.0)
                        ))
                        .child(Editor::new(&input).appearance(true).h_full()),
                )
                .when_some(message, |this, message| {
                    this.child(
                        div()
                            .debug_selector(|| "document-editor-error".into())
                            .text_size(px(12.0))
                            .text_color(theme::danger())
                            .child(message),
                    )
                })
                .button_props(
                    gpui_component::dialog::DialogButtonProps::default().ok_text("Insert"),
                )
                .confirm()
                .on_ok(move |_, window, cx| {
                    // Shell syntax or Extended JSON; a plain number is typed like
                    // mongosh types it, and `Long('5')` / `ISODate('…')` / `{"$numberLong": "5"}`
                    // give a value its type. The request carries canonical
                    // Extended JSON, so no value changes type on the way.
                    let payload = match engine_mongo_script::shell_value::parse_document(
                        input.read(cx).value().as_ref(),
                        engine_mongo_script::shell_value::NumberTyping::Mongosh,
                    ) {
                        Ok(document) => bson::Bson::Document(document).into_canonical_extjson(),
                        Err(parse_error) => {
                            *error.borrow_mut() =
                                Some(format!("Enter a document {{ … }}: {parse_error}"));
                            window.refresh();
                            return false;
                        }
                    };
                    let document = payload.clone();
                    let request = MutationRequest {
                        operation_id: crate::mongo_result::STAGED_OPERATION_ID,
                        kind: "insert".into(),
                        database: Some(tab.database.clone()),
                        object: Some(tab.collection.clone()),
                        identity: serde_json::json!({}),
                        payload,
                    };
                    let _ = root.update(cx, |this, cx| {
                        this.stage_mongo_mutation(
                            tab.id,
                            PendingMongoMutation::Insert { request, document },
                            cx,
                        );
                    });
                    true
                })
        });
        cx.notify();
    }
}
