//! The Tools tab of the Driver Manager (a filter chip beside the driver ones): one row per free-standing tool of the catalog (the
//! MCP server, the SQL formatter; an add-on of a driver has its own row under that driver): name,
//! what it is, installed / available version, Install / Update / Uninstall with the same progress,
//! cancel and error presentation as a driver row, the licence and source when the catalog names them,
//! and what the tool is for. The MCP server also shows its stable path as selectable text with the
//! two copy buttons. Plain official components: `Button`, `Progress`, `TextView`. The debug
//! selectors of a row end in `:<tool id>`.

use super::tools::{
    addon_actions, code_span, tool_actions, tool_description, tool_note, tool_version_lines,
    ToolAction, ToolJob,
};
use super::view::{
    badge_element, ACTION_GAP, ACTION_WIDTH, ADDON_DRIVER_ACTION_WIDTH,
    LIST_MIN_WIDTH_FOR_STATE_COLUMN, MORE_WIDTH, ROW_HEIGHT, ROW_PAD,
};
use engine_driver_host::MONGO_SHELL_TOOL_ID;
use super::*;
use engine_driver_host::{ToolCatalogItem, ToolCatalogState, ToolKind};
use gpui_component::progress::Progress;
use gpui_component::scroll::ScrollableElement;
use gpui_component::text::TextView;

/// Whether the search text (trimmed, lower-cased) is in the tool's name or description.
fn tool_matches(item: &ToolCatalogItem, query: &str) -> bool {
    query.is_empty()
        || item.display_name.to_lowercase().contains(query)
        || tool_description(item).to_lowercase().contains(query)
}

impl AppRoot {
    /// The Tools tab: the tools matching the search text, scrollable.
    pub(super) fn render_driver_tools(&self, cx: &mut Context<Self>) -> AnyElement {
        let ui = &self.driver_ui;
        let query = ui.model.query();
        let rows: Vec<AnyElement> = ui
            .tools
            .standalone()
            .filter(|item| tool_matches(item, query))
            .enumerate()
            .map(|(position, item)| self.render_tool_item(position, item, ui.tools.job_of(&item.id), cx))
            .collect();
        if rows.is_empty() {
            return div()
                .debug_selector(|| "driver-tools-empty".into())
                .pt(px(10.0))
                .text_color(theme::text_muted())
                .child(if !ui.installed_loaded || ui.catalog_task.is_some() {
                    "Loading tools\u{2026}"
                } else if ui.tools.standalone().next().is_none() {
                    "No tools available"
                } else {
                    "No tools match"
                })
                .into_any_element();
        }
        div()
            .debug_selector(|| "driver-tools".into())
            .flex_1()
            .min_h_0()
            .flex()
            .flex_col()
            .gap(px(8.0))
            .pt(px(10.0))
            .overflow_y_scrollbar()
            .children(rows)
            .into_any_element()
    }

    fn render_tool_item(
        &self,
        position: usize,
        item: &ToolCatalogItem,
        job: Option<&ToolJob>,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let (installed_line, available_line) = tool_version_lines(item);
        // Selectors end in the tool's id (more than one row can be on screen), and are built only
        // when asked for: each closure shares this one id.
        let tool_id: gpui::SharedString = item.id.clone().into();
        let selector_of = |base: &'static str| {
            let id = tool_id.clone();
            move || format!("{base}:{id}")
        };
        let badge = match &item.state {
            ToolCatalogState::Installed { .. } => Some(Badge::Installed),
            ToolCatalogState::UpdateAvailable { .. } => Some(Badge::Update),
            ToolCatalogState::Unavailable { reason } => Some(Badge::Unavailable(reason.clone())),
            ToolCatalogState::Available => None,
        };
        let update = matches!(item.state, ToolCatalogState::UpdateAvailable { .. });

        let middle = match job.map(|job| &job.state) {
            Some(state) if state.is_running() => {
                let (done, total) = match state {
                    InstallState::Downloading { bytes, total } => (*bytes, *total),
                    _ => {
                        let total = item
                            .entry
                            .as_ref()
                            .and_then(|entry| {
                                entry.artifact_for_target(&crate::drivers::current_target())
                            })
                            .map_or(0, |artifact| artifact.size_bytes);
                        (total, total)
                    }
                };
                let step = if job.is_some_and(ToolJob::cancelling) {
                    "Cancelling\u{2026}"
                } else {
                    state.step_label()
                };
                div()
                    .debug_selector(selector_of("tool-progress"))
                    .w(px(260.0))
                    .flex_shrink_0()
                    .flex()
                    .flex_col()
                    .gap(px(3.0))
                    .child(Progress::new(("tool-bar", position)).value(state.fraction() * 100.0))
                    .child(
                        div()
                            .truncate()
                            .text_size(px(11.0))
                            .text_color(theme::text_muted())
                            .child(format!(
                                "{}% \u{00B7} {} \u{00B7} {step}",
                                (state.fraction() * 100.0).round() as u32,
                                format_size_pair(done, total)
                            )),
                    )
                    .into_any_element()
            }
            _ => div()
                .debug_selector(selector_of("tool-versions"))
                .w(px(260.0))
                .flex_shrink_0()
                .flex()
                .flex_col()
                .text_size(px(12.0))
                .child(div().text_color(theme::text()).child(installed_line))
                .children(available_line.map(|line| {
                    div()
                        .text_color(if update {
                            theme::accent()
                        } else {
                            theme::text_muted()
                        })
                        .child(line)
                }))
                .into_any_element(),
        };

        let buttons = tool_actions(item, job).into_iter().map(|action| {
            let (base, button) = match action {
                ToolAction::Install => ("tool-action-install", Button::new(("tool-action-install", position)).label("Install").primary()),
                ToolAction::Update => ("tool-action-update", Button::new(("tool-action-update", position)).label("Update").primary()),
                ToolAction::Retry => ("tool-action-retry", Button::new(("tool-action-retry", position)).label("Retry").primary()),
                ToolAction::Uninstall => ("tool-action-uninstall", Button::new(("tool-action-uninstall", position)).label("Uninstall")),
                // A free-standing tool never offers these (they belong to add-ons).
                ToolAction::InstallDriverFirst | ToolAction::UpdateDriverFirst => {
                    ("tool-action-install", Button::new(("tool-action-install", position)).label("Install").primary())
                }
                ToolAction::Cancel => (
                    "tool-action-cancel",
                    Button::new(("tool-action-cancel", position))
                        .icon(Icon::default().path("icons/close.svg").size(px(12.0)))
                        .ghost(),
                ),
            };
            let id = tool_id.clone();
            button
                .debug_selector(selector_of(base))
                .compact()
                .on_click(cx.listener(move |this, _, _, cx| this.run_tool_action_of(&id, action, cx)))
        });

        // An install that failed, else the last uninstall that did.
        let failure = self.driver_ui.tools.row_failure(&item.id);
        let problem = item
            .problem
            .as_ref()
            .filter(|_| !matches!(item.state, ToolCatalogState::Unavailable { .. }));
        let origin = item.entry.as_ref().and_then(|entry| {
            match (entry.license.as_deref(), entry.source_repo.as_deref()) {
                (None, None) => None,
                (Some(license), None) => Some(format!("Licence {license}")),
                (None, Some(source)) => Some(format!("Source {source}")),
                (Some(license), Some(source)) => Some(format!("Licence {license} \u{00B7} Source {source}")),
            }
        });

        div()
            .debug_selector(selector_of("tool-row"))
            .flex()
            .flex_col()
            .gap(px(8.0))
            .p(px(12.0))
            .rounded(px(6.0))
            .border_1()
            .border_color(theme::border())
            .child(
                div()
                    .flex()
                    .items_center()
                    .gap(px(12.0))
                    .child(
                        div()
                            .flex_1()
                            .min_w_0()
                            .flex()
                            .flex_col()
                            .child(
                                div()
                                    .truncate()
                                    .text_size(px(13.0))
                                    .text_color(theme::text())
                                    .child(item.display_name.clone()),
                            )
                            .child(
                                div()
                                    .debug_selector(selector_of("tool-description"))
                                    .truncate()
                                    .text_size(px(11.0))
                                    .text_color(theme::text_muted())
                                    .child(tool_description(item).to_string()),
                            ),
                    )
                    .child(middle)
                    .child(
                        div()
                            .w(px(110.0))
                            .flex_shrink_0()
                            .flex()
                            .overflow_hidden()
                            .children(badge.as_ref().map(|badge| {
                                badge_element(badge).debug_selector(selector_of("tool-badge"))
                            })),
                    )
                    .child(div().flex().flex_shrink_0().gap(px(6.0)).children(buttons)),
            )
            .children(failure.map(|failure| {
                div()
                    .debug_selector(selector_of("tool-error"))
                    .text_size(px(11.0))
                    .text_color(theme::danger())
                    .child(format!("{}: {}", failure.code, failure.message))
            }))
            .children(problem.map(|problem| {
                div()
                    .debug_selector(selector_of("tool-problem"))
                    .text_size(px(11.0))
                    .text_color(theme::warning())
                    .child(problem.clone())
            }))
            .children(origin.map(|origin| {
                div()
                    .debug_selector(selector_of("tool-origin"))
                    .text_size(px(11.0))
                    .text_color(theme::text_muted())
                    .child(origin)
            }))
            .children(
                (item.kind == ToolKind::McpServer && item.installed.is_some())
                    .then(|| self.render_tool_install_line(item, &tool_id, cx)),
            )
            .child(
                div()
                    .debug_selector(selector_of("tool-note"))
                    .text_size(px(12.0))
                    .text_color(theme::text_muted())
                    .child(tool_note(item.kind)),
            )
            .into_any_element()
    }

    /// The stable path (selectable) and the two copy buttons, for the tool an MCP client is
    /// configured with.
    fn render_tool_install_line(
        &self,
        item: &ToolCatalogItem,
        tool_id: &gpui::SharedString,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        div()
            .flex()
            .items_center()
            .gap(px(8.0))
            .child(
                div()
                    .flex_shrink_0()
                    .text_size(px(12.0))
                    .text_color(theme::text_muted())
                    .child("Path"),
            )
            .child(
                div()
                    .debug_selector({
                        let id = tool_id.clone();
                        move || format!("tool-path:{id}")
                    })
                    .flex_1()
                    .min_w_0()
                    .text_size(px(12.0))
                    .child(
                        TextView::markdown(
                            "tool-path-text",
                            code_span(&item.path.display().to_string()),
                        )
                        .selectable(true),
                    ),
            )
            .child(
                Button::new("tool-copy-config")
                    .debug_selector({
                        let id = tool_id.clone();
                        move || format!("tool-copy-config:{id}")
                    })
                    .label("Copy client configuration")
                    .compact()
                    .on_click(
                        cx.listener(|this, _, window, cx| this.copy_tool_client_config(window, cx)),
                    ),
            )
            .child(
                Button::new("tool-copy-command")
                    .debug_selector({
                        let id = tool_id.clone();
                        move || format!("tool-copy-command:{id}")
                    })
                    .label("Copy Claude Code command")
                    .compact()
                    .on_click(
                        cx.listener(|this, _, window, cx| this.copy_tool_claude_command(window, cx)),
                    ),
            )
            .into_any_element()
    }
}

impl AppRoot {
    /// The MongoDB Shell add-on, as one fixed-height row of the driver list right under the
    /// MongoDB driver: version, size, state, progress, Install / Update / Retry / cancel, and
    /// Remove in its menu. While its driver is missing or too old the row says what it needs and
    /// the button installs (or updates) the driver first.
    pub(super) fn render_addon_row(
        &self,
        position: usize,
        root: &gpui::WeakEntity<Self>,
    ) -> Option<AnyElement> {
        let item = self.driver_ui.tools.shell_item()?;
        let tools = &self.driver_ui.tools;
        let job = tools.job_of(&item.id);
        let need = self.addon_need(item);
        let installed = item.installed.is_some();
        let state_column = self.driver_ui.list_width >= LIST_MIN_WIDTH_FOR_STATE_COLUMN;
        let size = item
            .entry
            .as_ref()
            .and_then(|entry| entry.artifact_for_target(&crate::drivers::current_target()))
            .map(|artifact| artifact.size_bytes)
            .or_else(|| item.installed.as_ref().map(|record| record.size_bytes));

        let badge = match (&need, &item.state) {
            (Some(need), _) if installed => Some(Badge::Unavailable(format!(
                "Needs {} \u{2265} {}",
                need.driver_name, need.requirement.min_version
            ))),
            (_, ToolCatalogState::Installed { .. }) => Some(Badge::Installed),
            (_, ToolCatalogState::UpdateAvailable { .. }) => Some(Badge::Update),
            (_, ToolCatalogState::Unavailable { reason }) => Some(Badge::Unavailable(reason.clone())),
            (_, ToolCatalogState::Available) => None,
        };
        let failure = tools.row_failure(&item.id);
        let sub = match (failure, &need) {
            (Some(failure), _) => div()
                .debug_selector(|| "addon-error".into())
                .truncate()
                .text_size(px(11.0))
                .text_color(theme::danger())
                .child(format!("{}: {}", failure.code, failure.message)),
            (None, _) => div()
                .truncate()
                .text_size(px(11.0))
                .text_color(theme::text_muted())
                .child("Add-on of the MongoDB driver".to_string()),
        };

        let middle = match job.map(|job| &job.state) {
            Some(state) if state.is_running() => {
                let total = size.unwrap_or(0);
                let (done, total) = match state {
                    InstallState::Downloading { bytes, total } => (*bytes, *total),
                    _ => (total, total),
                };
                let step = if job.is_some_and(ToolJob::cancelling) {
                    "Cancelling\u{2026}"
                } else {
                    state.step_label()
                };
                div()
                    .debug_selector(|| "addon-progress".into())
                    .flex_1()
                    .min_w_0()
                    .flex()
                    .flex_col()
                    .gap(px(3.0))
                    .child(Progress::new(("addon-bar", position)).value(state.fraction() * 100.0))
                    .child(
                        div()
                            .truncate()
                            .text_size(px(11.0))
                            .text_color(theme::text_muted())
                            .child(format!(
                                "{}% \u{00B7} {} \u{00B7} {step}",
                                (state.fraction() * 100.0).round() as u32,
                                format_size_pair(done, total)
                            )),
                    )
                    .into_any_element()
            }
            _ => {
                let (installed_text, available) = match (&item.installed, &item.entry) {
                    (Some(record), Some(entry)) if record.version != entry.version => {
                        (record.version.clone(), Some(entry.version.clone()))
                    }
                    (Some(record), _) => (record.version.clone(), None),
                    (None, Some(entry)) => ("\u{2014}".to_string(), Some(entry.version.clone())),
                    (None, None) => ("\u{2014}".to_string(), None),
                };
                div()
                    .flex_1()
                    .min_w_0()
                    .flex()
                    .items_center()
                    .gap(px(8.0))
                    .child(
                        div()
                            .debug_selector(|| "addon-version".into())
                            .w(px(112.0))
                            .flex_shrink_0()
                            .whitespace_nowrap()
                            .overflow_hidden()
                            .text_size(px(12.0))
                            .text_color(theme::text())
                            .child(installed_text)
                            .children(available.map(|target| {
                                div().text_color(theme::accent()).child(format!("\u{2192} {target}"))
                            })),
                    )
                    .child(
                        div()
                            .debug_selector(|| "addon-size".into())
                            .w(px(52.0))
                            .flex_shrink_0()
                            .text_size(px(12.0))
                            .text_color(theme::text_muted())
                            .child(size.map_or(String::new(), format_size)),
                    )
                    .when(state_column && need.is_none(), |this| {
                        this.child(
                            div()
                                .w(px(100.0))
                                .flex_shrink_0()
                                .flex()
                                .overflow_hidden()
                                .children(badge.as_ref().map(|badge| {
                                    badge_element(badge).debug_selector(|| "addon-badge".into())
                                })),
                        )
                    })
                    // What it needs from its driver, in the room the chips take on a driver row.
                    .children(need.as_ref().map(|need| {
                        let full = need.reason();
                        div()
                            .id("addon-reason")
                            .debug_selector(|| "addon-reason".into())
                            .flex_1()
                            .min_w_0()
                            .truncate()
                            .text_size(px(11.0))
                            .text_color(theme::warning())
                            // The button beside it can leave little room: the whole sentence is the tooltip.
                            .tooltip({
                                let full = full.clone();
                                move |window, cx| {
                                    gpui_component::tooltip::Tooltip::new(full.clone()).build(window, cx)
                                }
                            })
                            .child(need.short_reason())
                    }))
                    .into_any_element()
            }
        };

        let actions = addon_actions(item, job, need.as_ref());
        let primary = actions.iter().copied().find(|action| *action != ToolAction::Uninstall);
        let action_width = match primary {
            Some(ToolAction::InstallDriverFirst | ToolAction::UpdateDriverFirst) => {
                ADDON_DRIVER_ACTION_WIDTH
            }
            _ => ACTION_WIDTH,
        };
        let action = match primary {
            Some(action) => {
                let button = Button::new(("addon-action", position)).compact();
                let button = match action {
                    ToolAction::Install => button.label("Install").primary(),
                    ToolAction::Update => button.label("Update").primary(),
                    ToolAction::Retry => button.label("Retry").primary(),
                    // Short, so the reason beside it fits; the driver is named in the reason.
                    ToolAction::InstallDriverFirst => button.label("Install driver first").primary(),
                    ToolAction::UpdateDriverFirst => button.label("Update driver first").primary(),
                    ToolAction::Cancel => button
                        .icon(Icon::default().path("icons/close.svg").size(px(12.0)))
                        .ghost(),
                    ToolAction::Uninstall => button.label("Remove"),
                };
                div()
                    .debug_selector(|| "addon-action".into())
                    .w(px(action_width))
                    .flex_shrink_0()
                    .flex()
                    .justify_end()
                    .child(button.w(px(action_width)))
                    .into_any_element()
            }
            None => div().w(px(ACTION_WIDTH)).flex_shrink_0().into_any_element(),
        };
        let can_remove = installed && actions.contains(&ToolAction::Uninstall);
        let more = if installed {
            let root = root.clone();
            div()
                .w(px(MORE_WIDTH))
                .flex_shrink_0()
                .child(
                    Button::new(("addon-more", position))
                        .debug_selector(|| "addon-more".into())
                        .label("\u{22EF}")
                        .compact()
                        .ghost()
                        .w(px(MORE_WIDTH))
                        .dropdown_menu(move |menu, _, _| {
                            let root = root.clone();
                            menu.item(
                                PopupMenuItem::new("Remove")
                                    .disabled(!can_remove)
                                    .on_click(move |_, _, app| {
                                        let _ = root.update(app, |this, cx| {
                                            this.run_tool_action_of(
                                                MONGO_SHELL_TOOL_ID,
                                                ToolAction::Uninstall,
                                                cx,
                                            )
                                        });
                                    }),
                            )
                        }),
                )
                .into_any_element()
        } else {
            div().w(px(MORE_WIDTH)).flex_shrink_0().into_any_element()
        };
        Some(
            div()
                .debug_selector(|| "addon-row".into())
                .w_full()
                .h(px(ROW_HEIGHT))
                .px(px(ROW_PAD))
                .flex()
                .items_center()
                .gap(px(8.0))
                .border_b_1()
                .border_color(theme::border())
                .bg(theme::surface())
                // Under the MongoDB row, indented past its logo.
                .child(
                    div()
                        .w(px(28.0))
                        .flex_shrink_0()
                        .flex()
                        .justify_end()
                        .text_color(theme::text_faint())
                        .child("\u{21B3}"),
                )
                .child(
                    div()
                        .w(px(140.0))
                        .flex_shrink_0()
                        .flex()
                        .flex_col()
                        .child(
                            div()
                                .truncate()
                                .text_size(px(13.0))
                                .text_color(theme::text())
                                .child(item.display_name.clone()),
                        )
                        .child(sub),
                )
                .child(middle)
                .child(
                    div()
                        .flex_shrink_0()
                        .flex()
                        .items_center()
                        .gap(px(ACTION_GAP))
                        .child(action)
                        .child(more),
                )
                .into_any_element(),
        )
    }
}
