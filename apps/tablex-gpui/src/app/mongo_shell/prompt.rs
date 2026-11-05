//! "This is a MongoDB shell script. Install the MongoDB Shell plugin (N MB)?": whether the plugin
//! can run a script now, and the dialog that offers what is missing.

use super::super::driver_manager::{AddonNeed, ToolAction};
use super::*;
use engine_driver_host::{RequirementStatus, ToolCatalogState};

/// Whether the plugin can run a script now, from the catalog the Driver Manager holds (read off
/// the UI thread) and the jobs it runs. The file itself is verified when a run starts.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(in crate::app) enum PluginState {
    Ready,
    /// Its install is running (or its driver's, which it follows).
    Installing,
    /// Not installed; `bytes` is the download.
    Install { bytes: u64 },
    /// Needs its driver installed or updated first; `bytes` is both downloads.
    DriverFirst { need: AddonNeed, bytes: u64 },
    /// Cannot be had (no build for this system, no catalog, a driver the catalog cannot fix).
    Unavailable(String),
}

impl AppRoot {
    pub(in crate::app) fn shell_plugin_state(&self) -> PluginState {
        let ui = &self.driver_ui;
        let Some(item) = ui.tools.shell_item() else {
            return PluginState::Unavailable(if ui.installed_loaded {
                "the driver catalog does not list it (check the connection and \"Check for updates\" in the Driver Manager)".into()
            } else {
                "the driver catalog is still loading".into()
            });
        };
        let installing = ui.tools.job_of(MONGO_SHELL_TOOL_ID).is_some_and(|job| job.state.is_active())
            || ui.tools.after_driver.as_deref() == Some(MONGO_SHELL_TOOL_ID);
        if installing {
            return PluginState::Installing;
        }
        let need = self.addon_need(item);
        if item.installed.is_some() {
            // Installed: ready, unless its driver went missing or too old since.
            return match need {
                None => PluginState::Ready,
                Some(need) if need.fixable => PluginState::DriverFirst {
                    bytes: self.driver_download_bytes(&need),
                    need,
                },
                Some(need) => PluginState::Unavailable(need.reason()),
            };
        }
        let plugin_bytes = driver_manager::installable_entry(item)
            .and_then(|entry| entry.artifact_for_target(&crate::drivers::current_target()))
            .map(|artifact| artifact.size_bytes);
        let Some(plugin_bytes) = plugin_bytes else {
            return PluginState::Unavailable(match &item.state {
                ToolCatalogState::Unavailable { reason } => reason.to_lowercase(),
                _ => "the catalog offers no build of it for this system".into(),
            });
        };
        match need {
            None => PluginState::Install { bytes: plugin_bytes },
            Some(need) if need.fixable => PluginState::DriverFirst {
                bytes: plugin_bytes + self.driver_download_bytes(&need),
                need,
            },
            Some(need) => PluginState::Unavailable(need.reason()),
        }
    }

    fn driver_download_bytes(&self, need: &AddonNeed) -> u64 {
        self.driver_ui
            .model
            .row(&need.requirement.driver)
            .and_then(|row| row.size_bytes)
            .unwrap_or(0)
    }

    /// Asks whether to install what the script needs. The run was withdrawn: Cancel leaves the tab
    /// as it was; Install starts the install and the script follows it.
    pub(super) fn prompt_shell_install(
        &mut self,
        script: ShellScript,
        attempt: u64,
        state: PluginState,
        cx: &mut Context<Self>,
    ) {
        let (title, body, confirm, action) = match &state {
            PluginState::Install { bytes } => (
                "Install the MongoDB Shell plugin?".to_string(),
                format!(
                    "This is a MongoDB shell script. Install the MongoDB Shell plugin ({})?",
                    driver_manager::format_size(*bytes)
                ),
                "Install",
                Some(ToolAction::Install),
            ),
            PluginState::DriverFirst { need, bytes } => {
                let update = matches!(need.status, RequirementStatus::TooOld { .. });
                (
                    format!(
                        "{} the {} driver first?",
                        if update { "Update" } else { "Install" },
                        need.driver_name
                    ),
                    format!(
                        "This is a MongoDB shell script. The MongoDB Shell plugin {} ({}).",
                        lower_first(&need.reason()),
                        driver_manager::format_size(*bytes)
                    ),
                    if update { "Update driver and run" } else { "Install driver and run" },
                    Some(if update {
                        ToolAction::UpdateDriverFirst
                    } else {
                        ToolAction::InstallDriverFirst
                    }),
                )
            }
            PluginState::Unavailable(reason) => (
                "MongoDB Shell plugin is not available".to_string(),
                format!("This is a MongoDB shell script, but the MongoDB Shell plugin is not available: {reason}."),
                "Open Driver Manager",
                None,
            ),
            PluginState::Ready | PluginState::Installing => return,
        };
        let root = cx.entity().downgrade();
        let open_manager = action.is_none();
        self.with_driver_window(cx, move |window, cx| {
            window.open_dialog(cx, move |dialog, _, _| {
                let (root_ok, script) = (root.clone(), script.clone());
                dialog
                    .w(px(520.0))
                    .title(title.clone())
                    .child(
                        div()
                            .debug_selector(|| "shell-prompt-body".into())
                            .flex()
                            .flex_col()
                            .gap(px(8.0))
                            .text_size(px(13.0))
                            .child(body.clone())
                            .child(
                                div().text_color(theme::text_muted()).child(
                                    "The plugin runs scripts in a separate process and is downloaded from the driver catalog, verified, then installed from the Driver Manager.",
                                ),
                            ),
                    )
                    .footer(
                        gpui_component::dialog::DialogFooter::new()
                            .child(
                                div().debug_selector(|| "shell-prompt-cancel".into()).child(
                                    gpui_component::dialog::DialogClose::new()
                                        .trigger(|button| button.label("Cancel")),
                                ),
                            )
                            .child(
                                div().debug_selector(|| "shell-prompt-install".into()).child(
                                    gpui_component::dialog::DialogAction::new().child(
                                        Button::new("shell-prompt-install")
                                            .label(confirm)
                                            .primary(),
                                    ),
                                ),
                            ),
                    )
                    .on_ok(move |_, _, cx| {
                        let script = script.clone();
                        let _ = root_ok.update(cx, |this, cx| match action {
                            Some(action) => this.install_shell_plugin_for(script, attempt, action, cx),
                            None => {
                                debug_assert!(open_manager);
                                this.open_driver_manager(cx)
                            }
                        });
                        true
                    })
            });
        });
    }

    /// "Install": the tab waits (and shows the progress) and the script runs when it is in.
    fn install_shell_plugin_for(
        &mut self,
        script: ShellScript,
        attempt: u64,
        action: ToolAction,
        cx: &mut Context<Self>,
    ) {
        // Only the attempt that asked, and only while the tab still holds the same script.
        let tab_id = script.tab_id;
        let current = self.mongo_shell.attempts.get(&tab_id) == Some(&attempt)
            && self.workspace.tab(tab_id).is_some_and(|tab| {
                tab.query_mode.is_mongo_script() && (script.selection || tab.sql == script.script)
            });
        if !current {
            return;
        }
        self.wait_for_shell_plugin(script, attempt, cx);
        if !self.mongo_shell.waits.contains_key(&tab_id) {
            return;
        }
        self.run_tool_action_of(MONGO_SHELL_TOOL_ID, action, cx);
        // Nothing started (another install is running, the catalog changed): do not wait forever.
        let started = self.driver_ui.tools.job_of(MONGO_SHELL_TOOL_ID).is_some()
            || self.driver_ui.tools.after_driver.as_deref() == Some(MONGO_SHELL_TOOL_ID);
        if !started {
            self.on_addon_install_ended(
                MONGO_SHELL_TOOL_ID,
                Err(AddonInstallError::NotStarted),
                cx,
            );
        }
    }
}

fn lower_first(text: &str) -> String {
    let mut chars = text.chars();
    chars
        .next()
        .map(|first| first.to_lowercase().chain(chars).collect())
        .unwrap_or_default()
}
