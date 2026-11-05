// Result-area reconnect state for connection-bound tabs: a Disconnected,
// Failed or dropped (`*.not_connected`) session replaces the results with a
// "Disconnected" card; Reconnect fixes every tab of the connection.

use super::connection_ui::ResultConnectionState;

use super::tab_bar::TabIcon;

const LOST_CONNECTION: &str = crate::ui_fixture::POSTGRES_ID;

fn lost_state(
    app: &gpui::Entity<AppRoot>,
    visual: &gpui::VisualTestContext,
) -> Option<ResultConnectionState> {
    app.read_with(visual, |this, _| {
        this.workspace
            .active_tab()
            .and_then(|tab| this.result_connection_state(tab))
    })
}

fn tab_generation(app: &gpui::Entity<AppRoot>, visual: &gpui::VisualTestContext, tab: u64) -> u64 {
    app.read_with(visual, |this, _| {
        this.workspace.runtime(tab).unwrap().query.generation()
    })
}

fn drop_session(app: &gpui::Entity<AppRoot>, visual: &mut gpui::VisualTestContext) {
    app.update(visual, |this, cx| {
        this.sessions.disconnect(LOST_CONNECTION);
        this.sync_connected_ids();
        cx.notify();
    });
    settle(visual);
}

fn open_sql_editor_tab(
    app: &gpui::Entity<AppRoot>,
    visual: &mut gpui::VisualTestContext,
) -> u64 {
    let tab = app.update(visual, |this, cx| {
        let id = this.workspace.open_query_tab_with_metadata(
            "Editor",
            TabMetadata {
                connection_id: LOST_CONNECTION.into(),
                database: "warehouse".into(),
                ..Default::default()
            },
        );
        this.reset_active_view();
        cx.notify();
        id
    });
    settle(visual);
    tab
}

/// The reconnect state is on screen: the full placeholder for a tab without
/// a result, the slim banner over a stale one.
fn assert_placeholder(visual: &mut gpui::VisualTestContext, what: &str) {
    assert!(
        visual.debug_bounds("connection-lost-placeholder").is_some()
            || visual.debug_bounds("connection-lost-banner").is_some(),
        "{what}: reconnect state is rendered"
    );
}

fn assert_no_placeholder(visual: &mut gpui::VisualTestContext, what: &str) {
    assert!(
        visual.debug_bounds("connection-lost-placeholder").is_none()
            && visual.debug_bounds("connection-lost-banner").is_none(),
        "{what}: no reconnect state"
    );
}

fn assert_full_placeholder(visual: &mut gpui::VisualTestContext, what: &str) {
    assert!(visual.debug_bounds("connection-lost-placeholder").is_some(), "{what}: placeholder");
    assert!(visual.debug_bounds("connection-lost-banner").is_none(), "{what}: no banner");
}

fn assert_stale_result_under_banner(visual: &mut gpui::VisualTestContext, what: &str) {
    let banner = visual.debug_bounds("connection-lost-banner").unwrap_or_else(|| panic!("{what}: banner"));
    assert!(visual.debug_bounds("connection-lost-placeholder").is_none(), "{what}: not blocking");
    let cell = visual.debug_bounds("grid-body-cell-0-0").unwrap_or_else(|| panic!("{what}: stale grid stays"));
    assert!(cell.top() >= banner.bottom() - gpui::px(1.0), "{what}: grid sits below the banner");
}

#[gpui::test]
fn healthy_and_unbound_tabs_never_show_the_reconnect_state(cx: &mut TestAppContext) {
    let (app, visual, table) = sql_table_app(cx);
    assert_eq!(lost_state(&app, visual), None);
    assert_no_placeholder(visual, "connected table tab");
    assert!(visual.debug_bounds("connection-lost-reconnect").is_none());

    let editor = open_sql_editor_tab(&app, visual);
    assert_eq!(lost_state(&app, visual), None);
    assert_no_placeholder(visual, "connected SQL tab");

    // A tab with no connection (New Query) is never held back.
    click_debug_button(visual, "global-new-query");
    settle(visual);
    assert_no_placeholder(visual, "unbound tab");
    assert_ne!(table, editor);
}

#[gpui::test]
fn disconnected_table_tab_reconnects_and_reruns_its_query(cx: &mut TestAppContext) {
    let (app, visual, table) = sql_table_app(cx);
    let ran = tab_generation(&app, visual, table);
    let rows_before = sql_table_rows(&app, visual);
    assert!(rows_before > 0, "fixture table loaded");

    drop_session(&app, visual);
    assert_eq!(lost_state(&app, visual), Some(ResultConnectionState::Disconnected));
    assert_stale_result_under_banner(visual, "disconnected table tab keeps its rows");
    assert!(visual.debug_bounds("connection-lost-edit").is_none(), "Edit only on failure");
    let reconnect = visual.debug_bounds("connection-lost-reconnect").expect("Reconnect button");

    // Real click through the production reconnect path (zero-delay fixture).
    visual.simulate_click(reconnect.center(), Default::default());
    settle(visual);

    // Connected again: placeholder gone, same query ran once more.
    assert_eq!(lost_state(&app, visual), None);
    assert_no_placeholder(visual, "after reconnect");
    assert_eq!(tab_generation(&app, visual, table), ran + 1, "table query re-ran once");
    app.read_with(visual, |this, _| {
        assert!(matches!(
            this.workspace.runtime(table).unwrap().query.status(),
            crate::query::QueryStatus::Ready { .. }
        ));
    });
    assert_eq!(sql_table_rows(&app, visual), rows_before);
}

#[gpui::test]
fn sql_tab_keeps_its_editor_under_the_placeholder_and_never_auto_runs(cx: &mut TestAppContext) {
    let (app, visual, _table) = sql_table_app(cx);
    let editor_tab = open_sql_editor_tab(&app, visual);
    type_in_editor(visual, "delete from orders");
    drop_session(&app, visual);

    // Never-run tab has no result: editor and full placeholder share the panel.
    assert_full_placeholder(visual, "disconnected SQL tab");
    assert!(visual.debug_bounds("query-editor-area").is_some(), "editor stays visible");
    assert_eq!(editor_text(&app, visual, editor_tab), "delete from orders");
    let generation = tab_generation(&app, visual, editor_tab);

    click_debug_button(visual, "connection-lost-reconnect");
    settle(visual);

    assert_eq!(lost_state(&app, visual), None);
    assert_no_placeholder(visual, "after reconnect");
    assert_eq!(
        tab_generation(&app, visual, editor_tab),
        generation,
        "a SQL editor tab never auto-runs (it may hold writes)"
    );
    assert_eq!(editor_text(&app, visual, editor_tab), "delete from orders");
    assert_editor_only(visual, "never-run tab returns to the full-height editor");
}

#[gpui::test]
fn sql_tab_with_a_previous_result_shows_the_placeholder_over_the_results(cx: &mut TestAppContext) {
    let (app, visual, _table) = sql_table_app(cx);
    let editor_tab = open_sql_editor_tab(&app, visual);
    app.update(visual, |this, cx| {
        this.run_active_tab_query(cx);
    });
    settle(visual);
    assert_results_revealed(visual, "after a run");
    drop_session(&app, visual);

    assert_placeholder(visual, "ran SQL tab, disconnected");
    assert!(visual.debug_bounds("results-header").is_some(), "Results header stays");
    assert!(visual.debug_bounds("query-editor-area").is_some());
    let generation = tab_generation(&app, visual, editor_tab);

    click_debug_button(visual, "connection-lost-reconnect");
    settle(visual);
    assert_no_placeholder(visual, "after reconnect");
    assert_results_revealed(visual, "previous result area is back");
    assert_eq!(tab_generation(&app, visual, editor_tab), generation, "no auto-run");
}

#[gpui::test]
fn failed_connection_shows_the_redacted_reason_and_edit_connection(cx: &mut TestAppContext) {
    let (app, visual, _table) = sql_table_app(cx);
    app.update(visual, |this, cx| {
        this.sessions.disconnect(LOST_CONNECTION);
        let attempt = this.sessions.begin_connect(LOST_CONNECTION).unwrap();
        this.sessions.finish_connect(
            LOST_CONNECTION,
            attempt,
            Err("could not reach postgres://admin:hunter2@db.internal/app".into()),
        );
        this.sync_connected_ids();
        cx.notify();
    });
    settle(visual);

    let Some(ResultConnectionState::Failed(reason)) = lost_state(&app, visual) else {
        panic!("expected the failed state");
    };
    assert!(!reason.contains("hunter2"), "secret redacted: {reason}");
    assert_placeholder(visual, "failed connection");
    assert!(visual.debug_bounds("connection-lost-reconnect").is_some());

    assert!(app.read_with(visual, |this, _| this.connection_ui.last_inputs.is_none()));
    click_debug_button(visual, "connection-lost-edit");
    settle(visual);
    assert!(
        app.read_with(visual, |this, _| this.connection_ui.last_inputs.is_some()),
        "Edit Connection opened the connection dialog"
    );
}

#[gpui::test]
fn not_connected_driver_failure_flips_every_tab_of_the_connection(cx: &mut TestAppContext) {
    let (app, visual, table) = sql_table_app(cx);
    let editor = open_sql_editor_tab(&app, visual);
    let rows_before = {
        app.update(visual, |this, cx| {
            this.workspace.activate_tab(table);
            this.reset_active_view();
            cx.notify();
        });
        settle(visual);
        sql_table_rows(&app, visual)
    };
    let ran = tab_generation(&app, visual, table);
    let editor_ran = tab_generation(&app, visual, editor);
    assert_no_placeholder(visual, "healthy before the failure");

    app.update(visual, |this, cx| {
        let run = this.workspace.runtime_mut(table).unwrap().query.begin().unwrap();
        this.apply_query_result(
            table,
            run,
            Err("driver rejected the request: query.not_connected: session is gone".into()),
        );
        cx.notify();
    });
    settle(visual);

    app.read_with(visual, |this, _| {
        assert_eq!(
            this.sessions.status(LOST_CONNECTION),
            ConnectionStatus::Failed(crate::sessions::CONNECTION_LOST_REASON.into())
        );
    });
    assert!(matches!(lost_state(&app, visual), Some(ResultConnectionState::Failed(_))));
    assert_placeholder(visual, "table tab after not_connected");
    assert!(visual.debug_bounds("connection-lost-edit").is_some());

    // The other tab of the same connection switches too.
    activate_tab_by_click(visual, editor);
    assert_placeholder(visual, "SQL tab of the same connection");

    // Reconnecting once fixes both: the table re-runs, the editor does not.
    click_debug_button(visual, "connection-lost-reconnect");
    settle(visual);
    assert_no_placeholder(visual, "SQL tab after reconnect");
    assert_eq!(tab_generation(&app, visual, editor), editor_ran, "SQL tab not auto-run");
    activate_tab_by_click(visual, table);
    assert_no_placeholder(visual, "table tab after reconnect");
    assert_eq!(
        tab_generation(&app, visual, table),
        ran + 2,
        "the failed run plus one re-run on activation"
    );
    assert_eq!(sql_table_rows(&app, visual), rows_before);
}

#[gpui::test]
fn other_driver_failures_keep_the_session(cx: &mut TestAppContext) {
    let (app, visual, table) = sql_table_app(cx);
    app.update(visual, |this, cx| {
        let run = this.workspace.runtime_mut(table).unwrap().query.begin().unwrap();
        this.apply_query_result(
            table,
            run,
            Err("driver rejected the request: query.not_pageable: needs a sort".into()),
        );
        cx.notify();
    });
    settle(visual);
    assert_eq!(lost_state(&app, visual), None);
    assert_no_placeholder(visual, "ordinary query failure");
}

#[gpui::test]
fn connect_in_flight_shows_connecting_not_the_reconnect_button(cx: &mut TestAppContext) {
    let (app, visual, _table) = sql_table_app(cx);
    drop_session(&app, visual);
    assert!(visual.debug_bounds("connection-lost-reconnect").is_some());

    // A connect already in flight (startup restore, sidebar click).
    let attempt = app.update(visual, |this, cx| {
        let attempt = this.sessions.begin_connect(LOST_CONNECTION).unwrap();
        cx.notify();
        attempt
    });
    settle(visual);
    assert_eq!(lost_state(&app, visual), Some(ResultConnectionState::Connecting));
    assert_placeholder(visual, "connecting");
    assert!(
        visual.debug_bounds("connection-lost-reconnect").is_none(),
        "no Reconnect button flashes while connecting"
    );
    assert!(visual.debug_bounds("connection-lost-edit").is_none());

    app.update(visual, |this, cx| {
        this.sessions.finish_connect(LOST_CONNECTION, attempt, Ok(()));
        cx.notify();
    });
    settle(visual);
    assert_no_placeholder(visual, "connected");
}

#[gpui::test]
fn mongo_find_tab_reconnects_and_reruns_its_query(cx: &mut TestAppContext) {
    let (app, visual) = context_test_app(cx, "mongo-grid", "sample");
    visual.simulate_resize(gpui::size(gpui::px(1100.0), gpui::px(850.0)));
    settle(visual);
    let (tab, connection) = app.read_with(visual, |this, _| {
        let tab = this.workspace.active_tab().unwrap();
        assert!(tab.query_mode.is_mongo_find());
        (tab.id, tab.connection_id.clone())
    });
    app.update(visual, |this, cx| {
        // The fixture may not mark the session live; make it so, then drop it.
        if let Some(attempt) = this.sessions.begin_connect(&connection) {
            this.sessions.finish_connect(&connection, attempt, Ok(()));
        }
        this.run_active_tab_query(cx);
        cx.notify();
    });
    settle(visual);
    assert_no_placeholder(visual, "connected Mongo find tab");
    let ran = tab_generation(&app, visual, tab);

    app.update(visual, |this, cx| {
        this.sessions.disconnect(&connection);
        this.sync_connected_ids();
        cx.notify();
    });
    settle(visual);
    assert_placeholder(visual, "disconnected Mongo find tab");

    // This scenario has a real connect delay: the card shows "Connecting…"
    // until the clock passes it.
    click_debug_button(visual, "connection-lost-reconnect");
    settle(visual);
    assert_eq!(lost_state(&app, visual), Some(ResultConnectionState::Connecting));
    assert!(visual.debug_bounds("connection-lost-reconnect").is_none());
    assert_eq!(tab_generation(&app, visual, tab), ran, "nothing re-ran yet");
    visual.cx.executor().advance_clock(Duration::from_secs(5));
    settle(visual);
    assert_no_placeholder(visual, "after reconnect");
    assert_eq!(tab_generation(&app, visual, tab), ran + 1, "find re-ran once");
}

#[gpui::test]
fn scenario_tabs_on_never_connected_connections_keep_their_results(cx: &mut TestAppContext) {
    let (app, visual, table) = sql_table_app(cx);
    // Fixture state: Disconnected without ever having been connected.
    app.update(visual, |this, cx| {
        this.sessions.disconnect(LOST_CONNECTION);
        this.sessions.begin_connect(LOST_CONNECTION);
        // Back to a never-connected Disconnected by clearing the attempt.
        this.sessions.disconnect(LOST_CONNECTION);
        cx.notify();
    });
    // `disconnect` on a connecting session is not a drop of a live one.
    settle(visual);
    app.read_with(visual, |this, _| {
        assert!(!this.sessions.was_dropped(LOST_CONNECTION));
        assert_eq!(this.sessions.status(LOST_CONNECTION), ConnectionStatus::Disconnected);
        assert!(this.tab_holds_results(table));
    });
    assert_eq!(lost_state(&app, visual), None);
    assert_no_placeholder(visual, "never-connected fixture connection");
    assert!(visual.debug_bounds("grid-body-cell-0-0").is_some());
}

#[gpui::test]
fn disconnected_fixture_shows_banner_over_orders_and_placeholder_on_the_empty_tab(
    cx: &mut TestAppContext,
) {
    let (app, visual) = context_test_app(cx, "result-grid", "disconnected");
    visual.simulate_resize(gpui::size(gpui::px(1100.0), gpui::px(850.0)));
    settle(visual);
    let (orders, empty) = app.read_with(visual, |this, _| {
        assert!(this.sessions.was_dropped(LOST_CONNECTION));
        let find = |title: &str| {
            this.workspace.tabs().iter().find(|tab| tab.title == title).unwrap().id
        };
        (find("orders"), find("Query"))
    });
    assert_eq!(app.read_with(visual, |this, _| this.workspace.active_tab().unwrap().id), orders);
    assert_stale_result_under_banner(visual, "orders keeps its rows");

    activate_tab_by_click(visual, empty);
    assert_full_placeholder(visual, "empty Query tab");
    assert!(visual.debug_bounds("connection-lost-reconnect").is_some());

    let ran = tab_generation(&app, visual, orders);
    click_debug_button(visual, "connection-lost-reconnect");
    settle(visual);
    visual.cx.executor().advance_clock(Duration::from_secs(5));
    settle(visual);
    assert_no_placeholder(visual, "reconnected");
    activate_tab_by_click(visual, orders);
    assert_no_placeholder(visual, "orders after reconnect");
    // The scenario's orders tab is a legacy SQL-editor tab (`PostgresTable`):
    // like every editor tab it keeps its result and never auto-runs.
    assert_eq!(tab_generation(&app, visual, orders), ran, "no auto-run for editor tabs");
    assert!(visual.debug_bounds("grid-body-cell-0-0").is_some(), "rows are still shown");
}

fn assert_card_centered_in_body(visual: &mut gpui::VisualTestContext, what: &str) {
    let body = visual.debug_bounds("connection-lost-placeholder").unwrap_or_else(|| panic!("{what}: body"));
    let card = visual.debug_bounds("connection-state-card").unwrap_or_else(|| panic!("{what}: card"));
    let tolerance = gpui::px(1.0);
    assert!((card.center().x - body.center().x).abs() <= tolerance, "{what}: x-centered {body:?} {card:?}");
    assert!((card.center().y - body.center().y).abs() <= tolerance, "{what}: y-centered {body:?} {card:?}");
    assert!(
        card.left() >= body.left() && card.right() <= body.right()
            && card.top() >= body.top() && card.bottom() <= body.bottom(),
        "{what}: card inside the body {body:?} {card:?}"
    );
}

#[gpui::test]
fn placeholder_card_is_centered_in_the_results_body_at_any_width(cx: &mut TestAppContext) {
    let (app, visual) = context_test_app(cx, "result-grid", "disconnected");
    let empty = app.read_with(visual, |this, _| {
        this.workspace.tabs().iter().find(|tab| tab.title == "Query").unwrap().id
    });
    for (width, height) in [(1100.0, 850.0), (700.0, 600.0), (480.0, 700.0)] {
        visual.simulate_resize(gpui::size(gpui::px(width), gpui::px(height)));
        settle(visual);
        app.update(visual, |this, cx| {
            this.workspace.activate_tab(empty);
            this.reset_active_view();
            cx.notify();
        });
        settle(visual);
        assert_card_centered_in_body(visual, &format!("{width}x{height}"));
        let header = visual.debug_bounds("results-header").expect("Results header");
        let body = visual.debug_bounds("connection-lost-placeholder").unwrap();
        assert!(body.top() >= header.bottom() - gpui::px(1.0), "body sits under the Results header");
    }
}

#[gpui::test]
fn connection_overview_stays_horizontally_centered(cx: &mut TestAppContext) {
    let (app, visual) = sidebar_test_app(cx);
    for width in [1100.0, 640.0] {
        visual.simulate_resize(gpui::size(gpui::px(width), gpui::px(800.0)));
        app.update(visual, |this, cx| {
            this.workspace
                .open_or_activate_connection_tab(LOST_CONNECTION, "Postgres fixture");
            this.reset_active_view();
            cx.notify();
        });
        settle(visual);
        // Disconnected overview: header, grid and the "Not connected" panel.
        let card = visual.debug_bounds("connection-overview").expect("overview");
        let panel = visual.debug_bounds("status-bar").expect("main panel width reference");
        assert!(
            (card.center().x - panel.center().x).abs() <= gpui::px(1.0),
            "{width}: overview centered: {panel:?} {card:?}"
        );
        assert!(card.left() >= panel.left() && card.right() <= panel.right());
        assert!(card.size.width <= gpui::px(880.0), "{width}: capped at its maximum width: {card:?}");
    }
}

#[gpui::test]
fn workspace_tab_icons_match_the_connection_tree(cx: &mut TestAppContext) {
    let (app, visual, table) = sql_table_app(cx);
    let (connection_tab, editor) = app.update(visual, |this, cx| {
        let connection_tab = this
            .workspace
            .open_or_activate_connection_tab(LOST_CONNECTION, "Postgres fixture");
        let editor = this.workspace.open_query_tab("Scratch");
        cx.notify();
        (connection_tab, editor)
    });
    settle(visual);
    app.read_with(visual, |this, _| {
        let icon = |id: u64| this.tab_icon(this.workspace.tab(id).unwrap());
        assert!(
            matches!(icon(connection_tab), TabIcon::Brand("icons/postgres.svg")),
            "connection tabs wear the brand art like the tree row"
        );
        assert!(
            matches!(icon(table), TabIcon::Colored("icons/table.svg", color) if color == gpui::rgb(0x22c55e)),
            "table tabs use the tree's green table glyph"
        );
        assert!(matches!(icon(editor), TabIcon::Plain("icons/square-terminal.svg")));
    });
    // The bar renders every kind of icon without panicking.
    visual.simulate_resize(gpui::size(gpui::px(1100.0), gpui::px(800.0)));
    settle(visual);
    assert!(visual.debug_bounds(tb_sel(table)).is_some());
    assert!(visual.debug_bounds(tb_sel(connection_tab)).is_some());
}
