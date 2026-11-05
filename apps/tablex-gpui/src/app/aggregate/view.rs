//! Rendering of the Aggregate tab: header, Text editor + output preview,
//! stage cards, Options popover and the Run output area.
//!
//! Controls are the official ones: `Editor` / `EditorState` for pipeline
//! text, `Select` for the stage operator, `Switch`, `ButtonGroup`,
//! `DropdownButton`, `Popover` and `Dialog`. Output previews are virtualized
//! `uniform_list`s over pre-flattened rows; nothing here parses, formats or
//! flattens per frame.
use super::*;
use crate::mongo_result::MongoTreeRow;
use gpui_component::button::ButtonGroup;
use gpui_component::select::Select;

/// What one stage card needs of its stage.
struct StageView {
    id: u64,
    operator: String,
    enabled: bool,
}

/// Drag payload of a stage card's grip (also its ghost).
#[derive(Clone)]
struct StageDrag {
    tab_id: u64,
    stage_id: u64,
    label: gpui::SharedString,
}

impl Render for StageDrag {
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

fn menu_entry(
    selector: &'static str,
    label: &'static str,
    on_click: impl Fn(&mut Window, &mut gpui::App) + 'static,
) -> PopupMenuItem {
    PopupMenuItem::element(move |_, _| {
        div()
            .id(selector)
            .debug_selector(move || selector.to_string())
            .child(label)
    })
    .on_click(move |_, window, app| on_click(window, app))
}

fn muted(text: impl Into<gpui::SharedString>) -> gpui::Div {
    div()
        .text_size(px(11.0))
        .text_color(theme::text_muted())
        .child(text.into())
}

impl AppRoot {
    /// Everything below the tab bar for an Aggregate tab.
    pub(in crate::app) fn render_aggregate_tab(
        &mut self,
        tab_id: u64,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        if self.aggregate_draft(tab_id).is_none() {
            return div().into_any_element();
        }
        self.ensure_aggregate_derived(tab_id);
        self.start_aggregate_tab(tab_id, cx);
        let header = self.render_aggregate_header(tab_id, window, cx);
        let draft_mode = self.aggregate_draft(tab_id).map(|draft| draft.mode);
        let body = match draft_mode {
            Some(EditorMode::Stages) => self.render_aggregate_stages(tab_id, window, cx),
            _ => self.render_aggregate_text(tab_id, window, cx),
        };
        let ran = self
            .workspace
            .runtime(tab_id)
            .is_some_and(|runtime| !matches!(runtime.query.status(), crate::query::QueryStatus::Idle));
        let editor_area = div()
            .flex()
            .flex_col()
            .size_full()
            .min_h_0()
            .child(header)
            .child(div().flex_1().min_h_0().child(body));
        let content = if ran {
            let results = self.render_aggregate_results(tab_id, window, cx);
            gpui_component::resizable::v_resizable(("agg-split", tab_id))
                .child(
                    gpui_component::resizable::resizable_panel()
                        .size(px(430.0))
                        .size_range(px(260.0)..gpui::Pixels::MAX)
                        .child(editor_area),
                )
                .child(
                    gpui_component::resizable::resizable_panel()
                        .size_range(px(120.0)..gpui::Pixels::MAX)
                        .child(results),
                )
                .into_any_element()
        } else {
            editor_area.into_any_element()
        };
        div()
            .debug_selector(|| "agg-tab".into())
            .flex()
            .flex_col()
            .flex_1()
            .min_h_0()
            .min_w_0()
            .bg(theme::background())
            .child(content)
            .into_any_element()
    }

    // ---- header -------------------------------------------------------------------

    fn render_aggregate_header(
        &mut self,
        tab_id: u64,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let Some((title, dirty, preview_on, mode_now, allow_disk_use)) =
            self.aggregate_draft(tab_id).map(|draft| {
                (
                    draft.title().to_string(),
                    draft.dirty,
                    draft.preview,
                    draft.mode,
                    draft.options.allow_disk_use,
                )
            })
        else {
            return div().into_any_element();
        };
        let running = self
            .workspace
            .runtime(tab_id)
            .is_some_and(|runtime| !runtime.query.can_run());
        let notice = self
            .aggregate_ui
            .tab(tab_id)
            .and_then(|ui| ui.notice.clone());
        let root = cx.entity().downgrade();

        let name = div()
            .debug_selector(|| "agg-name".into())
            .flex()
            .items_center()
            .gap(px(6.0))
            .child(
                div()
                    .text_size(px(13.0))
                    .text_color(theme::text())
                    .child(title),
            )
            .when(dirty, |this| {
                this.child(
                    div()
                        .debug_selector(|| "agg-dirty".into())
                        .text_size(px(10.0))
                        .text_color(theme::text_muted())
                        .child("unsaved changes"),
                )
            });

        let save_root = root.clone();
        let save = gpui_component::button::DropdownButton::new(("agg-save-group", tab_id))
            .small()
            .button(
                Button::new(("agg-save-action", tab_id))
                    .small()
                    .debug_selector(|| "agg-save".into())
                    .label("Save")
                    .compact()
                    .ghost()
                    .on_click(cx.listener(move |this, _, window, cx| {
                        this.save_aggregate(tab_id, window, cx);
                    })),
            )
            .ghost()
            .dropdown_menu(move |menu, _, _| {
                let save = save_root.clone();
                let save_as = save_root.clone();
                menu.item(menu_entry("agg-menu-save", "Save", move |window, app| {
                    let _ = save.update(app, |this, cx| this.save_aggregate(tab_id, window, cx));
                }))
                .item(menu_entry(
                    "agg-menu-save-as",
                    "Save As…",
                    move |window, app| {
                        let _ = save_as.update(app, |this, cx| {
                            this.open_aggregate_save_dialog(tab_id, true, window, cx)
                        });
                    },
                ))
            });

        let create_new = Button::new(("agg-create-new", tab_id))
            .small()
            .debug_selector(|| "agg-create-new".into())
            .label("Create New")
            .compact()
            .ghost()
            .on_click(cx.listener(move |this, _, _, cx| {
                let Some((connection, database, collection)) =
                    this.workspace.tab(tab_id).map(|tab| {
                        (
                            tab.connection_id.clone(),
                            tab.database.clone(),
                            tab.collection.clone(),
                        )
                    })
                else {
                    return;
                };
                this.open_aggregate_tab(&connection, &database, &collection, AggregateDraft::new(), cx);
            }));

        let export_root = root.clone();
        let export_data = Button::new(("agg-export-data", tab_id))
            .small()
            .debug_selector(|| "agg-export-data".into())
            .label("Export Data")
            .dropdown_caret(true)
            .compact()
            .ghost()
            .dropdown_menu(move |menu, _, _| {
                [("agg-export-csv", "CSV", "csv"), ("agg-export-json", "JSON", "json"), ("agg-export-xlsx", "XLSX", "xlsx")]
                    .into_iter()
                    .fold(menu, |menu, (selector, label, format)| {
                        let root = export_root.clone();
                        menu.item(menu_entry(selector, label, move |window, app| {
                            let _ = root.update(app, |this, cx| {
                                this.export_aggregate_data(tab_id, format, window, cx)
                            });
                        }))
                    })
            });

        let export_code = Button::new(("agg-export-code", tab_id))
            .small()
            .debug_selector(|| "agg-export-code".into())
            .label("Export Code")
            .compact()
            .ghost()
            .on_click(cx.listener(move |this, _, _, cx| this.export_aggregate_code(tab_id, cx)));

        let preview_toggle = div()
            .debug_selector(|| "agg-preview-toggle".into())
            .flex()
            .items_center()
            .gap(px(6.0))
            .child(muted("Preview"))
            .child(
                Switch::new(("agg-preview-switch", tab_id))
                    .small()
                    .checked(preview_on)
                    .accessibility_label("Preview")
                    .on_click(cx.listener(move |this, checked: &bool, _, cx| {
                        this.set_aggregate_preview_enabled(tab_id, *checked, cx);
                    })),
            );

        const MODES: [(EditorMode, &str); 2] = [(EditorMode::Stages, "Stages"), (EditorMode::Text, "Text")];
        let mode_root = root.clone();
        let mode = div().debug_selector(|| "agg-mode".into()).child(
            ButtonGroup::new(("agg-mode-group", tab_id))
                .outline()
                .compact()
                .with_size(Size::XSmall)
                .children(MODES.iter().enumerate().map(|(index, (option, label))| {
                    Button::new(("agg-mode-option", index))
                        .debug_selector({
                            let label = label.to_lowercase();
                            move || format!("agg-mode-{label}")
                        })
                        .label(*label)
                        .selected(mode_now == *option)
                }))
                .on_click(move |selected, window, app| {
                    if let Some((option, _)) = selected.first().and_then(|index| MODES.get(*index)) {
                        let option = *option;
                        let _ = mode_root.update(app, |this, cx| {
                            this.set_aggregate_mode(tab_id, option, window, cx);
                        });
                    }
                }),
        );

        let explain_root = root.clone();
        let explain = gpui_component::button::DropdownButton::new(("agg-explain-group", tab_id))
            .small()
            .button(
                Button::new(("agg-explain-action", tab_id))
                    .small()
                    .debug_selector(|| "agg-explain".into())
                    .label("Explain")
                    .compact()
                    .ghost()
                    .on_click(cx.listener(move |this, _, window, cx| {
                        this.explain_aggregate(
                            tab_id,
                            engine::ExplainVerbosity::QueryPlanner,
                            window,
                            cx,
                        );
                    })),
            )
            .ghost()
            .dropdown_menu(move |menu, _, _| {
                [
                    ("agg-explain-planner", "Query planner", engine::ExplainVerbosity::QueryPlanner),
                    ("agg-explain-stats", "Execution stats", engine::ExplainVerbosity::ExecutionStats),
                    ("agg-explain-all", "All plans execution", engine::ExplainVerbosity::AllPlansExecution),
                ]
                .into_iter()
                .fold(menu, |menu, (selector, label, verbosity)| {
                    let root = explain_root.clone();
                    menu.item(menu_entry(selector, label, move |window, app| {
                        let _ = root.update(app, |this, cx| {
                            this.explain_aggregate(tab_id, verbosity, window, cx)
                        });
                    }))
                })
            });

        let options = self.render_aggregate_options(tab_id, allow_disk_use, window, cx);

        let run = Button::new(("agg-run", tab_id))
            .small()
            .debug_selector(|| "agg-run".into())
            .icon(Icon::empty().path("icons/play.svg").size(px(12.0)))
            .label(if running { "Cancel" } else { "Run" })
            .compact()
            .primary()
            .on_click(cx.listener(move |this, _, window, cx| {
                if this
                    .workspace
                    .runtime(tab_id)
                    .is_some_and(|runtime| !runtime.query.can_run())
                {
                    this.cancel_tab_query(tab_id, cx);
                    cx.notify();
                } else {
                    this.run_aggregate(tab_id, window, cx);
                }
            }));

        div()
            .debug_selector(|| "agg-header".into())
            .flex()
            .flex_col()
            .flex_shrink_0()
            .bg(theme::chrome())
            .border_b_1()
            .border_color(theme::border())
            .child(
                div()
                    .h(px(36.0))
                    .px(px(10.0))
                    .flex()
                    .items_center()
                    .gap(px(6.0))
                    .child(name)
                    .child(div().debug_selector(|| "agg-save-dropdown".into()).child(save))
                    .child(create_new)
                    .child(export_data)
                    .child(export_code)
                    .child(div().flex_1())
                    .child(preview_toggle)
                    .child(mode)
                    .child(div().debug_selector(|| "agg-explain-dropdown".into()).child(explain))
                    .child(options)
                    .child(run),
            )
            .when_some(notice, |this, notice| {
                this.child(
                    div()
                        .debug_selector(|| "agg-notice".into())
                        .px(px(12.0))
                        .pb(px(6.0))
                        .child(muted(notice)),
                )
            })
            .into_any_element()
    }

    // ---- options ------------------------------------------------------------------

    fn render_aggregate_options(
        &mut self,
        tab_id: u64,
        allow_disk_use: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let open = self
            .aggregate_ui
            .tab(tab_id)
            .is_some_and(|ui| ui.options_open);
        let trigger = Button::new(("agg-options", tab_id))
            .small()
            .debug_selector(|| "agg-options".into())
            .label("Options")
            .compact()
            .ghost()
            .selected(open);
        let root = cx.entity().downgrade();
        let popover = Popover::new(("agg-options-popover", tab_id))
            .trigger(trigger)
            .open(open)
            .p(px(0.0))
            .bg(theme::surface())
            .on_open_change(move |open, _, app| {
                let open = *open;
                let _ = root.update(app, |this, cx| {
                    this.aggregate_ui.tab_mut(tab_id).options_open = open;
                    cx.notify();
                });
            });
        if !open {
            return popover.into_any_element();
        }
        let inputs = self.aggregate_option_inputs(tab_id, window, cx);
        let error = self
            .aggregate_ui
            .tab(tab_id)
            .and_then(|ui| ui.options_error.clone());
        let labelled = |label: &'static str, selector: &'static str, input: &gpui::Entity<InputState>| {
            div()
                .flex()
                .items_center()
                .gap(px(8.0))
                .child(div().w(px(110.0)).child(muted(label)))
                .child(
                    div()
                        .flex_1()
                        .debug_selector(move || selector.to_string())
                        .child(Input::new(input).appearance(true).with_size(Size::Small)),
                )
        };
        popover
            .child(
                div()
                    .debug_selector(|| "agg-options-panel".into())
                    .w(px(420.0))
                    .flex()
                    .flex_col()
                    .gap(px(10.0))
                    .p(px(12.0))
                    .child(div().text_size(px(13.0)).child("Aggregation options"))
                    .child(labelled("Sample size", "agg-opt-sample", &inputs.0))
                    .child(labelled("Max time (ms)", "agg-opt-maxtime", &inputs.1))
                    .child(labelled("Collation", "agg-opt-collation", &inputs.2))
                    .child(
                        div()
                            .debug_selector(|| "agg-opt-disk".into())
                            .child(
                                Checkbox::new(("agg-opt-disk-box", tab_id))
                                    .label("Allow disk use")
                                    .checked(allow_disk_use)
                                    .on_click(cx.listener(move |this, checked: &bool, _, cx| {
                                        this.set_aggregate_disk_use(tab_id, *checked, cx);
                                    })),
                            ),
                    )
                    .child(muted(
                        "Sample size applies to previews. Max time 0 uses the default (30 s previews, 60 s runs).",
                    ))
                    .when_some(error, |this, error| {
                        this.child(
                            div()
                                .debug_selector(|| "agg-opt-error".into())
                                .text_size(px(11.0))
                                .text_color(theme::danger())
                                .child(error),
                        )
                    }),
            )
            .into_any_element()
    }

    fn aggregate_option_inputs(
        &mut self,
        tab_id: u64,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> (
        gpui::Entity<InputState>,
        gpui::Entity<InputState>,
        gpui::Entity<InputState>,
    ) {
        if let Some(inputs) = self
            .aggregate_ui
            .tab(tab_id)
            .and_then(|ui| ui.option_inputs.as_ref())
        {
            return (
                inputs.sample.clone(),
                inputs.max_time.clone(),
                inputs.collation.clone(),
            );
        }
        // Created once (the popover opened for the first time): read the
        // options by reference, no copy of the draft.
        let (sample_text, max_time_text, collation_text) = self
            .aggregate_draft(tab_id)
            .map(|draft| {
                (
                    draft.options.sample_size.to_string(),
                    draft.options.max_time_ms.to_string(),
                    draft.options.collation.clone(),
                )
            })
            .unwrap_or_default();
        let sample = cx.new(|cx| InputState::new(window, cx).default_value(sample_text));
        let max_time = cx.new(|cx| InputState::new(window, cx).default_value(max_time_text));
        let collation = cx.new(|cx| {
            InputState::new(window, cx)
                .placeholder("{ locale: \"en\", strength: 2 }")
                .default_value(collation_text)
        });
        let mut subscriptions = Vec::new();
        for (input, field) in [(&sample, 0u8), (&max_time, 1), (&collation, 2)] {
            subscriptions.push(cx.subscribe_in(
                input,
                window,
                move |this: &mut Self,
                      input: &gpui::Entity<InputState>,
                      event: &InputEvent,
                      _: &mut Window,
                      cx: &mut Context<Self>| {
                    if matches!(event, InputEvent::Change) {
                        let text = input.read(cx).value().to_string();
                        this.on_aggregate_option_edit(tab_id, field, text, cx);
                    }
                },
            ));
        }
        self.aggregate_ui.tab_mut(tab_id).option_inputs = Some(OptionInputs {
            sample: sample.clone(),
            max_time: max_time.clone(),
            collation: collation.clone(),
            _subscriptions: subscriptions,
        });
        (sample, max_time, collation)
    }

    fn on_aggregate_option_edit(&mut self, tab_id: u64, field: u8, text: String, cx: &mut Context<Self>) {
        let Some(draft) = self.aggregate_draft_mut(tab_id) else {
            return;
        };
        let mut error = None;
        match field {
            0 => match text.trim().parse::<u64>() {
                Ok(value) => draft.options.sample_size = value,
                Err(_) => error = Some("Sample size must be a whole number.".to_string()),
            },
            1 => match text.trim() {
                "" => draft.options.max_time_ms = 0,
                other => match other.parse::<u64>() {
                    Ok(value) => draft.options.max_time_ms = value,
                    Err(_) => error = Some("Max time must be a whole number of milliseconds.".to_string()),
                },
            },
            _ => draft.options.collation = text,
        }
        draft.touch();
        if error.is_none() {
            error = draft.options.validate().err();
        }
        self.aggregate_ui.tab_mut(tab_id).options_error = error;
        self.aggregate_edited(tab_id, cx);
    }

    fn set_aggregate_disk_use(&mut self, tab_id: u64, enabled: bool, cx: &mut Context<Self>) {
        let Some(draft) = self.aggregate_draft_mut(tab_id) else {
            return;
        };
        draft.options.allow_disk_use = enabled;
        draft.touch();
        self.aggregate_edited(tab_id, cx);
    }

    // ---- text mode ------------------------------------------------------------------

    fn render_aggregate_text(
        &mut self,
        tab_id: u64,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let Some(preview_on) = self.aggregate_draft(tab_id).map(|draft| draft.preview) else {
            return div().into_any_element();
        };
        let editor = self.aggregate_text_editor(tab_id, window, cx);
        // Cached per draft revision: rendering never parses the pipeline.
        let (error, disabled) = self
            .aggregate_derived(tab_id)
            .map(|derived| {
                (
                    derived.check.text_error.as_ref().map(ToString::to_string),
                    derived.disabled_count,
                )
            })
            .unwrap_or_default();
        let left = div()
            .debug_selector(|| "agg-text-pane".into())
            .flex()
            .flex_col()
            .size_full()
            .min_h_0()
            .child(
                div()
                    .h(px(28.0))
                    .px(px(10.0))
                    .flex()
                    .items_center()
                    .gap(px(8.0))
                    .border_b_1()
                    .border_color(theme::border())
                    .child(muted("Pipeline").flex_1())
                    .child(
                        Button::new(("agg-format", tab_id))
                            .with_size(Size::XSmall)
                            .debug_selector(|| "agg-format".into())
                            .icon(Icon::empty().path("icons/wand-sparkles.svg").size(px(12.0)))
                            .label("Format")
                            .compact()
                            .ghost()
                            .on_click(cx.listener(move |this, _, window, cx| {
                                this.format_aggregate_text(tab_id, window, cx);
                            })),
                    ),
            )
            .child(
                div()
                    .debug_selector(|| "agg-text-editor".into())
                    .flex_1()
                    .min_h_0()
                    .font_family("Menlo")
                    .text_size(px(12.0))
                    .child(Editor::new(&editor).appearance(false).bordered(false).h_full()),
            )
            .when(disabled > 0, |this| {
                this.child(
                    div()
                        .debug_selector(|| "agg-disabled-notice".into())
                        .px(px(10.0))
                        .py(px(4.0))
                        .border_t_1()
                        .border_color(theme::border())
                        .child(muted(format!(
                            "{disabled} disabled stage{} hidden. Switch to Stages to re-enable.",
                            if disabled == 1 { "" } else { "s" }
                        ))),
                )
            })
            .when_some(error, |this, error| {
                this.child(
                    div()
                        .debug_selector(|| "agg-text-error".into())
                        .px(px(10.0))
                        .py(px(6.0))
                        .border_t_1()
                        .border_color(theme::border())
                        .bg(theme::surface())
                        .text_size(px(11.0))
                        .text_color(theme::danger())
                        .child(error),
                )
            });
        let right = self.render_aggregate_main_preview(tab_id, preview_on, window, cx);
        gpui_component::resizable::h_resizable(("agg-text-split", tab_id))
            .child(
                gpui_component::resizable::resizable_panel()
                    .size(px(520.0))
                    .size_range(px(280.0)..gpui::Pixels::MAX)
                    .child(left),
            )
            .child(
                gpui_component::resizable::resizable_panel()
                    .size_range(px(280.0)..gpui::Pixels::MAX)
                    .child(right),
            )
            .into_any_element()
    }

    fn render_aggregate_main_preview(
        &mut self,
        tab_id: u64,
        preview_on: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let (status, count, rows, revision, view, docs, elapsed) = {
            let ui = self.aggregate_ui.tab(tab_id);
            let slot = ui.and_then(|ui| ui.slot(SlotKey::Main));
            (
                slot.map(|slot| slot.status.clone()).unwrap_or(PreviewStatus::Idle),
                slot.map_or(0, |slot| slot.docs.len()),
                slot.map(|slot| Arc::clone(&slot.rows)),
                slot.map_or(0, |slot| slot.revision),
                ui.map(|ui| ui.preview_view).unwrap_or_default(),
                slot.map(|slot| Arc::clone(&slot.docs)),
                slot.and_then(|slot| slot.elapsed_ms),
            )
        };
        let subtitle = if !preview_on {
            "Preview is off".to_string()
        } else {
            match &status {
                PreviewStatus::Ready => format!(
                    "Sample of {count} document{}{}",
                    if count == 1 { "" } else { "s" },
                    elapsed.map(|ms| format!(" · {ms} ms")).unwrap_or_default()
                ),
                PreviewStatus::Loading => "Loading…".to_string(),
                _ => String::new(),
            }
        };
        const VIEWS: [(PreviewView, &str); 2] = [(PreviewView::Tree, "Tree"), (PreviewView::Json, "JSON")];
        let root = cx.entity().downgrade();
        let toggle = div().debug_selector(|| "agg-preview-view".into()).child(
            ButtonGroup::new(("agg-preview-view-group", tab_id))
                .outline()
                .compact()
                .with_size(Size::XSmall)
                .children(VIEWS.iter().enumerate().map(|(index, (option, label))| {
                    Button::new(("agg-preview-view-option", index))
                        .debug_selector({
                            let label = label.to_lowercase();
                            move || format!("agg-preview-view-{label}")
                        })
                        .label(*label)
                        .selected(view == *option)
                }))
                .on_click(move |selected, _, app| {
                    if let Some((option, _)) = selected.first().and_then(|index| VIEWS.get(*index)) {
                        let option = *option;
                        let _ = root.update(app, |this, cx| {
                            this.aggregate_ui.tab_mut(tab_id).preview_view = option;
                            cx.notify();
                        });
                    }
                }),
        );
        let body: AnyElement = if !preview_on {
            muted("Turn Preview on to see the output of this pipeline.")
                .p(px(14.0))
                .debug_selector(|| "agg-preview-off".into())
                .into_any_element()
        } else {
            match status {
                PreviewStatus::Idle | PreviewStatus::Loading if rows.is_none() => {
                    muted("Loading preview…")
                        .p(px(14.0))
                        .debug_selector(|| "agg-preview-loading".into())
                        .into_any_element()
                }
                PreviewStatus::Invalid if rows.is_none() => muted("Fix the pipeline to see a preview.")
                    .p(px(14.0))
                    .debug_selector(|| "agg-preview-invalid".into())
                    .into_any_element(),
                PreviewStatus::Failed(message) => div()
                    .debug_selector(|| "agg-preview-error".into())
                    .p(px(14.0))
                    .text_size(px(12.0))
                    .text_color(theme::danger())
                    .child(crate::sessions::redact_secrets(&message))
                    .into_any_element(),
                _ => {
                    let docs = docs.unwrap_or_default();
                    if docs.is_empty() && status == PreviewStatus::Ready {
                        muted("No documents match this pipeline.")
                            .p(px(14.0))
                            .debug_selector(|| "agg-preview-empty".into())
                            .into_any_element()
                    } else if view == PreviewView::Json {
                        self.render_aggregate_json_preview(tab_id, revision, &docs, window, cx)
                    } else {
                        let rows = rows.unwrap_or_default();
                        self.render_aggregate_tree(tab_id, SlotKey::Main, rows, "agg-preview-tree", cx)
                    }
                }
            }
        };
        div()
            .debug_selector(|| "agg-preview-pane".into())
            .flex()
            .flex_col()
            .size_full()
            .min_h_0()
            .border_l_1()
            .border_color(theme::border())
            .child(
                div()
                    .px(px(12.0))
                    .py(px(6.0))
                    .flex()
                    .items_center()
                    .gap(px(8.0))
                    .border_b_1()
                    .border_color(theme::border())
                    .child(
                        div()
                            .flex_1()
                            .flex()
                            .flex_col()
                            .child(
                                div()
                                    .debug_selector(|| "agg-preview-title".into())
                                    .text_size(px(12.0))
                                    .text_color(theme::text())
                                    .child("Pipeline Output Preview"),
                            )
                            .child(
                                div()
                                    .debug_selector(|| "agg-preview-subtitle".into())
                                    .child(muted(subtitle)),
                            ),
                    )
                    .child(toggle),
            )
            .child(div().flex_1().min_h_0().child(body))
            .into_any_element()
    }

    /// JSON view of the whole preview sample in one read-only editor.
    fn render_aggregate_json_preview(
        &mut self,
        tab_id: u64,
        revision: u64,
        docs: &Arc<PreviewDocs>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let key = results::json_view::hash_key((tab_id, revision, Arc::as_ptr(docs) as usize));
        let docs = Arc::clone(docs);
        let ui = self.aggregate_ui.tab_mut(tab_id);
        results::json_view::JsonViewer::sync(
            &mut ui.json_viewer,
            key,
            results::json_view::JsonViewerStyle {
                soft_wrap: false,
                line_numbers: true,
                folding: true,
            },
            || {
                serde_json::to_string_pretty(&crate::cell_value::relaxed_ejson(&docs.as_array()))
                    .unwrap_or_default()
            },
            window,
            cx,
        );
        let Some(viewer) = self
            .aggregate_ui
            .tab(tab_id)
            .and_then(|ui| ui.json_viewer.as_ref())
        else {
            return div().into_any_element();
        };
        div()
            .debug_selector(|| "agg-preview-json".into())
            .size_full()
            .font_family("Menlo")
            .text_size(px(12.0))
            .child(
                Editor::new(&viewer.state)
                    .readonly(true)
                    .appearance(false)
                    .bordered(false)
                    .h_full(),
            )
            .into_any_element()
    }

    /// Virtualized tree of a preview slot.
    fn render_aggregate_tree(
        &self,
        tab_id: u64,
        key: SlotKey,
        rows: Arc<Vec<MongoTreeRow>>,
        selector: &'static str,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let id = match key {
            SlotKey::Main => format!("agg-tree-main-{tab_id}"),
            SlotKey::Stage(stage) => format!("agg-tree-stage-{tab_id}-{stage}"),
        };
        let expanded: HashSet<String> = self
            .aggregate_ui
            .tab(tab_id)
            .and_then(|ui| ui.slot(key))
            .map(|slot| slot.expanded.clone())
            .unwrap_or_default();
        let count = rows.len();
        let list = uniform_list(
            gpui::ElementId::Name(gpui::SharedString::from(id)),
            count,
            cx.processor(move |_this, range: Range<usize>, _window, cx| {
                range
                    .map(|index| {
                        let row = rows[index].clone();
                        let path = row.path.clone();
                        let is_expanded = expanded.contains(&path);
                        let mut element = div()
                            .id(("agg-tree-row", index))
                            .h(px(24.0))
                            .debug_selector(|| "agg-tree-row".into())
                            .flex()
                            .items_center()
                            .pl(px((row.depth as f32) * 18.0 + 10.0))
                            .gap(px(6.0))
                            .text_size(px(12.0));
                        if row.container {
                            element = element
                                .child(
                                    div()
                                        .w(px(12.0))
                                        .text_color(theme::text_muted())
                                        .child(if is_expanded { "v" } else { ">" }),
                                )
                                .on_click(cx.listener(move |this, _, _, cx| {
                                    if let Some(slot) = this
                                        .aggregate_ui
                                        .tabs
                                        .get_mut(&tab_id)
                                        .and_then(|ui| match key {
                                            SlotKey::Main => ui.main.as_mut(),
                                            SlotKey::Stage(id) => ui.stage_previews.get_mut(&id),
                                        })
                                    {
                                        slot.toggle(&path);
                                    }
                                    cx.notify();
                                }));
                        } else {
                            element = element.child(div().w(px(12.0)));
                        }
                        element
                            .when(!row.key.is_empty(), |element| {
                                element.child(
                                    div()
                                        .text_color(theme::accent())
                                        .child(format!("{}:", row.key)),
                                )
                            })
                            .child(
                                div()
                                    .text_color(mongo_tree_value_color(&row.type_name, row.container))
                                    .child(row.value),
                            )
                            .into_any_element()
                    })
                    .collect()
            }),
        )
        .with_horizontal_sizing_behavior(ListHorizontalSizingBehavior::Unconstrained)
        .size_full();
        div()
            .debug_selector(move || selector.to_string())
            .size_full()
            .child(list)
            .into_any_element()
    }

    // ---- stages mode ------------------------------------------------------------------

    fn render_aggregate_stages(
        &mut self,
        tab_id: u64,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let Some(total) = self.aggregate_draft(tab_id).map(|draft| draft.stages.len()) else {
            return div().into_any_element();
        };
        let mut list = div()
            .id(("agg-stages", tab_id))
            .debug_selector(|| "agg-stages".into())
            .flex()
            .flex_col()
            .gap(px(10.0))
            .p(px(12.0))
            .overflow_y_scroll()
            .size_full();
        if total == 0 {
            list = list.child(
                div()
                    .debug_selector(|| "agg-stages-empty".into())
                    .child(muted("No stages yet. Add a stage to start building the pipeline.")),
            );
        }
        let mut number = 0usize;
        for index in 0..total {
            // Only what a card shows is copied (an id, an operator name, a flag).
            let Some(stage) = self.aggregate_draft(tab_id).and_then(|draft| {
                draft.stages.get(index).map(|stage| StageView {
                    id: stage.id,
                    operator: stage.operator.clone(),
                    enabled: stage.enabled,
                })
            }) else {
                break;
            };
            if stage.enabled {
                number += 1;
            }
            let card = self.render_aggregate_stage_card(
                tab_id,
                &stage,
                total,
                stage.enabled.then_some(number),
                window,
                cx,
            );
            list = list.child(card);
        }
        list = list.child(
            div().child(
                Button::new(("agg-add-stage", tab_id))
                    .small()
                    .debug_selector(|| "agg-add-stage".into())
                    .icon(Icon::new(IconName::Plus).size(px(14.0)))
                    .label("Add Stage")
                    .compact()
                    .ghost()
                    .on_click(cx.listener(move |this, _, window, cx| {
                        this.aggregate_add_stage(tab_id, None, window, cx);
                    })),
            ),
        );
        list.into_any_element()
    }

    #[allow(clippy::too_many_arguments)]
    fn render_aggregate_stage_card(
        &mut self,
        tab_id: u64,
        stage: &StageView,
        total: usize,
        number: Option<usize>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let stage_id = stage.id;
        let Some((body, operator, custom)) =
            self.aggregate_stage_editors(tab_id, stage_id, window, cx)
        else {
            return div().into_any_element();
        };
        let error = self
            .aggregate_derived(tab_id)
            .and_then(|derived| derived.check.stage_error(stage_id))
            .map(str::to_string);
        let enabled = stage.enabled;
        let (collapsed, slot_status, slot_count, slot_rows, elapsed) = {
            let ui = self.aggregate_ui.tab(tab_id);
            let slot = ui.and_then(|ui| ui.slot(SlotKey::Stage(stage_id)));
            (
                ui.is_some_and(|ui| ui.collapsed.contains(&stage_id)),
                slot.map(|slot| slot.status.clone()).unwrap_or(PreviewStatus::Idle),
                slot.map_or(0, |slot| slot.docs.len()),
                slot.map(|slot| Arc::clone(&slot.rows)),
                slot.and_then(|slot| slot.elapsed_ms),
            )
        };
        let handle = div()
            .id(("agg-stage-handle", stage_id))
            .debug_selector(move || format!("agg-stage-handle-{stage_id}"))
            .w(px(18.0))
            .h(px(28.0))
            .flex()
            .flex_shrink_0()
            .items_center()
            .justify_center()
            .text_color(theme::text_faint())
            .when(total > 1, |this| {
                let drag = StageDrag {
                    tab_id,
                    stage_id,
                    label: format!("{} stage", stage.operator).into(),
                };
                this.cursor_grab()
                    .hover(|style| style.text_color(theme::text()))
                    .on_drag(drag, |drag, _, _, cx| cx.new(|_| drag.clone()))
            })
            .child(Icon::empty().path("icons/grip-vertical.svg").size(px(14.0)));
        let toggle = Button::new(("agg-stage-toggle", stage_id))
            .debug_selector(move || format!("agg-stage-toggle-{stage_id}"))
            .icon(
                Icon::empty()
                    .path(if enabled { "icons/eye.svg" } else { "icons/eye-off.svg" })
                    .size(px(14.0)),
            )
            .compact()
            .ghost()
            .tooltip(if enabled { "Disable stage" } else { "Enable stage" })
            .on_click(cx.listener(move |this, _, window, cx| {
                this.aggregate_toggle_stage(tab_id, stage_id, window, cx);
            }));
        let is_custom = operator_choice(&stage.operator) == CUSTOM_OPERATOR;
        let preview_toggle = Button::new(("agg-stage-preview-toggle", stage_id))
            .debug_selector(move || format!("agg-stage-preview-toggle-{stage_id}"))
            .icon(
                Icon::new(if collapsed {
                    IconName::ChevronRight
                } else {
                    IconName::ChevronDown
                })
                .size(px(14.0)),
            )
            .compact()
            .ghost()
            .tooltip(if collapsed { "Show output" } else { "Hide output" })
            .disabled(!enabled)
            .on_click(cx.listener(move |this, _, _, cx| {
                this.aggregate_toggle_stage_preview(tab_id, stage_id, cx);
            }));
        let add_after = Button::new(("agg-stage-add", stage_id))
            .debug_selector(move || format!("agg-stage-add-{stage_id}"))
            .icon(Icon::new(IconName::Plus).size(px(14.0)))
            .compact()
            .ghost()
            .tooltip("Add stage after this one")
            .on_click(cx.listener(move |this, _, window, cx| {
                this.aggregate_add_stage(tab_id, Some(stage_id), window, cx);
            }));
        let delete = Button::new(("agg-stage-delete", stage_id))
            .debug_selector(move || format!("agg-stage-delete-{stage_id}"))
            .icon(Icon::new(IconName::Delete).size(px(14.0)))
            .compact()
            .ghost()
            .tooltip("Delete stage")
            .on_click(cx.listener(move |this, _, window, cx| {
                this.aggregate_delete_stage(tab_id, stage_id, window, cx);
            }));
        let header = div()
            .flex()
            .items_center()
            .gap(px(6.0))
            .child(handle)
            .child(
                div()
                    .w(px(54.0))
                    .debug_selector(move || format!("agg-stage-number-{stage_id}"))
                    .child(muted(match number {
                        Some(number) => format!("Stage {number}"),
                        None => "Disabled".to_string(),
                    })),
            )
            .child(toggle)
            .child(
                div()
                    .debug_selector(move || format!("agg-stage-operator-{stage_id}"))
                    .w(px(170.0))
                    .flex_shrink_0()
                    .child(
                        Select::new(&operator)
                            .id(("agg-stage-operator-select", stage_id))
                            .with_size(Size::Small)
                            .search_placeholder("Search stages")
                            .menu_width(px(220.0))
                            .placeholder("Operator"),
                    ),
            )
            .when(is_custom, |this| {
                this.child(
                    div()
                        .debug_selector(move || format!("agg-stage-custom-{stage_id}"))
                        .w(px(150.0))
                        .child(Input::new(&custom).appearance(true).with_size(Size::Small)),
                )
            })
            .child(div().flex_1())
            .child(
                div()
                    .debug_selector(move || format!("agg-stage-count-{stage_id}"))
                    .child(muted(if enabled && !collapsed {
                        let label = doc_count_label_for(&slot_status, slot_count);
                        match (label.is_empty(), elapsed) {
                            (false, Some(ms)) if slot_status == PreviewStatus::Ready => {
                                format!("{label} · {ms} ms")
                            }
                            _ => label,
                        }
                    } else {
                        String::new()
                    })),
            )
            .child(preview_toggle)
            .child(add_after)
            .child(delete);

        let preview = (enabled && !collapsed).then(|| {
            let body: AnyElement = match &slot_status {
                PreviewStatus::Failed(message) => div()
                    .debug_selector(move || format!("agg-stage-preview-error-{stage_id}"))
                    .p(px(8.0))
                    .text_size(px(11.0))
                    .text_color(theme::danger())
                    .child(crate::sessions::redact_secrets(message))
                    .into_any_element(),
                PreviewStatus::Invalid => muted("Fix the stage to see its output.")
                    .p(px(8.0))
                    .into_any_element(),
                PreviewStatus::Ready if slot_count == 0 => muted("No documents.")
                    .p(px(8.0))
                    .debug_selector(move || format!("agg-stage-preview-empty-{stage_id}"))
                    .into_any_element(),
                PreviewStatus::Ready => {
                    let rows = slot_rows.clone().unwrap_or_default();
                    div()
                        .debug_selector(move || format!("agg-stage-preview-{stage_id}"))
                        .size_full()
                        .child(self.render_aggregate_tree(
                            tab_id,
                            SlotKey::Stage(stage_id),
                            rows,
                            "agg-stage-tree",
                            cx,
                        ))
                        .into_any_element()
                }
                _ => muted("Loading output…")
                    .p(px(8.0))
                    .debug_selector(move || format!("agg-stage-preview-loading-{stage_id}"))
                    .into_any_element(),
            };
            div()
                .h(px(168.0))
                .border_t_1()
                .border_color(theme::border())
                .child(body)
        });

        let target_drag_over = |style: gpui::StyleRefinement, _: &StageDrag, _: &mut Window, _: &mut gpui::App| {
            style.bg(theme::accent_surface())
        };
        div()
            .id(("agg-stage", stage_id))
            .debug_selector(move || format!("agg-stage-{stage_id}"))
            .flex()
            .flex_col()
            .gap(px(6.0))
            .p(px(8.0))
            .rounded(px(6.0))
            .border_1()
            .border_color(theme::border())
            .bg(theme::surface())
            .when(!enabled, |this| this.opacity(0.6))
            .drag_over::<StageDrag>(target_drag_over)
            .on_drop(cx.listener(move |this, drag: &StageDrag, window, cx| {
                if drag.tab_id == tab_id && drag.stage_id != stage_id {
                    this.aggregate_move_stage(tab_id, drag.stage_id, stage_id, window, cx);
                }
            }))
            .child(header)
            .child(
                div()
                    .debug_selector(move || format!("agg-stage-body-{stage_id}"))
                    .h(px(92.0))
                    .font_family("Menlo")
                    .text_size(px(12.0))
                    .child(Editor::new(&body).appearance(true).h_full()),
            )
            .when_some(error, |this, error| {
                this.child(
                    div()
                        .debug_selector(move || format!("agg-stage-error-{stage_id}"))
                        .text_size(px(11.0))
                        .text_color(theme::danger())
                        .child(error),
                )
            })
            .children(preview)
            .into_any_element()
    }

    // ---- run output ---------------------------------------------------------------------

    fn render_aggregate_results(
        &mut self,
        tab_id: u64,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        // The pipeline's edit-safety is cached per draft revision; only the
        // "result has _id" part depends on the grid and is a column scan.
        let grid = self.grid(tab_id);
        let has_id = (0..grid.column_count()).any(|column| grid.column_name(column) == Some("_id"));
        let editability = self.aggregate_derived(tab_id).map(|derived| {
            if has_id {
                derived.editability.clone()
            } else {
                crate::mongo_aggregate::Editability {
                    editable: false,
                    reason: Some("Output has no _id field.".into()),
                }
            }
        });
        let badge = editability.map(|state| {
            let text = if state.editable {
                "Documents are the collection's own: edit them in the collection's data tab (in-place editing is not available here)".to_string()
            } else {
                format!("Read-only output: {}", state.reason.unwrap_or_default())
            };
            div()
                .debug_selector(|| "agg-editability".into())
                .px(px(12.0))
                .py(px(4.0))
                .border_b_1()
                .border_color(theme::border())
                .child(muted(text))
        });
        div()
            .debug_selector(|| "agg-results".into())
            .flex()
            .flex_col()
            .size_full()
            .min_h_0()
            .children(badge)
            .child(self.render_results(tab_id, window, cx))
            .child(self.render_result_footer(tab_id, window, cx))
            .into_any_element()
    }
}

fn doc_count_label_for(status: &PreviewStatus, count: usize) -> String {
    match status {
        PreviewStatus::Loading => "Loading…".into(),
        PreviewStatus::Ready => format!("{count} document{}", if count == 1 { "" } else { "s" }),
        _ => String::new(),
    }
}
