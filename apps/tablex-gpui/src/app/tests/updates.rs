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
