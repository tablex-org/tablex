//! "A newer TableX exists": the check on launch (once a day, when the preference is on) and the
//! "Check for Updates" command, and the notice that reports them. The logic without UI is in
//! [`check`]; installing one (the signed manifest, download, swap) is in [`install`]. Nothing runs
//! besides the launch check and the command; nothing is sent but the requests for the latest release
//! and, when one is newer, its update manifest. Nothing is installed without a click.

pub(super) mod check;
pub(super) mod install;

use super::*;
use std::time::{Duration, SystemTime};
use check::{Mode, Outcome, Release, ReleaseFetcher};
use install::{Artifact, Bundle, Plan};
use gpui_component::notification::{Notification, NotificationType};
use gpui_component::ActiveTheme as _;

/// Marks the update notice, so a second one replaces the first instead of stacking.
struct UpdateNotice;

/// Opens a web page.
pub(in crate::app) type PageOpener = Box<dyn Fn(&str, &mut gpui::App)>;

/// Arranges that the bundle at the path, which an install has just put in place, is started once this
/// app has quit (however the quit comes about). Called once per run. Tests observe it.
pub(in crate::app) type Restarter = Box<dyn Fn(&mut gpui::App, &std::path::Path)>;

/// Quits the app the way Quit does (it may ask first). Tests count it instead of quitting.
pub(in crate::app) type Quitter = Box<dyn Fn(&mut gpui::App)>;

/// What the notice for a newer release offers.
#[derive(Clone)]
struct Offer {
    release: Release,
    /// A verified artifact and the bundle to replace: Install & Restart. Without: Download.
    install: Option<(Artifact, Bundle)>,
    /// A line under the title (why Install is not offered, why an install failed).
    note: Option<String>,
}

/// An install in flight.
struct InstallRun {
    progress: Arc<install::Progress>,
    cancel: Arc<std::sync::atomic::AtomicBool>,
}

pub(super) struct UpdateUi {
    pub(in crate::app) fetcher: Arc<dyn ReleaseFetcher>,
    /// The version of this build; tests stand in for other builds.
    pub(in crate::app) current: String,
    /// Debug builds never check on their own.
    pub(in crate::app) debug_build: bool,
    /// `<app data dir>/update-check.json`; none in a scenario (nothing is written there).
    pub(in crate::app) state_path: Option<std::path::PathBuf>,
    /// The check in flight; a newer request replaces it.
    pub(in crate::app) task: Option<gpui::Task<()>>,
    /// Opens a web page; tests observe it instead of starting a browser.
    pub(in crate::app) opener: PageOpener,
    /// Where the manifest and the zip come from.
    pub(in crate::app) source: Arc<dyn install::UpdateSource>,
    /// The keys that verify the update manifest: the ones that verify the driver catalog.
    pub(in crate::app) trust: Option<engine_driver_host::ManifestTrust>,
    /// The running executable (none in a scenario); an update replaces the `.app` it is in.
    pub(in crate::app) exe: Option<std::path::PathBuf>,
    /// The target the manifest's artifact is chosen by.
    pub(in crate::app) target: &'static str,
    pub(in crate::app) restart: Restarter,
    pub(in crate::app) quit: Quitter,
    /// The version that is installed and waits for a restart. This process runs from the previous
    /// bundle, which is moved aside: no further check or install until it has been restarted.
    pending_restart: Option<check::Version>,
    install: Option<InstallRun>,
    pub(in crate::app) install_task: Option<gpui::Task<()>>,
    /// Repaints the progress of the install.
    ticker: Option<gpui::Task<()>>,
    /// Every message shown, newest last (the notification keeps its text private).
    #[cfg(test)]
    pub(in crate::app) shown: Vec<String>,
}

impl UpdateUi {
    /// `<app data dir>/updates`: where a zip is downloaded to. None in a scenario.
    fn downloads_dir(&self) -> Option<std::path::PathBuf> {
        Some(self.state_path.as_ref()?.parent()?.join("updates"))
    }

    /// `isolated`: a scenario or a test, which must reach neither the network nor the user's files.
    pub(super) fn new(isolated: bool) -> Self {
        Self {
            fetcher: if isolated {
                Arc::new(check::OfflineFetcher)
            } else {
                Arc::new(check::HttpReleaseFetcher)
            },
            current: env!("CARGO_PKG_VERSION").to_owned(),
            debug_build: cfg!(debug_assertions),
            state_path: (!isolated)
                .then(crate::platform::app_data_dir)
                .flatten()
                .map(|dir| dir.join("update-check.json")),
            task: None,
            opener: Box::new(|url, cx| cx.open_url(url)),
            source: if isolated { Arc::new(install::OfflineSource) } else { Arc::new(install::HttpSource) },
            trust: crate::drivers::manifest_trust(engine_driver_host::TrustPolicy::Strict).ok(),
            exe: (!isolated).then(|| std::env::current_exe().ok().and_then(|exe| exe.canonicalize().ok())).flatten(),
            target: install::current_target(),
            restart: Box::new(|cx, bundle| {
                // The new app starts once this one has exited, whichever way the quit goes (it may ask
                // first about open transactions).
                let (bundle, pid) = (bundle.to_owned(), std::process::id());
                cx.on_app_quit(move |_| {
                    let _ = install::spawn_relaunch(&bundle, pid);
                    std::future::ready(())
                })
                .detach();
            }),
            quit: Box::new(|cx| cx.dispatch_action(&super::menus::Quit)),
            pending_restart: None,
            install: None,
            install_task: None,
            ticker: None,
            #[cfg(test)]
            shown: Vec::new(),
        }
    }
}

impl AppRoot {
    /// On launch: checks if the preference, the build and the 24-hour throttle allow it. Silent
    /// unless a newer version exists.
    pub(super) fn start_launch_update_check(&mut self, cx: &mut Context<Self>) {
        self.clean_up_after_update(cx);
        if check::auto_check_wanted(
            self.settings.updates.check_automatically,
            self.update_ui.debug_build,
            &self.update_ui.current,
        ) {
            self.start_update_check(Mode::Auto, cx);
        }
    }

    /// "Check for Updates": always asks, and always says what it found.
    pub(super) fn check_for_updates(&mut self, cx: &mut Context<Self>) {
        self.start_update_check(Mode::Manual, cx);
    }

    /// On a launch: what an earlier update left beside the app (the previous bundle, unpacked or
    /// downloaded files) goes, now that this version runs.
    fn clean_up_after_update(&self, cx: &mut Context<Self>) {
        let Ok(bundle) = install::locate_bundle(self.update_ui.exe.as_deref(), self.update_ui.debug_build) else {
            return;
        };
        let downloads = self.update_ui.downloads_dir();
        let executor = cx.background_executor().clone();
        cx.background_executor()
            .spawn(async move {
                let clean = || install::clean_up(&bundle, downloads.as_deref(), Duration::from_secs(60 * 60), SystemTime::now());
                clean();
                // The previous bundle is kept for a while (another instance may run from it): once more
                // when it is old enough.
                executor.timer(install::BACKUP_MIN_AGE + Duration::from_secs(30)).await;
                clean();
            })
            .detach();
    }

    fn start_update_check(&mut self, mode: Mode, cx: &mut Context<Self>) {
        // Installed already: this process runs from the moved-aside bundle, so only a restart is left.
        if let Some(version) = self.update_ui.pending_restart.clone() {
            if mode == Mode::Manual {
                self.show_restart_notice(&version, cx);
            }
            return;
        }
        let fetcher = Arc::clone(&self.update_ui.fetcher);
        let source = Arc::clone(&self.update_ui.source);
        let trust = self.update_ui.trust.clone();
        let exe = self.update_ui.exe.clone();
        let (debug_build, target) = (self.update_ui.debug_build, self.update_ui.target);
        let state_path = self.update_ui.state_path.clone();
        let current = self.update_ui.current.clone();
        let work = cx.background_executor().spawn(smol::unblock(move || {
            let outcome = check::run(&*fetcher, state_path.as_deref(), &current, mode, SystemTime::now());
            // A newer release: what can be offered for it (this fetches the signed manifest).
            let plan = match &outcome {
                Outcome::Available(release) => Some(install::plan(
                    &*source,
                    trust.as_ref(),
                    exe.as_deref(),
                    debug_build,
                    &release.version,
                    &current,
                    target,
                )),
                _ => None,
            };
            (outcome, plan)
        }));
        self.update_ui.task = Some(cx.spawn(async move |this, cx| {
            let (outcome, plan) = work.await;
            let _ = this.update(cx, |this, cx| {
                this.update_ui.task = None;
                this.finish_update_check(mode, outcome, plan, cx);
            });
        }));
    }

    fn finish_update_check(&mut self, mode: Mode, outcome: Outcome, plan: Option<Plan>, cx: &mut Context<Self>) {
        match (outcome, mode) {
            // An install in flight keeps its own notice.
            (Outcome::Available(_), _) if self.update_ui.install.is_some() => {}
            (Outcome::Available(release), _) => {
                let offer = match plan {
                    Some(Plan::Install { artifact, bundle }) => {
                        Offer { release, install: Some((artifact, bundle)), note: None }
                    }
                    Some(Plan::Download { note, problem }) => {
                        if let Some(problem) = problem {
                            let report = crate::error_log::Report::new(
                                crate::error_log::Severity::Info,
                                Area::App,
                                format!("Update {} is not installable here: {problem}", release.version),
                            );
                            self.report_event(None, report);
                        }
                        Offer { release, install: None, note }
                    }
                    None => Offer { release, install: None, note: None },
                };
                self.show_update_notice(offer, cx)
            }
            (Outcome::Failed(error), Mode::Auto) => {
                // Silent for the user; the Error Log keeps an Info entry.
                let report = crate::error_log::Report::new(
                    crate::error_log::Severity::Info,
                    Area::App,
                    format!("Update check skipped: {error}"),
                );
                self.report_event(None, report);
            }
            (Outcome::Failed(error), Mode::Manual) => self.show_update_message(
                NotificationType::Error,
                format!("Couldn't check for updates: {error}"),
                cx,
            ),
            (Outcome::UpToDate, Mode::Manual) => {
                let message = format!("You're up to date ({})", self.update_ui.current);
                self.show_update_message(NotificationType::Success, message, cx);
            }
            (Outcome::UpToDate | Outcome::NotDue, Mode::Auto) | (Outcome::NotDue, Mode::Manual) => {}
        }
    }

    fn show_update_message(&mut self, kind: NotificationType, message: String, cx: &mut Context<Self>) {
        #[cfg(test)]
        self.update_ui.shown.push(message.clone());
        self.with_ui_window(cx, move |window, cx| {
            window.push_notification(Notification::new().id::<UpdateNotice>().with_type(kind).message(message), cx);
        });
    }

    /// The notice: "TableX <v> is available" with Install & Restart (a verified update this app can
    /// install) or Download, then Later and Skip This Version. It stays until one of them (or its
    /// close button) is used.
    fn show_update_notice(&mut self, offer: Offer, cx: &mut Context<Self>) {
        let title = format!("TableX {} is available", offer.release.version);
        #[cfg(test)]
        self.update_ui.shown.push(title.clone());
        let root = cx.entity().downgrade();
        self.with_ui_window(cx, move |window, cx| {
            let notice = Notification::new()
                .id::<UpdateNotice>()
                .with_type(NotificationType::Info)
                .title(title)
                .autohide(false)
                .content(move |_, _, cx| {
                    let button = |id: &'static str, label: &'static str, run: fn(&mut AppRoot, &Offer, &mut Context<AppRoot>)| {
                        let (root, offer) = (root.clone(), offer.clone());
                        Button::new(id)
                            .debug_selector(|| id.into())
                            .label(label)
                            .small()
                            .on_click(move |_, window, cx| {
                                window.remove_notification::<UpdateNotice>(cx);
                                let _ = root.update(cx, |this, cx| run(this, &offer, cx));
                            })
                    };
                    let primary = if offer.install.is_some() {
                        button("update-install", "Install & Restart", |this, offer, cx| this.start_install(offer, cx))
                    } else {
                        button("update-download", "Download", |this, offer, cx| (this.update_ui.opener)(&offer.release.url, cx))
                    };
                    gpui_component::v_flex()
                        .when_some(offer.note.clone(), |this, note| {
                            this.child(div().text_sm().text_color(cx.theme().muted_foreground).child(note))
                        })
                        .child(
                            gpui_component::h_flex()
                                .gap_2()
                                .mt_2()
                                .child(primary.primary())
                                .child(button("update-later", "Later", |_, _, _| {}).ghost())
                                .child(button("update-skip", "Skip This Version", |this, offer, cx| {
                                    this.skip_update(&offer.release, cx)
                                }).ghost()),
                        )
                        .into_any_element()
                });
            window.push_notification(notice, cx);
        });
    }

    /// Install & Restart: downloads and checks the update off the UI thread (the notice shows where it
    /// is and offers Cancel), then swaps the bundle and restarts.
    fn start_install(&mut self, offer: &Offer, cx: &mut Context<Self>) {
        let (Some((artifact, bundle)), Some(downloads)) = (offer.install.clone(), self.update_ui.downloads_dir()) else {
            return self.install_failed(offer, "there is nowhere to download to".into(), cx);
        };
        if self.update_ui.install.is_some() || self.update_ui.pending_restart.is_some() {
            return;
        }
        let run = InstallRun { progress: Arc::default(), cancel: Arc::default() };
        let (progress, cancel) = (Arc::clone(&run.progress), Arc::clone(&run.cancel));
        self.update_ui.install = Some(run);
        self.show_install_progress(&offer.release.version, Arc::clone(&progress), Arc::clone(&cancel), cx);

        let source = Arc::clone(&self.update_ui.source);
        let (version, target, current) = (offer.release.version.clone(), self.update_ui.target, self.update_ui.current.clone());
        // Everything, the swap of the bundles included, runs off the UI thread.
        let work = cx.background_executor().spawn(smol::unblock(move || {
            let staged = install::download_and_stage(&*source, &artifact, &bundle, &version, target, &downloads, &cancel, &progress)?;
            install::install_staged(&bundle, staged, &current, &cancel).map(|()| bundle)
        }));
        let offer = offer.clone();
        self.update_ui.install_task = Some(cx.spawn(async move |this, cx| {
            let result = work.await;
            let _ = this.update(cx, |this, cx| {
                this.update_ui.install_task = None;
                this.update_ui.install = None;
                this.update_ui.ticker = None;
                this.finish_install(&offer, result, cx);
            });
        }));
        // The progress notice is repainted a few times a second while the install lasts.
        self.update_ui.ticker = Some(cx.spawn(async move |this, cx| loop {
            cx.background_executor().timer(Duration::from_millis(200)).await;
            let alive = this.update(cx, |this, cx| {
                let alive = this.update_ui.install.is_some();
                if alive {
                    this.with_ui_window(cx, |window, _| window.refresh());
                }
                alive
            });
            if !matches!(alive, Ok(true)) {
                break;
            }
        }));
    }

    fn show_install_progress(
        &mut self,
        version: &check::Version,
        progress: Arc<install::Progress>,
        cancel: Arc<std::sync::atomic::AtomicBool>,
        cx: &mut Context<Self>,
    ) {
        let title = format!("Installing TableX {version}");
        #[cfg(test)]
        self.update_ui.shown.push(title.clone());
        self.with_ui_window(cx, move |window, cx| {
            let notice = Notification::new()
                .id::<UpdateNotice>()
                .with_type(NotificationType::Info)
                .title(title)
                .autohide(false)
                // Closing the notice (its close button) stops the install, as Cancel does.
                .on_close({
                    let cancel = cancel.clone();
                    move |_, _| cancel.store(true, std::sync::atomic::Ordering::Release)
                })
                .content(move |_, _, cx| {
                    let stage = progress.stage();
                    let label = match stage {
                        install::Stage::Downloading => format!("Downloading {}%", (progress.fraction() * 100.0) as u32),
                        install::Stage::Verifying => "Verifying".to_owned(),
                        install::Stage::Installing => "Installing".to_owned(),
                    };
                    let cancel = cancel.clone();
                    gpui_component::v_flex()
                        .gap_1()
                        .child(div().text_sm().text_color(cx.theme().muted_foreground).child(label))
                        .child(div().debug_selector(|| "update-progress".into()).w_full().child(
                            gpui_component::progress::Progress::new("update-progress-bar").value(progress.fraction() * 100.0),
                        ))
                        .when(stage == install::Stage::Downloading, |this| {
                            this.child(
                                gpui_component::h_flex().mt_1().child(
                                    Button::new("update-cancel")
                                        .debug_selector(|| "update-cancel".into())
                                        .label("Cancel")
                                        .small()
                                        .ghost()
                                        .on_click(move |_, window, cx| {
                                            cancel.store(true, std::sync::atomic::Ordering::Release);
                                            window.remove_notification::<UpdateNotice>(cx);
                                        }),
                                ),
                            )
                        })
                        .into_any_element()
                });
            window.push_notification(notice, cx);
        });
    }

    /// The install is over: restart into the new bundle, or fall back to Download with the reason.
    fn finish_install(&mut self, offer: &Offer, result: Result<Bundle, install::InstallError>, cx: &mut Context<Self>) {
        match result {
            Ok(bundle) => {
                // This process now runs from the moved-aside bundle: nothing else is checked or
                // installed, the relaunch is armed once, and quitting (asked for now, or later if the
                // user cancels the prompt) starts the new version.
                self.update_ui.pending_restart = Some(offer.release.version.clone());
                (self.update_ui.restart)(cx, &bundle.path);
                self.show_restart_notice(&offer.release.version, cx);
                (self.update_ui.quit)(cx);
            }
            Err(install::InstallError::Cancelled) => {
                self.with_ui_window(cx, |window, cx| window.remove_notification::<UpdateNotice>(cx));
            }
            Err(error) => self.install_failed(offer, error.to_string(), cx),
        }
    }

    /// "Restart to finish updating": the new version is in place. Restart quits (asking first about
    /// open work, as Quit does) and the new version starts.
    fn show_restart_notice(&mut self, version: &check::Version, cx: &mut Context<Self>) {
        let title = format!("TableX {version} is installed");
        #[cfg(test)]
        self.update_ui.shown.push(title.clone());
        let root = cx.entity().downgrade();
        self.with_ui_window(cx, move |window, cx| {
            let notice = Notification::new()
                .id::<UpdateNotice>()
                .with_type(NotificationType::Success)
                .title(title)
                .autohide(false)
                .content(move |_, _, cx| {
                    let root = root.clone();
                    gpui_component::v_flex()
                        .child(div().text_sm().text_color(cx.theme().muted_foreground).child("Restart to finish updating."))
                        .child(
                            gpui_component::h_flex()
                                .gap_2()
                                .mt_2()
                                .child(
                                    Button::new("update-restart")
                                        .debug_selector(|| "update-restart".into())
                                        .label("Restart")
                                        .small()
                                        .primary()
                                        .on_click(move |_, window, cx| {
                                            window.remove_notification::<UpdateNotice>(cx);
                                            let _ = root.update(cx, |this, cx| (this.update_ui.quit)(cx));
                                        }),
                                )
                                .child(
                                    Button::new("update-restart-later")
                                        .debug_selector(|| "update-restart-later".into())
                                        .label("Later")
                                        .small()
                                        .ghost()
                                        .on_click(|_, window, cx| window.remove_notification::<UpdateNotice>(cx)),
                                ),
                        )
                        .into_any_element()
                });
            window.push_notification(notice, cx);
        });
    }

    /// The notice again, as Download, with what went wrong.
    fn install_failed(&mut self, offer: &Offer, reason: String, cx: &mut Context<Self>) {
        self.report_warning(Area::App, None, format!("Update {} not installed: {reason}", offer.release.version));
        let note = format!("Couldn't install: {reason}");
        self.show_update_notice(Offer { release: offer.release.clone(), install: None, note: Some(note) }, cx);
    }

    /// Skip This Version: remembered across restarts, off the UI thread.
    fn skip_update(&mut self, release: &Release, cx: &mut Context<Self>) {
        let Some(path) = self.update_ui.state_path.clone() else {
            return;
        };
        let version = release.version.clone();
        cx.background_executor()
            .spawn(async move {
                // Not being able to write only means the notice comes back next time.
                let _ = check::skip_version(&path, &version);
            })
            .detach();
    }
}
