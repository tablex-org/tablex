//! The "<Name> driver is not installed" dialog opened when a connect needs a
//! driver that is in the catalog but not installed.

use engine_connect::records::RuntimeUri;
use super::*;
use std::rc::Rc;
use gpui_component::progress::Progress;

#[derive(Clone, Debug, PartialEq, Eq)]
pub(in crate::app) enum PromptPhase {
    /// The catalog that says whether (and from where) the driver can be downloaded is being
    /// loaded: nothing to ask yet.
    LoadingCatalog,
    /// The catalog could not be loaded (offline, refused): Retry or the Driver Manager.
    CatalogFailed(String),
    /// Waiting for the user's choice.
    Asking,
    Installing,
    Failed(String),
    /// Installed; the held connect attempt is running.
    Connecting,
}

/// A step of the New Connection dialog that waits for a driver.
pub(in crate::app) type DialogStep = Rc<dyn Fn(&mut Window, &mut gpui::App)>;

/// What the prompt resumes once the driver is installed.
pub(in crate::app) enum PromptResume {
    /// A connect attempt parked until its driver is installed (or the user gives up). The
    /// connection stays `Connecting` meanwhile, so the runs queued on it (`pending_runs`) resume
    /// with it or fail with it.
    Connect {
        saved: SavedConnection,
        uri_override: Option<RuntimeUri>,
        attempt: u64,
    },
    /// A step of the New Connection dialog (Next, Test, Save and connect): runs again once the
    /// driver is installed; Cancel leaves the dialog as it was.
    Action(DialogStep),
}

pub(in crate::app) struct MissingDriverPrompt {
    pub(in crate::app) driver_id: String,
    driver_name: String,
    /// `None` until the catalog is loaded.
    entry: Option<DriverManifestEntry>,
    resume: PromptResume,
    /// A catalog fetch was started (or found running) for this prompt: a catalog that is still
    /// unusable after it is a failure, not a reason to fetch again.
    catalog_requested: bool,
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
            entry: Some(entry),
            resume: PromptResume::Connect { saved, uri_override, attempt },
            catalog_requested: true,
            phase,
        }
    }

    /// The prompt of a driver whose catalog entry is not known (yet).
    fn without_entry(kind: ConnectionKind, resume: PromptResume) -> Self {
        Self {
            driver_id: kind.driver_id().to_string(),
            driver_name: crate::connection_dialog::kind_label(kind).to_string(),
            entry: None,
            resume,
            catalog_requested: false,
            phase: PromptPhase::LoadingCatalog,
        }
    }

    /// The connection a held connect attempt is for.
    pub(in crate::app) fn connection(&self) -> Option<String> {
        match &self.resume {
            PromptResume::Connect { saved, .. } => Some(saved.id.to_string()),
            PromptResume::Action(_) => None,
        }
    }

    /// Downloaded → Verifying signature → Loading driver → Connecting.
    pub(in crate::app) fn steps(&self, job: Option<&InstallState>) -> [StepMark; 4] {
        use StepMark::{Done, Now, Todo};
        match &self.phase {
            PromptPhase::LoadingCatalog
            | PromptPhase::CatalogFailed(_)
            | PromptPhase::Asking
            | PromptPhase::Failed(_) => [Todo; 4],
            PromptPhase::Connecting => [Done, Done, Done, Now],
            PromptPhase::Installing => match job {
                Some(InstallState::Verifying(_)) => [Done, Now, Todo, Todo],
                Some(InstallState::Loading) => [Done, Done, Now, Todo],
                _ => [Now, Todo, Todo, Todo],
            },
        }
    }
}

/// What the catalog says about a driver that is not installed.
enum DriverOffer {
    /// The catalog is not loaded (or not trusted): nothing is known.
    Unknown,
    /// The catalog does not offer an installable build for this system.
    NotOffered,
    Offered(String, Box<DriverManifestEntry>),
}

/// Where the driver is downloaded from, for the dialog text.
fn source_label(url: &str) -> String {
    match url::Url::parse(url) {
        Ok(url) if url.scheme() == "file" => t!("driver_prompt.source_local").into_owned(),
        Ok(url) => url
            .host_str()
            .map_or_else(|| t!("driver_prompt.source_host").into_owned(), str::to_string),
        Err(_) => t!("driver_prompt.source_host").into_owned(),
    }
}

/// What the dialog shows, snapshotted from the app.
struct PromptView {
    title: String,
    body: String,
    phase: PromptPhase,
    steps: [StepMark; 4],
    step_labels: [String; 4],
    /// How many of `steps` are shown (a connect has a last "Connecting" step, an action not).
    step_count: usize,
    fraction: f32,
    /// The primary button's label while the user is asked.
    install_label: String,
}

#[derive(Clone, Copy)]
enum PromptAction {
    Install,
    Cancel,
    OpenManager,
}

impl AppRoot {
    /// Parks the connect attempt and opens the dialog when `saved`'s driver is not installed.
    /// `false`: connect normally.
    pub(in crate::app) fn prompt_missing_driver(
        &mut self,
        saved: &SavedConnection,
        uri_override: &Option<RuntimeUri>,
        attempt: u64,
        cx: &mut Context<Self>,
    ) -> bool {
        let resume = PromptResume::Connect {
            saved: saved.clone(),
            uri_override: uri_override.clone(),
            attempt,
        };
        self.open_missing_driver_prompt(saved.kind, resume, cx)
    }

    /// The same dialog for a step of the New Connection dialog: when `kind`'s driver is not
    /// installed it opens, and `run` runs once the driver is installed. `false`: nothing is
    /// missing (or nothing can be done about it), run `run` now.
    pub(in crate::app) fn prompt_driver_for_action(
        &mut self,
        kind: ConnectionKind,
        run: DialogStep,
        cx: &mut Context<Self>,
    ) -> bool {
        self.open_missing_driver_prompt(kind, PromptResume::Action(run), cx)
    }

    fn open_missing_driver_prompt(
        &mut self,
        kind: ConnectionKind,
        resume: PromptResume,
        cx: &mut Context<Self>,
    ) -> bool {
        let id = kind.driver_id();
        if self.scenario_ui.service.is_some()
            || self.driver_ui.prompt.is_some()
            || self.driver_runtime.window.is_none()
            || self.driver_ui.has_driver(id)
        {
            return false;
        }
        let prompt = match self.driver_offer(id) {
            DriverOffer::NotOffered => return false,
            DriverOffer::Unknown => MissingDriverPrompt::without_entry(kind, resume),
            DriverOffer::Offered(name, entry) => {
                let mut prompt = MissingDriverPrompt::without_entry(kind, resume);
                prompt.driver_name = name;
                prompt.entry = Some(*entry);
                prompt.catalog_requested = true;
                prompt.phase = self.asking_phase(id);
                prompt
            }
        };
        let loading = prompt.phase == PromptPhase::LoadingCatalog;
        self.driver_ui.prompt = Some(prompt);
        self.open_missing_driver_dialog(cx);
        if loading {
            self.on_driver_catalog_settled(cx);
        }
        true
    }

    /// Asking, or Installing when the driver is on its way already.
    fn asking_phase(&self, id: &str) -> PromptPhase {
        let installing = self.driver_ui.queue.job(id).is_some_and(|job| job.state.is_active());
        if installing { PromptPhase::Installing } else { PromptPhase::Asking }
    }

    fn driver_offer(&self, id: &str) -> DriverOffer {
        let ui = &self.driver_ui;
        if !ui.installed_loaded || !ui.catalog_is_current() || ui.catalog.is_empty() {
            return DriverOffer::Unknown;
        }
        ui.model
            .row(id)
            .filter(|row| row.status == RowStatus::Available)
            .and_then(|row| Some(DriverOffer::Offered(row.name.clone(), Box::new(row.entry.clone()?))))
            .unwrap_or(DriverOffer::NotOffered)
    }

    /// The catalog was (re)read, or a read of it failed: a prompt that waits for it moves on.
    pub(in crate::app) fn on_driver_catalog_settled(&mut self, cx: &mut Context<Self>) {
        let Some(prompt) = self
            .driver_ui
            .prompt
            .as_ref()
            .filter(|prompt| prompt.phase == PromptPhase::LoadingCatalog)
        else {
            return;
        };
        let id = prompt.driver_id.clone();
        if self.driver_ui.has_driver(&id) {
            return self.resume_without_install(cx);
        }
        match self.driver_offer(&id) {
            DriverOffer::Offered(name, entry) => {
                let phase = self.asking_phase(&id);
                if let Some(prompt) = self.driver_ui.prompt.as_mut() {
                    prompt.driver_name = name;
                    prompt.entry = Some(*entry);
                    prompt.phase = phase;
                }
            }
            // The catalog is there and has no build of this driver for this system: whatever
            // the user asked for proceeds the usual way.
            DriverOffer::NotOffered => return self.resume_without_install(cx),
            DriverOffer::Unknown => {
                let fetching = self.driver_ui.catalog_task.is_some();
                let requested = self.driver_ui.prompt.as_ref().is_some_and(|prompt| prompt.catalog_requested);
                if fetching && !self.driver_ui.installed_loaded {
                    // The startup read of the store is what is awaited; its end settles us again.
                    return;
                }
                if fetching {
                    // A network read is running: its end settles us again.
                    self.mark_catalog_requested();
                } else if !requested {
                    self.mark_catalog_requested();
                    self.refresh_driver_catalog(cx);
                } else {
                    let message = self
                        .driver_ui
                        .catalog_error
                        .clone()
                        .unwrap_or_else(|| t!("driver_prompt.catalog_empty").into_owned());
                    if let Some(prompt) = self.driver_ui.prompt.as_mut() {
                        prompt.phase = PromptPhase::CatalogFailed(message);
                    }
                }
            }
        }
        cx.notify();
    }

    fn mark_catalog_requested(&mut self) {
        if let Some(prompt) = self.driver_ui.prompt.as_mut() {
            prompt.catalog_requested = true;
        }
    }

    /// The driver is there (or cannot be installed from here): the held connect or step goes on.
    fn resume_without_install(&mut self, cx: &mut Context<Self>) {
        let Some(prompt) = self.driver_ui.prompt.take() else {
            return;
        };
        self.with_driver_window(cx, |window, cx| window.close_dialog(cx));
        self.resume_prompt(prompt.resume, cx);
        cx.notify();
    }

    fn resume_prompt(&mut self, resume: PromptResume, cx: &mut Context<Self>) {
        match resume {
            PromptResume::Connect { saved, uri_override, attempt } => {
                self.run_driver_connect(saved, uri_override, attempt, cx)
            }
            PromptResume::Action(run) => self.with_driver_window(cx, move |window, cx| run(window, cx)),
        }
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
                let retry = matches!(view.phase, PromptPhase::Failed(_) | PromptPhase::CatalogFailed(_));
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
                                    .label(t!("driver_prompt.open_manager"))
                                    .ghost()
                                    .on_click(action(root.clone(), PromptAction::OpenManager)),
                            )
                            .child(
                                Button::new("driver-prompt-cancel")
                                    .debug_selector(|| "driver-prompt-cancel".into())
                                    .label(t!("driver_prompt.cancel"))
                                    .on_click(action(root.clone(), PromptAction::Cancel)),
                            )
                            .child(
                                Button::new("driver-prompt-install")
                                    .debug_selector(|| "driver-prompt-install".into())
                                    .label(if retry {
                                        t!("driver_prompt.retry").into_owned()
                                    } else {
                                        view.install_label.clone()
                                    })
                                    .primary()
                                    .disabled(
                                        installing
                                            || matches!(
                                                view.phase,
                                                PromptPhase::Connecting | PromptPhase::LoadingCatalog
                                            ),
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
        let name = prompt.driver_name.as_str();
        let connects = matches!(prompt.resume, PromptResume::Connect { .. });
        let body = match (&prompt.phase, &prompt.entry, &prompt.resume) {
            (PromptPhase::LoadingCatalog, ..) => t!("driver_prompt.loading_catalog").into_owned(),
            (PromptPhase::CatalogFailed(error), ..) => {
                t!("driver_prompt.catalog_failed", error = error).into_owned()
            }
            (_, entry, resume) => {
                let signer = match entry.as_ref().map(|entry| entry.team_id.as_str()) {
                    Some(team) if !team.is_empty() => t!("driver_prompt.signer_team", team = team).into_owned(),
                    _ => t!("driver_prompt.signer_unsigned").into_owned(),
                };
                let source = entry
                    .as_ref()
                    .and_then(|entry| entry.artifact_for_target(&crate::drivers::current_target()))
                    .map_or_else(|| t!("driver_prompt.source_host").into_owned(), |artifact| source_label(&artifact.url));
                match resume {
                    PromptResume::Connect { saved, .. } => t!(
                        "driver_prompt.body_connect",
                        name = name,
                        source = source,
                        size = size,
                        signer = signer,
                        connection = saved.name
                    )
                    .into_owned(),
                    PromptResume::Action(_) => t!(
                        "driver_prompt.body_continue",
                        name = name,
                        source = source,
                        size = size,
                        signer = signer
                    )
                    .into_owned(),
                }
            }
        };
        let steps = prompt.steps(job);
        Some(PromptView {
            title: t!("driver_prompt.title", name = name).into_owned(),
            body,
            phase: prompt.phase.clone(),
            steps,
            step_labels: [
                if matches!(steps[0], StepMark::Done) {
                    t!("driver_prompt.step_downloaded", size = size).into_owned()
                } else {
                    t!("driver_prompt.step_downloading").into_owned()
                },
                t!("driver_prompt.step_verifying").into_owned(),
                t!("driver_prompt.step_loading").into_owned(),
                t!("driver_prompt.step_connecting").into_owned(),
            ],
            step_count: if connects { 4 } else { 3 },
            fraction: job.map_or(0.0, InstallState::fraction),
            install_label: if connects {
                t!("driver_prompt.install_and_connect").into_owned()
            } else {
                t!("driver_prompt.install").into_owned()
            },
        })
    }

    fn missing_driver_action(&mut self, action: PromptAction, cx: &mut Context<Self>) {
        match action {
            PromptAction::Install => {
                let Some(prompt) = self.driver_ui.prompt.as_mut() else {
                    return;
                };
                if matches!(prompt.phase, PromptPhase::CatalogFailed(_)) {
                    // Retry: read the catalog again.
                    prompt.phase = PromptPhase::LoadingCatalog;
                    prompt.catalog_requested = false;
                    self.on_driver_catalog_settled(cx);
                    cx.notify();
                    return;
                }
                let driver_id = prompt.driver_id.clone();
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
                        Enqueued::Stale => PromptPhase::Failed(t!("driver_prompt.error_stale").into_owned()),
                    },
                    None => PromptPhase::Failed(t!("driver_prompt.error_gone").into_owned()),
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

    /// The user gave up: stop a running install of this driver and let a held connect fail the
    /// usual way ("not installed"), which also fails the runs queued on it. A step of the New
    /// Connection dialog is simply not taken: the dialog is where it was.
    fn abandon_missing_driver(&mut self, cx: &mut Context<Self>) {
        let Some(prompt) = self.driver_ui.prompt.take() else {
            return;
        };
        self.driver_ui.queue.cancel(&prompt.driver_id);
        if let PromptResume::Connect { saved, uri_override, attempt } = prompt.resume {
            self.run_driver_connect(saved, uri_override, attempt, cx);
        }
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
                let PromptResume::Connect { saved, uri_override, attempt } = &prompt.resume else {
                    // A step of the New Connection dialog: the dialog closes and the step runs.
                    return self.resume_without_install(cx);
                };
                let (saved, uri, attempt) = (saved.clone(), uri_override.clone(), *attempt);
                prompt.phase = PromptPhase::Connecting;
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
            prompt.phase == PromptPhase::Connecting
                && prompt.connection().as_deref() == Some(connection)
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
                view.steps.iter().zip(&view.step_labels).take(view.step_count).map(|(step, label)| {
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
    fn the_dialog_speaks_vietnamese_when_the_language_is() {
        use crate::i18n::{self, Locale};
        i18n::set(Locale::Vi);
        let title = t!("driver_prompt.title", name = "PostgreSQL").into_owned();
        let loading = t!("driver_prompt.loading_catalog").into_owned();
        let retry = t!("driver_prompt.retry").into_owned();
        i18n::set(Locale::En);
        assert_eq!(title, "Driver PostgreSQL chưa được cài");
        assert_eq!(loading, "Đang tải danh sách driver…");
        assert_eq!(retry, "Thử lại");
        assert_eq!(t!("driver_prompt.title", name = "PostgreSQL"), "PostgreSQL driver is not installed");
    }

    #[test]
    fn the_download_source_is_named_for_the_dialog() {
        assert_eq!(source_label("https://github.com/o/r/releases/x.dylib"), "github.com");
        assert_eq!(source_label("file:///tmp/x.dylib"), "a local file (developer mode)");
        assert_eq!(source_label("nonsense"), "the driver host");
    }
}
