// The update check as a user meets it: the notice after the launch check (Download, Later, Skip This
// Version), the "Check for Updates" palette command, and silence for an automatic check that fails.
// The release comes from a fake fetcher, the page opener is observed, and the state file is a temp one.

struct FakeReleases {
    answer: std::sync::Mutex<Result<String, String>>,
    calls: std::sync::atomic::AtomicUsize,
}

impl FakeReleases {
    fn tag(tag: &str) -> std::sync::Arc<Self> {
        let releases = std::sync::Arc::new(Self {
            answer: std::sync::Mutex::new(Err(String::new())),
            calls: Default::default(),
        });
        releases.set_tag(tag);
        releases
    }

    fn failing(reason: &str) -> std::sync::Arc<Self> {
        let releases = Self::tag("v0.0.0");
        *releases.answer.lock().unwrap() = Err(reason.into());
        releases
    }

    fn set_tag(&self, tag: &str) {
        let body = serde_json::json!({
            "tag_name": tag,
            "html_url": format!("https://github.com/tablex-org/tablex/releases/tag/{tag}"),
        });
        *self.answer.lock().unwrap() = Ok(body.to_string());
    }

    fn calls(&self) -> usize {
        self.calls.load(std::sync::atomic::Ordering::SeqCst)
    }
}

impl updates::check::ReleaseFetcher for FakeReleases {
    fn fetch(&self) -> Result<String, String> {
        self.calls.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        self.answer.lock().unwrap().clone()
    }
}

struct UpdateRig<'a> {
    app: gpui::Entity<AppRoot>,
    visual: &'a mut gpui::VisualTestContext,
    releases: std::sync::Arc<FakeReleases>,
    state: std::path::PathBuf,
    opened: std::rc::Rc<std::cell::RefCell<Vec<String>>>,
}

impl<'a> UpdateRig<'a> {
    /// A release build of `current`, whose latest release is `releases`.
    fn new(cx: &'a mut TestAppContext, releases: std::sync::Arc<FakeReleases>, current: &str) -> Self {
        let (app, visual) = sidebar_test_app(cx);
        visual.simulate_resize(gpui::size(gpui::px(1400.0), gpui::px(850.0)));
        visual.update(|window, cx| {
            actions::bind_keys(cx);
            window.activate_window();
        });
        let state = std::env::temp_dir().join(format!("tablex-update-ui-{}", uuid::Uuid::new_v4())).join("update-check.json");
        let opened = std::rc::Rc::new(std::cell::RefCell::new(Vec::new()));
        let (fetcher, seen, path, current) = (releases.clone(), opened.clone(), state.clone(), current.to_owned());
        app.update(visual, |this, _| {
            this.update_ui.fetcher = fetcher;
            this.update_ui.current = current;
            this.update_ui.debug_build = false;
            this.update_ui.state_path = Some(path);
            this.update_ui.opener = Box::new(move |url, _| seen.borrow_mut().push(url.to_owned()));
        });
        settle(visual);
        Self { app, visual, releases, state, opened }
    }

    fn launch_check(&mut self) {
        self.app.update(self.visual, |this, cx| this.start_launch_update_check(cx));
        self.finish();
    }

    /// Waits for the check in flight, if any, and lets the notification land.
    fn finish(&mut self) {
        let app = self.app.clone();
        wait_until(self.visual, "the update check", |visual| app.read_with(visual, |this, _| this.update_ui.task.is_none()));
        for _ in 0..3 {
            settle(self.visual);
        }
    }

    fn shown(&self) -> Vec<String> {
        self.app.read_with(self.visual, |this, _| this.update_ui.shown.clone())
    }

    fn notifications(&mut self) -> usize {
        self.visual.update(|window, cx| window.notifications(cx).len())
    }

    fn click(&mut self, selector: &'static str) {
        let bounds = self.visual.debug_bounds(selector).unwrap_or_else(|| panic!("{selector} is on screen"));
        self.visual.simulate_click(bounds.center(), Default::default());
        for _ in 0..3 {
            settle(self.visual);
        }
        // The notification slides out before it is gone.
        self.visual.cx.executor().advance_clock(Duration::from_secs(2));
        settle(self.visual);
    }

    fn palette_check(&mut self) {
        palette_open(self.visual);
        palette_type(self.visual, "check for updates");
        self.visual.simulate_keystrokes("enter");
        palette_settle(self.visual);
        self.finish();
    }

    fn state(&self) -> updates::check::State {
        updates::check::State::load(&self.state)
    }

    fn forget_last_check(&self) {
        let mut state = self.state();
        state.last_check = None;
        state.save(&self.state).unwrap();
    }
}

impl Drop for UpdateRig<'_> {
    fn drop(&mut self) {
        if let Some(directory) = self.state.parent() {
            let _ = std::fs::remove_dir_all(directory);
        }
    }
}

#[gpui::test]
fn a_newer_release_shows_the_notice_and_download_opens_its_page(cx: &mut TestAppContext) {
    let mut rig = UpdateRig::new(cx, FakeReleases::tag("v0.2.0"), "0.1.0");
    rig.launch_check();
    assert_eq!(rig.shown(), ["TableX 0.2.0 is available"]);
    assert_eq!(rig.notifications(), 1);
    for button in ["update-download", "update-later", "update-skip"] {
        assert!(rig.visual.debug_bounds(button).is_some(), "{button}");
    }
    assert!(rig.opened.borrow().is_empty(), "nothing opens by itself");

    rig.click("update-download");
    assert_eq!(*rig.opened.borrow(), ["https://github.com/tablex-org/tablex/releases/tag/v0.2.0"]);
    assert_eq!(rig.notifications(), 0, "the notice goes away");
    assert_eq!(rig.state().skipped, None, "Download is not a skip");
}

#[gpui::test]
fn later_hides_the_notice_and_the_next_launch_check_shows_it_again(cx: &mut TestAppContext) {
    let mut rig = UpdateRig::new(cx, FakeReleases::tag("v0.2.0"), "0.1.0");
    rig.launch_check();
    rig.click("update-later");
    assert_eq!(rig.notifications(), 0);
    assert_eq!(rig.state().skipped, None);
    assert!(rig.opened.borrow().is_empty());

    // The same day: the throttle keeps the second launch quiet. A day later it shows again.
    rig.launch_check();
    assert_eq!(rig.releases.calls(), 1);
    rig.forget_last_check();
    rig.launch_check();
    assert_eq!(rig.releases.calls(), 2);
    assert_eq!(rig.shown().len(), 2);
    assert_eq!(rig.notifications(), 1);
}

#[gpui::test]
fn skip_this_version_is_remembered_and_silences_later_checks(cx: &mut TestAppContext) {
    let mut rig = UpdateRig::new(cx, FakeReleases::tag("v0.2.0"), "0.1.0");
    rig.launch_check();
    rig.click("update-skip");
    assert_eq!(rig.notifications(), 0);
    let state = rig.state.clone();
    wait_until(rig.visual, "the skip to be written", |_| updates::check::State::load(&state).skipped.is_some());
    assert_eq!(rig.state().skipped.as_deref(), Some("0.2.0"));
    assert!(rig.opened.borrow().is_empty());

    // Next launch, a day later: asked again, not announced.
    rig.forget_last_check();
    rig.launch_check();
    assert_eq!(rig.releases.calls(), 2);
    assert_eq!(rig.shown().len(), 1, "no second notice for the skipped version");
    assert_eq!(rig.notifications(), 0);

    // A newer one than the skipped one is announced.
    rig.releases.set_tag("v0.3.0");
    rig.forget_last_check();
    rig.launch_check();
    assert_eq!(rig.shown(), ["TableX 0.2.0 is available", "TableX 0.3.0 is available"]);
    assert_eq!(rig.notifications(), 1);
}

#[gpui::test]
fn the_palette_check_with_an_equal_version_says_up_to_date_even_when_not_due(cx: &mut TestAppContext) {
    let mut rig = UpdateRig::new(cx, FakeReleases::tag("v0.1.0"), "0.1.0");
    rig.launch_check();
    assert!(rig.shown().is_empty(), "an automatic check says nothing when there is nothing new");
    assert_eq!(rig.releases.calls(), 1);

    // Checked a moment ago: the command ignores the throttle.
    rig.palette_check();
    assert_eq!(rig.releases.calls(), 2);
    assert_eq!(rig.shown(), ["You're up to date (0.1.0)"]);
    assert_eq!(rig.notifications(), 1);
}

#[gpui::test]
fn the_palette_check_shows_a_newer_release_even_when_it_was_skipped(cx: &mut TestAppContext) {
    let mut rig = UpdateRig::new(cx, FakeReleases::tag("v0.2.0"), "0.1.0");
    updates::check::skip_version(&rig.state, &updates::check::Version::parse("0.2.0").unwrap()).unwrap();
    rig.palette_check();
    assert_eq!(rig.shown(), ["TableX 0.2.0 is available"]);
    assert!(rig.visual.debug_bounds("update-download").is_some());
}

#[gpui::test]
fn a_failing_manual_check_shows_the_error_and_a_failing_automatic_one_shows_nothing(cx: &mut TestAppContext) {
    let mut rig = UpdateRig::new(cx, FakeReleases::failing("connection refused"), "0.1.0");
    rig.launch_check();
    assert_eq!(rig.releases.calls(), 1);
    assert!(rig.shown().is_empty());
    assert_eq!(rig.notifications(), 0);
    let errors = rig.app.read_with(rig.visual, |this, _| {
        (0..this.error_log.log.len())
            .filter_map(|index| this.error_log.log.get(index))
            .filter(|entry| entry.severity != crate::error_log::Severity::Info)
            .count()
    });
    assert_eq!(errors, 0, "at most an Info line in the log");

    rig.palette_check();
    assert_eq!(rig.releases.calls(), 2, "the command asks again at once");
    assert_eq!(rig.shown(), ["Couldn't check for updates: connection refused"]);
    assert_eq!(rig.notifications(), 1);
}

#[gpui::test]
fn the_launch_check_does_not_fetch_when_the_preference_is_off_the_build_is_debug_or_the_version_is_dev(cx: &mut TestAppContext) {
    let mut rig = UpdateRig::new(cx, FakeReleases::tag("v0.2.0"), "0.1.0");
    rig.app.update(rig.visual, |this, _| {
        let mut settings = (*this.settings).clone();
        settings.updates.check_automatically = false;
        this.settings = std::sync::Arc::new(settings);
    });
    rig.launch_check();
    assert_eq!(rig.releases.calls(), 0, "preference off");

    rig.app.update(rig.visual, |this, _| {
        let mut settings = (*this.settings).clone();
        settings.updates.check_automatically = true;
        this.settings = std::sync::Arc::new(settings);
        this.update_ui.debug_build = true;
    });
    rig.launch_check();
    assert_eq!(rig.releases.calls(), 0, "debug build");

    rig.app.update(rig.visual, |this, _| {
        this.update_ui.debug_build = false;
        this.update_ui.current = "dev".into();
    });
    rig.launch_check();
    assert_eq!(rig.releases.calls(), 0, "dev version");
    assert!(rig.shown().is_empty() && rig.notifications() == 0);

    // The same build with everything in order does check.
    rig.app.update(rig.visual, |this, _| this.update_ui.current = "0.1.0".into());
    rig.launch_check();
    assert_eq!(rig.releases.calls(), 1);
    assert_eq!(rig.shown(), ["TableX 0.2.0 is available"]);
}

// --- Install & Restart ---------------------------------------------------------------------------
// The signed manifest decides what the notice offers; the install itself runs against a fake source
// (canned manifest and zip), a real bundle in a temp folder and a recorder in place of the restart.

use updates::install::tests as fixtures;

const INSTALL_TARGET: &str = "aarch64-apple-darwin";
const ZIP_URL: &str = "https://github.com/tablex-org/tablex/releases/download/v0.2.0/TableX-0.2.0-macos-apple-silicon.app.zip";

fn app_manifest(size: u64, sha256: &str) -> serde_json::Value {
    serde_json::json!({
        "kind": "tablex-app-update", "schema": 1, "version": "0.2.0", "generatedAt": "2026-11-11T00:00:00Z",
        "artifacts": [{"target": INSTALL_TARGET, "url": ZIP_URL, "size": size, "sha256": sha256}],
    })
}

/// What the restart hook was armed with, and how often the app was asked to quit.
#[derive(Clone, Default)]
struct Restarts {
    armed: std::rc::Rc<std::cell::RefCell<Vec<std::path::PathBuf>>>,
    quits: std::rc::Rc<std::cell::Cell<usize>>,
}

impl UpdateRig<'_> {
    /// Points the install at `fake`, the key 1 and `exe`; a restart is recorded instead of done.
    fn install_env(&mut self, exe: Option<std::path::PathBuf>) -> (std::sync::Arc<fixtures::Fake>, Restarts) {
        let fake = std::sync::Arc::new(fixtures::Fake::default());
        let restarts = Restarts::default();
        let (source, seen, quits) = (fake.clone(), restarts.armed.clone(), restarts.quits.clone());
        self.app.update(self.visual, move |this, _| {
            this.update_ui.source = source;
            this.update_ui.trust = Some(fixtures::trust(&fixtures::key(1)));
            this.update_ui.exe = exe;
            this.update_ui.target = INSTALL_TARGET;
            this.update_ui.restart = Box::new(move |_, bundle| seen.borrow_mut().push(bundle.to_owned()));
            this.update_ui.quit = Box::new(move |_| quits.set(quits.get() + 1));
        });
        (fake, restarts)
    }

    /// The next day's launch check.
    fn check_again(&mut self) {
        self.forget_last_check();
        self.launch_check();
    }

    fn wait_install(&mut self) {
        let app = self.app.clone();
        wait_until(self.visual, "the install", |visual| app.read_with(visual, |this, _| this.update_ui.install_task.is_none()));
        for _ in 0..3 {
            settle(self.visual);
        }
    }

    fn shows(&mut self, selector: &'static str) -> bool {
        self.visual.debug_bounds(selector).is_some()
    }
}

#[gpui::test]
fn install_and_restart_is_offered_only_with_a_verified_manifest_and_an_installable_app(cx: &mut TestAppContext) {
    let mut rig = UpdateRig::new(cx, FakeReleases::tag("v0.2.0"), "0.1.0");
    let scratch = fixtures::Scratch::new();
    let exe = fixtures::fake_app_layout(&scratch.0, "TableX.app");
    let (fake, _) = rig.install_env(Some(exe));

    // No manifest on the release: Download.
    rig.launch_check();
    assert!(rig.shows("update-download") && !rig.shows("update-install"));
    rig.click("update-later");

    // A manifest signed by a key the app does not trust: Download.
    fake.publish(&fixtures::key(2), &app_manifest(10, &"ab".repeat(32)));
    rig.check_again();
    assert!(rig.shows("update-download") && !rig.shows("update-install"));
    rig.click("update-later");

    // Signed by the pinned key: Install & Restart, Later and Skip, and no Download.
    fake.publish(&fixtures::key(1), &app_manifest(10, &"ab".repeat(32)));
    rig.check_again();
    assert!(rig.shows("update-install") && rig.shows("update-later") && rig.shows("update-skip"));
    assert!(!rig.shows("update-download"));
    assert!(rig.opened.borrow().is_empty(), "nothing happens without a click");
    rig.click("update-later");
    assert_eq!(rig.notifications(), 0);

    // The same verified manifest, but this run cannot replace itself (a development run): Download.
    rig.app.update(rig.visual, |this, _| this.update_ui.exe = Some("/repo/target/release/tablex-gpui".into()));
    rig.check_again();
    assert!(rig.shows("update-download") && !rig.shows("update-install"));
}

#[cfg(target_os = "macos")]
#[gpui::test]
fn install_and_restart_downloads_verifies_swaps_the_bundle_and_restarts(cx: &mut TestAppContext) {
    use sha2::Digest as _;
    let mut rig = UpdateRig::new(cx, FakeReleases::tag("v0.2.0"), "0.1.0");
    let scratch = fixtures::Scratch::new();
    let running = scratch.0.join("TableX.app");
    fixtures::make_bundle(&scratch.0, "TableX.app", "org.tablex.TableX", "0.1.0");
    let new = fixtures::make_bundle(&scratch.0.join("packed"), "TableX.app", "org.tablex.TableX", "0.2.0");
    let zip = scratch.0.join("release.zip");
    fixtures::zip_of(&new, &zip);
    let bytes = std::fs::read(&zip).unwrap();
    let (fake, restarts) = rig.install_env(Some(running.join("Contents/MacOS/TableX")));
    fake.serve(ZIP_URL, bytes.clone());
    fake.publish(&fixtures::key(1), &app_manifest(bytes.len() as u64, &hex::encode(sha2::Sha256::digest(&bytes))));

    rig.launch_check();
    rig.click("update-install");
    rig.wait_install();

    assert_eq!(*restarts.armed.borrow(), std::slice::from_ref(&running), "the app restarts into the bundle it has just been replaced by");
    let version = std::process::Command::new("/usr/bin/plutil")
        .args(["-extract", "CFBundleShortVersionString", "raw", "-o", "-"])
        .arg(running.join("Contents/Info.plist"))
        .output()
        .unwrap();
    assert_eq!(String::from_utf8_lossy(&version.stdout).trim(), "0.2.0");
    assert!(scratch.0.join("TableX.app.old-0.1.0/Contents/Info.plist").is_file(), "the old app waits beside it");
    let leftovers: Vec<_> = std::fs::read_dir(&scratch.0)
        .unwrap()
        .flatten()
        .map(|entry| entry.file_name().to_string_lossy().into_owned())
        .filter(|name| name.starts_with(".TableX-update-"))
        .collect();
    assert!(leftovers.is_empty(), "{leftovers:?}");
    assert_eq!(restarts.quits.get(), 1, "the app is asked to quit once, as Quit does");

    // The new version waits for the restart: this process runs from the moved-aside bundle, so it is
    // neither checked nor installed again; the notice offers Restart, and a check shows it again.
    assert_eq!(rig.notifications(), 1);
    assert!(rig.shows("update-restart") && !rig.shows("update-install") && !rig.shows("update-download"));
    let checks = rig.releases.calls();
    rig.click("update-restart-later");
    assert_eq!(rig.notifications(), 0);
    rig.forget_last_check();
    rig.launch_check();
    assert_eq!(rig.releases.calls(), checks, "no check while a restart is pending");
    rig.palette_check();
    assert_eq!(rig.releases.calls(), checks);
    assert!(rig.shows("update-restart"), "Check for Updates shows the pending restart again");
    assert_eq!(restarts.armed.borrow().len(), 1, "the relaunch is armed once");
    rig.click("update-restart");
    assert_eq!(restarts.quits.get(), 2, "Restart quits");
    assert_eq!(rig.notifications(), 0);
    assert!(rig.opened.borrow().is_empty());
}

#[cfg(target_os = "macos")]
#[gpui::test]
fn a_download_that_does_not_match_the_manifest_falls_back_to_download_and_changes_nothing(cx: &mut TestAppContext) {
    let mut rig = UpdateRig::new(cx, FakeReleases::tag("v0.2.0"), "0.1.0");
    let scratch = fixtures::Scratch::new();
    let running = scratch.0.join("TableX.app");
    fixtures::make_bundle(&scratch.0, "TableX.app", "org.tablex.TableX", "0.1.0");
    let (fake, restarts) = rig.install_env(Some(running.join("Contents/MacOS/TableX")));
    fake.serve(ZIP_URL, b"not the signed file".to_vec());
    fake.publish(&fixtures::key(1), &app_manifest(19, &"ab".repeat(32)));

    rig.launch_check();
    rig.click("update-install");
    rig.wait_install();

    assert!(restarts.armed.borrow().is_empty(), "no restart");
    assert!(rig.shows("update-download") && !rig.shows("update-install"), "Download is offered instead");
    rig.click("update-download");
    assert_eq!(*rig.opened.borrow(), ["https://github.com/tablex-org/tablex/releases/tag/v0.2.0"]);
    assert!(running.join("Contents/Info.plist").is_file());
    let entries = std::fs::read_dir(&scratch.0).unwrap().count();
    assert_eq!(entries, 1, "the running bundle is all that is there");
}

#[gpui::test]
fn the_install_shows_its_progress_and_cancel_stops_it_quietly(cx: &mut TestAppContext) {
    let mut rig = UpdateRig::new(cx, FakeReleases::tag("v0.2.0"), "0.1.0");
    let scratch = fixtures::Scratch::new();
    let exe = fixtures::fake_app_layout(&scratch.0, "TableX.app");
    let (fake, restarts) = rig.install_env(Some(exe));
    fake.serve(ZIP_URL, b"zip".to_vec());
    fake.publish(&fixtures::key(1), &app_manifest(3, &"ab".repeat(32)));
    fake.hold.store(true, std::sync::atomic::Ordering::Release);

    rig.launch_check();
    rig.click("update-install");
    assert_eq!(rig.shown().last().map(String::as_str), Some("Installing TableX 0.2.0"));
    assert_eq!(rig.notifications(), 1);
    assert!(rig.shows("update-progress") && rig.shows("update-cancel"));
    assert!(!rig.shows("update-install"), "the notice is the progress now");

    rig.click("update-cancel");
    rig.wait_install();
    assert_eq!(rig.notifications(), 0, "cancelling ends it without another notice");
    assert!(restarts.armed.borrow().is_empty() && rig.opened.borrow().is_empty());
    assert_eq!(rig.state().skipped, None);
}

#[gpui::test]
fn closing_the_progress_notice_cancels_the_install(cx: &mut TestAppContext) {
    let mut rig = UpdateRig::new(cx, FakeReleases::tag("v0.2.0"), "0.1.0");
    let scratch = fixtures::Scratch::new();
    let exe = fixtures::fake_app_layout(&scratch.0, "TableX.app");
    let (fake, restarts) = rig.install_env(Some(exe));
    fake.serve(ZIP_URL, b"zip".to_vec());
    fake.publish(&fixtures::key(1), &app_manifest(3, &"ab".repeat(32)));
    fake.hold.store(true, std::sync::atomic::Ordering::Release);

    rig.launch_check();
    rig.click("update-install");
    assert!(rig.shows("update-cancel"));
    // The close button of the notice (and any other way of closing it) ends the install.
    rig.visual.update(|window, cx| {
        use gpui_component::WindowExt as _;
        window.clear_notifications(cx);
    });
    // The notice slides out before it is gone (and only then is it closed).
    for _ in 0..3 {
        settle(rig.visual);
    }
    rig.visual.cx.executor().advance_clock(Duration::from_secs(2));
    settle(rig.visual);
    rig.wait_install();
    assert_eq!(rig.notifications(), 0, "cancelled quietly");
    assert!(restarts.armed.borrow().is_empty() && restarts.quits.get() == 0);
    assert!(!rig.shows("update-download"));
}
