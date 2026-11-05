//! The editable Inspector: rename the table, add / drop / rename / retype
//! columns, toggle NULL and the primary key, edit default and comment, manage
//! indexes and foreign keys. Every edit goes into the pending change-set
//! (`structure.rs`), never to the database.
//!
//! Only the selected column has editors (four inputs shared by every column,
//! created on first use), so a table with hundreds of columns builds no more
//! elements than a read-only one. The rows are plain elements in the scrolling
//! dock, capped like the other panels.

use super::panels::MAX_ROWS;
use super::structure::{FkForm, IndexForm};
use super::*;
use crate::app::results::diagram::overlay::TableState;
use crate::app::structure_edit;
use crate::sql_ddl::{Change, ForeignKeySpec};
use gpui::SharedString;
use gpui_component::checkbox::Checkbox;
use gpui_component::select::{Select, SelectEvent, SelectState};

fn section(title: &'static str) -> Div {
    div()
        .px(px(10.0))
        .pt(px(10.0))
        .pb(px(3.0))
        .flex()
        .items_center()
        .gap(px(6.0))
        .text_size(px(10.5))
        .font_weight(gpui::FontWeight::SEMIBOLD)
        .text_color(theme::text_muted())
        .child(title)
}

fn small_button(id: impl Into<gpui::ElementId>, label: &'static str, selector: &'static str) -> Button {
    Button::new(id).label(label).with_size(Size::Small).compact().ghost().debug_selector(move || selector.into())
}

fn icon_button(id: impl Into<gpui::ElementId>, path: &'static str, tooltip: &'static str, selector: String) -> Button {
    Button::new(id)
        .icon(Icon::empty().path(path).size(px(12.0)))
        .with_size(Size::Small)
        .compact()
        .ghost()
        .tooltip(tooltip)
        .debug_selector(move || selector.clone())
}

fn field(label: &'static str, input: impl IntoElement) -> Div {
    div()
        .flex()
        .flex_col()
        .gap(px(2.0))
        .child(div().text_size(px(10.0)).text_color(theme::text_faint()).child(label))
        .child(input)
}

impl DiagramView {
    /// The Inspector panel body: editable while the diagram may be edited.
    pub(super) fn inspector_panel(&mut self, window: &mut Window, cx: &mut Context<Self>) -> gpui::AnyElement {
        let Some(index) = self.selected.map(|id| id.0).filter(|&i| self.graph().is_some_and(|g| i < g.entities.len())) else {
            return self.inspector_read_only();
        };
        let ready = self.graph().and_then(|g| g.entities.get(index)).is_some_and(|e| e.state == EntityState::Ready);
        if !self.editable() || !ready {
            let reason = self.edit_disabled_reason().map(str::to_owned);
            return div()
                .flex()
                .flex_col()
                .children(reason.map(|reason| {
                    div()
                        .debug_selector(|| "inspector-readonly-reason".into())
                        .m(px(8.0))
                        .p(px(8.0))
                        .rounded(px(4.0))
                        .bg(theme::pending_cell())
                        .text_size(px(11.0))
                        .text_color(theme::text())
                        .child(reason)
                }))
                .child(self.inspector_read_only())
                .into_any_element();
        }
        self.ensure_editor_inputs(window, cx);
        self.sync_editor_inputs(window, cx);
        let Some(graph) = self.snapshot.as_ref().map(|s| s.graph.clone()) else {
            return self.inspector_read_only();
        };
        let entity = &graph.entities[index];
        // The forms belong to one table: another selection closes them.
        let key = TableKey::of(&entity.table);
        if self.editor.forms_for.as_ref() != Some(&key) {
            self.editor.index_form = None;
            self.editor.fk_form = None;
            self.editor.forms_for = Some(key);
        }
        let state = self.table_state(index);
        let selected_column = self.editor.column.filter(|&c| c < entity.columns.len());
        let mut panel = div().flex().flex_col();
        panel = panel.child(self.inspector_header(index, entity, state, cx));
        panel = panel.child(section("COLUMNS").child(
            div().flex_1(),
        ).child(
            small_button("inspector-add-column", "+ Add column", "inspector-add-column")
                .disabled(state == TableState::Dropped)
                .on_click(cx.listener(move |this, _, window, cx| this.add_column(index, window, cx))),
        ));
        for (column_index, column) in entity.columns.iter().enumerate().take(MAX_ROWS) {
            panel = panel.child(self.column_row_element(index, column_index, column, selected_column == Some(column_index), cx));
            if selected_column == Some(column_index) {
                panel = panel.child(self.column_editor(index, column_index, entity, cx));
            }
        }
        panel = panel.children(self.index_section(index, entity, &graph, window, cx));
        panel = panel.children(self.foreign_key_section(index, entity, &graph, window, cx));
        panel = panel.children(self.editor.error.clone().map(|error| {
            div()
                .debug_selector(|| "inspector-error".into())
                .m(px(8.0))
                .text_size(px(11.0))
                .text_color(theme::danger())
                .child(error)
        }));
        panel.into_any_element()
    }

    fn inspector_header(&self, index: usize, entity: &DiagramEntity, state: TableState, cx: &mut Context<Self>) -> Div {
        let inputs = self.editor.inputs.as_ref();
        let summary = match &entity.state {
            EntityState::Ready => format!("{} columns, {} indexes", entity.columns.len(), entity.indexes.len()),
            EntityState::Loading => "Loading...".to_string(),
            EntityState::Failed(error) => format!("Could not load: {error}"),
        };
        let badge = match state {
            TableState::Clean => None,
            TableState::Changed => Some(("Edited: not applied yet", theme::warning())),
            TableState::New => Some(("New table: not created yet", theme::success())),
            TableState::Dropped => Some(("Will be dropped when the changes are applied", theme::danger())),
        };
        let scope = self.edit.as_ref().map(|e| e.scope.clone());
        let can_remove = !entity.focus;
        div()
            .px(px(10.0))
            .py(px(8.0))
            .flex()
            .flex_col()
            .gap(px(6.0))
            .child(
                div()
                    .debug_selector(|| "inspector-title".into())
                    .text_size(px(10.0))
                    .text_color(theme::text_faint())
                    .child(format!("Table · {summary}")),
            )
            .child(
                div()
                    .debug_selector(|| "inspector-table-name".into())
                    .children(inputs.map(|inputs| Input::new(&inputs.table).with_size(Size::Small).disabled(state == TableState::Dropped))),
            )
            .children({
                let stale = self.session.stale.iter().filter(|reason| reason.is_some()).count();
                (stale > 0).then(|| {
                    div()
                        .debug_selector(|| "inspector-stale".into())
                        .text_size(px(10.5))
                        .text_color(theme::danger())
                        .child(format!(
                            "{stale} pending {} stale: the table changed since. Refresh the diagram, then discard or redo {}.",
                            if stale == 1 { "change is" } else { "changes are" },
                            if stale == 1 { "it" } else { "them" }
                        ))
                })
            })
            .children(badge.map(|(text, color)| {
                div()
                    .debug_selector(|| "inspector-state".into())
                    .text_size(px(10.5))
                    .text_color(color)
                    .child(text)
            }))
            .child(
                div()
                    .flex()
                    .flex_wrap()
                    .gap(px(4.0))
                    .child(if state == TableState::Dropped {
                        small_button("inspector-restore-table", "Restore table", "inspector-restore-table")
                            .on_click(cx.listener(move |this, _, _, cx| this.restore_table(index, cx)))
                    } else {
                        small_button("inspector-drop-table", "Drop table...", "inspector-drop-table")
                            .tooltip("Adds DROP TABLE to the pending changes (asks first)")
                            .on_click(cx.listener(move |this, _, window, cx| this.request_drop_table(index, window, cx)))
                    })
                    .child(
                        small_button("inspector-remove-table", "Remove from diagram", "inspector-remove-table")
                            .disabled(!can_remove)
                            .tooltip("Hides the table from this diagram; the database is not touched")
                            .on_click(cx.listener(move |this, _, _, cx| this.remove_from_diagram(&[EntityId(index)], cx))),
                    ),
            )
            .children(scope.map(|scope| {
                div()
                    .text_size(px(10.0))
                    .text_color(theme::text_faint())
                    .child(format!("{} · types: {}", scope.dialect().label(), scope.dialect().common_types().iter().take(4).copied().collect::<Vec<_>>().join(", ")))
            }))
    }

    fn column_row_element(&self, index: usize, column_index: usize, column: &DiagramColumn, selected: bool, cx: &mut Context<Self>) -> gpui::AnyElement {
        let marker = if column.primary_key {
            Some(("icons/key-round.svg", theme::warning()))
        } else if column.foreign_key {
            Some(("icons/link.svg", theme::text_faint()))
        } else {
            None
        };
        let mut detail = vec![if column.nullable { "NULL" } else { "NOT NULL" }.to_string()];
        if column.primary_key {
            detail.push("PK".into());
        }
        if column.foreign_key && !column.primary_key {
            detail.push("FK".into());
        }
        if let Some(default) = column.default.as_deref().filter(|d| !d.is_empty()) {
            detail.push(format!("default {default}"));
        }
        let comment = column.comment.clone().filter(|c| !c.is_empty());
        div()
            .id(("inspector-column", index * 4096 + column_index))
            .debug_selector(move || format!("inspector-column-{column_index}"))
            .px(px(10.0))
            .py(px(3.0))
            .border_b_1()
            .border_color(theme::divider())
            .cursor_pointer()
            .when(selected, |this| this.bg(theme::selected()))
            .on_click(cx.listener(move |this, _, window, cx| this.select_column(Some(column_index), window, cx)))
            .child(
                div()
                    .flex()
                    .items_center()
                    .gap(px(4.0))
                    .child(div().w(px(14.0)).flex_shrink_0().children(
                        marker.map(|(path, color)| Icon::empty().path(path).size(px(11.0)).text_color(color)),
                    ))
                    .child(
                        div()
                            .flex_1()
                            .min_w_0()
                            .truncate()
                            .text_size(px(12.0))
                            .text_color(Self::column_color(column.pending))
                            .when(column.pending == ColumnState::Dropped, |this| this.line_through())
                            .when(column.primary_key, |this| this.font_weight(gpui::FontWeight::BOLD))
                            .child(column.name.clone()),
                    )
                    .child(
                        div()
                            .max_w(px(110.0))
                            .truncate()
                            .text_size(px(10.5))
                            .text_color(theme::text_faint())
                            .child(column.data_type.clone()),
                    ),
            )
            .child(div().pl(px(18.0)).truncate().text_size(px(10.5)).text_color(theme::text_faint()).child(detail.join(" · ")))
            .children(comment.map(|comment| {
                div()
                    .debug_selector(move || format!("inspector-comment-{column_index}"))
                    .pl(px(18.0))
                    .truncate()
                    .text_size(px(10.5))
                    .italic()
                    .text_color(theme::text_muted())
                    .child(comment)
            }))
            .into_any_element()
    }

    /// The editors of the selected column.
    fn column_editor(&self, index: usize, column_index: usize, entity: &DiagramEntity, cx: &mut Context<Self>) -> gpui::AnyElement {
        let (Some(inputs), Some(scope)) = (self.editor.inputs.as_ref(), self.edit.as_ref().map(|e| e.scope.clone())) else {
            return div().into_any_element();
        };
        let column = &entity.columns[column_index];
        let dialect = scope.dialect();
        let caps = dialect.capabilities();
        let dropped = column.pending == ColumnState::Dropped;
        // SQLite cannot alter an existing column; one the draft adds can be shaped freely.
        let alterable = caps.alter_column || column.pending == ColumnState::Added;
        let types: Vec<&'static str> = dialect.common_types().to_vec();
        let view = cx.entity().downgrade();
        let type_menu = Button::new("inspector-type-menu")
            .dropdown_caret(true)
            .with_size(Size::Small)
            .compact()
            .ghost()
            .disabled(dropped || !alterable)
            .tooltip("Common types")
            .debug_selector(|| "inspector-type-menu".into())
            .dropdown_menu(move |menu, _, _| {
                types.iter().enumerate().fold(menu, |menu, (position, &data_type)| {
                    let view = view.clone();
                    // An element entry carries the debug selector the tests click.
                    let entry = PopupMenuItem::element(move |_, _| {
                        div().debug_selector(move || format!("inspector-type-option-{position}")).child(data_type)
                    });
                    menu.item(entry.on_click(move |_, _, app| {
                        let _ = view.update(app, |this, cx| {
                            this.commit_editor_inputs(cx);
                            this.edit_column(index, column_index, cx, |spec| spec.data_type = data_type.to_string());
                        });
                    }))
                })
            });
        let is_key = column.primary_key;
        div()
            .debug_selector(|| "inspector-column-editor".into())
            .px(px(10.0))
            .py(px(6.0))
            .flex()
            .flex_col()
            .gap(px(6.0))
            .bg(theme::grid_header())
            .border_b_1()
            .border_color(theme::divider())
            .child(div().debug_selector(|| "inspector-field-name".into()).child(field(
                "Name",
                Input::new(&inputs.name).with_size(Size::Small).disabled(dropped),
            )))
            .child(div().debug_selector(|| "inspector-field-type".into()).child(field(
                "Type",
                div()
                    .flex()
                    .items_center()
                    .gap(px(2.0))
                    .child(div().flex_1().child(Input::new(&inputs.data_type).with_size(Size::Small).disabled(dropped || !alterable)))
                    .child(type_menu),
            )))
            .child(div().debug_selector(|| "inspector-field-default".into()).child(field(
                "Default",
                Input::new(&inputs.default).with_size(Size::Small).disabled(dropped || !alterable),
            )))
            .child(div().debug_selector(|| "inspector-field-comment".into()).child(field(
                "Comment",
                Input::new(&inputs.comment).with_size(Size::Small).disabled(dropped || !alterable || !caps.column_comments),
            )))
            .children((!alterable).then(|| {
                div().debug_selector(|| "inspector-not-alterable".into()).text_size(px(10.0)).text_color(theme::text_faint()).child(format!(
                    "{} cannot change the type, nullability, default or comment of an existing column without rebuilding the table.",
                    dialect.label()
                ))
            }))
            .children((alterable && !caps.column_comments).then(|| {
                div().text_size(px(10.0)).text_color(theme::text_faint()).child(format!("{} has no column comments.", dialect.label()))
            }))
            .child(
                div()
                    .flex()
                    .items_center()
                    .gap(px(12.0))
                    .child(div().debug_selector(|| "inspector-nullable".into()).child(
                        Checkbox::new("inspector-nullable-box")
                            .label("Allows NULL")
                            .checked(column.nullable)
                            .disabled(dropped || is_key || !alterable)
                            .on_click(cx.listener(move |this, checked: &bool, _, cx| {
                                let nullable = *checked;
                                this.commit_editor_inputs(cx);
                                this.edit_column(index, column_index, cx, |spec| spec.nullable = nullable);
                            })),
                    ))
                    .child(div().debug_selector(|| "inspector-pk".into()).child(
                        Checkbox::new("inspector-pk-box")
                            .label("Primary key")
                            .checked(column.primary_key)
                            .disabled(dropped || !caps.primary_key_edit)
                            .on_click(cx.listener(move |this, checked: &bool, _, cx| {
                                this.commit_editor_inputs(cx);
                                this.set_primary_key(index, column_index, *checked, cx);
                            })),
                    )),
            )
            .children((!caps.primary_key_edit).then(|| {
                div().text_size(px(10.0)).text_color(theme::text_faint()).child(format!(
                    "{} cannot change the primary key of an existing table{}.",
                    dialect.label(),
                    if dialect.label() == "ClickHouse" { " (the sorting key is fixed)" } else { " without rebuilding it" }
                ))
            }))
            .child(
                div().flex().gap(px(4.0)).child(if dropped {
                    small_button("inspector-restore-column", "Restore column", "inspector-restore-column")
                        .on_click(cx.listener(move |this, _, _, cx| this.restore_column(index, column_index, cx)))
                } else {
                    small_button("inspector-drop-column", "Drop column", "inspector-drop-column")
                        .on_click(cx.listener(move |this, _, _, cx| this.drop_column(index, column_index, cx)))
                }),
            )
            .into_any_element()
    }

    // ---- indexes ----

    fn index_section(
        &mut self,
        index: usize,
        entity: &DiagramEntity,
        _graph: &DiagramGraph,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Vec<gpui::AnyElement> {
        let Some(scope) = self.edit.as_ref().map(|e| e.scope.clone()) else { return Vec::new() };
        let dialect = scope.dialect();
        let caps = dialect.capabilities();
        let table = self.table_id(index);
        let pending_index = |name: &str| {
            table.as_ref().is_some_and(|t| {
                self.session.draft.changes.iter().any(|c| matches!(c, Change::CreateIndex { table: ct, index: i } if ct == t && i.name == name))
            })
        };
        let mut out: Vec<gpui::AnyElement> = vec![section("INDEXES")
            .child(div().flex_1())
            .child(
                small_button("inspector-add-index", "+ Add index", "inspector-add-index")
                    .disabled(!caps.indexes)
                    .on_click(cx.listener(|this, _, _, cx| {
                        this.editor.index_form = Some(IndexForm::default());
                        cx.notify();
                    })),
            )
            .into_any_element()];
        if !caps.indexes {
            out.push(
                div()
                    .px(px(10.0))
                    .text_size(px(10.5))
                    .text_color(theme::text_faint())
                    .child(format!("{} indexes are defined in SQL.", dialect.label()))
                    .into_any_element(),
            );
        }
        if entity.indexes.is_empty() {
            out.push(
                div().px(px(10.0)).py(px(4.0)).text_size(px(11.0)).text_color(theme::text_faint()).child("No indexes reported").into_any_element(),
            );
        }
        for (position, i) in entity.indexes.iter().enumerate().take(MAX_ROWS) {
            let mut flags = Vec::new();
            if i.primary {
                flags.push("primary".to_string());
            } else if i.unique {
                flags.push("unique".to_string());
            }
            if let Some(method) = &i.method {
                flags.push(method.clone());
            }
            let name = i.name.clone();
            let is_new = pending_index(&i.name);
            out.push(
                div()
                    .debug_selector(move || format!("inspector-index-{position}"))
                    .px(px(10.0))
                    .py(px(3.0))
                    .border_b_1()
                    .border_color(theme::divider())
                    .flex()
                    .items_center()
                    .gap(px(4.0))
                    .child(
                        div()
                            .flex_1()
                            .min_w_0()
                            .child(
                                div()
                                    .truncate()
                                    .text_size(px(12.0))
                                    .text_color(if is_new { theme::success() } else { theme::text() })
                                    .child(i.name.clone()),
                            )
                            .child(div().truncate().text_size(px(10.5)).text_color(theme::text_faint()).child(format!(
                                "({}){}",
                                i.columns.join(", "),
                                if flags.is_empty() { String::new() } else { format!(" · {}", flags.join(" · ")) }
                            ))),
                    )
                    .children((!i.primary && caps.indexes).then(|| {
                        icon_button(("inspector-drop-index", position), "icons/trash.svg", "Drop this index", format!("inspector-drop-index-{position}"))
                            .on_click(cx.listener(move |this, _, _, cx| this.drop_index(index, &name, cx)))
                    }))
                    .into_any_element(),
            );
        }
        if let Some(form) = self.editor.index_form.as_ref() {
            out.push(self.index_form_element(index, entity, form, cx));
        }
        out
    }

    fn index_form_element(&self, index: usize, entity: &DiagramEntity, form: &IndexForm, cx: &mut Context<Self>) -> gpui::AnyElement {
        let Some(inputs) = self.editor.inputs.as_ref() else { return div().into_any_element() };
        let chips = entity.columns.iter().enumerate().filter(|(_, c)| c.pending != ColumnState::Dropped).map(|(position, column)| {
            let name = column.name.clone();
            let on = form.columns.contains(&column.name);
            Button::new(("inspector-index-chip", position))
                .label(column.name.clone())
                .with_size(Size::Small)
                .compact()
                .ghost()
                .selected(on)
                .debug_selector(move || format!("inspector-index-column-{position}"))
                .on_click(cx.listener(move |this, _, _, cx| {
                    if let Some(form) = this.editor.index_form.as_mut() {
                        match form.columns.iter().position(|c| *c == name) {
                            Some(at) => {
                                form.columns.remove(at);
                            }
                            None => form.columns.push(name.clone()),
                        }
                    }
                    cx.notify();
                }))
        });
        div()
            .debug_selector(|| "inspector-index-form".into())
            .px(px(10.0))
            .py(px(6.0))
            .flex()
            .flex_col()
            .gap(px(6.0))
            .bg(theme::grid_header())
            .child(div().debug_selector(|| "inspector-index-name".into()).child(field("Index name", Input::new(&inputs.index_name).with_size(Size::Small))))
            .child(div().text_size(px(10.0)).text_color(theme::text_faint()).child("Columns (in order)"))
            .child(div().flex().flex_wrap().gap(px(3.0)).children(chips))
            .child(div().debug_selector(|| "inspector-index-unique".into()).child(
                Checkbox::new("inspector-index-unique-box").label("Unique").checked(form.unique).on_click(cx.listener(
                    |this, checked: &bool, _, cx| {
                        if let Some(form) = this.editor.index_form.as_mut() {
                            form.unique = *checked;
                        }
                        cx.notify();
                    },
                )),
            ))
            .child(
                div()
                    .flex()
                    .gap(px(4.0))
                    .child(
                        small_button("inspector-index-create", "Create index", "inspector-index-create").on_click(cx.listener(
                            move |this, _, window, cx| {
                                let Some(name) = this.editor.inputs.as_ref().map(|i| i.index_name.read(cx).value().to_string()) else {
                                    return;
                                };
                                let Some(form) = this.editor.index_form.as_ref() else { return };
                                let (columns, unique) = (form.columns.clone(), form.unique);
                                if this.add_index(index, &name, columns, unique, cx) {
                                    this.editor.index_form = None;
                                    if let Some(inputs) = &this.editor.inputs {
                                        inputs.index_name.update(cx, |state, cx| state.set_value("", window, cx));
                                    }
                                }
                            },
                        )),
                    )
                    .child(small_button("inspector-index-cancel", "Cancel", "inspector-index-cancel").on_click(cx.listener(
                        |this, _, _, cx| {
                            this.editor.index_form = None;
                            cx.notify();
                        },
                    ))),
            )
            .into_any_element()
    }

    // ---- foreign keys ----

    fn foreign_key_section(
        &mut self,
        index: usize,
        entity: &DiagramEntity,
        graph: &DiagramGraph,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Vec<gpui::AnyElement> {
        let Some(scope) = self.edit.as_ref().map(|e| e.scope.clone()) else { return Vec::new() };
        let dialect = scope.dialect();
        let caps = dialect.capabilities();
        let is_new = self.table_state(index) == TableState::New;
        let allowed = caps.foreign_keys || (dialect.inline_foreign_keys() && is_new);
        let mut out: Vec<gpui::AnyElement> = vec![section("FOREIGN KEYS")
            .child(div().flex_1())
            .child(
                small_button("inspector-add-fk", "+ Add foreign key", "inspector-add-fk")
                    .disabled(!allowed)
                    .on_click(cx.listener(move |this, _, window, cx| this.open_fk_form(window, cx))),
            )
            .into_any_element()];
        if !allowed {
            out.push(
                div()
                    .px(px(10.0))
                    .text_size(px(10.5))
                    .text_color(theme::text_faint())
                    .child(if dialect.label() == "SQLite" {
                        "SQLite can only add a foreign key to a table created in the same changes (an existing table would be rebuilt)."
                            .to_string()
                    } else {
                        format!("{} has no foreign keys.", dialect.label())
                    })
                    .into_any_element(),
            );
        }
        let outgoing: Vec<(usize, &super::super::model::DiagramRelation)> =
            graph.relations.iter().enumerate().filter(|(_, r)| r.child.0 == index).collect();
        if outgoing.is_empty() {
            out.push(
                div().px(px(10.0)).py(px(4.0)).text_size(px(11.0)).text_color(theme::text_faint()).child("No outgoing foreign keys").into_any_element(),
            );
        }
        for (relation_index, relation) in outgoing.into_iter().take(MAX_ROWS) {
            let parent = &graph.entities[relation.parent.0];
            let columns = |entity: &DiagramEntity, ids: &[usize]| ids.iter().filter_map(|&c| entity.columns.get(c)).map(|c| c.name.clone()).collect::<Vec<_>>().join(", ");
            let (tag, color) = match relation.origin {
                RelationOrigin::Declared => ("", theme::text()),
                RelationOrigin::Draft => ("new", theme::success()),
                RelationOrigin::Logical(_) => ("logical", theme::text_muted()),
            };
            let logical = matches!(relation.origin, RelationOrigin::Logical(_));
            let name = if logical { format!("({})", relation.name) } else { relation.name.clone() };
            out.push(
                div()
                    .debug_selector(move || format!("inspector-fk-{relation_index}"))
                    .px(px(10.0))
                    .py(px(3.0))
                    .border_b_1()
                    .border_color(theme::divider())
                    .flex()
                    .items_center()
                    .gap(px(4.0))
                    .child(
                        div()
                            .flex_1()
                            .min_w_0()
                            .child(div().truncate().text_size(px(12.0)).text_color(color).child(format!(
                                "{name}{}",
                                if tag.is_empty() { String::new() } else { format!(" · {tag}") }
                            )))
                            .child(div().truncate().text_size(px(10.5)).text_color(theme::text_faint()).child(format!(
                                "({}) → {} ({})",
                                columns(entity, &relation.child_columns),
                                parent.label,
                                columns(parent, &relation.parent_columns)
                            ))),
                    )
                    .child(if logical {
                        small_button(("inspector-promote-fk", relation_index), "Create FK", "inspector-promote-fk")
                            .disabled(!allowed)
                            .tooltip("Turns this logical relationship into a real foreign key (pending until applied)")
                            .debug_selector(move || format!("inspector-promote-fk-{relation_index}"))
                            .on_click(cx.listener(move |this, _, _, cx| {
                                this.promote_logical(relation_index, cx);
                            }))
                    } else {
                        icon_button(
                            ("inspector-drop-fk", relation_index),
                            "icons/trash.svg",
                            "Drop this foreign key",
                            format!("inspector-drop-fk-{relation_index}"),
                        )
                        .disabled(!allowed)
                        .on_click(cx.listener(move |this, _, _, cx| this.drop_foreign_key(relation_index, cx)))
                    })
                    .into_any_element(),
            );
        }
        if self.editor.fk_form.is_some() {
            out.push(self.fk_form_element(index, entity, graph, window, cx));
        }
        out
    }

    /// Opens the "add foreign key" form for entity `index`.
    pub(super) fn open_fk_form(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(graph) = self.snapshot.as_ref().map(|s| s.graph.clone()) else { return };
        self.ensure_editor_inputs(window, cx);
        let choices: Vec<usize> = (0..graph.entities.len()).filter(|&i| self.table_state(i) != TableState::Dropped).collect();
        let labels: Vec<SharedString> = choices.iter().map(|&i| SharedString::from(graph.entities[i].label.clone())).collect();
        let select = cx.new(|cx| SelectState::new(labels, None, window, cx));
        let subscription = cx.subscribe(&select, |this, select, event: &SelectEvent<Vec<SharedString>>, cx| {
            if !matches!(event, SelectEvent::Confirm(Some(_))) {
                return;
            }
            let row = select.read(cx).selected_index(cx).map(|i| i.row);
            if let Some(form) = this.editor.fk_form.as_mut() {
                form.parent = row.and_then(|row| form.choices.get(row).copied());
                form.parent_columns.clear();
            }
            cx.notify();
        });
        let on_delete = cx.new(|cx| {
            SelectState::new(structure_edit::specs::fk_action_labels(), Some(gpui_component::IndexPath::default().row(0)), window, cx)
        });
        let delete_subscription = cx.subscribe(&on_delete, |this, select, event: &SelectEvent<Vec<SharedString>>, cx| {
            if !matches!(event, SelectEvent::Confirm(Some(_))) {
                return;
            }
            let row = select.read(cx).selected_index(cx).map(|i| i.row).unwrap_or(0);
            if let Some(form) = this.editor.fk_form.as_mut() {
                form.on_delete = structure_edit::specs::fk_action_at(row);
            }
            cx.notify();
        });
        self.editor.fk_form = Some(FkForm {
            columns: Vec::new(),
            parent: None,
            parent_columns: Vec::new(),
            on_delete: None,
            select,
            on_delete_select: on_delete,
            choices,
            _subscriptions: vec![subscription, delete_subscription],
        });
        cx.notify();
    }

    fn fk_form_element(
        &self,
        index: usize,
        entity: &DiagramEntity,
        graph: &DiagramGraph,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) -> gpui::AnyElement {
        let (Some(form), Some(inputs)) = (self.editor.fk_form.as_ref(), self.editor.inputs.as_ref()) else {
            return div().into_any_element();
        };
        let chip = |prefix: &'static str, position: usize, name: String, on: bool, local: bool| {
            let click_name = name.clone();
            Button::new((prefix, position))
                .label(name)
                .with_size(Size::Small)
                .compact()
                .ghost()
                .selected(on)
                .debug_selector(move || format!("{prefix}-{position}"))
                .on_click(cx.listener(move |this, _, _, cx| {
                    if let Some(form) = this.editor.fk_form.as_mut() {
                        let list = if local { &mut form.columns } else { &mut form.parent_columns };
                        match list.iter().position(|c| *c == click_name) {
                            Some(at) => {
                                list.remove(at);
                            }
                            None => list.push(click_name.clone()),
                        }
                    }
                    cx.notify();
                }))
        };
        let local = entity
            .columns
            .iter()
            .enumerate()
            .filter(|(_, c)| c.pending != ColumnState::Dropped)
            .map(|(p, c)| chip("inspector-fk-column", p, c.name.clone(), form.columns.contains(&c.name), true));
        let remote: Vec<_> = form
            .parent
            .and_then(|p| graph.entities.get(p))
            .map(|parent| {
                parent
                    .columns
                    .iter()
                    .enumerate()
                    .filter(|(_, c)| c.pending != ColumnState::Dropped)
                    .map(|(p, c)| chip("inspector-fk-parent-column", p, c.name.clone(), form.parent_columns.contains(&c.name), false))
                    .collect()
            })
            .unwrap_or_default();
        div()
            .debug_selector(|| "inspector-fk-form".into())
            .px(px(10.0))
            .py(px(6.0))
            .flex()
            .flex_col()
            .gap(px(6.0))
            .bg(theme::grid_header())
            .child(div().debug_selector(|| "inspector-fk-name".into()).child(field("Constraint name", Input::new(&inputs.fk_name).with_size(Size::Small))))
            .child(div().text_size(px(10.0)).text_color(theme::text_faint()).child("Columns of this table"))
            .child(div().flex().flex_wrap().gap(px(3.0)).children(local))
            .child(div().debug_selector(|| "inspector-fk-parent".into()).child(field(
                "References",
                Select::new(&form.select).with_size(Size::Small).placeholder("Table"),
            )))
            .child(div().flex().flex_wrap().gap(px(3.0)).children(remote))
            .child(div().debug_selector(|| "inspector-fk-on-delete".into()).child(field(
                "On delete",
                Select::new(&form.on_delete_select).with_size(Size::Small),
            )))
            .child(
                div()
                    .flex()
                    .gap(px(4.0))
                    .child(small_button("inspector-fk-create", "Create foreign key", "inspector-fk-create").on_click(cx.listener(
                        move |this, _, window, cx| {
                            let Some(form) = this.editor.fk_form.as_ref() else { return };
                            let Some(parent) = form.parent.and_then(|p| this.table_id(p)) else {
                                this.editor.error = Some("Choose the referenced table.".into());
                                cx.notify();
                                return;
                            };
                            let name = this.editor.inputs.as_ref().map(|i| i.fk_name.read(cx).value().trim().to_string()).unwrap_or_default();
                            let table_name = this.table_id(index).map(|t| t.name).unwrap_or_default();
                            let spec = ForeignKeySpec {
                                name: if name.is_empty() { structure_edit::specs::default_foreign_key_name(&table_name, &form.columns, |_| false) } else { name },
                                columns: form.columns.clone(),
                                ref_table: parent,
                                ref_columns: form.parent_columns.clone(),
                                on_delete: form.on_delete,
                                on_update: None,
                            };
                            if this.add_foreign_key(index, spec, cx) {
                                this.editor.fk_form = None;
                                if let Some(inputs) = &this.editor.inputs {
                                    inputs.fk_name.update(cx, |state, cx| state.set_value("", window, cx));
                                }
                            }
                        },
                    )))
                    .child(small_button("inspector-fk-cancel", "Cancel", "inspector-fk-cancel").on_click(cx.listener(|this, _, _, cx| {
                        this.editor.fk_form = None;
                        cx.notify();
                    }))),
            )
            .into_any_element()
    }
}
