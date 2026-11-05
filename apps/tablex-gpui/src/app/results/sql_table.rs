//! WHERE / ORDER BY bar of `sqlTable` tabs (SQL table-data tabs).
//!
//! Mirrors the Mongo result toolbar: `Query | WHERE … ⛶ | ORDER BY … ⛶ | ▶`.
//! The two fields are single-line `Input`s whose text is only an editor
//! buffer (`Tab::table_where` / `Tab::table_order_by`). The query itself is
//! built from the *applied* fragments (`Tab::applied_where` /
//! `Tab::applied_order_by`), which change only on Run/Enter/Clear/sort, so
//! paging and (auto-)refresh keep re-running what the user executed. The
//! statement is rebuilt on every run (`crate::sql_table::build_table_sql`),
//! so there is no per-frame work beyond reading two short strings.

use super::super::*;
use crate::sql_table::{self, SqlDialect};


/// Smallest width of a WHERE / ORDER BY field before it clips inside the
/// fields lane (label + a few characters + its buttons).
pub(in crate::app) const FIELD_MIN_WIDTH: f32 = 72.0;

#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub(in crate::app) enum SqlField {
    Where,
    OrderBy,
}

impl SqlField {
    fn label(self) -> &'static str {
        match self {
            Self::Where => "WHERE",
            Self::OrderBy => "ORDER BY",
        }
    }

    fn placeholder(self) -> &'static str {
        match self {
            Self::Where => "id > 10 AND status = 'active'",
            Self::OrderBy => "created DESC",
        }
    }

    fn title(self) -> &'static str {
        match self {
            Self::Where => "WHERE Condition",
            Self::OrderBy => "ORDER BY Terms",
        }
    }

    fn slug(self) -> &'static str {
        match self {
            Self::Where => "sql-where",
            Self::OrderBy => "sql-order",
        }
    }

    /// Element ids per control (`ElementId` needs `&'static str` prefixes).
    fn id(self, control: Control) -> &'static str {
        match (self, control) {
            (Self::Where, Control::Close) => "sql-where-close",
            (Self::Where, Control::Execute) => "sql-where-execute",
            (Self::Where, Control::Panel) => "sql-where-panel",
            (Self::Where, Control::Popover) => "sql-where-popover",
            (Self::Where, Control::Expand) => "sql-where-expand-button",
            (Self::Where, Control::Clear) => "sql-where-clear",
            (Self::OrderBy, Control::Close) => "sql-order-close",
            (Self::OrderBy, Control::Execute) => "sql-order-execute",
            (Self::OrderBy, Control::Panel) => "sql-order-panel",
            (Self::OrderBy, Control::Popover) => "sql-order-popover",
            (Self::OrderBy, Control::Expand) => "sql-order-expand-button",
            (Self::OrderBy, Control::Clear) => "sql-order-clear",
        }
    }
}

#[derive(Clone, Copy)]
enum Control {
    Close,
    Execute,
    Panel,
    Popover,
    Expand,
    Clear,
}

#[derive(Default)]
pub(in crate::app) struct SqlTableUiState {
    inputs: HashMap<(u64, SqlField), gpui::Entity<InputState>>,
    editors: HashMap<(u64, SqlField), gpui::Entity<EditorState>>,
    pub(in crate::app) filter: super::sql_filter::SqlFilterUiState,
    pub(in crate::app) column_search: HashMap<u64, gpui::Entity<InputState>>,
}

impl SqlTableUiState {
    /// Current text of a bar input (tests / debugging).
    #[cfg(test)]
    pub(in crate::app) fn input_value(
        &self,
        tab_id: u64,
        field: SqlField,
        cx: &gpui::App,
    ) -> Option<String> {
        self.inputs
            .get(&(tab_id, field))
            .map(|input| input.read(cx).value().to_string())
    }

    pub(in crate::app) fn remove_tab(&mut self, tab_id: u64) {
        self.inputs.retain(|(id, _), _| *id != tab_id);
        self.editors.retain(|(id, _), _| *id != tab_id);
        self.filter.remove_tab(tab_id);
        self.column_search.remove(&tab_id);
    }
}

fn tab_field(tab: &Tab, field: SqlField) -> &str {
    match field {
        SqlField::Where => &tab.table_where,
        SqlField::OrderBy => &tab.table_order_by,
    }
}

fn single_line(text: &str) -> String {
    text.replace(['\n', '\r'], " ")
}

impl AppRoot {
    /// Dialect of a connection: the syntax its driver advertises
    /// (`DriverCapabilities::sql_dialect`, read without the session lock),
    /// otherwise the per-kind default for connections whose driver advertises
    /// none (drivers that predate `sql_dialect`).
    pub(in crate::app) fn sql_dialect_for_connection(&self, connection: &str) -> Option<SqlDialect> {
        let saved = self
            .saved_connections
            .iter()
            .find(|saved| saved.matches_id(connection));
        let advertised = self
            .driver_capabilities(
                connection,
                saved.map_or("", |saved| saved.kind.driver_id()),
            )
            .and_then(|capabilities| capabilities.sql_dialect.as_ref());
        match advertised {
            Some(info) => Some(SqlDialect::from_capabilities(info)),
            None => saved.map(|saved| SqlDialect::for_kind(saved.kind)),
        }
    }

    /// How `connection`'s engine spells a byte string in SQL text.
    pub(in crate::app) fn binary_literal_for_connection(
        &self,
        connection: &str,
    ) -> crate::sql_table::BinaryLiteral {
        self.saved_connections
            .iter()
            .find(|saved| saved.matches_id(connection))
            .map(|saved| crate::sql_table::BinaryLiteral::for_kind(saved.kind))
            .unwrap_or_default()
    }

    /// Dialect used to quote identifiers for `tab`'s connection.
    pub(in crate::app) fn sql_table_dialect(&self, tab: &Tab) -> SqlDialect {
        self.sql_dialect_for_connection(&tab.connection_id)
            .unwrap_or_else(|| {
                if tab.query_mode.is_sqlite_table() {
                    SqlDialect::SQLITE
                } else {
                    SqlDialect::POSTGRES
                }
            })
    }

    /// Statement a `sqlTable` tab runs now (paging, refresh, auto-refresh):
    /// built from the APPLIED fragments, never the live editor text. `Err`
    /// carries the validation message shown in the bar.
    pub(in crate::app) fn sql_table_statement(&self, tab: &Tab) -> Result<String, String> {
        sql_table::build_table_sql(
            self.sql_table_dialect(tab),
            tab.schema.as_deref(),
            &tab.collection,
            &tab.applied_where,
            &tab.applied_order_by,
        )
    }

    /// Statement the editor buffers WOULD run if submitted (preview and
    /// validation of Enter / Run).
    pub(in crate::app) fn sql_table_draft_statement(&self, tab: &Tab) -> Result<String, String> {
        sql_table::build_table_sql(
            self.sql_table_dialect(tab),
            tab.schema.as_deref(),
            &tab.collection,
            &tab.table_where,
            &tab.table_order_by,
        )
    }

    /// Enter / Execute / Run: validates the editor buffers of the `sqlTable`
    /// tab, applies them (resetting paging when they changed) and runs. A
    /// rejected buffer is reported in the bar and nothing is applied or run.
    /// While staged changes block running, nothing is applied either.
    pub(in crate::app) fn submit_sql_table_query(&mut self, tab_id: u64, cx: &mut Context<Self>) {
        let Some(tab) = self.workspace.tab(tab_id).cloned() else {
            return;
        };
        if tab.query_mode.is_table_data()
            && !self.workspace.tab_has_staged_changes(tab.id)
        {
            if let Err(error) = self.sql_table_draft_statement(&tab) {
                if let Some(runtime) = self.workspace.runtime_mut(tab.id) {
                    runtime.sql_table.error = Some(error);
                }
                self.notify_tab(tab_id, cx);
                return;
            }
            self.apply_sql_table_fragments(
                tab.id,
                Some(tab.table_where.as_str()),
                Some(tab.table_order_by.as_str()),
            );
        }
        self.run_tab_query(tab_id, cx);
    }

    /// Makes the given fragments the applied ones; paging returns to the first
    /// page when the applied query actually changed.
    fn apply_sql_table_fragments(
        &mut self,
        tab_id: u64,
        where_clause: Option<&str>,
        order_by: Option<&str>,
    ) {
        let Some(tab) = self.workspace.tab_mut(tab_id) else {
            return;
        };
        let mut changed = false;
        if let Some(value) = where_clause.filter(|value| *value != tab.applied_where) {
            tab.applied_where = value.to_string();
            changed = true;
        }
        if let Some(value) = order_by.filter(|value| *value != tab.applied_order_by) {
            tab.applied_order_by = value.to_string();
            changed = true;
        }
        if changed {
            if let Some(runtime) = self.workspace.runtime_mut(tab_id) {
                runtime.pagination.reset();
            }
        }
    }

    /// Opens (or re-activates) the table-data tab for a sidebar object.
    pub(in crate::app) fn open_sql_table_tab(
        &mut self,
        title: &str,
        connection: &str,
        database: &str,
        schema: &str,
        object: &str,
    ) -> (u64, bool) {
        if let Some(existing) = self
            .workspace
            .tabs()
            .iter()
            .find(|tab| {
                tab.kind == TabKind::Query
                    && tab.query_mode.is_table_data()
                    && tab.connection_id == connection
                    && tab.database == database
                    && tab.schema.as_deref() == Some(schema)
                    && tab.collection == object
            })
            .map(|tab| tab.id)
        {
            self.workspace.activate_tab(existing);
            return (existing, false);
        }
        let id = self.workspace.open_query_tab_with_metadata(
            title,
            TabMetadata {
                connection_id: connection.into(),
                database: database.into(),
                schema: Some(schema.into()),
                collection: object.into(),
                query_mode: Some(QueryMode::SqlTable),
                ..Default::default()
            },
        );
        let dialect = self
            .sql_dialect_for_connection(connection)
            .unwrap_or(SqlDialect::POSTGRES);
        let base = self
            .workspace
            .tab_mut(id)
            .and_then(|tab| sql_table::base_select(dialect, Some(schema), &tab.collection).ok())
            .unwrap_or_default();
        if let Some(tab) = self.workspace.tab_mut(id) {
            tab.sql = base;
        }
        (id, true)
    }

    pub(in crate::app) fn sql_table_can_run(&self, tab_id: u64) -> bool {
        self.workspace
            .runtime(tab_id)
            .is_some_and(|runtime| runtime.query.can_run())
    }

    pub(in crate::app) fn sql_table_input(
        &mut self,
        tab_id: u64,
        field: SqlField,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> gpui::Entity<InputState> {
        if let Some(input) = self.sql_table_ui.inputs.get(&(tab_id, field)) {
            return input.clone();
        }
        let value = self
            .workspace
            .tabs()
            .iter()
            .find(|tab| tab.id == tab_id)
            .map(|tab| single_line(tab_field(tab, field)))
            .unwrap_or_default();
        let input = cx.new(|cx| {
            InputState::new(window, cx)
                .placeholder(field.placeholder())
                .default_value(value)
        });
        let input_for_events = input.clone();
        cx.subscribe(
            &input,
            move |this: &mut Self,
                  _: gpui::Entity<InputState>,
                  event: &gpui_component::input::InputEvent,
                  cx: &mut Context<Self>| {
                match event {
                    gpui_component::input::InputEvent::Change => {
                        let expanded = this.workspace.runtime(tab_id).is_some_and(|runtime| {
                            match field {
                                SqlField::Where => runtime.sql_table.where_expanded,
                                SqlField::OrderBy => runtime.sql_table.order_expanded,
                            }
                        });
                        if expanded {
                            return;
                        }
                        let value = input_for_events.read(cx).value().to_string();
                        this.set_sql_table_field_text(tab_id, field, value);
                        this.notify_tab(tab_id, cx);
                    }
                    gpui_component::input::InputEvent::PressEnter { .. } => {
                        this.submit_sql_table_query(tab_id, cx);
                    }
                    _ => {}
                }
            },
        )
        .detach();
        self.sql_table_ui
            .inputs
            .insert((tab_id, field), input.clone());
        input
    }

    fn set_sql_table_field_text(&mut self, tab_id: u64, field: SqlField, value: String) {
        // The input re-emits Change with unchanged text (e.g. on Enter); that
        // must not wipe the validation error the Enter just produced.
        if self
            .workspace
            .tabs()
            .iter()
            .find(|tab| tab.id == tab_id)
            .is_some_and(|tab| tab_field(tab, field) == value)
        {
            return;
        }
        if let Some(tab) = self.workspace.tab_mut(tab_id) {
            match field {
                SqlField::Where => tab.table_where = value,
                SqlField::OrderBy => tab.table_order_by = value,
            }
        }
        if let Some(runtime) = self.workspace.runtime_mut(tab_id) {
            runtime.sql_table.error = None;
        }
    }

    fn sql_table_expanded_editor(
        &mut self,
        tab_id: u64,
        field: SqlField,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> gpui::Entity<EditorState> {
        if let Some(editor) = self.sql_table_ui.editors.get(&(tab_id, field)) {
            return editor.clone();
        }
        let value = self
            .workspace
            .tabs()
            .iter()
            .find(|tab| tab.id == tab_id)
            .map(|tab| tab_field(tab, field).to_string())
            .unwrap_or_default();
        let completion = TabCompletion::for_tab(cx, tab_id, CompletionRole::TableFragment);
        let editor = cx.new(|cx| {
            EditorState::new(window, cx)
                .language("sql")
                .line_number(false)
                .soft_wrap(false)
                .default_value(value)
                .completion(completion)
        });
        let editor_for_events = editor.clone();
        cx.subscribe_in(
            &editor,
            window,
            move |this: &mut Self,
                  _: &gpui::Entity<EditorState>,
                  event: &gpui_component::input::InputEvent,
                  window: &mut Window,
                  cx: &mut Context<Self>| {
                if matches!(event, gpui_component::input::InputEvent::Blur) {
                    this.view_mut(tab_id).sql_enter_held = false;
                    return;
                }
                if !matches!(event, gpui_component::input::InputEvent::Change) {
                    return;
                }
                let value = editor_for_events.read(cx).value().to_string();
                this.set_sql_table_field_text(tab_id, field, value.clone());
                if let Some(compact) = this
                    .sql_table_ui
                    .inputs
                    .get(&(tab_id, field))
                    .cloned()
                {
                    let compact_value = single_line(&value);
                    if compact.read(cx).value().as_ref() != compact_value {
                        compact.update(cx, |state, cx| state.set_value(compact_value, window, cx));
                    }
                }
                this.notify_tab(tab_id, cx);
            },
        )
        .detach();
        self.sql_table_ui
            .editors
            .insert((tab_id, field), editor.clone());
        editor
    }

    pub(in crate::app) fn set_sql_table_expanded(
        &mut self,
        tab_id: u64,
        field: SqlField,
        expanded: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let value = self
            .workspace
            .tabs()
            .iter()
            .find(|tab| tab.id == tab_id)
            .map(|tab| tab_field(tab, field).to_string())
            .unwrap_or_default();
        if let Some(runtime) = self.workspace.runtime_mut(tab_id) {
            match field {
                SqlField::Where => runtime.sql_table.where_expanded = expanded,
                SqlField::OrderBy => runtime.sql_table.order_expanded = expanded,
            }
        }
        if expanded {
            if let Some(editor) = self.sql_table_ui.editors.get(&(tab_id, field)).cloned() {
                if editor.read(cx).value().as_ref() != value.as_str() {
                    editor.update(cx, |state, cx| state.set_value(value, window, cx));
                }
            }
        }
        self.notify_tab(tab_id, cx);
    }

    /// Writes `value` into a field (tab state + both editors) without
    /// running; programmatic `set_value` emits no Change event.
    pub(in crate::app) fn write_sql_table_field(
        &mut self,
        tab_id: u64,
        field: SqlField,
        value: &str,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.set_sql_table_field_text(tab_id, field, value.to_string());
        if let Some(input) = self.sql_table_ui.inputs.get(&(tab_id, field)).cloned() {
            let flat = single_line(value);
            if input.read(cx).value().as_ref() != flat {
                input.update(cx, |state, cx| state.set_value(flat, window, cx));
            }
        }
        if let Some(editor) = self.sql_table_ui.editors.get(&(tab_id, field)).cloned() {
            if editor.read(cx).value().as_ref() != value {
                editor.update(cx, |state, cx| state.set_value(value.to_string(), window, cx));
            }
        }
    }

    /// Clear ×: empties the field and re-runs (default order / all rows).
    pub(in crate::app) fn clear_sql_table_field(
        &mut self,
        tab_id: u64,
        field: SqlField,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if field == SqlField::Where {
            // The builder no longer describes the WHERE text.
            if let Some(tab) = self.workspace.tab_mut(tab_id) {
                tab.table_filter.generated_where = None;
            }
        }
        self.write_sql_table_field(tab_id, field, "", window, cx);
        self.rerun_sql_table(tab_id, field, "", cx);
    }

    /// Header click / context-menu sort: replaces ORDER BY and re-runs.
    pub(in crate::app) fn set_sql_table_order_by(
        &mut self,
        tab_id: u64,
        order_by: &str,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.write_sql_table_field(tab_id, SqlField::OrderBy, order_by, window, cx);
        self.rerun_sql_table(tab_id, SqlField::OrderBy, order_by, cx);
    }

    /// Clear / sort: applies just `field` (the other field's unsent draft
    /// stays a draft) and re-runs from the first page. Nothing is applied
    /// while staged changes block the run.
    pub(in crate::app) fn rerun_sql_table(&mut self, tab_id: u64, field: SqlField, value: &str, cx: &mut Context<Self>) {
        // No unconditional pagination reset: `apply_sql_table_fragments` resets
        // only when the applied query actually changes. Resetting here while
        // staged changes block the run left the pager on page 1 while the
        // grid still showed the old page.
        if !self.workspace.tab_has_staged_changes(tab_id) {
            match field {
                SqlField::Where => self.apply_sql_table_fragments(tab_id, Some(value), None),
                SqlField::OrderBy => self.apply_sql_table_fragments(tab_id, None, Some(value)),
            }
        }
        self.run_tab_query(tab_id, cx);
    }

    /// Header click on `column_name` of a `sqlTable` tab: ASC -> DESC -> default.
    pub(in crate::app) fn sort_sql_table_by_header(
        &mut self,
        tab_id: u64,
        column_name: &str,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(tab) = self.workspace.tab(tab_id).cloned() else {
            return;
        };
        if !tab.query_mode.is_table_data() {
            return;
        }
        if self.context_sort_blocked(tab.id) {
            window.open_alert_dialog(cx, |dialog, _, _| {
                dialog.title("Sort unavailable").child(
                    "Wait for the query and finish pending changes before sorting the table.",
                )
            });
            return;
        }
        let next = sql_table::next_sort(
            self.sql_table_dialect(&tab),
            &tab.applied_order_by,
            column_name,
        );
        self.set_sql_table_order_by(tab.id, &next, window, cx);
    }

    fn render_sql_expand_popover(
        &mut self,
        tab_id: u64,
        field: SqlField,
        editor: Option<gpui::Entity<EditorState>>,
        trigger: Button,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let open = editor.is_some();
        let can_run = self.sql_table_can_run(tab_id);
        let slug = field.slug();
        let root = cx.entity().downgrade();
        let panel = if let Some(editor) = editor {
            let editor_for_key = editor.clone();
            let editor_area = div()
                .debug_selector(move || format!("{slug}-editor-area"))
                .flex_1()
                .min_h(px(0.0))
                .p(px(10.0))
                .on_key_up(cx.listener(move |this, event: &gpui::KeyUpEvent, _, _| {
                    if event.keystroke.key == "enter" {
                        this.view_mut(tab_id).sql_enter_held = false;
                    }
                }))
                // Enter is taken as the editor's action, before the editor sees it, except while
                // its completion menu is open (the menu then accepts the highlighted item) and for
                // Shift+Enter (a newline). A key-down listener cannot tell: it runs after the
                // editor and its menu consumed the action, when the menu already reads as closed.
                .capture_action(cx.listener(
                    move |this, action: &gpui_component::input::Enter, _, cx| {
                        if action.shift || editor_for_key.read(cx).completion_menu_state().open {
                            return;
                        }
                        cx.stop_propagation();
                        if can_run && !this.view(tab_id).sql_enter_held {
                            this.view_mut(tab_id).sql_enter_held = true;
                            this.submit_sql_table_query(tab_id, cx);
                        }
                    },
                ))
                .child(Editor::new(&editor).appearance(true).h_full());
            let close = Button::new((field.id(Control::Close), tab_id))
                .label("Close")
                .compact()
                .ghost()
                .on_click(cx.listener(move |this, _, window, cx| {
                    this.set_sql_table_expanded(tab_id, field, false, window, cx);
                }));
            let execute = Button::new((field.id(Control::Execute), tab_id))
                .label("Execute")
                .compact()
                .primary()
                .disabled(!can_run)
                .on_click(cx.listener(move |this, _, _, cx| {
                    this.submit_sql_table_query(tab_id, cx);
                }));
            div()
                .id((field.id(Control::Panel), tab_id))
                .debug_selector(move || format!("{slug}-panel"))
                .w(px(520.0))
                .h(px(220.0))
                .flex()
                .flex_col()
                .overflow_hidden()
                .bg(theme::surface())
                .child(
                    div()
                        .h(px(40.0))
                        .px(px(12.0))
                        .flex()
                        .items_center()
                        .justify_between()
                        .child(div().text_size(px(13.0)).child(field.title()))
                        .child(close),
                )
                .child(div().h(px(1.0)).w_full().bg(theme::border()))
                .child(editor_area)
                .child(
                    div()
                        .h(px(40.0))
                        .px(px(12.0))
                        .flex()
                        .items_center()
                        .justify_between()
                        .border_t_1()
                        .border_color(theme::border())
                        .child(
                            div()
                                .text_size(px(10.0))
                                .text_color(theme::text_muted())
                                .child("Enter executes. Shift+Enter inserts a newline."),
                        )
                        .child(execute),
                )
                .into_any_element()
        } else {
            div().into_any_element()
        };
        Popover::new((field.id(Control::Popover), tab_id))
            .trigger(trigger)
            .open(open)
            .p(px(0.0))
            .bg(theme::surface())
            .on_open_change(move |open, window, app| {
                let _ = root.update(app, |this, cx| {
                    this.set_sql_table_expanded(tab_id, field, *open, window, cx);
                });
            })
            .child(panel)
            .into_any_element()
    }

    fn render_sql_query_preview(
        &mut self,
        tab_id: u64,
        statement: Result<String, String>,
        open: bool,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let button = Button::new(("sql-query-preview-button", tab_id))
            .compact()
            .text()
            .selected(open)
            .child(
                div()
                    .debug_selector(|| "sql-query-toolbar-text".into())
                    .text_size(px(11.0))
                    .line_height(px(14.0))
                    .child("Query"),
            )
            .tooltip("Show generated SQL");
        let text = statement.clone().unwrap_or_else(|error| error);
        let copyable = statement.ok();
        let copy = Button::new(("sql-query-preview-copy", tab_id))
            .icon(Icon::new(IconName::Copy).size(px(14.0)))
            .compact()
            .ghost()
            .disabled(copyable.is_none())
            .tooltip("Copy query")
            .on_click(cx.listener(move |_, _, _, cx| {
                if let Some(sql) = copyable.clone() {
                    cx.write_to_clipboard(gpui::ClipboardItem::new_string(sql));
                }
            }));
        let close = Button::new(("sql-query-preview-close", tab_id))
            .icon(Icon::new(IconName::Close).size(px(14.0)))
            .compact()
            .ghost()
            .tooltip("Close preview")
            .on_click(cx.listener(move |this, _, _, cx| {
                this.set_sql_table_preview_open(tab_id, false, cx);
            }));
        let panel = div()
            .id(("sql-query-panel", tab_id))
            .debug_selector(|| "sql-query-panel".into())
            .w(px(520.0))
            .h(px(180.0))
            .flex()
            .flex_col()
            .overflow_hidden()
            .bg(theme::surface())
            .child(
                div()
                    .h(px(40.0))
                    .px(px(12.0))
                    .flex()
                    .items_center()
                    .justify_between()
                    .child(div().text_size(px(13.0)).child("SQL Query Preview"))
                    .child(close),
            )
            .child(
                div()
                    .relative()
                    .flex_1()
                    .min_h(px(0.0))
                    .p(px(8.0))
                    .bg(theme::background())
                    .border_1()
                    .border_color(theme::border())
                    .rounded(px(4.0))
                    .child(div().absolute().top(px(4.0)).right(px(4.0)).child(copy))
                    .child(
                        div()
                            .debug_selector(|| "sql-query-command".into())
                            .id(("sql-query-code", tab_id))
                            .size_full()
                            .pt(px(30.0))
                            .overflow_x_scroll()
                            .overflow_y_scroll()
                            .font_family("Menlo")
                            .text_size(px(12.0))
                            .line_height(px(18.0))
                            .text_color(theme::text())
                            .child(text),
                    ),
            );
        let root = cx.entity().downgrade();
        Popover::new(("sql-query-preview-popover", tab_id))
            .trigger(button)
            .open(open)
            .p(px(0.0))
            .bg(theme::surface())
            .on_open_change(move |open, _, app| {
                let _ = root.update(app, |this, cx| {
                    this.set_sql_table_preview_open(tab_id, *open, cx);
                });
            })
            .child(panel)
            .into_any_element()
    }

    fn set_sql_table_preview_open(&mut self, tab_id: u64, open: bool, cx: &mut Context<Self>) {
        if let Some(runtime) = self.workspace.runtime_mut(tab_id) {
            runtime.sql_table.preview_open = open;
        }
        self.notify_tab(tab_id, cx);
    }

    /// The bar: same capsule geometry as the Mongo toolbar.
    pub(in crate::app) fn render_sql_table_toolbar(
        &mut self,
        tab_id: u64,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let Some(tab) = self.workspace.tab(tab_id).cloned() else {
            return div().into_any_element();
        };
        let (ui, query_error) = self
            .workspace
            .runtime(tab_id)
            .map(|runtime| {
                (
                    runtime.sql_table.clone(),
                    match runtime.query.status() {
                        crate::query::QueryStatus::Failed(message) => Some(message.clone()),
                        _ => None,
                    },
                )
            })
            .unwrap_or_default();
        let error = ui.error.clone().or(query_error);
        let statement = self.sql_table_draft_statement(&tab);
        let can_run = self.sql_table_can_run(tab_id);
        let ratio = self
            .workspace
            .runtime(tab_id)
            .map(|runtime| runtime.mongo.filter_sort_ratio_clamped())
            .unwrap_or(0.5) as f32;

        let preview = self.render_sql_query_preview(tab_id, statement, ui.preview_open, cx);
        let preview_control = div()
            .debug_selector(|| "sql-query-preview".into())
            .flex()
            .flex_shrink_0()
            .h(px(24.0))
            .items_center()
            .child(preview);

        let mut field_element = |field: SqlField,
                                 expanded: bool,
                                 basis: f32,
                                 window: &mut Window,
                                 cx: &mut Context<Self>|
         -> Div {
            let slug = field.slug();
            let input = self.sql_table_input(tab_id, field, window, cx);
            let editor = expanded.then(|| self.sql_table_expanded_editor(tab_id, field, window, cx));
            let expand_button = Button::new((field.id(Control::Expand), tab_id))
                .icon(Icon::new(IconName::Maximize).size(px(12.0)))
                .compact()
                .ghost()
                .selected(expanded)
                .tooltip(match field {
                    SqlField::Where => "Expand WHERE",
                    SqlField::OrderBy => "Expand ORDER BY",
                });
            let popover =
                self.render_sql_expand_popover(tab_id, field, editor, expand_button, cx);
            let has_text = !tab_field(&tab, field).trim().is_empty();
            let clear = Button::new((field.id(Control::Clear), tab_id))
                .debug_selector(move || format!("{slug}-clear"))
                .icon(Icon::new(IconName::Close).size(px(11.0)))
                .compact()
                .ghost()
                .tooltip(match field {
                    SqlField::Where => "Clear WHERE",
                    SqlField::OrderBy => "Clear ORDER BY",
                })
                .on_click(cx.listener(move |this, _, window, cx| {
                    this.clear_sql_table_field(tab_id, field, window, cx);
                }));
            let filter_control = (field == SqlField::Where)
                .then(|| self.render_sql_filter_control(&tab, window, cx));
            div()
                .debug_selector(move || format!("{slug}-field"))
                .flex()
                .flex_grow(1.0)
                .flex_basis(gpui::relative(basis))
                .min_w(px(FIELD_MIN_WIDTH))
                .h(px(24.0))
                .items_center()
                .gap(px(4.0))
                .children(filter_control)
                .child(
                    div()
                        .debug_selector(move || format!("{slug}-label"))
                        .flex_shrink_0()
                        .px(px(4.0))
                        .text_size(px(11.0))
                        .text_color(match field {
                            SqlField::Where => theme::accent(),
                            SqlField::OrderBy => theme::warning(),
                        })
                        .child(field.label()),
                )
                .child(
                    div()
                        .flex_1()
                        .min_w(px(0.0))
                        .child(Input::new(&input).appearance(false).disabled(expanded)),
                )
                .child(
                    div()
                        .debug_selector(move || format!("{slug}-expand"))
                        .flex()
                        .flex_shrink_0()
                        .h(px(24.0))
                        .items_center()
                        .when(has_text, |this| this.child(clear))
                        .child(popover),
                )
        };
        let where_field = field_element(SqlField::Where, ui.where_expanded, ratio, window, cx);
        let order_field =
            field_element(SqlField::OrderBy, ui.order_expanded, 1.0 - ratio, window, cx);

        let handle = div()
            .id(("sql-where-order-handle-drag", tab_id))
            .debug_selector(|| "sql-field-handle".into())
            .w(px(12.0))
            .h(px(24.0))
            .flex()
            .flex_shrink_0()
            .items_center()
            .justify_center()
            .cursor_col_resize()
            .child(div().w(px(1.0)).h(px(22.0)).bg(theme::border()))
            .on_mouse_down(
                MouseButton::Left,
                cx.listener(move |this, event: &gpui::MouseDownEvent, window, _| {
                    this.begin_filter_sort_resize(tab_id, event.position.x, window);
                }),
            )
            .on_mouse_up(
                MouseButton::Left,
                cx.listener(move |this, _, _, _| this.end_filter_sort_resize(tab_id)),
            )
            .on_mouse_up_out(
                MouseButton::Left,
                cx.listener(move |this, _, _, _| this.end_filter_sort_resize(tab_id)),
            )
            .on_drag(tab_id, |_, _, _, cx| cx.new(|_| ColumnResizeGhost))
            .on_drag_move(
                cx.listener(move |this, event: &gpui::DragMoveEvent<u64>, _, cx| {
                    if *event.drag(cx) == tab_id && this.view(tab_id).mongo.filter_sort_drag.is_some()
                    {
                        this.update_filter_sort_resize(tab_id, event.event.position.x);
                        this.notify_tab(tab_id, cx);
                    }
                }),
            );

        let run_button = Button::new(("sql-run", tab_id))
            .icon(Icon::empty().path("icons/play.svg").size(px(11.0)))
            .small()
            .compact()
            .tooltip("Execute query")
            .disabled(!can_run)
            .on_click(cx.listener(move |this, _, _, cx| this.submit_sql_table_query(tab_id, cx)));
        let run_control = div()
            .debug_selector(|| "sql-run".into())
            .flex()
            .flex_shrink_0()
            .h(px(24.0))
            .items_center()
            .child(run_button);

        let has_error = error.is_some();
        let error_marker = has_error.then(|| {
            div()
                .absolute()
                .size(px(1.0))
                .debug_selector(|| "sql-query-error".into())
        });
        let capsule = div()
            .id(("sql-query-capsule", tab_id))
            .debug_selector(|| "sql-query-capsule".into())
            .flex()
            .flex_1()
            .min_w(px(0.0))
            .h(px(30.0))
            .items_center()
            .gap(px(8.0))
            .px(px(8.0))
            .rounded(px(7.0))
            .bg(theme::background())
            .border_1()
            .border_color(if has_error {
                theme::danger()
            } else {
                theme::border()
            })
            .when_some(error, |this, message| {
                this.tooltip(move |window, cx| {
                    gpui_component::tooltip::Tooltip::new(message.clone()).build(window, cx)
                })
            })
            .children(error_marker)
            .child(preview_control)
            // The fields shrink and clip inside their own lane, so a narrow
            // pane never pushes Run out of the capsule.
            .child(
                div()
                    .debug_selector(|| "sql-query-fields".into())
                    .flex()
                    .flex_1()
                    .min_w(px(0.0))
                    .overflow_hidden()
                    .items_center()
                    .gap(px(8.0))
                    .child(where_field)
                    .child(handle)
                    .child(order_field),
            )
            .child(div().flex_shrink_0().w(px(1.0)).h(px(22.0)).bg(theme::border()))
            .child(run_control);

        // Same spot and behaviour as the Mongo "Columns k/N" control: a popover
        // that shows / hides columns in the grid (Properties stays on the rail).
        let columns_control = self.render_sql_columns_control(&tab, window, cx);

        let toolbar = div()
            .debug_selector(|| "sql-result-toolbar".into())
            .h(px(theme::RESULT_TOOLBAR_HEIGHT))
            .relative()
            .flex()
            .items_center()
            .gap(px(8.0))
            .px(px(8.0))
            .flex_shrink_0()
            .bg(theme::chrome())
            .border_b_1()
            .border_color(theme::border())
            .child(capsule)
            .child(div().flex_shrink_0().child(columns_control));
        toolbar.into_any_element()
    }
}
