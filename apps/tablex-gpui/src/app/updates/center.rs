//! The Update Center: the title bar's Updates button and the dialog behind it, with the tablex tab
//! (the state of the check and the install) and the Changelog tab (the app's releases, fetched when
//! the tab is first shown). The check, the install and the notice's actions are the ones in
//! [`super`]; this file only shows them.

use super::*;
use crate::app::assistant::markup;
use gpui::{AnyElement, SharedString};
use gpui_component::accordion::Accordion;
use gpui_component::collapsible::Collapsible;
use gpui_component::dialog::{Dialog, DialogFooter};
use gpui_component::tab::{Tab, TabBar};
use gpui_component::{h_flex, v_flex};
use std::collections::HashSet;

/// A click on the title bar button checks again when the last check is older than this.
const CHECK_AGAIN_AFTER: Duration = Duration::from_secs(5 * 60);

/// The releases listed open to a click; the rest sit under "Older versions".
const RECENT_RELEASES: usize = 5;

/// The tallest the body of a tab grows before it scrolls.
const BODY_MAX_HEIGHT: f32 = 380.0;

/// What the last check found, as the tablex tab shows it.
pub(in crate::app) enum Status {
    /// No check has finished in this run.
    Idle,
    UpToDate,
    Available(Offer),
    /// A newer release that the user chose to skip (a check from the Update Center shows it again).
    Skipped(Release),
    Failed(String),
}

pub(in crate::app) enum Changelog {
    /// Not asked for yet: the tab asks when it is first shown.
    Idle,
    Loading,
    /// Newest version first.
    Loaded(Vec<Release>),
    Failed(String),
}

pub(in crate::app) struct UpdateCenter {
    /// The dialog is on screen: results are shown in it instead of in a notice.
    pub(in crate::app) open: bool,
    /// 0: tablex, 1: Changelog.
    pub(in crate::app) tab: usize,
    pub(in crate::app) status: Status,
    /// When the last check started.
    checked_at: Option<Instant>,
    pub(in crate::app) changelog: Changelog,
    changelog_task: Option<gpui::Task<()>>,
    /// The versions whose notes are open.
    expanded: HashSet<String>,
    older_open: bool,
    /// The version Open Release opens on the Changelog tab (the last one opened; the newest at first).
    selected: Option<String>,
    extensions: gpui_component::text::MarkdownExtensions,
}

impl UpdateCenter {
    pub(super) fn new() -> Self {
        Self {
            open: false,
            tab: 0,
            status: Status::Idle,
            checked_at: None,
            changelog: Changelog::Idle,
            changelog_task: None,
            expanded: HashSet::new(),
            older_open: false,
            selected: None,
            extensions: markup::extensions(),
        }
    }

    /// A check has started: the Update Center does not ask again for a few minutes.
    pub(super) fn checking_started(&mut self) {
        self.checked_at = Some(Instant::now());
    }
}

impl AppRoot {
    /// Whether a newer version is known: found by a check, or installed and waiting for a restart.
    pub(in crate::app) fn update_known(&self) -> bool {
        matches!(self.update_ui.center.status, Status::Available(_)) || self.update_ui.pending_restart.is_some()
    }

    /// The line the tablex tab leads with.
    pub(in crate::app) fn update_status_line(&self) -> String {
        let ui = &self.update_ui;
        if ui.install.is_some() {
            return match &ui.center.status {
                Status::Available(offer) => format!("Installing tablex {}", offer.release.version),
                _ => "Installing tablex".to_owned(),
            };
        }
        if ui.task.is_some() {
            return "Checking for updates\u{2026}".to_owned();
        }
        if let Some(version) = &ui.pending_restart {
            return format!("Installed {version} \u{2014} restart to finish");
        }
        match &ui.center.status {
            Status::Idle => "No check has run yet.".to_owned(),
            Status::UpToDate => format!("tablex is up to date ({})", ui.current),
            Status::Available(Offer { release, .. }) | Status::Skipped(release) => {
                format!("tablex {} is available", release.version)
            }
            Status::Failed(_) => "Couldn't check for updates".to_owned(),
        }
    }

    /// The title bar's Updates button: a ghost icon button, with a dot while a newer version is known.
    pub(in crate::app) fn render_updates_button(&self, cx: &mut Context<Self>) -> impl IntoElement {
        div()
            .relative()
            .child(
                Button::new("title-updates")
                    .debug_selector(|| "title-updates-button".into())
                    .icon(Icon::empty().path("icons/cloud-download.svg").size(px(14.0)))
                    .compact()
                    .ghost()
                    .tooltip("Updates")
                    .on_click(cx.listener(|this, _, window, cx| this.open_update_center(window, false, cx))),
            )
            .when(self.update_known(), |this| this.child(title_bar_dot("title-updates-dot")))
    }

    /// Opens the Update Center on the tablex tab, and checks when no check ran lately (`force`: always).
    pub(in crate::app) fn open_update_center(&mut self, window: &mut Window, force: bool, cx: &mut Context<Self>) {
        if self.update_ui.center.open {
            if force {
                self.start_update_check(Mode::Manual, cx);
            }
            return;
        }
        let center = &mut self.update_ui.center;
        center.open = true;
        center.tab = 0;
        // The dialog shows what the notice would; an install's progress notice stays (closing it cancels).
        if self.update_ui.install.is_none() {
            window.remove_notification::<UpdateNotice>(cx);
        }
        let stale = force || center.checked_at.is_none_or(|at| at.elapsed() > CHECK_AGAIN_AFTER);
        if stale && self.update_ui.task.is_none() && self.update_ui.install.is_none() {
            self.start_update_check(Mode::Manual, cx);
        }
        let root = cx.entity().downgrade();
        window.open_dialog(cx, move |dialog, _, cx| render_dialog(dialog, &root, cx));
        cx.notify();
    }

    fn select_update_tab(&mut self, tab: usize, cx: &mut Context<Self>) {
        self.update_ui.center.tab = tab;
        if tab == 1 && matches!(self.update_ui.center.changelog, Changelog::Idle) {
            self.load_changelog(cx);
        }
        cx.notify();
    }

    /// Fetches the list of releases off the UI thread; a newer request replaces the one in flight.
    fn load_changelog(&mut self, cx: &mut Context<Self>) {
        let fetcher = Arc::clone(&self.update_ui.fetcher);
        self.update_ui.center.changelog = Changelog::Loading;
        let work = cx.background_executor().spawn(smol::unblock(move || {
            fetcher.fetch_list().and_then(|body| check::parse_list(&body))
        }));
        self.update_ui.center.changelog_task = Some(cx.spawn(async move |this, cx| {
            let result = work.await;
            let _ = this.update(cx, |this, cx| {
                let center = &mut this.update_ui.center;
                center.changelog_task = None;
                center.changelog = match result {
                    Ok(releases) => {
                        // The newest is open and is the one Open Release goes to.
                        if let Some(newest) = releases.first().map(|release| release.version.to_string()) {
                            center.expanded = HashSet::from([newest.clone()]);
                            center.selected = Some(newest);
                        }
                        Changelog::Loaded(releases)
                    }
                    Err(error) => Changelog::Failed(error),
                };
                cx.notify();
            });
        }));
    }

    /// The accordion of `group` reports which of its rows are open.
    fn set_open_releases(&mut self, group: &[String], open: &[usize], cx: &mut Context<Self>) {
        let center = &mut self.update_ui.center;
        for (index, version) in group.iter().enumerate() {
            if open.contains(&index) {
                if center.expanded.insert(version.clone()) {
                    center.selected = Some(version.clone());
                }
            } else {
                center.expanded.remove(version);
            }
        }
        cx.notify();
    }

    /// The page Open Release goes to; `None` while there is none to show.
    fn shown_release_url(&self) -> Option<String> {
        let center = &self.update_ui.center;
        if center.tab == 1 {
            let Changelog::Loaded(releases) = &center.changelog else {
                return None;
            };
            let selected = center.selected.as_deref();
            return releases
                .iter()
                .find(|release| Some(release.version.to_string().as_str()) == selected)
                .map(|release| release.url.clone());
        }
        Some(match &center.status {
            Status::Available(offer) => offer.release.url.clone(),
            Status::Skipped(release) => release.url.clone(),
            // Up to date: the page of the running version.
            _ => match check::Version::parse(&self.update_ui.current) {
                Some(version) => format!("https://github.com/tablex-org/tablex/releases/tag/v{version}"),
                None => check::RELEASES_PAGE.to_owned(),
            },
        })
    }

    fn open_shown_release(&mut self, cx: &mut Context<Self>) {
        if let Some(url) = self.shown_release_url().filter(|url| check::is_github_page(url)) {
            (self.update_ui.opener)(&url, cx);
        }
    }

    /// Check Now: the check, and on the Changelog tab the list again.
    fn check_now(&mut self, cx: &mut Context<Self>) {
        self.start_update_check(Mode::Manual, cx);
        if self.update_ui.center.tab == 1 {
            self.load_changelog(cx);
        }
        cx.notify();
    }
}

fn render_dialog(dialog: Dialog, root: &gpui::WeakEntity<AppRoot>, cx: &gpui::App) -> Dialog {
    let Some(entity) = root.upgrade() else {
        return dialog;
    };
    let this = entity.read(cx);
    let center = &this.update_ui.center;
    let body = if center.tab == 0 { release_tab(this, root, cx) } else { changelog_tab(this, root, cx) };
    let tabs = {
        let root = root.clone();
        TabBar::new("update-tabs")
            .underline()
            .selected_index(center.tab)
            .on_click(move |index, _, cx| {
                let index = *index;
                let _ = root.update(cx, |this, cx| this.select_update_tab(index, cx));
            })
            .child(Tab::new().label("tablex").debug_selector(|| "update-tab-release".into()))
            .child(Tab::new().label("Changelog").debug_selector(|| "update-tab-changelog".into()))
    };
    let footer = {
        let (open_root, check_root) = (root.clone(), root.clone());
        DialogFooter::new()
            .child(
                Button::new("update-open-release")
                    .debug_selector(|| "update-open-release".into())
                    .label("Open Release")
                    .disabled(this.shown_release_url().is_none())
                    .on_click(move |_, _, cx| {
                        let _ = open_root.update(cx, |this, cx| this.open_shown_release(cx));
                    }),
            )
            .child(
                Button::new("update-check-now")
                    .debug_selector(|| "update-check-now".into())
                    .label("Check Now")
                    .primary()
                    .on_click(move |_, _, cx| {
                        let _ = check_root.update(cx, |this, cx| this.check_now(cx));
                    }),
            )
    };
    let closed = root.clone();
    dialog
        .w(px(560.0))
        .title("Updates")
        .overlay_closable(true)
        .child(v_flex().gap_3().child(tabs).child(body))
        .footer(footer)
        .on_close(move |_, _, cx| {
            let _ = closed.update(cx, |this, cx| {
                this.update_ui.center.open = false;
                // An install goes on: its progress moves to a notice, whose close button cancels.
                if let (Some(run), Status::Available(offer)) = (&this.update_ui.install, &this.update_ui.center.status) {
                    let (version, progress, cancel) = (offer.release.version.clone(), run.progress.clone(), run.cancel.clone());
                    this.show_install_progress(&version, progress, cancel, cx);
                }
            });
        })
}

/// The scrolling frame of a tab's body.
fn body(id: &'static str) -> gpui::Stateful<gpui::Div> {
    v_flex().id(id).gap_2().max_h(px(BODY_MAX_HEIGHT)).overflow_y_scroll()
}

fn muted(text: impl Into<SharedString>, cx: &gpui::App) -> gpui::Div {
    div().text_sm().text_color(cx.theme().muted_foreground).child(text.into())
}

fn heading(text: impl Into<SharedString>) -> gpui::Div {
    div().text_base().font_weight(gpui::FontWeight::SEMIBOLD).child(text.into())
}

fn spinner(label: String, selector: &'static str, cx: &gpui::App) -> AnyElement {
    h_flex()
        .debug_selector(move || selector.into())
        .gap_2()
        .items_center()
        .child(gpui_component::spinner::Spinner::new().with_size(Size::Small))
        .child(muted(label, cx))
        .into_any_element()
}

fn action(id: &'static str, label: &'static str, root: &gpui::WeakEntity<AppRoot>, run: fn(&mut AppRoot, &mut Context<AppRoot>)) -> Button {
    let root = root.clone();
    Button::new(id)
        .debug_selector(move || id.into())
        .label(label)
        .on_click(move |_, _, cx| {
            let _ = root.update(cx, |this, cx| run(this, cx));
        })
}

/// Release notes as Markdown that loads and renders nothing by itself; a link opens only when it is
/// a github.com page.
fn notes(
    id: String,
    release: &Release,
    root: &gpui::WeakEntity<AppRoot>,
    extensions: &gpui_component::text::MarkdownExtensions,
    cx: &gpui::App,
) -> AnyElement {
    if release.notes.trim().is_empty() {
        return muted("No release notes.", cx).into_any_element();
    }
    let root = root.clone();
    markup::view(id, SharedString::from(release.notes.clone()), extensions)
        .on_link_click(move |url, event, _, cx| {
            if !event.is_right_click() && check::is_github_page(url) {
                let _ = root.update(cx, |this, cx| (this.update_ui.opener)(url, cx));
            }
        })
        .into_any_element()
}

fn release_tab(this: &AppRoot, root: &gpui::WeakEntity<AppRoot>, cx: &gpui::App) -> AnyElement {
    let ui = &this.update_ui;
    let frame = body("update-body");
    let line = this.update_status_line();
    if let Some(run) = &ui.install {
        use std::sync::atomic::Ordering;
        let label = match run.progress.stage() {
            install::Stage::Downloading => format!("Downloading {}%", (run.progress.fraction() * 100.0) as u32),
            install::Stage::Verifying => "Verifying".to_owned(),
            install::Stage::Installing => "Installing".to_owned(),
        };
        let cancel = run.cancel.clone();
        let downloading = run.progress.stage() == install::Stage::Downloading;
        return frame
            .child(heading(line).debug_selector(|| "update-status".into()))
            .child(muted(label, cx))
            .child(div().debug_selector(|| "update-progress".into()).w_full().child(
                gpui_component::progress::Progress::new("update-progress-bar").value(run.progress.fraction() * 100.0),
            ))
            .when(downloading, |this| {
                this.child(
                    h_flex().child(
                        Button::new("update-cancel")
                            .debug_selector(|| "update-cancel".into())
                            .label("Cancel")
                            .on_click(move |_, _, _| cancel.store(true, Ordering::Release)),
                    ),
                )
            })
            .into_any_element();
    }
    if ui.task.is_some() {
        return frame.child(spinner(line, "update-checking", cx)).into_any_element();
    }
    if ui.pending_restart.is_some() {
        return frame
            .child(heading(line).debug_selector(|| "update-status".into()))
            .child(h_flex().child(action("update-restart", "Restart", root, |this, cx| (this.update_ui.quit)(cx)).primary()))
            .into_any_element();
    }
    match &ui.center.status {
        Status::Idle => frame.child(muted(line, cx)).into_any_element(),
        Status::UpToDate => frame
            .child(heading(line).debug_selector(|| "update-status".into()))
            .into_any_element(),
        Status::Skipped(_) => frame
            .child(heading(line).debug_selector(|| "update-status".into()))
            .child(muted("You chose to skip this version. Check Now offers it again.", cx))
            .into_any_element(),
        Status::Failed(reason) => frame
            .child(heading(line).debug_selector(|| "update-status".into()))
            .child(muted(reason.clone(), cx).debug_selector(|| "update-error".into()))
            .child(h_flex().child(action("update-retry", "Retry", root, |this, cx| this.start_update_check(Mode::Manual, cx))))
            .into_any_element(),
        Status::Available(offer) => {
            let primary = if offer.install.is_some() {
                action("update-install", "Install & Restart", root, |this, cx| {
                    if let Status::Available(offer) = &this.update_ui.center.status {
                        let offer = offer.clone();
                        this.start_install(&offer, cx);
                    }
                })
            } else {
                action("update-download", "Download", root, |this, cx| {
                    if let Status::Available(offer) = &this.update_ui.center.status {
                        (this.update_ui.opener)(&offer.release.url, cx);
                    }
                })
            };
            frame
                .child(heading(line).debug_selector(|| "update-status".into()))
                .when_some(offer.note.clone(), |this, note| this.child(muted(note, cx)))
                .child(
                    h_flex().gap_2().child(primary.primary()).child(action(
                        "update-skip",
                        "Skip This Version",
                        root,
                        |this, cx| {
                            if let Status::Available(offer) = std::mem::replace(&mut this.update_ui.center.status, Status::Idle) {
                                this.skip_update(&offer.release, cx);
                                this.update_ui.center.status = Status::Skipped(offer.release);
                            }
                            cx.notify();
                        },
                    )),
                )
                .child(
                    div()
                        .debug_selector(|| "update-notes".into())
                        .child(notes(format!("update-notes-{}", offer.release.version), &offer.release, root, &ui.center.extensions, cx)),
                )
                .into_any_element()
        }
    }
}

fn changelog_tab(this: &AppRoot, root: &gpui::WeakEntity<AppRoot>, cx: &gpui::App) -> AnyElement {
    let center = &this.update_ui.center;
    let frame = body("changelog-body");
    match &center.changelog {
        Changelog::Idle | Changelog::Loading => {
            frame.child(spinner("Loading releases\u{2026}".to_owned(), "changelog-loading", cx)).into_any_element()
        }
        Changelog::Failed(reason) => frame
            .child(heading("Couldn't load the releases"))
            .child(muted(reason.clone(), cx).debug_selector(|| "changelog-error".into()))
            .child(h_flex().child(action("changelog-retry", "Retry", root, |this, cx| this.load_changelog(cx))))
            .into_any_element(),
        Changelog::Loaded(releases) if releases.is_empty() => {
            frame.child(muted("No releases found.", cx)).into_any_element()
        }
        Changelog::Loaded(releases) => {
            let (recent, older) = releases.split_at(releases.len().min(RECENT_RELEASES));
            frame
                .child(release_list("changelog-recent", recent, this, root, cx))
                .when(!older.is_empty(), |this_| {
                    let toggle = root.clone();
                    this_.child(
                        v_flex()
                            .gap_2()
                            .child(
                                h_flex().child(
                                    Button::new("changelog-older")
                                        .debug_selector(|| "changelog-older".into())
                                        .label(format!("Older versions ({})", older.len()))
                                        .icon(if center.older_open { IconName::ChevronDown } else { IconName::ChevronRight })
                                        .ghost()
                                        .on_click(move |_, _, cx| {
                                            let _ = toggle.update(cx, |this, cx| {
                                                this.update_ui.center.older_open = !this.update_ui.center.older_open;
                                                cx.notify();
                                            });
                                        }),
                                ),
                            )
                            .child(
                                Collapsible::new()
                                    .open(center.older_open)
                                    .content(release_list("changelog-older-list", older, this, root, cx)),
                            ),
                    )
                })
                .into_any_element()
        }
    }
}

/// An accordion of releases, each row a version and its date, opening to its notes.
fn release_list(
    id: &'static str,
    releases: &[Release],
    this: &AppRoot,
    root: &gpui::WeakEntity<AppRoot>,
    cx: &gpui::App,
) -> Accordion {
    let center = &this.update_ui.center;
    let group: Vec<String> = releases.iter().map(|release| release.version.to_string()).collect();
    // As tall as its rows: the body scrolls, the accordion does not clip.
    let mut list = Accordion::new(id).multiple(true).h_auto().flex_shrink_0();
    for (release, version) in releases.iter().zip(&group) {
        let open = center.expanded.contains(version);
        let current = *version == this.update_ui.current;
        list = list.item(|item| {
            item.open(open)
                .title(
                    h_flex()
                        .debug_selector({
                            let version = version.clone();
                            move || format!("changelog-row-{version}")
                        })
                        .gap_2()
                        .items_baseline()
                        .child(div().font_weight(gpui::FontWeight::SEMIBOLD).child(format!("tablex {version}")))
                        .when_some(release.date.clone(), |this, date| this.child(muted(date, cx)))
                        .when(current, |this| this.child(muted("(installed)", cx))),
                )
                .when(open, |item| {
                    item.child(div().w_full().child(notes(
                        format!("changelog-notes-{version}"),
                        release,
                        root,
                        &center.extensions,
                        cx,
                    )))
                })
        });
    }
    let root = root.clone();
    list.on_toggle_click(move |open, _, cx| {
        let (group, open) = (group.clone(), open.to_vec());
        let _ = root.update(cx, |this, cx| this.set_open_releases(&group, &open, cx));
    })
}

/// `--ui-scenario updates` / `updates-changelog`: the Update Center over canned releases (nothing
/// reaches the network).
#[cfg(any(test, feature = "ui-scenarios"))]
mod scenario {
    use super::*;

    struct Releases;

    const NOTES: &str = "## What's new\n\n- The **Update Center**: check, read what changed and install from one place.\n- Release notes for every version in the *Changelog* tab.\n\n## Fixes\n\n- Keep the filter builder open after `Clear Filter`.\n- Select and copy text in the tree view.\n";

    fn release(version: &str, notes: &str, day: &str) -> serde_json::Value {
        serde_json::json!({
            "tag_name": format!("v{version}"),
            "html_url": format!("https://github.com/tablex-org/tablex/releases/tag/v{version}"),
            "body": notes,
            "published_at": format!("{day}T09:00:00Z"),
        })
    }

    impl ReleaseFetcher for Releases {
        fn fetch(&self) -> Result<String, String> {
            Ok(release("0.1.7", NOTES, "2026-11-13").to_string())
        }

        fn fetch_list(&self) -> Result<String, String> {
            let mut list = vec![release("0.1.7", NOTES, "2026-11-13")];
            for patch in (0..=6).rev() {
                let notes = format!("## Changes\n\n- Improvements and fixes of 0.1.{patch}.\n- Visit the [releases page](https://github.com/tablex-org/tablex/releases).\n");
                list.push(release(&format!("0.1.{patch}"), &notes, &format!("2026-11-{:02}", 7 + patch)));
            }
            Ok(serde_json::Value::Array(list).to_string())
        }
    }

    impl AppRoot {
        pub(in crate::app) fn seed_updates_scenario(&mut self, changelog: bool, cx: &mut Context<Self>) {
            self.update_ui.fetcher = Arc::new(Releases);
            self.update_ui.current = "0.1.6".to_owned();
            // The window exists after the first frame.
            cx.spawn(async move |this, cx| {
                cx.background_executor().timer(Duration::from_millis(800)).await;
                let _ = this.update(cx, |this, cx| {
                    let root = cx.entity().downgrade();
                    this.with_ui_window(cx, move |window, cx| {
                        let _ = root.update(cx, |this, cx| {
                            this.open_update_center(window, true, cx);
                            if changelog {
                                this.select_update_tab(1, cx);
                            }
                        });
                    });
                });
            })
            .detach();
        }
    }
}

/// The dot on a title bar button whose window has something new (Updates, Driver Manager): placed in
/// the button's top right corner, so the button's parent is `relative`.
pub(in crate::app) fn title_bar_dot(selector: &'static str) -> gpui::Div {
    div()
        .debug_selector(move || selector.into())
        .absolute()
        .top(px(3.0))
        .right(px(3.0))
        .size(px(7.0))
        .rounded_full()
        .bg(theme::accent())
}
