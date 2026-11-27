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
    /// "tablex 0.1.0 - <active tab> · <connection>"; just the app and version with no tab.
    pub(super) fn window_title(&self) -> SharedString {
        let app = concat!("tablex ", env!("CARGO_PKG_VERSION"));
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
            .tooltip(t!("titlebar.theme"))
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

/// The language picker's entries; `current` is the checked one.
pub(super) fn language_menu_items(
    current: crate::settings::LanguageChoice,
    root: &gpui::WeakEntity<AppRoot>,
) -> Vec<PopupMenuItem> {
    use crate::settings::{Choice, LanguageChoice};
    let descriptor = crate::settings::registry::find("appearance.language");
    LanguageChoice::OPTIONS
        .iter()
        .filter_map(|(id, _)| LanguageChoice::from_id(id).map(|choice| (choice, *id)))
        .map(|(choice, id)| {
            // The language names read in their own language; only System follows the interface language.
            let label = SharedString::from(
                descriptor
                    .map(|descriptor| descriptor.option_text(id).into_owned())
                    .unwrap_or_default(),
            );
            let selector = format!("title-language-{id}");
            let root = root.clone();
            PopupMenuItem::element(move |_, _| {
                let selector = selector.clone();
                div()
                    .debug_selector(move || selector.clone())
                    .child(label.clone())
            })
            .checked(choice == current)
            .on_click(move |_, window, app| {
                let _ = root.update(app, |this, cx| this.set_language_choice(choice, window, cx));
            })
        })
        .collect()
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
        (ThemeChoice::Light, t!("titlebar.theme_light"), "title-theme-light"),
        (ThemeChoice::Dark, t!("titlebar.theme_dark"), "title-theme-dark"),
        (ThemeChoice::System, t!("titlebar.theme_system"), "title-theme-system"),
    ]
    .into_iter()
    .map(|(choice, label, selector)| {
        let root = root.clone();
        let label = SharedString::from(label);
        PopupMenuItem::element(move |_, _| {
            div().debug_selector(|| selector.into()).child(label.clone())
        })
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
        self.check_key_home(window, cx);
        self.show_access_notice(window, cx);
        self.schedule_session_save(cx);
        // Tests share one process, and with it the storage layer's warnings: they ask for them.
        #[cfg(not(test))]
        self.report_storage_warnings(cx);
        #[cfg(test)]
        self.show_storage_warnings(super::error_log_view::RENDER_WARNINGS.take());
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
            // Window title ("tablex 0.1.0 - <active tab>"),
            // centred in the space between the window controls and the Updates and theme buttons.
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
                    .child(self.render_updates_button(cx))
                    .child(self.render_theme_button(cx)),
            );

        // The left pane replays its last frame unless it was notified (see `SidebarView`). A change that is
        // only found here (state written during this render or since the pane last drew) cannot be notified
        // from inside a draw, so the pane is embedded uncached for this frame and renders now.
        let navigator = if self.sidebar_behind() {
            self.sidebar_view.clone().into_any_element()
        } else {
            self.sidebar_view
                .clone()
                .cached(gpui::StyleRefinement::default().size_full())
                .into_any_element()
        };
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
                    .label(t!("titlebar.new_connection"))
                    .small()
                    .compact()
                    .ghost()
                    .tooltip(t!("titlebar.new_connection_tooltip"))
                    .on_click(cx.listener(|this, _, window, cx| {
                        this.open_connection_dialog(None, window, cx);
                    })),
            )
            .child(
                Button::new("global-new-query")
                    .debug_selector(|| "global-new-query".into())
                    .icon(Icon::new(IconName::SquareTerminal).size(px(14.0)))
                    .label(t!("titlebar.new_query"))
                    .small()
                    .compact()
                    .ghost()
                    .tooltip(t!("titlebar.new_query_tooltip"))
                    .on_click(cx.listener(|this, _, _, cx| {
                        this.open_scoped_or_generic_query(cx);
                    })),
            )
            .child(
                Button::new("global-transfer")
                    .debug_selector(|| "global-transfer".into())
                    .icon(Icon::new(super::navigator_ui::SidebarGlyph::Transfer).size(px(14.0)))
                    .label(t!("titlebar.data_transfer"))
                    .small()
                    .compact()
                    .ghost()
                    .tooltip(t!("titlebar.data_transfer_tooltip"))
                    .on_click(cx.listener(|this, _, window, cx| this.open_transfer_from_toolbar(window, cx))),
            )
            .child(
                // A dot while drivers or tools have updates, as on the Updates button.
                div()
                    .relative()
                    .child(
                        Button::new("global-driver-manager")
                            .debug_selector(|| "global-driver-manager".into())
                            .icon(Icon::default().path("icons/settings-2.svg").size(px(14.0)))
                            .label(t!("titlebar.driver_manager"))
                            .small()
                            .compact()
                            .ghost()
                            .tooltip(if self.driver_ui.pending_updates() > 0 {
                                t!("titlebar.driver_manager_tooltip_updates")
                            } else {
                                t!("titlebar.driver_manager_tooltip")
                            })
                            .on_click(cx.listener(|this, _, _, cx| this.open_driver_manager(cx))),
                    )
                    .when(self.driver_ui.pending_updates() > 0, |this| {
                        this.child(super::updates::center::title_bar_dot("global-driver-manager-dot"))
                    }),
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
                            .label(t!("titlebar.assistant"))
                            .small()
                            .compact()
                            .ghost()
                            .selected(self.assistant.dock.open)
                            .tooltip(shortcuts::with_label(
                                &if self.assistant.asking {
                                    t!("titlebar.assistant_tooltip_asking")
                                } else {
                                    t!("titlebar.assistant_tooltip")
                                },
                                &self.settings.shortcuts,
                                "workspace.toggle_assistant",
                            ))
                            .on_click(
                                cx.listener(|this, _, window, cx| this.toggle_assistant(window, cx)),
                            ),
                    ),
                    ),
            )
            .child(
                Button::new("open-settings")
                    .debug_selector(|| "open-settings".into())
                    .icon(Icon::empty().path("icons/settings.svg").size(px(14.0)))
                    .label(t!("titlebar.settings"))
                    .small()
                    .compact()
                    .ghost()
                    .tooltip(shortcuts::with_label(
                        &t!("titlebar.settings"),
                        &self.settings.shortcuts,
                        "workspace.open_settings",
                    ))
                    .on_click(cx.listener(|this, _, window, cx| {
                        this.open_settings(window, cx);
                    })),
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
                    t!("titlebar.read_only", name = ty.name).into_owned()
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
        // The active tab's connection and database, when it has them.
        let status_target = self.workspace.active_tab().and_then(|tab| {
            let name = self.connection_name(&tab.connection_id)?;
            Some(if tab.database.is_empty() { name.to_owned() } else { format!("{name} \u{b7} {}", tab.database) })
        });
        let language = {
            let current = self.settings.appearance.language;
            let root = cx.entity().downgrade();
            Button::new("status-language")
                .debug_selector(|| "status-language".into())
                .xsmall()
                .compact()
                .ghost()
                .label(crate::i18n::code().to_uppercase())
                .text_color(theme::text_muted())
                .tooltip(t!("titlebar.language"))
                .dropdown_menu_with_anchor(gpui::Anchor::BottomRight, move |menu, _, _| {
                    language_menu_items(current, &root)
                        .into_iter()
                        .fold(menu.check_side(gpui_component::Side::Right), |menu, item| menu.item(item))
                })
        };
        // A thin vertical divider in front of each right-hand segment.
        let divider = || div().flex_shrink_0().w(px(1.0)).h(px(12.0)).mx(px(4.0)).bg(theme::border_strong());
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
                // A footer block of its own: a different tone from the panels above, a border on top
                // and the right-hand items cut into segments by dividers.
                div()
                    .h(px(theme::STATUS_HEIGHT))
                    .debug_selector(|| "status-bar".into())
                    .bg(theme::selected())
                    .border_t_1()
                    .border_color(theme::border_strong())
                    .pl(px(12.0))
                    .pr(px(4.0))
                    .flex()
                    .items_center()
                    .text_size(px(11.0))
                    .text_color(theme::text_muted())
                    .child(status_left)
                    .children(status_ssh.map(|via| {
                        div()
                            .debug_selector(|| "status-ssh".into())
                            .mx(px(4.0))
                            .child(via)
                    }))
                    .children(status_type.map(|(label, color)| {
                        div()
                            .debug_selector(|| "status-connection-type".into())
                            .mx(px(4.0))
                            .px(px(6.0))
                            .rounded(px(3.0))
                            .border_1()
                            .border_color(color)
                            .text_color(color)
                            .font_weight(gpui::FontWeight::SEMIBOLD)
                            .child(label)
                    }))
                    .when(show_fps, |this| {
                        this.child(divider()).child(
                            div()
                                .debug_selector(|| "status-fps".into())
                                .mx(px(4.0))
                                .text_color(theme::text_faint())
                                .child(format!("{} fps", self.shell.fps_display)),
                        )
                    })
                    .when_some(status_target, |this, target| {
                        this.child(divider()).child(
                            div()
                                .debug_selector(|| "status-target".into())
                                .flex()
                                .items_center()
                                .gap(px(4.0))
                                .mx(px(4.0))
                                .min_w_0()
                                .max_w(px(260.0))
                                .child(Icon::empty().path("icons/database.svg").size(px(11.0)).text_color(theme::text_muted()))
                                .child(div().min_w_0().truncate().child(target)),
                        )
                    })
                    .child(divider())
                    .child(language)
                    .child(divider())
                    .child(error_log_icon),
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
                                t!("titlebar.show_sidebar"),
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
            .on_action(cx.listener(|this, _: &RunActiveQuery, window, cx| {
                // A Results tab runs the query tab it shows.
                if let Some(tab_id) = this.active_results_tab() {
                    this.run_tab_as_its_run_button(tab_id, window, cx);
                }
            }))
            .on_action(cx.listener(|this, _: &RunScript, _, cx| {
                this.run_active_tab_span(super::workspace::RunSpan::Script, cx);
            }))
            .on_action(cx.listener(|this, _: &RunFromCursor, _, cx| {
                this.run_active_tab_span(super::workspace::RunSpan::FromCursor, cx);
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
            .on_action(cx.listener(|this, _: &ToggleVimMode, window, cx| {
                this.toggle_active_vim(window, cx);
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
            .on_action(cx.listener(|this, _: &OpenQueryHistory, window, cx| {
                this.open_query_history(None, window, cx);
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

/// The first window's bounds: GPUI's default size, clipped to `visible` (the display without the
/// taskbar, dock or menu bar) and centred in it.
fn fit_to_visible(visible: gpui::Bounds<gpui::Pixels>) -> gpui::Bounds<gpui::Pixels> {
    let size = gpui::DEFAULT_WINDOW_SIZE.min(&visible.size);
    let origin = gpui::point(
        visible.origin.x + (visible.size.width - size.width) / 2.0,
        visible.origin.y + (visible.size.height - size.height) / 2.0,
    );
    gpui::Bounds::new(origin, size)
}

pub(crate) fn open_with_scenario(scenario: Option<UiScenario>, cx: &mut gpui::App) {
    let scenario_size = scenario
        .as_ref()
        .map(UiScenario::window_size)
        .map_or((1100.0, 850.0), |(width, height)| (width as f32, height as f32));
    let scenario_title = scenario.as_ref().map(|scenario| {
        gpui::SharedString::from(format!("tablex UI Scenario: {}", scenario.name()))
    });
    actions::bind_keys(cx);
    let opened = cx.open_window(
        gpui::WindowOptions {
            window_bounds: match &scenario {
                Some(_) => Some(gpui::WindowBounds::centered(
                    None,
                    gpui::Size {
                        width: px(scenario_size.0),
                        height: px(scenario_size.1),
                    },
                    cx,
                )),
                // GPUI's own default fits the window to the whole display, so on a small screen it
                // runs under the Windows taskbar; fit it to the display's visible area instead.
                None => cx
                    .primary_display()
                    .map(|display| gpui::WindowBounds::Windowed(fit_to_visible(display.visible_bounds()))),
            },
            titlebar: Some({
                let mut titlebar = gpui_component::TitleBar::title_bar_options();
                titlebar.title = scenario_title.clone();
                titlebar
            }),
            is_resizable: true,
            // The Wayland app id / X11 window class: the Linux desktop file (scripts/package-linux.sh)
            // names it, which gives the window its icon in the dock and the task switcher.
            app_id: Some("tablex".into()),
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
    );
    if let Err(error) = opened {
        // No graphics backend: a panic would leave a double-click with no window and no word.
        super::window_failure::report_and_exit(&error);
    }
    // After the first window exists: install the menu bar and take the
    // foreground so the previous app's menus do not stay on screen.
    super::menus::finish_launch(cx, |cx| cx.activate(true));
}

#[cfg(test)]
mod window_bounds_tests {
    use gpui::{Bounds, point, px, size};

    #[test]
    fn the_first_window_fits_the_visible_area_of_a_small_display() {
        // 1280x800 with a 48 px taskbar: the default 1536x1095 would run under it.
        let visible = Bounds::new(point(px(0.), px(0.)), size(px(1280.), px(752.)));
        assert_eq!(super::fit_to_visible(visible), visible);
    }

    #[test]
    fn a_large_display_keeps_the_default_size_centred_in_its_visible_area() {
        // A menu bar of 25 px above the visible area.
        let visible = Bounds::new(point(px(0.), px(25.)), size(px(2560.), px(1415.)));
        let bounds = super::fit_to_visible(visible);
        assert_eq!(bounds.size, gpui::DEFAULT_WINDOW_SIZE);
        assert_eq!(bounds.origin, point(px(512.), px(185.)));
    }
}
