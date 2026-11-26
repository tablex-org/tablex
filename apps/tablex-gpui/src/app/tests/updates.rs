// The update check as a user meets it: the notice after the launch check (Download, Later, Skip This
// Version), the "Check for Updates" palette command, and silence for an automatic check that fails.
// The release comes from a fake fetcher, the page opener is observed, and the state file is a temp one.

struct FakeReleases {
    answer: std::sync::Mutex<Result<String, String>>,
    /// The list of releases (the changelog).
    list: std::sync::Mutex<Result<String, String>>,
    calls: std::sync::atomic::AtomicUsize,
    list_calls: std::sync::atomic::AtomicUsize,
}

impl FakeReleases {
    fn tag(tag: &str) -> std::sync::Arc<Self> {
        let releases = std::sync::Arc::new(Self {
            answer: std::sync::Mutex::new(Err(String::new())),
            list: std::sync::Mutex::new(Ok("[]".into())),
            calls: Default::default(),
            list_calls: Default::default(),
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
        self.set_release(tag, "");
    }

    fn set_release(&self, tag: &str, notes: &str) {
        let body = serde_json::json!({
            "tag_name": tag,
            "html_url": format!("https://github.com/tablex-org/tablex/releases/tag/{tag}"),
            "body": notes,
        });
        *self.answer.lock().unwrap() = Ok(body.to_string());
    }

    fn set_list(&self, releases: serde_json::Value) {
        *self.list.lock().unwrap() = Ok(releases.to_string());
    }

    fn fail_list(&self, reason: &str) {
        *self.list.lock().unwrap() = Err(reason.into());
    }

    fn list_calls(&self) -> usize {
        self.list_calls.load(std::sync::atomic::Ordering::SeqCst)
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

    fn fetch_list(&self) -> Result<String, String> {
        self.list_calls.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        self.list.lock().unwrap().clone()
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
        // The checks run on `smol::unblock` threads that wake the scheduler whenever they finish, which
        // may be before the first `wait_until`: parking is allowed from the start.
        visual.dispatcher.allow_parking();
        visual.simulate_resize(gpui::size(gpui::px(1400.0), gpui::px(850.0)));
        visual.update(|window, cx| {
            actions::bind_keys(cx);
            window.activate_window();
        });
        let state = std::env::temp_dir().join(format!("tablex-update-ui-{}", uuid::Uuid::new_v4())).join("tablex.db");
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

    fn center_open(&self) -> bool {
        self.app.read_with(self.visual, |this, _| this.update_ui.center.open)
    }

    fn status_line(&self) -> String {
        self.app.read_with(self.visual, |this, _| this.update_status_line())
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
    assert_eq!(rig.shown(), ["tablex 0.2.0 is available"]);
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
    assert_eq!(rig.shown(), ["tablex 0.2.0 is available", "tablex 0.3.0 is available"]);
    assert_eq!(rig.notifications(), 1);
}

#[gpui::test]
fn the_palette_check_with_an_equal_version_says_up_to_date_even_when_not_due(cx: &mut TestAppContext) {
    let mut rig = UpdateRig::new(cx, FakeReleases::tag("v0.1.0"), "0.1.0");
    rig.launch_check();
    assert!(rig.shown().is_empty(), "an automatic check says nothing when there is nothing new");
    assert_eq!(rig.releases.calls(), 1);

    // Checked a moment ago: the command ignores the throttle, and the Update Center says the result.
    rig.palette_check();
    assert_eq!(rig.releases.calls(), 2);
    assert!(rig.center_open() && rig.shows("update-status"));
    assert_eq!(rig.status_line(), "tablex is up to date (0.1.0)");
    assert!(rig.shown().is_empty(), "no notice besides the dialog");
    assert_eq!(rig.notifications(), 0);
}

#[gpui::test]
fn the_palette_check_shows_a_newer_release_even_when_it_was_skipped(cx: &mut TestAppContext) {
    let mut rig = UpdateRig::new(cx, FakeReleases::tag("v0.2.0"), "0.1.0");
    updates::check::skip_version(&rig.state, &updates::check::Version::parse("0.2.0").unwrap()).unwrap();
    rig.palette_check();
    assert!(rig.center_open());
    assert_eq!(rig.status_line(), "tablex 0.2.0 is available");
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
    assert_eq!(rig.status_line(), "Couldn't check for updates");
    assert!(rig.shows("update-error") && rig.shows("update-retry"));
    assert_eq!(rig.notifications(), 0);
}

#[gpui::test]
fn the_launch_check_does_not_fetch_when_the_preference_is_off_the_build_is_debug_or_the_version_is_dev(cx: &mut TestAppContext) {
    let mut rig = UpdateRig::new(cx, FakeReleases::tag("v0.2.0"), "0.1.0");
    rig.app.update(rig.visual, |this, _| {
        let mut settings = (**this.settings).clone();
        settings.updates.check_automatically = false;
        *this.settings = std::sync::Arc::new(settings);
    });
    rig.launch_check();
    assert_eq!(rig.releases.calls(), 0, "preference off");

    rig.app.update(rig.visual, |this, _| {
        let mut settings = (**this.settings).clone();
        settings.updates.check_automatically = true;
        *this.settings = std::sync::Arc::new(settings);
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
    assert_eq!(rig.shown(), ["tablex 0.2.0 is available"]);
}

// --- Install & Restart ---------------------------------------------------------------------------
// The signed manifest decides what the notice offers; the install itself runs against a fake source
// (canned manifest and zip), a real bundle in a temp folder and a recorder in place of the restart.

use updates::install::tests as fixtures;

// The pieces below serve the install tests, which are unix only.
#[cfg_attr(not(unix), allow(dead_code))]
const INSTALL_TARGET: &str = "aarch64-apple-darwin";
#[cfg_attr(not(unix), allow(dead_code))]
const ZIP_URL: &str = "https://github.com/tablex-org/tablex/releases/download/v0.2.0/tablex-0.2.0-macos-apple-silicon.app.zip";

#[cfg_attr(not(unix), allow(dead_code))]
fn app_manifest(size: u64, sha256: &str) -> serde_json::Value {
    serde_json::json!({
        "kind": "tablex-app-update", "schema": 1, "version": "0.2.0", "generatedAt": "2026-11-11T00:00:00Z",
        "artifacts": [{"target": INSTALL_TARGET, "url": ZIP_URL, "size": size, "sha256": sha256}],
    })
}

/// What the restart hook was armed with, and how often the app was asked to quit.
#[cfg_attr(not(unix), allow(dead_code))]
#[derive(Clone, Default)]
struct Restarts {
    armed: std::rc::Rc<std::cell::RefCell<Vec<std::path::PathBuf>>>,
    quits: std::rc::Rc<std::cell::Cell<usize>>,
    /// The bundles the install asked LaunchServices to refresh.
    refreshed: std::sync::Arc<std::sync::Mutex<Vec<std::path::PathBuf>>>,
}

#[cfg_attr(not(unix), allow(dead_code))]
impl UpdateRig<'_> {
    /// Points the install at `fake`, the key 1 and `exe`; a restart is recorded instead of done.
    fn install_env(&mut self, exe: Option<std::path::PathBuf>) -> (std::sync::Arc<fixtures::Fake>, Restarts) {
        let fake = std::sync::Arc::new(fixtures::Fake::default());
        let restarts = Restarts::default();
        let refreshed = restarts.refreshed.clone();
        let (source, seen, quits) = (fake.clone(), restarts.armed.clone(), restarts.quits.clone());
        self.app.update(self.visual, move |this, _| {
            this.update_ui.source = source;
            this.update_ui.trust = Some(fixtures::trust(&fixtures::key(1)));
            this.update_ui.exe = exe;
            this.update_ui.target = INSTALL_TARGET;
            // Never LaunchServices: the bundles of a test are temporary ones.
            this.update_ui.refresh = std::sync::Arc::new(move |bundle| refreshed.lock().unwrap().push(bundle.to_owned()));
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

// Installing in place is macOS-only: directory_writable is false on Windows, so no Install is ever offered there.
#[cfg(unix)]
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
    let new = fixtures::make_bundle(&scratch.0.join("packed"), "tablex.app", "org.tablex.TableX", "0.2.0");
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
    assert_eq!(*restarts.refreshed.lock().unwrap(), std::slice::from_ref(&running), "the new bundle is registered once, after the swap");
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

// Installing in place is macOS-only: directory_writable is false on Windows, so no Install is ever offered there.
#[cfg(unix)]
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
    assert_eq!(rig.shown().last().map(String::as_str), Some("Installing tablex 0.2.0"));
    assert_eq!(rig.notifications(), 1);
    assert!(rig.shows("update-progress") && rig.shows("update-cancel"));
    assert!(!rig.shows("update-install"), "the notice is the progress now");

    rig.click("update-cancel");
    rig.wait_install();
    assert_eq!(rig.notifications(), 0, "cancelling ends it without another notice");
    assert!(restarts.armed.borrow().is_empty() && rig.opened.borrow().is_empty());
    assert_eq!(rig.state().skipped, None);
}

// Installing in place is macOS-only: directory_writable is false on Windows, so no Install is ever offered there.
#[cfg(unix)]
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

// --- the Update Center -----------------------------------------------------------------------------
// The title bar button and the dialog behind it: the tablex tab (the check and the install) and the
// Changelog tab (the releases, from the fake list fetcher).

use updates::center::Changelog;

fn release_json(tag: &str, notes: &str) -> serde_json::Value {
    serde_json::json!({
        "tag_name": tag,
        "html_url": format!("https://github.com/tablex-org/tablex/releases/tag/{tag}"),
        "body": notes,
        "published_at": "2026-11-12T08:00:00Z",
    })
}

/// Eight app releases (newest first, 0.1.7 to 0.1.0) with a driver release between, one of them with an
/// address that is not GitHub's.
fn release_list() -> serde_json::Value {
    let mut list = vec![release_json("drivers-v2026.11.13", "Notes of the drivers.")];
    for patch in (0..=7).rev() {
        let mut release = release_json(&format!("v0.1.{patch}"), &format!("Notes of 0.1.{patch}."));
        if patch == 7 {
            release["html_url"] = "https://evil.example/releases/7".into();
        }
        list.push(release);
    }
    serde_json::Value::Array(list)
}

impl UpdateRig<'_> {
    /// The Markdown views built from now on, to read what they were given.
    fn watch_markdown(&mut self) -> std::sync::Arc<std::sync::Mutex<Vec<gpui::Entity<gpui_component::text::TextViewState>>>> {
        let states = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
        let seen = states.clone();
        self.visual.update(move |_, cx| {
            cx.observe_new(move |_: &mut gpui_component::text::TextViewState, _, cx| {
                seen.lock().unwrap().push(cx.entity());
            })
            .detach();
        });
        states
    }

    fn markdown_sources(&mut self, states: &std::sync::Mutex<Vec<gpui::Entity<gpui_component::text::TextViewState>>>) -> Vec<String> {
        let states = states.lock().unwrap().clone();
        states.iter().map(|state| state.read_with(self.visual, |state, _| state.rendered_text().source().to_string())).collect()
    }

    fn open_center(&mut self) {
        let bounds = self.visual.debug_bounds("title-updates-button").expect("the Updates button is on screen");
        self.visual.simulate_mouse_move(bounds.center(), None, Default::default());
        self.visual.simulate_click(bounds.center(), Default::default());
        self.finish();
    }

    fn open_changelog(&mut self) {
        self.click("update-tab-changelog");
        let app = self.app.clone();
        wait_until(self.visual, "the changelog", |visual| {
            app.read_with(visual, |this, _| !matches!(this.update_ui.center.changelog, Changelog::Loading))
        });
        for _ in 0..3 {
            settle(self.visual);
        }
    }

    fn changelog_versions(&self) -> Option<Vec<String>> {
        self.app.read_with(self.visual, |this, _| match &this.update_ui.center.changelog {
            Changelog::Loaded(releases) => Some(releases.iter().map(|release| release.version.to_string()).collect()),
            _ => None,
        })
    }

    fn y_of(&mut self, selector: &'static str) -> f32 {
        let bounds = self.visual.debug_bounds(selector).unwrap_or_else(|| panic!("{selector} is on screen"));
        bounds.origin.y.into()
    }
}

#[gpui::test]
fn the_updates_button_sits_left_of_the_theme_button_opens_the_center_and_escape_closes_it(cx: &mut TestAppContext) {
    let mut rig = UpdateRig::new(cx, FakeReleases::tag("v0.1.6"), "0.1.6");
    let updates = rig.visual.debug_bounds("title-updates-button").expect("the Updates button");
    let theme = rig.visual.debug_bounds("title-theme-button").expect("the theme button");
    let settings = rig.visual.debug_bounds("open-settings").expect("the gear");
    assert_eq!(updates.size, theme.size, "the same size as the theme button");
    assert!(updates.right() <= theme.left() && theme.right() <= settings.left(), "updates, theme, gear from left to right");
    assert!(!rig.center_open());

    rig.open_center();
    assert!(rig.center_open());
    assert!(rig.shows("update-tab-release") && rig.shows("update-tab-changelog"));
    assert_eq!(rig.releases.calls(), 1, "nothing was checked yet: opening checks");

    rig.visual.simulate_keystrokes("escape");
    for _ in 0..3 {
        settle(rig.visual);
    }
    rig.visual.cx.executor().advance_clock(Duration::from_secs(2));
    settle(rig.visual);
    assert!(!rig.center_open());
    assert!(!rig.shows("update-tab-release"), "the dialog is gone");

    // A check a moment ago stands: opening again asks nothing.
    rig.open_center();
    assert_eq!(rig.releases.calls(), 1);
    assert!(rig.center_open() && rig.shows("update-tab-release"));
}

#[gpui::test]
fn an_up_to_date_build_is_told_so_and_shows_no_dot(cx: &mut TestAppContext) {
    let mut rig = UpdateRig::new(cx, FakeReleases::tag("v0.1.6"), "0.1.6");
    rig.open_center();
    assert_eq!(rig.status_line(), "tablex is up to date (0.1.6)");
    assert!(rig.shows("update-status"));
    assert!(!rig.shows("title-updates-dot"));
    assert!(!rig.shows("update-install") && !rig.shows("update-download"));
    assert_eq!(rig.notifications(), 0);
}

// Installing in place is macOS-only: directory_writable is false on Windows, so no Install is ever offered there.
#[cfg(unix)]
#[gpui::test]
fn a_newer_release_puts_a_dot_on_the_button_and_the_center_shows_its_notes_and_install(cx: &mut TestAppContext) {
    let releases = FakeReleases::tag("v0.2.0");
    releases.set_release("v0.2.0", "Fixed the crash on exit.");
    let mut rig = UpdateRig::new(cx, releases, "0.1.0");
    let scratch = fixtures::Scratch::new();
    let exe = fixtures::fake_app_layout(&scratch.0, "TableX.app");
    let (fake, restarts) = rig.install_env(Some(exe));
    fake.serve(ZIP_URL, b"zip".to_vec());
    fake.publish(&fixtures::key(1), &app_manifest(3, &"ab".repeat(32)));
    fake.hold.store(true, std::sync::atomic::Ordering::Release);
    let markdown = rig.watch_markdown();

    assert!(!rig.shows("title-updates-dot"));
    rig.launch_check();
    assert!(rig.shows("title-updates-dot"), "a newer version is known");
    assert_eq!(rig.notifications(), 1, "the launch check keeps its notice");

    // The notice slides in over the title bar for a moment; the click comes after it has settled.
    rig.visual.cx.executor().advance_clock(Duration::from_secs(2));
    settle(rig.visual);
    rig.open_center();
    // The notice slides out before it is gone.
    rig.visual.cx.executor().advance_clock(Duration::from_secs(2));
    settle(rig.visual);
    assert_eq!(rig.notifications(), 0, "the dialog takes over from the notice");
    assert_eq!(rig.status_line(), "tablex 0.2.0 is available");
    assert!(rig.shows("update-notes"));
    assert!(rig.markdown_sources(&markdown).iter().any(|source| source == "Fixed the crash on exit."));
    assert!(rig.shows("update-install") && rig.shows("update-skip") && !rig.shows("update-download"));
    assert_eq!(rig.releases.calls(), 1, "the check of the launch is recent");

    // Install & Restart is the notice's install: the progress is in the dialog, with Cancel.
    rig.click("update-install");
    assert_eq!(rig.status_line(), "Installing tablex 0.2.0");
    assert!(rig.shows("update-progress") && rig.shows("update-cancel") && !rig.shows("update-install"));
    assert_eq!(rig.notifications(), 0, "no second progress notice");
    rig.click("update-cancel");
    rig.wait_install();
    assert!(rig.shows("update-install"), "back to the offer");
    assert!(restarts.armed.borrow().is_empty());
    assert!(rig.shows("title-updates-dot"));
}

#[gpui::test]
fn without_an_installable_app_the_center_offers_download_and_skip_is_remembered(cx: &mut TestAppContext) {
    let mut rig = UpdateRig::new(cx, FakeReleases::tag("v0.2.0"), "0.1.0");
    rig.open_center();
    assert!(rig.shows("update-download") && !rig.shows("update-install"));
    rig.click("update-download");
    assert_eq!(*rig.opened.borrow(), ["https://github.com/tablex-org/tablex/releases/tag/v0.2.0"]);

    rig.click("update-skip");
    let state = rig.state.clone();
    wait_until(rig.visual, "the skip to be written", |_| updates::check::State::load(&state).skipped.is_some());
    assert_eq!(rig.state().skipped.as_deref(), Some("0.2.0"));
    assert!(!rig.shows("title-updates-dot") && !rig.shows("update-download"));
    // Check Now offers it again.
    rig.click("update-check-now");
    rig.finish();
    assert!(rig.shows("update-download") && rig.shows("title-updates-dot"));
}

#[cfg(target_os = "macos")]
#[gpui::test]
fn a_finished_install_asks_for_a_restart_in_the_center(cx: &mut TestAppContext) {
    use sha2::Digest as _;
    let mut rig = UpdateRig::new(cx, FakeReleases::tag("v0.2.0"), "0.1.0");
    let scratch = fixtures::Scratch::new();
    let running = scratch.0.join("TableX.app");
    fixtures::make_bundle(&scratch.0, "TableX.app", "org.tablex.TableX", "0.1.0");
    let new = fixtures::make_bundle(&scratch.0.join("packed"), "tablex.app", "org.tablex.TableX", "0.2.0");
    let zip = scratch.0.join("release.zip");
    fixtures::zip_of(&new, &zip);
    let bytes = std::fs::read(&zip).unwrap();
    let (fake, restarts) = rig.install_env(Some(running.join("Contents/MacOS/TableX")));
    fake.serve(ZIP_URL, bytes.clone());
    fake.publish(&fixtures::key(1), &app_manifest(bytes.len() as u64, &hex::encode(sha2::Sha256::digest(&bytes))));

    rig.open_center();
    rig.click("update-install");
    rig.wait_install();
    assert_eq!(rig.status_line(), "Installed 0.2.0 \u{2014} restart to finish");
    assert_eq!(rig.notifications(), 0);
    assert!(rig.shows("update-restart") && rig.shows("title-updates-dot"));
    assert_eq!(restarts.quits.get(), 1);
    rig.click("update-restart");
    assert_eq!(restarts.quits.get(), 2);
}

#[gpui::test]
fn the_changelog_lists_app_releases_newest_first_and_folds_the_older_ones(cx: &mut TestAppContext) {
    let releases = FakeReleases::tag("v0.1.7");
    releases.set_list(release_list());
    let mut rig = UpdateRig::new(cx, releases, "0.1.6");
    let markdown = rig.watch_markdown();
    rig.open_center();
    assert_eq!(rig.releases.list_calls(), 0, "the list is fetched when the tab is first shown");

    rig.open_changelog();
    assert_eq!(rig.releases.list_calls(), 1);
    let versions: Vec<String> = (0..=7).rev().map(|patch| format!("0.1.{patch}")).collect();
    assert_eq!(rig.changelog_versions(), Some(versions), "newest first, no driver release");
    let (first, second, fifth) = (rig.y_of("changelog-row-0.1.7"), rig.y_of("changelog-row-0.1.6"), rig.y_of("changelog-row-0.1.3"));
    assert!(first < second && second < fifth);
    assert!(!rig.shows("changelog-row-0.1.2"), "older versions are folded");
    assert!(rig.shows("changelog-older"));
    // The newest is open.
    assert!(rig.markdown_sources(&markdown).iter().any(|source| source == "Notes of 0.1.7."));
    assert!(!rig.markdown_sources(&markdown).iter().any(|source| source == "Notes of 0.1.6."));

    // The newest release's address is not GitHub's: Open Release goes to the releases page, not there.
    rig.click("update-open-release");
    assert_eq!(*rig.opened.borrow(), ["https://github.com/tablex-org/tablex/releases/latest"]);

    // A row opens on a click, and Open Release goes to the last one opened.
    rig.click("changelog-row-0.1.6");
    assert!(rig.markdown_sources(&markdown).iter().any(|source| source == "Notes of 0.1.6."));
    rig.click("update-open-release");
    assert_eq!(rig.opened.borrow()[1], "https://github.com/tablex-org/tablex/releases/tag/v0.1.6");

    rig.click("changelog-older");
    assert!(rig.shows("changelog-row-0.1.2") && rig.shows("changelog-row-0.1.0"));
    rig.click("changelog-older");
    assert!(!rig.shows("changelog-row-0.1.2"), "folded again");
    rig.click("changelog-older");

    assert_eq!(rig.opened.borrow().len(), 2);
    assert!(rig.opened.borrow().iter().all(|url| url.starts_with("https://github.com/")));
}

#[gpui::test]
fn the_changelog_shows_a_failure_and_retry_and_check_now_fetch_again(cx: &mut TestAppContext) {
    let releases = FakeReleases::tag("v0.1.6");
    releases.fail_list("rate limited");
    let mut rig = UpdateRig::new(cx, releases, "0.1.6");
    rig.open_center();
    rig.open_changelog();
    assert!(rig.shows("changelog-error") && rig.shows("changelog-retry"));
    assert_eq!(rig.releases.list_calls(), 1);
    assert!(rig.visual.debug_bounds("update-open-release").is_some());

    rig.click("changelog-retry");
    let app = rig.app.clone();
    wait_until(rig.visual, "the retry", |visual| app.read_with(visual, |this, _| !matches!(this.update_ui.center.changelog, Changelog::Loading)));
    assert_eq!(rig.releases.list_calls(), 2);
    assert!(rig.shows("changelog-error"), "still failing");

    rig.releases.set_list(release_list());
    let checks = rig.releases.calls();
    rig.click("update-check-now");
    rig.finish();
    let app = rig.app.clone();
    wait_until(rig.visual, "the list", |visual| app.read_with(visual, |this, _| !matches!(this.update_ui.center.changelog, Changelog::Loading)));
    for _ in 0..3 {
        settle(rig.visual);
    }
    assert_eq!(rig.releases.list_calls(), 3, "Check Now fetches the list again");
    assert_eq!(rig.releases.calls(), checks + 1, "and checks");
    assert!(rig.shows("changelog-row-0.1.7") && !rig.shows("changelog-error"));
}

#[gpui::test]
fn a_failed_check_shows_its_reason_and_retry_checks_again(cx: &mut TestAppContext) {
    let mut rig = UpdateRig::new(cx, FakeReleases::failing("connection refused"), "0.1.0");
    rig.open_center();
    assert_eq!(rig.status_line(), "Couldn't check for updates");
    assert!(rig.shows("update-error") && rig.shows("update-retry"));
    assert_eq!(rig.releases.calls(), 1);

    rig.releases.set_tag("v0.1.0");
    rig.click("update-retry");
    rig.finish();
    assert_eq!(rig.releases.calls(), 2);
    assert_eq!(rig.status_line(), "tablex is up to date (0.1.0)");
    assert!(!rig.shows("update-retry"));
}
