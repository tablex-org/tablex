// The sort button of each result-grid column header and its menu.

use super::results::grid::{sort_menu_items, ColumnSort};

const HEADER_SORT_STATES: [&str; 5] = ["none", "database-asc", "database-desc", "page-asc", "page-desc"];

/// The state of header `column`'s sort button, from the selector it is painted under.
fn header_sort_state(visual: &mut gpui::VisualTestContext, column: usize) -> Option<&'static str> {
    HEADER_SORT_STATES.into_iter().find(|state| {
        let selector: &'static str = Box::leak(format!("grid-header-sort-{column}-{state}").into_boxed_str());
        visual.debug_bounds(selector).is_some()
    })
}

fn header_sort_bounds(visual: &mut gpui::VisualTestContext, column: usize) -> gpui::Bounds<gpui::Pixels> {
    let state = header_sort_state(visual, column).expect("the sort button is painted");
    let selector: &'static str = Box::leak(format!("grid-header-sort-{column}-{state}").into_boxed_str());
    visual.debug_bounds(selector).unwrap()
}

fn hover_header(visual: &mut gpui::VisualTestContext, column: usize) {
    let selector: &'static str = Box::leak(format!("grid-header-cell-{column}").into_boxed_str());
    let header = visual.debug_bounds(selector).expect("header cell is painted");
    visual.simulate_mouse_move(header.center(), None, Default::default());
    settle(visual);
}

/// Hovers header `column` and clicks its sort button.
fn open_header_sort_menu(visual: &mut gpui::VisualTestContext, column: usize) {
    hover_header(visual, column);
    let button = header_sort_bounds(visual, column);
    visual.simulate_click(button.center(), Default::default());
    settle(visual);
}

/// Moves down `downs` enabled entries of the open menu (labels and disabled entries are skipped)
/// and confirms.
fn pick_header_sort_entry(visual: &mut gpui::VisualTestContext, downs: usize) {
    for _ in 0..downs {
        visual.simulate_keystrokes("down");
    }
    visual.simulate_keystrokes("enter");
    settle(visual);
}

fn open_sort_menu_column(app: &gpui::Entity<AppRoot>, visual: &gpui::VisualTestContext) -> Option<usize> {
    app.read_with(visual, |this, _| this.av().grid_ui.sort_menu)
}

fn header_generation(app: &gpui::Entity<AppRoot>, visual: &gpui::VisualTestContext, tab_id: u64) -> u64 {
    app.read_with(visual, |this, _| this.workspace.runtime(tab_id).unwrap().query.generation())
}

fn column_named(app: &gpui::Entity<AppRoot>, visual: &gpui::VisualTestContext, name: &str) -> usize {
    app.read_with(visual, |this, _| {
        (0..this.active_grid().column_count())
            .find(|column| this.active_grid().column_name(*column) == Some(name))
            .expect("fixture column")
    })
}

#[test]
fn header_sort_menu_lists_both_scopes_and_checks_the_active_sort() {
    let entries = |sort: ColumnSort| {
        sort_menu_items(0, 0, sort, &gpui::WeakEntity::new_invalid())
            .iter()
            .map(|item| match item {
                PopupMenuItem::Label(text) => format!("# {text}"),
                PopupMenuItem::Separator => "---".to_string(),
                PopupMenuItem::Item { label, disabled, checked, .. } => {
                    format!("{label}{}{}", if *checked { " [x]" } else { "" }, if *disabled { " (disabled)" } else { "" })
                }
                _ => "?".to_string(),
            })
            .collect::<Vec<_>>()
    };
    let unsorted = ColumnSort { database: None, page: None, can_database_sort: true, menu_open: true };
    assert_eq!(
        entries(unsorted),
        [
            "Sort Database Ascending",
            "Sort Database Descending",
            "---",
            "Sort Current Page Ascending",
            "Sort Current Page Descending",
            "---",
            "Clear Sort (disabled)",
        ]
    );
    let sorted = ColumnSort { database: Some(true), page: Some(false), ..unsorted };
    assert_eq!(
        entries(sorted),
        [
            "Sort Database Ascending",
            "Sort Database Descending [x]",
            "---",
            "Sort Current Page Ascending [x]",
            "Sort Current Page Descending",
            "---",
            "Clear Sort",
        ]
    );
    let offline = ColumnSort { can_database_sort: false, ..unsorted };
    assert_eq!(
        entries(offline),
        [
            "Sort Database Ascending (disabled)",
            "Sort Database Descending (disabled)",
            "---",
            "Sort Current Page Ascending",
            "Sort Current Page Descending",
            "---",
            "Clear Sort (disabled)",
        ]
    );
}

#[gpui::test]
fn header_sort_button_shows_the_chevron_and_a_sorted_column_its_sort_icon(cx: &mut TestAppContext) {
    // Every column shows its button, unsorted ones the faint chevron, without hovering.
    let (app, visual, _) = sql_table_app(cx);
    visual.update(|window, _| window.activate_window());
    settle(visual);
    for column in 0..3 {
        assert_eq!(header_sort_state(visual, column), Some("none"), "column {column}");
    }
    app.update_in(visual, |this, window, cx| this.sort_column_page(this.active_id(), 1, true, window, cx));
    settle(visual);
    assert_eq!(header_sort_state(visual, 1), Some("page-desc"));
    assert_eq!(header_sort_state(visual, 2), Some("none"));
}

#[gpui::test]
fn header_sort_menu_sorts_the_table_in_the_database_and_remove_sort_restores_it(
    cx: &mut TestAppContext,
) {
    let (app, visual, tab_id) = sql_table_app(cx);
    visual.update(|window, _| window.activate_window());
    settle(visual);
    let generation = header_generation(&app, visual, tab_id);
    open_header_sort_menu(visual, 2);
    assert_eq!(open_sort_menu_column(&app, visual), Some(2), "the menu opened for this column");
    assert_eq!(
        header_generation(&app, visual, tab_id),
        generation,
        "opening the menu runs nothing"
    );
    // Sort Database Ascending is the first enabled entry, Descending the second.
    pick_header_sort_entry(visual, 2);
    app.read_with(visual, |this, _| {
        assert_eq!(this.workspace.active_tab().unwrap().applied_order_by, "\"status\" DESC");
    });
    assert!(header_generation(&app, visual, tab_id) > generation, "the query re-ran");
    assert_eq!(open_sort_menu_column(&app, visual), None, "choosing closes the menu");
    assert_eq!(header_sort_state(visual, 2), Some("database-desc"), "accent Z-A icon on the sorted column");
    assert_eq!(sql_table_first_id(&app, visual), "1", "NULL status rows come first descending");
    // The sorted column keeps its button; reopening checks the chosen entry.
    visual.simulate_mouse_move(gpui::point(gpui::px(2.0), gpui::px(2.0)), None, Default::default());
    settle(visual);
    assert_eq!(header_sort_state(visual, 2), Some("database-desc"));
    let sort = app.read_with(visual, |this, _| this.header_sort(this.active_id()));
    assert_eq!(sort.database, Some(("status".to_string(), true)));
    open_header_sort_menu(visual, 2);
    assert_eq!(open_sort_menu_column(&app, visual), Some(2));
    // Clear Sort: enabled now, after the four sort entries.
    pick_header_sort_entry(visual, 5);
    app.read_with(visual, |this, _| {
        assert_eq!(this.workspace.active_tab().unwrap().applied_order_by, "");
    });
    assert_eq!(header_sort_state(visual, 2), Some("none"), "back to the neutral chevron");
}

#[gpui::test]
fn header_sort_menu_sorts_the_loaded_rows_without_a_query_and_remove_sort_restores_them(
    cx: &mut TestAppContext,
) {
    let (app, visual) = context_test_app(cx, "result-grid", "sample");
    visual.update(|window, _| {
        window.activate_window();
        window.refresh();
    });
    settle(visual);
    let tab_id = app.read_with(visual, |this, _| this.active_id());
    let generation = header_generation(&app, visual, tab_id);
    open_header_sort_menu(visual, 0);
    // Sort Current Page Ascending is the third enabled entry, Descending the fourth.
    pick_header_sort_entry(visual, 4);
    app.read_with(visual, |this, _| {
        assert_ne!(this.active_grid().source_row(0), 0, "the loaded rows are reordered");
        assert_eq!(this.active_grid().page_sort(), Some((0, true)));
        assert!(context_runtime(this).context_sql_sort.is_none(), "no database sort");
    });
    assert_eq!(header_generation(&app, visual, tab_id), generation, "no query ran");
    assert_eq!(header_sort_state(visual, 0), Some("page-desc"), "muted Z-A icon");
    // Reopen: no database entry is checked, Clear Sort is enabled.
    open_header_sort_menu(visual, 0);
    pick_header_sort_entry(visual, 5);
    app.read_with(visual, |this, _| {
        assert_eq!(this.active_grid().source_row(0), 0, "source order is back");
        assert_eq!(this.active_grid().page_sort(), None);
    });
    assert_eq!(header_sort_state(visual, 0), Some("none"));
}

#[gpui::test]
fn header_sort_menu_sorts_a_custom_sql_result_in_the_database(cx: &mut TestAppContext) {
    let (app, visual) = context_test_app(cx, "result-grid", "sample");
    visual.update(|window, _| {
        window.activate_window();
        window.refresh();
    });
    settle(visual);
    let tab_id = app.read_with(visual, |this, _| this.active_id());
    let name = column_named(&app, visual, "name");
    let generation = header_generation(&app, visual, tab_id);
    open_header_sort_menu(visual, name);
    pick_header_sort_entry(visual, 2);
    app.read_with(visual, |this, _| {
        let sort = context_runtime(this).context_sql_sort.as_ref().expect("database sort");
        assert!(sort.sorted.contains("ORDER BY \"name\" DESC"), "{}", sort.sorted);
        assert_eq!((sort.column.as_str(), sort.descending), ("name", true));
    });
    assert!(header_generation(&app, visual, tab_id) > generation, "the query re-ran");
    assert_eq!(header_sort_state(visual, name), Some("database-desc"));
    assert_eq!(header_sort_state(visual, 0), Some("none"));
}

#[gpui::test]
fn header_sort_menu_writes_a_mongo_sort_document_and_the_button_follows_it(
    cx: &mut TestAppContext,
) {
    let (app, visual) = context_test_app(cx, "mongo-grid", "sample");
    visual.update(|window, _| {
        window.activate_window();
        window.refresh();
    });
    settle(visual);
    let tab_id = app.read_with(visual, |this, _| this.active_id());
    let name = column_named(&app, visual, "name");
    open_header_sort_menu(visual, name);
    pick_header_sort_entry(visual, 2);
    app.read_with(visual, |this, _| {
        assert_eq!(this.workspace.runtime(tab_id).unwrap().mongo.sort, r#"{"name":-1}"#);
    });
    assert_eq!(header_sort_state(visual, name), Some("database-desc"));
    open_header_sort_menu(visual, name);
    // Clear Sort sits after the four sort entries.
    pick_header_sort_entry(visual, 5);
    app.read_with(visual, |this, _| {
        assert_eq!(this.workspace.runtime(tab_id).unwrap().mongo.sort, "");
    });
    assert_eq!(header_sort_state(visual, name), Some("none"));
}

#[gpui::test]
fn header_sort_database_entries_are_disabled_when_the_result_cannot_sort_in_the_database(cx: &mut TestAppContext) {
    let (app, visual) = context_test_app(cx, "result-grid", "sample");
    visual.update(|window, _| {
        window.activate_window();
        window.refresh();
    });
    app.update(visual, |this, _| {
        let id = this.active_id();
        this.workspace.tab_mut(id).unwrap().query_mode = None;
    });
    settle(visual);
    let tab_id = app.read_with(visual, |this, _| this.active_id());
    let generation = header_generation(&app, visual, tab_id);
    open_header_sort_menu(visual, 0);
    // Both database entries are skipped: the first enabled entry is the page A → Z, the
    // second the page Z → A.
    pick_header_sort_entry(visual, 2);
    app.read_with(visual, |this, _| {
        assert_eq!(this.active_grid().page_sort(), Some((0, true)));
        assert!(context_runtime(this).context_sql_sort.is_none());
    });
    assert_eq!(header_generation(&app, visual, tab_id), generation);
    assert_eq!(header_sort_state(visual, 0), Some("page-desc"));
}

#[gpui::test]
fn header_sort_menu_dismisses_on_escape_and_click_outside_without_sorting(cx: &mut TestAppContext) {
    let (app, visual, tab_id) = sql_table_app(cx);
    visual.update(|window, _| window.activate_window());
    settle(visual);
    let generation = header_generation(&app, visual, tab_id);
    open_header_sort_menu(visual, 2);
    assert_eq!(open_sort_menu_column(&app, visual), Some(2));
    visual.simulate_keystrokes("escape");
    settle(visual);
    assert_eq!(open_sort_menu_column(&app, visual), None, "Escape closes it");

    open_header_sort_menu(visual, 2);
    assert_eq!(open_sort_menu_column(&app, visual), Some(2));
    let body = visual.debug_bounds("grid-body-cell-0-0").expect("a body cell");
    visual.simulate_click(body.center(), Default::default());
    settle(visual);
    assert_eq!(open_sort_menu_column(&app, visual), None, "a click outside closes it");
    app.read_with(visual, |this, _| {
        let tab = this.workspace.active_tab().unwrap();
        assert_eq!(tab.applied_order_by, "");
        assert_eq!(this.active_grid().page_sort(), None);
    });
    assert_eq!(header_generation(&app, visual, tab_id), generation);
}

#[gpui::test]
fn header_sort_button_click_neither_header_sorts_nor_resizes(
    cx: &mut TestAppContext,
) {
    let (app, visual, tab_id) = sql_table_app(cx);
    visual.update(|window, _| window.activate_window());
    settle(visual);
    let generation = header_generation(&app, visual, tab_id);
    let width = visual.debug_bounds("grid-header-cell-2").unwrap().size.width;
    open_header_sort_menu(visual, 2);
    app.read_with(visual, |this, _| {
        assert_eq!(
            this.workspace.active_tab().unwrap().applied_order_by,
            "",
            "the header's own click-to-sort did not fire"
        );
        assert!(this.av().grid_ui.column_resize.is_none(), "no resize started");
    });
    assert_eq!(header_generation(&app, visual, tab_id), generation);
    assert_eq!(open_sort_menu_column(&app, visual), Some(2));
    assert_eq!(visual.debug_bounds("grid-header-cell-2").unwrap().size.width, width);
}
