use super::super::super::*;

pub(in crate::app) const STALE_MONGO_PAGE: &str =
    "This result is out of date. Run the query again to open documents.";

impl AppRoot {
    /// `true` (and says why in the status row) while the Document view's
    /// editor is open: its text must leave through Save or Cancel, so nothing
    /// that would change the document or the view goes ahead meanwhile.
    pub(in crate::app) fn refuse_while_editing_mongo_document(&mut self, tab_id: u64, cx: &mut Context<Self>) -> bool {
        if self.view(tab_id).mongo.document_edit.is_none() {
            return false;
        }
        self.view_mut(tab_id).detail_ui.error = Some(super::document_edit::EDITING_MESSAGE.into());
        self.notify_tab(tab_id, cx);
        true
    }

    pub(in crate::app) fn set_mongo_view(
        &mut self,
        tab_id: u64,
        view: crate::mongo_result::MongoResultView,
        cx: &mut Context<Self>,
    ) {
        if self.refuse_while_editing_mongo_document(tab_id, cx) {
            return;
        }
        let should_select_first = self
            .workspace
            .runtime(tab_id)
            .map(|runtime| {
                runtime.mongo.inspector.selected.is_none()
                    && view != crate::mongo_result::MongoResultView::Table
            })
            .unwrap_or(false);
        if let Some(runtime) = self.workspace.runtime_mut(tab_id) {
            runtime.mongo.view = view;
            runtime.mongo.inspector.error = None;
        }
        if should_select_first {
            self.select_mongo_document(tab_id, 0, cx);
        }
        self.notify_tab(tab_id, cx);
    }

    pub(in crate::app) fn toggle_mongo_document_view(&mut self, tab_id: u64, cx: &mut Context<Self>) {
        if self.refuse_while_editing_mongo_document(tab_id, cx) {
            return;
        }
        if let Some(runtime) = self.workspace.runtime_mut(tab_id) {
            runtime.mongo.toggle_document_view();
        }
        self.notify_tab(tab_id, cx);
    }

    pub(in crate::app) fn apply_mongo_filter_rules(&mut self, tab_id: u64, cx: &mut Context<Self>) {
        let Some((manual_filter, rules)) = self.workspace.runtime(tab_id).map(|runtime| {
            (
                runtime.mongo.filter.clone(),
                runtime.mongo.filter_rules.clone(),
            )
        }) else {
            return;
        };
        match crate::mongo_result::merge_mongo_filters(&manual_filter, &rules) {
            Ok(filter) => {
                if let Some(runtime) = self.workspace.runtime_mut(tab_id) {
                    runtime.mongo.applied_filter = Some(filter);
                    runtime.mongo.filter_error = None;
                    runtime.mongo.filter_open = false;
                    runtime.pagination.reset();
                }
                self.run_tab_query(tab_id, cx);
            }
            Err(error) => {
                if let Some(runtime) = self.workspace.runtime_mut(tab_id) {
                    runtime.mongo.filter_error = Some(error);
                }
                self.notify_tab(tab_id, cx);
            }
        }
    }

    /// Clears the find filter and re-runs. `keep_builder_open`: the filter builder's own Clear
    /// Filter, which leaves the builder open on one fresh rule; the toolbar's clear closes it.
    pub(in crate::app) fn clear_mongo_filters(
        &mut self,
        tab_id: u64,
        keep_builder_open: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.clear_mongo_filter_rule_editors(tab_id);
        let first_field = self.grid(tab_id).column_name(0).unwrap_or("").to_string();
        if let Some(runtime) = self.workspace.runtime_mut(tab_id) {
            runtime.mongo.clear_filters();
            if keep_builder_open {
                runtime.mongo.ensure_filter_rule(first_field);
            } else {
                runtime.mongo.filter_open = false;
            }
            runtime.pagination.reset();
        }
        if let Some(editor) = self.mongo_ui.filter_editors.get(&tab_id).cloned() {
            if editor.read(cx).value().as_ref() != "{}" {
                editor.update(cx, |state, cx| state.set_value("{}", window, cx));
            }
        }
        if let Some(editor) = self.mongo_ui.expanded_find_editors.get(&tab_id).cloned() {
            if editor.read(cx).value().as_ref() != "{}" {
                editor.update(cx, |state, cx| state.set_value("{}", window, cx));
            }
        }
        self.run_tab_query(tab_id, cx);
    }

    pub(in crate::app) fn set_mongo_column_tab(
        &mut self,
        tab_id: u64,
        tab: crate::mongo_result::MongoColumnTab,
        cx: &mut Context<Self>,
    ) {
        if let Some(runtime) = self.workspace.runtime_mut(tab_id) {
            runtime.mongo.column_tab = tab;
        }
        self.notify_tab(tab_id, cx);
    }

    pub(in crate::app) fn toggle_mongo_column(
        &mut self,
        tab_id: u64,
        name: String,
        cx: &mut Context<Self>,
    ) {
        let visible_changed = if let Some(runtime) = self.workspace.runtime_mut(tab_id) {
            let column_tab = runtime.mongo.column_tab;
            match column_tab {
                crate::mongo_result::MongoColumnTab::Visible => {
                    let selected = !runtime.mongo.visible_columns.contains(&name);
                    runtime.mongo.set_visible_column(&name, selected);
                    rebuild_mongo_pending_overlay(runtime);
                    true
                }
                crate::mongo_result::MongoColumnTab::Projection => {
                    let selected = !runtime.mongo.projection_draft_columns.contains(&name);
                    runtime.mongo.set_projection_draft_column(&name, selected);
                    false
                }
            }
        } else {
            false
        };
        if visible_changed {
            self.reset_tab_view(tab_id);
        }
        self.notify_tab(tab_id, cx);
    }

    pub(in crate::app) fn show_all_mongo_columns(&mut self, tab_id: u64, cx: &mut Context<Self>) {
        if let Some(runtime) = self.workspace.runtime_mut(tab_id) {
            let result_columns = (0..runtime.grid.column_count())
                .filter_map(|column| runtime.grid.column_name(column).map(str::to_string))
                .collect::<HashSet<_>>();
            if !result_columns.is_empty() {
                runtime.mongo.visible_columns = result_columns;
                rebuild_mongo_pending_overlay(runtime);
            }
        }
        self.reset_tab_view(tab_id);
        self.notify_tab(tab_id, cx);
    }

    pub(in crate::app) fn select_all_mongo_projection(
        &mut self,
        tab_id: u64,
        cx: &mut Context<Self>,
    ) {
        if let Some(runtime) = self.workspace.runtime_mut(tab_id) {
            runtime.mongo.projection_draft_columns =
                runtime.mongo.known_columns.iter().cloned().collect();
        }
        self.notify_tab(tab_id, cx);
    }

    pub(in crate::app) fn apply_mongo_projection(&mut self, tab_id: u64, cx: &mut Context<Self>) {
        let applied = self
            .workspace
            .runtime_mut(tab_id)
            .filter(|runtime| !runtime.mongo.has_pending_mutations())
            .and_then(|runtime| runtime.mongo.apply_projection_draft().map(|_| ()))
            .is_some();
        if applied {
            if let Some(runtime) = self.workspace.runtime_mut(tab_id) {
                runtime.mongo.column_open = false;
                runtime.pagination.reset();
            }
            self.run_tab_query(tab_id, cx);
        } else {
            self.notify_tab(tab_id, cx);
        }
    }

    pub(in crate::app) fn set_mongo_columns_open(
        &mut self,
        tab_id: u64,
        open: bool,
        cx: &mut Context<Self>,
    ) {
        if let Some(runtime) = self.workspace.runtime_mut(tab_id) {
            runtime.mongo.column_open = open;
            if open {
                runtime.mongo.reset_projection_draft();
            }
        }
        self.notify_tab(tab_id, cx);
    }

    pub(in crate::app) fn set_mongo_find_expanded(
        &mut self,
        tab_id: u64,
        expanded: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let filter = self
            .workspace
            .runtime(tab_id)
            .map(|runtime| runtime.mongo.filter.clone())
            .unwrap_or_else(|| "{}".into());
        if let Some(runtime) = self.workspace.runtime_mut(tab_id) {
            runtime.mongo.set_find_expanded(expanded);
        }
        if expanded {
            if let Some(editor) = self.mongo_ui.expanded_find_editors.get(&tab_id).cloned() {
                if editor.read(cx).value().as_ref() != filter.as_str() {
                    editor.update(cx, |state, cx| state.set_value(filter, window, cx));
                }
            }
        }
        self.notify_tab(tab_id, cx);
    }

    pub(in crate::app) fn set_mongo_sort_expanded(
        &mut self,
        tab_id: u64,
        expanded: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let sort = self
            .workspace
            .runtime(tab_id)
            .map(|runtime| runtime.mongo.sort.clone())
            .unwrap_or_else(|| r#"{"_id":1}"#.into());
        if let Some(runtime) = self.workspace.runtime_mut(tab_id) {
            runtime.mongo.set_sort_expanded(expanded);
        }
        if expanded {
            if let Some(editor) = self.mongo_ui.expanded_sort_editors.get(&tab_id).cloned() {
                if editor.read(cx).value().as_ref() != sort.as_str() {
                    editor.update(cx, |state, cx| state.set_value(sort, window, cx));
                }
            }
        }
        self.notify_tab(tab_id, cx);
    }

    pub(in crate::app) fn set_mongo_query_preview_open(
        &mut self,
        tab_id: u64,
        open: bool,
        cx: &mut Context<Self>,
    ) {
        if let Some(runtime) = self.workspace.runtime_mut(tab_id) {
            runtime.mongo.set_query_preview_open(open);
        }
        self.notify_tab(tab_id, cx);
    }

    pub(in crate::app) fn clear_mongo_sort(
        &mut self,
        tab_id: u64,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if let Some(runtime) = self.workspace.runtime_mut(tab_id) {
            runtime.mongo.sort = String::new();
            runtime.mongo.inspector.error = None;
        }
        if let Some(editor) = self.mongo_ui.sort_editors.get(&tab_id).cloned() {
            if !editor.read(cx).value().is_empty() {
                editor.update(cx, |state, cx| state.set_value("", window, cx));
            }
        }
        if let Some(editor) = self.mongo_ui.expanded_sort_editors.get(&tab_id).cloned() {
            if !editor.read(cx).value().is_empty() {
                editor.update(cx, |state, cx| state.set_value("", window, cx));
            }
        }
        self.notify_tab(tab_id, cx);
    }

    pub(in crate::app) fn select_mongo_document(
        &mut self,
        tab_id: u64,
        row: usize,
        cx: &mut Context<Self>,
    ) {
        let Some(page_id) = self
            .workspace
            .runtime(tab_id)
            .and_then(|runtime| runtime.driver_page_id)
        else {
            return;
        };
        if row >= self.grid(tab_id).row_count() {
            return;
        }
        let key = crate::mongo_result::MongoDocumentKey {
            page_id,
            row_index: row as u64,
        };
        self.view(tab_id)
            .grid_ui
            .selection
            .borrow_mut()
            .select(crate::grid::CellPosition { row, column: 0 });
        if let Some(runtime) = self.workspace.runtime_mut(tab_id) {
            runtime.mongo.inspector.select(key);
        }
        match self.mongo_row_inspection(tab_id, row) {
            Ok(MongoRowInspection::Ready(fields)) => {
                self.finish_mongo_inspection(tab_id, key, Ok(fields));
            }
            Err(error) => self.finish_mongo_inspection(tab_id, key, Err(error)),
            Ok(MongoRowInspection::Driver {
                session,
                page_id,
                source_row,
            }) => {
                // The driver read happens off the UI thread and without the
                // session lock, so a busy session (navigator I/O) can neither
                // stall nor fail the inspector.
                let task = cx
                    .background_executor()
                    .spawn(async move { session.inspect_row(page_id, source_row) });
                cx.spawn(async move |this, cx| {
                    let result = task.await;
                    let _ = this.update(cx, |this, cx| {
                        match result {
                            Err(error) if is_stale_driver_page(&error) => {
                                this.on_stale_mongo_document(tab_id, key, cx);
                            }
                            result => this.finish_mongo_inspection(
                                tab_id,
                                key,
                                result.map_err(|error| error.to_string()),
                            ),
                        }
                        this.notify_tab(tab_id, cx);
                    });
                })
                .detach();
            }
        }
        self.notify_tab(tab_id, cx);
    }

    /// The driver no longer holds the page of the selected document. The tab
    /// re-runs for a current page (which selects the first document again);
    /// when it cannot, the inspector says so instead of echoing the driver.
    fn on_stale_mongo_document(
        &mut self,
        tab_id: u64,
        key: crate::mongo_result::MongoDocumentKey,
        cx: &mut Context<Self>,
    ) {
        let selected = self
            .workspace
            .runtime(tab_id)
            .is_some_and(|runtime| runtime.mongo.inspector.selected == Some(key));
        if selected && !self.refresh_stale_mongo_page(tab_id, key.page_id, cx) {
            self.finish_mongo_inspection(tab_id, key, Err(STALE_MONGO_PAGE.into()));
        }
    }

    /// Re-runs the tab's find because the driver dropped `page_id`. `true`
    /// when a run is under way. Once per stale page: the run's own page being
    /// stale as well, staged edits (a new page would drop their row marks) or
    /// a run that cannot start all answer `false`.
    fn refresh_stale_mongo_page(
        &mut self,
        tab_id: u64,
        page_id: u64,
        cx: &mut Context<Self>,
    ) -> bool {
        // Only a find re-runs on its own: a script or an aggregate may write.
        if !self
            .workspace
            .tab(tab_id)
            .is_some_and(|tab| tab.query_mode.is_mongo_find())
        {
            return false;
        }
        let Some(runtime) = self.workspace.runtime_mut(tab_id) else {
            return false;
        };
        if runtime.driver_page_id != Some(page_id) || runtime.mongo.stale_refresh {
            return false;
        }
        runtime.mongo.stale_refresh = true;
        self.rerun_stale_tab(tab_id, cx)
    }

    /// Applies an inspected row to the inspector, unless the user selected
    /// another document (or the tab went away) while it was being read.
    fn finish_mongo_inspection(
        &mut self,
        tab_id: u64,
        key: crate::mongo_result::MongoDocumentKey,
        result: Result<engine_driver_api::RowFields, String>,
    ) {
        let Some(runtime) = self.workspace.runtime_mut(tab_id) else {
            return;
        };
        if runtime.mongo.inspector.selected != Some(key) {
            return;
        }
        match result {
            Ok(row) => {
                let mutation_identity = Some(row.mutation_identity).filter(|id| !id.is_null());
                let document = serde_json::Value::Object(
                    row.fields
                        .into_iter()
                        .map(|field| (field.name, field.value))
                        .collect(),
                );
                let mut snapshot = crate::mongo_result::MongoDocumentSnapshot::new(key, document);
                snapshot.mutation_identity = mutation_identity;
                runtime.mongo.inspector.accept_snapshot(snapshot);
                runtime.mongo.stale_refresh = false;
            }
            Err(error) => {
                runtime.mongo.inspector.loading = false;
                runtime.mongo.inspector.error = Some(crate::sessions::redact_secrets(&error));
            }
        }
    }

    /// Resolves where the fields of `row` come from without reading them.
    fn mongo_row_inspection(
        &self,
        tab_id: u64,
        row: usize,
    ) -> Result<MongoRowInspection, String> {
        let grid = &self
            .workspace
            .runtime(tab_id)
            .ok_or("Mongo result tab is unavailable")?
            .grid;
        if row >= grid.row_count() {
            return Err("Mongo row is outside the current page".into());
        }
        let source_row = grid.source_row(row);
        let page_id = self
            .workspace
            .runtime(tab_id)
            .and_then(|runtime| runtime.driver_page_id)
            .ok_or_else(|| "Mongo result page is unavailable".to_string())?;
        let tab = self
            .workspace
            .tabs()
            .iter()
            .find(|tab| tab.id == tab_id)
            .ok_or_else(|| "Mongo result tab is unavailable".to_string())?;
        if self.scenario_ui.service.is_some() && tab.query_mode.is_mongo_find() {
            return self
                .scenario_ui
                .service
                .as_ref()
                .ok_or_else(|| "Mongo fixture is unavailable".to_string())?
                .fixture()
                .inspect_mongo_row(page_id, source_row as u64)
                .map(MongoRowInspection::Ready);
        }
        let session = self
            .driver_runtime
            .sessions
            .get(&tab.connection_id)
            .cloned()
            .ok_or_else(|| "Driver session is not connected".to_string())?;
        Ok(MongoRowInspection::Driver {
            session,
            page_id,
            source_row: source_row as u64,
        })
    }

    /// The fields of `row` for event handlers and detail builders that need
    /// them right now (start an inline edit, stage a delete, build the cell
    /// detail). They come from the per-selection [`crate::mongo_result::RowFieldsCache`], which
    /// [`Self::sync_mongo_row_fields`] fills on a background executor: the UI
    /// thread never calls into the driver, and a hit is an `Arc` clone. A row
    /// outside the cache (the selection moved a moment ago) answers "still
    /// loading"; the same action works once the read lands.
    pub(in crate::app) fn mongo_row_fields(
        &self,
        tab_id: u64,
        row: usize,
    ) -> Result<std::sync::Arc<engine_driver_api::RowFields>, String> {
        match self.mongo_row_inspection(tab_id, row)? {
            // The scenario fixture reads in-memory rows, not a driver.
            MongoRowInspection::Ready(fields) => Ok(std::sync::Arc::new(fields)),
            MongoRowInspection::Driver {
                page_id,
                source_row,
                ..
            } => self
                .workspace
                .runtime(tab_id)
                .and_then(|runtime| runtime.mongo.row_fields.get(page_id, source_row))
                .cloned()
                .ok_or_else(|| "Row details are still loading; try again in a moment.".to_string()),
        }
    }

    /// Runs every frame for each shown Mongo result: when the tab's selection
    /// changed (or a new page arrived), reads the selected rows from the driver
    /// on a background executor into the row cache. An unchanged selection costs
    /// one comparison; at most [`crate::mongo_result::RowFieldsCache::CAPACITY`] rows are read.
    pub(in crate::app) fn sync_mongo_row_fields(&mut self, tab_id: u64, cx: &mut Context<Self>) {
        if !self
            .workspace
            .tab(tab_id)
            .is_some_and(|tab| tab.query_mode.is_mongo_find())
        {
            return;
        }
        let Some(range) = self
            .view(tab_id)
            .grid_ui
            .selection
            .borrow()
            .range
            .map(|range| range.normalized())
        else {
            return;
        };
        let Some(page_id) = self
            .workspace
            .runtime(tab_id)
            .and_then(|runtime| runtime.driver_page_id)
        else {
            return;
        };
        let Some(last_row) = self.grid(tab_id).row_count().checked_sub(1) else {
            return;
        };
        let first = range.start.row;
        let last = range
            .end
            .row
            .min(last_row)
            .min(first.saturating_add(crate::mongo_result::RowFieldsCache::CAPACITY - 1));
        if first > last {
            return;
        }
        let Some(runtime) = self.workspace.runtime_mut(tab_id) else {
            return;
        };
        if !runtime.mongo.row_fields.wants(page_id, first, last) {
            return;
        }
        let wanted = runtime.mongo.row_fields.missing(
            page_id,
            (first..=last).map(|row| runtime.grid.source_row(row) as u64),
        );
        if wanted.is_empty() {
            runtime.mongo.row_fields.mark_requested(page_id, first, last);
            return;
        }
        // Without a session nothing is read, so nothing is recorded: the next
        // frame asks again (one map lookup) until the session is there.
        let Some(session) = self
            .workspace
            .tab(tab_id)
            .and_then(|tab| self.driver_runtime.sessions.get(&tab.connection_id))
            .cloned()
        else {
            return;
        };
        if let Some(runtime) = self.workspace.runtime_mut(tab_id) {
            runtime.mongo.row_fields.mark_requested(page_id, first, last);
        }
        let expected = wanted.len();
        // Lock-free reads of a page the driver already holds, in one blocking
        // task so a large selection does not occupy the shared pool.
        let work = cx.background_executor().spawn(async move {
            smol::unblock(move || {
                let mut stale = false;
                // The first error that is not a stale page, for the error log.
                let mut failure = None::<String>;
                let rows = wanted
                    .into_iter()
                    .filter_map(|row| match session.inspect_row(page_id, row) {
                        Ok(fields) => Some((row, fields)),
                        Err(error) => {
                            if is_stale_driver_page(&error) {
                                stale = true;
                            } else if failure.is_none() {
                                failure = Some(format!("Could not read row {} of the result: {error}", row + 1));
                            }
                            None
                        }
                    })
                    .collect::<Vec<_>>();
                (rows, stale, failure)
            })
            .await
        });
        cx.spawn(async move |this, cx| {
            let (fetched, stale, failure) = work.await;
            let _ = this.update(cx, |this, cx| {
                let current = this
                    .workspace
                    .runtime(tab_id)
                    .is_some_and(|runtime| runtime.driver_page_id == Some(page_id));
                if let Some(runtime) = this.workspace.runtime_mut(tab_id) {
                    // A newer page replaced this one while the read ran.
                    if current {
                        let complete = fetched.len() == expected;
                        runtime.mongo.row_fields.insert(page_id, fetched);
                        if !complete {
                            runtime.mongo.row_fields.read_failed(page_id, first, last);
                        }
                    }
                }
                if current {
                    // One report per read (a selection is read at most twice);
                    // identical consecutive reports collapse in the log.
                    if let Some(failure) = failure {
                        this.report_query_failure(tab_id, &failure);
                    }
                    // The panel was built at the click, before this read: it
                    // showed the grid's text (`<object>` for a document).
                    this.refresh_cell_detail_after_row_read(tab_id);
                }
                if current && stale {
                    this.refresh_stale_mongo_page(tab_id, page_id, cx);
                }
                this.notify_tab(tab_id, cx);
            });
        })
        .detach();
    }

    pub(in crate::app) fn update_mongo_filter_rule(
        &mut self,
        tab_id: u64,
        rule_id: u64,
        patch: impl FnOnce(&mut crate::mongo_result::MongoFilterRule),
        cx: &mut Context<Self>,
    ) {
        if let Some(runtime) = self.workspace.runtime_mut(tab_id) {
            if let Some(rule) = runtime
                .mongo
                .filter_rules
                .iter_mut()
                .find(|rule| rule.id == rule_id)
            {
                patch(rule);
                runtime.mongo.mark_filter_draft();
            }
        }
        self.notify_tab(tab_id, cx);
    }

    pub(in crate::app) fn select_mongo_filter_field(
        &mut self,
        tab_id: u64,
        rule_id: u64,
        field: String,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if let Some(runtime) = self.workspace.runtime_mut(tab_id) {
            if let Some(rule) = runtime
                .mongo
                .filter_rules
                .iter_mut()
                .find(|rule| rule.id == rule_id)
            {
                rule.field.clone_from(&field);
                runtime.mongo.mark_filter_draft();
            }
        }
        if let Some(editor) = self.mongo_ui.filter_field_editors.get(&(tab_id, rule_id)) {
            if editor.read(cx).value().as_ref() != field {
                editor.update(cx, |state, cx| state.set_value(field, window, cx));
            }
        }
        self.notify_tab(tab_id, cx);
    }

    pub(in crate::app) fn remove_mongo_filter_rule(
        &mut self,
        tab_id: u64,
        rule_id: u64,
        cx: &mut Context<Self>,
    ) {
        if let Some(runtime) = self.workspace.runtime_mut(tab_id) {
            runtime.mongo.filter_rules.retain(|rule| rule.id != rule_id);
            runtime.mongo.mark_filter_draft();
        }
        self.mongo_ui
            .filter_field_editors
            .remove(&(tab_id, rule_id));
        self.mongo_ui
            .filter_value_editors
            .remove(&(tab_id, rule_id));
        self.notify_tab(tab_id, cx);
    }

    pub(in crate::app) fn reset_mongo_filter_builder(
        &mut self,
        tab_id: u64,
        cx: &mut Context<Self>,
    ) {
        self.clear_mongo_filter_rule_editors(tab_id);
        let field = self.grid(tab_id).column_name(0).unwrap_or("").to_string();
        if let Some(runtime) = self.workspace.runtime_mut(tab_id) {
            runtime.mongo.filter_rules.clear();
            runtime.mongo.ensure_filter_rule(field);
            runtime.mongo.mark_filter_draft();
        }
        self.notify_tab(tab_id, cx);
    }

    pub(in crate::app) fn clear_mongo_filter_rule_editors(&mut self, tab_id: u64) {
        self.mongo_ui
            .filter_field_editors
            .retain(|(editor_tab_id, _), _| *editor_tab_id != tab_id);
        self.mongo_ui
            .filter_value_editors
            .retain(|(editor_tab_id, _), _| *editor_tab_id != tab_id);
    }
}

/// Where the fields of one Mongo result row are read from.
enum MongoRowInspection {
    /// Already available (the UI scenario fixture).
    Ready(engine_driver_api::RowFields),
    /// A page held by the driver behind `session`.
    Driver {
        session: Arc<engine_driver_host::SharedDriverSession>,
        page_id: u64,
        source_row: u64,
    },
}
