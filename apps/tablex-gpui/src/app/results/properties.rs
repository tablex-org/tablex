//! Properties sub-tabs backed by `TableProperties` (Keys, Foreign Keys,
//! Indexes, References, Triggers, DDL): the once-per-result background fetch,
//! the lazily built list models, and their rendering. The Columns sub-tab lives
//! in `columns_view.rs` and keeps working from the result's own metadata when
//! the fetch fails.

use super::super::*;
use super::columns_view::{COLUMNS_TOOLBAR_HEIGHT, HEADER_ROW_HEIGHT, ROW_HEIGHT};
use crate::app::actions::{PropertiesRedo, PropertiesUndo};
use crate::columns_panel::{PropertiesTab, CELL_PADDING};
use crate::properties_model::{
    CellStyle, DdlEditor, LoadStatus, PropertiesKey, PropertyTable,
};
use crate::catalog::TableProperties;
use engine_driver_api::sql as wire;

/// Text-width measurer for content-sized columns (window default font,
/// padded 10% because the themed UI font renders slightly wider).
fn text_measurer(window: &Window) -> impl Fn(&str, bool) -> f32 {
    let style = window.text_style();
    let system = window.text_system().clone();
    move |text: &str, header: bool| {
        if text.is_empty() {
            return 0.0;
        }
        let size = px(if header { 11.0 } else { 12.0 });
        f32::from(
            system
                .layout_line(text, size, &[style.to_run(text.len())], None)
                .width,
        ) * 1.1
    }
}

fn filter_placeholder(tab: PropertiesTab) -> &'static str {
    match tab {
        PropertiesTab::Columns => "Filter fields",
        PropertiesTab::Keys => "Filter keys",
        PropertiesTab::ForeignKeys => "Filter foreign keys",
        PropertiesTab::Indexes => "Filter indexes",
        PropertiesTab::References => "Filter references",
        PropertiesTab::Triggers => "Filter triggers",
        PropertiesTab::Ddl => "",
    }
}

impl AppRoot {
    /// Runs from `sync_columns_panel` while Properties is open: starts the
    /// (single) `table_properties` fetch for the current result, keeps the
    /// active sub-tab valid and builds/refilters its model. Every check is an
    /// O(1) key comparison; models are built once per fetched result.
    pub(in crate::app) fn sync_table_properties(
        &mut self,
        tab_id: u64,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let key = {
            let Some(tab) = self.workspace.tab(tab_id) else {
                return;
            };
            let Some(runtime) = self.workspace.runtime(tab_id) else {
                return;
            };
            let table_backed = tab.query_mode.is_sql_table()
                && !tab.connection_id.is_empty()
                && !tab.collection.is_empty();
            let schema = tab.schema.as_deref().unwrap_or("");
            // Table metadata does not depend on the result (paging, sorting,
            // filtering, auto-refresh): the key is the table plus the engine
            // session, so only an explicit refresh or a reconnect refetches.
            let session = self
                .driver_runtime
                .connection_ids
                .get(&tab.connection_id)
                .copied()
                .unwrap_or(0);
            let props = &runtime.columns_panel.props;
            let fresh = !table_backed
                || (!props.stale
                    && props.key.as_ref().is_some_and(|key| {
                        key.matches(&tab.connection_id, &tab.database, schema, &tab.collection, session)
                    }));
            (!fresh).then(|| PropertiesKey {
                connection: tab.connection_id.clone(),
                database: tab.database.clone(),
                schema: schema.to_string(),
                object: tab.collection.clone(),
                session,
            })
        };
        if let Some(key) = key {
            self.fetch_table_properties(tab_id, key, cx);
        }

        let Some(runtime) = self.workspace.runtime_mut(tab_id) else {
            return;
        };
        let panel = &mut runtime.columns_panel;
        // The active sub-tab disappears with its data (new table / failed fetch).
        if !panel.props.visible_tabs().any(|tab| tab == panel.properties_tab) {
            panel.properties_tab = PropertiesTab::Columns;
        }
        let tab = panel.properties_tab;
        match tab {
            PropertiesTab::Columns => {}
            PropertiesTab::Ddl => {
                let props = &mut panel.props;
                let Some(ddl) = props.data.as_ref().and_then(|data| data.ddl.as_deref()) else {
                    return;
                };
                let revision = props.revision;
                match props.ddl.as_mut() {
                    Some(editor) if editor.revision == revision => {}
                    Some(editor) => {
                        editor.revision = revision;
                        let text = ddl.to_string();
                        editor.state.update(cx, |state, cx| state.set_value(text, window, cx));
                    }
                    None => {
                        let text = ddl.to_string();
                        // Official code editor: tree-sitter SQL highlighting,
                        // line numbers and horizontal scrolling, read-only.
                        let state = cx.new(|cx| {
                            EditorState::new(window, cx)
                                .language("sql")
                                .line_number(true)
                                .soft_wrap(false)
                                .default_value(text)
                        });
                        props.ddl = Some(DdlEditor { revision, state });
                    }
                }
            }
            list => {
                let filter = &panel.filter;
                let props = &mut panel.props;
                if !props.tables.contains_key(&list) {
                    let Some(mut table) = props
                        .data
                        .as_deref()
                        .and_then(|data| PropertyTable::build(list, data))
                    else {
                        return;
                    };
                    table.measure_widths(&text_measurer(window));
                    table.refilter(filter);
                    props.tables.insert(list, table);
                } else if let Some(table) = props.tables.get_mut(&list) {
                    if table.filter_is_stale(filter) {
                        table.refilter(filter);
                    }
                }
            }
        }
    }

    /// Starts the background `table_properties` fetch for `key`.
    fn fetch_table_properties(
        &mut self,
        tab_id: u64,
        key: PropertiesKey,
        cx: &mut Context<Self>,
    ) {
        let Some(runtime) = self.workspace.runtime_mut(tab_id) else {
            return;
        };
        runtime.columns_panel.props.begin(key.clone());
        if let Some(service) = self.scenario_ui.service.as_ref() {
            let result = crate::ui_fixture_properties::table_properties(
                service.scenario().fixture,
                &key.connection,
                &key.schema,
                &key.object,
            );
            cx.spawn(async move |this, cx| {
                let _ = this.update(cx, |this, cx| {
                    this.apply_table_properties(tab_id, key, result, cx);
                });
            })
            .detach();
            return;
        }
        self.fetch_driver_table_properties(tab_id, key, cx);
    }

    /// The driver route of [`Self::fetch_table_properties`]: `describe_object`
    /// on the connection's session, off the UI thread. A driver without
    /// `metadata.table_properties` answers with nothing to show (every list
    /// hidden, Columns from the result's own metadata) and is never called.
    fn fetch_driver_table_properties(
        &mut self,
        tab_id: u64,
        key: PropertiesKey,
        cx: &mut Context<Self>,
    ) {
        let describes = self.driver_describes_tables(&key.connection);
        if !self.driver_runtime.sessions.contains_key(&key.connection) {
            let result = Err(DescribeError::NotConnected.to_string());
            return self.apply_table_properties(tab_id, key, result, cx);
        }
        if !describes {
            return self.apply_table_properties(tab_id, key, Ok(TableProperties::default()), cx);
        }
        let work = match self.spawn_describe(&key, cx) {
            Ok(work) => work,
            Err(error) => return self.apply_table_properties(tab_id, key, Err(error.to_string()), cx),
        };
        cx.spawn(async move |this, cx| {
            let result = work.await;
            let _ = this.update(cx, |this, cx| {
                if result.as_ref().is_err_and(DescribeError::is_connection_lost) {
                    this.mark_tab_connection_lost(tab_id);
                }
                let result = result.map_err(|error| {
                    crate::sessions::redact_secrets(&error.to_string())
                });
                this.apply_table_properties(tab_id, key, result, cx);
            });
        })
        .detach();
    }

    /// `describe_object` of `key` on its connection's session, off the UI thread. Lock-free: a
    /// slow describe never holds the session lock.
    pub(in crate::app) fn spawn_describe(
        &self,
        key: &PropertiesKey,
        cx: &mut Context<Self>,
    ) -> Result<gpui::Task<Result<TableProperties, DescribeError>>, DescribeError> {
        let session = self
            .driver_runtime
            .sessions
            .get(&key.connection)
            .cloned()
            .ok_or(DescribeError::NotConnected)?;
        let request = serde_json::to_value(wire::SqlObjectRef {
            database: Some(key.database.clone()).filter(|database| !database.is_empty()),
            schema: Some(key.schema.clone()).filter(|schema| !schema.is_empty()),
            object: key.object.clone(),
        })
        .map_err(DescribeError::Request)?;
        Ok(cx.background_executor().spawn(async move {
            smol::unblock(move || -> Result<TableProperties, DescribeError> {
                let described = session.describe_object(&request)?;
                serde_json::from_value::<TableProperties>(described)
                    .map_err(DescribeError::Unreadable)
            })
            .await
        }))
    }

    /// The foreign key form of tab `tab_id` chose `table` (a table of the tab's schema): describes
    /// it the way the Properties does and hands its column names to the form.
    pub(in crate::app) fn describe_referenced(&mut self, tab_id: u64, table: String, cx: &mut Context<Self>) {
        let Some(tab) = self.workspace.tab(tab_id) else {
            return;
        };
        let key = PropertiesKey {
            connection: tab.connection_id.clone(),
            database: tab.database.clone(),
            schema: tab.schema.clone().unwrap_or_default(),
            object: table.clone(),
            session: self.driver_runtime.connection_ids.get(&tab.connection_id).copied().unwrap_or(0),
        };
        self.describe_table(key, cx, move |this, result, cx| {
            let columns = result.map(|properties| properties.columns.into_iter().map(|column| column.name).collect());
            if let Some(edit) = this.properties_edits.get(&tab_id).cloned() {
                edit.update(cx, |edit, cx| edit.referenced_described(&table, columns, cx));
            }
        });
    }

    /// Describes `key` the way the Properties does (the UI fixture's answer, or `describe_object` off
    /// the UI thread) and hands the result to `deliver` on the UI thread.
    pub(in crate::app) fn describe_table(
        &mut self,
        key: PropertiesKey,
        cx: &mut Context<Self>,
        deliver: impl FnOnce(&mut Self, Result<TableProperties, String>, &mut Context<Self>) + 'static,
    ) {
        if let Some(service) = self.scenario_ui.service.as_ref() {
            let result = crate::ui_fixture_properties::table_properties(service.scenario().fixture, &key.connection, &key.schema, &key.object);
            cx.spawn(async move |this, cx| {
                let _ = this.update(cx, |this, cx| deliver(this, result, cx));
            })
            .detach();
            return;
        }
        let work = match self.spawn_describe(&key, cx) {
            Ok(work) => work,
            Err(error) => return deliver(self, Err(error.to_string()), cx),
        };
        cx.spawn(async move |this, cx| {
            let result = work.await.map_err(|error| crate::sessions::redact_secrets(&error.to_string()));
            let _ = this.update(cx, |this, cx| deliver(this, result, cx));
        })
        .detach();
    }

    fn apply_table_properties(
        &mut self,
        tab_id: u64,
        key: PropertiesKey,
        result: Result<TableProperties, String>,
        cx: &mut Context<Self>,
    ) {
        if let Some(runtime) = self.workspace.runtime_mut(tab_id) {
            runtime.columns_panel.props.finish(&key, result);
        }
        self.notify_tab(tab_id, cx);
    }

    /// Manual refresh of the Properties view: refetches the table's metadata
    /// while the cached data stays on screen.
    pub(in crate::app) fn refresh_table_properties(&mut self, tab_id: u64, cx: &mut Context<Self>) {
        if let Some(runtime) = self.workspace.runtime_mut(tab_id) {
            runtime.columns_panel.props.invalidate();
        }
        self.notify_tab(tab_id, cx);
    }

    /// Sidebar Refresh of a node: every open result whose table lies under it
    /// refetches its properties the next time Properties is shown.
    pub(in crate::app) fn invalidate_table_properties(&mut self, node: &NavigatorNodeId) {
        let (connection, database, schema, object) = match node {
            NavigatorNodeId::Connection { connection } => (connection, None, None, None),
            NavigatorNodeId::Database { connection, database } => {
                (connection, Some(database.as_str()), None, None)
            }
            NavigatorNodeId::Schema { connection, database, schema }
            | NavigatorNodeId::ObjectGroup { connection, database, schema, .. } => {
                (connection, Some(database.as_str()), Some(schema.as_str()), None)
            }
            NavigatorNodeId::Object { connection, database, schema, object }
            | NavigatorNodeId::Column { connection, database, schema, object, .. } => (
                connection,
                Some(database.as_str()),
                Some(schema.as_str()),
                Some(object.as_str()),
            ),
            NavigatorNodeId::Group { .. }
            | NavigatorNodeId::SchemaObject { .. }
            | NavigatorNodeId::Provider { .. } => return,
        };
        let ids: Vec<u64> = self.workspace.tabs().iter().map(|tab| tab.id).collect();
        for id in ids {
            if let Some(props) = self.workspace.runtime_mut(id).map(|runtime| &mut runtime.columns_panel.props) {
                if props
                    .key
                    .as_ref()
                    .is_some_and(|key| key.covered_by(connection, database, schema, object))
                {
                    props.invalidate();
                }
            }
        }
    }

    /// Refresh icon at the right end of the sub-tab strip (table-backed
    /// results only); disabled while a fetch is in flight.
    pub(in crate::app) fn render_properties_refresh(
        &self,
        tab_id: u64,
        cx: &mut Context<Self>,
    ) -> Option<AnyElement> {
        let props = &self.workspace.runtime(tab_id)?.columns_panel.props;
        props.key.as_ref()?;
        let loading = props.status == LoadStatus::Loading;
        Some(
            div()
                .debug_selector(|| "properties-refresh".into())
                .flex_shrink_0()
                .child(
                    Button::new(("properties-refresh", tab_id))
                        .icon(Icon::empty().path("icons/refresh-cw.svg").size(px(14.0)))
                        .with_size(Size::Small)
                        .compact()
                        .ghost()
                        .disabled(loading)
                        .tooltip("Refresh table properties")
                        .on_click(cx.listener(move |this, _, _, cx| {
                            this.refresh_table_properties(tab_id, cx);
                        })),
                )
                .into_any_element(),
        )
    }

    /// "Retry" link of the error row: refetches the same key.
    pub(in crate::app) fn retry_table_properties(&mut self, tab_id: u64, cx: &mut Context<Self>) {
        let key = self
            .workspace
            .runtime(tab_id)
            .and_then(|runtime| runtime.columns_panel.props.key.clone());
        if let Some(key) = key {
            self.fetch_table_properties(tab_id, key, cx);
        }
        self.notify_tab(tab_id, cx);
    }

    /// Sub-tab click: switches tab; the filter (one input shared by the
    /// tabs) starts empty for the new tab.
    pub(in crate::app) fn select_properties_tab(
        &mut self,
        tab_id: u64,
        tab: PropertiesTab,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        // What is being typed in a Columns cell is part of the draft before the grid goes away.
        self.commit_properties_edit_cell(tab_id, window, cx);
        let Some(runtime) = self.workspace.runtime_mut(tab_id) else {
            return;
        };
        if runtime.columns_panel.properties_tab != tab {
            runtime.columns_panel.properties_tab = tab;
            runtime.columns_panel.filter.clear();
            if let Some(input) = self.columns_filter_inputs.get(&tab_id).cloned() {
                input.update(cx, |state, cx| {
                    state.set_value("", window, cx);
                    state.set_placeholder(filter_placeholder(tab), window, cx);
                });
            }
        }
        self.notify_tab(tab_id, cx);
    }

    /// Click / arrow keys on a list row: highlight it (nothing else follows).
    fn select_property_row(&mut self, tab_id: u64, tab: PropertiesTab, row: usize, cx: &mut Context<Self>) {
        if let Some(table) = self
            .workspace
            .runtime_mut(tab_id)
            .and_then(|runtime| runtime.columns_panel.props.tables.get_mut(&tab))
        {
            table.selected = Some(row);
            if let Some(position) = table.position_of(row) {
                table
                    .scroll
                    .scroll_to_item(position, gpui::ScrollStrategy::Nearest);
            }
        }
        self.notify_tab(tab_id, cx);
    }

    /// DDL toolbar "Copy": the whole statement text to the clipboard.
    pub(in crate::app) fn copy_table_ddl(&mut self, tab_id: u64, cx: &mut Context<Self>) {
        let ddl = self
            .workspace
            .runtime(tab_id)
            .and_then(|runtime| runtime.columns_panel.props.data.as_ref()?.ddl.clone());
        if let Some(ddl) = ddl {
            cx.write_to_clipboard(gpui::ClipboardItem::new_string(ddl));
        }
    }

    /// Loading / error row shown under the tab strip while the properties are
    /// being fetched for the first time, or after a failed fetch (with a
    /// Retry link). Columns keeps rendering below it either way.
    pub(in crate::app) fn render_properties_status(
        &self,
        tab_id: u64,
        cx: &mut Context<Self>,
    ) -> Option<AnyElement> {
        let props = &self.workspace.runtime(tab_id)?.columns_panel.props;
        let row = || {
            div()
                .h(px(26.0))
                .w_full()
                .flex_shrink_0()
                .flex()
                .items_center()
                .gap(px(8.0))
                .px(px(12.0))
                .text_size(px(11.0))
                .text_color(theme::text_muted())
                .border_b_1()
                .border_color(theme::border())
        };
        match &props.status {
            LoadStatus::Loading if props.data.is_none() => Some(
                row()
                    .debug_selector(|| "properties-loading".into())
                    .child("Loading keys, indexes and DDL\u{2026}")
                    .into_any_element(),
            ),
            LoadStatus::Failed(error) => {
                let full = error.clone();
                Some(
                    row()
                        .debug_selector(|| "properties-error".into())
                        .child(
                            div()
                                .id(("properties-error-text", tab_id))
                                .min_w_0()
                                .truncate()
                                .tooltip(move |window, cx| {
                                    gpui_component::tooltip::Tooltip::new(full.clone()).build(window, cx)
                                })
                                .child(format!("Couldn't load table properties: {error}")),
                        )
                        .child(
                            div()
                                .id(("properties-retry", tab_id))
                                .debug_selector(|| "properties-retry".into())
                                .flex_shrink_0()
                                .cursor_pointer()
                                .text_color(theme::accent())
                                .hover(|style| style.underline())
                                .on_click(cx.listener(move |this, _, _, cx| {
                                    this.retry_table_properties(tab_id, cx);
                                }))
                                .child("Retry"),
                        )
                        .into_any_element(),
                )
            }
            _ => None,
        }
    }

    /// Toolbar row shared by the list tabs and DDL: `<Tab> (N)` + `right`.
    pub(in crate::app) fn properties_toolbar(title: String, right: impl IntoElement) -> Div {
        div()
            .debug_selector(|| "properties-toolbar".into())
            .h(px(COLUMNS_TOOLBAR_HEIGHT))
            .flex_shrink_0()
            .flex()
            .items_center()
            .gap(px(12.0))
            .px(px(12.0))
            .border_b_1()
            .border_color(theme::border())
            .bg(theme::grid_header())
            .child(
                div()
                    .debug_selector(|| "properties-title".into())
                    .flex_shrink_0()
                    .whitespace_nowrap()
                    .text_size(px(12.0))
                    .text_color(theme::text())
                    .child(title),
            )
            .child(right)
    }

    /// Content of a non-Columns sub-tab.
    pub(in crate::app) fn render_properties_content(
        &mut self,
        tab_id: u64,
        tab: PropertiesTab,
        cx: &mut Context<Self>,
    ) -> gpui::Stateful<Div> {
        let columns_focus = self.view_mut(tab_id).columns_focus(cx);
        if tab == PropertiesTab::Ddl {
            return self.render_ddl_content(tab_id, cx);
        }
        // Indexes and Foreign keys draw the draft's rows in edit mode.
        let edit = self.properties_edit_render(tab_id, cx);
        if let Some(state) = edit.clone().filter(|_| matches!(tab, PropertiesTab::Indexes | PropertiesTab::ForeignKeys)) {
            return self.render_edit_list_content(tab_id, tab, state, cx);
        }
        let pending_key = edit.as_ref().filter(|_| tab == PropertiesTab::Keys).and_then(|state| state.lists.key.clone());
        let Some(table) = self
            .workspace
            .runtime(tab_id)
            .and_then(|runtime| runtime.columns_panel.props.tables.get(&tab))
        else {
            // Model not built yet (built at the top of the next frame).
            return div().id(("properties-content", tab_id)).flex_1();
        };
        let (columns, rows, widths, filtered) = (
            table.columns.clone(),
            table.rows.clone(),
            table.widths.clone(),
            table.filtered.clone(),
        );
        let selected = table.selected;
        let table_width = table.total_width();
        let title = table.title();
        let empty_text = if rows.is_empty() {
            format!("No {}", tab.label().to_lowercase())
        } else {
            format!("No matching {}", tab.label().to_lowercase())
        };
        let cell = |width: f32, header: bool| {
            div()
                .w(px(width))
                .h_full()
                .flex_shrink_0()
                .px(px(CELL_PADDING))
                .flex()
                .items_center()
                .overflow_hidden()
                .border_r_1()
                .border_color(theme::border())
                .when(header, |this| this.text_size(px(11.0)).text_color(theme::text_muted()))
        };
        let header = div()
            .debug_selector(|| "properties-table-header".into())
            .h(px(HEADER_ROW_HEIGHT))
            .w(px(table_width))
            .flex_shrink_0()
            .flex()
            .items_center()
            .bg(theme::grid_header())
            .border_b_1()
            .border_color(theme::border())
            .children(
                columns
                    .iter()
                    .enumerate()
                    .map(|(index, column)| cell(widths[index], true).child(column.header)),
            );
        // The primary key's row is marked while the draft changes the key.
        let marked_key = pending_key.is_some();
        let list = {
            let (columns, rows, widths, filtered) =
                (columns.clone(), rows.clone(), widths.clone(), filtered.clone());
            uniform_list(
                (tab.selector(), tab_id),
                filtered.len(),
                cx.processor(move |_this, range: Range<usize>, _window, cx| {
                    range
                        .map(|position| {
                            let row_index = filtered[position];
                            let row = &rows[row_index];
                            let is_selected = selected == Some(row_index);
                            let key_row = marked_key
                                && columns.iter().position(|column| column.header == "Type").is_some_and(|at| row.cells[at].as_ref() == "PK");
                            div()
                                .id(("properties-row", position))
                                .debug_selector(move || format!("properties-row-{position}"))
                                .h(px(ROW_HEIGHT))
                                .w(px(table_width))
                                .flex()
                                .items_center()
                                .text_size(px(12.0))
                                .cursor_pointer()
                                .when(is_selected, |this| this.bg(theme::accent_surface()))
                                .when(!is_selected, |this| this.hover(|style| style.bg(theme::hover())))
                                .border_b_1()
                                .border_color(theme::divider())
                                .on_click(cx.listener(move |this, _, window, cx| {
                                    this.view_mut(tab_id).columns_focus(cx).focus(window, cx);
                                    this.select_property_row(tab_id, tab, row_index, cx);
                                }))
                                .children(columns.iter().enumerate().map(|(index, column)| {
                                    let text = row.cells[index].clone();
                                    let text = if key_row && index == 0 { gpui::SharedString::from(format!("* {text}")) } else { text };
                                    let content = div()
                                        .min_w_0()
                                        .truncate()
                                        .text_color(match column.style {
                                            _ if key_row && index == 0 => theme::warning(),
                                            CellStyle::Definition => theme::text_muted(),
                                            CellStyle::Name | CellStyle::Plain => theme::text(),
                                        });
                                    let content = match (column.style, row.tooltip.clone()) {
                                        (CellStyle::Definition, Some(full)) => content
                                            .id(("properties-definition", position))
                                            .tooltip(move |window, cx| {
                                                gpui_component::tooltip::Tooltip::new(full.clone())
                                                    .build(window, cx)
                                            })
                                            .child(text)
                                            .into_any_element(),
                                        _ => content.child(text).into_any_element(),
                                    };
                                    cell(widths[index], false).child(content)
                                }))
                                .into_any_element()
                        })
                        .collect()
                }),
            )
            .track_scroll(&table.scroll)
            .flex_1()
            .min_h_0()
        };
        let filter_input = self.columns_filter_inputs.get(&tab_id).cloned();
        let filter = div()
            .debug_selector(|| "columns-view-filter".into())
            .w(px(220.0))
            .min_w(px(96.0))
            .flex_shrink(1.0)
            .children(filter_input.map(|input| Input::new(&input).small().cleanable(true)));
        let empty = filtered.is_empty();
        // The Keys sub-tab carries the structure edit controls too (its rows are read-only here).
        let edit_toolbar = (tab == PropertiesTab::Keys)
            .then(|| self.render_properties_edit_toolbar(tab_id, tab, cx))
            .flatten();
        let editing = edit_toolbar.is_some() && edit.is_some();
        let filter = div().flex().items_center().gap(px(12.0)).child(filter).children(edit_toolbar);

        div()
            .id(("properties-content", tab_id))
            .track_focus(&columns_focus)
            .when(editing, |this| {
                this.key_context("properties-edit")
                    .on_action(cx.listener(move |this, _: &PropertiesUndo, _, cx| this.with_properties_edit(tab_id, cx, |edit, cx| edit.undo(cx))))
                    .on_action(cx.listener(move |this, _: &PropertiesRedo, _, cx| this.with_properties_edit(tab_id, cx, |edit, cx| edit.redo(cx))))
            })
            .on_key_down(cx.listener(move |this, event: &gpui::KeyDownEvent, _, cx| {
                let delta = match event.keystroke.key.as_str() {
                    "down" => 1,
                    "up" => -1,
                    _ => return,
                };
                let target = this
                    .workspace
                    .runtime(tab_id)
                    .and_then(|runtime| runtime.columns_panel.props.tables.get(&tab))
                    .and_then(|table| table.step_selection(delta));
                if let Some(row) = target {
                    this.select_property_row(tab_id, tab, row, cx);
                    cx.stop_propagation();
                }
            }))
            .debug_selector(|| "properties-content".into())
            .flex_1()
            .min_w_0()
            .min_h_0()
            .flex()
            .flex_col()
            .child(Self::properties_toolbar(title, filter))
            .children(pending_key.map(|key| {
                div()
                    .debug_selector(|| "properties-edit-key-pending".into())
                    .flex_shrink_0()
                    .px(px(12.0))
                    .py(px(4.0))
                    .text_size(px(11.0))
                    .text_color(theme::warning())
                    .border_b_1()
                    .border_color(theme::border())
                    .child(key.summary())
            }))
            .child(header)
            .child(if empty {
                div()
                    .debug_selector(|| "properties-empty".into())
                    .flex_1()
                    .flex()
                    .items_center()
                    .justify_center()
                    .text_size(px(11.0))
                    .text_color(theme::text_muted())
                    .child(empty_text)
                    .into_any_element()
            } else {
                list.into_any_element()
            })
    }

    /// DDL sub-tab: toolbar (title + Copy) above the read-only SQL editor.
    fn render_ddl_content(&self, tab_id: u64, cx: &mut Context<Self>) -> gpui::Stateful<Div> {
        let editor = self
            .workspace
            .runtime(tab_id)
            .and_then(|runtime| runtime.columns_panel.props.ddl.as_ref())
            .map(|ddl| ddl.state.clone());
        let copy = div().debug_selector(|| "properties-ddl-copy".into()).child(
            Button::new(("properties-ddl-copy", tab_id))
                .icon(Icon::empty().path("icons/copy.svg").size(px(14.0)))
                .label("Copy")
                .with_size(Size::Small)
                .compact()
                .outline()
                .on_click(cx.listener(move |this, _, _, cx| this.copy_table_ddl(tab_id, cx))),
        );
        div()
            .id(("properties-content", tab_id))
            .debug_selector(|| "properties-content".into())
            .flex_1()
            .min_w_0()
            .min_h_0()
            .flex()
            .flex_col()
            .child(Self::properties_toolbar("DDL".into(), copy))
            .child(match editor {
                Some(state) => div()
                    .debug_selector(|| "properties-ddl-editor".into())
                    .flex_1()
                    .min_h_0()
                    .font_family("Menlo")
                    .text_size(px(12.0))
                    .child(Editor::new(&state).readonly(true).appearance(false).bordered(false).h_full())
                    .into_any_element(),
                None => div().flex_1().into_any_element(),
            })
    }
}

/// Why a driver's table properties could not be fetched.
#[derive(Debug)]
pub(in crate::app) enum DescribeError {
    NotConnected,
    Request(serde_json::Error),
    /// The driver rejected the call; keeps its
    /// `driver rejected the request: <code>: <message>` text for the Error Log.
    Driver(engine_driver_host::LoaderError),
    Unreadable(serde_json::Error),
}

impl std::fmt::Display for DescribeError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::NotConnected => f.write_str("The connection is not open"),
            Self::Request(error) => std::fmt::Display::fmt(error, f),
            Self::Driver(error) => std::fmt::Display::fmt(error, f),
            Self::Unreadable(error) => write!(f, "Unreadable table properties: {error}"),
        }
    }
}

impl std::error::Error for DescribeError {}

impl DescribeError {
    /// The driver said the session is gone (`<area>.not_connected`).
    fn is_connection_lost(&self) -> bool {
        matches!(
            self,
            Self::Driver(engine_driver_host::LoaderError::DriverRejected { code, .. })
                if crate::sessions::is_connection_lost_code(code)
        )
    }
}

impl From<engine_driver_host::LoaderError> for DescribeError {
    fn from(error: engine_driver_host::LoaderError) -> Self {
        Self::Driver(error)
    }
}

#[cfg(test)]
mod describe_error_tests {
    use super::*;

    #[test]
    fn only_a_not_connected_rejection_is_a_lost_connection() {
        let rejected = |code: &str| {
            DescribeError::Driver(engine_driver_host::LoaderError::DriverRejected {
                code: code.into(),
                message: "gone".into(),
                retryable: false,
            })
        };
        assert!(rejected("describe.not_connected").is_connection_lost());
        assert!(!rejected("describe.failed").is_connection_lost());
        assert!(!DescribeError::NotConnected.is_connection_lost());
    }
}
