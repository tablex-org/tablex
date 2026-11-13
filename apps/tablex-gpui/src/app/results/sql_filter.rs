//! Filter builder popover of `sqlTable` tabs.
//!
//! The rules live on the tab (`Tab::table_filter`, persisted in the session);
//! this module is the UI plus the Apply / Clear flow. Apply turns the enabled
//! rules into a WHERE fragment (`crate::sql_filter::build_where`), validates it
//! with the same validator as hand-typed text, writes it into the WHERE field
//! and applies it through the ordinary applied/draft path
//! (`rerun_sql_table` -> `apply_sql_table_fragments`), so page 1 runs and
//! paging / refresh keep re-running exactly that fragment.
//!
//! Controls are the official ones: `Popover` (anchored below the filter
//! button), `Select` / `SelectState` for the column and mode dropdowns,
//! `Input` / `Textarea` for values, `Button` for everything clickable. Row
//! reorder uses GPUI's drag and drop (`on_drag` / `on_drop`). The panel is only
//! built while the popover is open, so a closed bar costs one button.

use super::super::*;
use crate::sql_filter::{
    self, SqlFilterColumnKind, SqlFilterConjunction, SqlFilterMode, SqlFilterRule,
};
use crate::sql_table::{self, SqlDialect};
use gpui_component::select::{Select, SelectEvent, SelectState};

type TextSelect = SelectState<Vec<String>>;

/// Editors of one rule row. Created lazily while the popover is open and
/// dropped with the rule, so memory is proportional to the rules shown.
#[derive(Clone)]
struct RuleEditors {
    column: gpui::Entity<TextSelect>,
    mode: gpui::Entity<TextSelect>,
    value: gpui::Entity<InputState>,
    end_value: gpui::Entity<InputState>,
    list: gpui::Entity<TextareaState>,
}

#[derive(Default)]
pub(in crate::app) struct SqlFilterUiState {
    rows: HashMap<(u64, u64), RuleEditors>,
    /// Column list last pushed into each tab's column selects.
    columns: HashMap<u64, Vec<String>>,
}

impl SqlFilterUiState {
    pub(in crate::app) fn remove_tab(&mut self, tab_id: u64) {
        self.rows.retain(|(id, _), _| *id != tab_id);
        self.columns.remove(&tab_id);
    }

    fn remove_rule(&mut self, tab_id: u64, rule_id: u64) {
        self.rows.remove(&(tab_id, rule_id));
    }

    /// Column select, mode select and value input of a rule (tests).
    #[cfg(test)]
    pub(in crate::app) fn editors_for_test(
        &self,
        tab_id: u64,
        rule_id: u64,
    ) -> Option<RuleEditorsView> {
        self.rows
            .get(&(tab_id, rule_id))
            .map(|row| (row.column.clone(), row.mode.clone(), row.value.clone()))
    }

    #[cfg(test)]
    pub(in crate::app) fn row_count(&self, tab_id: u64) -> usize {
        self.rows.keys().filter(|(id, _)| *id == tab_id).count()
    }
}

#[cfg(test)]
pub(in crate::app) type RuleEditorsView = (
    gpui::Entity<TextSelect>,
    gpui::Entity<TextSelect>,
    gpui::Entity<InputState>,
);

/// Drag payload of a rule's grip handle (also its ghost).
#[derive(Clone)]
struct RuleDrag {
    tab_id: u64,
    rule_id: u64,
    label: gpui::SharedString,
}

impl Render for RuleDrag {
    fn render(&mut self, _: &mut Window, _: &mut Context<Self>) -> impl IntoElement {
        div()
            .px(px(8.0))
            .h(px(24.0))
            .flex()
            .items_center()
            .rounded(px(4.0))
            .border_1()
            .border_color(theme::accent())
            .bg(theme::surface())
            .text_size(px(11.0))
            .text_color(theme::text())
            .child(self.label.clone())
    }
}

const STAGED_BLOCK_MESSAGE: &str = "Apply or discard staged SQL changes before filtering.";
const PANEL_WIDTH: f32 = 580.0;

fn column_kind_of(type_name: Option<&str>) -> SqlFilterColumnKind {
    use super::column_type::{column_type_category, ColumnTypeCategory as Category};
    let Some(type_name) = type_name else {
        return SqlFilterColumnKind::Unknown;
    };
    match column_type_category(type_name) {
        Category::Number => SqlFilterColumnKind::Number,
        Category::Boolean => SqlFilterColumnKind::Boolean,
        Category::Temporal => SqlFilterColumnKind::Temporal,
        Category::Text => SqlFilterColumnKind::Text,
        Category::Object | Category::Binary | Category::Uuid => SqlFilterColumnKind::Other,
    }
}

fn mode_labels() -> Vec<String> {
    SqlFilterMode::ALL
        .iter()
        .map(|mode| mode.label().to_string())
        .collect()
}

impl AppRoot {
    /// Columns offered by the builder: the current result's, else the catalog's.
    fn sql_filter_columns(&self, tab_id: u64) -> Vec<String> {
        let grid = self.grid(tab_id);
        let result: Vec<String> = (0..grid.column_count())
            .filter_map(|column| grid.column_name(column).map(str::to_string))
            .collect();
        if !result.is_empty() {
            return result;
        }
        self.workspace
            .runtime(tab_id)
            .map(|runtime| {
                runtime
                    .structure_columns
                    .iter()
                    .map(|column| column.name.clone())
                    .collect()
            })
            .unwrap_or_default()
    }

    /// Literal family of `name` from the shared column-type resolver (catalog
    /// type, then the engine's native type, then the Arrow type).
    fn sql_filter_column_kind(&self, tab_id: u64, name: &str) -> SqlFilterColumnKind {
        let grid = self.grid(tab_id);
        let source = (0..grid.column_count()).find(|column| grid.column_name(*column) == Some(name));
        let resolved = match source {
            Some(column) => self.resolved_column_type(tab_id, column),
            None => self.workspace.runtime(tab_id).and_then(|runtime| {
                super::column_type::resolve_column_type(&runtime.structure_columns, name, None, None)
                    .map(str::to_string)
            }),
        };
        column_kind_of(resolved.as_deref())
    }

    pub(in crate::app) fn set_sql_filter_open(
        &mut self,
        tab_id: u64,
        open: bool,
        cx: &mut Context<Self>,
    ) {
        if open {
            let first = self.sql_filter_columns(tab_id).into_iter().next();
            if let Some(tab) = self.workspace.tab_mut(tab_id) {
                tab.table_filter.ensure_rule(first.unwrap_or_default());
            }
        }
        if let Some(runtime) = self.workspace.runtime_mut(tab_id) {
            runtime.sql_table.filter_open = open;
            runtime.sql_table.filter_error = None;
        }
        self.notify_tab(tab_id, cx);
    }

    pub(in crate::app) fn update_sql_filter_rule(
        &mut self,
        tab_id: u64,
        rule_id: u64,
        patch: impl FnOnce(&mut SqlFilterRule),
        cx: &mut Context<Self>,
    ) {
        let changed = self
            .workspace
            .tab(tab_id)
            .and_then(|tab| tab.table_filter.rules.iter().find(|rule| rule.id == rule_id))
            .map(|rule| {
                let mut next = rule.clone();
                patch(&mut next);
                next
            })
            .filter(|next| {
                self.workspace
                    .tab(tab_id)
                    .and_then(|tab| tab.table_filter.rules.iter().find(|rule| rule.id == rule_id))
                    != Some(next)
            });
        if let Some(next) = changed {
            if let Some(rule) = self
                .workspace
                .tab_mut(tab_id)
                .and_then(|tab| tab.table_filter.rule_mut(rule_id))
            {
                *rule = next;
            }
            if let Some(runtime) = self.workspace.runtime_mut(tab_id) {
                runtime.sql_table.filter_error = None;
            }
            self.notify_tab(tab_id, cx);
        }
    }

    fn add_sql_filter_rule(&mut self, tab_id: u64, cx: &mut Context<Self>) {
        let column = self
            .sql_filter_columns(tab_id)
            .into_iter()
            .next()
            .unwrap_or_default();
        if let Some(tab) = self.workspace.tab_mut(tab_id) {
            tab.table_filter.add_rule(column);
        }
        self.notify_tab(tab_id, cx);
    }

    fn remove_sql_filter_rule(&mut self, tab_id: u64, rule_id: u64, cx: &mut Context<Self>) {
        if let Some(tab) = self.workspace.tab_mut(tab_id) {
            if tab.table_filter.rules.len() > 1 {
                tab.table_filter.remove_rule(rule_id);
            }
        }
        self.sql_table_ui.filter.remove_rule(tab_id, rule_id);
        self.notify_tab(tab_id, cx);
    }

    /// Drop of a dragged rule onto `target_rule`: it takes the target's place.
    pub(in crate::app) fn move_sql_filter_rule(
        &mut self,
        tab_id: u64,
        rule_id: u64,
        target_rule: u64,
        cx: &mut Context<Self>,
    ) {
        if let Some(tab) = self.workspace.tab_mut(tab_id) {
            if let Some(target) = tab
                .table_filter
                .rules
                .iter()
                .position(|rule| rule.id == target_rule)
            {
                tab.table_filter.move_rule(rule_id, target);
            }
        }
        self.notify_tab(tab_id, cx);
    }

    /// "Reset rules": back to one blank rule; the WHERE field is untouched.
    fn reset_sql_filter_rules(&mut self, tab_id: u64, cx: &mut Context<Self>) {
        let first = self.sql_filter_columns(tab_id).into_iter().next();
        if let Some(tab) = self.workspace.tab_mut(tab_id) {
            tab.table_filter.rules.clear();
            tab.table_filter.ensure_rule(first.unwrap_or_default());
        }
        self.sql_table_ui.filter.remove_tab(tab_id);
        if let Some(runtime) = self.workspace.runtime_mut(tab_id) {
            runtime.sql_table.filter_error = None;
        }
        self.notify_tab(tab_id, cx);
    }

    fn block_sql_filter_for_staged_changes(&mut self, tab_id: u64, cx: &mut Context<Self>) -> bool {
        if !self.workspace.tab_has_staged_changes(tab_id) {
            return false;
        }
        if let Some(runtime) = self.workspace.runtime_mut(tab_id) {
            runtime.sql_table.filter_error = Some(STAGED_BLOCK_MESSAGE.into());
        }
        self.notify_tab(tab_id, cx);
        true
    }

    /// Apply Filter: rules -> WHERE text -> applied query, page 1.
    pub(in crate::app) fn apply_sql_filter(
        &mut self,
        tab_id: u64,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(tab) = self.workspace.tab(tab_id).cloned() else {
            return;
        };
        if !tab.query_mode.is_table_data()
            || self.block_sql_filter_for_staged_changes(tab_id, cx)
        {
            return;
        }
        let dialect: SqlDialect = self.sql_table_dialect(&tab);
        let generated = sql_filter::build_where(dialect, &tab.table_filter.rules, |column| {
            self.sql_filter_column_kind(tab_id, column)
        })
        .and_then(|generated| {
            sql_table::validate_fragment(dialect, "WHERE", &generated)?;
            Ok(generated)
        });
        let generated = match generated {
            Ok(generated) => generated,
            Err(error) => {
                if let Some(runtime) = self.workspace.runtime_mut(tab_id) {
                    runtime.sql_table.filter_error = Some(error);
                }
                self.notify_tab(tab_id, cx);
                return;
            }
        };
        if let Some(tab) = self.workspace.tab_mut(tab_id) {
            tab.table_filter.generated_where = (!generated.is_empty()).then(|| generated.clone());
        }
        if let Some(runtime) = self.workspace.runtime_mut(tab_id) {
            runtime.sql_table.filter_open = false;
            runtime.sql_table.filter_error = None;
        }
        self.write_sql_table_field(tab_id, super::sql_table::SqlField::Where, &generated, window, cx);
        self.rerun_sql_table(tab_id, super::sql_table::SqlField::Where, &generated, cx);
    }

    /// Clear Filter: drops every rule and the WHERE text and re-runs.
    fn clear_sql_filter(&mut self, tab_id: u64, window: &mut Window, cx: &mut Context<Self>) {
        if self.block_sql_filter_for_staged_changes(tab_id, cx) {
            return;
        }
        // The popover stays open on one fresh rule, ready for the next filter.
        let first = self.sql_filter_columns(tab_id).into_iter().next();
        if let Some(tab) = self.workspace.tab_mut(tab_id) {
            tab.table_filter.rules.clear();
            tab.table_filter.ensure_rule(first.unwrap_or_default());
            tab.table_filter.generated_where = None;
        }
        self.sql_table_ui.filter.remove_tab(tab_id);
        if let Some(runtime) = self.workspace.runtime_mut(tab_id) {
            runtime.sql_table.filter_error = None;
        }
        self.write_sql_table_field(tab_id, super::sql_table::SqlField::Where, "", window, cx);
        self.rerun_sql_table(tab_id, super::sql_table::SqlField::Where, "", cx);
    }

    /// Editors of `rule`, created on first use.
    fn sql_filter_editors(
        &mut self,
        tab_id: u64,
        rule: &SqlFilterRule,
        columns: &[String],
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> RuleEditors {
        use gpui_component::input::InputEvent;
        if let Some(editors) = self.sql_table_ui.filter.rows.get(&(tab_id, rule.id)) {
            return editors.clone();
        }
        let rule_id = rule.id;
        let column_ix = columns.iter().position(|name| *name == rule.column);
        let column = cx.new(|cx| {
            SelectState::new(
                columns.to_vec(),
                column_ix.map(|row| gpui_component::IndexPath::default().row(row)),
                window,
                cx,
            )
            .searchable(true)
        });
        cx.subscribe(
            &column,
            move |this: &mut Self, _, event: &SelectEvent<Vec<String>>, cx| {
                if let SelectEvent::Confirm(Some(name)) = event {
                    let name = name.clone();
                    this.update_sql_filter_rule(tab_id, rule_id, move |rule| rule.column = name, cx);
                }
            },
        )
        .detach();
        let mode_ix = SqlFilterMode::ALL.iter().position(|mode| *mode == rule.mode);
        let mode = cx.new(|cx| {
            SelectState::new(
                mode_labels(),
                mode_ix.map(|row| gpui_component::IndexPath::default().row(row)),
                window,
                cx,
            )
        });
        cx.subscribe(
            &mode,
            move |this: &mut Self, _, event: &SelectEvent<Vec<String>>, cx| {
                if let SelectEvent::Confirm(Some(label)) = event {
                    if let Some(mode) = SqlFilterMode::from_label(label) {
                        this.update_sql_filter_rule(tab_id, rule_id, move |rule| rule.mode = mode, cx);
                    }
                }
            },
        )
        .detach();
        let value = cx.new(|cx| {
            InputState::new(window, cx)
                .placeholder("Value")
                .default_value(rule.value.clone())
        });
        cx.subscribe_in(
            &value,
            window,
            move |this: &mut Self,
                  input: &gpui::Entity<InputState>,
                  event: &InputEvent,
                  window: &mut Window,
                  cx: &mut Context<Self>| match event {
                InputEvent::Change => {
                    let text = input.read(cx).value().to_string();
                    this.update_sql_filter_rule(tab_id, rule_id, move |rule| rule.value = text, cx);
                }
                InputEvent::PressEnter { .. } => this.apply_sql_filter(tab_id, window, cx),
                _ => {}
            },
        )
        .detach();
        let end_value = cx.new(|cx| {
            InputState::new(window, cx)
                .placeholder("Range end")
                .default_value(rule.end_value.clone())
        });
        cx.subscribe_in(
            &end_value,
            window,
            move |this: &mut Self,
                  input: &gpui::Entity<InputState>,
                  event: &InputEvent,
                  window: &mut Window,
                  cx: &mut Context<Self>| match event {
                InputEvent::Change => {
                    let text = input.read(cx).value().to_string();
                    this.update_sql_filter_rule(tab_id, rule_id, move |rule| rule.end_value = text, cx);
                }
                InputEvent::PressEnter { .. } => this.apply_sql_filter(tab_id, window, cx),
                _ => {}
            },
        )
        .detach();
        let list = cx.new(|cx| {
            TextareaState::new(window, cx)
                .placeholder("Values, separated by commas or new lines")
                .default_value(rule.value.clone())
        });
        cx.subscribe(
            &list,
            move |this: &mut Self,
                  input: gpui::Entity<TextareaState>,
                  event: &InputEvent,
                  cx: &mut Context<Self>| {
                if matches!(event, InputEvent::Change) {
                    let text = input.read(cx).value().to_string();
                    this.update_sql_filter_rule(tab_id, rule_id, move |rule| rule.value = text, cx);
                }
            },
        )
        .detach();
        let editors = RuleEditors {
            column,
            mode,
            value,
            end_value,
            list,
        };
        self.sql_table_ui
            .filter
            .rows
            .insert((tab_id, rule_id), editors.clone());
        editors
    }

    /// Re-syncs editors with the rule model (mode switches reuse one text
    /// between the single and list editors; a changed result column list
    /// replaces the column items). Cheap string compares on a handful of rules.
    fn sync_sql_filter_editors(
        &mut self,
        tab_id: u64,
        rules: &[SqlFilterRule],
        columns: &[String],
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let columns_changed = self.sql_table_ui.filter.columns.get(&tab_id).map(Vec::as_slice)
            != Some(columns);
        for rule in rules {
            let Some(editors) = self.sql_table_ui.filter.rows.get(&(tab_id, rule.id)).cloned()
            else {
                continue;
            };
            if columns_changed {
                let items = columns.to_vec();
                let column = rule.column.clone();
                editors.column.update(cx, |state, cx| {
                    state.set_items(items, window, cx);
                    state.set_selected_value(&column, window, cx);
                });
            }
            let current = editors.column.read(cx).selected_value().cloned();
            if current.as_deref() != Some(rule.column.as_str()) && !columns_changed {
                let column = rule.column.clone();
                editors
                    .column
                    .update(cx, |state, cx| state.set_selected_value(&column, window, cx));
            }
            let label = rule.mode.label().to_string();
            if editors.mode.read(cx).selected_value() != Some(&label) {
                editors
                    .mode
                    .update(cx, |state, cx| state.set_selected_value(&label, window, cx));
            }
            if editors.value.read(cx).value().as_ref() != rule.value.as_str() {
                let text = rule.value.clone();
                editors
                    .value
                    .update(cx, |state, cx| state.set_value(text, window, cx));
            }
            if editors.list.read(cx).value().as_ref() != rule.value.as_str() {
                let text = rule.value.clone();
                editors
                    .list
                    .update(cx, |state, cx| state.set_value(text, window, cx));
            }
            if editors.end_value.read(cx).value().as_ref() != rule.end_value.as_str() {
                let text = rule.end_value.clone();
                editors
                    .end_value
                    .update(cx, |state, cx| state.set_value(text, window, cx));
            }
        }
        if columns_changed {
            self.sql_table_ui
                .filter
                .columns
                .insert(tab_id, columns.to_vec());
        }
    }

    fn render_sql_filter_rule(
        &mut self,
        tab_id: u64,
        index: usize,
        rule_count: usize,
        rule: &SqlFilterRule,
        editors: &RuleEditors,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let rule_id = rule.id;
        let disabled = rule.disabled;
        let mode = rule.mode;
        let id = |prefix: &'static str| super::mongo::mongo_filter_element_id(prefix, tab_id, rule_id);
        let selector = move |prefix: &'static str| move || format!("{prefix}-{rule_id}");

        let handle = div()
            .id(id("sql-filter-handle"))
            .debug_selector(selector("sql-filter-handle"))
            .w(px(18.0))
            .h(px(28.0))
            .flex()
            .flex_shrink_0()
            .items_center()
            .justify_center()
            .text_color(theme::text_faint())
            .when(rule_count > 1, |this| {
                let drag = RuleDrag {
                    tab_id,
                    rule_id,
                    label: format!(
                        "{} {}",
                        if rule.column.is_empty() { "(column)" } else { &rule.column },
                        mode.label()
                    )
                    .into(),
                };
                this.cursor_grab()
                    .hover(|style| style.text_color(theme::text()))
                    .on_drag(drag, |drag, _, _, cx| cx.new(|_| drag.clone()))
            })
            .child(Icon::empty().path("icons/grip-vertical.svg").size(px(14.0)));

        let column = div()
            .debug_selector(selector("sql-filter-column"))
            .w(px(168.0))
            .flex_shrink_0()
            .child(
                Select::new(&editors.column)
                    .id(id("sql-filter-column-select"))
                    .with_size(Size::Small)
                    .placeholder("Column")
                    .search_placeholder("Search columns")
                    .menu_width(px(240.0))
                    .disabled(disabled),
            );
        let mode_select = div()
            .debug_selector(selector("sql-filter-mode"))
            .w(px(132.0))
            .flex_shrink_0()
            .child(
                Select::new(&editors.mode)
                    .id(id("sql-filter-mode-select"))
                    .with_size(Size::Small)
                    .menu_width(px(168.0))
                    .disabled(disabled),
            );
        let value_slot = div().flex_1().min_w(px(0.0));
        let value_slot = if !mode.needs_value() {
            value_slot.child(
                div()
                    .debug_selector(selector("sql-filter-no-value"))
                    .h(px(28.0))
                    .flex()
                    .items_center()
                    .px(px(8.0))
                    .rounded(px(4.0))
                    .border_1()
                    .border_color(theme::border())
                    .text_size(px(11.0))
                    .text_color(theme::text_muted())
                    .child("No value"),
            )
        } else if mode.uses_range() || mode.uses_list() {
            // These belong on a second row; the slot keeps the grid aligned.
            value_slot
        } else {
            value_slot.child(
                div()
                    .debug_selector(selector("sql-filter-value"))
                    .child(
                        Input::new(&editors.value)
                            .appearance(true)
                            .with_size(Size::Small)
                            .disabled(disabled),
                    ),
            )
        };
        let toggle = Button::new(id("sql-filter-toggle"))
            .debug_selector(selector("sql-filter-toggle"))
            .icon(
                Icon::empty()
                    .path(if disabled {
                        "icons/eye-off.svg"
                    } else {
                        "icons/eye.svg"
                    })
                    .size(px(14.0)),
            )
            .compact()
            .ghost()
            .tooltip(if disabled { "Enable rule" } else { "Disable rule" })
            .on_click(cx.listener(move |this, _, _, cx| {
                this.update_sql_filter_rule(tab_id, rule_id, |rule| rule.disabled = !rule.disabled, cx);
            }));
        let remove = Button::new(id("sql-filter-remove"))
            .debug_selector(selector("sql-filter-remove"))
            .icon(Icon::new(IconName::Close).size(px(14.0)))
            .compact()
            .ghost()
            .disabled(rule_count == 1)
            .tooltip("Remove rule")
            .on_click(cx.listener(move |this, _, _, cx| {
                this.remove_sql_filter_rule(tab_id, rule_id, cx);
            }));

        let second_row = if mode.uses_range() {
            Some(
                div()
                    .flex()
                    .gap(px(6.0))
                    .child(
                        div()
                            .debug_selector(selector("sql-filter-value"))
                            .flex_1()
                            .child(
                                Input::new(&editors.value)
                                    .appearance(true)
                                    .with_size(Size::Small)
                                    .disabled(disabled),
                            ),
                    )
                    .child(
                        div()
                            .debug_selector(selector("sql-filter-end"))
                            .flex_1()
                            .child(
                                Input::new(&editors.end_value)
                                    .appearance(true)
                                    .with_size(Size::Small)
                                    .disabled(disabled),
                            ),
                    ),
            )
        } else if mode.uses_list() {
            Some(
                div()
                    .debug_selector(selector("sql-filter-list"))
                    .child(
                        Textarea::new(&editors.list)
                            .h(px(56.0))
                            .with_size(Size::Small)
                            .disabled(disabled),
                    ),
            )
        } else {
            None
        };

        let target_drag_over = |style: gpui::StyleRefinement, _: &RuleDrag, _: &mut Window, _: &mut gpui::App| {
            style.bg(theme::accent_surface())
        };
        let mut row = div()
            .id(id("sql-filter-rule"))
            .debug_selector(selector("sql-filter-rule"))
            .flex()
            .flex_col()
            .gap(px(4.0))
            .rounded(px(4.0))
            .drag_over::<RuleDrag>(target_drag_over)
            .on_drop(cx.listener(move |this, drag: &RuleDrag, _, cx| {
                if drag.tab_id == tab_id && drag.rule_id != rule_id {
                    this.move_sql_filter_rule(tab_id, drag.rule_id, rule_id, cx);
                }
            }))
            .child(
                div()
                    .flex()
                    .items_center()
                    .gap(px(6.0))
                    .child(handle)
                    .child(column)
                    .child(mode_select)
                    .child(value_slot)
                    .child(toggle)
                    .child(remove),
            );
        if let Some(second_row) = second_row {
            row = row.child(div().pl(px(24.0)).pr(px(60.0)).child(second_row));
        }
        let conjunction = (index > 0).then(|| {
            let current = rule.conjunction;
            div()
                .h(px(20.0))
                .flex()
                .items_center()
                .justify_center()
                .child(
                    Button::new(id("sql-filter-conjunction"))
                        .debug_selector(selector("sql-filter-conjunction"))
                        .with_size(Size::XSmall)
                        .label(current.label())
                        .compact()
                        .ghost()
                        .h(px(20.0))
                        .when(current == SqlFilterConjunction::Or, |button| {
                            button.text_color(theme::accent())
                        })
                        .tooltip("Toggle AND / OR")
                        .on_click(cx.listener(move |this, _, _, cx| {
                            this.update_sql_filter_rule(
                                tab_id,
                                rule_id,
                                |rule| rule.conjunction = rule.conjunction.toggled(),
                                cx,
                            );
                        })),
                )
        });
        div()
            .flex()
            .flex_col()
            .children(conjunction)
            .child(row)
            .into_any_element()
    }

    fn render_sql_filter_panel(
        &mut self,
        tab: &Tab,
        error: Option<String>,
        can_run: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let tab_id = tab.id;
        let mut columns = self.sql_filter_columns(tab_id);
        let rules = tab.table_filter.rules.clone();
        // A restored rule can name a column before any result is loaded.
        for rule in &rules {
            if !rule.column.is_empty() && !columns.contains(&rule.column) {
                columns.push(rule.column.clone());
            }
        }
        let editors: Vec<RuleEditors> = rules
            .iter()
            .map(|rule| self.sql_filter_editors(tab_id, rule, &columns, window, cx))
            .collect();
        self.sync_sql_filter_editors(tab_id, &rules, &columns, window, cx);

        let mut list = div().flex().flex_col();
        for (index, (rule, editors)) in rules.iter().zip(&editors).enumerate() {
            list = list.child(self.render_sql_filter_rule(tab_id, index, rules.len(), rule, editors, cx));
        }
        let manual_edit = tab
            .table_filter
            .generated_where
            .as_ref()
            .is_some_and(|generated| *generated != tab.table_where);

        let clear = Button::new(("sql-filter-clear", tab_id))
            .debug_selector(|| "sql-filter-clear".into())
            .with_size(Size::XSmall)
            .icon(Icon::new(IconName::Delete).size(px(14.0)))
            .label("Clear Filter")
            .compact()
            .ghost()
            .h(px(28.0))
            .on_click(cx.listener(move |this, _, window, cx| {
                this.clear_sql_filter(tab_id, window, cx);
            }));
        let add = Button::new(("sql-filter-add", tab_id))
            .debug_selector(|| "sql-filter-add".into())
            .with_size(Size::XSmall)
            .icon(Icon::new(IconName::Plus).size(px(14.0)))
            .label("Add rule")
            .compact()
            .ghost()
            .h(px(28.0))
            .disabled(columns.is_empty())
            .on_click(cx.listener(move |this, _, _, cx| this.add_sql_filter_rule(tab_id, cx)));
        let reset = Button::new(("sql-filter-reset", tab_id))
            .debug_selector(|| "sql-filter-reset".into())
            .with_size(Size::XSmall)
            .label("Reset rules")
            .compact()
            .ghost()
            .h(px(28.0))
            .on_click(cx.listener(move |this, _, _, cx| this.reset_sql_filter_rules(tab_id, cx)));
        let apply = Button::new(("sql-filter-apply", tab_id))
            .debug_selector(|| "sql-filter-apply".into())
            .with_size(Size::XSmall)
            .label("Apply Filter")
            .compact()
            .primary()
            .h(px(28.0))
            .disabled(!can_run)
            .on_click(cx.listener(move |this, _, window, cx| {
                this.apply_sql_filter(tab_id, window, cx);
            }));

        div()
            .id(("sql-filter-panel", tab_id))
            .debug_selector(|| "sql-filter-panel".into())
            .w(px(PANEL_WIDTH))
            .p(px(12.0))
            .flex()
            .flex_col()
            .gap(px(10.0))
            .bg(theme::surface())
            .child(
                div()
                    .h(px(28.0))
                    .flex()
                    .items_center()
                    .justify_between()
                    .child(
                        div()
                            .text_size(px(12.0))
                            .text_color(theme::text())
                            .child("Filter"),
                    )
                    .child(clear),
            )
            .child(list)
            .when(manual_edit, |this| {
                this.child(
                    div()
                        .debug_selector(|| "sql-filter-notice".into())
                        .text_size(px(10.0))
                        .text_color(theme::warning())
                        .child("WHERE edited manually - Apply Filter replaces it with these rules."),
                )
            })
            .when_some(error, |this, error| {
                this.child(
                    div()
                        .debug_selector(|| "sql-filter-error".into())
                        .text_size(px(10.0))
                        .text_color(theme::danger())
                        .child(error),
                )
            })
            .child(
                div()
                    .h(px(28.0))
                    .flex()
                    .items_center()
                    .justify_between()
                    .child(add)
                    .child(div().flex().gap(px(4.0)).child(reset).child(apply)),
            )
            .into_any_element()
    }

    /// Funnel button + popover that sits left of the WHERE field, plus the
    /// full-window backdrop that dismisses it (rendered by the shell at the
    /// root, below popups).
    ///
    /// The pinned `Popover` dismisses on any mouse-down outside its content
    /// *bounds*, which includes clicks on the options of a `Select` opened
    /// inside it (the option list is a deferred popup that extends past the
    /// panel), so choosing a column or mode with the mouse would close the
    /// popover and click through to the grid. The popover therefore keeps
    /// `overlay_closable(false)` (placement, focus and Escape stay official)
    /// and this backdrop owns outside-click dismissal: it sits above the page
    /// but under the popup layer, so the popover panel and any Select list
    /// receive their clicks first. Cost: one absolutely positioned div while
    /// open.
    pub(in crate::app) fn render_sql_filter_control(
        &mut self,
        tab: &Tab,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let tab_id = tab.id;
        let (open, error) = self
            .workspace
            .runtime(tab_id)
            .map(|runtime| {
                (
                    runtime.sql_table.filter_open,
                    runtime.sql_table.filter_error.clone(),
                )
            })
            .unwrap_or_default();
        let active = tab
            .table_filter
            .generated_where
            .as_ref()
            .is_some_and(|generated| *generated == tab.applied_where);
        let count = if active {
            tab.table_filter.active_count()
        } else {
            0
        };
        let button = Button::new(("sql-filter-button", tab_id))
            .icon(
                Icon::empty()
                    .path("icons/filter.svg")
                    .size(px(12.0))
                    .when(active, |icon| icon.text_color(theme::accent())),
            )
            .compact()
            .ghost()
            .selected(open || active)
            .tooltip("Filter builder");
        let panel = if open {
            let can_run = self.sql_table_can_run(tab_id);
            self.render_sql_filter_panel(tab, error, can_run, window, cx)
        } else {
            div().into_any_element()
        };
        let root = cx.entity().downgrade();
        let popover = Popover::new(("sql-filter-popover", tab_id))
            .trigger(button)
            .open(open)
            .p(px(0.0))
            .bg(theme::surface())
            .overlay_closable(false)
            .on_open_change(move |open, _, app| {
                let _ = root.update(app, |this, cx| {
                    this.set_sql_filter_open(tab_id, *open, cx);
                });
            })
            .child(panel);
        let overlay = open.then(|| {
            super::window_overlay(
                window,
                div()
                    .debug_selector(|| "sql-filter-overlay".into())
                    .size_full()
                    .occlude()
                    .on_mouse_down(
                        MouseButton::Left,
                        cx.listener(move |this, _, _, cx| {
                            this.set_sql_filter_open(tab_id, false, cx)
                        }),
                    ),
            )
        });
        div()
            .debug_selector(|| "sql-filter-control".into())
            .relative()
            .flex()
            .flex_shrink_0()
            .h(px(24.0))
            .items_center()
            .child(popover)
            .children(overlay)
            .when(count > 0, |this| {
                this.child(
                    div()
                        .debug_selector(|| "sql-filter-badge".into())
                        .absolute()
                        .right(px(-4.0))
                        .top(px(-2.0))
                        .min_w(px(14.0))
                        .h(px(14.0))
                        .px(px(3.0))
                        .flex()
                        .items_center()
                        .justify_center()
                        .rounded(px(7.0))
                        .bg(theme::accent())
                        .text_size(px(9.0))
                        .text_color(theme::background())
                        .child(count.to_string()),
                )
            })
            .into_any_element()
    }
}
