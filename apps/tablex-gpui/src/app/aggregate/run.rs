//! Run, Explain, cancel, the write-stage confirmation and Export Code.
use super::*;
use crate::mongo_aggregate::ExplainVerbosity;

/// State of the Explain dialog of one tab.
pub(in crate::app) struct ExplainUi {
    /// Identifies this explain: a late answer or a closing dialog of an older
    /// one must not touch a newer explain's operation.
    id: u64,
    operation: Option<DriverQueryOperation>,
    /// The request has not answered yet.
    in_flight: bool,
}

impl ExplainUi {
    #[cfg(test)]
    pub(in crate::app) fn in_flight(&self) -> bool {
        self.in_flight
    }

    #[cfg(test)]
    pub(in crate::app) fn id(&self) -> u64 {
        self.id
    }
}

/// Cancels the request behind an Explain dialog (tab closed / dialog closed).
pub(in crate::app) fn cancel_explain(explain: ExplainUi, cx: &mut gpui::App) {
    if let Some(operation) = explain.operation {
        drivers::cancel_driver_operation(operation, cx);
    }
}

/// Content of the Explain dialog: the server's plan in a read-only JSON
/// editor (relaxed Extended JSON).
pub(in crate::app) struct ExplainDialog {
    pub(in crate::app) verbosity: ExplainVerbosity,
    pub(in crate::app) editor: gpui::Entity<EditorState>,
    pub(in crate::app) status: ExplainStatus,
}

#[derive(Clone, Debug, PartialEq)]
pub(in crate::app) enum ExplainStatus {
    Loading,
    Ready,
    Failed(String),
}

impl Render for ExplainDialog {
    fn render(&mut self, _window: &mut Window, _cx: &mut Context<Self>) -> impl IntoElement {
        let body = match &self.status {
            ExplainStatus::Loading => div()
                .debug_selector(|| "agg-explain-loading".into())
                .p(px(16.0))
                .text_color(theme::text_muted())
                .child("Running explain…")
                .into_any_element(),
            ExplainStatus::Failed(message) => div()
                .debug_selector(|| "agg-explain-error".into())
                .p(px(16.0))
                .text_color(theme::danger())
                .child(message.clone())
                .into_any_element(),
            ExplainStatus::Ready => div()
                .debug_selector(|| "agg-explain-json".into())
                .h(px(420.0))
                .font_family("Menlo")
                .text_size(px(12.0))
                .child(
                    Editor::new(&self.editor)
                        .readonly(true)
                        .appearance(false)
                        .bordered(false)
                        .h_full(),
                )
                .into_any_element(),
        };
        div()
            .flex()
            .flex_col()
            .gap(px(6.0))
            .child(
                div()
                    .text_size(px(11.0))
                    .text_color(theme::text_muted())
                    .child(format!("Verbosity: {}", self.verbosity.as_str())),
            )
            .child(body)
    }
}

impl AppRoot {
    /// Run button: validates, asks for confirmation when the pipeline ends in
    /// `$out` / `$merge`, then runs the full pipeline into the grid.
    pub(in crate::app) fn run_aggregate(
        &mut self,
        tab_id: u64,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(draft) = self.aggregate_draft(tab_id).cloned() else {
            return;
        };
        if let Some(message) = aggregate_run_blocker(&draft) {
            self.aggregate_ui.tab_mut(tab_id).notice = Some(message);
            cx.notify();
            return;
        }
        if let Some(write) = draft.final_write() {
            if let Some(message) = self.read_only_message(tab_id, "A pipeline that writes ($out / $merge) is") {
                self.aggregate_ui.tab_mut(tab_id).notice = Some(message);
                cx.notify();
                return;
            }
            self.open_aggregate_write_confirm(tab_id, write, window, cx);
            return;
        }
        self.run_aggregate_confirmed(tab_id, false, cx);
    }

    /// Starts a run from page 1 (`confirm_write` only after the dialog).
    pub(in crate::app) fn run_aggregate_confirmed(
        &mut self,
        tab_id: u64,
        confirm_write: bool,
        cx: &mut Context<Self>,
    ) {
        if let Some(runtime) = self.workspace.runtime_mut(tab_id) {
            runtime.pagination.reset();
        }
        self.aggregate_ui.tab_mut(tab_id).notice = None;
        self.run_aggregate_page(tab_id, confirm_write, cx);
    }

    /// Paging / Refresh of an Aggregate tab. A pipeline that writes is never
    /// re-run implicitly: only the Run button (with its confirmation) may.
    pub(in crate::app) fn rerun_aggregate_page(&mut self, tab_id: u64, cx: &mut Context<Self>) {
        let Some(draft) = self.aggregate_draft(tab_id) else {
            return;
        };
        if draft.final_write().is_some() {
            self.aggregate_ui.tab_mut(tab_id).notice =
                Some("This pipeline writes data: use Run to run it again.".into());
            cx.notify();
            return;
        }
        self.run_aggregate_page(tab_id, false, cx);
    }

    fn run_aggregate_page(&mut self, tab_id: u64, confirm_write: bool, cx: &mut Context<Self>) {
        let Some(draft) = self.aggregate_draft(tab_id) else {
            return;
        };
        let payload = draft.run_payload(confirm_write);
        let Some((connection, database, collection)) = self.workspace.tab(tab_id).map(|tab| {
            (
                tab.connection_id.clone(),
                tab.database.clone(),
                tab.collection.clone(),
            )
        }) else {
            return;
        };
        let started = Instant::now();
        if self.scenario_ui.service.is_some() {
            self.run_scenario_aggregate(tab_id, &connection, &database, &collection, &payload);
            self.aggregate_ui.tab_mut(tab_id).run_ms =
                u64::try_from(started.elapsed().as_millis()).ok();
            cx.notify();
            return;
        }
        if !self.driver_runtime.sessions.contains_key(&connection) {
            self.fail_mongo_script(tab_id, "MongoDB connection is not connected", cx);
            return;
        }
        self.run_active_mongo_request(
            tab_id,
            connection,
            database,
            collection,
            (String::new(), String::new(), String::new(), None),
            Some(payload),
            None,
            cx,
        );
    }

    fn run_scenario_aggregate(
        &mut self,
        tab_id: u64,
        connection: &str,
        database: &str,
        collection: &str,
        payload: &serde_json::Value,
    ) {
        let Some((offset, limit, run)) = self.workspace.runtime_mut(tab_id).and_then(|runtime| {
            let run = runtime.query.begin()?;
            Some((runtime.pagination.offset().max(0) as u64, runtime.pagination.page_size(), run))
        }) else {
            return;
        };
        let Some(service) = self.scenario_ui.service.as_mut() else {
            return;
        };
        let result =
            service.query_mongo_aggregate(connection, database, collection, false, payload, offset, limit);
        self.apply_fixture_mongo_query_result(tab_id, run, result);
    }

    /// Confirmation before a pipeline that ends in `$out` / `$merge` runs.
    pub(in crate::app) fn open_aggregate_write_confirm(
        &mut self,
        tab_id: u64,
        write: engine::WriteStage,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let root = cx.entity().downgrade();
        let database = self
            .workspace
            .tab(tab_id)
            .map(|tab| tab.database.clone())
            .unwrap_or_default();
        window.open_alert_dialog(cx, move |dialog, _, _| {
            let root = root.clone();
            let text = format!(
                "This pipeline ends with {} and writes its output to \"{}\" in \"{}\". \
                 Existing documents in the target can be replaced or removed. \
                 Previews and Explain never run this stage.",
                write.operator, write.target, database
            );
            dialog
                .title("Run pipeline that writes data?")
                .child(div().debug_selector(|| "agg-write-warning".into()).child(text))
                .button_props(
                    gpui_component::dialog::DialogButtonProps::default().ok_text("Run and write"),
                )
                .confirm()
                .footer(
                    gpui_component::dialog::DialogFooter::new()
                        .child(
                            div().debug_selector(|| "agg-write-cancel".into()).child(
                                gpui_component::dialog::DialogClose::new()
                                    .trigger(|button| button.label("Cancel")),
                            ),
                        )
                        .child(
                            div().debug_selector(|| "agg-write-confirm".into()).child(
                                gpui_component::dialog::DialogAction::new().child(
                                    Button::new("agg-write-run").label("Run and write").danger(),
                                ),
                            ),
                        ),
                )
                .on_ok(move |_, _, cx| {
                    let _ = root.update(cx, |this, cx| {
                        this.run_aggregate_confirmed(tab_id, true, cx);
                    });
                    true
                })
        });
        cx.notify();
    }

    // ---- explain ------------------------------------------------------------

    pub(in crate::app) fn explain_aggregate(
        &mut self,
        tab_id: u64,
        verbosity: ExplainVerbosity,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(draft) = self.aggregate_draft(tab_id).cloned() else {
            return;
        };
        if let Some(message) = aggregate_run_blocker(&draft) {
            self.aggregate_ui.tab_mut(tab_id).notice = Some(message);
            cx.notify();
            return;
        }
        let Some((connection, database, collection)) = self.workspace.tab(tab_id).map(|tab| {
            (
                tab.connection_id.clone(),
                tab.database.clone(),
                tab.collection.clone(),
            )
        }) else {
            return;
        };
        let explain_id = {
            let ui = self.aggregate_ui.tab_mut(tab_id);
            ui.next_explain += 1;
            ui.next_explain
        };
        // A previous explain of this tab is superseded.
        if let Some(previous) = self.aggregate_ui.tab_mut(tab_id).explain.take() {
            cancel_explain(previous, cx);
        }
        let editor = cx.new(|cx| {
            EditorState::new(window, cx)
                .language("json")
                .line_number(true)
                .folding(true)
                .soft_wrap(false)
                .default_value(String::new())
        });
        let view = cx.new(|_| ExplainDialog {
            verbosity,
            editor,
            status: ExplainStatus::Loading,
        });
        let dialog_view = view.clone();
        let root = cx.entity().downgrade();
        window.open_dialog(cx, move |dialog, _, _| {
            let root = root.clone();
            dialog
                .title("Explain Plan")
                .w(px(780.0))
                .child(dialog_view.clone())
                .footer(
                    gpui_component::dialog::DialogFooter::new().child(
                        div().debug_selector(|| "agg-explain-close".into()).child(
                            gpui_component::dialog::DialogClose::new()
                                .trigger(|button| button.label("Close")),
                        ),
                    ),
                )
                .on_close(move |_, _, cx| {
                    let _ = root.update(cx, |this, cx| {
                        let ui = this.aggregate_ui.tab_mut(tab_id);
                        // Only this dialog's own explain (a newer one keeps running).
                        if ui.explain.as_ref().is_some_and(|explain| explain.id == explain_id) {
                            if let Some(explain) = ui.explain.take() {
                                cancel_explain(explain, cx);
                            }
                        }
                    });
                })
        });
        let call = AggregateCall {
            connection,
            database,
            collection,
            explain: true,
            payload: draft.explain_payload(verbosity),
        };
        match self.dispatch_aggregate_call(call, cx) {
            DispatchedAggregate::Done(result) => {
                self.aggregate_ui.tab_mut(tab_id).explain = Some(ExplainUi { id: explain_id, operation: None, in_flight: false });
                finish_explain(&view, result, window, cx);
            }
            DispatchedAggregate::Failed(message) => {
                self.report_query_failure(tab_id, &message);
                finish_explain(&view, Err(message), window, cx);
            }
            DispatchedAggregate::Pending { operation, task } => {
                self.aggregate_ui.tab_mut(tab_id).explain = Some(ExplainUi { id: explain_id, operation, in_flight: true });
                let window_handle = window.window_handle();
                cx.spawn(async move |this, cx| {
                    let result = task.await;
                    let _ = this.update(cx, |this, cx| {
                        if let Err(message) = &result {
                            this.report_query_failure(tab_id, message);
                        }
                        if let Some(explain) = this
                            .aggregate_ui
                            .tab_mut(tab_id)
                            .explain
                            .as_mut()
                            .filter(|explain| explain.id == explain_id)
                        {
                            explain.operation = None;
                            explain.in_flight = false;
                        }
                        let _ = cx.update_window(window_handle, |_, window, cx| {
                            finish_explain(&view, result, window, cx);
                        });
                    });
                })
                .detach();
            }
        }
    }

    // ---- export data ----------------------------------------------------------

    /// Export Data: the whole result through one aggregate cursor into a file
    /// (`csv`, `json` or `xlsx`). A pipeline that writes is refused; every
    /// failure is shown in a dialog.
    pub(in crate::app) fn export_aggregate_data(
        &mut self,
        tab_id: u64,
        format: &'static str,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(draft) = self.aggregate_draft(tab_id).cloned() else {
            return;
        };
        if self.aggregate_ui.tab_mut(tab_id).exporting {
            self.aggregate_ui.tab_mut(tab_id).notice = Some("An export is already running.".into());
            cx.notify();
            return;
        }
        let refusal = aggregate_run_blocker(&draft).or_else(|| {
            draft.final_write().map(|write| {
                format!(
                    "Export Data cannot run a pipeline that writes ({} to {}).",
                    write.operator, write.target
                )
            })
        });
        if let Some(message) = refusal {
            self.aggregate_ui.tab_mut(tab_id).notice = Some(message.clone());
            window.open_alert_dialog(cx, move |dialog, _, _| {
                dialog.title("Export failed").child(message.clone())
            });
            cx.notify();
            return;
        }
        let Some((connection, database, collection)) = self.workspace.tab(tab_id).map(|tab| {
            (
                tab.connection_id.clone(),
                tab.database.clone(),
                tab.collection.clone(),
            )
        }) else {
            return;
        };
        let destination = match aggregate_export_path(
            self.scenario_ui.service.is_some(),
            &collection,
            draft.name.as_deref(),
            format,
        ) {
            Ok(path) => path,
            Err(error) => {
                let message = error.to_string();
                window.open_alert_dialog(cx, move |dialog, _, _| {
                    dialog.title("Export failed").child(message.clone())
                });
                return;
            }
        };
        let payload = draft.run_payload(false);
        if self.scenario_ui.service.is_some() {
            let result = self.scenario_ui.service.as_mut().map(|service| {
                service.fixture_mut().export_mongo_aggregate(
                    &connection,
                    &database,
                    &collection,
                    &payload,
                    format,
                    &destination,
                )
            });
            if let Some(result) = result {
                self.finish_aggregate_export(tab_id, result, destination, window, cx);
            }
            return;
        }
        if !self
            .driver_runtime
            .capabilities
            .get(&connection)
            .is_some_and(|capabilities| capabilities.has(Capability::ExportMongoAggregate))
        {
            let message = "This MongoDB driver version cannot export aggregation results.".to_string();
            window.open_alert_dialog(cx, move |dialog, _, _| {
                dialog.title("Export failed").child(message.clone())
            });
            return;
        }
        let prepared = match self.begin_driver_query(&connection) {
            Ok(prepared) => prepared,
            Err(error) => {
                let message = error.to_string();
                window.open_alert_dialog(cx, move |dialog, _, _| {
                    dialog.title("Export failed").child(message.clone())
                });
                return;
            }
        };
        let drivers::PreparedDriverQuery {
            runner, operation, ..
        } = prepared;
        let request = engine_driver_api::ExportRequest {
            operation_id: operation.operation_id,
            format: format.into(),
            destination: destination.to_string_lossy().into_owned(),
            page_id: None,
            query: Some(QueryRequest {
                kind: "mongo_aggregate".into(),
                operation_id: operation.operation_id,
                database: Some(database),
                object: Some(collection),
                offset: 0,
                limit: 1,
                payload,
            }),
            options: serde_json::Value::Null,
        };
        {
            let ui = self.aggregate_ui.tab_mut(tab_id);
            ui.exporting = true;
            ui.export_operation = Some(operation);
            ui.notice = Some("Exporting…".into());
        }
        let task = cx.background_executor().spawn(async move {
            smol::unblock(move || runner.export(&request).map_err(|error| error.to_string())).await
        });
        let window_handle = window.window_handle();
        cx.spawn(async move |this, cx| {
            let result = task.await.map(|summary| (summary.rows_written, summary.bytes_written));
            let _ = this.update(cx, |this, cx| {
                let result = this.note_aggregate_export_done(tab_id, result);
                cx.notify();
                result
            })
            .map(|result| {
                let _ = cx.update_window(window_handle, |_, window, cx| {
                    export_result_dialog(result, destination, window, cx);
                });
            });
        })
        .detach();
        cx.notify();
    }

    fn finish_aggregate_export(
        &mut self,
        tab_id: u64,
        result: Result<(u64, u64), String>,
        destination: std::path::PathBuf,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let result = self.note_aggregate_export_done(tab_id, result);
        export_result_dialog(result, destination, window, cx);
        cx.notify();
    }

    /// Clears the "exporting" state; a failure also leaves a header notice.
    fn note_aggregate_export_done(
        &mut self,
        tab_id: u64,
        result: Result<(u64, u64), String>,
    ) -> Result<(u64, u64), String> {
        if let Some(ui) = self.aggregate_ui.tabs.get_mut(&tab_id) {
            ui.exporting = false;
            ui.export_operation = None;
            ui.notice = None;
        }
        result.map_err(|message| {
            let message = crate::sessions::redact_secrets(&message);
            self.report_error(Area::Export, None, format!("Export failed: {message}"));
            self.aggregate_ui.tab_mut(tab_id).notice = Some(format!("Export failed: {message}"));
            message
        })
    }

    // ---- export code ----------------------------------------------------------

    /// Copies the pipeline as `db.coll.aggregate([...])` (mongosh).
    pub(in crate::app) fn export_aggregate_code(&mut self, tab_id: u64, cx: &mut Context<Self>) {
        let collection = self
            .workspace
            .tab(tab_id)
            .map(|tab| tab.collection.clone())
            .unwrap_or_default();
        let Some(draft) = self.aggregate_draft(tab_id) else {
            return;
        };
        let notice = match draft.mongosh_code(&collection) {
            Ok(code) => {
                cx.write_to_clipboard(gpui::ClipboardItem::new_string(code));
                "Copied mongosh code to the clipboard".to_string()
            }
            Err(error) => format!("Cannot export code: {error}"),
        };
        self.aggregate_ui.tab_mut(tab_id).notice = Some(notice);
        cx.notify();
    }
}

fn finish_explain(
    view: &gpui::Entity<ExplainDialog>,
    result: Result<AggregateDocs, String>,
    window: &mut Window,
    cx: &mut gpui::App,
) {
    let (status, text) = match result {
        Ok(output) => match output.documents.into_iter().next() {
            Some(document) => (
                ExplainStatus::Ready,
                serde_json::to_string_pretty(&bson::Bson::Document(document).into_relaxed_extjson())
                    .unwrap_or_default(),
            ),
            None => (ExplainStatus::Failed("The server returned no plan.".into()), String::new()),
        },
        Err(message) => (ExplainStatus::Failed(crate::sessions::redact_secrets(&message)), String::new()),
    };
    let editor = view.read(cx).editor.clone();
    editor.update(cx, |state, cx| state.set_value(text, window, cx));
    view.update(cx, |dialog, cx| {
        dialog.status = status;
        cx.notify();
    });
}

fn export_result_dialog(
    result: Result<(u64, u64), String>,
    destination: std::path::PathBuf,
    window: &mut Window,
    cx: &mut gpui::App,
) {
    match result {
        Ok((rows, _bytes)) => window.open_alert_dialog(cx, move |dialog, _, _| {
            dialog.title("Export complete").child(
                div()
                    .debug_selector(|| "agg-export-done".into())
                    .child(format!("Exported {rows} rows to {}", destination.display())),
            )
        }),
        Err(message) => window.open_alert_dialog(cx, move |dialog, _, _| {
            dialog.title("Export failed").child(
                div()
                    .debug_selector(|| "agg-export-error".into())
                    .child(message.clone()),
            )
        }),
    }
}

/// `~/Downloads/TableX/<collection>-<name>-<id>.<ext>` (the folder the other
/// exports use); the scenario fixture writes into the temp dir instead.
fn aggregate_export_path(
    scenario: bool,
    collection: &str,
    name: Option<&str>,
    extension: &str,
) -> Result<std::path::PathBuf, String> {
    let safe = |text: &str| {
        text.chars()
            .map(|ch| if ch.is_ascii_alphanumeric() || ch == '_' || ch == '-' { ch } else { '_' })
            .collect::<String>()
    };
    let stem = match name {
        Some(name) => format!("{}-{}", safe(collection), safe(name)),
        None => safe(collection),
    };
    let dir = if scenario {
        std::env::temp_dir().join("tablex-aggregate-exports")
    } else {
        crate::platform::exports_dir().ok_or("Downloads directory unavailable")?
    };
    Ok(dir.join(format!("{stem}-{}.{extension}", uuid::Uuid::new_v4())))
}

/// Why Run / Explain cannot start: invalid pipeline or options.
pub(in crate::app) fn aggregate_run_blocker(draft: &AggregateDraft) -> Option<String> {
    let check = draft.check();
    if let Some(error) = check.text_error {
        return Some(format!("Fix the pipeline first: {error}"));
    }
    if let Some((_, message)) = check.stage_errors.first() {
        return Some(format!("Fix the stages first: {message}"));
    }
    draft.options.validate().err()
}
