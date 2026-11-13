use super::*;
use gpui::SharedString;

pub(super) struct ShellState {
    pub(super) fps_display: u64,
    pub(super) sidebar_width: f32,
    pub(super) sidebar_collapsed: bool,
    pub(super) tab_bar: super::tab_bar::TabBarUi,
    /// Painted bounds of the panes (directional focus).
    pub(super) pane_bounds: super::panes_ui::PaneBounds,
    pub(super) session: super::session_persist::SessionPersist,
    /// Tabs closed most recently, oldest first, for "Reopen Closed Tab" (touched only on close
    /// and reopen).
    pub(super) closed_tabs: std::collections::VecDeque<crate::gpui_session::SessionTab>,
    /// Last title given to the OS window (set only when it changes).
    pub(super) window_title: SharedString,
    /// Scenario windows keep the title the capture tool finds them by.
    pub(super) fixed_window_title: bool,
    /// The window's close request is routed through the discard guard (installed once, on the
    /// first frame).
    pub(super) close_guarded: bool,
}

impl Default for ShellState {
    fn default() -> Self {
        Self {
            fps_display: 0,
            sidebar_width: theme::SIDEBAR_WIDTH,
            sidebar_collapsed: false,
            tab_bar: Default::default(),
            pane_bounds: Default::default(),
            session: Default::default(),
            closed_tabs: Default::default(),
            window_title: SharedString::default(),
            fixed_window_title: false,
            close_guarded: false,
        }
    }
}

impl AppRoot {
    /// "TableX 0.1.0 - <active tab> · <connection>"; just the app and version with no tab.
    pub(super) fn window_title(&self) -> SharedString {
        let app = concat!("TableX ", env!("CARGO_PKG_VERSION"));
        let Some(tab) = self.workspace.active_tab() else {
            return SharedString::new_static(app);
        };
        let label = super::tab_bar::tab_label(tab, false);
        match self.connection_name(&tab.connection_id) {
            Some(connection) if !label.contains(connection) => {
                format!("{app} - {label} · {connection}").into()
            }
            _ => format!("{app} - {label}").into(),
        }
    }

    /// The title bar's theme picker: a ghost icon button showing the current
    /// choice, opening Light / Dark / System with the current one checked.
    fn render_theme_button(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let current = self.settings.appearance.theme;
        let root = cx.entity().downgrade();
        Button::new("title-theme")
            .debug_selector(|| "title-theme-button".into())
            .icon(Icon::empty().path(theme_icon(current)).size(px(14.0)))
            .compact()
            .ghost()
            .tooltip("Theme")
            .dropdown_menu_with_anchor(gpui::Anchor::TopRight, move |menu, _, _| {
                // Icon on the left, check on the right: both show at once.
                theme_menu_items(current, &root)
                    .into_iter()
                    .fold(menu.check_side(gpui_component::Side::Right), |menu, item| {
                        menu.item(item)
                    })
            })
    }
}

pub(super) fn theme_icon(choice: crate::settings::ThemeChoice) -> &'static str {
    use crate::settings::ThemeChoice;
    match choice {
        ThemeChoice::Light => "icons/sun.svg",
        ThemeChoice::Dark => "icons/moon.svg",
        ThemeChoice::System => "icons/monitor.svg",
    }
}

/// The theme picker's entries; `current` is the checked one.
pub(super) fn theme_menu_items(
    current: crate::settings::ThemeChoice,
    root: &gpui::WeakEntity<AppRoot>,
) -> Vec<PopupMenuItem> {
    use crate::settings::ThemeChoice;
    [
        (ThemeChoice::Light, "Light", "title-theme-light"),
        (ThemeChoice::Dark, "Dark", "title-theme-dark"),
        (ThemeChoice::System, "System", "title-theme-system"),
    ]
    .into_iter()
    .map(|(choice, label, selector)| {
        let root = root.clone();
        PopupMenuItem::element(move |_, _| div().debug_selector(|| selector.into()).child(label))
            .icon(Icon::empty().path(theme_icon(choice)))
            .checked(choice == current)
            .on_click(move |_, window, app| {
                let _ = root.update(app, |this, cx| this.set_theme_choice(choice, window, cx));
            })
    })
    .collect()
}

impl Render for AppRoot {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        FRAME_COUNT.fetch_add(1, Ordering::Relaxed);
        self.driver_runtime
            .window
            .get_or_insert_with(|| window.window_handle());
        if self.window_root.is_none() {
            self.window_root = window
                .root::<gpui_component::Root>()
                .flatten()
                .map(|root| root.downgrade());
        }
        self.guard_window_close(window, cx);
        self.schedule_session_save(cx);
        self.flush_error_log(cx);
        let active_tab_id = self.workspace.active_tab().map(|tab| tab.id);
        #[cfg(feature = "ui-scenarios")]
        self.emit_render_state();
        let active_runtime = active_tab_id.and_then(|id| self.workspace.runtime(id));
        let status_text = active_runtime
            .map(|runtime| runtime.query.status_text())
            .unwrap_or_else(|| "No tab".to_string());

        let window_title = self.window_title();
        // The OS title (Dock / taskbar / window switcher) follows the bar.
        // Scenario windows keep their fixed title: the capture tool finds
        // them by it.
        if !self.shell.fixed_window_title && self.shell.window_title != window_title {
            window.set_window_title(&window_title);
            self.shell.window_title = window_title.clone();
        }

        let title_bar = gpui_component::TitleBar::new()
            .bg(theme::chrome())
            .border_b_1()
            .border_color(theme::border())
            // Window title ("TableX 0.1.0 - <active tab>"),
            // centred in the space between the window controls and the gear.
            .child(
                div()
                    .debug_selector(|| "window-title".into())
                    .h_full()
                    .flex_1()
                    .min_w_0()
                    .flex()
                    .items_center()
                    .justify_center()
                    .overflow_hidden()
                    .child(
                        div()
                            .min_w_0()
                            .truncate()
                            .text_size(px(12.0))
                            .text_color(theme::text_muted())
                            .child(window_title.clone()),
                    ),
            )
            .child(
                div()
                    .h_full()
                    .flex()
                    .flex_shrink_0()
                    .items_center()
                    .pr(px(8.0))
                    .child(self.render_theme_button(cx))
                    .child(
                        Button::new("open-settings")
                            .debug_selector(|| "open-settings".into())
                            .icon(Icon::empty().path("icons/settings.svg").size(px(14.0)))
                            .compact()
                            .ghost()
                            .tooltip(shortcuts::with_label(
                                "Settings",
                                &self.settings.shortcuts,
                                "workspace.open_settings",
                            ))
                            .on_click(cx.listener(|this, _, window, cx| {
                                this.open_settings(window, cx);
                            })),
                    ),
            );

        let navigator = self.render_navigator(window, cx);
        let sidebar_width = self.shell.sidebar_width;

        // The New Connection / New Query / Data Transfer
        // actions live in the global window toolbar above the sidebar+main
        // split, never inside the sidebar pane, so a
        // narrow pane cannot clip them.
        let global_toolbar = div()
            .h(px(40.0))
            .flex_shrink_0()
            .flex()
            .items_center()
            .gap(px(4.0))
            .px(px(8.0))
            .bg(theme::chrome())
            .border_b_1()
            .border_color(theme::border())
            .child(
                Button::new("global-new-connection")
                    .debug_selector(|| "global-new-connection".into())
                    .icon(Icon::new(IconName::Plus).size(px(14.0)))
                    .label("New Connection")
                    .small()
                    .compact()
                    .ghost()
                    .tooltip("New connection")
                    .on_click(cx.listener(|this, _, window, cx| {
                        this.open_connection_dialog(None, window, cx);
                    })),
            )
            .child(
                Button::new("global-new-query")
                    .debug_selector(|| "global-new-query".into())
                    .icon(Icon::new(IconName::SquareTerminal).size(px(14.0)))
                    .label("New Query")
                    .small()
                    .compact()
                    .ghost()
                    .tooltip("New query for the selected sidebar node")
                    .on_click(cx.listener(|this, _, _, cx| {
                        this.open_scoped_or_generic_query(cx);
                    })),
            )
            .child(
                Button::new("global-transfer")
                    .debug_selector(|| "global-transfer".into())
                    .icon(Icon::new(super::navigator_ui::SidebarGlyph::Transfer).size(px(14.0)))
                    .label("Data Transfer")
                    .small()
                    .compact()
                    .ghost()
                    .tooltip("Copy tables or a query into tables or files")
                    .on_click(cx.listener(|this, _, window, cx| this.open_transfer_from_toolbar(window, cx))),
            )
            .child(
                Button::new("global-driver-manager")
                    .debug_selector(|| "global-driver-manager".into())
                    .icon(Icon::default().path("icons/settings-2.svg").size(px(14.0)))
                    .label("Driver Manager")
                    .small()
                    .compact()
                    .ghost()
                    .tooltip(if self.driver_ui.updates_available > 0 {
                        "Install, update and remove database drivers (updates available)"
                    } else {
                        "Install, update and remove database drivers"
                    })
                    .on_click(cx.listener(|this, _, _, cx| this.open_driver_manager(cx))),
            )
            .child(
                // A mark on the button while a statement waits for the user, dock open or not.
                div()
                    .debug_selector({
                        let asking = self.assistant.asking;
                        move || {
                            if asking {
                                "assistant-toggle-waiting"
                            } else {
                                "assistant-toggle-idle"
                            }
                            .into()
                        }
                    })
                    .child(
                gpui_component::badge::Badge::new()
                    .count(usize::from(self.assistant.asking))
                    .color(theme::warning())
                    .child(
                        Button::new("global-assistant")
                            .debug_selector(|| "global-assistant".into())
                            .icon(
                                Icon::default()
                                    .path("icons/wand-sparkles.svg")
                                    .size(px(14.0)),
                            )
                            .label("Assistant")
                            .small()
                            .compact()
                            .ghost()
                            .selected(self.assistant.dock.open)
                            .tooltip(shortcuts::with_label(
                                if self.assistant.asking {
                                    "A statement waits for your answer. Show or hide the AI assistant"
                                } else {
                                    "Show or hide the AI assistant"
                                },
                                &self.settings.shortcuts,
                                "workspace.toggle_assistant",
                            ))
                            .on_click(
                                cx.listener(|this, _, window, cx| this.toggle_assistant(window, cx)),
                            ),
                    ),
                    ),
            );

        let root = cx.entity().downgrade();
        let upper = div()
            .flex()
            .flex_col()
            .flex_1()
            .min_h_0()
            .min_w_0()
            .overflow_hidden()
            .child(self.render_panes(window, cx));
        // Test probe: an element-keyed state inside the main area, to see it survive the dock.
        #[cfg(test)]
        let upper = upper.child(
            div()
                .debug_selector(|| "assistant-toggle-probe".into())
                .h(px(1.0))
                .overflow_hidden()
                .child(gpui_component::text::TextView::markdown(
                    "assistant-toggle-probe",
                    "probe",
                )),
        );
        let upper = self.with_error_log_pane(upper, window, cx);
        let show_fps = self.settings.appearance.show_fps;
        // Production (or any read-only type) of the active tab's connection.
        let status_type = self
            .workspace
            .active_tab()
            .and_then(|tab| self.connection_type_of(&tab.connection_id))
            .filter(|ty| ty.is_production() || ty.read_only)
            .map(|ty| {
                let label = if ty.read_only {
                    format!("{} \u{00B7} read-only", ty.name)
                } else {
                    ty.name.clone()
                };
                (label, connection_policy::type_color(ty))
            });
        // The active tab's connection goes through an SSH tunnel.
        let status_ssh = self
            .workspace
            .active_tab()
            .and_then(|tab| self.via_ssh(&tab.connection_id));
        let status_left = self.render_status_text(status_text, cx);
        let error_log_icon = self.render_error_log_icon(cx);
        let main_panel = div()
            .flex()
            .flex_col()
            .flex_1()
            .min_h_0()
            .min_w_0()
            .overflow_hidden()
            .bg(theme::background())
            .child(upper)
            .child(
                div()
                    .h(px(theme::STATUS_HEIGHT))
                    .debug_selector(|| "status-bar".into())
                    .bg(theme::chrome())
                    .border_t_1()
                    .border_color(theme::border())
                    .pl(px(12.0))
                    .pr(px(8.0))
                    .flex()
                    .items_center()
                    .text_size(px(11.0))
                    .text_color(theme::text_muted())
                    .child(status_left)
                    .children(status_ssh.map(|via| {
                        div()
                            .debug_selector(|| "status-ssh".into())
                            .mr(px(8.0))
                            .child(via)
                    }))
                    .children(status_type.map(|(label, color)| {
                        div()
                            .debug_selector(|| "status-connection-type".into())
                            .mr(px(8.0))
                            .px(px(6.0))
                            .rounded(px(3.0))
                            .border_1()
                            .border_color(color)
                            .text_color(color)
                            .font_weight(gpui::FontWeight::SEMIBOLD)
                            .child(label)
                    }))
                    .child(error_log_icon)
                    .when(show_fps, |this| {
                        this.child(
                            div()
                                .debug_selector(|| "status-fps".into())
                                .text_color(theme::text_faint())
                                .child(format!("{} fps", self.shell.fps_display)),
                        )
                    }),
            );
        // The assistant dock sits beside the main panel in a split group that is always there
        // (one panel while the dock is closed), so opening or closing the dock does not change
        // the element-id path of anything in the main area: element-keyed state there (scroll
        // offsets, split sizes, text views) survives a toggle. The split's own sizes are an
        // entity for the same reason.
        let dock = self.render_assistant_dock(window, cx);
        let dock_root = root.clone();
        let dock_width = self.assistant.dock.width;
        let main_panel = gpui_component::resizable::h_resizable("assistant-split")
            .with_state(&self.assistant.dock.split_state(cx))
            .on_resize(move |state, _, app| {
                // Only a two-panel group (dock open) has a handle to drag.
                let Some(width) = state.read(app).sizes().get(1).copied() else {
                    return;
                };
                let _ = dock_root.update(app, |this, cx| {
                    this.assistant_resized(f32::from(width), cx);
                });
            })
            .child(
                gpui_component::resizable::resizable_panel()
                    .size_range(px(320.0)..gpui::Pixels::MAX)
                    .child(main_panel),
            )
            .when_some(dock, |group, dock| {
                group.child(
                    gpui_component::resizable::resizable_panel()
                        .size(px(dock_width))
                        .size_range(
                            px(crate::sidebar_preferences::ASSISTANT_WIDTH_MIN)
                                ..px(crate::sidebar_preferences::ASSISTANT_WIDTH_MAX),
                        )
                        .child(div().size_full().child(dock)),
                )
            })
            .into_any_element();
        let shell = if self.shell.sidebar_collapsed {
            div()
                .flex()
                .flex_1()
                .min_h_0()
                .child(
                    div()
                        .w(px(36.0))
                        .flex_shrink_0()
                        .flex()
                        .flex_col()
                        .items_center()
                        .py(px(8.0))
                        .bg(theme::sidebar())
                        .border_r_1()
                        .border_color(theme::border())
                        .child(
                            ui::icon_button(
                                "expand-sidebar",
                                IconName::ChevronRight,
                                "Show sidebar",
                            )
                            .debug_selector(|| "expand-sidebar".into())
                            .on_click(cx.listener(|this, _, _, cx| {
                                this.shell.sidebar_collapsed = false;
                                this.notify_chrome(cx);
                            })),
                        ),
                )
                .child(main_panel)
                .into_any_element()
        } else {
            gpui_component::resizable::h_resizable("main-split")
                .on_resize(move |state, _, app| {
                    let Some(width) = state.read(app).sizes().first().copied() else {
                        return;
                    };
                    // on_resize fires at drag end only (mouse-up persist in
                    // the pinned resizable panel), so this single file write
                    // persists on mouse-up only — no per-frame disk writes.
                    let _ = root.update(app, |this, cx| {
                        this.shell.sidebar_width = f32::from(width);
                        this.persist_sidebar_width(f32::from(width), cx);
                        this.notify_chrome(cx);
                    });
                })
                .child(
                    gpui_component::resizable::resizable_panel()
                        .size(px(sidebar_width))
                        .size_range(px(theme::SIDEBAR_MIN_WIDTH)..px(theme::SIDEBAR_MAX_WIDTH))
                        .child(navigator),
                )
                .child(
                    gpui_component::resizable::resizable_panel()
                        .size_range(px(320.0)..gpui::Pixels::MAX)
                        .child(main_panel),
                )
                .into_any_element()
        };

        let root = div()
            .relative()
            .size_full()
            .flex()
            .flex_col()
            .bg(theme::background())
            // Global Cmd-S: the dialog itself silently ignores non-SQL tabs,
            // so this never hijacks keystrokes from unrelated inputs.
            .on_action(cx.listener(|this, _: &SaveActiveSql, window, cx| {
                this.save_active_sql(window, cx);
            }))
            .on_action(cx.listener(|this, _: &SaveSqlFileAs, window, cx| {
                this.save_sql_file_as(window, cx);
            }))
            .on_action(cx.listener(|this, _: &OpenSqlFile, window, cx| {
                this.open_sql_file(window, cx);
            }))
            .on_action(cx.listener(|this, _: &FormatSql, window, cx| {
                this.format_sql(window, cx);
            }))
            .on_action(cx.listener(|this, _: &ExplainPlan, _, cx| {
                this.explain_active_tab(engine_plan::ExplainMode::Estimate, cx);
            }))
            .on_action(cx.listener(|this, _: &ExplainPlanAnalyze, _, cx| {
                this.explain_active_tab(engine_plan::ExplainMode::Analyze, cx);
            }))
            .on_action(cx.listener(|this, _: &CommitTransaction, window, cx| {
                this.end_active_transaction(transaction::TxEnd::Commit, window, cx);
            }))
            .on_action(cx.listener(|this, _: &RollbackTransaction, window, cx| {
                this.end_active_transaction(transaction::TxEnd::Rollback, window, cx);
            }))
            .on_action(cx.listener(|this, _: &ToggleAutoCommit, window, cx| {
                this.toggle_commit_mode(window, cx);
            }))
            .on_action(cx.listener(|this, _: &CloseActiveTab, window, cx| {
                this.close_active_tab_action(window, cx);
            }))
            .on_action(cx.listener(|this, _: &ReopenClosedTab, _, cx| {
                this.reopen_closed_tab(cx);
            }))
            .on_action(cx.listener(|this, _: &NextTabGroup, _, cx| {
                this.step_tab_group(1, cx);
            }))
            .on_action(cx.listener(|this, _: &PreviousTabGroup, _, cx| {
                this.step_tab_group(-1, cx);
            }))
            .on_action(cx.listener(|this, _: &FocusPaneLeft, _, cx| {
                this.focus_pane_direction(crate::panes::Edge::Left, cx);
            }))
            .on_action(cx.listener(|this, _: &FocusPaneRight, _, cx| {
                this.focus_pane_direction(crate::panes::Edge::Right, cx);
            }))
            .on_action(cx.listener(|this, _: &FocusPaneUp, _, cx| {
                this.focus_pane_direction(crate::panes::Edge::Top, cx);
            }))
            .on_action(cx.listener(|this, _: &FocusPaneDown, _, cx| {
                this.focus_pane_direction(crate::panes::Edge::Bottom, cx);
            }))
            .on_action(cx.listener(|this, _: &ToggleErrorLog, window, cx| {
                this.toggle_error_log(window, cx);
            }))
            .on_action(cx.listener(|this, _: &ToggleAssistant, window, cx| {
                this.toggle_assistant(window, cx);
            }))
            .on_action(cx.listener(|this, _: &AssistantExplain, window, cx| {
                this.assistant_editor_ask(assistant::EditorAsk::Explain, window, cx);
            }))
            .on_action(cx.listener(|this, _: &AssistantFix, window, cx| {
                this.assistant_editor_ask(assistant::EditorAsk::Fix, window, cx);
            }))
            .on_action(cx.listener(|this, _: &AssistantOptimise, window, cx| {
                this.assistant_editor_ask(assistant::EditorAsk::Optimise, window, cx);
            }))
            .child(title_bar)
            .child(global_toolbar)
            .child(div().flex().flex_1().min_h_0().child(shell));

        root
    }
}

pub(crate) fn open_with_scenario(scenario: Option<UiScenario>, cx: &mut gpui::App) {
    let scenario_size = scenario
        .as_ref()
        .filter(|scenario| scenario.fixture == FixtureKind::Narrow)
        .map(|_| (720.0, 650.0))
        .unwrap_or((1100.0, 850.0));
    let scenario_title = scenario.as_ref().map(|scenario| {
        gpui::SharedString::from(format!("TableX UI Scenario: {}", scenario.name()))
    });
    actions::bind_keys(cx);
    cx.open_window(
        gpui::WindowOptions {
            window_bounds: scenario.as_ref().map(|_| {
                gpui::WindowBounds::centered(
                    gpui::Size {
                        width: px(scenario_size.0),
                        height: px(scenario_size.1),
                    },
                    cx,
                )
            }),
            titlebar: Some({
                let mut titlebar = gpui_component::TitleBar::title_bar_options();
                titlebar.title = scenario_title.clone();
                titlebar
            }),
            is_resizable: true,
            // Linux defaults to server-side decorations, where the window
            // manager draws its own bar and `TitleBar` omits its min / max /
            // close buttons. Ask for client-side so the title bar is the only
            // chrome (ignored on macOS and Windows, which never use it).
            window_decorations: cfg!(target_os = "linux")
                .then_some(gpui::WindowDecorations::Client),
            ..Default::default()
        },
        move |window, cx| {
            let scenario_for_view = scenario.clone();
            let view = cx.new(|cx| match scenario_for_view {
                Some(scenario) => {
                    let mut app = AppRoot::new_for_scenario(scenario, cx);
                    app.shell.fixed_window_title = true;
                    app
                }
                None => AppRoot::new(cx),
            });
            let root = cx.new(|cx| gpui_component::Root::new(view.clone(), window, cx));
            AppRoot::follow_system_appearance(&view, window, cx);
            let drop_view = view.clone();
            let overrides_view = view.clone();
            let import_view = view.clone();
            let redis_view = view.clone();
            if scenario.as_ref().is_some_and(|scenario| {
                scenario.kind == crate::ui_scenario::ScenarioKind::NewConnection
            }) {
                window.on_next_frame(move |window, cx| {
                    view.update(cx, |this, cx| {
                        this.open_connection_dialog(None, window, cx);
                    });
                });
            }
            if scenario.as_ref().is_some_and(|scenario| {
                scenario.kind == crate::ui_scenario::ScenarioKind::ConnectionOverrides
            }) {
                window.on_next_frame(move |window, cx| {
                    overrides_view.update(cx, |this, cx| {
                        this.open_scenario_overrides_dialog(window, cx);
                    });
                });
            }
            if scenario.as_ref().is_some_and(|scenario| {
                scenario.kind == crate::ui_scenario::ScenarioKind::ImportSelection
            }) {
                window.on_next_frame(move |window, cx| {
                    import_view.update(cx, |this, cx| {
                        this.open_scenario_import_selection(window, cx);
                    });
                });
            }
            if scenario
                .as_ref()
                .is_some_and(|scenario| scenario.kind == crate::ui_scenario::ScenarioKind::Redis)
                && std::env::var("TABLEX_SEED_REDIS").as_deref() == Ok("confirm")
            {
                window.on_next_frame(move |window, cx| {
                    redis_view.update(cx, |this, cx| {
                        this.open_scenario_redis_confirmation(window, cx);
                    });
                });
            }
            if scenario.as_ref().is_some_and(|scenario| {
                scenario.kind == crate::ui_scenario::ScenarioKind::SplitPanesDrop
            }) {
                // The pinned drop zone is sized from the painted pane bounds,
                // which exist after the first frame.
                window.on_next_frame(move |window, cx| {
                    drop_view.update(cx, |this, cx| this.notify_pane_views(cx));
                    window.refresh();
                });
            }
            root
        },
    )
    .expect("failed to open TableX window");
    // After the first window exists: install the menu bar and take the
    // foreground so the previous app's menus do not stay on screen.
    super::menus::finish_launch(cx, |cx| cx.activate(true));
}
