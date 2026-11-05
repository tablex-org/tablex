//! "Columns k/N" popover of `sqlTable` tabs: show / hide result columns in
//! the grid, like the Mongo toolbar's (shared `column_picker`).
//!
//! Hidden columns are stored per tab by name (`Tab::table_hidden_columns`,
//! persisted in the session). They only change the display->source map built
//! by `reset_tab_view`: the statement still selects every column, so
//! nothing is re-queried, Properties keeps describing the whole table, and
//! selection / copy / export / stats / cell detail all go through the same
//! display mapping that Mongo's hidden columns use. At least one column stays
//! visible. Staged edits are keyed by display column, so the picker is locked
//! while changes are staged.

use super::super::*;

const LOCKED_HINT: &str = "Apply or discard staged changes first";
const KEEP_ONE_HINT: &str = "At least one column stays visible";

impl AppRoot {
    /// Names of the current result's columns, in result order.
    pub(in crate::app) fn sql_result_columns(&self, tab_id: u64) -> Vec<String> {
        let grid = self.grid(tab_id);
        (0..grid.column_count())
            .filter_map(|column| grid.column_name(column).map(str::to_string))
            .collect()
    }

    fn sql_columns_locked(&self, tab_id: u64) -> bool {
        self.workspace.tab_has_staged_changes(tab_id)
    }

    pub(in crate::app) fn set_sql_columns_open(
        &mut self,
        tab_id: u64,
        open: bool,
        cx: &mut Context<Self>,
    ) {
        if let Some(runtime) = self.workspace.runtime_mut(tab_id) {
            runtime.sql_table.columns_open = open;
        }
        self.notify_tab(tab_id, cx);
    }

    /// Replaces the hidden set (never hiding every column) and re-maps the grid.
    fn set_sql_hidden_columns(&mut self, tab_id: u64, hidden: Vec<String>, cx: &mut Context<Self>) {
        let columns = self.sql_result_columns(tab_id);
        let hidden: Vec<String> = columns
            .iter()
            .filter(|name| hidden.contains(name))
            .cloned()
            .collect();
        if columns.is_empty() || hidden.len() >= columns.len() {
            return;
        }
        let Some(tab) = self.workspace.tab_mut(tab_id) else {
            return;
        };
        if tab.table_hidden_columns == hidden {
            return;
        }
        tab.table_hidden_columns = hidden;
        self.reset_tab_view(tab_id);
        self.notify_tab(tab_id, cx);
    }

    pub(in crate::app) fn toggle_sql_column(&mut self, tab_id: u64, name: String, cx: &mut Context<Self>) {
        if self.sql_columns_locked(tab_id) {
            return;
        }
        let mut hidden = self
            .workspace
            .tab(tab_id)
            .map(|tab| tab.table_hidden_columns.clone())
            .unwrap_or_default();
        if let Some(position) = hidden.iter().position(|hidden| *hidden == name) {
            hidden.remove(position);
        } else {
            hidden.push(name);
        }
        self.set_sql_hidden_columns(tab_id, hidden, cx);
    }

    /// "Select all": every column visible again.
    pub(in crate::app) fn show_all_sql_columns(&mut self, tab_id: u64, cx: &mut Context<Self>) {
        if self.sql_columns_locked(tab_id) {
            return;
        }
        self.set_sql_hidden_columns(tab_id, Vec::new(), cx);
    }

    /// "Select none": hides everything except the first column.
    pub(in crate::app) fn hide_all_sql_columns_but_first(
        &mut self,
        tab_id: u64,
        cx: &mut Context<Self>,
    ) {
        if self.sql_columns_locked(tab_id) {
            return;
        }
        let hidden = self.sql_result_columns(tab_id).into_iter().skip(1).collect();
        self.set_sql_hidden_columns(tab_id, hidden, cx);
    }

    fn sql_column_search_editor(
        &mut self,
        tab_id: u64,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> gpui::Entity<InputState> {
        if let Some(editor) = self.sql_table_ui.column_search.get(&tab_id) {
            return editor.clone();
        }
        let value = self
            .workspace
            .runtime(tab_id)
            .map(|runtime| runtime.sql_table.column_search.clone())
            .unwrap_or_default();
        let editor = Self::new_column_search_input(tab_id, value, window, cx, |this, tab_id, text| {
            if let Some(runtime) = this.workspace.runtime_mut(tab_id) {
                runtime.sql_table.column_search = text;
            }
        });
        self.sql_table_ui.column_search.insert(tab_id, editor.clone());
        editor
    }

    /// The bar's "Columns k/N" button and its popover.
    pub(in crate::app) fn render_sql_columns_control(
        &mut self,
        tab: &Tab,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let tab_id = tab.id;
        let (open, search) = self
            .workspace
            .runtime(tab_id)
            .map(|runtime| {
                (
                    runtime.sql_table.columns_open,
                    runtime.sql_table.column_search.clone(),
                )
            })
            .unwrap_or_default();
        let columns = self.sql_result_columns(tab_id);
        let total = columns.len();
        let selected: HashSet<String> = (0..self.display_column_count(tab_id))
            .filter_map(|display| {
                let source = self.source_column_for_display(tab_id, display)?;
                self.grid(tab_id).column_name(source).map(str::to_string)
            })
            .collect();
        let locked = self.sql_columns_locked(tab_id);
        let button = Button::new(("sql-columns", tab_id))
            .debug_selector(|| "sql-columns-button".into())
            .icon(Icon::empty().path("icons/list-checks.svg").size(px(13.0)))
            .label(columns_button_label(selected.len(), total))
            .xsmall()
            .compact()
            .selected(open)
            .disabled(total == 0)
            .tooltip("Show or hide columns");
        let search_editor = self.sql_column_search_editor(tab_id, window, cx);
        let footer = column_picker_footer(
            "sql-column",
            if locked { LOCKED_HINT } else { KEEP_ONE_HINT },
            div()
                .flex()
                .gap(px(4.0))
                .child(
                    Button::new(("sql-column-select-all", tab_id))
                        .debug_selector(|| "sql-column-select-all".into())
                        .with_size(Size::XSmall)
                        .label("Select all")
                        .compact()
                        .ghost()
                        .disabled(locked)
                        .on_click(cx.listener(move |this, _, _, cx| {
                            this.show_all_sql_columns(tab_id, cx);
                        })),
                )
                .child(
                    Button::new(("sql-column-select-none", tab_id))
                        .debug_selector(|| "sql-column-select-none".into())
                        .with_size(Size::XSmall)
                        .label("Select none")
                        .compact()
                        .ghost()
                        .disabled(locked)
                        .on_click(cx.listener(move |this, _, _, cx| {
                            this.hide_all_sql_columns_but_first(tab_id, cx);
                        })),
                ),
        );
        let popover = self.render_column_picker(
            ColumnPicker {
                prefix: "sql-column",
                tab_id,
                columns,
                selected,
                unavailable: HashSet::new(),
                search,
                open,
                locked,
                tabs: None,
                footer,
                trigger: button,
                search_editor,
            },
            move |this, name, cx| this.toggle_sql_column(tab_id, name, cx),
            move |this, open, cx| this.set_sql_columns_open(tab_id, open, cx),
            cx,
        );
        div()
            .debug_selector(|| "sql-columns".into())
            .flex()
            .flex_shrink_0()
            .h(px(24.0))
            .items_center()
            .child(popover)
            .into_any_element()
    }
}
