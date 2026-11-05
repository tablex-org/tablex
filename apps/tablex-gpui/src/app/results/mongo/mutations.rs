use super::super::super::*;

impl AppRoot {
    pub(in crate::app) fn rollback_mongo_mutations(&mut self, tab_id: u64, cx: &mut Context<Self>) {
        if self.refuse_while_editing_mongo_document(tab_id, cx) {
            return;
        }
        if let Some(runtime) = self.workspace.runtime_mut(tab_id) {
            runtime.mongo.mutations.rollback();
            runtime.mongo.commit_status = crate::mongo_result::MongoCommitStatus::Idle;
            rebuild_mongo_pending_overlay(runtime);
        }
        self.notify_tab(tab_id, cx);
    }

    pub(in crate::app) fn undo_mongo_mutation(&mut self, tab_id: u64, cx: &mut Context<Self>) {
        if self.refuse_while_editing_mongo_document(tab_id, cx) {
            return;
        }
        if let Some(runtime) = self.workspace.runtime_mut(tab_id) {
            runtime.mongo.mutations.undo();
            rebuild_mongo_pending_overlay(runtime);
        }
        self.notify_tab(tab_id, cx);
    }

    pub(in crate::app) fn redo_mongo_mutation(&mut self, tab_id: u64, cx: &mut Context<Self>) {
        if self.refuse_while_editing_mongo_document(tab_id, cx) {
            return;
        }
        if let Some(runtime) = self.workspace.runtime_mut(tab_id) {
            runtime.mongo.mutations.redo();
            rebuild_mongo_pending_overlay(runtime);
        }
        self.notify_tab(tab_id, cx);
    }

    pub(in crate::app) fn stage_mongo_mutation(
        &mut self,
        tab_id: u64,
        mutation: PendingMongoMutation,
        cx: &mut Context<Self>,
    ) {
        if self.refuse_read_only_edit(tab_id, cx) {
            return;
        }
        if let Some(runtime) = self.workspace.runtime_mut(tab_id) {
            runtime.mongo.mutations.stage(mutation);
            runtime.mongo.commit_status = crate::mongo_result::MongoCommitStatus::Idle;
            rebuild_mongo_pending_overlay(runtime);
        }
        self.notify_tab(tab_id, cx);
    }

    /// [`Self::stage_mongo_mutation`] for several changes that are staged
    /// together: one undo step, one overlay rebuild for all.
    pub(in crate::app) fn stage_mongo_mutations(
        &mut self,
        tab_id: u64,
        mutations: Vec<PendingMongoMutation>,
        cx: &mut Context<Self>,
    ) {
        if mutations.is_empty() || self.refuse_read_only_edit(tab_id, cx) {
            return;
        }
        if let Some(runtime) = self.workspace.runtime_mut(tab_id) {
            runtime.mongo.mutations.stage_group(mutations);
            runtime.mongo.commit_status = crate::mongo_result::MongoCommitStatus::Idle;
            rebuild_mongo_pending_overlay(runtime);
        }
        self.notify_tab(tab_id, cx);
    }

    pub(in crate::app) fn commit_mongo_mutations(&mut self, tab_id: u64, cx: &mut Context<Self>) {
        if self.refuse_while_editing_mongo_document(tab_id, cx) {
            return;
        }
        let Some(tab) = self.workspace.tab(tab_id).cloned() else {
            return;
        };
        if self.scenario_ui.service.is_some() && tab.query_mode.is_mongo_find() {
            let Some(requests) = self.workspace.runtime(tab.id).map(|runtime| {
                runtime
                    .mongo
                    .mutations
                    .active()
                    .iter()
                    .map(|mutation| mutation.request().clone())
                    .collect::<Vec<_>>()
            }) else {
                return;
            };
            if requests.is_empty() {
                return;
            }
            let result = self.scenario_ui.service.as_mut().map(|service| {
                service.apply_mongo_mutations(
                    &tab.connection_id,
                    &tab.database,
                    &tab.collection,
                    &requests,
                )
            });
            if let Some(runtime) = self.workspace.runtime_mut(tab.id) {
                match result.unwrap_or_else(|| Err("Mongo fixture unavailable".into())) {
                    Ok(committed) if committed == requests.len() => {
                        runtime.mongo.mutations.remove_committed_prefix(committed);
                        runtime.mongo.commit_status = crate::mongo_result::MongoCommitStatus::Idle;
                    }
                    Ok(committed) => {
                        runtime.mongo.mutations.remove_committed_prefix(committed);
                        runtime.mongo.commit_status =
                            crate::mongo_result::MongoCommitStatus::Partial {
                                committed,
                                error: "Mongo fixture committed only part of the batch".into(),
                            };
                    }
                    Err(error) => {
                        runtime.mongo.commit_status =
                            crate::mongo_result::MongoCommitStatus::Partial {
                                committed: 0,
                                error,
                            };
                    }
                }
                rebuild_mongo_pending_overlay(runtime);
            }
            self.run_scenario_query_for_tab(tab.id);
            self.notify_tab(tab_id, cx);
            return;
        }
        if self.scenario_ui.service.is_some() {
            return;
        }
        let Some(requests) = self.workspace.runtime(tab.id).map(|runtime| {
            runtime
                .mongo
                .mutations
                .active()
                .iter()
                .map(|mutation| mutation.request().clone())
                .collect::<Vec<_>>()
        }) else {
            return;
        };
        if requests.is_empty() || !self.driver_runtime.sessions.contains_key(&tab.connection_id) {
            return;
        }
        if let Some(runtime) = self.workspace.runtime_mut(tab.id) {
            runtime.mongo.commit_status = crate::mongo_result::MongoCommitStatus::Running;
        }
        self.send_next_mongo_mutation(tab.id, tab.connection_id, requests.into_iter(), 0, cx);
        self.notify_tab(tab_id, cx);
    }

    /// Sends the next staged request, then the one after it from its
    /// completion. Lock-free: the batch never holds the session lock, so
    /// navigator calls and page adoption on the connection proceed while a slow
    /// mutation runs. Every request runs under its own host operation,
    /// registered for the tab, so Cancel reaches the request in flight (a driver
    /// ignores a cancel for an operation id older than the newest it has seen).
    /// A Cancel takes that registration: when it is gone after a request
    /// finished, the user cancelled, so no further request is sent and the
    /// unsent changes stay staged.
    fn send_next_mongo_mutation(
        &mut self,
        tab_id: u64,
        connection: String,
        mut remaining: std::vec::IntoIter<engine_driver_api::MutationRequest>,
        committed: usize,
        cx: &mut Context<Self>,
    ) {
        let Some(request) = remaining.next() else {
            self.finish_mongo_commit(tab_id, &connection, Ok(committed), cx);
            return;
        };
        let PreparedDriverQuery {
            session,
            runner,
            operation,
        } = match self.begin_driver_query(&connection) {
            Ok(prepared) => prepared,
            Err(error) => {
                self.finish_mongo_commit(tab_id, &connection, Err((committed, error)), cx);
                return;
            }
        };
        let operation_id = operation.operation_id;
        self.driver_runtime.query_operations.insert(tab_id, operation);
        let work = cx.background_executor().spawn(async move {
            smol::unblock(move || {
                session
                    .mutate(runner.operation(), request)
                    .map_err(DriverQueryError::from)
            })
            .await
        });
        let task = cx.spawn(async move |this, cx| {
            let result = work.await;
            let _ = this.update(cx, |this, cx| {
                // Only this request's own operation: a Cancel already took it.
                let cancelled = !this
                    .driver_runtime
                    .query_operations
                    .get(&tab_id)
                    .is_some_and(|operation| operation.operation_id == operation_id);
                if !cancelled {
                    this.driver_runtime.query_operations.remove(&tab_id);
                }
                match result {
                    Err(error) => {
                        this.finish_mongo_commit(tab_id, &connection, Err((committed, error)), cx)
                    }
                    Ok(done) => {
                        // Other tabs of the connection may show pages this
                        // write dropped; this tab re-runs after the commit.
                        this.invalidate_driver_pages(
                            &connection,
                            &done.invalidated_page_ids,
                            Some(tab_id),
                            cx,
                        );
                        let committed = committed + 1;
                        if cancelled && remaining.len() > 0 {
                            let stop = Err((committed, DriverQueryError::Cancelled));
                            this.finish_mongo_commit(tab_id, &connection, stop, cx);
                        } else {
                            this.send_next_mongo_mutation(tab_id, connection, remaining, committed, cx);
                        }
                    }
                }
            });
        });
        task.detach();
    }

    /// Ends a commit: drops the `committed` leading changes from the staging
    /// and shows the outcome. The staged rest stays for another attempt.
    pub(in crate::app) fn finish_mongo_commit(
        &mut self,
        tab_id: u64,
        connection: &str,
        result: Result<usize, (usize, DriverQueryError)>,
        cx: &mut Context<Self>,
    ) {
        let mut refresh = false;
        let mut mutation_failure = None;
        if let Some(runtime) = self.workspace.runtime_mut(tab_id) {
            match &result {
                Ok(committed) => {
                    runtime.mongo.mutations.remove_committed_prefix(*committed);
                    runtime.mongo.commit_status = crate::mongo_result::MongoCommitStatus::Idle;
                    refresh = *committed > 0;
                }
                Err((committed, error)) => {
                    // The user's own cancel is not an Error Log entry.
                    let error_text = error.to_string();
                    if !error.is_cancelled() {
                        mutation_failure = Some(error_text.clone());
                    }
                    runtime.mongo.mutations.remove_committed_prefix(*committed);
                    runtime.mongo.commit_status = crate::mongo_result::MongoCommitStatus::Partial {
                        committed: *committed,
                        error: crate::sessions::redact_secrets(&error_text),
                    };
                    refresh = *committed > 0;
                }
            }
            rebuild_mongo_pending_overlay(runtime);
        }
        if let Err((_, error)) = &result {
            self.note_driver_connection_lost(tab_id, error);
        }
        if let Some(error) = mutation_failure {
            self.report_error(Area::Mutation, Some(connection), error);
        }
        if refresh {
            self.run_tab_query(tab_id, cx);
        }
        self.notify_tab(tab_id, cx);
    }

    pub(in crate::app) fn open_mongo_insert_row_editor_from_menu(
        &mut self,
        tab_id: u64,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(tab) = self.workspace.tab(tab_id).cloned() else {
            return;
        };
        self.open_mongo_insert_row_editor(tab, window, cx);
    }
}
