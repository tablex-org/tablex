//! The prompt for `:name` / `${name}` parameters, shown before a run (and an Explain) of the
//! editor's SQL. Each name is asked once, with a type (text, number, NULL, raw SQL); what was
//! typed is offered again by the next run of the same tab. The values are written into the
//! statement as literals quoted for the connection's dialect (`sql_params`).

use super::*;
use crate::sql_params::{self, BindError, Parameter, Value, ValueKind};
use gpui::{Subscription, WeakEntity};
use gpui_component::dialog::DialogFooter;
use gpui_component::select::{Select, SelectState};
use gpui_component::IndexPath;

/// What runs once the parameters are bound.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum AfterParams {
    Run,
    Explain(engine_plan::ExplainMode),
}

struct ParamRow {
    name: String,
    input: gpui::Entity<InputState>,
    kind: gpui::Entity<SelectState<Vec<&'static str>>>,
}

/// The dialog's body: it owns the inputs, because the dialog builder runs during the root's
/// render and must never touch `AppRoot`.
struct ParamDialog {
    root: WeakEntity<AppRoot>,
    tab_id: u64,
    sql: String,
    parameters: Vec<Parameter>,
    rows: Vec<ParamRow>,
    then: AfterParams,
    error: Option<String>,
    _subscriptions: Vec<Subscription>,
}

impl ParamDialog {
    fn values(&self, cx: &gpui::App) -> HashMap<String, Value> {
        self.rows
            .iter()
            .map(|row| {
                let label = row.kind.read(cx).selected_value().copied();
                let kind = ValueKind::ALL
                    .into_iter()
                    .find(|kind| Some(kind.label()) == label)
                    .unwrap_or_default();
                (row.name.clone(), Value { kind, text: row.input.read(cx).value().to_string() })
            })
            .collect()
    }

    /// Binds and runs. `true` closes the dialog; a value that cannot be written (a number that is
    /// not one) keeps it open and says why.
    fn submit(&mut self, cx: &mut Context<Self>) -> bool {
        let values = self.values(cx);
        let outcome = self.root.update(cx, |root, cx| {
            root.submit_parameters(self.tab_id, &self.sql, &self.parameters, values, self.then, cx)
        });
        match outcome {
            Ok(Ok(())) => true,
            Ok(Err(error)) => {
                self.error = Some(error.to_string());
                cx.notify();
                false
            }
            // The app is gone.
            Err(_) => true,
        }
    }
}

impl Render for ParamDialog {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let rows = self.rows.iter().map(|row| {
            let off = row.kind.read(cx).selected_value().copied() == Some(ValueKind::Null.label());
            let (name, input_selector, kind_selector) =
                (row.name.clone(), format!("param-input-{}", row.name), format!("param-kind-{}", row.name));
            div()
                .flex()
                .items_center()
                .gap(px(8.0))
                .child(
                    div()
                        .w(px(110.0))
                        .flex_shrink_0()
                        .truncate()
                        .text_size(px(12.0))
                        .text_color(theme::text_muted())
                        .child(format!(":{name}")),
                )
                .child(
                    div()
                        .w(px(110.0))
                        .flex_shrink_0()
                        .debug_selector(move || kind_selector.clone())
                        .child(Select::new(&row.kind).small()),
                )
                .child(
                    div()
                        .flex_1()
                        .min_w_0()
                        .debug_selector(move || input_selector.clone())
                        .child(Input::new(&row.input).small().disabled(off)),
                )
        });
        div()
            .flex()
            .flex_col()
            .gap(px(8.0))
            .child(
                div()
                    .text_size(px(11.0))
                    .text_color(theme::text_muted())
                    .child("Values are written into the statement as literals quoted for this database."),
            )
            .children(rows)
            .when_some(self.error.clone(), |this, error| {
                this.child(
                    div()
                        .debug_selector(|| "param-error".into())
                        .text_size(px(11.0))
                        .text_color(theme::danger())
                        .child(error),
                )
            })
    }
}

impl AppRoot {
    /// Asks for the parameters of `sql` when it has any (and the setting is on), then goes on
    /// with `then`. `true` when the prompt took over: the caller must not run `sql` itself.
    pub(super) fn prompt_parameters(
        &mut self,
        tab_id: u64,
        sql: &str,
        then: AfterParams,
        cx: &mut Context<Self>,
    ) -> bool {
        if !self.settings.editor.parameters
            || !self
                .workspace
                .tab(tab_id)
                .is_some_and(|tab| RunKind::of_tab(tab) == RunKind::Sql && !tab.query_mode.is_mongo())
        {
            return false;
        }
        let parameters = sql_params::find(sql, &self.sql_lex(tab_id));
        if parameters.is_empty() {
            return false;
        }
        let Some(window) = self.driver_runtime.window else {
            self.refuse_statement(tab_id, sql, "Could not ask for parameters: no window to ask in".into(), cx);
            return true;
        };
        let remembered = self
            .workspace
            .runtime(tab_id)
            .map(|runtime| runtime.param_values.clone())
            .unwrap_or_default();
        let (sql, root) = (sql.to_owned(), cx.entity().downgrade());
        // The window cannot be updated while this entity is: open the dialog once the current
        // update has finished.
        cx.defer(move |cx| {
            let _ = window.update(cx, |_, window, cx| {
                let _ = root.update(cx, |this, cx| {
                    this.open_parameter_dialog(tab_id, sql, parameters, remembered, then, window, cx);
                });
            });
        });
        true
    }

    /// Remembers what was typed and, when every value can be written, runs `then` with the
    /// statement bound.
    pub(super) fn submit_parameters(
        &mut self,
        tab_id: u64,
        sql: &str,
        parameters: &[Parameter],
        values: HashMap<String, Value>,
        then: AfterParams,
        cx: &mut Context<Self>,
    ) -> Result<(), BindError> {
        let bound = sql_params::bind(sql, parameters, &values, &self.sql_lex(tab_id));
        if let Some(runtime) = self.workspace.runtime_mut(tab_id) {
            runtime.param_values.extend(values);
        }
        let bound = bound?;
        match then {
            AfterParams::Run => self.run_typed_sql(tab_id, bound, true, cx),
            AfterParams::Explain(mode) => self.explain_bound(tab_id, mode, bound, cx),
        }
        Ok(())
    }

    #[allow(clippy::too_many_arguments)]
    fn open_parameter_dialog(
        &mut self,
        tab_id: u64,
        sql: String,
        parameters: Vec<Parameter>,
        remembered: HashMap<String, Value>,
        then: AfterParams,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let root = cx.entity().downgrade();
        let labels: Vec<&'static str> = ValueKind::ALL.iter().map(|kind| kind.label()).collect();
        let view = cx.new(|cx| {
            let mut subscriptions = Vec::new();
            let rows = parameters
                .iter()
                .map(|parameter| {
                    let value = remembered.get(&parameter.name).cloned().unwrap_or_default();
                    let row = ValueKind::ALL.iter().position(|kind| *kind == value.kind).unwrap_or(0);
                    let input = cx.new(|cx| InputState::new(window, cx).default_value(value.text));
                    let kind = cx.new(|cx| {
                        SelectState::new(labels.clone(), Some(IndexPath::default().row(row)), window, cx)
                    });
                    // The kind decides whether the input is used: redraw when it changes.
                    subscriptions.push(cx.observe(&kind, |_, _, cx| cx.notify()));
                    ParamRow { name: parameter.name.clone(), input, kind }
                })
                .collect();
            ParamDialog { root, tab_id, sql, parameters, rows, then, error: None, _subscriptions: subscriptions }
        });
        let first = view.read(cx).rows.first().map(|row| row.input.clone());
        window.open_dialog(cx, move |dialog, _, _| {
            let submit_view = view.clone();
            let enter_view = view.clone();
            dialog
                .title("Parameters")
                .w(px(520.0))
                .child(view.clone())
                // Enter confirms through the dialog; the dialog stays when a value is refused.
                .on_ok(move |_, _, cx| enter_view.update(cx, |this, cx| this.submit(cx)))
                .footer(
                    DialogFooter::new()
                        .child(
                            Button::new("param-cancel")
                                .label("Cancel")
                                .compact()
                                .ghost()
                                .debug_selector(|| "param-cancel".into())
                                .on_click(|_, window, cx| window.close_dialog(cx)),
                        )
                        .child(
                            Button::new("param-run")
                                .label("Run")
                                .compact()
                                .primary()
                                .debug_selector(|| "param-run".into())
                                .on_click(move |_, window, cx| {
                                    if submit_view.update(cx, |this, cx| this.submit(cx)) {
                                        window.close_dialog(cx);
                                    }
                                }),
                        ),
                )
        });
        // Opening a dialog moves focus to the dialog itself; the first value takes it back so
        // typing and Enter work at once.
        if let Some(first) = first {
            first.update(cx, |state, cx| state.focus(window, cx));
        }
    }
}
