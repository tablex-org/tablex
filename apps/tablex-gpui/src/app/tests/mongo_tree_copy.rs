// The Mongo Document view's Tree: selecting a row, copying with Cmd+C, the
// right-click menu and selecting text inside one value, all through real input
// on the Mongo fixture (document 1).

fn tree_app(
    cx: &mut TestAppContext,
) -> (gpui::Entity<AppRoot>, &mut gpui::VisualTestContext, u64) {
    let (app, visual, tab_id) = document_edit_app(cx);
    app.update(visual, |this, cx| this.toggle_mongo_document_view(tab_id, cx));
    settle(visual);
    (app, visual, tab_id)
}

fn tree_selected(app: &gpui::Entity<AppRoot>, visual: &mut gpui::VisualTestContext, tab_id: u64) -> Option<String> {
    app.read_with(visual, |this, _| {
        this.workspace.runtime(tab_id).unwrap().mongo.inspector.selected_path.clone()
    })
}

fn tree_expanded(app: &gpui::Entity<AppRoot>, visual: &mut gpui::VisualTestContext, tab_id: u64, path: &str) -> bool {
    app.read_with(visual, |this, _| {
        this.workspace.runtime(tab_id).unwrap().mongo.inspector.expanded_paths.contains(path)
    })
}

/// The visible row index of the row at `path`.
fn tree_row(app: &gpui::Entity<AppRoot>, visual: &mut gpui::VisualTestContext, tab_id: u64, path: &str) -> usize {
    app.read_with(visual, |this, _| {
        let inspector = &this.workspace.runtime(tab_id).unwrap().mongo.inspector;
        let document = &inspector.snapshot.as_ref().unwrap().document;
        crate::mongo_result::flatten_tree(document, &inspector.expanded_paths)
            .iter()
            .position(|row| row.path == path)
            .unwrap_or_else(|| panic!("{path} is a visible row"))
    })
}

fn tree_selector(prefix: &str, index: usize) -> &'static str {
    Box::leak(format!("{prefix}-{index}").into_boxed_str())
}

/// Clicks the row's key area (left of the value text).
fn click_tree_row(app: &gpui::Entity<AppRoot>, visual: &mut gpui::VisualTestContext, tab_id: u64, path: &str) {
    let index = tree_row(app, visual, tab_id, path);
    let bounds = visual
        .debug_bounds(tree_selector("mongo-tree-row", index))
        .unwrap_or_else(|| panic!("row {path} is laid out"));
    visual.simulate_click(bounds.origin + gpui::point(gpui::px(30.0), bounds.size.height / 2.0), Default::default());
    settle(visual);
}

fn clipboard_text(visual: &mut gpui::VisualTestContext) -> String {
    visual.cx.read_from_clipboard().and_then(|item| item.text()).unwrap_or_default()
}

fn tree_document(app: &gpui::Entity<AppRoot>, visual: &mut gpui::VisualTestContext, tab_id: u64) -> serde_json::Value {
    app.read_with(visual, |this, _| {
        this.workspace.runtime(tab_id).unwrap().mongo.inspector.snapshot.as_ref().unwrap().document.clone()
    })
}

fn expand_tree_path(app: &gpui::Entity<AppRoot>, visual: &mut gpui::VisualTestContext, tab_id: u64, path: &str) {
    click_tree_row(app, visual, tab_id, path);
    assert!(tree_expanded(app, visual, tab_id, path), "{path} opens");
}

#[gpui::test]
fn clicking_a_tree_row_selects_it_and_a_container_also_toggles(cx: &mut TestAppContext) {
    let (app, visual, tab_id) = tree_app(cx);
    assert_eq!(tree_selected(&app, visual, tab_id), None);
    click_tree_row(&app, visual, tab_id, "/name");
    assert_eq!(tree_selected(&app, visual, tab_id).as_deref(), Some("/name"));
    assert!(!tree_expanded(&app, visual, tab_id, "/name"));

    click_tree_row(&app, visual, tab_id, "/items");
    assert_eq!(tree_selected(&app, visual, tab_id).as_deref(), Some("/items"));
    assert!(tree_expanded(&app, visual, tab_id, "/items"), "a container row opens on click");
    click_tree_row(&app, visual, tab_id, "/items");
    assert!(!tree_expanded(&app, visual, tab_id, "/items"), "and closes on the next");
    assert_eq!(tree_selected(&app, visual, tab_id).as_deref(), Some("/items"));

    // Up / Down move the selection through the visible rows.
    click_tree_row(&app, visual, tab_id, "/name");
    visual.simulate_keystrokes("down");
    assert_eq!(tree_selected(&app, visual, tab_id).as_deref(), Some("/nullable"));
    visual.simulate_keystrokes("up up");
    assert_eq!(tree_selected(&app, visual, tab_id).as_deref(), Some("/_id"));
    visual.simulate_keystrokes("up");
    assert_eq!(tree_selected(&app, visual, tab_id).as_deref(), Some("/_id"), "stops at the first row");
}

#[gpui::test]
fn changing_the_document_clears_the_tree_selection(cx: &mut TestAppContext) {
    let (app, visual, tab_id) = tree_app(cx);
    click_tree_row(&app, visual, tab_id, "/name");
    assert!(tree_selected(&app, visual, tab_id).is_some());
    app.update(visual, |this, cx| this.select_mongo_document(tab_id, 2, cx));
    settle(visual);
    assert_eq!(tree_selected(&app, visual, tab_id), None);
}

#[gpui::test]
fn cmd_c_copies_the_selected_tree_rows_value(cx: &mut TestAppContext) {
    let (app, visual, tab_id) = tree_app(cx);
    let document = tree_document(&app, visual, tab_id);
    let copied = |visual: &mut gpui::VisualTestContext, path: &str| {
        visual.cx.write_to_clipboard(gpui::ClipboardItem::new_string("before".into()));
        click_tree_row(&app, visual, tab_id, path);
        visual.simulate_keystrokes("cmd-c");
        clipboard_text(visual)
    };
    assert_eq!(copied(visual, "/name"), "mongo-customer-001", "a string without its quotes");
    assert_eq!(copied(visual, "/_id"), "000000000000000000000002", "an ObjectId as its hex");
    assert_eq!(copied(visual, "/active"), "false");
    let (_, created) = crate::mongo_result::ejson_scalar(&document["createdAt"]).unwrap();
    assert_eq!(created, "date_time");
    assert_eq!(copied(visual, "/createdAt"), crate::mongo_result::ejson_scalar(&document["createdAt"]).unwrap().0);
    assert!(copied(visual, "/createdAt").starts_with("2024-09-02T00:00:00"), "a date as ISO text");
    assert_eq!(copied(visual, "/count"), crate::mongo_result::ejson_scalar(&document["count"]).map(|(text, _)| text).unwrap_or_else(|| document["count"].to_string()));
    // A container copies its whole subtree (collapsed or not) as the JSON view writes it.
    let subtree = crate::cell_value::shell_text(&document["nested"], true).unwrap();
    assert_eq!(copied(visual, "/nested"), subtree);
    assert!(subtree.contains("owner-001"), "{subtree}");
}

#[gpui::test]
fn the_tree_context_menu_copies_the_row(cx: &mut TestAppContext) {
    let (app, visual, tab_id) = tree_app(cx);
    let document = tree_document(&app, visual, tab_id);
    expand_tree_path(&app, visual, tab_id, "/items");
    expand_tree_path(&app, visual, tab_id, "/items/2");
    let path = "/items/2/sku";
    // Item `n` of the menu: Copy Value, Copy Field Name, Copy Path, Copy as JSON.
    let menu_copy = |visual: &mut gpui::VisualTestContext, item: usize| {
        visual.cx.write_to_clipboard(gpui::ClipboardItem::new_string("before".into()));
        let index = tree_row(&app, visual, tab_id, path);
        let bounds = visual.debug_bounds(tree_selector("mongo-tree-row", index)).expect("row is laid out");
        visual.simulate_mouse_down(bounds.center(), gpui::MouseButton::Right, Default::default());
        visual.simulate_mouse_up(bounds.center(), gpui::MouseButton::Right, Default::default());
        visual.run_until_parked();
        assert_eq!(tree_selected(&app, visual, tab_id).as_deref(), Some(path), "a right-click selects the row");
        for _ in 0..item {
            visual.simulate_keystrokes("down");
        }
        visual.simulate_keystrokes("enter");
        visual.run_until_parked();
        clipboard_text(visual)
    };
    assert_eq!(menu_copy(visual, 1), "SKU-001");
    assert_eq!(menu_copy(visual, 2), "sku");
    assert_eq!(menu_copy(visual, 3), "items.2.sku", "array items by index");
    let json = menu_copy(visual, 4);
    let expected = crate::cell_value::shell_text(&serde_json::json!({"sku": document["items"][2]["sku"]}), true).unwrap();
    assert_eq!(json, expected);
    assert!(json.contains("\"sku\"") && json.contains("SKU-001"), "{json}");
    // A container's JSON is its whole subtree under its field name.
    click_tree_row(&app, visual, tab_id, "/items/3");
    let index = tree_row(&app, visual, tab_id, "/items/3");
    let bounds = visual.debug_bounds(tree_selector("mongo-tree-row", index)).unwrap();
    visual.cx.write_to_clipboard(gpui::ClipboardItem::new_string("before".into()));
    visual.simulate_mouse_down(bounds.center(), gpui::MouseButton::Right, Default::default());
    visual.simulate_mouse_up(bounds.center(), gpui::MouseButton::Right, Default::default());
    visual.run_until_parked();
    for _ in 0..4 {
        visual.simulate_keystrokes("down");
    }
    visual.simulate_keystrokes("enter");
    visual.run_until_parked();
    let expected = crate::cell_value::shell_text(&serde_json::json!({"3": document["items"][3]}), true).unwrap();
    assert_eq!(clipboard_text(visual), expected);
}

#[gpui::test]
fn a_right_click_below_the_rows_opens_no_menu(cx: &mut TestAppContext) {
    let (app, visual, tab_id) = tree_app(cx);
    click_tree_row(&app, visual, tab_id, "/name");
    let last = tree_row(&app, visual, tab_id, "/longText");
    let bounds = visual.debug_bounds(tree_selector("mongo-tree-row", last)).unwrap();
    let blank = bounds.origin + gpui::point(gpui::px(40.0), bounds.size.height + gpui::px(60.0));
    visual.cx.write_to_clipboard(gpui::ClipboardItem::new_string("before".into()));
    visual.simulate_mouse_down(blank, gpui::MouseButton::Right, Default::default());
    visual.simulate_mouse_up(blank, gpui::MouseButton::Right, Default::default());
    visual.run_until_parked();
    // With no menu, Down moves the row selection and Enter does nothing.
    visual.simulate_keystrokes("down enter");
    assert_eq!(clipboard_text(visual), "before");
    assert_eq!(tree_selected(&app, visual, tab_id).as_deref(), Some("/nullable"));
}

/// Drags from `from` to `to` with the left button.
fn drag_select(visual: &mut gpui::VisualTestContext, from: gpui::Point<gpui::Pixels>, to: gpui::Point<gpui::Pixels>) {
    visual.simulate_mouse_down(from, gpui::MouseButton::Left, Default::default());
    visual.simulate_mouse_move(to, gpui::MouseButton::Left, Default::default());
    visual.simulate_mouse_up(to, gpui::MouseButton::Left, Default::default());
    settle(visual);
}

/// The laid-out text of the row at `index`.
fn tree_text_bounds(visual: &mut gpui::VisualTestContext, index: usize) -> gpui::Bounds<gpui::Pixels> {
    visual.debug_bounds(tree_selector("mongo-tree-value", index)).expect("the row text is laid out")
}

#[gpui::test]
fn dragging_inside_a_value_selects_its_text_without_toggling_the_row(cx: &mut TestAppContext) {
    let (app, visual, tab_id) = tree_app(cx);
    let index = tree_row(&app, visual, tab_id, "/name");
    let text = tree_text_bounds(visual, index);
    let y = text.origin.y + text.size.height / 2.0;
    visual.cx.write_to_clipboard(gpui::ClipboardItem::new_string("before".into()));
    drag_select(visual, gpui::point(text.origin.x + gpui::px(1.0), y), gpui::point(text.origin.x + text.size.width / 2.0, y));
    // The press selects the row (the highlight follows the pointer), the release does not toggle.
    assert_eq!(tree_selected(&app, visual, tab_id).as_deref(), Some("/name"));
    visual.simulate_keystrokes("cmd-c");
    let copied = clipboard_text(visual);
    let shown = "name: \"mongo-customer-001\"";
    assert!(!copied.is_empty() && shown.starts_with(&copied) && copied.len() < shown.len(), "part of the line: {copied:?}");

    // The same drag on a container's text leaves it closed.
    let index = tree_row(&app, visual, tab_id, "/nested");
    let text = tree_text_bounds(visual, index);
    let y = text.origin.y + text.size.height / 2.0;
    drag_select(visual, gpui::point(text.origin.x + gpui::px(1.0), y), gpui::point(text.origin.x + text.size.width / 2.0, y));
    assert!(!tree_expanded(&app, visual, tab_id, "/nested"));

    // A plain click on the text still selects the row and opens a container.
    visual.simulate_click(text.center(), Default::default());
    settle(visual);
    assert_eq!(tree_selected(&app, visual, tab_id).as_deref(), Some("/nested"));
    assert!(tree_expanded(&app, visual, tab_id, "/nested"));
}

#[gpui::test]
fn a_click_in_the_empty_part_of_a_row_selects_it(cx: &mut TestAppContext) {
    let (app, visual, tab_id) = tree_app(cx);
    let panel = visual.debug_bounds("mongo-document-panel").expect("the panel is laid out");
    for path in ["/name", "/nested"] {
        let index = tree_row(&app, visual, tab_id, path);
        let row = visual.debug_bounds(tree_selector("mongo-tree-row", index)).unwrap();
        let text = tree_text_bounds(visual, index);
        assert!(row.right() >= panel.right() - gpui::px(1.0), "{path}: the row spans the panel: {row:?} / {panel:?}");
        let blank = gpui::point(text.right() + gpui::px(150.0), row.center().y);
        assert!(blank.x < panel.right(), "the probe point is inside the panel");
        visual.simulate_click(blank, Default::default());
        settle(visual);
        assert_eq!(tree_selected(&app, visual, tab_id).as_deref(), Some(path));
    }
    assert!(tree_expanded(&app, visual, tab_id, "/nested"), "a click on the blank part of a container row opens it too");
}

#[gpui::test]
fn dragging_across_rows_copies_the_lines_as_the_tree_shows_them(cx: &mut TestAppContext) {
    let (app, visual, tab_id) = tree_app(cx);
    let first = tree_row(&app, visual, tab_id, "/name");
    let first = tree_text_bounds(visual, first);
    let last = tree_row(&app, visual, tab_id, "/active");
    let last = tree_text_bounds(visual, last);
    visual.cx.write_to_clipboard(gpui::ClipboardItem::new_string("before".into()));
    drag_select(
        visual,
        gpui::point(first.origin.x + gpui::px(1.0), first.center().y),
        gpui::point(last.origin.x + last.size.width / 2.0, last.center().y),
    );
    visual.simulate_keystrokes("cmd-c");
    let copied = clipboard_text(visual);
    let lines: Vec<&str> = copied.lines().collect();
    assert_eq!(lines.len(), 3, "{copied:?}");
    assert_eq!(lines[0], "name: \"mongo-customer-001\"");
    assert_eq!(lines[1], "nullable: \"present\"");
    assert!(!lines[2].is_empty() && "active: false".starts_with(lines[2]) && lines[2].len() < "active: false".len(), "{copied:?}");
}

/// A tree of `items` x 4 fields, all open: 2001 rows.
fn seed_big_tree(app: &gpui::Entity<AppRoot>, visual: &mut gpui::VisualTestContext, tab_id: u64) -> usize {
    let items: Vec<serde_json::Value> = (0..400)
        .map(|n| serde_json::json!({"sku": format!("SKU-{n:04}"), "qty": n, "ok": n % 2 == 0, "note": "some text"}))
        .collect();
    app.update(visual, |this, cx| {
        let inspector = &mut this.workspace.runtime_mut(tab_id).unwrap().mongo.inspector;
        let key = inspector.selected.unwrap();
        inspector.expanded_paths.insert("/items".into());
        (0..400).for_each(|n| {
            inspector.expanded_paths.insert(format!("/items/{n}"));
        });
        inspector.snapshot = Some(std::sync::Arc::new(crate::mongo_result::MongoDocumentSnapshot::new(
            key,
            serde_json::json!({"items": items}),
        )));
        cx.notify();
    });
    settle(visual);
    2001
}

#[gpui::test]
fn a_text_selection_survives_scrolling_while_its_rows_stay_in_view(cx: &mut TestAppContext) {
    let (app, visual, tab_id) = tree_app(cx);
    seed_big_tree(&app, visual, tab_id);
    let a = tree_text_bounds(visual, 2);
    let b = tree_text_bounds(visual, 4);
    drag_select(visual, gpui::point(a.origin.x + gpui::px(1.0), a.center().y), gpui::point(b.right() - gpui::px(1.0), b.center().y));
    visual.simulate_keystrokes("cmd-c");
    let before = clipboard_text(visual);
    assert_eq!(before.lines().count(), 3, "{before:?}");
    assert!(before.contains("sku: \"SKU-0000\"") && before.contains("qty: 0") && before.contains("ok: true"), "{before:?}");

    // Row 0 scrolls out of view; the selected rows move up but stay in view.
    let y_before = tree_text_bounds(visual, 2).origin.y;
    app.update(visual, |this, cx| {
        this.view(tab_id).mongo_detail_scroll.scroll_to_item_strict(1, gpui::ScrollStrategy::Top);
        this.notify_tab(tab_id, cx);
    });
    settle(visual);
    assert!(tree_text_bounds(visual, 2).origin.y < y_before, "the rows moved up");
    visual.cx.write_to_clipboard(gpui::ClipboardItem::new_string("cleared".into()));
    visual.simulate_keystrokes("cmd-c");
    assert_eq!(clipboard_text(visual), before, "the same lines are still selected after the scroll");
    app.update(visual, |this, cx| {
        this.view(tab_id).mongo_detail_scroll.scroll_to_item_strict(300, gpui::ScrollStrategy::Top);
        this.notify_tab(tab_id, cx);
    });
    settle(visual);
    visual.cx.write_to_clipboard(gpui::ClipboardItem::new_string("cleared".into()));
    visual.simulate_keystrokes("cmd-c");
    // Scrolled so far that the selection's end rows are gone, the text selection ends (a
    // per-row participant leaves with its row) and Cmd+C copies the selected row's value.
    assert_eq!(clipboard_text(visual), "SKU-0000");
}
