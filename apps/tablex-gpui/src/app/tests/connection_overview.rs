// The Connection tab as an overview: header, Connection grid and the databases of the connection.
// Real clicks, typing, hovering and wheel events on the production theme.

const CO_PG: &str = crate::ui_fixture::POSTGRES_ID;
const CO_ROOT_SEED_DBS: [&str; 8] =
    ["analytics", "billing", "crm", "inventory", "postgres", "reporting", "staging", "warehouse"];

fn co_app(
    cx: &mut TestAppContext,
    fixture: FixtureKind,
    delay: Duration,
) -> (gpui::Entity<AppRoot>, &mut gpui::VisualTestContext) {
    cx.update(|cx| {
        gpui_component::init(cx);
        theme::init(cx);
    });
    let scenario = UiScenario {
        kind: crate::ui_scenario::ScenarioKind::ConnectionOverview,
        fixture,
        delay,
        timeout_delay: delay,
    };
    let (root, visual) = cx.add_window_view(|window, cx| {
        let app = cx.new(|cx| AppRoot::new_for_scenario(scenario, cx));
        gpui_component::Root::new(app, window, cx)
    });
    let app = visual
        .cx
        .read(|cx| root.read(cx).view().clone().downcast::<AppRoot>().unwrap());
    visual.simulate_resize(gpui::size(gpui::px(1200.0), gpui::px(900.0)));
    settle(visual);
    (app, visual)
}

fn co_has(visual: &mut gpui::VisualTestContext, selector: impl Into<String>) -> bool {
    visual.debug_bounds(d3_sel(selector.into())).is_some()
}

fn co_click(visual: &mut gpui::VisualTestContext, selector: impl Into<String>) {
    let selector = selector.into();
    let bounds = visual
        .debug_bounds(d3_sel(selector.clone()))
        .unwrap_or_else(|| panic!("{selector} is drawn"));
    visual.simulate_click(bounds.center(), Default::default());
    settle(visual);
    settle(visual);
}

fn co_tab(app: &gpui::Entity<AppRoot>, visual: &mut gpui::VisualTestContext) -> u64 {
    app.read_with(visual, |this, _| this.workspace.active_tab().expect("a tab").id)
}

/// The databases the active overview lists, in display order.
fn co_names(app: &gpui::Entity<AppRoot>, visual: &mut gpui::VisualTestContext) -> Vec<String> {
    let tab = co_tab(app, visual);
    app.read_with(visual, |this, _| {
        let list = this.connection_overviews[&tab].databases();
        (0..list.len()).map(|position| list.get(position).unwrap().name.clone()).collect()
    })
}

fn co_rows_drawn(visual: &mut gpui::VisualTestContext, total: usize) -> Vec<usize> {
    (0..total)
        .filter(|index| visual.debug_bounds(d3_sel(format!("connection-database-row-{index}"))).is_some())
        .collect()
}

fn co_edit_saved(
    app: &gpui::Entity<AppRoot>,
    visual: &mut gpui::VisualTestContext,
    id: &str,
    edit: impl FnOnce(&mut SavedConnection),
) {
    app.update(visual, |this, cx| {
        edit(this.saved_connections.iter_mut().find(|saved| saved.matches_id(id)).unwrap());
        cx.notify();
    });
    settle(visual);
}

fn co_open_tab(app: &gpui::Entity<AppRoot>, visual: &mut gpui::VisualTestContext, id: &str, name: &str) -> u64 {
    let tab = app.update(visual, |this, cx| {
        let tab = this.workspace.open_or_activate_connection_tab(id, name);
        this.reset_active_view();
        cx.notify();
        tab
    });
    settle(visual);
    tab
}

fn co_state(app: &gpui::Entity<AppRoot>, visual: &mut gpui::VisualTestContext, id: &str, state: ConnectionStatus) {
    app.update(visual, |this, cx| {
        let attempt = this.sessions.begin_connect(id).expect("a fresh connection starts");
        match state {
            ConnectionStatus::Failed(error) => assert!(this.sessions.finish_connect(id, attempt, Err(error))),
            ConnectionStatus::Connected => assert!(this.sessions.finish_connect(id, attempt, Ok(()))),
            _ => {}
        }
        this.sync_connected_ids();
        cx.notify();
    });
    settle(visual);
}

fn co_databases_request(app: &gpui::Entity<AppRoot>, visual: &mut gpui::VisualTestContext) -> (Option<u64>, u64) {
    app.read_with(visual, |this, _| {
        (
            this.navigator.load_generation(&LoadRequest::Databases { connection: CO_PG.into() }),
            this.navigator.catalog_revision(),
        )
    })
}

#[gpui::test]
fn the_connected_overview_shows_the_header_the_badges_the_versions_and_the_grid(cx: &mut TestAppContext) {
    let (app, visual) = co_app(cx, FixtureKind::Sample, Duration::ZERO);
    let via = app.read_with(visual, |this, _| this.via_ssh(CO_PG)).expect("the fixture's tunnel");
    let expect = [
        "connection-overview".to_string(),
        "connection-overview-title=Postgres fixture".into(),
        "connection-overview-status=Connected".into(),
        "connection-overview-subtitle=PostgreSQL 16.2.0 \u{00B7} driver 0.3.1".into(),
        "connection-type-badge".into(),
        "connection-route-badge".into(),
        "connection-read-only-badge".into(),
        format!("sidebar-status-dot-connected-{}", usize::MAX),
        "connection-overview-grid".into(),
        "connection-info-value-type=PostgreSQL".into(),
        "connection-info-value-host=fixture.invalid".into(),
        "connection-info-value-database=warehouse".into(),
        format!("connection-info-value-route={via}"),
    ];
    for selector in expect {
        assert!(co_has(visual, selector.clone()), "{selector} is drawn");
    }
    assert!(!co_has(visual, "connection-info-user"), "no user in this URI: no row");
    assert!(!co_has(visual, "connection-via-ssh"), "the old raw layout is gone");
    for button in ["connection-new-query", "connection-edit", "connection-refresh", "connection-disconnect"] {
        assert!(co_has(visual, button), "{button}");
    }
    assert!(!co_has(visual, "connection-connect"), "Disconnect, not Connect, while live");
    // Header above the grid above the databases, inside the tab.
    let top = |visual: &mut gpui::VisualTestContext, selector: &str| visual.debug_bounds(d3_sel(selector.into())).unwrap().top();
    assert!(top(visual, "connection-overview-title=Postgres fixture") < top(visual, "connection-new-query"));
    assert!(top(visual, "connection-new-query") < top(visual, "connection-overview-grid"));
    assert!(top(visual, "connection-overview-grid") < top(visual, "connection-database-list"));

    // The light theme draws the same page.
    cx_light(&app, visual);
    assert!(co_has(visual, "connection-overview-grid") && co_has(visual, "connection-database-list"));
}

fn cx_light(app: &gpui::Entity<AppRoot>, visual: &mut gpui::VisualTestContext) {
    visual.update(|_, cx| theme::apply(gpui_component::ThemeMode::Light, cx));
    app.update(visual, |_, cx| cx.notify());
    settle(visual);
}

#[gpui::test]
fn a_password_or_secret_in_the_uri_is_never_drawn(cx: &mut TestAppContext) {
    let (app, visual) = co_app(cx, FixtureKind::Sample, Duration::ZERO);
    co_edit_saved(&app, visual, CO_PG, |saved| {
        saved.uri = "postgres://admin:s3cret%21@db.example.com:6432/shop?sslmode=require&password=leak&sslpassword=leak2"
            .into();
    });
    for selector in [
        "connection-info-value-host=db.example.com:6432",
        "connection-info-value-user=admin",
        "connection-info-value-database=shop",
    ] {
        assert!(co_has(visual, selector), "{selector}");
    }
    let tab = co_tab(&app, visual);
    app.read_with(visual, |this, _| {
        let shown = format!("{:?}", this.connection_overviews[&tab].info());
        for secret in ["s3cret", "leak", "%21", "password"] {
            assert!(!shown.contains(secret), "{secret} in {shown}");
        }
    });
    // The same URI on a failed connection (the message is the driver's; the page adds nothing).
    co_edit_saved(&app, visual, CO_PG, |saved| saved.save_password = false);
    assert!(co_has(visual, "connection-info-value-user=admin"));
}

#[gpui::test]
fn the_grid_has_the_rows_that_apply_to_each_kind(cx: &mut TestAppContext) {
    use crate::ui_fixture::{MONGO_ID, SQLITE_ID};
    let (app, visual) = co_app(cx, FixtureKind::Sample, Duration::ZERO);

    co_edit_saved(&app, visual, SQLITE_ID, |saved| saved.uri = "sqlite:///data/my app.db".into());
    co_open_tab(&app, visual, SQLITE_ID, "SQLite fixture");
    assert!(co_has(visual, "connection-overview-title=SQLite fixture"));
    assert!(co_has(visual, "connection-info-value-type=SQLite"));
    assert!(co_has(visual, "connection-info-value-file=/data/my app.db"));
    assert!(!co_has(visual, "connection-info-host") && !co_has(visual, "connection-info-user"));
    assert!(co_has(visual, "connection-info-file"), "the row itself");

    co_edit_saved(&app, visual, MONGO_ID, |saved| {
        saved.uri = "mongodb+srv://ana:pw@cluster0.abc.mongodb.net/shop".into();
    });
    co_open_tab(&app, visual, MONGO_ID, "Mongo fixture");
    assert!(co_has(visual, "connection-info-value-type=MongoDB (SRV)"));
    assert!(co_has(visual, "connection-info-value-host=cluster0.abc.mongodb.net"));
    assert!(co_has(visual, "connection-info-value-user=ana"));
    assert!(co_has(visual, "connection-info-value-database=shop"));

    co_edit_saved(&app, visual, MONGO_ID, |saved| {
        saved.uri = "mongodb://u:pw@a.example:27017,b.example:27018/?replicaSet=rs0".into();
        saved.ssh_tunnel = true;
    });
    assert!(co_has(visual, "connection-info-value-hosts=a.example:27017, b.example:27018"));
    assert!(!co_has(visual, "connection-info-host"), "a list, not one host");
    // A routed connection that is not live says where it will go.
    assert!(co_has(visual, "connection-route-badge"));
    assert!(co_has(visual, "connection-info-value-route=SSH tunnel or proxy, opened when connecting"));
}

#[gpui::test]
fn the_database_list_searches_sorts_reveals_and_opens_a_scoped_query(cx: &mut TestAppContext) {
    let (app, visual) = co_app(cx, FixtureKind::Sample, Duration::ZERO);
    assert_eq!(co_names(&app, visual), CO_ROOT_SEED_DBS);
    assert!(co_has(visual, "connection-databases-title=DATABASES (8)"));
    assert_eq!(co_rows_drawn(visual, 12), (0..8).collect::<Vec<_>>());

    // Search: real typing into the input, case-insensitive.
    co_click(visual, "connection-database-search");
    visual.simulate_input("STA");
    settle(visual);
    assert_eq!(co_names(&app, visual), ["staging"]);
    assert!(co_has(visual, "connection-databases-title=DATABASES (1 of 8)"));
    assert_eq!(co_rows_drawn(visual, 12), [0]);
    visual.simulate_input("zzz");
    settle(visual);
    assert!(co_names(&app, visual).is_empty());
    assert!(co_has(visual, "connection-databases-message"), "says nothing matches");
    visual.simulate_keystrokes("secondary-a backspace");
    settle(visual);
    assert_eq!(co_names(&app, visual), CO_ROOT_SEED_DBS);

    // Sort: the button flips the order and back.
    co_click(visual, "connection-database-sort");
    let mut reversed = CO_ROOT_SEED_DBS.to_vec();
    reversed.reverse();
    assert_eq!(co_names(&app, visual), reversed);
    co_click(visual, "connection-database-sort");
    assert_eq!(co_names(&app, visual), CO_ROOT_SEED_DBS);

    // Click a row: the navigator selects it, expands the connection and the database, in view.
    let staging = NavigatorNodeId::Database { connection: CO_PG.into(), database: "staging".into() };
    app.read_with(visual, |this, _| {
        assert!(this.navigator.visible_rows().iter().all(|row| row.id != staging), "collapsed before");
    });
    co_click(visual, "connection-database-row-6");
    app.read_with(visual, |this, _| {
        assert_eq!(this.navigator.selected(), Some(&staging));
        let row = this.navigator.visible_rows().into_iter().find(|row| row.id == staging).expect("revealed");
        assert!(row.expanded, "the database is expanded");
    });

    // Hover a row: its New Query appears and opens a query scoped to that database.
    let tabs = app.read_with(visual, |this, _| this.workspace.tabs().len());
    let row = visual.debug_bounds("connection-database-row-2").expect("crm row");
    visual.simulate_mouse_move(row.center(), None, Default::default());
    settle(visual);
    co_click(visual, "connection-database-new-query-2");
    app.read_with(visual, |this, _| {
        assert_eq!(this.workspace.tabs().len(), tabs + 1);
        let tab = this.workspace.active_tab().unwrap();
        assert_eq!(tab.kind, TabKind::Query);
        assert_eq!((tab.connection_id.as_str(), tab.database.as_str()), (CO_PG, "crm"));
        assert_eq!(this.navigator.selected(), Some(&staging), "the click did not also select the row");
    });
}

#[gpui::test]
fn two_thousand_databases_draw_only_the_visible_rows(cx: &mut TestAppContext) {
    let (app, visual) = co_app(cx, FixtureKind::Sample, Duration::ZERO);
    app.update(visual, |this, cx| {
        let root = NavigatorNodeId::Connection { connection: CO_PG.into() };
        this.navigator.reset_node(&root);
        let request = this.navigator.begin_background_load(&root).expect("asked again");
        let generation = this.navigator.load_generation(&request).unwrap();
        let children = (0..2_000)
            .map(|index| navigator::database_child(CO_PG, &format!("db-{index:04}")))
            .collect();
        this.navigator.apply_load(&request, generation, Ok(children));
        cx.notify();
    });
    settle(visual);
    assert!(co_has(visual, "connection-databases-title=DATABASES (2000)"));
    let drawn = co_rows_drawn(visual, 2_000);
    assert!((10..=24).contains(&drawn.len()), "{} rows drawn of 2000", drawn.len());
    assert_eq!(drawn[0], 0);

    // The wheel moves the window of rows.
    let list = visual.debug_bounds("connection-database-list").expect("list");
    visual.simulate_event(gpui::ScrollEvent {
        position: list.center(),
        delta: gpui::ScrollDelta::Pixels(gpui::point(gpui::px(0.0), gpui::px(-900.0))),
        touch_phase: gpui::TouchPhase::Moved,
        modifiers: Default::default(),
        source: gpui::ScrollSource::Pointer,
    });
    settle(visual);
    let scrolled = co_rows_drawn(visual, 2_000);
    assert!(scrolled.len() <= 24 && scrolled[0] > 20, "{scrolled:?}");

    // Search and order still work on the whole set, and only some rows are drawn.
    co_click(visual, "connection-database-search");
    visual.simulate_input("db-19");
    settle(visual);
    assert!(co_has(visual, "connection-databases-title=DATABASES (100 of 2000)"));
    co_click(visual, "connection-database-sort");
    assert_eq!(co_names(&app, visual)[0], "db-1999");
    assert!(co_rows_drawn(visual, 100).len() <= 24);
}

#[gpui::test]
fn every_one_of_two_thousand_databases_is_reachable_by_scrolling_the_list(cx: &mut TestAppContext) {
    const TOTAL: usize = 2_000;
    let (app, visual) = co_app(cx, FixtureKind::Sample, Duration::ZERO);
    app.update(visual, |this, cx| {
        let root = NavigatorNodeId::Connection { connection: CO_PG.into() };
        this.navigator.reset_node(&root);
        let request = this.navigator.begin_background_load(&root).expect("asked again");
        let generation = this.navigator.load_generation(&request).unwrap();
        let children = (0..TOTAL)
            .map(|index| navigator::database_child(CO_PG, &format!("db-{index:04}")))
            .collect();
        this.navigator.apply_load(&request, generation, Ok(children));
        cx.notify();
    });
    settle(visual);
    assert_eq!(co_names(&app, visual).len(), TOTAL, "the overview holds every database, not a first page");
    let top = co_rows_drawn(visual, TOTAL);
    assert!((10..=24).contains(&top.len()), "{} rows drawn of {TOTAL}", top.len());
    assert!(!co_has(visual, format!("connection-database-row-{}", TOTAL - 1)), "the last row starts out of view");

    // Wheel down in steps: the window of drawn rows only ever holds a screenful, and it ends on the last database.
    let list_box = |visual: &mut gpui::VisualTestContext| visual.debug_bounds("connection-database-list").expect("list");
    let mut previous_first = 0;
    for _ in 0..200 {
        let at = list_box(visual).center();
        co_wheel(visual, at, -2_000.0);
        let drawn = co_rows_drawn(visual, TOTAL);
        assert!(drawn.len() <= 24, "{} rows drawn of {TOTAL}", drawn.len());
        let first = drawn[0];
        if first == previous_first {
            break;
        }
        previous_first = first;
    }
    let bottom = co_rows_drawn(visual, TOTAL);
    assert_eq!(bottom.last(), Some(&(TOTAL - 1)), "the wheel reached the last database: {bottom:?}");
    let last = visual.debug_bounds(d3_sel(format!("connection-database-row-{}", TOTAL - 1))).expect("the last row is drawn");
    assert!(last.bottom() <= list_box(visual).bottom(), "and it is inside the list box");

    // And back: the first one again.
    let at = list_box(visual).center();
    co_wheel(visual, at, 10_000_000.0);
    assert_eq!(co_rows_drawn(visual, TOTAL).first(), Some(&0));
}

#[gpui::test]
fn the_wheel_scrolls_the_list_first_and_the_page_once_the_list_is_at_its_end(
    cx: &mut TestAppContext,
) {
    let (app, visual) = co_app(cx, FixtureKind::Sample, Duration::ZERO);
    app.update(visual, |this, cx| {
        let root = NavigatorNodeId::Connection { connection: CO_PG.into() };
        this.navigator.reset_node(&root);
        let request = this.navigator.begin_background_load(&root).expect("asked again");
        let generation = this.navigator.load_generation(&request).unwrap();
        let children = (0..40)
            .map(|index| navigator::database_child(CO_PG, &format!("db-{index:04}")))
            .collect();
        this.navigator.apply_load(&request, generation, Ok(children));
        cx.notify();
    });
    settle(visual);
    let list = |visual: &mut gpui::VisualTestContext| {
        visual.debug_bounds("connection-database-list").expect("list")
    };
    let wheel = |visual: &mut gpui::VisualTestContext, dy: f32| {
        let at = list(visual).center();
        visual.simulate_event(gpui::ScrollEvent {
            position: at,
            delta: gpui::ScrollDelta::Pixels(gpui::point(gpui::px(0.0), gpui::px(dy))),
            touch_phase: gpui::TouchPhase::Moved,
            modifiers: Default::default(),
            source: gpui::ScrollSource::Pointer,
        });
        settle(visual);
    };

    let page_top = list(visual).origin.y;
    wheel(visual, -100.0);
    assert!(co_rows_drawn(visual, 40)[0] > 0, "the list scrolled");
    assert_eq!(list(visual).origin.y, page_top, "and the page stayed where it was");

    wheel(visual, -100_000.0);
    let at_end = co_rows_drawn(visual, 40);
    let page_top = list(visual).origin.y;
    wheel(visual, -60.0);
    assert_eq!(co_rows_drawn(visual, 40), at_end, "the list is at its end");
    assert!(list(visual).origin.y < page_top, "so the page scrolls");
}

/// A wheel event of `dy` pixels (negative scrolls down) at `at`.
fn co_wheel(visual: &mut gpui::VisualTestContext, at: gpui::Point<gpui::Pixels>, dy: f32) {
    visual.simulate_event(gpui::ScrollEvent {
        position: at,
        delta: gpui::ScrollDelta::Pixels(gpui::point(gpui::px(0.0), gpui::px(dy))),
        touch_phase: gpui::TouchPhase::Moved,
        modifiers: Default::default(),
        source: gpui::ScrollSource::Pointer,
    });
    settle(visual);
}

#[gpui::test]
fn the_wheel_scrolls_a_long_error_first_and_the_page_once_the_error_is_at_its_end(cx: &mut TestAppContext) {
    let (app, visual) = co_app(cx, FixtureKind::Disconnected, Duration::from_millis(50));
    // A window short enough for the page to scroll: its `Scrollable` then owns every wheel event
    // over it, the error box included.
    visual.simulate_resize(gpui::size(gpui::px(1200.0), gpui::px(560.0)));
    let message = (1..=40).map(|n| format!("attempt {n}: the server refused the connection")).collect::<Vec<_>>().join("\n");
    co_state(&app, visual, CO_PG, ConnectionStatus::Failed(message));
    let top = |visual: &mut gpui::VisualTestContext, selector: &str| visual.debug_bounds(d3_sel(selector.to_string())).expect(selector).origin.y;
    // How far the error text is scrolled inside its box, and where the page is.
    let inside = |visual: &mut gpui::VisualTestContext| top(visual, "connection-error-text") - top(visual, "connection-error-body");
    let page = |visual: &mut gpui::VisualTestContext| top(visual, "connection-overview-grid");
    let over_the_error = |visual: &mut gpui::VisualTestContext| {
        let error = visual.debug_bounds("connection-error-text").expect("the error box");
        gpui::point(error.center().x, error.origin.y + gpui::px(40.0))
    };
    // The page is taller than the window, and the error box is in view.
    let page_box = visual.debug_bounds("connection-overview").expect("the page");
    assert!(page_box.bottom() > gpui::px(560.0), "the page overflows the window: {page_box:?}");
    assert!(over_the_error(visual).y < gpui::px(560.0), "the error box is in view");
    let (start_page, start_inside) = (page(visual), inside(visual));

    let at = over_the_error(visual);
    co_wheel(visual, at, -60.0);
    assert!(inside(visual) > start_inside, "the error text scrolled");
    assert_eq!(page(visual), start_page, "and the page stayed where it was");

    let at = over_the_error(visual);
    co_wheel(visual, at, -100_000.0);
    let (end_inside, end_page) = (inside(visual), page(visual));
    assert!(end_inside > gpui::px(200.0), "the text went to its end: {end_inside:?}");
    let at = over_the_error(visual);
    co_wheel(visual, at, -60.0);
    assert_eq!(inside(visual), end_inside, "the error is at its end");
    assert!(page(visual) < end_page, "so the page scrolls");
}

#[gpui::test]
fn a_disconnected_overview_keeps_header_and_grid_and_connects_then_loads_the_databases_once(
    cx: &mut TestAppContext,
) {
    let (app, visual) = co_app(cx, FixtureKind::Disconnected, Duration::from_millis(50));
    for selector in [
        "connection-overview-title=Postgres fixture",
        "connection-overview-status=Disconnected",
        "connection-overview-subtitle=PostgreSQL",
        "connection-overview-grid",
        "connection-info-value-host=fixture.invalid",
        "connection-state-panel",
        "connection-panel-connect",
        "connection-connect",
    ] {
        assert!(co_has(visual, selector), "{selector}");
    }
    for selector in ["connection-database-list", "connection-database-search", "connection-disconnect", "connection-info-route"] {
        assert!(!co_has(visual, selector), "{selector} only while connected");
    }

    co_click(visual, "connection-panel-connect");
    assert!(co_has(visual, "connection-overview-status=Connecting\u{2026}"));
    assert!(co_has(visual, "connection-databases-message") && !co_has(visual, "connection-panel-connect"));
    visual.cx.executor().advance_clock(Duration::from_millis(60));
    settle(visual);
    assert!(co_has(visual, "connection-overview-status=Connected"));
    // Asked without expanding the sidebar's connection: the fixture's two databases.
    assert_eq!(co_names(&app, visual), ["reporting", "warehouse"]);
    app.read_with(visual, |this, _| {
        let connection = NavigatorNodeId::Connection { connection: CO_PG.into() };
        assert!(this.navigator.visible_rows().iter().any(|row| row.id == connection && !row.expanded));
    });

    // Nothing asks again, however many frames follow.
    let asked = co_databases_request(&app, visual);
    for _ in 0..6 {
        settle(visual);
        app.update(visual, |_, cx| cx.notify());
    }
    assert_eq!(co_databases_request(&app, visual), asked);
}

#[gpui::test]
fn a_failed_overview_shows_the_whole_error_with_retry_copy_and_edit(cx: &mut TestAppContext) {
    let (app, visual) = co_app(cx, FixtureKind::Error, Duration::from_millis(50));
    for selector in [
        "connection-overview-status=Connection failed",
        "connection-overview-grid",
        "connection-state-panel",
        "connection-error-text",
        "connection-failed-retry",
        "connection-failed-copy",
        "connection-failed-edit",
        "connection-connect",
    ] {
        assert!(co_has(visual, selector), "{selector}");
    }
    assert!(!co_has(visual, "connection-database-list"));

    // Copy writes the whole message.
    let message = app.read_with(visual, |this, _| match this.sessions.status(CO_PG) {
        ConnectionStatus::Failed(message) => message,
        other => panic!("{other:?}"),
    });
    co_click(visual, "connection-failed-copy");
    let copied = visual.update(|_, cx| cx.read_from_clipboard().and_then(|item| item.text()));
    assert_eq!(copied.as_deref(), Some(message.as_str()));

    // Edit opens the connection's dialog.
    assert!(!co_has(visual, "dialog-layer"));
    co_click(visual, "connection-failed-edit");
    assert!(co_has(visual, "dialog-layer"), "the Edit Connection dialog");
}

#[gpui::test]
fn retry_reconnects_and_a_failed_database_load_waits_for_its_own_retry(cx: &mut TestAppContext) {
    // The Error fixture connects (by name) but its catalog fails.
    let (app, visual) = co_app(cx, FixtureKind::Error, Duration::from_millis(50));
    co_click(visual, "connection-failed-retry");
    assert!(co_has(visual, "connection-overview-status=Connecting\u{2026}"));
    visual.cx.executor().advance_clock(Duration::from_millis(60));
    settle(visual);
    assert!(co_has(visual, "connection-overview-status=Connected"));
    assert!(co_has(visual, "connection-databases-retry"), "the load failed");
    assert!(co_has(visual, "connection-error-text"));
    assert!(!co_has(visual, "connection-database-list"));

    // Frames do not retry it.
    let failed = co_databases_request(&app, visual);
    assert!(failed.0.is_some());
    for _ in 0..6 {
        settle(visual);
        app.update(visual, |_, cx| cx.notify());
    }
    assert_eq!(co_databases_request(&app, visual), failed, "asked once");

    // Retry asks once more.
    co_click(visual, "connection-databases-retry");
    let again = co_databases_request(&app, visual);
    assert_ne!(again.0, failed.0, "a new request");
    assert!(co_has(visual, "connection-databases-retry"), "which fails the same way");
    for _ in 0..3 {
        settle(visual);
    }
    assert_eq!(co_databases_request(&app, visual), again);
}

#[gpui::test]
fn password_required_connecting_missing_and_unsupported_states(cx: &mut TestAppContext) {
    let (app, visual) = co_app(cx, FixtureKind::Disconnected, Duration::from_millis(50));
    co_state(&app, visual, CO_PG, ConnectionStatus::Failed(crate::connections::missing_password_error()));
    for selector in [
        "connection-overview-status=Password required",
        "connection-overview-grid",
        "connection-edit-password",
    ] {
        assert!(co_has(visual, selector), "{selector}");
    }
    assert!(!co_has(visual, "connection-failed-retry") && !co_has(visual, "connection-database-list"));
    co_click(visual, "connection-edit-password");
    assert!(co_has(visual, "dialog-layer"));

    // Connecting: the grid, a message, no Connect.
    let (app, visual) = co_app(cx, FixtureKind::Loading, Duration::from_millis(50));
    assert!(co_has(visual, "connection-overview-status=Connecting\u{2026}"));
    assert!(co_has(visual, "connection-overview-grid") && co_has(visual, "connection-databases-message"));
    assert!(!co_has(visual, "connection-panel-connect") && !co_has(visual, "connection-database-list"));
    assert!(co_has(visual, format!("sidebar-status-dot-connecting-{}", usize::MAX)));

    // A deleted connection and an unsupported one keep the simple card.
    co_open_tab(&app, visual, "00000000-0000-4000-8000-000000000000", "Gone");
    assert!(co_has(visual, "connection-state-card") && !co_has(visual, "connection-overview"));
    co_open_tab(&app, visual, CO_PG, "Postgres fixture");
    assert!(co_has(visual, "connection-overview"));
    co_edit_saved(&app, visual, CO_PG, |saved| saved.disabled = true);
    assert!(co_has(visual, "connection-state-card") && !co_has(visual, "connection-overview"));
}

#[gpui::test]
fn the_header_buttons_do_what_they_say(cx: &mut TestAppContext) {
    let (app, visual) = co_app(cx, FixtureKind::Sample, Duration::ZERO);

    // New Query: a query tab on this connection.
    let overview = co_tab(&app, visual);
    co_click(visual, "connection-new-query");
    app.read_with(visual, |this, _| {
        let tab = this.workspace.active_tab().unwrap();
        assert_eq!((tab.kind, tab.connection_id.as_str()), (TabKind::Query, CO_PG));
        assert_ne!(tab.id, overview);
    });
    app.update(visual, |this, cx| {
        this.workspace.activate_tab(overview);
        this.reset_active_view();
        cx.notify();
    });
    settle(visual);

    // Edit: the connection's dialog.
    co_click(visual, "connection-edit");
    assert!(co_has(visual, "dialog-layer"));
    visual.simulate_keystrokes("escape");
    settle(visual);

    // Refresh: the catalog is dropped and asked for again. Without the fixture a driver is needed,
    // so the new request fails: the retry panel is the proof it ran.
    app.update(visual, |this, _| this.scenario_ui.service = None);
    assert!(!co_has(visual, "connection-databases-retry"));
    co_click(visual, "connection-refresh");
    assert!(co_has(visual, "connection-databases-retry"), "reloaded");

    // Disconnect: the session ends, the tree resets, the page offers Connect.
    let (app, visual) = co_app(cx, FixtureKind::Sample, Duration::ZERO);
    co_click(visual, "connection-disconnect");
    app.read_with(visual, |this, _| {
        assert_eq!(this.sessions.status(CO_PG), ConnectionStatus::Disconnected);
        assert!(this.navigator.connection_databases(CO_PG).is_empty());
    });
    assert!(co_has(visual, "connection-overview-status=Disconnected"));
    assert!(co_has(visual, "connection-panel-connect") && co_has(visual, "connection-connect"));
    assert!(!co_has(visual, "connection-database-list") && !co_has(visual, "connection-disconnect"));
    assert!(co_has(visual, "connection-info-value-host=fixture.invalid"), "the grid stays");
}
