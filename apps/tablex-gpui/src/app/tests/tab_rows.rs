// Grouped-rows tab strip: group chips on the top row, the active group's tabs
// on a second row. Driven through the real UI (bar menu, clicks, drags, keys).

/// Primary-modifier chord for a keystroke (`secondary-` is Cmd on macOS, Ctrl
/// elsewhere).
fn tr_chord(rest: &str) -> String {
    format!("{}-{rest}", if cfg!(target_os = "macos") { "cmd" } else { "ctrl" })
}

struct TrTabs {
    welcome: u64,
    pg: [u64; 3],
    sq: [u64; 2],
}

/// Two connections x several tabs (plus the unbound Welcome tab), grouped by
/// connection, still in inline mode. Order after grouping:
/// `[pg1 pg2 pg3 sq1 sq2 welcome]`; the last opened tab (sq2) is active. In
/// rows mode the chips are postgres and sqlite; Welcome is a global tab.
fn tr_app(
    cx: &mut TestAppContext,
) -> (gpui::Entity<AppRoot>, &mut gpui::VisualTestContext, TrTabs) {
    let (app, visual) = pn_app(cx);
    let welcome = tb_order(&app, visual)[0];
    let (pg, sqlite) = (crate::ui_fixture::POSTGRES_ID, crate::ui_fixture::SQLITE_ID);
    let pg1 = tb_open(&app, visual, "pg1", pg, "warehouse");
    let pg2 = tb_open(&app, visual, "pg2", pg, "warehouse");
    let pg3 = tb_open(&app, visual, "pg3", pg, "warehouse");
    let sq1 = tb_open(&app, visual, "sq1", sqlite, "main");
    let sq2 = tb_open(&app, visual, "sq2", sqlite, "main");
    app.update(visual, |this, _| this.sidebar.seed_scenario_colors());
    tb_set_group_by(&app, visual, crate::workspace::TabGroupBy::Connection);
    let tabs = TrTabs {
        welcome,
        pg: [pg1, pg2, pg3],
        sq: [sq1, sq2],
    };
    (app, visual, tabs)
}

/// The fixed icon button at the trailing end of the tabs row.
fn tr_toggle_rows(visual: &mut gpui::VisualTestContext) {
    let at = visual
        .debug_bounds("tab-rows-toggle")
        .expect("the toggle is always drawn")
        .center();
    visual.simulate_click(at, Default::default());
    tb_settle(visual);
}

fn tr_rows_on(app: &gpui::Entity<AppRoot>, visual: &mut gpui::VisualTestContext) -> bool {
    app.read_with(visual, |this, _| this.workspace.tab_rows())
}

fn tr_active(app: &gpui::Entity<AppRoot>, visual: &mut gpui::VisualTestContext) -> u64 {
    app.read_with(visual, |this, _| this.workspace.active_tab().unwrap().id)
}

/// Selectors `tab-group-chip-0..` that are drawn, in order.
fn tr_chip_count(visual: &mut gpui::VisualTestContext, suffix: &str) -> usize {
    (0..)
        .take_while(|index| {
            let selector: &'static str =
                Box::leak(format!("tab-group-chip-{index}{suffix}").into_boxed_str());
            visual.debug_bounds(selector).is_some()
        })
        .count()
}

fn tr_chip(visual: &mut gpui::VisualTestContext, index: usize) -> gpui::Bounds<gpui::Pixels> {
    let selector: &'static str = Box::leak(format!("tab-group-chip-{index}").into_boxed_str());
    visual
        .debug_bounds(selector)
        .unwrap_or_else(|| panic!("chip {index} is drawn"))
}

fn tr_click_chip(visual: &mut gpui::VisualTestContext, index: usize) {
    let at = tr_chip(visual, index).center();
    tr_click(visual, at);
}

fn tr_click_tab(visual: &mut gpui::VisualTestContext, id: u64) {
    let at = tb_bounds(visual, id).center();
    tr_click(visual, at);
}

fn tr_click(visual: &mut gpui::VisualTestContext, at: gpui::Point<gpui::Pixels>) {
    visual.simulate_click(at, Default::default());
    tb_settle(visual);
}

#[gpui::test]
fn rows_mode_is_off_by_default_and_toggles_through_the_strip_button_and_persists(
    cx: &mut TestAppContext,
) {
    let (app, visual, tabs) = tr_app(cx);
    let path = tb_prefs_file(&app, visual);
    assert!(!tr_rows_on(&app, visual), "existing users keep the inline strip");
    assert!(visual.debug_bounds("workspace-group-bar").is_none());
    assert!(
        visual.debug_bounds(tb_sel(tabs.pg[0])).is_some()
            && visual.debug_bounds(tb_sel(tabs.sq[0])).is_some(),
        "inline: every tab of every group is in the one row"
    );
    // The button is in the tabs row (inline: the only row), fixed at its end.
    let toggle = visual.debug_bounds("tab-rows-toggle").expect("toggle in inline mode");
    let bar = visual.debug_bounds("workspace-tab-bar").unwrap();
    assert!(bar.contains(&toggle.center()));
    assert!(toggle.center().x > bar.center().x, "trailing end");

    tr_toggle_rows(visual);
    assert!(tr_rows_on(&app, visual));
    let prefs = crate::sidebar_preferences::SidebarPreferences::load(&path).unwrap();
    assert!(prefs.tab_rows, "written to the sidebar preferences file");

    // Two rows now, chips on top, the tabs row directly below, the toggle at
    // the end of the tabs row.
    let chips = visual.debug_bounds("workspace-group-bar").expect("chips row");
    let tab_row = visual.debug_bounds("workspace-tab-bar").expect("tabs row");
    assert_eq!(chips.bottom(), tab_row.top(), "second row sits directly below");
    assert_eq!(chips.size.height, px(theme::GROUP_ROW_HEIGHT));
    assert_eq!(tab_row.size.height, px(theme::TAB_HEIGHT));
    let toggle = visual.debug_bounds("tab-rows-toggle").expect("toggle in rows mode");
    assert!(tab_row.contains(&toggle.center()) && !chips.contains(&toggle.center()));

    // Toggling again returns to the inline strip.
    tr_toggle_rows(visual);
    assert!(!tr_rows_on(&app, visual));
    assert!(visual.debug_bounds("workspace-group-bar").is_none());
    assert!(visual.debug_bounds(tb_sel(tabs.pg[0])).is_some());
    assert!(!crate::sidebar_preferences::SidebarPreferences::load(&path).unwrap().tab_rows);
    std::fs::remove_file(path).unwrap();
}

#[gpui::test]
fn the_toggle_is_not_in_any_menu_and_never_scrolls_away(cx: &mut TestAppContext) {
    let (app, visual, tabs) = tr_app(cx);
    for entries in [
        tb_menu_entries(&app, visual, tabs.sq[1]),
        app.read_with(visual, |this, _| {
            tab_bar::bar_menu_entries(this.workspace.group_by(), false)
        }),
    ] {
        assert!(
            !tb_labels(&entries).iter().any(|label| label.contains("Second Row")),
            "{:?}",
            tb_labels(&entries)
        );
    }
    // Overflow the strip: the toggle (and the picker) stay put and clickable.
    for index in 0..14 {
        tb_open(&app, visual, &format!("many-{index}"), crate::ui_fixture::POSTGRES_ID, "warehouse");
    }
    let before = visual.debug_bounds("tab-rows-toggle").unwrap();
    app.update(visual, |this, cx| this.activate_tab_ui(tabs.sq[0], cx));
    tb_settle(visual);
    assert_eq!(visual.debug_bounds("tab-rows-toggle").unwrap(), before);
    let picker = visual.debug_bounds("open-tabs").expect("picker in the overflowing strip");
    assert!(before.right() <= picker.left(), "toggle sits next to the picker");
    tr_toggle_rows(visual);
    assert!(tr_rows_on(&app, visual));
}

#[gpui::test]
fn top_row_has_only_chips_and_the_second_row_only_the_active_groups_tabs(cx: &mut TestAppContext) {
    let (app, visual, tabs) = tr_app(cx);
    app.update(visual, |this, cx| this.set_tab_rows(true, cx));
    tb_settle(visual);
    let chips_row = visual.debug_bounds("workspace-group-bar").unwrap();
    let tab_row = visual.debug_bounds("workspace-tab-bar").unwrap();

    // Chips: postgres and sqlite. Welcome is a global tab, so no "Other".
    assert_eq!(tr_chip_count(visual, ""), 2);
    for index in 0..2 {
        let chip = tr_chip(visual, index);
        assert!(chips_row.contains(&chip.center()), "chip {index} is in the top row");
    }
    app.read_with(visual, |this, _| {
        let pane = this.workspace.focused_pane();
        let rows = this.workspace.pane_rows(pane);
        assert_eq!(rows.chips.iter().map(|chip| chip.count).collect::<Vec<_>>(), vec![3, 2]);
        assert_eq!(rows.active, Some(1), "sq2 is active: the sqlite chip");
    });

    // Global Welcome first, then the active group's tabs, all in the second row.
    let welcome = tb_bounds(visual, tabs.welcome);
    assert!(tab_row.contains(&welcome.center()));
    for sq in tabs.sq {
        let bounds = tb_bounds(visual, sq);
        assert!(tab_row.contains(&bounds.center()), "sqlite tab in the second row");
        assert!(!chips_row.contains(&bounds.center()));
        assert!(welcome.right() <= bounds.left(), "global tabs lead the row");
    }
    for hidden in tabs.pg {
        assert!(
            visual.debug_bounds(tb_sel(hidden)).is_none(),
            "tab {hidden} belongs to another group"
        );
    }
}

#[gpui::test]
fn clicking_a_chip_switches_group_and_returns_to_its_last_active_tab(cx: &mut TestAppContext) {
    let (app, visual, tabs) = tr_app(cx);
    app.update(visual, |this, cx| this.set_tab_rows(true, cx));
    // Make pg2 (not the first, not the last) the postgres group's last active tab.
    app.update(visual, |this, cx| this.activate_tab_ui(tabs.pg[1], cx));
    tb_settle(visual);
    assert_eq!(tr_active(&app, visual), tabs.pg[1]);
    assert!(visual.debug_bounds(tb_sel(tabs.pg[2])).is_some());
    assert!(visual.debug_bounds(tb_sel(tabs.sq[0])).is_none());

    // sqlite chip: its last active tab (sq2) is shown, postgres tabs go away.
    tr_click_chip(visual, 1);
    assert_eq!(tr_active(&app, visual), tabs.sq[1]);
    assert!(visual.debug_bounds(tb_sel(tabs.sq[0])).is_some());
    assert!(visual.debug_bounds(tb_sel(tabs.pg[0])).is_none());

    // Back to postgres: pg2, the one it showed last.
    tr_click_chip(visual, 0);
    assert_eq!(tr_active(&app, visual), tabs.pg[1]);
    assert!(visual.debug_bounds(tb_sel(tabs.pg[0])).is_some());

    // The global Welcome tab stays in the row whichever group is shown.
    assert!(visual.debug_bounds(tb_sel(tabs.welcome)).is_some());

    // A tab click in the second row still activates it.
    tr_click_chip(visual, 0);
    tr_click_tab(visual, tabs.pg[2]);
    assert_eq!(tr_active(&app, visual), tabs.pg[2]);
}

#[gpui::test]
fn closing_the_last_tab_of_a_group_removes_its_chip_and_a_lone_group_chip_leads_the_row(
    cx: &mut TestAppContext,
) {
    let (app, visual, tabs) = tr_app(cx);
    app.update(visual, |this, cx| this.set_tab_rows(true, cx));
    tb_settle(visual);
    assert_eq!(tr_chip_count(visual, ""), 2);
    for sq in tabs.sq {
        let close: &'static str = Box::leak(format!("tab-close-{sq}").into_boxed_str());
        let at = visual.debug_bounds(close).expect("close button").center();
        tr_click(visual, at);
        if sq == tabs.sq[0] {
            assert_eq!(tr_chip_count(visual, ""), 2, "one sqlite tab left");
        }
    }
    // Only postgres remains: its chip leads the one row, the setting stays on.
    assert!(visual.debug_bounds("workspace-group-bar").is_none(), "no chip row");
    assert_eq!(tr_chip_count(visual, ""), 1, "the sqlite chip is gone, postgres stays");
    let tab_row = visual.debug_bounds("workspace-tab-bar").unwrap();
    assert!(tab_row.contains(&tr_chip(visual, 0).center()), "the lone chip is in the tabs row");
    assert!(tr_rows_on(&app, visual), "the setting stays on");
    for pg in tabs.pg {
        assert!(visual.debug_bounds(tb_sel(pg)).is_some(), "all tabs in the one row");
    }
    assert!(visual.debug_bounds(tb_sel(tabs.welcome)).is_some());
    let bar = visual.debug_bounds("workspace-tab-bar").unwrap();
    assert_eq!(bar.size.height, px(theme::TAB_HEIGHT));

    // A second group brings the chip row back.
    tb_open(&app, visual, "sq3", crate::ui_fixture::SQLITE_ID, "main");
    assert_eq!(tr_chip_count(visual, ""), 2);
    assert!(visual.debug_bounds("workspace-group-bar").is_some());
}

#[gpui::test]
fn tabs_reorder_by_drag_within_the_second_row_and_chips_take_matching_drops(
    cx: &mut TestAppContext,
) {
    let (app, visual, tabs) = tr_app(cx);
    app.update(visual, |this, cx| this.set_tab_rows(true, cx));
    tr_click_chip(visual, 0);
    let [pg1, pg2, pg3] = tabs.pg;
    assert_eq!(&tb_order(&app, visual)[..3], &[pg1, pg2, pg3]);

    // Drag pg3 onto the left half of pg1, inside the second row.
    let from = tb_bounds(visual, pg3).center();
    let to = tb_left_of(tb_bounds(visual, pg1));
    tb_drag(visual, from, to, true);
    assert_eq!(&tb_order(&app, visual)[..3], &[pg3, pg1, pg2], "reordered in the row");

    // A chip of the same group accepts the drop (becomes the group's first tab)...
    let from = tb_bounds(visual, pg2).center();
    let chip = tr_chip(visual, 0).center();
    tb_drag(visual, from, chip, false);
    app.read_with(visual, |this, _| {
        let (target, plan) = this.shell.tab_bar.hover.clone().expect("drop candidate");
        assert!(matches!(target, crate::workspace::TabDropTarget::GroupStart(..)));
        assert!(matches!(plan, crate::workspace::TabMovePlan::Move(_)));
    });
    visual.simulate_mouse_up(chip, gpui::MouseButton::Left, Default::default());
    tb_settle(visual);
    assert_eq!(&tb_order(&app, visual)[..3], &[pg2, pg3, pg1]);

    // ...a chip of another group refuses it.
    let from = tb_bounds(visual, pg1).center();
    let other_chip = tr_chip(visual, 1).center();
    tb_drag(visual, from, other_chip, true);
    assert_eq!(&tb_order(&app, visual)[..3], &[pg2, pg3, pg1], "refused: unchanged");
}

#[gpui::test]
fn split_panes_each_get_their_own_rows(cx: &mut TestAppContext) {
    let (app, visual, tabs) = tr_app(cx);
    app.update(visual, |this, cx| this.set_tab_rows(true, cx));
    tb_settle(visual);
    let left = pn_focused(&app, visual);
    // Move a sqlite tab into a new pane on the right.
    pn_drag_to(visual, tabs.sq[1], left, 0.95, 0.5, true);
    let panes = pn_panes(&app, visual);
    assert_eq!(panes.len(), 2);
    let right = panes[1].0;

    // The right pane holds one group, but sits beside a pane with a chip row:
    // its lone chip goes on an aligned chip row, not inline.
    assert!(visual.debug_bounds("workspace-group-bar").is_some());
    assert!(visual.debug_bounds("workspace-group-bar-p1").is_some(), "aligned chip row");
    assert_eq!(tr_chip_count(visual, "-p1"), 1);
    assert!(visual.debug_bounds("tab-group-separator").is_none(), "no inline lone chip");
    assert_eq!(
        visual.debug_bounds("workspace-tab-bar").unwrap().top(),
        visual.debug_bounds("workspace-tab-bar-p1").unwrap().top()
    );
    assert_eq!(tr_chip_count(visual, ""), 2);
    assert!(
        visual.debug_bounds("tab-rows-toggle").is_some()
            && visual.debug_bounds("tab-rows-toggle-p1").is_some(),
        "each pane's strip carries the toggle"
    );

    // A postgres tab joins the right pane's tabs row: now it has two groups
    // and its own two rows.
    tr_click_chip(visual, 0);
    // (Next to a tab of another group would be refused; the tail takes any tab.)
    let tail = visual.debug_bounds("workspace-tab-tail-p1").unwrap().center();
    let from = tb_bounds(visual, tabs.pg[2]).center();
    tb_drag(visual, from, tail, true);
    assert_eq!(pn_tabs(&app, visual)[1].len(), 2);
    for suffix in ["", "-p1"] {
        let bars: &'static str = Box::leak(format!("workspace-group-bar{suffix}").into_boxed_str());
        let tabs_row: &'static str = Box::leak(format!("workspace-tab-bar{suffix}").into_boxed_str());
        let chips_row = visual.debug_bounds(bars).unwrap_or_else(|| panic!("{bars}"));
        let tab_row = visual.debug_bounds(tabs_row).unwrap_or_else(|| panic!("{tabs_row}"));
        assert_eq!(chips_row.bottom(), tab_row.top());
    }
    app.read_with(visual, |this, _| {
        let right_rows = this.workspace.pane_rows(right);
        assert_eq!(right_rows.chips.len(), 2, "its own chips: pg and sq");
        assert!(right_rows.tabs.contains(&tabs.sq[1]) || right_rows.tabs.contains(&tabs.pg[2]));
    });
    assert_eq!(tr_chip_count(visual, "-p1"), 2);

    // A chip in the right pane selects within that pane only.
    let right_chip = visual.debug_bounds("tab-group-chip-1-p1").unwrap();
    tr_click(visual, right_chip.center());
    assert_eq!(pn_focused(&app, visual), right);
    assert_eq!(tr_active(&app, visual), tabs.sq[1]);
}

#[gpui::test]
fn turning_rows_on_without_grouping_groups_by_connection(cx: &mut TestAppContext) {
    let (app, visual) = pn_app(cx);
    let path = tb_prefs_file(&app, visual);
    tb_open(&app, visual, "a", crate::ui_fixture::POSTGRES_ID, "warehouse");
    tb_open(&app, visual, "b", crate::ui_fixture::SQLITE_ID, "main");
    app.read_with(visual, |this, _| {
        assert_eq!(this.workspace.group_by(), crate::sidebar_preferences::TabGroupBy::None)
    });
    app.update(visual, |this, cx| this.toggle_tab_rows(cx));
    tb_settle(visual);
    app.read_with(visual, |this, _| {
        assert_eq!(
            this.workspace.group_by(),
            crate::sidebar_preferences::TabGroupBy::Connection
        );
        assert!(this.workspace.tab_rows());
    });
    assert!(visual.debug_bounds("workspace-group-bar").is_some());

    // Ungrouping again falls back to the one-row strip (nothing to put on top).
    tb_set_group_by(&app, visual, crate::sidebar_preferences::TabGroupBy::None);
    assert!(!tr_rows_on(&app, visual));
    assert!(visual.debug_bounds("workspace-group-bar").is_none());
    std::fs::remove_file(path).unwrap();
}

#[gpui::test]
fn rows_mode_survives_a_session_save_and_relaunch(cx: &mut TestAppContext) {
    let (app, visual, tabs) = tr_app(cx);
    let dir = tb_session_dir();
    let session = dir.join("gpui-workspace-session.json");
    app.update(visual, |this, _| this.shell.session.path = Some(session.clone()));
    let prefs = tb_prefs_file(&app, visual);
    tb_settle(visual);
    tr_toggle_rows(visual);
    app.update(visual, |this, cx| this.activate_tab_ui(tabs.pg[1], cx));
    tb_settle(visual);
    tb_advance(visual, 1300);
    assert!(session.exists(), "session written");

    // Relaunch: preferences are re-applied, the session restored.
    tb_relaunch(&app, visual, tb_no_swift);
    assert!(tr_rows_on(&app, visual), "mode restored");
    assert!(visual.debug_bounds("workspace-group-bar").is_some());
    assert_eq!(tr_chip_count(visual, ""), 2);
    let active = tr_active(&app, visual);
    app.read_with(visual, |this, _| {
        assert_eq!(this.workspace.tab(active).unwrap().title, "pg2");
    });
    assert!(visual.debug_bounds(tb_sel(active)).is_some());
    std::fs::remove_dir_all(dir).unwrap();
    std::fs::remove_file(prefs).unwrap();
}

fn tr_cycle_groups_with_keys(cx: &mut TestAppContext, rows: bool) {
    let (app, visual, tabs) = tr_app(cx);
    app.update(visual, |this, cx| this.set_tab_rows(rows, cx));
    tb_settle(visual);
    tb_focus_root(&app, visual);
    // Chips: postgres, sqlite. sq2 (sqlite) is active; two chips wrap around.
    for (chord, expected) in [
        ("shift-pagedown", tabs.pg[0]),
        ("shift-pagedown", tabs.sq[1]),
        ("shift-pageup", tabs.pg[0]),
        ("shift-pageup", tabs.sq[1]),
    ] {
        visual.simulate_keystrokes(&tr_chord(chord));
        tb_settle(visual);
        assert_eq!(tr_active(&app, visual), expected, "{chord}");
    }
    if rows {
        // The second row follows the group.
        assert!(visual.debug_bounds(tb_sel(tabs.sq[0])).is_some());
        assert!(visual.debug_bounds(tb_sel(tabs.pg[0])).is_none());
    }
}

#[gpui::test]
fn group_shortcuts_cycle_the_chips_in_rows_mode(cx: &mut TestAppContext) {
    tr_cycle_groups_with_keys(cx, true);
}

#[gpui::test]
fn group_shortcuts_also_work_in_inline_mode(cx: &mut TestAppContext) {
    tr_cycle_groups_with_keys(cx, false);
}

#[gpui::test]
fn palette_action_toggles_the_rows(cx: &mut TestAppContext) {
    let (app, visual) = palette_app(cx);
    palette_open(visual);
    palette_type(visual, "second row");
    visual.simulate_keystrokes("enter");
    palette_settle(visual);
    app.read_with(visual, |this, _| {
        assert!(this.workspace.tab_rows(), "palette toggled the strip");
        assert_eq!(
            this.workspace.group_by(),
            crate::sidebar_preferences::TabGroupBy::Connection
        );
    });
    assert!(visual.debug_bounds("tab-rows-toggle").is_some());
    assert!(!palette_is_open(visual));
}

#[test]
fn group_navigation_bindings_use_the_secondary_modifier() {
    for chord in [
        "secondary-shift-pagedown",
        "secondary-shift-pageup",
        "secondary-shift-]",
        "secondary-shift-[",
    ] {
        let stroke = shortcuts::parse_chord(chord).expect("valid keystroke");
        let primary = if cfg!(target_os = "macos") { stroke.modifiers.platform } else { stroke.modifiers.control };
        assert!(primary && stroke.modifiers.shift, "{chord}");
    }
}

#[gpui::test]
fn pinned_tabs_lead_the_tabs_row_whatever_group_is_shown(cx: &mut TestAppContext) {
    let (app, visual, tabs) = tr_app(cx);
    // Pin a postgres tab: it leaves its group and becomes global.
    app.update(visual, |this, _| {
        this.workspace.set_pinned(tabs.pg[0], true);
    });
    app.update(visual, |this, cx| this.set_tab_rows(true, cx));
    tb_settle(visual);
    app.read_with(visual, |this, _| {
        let rows = this.workspace.pane_rows(this.workspace.focused_pane());
        assert_eq!(
            rows.chips.iter().map(|chip| chip.count).collect::<Vec<_>>(),
            vec![2, 2],
            "the pinned tab is in no chip"
        );
    });
    for chip in [0, 1, 0, 1] {
        tr_click_chip(visual, chip);
        let pinned = tb_bounds(visual, tabs.pg[0]);
        let welcome = tb_bounds(visual, tabs.welcome);
        assert!(pinned.right() <= welcome.left(), "pinned first, then global");
        let tab_row = visual.debug_bounds("workspace-tab-bar").unwrap();
        assert!(tab_row.contains(&pinned.center()));
        let group_tab = if chip == 0 { tabs.pg[1] } else { tabs.sq[0] };
        assert!(welcome.right() <= tb_bounds(visual, group_tab).left());
    }
    // Activating the pinned tab keeps the shown group (and its chip) in place.
    tr_click_chip(visual, 1);
    tr_click_tab(visual, tabs.pg[0]);
    assert_eq!(tr_active(&app, visual), tabs.pg[0]);
    assert!(visual.debug_bounds(tb_sel(tabs.sq[0])).is_some(), "sqlite tabs still shown");
}

#[gpui::test]
fn other_chip_only_holds_loose_query_tabs_and_welcome_is_a_row_tab(cx: &mut TestAppContext) {
    let (app, visual, tabs) = tr_app(cx);
    app.update(visual, |this, cx| this.set_tab_rows(true, cx));
    tb_settle(visual);
    assert_eq!(tr_chip_count(visual, ""), 2, "no Other chip for Welcome alone");
    assert!(visual.debug_bounds(tb_sel(tabs.welcome)).is_some());

    // A connection-less query tab is not global: it appears as "Other".
    let loose = tb_open(&app, visual, "loose", "", "");
    assert_eq!(tr_chip_count(visual, ""), 3);
    assert!(visual.debug_bounds(tb_sel(loose)).is_some(), "loose tab is shown (active)");
    assert!(visual.debug_bounds(tb_sel(tabs.welcome)).is_some());
    assert!(visual.debug_bounds(tb_sel(tabs.sq[0])).is_none());
    tr_click_chip(visual, 0);
    assert!(visual.debug_bounds(tb_sel(loose)).is_none());
    tr_click_chip(visual, 2);
    assert_eq!(tr_active(&app, visual), loose);

    // Closing it removes the chip again.
    let close: &'static str = Box::leak(format!("tab-close-{loose}").into_boxed_str());
    let at = visual.debug_bounds(close).unwrap().center();
    tr_click(visual, at);
    assert_eq!(tr_chip_count(visual, ""), 2);
}

#[gpui::test]
fn chips_show_the_connection_state_like_the_sidebar(cx: &mut TestAppContext) {
    use crate::sessions::ConnectionStatus;
    let (app, visual, _tabs) = tr_app(cx);
    app.update(visual, |this, cx| this.set_tab_rows(true, cx));
    tb_settle(visual);
    let (pg, sqlite) = (crate::ui_fixture::POSTGRES_ID, crate::ui_fixture::SQLITE_ID);
    app.update(visual, |this, cx| {
        let attempt = this.sessions.begin_connect(pg).unwrap();
        this.sessions.finish_connect(pg, attempt, Ok(()));
        this.sessions.begin_connect(sqlite).unwrap();
        cx.notify();
        assert!(matches!(this.sessions.status(pg), ConnectionStatus::Connected));
        assert_eq!(this.connection_status_color(pg), theme::success());
        assert_eq!(this.connection_status_color(sqlite), theme::warning());
        assert_eq!(this.connection_status_color("unknown"), theme::text_faint());
    });
    app.update(visual, |this, _| {
        let attempt = this.sessions.begin_connect("failing").unwrap();
        this.sessions.finish_connect("failing", attempt, Err("boom".into()));
        assert_eq!(this.connection_status_color("failing"), theme::danger());
    });
    tb_settle(visual);
    // Each chip draws its state dot (the decorative dot is gone).
    for chip in 0..2 {
        let dot: &'static str = Box::leak(format!("tab-group-chip-state-{chip}").into_boxed_str());
        let dot = visual.debug_bounds(dot).unwrap_or_else(|| panic!("state dot {chip}"));
        assert!(tr_chip(visual, chip).contains(&dot.center()));
    }
}

fn tr_chip_menu(visual: &mut gpui::VisualTestContext, chip: usize, downs: usize) {
    let at = tr_chip(visual, chip).center();
    tb_right_click(visual, at);
    for _ in 0..downs {
        visual.simulate_keystrokes("down");
    }
    visual.simulate_keystrokes("enter");
    tb_settle(visual);
}

#[gpui::test]
fn chip_menu_closes_the_group_confirms_staged_changes_and_disconnects(cx: &mut TestAppContext) {
    use crate::sessions::ConnectionStatus;
    let (app, visual, tabs) = tr_app(cx);
    app.update(visual, |this, cx| this.set_tab_rows(true, cx));
    tb_settle(visual);
    let sqlite = crate::ui_fixture::SQLITE_ID;

    // Menu: [New Query] [Close All Tabs in Group] [Disconnect (disabled until connected)].
    // Close All Tabs in Group on the sqlite chip, with a staged change: confirm first.
    tb_stage_insert(&app, visual, tabs.sq[1]);
    tr_chip_menu(visual, 1, 2);
    assert!(visual.update(|window, cx| window.has_active_dialog(cx)), "confirmation dialog");
    app.read_with(visual, |this, _| {
        assert!(this.workspace.tab(tabs.sq[0]).is_some(), "nothing closed yet");
    });
    visual.simulate_keystrokes("escape");
    tb_settle(visual);
    app.read_with(visual, |this, _| assert!(this.workspace.tab(tabs.sq[1]).is_some()));

    // Without staged changes it closes every tab of the group and nothing else.
    app.update(visual, |this, _| {
        this.workspace.runtime_mut(tabs.sq[1]).unwrap().pending_sql_rows.clear();
    });
    tr_chip_menu(visual, 1, 2);
    app.read_with(visual, |this, _| {
        assert!(this.workspace.tab(tabs.sq[0]).is_none() && this.workspace.tab(tabs.sq[1]).is_none());
        assert!(tabs.pg.iter().all(|id| this.workspace.tab(*id).is_some()));
        assert!(this.workspace.tab(tabs.welcome).is_some(), "global tabs are not in the group");
    });

    // Disconnect uses the sidebar's disconnect: enabled only while connected.
    app.update(visual, |this, cx| {
        let pg = crate::ui_fixture::POSTGRES_ID;
        let attempt = this.sessions.begin_connect(pg).unwrap();
        this.sessions.finish_connect(pg, attempt, Ok(()));
        this.scenario_ui.service = None;
        cx.notify();
    });
    // Disconnecting hands the engine session to a background thread.
    visual.executor().allow_parking();
    tb_settle(visual);
    tb_open(&app, visual, "sq", sqlite, "main");
    tr_chip_menu(visual, 0, 3);
    app.read_with(visual, |this, _| {
        assert!(matches!(
            this.sessions.status(crate::ui_fixture::POSTGRES_ID),
            ConnectionStatus::Disconnected
        ));
    });
}

#[gpui::test]
fn chip_menu_new_query_opens_a_query_for_the_chips_connection(cx: &mut TestAppContext) {
    let (app, visual, tabs) = tr_app(cx);
    app.update(visual, |this, cx| this.set_tab_rows(true, cx));
    tb_settle(visual);
    let before = tb_order(&app, visual).len();
    tr_chip_menu(visual, 0, 1);
    app.read_with(visual, |this, _| {
        assert_eq!(this.workspace.tabs().len(), before + 1);
        let active = this.workspace.active_tab().unwrap();
        assert_eq!(active.connection_id, crate::ui_fixture::POSTGRES_ID);
        assert!(!tabs.pg.contains(&active.id));
    });
}

/// Keys of one pane's chips in order (`None` = "Other").
fn tr_chip_keys(
    app: &gpui::Entity<AppRoot>,
    visual: &mut gpui::VisualTestContext,
    pane: PaneId,
) -> Vec<Option<crate::workspace::TabGroupKey>> {
    app.read_with(visual, |this, _| {
        this.workspace
            .pane_rows(pane)
            .chips
            .iter()
            .map(|chip| chip.group.cloned())
            .collect()
    })
}

/// User report: two groups in a pane; two tabs dragged into a split, regrouped
/// (group-by switched), then dragged back. Chips read "Mongo 2", "trans_his",
/// "Mongo 1": two chips of one connection that looked identical.
///
/// Evidence (this test dumps the keys): in Database mode the chips of the same
/// connection have different `database` keys (the Mongo script tab's database
/// differs from the collection tabs'), so they are legitimately distinct
/// groups, and (1) they were not adjacent, (2) the label truncated at its end
/// hid the database. No group is ever split across equal keys.
#[gpui::test]
fn dragging_tabs_back_after_regrouping_keeps_keys_unique_adjacent_and_readable(
    cx: &mut TestAppContext,
) {
    use crate::sidebar_preferences::TabGroupBy;
    let (app, visual) = pn_app(cx);
    let (mongo, pg) = (crate::ui_fixture::MONGO_ID, crate::ui_fixture::POSTGRES_ID);
    tb_open(&app, visual, "orders", mongo, "shop");
    tb_open(&app, visual, "invoices", mongo, "shop");
    let script = tb_open(&app, visual, "Mongo Script", mongo, "banvietbank");
    let trans = tb_open(&app, visual, "trans-his", pg, "trans_his_v3");
    tb_set_group_by(&app, visual, TabGroupBy::Database);
    app.update(visual, |this, cx| this.set_tab_rows(true, cx));
    tb_settle(visual);
    let left = pn_focused(&app, visual);

    // Two tabs into a split on the right.
    pn_drag_to(visual, trans, left, 0.95, 0.5, true);
    // (script is in the left pane: move it right too, via the right pane's tail)
    let right_tail = visual.debug_bounds("workspace-tab-tail-p1").unwrap().center();
    let from = visual.debug_bounds(tb_sel(script));
    if let Some(from) = from {
        tb_drag(visual, from.center(), right_tail, true);
    } else {
        // Not shown (another group is): reveal it first.
        app.update(visual, |this, cx| this.activate_tab_ui(script, cx));
        tb_settle(visual);
        let from = tb_bounds(visual, script).center();
        tb_drag(visual, from, right_tail, true);
    }
    assert_eq!(pn_tabs(&app, visual)[1].len(), 2, "both tabs are in the right pane");

    // Regroup as the user did: by connection, then (tab menu) back by database.
    tb_set_group_by(&app, visual, TabGroupBy::Connection);
    tb_set_group_by(&app, visual, TabGroupBy::Database);

    // Drag both back into the original pane (its tab row's empty tail).
    for tab in [trans, script] {
        app.update(visual, |this, cx| this.activate_tab_ui(tab, cx));
        tb_settle(visual);
        let from = tb_bounds(visual, tab).center();
        let left_tail = visual.debug_bounds("workspace-tab-tail").unwrap().center();
        tb_drag(visual, from, left_tail, true);
    }
    assert_eq!(pn_tabs(&app, visual).len(), 1, "the emptied pane closed");
    let pane = pn_focused(&app, visual);

    let keys = tr_chip_keys(&app, visual, pane);
    eprintln!("chip keys: {keys:?}");
    // No two chips of a pane share a key (a group is never split).
    for (index, key) in keys.iter().enumerate() {
        assert!(!keys[..index].contains(key), "duplicate chip key {key:?}");
    }
    assert_eq!(keys.len(), 3, "mongo/shop, mongo/banvietbank, pg: {keys:?}");
    // Chips of one connection are adjacent: the mongo chips come first.
    let connections: Vec<_> = keys
        .iter()
        .map(|key| key.as_ref().unwrap().connection_id.as_str())
        .collect();
    assert_eq!(connections, vec![mongo, mongo, pg], "same connection adjacent");

    // Every chip label is distinct and its database stays visible.
    app.read_with(visual, |this, _| {
        let labels: Vec<String> = keys
            .iter()
            .map(|key| this.group_label(key.as_ref().unwrap()))
            .collect();
        for (index, label) in labels.iter().enumerate() {
            assert!(!labels[..index].contains(label), "ambiguous label {label}");
        }
    });
    for chip in 0..3 {
        let suffix: &'static str =
            Box::leak(format!("tab-group-chip-db-{chip}").into_boxed_str());
        let suffix = visual.debug_bounds(suffix).unwrap_or_else(|| panic!("db suffix {chip}"));
        assert!(suffix.size.width > px(10.0));
        assert!(tr_chip(visual, chip).contains(&suffix.center()));
        assert!(suffix.right() <= tr_chip(visual, chip).right());
    }
    // Dropping a tab onto a pane merges it into the group with the same key.
    let extra = tb_open(&app, visual, "extra", mongo, "shop");
    app.read_with(visual, |this, _| {
        let rows = this.workspace.pane_rows(this.workspace.focused_pane());
        assert_eq!(rows.chips.len(), 3, "joined the existing mongo/shop group");
        assert_eq!(rows.chips[0].count, 3);
        assert!(this.workspace.tab(extra).is_some());
    });
}

#[gpui::test]
fn dragging_tabs_back_in_connection_mode_merges_them_into_the_existing_group(
    cx: &mut TestAppContext,
) {
    use crate::sidebar_preferences::TabGroupBy;
    let (app, visual) = pn_app(cx);
    let (mongo, pg) = (crate::ui_fixture::MONGO_ID, crate::ui_fixture::POSTGRES_ID);
    let first = tb_open(&app, visual, "orders", mongo, "shop");
    let script = tb_open(&app, visual, "Mongo Script", mongo, "banvietbank");
    let trans = tb_open(&app, visual, "trans-his", pg, "trans_his_v3");
    tb_set_group_by(&app, visual, TabGroupBy::Connection);
    app.update(visual, |this, cx| this.set_tab_rows(true, cx));
    tb_settle(visual);
    let left = pn_focused(&app, visual);
    pn_drag_to(visual, trans, left, 0.95, 0.5, true);
    let right = pn_panes(&app, visual)[1].0;
    let right_tail = visual.debug_bounds("workspace-tab-tail-p1").unwrap().center();
    app.update(visual, |this, cx| this.activate_tab_ui(script, cx));
    tb_settle(visual);
    // (The trailing toggle / picker buttons can cover a scrolled tab's centre.)
    let from = tb_left_of(tb_bounds(visual, script));
    tb_drag(visual, from, right_tail, true);
    assert_eq!(pn_tabs(&app, visual)[1].len(), 2);
    let _ = right;
    for tab in [trans, script] {
        app.update(visual, |this, cx| this.activate_tab_ui(tab, cx));
        tb_settle(visual);
        let from = tb_left_of(tb_bounds(visual, tab));
        let left_tail = visual.debug_bounds("workspace-tab-tail").unwrap().center();
        tb_drag(visual, from, left_tail, true);
    }
    let pane = pn_focused(&app, visual);
    let keys = tr_chip_keys(&app, visual, pane);
    assert_eq!(keys.len(), 2, "mongo (both databases) and pg: {keys:?}");
    app.read_with(visual, |this, _| {
        let rows = this.workspace.pane_rows(pane);
        assert_eq!(rows.chips[0].count, 2, "first + script rejoined one group");
        assert!(this.workspace.tab(first).is_some());
    });
}

/// Two panes: left `[l1 l2 (mongo/sqlite)]`, right `[script (mongo), table (pg)]`
/// built by real drags, both in rows mode grouped by connection.
fn tr_two_panes(
    app: &gpui::Entity<AppRoot>,
    visual: &mut gpui::VisualTestContext,
) -> (PaneId, PaneId, [u64; 4]) {
    use crate::sidebar_preferences::TabGroupBy;
    let (mongo, sqlite) = (crate::ui_fixture::MONGO_ID, crate::ui_fixture::SQLITE_ID);
    let l1 = tb_open(app, visual, "l1", mongo, "shop");
    let l2 = tb_open(app, visual, "l2", sqlite, "main");
    app.update(visual, |this, cx| {
        this.open_mongo_script_tab(mongo, "banvietbank", "txn");
        cx.notify();
    });
    tb_settle(visual);
    let script = app.read_with(visual, |this, _| this.workspace.active_tab().unwrap().id);
    let table = tb_open_typed(app, visual, "orders", "sqlTable");
    tb_set_group_by(app, visual, TabGroupBy::Connection);
    app.update(visual, |this, cx| this.set_tab_rows(true, cx));
    tb_settle(visual);
    let left = pn_focused(app, visual);
    app.update(visual, |this, cx| this.activate_tab_ui(script, cx));
    tb_settle(visual);
    pn_drag_to(visual, script, left, 0.95, 0.5, true);
    let right = pn_panes(app, visual)[1].0;
    let tail = visual.debug_bounds("workspace-tab-tail-p1").unwrap().center();
    app.update(visual, |this, cx| this.activate_tab_ui(table, cx));
    tb_settle(visual);
    let from = tb_bounds(visual, table).center();
    tb_drag(visual, from, tail, true);
    (left, right, [l1, l2, script, table])
}

#[gpui::test]
fn a_pane_made_by_a_split_inherits_the_strip_settings_and_shows_its_own_chips(
    cx: &mut TestAppContext,
) {
    use crate::sidebar_preferences::TabGroupBy;
    let (app, visual) = pn_app(cx);
    let (left, right, [_, _, script, table]) = tr_two_panes(&app, visual);
    app.read_with(visual, |this, _| {
        for pane in [left, right] {
            let strip = this.workspace.strip_of(pane);
            assert_eq!(strip.group_by, TabGroupBy::Connection, "inherited by the split");
            assert!(strip.rows);
        }
        // Different connections in the right pane: two chips of its own.
        let rows = this.workspace.pane_rows(right);
        assert_eq!(rows.chips.len(), 2);
        assert!(rows.tabs.contains(&script) || rows.tabs.contains(&table));
    });
    assert!(visual.debug_bounds("workspace-group-bar").is_some());
    assert!(visual.debug_bounds("workspace-group-bar-p1").is_some());
    assert_eq!(tr_chip_count(visual, "-p1"), 2);
}

#[gpui::test]
fn two_tabs_of_one_connection_share_one_chip_until_the_pane_groups_by_database(
    cx: &mut TestAppContext,
) {
    use crate::sidebar_preferences::TabGroupBy;
    let (app, visual) = pn_app(cx);
    let mongo = crate::ui_fixture::MONGO_ID;
    tb_open(&app, visual, "a", mongo, "shop");
    tb_open(&app, visual, "b", mongo, "banvietbank");
    tb_set_group_by(&app, visual, TabGroupBy::Connection);
    app.update(visual, |this, cx| this.set_tab_rows(true, cx));
    tb_settle(visual);
    // One connection = one group = one row, by design (no chips to choose from).
    assert!(visual.debug_bounds("workspace-group-bar").is_none());
    let pane = pn_focused(&app, visual);
    app.update(visual, |this, cx| this.set_pane_group_by(pane, TabGroupBy::Database, cx));
    tb_settle(visual);
    assert_eq!(tr_chip_count(visual, ""), 2, "one chip per database now");
}

#[gpui::test]
fn group_by_and_rows_change_only_the_pane_they_are_used_in(cx: &mut TestAppContext) {
    use crate::sidebar_preferences::TabGroupBy;
    let (app, visual) = pn_app(cx);
    let (left, right, _) = tr_two_panes(&app, visual);

    // "Group Tabs By > None" in the RIGHT pane's bar menu (its empty tail).
    // Menu: [label] None, Connection, Database, ...; the first down skips the label.
    let tail = visual.debug_bounds("workspace-tab-tail-p1").unwrap().center();
    tb_right_click(visual, tail);
    for _ in 0..1 {
        visual.simulate_keystrokes("down");
    }
    visual.simulate_keystrokes("enter");
    tb_settle(visual);
    app.read_with(visual, |this, _| {
        assert_eq!(this.workspace.group_by_of(right), TabGroupBy::None);
        assert_eq!(
            this.workspace.group_by_of(left),
            TabGroupBy::Connection,
            "the left pane keeps its grouping"
        );
    });
    assert!(visual.debug_bounds("workspace-group-bar").is_some(), "left still has chips");
    assert!(visual.debug_bounds("workspace-group-bar-p1").is_none());
    assert_eq!(tr_chip_count(visual, "-p1"), 0);

    // The right pane's own toggle: on -> grouped by connection again, rows on;
    // the left pane's toggle then turns only the left pane's rows off.
    let at = visual.debug_bounds("tab-rows-toggle-p1").unwrap().center();
    tr_click(visual, at);
    app.read_with(visual, |this, _| {
        assert!(this.workspace.tab_rows_of(right));
        assert_eq!(this.workspace.group_by_of(right), TabGroupBy::Connection);
    });
    let at = visual.debug_bounds("tab-rows-toggle").unwrap().center();
    tr_click(visual, at);
    app.read_with(visual, |this, _| {
        assert!(!this.workspace.tab_rows_of(left));
        assert!(this.workspace.tab_rows_of(right), "right pane untouched");
    });
    assert!(visual.debug_bounds("workspace-group-bar").is_none());
    assert!(visual.debug_bounds("workspace-group-bar-p1").is_some());

    // Panes with different modes side by side, both showing chips.
    app.update(visual, |this, cx| {
        this.set_pane_tab_rows(left, true, cx);
        this.set_pane_group_by(right, TabGroupBy::Database, cx);
    });
    tb_settle(visual);
    assert!(tr_chip_count(visual, "") >= 2 && tr_chip_count(visual, "-p1") >= 2);
}

#[gpui::test]
fn dragging_between_panes_with_different_modes_judges_the_tab_by_the_destination(
    cx: &mut TestAppContext,
) {
    use crate::sidebar_preferences::TabGroupBy;
    let (app, visual) = pn_app(cx);
    let (_left, right, [l1, _, script, _]) = tr_two_panes(&app, visual);
    app.update(visual, |this, cx| this.set_pane_group_by(right, TabGroupBy::Database, cx));
    tb_settle(visual);
    // l1 (mongo/shop) joins the right pane next to the mongo script tab
    // (mongo/banvietbank): another database group there, not refused.
    app.update(visual, |this, cx| this.activate_tab_ui(l1, cx));
    tb_settle(visual);
    let tail = visual.debug_bounds("workspace-tab-tail-p1").unwrap().center();
    let from = tb_bounds(visual, l1).center();
    tb_drag(visual, from, tail, true);
    app.read_with(visual, |this, _| {
        assert_eq!(this.workspace.pane_of(l1), Some(right));
        let rows = this.workspace.pane_rows(right);
        let dbs: Vec<_> = rows
            .chips
            .iter()
            .filter_map(|chip| chip.group.map(|key| key.database.clone()))
            .collect();
        assert!(dbs.contains(&"shop".to_string()) && dbs.contains(&"banvietbank".to_string()), "{dbs:?}");
        let _ = script;
    });
}

#[gpui::test]
fn a_lone_group_chip_leads_the_tabs_row_after_pinned_and_global_tabs(cx: &mut TestAppContext) {
    let (app, visual) = pn_app(cx);
    let pg = crate::ui_fixture::POSTGRES_ID;
    let welcome = tb_order(&app, visual)[0];
    let pinned = tb_open(&app, visual, "pinned", pg, "warehouse");
    let a = tb_open(&app, visual, "a", pg, "warehouse");
    let b = tb_open(&app, visual, "b", pg, "warehouse");
    app.update(visual, |this, cx| {
        this.workspace.set_pinned(pinned, true);
        this.set_tab_rows(true, cx);
    });
    tb_settle(visual);

    // One group: no chip row, the chip sits in the tabs row between the
    // pinned / global tabs and the group's tabs, followed by a separator.
    assert!(visual.debug_bounds("workspace-group-bar").is_none());
    let row = visual.debug_bounds("workspace-tab-bar").unwrap();
    assert_eq!(row.size.height, px(theme::TAB_HEIGHT), "no extra row");
    let (pinned_at, welcome_at) = (tb_bounds(visual, pinned), tb_bounds(visual, welcome));
    let chip = tr_chip(visual, 0);
    let separator = visual.debug_bounds("tab-group-separator").expect("separator after the chip");
    let (a_at, b_at) = (tb_bounds(visual, a), tb_bounds(visual, b));
    assert!(row.contains(&chip.center()));
    assert!(pinned_at.right() <= welcome_at.left(), "pinned first, then global");
    assert!(welcome_at.right() <= chip.left(), "global tabs, then the chip");
    assert!(chip.right() <= separator.left() && separator.right() <= a_at.left());
    assert!(a_at.right() <= b_at.left());
    // The chip is the real chip: state dot, database-less label, count 3? (pinned is not in it).
    assert!(visual.debug_bounds("tab-group-chip-state-0").is_some());
    app.read_with(visual, |this, _| {
        let rows = this.workspace.pane_rows(this.workspace.focused_pane());
        assert_eq!(rows.chips.len(), 1);
        assert_eq!(rows.chips[0].count, 2, "a and b; the pinned tab is global");
    });
    // Its right-click menu is the chip menu: New Query opens a query tab.
    let before = tb_order(&app, visual).len();
    tr_chip_menu(visual, 0, 1);
    assert_eq!(tb_order(&app, visual).len(), before + 1);

    // A second group moves the chips to their own row, as before.
    tb_open(&app, visual, "s", crate::ui_fixture::SQLITE_ID, "main");
    assert!(visual.debug_bounds("workspace-group-bar").is_some());
    assert!(visual.debug_bounds("tab-group-separator").is_none());
    let bar = visual.debug_bounds("workspace-group-bar").unwrap();
    assert!(bar.contains(&tr_chip(visual, 0).center()));
}

fn tr_strips(
    app: &gpui::Entity<AppRoot>,
    visual: &mut gpui::VisualTestContext,
) -> Vec<crate::workspace::StripSettings> {
    app.read_with(visual, |this, _| {
        this.workspace.panes().panes().map(|pane| pane.strip()).collect()
    })
}

fn tr_top(visual: &mut gpui::VisualTestContext, selector: &'static str) -> gpui::Pixels {
    visual.debug_bounds(selector).unwrap_or_else(|| panic!("{selector}")).top()
}

#[gpui::test]
fn side_by_side_grouped_panes_align_their_chip_rows_without_touching_any_setting(
    cx: &mut TestAppContext,
) {
    use crate::sidebar_preferences::TabGroupBy;
    let (app, visual, tabs) = tr_app(cx);
    app.update(visual, |this, cx| this.set_tab_rows(true, cx));
    tb_settle(visual);
    let left = pn_focused(&app, visual);
    // Right pane: one sqlite tab (one group). Left keeps two groups.
    pn_drag_to(visual, tabs.sq[1], left, 0.95, 0.5, true);
    let right = pn_panes(&app, visual)[1].0;
    let settings = tr_strips(&app, visual);

    // Both panes have a chip row; the lone chip is on it; tab rows line up.
    assert_eq!(tr_top(visual, "workspace-tab-bar"), tr_top(visual, "workspace-tab-bar-p1"));
    assert_eq!(tr_top(visual, "workspace-group-bar"), tr_top(visual, "workspace-group-bar-p1"));
    assert_eq!(tr_chip_count(visual, "-p1"), 1);
    assert!(visual.debug_bounds("tab-group-separator").is_none());
    // An ungrouped neighbour keeps one row (no spacer band) and the grouped
    // pane then has no reason to show a chip row for its lone chip.
    app.update(visual, |this, cx| this.set_pane_group_by(right, TabGroupBy::None, cx));
    tb_settle(visual);
    assert!(visual.debug_bounds("workspace-group-bar-p1").is_none());
    assert!(visual.debug_bounds("workspace-group-bar").is_some(), "left keeps its chip row");
    let right_row = visual.debug_bounds("workspace-tab-bar-p1").unwrap();
    assert_eq!(right_row.size.height, px(theme::TAB_HEIGHT));
    assert!(right_row.top() < tr_top(visual, "workspace-tab-bar"), "no spacer: tab row sits higher");
    // A pane that is not in rows mode is excluded the same way.
    app.update(visual, |this, cx| {
        this.set_pane_group_by(right, TabGroupBy::Connection, cx);
        this.set_pane_tab_rows(right, false, cx);
    });
    tb_settle(visual);
    assert!(visual.debug_bounds("workspace-group-bar-p1").is_none());
    assert!(visual.debug_bounds("tab-group-separator").is_none(), "rows off: plain tabs");

    // Back in rows mode it aligns again; the alignment never changed settings.
    app.update(visual, |this, cx| this.set_pane_tab_rows(right, true, cx));
    tb_settle(visual);
    assert_eq!(tr_top(visual, "workspace-tab-bar"), tr_top(visual, "workspace-tab-bar-p1"));
    assert_eq!(tr_strips(&app, visual), settings);
}

#[gpui::test]
fn stacked_panes_do_not_align_and_a_lone_chip_stays_inline(cx: &mut TestAppContext) {
    let (app, visual, tabs) = tr_app(cx);
    app.update(visual, |this, cx| this.set_tab_rows(true, cx));
    tb_settle(visual);
    let top = pn_focused(&app, visual);
    // Split the pane downwards (column axis), moving one sqlite tab to the bottom.
    app.update(visual, |this, cx| {
        this.split_tab_ui(tabs.sq[1], crate::panes::Edge::Bottom, cx);
    });
    tb_settle(visual);
    let panes = pn_panes(&app, visual);
    assert_eq!(panes.len(), 2);
    assert_ne!(panes[0].0, panes[1].0);
    let _ = top;
    // Top pane: two groups -> chip row. Bottom pane: one group -> inline chip.
    assert!(visual.debug_bounds("workspace-group-bar").is_some());
    assert!(visual.debug_bounds("workspace-group-bar-p1").is_none(), "column splits do not align");
    assert!(visual.debug_bounds("tab-group-separator").is_some(), "lone chip inline in the lower pane");
    assert_eq!(tr_chip_count(visual, "-p1"), 1);
}
