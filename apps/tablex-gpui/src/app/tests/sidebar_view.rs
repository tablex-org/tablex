// The left pane is a cached view (`SidebarView`): a window draw that is not about it replays its last frame, and
// every change to what it shows has to bring a render. These tests never refresh the window (`settle` does, and
// that re-renders every cached view, so it would hide a stale pane): they let the queued draws run and look at
// what is drawn, with real clicks, keys and typing where the product has them.

fn sv_park(visual: &mut gpui::VisualTestContext) {
    visual.run_until_parked();
}

fn sv_renders() -> u64 {
    super::navigator_ui::sidebar_renders()
}

fn sv_row_drawn(visual: &mut gpui::VisualTestContext, index: usize) -> bool {
    visual.debug_bounds(d3_sel(format!("sidebar-row-{index}"))).is_some()
}

/// How many rows of the tree are drawn (the list draws the ones in view).
fn sv_rows_drawn(visual: &mut gpui::VisualTestContext) -> usize {
    (0..200).filter(|index| sv_row_drawn(visual, *index)).count()
}

fn sv_index(app: &gpui::Entity<AppRoot>, visual: &mut gpui::VisualTestContext, id: &NavigatorNodeId) -> usize {
    app.read_with(visual, |this, _| this.navigator.visible_index(id)).expect("a visible row")
}

/// A window with a tab that has results (a cached view of its own) beside the sidebar, and one that has an editor.
fn sv_app(cx: &mut TestAppContext) -> (gpui::Entity<AppRoot>, &mut gpui::VisualTestContext, u64, u64) {
    let (app, visual, ..) = oa_pg(cx);
    let (grid_tab, editor_tab) = app.update(visual, |this, cx| {
        let editor_tab = this.workspace.open_query_tab("typing");
        let grid_tab = this.workspace.open_ran_query_tab("grid");
        this.set_active_grid(crate::grid::GridState::new(GridContentSource::Arrow(
            crate::grid_arrow::ArrowGridSource::new(vec![std::sync::Arc::new(wide_grid_batch(60, 40))]),
        )));
        cx.notify();
        (grid_tab, editor_tab)
    });
    sv_park(visual);
    (app, visual, grid_tab, editor_tab)
}

#[gpui::test]
fn draws_that_are_not_about_the_sidebar_replay_it(cx: &mut TestAppContext) {
    let (app, visual, grid_tab, editor_tab) = sv_app(cx);
    assert!(sv_rows_drawn(visual) > 3, "the tree is drawn");
    let before = sv_renders();

    // Typing in the editor: the window draws for every key, the sidebar is not rendered.
    app.update(visual, |this, cx| {
        this.workspace.activate_tab(editor_tab);
        cx.notify();
    });
    sv_park(visual);
    visual.update(|window, cx| {
        let editor = app.read(cx).query_editors.by_tab[&editor_tab].clone();
        let handle = editor.read(cx).focus_handle(cx);
        window.focus(&handle, cx);
    });
    sv_park(visual);
    // The first key of a tab may draw the sidebar once (the tab's own bookkeeping); the rest do not.
    visual.simulate_input("s");
    sv_park(visual);
    let typing_from = sv_renders();
    visual.simulate_input("elect 1");
    sv_park(visual);
    assert_eq!(app.read_with(visual, |this, _| this.workspace.tab(editor_tab).map(|tab| tab.sql.clone())), Some("select 1".into()));
    assert_eq!(sv_renders(), typing_from, "typing renders no sidebar row");

    // Scrolling the results grid (a notification of the tab only).
    app.update(visual, |this, cx| {
        this.workspace.activate_tab(grid_tab);
        cx.notify();
    });
    sv_park(visual);
    let grid_from = sv_renders();
    let tab_renders = results::surface::RENDERS.with(|renders| renders.borrow().get(&grid_tab).copied().unwrap_or(0));
    for step in 1..=5 {
        app.update(visual, |this, cx| {
            let base = this.view(grid_tab).grid_scroll.0.borrow().base_handle.clone();
            base.set_offset(gpui::point(base.offset().x, gpui::px(-30.0 * step as f32)));
            this.notify_tab(grid_tab, cx);
        });
        sv_park(visual);
    }
    assert!(results::surface::RENDERS.with(|renders| renders.borrow().get(&grid_tab).copied().unwrap_or(0)) > tab_renders, "the grid was drawn");
    assert_eq!(sv_renders(), grid_from, "scrolling the grid renders no sidebar row");
    assert!(sv_renders() >= before);
}

#[gpui::test]
fn a_notification_of_the_root_that_changes_nothing_renders_no_sidebar_row(cx: &mut TestAppContext) {
    // The fps counter notifies the chrome every half second, and any code may notify the root: the pane is told
    // only when what it draws moved (the epoch), so these draws replay it.
    let (app, visual, ..) = sv_app(cx);
    sv_park(visual);
    sv_park(visual);
    let before = sv_renders();
    for _ in 0..3 {
        app.update(visual, |_, cx| cx.notify());
        sv_park(visual);
        app.update(visual, |this, cx| this.notify_chrome(cx));
        sv_park(visual);
    }
    assert_eq!(sv_renders(), before, "no sidebar row was rendered");
    // And the same notification does bring a change: one render.
    app.update(visual, |this, cx| {
        this.sidebar.notice = Some("a notice".into());
        cx.notify();
    });
    sv_park(visual);
    assert!(sv_renders() > before, "a change told with the same notification renders the pane");
}

#[gpui::test]
fn the_connecting_dot_pulses_without_rendering_the_sidebar(cx: &mut TestAppContext) {
    let (app, visual, ..) = sv_app(cx);
    let postgres = NavigatorNodeId::Connection { connection: crate::ui_fixture::POSTGRES_ID.into() };
    let index = sv_index(&app, visual, &postgres);
    app.update(visual, |this, cx| {
        this.sessions.disconnect(crate::ui_fixture::POSTGRES_ID);
        this.sessions.begin_connect(crate::ui_fixture::POSTGRES_ID);
        this.sync_sidebar_connections(cx);
    });
    sv_park(visual);
    assert!(ns_has(visual, format!("sidebar-status-dot-connecting-{index}")), "the connecting dot is drawn");
    let before = sv_renders();
    // Two seconds of the pulse: the window animates the dot itself.
    for _ in 0..20 {
        visual.cx.executor().advance_clock(Duration::from_millis(100));
        sv_park(visual);
    }
    assert!(ns_has(visual, format!("sidebar-status-dot-connecting-{index}")));
    assert_eq!(sv_renders(), before, "the pulse does not render the sidebar");
}

#[gpui::test]
fn a_connection_that_connects_drops_or_is_lost_shows_at_once_whichever_way_the_window_is_told(cx: &mut TestAppContext) {
    let (app, visual, grid_tab, _) = sv_app(cx);
    let postgres = NavigatorNodeId::Connection { connection: crate::ui_fixture::POSTGRES_ID.into() };
    let index = sv_index(&app, visual, &postgres);
    let id = crate::ui_fixture::POSTGRES_ID;
    let dot = |kind: &str| format!("sidebar-status-dot-{kind}-{index}");

    // Disconnected: no dot.
    app.update(visual, |this, cx| {
        this.sessions.disconnect(id);
        cx.notify();
    });
    sv_park(visual);
    assert!(!ns_has(visual, dot("connected")) && !ns_has(visual, dot("connecting")));

    // Connecting, told with the root's notification, as a connect does.
    app.update(visual, |this, cx| {
        this.sessions.begin_connect(id).expect("a fresh connection starts");
        this.sync_sidebar_connections(cx);
    });
    sv_park(visual);
    assert!(ns_has(visual, dot("connecting")), "connecting");

    // Connected, told to the chrome only.
    app.update(visual, |this, cx| {
        let attempt = this.sessions.generation(id).expect("a connect in flight");
        assert!(this.sessions.finish_connect(id, attempt, Ok(())));
        this.notify_chrome(cx);
    });
    sv_park(visual);
    assert!(ns_has(visual, dot("connected")) && !ns_has(visual, dot("connecting")), "connected");

    // Lost, told to a tab only (a query of that tab found the connection gone).
    app.update(visual, |this, cx| {
        assert!(this.sessions.mark_lost_because(id, "The connection was lost.".into()));
        this.notify_tab(grid_tab, cx);
    });
    sv_park(visual);
    assert!(!ns_has(visual, dot("connected")), "no longer connected");
    assert!(ns_has(visual, format!("sidebar-problem-{index}")), "the failure marker shows");

    // Connected again, then released, told to a tab only.
    app.update(visual, |this, cx| {
        let attempt = this.sessions.begin_connect(id).expect("a failed connection starts again");
        assert!(this.sessions.finish_connect(id, attempt, Ok(())));
        this.notify_tab(grid_tab, cx);
    });
    sv_park(visual);
    assert!(ns_has(visual, dot("connected")), "connected again");
    app.update(visual, |this, cx| {
        this.sessions.disconnect(id);
        this.notify_tab(grid_tab, cx);
    });
    sv_park(visual);
    assert!(!ns_has(visual, dot("connected")), "released");
}

#[gpui::test]
fn a_connection_changed_with_no_notification_at_all_shows_on_the_next_draw_of_the_root(cx: &mut TestAppContext) {
    // A path that changes a connection and ends without telling the window: whatever draws the root next brings the
    // sidebar up to date in that same frame. The draw here is the one a tab's own view causes (the root is its
    // ancestor, so it re-renders, but nothing notified `AppRoot`, so its observer does not run): a notification sent
    // from inside a draw is dropped by GPUI, so the root's render has to draw the pane itself. Exactly one draw.
    let (app, visual, grid_tab, _) = sv_app(cx);
    let postgres = NavigatorNodeId::Connection { connection: crate::ui_fixture::POSTGRES_ID.into() };
    let index = sv_index(&app, visual, &postgres);
    let id = crate::ui_fixture::POSTGRES_ID;
    app.update(visual, |this, cx| {
        this.sessions.disconnect(id);
        this.notify_chrome(cx);
    });
    sv_park(visual);
    assert!(!ns_has(visual, format!("sidebar-status-dot-connected-{index}")));
    app.update(visual, |this, cx| {
        let attempt = this.sessions.begin_connect(id).expect("starts");
        this.sessions.finish_connect(id, attempt, Ok(()));
        // Not a notification of the root, of the chrome or of the sidebar: only the tab's result view.
        let surface = this.tab_views[&grid_tab].surface.as_ref().expect("the tab is shown").entity_id();
        let app: &mut gpui::App = cx;
        app.notify(surface);
    });
    sv_park(visual);
    assert!(ns_has(visual, format!("sidebar-status-dot-connected-{index}")), "the draw that found the change drew it");
}

#[gpui::test]
fn a_storage_warning_found_by_the_root_render_shows_under_the_sidebar_in_that_frame(cx: &mut TestAppContext) {
    // The warning is taken and the notice written while the root renders (where a notification is dropped), and
    // nothing notifies the sidebar: the very draw that took it has to show it. The draw is the one a tab's own view
    // causes; one park.
    let (app, visual, grid_tab, _) = sv_app(cx);
    let message = format!("the saved data in /old/{}.db could not be moved", uuid::Uuid::new_v4());
    let line = format!("sidebar-line-{message}");
    assert!(!ns_has(visual, line.clone()));
    error_log_view::RENDER_WARNINGS.with(|warnings| warnings.borrow_mut().push(message.clone()));
    app.update(visual, |this, cx| {
        let surface = this.tab_views[&grid_tab].surface.as_ref().expect("the tab is shown").entity_id();
        let app: &mut gpui::App = cx;
        app.notify(surface);
    });
    sv_park(visual);
    assert_eq!(app.read_with(visual, |this, _| this.sidebar.notice.clone()), Some(message), "the render took it");
    assert!(ns_has(visual, line), "and the pane drew it in that frame");
}

#[gpui::test]
fn dragging_a_connection_over_the_rows_targets_only_the_row_under_the_pointer(cx: &mut TestAppContext) {
    // GPUI sends the move of a drag to every row that listens, wherever the pointer is: only the row under it
    // may become the drop target, and the pane renders when that changes, not for every mouse move.
    cx.dispatcher.allow_parking();
    let (app, visual, ..) = sv_app(cx);
    let connections: Vec<usize> = app.read_with(visual, |this, _| {
        (0..this.navigator.visible_row_count())
            .filter(|index| this.navigator.visible_row_at(*index).is_some_and(|row| matches!(row.id, NavigatorNodeId::Connection { .. })))
            .collect()
    });
    let connections: Vec<usize> = connections.into_iter().filter(|index| sv_row_drawn(visual, *index)).collect();
    assert!(connections.len() >= 2, "two connections are drawn to drag between: {connections:?}");
    // Dragged from the second row onto the first: the first row's listener runs before the second's, so a drop
    // target set by every listener would end on the second.
    let (source, over) = (connections[1], connections[0]);
    let target_id = app.read_with(visual, |this, _| this.navigator.visible_row_at(over).unwrap().id.clone());
    let at = |visual: &mut gpui::VisualTestContext, index: usize| visual.debug_bounds(d3_sel(format!("sidebar-row-{index}"))).expect("a row").center();
    let (from, on) = (at(visual, source), at(visual, over));
    let target = |visual: &mut gpui::VisualTestContext| app.read_with(visual, |this, _| this.navigator_ui.drop_target.clone());

    visual.simulate_mouse_down(from, gpui::MouseButton::Left, Default::default());
    visual.simulate_mouse_move(gpui::point(from.x, from.y + gpui::px(3.0)), gpui::MouseButton::Left, Default::default());
    sv_park(visual);
    visual.simulate_mouse_move(on, gpui::MouseButton::Left, Default::default());
    sv_park(visual);
    assert_eq!(target(visual), Some(target_id.clone()), "the row under the pointer is the drop target, not the last row that listened");

    // More moves inside the same row write nothing: the epoch, which moves with every write of the state the pane
    // draws (and so with every notification of it), stays. (A drag draws the window for each move whatever this
    // does, so the render count says nothing here.)
    let epoch = |visual: &mut gpui::VisualTestContext| app.read_with(visual, |this, _| this.sidebar_epoch.now());
    let settled = epoch(visual);
    for step in 1..=4 {
        visual.simulate_mouse_move(gpui::point(on.x + gpui::px(step as f32), on.y), gpui::MouseButton::Left, Default::default());
        sv_park(visual);
    }
    assert_eq!(target(visual), Some(target_id), "still that row");
    assert_eq!(epoch(visual), settled, "a move within the row changes nothing");

    // The next row takes over with one change.
    let next = source;
    let onto = at(visual, next);
    let next_id = app.read_with(visual, |this, _| this.navigator.visible_row_at(next).unwrap().id.clone());
    visual.simulate_mouse_move(onto, gpui::MouseButton::Left, Default::default());
    sv_park(visual);
    assert_eq!(target(visual), Some(next_id));
    assert_eq!(epoch(visual), settled + 1, "one change for the new row");
    visual.simulate_mouse_up(onto, gpui::MouseButton::Left, Default::default());
    sv_park(visual);
}

#[gpui::test]
fn expanding_selecting_and_the_keyboard_show_in_the_rows_with_real_input(cx: &mut TestAppContext) {
    let (app, visual, ..) = sv_app(cx);
    let schema = NavigatorNodeId::Schema {
        connection: crate::ui_fixture::POSTGRES_ID.into(),
        database: "warehouse".into(),
        schema: "public".into(),
    };
    let at = sv_index(&app, visual, &schema);
    let rows = app.read_with(visual, |this, _| this.navigator.visible_row_count());
    let drawn = sv_rows_drawn(visual);
    assert!(app.read_with(visual, |this, _| this.navigator.visible_row_at(at).is_some_and(|row| row.expanded)), "the schema is open");

    // A click on its disclosure closes it: the rows under it leave the list.
    let before = sv_renders();
    let disclosure = visual.debug_bounds(d3_sel(format!("nav-disclosure-{at}"))).expect("the disclosure");
    visual.simulate_click(disclosure.center(), Default::default());
    sv_park(visual);
    assert!(sv_renders() > before, "the click rendered the sidebar");
    let closed = app.read_with(visual, |this, _| this.navigator.visible_row_count());
    assert!(closed < rows, "{closed} rows after, {rows} before");
    assert!(!sv_row_drawn(visual, closed), "no row is drawn past the end of the list");
    assert!(sv_row_drawn(visual, closed - 1));

    // And opens it again.
    let disclosure = visual.debug_bounds(d3_sel(format!("nav-disclosure-{at}"))).expect("the disclosure");
    visual.simulate_click(disclosure.center(), Default::default());
    sv_park(visual);
    assert_eq!(app.read_with(visual, |this, _| this.navigator.visible_row_count()), rows);
    assert_eq!(sv_rows_drawn(visual), drawn, "the rows are back");

    // A click on a row selects it (and renders it selected); the arrow keys move the selection.
    let before = sv_renders();
    let row = visual.debug_bounds(d3_sel(format!("sidebar-row-{at}"))).expect("the row");
    visual.simulate_click(row.center(), Default::default());
    sv_park(visual);
    assert_eq!(app.read_with(visual, |this, _| this.navigator.selected().cloned()), Some(schema.clone()));
    assert!(sv_renders() > before, "a selection renders the sidebar");
    let before = sv_renders();
    visual.simulate_keystrokes("down");
    sv_park(visual);
    let moved = app.read_with(visual, |this, _| this.navigator.selected().cloned());
    assert_ne!(moved, Some(schema), "the key moved the selection");
    assert!(sv_renders() > before, "a key in the sidebar renders it");
}

#[gpui::test]
fn typing_in_the_search_field_filters_the_rows_as_it_types(cx: &mut TestAppContext) {
    let (app, visual, ..) = sv_app(cx);
    let all = sv_rows_drawn(visual);
    visual.update(|window, cx| {
        let input = app.update(cx, |this, cx| this.sidebar_search_input(window, cx));
        let handle = input.read(cx).focus_handle(cx);
        window.focus(&handle, cx);
    });
    sv_park(visual);
    visual.simulate_input("zzz-no-such-object");
    visual.cx.executor().advance_clock(super::remote_search::DEBOUNCE + Duration::from_millis(10));
    sv_park(visual);
    let filtered = sv_rows_drawn(visual);
    assert!(filtered < all, "{filtered} rows drawn after the filter, {all} before");
    assert_eq!(app.read_with(visual, |this, _| this.navigator.search_options().query.clone()), "zzz-no-such-object");

    // The "active connections only" toggle is a real button: with nothing connected it empties the tree.
    app.update(visual, |this, cx| {
        let mut options = this.navigator.search_options().clone();
        options.query.clear();
        this.update_sidebar_search(options, cx);
    });
    sv_park(visual);
    assert_eq!(sv_rows_drawn(visual), all, "the filter is gone");
}

#[gpui::test]
fn the_table_stats_setting_shows_and_hides_the_row_estimates_without_a_refresh(cx: &mut TestAppContext) {
    let (app, visual, ..) = sv_app(cx);
    let orders = oa_pg_object("orders");
    let index = sv_index(&app, visual, &orders);
    let label = move |visual: &mut gpui::VisualTestContext| ns_has(visual, format!("sidebar-stats-{index}"));
    assert!(!label(visual), "off by default");
    for show in [true, false, true] {
        app.update(visual, |this, cx| {
            let mut next = (**this.settings).clone();
            next.navigation.show_table_stats = show;
            this.apply_settings(next, None, cx);
        });
        for _ in 0..3 {
            sv_park(visual);
        }
        assert_eq!(label(visual), show, "show_table_stats = {show}");
    }
}

#[gpui::test]
fn hiding_and_showing_the_sidebar_and_resizing_the_window_follow_at_once(cx: &mut TestAppContext) {
    let (app, visual, ..) = sv_app(cx);
    let full = sv_rows_drawn(visual);
    assert!(full > 8, "{full}");

    // A shorter window draws fewer rows (the list's bounds changed).
    visual.simulate_resize(gpui::size(gpui::px(1280.0), gpui::px(400.0)));
    sv_park(visual);
    let short = sv_rows_drawn(visual);
    assert!(short < full, "{short} rows in a short window, {full} in a tall one");
    visual.simulate_resize(gpui::size(gpui::px(1280.0), gpui::px(900.0)));
    sv_park(visual);
    assert_eq!(sv_rows_drawn(visual), full);

    // Hide: the rail stays, the rows go; show: they come back.
    let hide = visual.debug_bounds("sidebar-hide").expect("the hide button");
    visual.simulate_click(hide.center(), Default::default());
    sv_park(visual);
    assert!(app.read_with(visual, |this, _| this.shell.sidebar_collapsed));
    assert_eq!(sv_rows_drawn(visual), 0);
    let expand = visual.debug_bounds("expand-sidebar").expect("the rail button");
    visual.simulate_click(expand.center(), Default::default());
    sv_park(visual);
    assert_eq!(sv_rows_drawn(visual), full);
}

#[gpui::test]
fn a_theme_switch_renders_the_sidebar_in_the_new_theme(cx: &mut TestAppContext) {
    let (_, visual, ..) = sv_app(cx);
    let before = sv_renders();
    visual.update(|_, cx| theme::apply(gpui_component::theme::ThemeMode::Light, cx));
    sv_park(visual);
    assert!(sv_renders() > before, "the theme refreshes every cached view");
    visual.update(|_, cx| theme::apply(gpui_component::theme::ThemeMode::Dark, cx));
    sv_park(visual);
}

#[gpui::test]
fn the_active_connections_filter_follows_a_connection_lost_through_a_tab(cx: &mut TestAppContext) {
    // The tree filters on a copy of the live set (`connected_ids`); a path that changes a session and tells only a
    // tab used to leave the copy, and with it the rows, as they were.
    let (app, visual, grid_tab, _) = sv_app(cx);
    let id = crate::ui_fixture::POSTGRES_ID;
    let postgres = NavigatorNodeId::Connection { connection: id.into() };
    app.update(visual, |this, cx| {
        this.sessions.disconnect(id);
        let attempt = this.sessions.begin_connect(id).expect("a fresh connection starts");
        assert!(this.sessions.finish_connect(id, attempt, Ok(())));
        this.sync_connected_ids();
        let mut options = this.navigator.search_options().clone();
        options.connected_only = true;
        this.update_sidebar_search(options, cx);
    });
    sv_park(visual);
    assert!(app.read_with(visual, |this, _| this.navigator.visible_index(&postgres)).is_some(), "a live connection stays listed");
    let listed = sv_rows_drawn(visual);

    app.update(visual, |this, cx| {
        this.sessions.disconnect(id);
        this.notify_tab(grid_tab, cx);
    });
    sv_park(visual);
    assert!(app.read_with(visual, |this, _| this.navigator.visible_index(&postgres)).is_none(), "a dropped connection leaves the filtered tree");
    assert!(sv_rows_drawn(visual) < listed, "its rows are gone from the drawn list");

    // The same when the window is told through the chrome, which used to mark the sessions as seen without the copy.
    app.update(visual, |this, cx| {
        let attempt = this.sessions.begin_connect(id).expect("a dropped connection starts again");
        assert!(this.sessions.finish_connect(id, attempt, Ok(())));
        this.notify_chrome(cx);
    });
    sv_park(visual);
    assert!(app.read_with(visual, |this, _| this.navigator.visible_index(&postgres)).is_some(), "a connection that comes up is listed");
    app.update(visual, |this, cx| {
        this.sessions.disconnect(id);
        this.notify_chrome(cx);
    });
    sv_park(visual);
    assert!(app.read_with(visual, |this, _| this.navigator.visible_index(&postgres)).is_none(), "and leaves again");
}

#[gpui::test]
fn a_change_to_the_tree_told_to_a_tab_only_still_shows_in_the_rows(cx: &mut TestAppContext) {
    // Every `&mut` borrow of the state the pane draws moves the sidebar epoch, whichever view the code then notifies.
    let (app, visual, grid_tab, _) = sv_app(cx);
    let schema = NavigatorNodeId::Schema {
        connection: crate::ui_fixture::POSTGRES_ID.into(),
        database: "warehouse".into(),
        schema: "public".into(),
    };
    let open = sv_rows_drawn(visual);

    // The tree: closing the schema, told to a tab.
    app.update(visual, |this, cx| {
        this.navigator.toggle_expansion(&schema);
        this.notify_tab(grid_tab, cx);
    });
    sv_park(visual);
    assert!(sv_rows_drawn(visual) < open, "the closed schema's rows left the drawn list");

    // The selection: the row is drawn selected (a different background, so a different frame).
    let renders = sv_renders();
    app.update(visual, |this, cx| {
        this.navigator.select(schema.clone());
        this.notify_tab(grid_tab, cx);
    });
    sv_park(visual);
    assert!(sv_renders() > renders, "a selection told to a tab renders the sidebar");

    // The pane's notice line, the drop highlight, the stats setting and a connection record: one render each.
    type Touch = fn(&mut AppRoot);
    let touches: [(&str, Touch); 7] = [
        ("notice", |this| this.sidebar.notice = Some("a notice".into())),
        ("drop target", |this| this.navigator_ui.drop_target = None),
        ("settings", |this| *this.settings = Arc::new((**this.settings).clone())),
        ("saved connections", |this| this.saved_connections.truncate(usize::MAX)),
        ("connection policy", |this| {
            let _ = &mut *this.connection_policy;
        }),
        ("table stats", |this| {
            let _ = &mut *this.table_stats;
        }),
        ("tunnels", |this| {
            let _ = &mut *this.tunnel_ui;
        }),
    ];
    for (what, touch) in touches {
        let renders = sv_renders();
        app.update(visual, |this, cx| {
            touch(this);
            this.notify_tab(grid_tab, cx);
        });
        sv_park(visual);
        assert!(sv_renders() > renders, "{what}: a change told to a tab renders the sidebar");
    }
}

#[gpui::test]
fn reading_the_state_the_pane_draws_does_not_render_it(cx: &mut TestAppContext) {
    // Only a `&mut` borrow moves the epoch: the reads that run on every draw (and the scroll and typing paths that
    // notify a tab) keep the pane cached.
    let (app, visual, grid_tab, _) = sv_app(cx);
    let renders = sv_renders();
    app.update(visual, |this, cx| {
        let _ = (this.navigator.visible_row_count(), this.navigator.selected().cloned(), this.sidebar.notice.clone());
        let _ = (this.saved_connections.len(), this.settings.navigation.show_table_stats, this.table_stats.len());
        let _ = (this.navigator_ui.drop_target.clone(), this.connection_policy.revision(), this.tunnel_ui.live.len());
        this.notify_tab(grid_tab, cx);
    });
    sv_park(visual);
    assert_eq!(sv_renders(), renders, "reads leave the sidebar cached");
}

/// The state the left pane draws: the `AppRoot` fields held in `sidebar_watch::Watched` (see its docs for what each one
/// holds). Adding to this list is how a new piece of sidebar state is declared.
const SIDEBAR_WATCHED: [&str; 8] =
    ["navigator", "sidebar", "navigator_ui", "saved_connections", "connection_policy", "settings", "table_stats", "tunnel_ui"];

fn sidebar_sources() -> Vec<(std::path::PathBuf, String)> {
    fn sources(dir: &std::path::Path, out: &mut Vec<std::path::PathBuf>) {
        for entry in std::fs::read_dir(dir).unwrap() {
            let path = entry.unwrap().path();
            if path.is_dir() {
                if path.file_name().is_some_and(|name| name != "tests") {
                    sources(&path, out);
                }
            } else if path.extension().is_some_and(|ext| ext == "rs") {
                out.push(path);
            }
        }
    }
    let mut files = Vec::new();
    sources(&std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("src"), &mut files);
    assert!(files.len() > 100, "the scan found the sources");
    files.into_iter().map(|file| (file.clone(), std::fs::read_to_string(&file).unwrap())).collect()
}

/// The fields of `AppRoot` as `(name, type)`.
fn app_root_fields() -> Vec<(String, String)> {
    let text = std::fs::read_to_string(std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("src/app/mod.rs")).unwrap();
    let body = text.split("pub struct AppRoot {\n").nth(1).expect("the struct").split("\n}\n").next().unwrap();
    body.lines()
        .filter(|line| line.starts_with("    ") && !line.starts_with("     ") && !line.trim_start().starts_with("//") && !line.trim_start().starts_with('#'))
        .filter_map(|line| line.trim().split_once(": ").map(|(name, ty)| (name.trim_start_matches("pub ").trim_start_matches("pub(super) ").to_owned(), ty.to_owned())))
        .collect()
}

/// A change to what the cached left pane draws has to reach it. The state it draws is held in `Watched` fields of
/// `AppRoot`, so no code can borrow it mutably without moving the epoch that `sync_sidebar` compares. This keeps that
/// true: the declared set is exactly the watched fields (a field turned back into a plain one, or a new one that the
/// pane draws and nobody declared, fails here), the pane's render reads no other field of `AppRoot`, and nothing but
/// the notification path writes the marker that says the pane is current.
#[test]
fn the_state_the_sidebar_draws_is_only_written_through_the_watch() {
    let fields = app_root_fields();
    assert!(fields.len() > 50, "the scan found the fields of AppRoot: {}", fields.len());
    // 1. Exactly the declared fields are `Watched`.
    for name in SIDEBAR_WATCHED {
        let ty = &fields.iter().find(|(field, _)| field == name).unwrap_or_else(|| panic!("AppRoot has no field `{name}`")).1;
        assert!(
            ty.starts_with("sidebar_watch::Watched<"),
            "`AppRoot::{name}` is state the sidebar draws and must be declared `sidebar_watch::Watched<..>` (found `{ty}`): a plain field can be \
             changed without the sidebar hearing of it"
        );
    }
    for (name, ty) in &fields {
        assert!(
            !ty.contains("sidebar_watch::Watched<") || SIDEBAR_WATCHED.contains(&name.as_str()),
            "`AppRoot::{name}` is `Watched` but not in SIDEBAR_WATCHED and the inventory in `sidebar_watch.rs`: list it in both"
        );
    }

    // 2. What the pane draws reads only watched fields, `sessions` (its own revision), `shell` (a click writes
    //    `sidebar_collapsed`) and the pane's own bookkeeping: the render and every helper it calls while drawing.
    //    Every method the render (and those helpers) call on `AppRoot` is classified, so a new one cannot slip in.
    const ALSO_READ: &[&str] = &[
        "sessions",
        "shell",
        // The marker of what the pane drew (`mark_sidebar_drawn`) and the epoch it compares with.
        "sidebar_drawn",
        "sidebar_epoch",
        // The pane's own search field: a GPUI entity, its changes reach the pane through GPUI.
        "search_field",
    ];
    // Called while the pane draws: scanned, with everything they call in turn.
    const DRAW_HELPERS: &[&str] = &[
        "mark_sidebar_drawn",
        "sidebar_search_input",
        "connected_in_scope",
        "object_drag_supported",
        "remote_truncated_notice",
        "remote_unsearched_notice",
        "selected_sidebar_connection_ids",
        "sidebar_single_config_copy_target",
        "connection_type_of",
        "create_menu_kinds",
        "sql_kind_of",
        "diagram_node_supported",
        "mongo_node_target",
        "redis_node_target",
        "saved_aggregation_target",
        "no_connect_tooltip",
        "connection_user",
        "offers_another_user",
        "render_row_problem",
        "route_badge",
        "via_ssh",
        "sidebar_color",
        "table_stats_missing",
        "table_stats_of",
        "table_stats_supported",
        "stats_key",
        "stats_stamp",
    ];
    // Called from an event handler or a menu built when it opens (a click, a key, a drag, a drop, a context menu):
    // not part of a draw, so what they read is not what the pane shows.
    const EVENT_TIME: &[&str] = &[
        "assistant_table_ask",
        "collapse_navigator",
        "confirm_delete_selected_connections",
        "confirm_delete_sidebar_group",
        "connect_as_another_user",
        "copy_connection_name",
        "copy_connection_string",
        "copy_navigator_names",
        "copy_navigator_sql",
        "copy_sidebar_connection_config",
        "copy_tree_objects",
        "delete_connection_with_confirmation",
        "delete_selected_sidebar_node",
        "drop_files_on_navigator",
        "drop_sidebar_drag",
        "export_sidebar_config_file",
        "file_drag_left",
        "file_drag_over_navigator",
        "handle_connection_double_click",
        "handle_navigator_click",
        "handle_navigator_disclosure",
        "handle_navigator_open",
        "handle_navigator_row_click",
        "handle_sidebar_key",
        "has_object_entries",
        "import_foreign_connections",
        "import_sidebar_config_file",
        "locate_active_sidebar_tab",
        "mongo_supports_script",
        "move_selected_sidebar_connections",
        "move_sidebar_group",
        "new_query_for_sidebar_node",
        "open_connection_dialog",
        "open_create_selected_group_dialog",
        "open_diagram_for_node",
        "open_new_sidebar_group_dialog",
        "open_rename_sidebar_group_dialog",
        "open_schema_object_source",
        "open_selected_sidebar_node",
        "paste_sidebar_connection_config",
        "paste_tree_objects",
        "refresh_connection",
        "refresh_saved_connections",
        "rename_selected_sidebar_node",
        "request_disconnect",
        "request_disconnect_all",
        "selected_sidebar_snapshot_matches",
        "set_sidebar_connection_hidden",
        "set_sidebar_local_search",
        "sort_sidebar_connections",
        "toggle_connection_pin",
        "toggle_sidebar_group_connections",
        "tree_copy_selection",
        "update_sidebar_search",
        // The drag handler, the notifications the handlers end with, and the stats read the hover (and a defer after the
        // draw) starts.
        "drag_over_row",
        "notify_chrome",
        "ensure_table_stats",
    ];
    // The calls that are not on `AppRoot` at all (a weak handle, GPUI, a style).
    const NOT_APPROOT: &[&str] = &["clone", "update", "write_to_clipboard", "debug_selector", "opacity", "text_color"];

    /// The body of every `fn name` in the sources (the same name in several impls: all of them).
    fn fn_bodies(name: &str, sources: &[(std::path::PathBuf, String)]) -> Vec<String> {
        let mut out = Vec::new();
        for (_, text) in sources {
            let text = text.split("\n#[cfg(test)]\nmod tests").next().unwrap();
            for (at, _) in text.match_indices(&format!("fn {name}")) {
                let after = &text[at + 3 + name.len()..];
                if !(after.starts_with('(') || after.starts_with('<')) {
                    continue;
                }
                // The signature ends at the first `{` (a body) or `;` (no body) outside the parameter list.
                let (mut depth, mut start) = (0i32, None);
                for (offset, c) in after.char_indices() {
                    match c {
                        '(' | '<' => depth += 1,
                        ')' | '>' => depth -= 1,
                        '{' if depth <= 0 => {
                            start = Some(offset);
                            break;
                        }
                        ';' if depth <= 0 => break,
                        _ => {}
                    }
                }
                let Some(start) = start else { continue };
                let mut depth = 0i32;
                for (offset, c) in after[start..].char_indices() {
                    match c {
                        '{' => depth += 1,
                        '}' => {
                            depth -= 1;
                            if depth == 0 {
                                out.push(after[start..start + offset + 1].to_owned());
                                break;
                            }
                        }
                        _ => {}
                    }
                }
            }
        }
        out
    }
    /// `(fields, methods)` named after a `self.` / `this.` / `root.` / `app.` receiver.
    fn receivers(code: &str) -> (std::collections::BTreeSet<String>, std::collections::BTreeSet<String>) {
        let (mut fields, mut methods) = (std::collections::BTreeSet::new(), std::collections::BTreeSet::new());
        for receiver in ["self.", "this.", "root.", "app."] {
            for (at, _) in code.match_indices(receiver) {
                if code[..at].chars().next_back().is_some_and(|c| c.is_ascii_alphanumeric() || c == '_' || c == '.') {
                    continue;
                }
                let rest = &code[at + receiver.len()..];
                let name: String = rest.chars().take_while(|c| c.is_ascii_alphanumeric() || *c == '_').collect();
                if name.is_empty() {
                    continue;
                }
                if rest[name.len()..].starts_with('(') {
                    methods.insert(name);
                } else {
                    fields.insert(name);
                }
            }
        }
        (fields, methods)
    }
    let all_sources = sidebar_sources();
    let render = std::fs::read_to_string(std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("src/app/navigator_ui.rs")).unwrap();
    let render = render.split("fn render_navigator(").nth(1).expect("render_navigator").split("fn apply_navigator_load(").next().unwrap().to_owned();
    let event_time = |name: &str| EVENT_TIME.contains(&name);
    let mut read = std::collections::BTreeSet::new();
    let mut scanned = std::collections::BTreeSet::new();
    let mut queue = vec![("render_navigator".to_owned(), render.clone())];
    while let Some((owner, code)) = queue.pop() {
        let (fields, methods) = receivers(&code);
        for field in fields {
            assert!(
                SIDEBAR_WATCHED.contains(&field.as_str()) || ALSO_READ.contains(&field.as_str()),
                "the sidebar's draw (`{owner}`) reads `AppRoot::{field}`, which is neither watched nor in ALSO_READ: change it only through a \
                 `Watched` field (`sidebar_watch::Watched`, add it to SIDEBAR_WATCHED and the inventory), or the pane will not redraw when it \
                 changes"
            );
            read.insert(field);
        }
        for method in methods {
            if NOT_APPROOT.contains(&method.as_str()) || event_time(&method) {
                continue;
            }
            assert!(
                DRAW_HELPERS.contains(&method.as_str()),
                "`{owner}` calls `{method}`, which is in none of DRAW_HELPERS (called while the pane draws: its reads are checked), \
                 EVENT_TIME (a click, key, drop or menu) or NOT_APPROOT: classify it"
            );
            if scanned.insert(method.clone()) {
                let bodies = fn_bodies(&method, &all_sources);
                assert!(!bodies.is_empty(), "DRAW_HELPERS lists `{method}`, which no source defines");
                queue.extend(bodies.into_iter().map(|body| (method.clone(), body)));
            }
        }
    }
    assert!(read.contains("navigator") && read.contains("sidebar"), "the scan read the render: {read:?}");
    for helper in DRAW_HELPERS {
        assert!(scanned.contains(*helper), "DRAW_HELPERS lists `{helper}`, which the draw does not reach (drop it from the list)");
    }
    for name in EVENT_TIME {
        assert!(render.contains(&format!(".{name}(")), "EVENT_TIME lists `{name}`, which the render does not call (drop it)");
    }

    // 2b. The state the pane draws holds no interior mutability: a `Cell`, `RefCell`, `Rc`, lock or atomic in it can be
    //     changed through a `&` borrow, which never moves the epoch. (A GPUI handle or a service the pane does not draw
    //     is allowed, with the reason.)
    const WATCHED_TYPES: [(&str, &str); 8] = [
        ("StatsCache", "apps/tablex-gpui/src/app/table_stats.rs"),
        ("NavigatorState", "apps/tablex-gpui/src/navigator.rs"),
        ("NavigatorUiState", "apps/tablex-gpui/src/app/navigator_ui.rs"),
        ("SidebarState", "apps/tablex-gpui/src/app/sidebar.rs"),
        ("SavedConnection", "rust-engine/crates/engine-connect/src/records.rs"),
        ("ConnectionPolicy", "apps/tablex-gpui/src/app/connection_policy.rs"),
        ("TunnelUi", "apps/tablex-gpui/src/app/tunnel_ui.rs"),
        ("Settings", "apps/tablex-gpui/src/settings/mod.rs"),
    ];
    const SHARED_FIELDS: [(&str, &str, &str); 4] = [
        ("SidebarState", "scroll", "the list's scroll handle: GPUI notifies the pane for it"),
        ("SidebarState", "focus", "the pane's focus handle: GPUI notifies the pane for it"),
        ("SidebarState", "pin_writes", "a background write; nothing of it is drawn"),
        ("TunnelUi", "agent", "the agent's connections, a service; the badge reads `live`"),
    ];
    for (name, ty) in &fields {
        if SIDEBAR_WATCHED.contains(&name.as_str()) {
            let inner = ty.trim_start_matches("sidebar_watch::Watched<");
            assert!(
                WATCHED_TYPES.iter().any(|(declared, _)| {
                    inner.split(['<', '>']).any(|part| part.rsplit("::").next() == Some(declared))
                }),
                "`AppRoot::{name}: {ty}` is watched but its type is not in WATCHED_TYPES: add it so its fields are checked"
            );
        }
    }
    let workspace = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../..");
    for (ty, file) in WATCHED_TYPES {
        let text = std::fs::read_to_string(workspace.join(file)).unwrap();
        let header = text.find(&format!("struct {ty} {{")).unwrap_or_else(|| panic!("{file} defines no `struct {ty}`"));
        let body = &text[header..];
        let body = &body[..body.find("\n}").expect("the struct ends")];
        for line in body.lines().skip(1) {
            let code = line.split("//").next().unwrap().trim();
            let field = code.trim_start_matches("pub(super) ").trim_start_matches("pub(crate) ").trim_start_matches("pub ").split(':').next().unwrap();
            let interior = ["Cell<", "Rc<", "Mutex", "RwLock", "Atomic", "Arc<"].iter().any(|token| code.contains(token))
                && !code.contains("Arc<dyn");
            assert!(
                !interior || SHARED_FIELDS.iter().any(|(owner, name, _)| *owner == ty && *name == field),
                "`{ty}::{field}` (`{code}`) is state the pane draws, with interior mutability: a `&` borrow can change it without moving the \
                 epoch. Make it plain data, or list it in SHARED_FIELDS with the reason it is not drawn"
            );
        }
    }
    for (ty, field, _) in SHARED_FIELDS {
        let file = WATCHED_TYPES.iter().find(|(declared, _)| *declared == ty).unwrap().1;
        assert!(std::fs::read_to_string(workspace.join(file)).unwrap().contains(&format!("{field}: ")), "SHARED_FIELDS lists `{ty}::{field}`, which is gone");
    }

    // 3. Only the notification path says the pane is current, and only `AppRoot::new` builds the guard.
    for (file, text) in sidebar_sources() {
        let name = file.file_name().unwrap().to_string_lossy().into_owned();
        let production = text.split("\n#[cfg(test)]\nmod tests").next().unwrap();
        for (number, line) in production.lines().enumerate() {
            let code = line.split("//").next().unwrap();
            let marks_current = code.contains("sidebar_seen =") && !code.contains("sidebar_seen ==");
            let builds_guard = code.contains("SidebarEpoch::default(") || code.contains("epoch.watch(");
            let allowed = (marks_current && matches!(name.as_str(), "surface.rs" | "mod.rs")) || (builds_guard && name == "mod.rs") || name == "sidebar_watch.rs";
            assert!(
                !(marks_current || builds_guard) || allowed,
                "{}:{}: `{}` bypasses the sidebar watch: call `notify_sidebar` / `sync_sidebar` (it records what the pane has drawn) and put \
                 state in a `Watched` field instead",
                file.display(),
                number + 1,
                line.trim()
            );
        }
    }
}
