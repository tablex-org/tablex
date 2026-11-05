use super::*;

#[cfg(test)]
pub(super) fn queue_pending_mongo(
    workspace: &mut crate::workspace::WorkspaceState,
    tasks: &mut super::workspace::QueryTaskState,
    tab_id: u64,
    connection: &str,
    epoch: u64,
    sql: &str,
) -> Option<crate::query::QueryRunId> {
    queue_pending_mongo_script(workspace, tasks, tab_id, connection, epoch, sql, false)
}

pub(super) fn queue_pending_mongo_script(
    workspace: &mut crate::workspace::WorkspaceState,
    tasks: &mut super::workspace::QueryTaskState,
    tab_id: u64,
    connection: &str,
    epoch: u64,
    sql: &str,
    selection: bool,
) -> Option<crate::query::QueryRunId> {
    if tasks.pending_runs.contains_key(&tab_id) {
        return None;
    }
    let run = workspace.runtime_mut(tab_id)?.query.begin()?;
    tasks.pending_runs.insert(
        tab_id,
        super::workspace::PendingRun {
            connection: connection.into(),
            epoch,
            run,
            kind: super::workspace::PendingRunKind::MongoScript {
                sql: sql.into(),
                selection,
            },
        },
    );
    Some(run)
}

impl AppRoot {
    /// Run / Cmd+Enter / a history entry: runs the editor's whole text.
    pub(super) fn run_mongo_script_tab(&mut self, tab_id: u64, cx: &mut Context<Self>) {
        self.run_mongo_script_tab_with_run(tab_id, None, None, true, cx);
    }

    /// Runs only `script` (the selection or the statement at the cursor)
    /// instead of the tab's whole text. The user asked for it (Run Selected).
    pub(super) fn run_mongo_script_text(
        &mut self,
        tab_id: u64,
        script: String,
        cx: &mut Context<Self>,
    ) {
        self.run_mongo_script_tab_with_run(tab_id, None, Some(script), true, cx);
    }

    /// Paging, Refresh, a Limit change, a staged edit's reload, a reconnect:
    /// runs again the read on screen (`last_executed_query`) and nothing else.
    /// The editor text may hold anything typed since, and a shell script (it
    /// can write) only ever runs because the user pressed Run.
    pub(super) fn rerun_mongo_script_tab(&mut self, tab_id: u64, cx: &mut Context<Self>) {
        let shown = self
            .workspace
            .runtime(tab_id)
            .and_then(|runtime| runtime.last_executed_query.clone())
            .filter(|script| {
                matches!(
                    crate::mongo_script::classify(script),
                    crate::mongo_script::ScriptKind::Read(_)
                )
            });
        match shown {
            Some(script) => {
                self.run_mongo_script_tab_with_run(tab_id, None, Some(script), false, cx)
            }
            None => self.refuse_implicit_script_run(tab_id, cx),
        }
    }

    fn refuse_implicit_script_run(&mut self, tab_id: u64, cx: &mut Context<Self>) {
        self.view_mut(tab_id).detail_ui.error = Some("Run the script again with Run.".into());
        self.notify_tab(tab_id, cx);
    }

    /// `explicit`: the user pressed Run (or Run Selected); only then may the
    /// script be a shell script.
    fn run_mongo_script_tab_with_run(
        &mut self,
        tab_id: u64,
        pending_run: Option<crate::query::QueryRunId>,
        script_override: Option<String>,
        explicit: bool,
        cx: &mut Context<Self>,
    ) {
        if pending_run.is_none() && self.query_tasks.pending_runs.contains_key(&tab_id) {
            return;
        }
        let Some(tab) = self
            .workspace
            .tabs()
            .iter()
            .find(|tab| tab.id == tab_id && tab.query_mode.is_mongo_script())
            .cloned()
        else {
            return;
        };
        let Some(saved) = self
            .saved_connections
            .iter()
            .find(|saved| {
                saved.matches_id(&tab.connection_id)
                    && !saved.disabled
                    && saved.kind == ConnectionKind::Mongodb
            })
            .cloned()
        else {
            self.fail_mongo_script(tab_id, "MongoDB connection is unavailable", cx);
            return;
        };
        let selection = script_override.is_some();
        let script_text = script_override.unwrap_or_else(|| tab.sql.clone());
        // A single find stays the fast path (an editable paged grid); anything else is a shell
        // script for the MongoDB Shell plugin.
        let kind = crate::mongo_script::classify(&script_text);
        let database = match crate::mongo_script::script_database(&tab.database, &saved.uri) {
            Ok(database) => database,
            Err(error) => {
                self.fail_mongo_script(tab_id, &error.to_string(), cx);
                return;
            }
        };
        let plan = match kind {
            crate::mongo_script::ScriptKind::Read(plan) => plan,
            crate::mongo_script::ScriptKind::Shell if !explicit => {
                self.refuse_implicit_script_run(tab_id, cx);
                return;
            }
            crate::mongo_script::ScriptKind::Shell => {
                self.run_mongo_shell_script(
                    tab_id,
                    script_text,
                    selection,
                    database,
                    pending_run,
                    cx,
                );
                return;
            }
            crate::mongo_script::ScriptKind::Empty(message) => {
                self.fail_mongo_script(tab_id, &message, cx);
                return;
            }
        };
        if let Some(runtime) = self.workspace.runtime_mut(tab_id) {
            if runtime.last_executed_query.as_deref() != Some(script_text.as_str()) {
                runtime.pagination.reset();
            }
        }
        if let Some(service) = self.scenario_ui.service.as_ref() {
            if service.fixture().mongo_scripts_enabled() {
                self.run_scenario_mongo_script(tab_id, &database, &plan, &script_text, cx);
            } else {
                self.fail_mongo_script(
                    tab_id,
                    "MongoDB script execution is unavailable in a UI scenario",
                    cx,
                );
            }
            return;
        }
        if let Some(runtime) = self.workspace.runtime_mut(tab_id) {
            // A find replaces what the last shell run showed beside its grid.
            runtime.shell = None;
        }
        if self.driver_runtime.sessions.contains_key(&tab.connection_id) {
            // Capabilities are fixed per session and cached at connect time:
            // never `try_lock` the session here, because navigator calls hold
            // that lock during network I/O and a busy session is not an
            // unsupported one.
            match self.driver_capabilities(&tab.connection_id, saved.kind.expected_scheme()) {
                Some(capabilities) if capabilities.has(Capability::QueryMongoScript) => {}
                Some(_) => {
                    let message = self.outdated_driver_message(
                        saved.kind.driver_id(),
                        "MongoDB",
                        "does not support script queries",
                    );
                    self.fail_mongo_script(tab_id, &message, cx);
                    return;
                }
                None => {
                    self.fail_mongo_script(
                        tab_id,
                        "MongoDB driver capabilities are unavailable; reconnect and run again",
                        cx,
                    );
                    return;
                }
            }
            self.run_active_mongo_query(
                tab_id,
                tab.connection_id,
                database,
                plan.collection,
                String::new(),
                String::new(),
                String::new(),
                Some(script_text),
                pending_run,
                cx,
            );
            return;
        }
        self.ensure_driver_connected(saved, None, None, cx);
        if matches!(
            self.sessions.status(&tab.connection_id),
            ConnectionStatus::Connecting
        ) {
            if let Some(epoch) = self.sessions.generation(&tab.connection_id) {
                queue_pending_mongo_script(
                    &mut self.workspace,
                    &mut self.query_tasks,
                    tab_id,
                    &tab.connection_id,
                    epoch,
                    &script_text,
                    selection,
                );
                cx.notify();
            }
        } else {
            self.fail_mongo_script(
                tab_id,
                "Could not connect MongoDB driver; check connection status",
                cx,
            );
        }
    }

    /// Scenario fixture stand-in for a script run: the read plan becomes the
    /// fixture's find (filter / sort / skip / limit), paged like a driver run.
    fn run_scenario_mongo_script(
        &mut self,
        tab_id: u64,
        database: &str,
        plan: &engine_mongo_script::MongoReadScript,
        script_text: &str,
        cx: &mut Context<Self>,
    ) {
        if let Some(runtime) = self.workspace.runtime_mut(tab_id) {
            // Paging continues only while the same script keeps running.
            runtime.last_executed_query = Some(script_text.to_string());
        }
        let Some(connection) = self
            .workspace
            .tab(tab_id)
            .map(|tab| tab.connection_id.clone())
        else {
            return;
        };
        let Some((run, offset, limit)) = self.workspace.runtime_mut(tab_id).and_then(|runtime| {
            let run = runtime.query.begin()?;
            let page_offset = runtime.pagination.offset().max(0) as u64;
            // The script's own `.limit(n)` caps what any page may show.
            let remaining = plan
                .limit
                .map_or(i64::MAX, |limit| limit.saturating_sub(page_offset) as i64);
            Some((
                run,
                page_offset.saturating_add(plan.skip),
                runtime.pagination.page_size().min(remaining.max(1)),
            ))
        }) else {
            return;
        };
        let to_json = |document: &bson::Document| {
            serde_json::to_string(&bson::Bson::Document(document.clone()).into_relaxed_extjson())
                .unwrap_or_default()
        };
        let filter = to_json(&plan.filter);
        let sort = plan.sort.as_ref().map(to_json);
        let Some(service) = self.scenario_ui.service.as_mut() else {
            return;
        };
        let result = service.query_mongo_page(
            &connection,
            database,
            &plan.collection,
            offset,
            limit,
            Some(&filter),
            sort.as_deref(),
        );
        self.apply_fixture_mongo_query_result(tab_id, run, result);
        cx.notify();
    }

    /// Runs a queued Mongo script now that its connection is open.
    pub(super) fn resume_pending_mongo_script(
        &mut self,
        tab_id: u64,
        connection: &str,
        run: crate::query::QueryRunId,
        (sql, selection): (String, bool),
        cx: &mut Context<Self>,
    ) {
        if let Some(tab) = self
            .workspace
            .tabs()
            .iter()
            .find(|tab| tab.id == tab_id && tab.connection_id == connection)
        {
            if !selection && tab.sql != sql {
                self.fail_mongo_script(
                    tab_id,
                    "MongoDB script changed while connecting; run it again",
                    cx,
                );
            } else {
                let script = selection.then_some(sql);
                self.run_mongo_script_tab_with_run(tab_id, Some(run), script, true, cx);
            }
        }
    }

    pub(super) fn fail_mongo_script(&mut self, tab_id: u64, message: &str, cx: &mut Context<Self>) {
        self.report_query_failure(tab_id, message);
        if let Some(runtime) = self.workspace.runtime_mut(tab_id) {
            let run = match runtime.query.status() {
                crate::query::QueryStatus::Running { run } => Some(*run),
                _ => runtime.query.begin(),
            };
            if let Some(run) = run {
                runtime
                    .query
                    .fail(run, crate::sessions::redact_secrets(message));
            }
        }
        cx.notify();
    }
}
