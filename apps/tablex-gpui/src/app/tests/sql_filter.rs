// SQL table-data tabs: structured filter builder popover.

fn filter_tab(app: &gpui::Entity<AppRoot>, visual: &mut gpui::VisualTestContext) -> crate::workspace::Tab {
    app.read_with(visual, |this, _| this.workspace.active_tab().unwrap().clone())
}

fn open_filter_popover(visual: &mut gpui::VisualTestContext) {
    let button = visual.debug_bounds("sql-filter-control").expect("filter button");
    visual.simulate_click(button.center(), Default::default());
    settle(visual);
    assert!(visual.debug_bounds("sql-filter-panel").is_some(), "popover opens");
}

fn click_selector(visual: &mut gpui::VisualTestContext, selector: &'static str) {
    let bounds = visual
        .debug_bounds(selector)
        .unwrap_or_else(|| panic!("{selector} should render"));
    visual.simulate_click(bounds.center(), Default::default());
    settle(visual);
}

/// Opens a rule's Select (real click) and moves `steps` options down (up when
/// negative) from the current one with real keys, then confirms with Enter.
fn pick_with_keys(visual: &mut gpui::VisualTestContext, selector: &'static str, steps: isize) {
    click_selector(visual, selector);
    for _ in 0..steps.unsigned_abs() {
        visual.simulate_keystrokes(if steps < 0 { "up" } else { "down" });
    }
    visual.simulate_keystrokes("enter");
    settle(visual);
}

fn type_into(visual: &mut gpui::VisualTestContext, selector: &'static str, text: &str) {
    click_selector(visual, selector);
    visual.simulate_input(text);
    settle(visual);
}

/// Fixture `orders` ids matching `keep`.
fn orders_ids(keep: impl Fn(i64) -> bool) -> Vec<i64> {
    (1..=240).filter(|id| keep(*id)).collect()
}

/// Fixture `status` of an order id (see `FixtureRow::new`): NULL when
/// `index % 11 == 0`, else `active` for even indices and `pending` for odd.
fn fixture_status(id: i64) -> Option<&'static str> {
    let index = id - 1;
    (index % 11 != 0).then_some(if index % 2 == 0 { "active" } else { "pending" })
}

fn grid_ids(app: &gpui::Entity<AppRoot>, visual: &mut gpui::VisualTestContext) -> Vec<i64> {
    app.read_with(visual, |this, _| {
        (0..this.active_grid().row_count())
            .map(|row| {
                this.active_grid()
                    .full_cell_text(crate::grid::CellPosition { row, column: 0 })
                    .unwrap()
                    .parse()
                    .unwrap()
            })
            .collect()
    })
}

/// Builds `status = pending` AND `id > 100` through the real controls.
fn build_two_rules(visual: &mut gpui::VisualTestContext) {
    open_filter_popover(visual);
    click_selector(visual, "sql-filter-add");
    pick_with_keys(visual, "sql-filter-column-1", 2); // id -> name -> status
    type_into(visual, "sql-filter-value-1", "pending");
    pick_with_keys(visual, "sql-filter-mode-2", 12); // Equals ... -> ">"
    type_into(visual, "sql-filter-value-2", "100");
}

#[gpui::test]
fn filter_popover_builds_two_rules_applies_and_filters_the_fixture(cx: &mut TestAppContext) {
    cx.dispatcher.allow_parking();
    let (app, visual, tab_id) = sql_table_app(cx);
    assert!(visual.debug_bounds("sql-filter-panel").is_none());
    assert!(visual.debug_bounds("sql-filter-badge").is_none());
    open_filter_popover(visual);
    let tab = filter_tab(&app, visual);
    assert_eq!(tab.table_filter.rules.len(), 1, "one blank rule on first open");
    assert_eq!(tab.table_filter.rules[0].column, "id");
    assert!(visual.debug_bounds("sql-filter-rule-1").is_some());
    assert!(visual.debug_bounds("sql-filter-conjunction-1").is_none(), "no AND before the first rule");

    click_selector(visual, "sql-filter-add");
    assert_eq!(filter_tab(&app, visual).table_filter.rules.len(), 2);
    assert!(visual.debug_bounds("sql-filter-conjunction-2").is_some(), "AND between rules");

    // Column Select: real click opens it, keys move, Enter confirms.
    pick_with_keys(visual, "sql-filter-column-1", 2);
    app.read_with(visual, |this, _| {
        let rules = &this.workspace.active_tab().unwrap().table_filter.rules;
        assert_eq!(rules[0].column, "status");
        assert_eq!(rules[1].column, "id", "the other rule is untouched");
    });
    assert!(visual.debug_bounds("sql-filter-panel").is_some(), "choosing keeps the popover open");
    type_into(visual, "sql-filter-value-1", "pending");
    // Mode Select on rule 2: Equals -> ">".
    pick_with_keys(visual, "sql-filter-mode-2", 12);
    type_into(visual, "sql-filter-value-2", "100");
    app.read_with(visual, |this, _| {
        let rules = &this.workspace.active_tab().unwrap().table_filter.rules;
        assert_eq!(rules[0].value, "pending");
        assert_eq!(rules[1].mode, crate::sql_filter::SqlFilterMode::GreaterThan);
        assert_eq!(rules[1].value, "100");
        // Drafts do not touch the applied query.
        assert_eq!(this.workspace.active_tab().unwrap().applied_where, "");
        assert_eq!(this.active_grid().row_count(), 200);
    });

    click_selector(visual, "sql-filter-apply");
    let expected = orders_ids(|id| fixture_status(id) == Some("pending") && id > 100);
    app.read_with(visual, |this, cx| {
        let tab = this.workspace.active_tab().unwrap();
        let sql = "\"status\" = 'pending' AND \"id\" > 100";
        assert_eq!(tab.applied_where, sql);
        assert_eq!(tab.table_where, sql);
        assert_eq!(tab.table_filter.generated_where.as_deref(), Some(sql));
        assert_eq!(
            this.sql_table_ui
                .input_value(tab_id, results::sql_table::SqlField::Where, cx)
                .as_deref(),
            Some(sql),
            "the WHERE field shows the generated text"
        );
        assert_eq!(this.workspace.runtime(tab_id).unwrap().pagination.offset(), 0);
        assert!(!this.workspace.runtime(tab_id).unwrap().sql_table.filter_open, "Apply closes the popover");
        assert_eq!(
            this.sql_table_statement(tab).unwrap(),
            format!("SELECT * FROM \"public\".\"orders\"\nWHERE {sql}")
        );
    });
    assert_eq!(grid_ids(&app, visual), expected);
    assert!(visual.debug_bounds("sql-filter-panel").is_none());
    assert!(visual.debug_bounds("sql-filter-badge").is_some(), "active filter shows its count");
}

#[gpui::test]
fn filter_disabled_rules_are_excluded_and_or_toggles_the_group(cx: &mut TestAppContext) {
    cx.dispatcher.allow_parking();
    let (app, visual, _) = sql_table_app(cx);
    build_two_rules(visual);

    // The eye toggle disables rule 1: only `id > 100` is generated.
    click_selector(visual, "sql-filter-toggle-1");
    assert!(filter_tab(&app, visual).table_filter.rules[0].disabled);
    click_selector(visual, "sql-filter-apply");
    app.read_with(visual, |this, _| {
        assert_eq!(this.workspace.active_tab().unwrap().applied_where, "\"id\" > 100");
    });
    assert_eq!(grid_ids(&app, visual), orders_ids(|id| id > 100).into_iter().take(200).collect::<Vec<_>>());

    // Re-enable and flip AND -> OR by clicking the conjunction label.
    open_filter_popover(visual);
    click_selector(visual, "sql-filter-toggle-1");
    click_selector(visual, "sql-filter-conjunction-2");
    assert_eq!(
        filter_tab(&app, visual).table_filter.rules[1].conjunction,
        crate::sql_filter::SqlFilterConjunction::Or
    );
    click_selector(visual, "sql-filter-apply");
    app.read_with(visual, |this, _| {
        assert_eq!(
            this.workspace.active_tab().unwrap().applied_where,
            "\"status\" = 'pending' OR \"id\" > 100"
        );
    });
    let expected = orders_ids(|id| fixture_status(id) == Some("pending") || id > 100);
    assert_eq!(grid_ids(&app, visual), expected.into_iter().take(200).collect::<Vec<_>>());
}

#[gpui::test]
fn filter_between_in_and_blank_modes_run_against_the_fixture(cx: &mut TestAppContext) {
    cx.dispatcher.allow_parking();
    let (app, visual, _) = sql_table_app(cx);
    open_filter_popover(visual);
    // id BETWEEN 10 AND 14 (two inputs)
    pick_with_keys(visual, "sql-filter-mode-1", 16);
    assert!(visual.debug_bounds("sql-filter-end-1").is_some(), "range shows two inputs");
    type_into(visual, "sql-filter-value-1", "10");
    type_into(visual, "sql-filter-end-1", "14");
    click_selector(visual, "sql-filter-apply");
    app.read_with(visual, |this, _| {
        assert_eq!(
            this.workspace.active_tab().unwrap().applied_where,
            "\"id\" BETWEEN 10 AND 14"
        );
    });
    assert_eq!(grid_ids(&app, visual), vec![10, 11, 12, 13, 14]);

    // id IN (1, 2, 3) through the multi-line list editor (mode Between -> In list).
    open_filter_popover(visual);
    pick_with_keys(visual, "sql-filter-mode-1", -2);
    assert!(visual.debug_bounds("sql-filter-end-1").is_none());
    assert!(visual.debug_bounds("sql-filter-list-1").is_some(), "list mode shows the multi-line editor");
    click_selector(visual, "sql-filter-list-1");
    visual.simulate_keystrokes("secondary-a");
    visual.simulate_input("1, 2, 3");
    settle(visual);
    // Second rule: status IS NULL (no value editor).
    click_selector(visual, "sql-filter-add");
    pick_with_keys(visual, "sql-filter-column-2", 2);
    pick_with_keys(visual, "sql-filter-mode-2", 2);
    assert!(visual.debug_bounds("sql-filter-no-value-2").is_some());
    assert!(visual.debug_bounds("sql-filter-value-2").is_none(), "NULL checks hide the value input");
    click_selector(visual, "sql-filter-apply");
    app.read_with(visual, |this, _| {
        assert_eq!(
            this.workspace.active_tab().unwrap().applied_where,
            "\"id\" IN (1, 2, 3) AND \"status\" IS NULL"
        );
    });
    assert_eq!(grid_ids(&app, visual), vec![1], "only order 1 has a NULL status");
}

#[gpui::test]
fn clear_filter_drops_rules_and_where_and_reruns(cx: &mut TestAppContext) {
    cx.dispatcher.allow_parking();
    let (app, visual, tab_id) = sql_table_app(cx);
    build_two_rules(visual);
    click_selector(visual, "sql-filter-apply");
    assert!(grid_ids(&app, visual).len() < 200);
    let generation = app.read_with(visual, |this, _| {
        this.workspace.runtime(tab_id).unwrap().query.generation()
    });

    open_filter_popover(visual);
    click_selector(visual, "sql-filter-clear");
    app.read_with(visual, |this, cx| {
        let tab = this.workspace.active_tab().unwrap();
        assert!(tab.table_filter.rules.is_empty());
        assert_eq!(tab.table_filter.generated_where, None);
        assert_eq!((tab.table_where.as_str(), tab.applied_where.as_str()), ("", ""));
        assert_eq!(
            this.sql_table_ui
                .input_value(tab_id, results::sql_table::SqlField::Where, cx)
                .as_deref(),
            Some("")
        );
        assert!(this.workspace.runtime(tab_id).unwrap().query.generation() > generation, "re-ran");
        assert_eq!(this.sql_table_ui.filter.row_count(tab_id), 0, "rule editors are dropped");
    });
    assert_eq!(grid_ids(&app, visual).len(), 200, "full first page again");
    assert!(visual.debug_bounds("sql-filter-panel").is_none(), "Clear Filter closes the popover");
    assert!(visual.debug_bounds("sql-filter-badge").is_none());

    // Reopening starts from one blank rule again.
    open_filter_popover(visual);
    assert_eq!(filter_tab(&app, visual).table_filter.rules.len(), 1);
}

#[gpui::test]
fn reset_rules_restores_one_blank_rule_without_touching_the_query(cx: &mut TestAppContext) {
    cx.dispatcher.allow_parking();
    let (app, visual, _) = sql_table_app(cx);
    build_two_rules(visual);
    click_selector(visual, "sql-filter-apply");
    open_filter_popover(visual);
    click_selector(visual, "sql-filter-reset");
    let tab = filter_tab(&app, visual);
    assert_eq!(tab.table_filter.rules.len(), 1);
    assert_eq!(tab.table_filter.rules[0].column, "id");
    assert_eq!(tab.table_filter.rules[0].value, "");
    assert_eq!(tab.applied_where, "\"status\" = 'pending' AND \"id\" > 100", "query is unchanged");
    assert!(visual.debug_bounds("sql-filter-panel").is_some());
}

#[gpui::test]
fn staged_changes_block_apply_and_clear_like_run(cx: &mut TestAppContext) {
    cx.dispatcher.allow_parking();
    let (app, visual, tab_id) = sql_table_app(cx);
    build_two_rules(visual);
    app.update(visual, |this, cx| {
        this.select_grid_cell(this.active_id(), crate::grid::CellPosition { row: 0, column: 1 });
        let edits = this.prepare_context_edits(this.active_id(), Some("renamed")).expect("editable name cell");
        this.stage_context_edits(this.active_id(), edits, Some("renamed"), cx).expect("staged");
    });
    settle(visual);
    let before = filter_tab(&app, visual);
    let generation = app.read_with(visual, |this, _| {
        this.workspace.runtime(tab_id).unwrap().query.generation()
    });
    assert!(visual.debug_bounds("sql-filter-panel").is_some(), "staging keeps the popover");

    click_selector(visual, "sql-filter-apply");
    app.read_with(visual, |this, _| {
        let tab = this.workspace.active_tab().unwrap();
        assert_eq!(tab.applied_where, "", "nothing applied while changes are staged");
        assert_eq!(tab.table_where, "");
        assert_eq!(tab.table_filter, before.table_filter, "rules are untouched");
        let runtime = this.workspace.runtime(tab_id).unwrap();
        assert!(runtime.sql_table.filter_error.as_deref().is_some_and(|e| e.contains("staged")));
        assert_eq!(runtime.query.generation(), generation, "no query ran");
        assert!(runtime.sql_table.filter_open);
    });
    assert!(visual.debug_bounds("sql-filter-error").is_some(), "the block is explained in the popover");

    click_selector(visual, "sql-filter-clear");
    app.read_with(visual, |this, _| {
        let tab = this.workspace.active_tab().unwrap();
        assert_eq!(tab.table_filter.rules.len(), 2, "Clear Filter is blocked too");
        assert_eq!(this.workspace.runtime(tab_id).unwrap().query.generation(), generation);
    });

    // Editing a rule clears the stale message.
    type_into(visual, "sql-filter-value-2", "1");
    assert!(visual.debug_bounds("sql-filter-error").is_none());
}

#[gpui::test]
fn manual_where_edit_shows_the_notice_and_apply_overwrites_it(cx: &mut TestAppContext) {
    cx.dispatcher.allow_parking();
    let (app, visual, tab_id) = sql_table_app(cx);
    build_two_rules(visual);
    click_selector(visual, "sql-filter-apply");
    open_filter_popover(visual);
    assert!(visual.debug_bounds("sql-filter-notice").is_none(), "in sync after Apply");
    // Close, edit the WHERE text by hand, reopen.
    visual.simulate_keystrokes("escape");
    settle(visual);
    let field = visual.debug_bounds("sql-where-field").unwrap();
    visual.simulate_click(field.center(), Default::default());
    visual.simulate_keystrokes("end");
    visual.simulate_input(" OR id = 1");
    settle(visual);
    open_filter_popover(visual);
    assert!(visual.debug_bounds("sql-filter-notice").is_some(), "WHERE edited manually");
    app.read_with(visual, |this, _| {
        assert_eq!(
            this.workspace.active_tab().unwrap().table_where,
            "\"status\" = 'pending' AND \"id\" > 100 OR id = 1"
        );
        assert_eq!(this.workspace.active_tab().unwrap().applied_where, "\"status\" = 'pending' AND \"id\" > 100");
    });
    click_selector(visual, "sql-filter-apply");
    app.read_with(visual, |this, _| {
        let tab = this.workspace.active_tab().unwrap();
        assert_eq!(tab.table_where, "\"status\" = 'pending' AND \"id\" > 100", "Apply overwrites, no confirm");
        assert_eq!(tab.applied_where, tab.table_where);
    });
    open_filter_popover(visual);
    assert!(visual.debug_bounds("sql-filter-notice").is_none());
    let _ = tab_id;
}

#[gpui::test]
fn column_select_dismisses_with_escape_and_reopens_on_the_selected_value(cx: &mut TestAppContext) {
    cx.dispatcher.allow_parking();
    let (app, visual, _) = sql_table_app(cx);
    open_filter_popover(visual);
    // Open the mode Select, move the cursor, dismiss with Escape: nothing changes
    // and the popover itself stays open.
    click_selector(visual, "sql-filter-mode-1");
    visual.simulate_keystrokes("down");
    visual.simulate_keystrokes("down");
    visual.simulate_keystrokes("escape");
    settle(visual);
    assert!(visual.debug_bounds("sql-filter-panel").is_some(), "Escape closes only the dropdown");
    assert_eq!(filter_tab(&app, visual).table_filter.rules[0].mode, crate::sql_filter::SqlFilterMode::Equals);
    // Pick "Not equals", reopen: the cursor starts on the selected value, so one
    // `down` + Enter lands on the next option ("Is NULL").
    pick_with_keys(visual, "sql-filter-mode-1", 1);
    assert_eq!(filter_tab(&app, visual).table_filter.rules[0].mode, crate::sql_filter::SqlFilterMode::NotEquals);
    pick_with_keys(visual, "sql-filter-mode-1", 1);
    assert_eq!(filter_tab(&app, visual).table_filter.rules[0].mode, crate::sql_filter::SqlFilterMode::IsNull);
    // Escape with no dropdown open closes the popover (standard dismissal).
    visual.simulate_keystrokes("escape");
    settle(visual);
    assert!(visual.debug_bounds("sql-filter-panel").is_none());
}

#[gpui::test]
fn popover_dismisses_on_outside_click_and_does_not_click_through(cx: &mut TestAppContext) {
    cx.dispatcher.allow_parking();
    let (app, visual, _) = sql_table_app(cx);
    open_filter_popover(visual);
    let panel = visual.debug_bounds("sql-filter-panel").unwrap();
    let button = visual.debug_bounds("sql-filter-control").unwrap();
    // Anchored below the funnel button, left aligned, fully inside the window.
    assert!(panel.top() >= button.bottom(), "below the trigger: {panel:?} {button:?}");
    assert!((panel.left() - button.left()).abs() < gpui::px(8.0), "left aligned: {panel:?} {button:?}");
    assert!(panel.right() <= gpui::px(1400.0) && panel.bottom() <= gpui::px(850.0));

    // A click inside the panel on an inert area keeps it open.
    let inside = gpui::point(panel.center().x, panel.top() + gpui::px(14.0));
    visual.simulate_click(inside, Default::default());
    settle(visual);
    assert!(visual.debug_bounds("sql-filter-panel").is_some());

    // Click outside, on a grid header: the popover closes; the header (below the
    // popover) must not sort through the open overlay's first click.
    let header = visual.debug_bounds("grid-header-cell-6").expect("a header cell");
    let outside = header.center();
    assert!(!panel.contains(&outside), "header is outside the panel");
    visual.simulate_click(outside, Default::default());
    settle(visual);
    assert!(visual.debug_bounds("sql-filter-panel").is_none(), "outside click dismisses");
    app.read_with(visual, |this, _| {
        assert!(!this.workspace.runtime(this.workspace.active_tab().unwrap().id).unwrap().sql_table.filter_open);
    });
    // Reopen through the trigger.
    open_filter_popover(visual);
}

#[gpui::test]
fn dragging_a_rule_handle_reorders_rules(cx: &mut TestAppContext) {
    cx.dispatcher.allow_parking();
    let (app, visual, _) = sql_table_app(cx);
    build_two_rules(visual);
    let order = |app: &gpui::Entity<AppRoot>, visual: &mut gpui::VisualTestContext| -> Vec<u64> {
        filter_tab(app, visual).table_filter.rules.iter().map(|rule| rule.id).collect()
    };
    assert_eq!(order(&app, visual), vec![1, 2]);
    let handle = visual.debug_bounds("sql-filter-handle-2").expect("grip handle");
    let target = visual.debug_bounds("sql-filter-rule-1").expect("first rule row");
    let drop_at = gpui::point(target.center().x, target.center().y);
    visual.simulate_mouse_down(handle.center(), gpui::MouseButton::Left, Default::default());
    visual.simulate_mouse_move(
        gpui::point(handle.center().x, handle.center().y - gpui::px(12.0)),
        gpui::MouseButton::Left,
        Default::default(),
    );
    visual.simulate_mouse_move(drop_at, gpui::MouseButton::Left, Default::default());
    visual.simulate_mouse_up(drop_at, gpui::MouseButton::Left, Default::default());
    settle(visual);
    assert_eq!(order(&app, visual), vec![2, 1], "rule 2 moved above rule 1");
    // Apply reflects the new order; the conjunction of the moved rule is kept.
    click_selector(visual, "sql-filter-apply");
    app.read_with(visual, |this, _| {
        assert_eq!(
            this.workspace.active_tab().unwrap().applied_where,
            "\"id\" > 100 AND \"status\" = 'pending'"
        );
    });
}

#[gpui::test]
fn filter_rules_persist_per_tab_in_the_session_snapshot(cx: &mut TestAppContext) {
    cx.dispatcher.allow_parking();
    let (app, visual, _) = sql_table_app(cx);
    build_two_rules(visual);
    click_selector(visual, "sql-filter-toggle-2");
    click_selector(visual, "sql-filter-apply");
    let snapshot = app.read_with(visual, |this, _| this.workspace.snapshot());
    let saved = snapshot.tabs.iter().find(|tab| tab.collection == "orders").unwrap();
    assert_eq!(saved.table_filter_rules.len(), 2);
    assert!(saved.table_filter_rules[1].disabled);
    assert_eq!(saved.table_filter_where.as_deref(), Some("\"status\" = 'pending'"));
    assert_eq!(saved.table_where, "\"status\" = 'pending'");

    // Through the on-disk JSON format and back into a fresh workspace.
    let json = crate::gpui_session::encode(&snapshot).expect("encode");
    let decoded: crate::gpui_session::GpuiSession = serde_json::from_slice(&json).expect("decode");
    let mut other = crate::workspace::WorkspaceState::default();
    other.restore_snapshot(&decoded, |_| true);
    let tab = other.tabs().iter().find(|tab| tab.collection == "orders").unwrap();
    let original = app.read_with(visual, |this, _| this.workspace.active_tab().unwrap().table_filter.clone());
    assert_eq!(tab.table_filter, original);
    assert_eq!(tab.applied_where, "\"status\" = 'pending'");
    // A second snapshot is identical (stable round trip).
    assert_eq!(other.snapshot().tabs.iter().find(|tab| tab.collection == "orders"), Some(saved));
}

#[gpui::test]
fn restored_filter_rules_reopen_with_their_values(cx: &mut TestAppContext) {
    cx.dispatcher.allow_parking();
    let (app, visual, _) = sql_table_app(cx);
    build_two_rules(visual);
    click_selector(visual, "sql-filter-apply");
    let snapshot = app.read_with(visual, |this, _| this.workspace.snapshot());
    // Restore into the running workspace (what relaunch does) and reopen.
    app.update(visual, |this, cx| {
        this.workspace.restore_snapshot(&snapshot, |_| true);
        this.sql_table_ui = Default::default();
        this.reset_active_view();
        cx.notify();
    });
    settle(visual);
    assert!(visual.debug_bounds("sql-filter-badge").is_some(), "restored active filter shows its badge");
    open_filter_popover(visual);
    let rules = filter_tab(&app, visual).table_filter.rules;
    assert_eq!((rules[0].column.as_str(), rules[0].value.as_str()), ("status", "pending"));
    assert_eq!((rules[1].column.as_str(), rules[1].value.as_str()), ("id", "100"));
    assert_eq!(rules[1].mode, crate::sql_filter::SqlFilterMode::GreaterThan);
    // The editors show the restored text and the selects the restored values.
    app.read_with(visual, |this, cx| {
        let tab = this.workspace.active_tab().unwrap();
        let editors = this.sql_table_ui.filter.editors_for_test(tab.id, 2).expect("rule 2 editors");
        assert_eq!(editors.0.read(cx).selected_value().map(String::as_str), Some("id"));
        assert_eq!(editors.1.read(cx).selected_value().map(String::as_str), Some(">"));
        assert_eq!(editors.2.read(cx).value().as_ref(), "100");
    });
}

/// Item pitch of a `Select` list at the `Small` size (measured: 26.7px).
const SELECT_ITEM_PITCH: f32 = 26.7;

/// Clicks option `index` of an open `Select` list. `first` is the distance from
/// the trigger's bottom edge to the first option's centre: 22px for a plain
/// list, 48.7px for a searchable one (the search row takes one item height).
fn click_select_option(
    visual: &mut gpui::VisualTestContext,
    trigger: &'static str,
    first: f32,
    index: usize,
) {
    let bounds = visual.debug_bounds(trigger).unwrap_or_else(|| panic!("{trigger}"));
    let at = gpui::point(
        bounds.center().x,
        bounds.bottom() + gpui::px(first + SELECT_ITEM_PITCH * index as f32),
    );
    visual.simulate_click(at, Default::default());
    settle(visual);
}

#[gpui::test]
fn select_options_clicked_outside_the_panel_apply_without_closing_or_clicking_through(
    cx: &mut TestAppContext,
) {
    cx.dispatcher.allow_parking();
    let (app, visual, tab_id) = sql_table_app(cx);
    open_filter_popover(visual);
    let panel = visual.debug_bounds("sql-filter-panel").unwrap();
    let selection_before = app.read_with(visual, |this, _| this.av().grid_ui.selection.borrow().range);

    // Column Select (searchable): open with a click, choose `created` (5th
    // option) with a click on its row. That row lies below the panel's bottom
    // edge, the case where a plain `Popover` dismisses itself (its outside-click
    // test is geometric) and the click lands on the grid.
    click_selector(visual, "sql-filter-column-1");
    let column = visual.debug_bounds("sql-filter-column-1").unwrap();
    let option_y = column.bottom() + gpui::px(48.7 + SELECT_ITEM_PITCH * 4.0);
    assert!(option_y > panel.bottom(), "the option list extends past the panel: {option_y:?} {panel:?}");
    click_select_option(visual, "sql-filter-column-1", 48.7, 4);
    app.read_with(visual, |this, _| {
        let tab = this.workspace.active_tab().unwrap();
        assert_eq!(tab.table_filter.rules[0].column, "created");
        assert_eq!(this.av().grid_ui.selection.borrow().range, selection_before, "no click-through to the grid");
        assert!(this.workspace.runtime(tab_id).unwrap().sql_table.filter_open, "popover stays open");
    });
    assert!(visual.debug_bounds("sql-filter-panel").is_some());

    // Mode Select (plain list): choose "Not equals" (2nd option) by clicking.
    click_selector(visual, "sql-filter-mode-1");
    click_select_option(visual, "sql-filter-mode-1", 22.0, 1);
    assert_eq!(
        filter_tab(&app, visual).table_filter.rules[0].mode,
        crate::sql_filter::SqlFilterMode::NotEquals
    );
    assert!(visual.debug_bounds("sql-filter-panel").is_some());

    // Dismiss + reopen: the cursor starts on the selected value, so Enter
    // keeps it and one `up` moves to the previous option ("Equals").
    click_selector(visual, "sql-filter-mode-1");
    visual.simulate_keystrokes("escape");
    settle(visual);
    pick_with_keys(visual, "sql-filter-mode-1", 0);
    assert_eq!(
        filter_tab(&app, visual).table_filter.rules[0].mode,
        crate::sql_filter::SqlFilterMode::NotEquals,
        "reopened on the selected option"
    );
    pick_with_keys(visual, "sql-filter-mode-1", -1);
    assert_eq!(
        filter_tab(&app, visual).table_filter.rules[0].mode,
        crate::sql_filter::SqlFilterMode::Equals
    );

    // Clicking the funnel while open (it sits under the backdrop) closes it once.
    let button = visual.debug_bounds("sql-filter-control").unwrap();
    visual.simulate_click(button.center(), Default::default());
    settle(visual);
    assert!(visual.debug_bounds("sql-filter-panel").is_none(), "trigger click closes the popover");
    assert!(visual.debug_bounds("sql-filter-overlay").is_none(), "backdrop is removed with it");
    assert_eq!(app.read_with(visual, |this, _| this.av().grid_ui.selection.borrow().range), selection_before);
}
