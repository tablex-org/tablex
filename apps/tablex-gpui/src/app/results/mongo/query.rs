use super::super::super::*;

pub(in crate::app) const MONGO_PAGE_MAX: i64 = 200;

pub(in crate::app) fn normalize_mongo_page(
    workspace: &mut crate::workspace::WorkspaceState,
    tab_id: u64,
) -> Option<i64> {
    let tab = workspace.tab_mut(tab_id)?;
    if !tab.query_mode.is_mongo() {
        return None;
    }
    let size = tab.output_limit.clamp(1, MONGO_PAGE_MAX);
    tab.output_limit = size;
    if let Some(runtime) = workspace.runtime_mut(tab_id) {
        if runtime.pagination.page_size() != size {
            runtime.pagination = crate::query::PaginationState::new(size);
        }
    }
    Some(size)
}

#[allow(clippy::too_many_arguments)] // each parameter is one input of the operation; a parameter struct would only rename the list
pub(in crate::app) fn mongo_request(
    operation_id: OperationId,
    database: &str,
    collection: &str,
    offset: u64,
    limit: u64,
    filter: &str,
    sort: &str,
    project: &str,
    script: Option<&str>,
) -> QueryRequest {
    QueryRequest {
        kind: if script.is_some() {
            "mongo_script".into()
        } else {
            "mongo_find".into()
        },
        operation_id,
        database: Some(database.into()),
        object: Some(collection.into()),
        offset,
        limit: limit.max(1),
        payload: match script {
            Some(script) => serde_json::json!({"script":script}),
            None => mongo_query_payload(filter, sort, project),
        },
    }
}

/// `mongo_aggregate` full-run request (one page of the Run output).
pub(in crate::app) fn mongo_aggregate_request(
    operation_id: OperationId,
    database: &str,
    collection: &str,
    offset: u64,
    limit: u64,
    payload: serde_json::Value,
) -> QueryRequest {
    QueryRequest {
        kind: "mongo_aggregate".into(),
        operation_id,
        database: Some(database.into()),
        object: Some(collection.into()),
        offset,
        limit: limit.max(1),
        payload,
    }
}

impl AppRoot {
    #[allow(clippy::too_many_arguments)] // each parameter is one input of the operation; a parameter struct would only rename the list
    pub(in crate::app) fn run_active_mongo_query(
        &mut self,
        tab_id: u64,
        connection: String,
        database: String,
        collection: String,
        filter: String,
        sort: String,
        project: String,
        script: Option<String>,
        pending_run: Option<crate::query::QueryRunId>,
        cx: &mut Context<Self>,
    ) {
        self.run_active_mongo_request(
            tab_id,
            connection,
            database,
            collection,
            (filter, sort, project, script),
            None,
            pending_run,
            cx,
        );
    }

    #[allow(clippy::too_many_arguments)] // each parameter is one input of the operation; a parameter struct would only rename the list
    /// Runs one page of a Mongo tab through the loaded driver. `aggregate`
    /// carries the `mongo_aggregate` payload of an Aggregate tab; otherwise
    /// the find / script parts decide the request.
    pub(in crate::app) fn run_active_mongo_request(
        &mut self,
        tab_id: u64,
        connection: String,
        database: String,
        collection: String,
        (filter, sort, project, script): (String, String, String, Option<String>),
        aggregate: Option<serde_json::Value>,
        pending_run: Option<crate::query::QueryRunId>,
        cx: &mut Context<Self>,
    ) {
        let Some(limit) = normalize_mongo_page(&mut self.workspace, tab_id) else {
            return;
        };
        let (run, offset) = {
            let Some(runtime) = self.workspace.runtime_mut(tab_id) else {
                return;
            };
            let run = match pending_run {
                Some(run) if matches!(runtime.query.status(), crate::query::QueryStatus::Running { run: current } if *current == run) => {
                    Some(run)
                }
                Some(_) => None,
                None => runtime.query.begin(),
            };
            let Some(run) = run else {
                return;
            };
            (run, runtime.pagination.offset().max(0) as u64)
        };
        if !self.query_tasks.registry.register(tab_id, run) {
            if let Some(runtime) = self.workspace.runtime_mut(tab_id) {
                let _ = runtime.query.fail(run, "Query task already exists");
            }
            self.report_query_failure(tab_id, "Query task already exists");
            return;
        }
        self.query_tasks
            .started_at
            .insert(tab_id, (run, Instant::now()));
        // Lock-free and host-allocated: the UI thread never waits for the
        // session lock, and the operation id comes from the driver host.
        let prepared = match self.begin_driver_query(&connection) {
            Ok(prepared) => prepared,
            Err(error) => {
                let error = error.to_string();
                self.report_query_failure(tab_id, &error);
                if let Some(runtime) = self.workspace.runtime_mut(tab_id) {
                    runtime.query.fail(run, error);
                }
                self.query_tasks.registry.clear(tab_id, run);
                self.query_tasks.started_at.remove(&tab_id);
                self.notify_tab(tab_id, cx);
                return;
            }
        };
        let PreparedDriverQuery {
            session,
            runner,
            operation,
        } = prepared;
        if let Some(runtime) = self.workspace.runtime_mut(tab_id) {
            runtime.last_executed_query = Some(match (&aggregate, &script) {
                (Some(payload), _) => payload["pipeline"].as_str().unwrap_or_default().to_string(),
                (None, Some(script)) => script.clone(),
                (None, None) => mongo_query_signature_from_parts(&filter, &sort, &project),
            });
        }
        let operation_id = operation.operation_id;
        self.driver_runtime
            .query_operations
            .insert(tab_id, operation);
        let mut request = match aggregate {
            Some(payload) => mongo_aggregate_request(
                operation_id,
                &database,
                &collection,
                offset,
                limit.max(1) as u64,
                payload,
            ),
            None => mongo_request(
                operation_id,
                &database,
                &collection,
                offset,
                limit.max(1) as u64,
                &filter,
                &sort,
                &project,
                script.as_deref(),
            ),
        };
        if let Some(page_id) = self
            .workspace
            .runtime(tab_id)
            .and_then(|runtime| runtime.driver_page_id)
        {
            request.payload["replacement_page_id"] = page_id.into();
        }
        let request_json = serde_json::to_string(&request).expect("Mongo query request serializes");
        // The query itself runs through a detached runner, never under the
        // session lock: a slow aggregate must not queue the other tabs (and
        // the sidebar) of this connection behind it. The lock is taken only
        // (on the background executor) to register / read the finished page.
        let task = cx.background_executor().spawn(async move {
            let session_for_guard = Arc::clone(&session);
            let page = runner
                .open_query(&request_json)
                .map_err(|error| error.to_string())?;
            let (page, payload) = {
                // A poisoned lock after `open_query` must not leak the page.
                let mut session = session.lock().map_err(|_| {
                    runner.release_page(page.page_id);
                    "Driver session lock poisoned".to_string()
                })?;
                session
                    .adopt_page(&runner, page.page_id)
                    .map_err(|error| error.to_string())?;
                let payload = match session.acquire_page(page.page_id, 0, page.row_count) {
                    Ok(page_lease) => DriverMongoResult::PageLease(page_lease),
                    Err(engine_driver_host::PageError::Driver(
                        engine_driver_host::LoaderError::UnsupportedFeature { feature },
                    )) if feature == "query.page_lease" => {
                        let batch = session
                            .render_cell_batch(
                                page.page_id,
                                0,
                                page.row_count,
                                0,
                                page.columns.len() as u64,
                            )
                            .map_err(|error| error.to_string())
                            .and_then(|batch| {
                                let cells = (0..batch.cell_count())
                                    .filter_map(|index| batch.cell(index))
                                    .collect::<Vec<_>>();
                                crate::grid_arrow::host_cells_to_record_batch(
                                    &page.columns,
                                    page.row_count,
                                    cells,
                                )
                                .map(DriverMongoResult::CellBatch)
                                .map_err(|error| error.to_string())
                            });
                        match batch {
                            Ok(batch) => batch,
                            Err(error) => {
                                let _ = session.release_page(page.page_id);
                                return Err(error);
                            }
                        }
                    }
                    Err(error) => {
                        let _ = session.release_page(page.page_id);
                        return Err(error.to_string());
                    }
                };
                (page, payload)
            };
            let page_id = page.page_id;
            Ok::<_, String>((
                page,
                payload,
                DriverPageGuard::new(session_for_guard, page_id),
            ))
        });
        let task = cx.spawn(async move |this, cx| {
            let result = task.await;
            let _ = this.update(cx, |this, cx| {
                let applied = this.apply_driver_query_result(tab_id, run, connection, result);
                if applied {
                    this.select_mongo_document(tab_id, 0, cx);
                }
                this.notify_tab(tab_id, cx);
            });
        });
        self.query_tasks.tasks.insert(tab_id, task);
        self.notify_tab(tab_id, cx);
    }
}
