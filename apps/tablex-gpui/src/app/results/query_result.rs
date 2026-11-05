use super::super::*;
use crate::columns_panel::Presentation;

/// Why a finished run's result was not shown over staged SQL changes.
const STAGED_CHANGES_BLOCK_RESULT: &str =
    "The result was not shown: apply or discard staged SQL changes before running another query.";

impl AppRoot {
    /// How the byte strings of `tab_id`'s result read: Redis keeps its escaped
    /// text; every SQL result shows `0x` + hex, the form the drivers take back.
    pub(in crate::app) fn binary_display(&self, tab_id: u64) -> crate::grid_arrow::BinaryDisplay {
        if self
            .workspace
            .tab(tab_id)
            .is_some_and(|tab| tab.query_mode.is_redis())
        {
            crate::grid_arrow::BinaryDisplay::ByteString
        } else {
            crate::grid_arrow::BinaryDisplay::Hex
        }
    }

    /// Applies a finished query to the active tab only when the run is live.
    pub(in crate::app) fn apply_query_result(
        &mut self,
        tab_id: u64,
        run: crate::query::QueryRunId,
        result: Result<crate::query::QueryResultBatch, String>,
    ) {
        self.finish_query_timing(tab_id, run);
        // A new result must not replace the rows staged SQL changes belong to.
        let result = if result.is_ok()
            && self
                .workspace
                .runtime(tab_id)
                .is_some_and(|runtime| runtime.has_staged_sql_changes())
        {
            Err(STAGED_CHANGES_BLOCK_RESULT.to_string())
        } else {
            result
        };
        let applied = match &result {
            Ok(query_result) => {
                let rows = query_result.total_count.max(0) as usize;
                self.workspace
                    .runtime_mut(tab_id)
                    .map(|runtime| runtime.query.complete(run, rows))
                    .unwrap_or(false)
            }
            Err(error) => {
                let error = crate::sessions::redact_secrets(error);
                self.workspace
                    .runtime_mut(tab_id)
                    .map(|runtime| runtime.query.fail(run, error))
                    .unwrap_or(false)
            }
        };
        if let (true, Err(error)) = (applied, &result) {
            if let Some(runtime) = self.workspace.runtime_mut(tab_id) {
                runtime.abandon_sql_run();
            }
            self.report_query_failure(tab_id, error);
            self.note_connection_lost(tab_id, error);
        }
        self.finish_query_task(tab_id, run);
        if applied {
            if let Ok(query_result) = result {
                let page_rows = query_result
                    .batches
                    .iter()
                    .map(|batch| batch.num_rows())
                    .sum();
                let grid = GridState::new(GridContentSource::Arrow(
                    ArrowGridSource::new(query_result.batches)
                        .with_binary_display(self.binary_display(tab_id)),
                ));
                let is_mongo = self
                    .workspace
                    .tabs()
                    .iter()
                    .find(|tab| tab.id == tab_id)
                    .is_some_and(|tab| tab.query_mode.is_mongo_find());
                let columns = is_mongo.then(|| {
                    (0..grid.column_count())
                        .filter_map(|column| grid.column_name(column).map(str::to_string))
                        .collect::<Vec<_>>()
                });
                if let Some(runtime) = self.workspace.runtime_mut(tab_id) {
                    runtime.pagination.set_result_count(page_rows);
                    runtime.commit_sql_run();
                    runtime.grid = grid;
                    if let Some(columns) = columns {
                        runtime.mongo.reset_documents();
                        runtime.mongo.observe_result_columns(columns);
                    }
                }
                self.view_mut(tab_id)
                    .grid_scroll
                    .scroll_to_item_strict(0, gpui::ScrollStrategy::Top);
                self.reset_tab_view(tab_id);
            }
        }
    }

    pub(in crate::app) fn apply_fixture_mongo_query_result(
        &mut self,
        tab_id: u64,
        run: crate::query::QueryRunId,
        result: Result<crate::ui_fixture::FixtureMongoPage, String>,
    ) {
        let applied = match &result {
            Ok(page) => self
                .workspace
                .runtime_mut(tab_id)
                .map(|runtime| runtime.query.complete(run, page.page.row_count as usize))
                .unwrap_or(false),
            Err(error) => self
                .workspace
                .runtime_mut(tab_id)
                .map(|runtime| runtime.query.fail(run, error.clone()))
                .unwrap_or(false),
        };
        if let (true, Err(error)) = (applied, &result) {
            self.report_query_failure(tab_id, error);
            self.note_connection_lost(tab_id, error);
        }
        self.finish_query_task(tab_id, run);
        if !applied {
            return;
        }
        let Ok(page) = result else {
            return;
        };
        let columns = page.page.columns.clone();
        let row_count = page.page.row_count;
        let total_count = page.page.total_count;
        let total_is_exact = page.page.total_is_exact;
        let page_id = page.page.page_id;
        let grid = GridState::new(GridContentSource::Bson(BsonGridSource::new(
            page.lease,
            columns.clone(),
            row_count,
        )));
        if let Some(runtime) = self.workspace.runtime_mut(tab_id) {
            runtime.mongo.reset_documents();
            runtime
                .mongo
                .observe_result_columns(columns.iter().map(|column| column.name.clone()));
            runtime.shell = None;
            runtime.driver_page_id = Some(page_id);
            runtime.pagination.set_page_result_count(
                row_count as usize,
                total_count,
                total_is_exact,
            );
            runtime.grid = grid;
        }
        self.view_mut(tab_id)
            .grid_scroll
            .scroll_to_item_strict(0, gpui::ScrollStrategy::Top);
        self.reset_tab_view(tab_id);
    }

    pub(in crate::app) fn apply_driver_query_result(
        &mut self,
        tab_id: u64,
        run: crate::query::QueryRunId,
        connection: String,
        result: Result<
            (
                engine_driver_api::QueryPage,
                DriverMongoResult,
                DriverPageGuard,
            ),
            String,
        >,
    ) -> bool {
        self.finish_query_timing(tab_id, run);
        let incoming_page_id = result.as_ref().ok().map(|(page, _, _)| page.page_id);
        let old_page_id = self
            .workspace
            .runtime(tab_id)
            .and_then(|runtime| runtime.driver_page_id);
        let applied = match &result {
            Ok((page, _, _)) => self
                .workspace
                .runtime_mut(tab_id)
                .map(|runtime| runtime.query.complete(run, page.row_count as usize))
                .unwrap_or(false),
            Err(error) => self
                .workspace
                .runtime_mut(tab_id)
                .map(|runtime| {
                    runtime
                        .query
                        .fail(run, crate::sessions::redact_secrets(error))
                })
                .unwrap_or(false),
        };
        if let (true, Err(error)) = (applied, &result) {
            self.report_query_failure(tab_id, error);
            self.note_connection_lost(tab_id, error);
        }
        self.finish_query_task(tab_id, run);
        let Some(session) = self.driver_runtime.sessions.get(&connection).cloned() else {
            return false;
        };
        if !applied {
            return false;
        }
        if let Some(page_id) = superseded_driver_page(old_page_id, incoming_page_id) {
            release_driver_page_async(Arc::clone(&session), page_id);
        }
        if let Ok((page, payload, mut page_guard)) = result {
            let grid = match payload {
                DriverMongoResult::PageLease(page_lease) => {
                    GridState::new(GridContentSource::Bson(BsonGridSource::new(
                        page_lease,
                        page.columns.clone(),
                        page.row_count,
                    )))
                }
                DriverMongoResult::CellBatch(batch) => GridState::new(GridContentSource::Arrow(
                    ArrowGridSource::with_column_types(
                        vec![Arc::new(batch)],
                        page.columns.iter().map(|column| column.data_type.clone()),
                    ),
                )),
            };
            if let Some(runtime) = self.workspace.runtime_mut(tab_id) {
                runtime.mongo.reset_documents();
                runtime
                    .mongo
                    .observe_result_columns(page.columns.iter().map(|column| column.name.clone()));
                // A find replaces what a shell run showed.
                runtime.shell = None;
                runtime.driver_page_id = Some(page.page_id);
                runtime.pagination.set_page_result_count(
                    page.row_count as usize,
                    page.total_count,
                    page.total_is_exact,
                );
                runtime.grid = grid;
            }
            page_guard.disarm();
            self.view_mut(tab_id)
                .grid_scroll
                .scroll_to_item_strict(0, gpui::ScrollStrategy::Top);
            self.reset_tab_view(tab_id);
            true
        } else {
            false
        }
    }

    /// Applies a finished driver SQL run to its tab. `true` when it produced
    /// the tab's new result.
    pub(in crate::app) fn apply_driver_arrow_query_result(
        &mut self,
        tab_id: u64,
        run: crate::query::QueryRunId,
        connection: String,
        result: Result<
            (
                engine_driver_api::QueryPage,
                engine_driver_host::ImportedArrowBatch,
                DriverPageGuard,
            ),
            DriverQueryError,
        >,
    ) -> bool {
        self.finish_query_timing(tab_id, run);
        let incoming_page_id = result.as_ref().ok().map(|(page, _, _)| page.page_id);
        let old_page_id = self
            .workspace
            .runtime(tab_id)
            .and_then(|runtime| runtime.driver_page_id);
        let session = self.tab_session_for(tab_id, &connection);
        let staged = self
            .workspace
            .runtime(tab_id)
            .is_some_and(|runtime| runtime.has_staged_sql_changes());
        // A result with nowhere to be shown (the session was released while it
        // ran, or staged changes hold the grid) fails the run: it must not read
        // "Ready" over the old rows.
        let applied = match &result {
            Ok(_) if session.is_none() => {
                self.apply_driver_sql_failure(tab_id, run, &DriverQueryError::NotConnected)
            }
            Ok(_) if staged => self.reject_driver_result(tab_id, run, STAGED_CHANGES_BLOCK_RESULT),
            Ok((page, _, _)) => self
                .workspace
                .runtime_mut(tab_id)
                .map(|runtime| runtime.query.complete(run, page.row_count as usize))
                .unwrap_or(false),
            Err(error) => self.apply_driver_sql_failure(tab_id, run, error),
        };
        self.finish_query_task(tab_id, run);
        let shown = applied && session.is_some() && !staged;
        let (Some(session), true) = (session, shown) else {
            // Dropping the page returns it to the driver.
            drop(result);
            return false;
        };
        if let Some(page_id) = superseded_driver_page(old_page_id, incoming_page_id) {
            release_driver_page_async(Arc::clone(&session), page_id);
        }
        let Ok((page, imported, page_guard)) = result else {
            return false;
        };
        let source = ArrowGridSource::from_imported_page(imported)
            .with_driver_types(page.columns.iter().map(|column| column.data_type.clone()))
            .with_binary_display(self.binary_display(tab_id));
        let landed_sql = self
            .workspace
            .runtime(tab_id)
            .and_then(|runtime| runtime.pending_sql.clone());
        let landed_page = crate::script_run::ResultPage {
            page: 0,
            total: page.total_count,
            total_is_exact: page.total_is_exact,
            truncated: page.flags.truncated,
        };
        let grid = GridState::new(GridContentSource::Arrow(source.clone()));
        if let Some(runtime) = self.workspace.runtime_mut(tab_id) {
            runtime.driver_page_id = None;
            // The driver decides whether another page exists.
            runtime.pagination.set_driver_page(
                page.total_count,
                page.total_is_exact,
                page.flags.truncated,
            );
            runtime.commit_sql_run();
            runtime.grid = grid;
            // New rows replace a plan on screen; the plan stays behind its rail button.
            if runtime.columns_panel.presentation == Presentation::Plan {
                runtime.columns_panel.presentation = Presentation::Grid;
            }
        }
        // The page of a script's result tab (paging, Refresh) is kept; an editor run ends the script.
        let landed_page = crate::script_run::ResultPage {
            // The page the pagination has committed by now.
            page: self.workspace.runtime(tab_id).map_or(0, |runtime| runtime.pagination.page()),
            ..landed_page
        };
        self.single_page_landed(tab_id, landed_sql.as_deref(), &source, landed_page);
        self.view_mut(tab_id)
            .grid_scroll
            .scroll_to_item_strict(0, gpui::ScrollStrategy::Top);
        self.reset_tab_view(tab_id);
        drop(page_guard);
        true
    }

    /// Applies a finished Explain run to its tab: the plan becomes the Plan presentation and
    /// the rows, pagination and last executed statement the tab shows are left alone. A failure
    /// goes through the same paths as a failed Run (cancel, lost session, Error Log).
    pub(in crate::app) fn finish_explain_run(
        &mut self,
        run: crate::query::QueryRunId,
        request: SqlRun,
        explain: ExplainRun,
        outcome: Result<engine_plan::Plan, ExplainRunError>,
        cx: &mut Context<Self>,
    ) {
        if let Err(ExplainRunError::Query(error)) = &outcome {
            if let Some(reason) = error.confirmation_reason() {
                let reason = reason.to_string();
                self.await_sql_confirmation(run, request, reason, cx);
                return;
            }
        }
        let tab_id = request.tab_id;
        // A transaction of the user's is open (MySQL / MariaDB cannot run read-only inside it): an
        // Estimate executes nothing, so it runs once more without the read-only request; the run
        // stays the tab's live run. An Analyze is never run unguarded (said below).
        if let Err(ExplainRunError::Query(error)) = &outcome {
            if let Some(retry) = explain_retry(error, &request) {
                let live = self.workspace.runtime(tab_id).is_some_and(|runtime| {
                    matches!(
                        runtime.query.status(),
                        crate::query::QueryStatus::Running { run: current } if *current == run
                    )
                });
                if live {
                    self.finish_query_timing(tab_id, run);
                    self.finish_query_task(tab_id, run);
                    self.start_driver_sql_run(run, retry, cx);
                    return;
                }
            }
        }
        self.finish_query_timing(tab_id, run);
        let applied = match &outcome {
            Ok(plan) => self
                .workspace
                .runtime_mut(tab_id)
                .is_some_and(|runtime| runtime.query.complete(run, plan.nodes.len())),
            // The server ran the Analyze read-only and the statement tried to write: nothing was
            // changed, and the reason is said plainly.
            Err(ExplainRunError::Query(error)) if error.is_read_only_violation() => self.reject_driver_result(
                tab_id,
                run,
                explain_violation_message(explain.mode),
            ),
            Err(ExplainRunError::Query(error)) if error.is_read_only_unavailable() => self.reject_driver_result(
                tab_id,
                run,
                match explain.mode {
                    engine_plan::ExplainMode::Analyze => {
                        "Explain Analyze needs a read-only run; finish the open transaction first"
                    }
                    engine_plan::ExplainMode::Estimate => {
                        "This connection is read-only, so Explain needs a read-only run; finish the open transaction first"
                    }
                },
            ),
            Err(ExplainRunError::Query(error)) => self.apply_driver_sql_failure(tab_id, run, error),
            Err(error) => self.reject_driver_result(tab_id, run, &error.to_string()),
        };
        self.finish_query_task(tab_id, run);
        let (true, Ok(plan)) = (applied, outcome) else {
            return;
        };
        if let Some(runtime) = self.workspace.runtime_mut(tab_id) {
            runtime.plan = Some(crate::plan_view::PlanView::new(
                plan,
                explain.dialect,
                explain.mode,
                request.sql,
            ));
            runtime.columns_panel.presentation = Presentation::Plan;
        }
        self.view_mut(tab_id)
            .plan_scroll
            .scroll_to_item_strict(0, gpui::ScrollStrategy::Top);
        self.notify_tab(tab_id, cx);
    }

    /// Fails the live run because its result cannot be shown. `true` when the
    /// run was the live one.
    fn reject_driver_result(
        &mut self,
        tab_id: u64,
        run: crate::query::QueryRunId,
        reason: &str,
    ) -> bool {
        let failed = self.workspace.runtime_mut(tab_id).is_some_and(|runtime| {
            let failed = runtime.query.fail(run, reason);
            if failed {
                runtime.abandon_sql_run();
            }
            failed
        });
        if failed {
            self.report_query_failure(tab_id, reason);
        }
        failed
    }

    /// A driver SQL run failed. `true` when the failure ended the tab's live
    /// run. A user cancel is a status, not an error; a refusal to page keeps
    /// the shown result (a notice, not an error); a lost session flips the
    /// connection to its reconnect state; everything else is an error with its
    /// driver code.
    fn apply_driver_sql_failure(
        &mut self,
        tab_id: u64,
        run: crate::query::QueryRunId,
        error: &DriverQueryError,
    ) -> bool {
        let text = error.to_string();
        // A write the driver refused in its read-only run (it got past the app's own check).
        let read_only_notice = error
            .is_read_only_violation()
            .then(|| self.read_only_message(tab_id, "This statement is"))
            .flatten();
        let Some(runtime) = self.workspace.runtime_mut(tab_id) else {
            return false;
        };
        let live = matches!(
            runtime.query.status(),
            crate::query::QueryStatus::Running { run: current } if *current == run
        );
        if !live {
            return false;
        }
        if error.is_cancelled() {
            runtime.abandon_sql_run();
            return runtime.query.cancel().is_some();
        }
        if error.is_not_pageable() {
            runtime.query.withdraw(run);
            runtime.abandon_sql_run();
            runtime.pagination.stop_paging();
            let connection = self
                .workspace
                .tab(tab_id)
                .map(|tab| tab.connection_id.clone());
            self.report_warning(Area::Query, connection.as_deref(), text);
            return false;
        }
        runtime.abandon_sql_run();
        let shown = read_only_notice.unwrap_or_else(|| crate::sessions::redact_secrets(&error.status_text()));
        runtime.query.fail(run, shown);
        self.report_query_failure(tab_id, &text);
        self.note_driver_connection_lost(tab_id, error);
        true
    }
}

/// The request to send again after an Explain failed with `query.read_only_unavailable`: the same
/// Estimate without the read-only request, once. `None` for any other failure, for an Analyze, for
/// a request that already dropped it, and on a read-only connection (`request.read_only`): there
/// the read-only run is the guarantee, and a statement the app's lexer misread could write.
pub(in crate::app) fn explain_retry(error: &DriverQueryError, request: &SqlRun) -> Option<SqlRun> {
    let RunKind::Explain(explain) = request.kind else {
        return None;
    };
    if !error.is_read_only_unavailable() || request.read_only {
        return None;
    }
    Some(SqlRun {
        kind: RunKind::Explain(explain.without_read_only()?),
        read_only: false,
        ..request.clone()
    })
}

/// What the tab says when the server refused the Explain's statement in its read-only run.
pub(in crate::app) fn explain_violation_message(mode: engine_plan::ExplainMode) -> &'static str {
    match mode {
        engine_plan::ExplainMode::Analyze => {
            "Explain Analyze refused: the statement writes (it ran read-only, nothing was changed)"
        }
        engine_plan::ExplainMode::Estimate => {
            "Explain refused: the statement writes (it ran read-only, nothing was changed)"
        }
    }
}
