//! The "<Name> driver is not installed" dialog opened when a connect needs a
//! driver that is in the catalog but not installed.

use engine_connect::records::RuntimeUri;
use super::*;
use gpui_component::progress::Progress;

#[derive(Clone, Debug, PartialEq, Eq)]
pub(in crate::app) enum PromptPhase {
    /// Waiting for the user's choice.
    Asking,
    Installing,
    Failed(String),
    /// Installed; the held connect attempt is running.
    Connecting,
}

/// A connect attempt parked until its driver is installed (or the user gives
/// up). The connection stays `Connecting` meanwhile, so the runs queued on it
/// (`pending_runs`) resume with it or fail with it.
pub(in crate::app) struct MissingDriverPrompt {
    pub(in crate::app) driver_id: String,
    driver_name: String,
    entry: DriverManifestEntry,
    connection_name: String,
    saved: SavedConnection,
    uri_override: Option<RuntimeUri>,
    attempt: u64,
    pub(in crate::app) phase: PromptPhase,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(in crate::app) enum StepMark {
    Todo,
    Now,
    Done,
}

impl MissingDriverPrompt {
    pub(in crate::app) fn new(
        driver_name: String,
        entry: DriverManifestEntry,
        saved: SavedConnection,
        uri_override: Option<RuntimeUri>,
        attempt: u64,
        phase: PromptPhase,
    ) -> Self {
        Self {
            driver_id: entry.id.clone(),
            driver_name,
            entry,
            connection_name: saved.name.clone(),
            saved,
            uri_override,
            attempt,
            phase,
        }
    }

    pub(in crate::app) fn connection(&self) -> String {
        self.saved.id.to_string()
    }

    /// Downloaded → Verifying signature → Loading driver → Connecting.
    pub(in crate::app) fn steps(&self, job: Option<&InstallState>) -> [StepMark; 4] {
        use StepMark::{Done, Now, Todo};
        match &self.phase {
            PromptPhase::Asking | PromptPhase::Failed(_) => [Todo; 4],
            PromptPhase::Connecting => [Done, Done, Done, Now],
            PromptPhase::Installing => match job {
                Some(InstallState::Verifying(_)) => [Done, Now, Todo, Todo],
                Some(InstallState::Loading) => [Done, Done, Now, Todo],
                _ => [Now, Todo, Todo, Todo],
            },
        }
    }
}

/// Where the driver is downloaded from, for the dialog text.
fn source_label(url: &str) -> String {
    match url::Url::parse(url) {
        Ok(url) if url.scheme() == "file" => "a local file (developer mode)".into(),
        Ok(url) => url.host_str().unwrap_or("the driver host").to_string(),
        Err(_) => "the driver host".into(),
    }
}

/// What the dialog shows, snapshotted from the app.
struct PromptView {
    title: String,
    body: String,
    phase: PromptPhase,
    steps: [StepMark; 4],
    step_labels: [String; 4],
    fraction: f32,
}

#[derive(Clone, Copy)]
enum PromptAction {
    Install,
    Cancel,
    OpenManager,
}

impl AppRoot {
    /// Parks the connect attempt and opens the dialog when `saved`'s driver is
    /// downloadable but not installed. `false`: connect normally.
    pub(in crate::app) fn prompt_missing_driver(
        &mut self,
        saved: &SavedConnection,
        uri_override: &Option<RuntimeUri>,
        attempt: u64,
        cx: &mut Context<Self>,
    ) -> bool {
        let id = saved.kind.driver_id();
        if self.scenario_ui.service.is_some()
            || self.driver_ui.prompt.is_some()
            || self.driver_runtime.window.is_none()
            || !self.driver_ui.installed_loaded
            || self.driver_ui.has_driver(id)
            || !self.driver_ui.is_downloadable(id)
        {
            return false;
        }
        let Some(row) = self.driver_ui.model.row(id) else {
            return false;
        };
        let Some(entry) = row.entry.clone() else {
            return false;
        };
        let installing = self
            .driver_ui
            .queue
            .job(id)
            .is_some_and(|job| job.state.is_active());
        self.driver_ui.prompt = Some(MissingDriverPrompt::new(
            row.name.clone(),
            entry,
            saved.clone(),
            uri_override.clone(),
            attempt,
            if installing { PromptPhase::Installing } else { PromptPhase::Asking },
        ));
        self.open_missing_driver_dialog(cx);
        true
    }

    pub(in crate::app) fn open_missing_driver_dialog(&mut self, cx: &mut Context<Self>) {
        let root = cx.entity().downgrade();
        self.with_driver_window(cx, move |window, cx| {
            window.open_dialog(cx, move |dialog, _, cx| {
                let view = root
                    .upgrade()
                    .and_then(|root| root.read(cx).prompt_view());
                let Some(view) = view else {
                    return dialog.w(px(520.0));
                };
                let installing = view.phase == PromptPhase::Installing;
                let failed = matches!(view.phase, PromptPhase::Failed(_));
                let action = |root: gpui::WeakEntity<AppRoot>, action: PromptAction| {
                    move |_: &gpui::ClickEvent, window: &mut Window, cx: &mut gpui::App| {
                        let closes = !matches!(action, PromptAction::Install);
                        let _ = root.update(cx, |this, cx| this.missing_driver_action(action, cx));
                        if closes {
                            window.close_dialog(cx);
                        }
                    }
                };
                let dismissed = root.clone();
                dialog
                    .w(px(520.0))
                    .title(view.title.clone())
                    .on_close(move |_, _, cx| {
                        let _ = dismissed.update(cx, |this, cx| {
                            this.missing_driver_action(PromptAction::Cancel, cx)
                        });
                    })
                    .child(prompt_body(&view))
                    .footer(
                        gpui_component::dialog::DialogFooter::new()
                            .child(
                                Button::new("driver-prompt-open-manager")
                                    .debug_selector(|| "driver-prompt-open-manager".into())
                                    .label("Open Driver Manager")
                                    .ghost()
                                    .on_click(action(root.clone(), PromptAction::OpenManager)),
                            )
                            .child(
                                Button::new("driver-prompt-cancel")
                                    .debug_selector(|| "driver-prompt-cancel".into())
                                    .label("Cancel")
                                    .on_click(action(root.clone(), PromptAction::Cancel)),
                            )
                            .child(
                                Button::new("driver-prompt-install")
                                    .debug_selector(|| "driver-prompt-install".into())
                                    .label(if failed { "Retry" } else { "Install and connect" })
                                    .primary()
                                    .disabled(
                                        installing || view.phase == PromptPhase::Connecting,
                                    )
                                    .on_click(action(root.clone(), PromptAction::Install)),
                            ),
                    )
            });
        });
    }

    fn prompt_view(&self) -> Option<PromptView> {
        let prompt = self.driver_ui.prompt.as_ref()?;
        let job = self.driver_ui.queue.job(&prompt.driver_id).map(|job| &job.state);
        let size = self
            .driver_ui
            .model
            .row(&prompt.driver_id)
            .and_then(|row| row.size_bytes)
            .map_or_else(String::new, format_size);
        let signer = if prompt.entry.team_id.is_empty() {
            "an unsigned developer build".to_string()
        } else {
            format!("signed by Team {}", prompt.entry.team_id)
        };
        let source = prompt
            .entry
            .artifact_for_target(&crate::drivers::current_target())
            .map_or_else(|| "the driver host".into(), |artifact| source_label(&artifact.url));
        Some(PromptView {
            title: format!("{} driver is not installed", prompt.driver_name),
            body: format!(
                "{} connections open through a driver that is installed on demand. TableX downloads it from {source} ({size}, {signer}), then connects to {}.",
                prompt.driver_name, prompt.connection_name
            ),
            phase: prompt.phase.clone(),
            steps: prompt.steps(job),
            step_labels: [
                if matches!(prompt.steps(job)[0], StepMark::Done) {
                    format!("Downloaded {size}")
                } else {
                    "Downloading".into()
                },
                "Verifying signature".into(),
                "Loading driver".into(),
                "Connecting".into(),
            ],
            fraction: job.map_or(0.0, InstallState::fraction),
        })
    }

    fn missing_driver_action(&mut self, action: PromptAction, cx: &mut Context<Self>) {
        match action {
            PromptAction::Install => {
                let Some(driver_id) = self.driver_ui.prompt.as_ref().map(|prompt| prompt.driver_id.clone()) else {
                    return;
                };
                // The catalog's current entry (the one the prompt was opened with may be from a
                // catalog that is not trusted any more).
                let entry = self
                    .driver_ui
                    .model
                    .row(&driver_id)
                    .filter(|row| !matches!(row.status, RowStatus::Unavailable(_)))
                    .and_then(|row| row.entry.clone());
                let phase = match entry {
                    Some(entry) => match self.enqueue_driver_install(entry, JobKind::Install, cx) {
                        Enqueued::Queued | Enqueued::AlreadyActive => PromptPhase::Installing,
                        Enqueued::Stale => PromptPhase::Failed(
                            "The driver catalog changed. Open the Driver Manager and check for updates, then try again".into(),
                        ),
                    },
                    None => PromptPhase::Failed(
                        "The catalog no longer offers this driver for this system".into(),
                    ),
                };
                if let Some(prompt) = self.driver_ui.prompt.as_mut() {
                    prompt.phase = phase;
                }
            }
            PromptAction::Cancel => self.abandon_missing_driver(cx),
            PromptAction::OpenManager => {
                self.abandon_missing_driver(cx);
                self.open_driver_manager(cx);
            }
        }
        cx.notify();
    }

    /// The user gave up: stop a running install of this driver and let the
    /// held connect fail the usual way ("not installed"), which also fails
    /// the runs queued on it.
    fn abandon_missing_driver(&mut self, cx: &mut Context<Self>) {
        let Some(prompt) = self.driver_ui.prompt.take() else {
            return;
        };
        self.driver_ui.queue.cancel(&prompt.driver_id);
        self.run_driver_connect(prompt.saved, prompt.uri_override, prompt.attempt, cx);
    }

    /// An install finished; resume (or fail) the connect that waited for it.
    pub(in crate::app) fn on_prompt_install_finished(
        &mut self,
        id: &str,
        outcome: PromptInstall,
        cx: &mut Context<Self>,
    ) {
        let waiting = self
            .driver_ui
            .prompt
            .as_ref()
            .is_some_and(|prompt| prompt.driver_id == id && prompt.phase == PromptPhase::Installing);
        if !waiting {
            return;
        }
        match outcome {
            PromptInstall::Installed => {
                let Some(prompt) = self.driver_ui.prompt.as_mut() else {
                    return;
                };
                prompt.phase = PromptPhase::Connecting;
                let (saved, uri, attempt) =
                    (prompt.saved.clone(), prompt.uri_override.clone(), prompt.attempt);
                self.run_driver_connect(saved, uri, attempt, cx);
            }
            PromptInstall::Cancelled => {
                self.abandon_missing_driver(cx);
                self.with_driver_window(cx, |window, cx| window.close_dialog(cx));
            }
            PromptInstall::Failed(message) => {
                if let Some(prompt) = self.driver_ui.prompt.as_mut() {
                    prompt.phase = PromptPhase::Failed(message);
                }
            }
        }
    }

    /// A driver connect ended: the dialog's last step is over.
    pub(in crate::app) fn on_driver_connect_finished(&mut self, connection: &str, cx: &mut Context<Self>) {
        let done = self.driver_ui.prompt.as_ref().is_some_and(|prompt| {
            prompt.phase == PromptPhase::Connecting && prompt.connection() == connection
        });
        if done {
            self.driver_ui.prompt = None;
            self.with_driver_window(cx, |window, cx| window.close_dialog(cx));
        }
    }
}

fn prompt_body(view: &PromptView) -> AnyElement {
    let mark = |mark: StepMark| match mark {
        StepMark::Done => ("\u{2713}", theme::success()),
        StepMark::Now => ("\u{25CF}", theme::text()),
        StepMark::Todo => ("\u{25CB}", theme::text_muted()),
    };
    let running = matches!(view.phase, PromptPhase::Installing | PromptPhase::Connecting);
    div()
        .debug_selector(|| "driver-prompt-body".into())
        .flex()
        .flex_col()
        .gap(px(10.0))
        .child(
            div()
                .text_size(px(13.0))
                .text_color(theme::text_muted())
                .child(view.body.clone()),
        )
        .when(running, |this| {
            this.child(
                div()
                    .debug_selector(|| "driver-prompt-progress".into())
                    .child(Progress::new("driver-prompt-bar").value(view.fraction * 100.0)),
            )
            .child(div().flex().flex_wrap().gap(px(14.0)).text_size(px(12.0)).children(
                view.steps.iter().zip(&view.step_labels).map(|(step, label)| {
                    let (glyph, color) = mark(*step);
                    div().text_color(color).child(format!("{glyph} {label}"))
                }),
            ))
        })
        .when_some(
            match &view.phase {
                PromptPhase::Failed(message) => Some(message.clone()),
                _ => None,
            },
            |this, message| {
                this.child(
                    div()
                        .debug_selector(|| "driver-prompt-error".into())
                        .text_size(px(12.0))
                        .text_color(theme::danger())
                        .child(message),
                )
            },
        )
        .into_any_element()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn prompt(phase: PromptPhase) -> MissingDriverPrompt {
        MissingDriverPrompt::new(
            "PostgreSQL".into(),
            DriverManifestEntry { id: "postgresql".into(), ..Default::default() },
            SavedConnection {
                id: uuid::Uuid::new_v4(),
                name: "dev".into(),
                uri: "postgres://h/d".into(),
                kind: ConnectionKind::Postgres,
                save_password: false,
                is_pinned: false,
                disabled: false,
                ssh_tunnel: false,
            },
            None,
            1,
            phase,
        )
    }

    #[test]
    fn steps_follow_the_phase_and_the_install_state() {
        use StepMark::{Done, Now, Todo};
        assert_eq!(prompt(PromptPhase::Asking).steps(None), [Todo; 4]);
        let installing = prompt(PromptPhase::Installing);
        let download = InstallState::Downloading { bytes: 1, total: 2 };
        assert_eq!(installing.steps(Some(&download)), [Now, Todo, Todo, Todo]);
        let verifying = InstallState::Verifying(engine_driver_host::InstallStep::VerifyingSignature);
        assert_eq!(installing.steps(Some(&verifying)), [Done, Now, Todo, Todo]);
        assert_eq!(installing.steps(Some(&InstallState::Loading)), [Done, Done, Now, Todo]);
        assert_eq!(prompt(PromptPhase::Connecting).steps(None), [Done, Done, Done, Now]);
        assert_eq!(prompt(PromptPhase::Failed("x".into())).steps(None), [Todo; 4]);
    }

    #[test]
    fn the_download_source_is_named_for_the_dialog() {
        assert_eq!(source_label("https://github.com/o/r/releases/x.dylib"), "github.com");
        assert_eq!(source_label("file:///tmp/x.dylib"), "a local file (developer mode)");
        assert_eq!(source_label("nonsense"), "the driver host");
    }
}
