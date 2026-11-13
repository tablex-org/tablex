use super::super::super::*;

impl AppRoot {
    #[allow(clippy::too_many_arguments)] // each parameter is one input of the operation; a parameter struct would only rename the list
    pub(in crate::app) fn render_mongo_column_popover(
        &mut self,
        tab_id: u64,
        columns: Vec<String>,
        visible_columns: HashSet<String>,
        projection_draft_columns: HashSet<String>,
        column_tab: crate::mongo_result::MongoColumnTab,
        column_search: String,
        pending: usize,
        open: bool,
        trigger: Button,
        search_editor: gpui::Entity<InputState>,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let selected = match column_tab {
            crate::mongo_result::MongoColumnTab::Visible => visible_columns,
            crate::mongo_result::MongoColumnTab::Projection => projection_draft_columns.clone(),
        };
        // Visible tab: columns outside the projection draft are not available.
        let unavailable = match column_tab {
            crate::mongo_result::MongoColumnTab::Visible => columns
                .iter()
                .filter(|name| !projection_draft_columns.contains(*name))
                .cloned()
                .collect(),
            crate::mongo_result::MongoColumnTab::Projection => HashSet::new(),
        };
        let tab_visible = Button::new(("mongo-column-tab-visible", tab_id))
            .with_size(gpui_component::Size::XSmall)
            .debug_selector(|| "mongo-column-tab-visible".into())
            .label("Visible")
            .flex_1()
            .h(px(24.0))
            .compact()
            .ghost()
            .selected(column_tab == crate::mongo_result::MongoColumnTab::Visible)
            .on_click(cx.listener(move |this, _, _, cx| {
                this.set_mongo_column_tab(tab_id, crate::mongo_result::MongoColumnTab::Visible, cx);
            }));
        let tab_projection = Button::new(("mongo-column-tab-projection", tab_id))
            .with_size(gpui_component::Size::XSmall)
            .debug_selector(|| "mongo-column-tab-projection".into())
            .label("Projection")
            .flex_1()
            .h(px(24.0))
            .compact()
            .ghost()
            .selected(column_tab == crate::mongo_result::MongoColumnTab::Projection)
            .on_click(cx.listener(move |this, _, _, cx| {
                this.set_mongo_column_tab(
                    tab_id,
                    crate::mongo_result::MongoColumnTab::Projection,
                    cx,
                );
            }));
        let tab_switch = div()
            .debug_selector(|| "mongo-column-tabs".into())
            .h(px(26.0))
            .w_full()
            .flex()
            .items_center()
            .gap(px(0.0))
            .p(px(2.0))
            .rounded(px(6.0))
            .bg(theme::background())
            .child(tab_visible)
            .child(tab_projection)
            .into_any_element();
        let footer = match column_tab {
            crate::mongo_result::MongoColumnTab::Visible => column_picker_footer(
                "mongo-column",
                "At least one column stays visible",
                Button::new(("mongo-column-show-all", tab_id))
                    .with_size(gpui_component::Size::XSmall)
                    .label("Show all")
                    .compact()
                    .ghost()
                    .on_click(cx.listener(move |this, _, _, cx| {
                        this.show_all_mongo_columns(tab_id, cx);
                    })),
            ),
            crate::mongo_result::MongoColumnTab::Projection => column_picker_footer(
                "mongo-column",
                "Projection applies on query",
                div()
                    .flex()
                    .gap(px(4.0))
                    .child(
                        Button::new(("mongo-column-select-all", tab_id))
                            .with_size(gpui_component::Size::XSmall)
                            .label("Select all")
                            .compact()
                            .ghost()
                            .on_click(cx.listener(move |this, _, _, cx| {
                                this.select_all_mongo_projection(tab_id, cx);
                            })),
                    )
                    .child(
                        Button::new(("mongo-column-apply", tab_id))
                            .with_size(gpui_component::Size::XSmall)
                            .label("Apply")
                            .compact()
                            .primary()
                            .disabled(pending > 0)
                            .on_click(cx.listener(move |this, _, _, cx| {
                                this.apply_mongo_projection(tab_id, cx);
                            })),
                    ),
            ),
        };
        self.render_column_picker(
            ColumnPicker {
                prefix: "mongo-column",
                tab_id,
                columns,
                selected,
                unavailable,
                search: column_search,
                open,
                locked: false,
                tabs: Some(tab_switch),
                footer,
                trigger,
                search_editor,
            },
            move |this, name, cx| this.toggle_mongo_column(tab_id, name, cx),
            move |this, open, cx| this.set_mongo_columns_open(tab_id, open, cx),
            cx,
        )
    }

    pub(in crate::app) fn render_mongo_filter_menu(
        &self,
        tab_id: u64,
        target: crate::mongo_result::MongoFilterMenu,
        columns: &[String],
        rule: &crate::mongo_result::MongoFilterRule,
        trigger: Button,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let rule_id = match target {
            crate::mongo_result::MongoFilterMenu::Field(rule_id)
            | crate::mongo_result::MongoFilterMenu::Mode(rule_id)
            | crate::mongo_result::MongoFilterMenu::ValueType(rule_id)
            | crate::mongo_result::MongoFilterMenu::Conjunction(rule_id) => rule_id,
        };
        let root = cx.entity().downgrade();
        match target {
            crate::mongo_result::MongoFilterMenu::Field(_) => {
                let selected = rule.field.clone();
                let columns = columns.to_vec();
                trigger
                    .dropdown_menu(move |mut menu, _, _| {
                        for column in &columns {
                            let option = column.clone();
                            let root = root.clone();
                            menu = menu.item(
                                PopupMenuItem::new(column.clone())
                                    .checked(*column == selected)
                                    .on_click(move |_, window, app| {
                                        let _ = root.update(app, |this, cx| {
                                            this.select_mongo_filter_field(
                                                tab_id,
                                                rule_id,
                                                option.clone(),
                                                window,
                                                cx,
                                            );
                                        });
                                    }),
                            );
                        }
                        menu.max_w(px(220.0)).max_h(px(240.0)).scrollable(true)
                    })
                    .into_any_element()
            }
            crate::mongo_result::MongoFilterMenu::Mode(_) => {
                let selected = rule.mode;
                trigger
                    .dropdown_menu(move |mut menu, _, _| {
                        for mode in [
                            crate::mongo_result::MongoFilterMode::Equals,
                            crate::mongo_result::MongoFilterMode::NotEquals,
                            crate::mongo_result::MongoFilterMode::Contains,
                            crate::mongo_result::MongoFilterMode::NotContains,
                            crate::mongo_result::MongoFilterMode::GreaterThan,
                            crate::mongo_result::MongoFilterMode::GreaterThanOrEqual,
                            crate::mongo_result::MongoFilterMode::LessThan,
                            crate::mongo_result::MongoFilterMode::LessThanOrEqual,
                            crate::mongo_result::MongoFilterMode::IsNull,
                            crate::mongo_result::MongoFilterMode::IsNotNull,
                        ] {
                            let root = root.clone();
                            menu = menu.item(
                                PopupMenuItem::new(mongo_filter_mode_label(mode))
                                    .checked(mode == selected)
                                    .on_click(move |_, _, app| {
                                        let _ = root.update(app, |this, cx| {
                                            this.update_mongo_filter_rule(
                                                tab_id,
                                                rule_id,
                                                |rule| rule.mode = mode,
                                                cx,
                                            );
                                        });
                                    }),
                            );
                        }
                        menu.max_w(px(220.0)).max_h(px(300.0)).scrollable(true)
                    })
                    .into_any_element()
            }
            crate::mongo_result::MongoFilterMenu::ValueType(_) => {
                let selected = rule.value_type;
                trigger
                    .dropdown_menu(move |mut menu, _, _| {
                        for value_type in [
                            crate::mongo_result::MongoFilterValueType::Auto,
                            crate::mongo_result::MongoFilterValueType::String,
                            crate::mongo_result::MongoFilterValueType::Number,
                            crate::mongo_result::MongoFilterValueType::Boolean,
                            crate::mongo_result::MongoFilterValueType::ObjectId,
                            crate::mongo_result::MongoFilterValueType::Date,
                            crate::mongo_result::MongoFilterValueType::Int32,
                            crate::mongo_result::MongoFilterValueType::Int64,
                            crate::mongo_result::MongoFilterValueType::Decimal128,
                            crate::mongo_result::MongoFilterValueType::Json,
                        ] {
                            let root = root.clone();
                            menu = menu.item(
                                PopupMenuItem::new(mongo_filter_type_label(value_type))
                                    .checked(value_type == selected)
                                    .on_click(move |_, _, app| {
                                        let _ = root.update(app, |this, cx| {
                                            this.update_mongo_filter_rule(
                                                tab_id,
                                                rule_id,
                                                |rule| rule.value_type = value_type,
                                                cx,
                                            );
                                        });
                                    }),
                            );
                        }
                        menu.max_w(px(160.0)).max_h(px(300.0)).scrollable(true)
                    })
                    .into_any_element()
            }
            crate::mongo_result::MongoFilterMenu::Conjunction(_) => {
                let selected = rule.conjunction;
                trigger
                    .dropdown_menu(move |mut menu, _, _| {
                        for conjunction in [
                            crate::mongo_result::MongoFilterConjunction::And,
                            crate::mongo_result::MongoFilterConjunction::Or,
                        ] {
                            let root = root.clone();
                            menu = menu.item(
                                PopupMenuItem::new(mongo_filter_conjunction_label(conjunction))
                                    .checked(conjunction == selected)
                                    .on_click(move |_, _, app| {
                                        let _ = root.update(app, |this, cx| {
                                            this.update_mongo_filter_rule(
                                                tab_id,
                                                rule_id,
                                                |rule| rule.conjunction = conjunction,
                                                cx,
                                            );
                                        });
                                    }),
                            );
                        }
                        menu.max_w(px(100.0))
                    })
                    .into_any_element()
            }
        }
    }

    #[allow(clippy::too_many_arguments)] // each parameter is one input of the operation; a parameter struct would only rename the list
    pub(in crate::app) fn render_mongo_filter_popover(
        &mut self,
        tab_id: u64,
        columns: Vec<String>,
        rules: Vec<crate::mongo_result::MongoFilterRule>,
        error: Option<String>,
        open: bool,
        trigger: Button,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let mut rows = div().flex().flex_col();
        for (index, rule) in rules.iter().enumerate() {
            let rule_id = rule.id;
            let field = rule.field.clone();
            let mode = rule.mode;
            let value_type = rule.value_type;
            let value = rule.value.clone();
            let conjunction_value = rule.conjunction;
            let value_editor = mode
                .needs_value()
                .then(|| self.mongo_filter_rule_editor(tab_id, rule_id, value, false, window, cx));
            let field_button = mongo_filter_dropdown_button(
                mongo_filter_element_id("mongo-filter-fields", tab_id, rule_id),
                field.clone(),
                px(104.0),
                format!("mongo-filter-field-label-{rule_id}"),
            );
            let mode_button = mongo_filter_dropdown_button(
                mongo_filter_element_id("mongo-filter-mode", tab_id, rule_id),
                mongo_filter_mode_label(mode),
                px(94.0),
                format!("mongo-filter-mode-label-{rule_id}"),
            );
            let type_button = mongo_filter_dropdown_button(
                mongo_filter_element_id("mongo-filter-type", tab_id, rule_id),
                mongo_filter_type_label(value_type),
                px(88.0),
                format!("mongo-filter-type-label-{rule_id}"),
            )
            .disabled(!mode.needs_value());
            if index > 0 {
                let target = crate::mongo_result::MongoFilterMenu::Conjunction(rule_id);
                let conjunction_button = Button::new(mongo_filter_element_id(
                    "mongo-filter-conjunction",
                    tab_id,
                    rule_id,
                ))
                .with_size(gpui_component::Size::XSmall)
                .label(mongo_filter_conjunction_label(conjunction_value))
                .compact()
                .dropdown_caret(true)
                .h(px(20.0));
                rows = rows.child(
                    div()
                        .debug_selector(|| "mongo-filter-conjunction-row".into())
                        .h(px(20.0))
                        .flex()
                        .items_center()
                        .justify_center()
                        .child(self.render_mongo_filter_menu(
                            tab_id,
                            target,
                            &columns,
                            rule,
                            conjunction_button,
                            cx,
                        )),
                );
            }
            let remove = ui::icon_button(
                mongo_filter_element_id("mongo-filter-remove", tab_id, rule_id),
                IconName::Close,
                "Remove filter rule",
            )
            .with_size(gpui_component::Size::XSmall)
            .disabled(rules.len() == 1)
            .on_click(cx.listener(move |this, _, _, cx| {
                this.remove_mongo_filter_rule(tab_id, rule_id, cx);
            }));
            let mut row = div()
                .debug_selector(|| "mongo-filter-rule-row".into())
                .h(px(32.0))
                .flex()
                .items_center()
                .gap(px(6.0))
                .child(
                    div()
                        .debug_selector(|| format!("mongo-filter-field-control-{rule_id}"))
                        .child(self.render_mongo_filter_menu(
                            tab_id,
                            crate::mongo_result::MongoFilterMenu::Field(rule_id),
                            &columns,
                            rule,
                            field_button,
                            cx,
                        )),
                )
                .child(
                    div()
                        .debug_selector(|| "mongo-filter-mode-control".into())
                        .child(self.render_mongo_filter_menu(
                            tab_id,
                            crate::mongo_result::MongoFilterMenu::Mode(rule_id),
                            &columns,
                            rule,
                            mode_button,
                            cx,
                        )),
                )
                .child(
                    div()
                        .debug_selector(|| format!("mongo-filter-type-control-{rule_id}"))
                        .child(self.render_mongo_filter_menu(
                            tab_id,
                            crate::mongo_result::MongoFilterMenu::ValueType(rule_id),
                            &columns,
                            rule,
                            type_button,
                            cx,
                        )),
                );
            if let Some(value_editor) = value_editor {
                let editor = value_editor.clone();
                row = row.child(
                    div()
                        .debug_selector(|| format!("mongo-filter-value-control-{rule_id}"))
                        .w(px(104.0))
                        .h(px(32.0))
                        .on_mouse_down(MouseButton::Left, move |_, window, cx| {
                            editor.update(cx, |state, cx| state.focus(window, cx));
                            window.prevent_default();
                        })
                        .child(<Input as Styled>::h(
                            Input::new(&value_editor)
                                .appearance(true)
                                .with_size(gpui_component::Size::XSmall),
                            px(32.0),
                        )),
                );
            } else {
                row = row.child(
                    div()
                        .w(px(98.0))
                        .h(px(32.0))
                        .flex()
                        .items_center()
                        .px(px(8.0))
                        .rounded(px(4.0))
                        .border_1()
                        .border_color(theme::border())
                        .text_size(px(12.0))
                        .text_color(theme::text_muted())
                        .child("No value"),
                );
            }
            rows = rows.child(row.child(remove));
        }
        let add = Button::new(("mongo-filter-add", tab_id))
            .with_size(gpui_component::Size::XSmall)
            .icon(Icon::new(IconName::Plus).size(px(14.0)))
            .label("Add rule")
            .compact()
            .ghost()
            .h(px(28.0))
            .on_click(cx.listener(move |this, _, _, cx| {
                let field = this.grid(tab_id).column_name(0).unwrap_or("").to_string();
                if let Some(runtime) = this.workspace.runtime_mut(tab_id) {
                    runtime.mongo.add_filter_rule(field);
                    runtime.mongo.mark_filter_draft();
                }
                this.notify_tab(tab_id, cx);
            }));
        let reset = Button::new(("mongo-filter-reset", tab_id))
            .with_size(gpui_component::Size::XSmall)
            .label("Reset rules")
            .compact()
            .ghost()
            .h(px(28.0))
            .on_click(cx.listener(move |this, _, _, cx| {
                this.reset_mongo_filter_builder(tab_id, cx);
            }));
        let clear = Button::new(("mongo-filter-clear", tab_id))
            .debug_selector(|| "mongo-filter-clear".into())
            .with_size(gpui_component::Size::XSmall)
            .icon(Icon::new(IconName::Delete).size(px(14.0)))
            .label("Clear Filter")
            .compact()
            .ghost()
            .h(px(28.0))
            .on_click(cx.listener(move |this, _, window, cx| {
                this.clear_mongo_filters(tab_id, true, window, cx);
            }));
        let apply = Button::new(("mongo-filter-apply", tab_id))
            .with_size(gpui_component::Size::XSmall)
            .label("Apply Filter")
            .compact()
            .primary()
            .h(px(28.0))
            .on_click(cx.listener(move |this, _, _, cx| {
                this.apply_mongo_filter_rules(tab_id, cx);
            }));
        let panel = div()
            .id("mongo-filter-panel")
            .debug_selector(|| "mongo-filter-panel".into())
            .w(px(468.0))
            .p(px(12.0))
            .flex()
            .flex_col()
            .gap(px(12.0))
            .child(
                div()
                    .debug_selector(|| "mongo-filter-header".into())
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
            .child(rows)
            .when_some(error, |element, error| {
                element.child(
                    div()
                        .text_size(px(10.0))
                        .text_color(theme::danger())
                        .child(error),
                )
            })
            .child(
                div()
                    .debug_selector(|| "mongo-filter-footer".into())
                    .h(px(28.0))
                    .flex()
                    .items_center()
                    .justify_between()
                    .child(add)
                    .child(div().flex().gap(px(4.0)).child(reset).child(apply)),
            )
            .bg(theme::surface())
            .border_1()
            .border_color(theme::border())
            .rounded(px(6.0))
            .shadow_lg()
            .occlude()
            .on_mouse_down(MouseButton::Left, |_, _, cx| cx.stop_propagation())
            .into_any_element();
        let trigger = trigger.on_click(cx.listener(move |this, _, _, cx| {
            let field = this.grid(tab_id).column_name(0).unwrap_or("").to_string();
            if let Some(runtime) = this.workspace.runtime_mut(tab_id) {
                runtime.mongo.filter_open = !runtime.mongo.filter_open;
                if runtime.mongo.filter_open {
                    runtime.mongo.ensure_filter_rule(field);
                } else {
                    runtime.mongo.close_filter_menu();
                }
            }
            this.notify_tab(tab_id, cx);
        }));
        let trigger_bounds = self.view(tab_id).mongo.filter_trigger_bounds.clone();
        let trigger = div()
            .id(("mongo-filter-anchor", tab_id))
            .relative()
            .child(trigger)
            .child(
                gpui::canvas(
                    move |bounds, _, _| *trigger_bounds.borrow_mut() = Some(bounds),
                    |_, _, _, _| {},
                )
                .absolute()
                .size_full(),
            )
            .into_any_element();
        let overlay = open
            .then(|| *self.view(tab_id).mongo.filter_trigger_bounds.borrow())
            .flatten()
            .map(|bounds| {
                super::super::window_overlay(
                    window,
                    div()
                        .debug_selector(|| "mongo-filter-overlay".into())
                        .size_full()
                        .occlude()
                        .on_mouse_down(
                            MouseButton::Left,
                            cx.listener(move |this, _, _, cx| {
                                if let Some(runtime) = this.workspace.runtime_mut(tab_id) {
                                    runtime.mongo.filter_open = false;
                                    runtime.mongo.close_filter_menu();
                                }
                                this.notify_tab(tab_id, cx);
                            }),
                        )
                        .child(
                            gpui::anchored()
                                .position(Point::new(bounds.left(), bounds.bottom() + px(2.0)))
                                .snap_to_window_with_margin(px(8.0))
                                .child(panel),
                        ),
                )
            });
        div().child(trigger).children(overlay).into_any_element()
    }
}
