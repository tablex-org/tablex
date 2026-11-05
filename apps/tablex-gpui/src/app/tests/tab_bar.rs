// Workspace tab bar: drag reorder, pinning, groups, context menu, open-tabs picker.

fn tb_app(cx: &mut TestAppContext) -> (gpui::Entity<AppRoot>, &mut gpui::VisualTestContext) {
    let (app, visual) = sidebar_test_app(cx);
    visual.update(|_, cx| actions::bind_keys(cx));
    (app, visual)
}

fn tb_sel(id: u64) -> &'static str {
    Box::leak(format!("workspace-tab-{id}").into_boxed_str())
}

fn tb_open(
    app: &gpui::Entity<AppRoot>,
    visual: &mut gpui::VisualTestContext,
    title: &str,
    connection: &str,
    database: &str,
) -> u64 {
    let (title, connection, database) = (
        title.to_string(),
        connection.to_string(),
        database.to_string(),
    );
    let id = app.update(visual, |this, cx| {
        let id = this.workspace.open_query_tab_with_metadata(
            title,
            TabMetadata {
                connection_id: connection,
                database,
                ..Default::default()
            },
        );
        cx.notify();
        id
    });
    tb_settle(visual);
    id
}

fn tb_settle(visual: &mut gpui::VisualTestContext) {
    visual.update(|window, _| window.refresh());
    visual.run_until_parked();
    visual.update(|window, _| window.refresh());
    visual.run_until_parked();
}

fn tb_order(app: &gpui::Entity<AppRoot>, visual: &mut gpui::VisualTestContext) -> Vec<u64> {
    app.read_with(visual, |this, _| {
        this.workspace.tabs().iter().map(|tab| tab.id).collect()
    })
}

fn tb_bounds(visual: &mut gpui::VisualTestContext, id: u64) -> gpui::Bounds<gpui::Pixels> {
    visual
        .debug_bounds(tb_sel(id))
        .unwrap_or_else(|| panic!("tab {id} is rendered"))
}

/// Left-button drag with the harness' real mouse pipeline: down, move past the
/// drag threshold, hover the target, optionally release.
fn tb_drag(
    visual: &mut gpui::VisualTestContext,
    from: gpui::Point<gpui::Pixels>,
    to: gpui::Point<gpui::Pixels>,
    release: bool,
) {
    let left = Some(gpui::MouseButton::Left);
    visual.simulate_mouse_down(from, gpui::MouseButton::Left, Default::default());
    visual.simulate_mouse_move(
        from + gpui::point(px(8.0), px(0.0)),
        left,
        Default::default(),
    );
    visual.simulate_mouse_move(to, left, Default::default());
    visual.simulate_mouse_move(to + gpui::point(px(1.0), px(0.0)), left, Default::default());
    tb_settle(visual);
    if release {
        visual.simulate_mouse_up(to, gpui::MouseButton::Left, Default::default());
        tb_settle(visual);
    }
}

fn tb_left_of(bounds: gpui::Bounds<gpui::Pixels>) -> gpui::Point<gpui::Pixels> {
    gpui::point(bounds.origin.x + px(14.0), bounds.center().y)
}

fn tb_right_of(bounds: gpui::Bounds<gpui::Pixels>) -> gpui::Point<gpui::Pixels> {
    gpui::point(
        bounds.origin.x + bounds.size.width - px(14.0),
        bounds.center().y,
    )
}

#[gpui::test]
fn tabbar_drag_reorders_before_and_after_with_an_indicator(cx: &mut TestAppContext) {
    let (app, visual) = tb_app(cx);
    let welcome = tb_order(&app, visual)[0];
    let a = tb_open(&app, visual, "A", "", "");
    let b = tb_open(&app, visual, "B", "", "");
    let c = tb_open(&app, visual, "C", "", "");
    assert_eq!(tb_order(&app, visual), vec![welcome, a, b, c]);

    // Hover the left half of A: the accent indicator previews "before A".
    let from = tb_bounds(visual, c).center();
    let a_bounds = tb_bounds(visual, a);
    tb_drag(visual, from, tb_left_of(a_bounds), false);
    app.read_with(visual, |this, _| {
        let (target, plan) = this.shell.tab_bar.hover.clone().expect("drop candidate");
        assert_eq!(target, crate::workspace::TabDropTarget::Before(a));
        assert!(matches!(plan, crate::workspace::TabMovePlan::Move(_)));
    });
    assert!(
        visual.debug_bounds("tab-drop-indicator").is_some(),
        "insertion indicator is drawn"
    );
    visual.simulate_mouse_up(
        tb_left_of(a_bounds),
        gpui::MouseButton::Left,
        Default::default(),
    );
    tb_settle(visual);
    assert_eq!(
        tb_order(&app, visual),
        vec![welcome, c, a, b],
        "dropped before A"
    );
    assert!(
        visual.debug_bounds("tab-drop-indicator").is_none(),
        "indicator clears after the drop"
    );
    app.read_with(visual, |this, _| {
        assert!(this.shell.tab_bar.hover.is_none())
    });

    // Right half of B: "after B".
    let from = tb_bounds(visual, c).center();
    let b_bounds = tb_bounds(visual, b);
    tb_drag(visual, from, tb_right_of(b_bounds), true);
    assert_eq!(
        tb_order(&app, visual),
        vec![welcome, a, b, c],
        "dropped after B"
    );
    // The dragged tab is not activated by the drag itself.
    app.read_with(visual, |this, _| {
        assert_eq!(this.workspace.active_tab().map(|tab| tab.id), Some(c));
    });
}

#[gpui::test]
fn tabbar_escape_and_dropping_outside_cancel_a_tab_drag(cx: &mut TestAppContext) {
    let (app, visual) = tb_app(cx);
    let a = tb_open(&app, visual, "A", "", "");
    let b = tb_open(&app, visual, "B", "", "");
    let before = tb_order(&app, visual);

    // Escape mid-drag: gpui's drag ends, nothing moves, indicator goes away.
    let from = tb_bounds(visual, b).center();
    let a_bounds = tb_bounds(visual, a);
    tb_drag(visual, from, tb_left_of(a_bounds), false);
    assert!(visual.debug_bounds("tab-drop-indicator").is_some());
    visual.simulate_keystrokes("escape");
    tb_settle(visual);
    assert!(
        visual.debug_bounds("tab-drop-indicator").is_none(),
        "escape clears indicator"
    );
    visual.update(|_, cx| assert!(!cx.has_active_drag(), "drag cancelled"));
    visual.simulate_mouse_up(
        tb_left_of(a_bounds),
        gpui::MouseButton::Left,
        Default::default(),
    );
    tb_settle(visual);
    assert_eq!(tb_order(&app, visual), before);

    // Releasing over the results area (outside the bar) cancels too.
    let from = tb_bounds(visual, b).center();
    let outside = gpui::point(px(600.0), px(500.0));
    tb_drag(visual, from, outside, true);
    assert_eq!(tb_order(&app, visual), before);
    app.read_with(visual, |this, _| {
        assert!(this.shell.tab_bar.hover.is_none())
    });
    assert!(visual.debug_bounds("tab-drop-indicator").is_none());
}

fn tb_right_click(visual: &mut gpui::VisualTestContext, at: gpui::Point<gpui::Pixels>) {
    visual.simulate_mouse_down(at, gpui::MouseButton::Right, Default::default());
    visual.simulate_mouse_up(at, gpui::MouseButton::Right, Default::default());
    tb_settle(visual);
}

/// Selectable position of `label` (enabled items and submenu triggers count).
fn tb_clickable_position(entries: &[tab_bar::TabMenuEntry], label: &str) -> usize {
    let mut position = 0;
    for entry in entries {
        match entry {
            tab_bar::TabMenuEntry::Item {
                label: item_label,
                disabled,
                ..
            } => {
                if *item_label == label {
                    assert!(!disabled, "{label} is disabled");
                    return position;
                }
                if !disabled {
                    position += 1;
                }
            }
            tab_bar::TabMenuEntry::GroupBySubmenu => position += 1,
            tab_bar::TabMenuEntry::Separator => {}
        }
    }
    panic!("menu has no entry {label}");
}

fn tb_menu_entries(
    app: &gpui::Entity<AppRoot>,
    visual: &mut gpui::VisualTestContext,
    id: u64,
) -> Vec<tab_bar::TabMenuEntry> {
    app.read_with(visual, |this, _| {
        tab_bar::tab_menu_entries(&this.tab_menu_state(id).expect("tab exists"))
    })
}

fn tb_labels(entries: &[tab_bar::TabMenuEntry]) -> Vec<&'static str> {
    entries
        .iter()
        .filter_map(|entry| match entry {
            tab_bar::TabMenuEntry::Item { label, .. } => Some(*label),
            _ => None,
        })
        .collect()
}

/// Right-clicks the tab and chooses `label` through the real popup menu
/// (keyboard navigation + Enter, like the sidebar menu tests).
fn tb_pick_tab_menu(
    app: &gpui::Entity<AppRoot>,
    visual: &mut gpui::VisualTestContext,
    id: u64,
    label: &str,
) {
    let entries = tb_menu_entries(app, visual, id);
    let position = tb_clickable_position(&entries, label);
    let total = entries
        .iter()
        .filter(|entry| match entry {
            tab_bar::TabMenuEntry::Item { disabled, .. } => !disabled,
            tab_bar::TabMenuEntry::GroupBySubmenu => true,
            tab_bar::TabMenuEntry::Separator => false,
        })
        .count();
    let submenu_at = entries
        .iter()
        .filter(|entry| !matches!(entry, tab_bar::TabMenuEntry::Separator))
        .position(|entry| matches!(entry, tab_bar::TabMenuEntry::GroupBySubmenu));
    let center = tb_bounds(visual, id).center();
    tb_right_click(visual, center);
    // Selecting the "Group Tabs By" trigger opens its submenu and takes the
    // keyboard, so entries after it are reached from the bottom with Up.
    if submenu_at.is_some_and(|at| position > at) {
        for _ in 0..(total - position) {
            visual.simulate_keystrokes("up");
        }
    } else {
        for _ in 0..=position {
            visual.simulate_keystrokes("down");
        }
    }
    visual.simulate_keystrokes("enter");
    tb_settle(visual);
}

fn tb_prefs_file(
    app: &gpui::Entity<AppRoot>,
    visual: &mut gpui::VisualTestContext,
) -> std::path::PathBuf {
    let path = std::env::temp_dir().join(format!("tablex-tab-bar-{}.json", uuid::Uuid::new_v4()));
    app.update(visual, |this, _| this.sidebar.path = Some(path.clone()));
    path
}

fn tb_focus_root(app: &gpui::Entity<AppRoot>, visual: &mut gpui::VisualTestContext) {
    visual.update(|window, cx| {
        app.update(cx, |this, cx| this.sidebar.focus.focus(window, cx));
    });
    tb_settle(visual);
}

#[gpui::test]
fn tabbar_pin_and_unpin_through_the_context_menu(cx: &mut TestAppContext) {
    let (app, visual) = tb_app(cx);
    let welcome = tb_order(&app, visual)[0];
    let a = tb_open(&app, visual, "A", "", "");
    let b = tb_open(&app, visual, "B", "", "");
    assert!(visual
        .debug_bounds(&*format!("tab-close-{b}").leak())
        .is_some());
    let regular_width = tb_bounds(visual, b).size.width;

    tb_pick_tab_menu(&app, visual, b, "Pin Tab");
    assert_eq!(
        tb_order(&app, visual),
        vec![b, welcome, a],
        "pinned tabs sit at the start"
    );
    let pinned_width = tb_bounds(visual, b).size.width;
    assert!(
        pinned_width <= px(tab_bar::PINNED_TAB_WIDTH),
        "pinned tabs are compact"
    );
    assert!(pinned_width < regular_width);
    assert!(
        visual
            .debug_bounds(&*format!("tab-close-{b}").leak())
            .is_none(),
        "pinned tabs render no close button"
    );
    let labels = tb_labels(&tb_menu_entries(&app, visual, b));
    assert!(labels.contains(&"Unpin Tab") && !labels.contains(&"Pin Tab"));
    assert!(labels.contains(&"Close Pinned Tab"));

    tb_pick_tab_menu(&app, visual, b, "Unpin Tab");
    app.read_with(visual, |this, _| {
        assert!(!this.workspace.tab(b).unwrap().is_pinned);
    });
    assert_eq!(
        tb_bounds(visual, b).size.width,
        regular_width,
        "back to a regular tab"
    );
    assert!(visual
        .debug_bounds(&*format!("tab-close-{b}").leak())
        .is_some());
}

#[gpui::test]
fn tabbar_pinned_and_unpinned_tabs_only_reorder_within_their_region(cx: &mut TestAppContext) {
    let (app, visual) = tb_app(cx);
    let welcome = tb_order(&app, visual)[0];
    let a = tb_open(&app, visual, "A", "", "");
    let b = tb_open(&app, visual, "B", "", "");
    let c = tb_open(&app, visual, "C", "", "");
    app.update(visual, |this, cx| {
        this.workspace.set_pinned(a, true);
        this.workspace.set_pinned(b, true);
        cx.notify();
    });
    tb_settle(visual);
    assert_eq!(tb_order(&app, visual), vec![a, b, welcome, c]);

    // Unpinned C onto the pinned A: refused, no indicator, order intact.
    let from = tb_bounds(visual, c).center();
    let a_bounds = tb_bounds(visual, a);
    tb_drag(visual, from, tb_left_of(a_bounds), false);
    app.read_with(visual, |this, _| {
        let (_, plan) = this.shell.tab_bar.hover.clone().expect("hover recorded");
        assert_eq!(plan, crate::workspace::TabMovePlan::Refused);
    });
    assert!(
        visual.debug_bounds("tab-drop-indicator").is_none(),
        "no indicator when refused"
    );
    visual.simulate_mouse_up(
        tb_left_of(a_bounds),
        gpui::MouseButton::Left,
        Default::default(),
    );
    tb_settle(visual);
    assert_eq!(tb_order(&app, visual), vec![a, b, welcome, c]);

    // Pinned A onto the unpinned Welcome: refused as well.
    let from = tb_bounds(visual, a).center();
    let welcome_bounds = tb_bounds(visual, welcome);
    tb_drag(visual, from, tb_right_of(welcome_bounds), true);
    assert_eq!(tb_order(&app, visual), vec![a, b, welcome, c]);

    // Pinned reorder among pinned works; unpinned among unpinned works.
    let from = tb_bounds(visual, a).center();
    let b_bounds = tb_bounds(visual, b);
    tb_drag(visual, from, tb_right_of(b_bounds), true);
    assert_eq!(tb_order(&app, visual), vec![b, a, welcome, c]);
    let from = tb_bounds(visual, c).center();
    let welcome_bounds = tb_bounds(visual, welcome);
    tb_drag(visual, from, tb_left_of(welcome_bounds), true);
    assert_eq!(tb_order(&app, visual), vec![b, a, c, welcome]);
}

#[gpui::test]
fn tabbar_middle_click_closes_regular_tabs_but_never_pinned(cx: &mut TestAppContext) {
    let (app, visual) = tb_app(cx);
    let a = tb_open(&app, visual, "A", "", "");
    let b = tb_open(&app, visual, "B", "", "");
    app.update(visual, |this, cx| {
        this.workspace.set_pinned(a, true);
        cx.notify();
    });
    tb_settle(visual);
    let at = tb_bounds(visual, a).center();
    visual.simulate_mouse_down(at, gpui::MouseButton::Middle, Default::default());
    visual.simulate_mouse_up(at, gpui::MouseButton::Middle, Default::default());
    tb_settle(visual);
    assert!(
        tb_order(&app, visual).contains(&a),
        "middle-click ignores pinned tabs"
    );
    let at = tb_bounds(visual, b).center();
    visual.simulate_mouse_down(at, gpui::MouseButton::Middle, Default::default());
    visual.simulate_mouse_up(at, gpui::MouseButton::Middle, Default::default());
    tb_settle(visual);
    assert!(
        !tb_order(&app, visual).contains(&b),
        "middle-click closes a regular tab"
    );
}

#[gpui::test]
fn tabbar_close_variants_never_close_pinned_tabs_from_regular_tabs(cx: &mut TestAppContext) {
    let (app, visual) = tb_app(cx);
    let welcome = tb_order(&app, visual)[0];
    let a = tb_open(&app, visual, "A", "", "");
    let b = tb_open(&app, visual, "B", "", "");
    let c = tb_open(&app, visual, "C", "", "");
    let d = tb_open(&app, visual, "D", "", "");
    app.update(visual, |this, cx| {
        this.workspace.set_pinned(a, true);
        this.workspace.set_pinned(d, true);
        cx.notify();
    });
    tb_settle(visual);
    assert_eq!(tb_order(&app, visual), vec![a, d, welcome, b, c]);
    let labels = tb_labels(&tb_menu_entries(&app, visual, b));
    assert!(labels.contains(&"Close Other Regular Tabs"), "{labels:?}");
    assert!(labels.contains(&"Close All Regular Tabs"));
    let labels = tb_labels(&tb_menu_entries(&app, visual, a));
    assert!(labels.contains(&"Close Other Pinned Tabs"), "{labels:?}");
    assert!(labels.contains(&"Close All Pinned Tabs"));

    // Close Tabs to the Right from Welcome (regular region): only C... and B.
    app.update(visual, |this, cx| {
        let (_, window) = (0, 0);
        let _ = window;
        cx.notify();
        this.workspace.activate_tab(b);
    });
    tb_pick_tab_menu(&app, visual, welcome, "Close Tabs to the Right");
    assert_eq!(
        tb_order(&app, visual),
        vec![a, d, welcome],
        "pinned survive Close Right"
    );

    let e = tb_open(&app, visual, "E", "", "");
    let f = tb_open(&app, visual, "F", "", "");
    // Close Other Regular Tabs from E through the real menu.
    tb_pick_tab_menu(&app, visual, e, "Close Other Regular Tabs");
    assert_eq!(
        tb_order(&app, visual),
        vec![a, d, e],
        "pinned survive Close Others"
    );
    assert!(!tb_order(&app, visual).contains(&f));
    app.read_with(visual, |this, _| {
        assert_eq!(
            this.workspace.active_tab().map(|tab| tab.id),
            Some(e),
            "clicked tab is active"
        );
    });

    // Close Left from E must not touch the pinned region.
    app.update(visual, |this, cx| {
        assert!(this
            .workspace
            .close_targets(e, crate::workspace::TabCloseScope::Left)
            .is_empty());
        cx.notify();
    });
    let g = tb_open(&app, visual, "G", "", "");
    tb_pick_tab_menu(&app, visual, g, "Close All Regular Tabs");
    assert_eq!(
        tb_order(&app, visual),
        vec![a, d],
        "Close All Regular keeps pinned"
    );

    // From a pinned tab the commands act on the pinned region.
    tb_pick_tab_menu(&app, visual, a, "Close Other Pinned Tabs");
    assert_eq!(tb_order(&app, visual), vec![a]);
    tb_pick_tab_menu(&app, visual, a, "Close Pinned Tab");
    assert!(
        tb_order(&app, visual).is_empty(),
        "the menu can close a pinned tab"
    );
}

fn tb_stage_insert(app: &gpui::Entity<AppRoot>, visual: &mut gpui::VisualTestContext, tab: u64) {
    app.update(visual, |this, _| {
        this.workspace
            .runtime_mut(tab)
            .unwrap()
            .pending_sql_rows
            .push(crate::workspace::PendingSqlRow::Insert(crate::staged_sql::InsertRowRequest {
                database: "db".into(),
                schema: "main".into(),
                table: "orders".into(),
                values: vec![("name".into(), MutationValue::Text("new".into()))],
            }));
    });
}

fn tb_stage_delete(app: &gpui::Entity<AppRoot>, visual: &mut gpui::VisualTestContext, tab: u64) {
    // Same state the real "delete row" flow stages (mutations.rs).
    app.update(visual, |this, _| {
        let runtime = this.workspace.runtime_mut(tab).unwrap();
        runtime.pending_deleted_rows.insert(0);
        runtime
            .pending_sql_rows
            .push(crate::workspace::PendingSqlRow::Delete {
                row: 0,
                request: crate::staged_sql::DeleteRowRequest {
                    database: "db".into(),
                    schema: "main".into(),
                    table: "orders".into(),
                    key_column: "id".into(),
                    key_value: MutationValue::Text("1".into()),
                },
                key: Default::default(),
            });
    });
}

#[gpui::test]
fn close_others_confirms_staged_inserts_and_deletes_in_inactive_tabs(cx: &mut TestAppContext) {
    let (app, visual) = tb_app(cx);
    let keep = tb_open(&app, visual, "Keep", "", "");
    let inserted = tb_open(&app, visual, "Inserted", "", "");
    let deleted = tb_open(&app, visual, "Deleted", "", "");
    let clean = tb_open(&app, visual, "Clean", "", "");
    tb_stage_insert(&app, visual, inserted);
    tb_stage_delete(&app, visual, deleted);
    app.update(visual, |this, cx| {
        this.workspace.activate_tab(keep);
        cx.notify();
    });
    tb_settle(visual);
    let before = tb_order(&app, visual);

    // Close Others through the real menu: staged tabs raise ONE confirmation
    // and nothing closes until it is accepted.
    tb_pick_tab_menu(&app, visual, keep, "Close Other Tabs");
    assert!(
        visual.update(|window, cx| window.has_active_dialog(cx)),
        "staged SQL insert/delete in inactive tabs must be confirmed"
    );
    assert_eq!(tb_order(&app, visual), before, "nothing closed before confirming");

    // Cancel keeps every tab; the dialog is gone.
    visual.simulate_keystrokes("escape");
    tb_settle(visual);
    assert!(!visual.update(|window, cx| window.has_active_dialog(cx)));
    assert_eq!(tb_order(&app, visual), before);

    // A clean tab alone closes without any prompt.
    app.update(visual, |this, _| {
        let runtime = this.workspace.runtime_mut(inserted).unwrap();
        runtime.pending_sql_rows.clear();
        let runtime = this.workspace.runtime_mut(deleted).unwrap();
        runtime.pending_sql_rows.clear();
        runtime.pending_deleted_rows.clear();
    });
    tb_pick_tab_menu(&app, visual, keep, "Close Other Tabs");
    assert!(!visual.update(|window, cx| window.has_active_dialog(cx)));
    assert!(!tb_order(&app, visual).contains(&clean));
}

#[gpui::test]
fn single_tab_close_confirms_every_kind_of_staged_change(cx: &mut TestAppContext) {
    let (app, visual) = tb_app(cx);
    let insert = tb_open(&app, visual, "Insert", "", "");
    let delete = tb_open(&app, visual, "Delete", "", "");
    let applying = tb_open(&app, visual, "Applying", "", "");
    tb_stage_insert(&app, visual, insert);
    tb_stage_delete(&app, visual, delete);
    app.update(visual, |this, _| {
        this.workspace.runtime_mut(applying).unwrap().sql_apply_running = true;
    });
    for tab in [insert, delete, applying] {
        app.update(visual, |this, cx| {
            this.workspace.activate_tab(tab);
            cx.notify();
        });
        tb_settle(visual);
        app.update_in(visual, |this, window, cx| this.confirm_close_tab(tab, window, cx));
        tb_settle(visual);
        assert!(
            visual.update(|window, cx| window.has_active_dialog(cx)),
            "closing tab {tab} must ask before discarding staged work"
        );
        assert!(tb_order(&app, visual).contains(&tab), "still open before confirming");
        visual.simulate_keystrokes("escape");
        tb_settle(visual);
    }
    app.read_with(visual, |this, _| {
        assert!(!this.workspace.tab_has_staged_changes(u64::MAX));
    });
}

#[gpui::test]
fn tabbar_cmd_w_closes_the_active_tab_and_the_menu_shows_the_hint(cx: &mut TestAppContext) {
    let (app, visual) = tb_app(cx);
    let a = tb_open(&app, visual, "A", "", "");
    let b = tb_open(&app, visual, "B", "", "");
    tb_focus_root(&app, visual);
    visual.simulate_keystrokes("secondary-w");
    tb_settle(visual);
    assert!(
        !tb_order(&app, visual).contains(&b),
        "Cmd-W closed the active tab"
    );
    app.update(visual, |this, cx| {
        this.workspace.set_pinned(a, true);
        cx.notify();
    });
    tb_focus_root(&app, visual);
    visual.simulate_keystrokes("secondary-w");
    tb_settle(visual);
    assert!(
        !tb_order(&app, visual).contains(&a),
        "consistent with the menu: pinned may close"
    );
    visual.update(|window, cx| {
        let bound = window
            .bindings_for_action(&CloseActiveTab)
            .first()
            .map(|binding| binding.keystrokes().to_vec());
        assert!(
            bound.is_some(),
            "Cmd-W is registered so the menu can display it"
        );
        let _ = cx;
    });
}

#[gpui::test]
fn tabbar_group_by_setting_persists_through_the_bar_menu(cx: &mut TestAppContext) {
    let (app, visual) = tb_app(cx);
    let path = tb_prefs_file(&app, visual);
    let tail = visual
        .debug_bounds("workspace-tab-tail")
        .expect("empty tab-bar area is a hit target");
    // Bar menu: [label] None, Connection, Database, --, Shorten Tab Titles; the first down skips the label.
    let choose = |visual: &mut gpui::VisualTestContext, index: usize| {
        tb_right_click(visual, tail.center());
        for _ in 0..=index {
            visual.simulate_keystrokes("down");
        }
        visual.simulate_keystrokes("enter");
        tb_settle(visual);
    };
    choose(visual, 1);
    app.read_with(visual, |this, _| {
        assert_eq!(
            this.workspace.group_by(),
            crate::sidebar_preferences::TabGroupBy::Connection
        );
    });
    assert_eq!(
        crate::sidebar_preferences::SidebarPreferences::load(&path)
            .unwrap()
            .tab_group_by,
        crate::sidebar_preferences::TabGroupBy::Connection,
        "written to the sidebar preferences file"
    );
    choose(visual, 2);
    assert_eq!(
        crate::sidebar_preferences::SidebarPreferences::load(&path)
            .unwrap()
            .tab_group_by,
        crate::sidebar_preferences::TabGroupBy::Database
    );

    // Restart: a fresh workspace re-reads the preference.
    app.update(visual, |this, cx| {
        this.workspace
            .set_group_by(crate::sidebar_preferences::TabGroupBy::None);
        this.reload_sidebar_preferences(cx).unwrap();
        this.apply_tab_bar_preferences(cx);
        assert_eq!(
            this.workspace.group_by(),
            crate::sidebar_preferences::TabGroupBy::Database
        );
    });
    choose(visual, 0);
    app.read_with(visual, |this, _| {
        assert_eq!(
            this.workspace.group_by(),
            crate::sidebar_preferences::TabGroupBy::None
        );
    });
    std::fs::remove_file(path).unwrap();
}

fn tb_set_group_by(
    app: &gpui::Entity<AppRoot>,
    visual: &mut gpui::VisualTestContext,
    mode: crate::sidebar_preferences::TabGroupBy,
) {
    app.update(visual, |this, cx| this.set_tab_group_by(mode, cx));
    tb_settle(visual);
}

/// Regression (user report 2026-10-08): with every other group already
/// collapsed and no ungrouped tab, the chip of the group holding the active tab
/// did nothing — there was no visible tab to hand activation to. Collapsing it
/// must now take effect and move activation into the nearest other group.
#[gpui::test]
fn chip_collapses_the_active_group_when_every_other_group_is_collapsed(cx: &mut TestAppContext) {
    use crate::sidebar_preferences::TabGroupBy;
    let (app, visual) = tb_app(cx);
    let welcome = tb_order(&app, visual)[0];
    let pg = tb_open(
        &app,
        visual,
        "pg",
        crate::ui_fixture::POSTGRES_ID,
        "warehouse",
    );
    let mongo = tb_open(&app, visual, "mongo", crate::ui_fixture::MONGO_ID, "app");
    let sq1 = tb_open(&app, visual, "sq1", crate::ui_fixture::SQLITE_ID, "main");
    let sq2 = tb_open(&app, visual, "sq2", crate::ui_fixture::SQLITE_ID, "main");
    app.update(visual, |this, _| {
        this.workspace.close_tab(welcome);
    });
    tb_set_group_by(&app, visual, TabGroupBy::Connection);
    // Collapse the Postgres and Mongo groups while a SQLite tab is active.
    app.update(visual, |this, cx| {
        this.activate_tab_ui(sq1, cx);
        for id in [pg, mongo] {
            let key = this
                .workspace
                .group_key(this.workspace.tab(id).unwrap())
                .unwrap();
            assert!(this.workspace.toggle_group_collapsed(&key));
        }
    });
    tb_settle(visual);
    assert!(visual.debug_bounds(tb_sel(pg)).is_none());
    assert!(visual.debug_bounds(tb_sel(mongo)).is_none());
    assert!(visual.debug_bounds(tb_sel(sq1)).is_some());

    // The SQLite chip is the last one; clicking it must collapse its group.
    let chip = visual
        .debug_bounds("tab-group-chip-2")
        .expect("three group chips");
    visual.simulate_click(chip.center(), Default::default());
    tb_settle(visual);
    assert!(
        visual.debug_bounds(tb_sel(sq1)).is_none() && visual.debug_bounds(tb_sel(sq2)).is_none(),
        "the SQLite group collapsed"
    );
    app.read_with(visual, |this, _| {
        let active = this.workspace.active_tab().expect("an active tab remains");
        assert_eq!(
            active.id, mongo,
            "activation moved to the nearest tab outside the group"
        );
    });
    assert!(
        visual.debug_bounds(tb_sel(mongo)).is_some(),
        "the newly active tab's group expanded"
    );
}

#[gpui::test]
fn tabbar_groups_are_contiguous_with_chips_and_collapse(cx: &mut TestAppContext) {
    use crate::sidebar_preferences::TabGroupBy;
    let (app, visual) = tb_app(cx);
    let welcome = tb_order(&app, visual)[0];
    let pg1 = tb_open(
        &app,
        visual,
        "pg1",
        crate::ui_fixture::POSTGRES_ID,
        "warehouse",
    );
    let sq1 = tb_open(&app, visual, "sq1", crate::ui_fixture::SQLITE_ID, "main");
    let pg2 = tb_open(
        &app,
        visual,
        "pg2",
        crate::ui_fixture::POSTGRES_ID,
        "warehouse",
    );
    assert_eq!(tb_order(&app, visual), vec![welcome, pg1, sq1, pg2]);
    assert!(
        visual.debug_bounds("tab-group-chip-0").is_none(),
        "no chips without grouping"
    );

    app.update(visual, |this, _| this.sidebar.seed_scenario_colors());
    tb_set_group_by(&app, visual, TabGroupBy::Connection);
    assert_eq!(
        tb_order(&app, visual),
        vec![pg1, pg2, sq1, welcome],
        "contiguous, unbound last"
    );
    let chip0 = visual
        .debug_bounds("tab-group-chip-0")
        .expect("postgres chip");
    let chip1 = visual
        .debug_bounds("tab-group-chip-1")
        .expect("sqlite chip");
    assert!(visual.debug_bounds("tab-group-chip-2").is_none());
    assert!(
        chip0.origin.x < tb_bounds(visual, pg1).origin.x,
        "chip leads its group"
    );
    assert!(tb_bounds(visual, pg2).origin.x < chip1.origin.x);
    app.read_with(visual, |this, _| {
        let layout = this.workspace.layout();
        let crate::workspace::TabBarItem::Group { key, count, .. } = &layout[0] else {
            panic!("first item is a chip: {layout:?}");
        };
        assert_eq!(*count, 2);
        assert_eq!(this.group_label(key), "Postgres fixture");
        let expected = this
            .sidebar
            .color(crate::ui_fixture::POSTGRES_ID)
            .and_then(super::navigator_ui::parse_hex_color)
            .expect("fixture connection has a color");
        assert_eq!(
            this.group_color(key),
            expected,
            "chip uses the connection color"
        );
        assert_eq!(expected, gpui::rgb(0x336791));
    });

    // Clicking the chip collapses the group: tabs hidden, chip remains.
    visual.simulate_click(chip0.center(), Default::default());
    tb_settle(visual);
    assert!(
        visual.debug_bounds(tb_sel(pg1)).is_none() && visual.debug_bounds(tb_sel(pg2)).is_none()
    );
    assert!(
        visual.debug_bounds("tab-group-chip-0").is_some(),
        "collapsed group keeps its chip"
    );
    assert!(
        visual.debug_bounds(tb_sel(sq1)).is_some(),
        "other groups unaffected"
    );
    app.read_with(visual, |this, _| {
        assert!(matches!(
            this.workspace.layout()[0],
            crate::workspace::TabBarItem::Group {
                count: 2,
                collapsed: true,
                ..
            }
        ));
    });

    // Activating a hidden tab by another route expands its group again.
    app.update(visual, |this, cx| this.activate_tab_from_picker(pg2, cx));
    tb_settle(visual);
    assert!(visual.debug_bounds(tb_sel(pg1)).is_some(), "group expanded");
    assert_eq!(
        app.read_with(visual, |this, _| this
            .workspace
            .active_tab()
            .map(|tab| tab.id)),
        Some(pg2)
    );

    // Chip click collapses and expands; collapsing hands activation to a visible tab.
    let chip0 = visual.debug_bounds("tab-group-chip-0").unwrap();
    visual.simulate_click(chip0.center(), Default::default());
    tb_settle(visual);
    assert!(visual.debug_bounds(tb_sel(pg1)).is_none());
    app.read_with(visual, |this, _| {
        let active = this.workspace.active_tab().unwrap();
        assert_ne!(
            active.connection_id,
            crate::ui_fixture::POSTGRES_ID,
            "active tab left the group"
        );
    });
    let chip0 = visual.debug_bounds("tab-group-chip-0").unwrap();
    visual.simulate_click(chip0.center(), Default::default());
    tb_settle(visual);
    assert!(
        visual.debug_bounds(tb_sel(pg1)).is_some(),
        "second click expands"
    );

    // Database mode splits per database.
    let pg3 = tb_open(
        &app,
        visual,
        "pg3",
        crate::ui_fixture::POSTGRES_ID,
        "reporting",
    );
    tb_set_group_by(&app, visual, TabGroupBy::Database);
    assert!(
        visual.debug_bounds("tab-group-chip-2").is_some(),
        "three (connection, db) groups"
    );
    app.read_with(visual, |this, _| {
        let key = this
            .workspace
            .layout()
            .iter()
            .find_map(|item| match item {
                crate::workspace::TabBarItem::Group { key, .. } if key.database == "reporting" => {
                    Some(key.clone())
                }
                _ => None,
            })
            .expect("reporting group");
        assert_eq!(this.group_label(&key), "Postgres fixture · reporting");
        let _ = pg3;
    });
}

#[gpui::test]
fn tabbar_dragging_into_groups_accepts_matching_keys_and_refuses_others(cx: &mut TestAppContext) {
    use crate::sidebar_preferences::TabGroupBy;
    let (app, visual) = tb_app(cx);
    let welcome = tb_order(&app, visual)[0];
    let pg1 = tb_open(
        &app,
        visual,
        "pg1",
        crate::ui_fixture::POSTGRES_ID,
        "warehouse",
    );
    let pg2 = tb_open(
        &app,
        visual,
        "pg2",
        crate::ui_fixture::POSTGRES_ID,
        "warehouse",
    );
    let sq1 = tb_open(&app, visual, "sq1", crate::ui_fixture::SQLITE_ID, "main");
    tb_set_group_by(&app, visual, TabGroupBy::Connection);
    assert_eq!(tb_order(&app, visual), vec![pg1, pg2, sq1, welcome]);

    // SQLite tab onto a postgres tab: refused, nothing moves.
    let from = tb_bounds(visual, sq1).center();
    let pg1_bounds = tb_bounds(visual, pg1);
    tb_drag(visual, from, tb_left_of(pg1_bounds), false);
    app.read_with(visual, |this, _| {
        let (_, plan) = this.shell.tab_bar.hover.clone().unwrap();
        assert_eq!(plan, crate::workspace::TabMovePlan::Refused);
    });
    assert!(visual.debug_bounds("tab-drop-indicator").is_none());
    visual.simulate_mouse_up(
        tb_left_of(pg1_bounds),
        gpui::MouseButton::Left,
        Default::default(),
    );
    tb_settle(visual);
    assert_eq!(tb_order(&app, visual), vec![pg1, pg2, sq1, welcome]);

    // Welcome (no connection) onto the postgres chip: refused.
    let from = tb_bounds(visual, welcome).center();
    let chip0 = visual.debug_bounds("tab-group-chip-0").unwrap();
    tb_drag(visual, from, chip0.center(), true);
    assert_eq!(tb_order(&app, visual), vec![pg1, pg2, sq1, welcome]);

    // A postgres tab onto its own group's chip is accepted (first in group).
    let from = tb_bounds(visual, pg2).center();
    let chip0 = visual.debug_bounds("tab-group-chip-0").unwrap();
    tb_drag(visual, from, chip0.center(), false);
    assert!(
        visual.debug_bounds("tab-drop-indicator").is_some(),
        "chip shows a drop indicator"
    );
    visual.simulate_mouse_up(chip0.center(), gpui::MouseButton::Left, Default::default());
    tb_settle(visual);
    assert_eq!(tb_order(&app, visual), vec![pg2, pg1, sq1, welcome]);

    // Between tabs of the same group is accepted too.
    let from = tb_bounds(visual, pg2).center();
    let pg1_bounds = tb_bounds(visual, pg1);
    tb_drag(visual, from, tb_right_of(pg1_bounds), true);
    assert_eq!(tb_order(&app, visual), vec![pg1, pg2, sq1, welcome]);
}

fn tb_open_typed(
    app: &gpui::Entity<AppRoot>,
    visual: &mut gpui::VisualTestContext,
    title: &str,
    kind: &str,
) -> u64 {
    let (title, kind) = (title.to_string(), kind.to_string());
    let id = app.update(visual, |this, cx| {
        let id = match kind.as_str() {
            "sqlTable" => this.workspace.open_query_tab_with_metadata(
                title,
                TabMetadata {
                    connection_id: crate::ui_fixture::POSTGRES_ID.into(),
                    database: "warehouse".into(),
                    schema: Some("public".into()),
                    collection: "orders".into(),
                    query_mode: Some(QueryMode::SqlTable),
                    ..Default::default()
                },
            ),
            "mongoFind" => {
                this.workspace
                    .open_or_activate_collection_tab(
                        crate::ui_fixture::MONGO_ID,
                        "shop",
                        "invoices",
                    )
                    .0
            }
            "driver" => this.workspace.open_driver_manager_tab(),
            "connection" => this
                .workspace
                .open_or_activate_connection_tab(crate::ui_fixture::POSTGRES_ID, title),
            _ => this.workspace.open_query_tab_with_metadata(
                title,
                TabMetadata {
                    connection_id: crate::ui_fixture::POSTGRES_ID.into(),
                    database: "warehouse".into(),
                    ..Default::default()
                },
            ),
        };
        cx.notify();
        id
    });
    tb_settle(visual);
    id
}

#[gpui::test]
fn tabbar_context_menu_entries_follow_tab_kind_and_pin_state(cx: &mut TestAppContext) {
    use crate::sidebar_preferences::TabGroupBy;
    let (app, visual) = tb_app(cx);
    let welcome = tb_order(&app, visual)[0];
    let query = tb_open_typed(&app, visual, "Report", "query");
    let table = tb_open_typed(&app, visual, "orders", "sqlTable");
    let mongo = tb_open_typed(&app, visual, "invoices", "mongoFind");
    let driver = tb_open_typed(&app, visual, "Drivers", "driver");
    let connection = tb_open_typed(&app, visual, "Postgres", "connection");

    let has = |labels: &[&'static str], label: &str| labels.contains(&label);
    let welcome_labels = tb_labels(&tb_menu_entries(&app, visual, welcome));
    assert_eq!(
        welcome_labels[0], "Shorten Tab Titles",
        "title toggle leads the menu"
    );
    for label in [
        "Copy Name",
        "Pin Tab",
        "Close Tabs to the Right",
        "Close All Tabs",
        "Close Tab",
    ] {
        assert!(has(&welcome_labels, label), "{label} in {welcome_labels:?}");
    }
    for label in [
        "Rename Tab",
        "Duplicate Tab",
        "Locate in Sidebar",
        "Close Tab Group",
        "Close Pinned Tab",
    ] {
        assert!(!has(&welcome_labels, label), "{label} hidden for Welcome");
    }
    // Split Right / Down need a second tab in the pane; the pane commands
    // need two panes.
    for label in ["Split Right", "Split Down"] {
        assert!(has(&welcome_labels, label), "{label} offered with several tabs");
    }
    for label in ["Move to Other Pane", "Close Pane"] {
        assert!(!has(&welcome_labels, label), "{label} hidden with one pane");
    }

    let query_labels = tb_labels(&tb_menu_entries(&app, visual, query));
    for label in [
        "Rename Tab",
        "Duplicate Tab",
        "Copy Name",
        "Locate in Sidebar",
    ] {
        assert!(has(&query_labels, label), "{label} for query tab");
    }
    let table_labels = tb_labels(&tb_menu_entries(&app, visual, table));
    assert!(has(&table_labels, "Duplicate Tab") && has(&table_labels, "Locate in Sidebar"));
    assert!(
        !has(&table_labels, "Rename Tab"),
        "table tabs are named after their object"
    );
    let mongo_labels = tb_labels(&tb_menu_entries(&app, visual, mongo));
    assert!(has(&mongo_labels, "Duplicate Tab") && !has(&mongo_labels, "Rename Tab"));
    let driver_labels = tb_labels(&tb_menu_entries(&app, visual, driver));
    for label in ["Rename Tab", "Duplicate Tab", "Locate in Sidebar"] {
        assert!(
            !has(&driver_labels, label),
            "{label} hidden for Driver Manager"
        );
    }
    let connection_labels = tb_labels(&tb_menu_entries(&app, visual, connection));
    assert!(has(&connection_labels, "Locate in Sidebar"));
    assert!(!has(&connection_labels, "Duplicate Tab") && !has(&connection_labels, "Rename Tab"));

    // Order: title toggle .. Copy Name .. Pin .. group-by .. close entries .. Close Tab.
    let entries = tb_menu_entries(&app, visual, query);
    let mut kinds = entries.iter().map(|entry| match entry {
        tab_bar::TabMenuEntry::Separator => "sep",
        tab_bar::TabMenuEntry::GroupBySubmenu => "group",
        tab_bar::TabMenuEntry::Item { .. } => "item",
    });
    // title toggle | pin + group-by | split | close entries (the split section
    // is there because the pane holds several tabs).
    assert_eq!(kinds.clone().filter(|kind| *kind == "sep").count(), 3);
    assert_eq!(kinds.next(), Some("item"));
    assert_eq!(tb_labels(&entries).last(), Some(&"Close Tab"));

    // "Close Tab Group" exists only while grouping is on.
    assert!(!has(&query_labels, "Close Tab Group"));
    tb_set_group_by(&app, visual, TabGroupBy::Connection);
    let grouped = tb_labels(&tb_menu_entries(&app, visual, query));
    assert!(has(&grouped, "Close Tab Group"));
    assert!(!has(
        &tb_labels(&tb_menu_entries(&app, visual, welcome)),
        "Close Tab Group"
    ));

    // Pinned tab labels, and "Others" disabled when nothing else can close.
    app.update(visual, |this, cx| {
        this.workspace.set_pinned(query, true);
        cx.notify();
    });
    let pinned = tb_labels(&tb_menu_entries(&app, visual, query));
    for label in [
        "Unpin Tab",
        "Close Pinned Tab",
        "Close Other Pinned Tabs",
        "Close All Pinned Tabs",
    ] {
        assert!(has(&pinned, label), "{label} in {pinned:?}");
    }
    assert!(
        !has(&pinned, "Close Tab Group"),
        "pinned tabs are never grouped"
    );
    let others_disabled = |entries: &[tab_bar::TabMenuEntry]| {
        entries.iter().any(|entry| {
            matches!(entry, tab_bar::TabMenuEntry::Item { label, disabled: true, .. }
                if label.starts_with("Close Other"))
        })
    };
    assert!(
        others_disabled(&tb_menu_entries(&app, visual, query)),
        "single pinned tab"
    );
    assert!(!others_disabled(&tb_menu_entries(&app, visual, table)));
}

#[gpui::test]
fn tabbar_rename_commits_with_enter_and_cancels_with_escape(cx: &mut TestAppContext) {
    let (app, visual) = tb_app(cx);
    let query = tb_open_typed(&app, visual, "Report", "query");

    tb_pick_tab_menu(&app, visual, query, "Rename Tab");
    app.read_with(visual, |this, _| {
        assert_eq!(
            this.shell
                .tab_bar
                .rename
                .as_ref()
                .map(|rename| rename.tab_id),
            Some(query)
        );
    });
    visual.simulate_keystrokes("secondary-a");
    visual.simulate_input("Orders");
    visual.simulate_keystrokes("enter");
    tb_settle(visual);
    app.read_with(visual, |this, _| {
        assert_eq!(this.workspace.tab(query).unwrap().title, "Orders");
        assert!(this.shell.tab_bar.rename.is_none(), "rename ended");
    });

    tb_pick_tab_menu(&app, visual, query, "Rename Tab");
    visual.simulate_keystrokes("secondary-a");
    visual.simulate_input("Discarded");
    visual.simulate_keystrokes("escape");
    tb_settle(visual);
    app.read_with(visual, |this, _| {
        assert_eq!(
            this.workspace.tab(query).unwrap().title,
            "Orders",
            "Escape keeps the old title"
        );
        assert!(this.shell.tab_bar.rename.is_none());
    });

    // Blank names are rejected and the old title stays.
    tb_pick_tab_menu(&app, visual, query, "Rename Tab");
    visual.simulate_keystrokes("secondary-a backspace enter");
    tb_settle(visual);
    app.read_with(visual, |this, _| {
        assert_eq!(this.workspace.tab(query).unwrap().title, "Orders");
    });
}

#[gpui::test]
fn tabbar_duplicate_copy_name_and_locate_through_the_menu(cx: &mut TestAppContext) {
    let (app, visual) = tb_app(cx);
    let table = tb_open_typed(&app, visual, "orders", "sqlTable");
    let query = tb_open_typed(&app, visual, "Report", "query");
    app.update(visual, |this, _| {
        this.workspace.tab_mut(query).unwrap().sql = "SELECT 1".into();
    });

    // Duplicate: same connection/text, placed right after the source and active.
    tb_pick_tab_menu(&app, visual, query, "Duplicate Tab");
    app.read_with(visual, |this, _| {
        let order: Vec<u64> = this.workspace.tabs().iter().map(|tab| tab.id).collect();
        let source = order.iter().position(|id| *id == query).unwrap();
        let copy = this.workspace.tabs()[source + 1].clone();
        assert_eq!(copy.sql, "SELECT 1");
        assert_eq!(copy.connection_id, crate::ui_fixture::POSTGRES_ID);
        assert_eq!(copy.title, "Report");
        assert_eq!(this.workspace.active_tab().map(|tab| tab.id), Some(copy.id));
    });

    // Copy Name copies the displayed title and confirms with a notification.
    tb_pick_tab_menu(&app, visual, table, "Copy Name");
    let copied = visual
        .update(|_, cx| cx.read_from_clipboard())
        .and_then(|item| item.text());
    assert_eq!(
        copied.as_deref(),
        Some("orders@warehouse.public"),
        "full title includes @database.schema"
    );
    let notes = visual.update(|window, cx| {
        use gpui_component::WindowExt as _;
        window.notifications(cx).len()
    });
    assert_eq!(notes, 1, "a Copied notification is shown");

    // Locate selects the tab's object in the sidebar (activating the tab first).
    let table_meta = app.read_with(visual, |this, _| {
        this.workspace.tab(table).cloned().unwrap()
    });
    tb_pick_tab_menu(&app, visual, table, "Locate in Sidebar");
    visual.run_until_parked();
    app.read_with(visual, |this, _| {
        assert_eq!(this.workspace.active_tab().map(|tab| tab.id), Some(table));
        assert!(
            matches!(this.navigator.selected(), Some(NavigatorNodeId::Object { database, schema, object, .. })
                if *database == table_meta.database
                    && Some(schema.as_str()) == table_meta.schema.as_deref()
                    && *object == table_meta.collection),
            "sidebar selection: {:?} / {:?}",
            this.navigator.selected(),
            this.sidebar.locate_error
        );
    });
}

#[gpui::test]
fn tabbar_shorten_tab_titles_toggles_labels_and_persists(cx: &mut TestAppContext) {
    let (app, visual) = tb_app(cx);
    let path = tb_prefs_file(&app, visual);
    let table = tb_open_typed(&app, visual, "orders", "sqlTable");
    let full_width = tb_bounds(visual, table).size.width;
    app.read_with(visual, |this, _| {
        let tab = this.workspace.tab(table).unwrap();
        assert_eq!(tab_bar::tab_label(tab, false), "orders@warehouse.public");
        assert_eq!(tab_bar::tab_label(tab, true), "orders");
    });

    tb_pick_tab_menu(&app, visual, table, "Shorten Tab Titles");
    app.read_with(visual, |this, _| assert!(this.shell.tab_bar.compact_titles));
    assert!(
        tb_bounds(visual, table).size.width < full_width,
        "shortened tabs are narrower"
    );
    assert!(
        crate::sidebar_preferences::SidebarPreferences::load(&path)
            .unwrap()
            .compact_tab_titles,
        "persisted"
    );
    let labels = tb_labels(&tb_menu_entries(&app, visual, table));
    assert_eq!(labels[0], "Show Full Tab Titles");

    // Restart re-reads the preference.
    app.update(visual, |this, cx| {
        this.shell.tab_bar.compact_titles = false;
        this.reload_sidebar_preferences(cx).unwrap();
        this.apply_tab_bar_preferences(cx);
        assert!(this.shell.tab_bar.compact_titles);
    });
    tb_pick_tab_menu(&app, visual, table, "Show Full Tab Titles");
    app.read_with(visual, |this, _| {
        assert!(!this.shell.tab_bar.compact_titles)
    });
    assert!(
        !crate::sidebar_preferences::SidebarPreferences::load(&path)
            .unwrap()
            .compact_tab_titles
    );
    std::fs::remove_file(path).unwrap();
}

#[gpui::test]
fn tabbar_pins_survive_a_session_restore_round_trip(cx: &mut TestAppContext) {
    // Pinned state is read from the persisted session metadata (the shell
    // never writes the Swift session back); restore places pinned tabs first.
    let (app, visual) = tb_app(cx);
    app.update(visual, |this, cx| {
        let connections = this.saved_connections.clone();
        let restored = vec![
            crate::session::RestoredTab {
                source_id: "s1".into(),
                active: false,
                kind: "queryDraft".into(),
                connection_id: crate::ui_fixture::POSTGRES_ID.into(),
                database: "warehouse".into(),
                schema: None,
                collection: String::new(),
                query_mode: None,
                title: "Plain".into(),
                sql: "SELECT 1".into(),
                output_limit: 200,
                is_pinned: false,
                where_clause: String::new(),
                order_by: String::new(),
            },
            crate::session::RestoredTab {
                source_id: "s2".into(),
                active: true,
                kind: "queryDraft".into(),
                connection_id: crate::ui_fixture::POSTGRES_ID.into(),
                database: "warehouse".into(),
                schema: None,
                collection: String::new(),
                query_mode: None,
                title: "Pinned".into(),
                sql: "SELECT 2".into(),
                output_limit: 200,
                is_pinned: true,
                where_clause: String::new(),
                order_by: String::new(),
            },
        ];
        workspace::restore_tabs_into(&mut this.workspace, &connections, restored);
        cx.notify();
    });
    tb_settle(visual);
    app.read_with(visual, |this, _| {
        let titles: Vec<_> = this
            .workspace
            .tabs()
            .iter()
            .map(|tab| tab.title.as_str())
            .collect();
        assert_eq!(
            titles,
            vec!["Pinned", "Welcome", "Plain"],
            "restored pins lead the bar"
        );
        assert!(this.workspace.tabs()[0].is_pinned);
    });
}

fn tb_open_many(
    app: &gpui::Entity<AppRoot>,
    visual: &mut gpui::VisualTestContext,
    count: usize,
) -> Vec<u64> {
    (1..=count)
        .map(|n| tb_open(app, visual, &format!("Tab {n}"), "", ""))
        .collect()
}

fn tb_open_picker(visual: &mut gpui::VisualTestContext) {
    let button = visual
        .debug_bounds("open-tabs")
        .expect("open-tabs button is shown");
    visual.simulate_click(button.center(), Default::default());
    tb_settle(visual);
    let panel = visual
        .debug_bounds("open-tabs-panel")
        .expect("picker opens");
    // Popover placement: below the button, right-aligned (end), inside the window.
    assert!(panel.origin.y >= button.origin.y + button.size.height - px(1.0));
    let button_right = button.origin.x + button.size.width;
    assert!(panel.origin.x + panel.size.width <= button_right + px(1.0));
    assert!(panel.origin.x >= px(0.0));
}

fn tb_picker_rows(app: &gpui::Entity<AppRoot>, visual: &mut gpui::VisualTestContext) -> Vec<u64> {
    app.read_with(visual, |this, _| this.picker_filtered())
}

#[gpui::test]
fn tabbar_open_tabs_button_appears_when_tabs_overflow(cx: &mut TestAppContext) {
    let (app, visual) = tb_app(cx);
    tb_open_many(&app, visual, 3);
    assert!(
        visual.debug_bounds("open-tabs").is_none(),
        "few tabs: no overflow control"
    );
    tb_open_many(&app, visual, 6);
    tb_settle(visual);
    let button = visual
        .debug_bounds("open-tabs")
        .expect("shown with many tabs");
    let tabs = visual.debug_bounds("workspace-tabs").unwrap();
    assert!(
        button.origin.x >= tabs.origin.x + tabs.size.width - px(1.0),
        "right end of the bar"
    );
}

#[gpui::test]
fn tabbar_open_tabs_picker_filters_and_selects_with_the_keyboard(cx: &mut TestAppContext) {
    let (app, visual) = tb_app(cx);
    let ids = tb_open_many(&app, visual, 9);
    tb_open_picker(visual);
    // All tabs are listed (Welcome + 9); the active tab starts highlighted.
    assert_eq!(tb_picker_rows(&app, visual).len(), 10);
    for id in [ids[0], ids[8]] {
        assert!(
            visual
                .debug_bounds(&*format!("open-tab-row-{id}").leak())
                .is_some(),
            "row for tab {id}"
        );
    }
    let focused = visual.update(|window, cx| {
        app.read(cx)
            .shell
            .tab_bar
            .picker
            .input_focus_handle(cx)
            .is_some_and(|handle| handle.is_focused(window))
    });
    assert!(focused, "search input is focused when the picker opens");

    // Case-insensitive filter on the title.
    visual.simulate_input("tAB 3");
    tb_settle(visual);
    assert_eq!(tb_picker_rows(&app, visual), vec![ids[2]]);
    assert!(visual
        .debug_bounds(&*format!("open-tab-row-{}", ids[0]).leak())
        .is_none());

    // No match shows the empty state.
    visual.simulate_input("zzz");
    tb_settle(visual);
    assert!(tb_picker_rows(&app, visual).is_empty());
    assert!(
        visual.debug_bounds("open-tabs-empty").is_some(),
        "No matching tabs"
    );

    // Clear, then Up/Down move the highlight and Enter activates.
    visual.simulate_keystrokes("secondary-a backspace");
    visual.simulate_input("Tab");
    tb_settle(visual);
    assert_eq!(tb_picker_rows(&app, visual).len(), 9);
    app.read_with(visual, |this, _| {
        assert_eq!(this.shell.tab_bar.picker.selected, 0)
    });
    visual.simulate_keystrokes("down down down");
    app.read_with(visual, |this, _| {
        assert_eq!(this.shell.tab_bar.picker.selected, 3)
    });
    visual.simulate_keystrokes("up");
    app.read_with(visual, |this, _| {
        assert_eq!(this.shell.tab_bar.picker.selected, 2)
    });
    visual.simulate_keystrokes("enter");
    tb_settle(visual);
    app.read_with(visual, |this, _| {
        assert_eq!(
            this.workspace.active_tab().map(|tab| tab.id),
            Some(ids[2]),
            "Enter activates the highlighted tab"
        );
        assert!(
            !this.shell.tab_bar.picker.open,
            "picker closes after selecting"
        );
    });
    assert!(visual.debug_bounds("open-tabs-panel").is_none());
}

#[gpui::test]
fn tabbar_open_tabs_picker_matches_connection_and_database_names(cx: &mut TestAppContext) {
    let (app, visual) = tb_app(cx);
    tb_open_many(&app, visual, 8);
    let pg = tb_open(
        &app,
        visual,
        "Sales",
        crate::ui_fixture::POSTGRES_ID,
        "warehouse",
    );
    tb_open_picker(visual);
    visual.simulate_input("postgres fix");
    tb_settle(visual);
    assert_eq!(
        tb_picker_rows(&app, visual),
        vec![pg],
        "matches the connection name"
    );
    visual.simulate_keystrokes("secondary-a");
    visual.simulate_input("WAREHOUSE");
    tb_settle(visual);
    assert_eq!(
        tb_picker_rows(&app, visual),
        vec![pg],
        "matches the database name"
    );
}

#[gpui::test]
fn tabbar_open_tabs_picker_click_activates_collapsed_group_tab_and_reveals_it(
    cx: &mut TestAppContext,
) {
    use crate::sidebar_preferences::TabGroupBy;
    let (app, visual) = tb_app(cx);
    tb_open_many(&app, visual, 8);
    let pg1 = tb_open(
        &app,
        visual,
        "pg1",
        crate::ui_fixture::POSTGRES_ID,
        "warehouse",
    );
    let _pg2 = tb_open(
        &app,
        visual,
        "pg2",
        crate::ui_fixture::POSTGRES_ID,
        "warehouse",
    );
    tb_set_group_by(&app, visual, TabGroupBy::Connection);
    // The strip is scrolled to the active tab; bring the group's chip into view.
    app.update(visual, |this, _| {
        let pane = this.workspace.focused_pane();
        this.shell
            .tab_bar
            .strip(pane)
            .scroll
            .set_offset(gpui::point(px(0.0), px(0.0)));
    });
    tb_settle(visual);
    let chip = visual.debug_bounds("tab-group-chip-0").unwrap();
    visual.simulate_click(chip.center(), Default::default());
    tb_settle(visual);
    assert!(
        visual.debug_bounds(tb_sel(pg1)).is_none(),
        "group collapsed, tab hidden"
    );

    tb_open_picker(visual);
    let row = visual
        .debug_bounds(&*format!("open-tab-row-{pg1}").leak())
        .expect("hidden tabs are still listed");
    visual.simulate_click(row.center(), Default::default());
    tb_settle(visual);
    app.read_with(visual, |this, _| {
        assert_eq!(this.workspace.active_tab().map(|tab| tab.id), Some(pg1));
        assert!(!this.shell.tab_bar.picker.open);
    });
    let tab = visual
        .debug_bounds(tb_sel(pg1))
        .expect("its group expanded and the tab is drawn");
    let strip = visual.debug_bounds("workspace-tabs").unwrap();
    assert!(
        tab.origin.x >= strip.origin.x
            && tab.origin.x + tab.size.width <= strip.origin.x + strip.size.width,
        "the activated tab is scrolled into view: {tab:?} in {strip:?}"
    );
}

#[gpui::test]
fn tabbar_open_tabs_picker_closes_tabs_and_hides_close_for_pinned(cx: &mut TestAppContext) {
    let (app, visual) = tb_app(cx);
    let ids = tb_open_many(&app, visual, 9);
    app.update(visual, |this, cx| {
        this.workspace.set_pinned(ids[0], true);
        cx.notify();
    });
    tb_settle(visual);
    tb_open_picker(visual);
    assert!(
        visual
            .debug_bounds(&*format!("open-tab-close-{}", ids[0]).leak())
            .is_none(),
        "pinned row has no close button"
    );
    let close = visual
        .debug_bounds(&*format!("open-tab-close-{}", ids[3]).leak())
        .expect("regular row has a close button");
    visual.simulate_click(close.center(), Default::default());
    tb_settle(visual);
    let order = tb_order(&app, visual);
    assert!(!order.contains(&ids[3]), "closed from the list");
    app.read_with(visual, |this, _| {
        assert!(
            this.shell.tab_bar.picker.open,
            "the list stays open after closing a tab"
        );
    });
    assert!(
        visual
            .debug_bounds(&*format!("open-tab-row-{}", ids[3]).leak())
            .is_none(),
        "row removed"
    );
}

#[gpui::test]
fn tabbar_open_tabs_picker_dismisses_on_escape_and_outside_click(cx: &mut TestAppContext) {
    let (app, visual) = tb_app(cx);
    tb_open_many(&app, visual, 9);
    tb_open_picker(visual);
    visual.simulate_keystrokes("escape");
    tb_settle(visual);
    assert!(
        visual.debug_bounds("open-tabs-panel").is_none(),
        "Escape closes"
    );
    app.read_with(visual, |this, _| assert!(!this.shell.tab_bar.picker.open));

    tb_open_picker(visual);
    let before = tb_order(&app, visual);
    // Click on the results area: dismisses without click-through side effects.
    visual.simulate_click(gpui::point(px(500.0), px(600.0)), Default::default());
    tb_settle(visual);
    assert!(
        visual.debug_bounds("open-tabs-panel").is_none(),
        "outside click closes"
    );
    app.read_with(visual, |this, _| assert!(!this.shell.tab_bar.picker.open));
    assert_eq!(tb_order(&app, visual), before);

    // Reopening starts with a cleared search.
    tb_open_picker(visual);
    assert_eq!(tb_picker_rows(&app, visual).len(), before.len());
}

#[gpui::test]
fn tabbar_open_tabs_picker_row_right_click_opens_the_tab_menu(cx: &mut TestAppContext) {
    let (app, visual) = tb_app(cx);
    let ids = tb_open_many(&app, visual, 9);
    tb_open_picker(visual);
    let row = visual
        .debug_bounds(&*format!("open-tab-row-{}", ids[4]).leak())
        .unwrap();
    tb_right_click(visual, row.center());
    // "Pin Tab" is the 5th selectable entry (Shorten, Rename, Duplicate, Copy Name, Pin).
    for _ in 0..5 {
        visual.simulate_keystrokes("down");
    }
    visual.simulate_keystrokes("enter");
    tb_settle(visual);
    app.read_with(visual, |this, _| {
        assert!(
            this.workspace.tab(ids[4]).unwrap().is_pinned,
            "row menu pinned the tab"
        );
    });
}

/// A real mouse double-click: two down/up pairs carrying click counts 1 and 2.
fn tb_double_click(visual: &mut gpui::VisualTestContext, at: gpui::Point<gpui::Pixels>) {
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
}

fn tb_row_bounds(
    app: &gpui::Entity<AppRoot>,
    visual: &mut gpui::VisualTestContext,
    label: &str,
) -> gpui::Bounds<gpui::Pixels> {
    let index = app
        .read_with(visual, |this, _| {
            this.navigator
                .visible_rows()
                .iter()
                .position(|row| row.label == label)
        })
        .unwrap_or_else(|| panic!("row {label} is visible"));
    let selector: &'static str = Box::leak(format!("sidebar-row-{index}").into_boxed_str());
    visual
        .debug_bounds(selector)
        .unwrap_or_else(|| panic!("row {label} bounds"))
}

#[gpui::test]
fn sidebar_double_click_opens_table_and_collection_data_tabs(cx: &mut TestAppContext) {
    let (app, visual) = tb_app(cx);
    app.update(visual, |this, _| this.seed_connection_tree_scenario());
    tb_settle(visual);
    let before = tb_order(&app, visual).len();

    // Table row (Postgres `orders`): a real double-click opens a table-data tab.
    let orders = tb_row_bounds(&app, visual, "orders");
    tb_double_click(visual, orders.center());
    app.read_with(visual, |this, _| {
        let tab = this.workspace.active_tab().expect("tab opened");
        assert_eq!(tab.collection, "orders");
        assert_eq!(tab.query_mode, Some(QueryMode::SqlTable));
        assert_eq!(this.workspace.tabs().len(), before + 1);
    });

    // Mongo collection row: double-click opens a collection tab (one tab, not two).
    let collection = tb_row_bounds(&app, visual, "fixture_collection_00");
    tb_double_click(visual, collection.center());
    app.read_with(visual, |this, _| {
        let tab = this.workspace.active_tab().expect("tab opened");
        assert_eq!(tab.collection, "fixture_collection_00");
        assert_eq!(tab.query_mode, Some(QueryMode::MongoFind));
        assert_eq!(this.workspace.tabs().len(), before + 2);
    });
}

#[test]
fn tab_text_fit_follows_content_and_truncates_the_suffix_before_the_name() {
    use tab_bar::{fit_tab_text, TAB_MAX_WIDTH, TAB_MIN_WIDTH};
    // Short content: clamped up to the minimum width, nothing truncated.
    let fit = fit_tab_text(30.0, None, false);
    assert!(fit.width >= TAB_MIN_WIDTH, "clamped up to the minimum: {fit:?}");
    assert!(!fit.truncated && fit.suffix_width.is_none());

    // Two similar long names with their suffixes both stay distinguishable.
    let fit = fit_tab_text(90.0, Some(60.0), false);
    assert!(!fit.truncated, "name and suffix fit in full: {fit:?}");
    assert!(fit.width > TAB_MIN_WIDTH && fit.width <= TAB_MAX_WIDTH);
    assert_eq!(fit.suffix_width, Some(60.0));

    // Too wide: the suffix shrinks first, the name keeps its full width.
    let fit = fit_tab_text(120.0, Some(100.0), false);
    assert_eq!(fit.width, TAB_MAX_WIDTH);
    assert!(fit.truncated);
    assert!(fit.name_width >= 120.0, "name untouched: {fit:?}");
    let suffix = fit.suffix_width.expect("suffix still drawn, ellipsized");
    assert!((28.0..100.0).contains(&suffix));

    // Still too wide: the suffix is dropped and the name is ellipsized.
    let fit = fit_tab_text(200.0, Some(80.0), false);
    assert!(fit.truncated && fit.suffix_width.is_none());
    assert!(fit.name_width < 200.0, "name ellipsized: {fit:?}");

    // Pinned tabs apply the same rule inside 120px.
    let fit = fit_tab_text(70.0, Some(60.0), true);
    assert_eq!(fit.width, tab_bar::PINNED_TAB_WIDTH);
    assert!(fit.truncated && fit.suffix_width.is_none());
    let fit = fit_tab_text(20.0, None, true);
    assert!(
        fit.width < tab_bar::PINNED_TAB_WIDTH,
        "short pinned tabs stay compact"
    );
}

#[test]
fn tab_label_parts_split_name_and_muted_suffix() {
    let mut workspace = WorkspaceState::new();
    let id = workspace.open_query_tab_with_metadata(
        "fixture_table_01",
        TabMetadata {
            database: "warehouse".into(),
            collection: "fixture_table_01".into(),
            schema: Some("public".into()),
            ..Default::default()
        },
    );
    let tab = workspace.tab(id).unwrap();
    let (name, suffix) = tab_bar::tab_label_parts(tab, false);
    assert_eq!(name, "fixture_table_01");
    assert_eq!(suffix.as_deref(), Some("@warehouse.public"));
    assert_eq!(tab_bar::tab_label_parts(tab, true).1, None);
    assert_eq!(
        tab_bar::tab_label(tab, false),
        "fixture_table_01@warehouse.public"
    );
}

// ---- GPUI workspace session persistence -------------------------------------

fn tb_session_dir() -> std::path::PathBuf {
    let dir = std::env::temp_dir().join(format!("tablex-session-test-{}", uuid::Uuid::new_v4()));
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

fn tb_read_session(path: &std::path::Path) -> crate::gpui_session::GpuiSession {
    match crate::gpui_session::load(path) {
        crate::gpui_session::LoadOutcome::Loaded(session) => session,
        other => panic!("session file: {other:?}"),
    }
}

/// Simulates a relaunch: a brand-new workspace restored from the file.
fn tb_relaunch(
    app: &gpui::Entity<AppRoot>,
    visual: &mut gpui::VisualTestContext,
    swift: impl FnOnce(&mut WorkspaceState, &[SavedConnection]),
) {
    app.update(visual, |this, cx| {
        this.workspace = WorkspaceState::default();
        this.apply_tab_bar_preferences(cx);
        this.restore_workspace_session(true, swift, cx);
        cx.notify();
    });
    tb_settle(visual);
}

fn tb_no_swift(_: &mut WorkspaceState, _: &[SavedConnection]) {
    panic!("the Swift import must not run when the GPUI file loads");
}

fn tb_advance(visual: &mut gpui::VisualTestContext, millis: u64) {
    visual
        .executor()
        .advance_clock(Duration::from_millis(millis));
    visual.run_until_parked();
}

#[gpui::test]
fn session_scenario_runs_never_point_at_the_real_application_support_file(cx: &mut TestAppContext) {
    let (app, visual) = tb_app(cx);
    app.read_with(visual, |this, _| {
        assert!(
            this.shell.session.path.is_none(),
            "tests start with persistence off"
        );
    });
    let real = crate::gpui_session::default_path().unwrap();
    assert!(real.ends_with("tablex/gpui-workspace-session.json"));
}

#[gpui::test]
fn session_round_trips_order_pins_active_text_filters_and_groups(cx: &mut TestAppContext) {
    use crate::sidebar_preferences::TabGroupBy;
    let (app, visual) = tb_app(cx);
    let dir = tb_session_dir();
    let path = dir.join("gpui-workspace-session.json");
    app.update(visual, |this, _| {
        this.shell.session.path = Some(path.clone())
    });
    // Group-by lives in the sidebar preferences file (also a temp file here).
    let prefs = tb_prefs_file(&app, visual);
    tb_settle(visual);

    let report = tb_open(
        &app,
        visual,
        "Report",
        crate::ui_fixture::POSTGRES_ID,
        "warehouse",
    );
    let table = tb_open_typed(&app, visual, "orders", "sqlTable");
    let sq = tb_open(&app, visual, "Lite", crate::ui_fixture::SQLITE_ID, "main");
    app.update(visual, |this, cx| {
        this.workspace.tab_mut(report).unwrap().sql = "SELECT 42".into();
        this.workspace.tab_mut(report).unwrap().output_limit = 77;
        // Applied (as if run) and live buffer; the session stores the applied query.
        let table_tab = this.workspace.tab_mut(table).unwrap();
        table_tab.table_where = "id > 3".into();
        table_tab.table_order_by = "id DESC".into();
        table_tab.applied_where = "id > 3".into();
        table_tab.applied_order_by = "id DESC".into();
        this.workspace.set_pinned(sq, true);
        this.set_tab_group_by(TabGroupBy::Connection, cx);
        this.workspace.activate_tab(report);
        let key = crate::workspace::TabGroupKey {
            connection_id: crate::ui_fixture::POSTGRES_ID.into(),
            database: String::new(),
        };
        // Collapse needs another visible tab outside the group: the pinned one.
        this.workspace.activate_tab(sq);
        assert!(this.workspace.toggle_group_collapsed(&key));
        cx.notify();
    });
    tb_settle(visual);
    let expected_titles: Vec<String> = app.read_with(visual, |this, _| {
        this.workspace
            .tabs()
            .iter()
            .map(|tab| tab.title.clone())
            .collect()
    });

    // Debounced: nothing is written until the changes go quiet for 500ms.
    app.read_with(visual, |this, _| assert_eq!(this.shell.session.writes, 0));
    tb_advance(visual, 300);
    assert!(!path.exists(), "still inside the debounce window");
    // The scheduling render itself reads tabs mutably (bumping the revision
    // once), so the first quiet period can take up to two windows.
    tb_advance(visual, 800);
    app.read_with(visual, |this, _| assert_eq!(this.shell.session.writes, 1));
    let saved = tb_read_session(&path);
    assert_eq!(saved.version, crate::gpui_session::SESSION_VERSION);
    assert_eq!(
        saved
            .tabs
            .iter()
            .map(|tab| tab.title.clone())
            .collect::<Vec<_>>(),
        expected_titles,
        "bar order is saved"
    );
    assert!(saved.tabs[0].is_pinned, "pinned tab leads");
    assert_eq!(saved.collapsed_groups.len(), 1);

    // Relaunch: the workspace comes back identically (fresh tab ids).
    tb_relaunch(&app, visual, tb_no_swift);
    app.read_with(visual, |this, _| {
        let tabs = this.workspace.tabs();
        let titles: Vec<_> = tabs.iter().map(|tab| tab.title.clone()).collect();
        assert_eq!(titles, expected_titles);
        assert!(tabs[0].is_pinned && tabs[0].title == "Lite");
        let report = tabs.iter().find(|tab| tab.title == "Report").unwrap();
        assert_eq!(report.sql, "SELECT 42");
        assert_eq!(report.output_limit, 77);
        assert_eq!(report.connection_id, crate::ui_fixture::POSTGRES_ID);
        let orders = tabs.iter().find(|tab| tab.title == "orders").unwrap();
        assert_eq!(orders.table_where, "id > 3");
        assert_eq!(orders.table_order_by, "id DESC");
        assert_eq!(orders.query_mode, Some(QueryMode::SqlTable));
        assert_eq!(
            this.workspace.active_tab().map(|tab| tab.title.as_str()),
            Some("Lite")
        );
        assert!(
            this.workspace.layout().iter().any(|item| matches!(
                item,
                crate::workspace::TabBarItem::Group {
                    collapsed: true,
                    ..
                }
            )),
            "collapsed group restored"
        );
    });
    std::fs::remove_dir_all(dir).unwrap();
    std::fs::remove_file(prefs).unwrap();
}

fn tb_open_unsettled(app: &gpui::Entity<AppRoot>, visual: &mut gpui::VisualTestContext, title: &str) {
    // No settle: leaves queued background work (a dispatched write) un-run.
    app.update(visual, |this, cx| {
        this.workspace
            .open_query_tab_with_metadata(title.to_string(), TabMetadata::default());
        cx.notify();
    });
}

fn tb_saved_titles(path: &std::path::Path) -> Vec<String> {
    tb_read_session(path).tabs.into_iter().map(|tab| tab.title).collect()
}

#[gpui::test]
fn quit_flush_after_a_background_flush_persists_the_latest_state_and_is_not_clobbered(
    cx: &mut TestAppContext,
) {
    let (app, visual) = tb_app(cx);
    let dir = tb_session_dir();
    let path = dir.join("gpui-workspace-session.json");
    app.update(visual, |this, _| this.shell.session.path = Some(path.clone()));

    // A debounce flush is dispatched (background write queued, not yet run)...
    tb_open_unsettled(&app, visual, "First");
    app.update(visual, |this, cx| this.flush_session(cx));
    // ...then the state changes again and the app quits before it ran.
    tb_open_unsettled(&app, visual, "Second");
    app.update(visual, |this, _| this.flush_session_blocking());
    assert_eq!(
        tb_saved_titles(&path),
        ["Welcome", "First", "Second"],
        "quit wrote the latest state synchronously"
    );

    // The older queued write finally runs: it must not overwrite the quit write.
    visual.run_until_parked();
    assert_eq!(
        tb_saved_titles(&path),
        ["Welcome", "First", "Second"],
        "a late background write must not clobber the quit-time snapshot"
    );
    std::fs::remove_dir_all(dir).unwrap();
}

#[gpui::test]
fn quit_flush_writes_even_when_the_identical_background_write_has_not_run_yet(
    cx: &mut TestAppContext,
) {
    let (app, visual) = tb_app(cx);
    let dir = tb_session_dir();
    let path = dir.join("gpui-workspace-session.json");
    app.update(visual, |this, _| this.shell.session.path = Some(path.clone()));
    tb_open_unsettled(&app, visual, "Only");
    app.update(visual, |this, cx| this.flush_session(cx));
    assert!(!path.exists(), "background write is still queued");
    // Same snapshot: the old "bytes already submitted" shortcut skipped this,
    // so quitting right after a debounce flush lost the state.
    app.update(visual, |this, _| this.flush_session_blocking());
    assert_eq!(tb_saved_titles(&path), ["Welcome", "Only"]);
    visual.run_until_parked();
    assert_eq!(tb_saved_titles(&path), ["Welcome", "Only"]);
    std::fs::remove_dir_all(dir).unwrap();
}

#[gpui::test]
fn session_saves_are_debounced_and_skip_unchanged_state(cx: &mut TestAppContext) {
    let (app, visual) = tb_app(cx);
    let dir = tb_session_dir();
    let path = dir.join("gpui-workspace-session.json");
    app.update(visual, |this, _| {
        this.shell.session.path = Some(path.clone())
    });
    for n in 0..5 {
        tb_open(&app, visual, &format!("Burst {n}"), "", "");
        tb_advance(visual, 200); // each change restarts the quiet period
    }
    app.read_with(visual, |this, _| {
        assert_eq!(this.shell.session.writes, 0, "never quiet yet")
    });
    tb_advance(visual, 600);
    app.read_with(visual, |this, _| {
        assert_eq!(this.shell.session.writes, 1, "one write per burst")
    });
    assert_eq!(tb_read_session(&path).tabs.len(), 6);
    // Touching a tab without changing it re-schedules but writes nothing new.
    app.update(visual, |this, cx| {
        let _ = this.workspace.active_tab_mut();
        cx.notify();
    });
    tb_settle(visual);
    tb_advance(visual, 700);
    app.read_with(visual, |this, _| assert_eq!(this.shell.session.writes, 1));
    std::fs::remove_dir_all(dir).unwrap();
}

#[gpui::test]
fn session_corrupted_file_is_ignored_with_a_notice(cx: &mut TestAppContext) {
    let (app, visual) = tb_app(cx);
    let dir = tb_session_dir();
    let path = dir.join("gpui-workspace-session.json");
    std::fs::write(&path, "{ definitely not json").unwrap();
    app.update(visual, |this, _| {
        this.shell.session.path = Some(path.clone())
    });
    tb_relaunch(&app, visual, |_, _| {});
    app.read_with(visual, |this, _| {
        assert_eq!(
            this.workspace.tabs().len(),
            1,
            "falls back to the default Welcome tab"
        );
        assert!(
            this.sidebar
                .notice
                .as_deref()
                .is_some_and(|notice| notice.contains("corrupted")),
            "notice: {:?}",
            this.sidebar.notice
        );
    });
    assert!(
        dir.join("gpui-workspace-session.json.corrupt").exists(),
        "bad file kept aside"
    );
    assert!(!path.exists());
    std::fs::remove_dir_all(dir).unwrap();
}

#[gpui::test]
fn session_falls_back_to_the_swift_import_when_the_gpui_file_is_missing(cx: &mut TestAppContext) {
    let (app, visual) = tb_app(cx);
    let dir = tb_session_dir();
    let path = dir.join("gpui-workspace-session.json");
    app.update(visual, |this, _| {
        this.shell.session.path = Some(path.clone())
    });
    // restore_workspace_session(scenario = false) is the real-run path.
    app.update(visual, |this, cx| {
        this.workspace = WorkspaceState::default();
        this.restore_workspace_session(
            false,
            |workspace, connections| {
                workspace::restore_tabs_into(
                    workspace,
                    connections,
                    vec![crate::session::RestoredTab {
                        source_id: "swift-1".into(),
                        active: true,
                        kind: "queryDraft".into(),
                        connection_id: crate::ui_fixture::POSTGRES_ID.into(),
                        database: "warehouse".into(),
                        schema: None,
                        collection: String::new(),
                        query_mode: None,
                        title: "From Swift".into(),
                        sql: "SELECT 1".into(),
                        output_limit: 200,
                        is_pinned: true,
                        where_clause: String::new(),
                        order_by: String::new(),
                    }],
                );
            },
            cx,
        );
    });
    app.read_with(visual, |this, _| {
        let titles: Vec<_> = this
            .workspace
            .tabs()
            .iter()
            .map(|tab| tab.title.as_str())
            .collect();
        assert_eq!(titles, vec!["From Swift", "Welcome"]);
    });
    // Once the shell saves, the GPUI file takes over and the import is skipped.
    app.update(visual, |this, cx| {
        this.workspace
            .rename_tab(this.workspace.tabs()[0].id, "Renamed");
        cx.notify();
    });
    tb_settle(visual);
    tb_advance(visual, 700);
    assert!(path.exists(), "GPUI file written, Swift file untouched");
    tb_relaunch(&app, visual, tb_no_swift);
    app.read_with(visual, |this, _| {
        assert_eq!(this.workspace.tabs()[0].title, "Renamed");
    });
    std::fs::remove_dir_all(dir).unwrap();
}

#[gpui::test]
fn session_drops_tabs_whose_connection_was_deleted(cx: &mut TestAppContext) {
    let (app, visual) = tb_app(cx);
    let dir = tb_session_dir();
    let path = dir.join("gpui-workspace-session.json");
    let session = crate::gpui_session::GpuiSession {
        tabs: vec![
            crate::gpui_session::SessionTab {
                title: "Gone".into(),
                connection_id: "99999999-9999-4999-8999-999999999999".into(),
                ..Default::default()
            },
            crate::gpui_session::SessionTab {
                title: "Kept".into(),
                ..Default::default()
            },
        ],
        active: Some(0),
        ..Default::default()
    };
    crate::gpui_session::write_atomic(&path, &crate::gpui_session::encode(&session).unwrap())
        .unwrap();
    app.update(visual, |this, _| {
        this.shell.session.path = Some(path.clone())
    });
    tb_relaunch(&app, visual, tb_no_swift);
    app.read_with(visual, |this, _| {
        let titles: Vec<_> = this
            .workspace
            .tabs()
            .iter()
            .map(|tab| tab.title.as_str())
            .collect();
        assert_eq!(titles, vec!["Kept"]);
        assert_eq!(
            this.workspace.active_tab().map(|tab| tab.title.as_str()),
            Some("Kept")
        );
    });
    std::fs::remove_dir_all(dir).unwrap();
}
