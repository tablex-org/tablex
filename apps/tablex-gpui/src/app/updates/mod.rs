//! "A newer TableX exists": the check on launch (once a day, when the preference is on) and the
//! "Check for Updates" command, and the notice that reports them. The logic without UI is in
//! [`check`]. Nothing runs besides the launch check and the command; nothing is sent but the
//! request for the latest release.

pub(super) mod check;

use super::*;
use std::time::SystemTime;
use check::{Mode, Outcome, Release, ReleaseFetcher};
use gpui_component::notification::{Notification, NotificationType};

/// Marks the update notice, so a second one replaces the first instead of stacking.
struct UpdateNotice;

/// Opens a web page.
pub(in crate::app) type PageOpener = Box<dyn Fn(&str, &mut gpui::App)>;

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
    /// Every message shown, newest last (the notification keeps its text private).
    #[cfg(test)]
    pub(in crate::app) shown: Vec<String>,
}

impl UpdateUi {
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
            #[cfg(test)]
            shown: Vec::new(),
        }
    }
}

impl AppRoot {
    /// On launch: checks if the preference, the build and the 24-hour throttle allow it. Silent
    /// unless a newer version exists.
    pub(super) fn start_launch_update_check(&mut self, cx: &mut Context<Self>) {
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

    fn start_update_check(&mut self, mode: Mode, cx: &mut Context<Self>) {
        let fetcher = Arc::clone(&self.update_ui.fetcher);
        let state_path = self.update_ui.state_path.clone();
        let current = self.update_ui.current.clone();
        let work = cx.background_executor().spawn(smol::unblock(move || {
            check::run(&*fetcher, state_path.as_deref(), &current, mode, SystemTime::now())
        }));
        self.update_ui.task = Some(cx.spawn(async move |this, cx| {
            let outcome = work.await;
            let _ = this.update(cx, |this, cx| {
                this.update_ui.task = None;
                this.finish_update_check(mode, outcome, cx);
            });
        }));
    }

    fn finish_update_check(&mut self, mode: Mode, outcome: Outcome, cx: &mut Context<Self>) {
        match (outcome, mode) {
            (Outcome::Available(release), _) => self.show_update_notice(release, cx),
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

    /// The notice: "TableX <v> is available" with Download, Later and Skip This Version. It stays
    /// until one of them (or its close button) is used.
    fn show_update_notice(&mut self, release: Release, cx: &mut Context<Self>) {
        let title = format!("TableX {} is available", release.version);
        #[cfg(test)]
        self.update_ui.shown.push(title.clone());
        let root = cx.entity().downgrade();
        self.with_ui_window(cx, move |window, cx| {
            let notice = Notification::new()
                .id::<UpdateNotice>()
                .with_type(NotificationType::Info)
                .title(title)
                .autohide(false)
                .content(move |_, _, _| {
                    let button = |id: &'static str, label: &'static str, run: fn(&mut AppRoot, &Release, &mut Context<AppRoot>)| {
                        let (root, release) = (root.clone(), release.clone());
                        Button::new(id)
                            .debug_selector(|| id.into())
                            .label(label)
                            .small()
                            .on_click(move |_, window, cx| {
                                window.remove_notification::<UpdateNotice>(cx);
                                let _ = root.update(cx, |this, cx| run(this, &release, cx));
                            })
                    };
                    gpui_component::h_flex()
                        .gap_2()
                        .mt_2()
                        .child(button("update-download", "Download", |this, release, cx| {
                            (this.update_ui.opener)(&release.url, cx)
                        }).primary())
                        .child(button("update-later", "Later", |_, _, _| {}).ghost())
                        .child(button("update-skip", "Skip This Version", |this, release, cx| {
                            this.skip_update(release, cx)
                        }).ghost())
                        .into_any_element()
                });
            window.push_notification(notice, cx);
        });
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
