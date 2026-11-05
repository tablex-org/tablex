//! The name dialog of Rename (table, view, materialized view) and of New schema / New database:
//! one official `Input`, validated against the dialect and the names the navigator has loaded,
//! then the review dialog. Enter and the primary button both go to the review; Escape, Cancel and
//! a click outside dismiss without touching anything.

use std::collections::HashSet;

use gpui::{Subscription, WeakEntity};
use gpui_component::dialog::DialogFooter;
use gpui_component::input::InputEvent;

use super::super::structure_edit::apply::TableOrigin;
use super::super::*;
use super::{object_table, ObjectKind};
use crate::sql_ddl::{checked_name, dialect_for, Change, DdlDialect, NamespaceKind};

/// What the name is for.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum NameFlow {
    Rename(ObjectKind),
    Namespace(NamespaceKind),
}

impl NameFlow {
    fn title(self) -> String {
        match self {
            Self::Rename(kind) => format!("Rename {}", kind.noun()),
            Self::Namespace(kind) => format!("New {}", kind.noun()),
        }
    }

    /// What the name is of: "table", "schema".
    fn noun(self) -> &'static str {
        match self {
            Self::Rename(kind) => kind.noun(),
            Self::Namespace(kind) => kind.noun(),
        }
    }
}

/// Why `name` cannot be the name: the dialect's own checks, then the current name and the names
/// the namespace already holds.
pub(super) fn check_new_name(
    dialect: &dyn DdlDialect,
    flow: NameFlow,
    current: &str,
    taken: &HashSet<String>,
    name: &str,
) -> Result<(), String> {
    checked_name(dialect, flow.noun(), name).map_err(|error| error.to_string())?;
    if matches!(flow, NameFlow::Rename(_)) && name == current {
        return Err("That is the current name.".to_owned());
    }
    if taken.contains(name) {
        return Err(match flow {
            NameFlow::Rename(_) => format!("A table or view named \"{name}\" exists already."),
            NameFlow::Namespace(kind) => format!("A {} named \"{name}\" exists already.", kind.noun()),
        });
    }
    Ok(())
}

struct NameDialog {
    root: WeakEntity<AppRoot>,
    flow: NameFlow,
    origin: TableOrigin,
    dialect: &'static dyn DdlDialect,
    taken: HashSet<String>,
    input: gpui::Entity<InputState>,
    error: Option<String>,
    _subscription: Subscription,
}

impl NameDialog {
    /// Checks the name and, when it is good, replaces this dialog with the review of the change.
    fn submit(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let name = self.input.read(cx).value().to_string();
        let current = self.origin.object.clone();
        if let Err(message) = check_new_name(self.dialog_dialect(), self.flow, &current, &self.taken, &name) {
            self.error = Some(message);
            return cx.notify();
        }
        let flow = self.flow;
        let origin = self.origin.clone();
        // This dialog goes before the review opens: one dialog at a time.
        window.close_dialog(cx);
        let _ = self.root.update(cx, |root, cx| {
            root.open_object_review(
                origin,
                move |context, origin| {
                    vec![match flow {
                        NameFlow::Rename(ObjectKind::Table) => Change::RenameTable { table: object_table(context, origin), to: name },
                        NameFlow::Rename(kind) => Change::RenameView {
                            table: object_table(context, origin),
                            to: name,
                            materialized: kind == ObjectKind::MaterializedView,
                        },
                        NameFlow::Namespace(kind) => Change::CreateNamespace { kind, name },
                    }]
                },
                window,
                cx,
            )
        });
    }

    fn dialog_dialect(&self) -> &dyn DdlDialect {
        self.dialect
    }
}

impl Render for NameDialog {
    fn render(&mut self, _window: &mut Window, _cx: &mut Context<Self>) -> impl IntoElement {
        let prompt = match self.flow {
            NameFlow::Rename(kind) => format!("New name for the {} {}", kind.noun(), self.origin.object),
            NameFlow::Namespace(kind) => format!("Name of the new {}", kind.noun()),
        };
        div()
            .flex()
            .flex_col()
            .gap(px(8.0))
            .child(div().text_size(px(12.0)).child(prompt))
            .child(div().debug_selector(|| "object-name-input".into()).child(Input::new(&self.input)))
            .when_some(self.error.clone(), |this, error| {
                this.child(
                    div().debug_selector(|| "object-name-error".into()).text_size(px(11.0)).text_color(theme::danger()).child(error),
                )
            })
            .child(div().text_size(px(11.0)).text_color(theme::text_muted()).child(match self.flow {
                NameFlow::Rename(_) => "Queries, views and code that use the old name break. The SQL is shown next, and nothing runs until you apply it.",
                NameFlow::Namespace(_) => "The SQL is shown next, and nothing runs until you apply it.",
            }))
    }
}

impl AppRoot {
    pub(super) fn open_name_dialog(&mut self, flow: NameFlow, origin: TableOrigin, window: &mut Window, cx: &mut Context<Self>) {
        let Some(dialect) = self
            .saved_connections
            .iter()
            .find(|saved| saved.matches_id(&origin.connection))
            .and_then(|saved| dialect_for(saved.kind))
        else {
            self.sidebar.notice = Some("This connection has no SQL structure to change.".to_owned());
            return cx.notify();
        };
        let taken = match flow {
            NameFlow::Rename(_) => self.loaded_object_names(&origin),
            NameFlow::Namespace(kind) => self.loaded_namespace_names(&origin, kind),
        };
        let initial = if matches!(flow, NameFlow::Rename(_)) { origin.object.clone() } else { String::new() };
        let input = cx.new(|cx| InputState::new(window, cx).default_value(initial).placeholder(match flow {
            NameFlow::Rename(_) => "New name",
            NameFlow::Namespace(kind) => if kind == NamespaceKind::Schema { "Schema name" } else { "Database name" },
        }));
        let focus_input = input.clone();
        let root = cx.entity().downgrade();
        let view = cx.new(|cx| {
            // Enter reaches the dialog's own `on_ok` below; only edits are observed here.
            let subscription = cx.subscribe(&input, |this: &mut NameDialog, _, event: &InputEvent, cx| {
                if matches!(event, InputEvent::Change) {
                    this.error = None;
                    cx.notify();
                }
            });
            NameDialog { root, flow, origin, dialect, taken, input, error: None, _subscription: subscription }
        });
        let title = flow.title();
        window.open_dialog(cx, move |dialog, _, _| {
            let (submit_view, enter_view) = (view.clone(), view.clone());
            dialog
                .title(title.clone())
                .w(px(440.0))
                .child(view.clone())
                // Enter goes to the review; the dialog closes itself only once the name is good.
                .on_ok(move |_, window, cx| {
                    enter_view.update(cx, |this, cx| this.submit(window, cx));
                    false
                })
                .footer(
                    DialogFooter::new()
                        .child(
                            Button::new("object-name-cancel")
                                .label("Cancel")
                                .compact()
                                .ghost()
                                .debug_selector(|| "object-name-cancel".into())
                                .on_click(|_, window, cx| window.close_dialog(cx)),
                        )
                        .child(
                            Button::new("object-name-submit")
                                .label("Review SQL")
                                .compact()
                                .primary()
                                .debug_selector(|| "object-name-submit".into())
                                .on_click(move |_, window, cx| {
                                    submit_view.update(cx, |this, cx| this.submit(window, cx));
                                }),
                        ),
                )
        });
        // Opening a dialog moves focus to the dialog itself; the field takes it back so typing and
        // Enter work at once.
        focus_input.update(cx, |state, cx| state.focus(window, cx));
    }

    /// The schemas (PostgreSQL) or databases the navigator has loaded where a new one would go.
    fn loaded_namespace_names(&self, origin: &TableOrigin, kind: NamespaceKind) -> HashSet<String> {
        let parent = match kind {
            NamespaceKind::Schema => NavigatorNodeId::Database { connection: origin.connection.clone(), database: origin.database.clone() },
            NamespaceKind::Database => NavigatorNodeId::Connection { connection: origin.connection.clone() },
        };
        self.navigator
            .loaded_children(&parent)
            .into_iter()
            .flatten()
            .filter_map(|child| match child {
                NavigatorNodeId::Schema { schema, .. } => Some(schema.clone()),
                NavigatorNodeId::Database { database, .. } => Some(database.clone()),
                _ => None,
            })
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::connections::ConnectionKind;

    fn check(kind: ConnectionKind, flow: NameFlow, current: &str, taken: &[&str], name: &str) -> Result<(), String> {
        let taken: HashSet<String> = taken.iter().map(|name| (*name).to_owned()).collect();
        check_new_name(dialect_for(kind).unwrap(), flow, current, &taken, name)
    }

    #[test]
    fn a_rename_is_refused_for_the_current_name_a_taken_one_and_a_name_that_is_not_one() {
        let rename = NameFlow::Rename(ObjectKind::Table);
        let pg = ConnectionKind::Postgres;
        assert_eq!(check(pg, rename, "items", &["items", "other"], "things"), Ok(()));
        assert_eq!(check(pg, rename, "items", &[], "items").unwrap_err(), "That is the current name.");
        assert_eq!(check(pg, rename, "items", &["other"], "other").unwrap_err(), "A table or view named \"other\" exists already.");
        for name in ["", "   ", " x", "x ", "a\nb", "a\0b"] {
            assert!(check(pg, rename, "items", &[], name).is_err(), "{name:?}");
        }
        assert!(check(pg, rename, "items", &[], &"x".repeat(64)).unwrap_err().contains("longer than the 63 bytes"));
        // Quotes, backticks and semicolons are names: they are quoted, not refused.
        assert_eq!(check(pg, rename, "items", &[], "we\"ird`;name"), Ok(()));
    }

    #[test]
    fn a_new_namespace_name_is_checked_against_the_namespaces_the_navigator_holds() {
        let schema = NameFlow::Namespace(NamespaceKind::Schema);
        assert_eq!(check(ConnectionKind::Postgres, schema, "", &["public"], "reports"), Ok(()));
        assert_eq!(
            check(ConnectionKind::Postgres, schema, "", &["public"], "public").unwrap_err(),
            "A schema named \"public\" exists already."
        );
        let database = NameFlow::Namespace(NamespaceKind::Database);
        assert!(check(ConnectionKind::Mysql, database, "", &[], &"d".repeat(65)).is_err());
        // A namespace may be named like the (empty) current name: only a rename compares with it.
        assert_eq!(check(ConnectionKind::Mysql, database, "", &[], "shop"), Ok(()));
    }
}
