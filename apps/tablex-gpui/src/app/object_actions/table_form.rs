//! The New table form: the table's name and comment, its columns (name, type, null, primary key,
//! default, comment) as official `Input`s and `Checkbox`es, the dialect's common types behind a
//! caret menu. It collects a `CreateTable` change, checks it the way the structure editor does
//! (the names, the type and default text through the dialect's own generation) and hands it to the
//! shared review dialog: nothing runs from here.
//!
//! The form is a modal dialog with a handful of columns, built once and kept by one entity; the
//! grid-sized virtualization of the Properties editor does not apply to it.

use std::collections::HashSet;

use gpui::{ScrollHandle, Subscription, WeakEntity};
use gpui_component::dialog::DialogFooter;
use gpui_component::input::InputEvent;
use gpui_component::menu::DropdownMenu;

use super::super::structure_edit::{apply::TableOrigin, EditScope};
use super::super::*;
use crate::connections::ConnectionKind;
use crate::sql_ddl::{checked_name, dialect_for, plan_with_lock_timeout, Change, ColumnSpec, DdlDialect, DEFAULT_LOCK_TIMEOUT_SECS};

/// The text a column is typed as, before it is checked.
struct ColumnRow {
    /// Stable across removals, so the element ids of the other rows do not move.
    key: usize,
    name: gpui::Entity<InputState>,
    data_type: gpui::Entity<InputState>,
    default: gpui::Entity<InputState>,
    comment: gpui::Entity<InputState>,
    nullable: bool,
    primary_key: bool,
}

pub(in crate::app) struct NewTableForm {
    root: WeakEntity<AppRoot>,
    origin: TableOrigin,
    kind: ConnectionKind,
    dialect: &'static dyn DdlDialect,
    /// The table and view names the namespace holds, as far as the navigator has loaded them.
    taken: HashSet<String>,
    name: gpui::Entity<InputState>,
    comment: gpui::Entity<InputState>,
    columns: Vec<ColumnRow>,
    next_key: usize,
    error: Option<String>,
    scroll: ScrollHandle,
    subscriptions: Vec<Subscription>,
}

impl NewTableForm {
    /// A row for a new column; `id` fills it with the usual first column.
    fn new_row(&mut self, id: bool, window: &mut Window, cx: &mut Context<Self>) -> ColumnRow {
        let key = self.next_key;
        self.next_key += 1;
        let first_type = self.dialect.common_types().first().copied().unwrap_or_default();
        let mut input = |placeholder: &'static str, value: &str| {
            let state = cx.new(|cx| InputState::new(window, cx).placeholder(placeholder).default_value(value.to_owned()));
            self.subscriptions.push(cx.subscribe(&state, |this: &mut Self, _, event: &InputEvent, cx| {
                if matches!(event, InputEvent::Change) && this.error.take().is_some() {
                    cx.notify();
                }
            }));
            state
        };
        ColumnRow {
            key,
            name: input("column", if id { "id" } else { "" }),
            data_type: input("type", if id { first_type } else { "" }),
            default: input("default", ""),
            comment: input("comment", ""),
            nullable: !id,
            primary_key: id,
        }
    }

    /// Capture and tests: the form as if typed. Rows beyond the first are added.
    #[cfg(all(test, feature = "bench"))]
    pub(in crate::app) fn fill(&mut self, name: &str, comment: &str, rows: &[(&str, &str, bool, bool)], window: &mut Window, cx: &mut Context<Self>) {
        self.name.update(cx, |state, cx| state.set_value(name, window, cx));
        self.comment.update(cx, |state, cx| state.set_value(comment, window, cx));
        while self.columns.len() < rows.len() {
            let row = self.new_row(false, window, cx);
            self.columns.push(row);
        }
        for (row, (column, data_type, nullable, primary_key)) in self.columns.iter_mut().zip(rows) {
            row.name.update(cx, |state, cx| state.set_value(*column, window, cx));
            row.data_type.update(cx, |state, cx| state.set_value(*data_type, window, cx));
            row.nullable = *nullable;
            row.primary_key = *primary_key;
        }
        cx.notify();
    }

    fn scope(&self) -> EditScope {
        EditScope { kind: self.kind, database: self.origin.database.clone(), schema: Some(self.origin.schema.clone()).filter(|schema| !schema.is_empty()) }
    }

    /// The `CreateTable` the form describes, or what is wrong with it (the first problem).
    fn change(&self, cx: &gpui::App) -> Result<Change, String> {
        let text = |state: &gpui::Entity<InputState>| state.read(cx).value().to_string();
        let name = text(&self.name);
        checked_name(self.dialect, "table", &name).map_err(|error| error.to_string())?;
        if self.taken.contains(&name) {
            return Err(format!("A table or view named \"{name}\" exists already."));
        }
        let rules = self.dialect.name_rules();
        let mut columns: Vec<ColumnSpec> = Vec::new();
        let mut primary_key = Vec::new();
        for (at, row) in self.columns.iter().enumerate() {
            let (column, data_type, default, comment) = (text(&row.name), text(&row.data_type), text(&row.default), text(&row.comment));
            // A row nothing was typed in is an empty line, not a column.
            if [&column, &data_type, &default, &comment].iter().all(|text| text.trim().is_empty()) && !row.primary_key {
                continue;
            }
            let label = |problem: String| format!("Column {}: {problem}", at + 1);
            checked_name(self.dialect, "column", &column).map_err(|error| label(error.to_string()))?;
            if data_type.trim().is_empty() {
                return Err(label(format!("\"{column}\" needs a type.")));
            }
            if columns.iter().any(|existing| rules.same(&existing.name, &column)) {
                return Err(label(format!("there is a column named \"{column}\" already.")));
            }
            let nonblank = |text: String| Some(text.trim().to_owned()).filter(|text| !text.is_empty());
            if row.primary_key {
                primary_key.push(column.clone());
            }
            columns.push(ColumnSpec {
                name: column,
                data_type: data_type.trim().to_owned(),
                nullable: row.nullable && !row.primary_key,
                default: nonblank(default),
                comment: nonblank(comment),
                ..ColumnSpec::default()
            });
        }
        if columns.is_empty() {
            return Err("A table needs at least one column.".to_owned());
        }
        let table = self.scope().table_id(&self.origin.database, Some(self.origin.schema.as_str()), &name);
        let change = Change::CreateTable { table, columns, primary_key, comment: Some(text(&self.comment).trim().to_owned()).filter(|comment| !comment.is_empty()) };
        // What the dialect refuses of the typed text (a type with `;` in it, a comment on SQLite).
        let planned = plan_with_lock_timeout(self.dialect, std::slice::from_ref(&change), DEFAULT_LOCK_TIMEOUT_SECS);
        match planned.first_error() {
            Some((_, error)) => Err(error.to_string()),
            None => Ok(change),
        }
    }

    /// Checks the form and, when it is good, replaces this dialog with the review of the table.
    fn submit(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let change = match self.change(cx) {
            Ok(change) => change,
            Err(message) => {
                self.error = Some(message);
                return cx.notify();
            }
        };
        let origin = self.origin.clone();
        window.close_dialog(cx);
        let _ = self.root.update(cx, |root, cx| root.open_object_review(origin, move |_, _| vec![change], window, cx));
    }

    fn add_column(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let row = self.new_row(false, window, cx);
        let focus = row.name.clone();
        self.columns.push(row);
        self.error = None;
        focus.update(cx, |state, cx| state.focus(window, cx));
        cx.notify();
        // The new row is at the bottom of the list.
        self.scroll.scroll_to_bottom();
    }

    fn remove_column(&mut self, key: usize, cx: &mut Context<Self>) {
        self.columns.retain(|row| row.key != key);
        self.error = None;
        cx.notify();
    }

    fn column_row(&self, row: &ColumnRow, comments: bool, cx: &mut Context<Self>) -> impl IntoElement {
        let key = row.key;
        let types: Vec<&'static str> = self.dialect.common_types().to_vec();
        let type_input = row.data_type.clone();
        let cell = |selector: &'static str, input: &gpui::Entity<InputState>| {
            div().debug_selector(move || format!("{selector}-{key}")).flex_1().min_w(px(90.0)).child(Input::new(input).small())
        };
        div()
            .debug_selector(move || format!("object-column-{key}"))
            .flex()
            .items_center()
            .gap(px(6.0))
            .child(cell("object-column-name", &row.name))
            .child(
                div()
                    .flex()
                    .flex_1()
                    .min_w(px(120.0))
                    .items_center()
                    .gap(px(2.0))
                    .child(cell("object-column-type", &row.data_type))
                    .child(
                        Button::new(("object-column-types", key))
                            .debug_selector(move || format!("object-column-types-{key}"))
                            .icon(IconName::ChevronDown)
                            .ghost()
                            .compact()
                            .tooltip("Common types")
                            .dropdown_menu(move |menu, _, _| {
                                types.iter().fold(menu, |menu, &data_type| {
                                    let type_input = type_input.clone();
                                    menu.item(PopupMenuItem::new(data_type).on_click(move |_, window, cx| {
                                        type_input.update(cx, |state, cx| state.set_value(data_type, window, cx));
                                    }))
                                })
                            }),
                    ),
            )
            .child(
                div().debug_selector(move || format!("object-column-null-{key}")).w(px(44.0)).flex().justify_center().child(
                    Checkbox::new(("object-column-null-box", key)).checked(row.nullable && !row.primary_key).disabled(row.primary_key).on_click(
                        cx.listener(move |this, checked: &bool, _, cx| {
                            let checked = *checked;
                            if let Some(row) = this.columns.iter_mut().find(|row| row.key == key) {
                                row.nullable = checked;
                            }
                            cx.notify();
                        }),
                    ),
                ),
            )
            .child(
                div().debug_selector(move || format!("object-column-key-{key}")).w(px(36.0)).flex().justify_center().child(
                    Checkbox::new(("object-column-key-box", key)).checked(row.primary_key).on_click(cx.listener(move |this, checked: &bool, _, cx| {
                        let checked = *checked;
                        if let Some(row) = this.columns.iter_mut().find(|row| row.key == key) {
                            row.primary_key = checked;
                        }
                        cx.notify();
                    })),
                ),
            )
            .child(cell("object-column-default", &row.default))
            .children(comments.then(|| cell("object-column-comment", &row.comment)))
            .child(
                Button::new(("object-column-remove", key))
                    .debug_selector(move || format!("object-column-remove-{key}"))
                    .icon(IconName::Close)
                    .ghost()
                    .compact()
                    .tooltip("Remove the column")
                    .on_click(cx.listener(move |this, _, _, cx| this.remove_column(key, cx))),
            )
    }
}

impl Render for NewTableForm {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let caps = self.dialect.capabilities();
        let heading = |label: &'static str, width: Option<f32>| {
            let cell = div().text_size(px(11.0)).text_color(theme::text_muted()).child(label);
            match width {
                Some(width) => cell.w(px(width)).flex().justify_center(),
                None => cell.flex_1().min_w(px(90.0)),
            }
        };
        let rows: Vec<_> = self.columns.iter().map(|row| self.column_row(row, caps.column_comments, cx).into_any_element()).collect();
        div()
            .flex()
            .flex_col()
            .gap(px(10.0))
            .child(
                div()
                    .flex()
                    .gap(px(8.0))
                    .child(
                        div()
                            .debug_selector(|| "object-table-name".into())
                            .flex_1()
                            .child(Input::new(&self.name)),
                    )
                    .child(
                        div()
                            .debug_selector(|| "object-table-comment".into())
                            .flex_1()
                            .child(Input::new(&self.comment)),
                    ),
            )
            .child(
                div()
                    .flex()
                    .flex_col()
                    .gap(px(4.0))
                    .child(
                        div()
                            .flex()
                            .gap(px(6.0))
                            .child(heading("Column", None))
                            .child(heading("Type", None))
                            .child(heading("Null", Some(44.0)))
                            .child(heading("PK", Some(36.0)))
                            .child(heading("Default", None))
                            .children(caps.column_comments.then(|| heading("Comment", None)))
                            .child(div().w(px(28.0))),
                    )
                    .child(
                        div()
                            .id("object-table-columns")
                            .debug_selector(|| "object-table-columns".into())
                            .max_h(px(280.0))
                            .overflow_y_scroll()
                            .track_scroll(&self.scroll)
                            .flex()
                            .flex_col()
                            .gap(px(4.0))
                            .children(rows),
                    ),
            )
            .child(
                div().flex().child(
                    Button::new("object-table-add-column")
                        .debug_selector(|| "object-table-add-column".into())
                        .label("Add column")
                        .icon(IconName::Plus)
                        .compact()
                        .ghost()
                        .on_click(cx.listener(|this, _, window, cx| this.add_column(window, cx))),
                ),
            )
            .when_some(self.error.clone(), |this, error| {
                this.child(div().debug_selector(|| "object-table-error".into()).text_size(px(11.5)).text_color(theme::danger()).child(error))
            })
            .child(div().text_size(px(11.0)).text_color(theme::text_muted()).child(
                "The SQL is shown next, and nothing runs until you apply it. An empty line is not a column.",
            ))
    }
}

impl AppRoot {
    /// New table in the namespace `origin` names: the form, then the review.
    pub(super) fn open_new_table_dialog(&mut self, origin: TableOrigin, window: &mut Window, cx: &mut Context<Self>) -> Option<gpui::Entity<NewTableForm>> {
        let Some(kind) = self.saved_connections.iter().find(|saved| saved.matches_id(&origin.connection)).map(|saved| saved.kind) else {
            self.sidebar.notice = Some("This connection has no SQL structure to change.".to_owned());
            cx.notify();
            return None;
        };
        let Some(dialect) = dialect_for(kind) else {
            self.sidebar.notice = Some("This connection has no SQL structure to change.".to_owned());
            cx.notify();
            return None;
        };
        let taken = self.loaded_object_names(&origin);
        let root = cx.entity().downgrade();
        let namespace = if origin.schema.is_empty() { origin.database.clone() } else { origin.schema.clone() };
        let view = cx.new(|cx| {
            let name = cx.new(|cx| InputState::new(window, cx).placeholder("Table name"));
            let comment = cx.new(|cx| InputState::new(window, cx).placeholder("Table comment (optional)"));
            let mut form = NewTableForm {
                root,
                origin,
                kind,
                dialect,
                taken,
                name,
                comment,
                columns: Vec::new(),
                next_key: 0,
                error: None,
                scroll: ScrollHandle::new(),
                subscriptions: Vec::new(),
            };
            for state in [form.name.clone(), form.comment.clone()] {
                form.subscriptions.push(cx.subscribe(&state, |this: &mut NewTableForm, _, event: &InputEvent, cx| {
                    if matches!(event, InputEvent::Change) && this.error.take().is_some() {
                        cx.notify();
                    }
                }));
            }
            let first = form.new_row(true, window, cx);
            form.columns.push(first);
            form
        });
        let focus_name = view.read(cx).name.clone();
        let title = format!("New table in {namespace}");
        let form = view.clone();
        window.open_dialog(cx, move |dialog, _, _| {
            let (submit_view, enter_view) = (view.clone(), view.clone());
            dialog
                .title(title.clone())
                .w(px(940.0))
                .child(view.clone())
                // Enter goes to the review; the dialog closes itself only once the form is good.
                .on_ok(move |_, window, cx| {
                    enter_view.update(cx, |this, cx| this.submit(window, cx));
                    false
                })
                .footer(
                    DialogFooter::new()
                        .child(
                            Button::new("object-table-cancel")
                                .label("Cancel")
                                .compact()
                                .ghost()
                                .debug_selector(|| "object-table-cancel".into())
                                .on_click(|_, window, cx| window.close_dialog(cx)),
                        )
                        .child(
                            Button::new("object-table-submit")
                                .label("Review SQL")
                                .compact()
                                .primary()
                                .debug_selector(|| "object-table-submit".into())
                                .on_click(move |_, window, cx| {
                                    submit_view.update(cx, |this, cx| this.submit(window, cx));
                                }),
                        ),
                )
        });
        focus_name.update(cx, |state, cx| state.focus(window, cx));
        Some(form)
    }
}
