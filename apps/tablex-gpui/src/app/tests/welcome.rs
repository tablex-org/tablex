// The Welcome tab: its sections, what its buttons do, the Recent list and the note about drivers.

fn welcome_app(cx: &mut TestAppContext) -> (gpui::Entity<AppRoot>, &mut gpui::VisualTestContext) {
    let (app, visual) = sidebar_test_app(cx);
    visual.simulate_resize(gpui::size(gpui::px(1100.0), gpui::px(800.0)));
    app.update(visual, |this, cx| {
        let welcome = this.workspace.tabs().iter().find(|tab| tab.kind == TabKind::Welcome).map(|tab| tab.id);
        let welcome = welcome.unwrap_or_else(|| this.workspace.open_welcome_tab());
        this.workspace.activate_tab(welcome);
        cx.notify();
    });
    settle(visual);
    assert_eq!(active_kind(&app, visual), Some(TabKind::Welcome));
    (app, visual)
}

fn welcome_saved(name: &str, id: u128) -> SavedConnection {
    SavedConnection {
        id: uuid::Uuid::from_u128(id),
        name: name.into(),
        uri: "sqlite:///tmp/x.db".into(),
        kind: ConnectionKind::Sqlite,
        save_password: false,
        is_pinned: false,
        disabled: false,
        ssh_tunnel: false,
    }
}

fn welcome_names(list: Vec<&SavedConnection>) -> Vec<&str> {
    list.into_iter().map(|saved| saved.name.as_str()).collect()
}

#[test]
fn recent_connections_are_the_latest_used_first_then_the_rest_in_order() {
    use super::welcome::recent_connections;
    let saved: Vec<SavedConnection> = (1..=8).map(|n| welcome_saved(&format!("c{n}"), n)).collect();
    let id = |n: u128| uuid::Uuid::from_u128(n);

    // Nothing used yet: the sidebar's order, five of them.
    let none = HashMap::new();
    assert_eq!(welcome_names(recent_connections(&saved, &none, 5)), ["c1", "c2", "c3", "c4", "c5"]);

    // Used ones first, the latest on top; the others follow in order.
    let used = HashMap::from([(id(7), 300.0), (id(3), 100.0), (id(5), 200.0)]);
    assert_eq!(welcome_names(recent_connections(&saved, &used, 5)), ["c7", "c5", "c3", "c1", "c2"]);
    assert_eq!(welcome_names(recent_connections(&saved, &used, 2)), ["c7", "c5"]);

    // One this build cannot open is not offered; an empty list gives an empty list.
    let mut saved = saved;
    saved[6].disabled = true;
    assert!(!welcome_names(recent_connections(&saved, &used, 5)).contains(&"c7"));
    assert!(recent_connections(&[], &used, 5).is_empty());
}

#[gpui::test]
fn the_welcome_page_shows_its_sections(cx: &mut TestAppContext) {
    let (_app, visual) = welcome_app(cx);
    for selector in [
        "welcome-page",
        "welcome-header",
        "welcome-version",
        "welcome-start",
        "welcome-new-connection",
        "welcome-new-query",
        "welcome-open-sql",
        "welcome-import",
        "welcome-driver-manager",
        "welcome-recent",
        "welcome-recent-0",
        "welcome-keyboard",
        "welcome-links",
        "welcome-link-repository",
        "welcome-link-release-notes",
    ] {
        assert!(rendered(visual, selector.into()), "{selector} is drawn");
    }
    assert!(!rendered(visual, "welcome-recent-empty".into()));
}

#[gpui::test]
fn the_welcome_page_fits_a_small_window(cx: &mut TestAppContext) {
    let (_app, visual) = welcome_app(cx);
    visual.simulate_resize(gpui::size(gpui::px(520.0), gpui::px(360.0)));
    settle(visual);
    let page = visual.debug_bounds("welcome-page").expect("the page is drawn");
    assert!(page.size.width <= gpui::px(520.0) && page.size.height <= gpui::px(360.0), "{page:?}");
    // The sections wrap under each other instead of being cut off at the side.
    for selector in ["welcome-start", "welcome-recent", "welcome-keyboard", "welcome-links"] {
        let bounds = visual.debug_bounds(selector).unwrap_or_else(|| panic!("{selector} is drawn"));
        assert!(bounds.right() <= page.right() + gpui::px(1.0), "{selector} {bounds:?} in {page:?}");
    }
    let start = visual.debug_bounds("welcome-start").unwrap();
    let recent = visual.debug_bounds("welcome-recent").unwrap();
    assert!(recent.top() >= start.bottom() || recent.left() >= start.right(), "{start:?} {recent:?}");
}

#[gpui::test]
fn the_welcome_buttons_do_what_their_labels_say(cx: &mut TestAppContext) {
    // New Connection: the connection dialog.
    let (_app, visual) = welcome_app(cx);
    click(visual, "welcome-new-connection".into());
    assert!(rendered(visual, "connection-picker-search".into()), "the New Connection dialog opens");
}

#[gpui::test]
fn the_new_query_button_opens_a_query_tab(cx: &mut TestAppContext) {
    let (app, visual) = welcome_app(cx);
    click(visual, "welcome-new-query".into());
    assert_eq!(active_kind(&app, visual), Some(TabKind::Query));
}

#[gpui::test]
fn the_driver_manager_button_opens_the_driver_manager(cx: &mut TestAppContext) {
    let (app, visual) = welcome_app(cx);
    click(visual, "welcome-driver-manager".into());
    assert_eq!(active_kind(&app, visual), Some(TabKind::DriverManager));
}

#[gpui::test]
fn the_import_button_lists_the_four_sources(cx: &mut TestAppContext) {
    let (_app, visual) = welcome_app(cx);
    assert!(!rendered(visual, "welcome-import-dbx".into()), "closed");
    click(visual, "welcome-import".into());
    for source in ["dbx", "navicat", "dbeaver", "datagrip"] {
        assert!(rendered(visual, format!("welcome-import-{source}")), "{source} is listed");
    }
}

#[gpui::test]
fn a_recent_row_opens_its_connection_like_the_sidebar(cx: &mut TestAppContext) {
    let (app, visual) = welcome_app(cx);
    let first = app.read_with(visual, |this, _| {
        super::welcome::recent_connections(&this.saved_connections, &this.welcome.last_used, 5)[0].clone()
    });
    click(visual, "welcome-recent-0".into());
    let (kind, connection) = app.read_with(visual, |this, _| {
        let tab = this.workspace.active_tab().unwrap();
        (tab.kind, tab.connection_id.clone())
    });
    assert_eq!(kind, TabKind::Connection);
    assert_eq!(connection, first.id.to_string());
}

#[gpui::test]
fn a_connection_that_comes_up_moves_to_the_top_of_the_recent_list(cx: &mut TestAppContext) {
    let (app, visual) = welcome_app(cx);
    let (before, last) = app.read_with(visual, |this, _| {
        let all = super::welcome::recent_connections(&this.saved_connections, &this.welcome.last_used, 100);
        (all[0].id, all.last().unwrap().id.to_string())
    });
    app.update(visual, |this, cx| this.welcome_note_connected(&last, cx));
    let now_first = app.read_with(visual, |this, _| {
        super::welcome::recent_connections(&this.saved_connections, &this.welcome.last_used, 5)[0].id.to_string()
    });
    assert_eq!(now_first, last);
    assert_ne!(now_first, before.to_string());
}

#[gpui::test]
fn the_recent_list_says_so_when_there_are_no_connections(cx: &mut TestAppContext) {
    let (app, visual) = welcome_app(cx);
    app.update(visual, |this, cx| {
        this.saved_connections.clear();
        cx.notify();
    });
    settle(visual);
    assert!(rendered(visual, "welcome-recent-empty".into()));
    assert!(!rendered(visual, "welcome-recent-0".into()));
}

#[gpui::test]
fn the_drivers_note_shows_while_no_driver_is_installed(cx: &mut TestAppContext) {
    let (app, visual) = welcome_app(cx);
    let set = |app: &gpui::Entity<AppRoot>, visual: &mut gpui::VisualTestContext, loaded: bool, installed: &[&str]| {
        app.update(visual, |this, cx| {
            this.driver_ui.installed_loaded = loaded;
            this.driver_ui.installed = installed.iter().map(|id| id.to_string()).collect();
            cx.notify();
        });
        settle(visual);
    };
    // Still reading the store: no claim yet.
    set(&app, visual, false, &[]);
    assert!(!rendered(visual, "welcome-drivers-hint".into()));
    set(&app, visual, true, &[]);
    assert!(rendered(visual, "welcome-drivers-hint".into()));
    set(&app, visual, true, &["sqlite"]);
    assert!(!rendered(visual, "welcome-drivers-hint".into()));
    // The note's button leads to the Driver Manager.
    set(&app, visual, true, &[]);
    click(visual, "welcome-install-drivers".into());
    assert_eq!(active_kind(&app, visual), Some(TabKind::DriverManager));
}

#[test]
fn the_welcome_shortcuts_come_from_the_keymap() {
    use super::welcome::{hints, Hint};
    use gpui_component::kbd::Kbd;
    let shown = |overrides: &shortcuts::Overrides| -> Vec<(Hint, String)> {
        hints(overrides).into_iter().map(|(hint, stroke)| (hint, Kbd::format(&stroke))).collect()
    };
    let defaults = shown(&shortcuts::Overrides::default());
    let palette = defaults.iter().find(|(hint, _)| *hint == Hint::CommandPalette).unwrap();
    let settings = defaults.iter().find(|(hint, _)| *hint == Hint::Settings).unwrap();
    if cfg!(target_os = "macos") {
        assert_eq!(palette.1, "\u{2318}K");
        assert_eq!(settings.1, "\u{2318},");
    } else {
        assert_eq!(palette.1, "Ctrl+K");
        assert_eq!(settings.1, "Ctrl+,");
    }
    // A chord the user changed shows as changed.
    let mut overrides = shortcuts::Overrides::default();
    overrides.insert("workspace.open_command_palette".into(), vec!["secondary-j".into()]);
    let changed = shown(&overrides);
    let palette = changed.iter().find(|(hint, _)| *hint == Hint::CommandPalette).unwrap();
    assert!(palette.1.ends_with('J'), "{}", palette.1);
    // One with no key is left out.
    overrides.insert("workspace.open_command_palette".into(), vec![]);
    assert!(!shown(&overrides).iter().any(|(hint, _)| *hint == Hint::CommandPalette));
}

#[test]
fn the_welcome_page_reads_in_vietnamese_too() {
    for key in [
        "welcome.subtitle",
        "welcome.start",
        "welcome.recent",
        "welcome.keyboard",
        "welcome.links",
        "welcome.open_sql_file",
        "welcome.import_connections",
        "welcome.recent_empty",
        "welcome.no_drivers_title",
        "welcome.no_drivers_body",
        "welcome.install_drivers",
        "welcome.command_palette",
        "welcome.go_to_object",
        "welcome.run_query",
        "welcome.repository",
        "welcome.release_notes",
    ] {
        let en = rust_i18n::t!(key, locale = "en");
        let vi = rust_i18n::t!(key, locale = "vi");
        assert_ne!(en, key, "{key} has an English text");
        assert_ne!(vi, key, "{key} has a Vietnamese text");
        assert_ne!(en, vi, "{key} is translated");
    }
}

#[gpui::test]
fn a_failed_last_used_write_is_in_the_error_log(cx: &mut TestAppContext) {
    let (app, visual) = welcome_app(cx);
    let id = app.read_with(visual, |this, _| this.saved_connections[0].id.to_string());
    let before = app.read_with(visual, |this, _| this.error_log.log.len());
    app.update(visual, |this, _| this.welcome_touch_failed(&id, &"database is locked"));
    app.read_with(visual, |this, _| {
        assert_eq!(this.error_log.log.len(), before + 1);
        let entry = this.error_log.log.last().unwrap();
        assert!(entry.first_line.contains("database is locked"), "{entry:?}");
    });
}
