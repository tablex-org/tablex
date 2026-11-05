// Pinned result columns: the header menu's Pin / Unpin, the sticky block that keeps them (and the
// row gutter) at the left edge while the grid scrolls horizontally, selection and keyboard across the
// boundary, persistence per tab, and a script run's result tabs.

/// Right-clicks header cell `column` and picks the `item`-th entry of its menu with the keyboard
/// (0 = the first).
fn pick_header_menu_item(visual: &mut gpui::VisualTestContext, column: usize, item: usize) {
    let selector: &'static str = Box::leak(format!("grid-header-cell-{column}").into_boxed_str());
    let header = visual.debug_bounds(selector).unwrap_or_else(|| panic!("{selector} is painted"));
    visual.simulate_event(gpui::MouseDownEvent {
        button: MouseButton::Right,
        position: header.center(),
        modifiers: Default::default(),
        click_count: 1,
        first_mouse: false,
    });
    settle(visual);
    for _ in 0..=item {
        visual.simulate_keystrokes("down");
    }
    visual.simulate_keystrokes("enter");
    settle(visual);
}

/// A pin as the tests write it: `name` for the first column of that name, `name#n` for the n-th.
fn pin_labels(pins: &[crate::pinned_column::PinnedColumn]) -> Vec<String> {
    pins.iter()
        .map(|pin| if pin.occurrence == 0 { pin.name.clone() } else { format!("{}#{}", pin.name, pin.occurrence) })
        .collect()
}

fn pinned_state(app: &gpui::Entity<AppRoot>, visual: &gpui::VisualTestContext) -> (usize, Vec<usize>, Vec<String>) {
    app.read_with(visual, |this, _| {
        let view = this.av();
        (
            view.grid_ui.pinned,
            view.grid_ui.visible_columns.clone(),
            pin_labels(&this.workspace.active_tab().unwrap().pinned_columns),
        )
    })
}

fn left_of(visual: &mut gpui::VisualTestContext, selector: &'static str) -> f32 {
    f32::from(visual.debug_bounds(selector).unwrap_or_else(|| panic!("{selector} is painted")).left())
}

#[gpui::test]
fn the_header_menu_pins_a_column_first_and_it_stays_put_while_the_grid_scrolls(cx: &mut TestAppContext) {
    let (app, visual) = wide_grid_app(cx, 60, 100);
    visual.update(|window, _| window.activate_window());
    assert_eq!(pinned_state(&app, visual), (0, (0..100).collect(), vec![]));

    // Pin Column is the menu's first entry; column 3 moves to the front.
    pick_header_menu_item(visual, 3, 0);
    let (pinned, order, names) = pinned_state(&app, visual);
    assert_eq!((pinned, names), (1, vec!["col3".to_string()]));
    assert_eq!(&order[..4], &[3, 0, 1, 2], "the pinned column leads, the rest keep their order");
    app.read_with(visual, |this, _| assert_eq!(this.active_grid().column_name(this.av().grid_ui.visible_columns[0]), Some("col3")));
    let at_rest = (left_of(visual, "grid-header-cell-0"), left_of(visual, "grid-body-cell-0-0"));
    assert_eq!(at_rest.0, at_rest.1);

    // Scroll to the far right: the pinned header and cells did not move, the scrolling ones are culled.
    let max_x = app.update(visual, |this, _| f32::from(this.av().grid_horizontal_scroll.max_offset().x));
    scroll_wide_grid(&app, visual, Some(max_x), Some(5.0 * theme::ROW_HEIGHT));
    assert_eq!(left_of(visual, "grid-header-cell-0"), at_rest.0, "header cell stays at its x");
    assert_eq!(left_of(visual, "grid-body-cell-7-0"), at_rest.1, "body cell stays at its x");
    assert!(visual.debug_bounds("grid-header-cell-1").is_none(), "an unpinned column scrolled away");
    assert!(visual.debug_bounds("grid-header-cell-99").is_some());
    assert!(visual.debug_bounds("grid-body-cell-7-99").is_some());
    // The unpinned column on screen ends at the grid's right edge, not under the block.
    let sticky_right = left_of(visual, "grid-header-cell-0") + 160.0;
    let first_scrolling = (2..100)
        .filter_map(|column| {
            let selector: &'static str = Box::leak(format!("grid-body-cell-7-{column}").into_boxed_str());
            visual.debug_bounds(selector).map(|bounds| f32::from(bounds.left()))
        })
        .fold(f32::MAX, f32::min);
    assert!(first_scrolling < sticky_right, "some scrolled cells sit under the sticky block");
    // Scrolling back to the left restores the natural layout.
    scroll_wide_grid(&app, visual, Some(0.0), None);
    assert_eq!(left_of(visual, "grid-header-cell-0"), at_rest.0);
    assert!(visual.debug_bounds("grid-header-cell-1").is_some());

    // Unpin through the header menu of the pinned column: Unpin Column is its first entry.
    pick_header_menu_item(visual, 0, 0);
    assert_eq!(pinned_state(&app, visual), (0, (0..100).collect(), vec![]));
}

#[gpui::test]
fn freeze_first_column_toggles_and_unpin_all_clears(cx: &mut TestAppContext) {
    let (app, visual) = wide_grid_app(cx, 60, 100);
    visual.update(|window, _| window.activate_window());
    let pinned = |app: &gpui::Entity<AppRoot>, visual: &gpui::VisualTestContext| pinned_state(app, visual).2;
    // Entries: Pin column, Freeze first column, and Unpin all columns once something is pinned.
    // "Freeze first column" pins the FIRST column, whichever header was clicked.
    pick_header_menu_item(visual, 3, 1);
    assert_eq!(pinned(&app, visual), ["col0"]);
    pick_header_menu_item(visual, 3, 0);
    assert_eq!(pinned(&app, visual), ["col0", "col3"], "pins keep their order");
    assert_eq!(&pinned_state(&app, visual).1[..4], &[0, 3, 1, 2]);
    // Freeze first column is on (checked) while col0 is pinned; choosing it again unpins col0 only.
    app.read_with(visual, |this, _| assert!(this.first_column_frozen(this.active_id())));
    pick_header_menu_item(visual, 2, 1);
    assert_eq!(pinned(&app, visual), ["col3"]);
    app.read_with(visual, |this, _| assert!(!this.first_column_frozen(this.active_id())));
    // Pinning only col3 does not make "Freeze first column" read as on; choosing it pins col0 in FRONT.
    pick_header_menu_item(visual, 2, 1);
    assert_eq!(pinned(&app, visual), ["col0", "col3"]);
    pick_header_menu_item(visual, 2, 1);
    assert_eq!(pinned(&app, visual), ["col3"]);
    // Unpin all columns is the third entry.
    pick_header_menu_item(visual, 2, 2);
    assert_eq!(pinned_state(&app, visual), (0, (0..100).collect(), vec![]));
    // The toggle itself: on, then off (the first column is pinned, so it unpins).
    app.update(visual, |this, cx| this.toggle_freeze_first_column(this.active_id(), cx));
    settle(visual);
    assert_eq!(pinned_state(&app, visual).0, 1);
    app.update(visual, |this, cx| this.toggle_freeze_first_column(this.active_id(), cx));
    settle(visual);
    assert_eq!(pinned_state(&app, visual).0, 0);
}

#[gpui::test]
fn selection_and_arrow_keys_cross_the_pinned_boundary(cx: &mut TestAppContext) {
    let (app, visual) = wide_grid_app(cx, 60, 100);
    visual.update(|window, _| window.activate_window());
    app.update(visual, |this, cx| {
        let tab = this.active_id();
        this.pin_column(tab, 0, cx);
        this.pin_column(tab, 1, cx);
    });
    settle(visual);
    assert_eq!(pinned_state(&app, visual).0, 2);
    let max_x = app.update(visual, |this, _| f32::from(this.av().grid_horizontal_scroll.max_offset().x));
    scroll_wide_grid(&app, visual, Some(max_x), None);

    // A click on a pinned cell, which is painted at the left edge while the content is scrolled
    // 15 000 px away, selects that cell.
    let target = visual.debug_bounds("grid-body-cell-3-1").expect("a pinned cell is painted");
    visual.simulate_event(gpui::MouseDownEvent {
        button: MouseButton::Left,
        position: target.center(),
        modifiers: Default::default(),
        click_count: 1,
        first_mouse: false,
    });
    settle(visual);
    let selected = |app: &gpui::Entity<AppRoot>, visual: &gpui::VisualTestContext| {
        app.read_with(visual, |this, _| this.av().grid_ui.selection.borrow().range.map(|range| range.end))
    };
    assert_eq!(selected(&app, visual), Some(CellPosition { row: 3, column: 1 }));
    // A click on a scrolled cell right of the block selects it by its own column.
    let scrolling = visual.debug_bounds("grid-body-cell-3-99").expect("the last column is painted");
    visual.simulate_event(gpui::MouseDownEvent {
        button: MouseButton::Left,
        position: scrolling.center(),
        modifiers: Default::default(),
        click_count: 1,
        first_mouse: false,
    });
    settle(visual);
    assert_eq!(selected(&app, visual), Some(CellPosition { row: 3, column: 99 }));

    // Back on the pinned cell: Right crosses into the first scrolling column, which scrolls into view
    // right of the block rather than under it.
    let target = visual.debug_bounds("grid-body-cell-3-1").unwrap();
    visual.simulate_event(gpui::MouseDownEvent {
        button: MouseButton::Left,
        position: target.center(),
        modifiers: Default::default(),
        click_count: 1,
        first_mouse: false,
    });
    visual.simulate_keystrokes("right");
    settle(visual);
    assert_eq!(selected(&app, visual), Some(CellPosition { row: 3, column: 2 }));
    let block_right = f32::from(visual.debug_bounds("grid-body-cell-3-1").unwrap().right());
    let crossed = f32::from(visual.debug_bounds("grid-body-cell-3-2").expect("column 2 scrolled into view").left());
    assert!(crossed >= block_right - 0.5, "{crossed} is not right of the block's edge {block_right}");
    visual.simulate_keystrokes("right");
    settle(visual);
    assert_eq!(selected(&app, visual), Some(CellPosition { row: 3, column: 3 }));
    // And Left comes back across the boundary to the pinned cell.
    visual.simulate_keystrokes("left left");
    settle(visual);
    assert_eq!(selected(&app, visual), Some(CellPosition { row: 3, column: 1 }));
    // Down keeps the column.
    visual.simulate_keystrokes("down");
    settle(visual);
    assert_eq!(selected(&app, visual), Some(CellPosition { row: 4, column: 1 }));
}

#[gpui::test]
fn pinned_columns_that_would_cover_the_grid_scroll_like_the_others(cx: &mut TestAppContext) {
    let (app, visual) = wide_grid_app(cx, 20, 30);
    visual.update(|window, _| window.activate_window());
    app.update(visual, |this, cx| {
        let tab = this.active_id();
        for column in 0..8 {
            this.pin_column(tab, column, cx);
        }
    });
    settle(visual);
    app.read_with(visual, |this, _| {
        assert_eq!(this.av().grid_ui.pinned, 8);
        assert_eq!(this.av().grid_ui.frozen, 0, "8 x 160 px is more than 60% of the pane");
    });
    let max_x = app.update(visual, |this, _| f32::from(this.av().grid_horizontal_scroll.max_offset().x));
    let before = left_of(visual, "grid-header-cell-0");
    scroll_wide_grid(&app, visual, Some(max_x), None);
    assert!(visual.debug_bounds("grid-header-cell-0").is_none(), "they scroll away like any column");
    scroll_wide_grid(&app, visual, Some(0.0), None);
    assert_eq!(left_of(visual, "grid-header-cell-0"), before);
}

#[gpui::test]
fn a_pinned_cell_edits_in_place_and_staged_changes_lock_pinning(cx: &mut TestAppContext) {
    let db = SqliteDb::new(EDIT_ITEMS_SEED);
    let mut rig = SqlRig::sqlite(cx, &db);
    rig.open_table("items");
    rig.wait_structure();
    let tab = rig.tab;
    // Pin `name` (display 1): the grid is name, id, qty.
    rig.app.update(rig.visual, |this, cx| this.pin_column(tab, 1, cx));
    settle(rig.visual);
    assert_eq!(rig.grid_rows()[0], ["1", "a", "1"], "the page itself is not reordered");
    rig.app.read_with(rig.visual, |this, _| assert_eq!(this.view(tab).grid_ui.visible_columns, [1, 0, 2]));

    // Editing the pinned cell (display column 0, source `name`) stages the change on `name`.
    rig.edit_cell(0, 0, "alpha");
    assert_eq!(rig.staged(), (1, 0));
    // With a change staged the display columns are fixed: pinning and unpinning refuse.
    rig.app.update(rig.visual, |this, cx| {
        this.pin_column(tab, 2, cx);
        this.unpin_column(tab, 0, cx);
    });
    rig.app.read_with(rig.visual, |this, _| {
        assert_eq!(this.view(tab).grid_ui.visible_columns, [1, 0, 2]);
        assert_eq!(pin_labels(&this.workspace.tab(tab).unwrap().pinned_columns), ["name"]);
    });

    rig.apply();
    rig.wait_apply_finished();
    assert_eq!(db.sql(ITEMS_DUMP), "1:alpha:1|2:b:2|3:c:3", "the edit reached `name`, not the column shown first");
    rig.wait_not_running("the reload after Apply");
    // The pin survives the reload (it is by name).
    rig.app.read_with(rig.visual, |this, _| assert_eq!(this.view(tab).grid_ui.visible_columns, [1, 0, 2]));
}

#[gpui::test]
fn pins_persist_per_tab_and_a_second_tab_starts_without(cx: &mut TestAppContext) {
    cx.dispatcher.allow_parking();
    let (app, visual, tab_id) = sql_table_app(cx);
    app.update(visual, |this, cx| {
        this.pin_column(tab_id, 2, cx);
        this.pin_column(tab_id, 5, cx);
    });
    settle(visual);
    assert_eq!(shown_columns(&app, visual)[..3], ["status", "score", "id"]);
    let snapshot = app.read_with(visual, |this, _| this.workspace.snapshot());
    let saved = snapshot.tabs.iter().find(|tab| tab.collection == "orders").unwrap();
    assert_eq!(pin_labels(&saved.pinned_columns), ["status", "score"]);

    let json = crate::gpui_session::encode(&snapshot).expect("encode");
    let decoded: crate::gpui_session::GpuiSession = serde_json::from_slice(&json).expect("decode");
    app.update(visual, |this, cx| {
        this.workspace.restore_snapshot(&decoded, |_| true);
        this.sql_table_ui = Default::default();
        this.reset_active_view();
        this.run_active_tab_query(cx);
    });
    settle(visual);
    assert_eq!(shown_columns(&app, visual)[..3], ["status", "score", "id"], "the restored tab pins them again");
    app.read_with(visual, |this, _| assert_eq!(this.av().grid_ui.pinned, 2));

    // Another tab: its own, empty, pinned set (and an old session file without the field loads).
    let other = app.update(visual, |this, _| {
        this.workspace.open_query_tab_with_metadata(
            "orders-2",
            crate::workspace::TabMetadata {
                connection_id: crate::ui_fixture::POSTGRES_ID.into(),
                database: "warehouse".into(),
                schema: Some("public".into()),
                collection: "orders".into(),
                query_mode: Some(QueryMode::SqlTable),
                ..Default::default()
            },
        )
    });
    app.read_with(visual, |this, _| {
        assert!(this.workspace.tab(other).unwrap().pinned_columns.is_empty());
        assert!(this.pinned_columns(other).is_empty());
    });
    let old: crate::gpui_session::SessionTab = serde_json::from_str(r#"{"kind":"query","title":"t"}"#).unwrap();
    assert!(old.pinned_columns.is_empty());
}

#[gpui::test]
fn each_script_result_tab_keeps_its_own_pins(cx: &mut TestAppContext) {
    let db = SqliteDb::new(SCRIPT_SEED);
    let mut rig = SqlRig::sqlite(cx, &db);
    rig.run_script("SELECT id, name FROM items ORDER BY id;\nSELECT name, id FROM items ORDER BY id");
    rig.wait_script_done();
    rig.wait_rendered("script-tab-1");
    let tab = rig.tab;
    let order = |rig: &SqlRig<'_>| {
        rig.app.read_with(rig.visual, |this, _| {
            let grid = this.grid(tab);
            this.view(tab)
                .grid_ui
                .visible_columns
                .iter()
                .filter_map(|source| grid.column_name(*source).map(str::to_string))
                .collect::<Vec<_>>()
        })
    };
    assert_eq!(order(&rig), ["id", "name"]);

    // Result 1: pin `name` -> name, id.
    rig.app.update(rig.visual, |this, cx| this.pin_column(tab, 1, cx));
    settle(rig.visual);
    assert_eq!(order(&rig), ["name", "id"]);
    // Result 2 starts unpinned (its columns are name, id), and pinning `id` there is its own.
    rig.click("script-tab-1");
    assert_eq!(rig.script_shown(), Some(ScriptTab::Result(1)));
    assert_eq!(order(&rig), ["name", "id"]);
    rig.app.read_with(rig.visual, |this, _| assert_eq!(this.view(tab).grid_ui.pinned, 0));
    rig.app.update(rig.visual, |this, cx| this.pin_column(tab, 1, cx));
    settle(rig.visual);
    assert_eq!(order(&rig), ["id", "name"]);
    // Back on Result 1 its pin is still there; the tab's own set was never touched.
    rig.click("script-tab-0");
    assert_eq!(order(&rig), ["name", "id"]);
    rig.app.read_with(rig.visual, |this, _| {
        assert!(this.workspace.tab(tab).unwrap().pinned_columns.is_empty());
        let script = this.workspace.runtime(tab).unwrap().script.as_ref().unwrap();
        assert_eq!(pin_labels(&script.results[0].pinned), ["name"]);
        assert_eq!(pin_labels(&script.results[1].pinned), ["id"]);
    });
}

#[gpui::test]
fn pinning_is_not_offered_on_a_mongodb_or_redis_result(cx: &mut TestAppContext) {
    let (app, visual) = wide_grid_app(cx, 5, 4);
    app.update(visual, |this, cx| {
        let tab = this.active_id();
        assert!(this.pinning_available(tab));
        for (mode, offered) in [
            (QueryMode::MongoFind, false),
            (QueryMode::RedisKey, false),
            (QueryMode::SqlTable, true),
        ] {
            this.workspace.tab_mut(tab).unwrap().query_mode = Some(mode.clone());
            assert_eq!(this.pinning_available(tab), offered, "{mode:?}");
            this.pin_column(tab, 1, cx);
            assert_eq!(this.av().grid_ui.pinned > 0, offered, "{mode:?}");
            this.unpin_all_columns(tab, cx);
        }
    });
}

#[gpui::test]
fn freeze_first_column_pins_the_first_column_that_is_shown(cx: &mut TestAppContext) {
    cx.dispatcher.allow_parking();
    let (app, visual, tab_id) = sql_table_app(cx);
    app.update(visual, |this, cx| {
        this.toggle_sql_column(tab_id, "id".into(), cx);
        this.toggle_freeze_first_column(tab_id, cx);
    });
    settle(visual);
    app.read_with(visual, |this, _| {
        assert_eq!(pin_labels(this.pinned_columns(tab_id)), ["name"], "`id` is hidden: the first shown column is frozen");
        assert!(this.first_column_frozen(tab_id));
        assert_eq!(this.view(tab_id).grid_ui.pinned, 1);
    });
}

// --- Duplicate column names: a pin is the name plus the occurrence among same-named columns. ---

/// 60 rows by `columns` text columns named `col{i}`, except that the columns at 0 and `second_id` are both `id`.
fn duplicate_id_batch(rows: usize, columns: usize, second_id: usize) -> arrow_array::RecordBatch {
    let schema = std::sync::Arc::new(arrow_schema::Schema::new(
        (0..columns)
            .map(|i| {
                let name = if i == 0 || i == second_id { "id".to_string() } else { format!("col{i}") };
                arrow_schema::Field::new(name, arrow_schema::DataType::Utf8, true)
            })
            .collect::<Vec<_>>(),
    ));
    let arrays: Vec<arrow_array::ArrayRef> = (0..columns)
        .map(|c| {
            std::sync::Arc::new(arrow_array::StringArray::from_iter((0..rows).map(|r| Some(format!("r{r}c{c}")))))
                as arrow_array::ArrayRef
        })
        .collect();
    arrow_array::RecordBatch::try_new(schema, arrays).expect("valid batch")
}

/// A wide result whose columns 0 and 25 are both named `id`.
fn duplicate_id_app(cx: &mut TestAppContext) -> (gpui::Entity<AppRoot>, &mut gpui::VisualTestContext) {
    let (app, visual) = wide_grid_app(cx, 60, 40);
    app.update(visual, |this, cx| {
        this.set_active_grid(crate::grid::GridState::new(GridContentSource::Arrow(
            crate::grid_arrow::ArrowGridSource::new(vec![std::sync::Arc::new(duplicate_id_batch(60, 40, 25))]),
        )));
        this.reset_active_view();
        cx.notify();
    });
    visual.update(|window, _| window.activate_window());
    settle(visual);
    (app, visual)
}

/// Scrolls so that the second `id` (column 25) is on screen, culled columns being unpainted.
fn reveal_second_id(app: &gpui::Entity<AppRoot>, visual: &mut gpui::VisualTestContext) {
    scroll_wide_grid(app, visual, Some(25.0 * 160.0), None);
    assert!(visual.debug_bounds("grid-header-cell-25").is_some(), "the second `id` is painted");
}

/// Text of the first row in the column shown first.
fn first_displayed_cell(app: &gpui::Entity<AppRoot>, visual: &gpui::VisualTestContext) -> String {
    app.read_with(visual, |this, _| {
        let source = this.av().grid_ui.visible_columns[0];
        this.active_grid().cell_text(crate::grid::CellPosition { row: 0, column: source }).unwrap().to_string()
    })
}

#[gpui::test]
fn the_second_of_two_same_named_columns_pins_on_its_own_and_stays_put_on_scroll(cx: &mut TestAppContext) {
    let (app, visual) = duplicate_id_app(cx);
    assert_eq!(pinned_state(&app, visual).0, 0);

    // Pin Column on the second `id` (column 25): only it is pinned, and it leads.
    reveal_second_id(&app, visual);
    pick_header_menu_item(visual, 25, 0);
    let (pinned, order, pins) = pinned_state(&app, visual);
    assert_eq!((pinned, pins), (1, vec!["id#1".to_string()]));
    assert_eq!(&order[..4], &[25, 0, 1, 2], "the picked column leads, the first `id` keeps its place");
    assert_eq!(first_displayed_cell(&app, visual), "r0c25");

    // Its header and cells stay put at the far right; the first `id` scrolled away.
    let at_rest = (left_of(visual, "grid-header-cell-0"), left_of(visual, "grid-body-cell-0-0"));
    let max_x = app.update(visual, |this, _| f32::from(this.av().grid_horizontal_scroll.max_offset().x));
    scroll_wide_grid(&app, visual, Some(max_x), None);
    assert_eq!(left_of(visual, "grid-header-cell-0"), at_rest.0);
    assert_eq!(left_of(visual, "grid-body-cell-0-0"), at_rest.1);
    assert!(visual.debug_bounds("grid-header-cell-1").is_none(), "the first `id` is not pinned");
    scroll_wide_grid(&app, visual, Some(0.0), None);

    // The first `id` is now display column 1: its menu offers Pin; picking it pins it after the second.
    pick_header_menu_item(visual, 1, 0);
    let (pinned, order, pins) = pinned_state(&app, visual);
    assert_eq!((pinned, pins), (2, vec!["id#1".to_string(), "id".to_string()]), "pin order");
    assert_eq!(&order[..4], &[25, 0, 1, 2]);

    // Unpin the first `id` (its menu now offers Unpin): the second stays pinned.
    pick_header_menu_item(visual, 1, 0);
    let (pinned, order, pins) = pinned_state(&app, visual);
    assert_eq!((pinned, pins), (1, vec!["id#1".to_string()]));
    assert_eq!(&order[..3], &[25, 0, 1]);

    // And unpinning the second one (display column 0) clears the rest.
    pick_header_menu_item(visual, 0, 0);
    assert_eq!(pinned_state(&app, visual), (0, (0..40).collect(), vec![]));
}

#[gpui::test]
fn freeze_first_column_reads_as_on_only_for_the_first_of_the_duplicates(cx: &mut TestAppContext) {
    let (app, visual) = duplicate_id_app(cx);
    let frozen = |app: &gpui::Entity<AppRoot>, visual: &gpui::VisualTestContext| {
        app.read_with(visual, |this, _| this.first_column_frozen(this.active_id()))
    };
    assert!(!frozen(&app, visual));
    // Pinning the second `id` is not freezing the first column.
    reveal_second_id(&app, visual);
    pick_header_menu_item(visual, 25, 0);
    assert!(!frozen(&app, visual), "the pinned `id` is the second one");
    scroll_wide_grid(&app, visual, Some(0.0), None);
    // Freeze first column (second entry) pins the first `id` in front, leaving the second pinned.
    pick_header_menu_item(visual, 2, 1);
    assert_eq!(pinned_state(&app, visual).2, ["id", "id#1"]);
    assert!(frozen(&app, visual));
    assert_eq!(pinned_state(&app, visual).2, ["id", "id#1"]);
    // Choosing it again unpins the first `id` only.
    pick_header_menu_item(visual, 2, 1);
    assert!(!frozen(&app, visual));
    assert_eq!(pinned_state(&app, visual).2, ["id#1"]);
    // Unpin all columns (third entry).
    pick_header_menu_item(visual, 2, 2);
    assert_eq!(pinned_state(&app, visual), (0, (0..40).collect(), vec![]));
}

#[gpui::test]
fn the_cell_menu_action_pins_the_column_under_the_cell_not_its_namesake(cx: &mut TestAppContext) {
    let (app, visual) = duplicate_id_app(cx);
    // Pin Column / Unpin Column of the cell menu act on the right-clicked (display) column.
    let right_click_cell = |visual: &mut gpui::VisualTestContext, selector: &'static str| {
        let cell = visual.debug_bounds(selector).unwrap_or_else(|| panic!("{selector} is painted"));
        visual.simulate_event(gpui::MouseDownEvent {
            button: MouseButton::Right,
            position: cell.center(),
            modifiers: Default::default(),
            click_count: 1,
            first_mouse: false,
        });
        settle(visual);
    };
    reveal_second_id(&app, visual);
    right_click_cell(visual, "grid-body-cell-2-25");
    visual.dispatch_action(actions::PinGridColumn);
    settle(visual);
    assert_eq!(pinned_state(&app, visual).2, ["id#1"]);
    assert_eq!(first_displayed_cell(&app, visual), "r0c25");
    scroll_wide_grid(&app, visual, Some(0.0), None);
    visual.simulate_keystrokes("escape");
    settle(visual);
    // The pinned column is display column 0 now: its cell menu unpins it, and only it.
    right_click_cell(visual, "grid-body-cell-2-0");
    visual.dispatch_action(actions::UnpinGridColumn);
    settle(visual);
    assert_eq!(pinned_state(&app, visual).0, 0);
}

#[gpui::test]
fn a_duplicate_pin_survives_the_session_snapshot_and_old_plain_names_still_load(cx: &mut TestAppContext) {
    let (app, visual) = duplicate_id_app(cx);
    app.update(visual, |this, cx| {
        let tab = this.active_id();
        this.pin_column(tab, 25, cx);
        this.pin_column(tab, 1, cx);
    });
    settle(visual);
    assert_eq!(pinned_state(&app, visual).2, ["id#1", "id"]);
    let snapshot = app.read_with(visual, |this, _| this.workspace.snapshot());
    let json = crate::gpui_session::encode(&snapshot).expect("encode");
    let raw: serde_json::Value = serde_json::from_slice(&json).unwrap();
    let stored = raw["tabs"].as_array().unwrap().iter().find_map(|tab| tab["pinned_columns"].as_array().filter(|pins| !pins.is_empty()));
    assert_eq!(
        stored.expect("the tab with pins"),
        &[serde_json::json!({"name": "id", "occurrence": 1}), serde_json::json!("id")],
        "the second `id` is stored with its occurrence, the first as the plain name"
    );
    let decoded: crate::gpui_session::GpuiSession = serde_json::from_slice(&json).expect("decode");
    let saved = decoded.tabs.iter().find(|tab| !tab.pinned_columns.is_empty()).expect("the tab with pins");
    assert_eq!(pin_labels(&saved.pinned_columns), ["id#1", "id"]);
    app.update(visual, |this, _| this.workspace.restore_snapshot(&decoded, |_| true));
    app.read_with(visual, |this, _| {
        let restored = this.workspace.tabs().iter().find(|tab| !tab.pinned_columns.is_empty()).expect("restored pins");
        assert_eq!(pin_labels(&restored.pinned_columns), ["id#1", "id"]);
    });

    // A snapshot written before occurrences existed holds plain names: each is the first of its name.
    let old: crate::gpui_session::SessionTab =
        serde_json::from_str(r#"{"kind":"query","title":"t","pinned_columns":["id","name"]}"#).unwrap();
    assert_eq!(
        old.pinned_columns,
        [crate::pinned_column::PinnedColumn::new("id", 0), crate::pinned_column::PinnedColumn::new("name", 0)]
    );
}

#[gpui::test]
fn a_rerun_without_the_second_column_applies_no_pin_to_the_first(cx: &mut TestAppContext) {
    let db = SqliteDb::new(SCRIPT_SEED);
    let mut rig = SqlRig::sqlite(cx, &db);
    let tab = rig.tab;
    rig.run("SELECT a.id, b.id FROM items a JOIN items b ON a.id = b.id ORDER BY a.id");
    rig.wait_not_running("the join");
    rig.wait_rendered("grid-header-cell-1");
    let state = |rig: &SqlRig<'_>| {
        rig.app.read_with(rig.visual, |this, _| {
            let ui = &this.view(tab).grid_ui;
            (ui.pinned, ui.visible_columns.clone(), pin_labels(&this.workspace.tab(tab).unwrap().pinned_columns))
        })
    };
    assert_eq!(state(&rig).1, [0, 1]);
    rig.app.update(rig.visual, |this, cx| this.pin_column(tab, 1, cx));
    settle(rig.visual);
    assert_eq!(state(&rig), (1, vec![1, 0], vec!["id#1".to_string()]));

    // The query changes: only one `id` is left, and the pin on the second one does not move to it.
    rig.run("SELECT id, name FROM items ORDER BY id");
    rig.wait_not_running("the plain select");
    let (pinned, order, pins) = state(&rig);
    assert_eq!((pinned, order), (0, vec![0, 1]), "no pin applies");
    assert_eq!(pins, ["id#1"], "the stored pin is skipped, not rewritten");

    // The second `id` is back: its pin applies again.
    rig.run("SELECT a.id, b.id FROM items a JOIN items b ON a.id = b.id ORDER BY a.id");
    rig.wait_not_running("the join again");
    assert_eq!(state(&rig).0, 1);
    assert_eq!(state(&rig).1, [1, 0]);
}

#[gpui::test]
fn each_script_result_tab_pins_its_own_duplicate(cx: &mut TestAppContext) {
    let db = SqliteDb::new(SCRIPT_SEED);
    let mut rig = SqlRig::sqlite(cx, &db);
    rig.run_script("SELECT a.id, b.id FROM items a JOIN items b ON a.id = b.id ORDER BY a.id;\nSELECT id, name FROM items ORDER BY id");
    rig.wait_script_done();
    rig.wait_rendered("script-tab-1");
    let tab = rig.tab;
    let order = |rig: &SqlRig<'_>| {
        rig.app.read_with(rig.visual, |this, _| (this.view(tab).grid_ui.pinned, this.view(tab).grid_ui.visible_columns.clone()))
    };
    assert_eq!(order(&rig), (0, vec![0, 1]));
    rig.app.update(rig.visual, |this, cx| this.pin_column(tab, 1, cx));
    settle(rig.visual);
    assert_eq!(order(&rig), (1, vec![1, 0]), "the second `id` of the join leads");
    // The other result has one `id`: it starts unpinned.
    rig.click("script-tab-1");
    assert_eq!(order(&rig), (0, vec![0, 1]));
    rig.click("script-tab-0");
    assert_eq!(order(&rig), (1, vec![1, 0]));
    rig.app.read_with(rig.visual, |this, _| {
        assert!(this.workspace.tab(tab).unwrap().pinned_columns.is_empty());
        let script = this.workspace.runtime(tab).unwrap().script.as_ref().unwrap();
        assert_eq!(pin_labels(&script.results[0].pinned), ["id#1"]);
        assert!(script.results[1].pinned.is_empty());
    });
}
