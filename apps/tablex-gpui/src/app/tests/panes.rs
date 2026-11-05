// Split panes through the real UI: tab drags onto pane edges / centres / strips,
// context-menu splits, focus, close, groups per pane, session and splitters.

use crate::panes::{Edge, Node, PaneId, SplitAxis};

fn pn_app(cx: &mut TestAppContext) -> (gpui::Entity<AppRoot>, &mut gpui::VisualTestContext) {
    let (app, visual) = tb_app(cx);
    visual.simulate_resize(gpui::size(px(1300.0), px(900.0)));
    tb_settle(visual);
    (app, visual)
}

/// Every pane in visual order with its tabs.
fn pn_panes(
    app: &gpui::Entity<AppRoot>,
    visual: &mut gpui::VisualTestContext,
) -> Vec<(PaneId, Vec<u64>)> {
    app.read_with(visual, |this, _| {
        this.workspace
            .panes()
            .panes()
            .map(|pane| (pane.id(), pane.tabs().to_vec()))
            .collect()
    })
}

fn pn_tabs(app: &gpui::Entity<AppRoot>, visual: &mut gpui::VisualTestContext) -> Vec<Vec<u64>> {
    pn_panes(app, visual)
        .into_iter()
        .map(|(_, tabs)| tabs)
        .collect()
}

fn pn_focused(app: &gpui::Entity<AppRoot>, visual: &mut gpui::VisualTestContext) -> PaneId {
    app.read_with(visual, |this, _| this.workspace.focused_pane())
}

fn pn_pane_bounds(
    visual: &mut gpui::VisualTestContext,
    pane: PaneId,
) -> gpui::Bounds<gpui::Pixels> {
    let selector: &'static str = Box::leak(format!("pane-{}", pane.get()).into_boxed_str());
    visual
        .debug_bounds(selector)
        .unwrap_or_else(|| panic!("pane {} is rendered", pane.get()))
}

/// A point in a pane's content area (below its tab strip), as fractions of it.
fn pn_content_at(
    visual: &mut gpui::VisualTestContext,
    pane: PaneId,
    fx: f32,
    fy: f32,
) -> gpui::Point<gpui::Pixels> {
    let bounds = pn_pane_bounds(visual, pane);
    let strip = px(theme::TAB_HEIGHT);
    gpui::point(
        bounds.left() + bounds.size.width * fx,
        bounds.top() + strip + (bounds.size.height - strip) * fy,
    )
}

/// Three query tabs in one pane: `[welcome, a, b]`.
fn pn_three(
    app: &gpui::Entity<AppRoot>,
    visual: &mut gpui::VisualTestContext,
) -> (u64, u64, u64) {
    let welcome = tb_order(app, visual)[0];
    let a = tb_open(app, visual, "A", "", "");
    let b = tb_open(app, visual, "B", "", "");
    (welcome, a, b)
}

fn pn_release_at(visual: &mut gpui::VisualTestContext, pane: PaneId, fx: f32, fy: f32) {
    let at = pn_content_at(visual, pane, fx, fy);
    visual.simulate_mouse_up(at, gpui::MouseButton::Left, Default::default());
    tb_settle(visual);
}

fn pn_click_at(visual: &mut gpui::VisualTestContext, pane: PaneId, fx: f32, fy: f32) {
    let at = pn_content_at(visual, pane, fx, fy);
    visual.simulate_click(at, Default::default());
    tb_settle(visual);
}

fn pn_click_tab(visual: &mut gpui::VisualTestContext, tab: u64) {
    let at = tb_bounds(visual, tab).center();
    visual.simulate_click(at, Default::default());
    tb_settle(visual);
}

fn pn_drawn(visual: &mut gpui::VisualTestContext, pane: PaneId) -> bool {
    let selector: &'static str = Box::leak(format!("pane-{}", pane.get()).into_boxed_str());
    visual.debug_bounds(selector).is_some()
}

/// Drags `tab` into a pane's content area at the given fractions.
fn pn_drag_to(
    visual: &mut gpui::VisualTestContext,
    tab: u64,
    pane: PaneId,
    fx: f32,
    fy: f32,
    release: bool,
) {
    let to = pn_content_at(visual, pane, fx, fy);
    pn_drag_tab(visual, tab, to, release);
}

/// Drops `tab` on the left / right half of another tab and releases.
fn pn_drag_onto_tab(visual: &mut gpui::VisualTestContext, tab: u64, anchor: u64, left_half: bool) {
    let bounds = tb_bounds(visual, anchor);
    let to = if left_half { tb_left_of(bounds) } else { tb_right_of(bounds) };
    pn_drag_tab(visual, tab, to, true);
}

/// Drags tab `tab` onto `to`, optionally releasing.
fn pn_drag_tab(
    visual: &mut gpui::VisualTestContext,
    tab: u64,
    to: gpui::Point<gpui::Pixels>,
    release: bool,
) {
    let from = tb_bounds(visual, tab).center();
    tb_drag(visual, from, to, release);
}

#[gpui::test]
fn dragging_a_tab_to_the_right_edge_splits_side_by_side(cx: &mut TestAppContext) {
    let (app, visual) = pn_app(cx);
    let (welcome, a, b) = pn_three(&app, visual);
    let pane = pn_focused(&app, visual);
    let to = pn_content_at(visual, pane, 0.95, 0.5);
    pn_drag_tab(visual, b, to, false);
    // The drop-zone overlay previews the right half before the release.
    let overlay = visual.debug_bounds("pane-drop-overlay").expect("overlay while dragging");
    let area = pn_pane_bounds(visual, pane);
    assert!(
        (overlay.left() - (area.left() + area.size.width * 0.5)).abs() < px(2.0),
        "right half highlighted: {overlay:?} in {area:?}"
    );
    visual.simulate_mouse_up(to, gpui::MouseButton::Left, Default::default());
    tb_settle(visual);
    assert_eq!(pn_tabs(&app, visual), vec![vec![welcome, a], vec![b]]);
    assert!(visual.debug_bounds("pane-drop-overlay").is_none(), "overlay cleared");
    app.read_with(visual, |this, _| {
        let Node::Split(split) = this.workspace.panes().root() else { panic!("split root") };
        assert_eq!(split.axis(), SplitAxis::Row);
        assert_eq!(this.workspace.active_tab().map(|tab| tab.id), Some(b), "the moved tab is active");
        assert_eq!(this.workspace.pane_of(b), Some(this.workspace.focused_pane()));
    });
    // Side by side: both strips on the same row, second to the right.
    let panes = pn_panes(&app, visual);
    let (left, right) = (pn_pane_bounds(visual, panes[0].0), pn_pane_bounds(visual, panes[1].0));
    assert!(right.left() >= left.right() - px(1.0) && (right.top() - left.top()).abs() < px(1.0));
    assert!(visual.debug_bounds("pane-focus-line").is_some(), "the focused strip is marked");
    // The moved tab is drawn in the right pane's strip.
    assert!(tb_bounds(visual, b).left() >= right.left());
}

#[gpui::test]
fn dragging_a_tab_to_the_bottom_edge_splits_vertically(cx: &mut TestAppContext) {
    let (app, visual) = pn_app(cx);
    let (welcome, a, b) = pn_three(&app, visual);
    let pane = pn_focused(&app, visual);
    pn_drag_to(visual, a, pane, 0.5, 0.97, true);
    assert_eq!(pn_tabs(&app, visual), vec![vec![welcome, b], vec![a]]);
    app.read_with(visual, |this, _| {
        let Node::Split(split) = this.workspace.panes().root() else { panic!("split root") };
        assert_eq!(split.axis(), SplitAxis::Column);
    });
    let panes = pn_panes(&app, visual);
    let (top, bottom) = (pn_pane_bounds(visual, panes[0].0), pn_pane_bounds(visual, panes[1].0));
    assert!(bottom.top() >= top.bottom() - px(1.0), "stacked");
    assert!((top.size.width - bottom.size.width).abs() < px(1.0));
}

#[gpui::test]
fn dragging_to_the_top_and_left_edges_puts_the_new_pane_first(cx: &mut TestAppContext) {
    let (app, visual) = pn_app(cx);
    let (welcome, a, b) = pn_three(&app, visual);
    let pane = pn_focused(&app, visual);
    pn_drag_to(visual, b, pane, 0.04, 0.5, true);
    assert_eq!(pn_tabs(&app, visual), vec![vec![b], vec![welcome, a]]);
    let right = pn_panes(&app, visual)[1].0;
    pn_drag_to(visual, a, right, 0.5, 0.03, true);
    app.read_with(visual, |this, _| {
        let Node::Split(outer) = this.workspace.panes().root() else { panic!("split root") };
        assert_eq!(outer.axis(), SplitAxis::Row);
        let Node::Split(inner) = &outer.children()[1] else { panic!("nested column") };
        assert_eq!(inner.axis(), SplitAxis::Column);
    });
    assert_eq!(pn_tabs(&app, visual), vec![vec![b], vec![a], vec![welcome]]);
}

#[gpui::test]
fn dropping_on_a_strip_tab_inserts_at_that_position_across_panes(cx: &mut TestAppContext) {
    let (app, visual) = pn_app(cx);
    let (welcome, a, b) = pn_three(&app, visual);
    let c = tb_open(&app, visual, "C", "", "");
    let pane = pn_focused(&app, visual);
    // [welcome a b c] -> [welcome a] | [b c]
    pn_drag_to(visual, b, pane, 0.95, 0.5, true);
    let right = pn_panes(&app, visual)[1].0;
    // Centre of the other pane: appended and activated there.
    pn_drag_to(visual, c, right, 0.5, 0.5, true);
    assert_eq!(pn_tabs(&app, visual), vec![vec![welcome, a], vec![b, c]]);
    app.read_with(visual, |this, _| {
        assert_eq!(this.workspace.panes().pane(right).unwrap().active(), Some(c));
        assert_eq!(this.workspace.focused_pane(), right);
    });
    // `a` onto the right half of `b`'s tab: after it.
    pn_drag_onto_tab(visual, a, b, false);
    assert_eq!(pn_tabs(&app, visual), vec![vec![welcome], vec![b, a, c]]);
    // `welcome` onto the left half of `c`'s tab: before it.
    pn_drag_onto_tab(visual, welcome, c, true);
    assert_eq!(pn_tabs(&app, visual), vec![vec![b, a, welcome, c]], "the emptied pane is gone");
}

#[gpui::test]
fn dragging_the_last_tab_out_of_a_pane_removes_the_pane(cx: &mut TestAppContext) {
    let (app, visual) = pn_app(cx);
    let (welcome, a, b) = pn_three(&app, visual);
    let pane = pn_focused(&app, visual);
    pn_drag_to(visual, b, pane, 0.95, 0.5, true);
    let panes = pn_panes(&app, visual);
    assert_eq!(panes.len(), 2);
    // Back into the first pane through its content.
    pn_drag_to(visual, b, panes[0].0, 0.5, 0.5, true);
    assert_eq!(pn_tabs(&app, visual), vec![vec![welcome, a, b]]);
    app.read_with(visual, |this, _| {
        assert!(matches!(this.workspace.panes().root(), Node::Pane(_)), "tree collapsed");
        assert_eq!(this.workspace.focused_pane(), panes[0].0);
    });
    assert!(visual.debug_bounds("pane-focus-line").is_none(), "no marker with one pane");
}

#[gpui::test]
fn dropping_a_pane_onto_itself_or_splitting_its_only_tab_is_ignored(cx: &mut TestAppContext) {
    let (app, visual) = pn_app(cx);
    let (welcome, a, b) = pn_three(&app, visual);
    let pane = pn_focused(&app, visual);
    // Centre of its own pane: nothing to do, and no overlay.
    pn_drag_to(visual, b, pane, 0.5, 0.5, false);
    assert!(visual.debug_bounds("pane-drop-overlay").is_none());
    pn_release_at(visual, pane, 0.5, 0.5);
    tb_settle(visual);
    assert_eq!(pn_tabs(&app, visual), vec![vec![welcome, a, b]]);
    // Escape cancels a drag over an edge.
    pn_drag_to(visual, b, pane, 0.95, 0.5, false);
    assert!(visual.debug_bounds("pane-drop-overlay").is_some());
    visual.simulate_keystrokes("escape");
    tb_settle(visual);
    assert!(visual.debug_bounds("pane-drop-overlay").is_none(), "overlay gone after Escape");
    assert_eq!(pn_tabs(&app, visual), vec![vec![welcome, a, b]]);
    // A pane with one tab cannot be split off itself.
    pn_drag_to(visual, b, pane, 0.95, 0.5, true);
    let right = pn_panes(&app, visual)[1].0;
    pn_drag_to(visual, b, right, 0.97, 0.5, false);
    assert!(visual.debug_bounds("pane-drop-overlay").is_none(), "no zone for a lone tab");
    pn_release_at(visual, right, 0.97, 0.5);
    tb_settle(visual);
    assert_eq!(pn_tabs(&app, visual), vec![vec![welcome, a], vec![b]]);
}

#[gpui::test]
fn context_menu_splits_and_pane_commands(cx: &mut TestAppContext) {
    let (app, visual) = pn_app(cx);
    let (welcome, a, b) = pn_three(&app, visual);
    tb_pick_tab_menu(&app, visual, b, "Split Right");
    assert_eq!(pn_tabs(&app, visual), vec![vec![welcome, a], vec![b]]);
    app.read_with(visual, |this, _| {
        let Node::Split(split) = this.workspace.panes().root() else { panic!("split root") };
        assert_eq!(split.axis(), SplitAxis::Row);
    });
    // The right pane holds one tab: Split is not offered for it, the pane
    // commands are.
    let labels = tb_labels(&tb_menu_entries(&app, visual, b));
    assert!(!labels.contains(&"Split Right") && !labels.contains(&"Split Down"), "{labels:?}");
    assert!(labels.contains(&"Move to Other Pane") && labels.contains(&"Close Pane"), "{labels:?}");
    tb_pick_tab_menu(&app, visual, a, "Split Down");
    assert_eq!(pn_tabs(&app, visual), vec![vec![welcome], vec![a], vec![b]]);
    app.read_with(visual, |this, _| {
        let Node::Split(outer) = this.workspace.panes().root() else { panic!("split root") };
        assert!(matches!(&outer.children()[0], Node::Split(inner) if inner.axis() == SplitAxis::Column));
    });
    tb_pick_tab_menu(&app, visual, a, "Move to Other Pane");
    let tabs = pn_tabs(&app, visual);
    assert_eq!(tabs.len(), 2, "the emptied pane is removed: {tabs:?}");
    // Close Pane folds the tabs into the neighbour; nothing is closed.
    let last = *tabs.last().unwrap().last().unwrap();
    tb_pick_tab_menu(&app, visual, last, "Close Pane");
    let tabs = pn_tabs(&app, visual);
    assert_eq!(tabs.len(), 1);
    let mut all = tabs[0].clone();
    all.sort_unstable();
    assert_eq!(all, vec![welcome, a, b]);
}

#[gpui::test]
fn new_tabs_open_in_the_focused_pane_and_cmd_w_closes_there(cx: &mut TestAppContext) {
    let (app, visual) = pn_app(cx);
    let (welcome, a, b) = pn_three(&app, visual);
    let pane = pn_focused(&app, visual);
    pn_drag_to(visual, b, pane, 0.95, 0.5, true);
    let panes = pn_panes(&app, visual);
    let (left, right) = (panes[0].0, panes[1].0);
    assert_eq!(pn_focused(&app, visual), right);
    click_debug_button(visual, "global-new-query");
    tb_settle(visual);
    let right_tabs = pn_tabs(&app, visual)[1].clone();
    assert_eq!(right_tabs.len(), 2, "opened in the focused (right) pane");
    let c = right_tabs[1];
    // Clicking a tab of the left pane focuses that pane.
    pn_click_tab(visual, a);
    tb_settle(visual);
    assert_eq!(pn_focused(&app, visual), left);
    click_debug_button(visual, "global-new-query");
    tb_settle(visual);
    let tabs = pn_tabs(&app, visual);
    assert_eq!(tabs[0].len(), 3, "opened in the left pane now: {tabs:?}");
    assert_eq!(tabs[1], right_tabs);
    // Cmd-W closes the focused pane's active tab only.
    let new_left = *tabs[0].last().unwrap();
    tb_focus_root(&app, visual);
    visual.simulate_keystrokes("secondary-w");
    tb_settle(visual);
    let tabs = pn_tabs(&app, visual);
    assert_eq!(tabs[0], vec![welcome, a]);
    assert_eq!(tabs[1], right_tabs, "the other pane is untouched");
    assert!(!tabs.concat().contains(&new_left));
    // Focus the right pane through its content: Cmd-W closes there.
    pn_click_at(visual, right, 0.5, 0.5);
    tb_settle(visual);
    assert_eq!(pn_focused(&app, visual), right);
    tb_focus_root(&app, visual);
    visual.simulate_keystrokes("secondary-w");
    tb_settle(visual);
    assert_eq!(pn_tabs(&app, visual)[1], vec![b]);
    assert!(!pn_tabs(&app, visual).concat().contains(&c));
}

#[gpui::test]
fn tab_groups_collapse_per_pane(cx: &mut TestAppContext) {
    let (app, visual) = pn_app(cx);
    tb_set_group_by(&app, visual, crate::workspace::TabGroupBy::Connection);
    let pg = crate::ui_fixture::POSTGRES_ID;
    let p1 = tb_open(&app, visual, "p1", pg, "warehouse");
    let p2 = tb_open(&app, visual, "p2", pg, "warehouse");
    let p3 = tb_open(&app, visual, "p3", pg, "warehouse");
    let pane = pn_focused(&app, visual);
    // [welcome p1 p2 p3] -> [welcome p1 p2] | [p3]
    pn_drag_to(visual, p3, pane, 0.95, 0.5, true);
    let right = pn_panes(&app, visual)[1].0;
    // Another connection's tab next to p3 so the right pane keeps a tab
    // outside the group while collapsed.
    pn_drag_to(visual, p1, right, 0.5, 0.5, true);
    // An ungrouped tab beside them, so the right pane can collapse its group.
    let plain = tb_open(&app, visual, "plain", "", "");
    pn_drag_to(visual, plain, right, 0.5, 0.5, true);
    let right_tabs = pn_tabs(&app, visual)[1].clone();
    assert!(right_tabs.contains(&p3) && right_tabs.contains(&p1) && right_tabs.contains(&plain));
    app.update(visual, |this, cx| {
        let key = crate::workspace::TabGroupKey {
            connection_id: pg.into(),
            database: String::new(),
        };
        let left = this.workspace.panes().panes().next().unwrap().id();
        // Collapse in the left pane only: its active tab is p2 (in the group),
        // the welcome tab takes over.
        assert!(this.workspace.toggle_group_collapsed_in(left, &key));
        cx.notify();
    });
    tb_settle(visual);
    assert!(visual.debug_bounds(tb_sel(p2)).is_none(), "left pane's group is collapsed");
    assert!(visual.debug_bounds(tb_sel(p3)).is_some(), "right pane's tabs stay visible");
    assert!(visual.debug_bounds(tb_sel(p1)).is_some());
    // Clicking the right pane's own chip collapses its group independently.
    let chip = visual.debug_bounds("tab-group-chip-0-p1").expect("right pane chip");
    visual.simulate_click(chip.center(), Default::default());
    tb_settle(visual);
    app.read_with(visual, |this, _| {
        for pane in this.workspace.panes().panes() {
            assert_eq!(pane.collapsed().len(), 1, "each pane collapsed its own group");
        }
    });
}

#[gpui::test]
fn splitter_drag_changes_the_stored_ratio_and_marks_the_session_dirty(cx: &mut TestAppContext) {
    let (app, visual) = pn_app(cx);
    let (_welcome, _a, b) = pn_three(&app, visual);
    let pane = pn_focused(&app, visual);
    pn_drag_to(visual, b, pane, 0.95, 0.5, true);
    let panes = pn_panes(&app, visual);
    let left = pn_pane_bounds(visual, panes[0].0);
    let before = app.read_with(visual, |this, _| this.workspace.revision());
    let handle = gpui::point(left.right(), left.center().y);
    let left_button = Some(gpui::MouseButton::Left);
    visual.simulate_mouse_down(handle, gpui::MouseButton::Left, Default::default());
    visual.simulate_mouse_move(handle + gpui::point(px(-60.0), px(0.0)), left_button, Default::default());
    visual.simulate_mouse_move(handle + gpui::point(px(-200.0), px(0.0)), left_button, Default::default());
    visual.simulate_mouse_up(handle + gpui::point(px(-200.0), px(0.0)), gpui::MouseButton::Left, Default::default());
    tb_settle(visual);
    app.read_with(visual, |this, _| {
        let Node::Split(split) = this.workspace.panes().root() else { panic!("split root") };
        assert!(split.ratios()[0] < 0.45, "left pane shrank: {:?}", split.ratios());
        assert!(this.workspace.revision() != before, "the session saver sees the change");
    });
    let after = pn_pane_bounds(visual, panes[0].0);
    assert!(after.size.width < left.size.width - px(100.0));
    // Double-click on the splitter resets to equal shares.
    let handle = gpui::point(after.right(), after.center().y);
    visual.simulate_event(gpui::MouseDownEvent {
        position: handle,
        modifiers: Default::default(),
        button: gpui::MouseButton::Left,
        click_count: 2,
        first_mouse: false,
    });
    visual.simulate_mouse_up(handle, gpui::MouseButton::Left, Default::default());
    tb_settle(visual);
    app.read_with(visual, |this, _| {
        let Node::Split(split) = this.workspace.panes().root() else { panic!("split root") };
        assert!((split.ratios()[0] - 0.5).abs() < 0.01, "{:?}", split.ratios());
    });
}

#[gpui::test]
fn session_round_trip_restores_panes_ratios_and_focus(cx: &mut TestAppContext) {
    let (app, visual) = pn_app(cx);
    let dir = tb_session_dir();
    let path = dir.join("gpui-workspace-session.json");
    app.update(visual, |this, _| this.shell.session.path = Some(path.clone()));
    let (welcome, a, b) = pn_three(&app, visual);
    let pane = pn_focused(&app, visual);
    pn_drag_to(visual, b, pane, 0.95, 0.5, true);
    app.update(visual, |this, cx| {
        let Node::Split(split) = this.workspace.panes().root() else { panic!("split root") };
        let id = split.id();
        this.workspace.set_split_ratios(id, &[0.3, 0.7]);
        let first = this.workspace.panes().panes().next().unwrap().id();
        this.workspace.focus_pane(first);
        cx.notify();
    });
    tb_settle(visual);
    for _ in 0..3 {
        tb_advance(visual, 700);
    }
    let session = tb_read_session(&path);
    assert_eq!(session.tabs.len(), 3);
    assert!(session.panes.is_some(), "multi-pane layouts are persisted");
    tb_relaunch(&app, visual, tb_no_swift);
    let _ = (welcome, a, b);
    app.read_with(visual, |this, _| {
        let titles: Vec<Vec<String>> = this
            .workspace
            .panes()
            .panes()
            .map(|pane| {
                pane.tabs()
                    .iter()
                    .map(|id| this.workspace.tab(*id).unwrap().title.clone())
                    .collect()
            })
            .collect();
        assert_eq!(titles, vec![vec!["Welcome".to_string(), "A".into()], vec!["B".to_string()]]);
        let Node::Split(split) = this.workspace.panes().root() else { panic!("split root") };
        assert!((split.ratios()[0] - 0.3).abs() < 0.01, "{:?}", split.ratios());
        let focused = this.workspace.panes().focused_pane().unwrap();
        assert!(focused.tabs().len() == 2, "the first pane was focused");
    });
    // And the layout is drawn: two panes with the saved shares.
    let panes = pn_panes(&app, visual);
    let (left, right) = (pn_pane_bounds(visual, panes[0].0), pn_pane_bounds(visual, panes[1].0));
    let share = f32::from(left.size.width) / f32::from(left.size.width + right.size.width);
    assert!((share - 0.3).abs() < 0.03, "restored widths {share}");
    std::fs::remove_dir_all(dir).unwrap();
}

#[gpui::test]
fn sessions_without_panes_load_as_one_pane(cx: &mut TestAppContext) {
    let (app, visual) = pn_app(cx);
    let dir = tb_session_dir();
    let path = dir.join("gpui-workspace-session.json");
    // A file written before panes existed has no `panes` key.
    let text = r#"{"version":1,"tabs":[{"kind":"welcome","title":"Welcome"},{"kind":"query","title":"Old"}],"active":1,"collapsed_groups":[]}"#;
    std::fs::write(&path, text).unwrap();
    app.update(visual, |this, _| this.shell.session.path = Some(path.clone()));
    tb_relaunch(&app, visual, tb_no_swift);
    app.read_with(visual, |this, _| {
        assert_eq!(this.workspace.panes().pane_count(), 1);
        assert_eq!(this.workspace.tabs().len(), 2);
        assert_eq!(this.workspace.active_tab().map(|tab| tab.title.as_str()), Some("Old"));
    });
    std::fs::remove_dir_all(dir).unwrap();
}

#[gpui::test]
fn focus_follows_clicks_and_directional_and_palette_commands(cx: &mut TestAppContext) {
    let (app, visual) = pn_app(cx);
    let (_welcome, _a, b) = pn_three(&app, visual);
    let pane = pn_focused(&app, visual);
    pn_drag_to(visual, b, pane, 0.95, 0.5, true);
    let panes = pn_panes(&app, visual);
    let (left, right) = (panes[0].0, panes[1].0);
    assert_eq!(pn_focused(&app, visual), right);
    tb_focus_root(&app, visual);
    // Cmd+Alt+arrows on macOS, Alt+arrows elsewhere: the chords the shortcut table binds here.
    let chord = |id: &str| shortcuts::find(id).expect("registered action").default_chords().remove(0);
    let (focus_left, focus_right, focus_up) = (
        chord("workspace.focus_pane_left"),
        chord("workspace.focus_pane_right"),
        chord("workspace.focus_pane_up"),
    );
    visual.simulate_keystrokes(&focus_left);
    tb_settle(visual);
    assert_eq!(pn_focused(&app, visual), left, "Focus Pane Left");
    visual.simulate_keystrokes(&focus_left);
    assert_eq!(pn_focused(&app, visual), left, "nothing further left");
    visual.simulate_keystrokes(&focus_right);
    tb_settle(visual);
    assert_eq!(pn_focused(&app, visual), right, "Focus Pane Right");
    visual.simulate_keystrokes(&focus_up);
    assert_eq!(pn_focused(&app, visual), right, "no pane above");
    // Palette: Focus Next Pane / Split Editor Right / Down.
    app.update(visual, |this, cx| {
        this.focus_next_pane(cx);
    });
    assert_eq!(pn_focused(&app, visual), left);
    visual.update(|window, cx| {
        app.update(cx, |this, cx| {
            this.split_editor(Edge::Bottom, window, cx);
        });
    });
    tb_settle(visual);
    let panes = pn_panes(&app, visual);
    assert_eq!(panes.len(), 3, "Split Editor Down split the focused pane: {panes:?}");
    visual.update(|window, cx| {
        app.update(cx, |this, cx| {
            // The right pane holds one tab: refused with a notice, no change.
            this.focus_pane_ui(right, cx);
            this.split_editor(Edge::Right, window, cx);
        });
    });
    assert_eq!(pn_panes(&app, visual).len(), 3);
}

#[gpui::test]
fn double_clicking_a_tab_maximizes_its_pane_and_again_restores(cx: &mut TestAppContext) {
    let (app, visual) = pn_app(cx);
    let (_welcome, a, b) = pn_three(&app, visual);
    let pane = pn_focused(&app, visual);
    pn_drag_to(visual, b, pane, 0.95, 0.5, true);
    let panes = pn_panes(&app, visual);
    let double = |visual: &mut gpui::VisualTestContext, at: gpui::Point<gpui::Pixels>| {
        for count in [1, 2] {
            visual.simulate_event(gpui::MouseDownEvent {
                position: at,
                modifiers: Default::default(),
                button: gpui::MouseButton::Left,
                click_count: count,
                first_mouse: false,
            });
            visual.simulate_event(gpui::MouseUpEvent {
                position: at,
                modifiers: Default::default(),
                button: gpui::MouseButton::Left,
                click_count: count,
            });
        }
        tb_settle(visual);
    };
    let at = tb_bounds(visual, a).center();
    double(visual, at);
    app.read_with(visual, |this, _| {
        assert_eq!(this.workspace.panes().maximized(), Some(panes[0].0));
    });
    assert!(!pn_drawn(visual, panes[1].0), "the other pane is not drawn");
    let at = tb_bounds(visual, a).center();
    double(visual, at);
    app.read_with(visual, |this, _| assert_eq!(this.workspace.panes().maximized(), None));
    assert!(pn_drawn(visual, panes[1].0));
}

#[gpui::test]
fn the_sql_editor_of_an_unfocused_pane_still_saves_its_text(cx: &mut TestAppContext) {
    let (app, visual) = pn_app(cx);
    let (_welcome, a, b) = pn_three(&app, visual);
    let pane = pn_focused(&app, visual);
    pn_drag_to(visual, b, pane, 0.95, 0.5, true);
    // Focus the left pane: B's editor (right pane) is now unfocused but drawn.
    pn_click_tab(visual, a);
    tb_settle(visual);
    let editor = app.read_with(visual, |this, _| this.query_editors.by_tab[&b].clone());
    // Keyboard focus on B's editor without a mouse press: the workspace focus
    // stays with the left pane.
    visual.update(|window, cx| editor.update(cx, |state, cx| state.focus(window, cx)));
    visual.simulate_input("SELECT 42");
    tb_settle(visual);
    app.read_with(visual, |this, _| {
        assert_eq!(this.workspace.tab(b).unwrap().sql, "SELECT 42");
        assert_eq!(this.workspace.active_tab().map(|tab| tab.id), Some(a));
    });
}

#[gpui::test]
fn drag_hover_re_renders_only_the_involved_pane_views(cx: &mut TestAppContext) {
    let (app, visual) = pn_app(cx);
    let (_welcome, _a, b) = pn_three(&app, visual);
    let pane = pn_focused(&app, visual);
    pn_drag_to(visual, b, pane, 0.95, 0.5, true);
    let panes = pn_panes(&app, visual);
    let snapshot = |id: PaneId| panes_ui::PANE_RENDERS.with(|r| r.borrow().get(&id.get()).copied().unwrap_or(0));
    let results = || results::surface::RENDERS.with(|r| r.borrow().values().sum::<u64>());
    let (left_before, right_before, results_before) = (snapshot(panes[0].0), snapshot(panes[1].0), results());
    // Hover the right pane's edge zone with a tab from the left pane: the
    // overlay lives in the right pane only.
    let welcome = tb_order(&app, visual)[0];
    let target = pn_content_at(visual, panes[1].0, 0.95, 0.5);
    pn_drag_tab(visual, welcome, target, false);
    assert!(visual.debug_bounds("pane-drop-overlay").is_some());
    assert!(snapshot(panes[1].0) > right_before, "the hovered pane re-rendered for its overlay");
    // The drag moved over the left strip first (hover indicator), which is
    // allowed to touch panes; results views never re-render for a hover.
    assert_eq!(results(), results_before, "tab results are replayed while dragging");
    let _ = left_before;
    visual.simulate_mouse_up(target, gpui::MouseButton::Left, Default::default());
    tb_settle(visual);
}

#[gpui::test]
fn split_panes_scenario_shows_the_grid_beside_the_editor(cx: &mut TestAppContext) {
    let (app, visual) = context_test_app(cx, "split-panes", "populated");
    visual.simulate_resize(gpui::size(px(1300.0), px(900.0)));
    tb_settle(visual);
    let panes = pn_panes(&app, visual);
    assert_eq!(panes.len(), 2);
    app.read_with(visual, |this, _| {
        let Node::Split(split) = this.workspace.panes().root() else { panic!("split root") };
        assert!((split.ratios()[0] - 0.55).abs() < 0.01);
        let left = this.workspace.pane_active_tab(panes[0].0).unwrap();
        assert_eq!(left.title, "orders", "the grid tab is on the left");
        assert_eq!(this.workspace.focused_pane(), panes[0].0, "and focused");
        let right = this.workspace.pane_active_tab(panes[1].0).unwrap();
        assert_eq!(right.title, "Query");
    });
    let (left, right) = (pn_pane_bounds(visual, panes[0].0), pn_pane_bounds(visual, panes[1].0));
    let share = f32::from(left.size.width) / f32::from(left.size.width + right.size.width);
    assert!((share - 0.55).abs() < 0.03, "{share}");
    // The right pane draws its editor, the left one its result footer.
    assert!(visual.debug_bounds("query-editor-area").is_some());
    assert!(visual.debug_bounds("result-status-row").is_some());
    assert!(visual.debug_bounds("pane-drop-overlay").is_none());
}

#[gpui::test]
fn split_panes_drop_scenario_pins_the_drop_zone_overlay(cx: &mut TestAppContext) {
    let (app, visual) = context_test_app(cx, "split-panes-drop", "populated");
    visual.simulate_resize(gpui::size(px(1300.0), px(900.0)));
    tb_settle(visual);
    // The zone is sized from the painted pane bounds one frame later.
    app.update(visual, |this, cx| this.notify_pane_views(cx));
    tb_settle(visual);
    let panes = pn_panes(&app, visual);
    let right = pn_pane_bounds(visual, panes[1].0);
    let overlay = visual.debug_bounds("pane-drop-overlay").expect("pinned overlay");
    assert!(
        (overlay.left() - (right.left() + right.size.width * 0.5)).abs() < px(2.0),
        "right half of the right pane: {overlay:?} in {right:?}"
    );
}
