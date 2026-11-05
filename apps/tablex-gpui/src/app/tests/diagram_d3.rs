// D3 (structure) interactions of the ER diagram, driven through the real
// events: edit a table in the Inspector, the pending badge and Changes
// counter, the review dialog with the SQL, Apply (confirmations, read-only),
// drafts that survive a reopen, undo / redo of draft edits, "New table", and
// "Remove from diagram" against "Drop table...".

use crate::app::structure_edit::EditScope;
use crate::app::results::diagram::view::DiagramEvent;
use crate::app::structure_edit::EditContext;
use crate::sql_ddl::Change;

fn d3_context(disabled: Option<&str>, confirm: bool) -> EditContext {
    EditContext {
        scope: EditScope { kind: ConnectionKind::Postgres, database: "db".into(), schema: Some("public".into()) },
        subject: "Production \u{00B7} shop".into(),
        confirm_execute: confirm,
        disabled: disabled.map(str::to_owned),
        sql_mode: None,
        catalog: None,
        catalog_error: None,
    }
}

/// The `Apply` events a diagram window recorded, in order.
type AppliedChanges = std::rc::Rc<std::cell::RefCell<Vec<Vec<Change>>>>;

/// The D2 window (five tables) made editable, with the Apply events recorded.
fn d3_window_with(
    cx: &mut TestAppContext,
    context: EditContext,
) -> (gpui::Entity<DiagramView>, &mut gpui::VisualTestContext, AppliedChanges) {
    let (view, visual) = d2_window(cx);
    let applied = std::rc::Rc::new(std::cell::RefCell::new(Vec::new()));
    let sink = applied.clone();
    visual.update(|_, app| {
        app.subscribe(&view, move |_, event: &DiagramEvent, _| {
            if let DiagramEvent::Apply(request) = event {
                sink.borrow_mut().push(request.changes.clone());
            }
        })
        .detach();
    });
    view.update(visual, |view, cx| view.set_edit_context(Some(context), cx));
    settle(visual);
    (view, visual, applied)
}

fn d3_window(cx: &mut TestAppContext) -> (gpui::Entity<DiagramView>, &mut gpui::VisualTestContext) {
    let (view, visual, _) = d3_window_with(cx, d3_context(None, false));
    (view, visual)
}

/// Opens the Inspector and selects `table` with a click on its box.
fn d3_select(view: &gpui::Entity<DiagramView>, visual: &mut gpui::VisualTestContext, table: &str) -> usize {
    if view.read_with(visual, |view, _| view.panel) != Some(Panel::Inspector) {
        view.update(visual, |view, cx| view.toggle_panel(Panel::Inspector, cx));
        settle(visual);
    }
    let index = d2_entity(view, visual, table);
    let selector: &'static str = Box::leak(format!("diagram-entity-{index}").into_boxed_str());
    let bounds = visual.debug_bounds(selector).unwrap_or_else(|| panic!("{table} is drawn"));
    visual.simulate_click(bounds.origin + gpui::point(gpui::px(30.0), gpui::px(8.0)), Default::default());
    settle(visual);
    index
}

/// Scrolls the dock until `selector` is inside it (a long table's rows and
/// editors do not all fit).
fn d3_reveal(visual: &mut gpui::VisualTestContext, selector: &'static str) -> gpui::Bounds<gpui::Pixels> {
    let dock = visual.debug_bounds("diagram-dock").expect("the dock is open");
    for _ in 0..40 {
        let bounds = visual.debug_bounds(selector).unwrap_or_else(|| panic!("{selector} is rendered"));
        let (top, bottom) = (dock.top() + gpui::px(40.0), dock.bottom() - gpui::px(10.0));
        let delta = if bounds.center().y < top {
            120.0
        } else if bounds.center().y > bottom {
            -120.0
        } else {
            return bounds;
        };
        visual.simulate_event(gpui::ScrollEvent {
            position: dock.center(),
            delta: gpui::ScrollDelta::Pixels(gpui::point(gpui::px(0.0), gpui::px(delta))),
            modifiers: Default::default(),
            touch_phase: gpui::TouchPhase::Moved,
            source: gpui::ScrollSource::Pointer,
        });
        settle(visual);
    }
    panic!("{selector} cannot be scrolled into the dock");
}

/// Clicks `selector` after scrolling it into the dock.
fn d3_click(visual: &mut gpui::VisualTestContext, selector: &'static str) {
    let bounds = d3_reveal(visual, selector);
    visual.simulate_click(bounds.center(), Default::default());
    settle(visual);
}

/// Focuses the input in `selector`, replaces its text and presses Enter.
fn d3_type(visual: &mut gpui::VisualTestContext, selector: &'static str, text: &str) {
    let bounds = d3_reveal(visual, selector);
    visual.simulate_click(bounds.center(), Default::default());
    visual.simulate_keystrokes("secondary-a");
    visual.simulate_input(text);
    visual.simulate_keystrokes("enter");
    settle(visual);
}

/// A selector built at run time.
fn d3_sel(text: String) -> &'static str {
    Box::leak(text.into_boxed_str())
}

fn d3_titles(view: &gpui::Entity<DiagramView>, visual: &mut gpui::VisualTestContext) -> Vec<String> {
    view.read_with(visual, |view, _| view.session.draft.changes.iter().map(Change::title).collect())
}

#[gpui::test]
fn the_inspector_edits_a_table_into_a_pending_change_set(cx: &mut TestAppContext) {
    let (view, visual) = d3_window(cx);
    let orders = d3_select(&view, visual, "orders");
    for selector in ["inspector-table-name", "inspector-add-column", "inspector-drop-table", "inspector-remove-table", "inspector-add-index", "inspector-add-fk"] {
        assert!(visual.debug_bounds(selector).is_some(), "{selector} is offered");
    }
    assert!(visual.debug_bounds("diagram-changes").is_some(), "the Changes button is in the toolbar");
    assert!(visual.debug_bounds(d3_sel(format!("diagram-pending-{orders}"))).is_none(), "nothing is pending yet");

    d3_type(visual, "inspector-table-name", "purchases");
    assert_eq!(d3_titles(&view, visual), ["Rename table public.orders to purchases"]);
    assert_eq!(view.read_with(visual, |view, _| view.changes_label()), "Changes (1)");
    assert!(visual.debug_bounds("diagram-pending-2").is_some(), "the renamed table carries the pending badge");
    view.read_with(visual, |view, _| {
        let entity = &view.snapshot.as_ref().unwrap().graph.entities[orders];
        assert_eq!((entity.label.as_str(), entity.table.table.as_str()), ("purchases", "orders"), "label shows the new name, identity stays");
    });

    // A new column: selected, named, typed, made NOT NULL.
    diagram_click(visual, "inspector-add-column");
    d3_type(visual, "inspector-field-name", "shipped_at");
    d3_type(visual, "inspector-field-type", "timestamptz");
    d3_type(visual, "inspector-field-default", "now()");
    d3_type(visual, "inspector-field-comment", "when it left");
    diagram_click(visual, "inspector-nullable");
    let titles = d3_titles(&view, visual);
    assert_eq!(titles, ["Rename table public.orders to purchases", "Add column public.purchases.shipped_at"]);
    assert_eq!(view.read_with(visual, |view, _| view.changes_label()), "Changes (2)");
    view.read_with(visual, |view, _| {
        let Change::AddColumn { column, .. } = &view.session.draft.changes[1] else { panic!("an added column") };
        assert_eq!(column.name, "shipped_at");
        assert_eq!(column.data_type, "timestamptz");
        assert_eq!(column.default.as_deref(), Some("now()"));
        assert_eq!(column.comment.as_deref(), Some("when it left"));
        assert!(!column.nullable);
    });
}

/// Renames `orders`, adds a commented column and drops `orders.note`.
fn d3_edit_orders(view: &gpui::Entity<DiagramView>, visual: &mut gpui::VisualTestContext) {
    d3_select(view, visual, "orders");
    d3_type(visual, "inspector-table-name", "purchases");
    diagram_click(visual, "inspector-add-column");
    d3_type(visual, "inspector-field-name", "shipped_at");
    d3_type(visual, "inspector-field-type", "timestamptz");
    d3_type(visual, "inspector-field-comment", "when it left");
}

/// Clicks Copy SQL and returns what it put on the clipboard (the test
/// platform's clipboard belongs to this test's app context).
fn d3_copy_sql(visual: &mut gpui::VisualTestContext) -> String {
    diagram_click(visual, "changes-copy");
    visual.read_from_clipboard().and_then(|item| item.text()).unwrap_or_default()
}

#[gpui::test]
fn the_changes_dialog_lists_every_change_with_its_sql_and_copies_the_script(cx: &mut TestAppContext) {
    let (view, visual) = d3_window(cx);
    assert!(visual.debug_bounds("diagram-changes").is_some());
    d3_edit_orders(&view, visual);
    open_review(visual, "diagram-changes");
    assert!(visual.debug_bounds("changes-subject").is_some(), "the dialog is open");
    assert!(visual.debug_bounds("changes-mode").is_some(), "PostgreSQL: the transaction note");
    // Execution order: the rename first, then the column and its comment.
    for selector in ["changes-item-0", "changes-item-1", "changes-sql-0-0", "changes-sql-1-0", "changes-sql-1-1"] {
        assert!(visual.debug_bounds(selector).is_some(), "{selector}");
    }
    assert!(visual.debug_bounds("changes-sql-1-2").is_none());
    assert!(visual.debug_bounds("changes-warning-0").is_some(), "a rename carries a caution");
    assert!(visual.debug_bounds("changes-understood").is_none(), "nothing destructive yet");
    assert!(visual.debug_bounds("changes-confirm").is_none(), "this connection does not ask");

    assert_eq!(
        d3_copy_sql(visual),
        "ALTER TABLE \"public\".\"orders\" RENAME TO \"purchases\";\n\
         ALTER TABLE \"public\".\"purchases\" ADD COLUMN \"shipped_at\" timestamptz;\n\
         COMMENT ON COLUMN \"public\".\"purchases\".\"shipped_at\" IS 'when it left';"
    );

    // Discard one change from the dialog: the list follows.
    diagram_click(visual, "changes-discard-1");
    assert_eq!(d3_titles(&view, visual), ["Rename table public.orders to purchases"]);
    assert!(visual.debug_bounds("changes-item-1").is_none() && visual.debug_bounds("changes-item-0").is_some());
    diagram_click(visual, "changes-discard-all");
    assert!(view.read_with(visual, |view, _| view.session.draft.is_empty()));
    assert!(visual.debug_bounds("changes-empty").is_some());
    diagram_click(visual, "changes-close");
    assert!(visual.debug_bounds("changes-subject").is_none(), "closed");
}

#[gpui::test]
fn apply_needs_the_acknowledgements_the_changes_and_the_connection_ask_for(cx: &mut TestAppContext) {
    let (view, visual, applied) = d3_window_with(cx, d3_context(None, true));
    d3_edit_orders(&view, visual);
    // Dropping a column is destructive.
    let (orders, note) = (d2_entity(&view, visual, "orders"), 3);
    view.update(visual, |view, cx| view.drop_column(orders, note, cx));
    settle(visual);
    open_review(visual, "diagram-changes");
    assert!(visual.debug_bounds("changes-understood").is_some(), "destructive: needs the acknowledgement");
    assert!(visual.debug_bounds("changes-confirm").is_some(), "the connection asks: needs the confirmation naming it");

    // Apply is disabled until both are ticked.
    diagram_click(visual, "changes-apply");
    assert!(applied.borrow().is_empty(), "nothing is sent without the boxes");
    diagram_click(visual, "changes-understood");
    diagram_click(visual, "changes-apply");
    assert!(applied.borrow().is_empty(), "the connection's confirmation is still missing");
    diagram_click(visual, "changes-confirm");
    diagram_click(visual, "changes-apply");
    let sent = applied.borrow().clone();
    assert_eq!(sent.len(), 1, "one Apply");
    assert_eq!(sent[0].len(), 3, "the rename, the new column and the drop");
    assert!(view.read_with(visual, |view, _| view.review_applying()), "the dialog waits for the answer");

    // The answer: the dialog stays open on a failure and shows what ran.
    view.update(visual, |view, cx| view.apply_finished(&[], &[], "Statement 2 of 3 failed: boom".into(), true, cx));
    settle(visual);
    assert!(visual.debug_bounds("changes-report").is_some());
    assert_eq!(view.read_with(visual, |view, _| view.session.draft.len()), 3, "nothing applied, the draft is intact");
    // A success removes the applied changes and closes the dialog.
    diagram_click(visual, "changes-understood");
    diagram_click(visual, "changes-confirm");
    diagram_click(visual, "changes-apply");
    assert_eq!(applied.borrow().len(), 2);
    view.update(visual, |view, cx| view.apply_finished(&[0, 1, 2], &[], "Applied 4 statements.".into(), false, cx));
    settle(visual);
    assert!(view.read_with(visual, |view, _| view.session.draft.is_empty()));
    assert!(visual.debug_bounds("changes-subject").is_none(), "the dialog closed itself");
}

#[gpui::test]
fn a_read_only_connection_disables_editing_with_the_reason(cx: &mut TestAppContext) {
    let reason = "Structure editing is blocked: this connection is read-only (Production).";
    let (view, visual, _) = d3_window_with(cx, d3_context(Some(reason), false));
    d3_select(&view, visual, "orders");
    assert!(visual.debug_bounds("inspector-readonly-reason").is_some(), "the reason is shown");
    for selector in ["inspector-table-name", "inspector-add-column", "inspector-drop-table", "inspector-add-index", "inspector-column-editor"] {
        assert!(visual.debug_bounds(selector).is_none(), "{selector} is not offered");
    }
    // The columns and indexes are still listed.
    assert!(visual.debug_bounds("inspector-column-0").is_some());
    assert!(!view.read_with(visual, |view, _| view.editable()));
    assert!(!view.update(visual, |view, cx| view.edit_draft(cx, |draft| draft.push(Change::DropTable { table: crate::sql_ddl::TableId::new(Some("public"), "orders"), cascade: false }))));
    assert!(view.read_with(visual, |view, _| view.session.draft.is_empty()));
    // New table is offered but disabled in the canvas menu: choosing it does nothing.
    let empty = d2_empty_point(&view, visual);
    d2_right_click(visual, empty);
    visual.simulate_keystrokes("down down enter");
    settle(visual);
    assert!(view.read_with(visual, |view, _| view.session.draft.is_empty()));
    // The review dialog explains why Apply is off.
    visual.simulate_keystrokes("escape");
    open_review(visual, "diagram-changes");
    assert!(visual.debug_bounds("changes-disabled").is_some());
}

/// Like [`d2_stored_window`], editable.
fn d3_stored_window<'a>(
    cx: &'a mut TestAppContext,
    dir: &std::path::Path,
) -> (gpui::Entity<DiagramView>, &'a mut gpui::VisualTestContext) {
    let (view, visual) = d2_window(cx);
    view.update(visual, |view, cx| view.set_edit_context(Some(d3_context(None, false)), cx));
    let store = std::sync::Arc::new(LayoutStore::new(dir.to_path_buf()));
    view.update(visual, |view, cx| view.set_persistence(Some((store, d2_key())), cx));
    settle(visual);
    (view, visual)
}

#[gpui::test]
fn pending_changes_survive_closing_and_reopening_the_diagram(cx: &mut TestAppContext) {
    cx.dispatcher.allow_parking();
    let dir = std::env::temp_dir().join(format!("tablex-d3-{}", uuid::Uuid::new_v4()));
    let (view, visual) = d3_stored_window(cx, &dir);
    wait_until(visual, "the (empty) document to be read", |visual| view.read_with(visual, |view, _| view.persisting()));
    d3_edit_orders(&view, visual);
    let empty = d2_empty_point(&view, visual);
    d2_right_click(visual, empty);
    visual.simulate_keystrokes("down down enter"); // Add note, New table
    settle(visual);
    let before = view.read_with(visual, |view, _| view.session.draft.changes.clone());
    assert_eq!(before.len(), 3, "rename, add column, create table: {before:?}");
    let files = |dir: &std::path::Path| std::fs::read_dir(dir).map(|entries| entries.count()).unwrap_or(0);
    visual.cx.executor().advance_clock(crate::app::results::diagram::view::SAVE_DEBOUNCE + std::time::Duration::from_millis(10));
    wait_until(visual, "the saved document", |_| files(&dir) == 1);

    // A fresh view of the same diagram reads the draft back and shows it.
    let (fresh, visual) = d3_stored_window(cx, &dir);
    wait_until(visual, "the draft to load", |visual| fresh.read_with(visual, |view, _| !view.session.draft.is_empty()));
    settle(visual);
    assert_eq!(fresh.read_with(visual, |view, _| view.session.draft.changes.clone()), before);
    fresh.read_with(visual, |view, _| {
        let graph = &view.snapshot.as_ref().unwrap().graph;
        assert_eq!(graph.entities.len(), 6, "the draft table is an entity again");
        assert_eq!(view.pending_count(), 3);
        assert_eq!(view.session.history.undo_len(), 0, "history is not persisted");
        let labels: Vec<&str> = graph.entities.iter().map(|e| e.label.as_str()).collect();
        assert!(labels.contains(&"purchases") && labels.contains(&"new_table"), "{labels:?}");
    });
    assert!(visual.debug_bounds("diagram-pending-2").is_some(), "the badge is back");
    // The pending draft table was placed where it was created, and that is saved too.
    let created = d2_entity(&fresh, visual, "new_table");
    assert!(visual.debug_bounds(d3_sel(format!("diagram-entity-{created}"))).is_some());

    // Discarding everything leaves nothing worth a file.
    fresh.update(visual, |view, cx| view.discard_all_changes(cx));
    visual.cx.executor().advance_clock(crate::app::results::diagram::view::SAVE_DEBOUNCE + std::time::Duration::from_millis(10));
    wait_until(visual, "the file to go", |_| files(&dir) == 0);
    std::fs::remove_dir_all(dir).ok();
}

#[gpui::test]
fn undo_and_redo_walk_the_draft_edits(cx: &mut TestAppContext) {
    let (view, visual) = d3_window(cx);
    d3_select(&view, visual, "orders");
    d3_type(visual, "inspector-table-name", "purchases");
    diagram_click(visual, "inspector-add-column");
    d3_type(visual, "inspector-field-name", "shipped_at");
    let added = |view: &gpui::Entity<DiagramView>, visual: &mut gpui::VisualTestContext| {
        view.read_with(visual, |view, _| {
            view.session.draft.changes.iter().find_map(|c| match c {
                Change::AddColumn { column, .. } => Some(column.name.clone()),
                _ => None,
            })
        })
    };
    assert_eq!(d3_titles(&view, visual).len(), 2);
    assert_eq!(added(&view, visual).as_deref(), Some("shipped_at"));
    assert_eq!(view.read_with(visual, |view, _| view.session.history.undo_len()), 3, "rename table, add column, rename that column");

    // Undo steps back one edit at a time: the column's name, the column, the table's name.
    diagram_click(visual, "diagram-undo");
    assert_eq!(added(&view, visual).as_deref(), Some("new_column"));
    diagram_click(visual, "diagram-undo");
    assert_eq!(added(&view, visual), None);
    assert_eq!(d3_titles(&view, visual), ["Rename table public.orders to purchases"]);
    assert!(visual.debug_bounds("diagram-pending-2").is_some(), "the rename is still pending");
    diagram_click(visual, "diagram-undo");
    assert!(view.read_with(visual, |view, _| view.session.draft.is_empty()), "everything undone");
    assert!(visual.debug_bounds("diagram-pending-2").is_none(), "the badge went with it");
    assert!(visual.debug_bounds("diagram-changes").is_some());
    // Redo brings them back in order.
    diagram_click(visual, "diagram-redo");
    assert_eq!(d3_titles(&view, visual), ["Rename table public.orders to purchases"]);
    diagram_click(visual, "diagram-redo");
    diagram_click(visual, "diagram-redo");
    assert_eq!(added(&view, visual).as_deref(), Some("shipped_at"));
    assert!(visual.debug_bounds("diagram-pending-2").is_some());
    // The canvas shortcut works on draft edits too, and a new edit clears what could be redone.
    visual.simulate_keystrokes("secondary-z");
    assert_eq!(added(&view, visual).as_deref(), Some("new_column"));
    visual.simulate_keystrokes("secondary-shift-z");
    assert_eq!(added(&view, visual).as_deref(), Some("shipped_at"));
    visual.simulate_keystrokes("secondary-z");
    d3_type(visual, "inspector-field-comment", "when");
    assert!(!view.read_with(visual, |view, _| view.session.history.can_redo()));
}

#[gpui::test]
fn remove_from_diagram_hides_the_view_only_while_drop_table_goes_into_the_changes(cx: &mut TestAppContext) {
    let (view, visual) = d3_window(cx);
    let regions = d2_entity(&view, visual, "regions");
    let bounds = visual.debug_bounds(d3_sel(format!("diagram-entity-{regions}"))).unwrap();
    // Right click: ..., Add note here, Remove from diagram, Drop table...
    d2_right_click(visual, bounds.origin + gpui::point(gpui::px(30.0), gpui::px(8.0)));
    visual.simulate_keystrokes("up up enter"); // Remove from diagram
    settle(visual);
    assert!(view.read_with(visual, |view, _| view.session.draft.is_empty()), "the database is not touched");
    assert_eq!(view.read_with(visual, |view, _| view.annotations.removed.len()), 1);
    assert!(visual.debug_bounds(d3_sel(format!("diagram-entity-{regions}"))).is_none(), "the table is gone from the canvas");
    assert_eq!(view.read_with(visual, |view, _| view.snapshot.as_ref().unwrap().graph.entities.len()), 5, "but not from the graph");

    // Undo brings it back; "Show removed tables" does too.
    diagram_click(visual, "diagram-undo");
    assert!(visual.debug_bounds(d3_sel(format!("diagram-entity-{regions}"))).is_some());
    diagram_click(visual, "diagram-redo");
    let empty = d2_empty_point(&view, visual);
    d2_right_click(visual, empty);
    visual.simulate_keystrokes("up enter"); // Show 1 removed table
    settle(visual);
    assert!(visual.debug_bounds(d3_sel(format!("diagram-entity-{regions}"))).is_some());

    // Drop table... asks first, then adds DROP TABLE to the pending changes.
    let bounds = visual.debug_bounds(d3_sel(format!("diagram-entity-{regions}"))).unwrap();
    d2_right_click(visual, bounds.origin + gpui::point(gpui::px(30.0), gpui::px(8.0)));
    visual.simulate_keystrokes("up enter");
    settle(visual);
    assert!(visual.debug_bounds("drop-table-confirm").is_some(), "a confirmation names the table");
    assert!(view.read_with(visual, |view, _| view.session.draft.is_empty()), "not yet");
    visual.simulate_keystrokes("escape");
    settle(visual);
    assert!(view.read_with(visual, |view, _| view.session.draft.is_empty()), "cancelled");
    d2_right_click(visual, bounds.origin + gpui::point(gpui::px(30.0), gpui::px(8.0)));
    visual.simulate_keystrokes("up enter");
    settle(visual);
    visual.simulate_keystrokes("enter"); // confirm
    settle(visual);
    assert_eq!(d3_titles(&view, visual), ["Drop table public.regions"]);
    assert!(visual.debug_bounds(d3_sel(format!("diagram-entity-{regions}"))).is_some(), "still drawn, marked");
    assert!(visual.debug_bounds(d3_sel(format!("diagram-pending-{regions}"))).is_some());
    view.read_with(visual, |view, _| {
        assert_eq!(view.table_state(regions), crate::app::results::diagram::overlay::TableState::Dropped);
    });
    // The Inspector offers to take the drop back.
    d3_select(&view, visual, "regions");
    diagram_click(visual, "inspector-restore-table");
    assert!(view.read_with(visual, |view, _| view.session.draft.is_empty()));
}

#[gpui::test]
fn new_table_from_the_canvas_menu_is_a_draft_entity_edited_in_the_inspector(cx: &mut TestAppContext) {
    let (view, visual) = d3_window(cx);
    let empty = d2_empty_point(&view, visual);
    d2_right_click(visual, empty);
    visual.simulate_keystrokes("down down enter"); // Add note, New table
    settle(visual);
    let created = d2_entity(&view, visual, "new_table");
    view.read_with(visual, |view, _| {
        assert_eq!(view.pending_count(), 1);
        assert_eq!(view.panel, Some(Panel::Inspector), "the Inspector opens on it");
        assert_eq!(view.selected, Some(crate::app::results::diagram::model::EntityId(created)));
        let entity = &view.snapshot.as_ref().unwrap().graph.entities[created];
        assert_eq!(entity.columns.iter().map(|c| (c.name.as_str(), c.primary_key)).collect::<Vec<_>>(), [("id", true)]);
        let rect = view.layout.rects[created];
        let at = view.viewport.to_diagram(DiagramPoint { x: 24.0, y: 24.0 });
        assert!((rect.x - at.x).abs() < 1.0 && (rect.y - at.y).abs() < 1.0, "placed where the menu opened: {rect:?} {at:?}");
    });
    assert!(visual.debug_bounds(d3_sel(format!("diagram-pending-{created}"))).is_some());
    // Its name is focused: type over it.
    visual.simulate_keystrokes("secondary-a");
    visual.simulate_input("audit_log");
    visual.simulate_keystrokes("enter");
    settle(visual);
    assert_eq!(d3_titles(&view, visual), ["Create table public.audit_log"], "the rename folded into the creation");
    // Add a column and a foreign key to a real table (the new table is New: any dialect may).
    diagram_click(visual, "inspector-add-column");
    d3_type(visual, "inspector-field-name", "customer_id");
    d3_type(visual, "inspector-field-type", "integer");
    diagram_click(visual, "inspector-add-fk");
    diagram_click(visual, "inspector-fk-column-1");
    // Choose the referenced table: customers, with the keyboard on the Select.
    diagram_click(visual, "inspector-fk-parent");
    visual.simulate_keystrokes("down enter");
    settle(visual);
    view.read_with(visual, |view, _| assert!(view.editor_fk_parent().is_some(), "a table was chosen"));
}

#[gpui::test]
fn the_changes_button_joins_the_overflow_priority_and_never_squeezes_zoom_or_search(cx: &mut TestAppContext) {
    let (view, visual) = d3_window(cx);
    let level = |view: &gpui::Entity<DiagramView>, visual: &mut gpui::VisualTestContext| view.read_with(visual, |view, _| view.toolbar_level());
    let drawn = d2_check_toolbar(visual, 1400.0);
    assert_eq!(level(&view, visual), 0);
    assert!(visual.debug_bounds("diagram-changes").is_some() && !drawn.contains(&"diagram-overflow"));
    // The Changes button is the last control to collapse: at 900 the lower
    // priority ones are in the menu, Changes is still on the toolbar.
    let drawn = d2_check_toolbar(visual, 900.0);
    assert!(level(&view, visual) >= 1 && level(&view, visual) <= 4, "level {}", level(&view, visual));
    let toolbar = visual.debug_bounds("diagram-toolbar").unwrap();
    let changes = visual.debug_bounds("diagram-changes").expect("Changes stays at 900");
    assert!(changes.left() >= toolbar.left() && changes.right() <= toolbar.right());
    assert!(drawn.contains(&"diagram-overflow"));
    // With a long label ("Changes (12)") it still fits wherever it is drawn: the
    // widths the toolbar estimates (TOOLBAR_CHANGES) hold, at every width.
    view.update(visual, |view, cx| {
        view.edit_draft(cx, |draft| {
            for n in 0..12 {
                draft.push(Change::DropIndex { table: crate::sql_ddl::TableId::new(Some("public"), "orders"), name: format!("idx_{n}") });
            }
        });
    });
    assert_eq!(view.read_with(visual, |view, _| view.changes_label()), "Changes (12)");
    for width in (600..=1400).step_by(20) {
        let drawn = d2_check_toolbar(visual, width as f32);
        let toolbar = visual.debug_bounds("diagram-toolbar").unwrap();
        match visual.debug_bounds("diagram-changes") {
            Some(changes) => assert!(
                changes.left() >= toolbar.left() && changes.right() <= toolbar.right() + gpui::px(0.5),
                "Changes leaves the toolbar at {width}: {changes:?} in {toolbar:?}"
            ),
            None => assert!(drawn.contains(&"diagram-overflow"), "Changes is in the menu at {width}"),
        }
    }
    view.update(visual, |view, cx| view.discard_all_changes(cx));
    // Down to the fixed controls: Changes moves into the menu with them.
    for width in [700.0, 640.0, 600.0] {
        let _ = d2_check_toolbar(visual, width);
    }
    let drawn = d2_check_toolbar(visual, 640.0);
    assert_eq!(level(&view, visual), 5);
    assert!(visual.debug_bounds("diagram-changes").is_none(), "Changes collapsed at 640");
    assert!(drawn.contains(&"diagram-overflow"));
    // Pending edits keep it reachable: the menu entry opens the review.
    d3_select(&view, visual, "orders");
    diagram_click(visual, "diagram-overflow");
    open_review(visual, "diagram-changes");
    assert!(visual.debug_bounds("changes-subject").is_some(), "the menu entry opens the Changes dialog");
    diagram_click(visual, "changes-close");
    // The window grows: the button is back.
    let _ = d2_check_toolbar(visual, 1400.0);
    assert!(visual.debug_bounds("diagram-changes").is_some());
}

#[gpui::test]
fn column_edits_primary_key_and_the_type_menu(cx: &mut TestAppContext) {
    let (view, visual) = d3_window(cx);
    let orders = d3_select(&view, visual, "orders");
    let editor = |visual: &mut gpui::VisualTestContext| diagram_click(visual, "inspector-column-3"); // note
    editor(visual);
    assert!(visual.debug_bounds("inspector-column-editor").is_some());
    // Type from the menu of common types.
    diagram_click(visual, "inspector-type-menu");
    diagram_click(visual, "inspector-type-option-0");
    let titles = d3_titles(&view, visual);
    assert_eq!(titles, ["Alter column public.orders.note"], "{titles:?}");
    view.read_with(visual, |view, _| {
        let Change::AlterColumn { from, to, .. } = &view.session.draft.changes[0] else { panic!() };
        assert_eq!((from.data_type.as_str(), to.data_type.as_str()), ("text", "integer"));
    });
    // Typing the same text back cancels the change.
    d3_type(visual, "inspector-field-type", "text");
    assert!(view.read_with(visual, |view, _| view.session.draft.is_empty()));
    // Nullability, default and rename in one go are one undo step per edit.
    diagram_click(visual, "inspector-nullable");
    d3_type(visual, "inspector-field-default", "none");
    d3_type(visual, "inspector-field-name", "remark");
    view.read_with(visual, |view, _| {
        assert_eq!(view.session.draft.len(), 2, "{:?}", view.session.draft.changes);
        let alter = view.session.draft.changes.iter().find_map(|c| match c { Change::AlterColumn { from, to, .. } => Some((from, to)), _ => None }).unwrap();
        assert_eq!((alter.0.name.as_str(), alter.1.name.as_str()), ("remark", "remark"), "the alteration follows the rename");
        assert!(!alter.1.nullable && alter.1.default.as_deref() == Some("none"));
    });
    // The primary key: tick a second column.
    diagram_click(visual, "inspector-column-1");
    diagram_click(visual, "inspector-pk");
    view.read_with(visual, |view, _| {
        let Some(Change::SetPrimaryKey { columns, previous, .. }) = view.session.draft.changes.last() else { panic!("{:?}", view.session.draft.changes) };
        assert_eq!(columns, &["id", "customers_id"]);
        assert_eq!(previous.as_ref().map(|p| p.columns.clone()), Some(vec!["id".to_string()]));
        let entity = &view.snapshot.as_ref().unwrap().graph.entities[orders];
        assert!(entity.columns[1].primary_key && !entity.columns[1].nullable, "shown as a key column");
    });
    // Dropping and restoring a column.
    diagram_click(visual, "inspector-column-2");
    diagram_click(visual, "inspector-drop-column");
    assert!(view.read_with(visual, |view, _| view.session.draft.changes.iter().any(|c| matches!(c, Change::DropColumn { column, .. } if column == "shipped_by"))));
    diagram_click(visual, "inspector-restore-column");
    assert!(!view.read_with(visual, |view, _| view.session.draft.changes.iter().any(|c| matches!(c, Change::DropColumn { .. }))));
}

#[gpui::test]
fn indexes_foreign_keys_and_promoting_a_logical_relationship(cx: &mut TestAppContext) {
    let (view, visual) = d3_window(cx);
    let orders = d3_select(&view, visual, "orders");
    // An index through the form.
    diagram_click(visual, "inspector-add-index");
    d3_type(visual, "inspector-index-name", "orders_note_idx");
    diagram_click(visual, "inspector-index-column-3");
    diagram_click(visual, "inspector-index-unique");
    diagram_click(visual, "inspector-index-create");
    assert_eq!(d3_titles(&view, visual), ["Create index orders_note_idx on public.orders"]);
    view.read_with(visual, |view, _| {
        let entity = &view.snapshot.as_ref().unwrap().graph.entities[orders];
        let index = entity.indexes.iter().find(|i| i.name == "orders_note_idx").expect("shown in the inspector");
        assert!(index.unique && index.columns == ["note"]);
    });
    diagram_click(visual, "inspector-drop-index-0");
    assert!(view.read_with(visual, |view, _| view.session.draft.is_empty()), "dropping a pending index cancels it");

    // A logical relationship becomes a pending real foreign key.
    let (child, column) = d2_column(&view, visual, "orders", "customers_id");
    let (parent, parent_column) = d2_column(&view, visual, "customers", "id");
    d2_link(visual, (child, column), (parent, parent_column));
    assert_eq!(view.read_with(visual, |view, _| view.annotations.relations.len()), 1);
    d3_select(&view, visual, "orders");
    let logical = view.read_with(visual, |view, _| {
        view.snapshot.as_ref().unwrap().graph.relations.iter().position(|r| matches!(r.origin, RelationOrigin::Logical(_))).unwrap()
    });
    diagram_click(visual, d3_sel(format!("inspector-promote-fk-{logical}")));
    let titles = d3_titles(&view, visual);
    assert_eq!(titles.len(), 1, "{titles:?}");
    assert!(titles[0].starts_with("Add foreign key fk_orders_customers_id on public.orders"), "{titles:?}");
    view.read_with(visual, |view, _| {
        let graph = &view.snapshot.as_ref().unwrap().graph;
        assert_eq!(graph.relations.iter().filter(|r| r.origin == RelationOrigin::Draft).count(), 1);
        assert!(!graph.relations.iter().any(|r| matches!(r.origin, RelationOrigin::Logical(_))), "the draft key replaces the logical one");
    });
    // The generated SQL is the real key.
    open_review(visual, "diagram-changes");
    assert!(visual.debug_bounds("changes-sql-0-0").is_some());
    assert_eq!(
        d3_copy_sql(visual),
        "ALTER TABLE \"public\".\"orders\" ADD CONSTRAINT \"fk_orders_customers_id\" FOREIGN KEY (\"customers_id\") REFERENCES \"public\".\"customers\" (\"id\");"
    );
    diagram_click(visual, "changes-close");
    // Dropping the pending key brings the logical relationship back.
    let draft_fk = view.read_with(visual, |view, _| {
        view.snapshot.as_ref().unwrap().graph.relations.iter().position(|r| r.origin == RelationOrigin::Draft).unwrap()
    });
    diagram_click(visual, d3_sel(format!("inspector-drop-fk-{draft_fk}")));
    assert!(view.read_with(visual, |view, _| view.session.draft.is_empty()));
    assert!(view.read_with(visual, |view, _| view.snapshot.as_ref().unwrap().graph.relations.iter().any(|r| matches!(r.origin, RelationOrigin::Logical(_)))));

    // Dropping a real foreign key of the catalog.
    d3_select(&view, visual, "order_items");
    let declared = view.read_with(visual, |view, _| {
        let items = d2_entity_in(view, "order_items");
        view.snapshot.as_ref().unwrap().graph.relations.iter().position(|r| r.child.0 == items && r.origin == RelationOrigin::Declared).unwrap()
    });
    diagram_click(visual, d3_sel(format!("inspector-drop-fk-{declared}")));
    assert_eq!(d3_titles(&view, visual), ["Drop foreign key fk_product_id on public.order_items"]);
}

fn d2_entity_in(view: &DiagramView, name: &str) -> usize {
    view.snapshot.as_ref().unwrap().graph.entities.iter().position(|e| e.table.table == name).unwrap()
}

#[gpui::test]
fn apply_runs_the_statements_in_order_clears_the_draft_and_refreshes_the_diagram(cx: &mut TestAppContext) {
    cx.dispatcher.allow_parking();
    let (app, visual, view) = d1_scenario(cx);
    let tab_id = app.read_with(visual, |this, _| this.workspace.active_tab().unwrap().id);
    let ran = std::sync::Arc::new(std::sync::Mutex::new(Vec::<String>::new()));
    let log = ran.clone();
    let fail = std::sync::Arc::new(std::sync::Mutex::new(None::<&'static str>));
    let failing = fail.clone();
    app.update(visual, |this, _| {
        this.diagram.injected_exec = Some(std::sync::Arc::new(move |sql: &str, _: &crate::app::structure_edit::apply::ApplyControl| {
            if failing.lock().unwrap().is_some_and(|needle| sql.contains(needle)) {
                return Err(format!("boom: {sql}"));
            }
            log.lock().unwrap().push(sql.to_owned());
            Ok(())
        }));
    });
    settle(visual);
    let generation = |app: &gpui::Entity<AppRoot>, visual: &mut gpui::VisualTestContext| {
        app.read_with(visual, |this, _| this.diagram_graph(tab_id).map(|s| s.generation))
    };
    let first = generation(&app, visual).expect("loaded");
    assert!(view.read_with(visual, |view, _| view.editable()), "the fixture connection may be edited");

    // Edit Track: a new nullable column and a rename of Composer.
    view.update(visual, |view, cx| view.toggle_panel(Panel::Inspector, cx));
    let track = d2_entity(&view, visual, "Track");
    let bounds = visual.debug_bounds(d3_sel(format!("diagram-entity-{track}"))).unwrap();
    visual.simulate_click(bounds.origin + gpui::point(gpui::px(30.0), gpui::px(8.0)), Default::default());
    settle(visual);
    diagram_click(visual, "inspector-add-column");
    d3_type(visual, "inspector-field-name", "BitRate");
    d3_type(visual, "inspector-field-type", "INTEGER");
    let composer = view.read_with(visual, |view, _| {
        view.snapshot.as_ref().unwrap().graph.entities[track].columns.iter().position(|c| c.name == "Composer").unwrap()
    });
    d3_click(visual, d3_sel(format!("inspector-column-{composer}")));
    d3_type(visual, "inspector-field-name", "Writer");
    assert_eq!(view.read_with(visual, |view, _| view.session.draft.len()), 2);

    // A failing statement: the report says what ran, the draft is kept (SQLite rolls back).
    *fail.lock().unwrap() = Some("ADD COLUMN");
    open_review(visual, "diagram-changes");
    diagram_click(visual, "changes-apply");
    wait_until(visual, "the apply to answer", |visual| view.read_with(visual, |view, _| !view.review_applying()));
    settle(visual);
    assert_eq!(
        ran.lock().unwrap().clone(),
        ["PRAGMA busy_timeout = 10000", "BEGIN", "ALTER TABLE \"main\".\"Track\" RENAME COLUMN \"Composer\" TO \"Writer\"", "ROLLBACK"],
        "SQLite runs the batch in a transaction and rolls it back on the first error"
    );
    assert!(visual.debug_bounds("changes-report").is_some(), "the dialog shows the report");
    assert_eq!(view.read_with(visual, |view, _| view.session.draft.len()), 2, "nothing was applied, nothing is cleared");
    assert_eq!(generation(&app, visual), Some(first), "no refresh after a rollback");
    assert!(app.read_with(visual, |this, _| (0..this.error_log.log.len()).filter_map(|i| this.error_log.log.get(i)).any(|e| e.area == crate::error_log::Area::Diagram)));

    // It works the second time.
    *fail.lock().unwrap() = None;
    ran.lock().unwrap().clear();
    diagram_click(visual, "changes-apply");
    wait_until(visual, "the diagram to refresh", |visual| generation(&app, visual).is_some_and(|g| g > first));
    settle(visual);
    assert_eq!(
        ran.lock().unwrap().clone(),
        [
            "PRAGMA busy_timeout = 10000",
            "BEGIN",
            "ALTER TABLE \"main\".\"Track\" RENAME COLUMN \"Composer\" TO \"Writer\"",
            "ALTER TABLE \"main\".\"Track\" ADD COLUMN \"BitRate\" INTEGER",
            "COMMIT"
        ]
    );
    assert!(view.read_with(visual, |view, _| view.session.draft.is_empty()), "applied changes leave the draft");
    assert!(visual.debug_bounds("changes-subject").is_none(), "the dialog closed");
    assert_eq!(view.read_with(visual, |view, _| view.session.history.undo_len()), 0, "undoing a draft edit would bring back DDL that already ran");
}

#[gpui::test]
fn a_production_connection_asks_for_a_named_confirmation_and_a_read_only_one_refuses(cx: &mut TestAppContext) {
    use crate::settings::connection::{ConnectionProfile, PRODUCTION};
    cx.dispatcher.allow_parking();
    let (app, visual, view) = d1_scenario(cx);
    let connection = app.read_with(visual, |this, _| this.workspace.active_tab().unwrap().connection_id.clone());
    let ran = std::sync::Arc::new(std::sync::Mutex::new(Vec::<String>::new()));
    let log = ran.clone();
    app.update(visual, |this, _| {
        this.diagram.injected_exec = Some(std::sync::Arc::new(move |sql: &str, _: &crate::app::structure_edit::apply::ApplyControl| {
            log.lock().unwrap().push(sql.to_owned());
            Ok(())
        }));
    });
    view.update(visual, |view, cx| view.toggle_panel(Panel::Inspector, cx));
    let track = d2_entity(&view, visual, "Track");
    let bounds = visual.debug_bounds(d3_sel(format!("diagram-entity-{track}"))).unwrap();
    visual.simulate_click(bounds.origin + gpui::point(gpui::px(30.0), gpui::px(8.0)), Default::default());
    settle(visual);
    d3_type(visual, "inspector-table-name", "Tracks");

    // Production (not read-only): Apply needs the confirmation that names the connection.
    publish_types(&app, visual, false);
    give_profile(&app, visual, &connection, ConnectionProfile { type_id: PRODUCTION.into(), ..Default::default() });
    let subject = view.read_with(visual, |view, _| {
        let edit = view.edit.as_ref().expect("editable");
        assert!(edit.confirm_execute && edit.disabled.is_none());
        edit.subject.clone()
    });
    assert!(subject.starts_with("Production"), "{subject}");
    open_review(visual, "diagram-changes");
    assert!(visual.debug_bounds("changes-confirm").is_some());
    diagram_click(visual, "changes-apply");
    assert!(ran.lock().unwrap().is_empty(), "nothing runs without the confirmation");
    diagram_click(visual, "changes-confirm");
    diagram_click(visual, "changes-apply");
    wait_until(visual, "the statements to run", |_| !ran.lock().unwrap().is_empty());
    assert!(ran.lock().unwrap().iter().any(|sql| sql == "BEGIN"));
    settle(visual);

    // The same connection made read-only: the editing UI is off and says why.
    publish_types(&app, visual, true);
    give_profile(&app, visual, &connection, ConnectionProfile { type_id: PRODUCTION.into(), ..Default::default() });
    let reason = view.read_with(visual, |view, _| view.edit_disabled_reason().map(str::to_owned));
    assert!(reason.as_deref().is_some_and(|r| r.contains("read-only")), "{reason:?}");
    d3_select(&view, visual, "Track");
    assert!(visual.debug_bounds("inspector-readonly-reason").is_some());
    assert!(visual.debug_bounds("inspector-add-column").is_none());
}

// ---- column comments from live servers (ignored without TABLEX_TEST_*_URI) ----------------

/// Creates a table with a commented column on a live server (through the
/// generated DDL), describes it with the real driver, and shows it in the
/// Inspector of a diagram: the comment must reach the screen.
fn d3_live_comment_in_the_inspector(
    cx: &mut TestAppContext,
    fixture: &dyn engine_driver_conformance::SqlFixture,
    uri: &str,
    dylib: std::path::PathBuf,
    kind: ConnectionKind,
    id_type: &str,
    text_type: &str,
) {
    use crate::app::results::diagram::model::{DiagramGraph, RelatedProperties};
    use crate::sql_ddl::apply::apply;
    use crate::sql_ddl::draft::Draft;
    use crate::sql_ddl::{plan, ColumnSpec, TableId};
    use engine_driver_conformance::harness::Ctx;
    use serde_json::json;
    let ctx = Ctx::load(&dylib, uri, fixture).expect("load the driver");
    let scratch = ctx.scratch(false).expect("scratch namespace");
    let mut conn = ctx.connect().expect("connect");
    let name = format!("commented_{}", ctx.unique());
    let namespace = scratch.scratch.name.clone();
    let table = TableId::new(Some(&namespace), &name);
    let dialect = crate::sql_ddl::dialect_for(kind).unwrap();
    let mut draft = Draft::default();
    draft.push(Change::CreateTable {
        table,
        columns: vec![
            ColumnSpec { name: "id".into(), data_type: id_type.into(), nullable: false, ..ColumnSpec::default() },
            ColumnSpec { name: "label".into(), data_type: text_type.into(), nullable: true, comment: Some("Shown to the customer".into()), ..ColumnSpec::default() },
        ],
        primary_key: vec!["id".into()],
        comment: None,
    });
    let planned = plan(dialect, &draft.changes);
    let outcome = apply(&planned, &mut crate::sql_ddl::live::Statements(&mut conn));
    assert!(outcome.succeeded(), "{}", outcome.report(&planned.steps()));
    let (schema, database) = match kind {
        ConnectionKind::Postgres => (Some(namespace.clone()), Some(scratch.scratch.database.clone())),
        _ => (None, Some(namespace.clone())),
    };
    let described = conn
        .describe(&json!({"database": database, "schema": schema, "object": name}))
        .expect("describe through the real driver");
    let properties: crate::catalog::TableProperties = serde_json::from_value(described).unwrap();
    let table_ref = TableRef { database: database.clone().unwrap_or_default(), schema: schema.clone(), table: name.clone() };
    let snapshot = DiagramSnapshot {
        graph: std::sync::Arc::new(DiagramGraph::build_schema(&[(table_ref, RelatedProperties::Ready(std::sync::Arc::new(properties)))])),
        generation: 1,
        error: None,
        progress: None,
        truncated: None,
    };
    cx.update(|app| {
        gpui_component::init(app);
        theme::init(app);
    });
    let (root, visual) = cx.add_window_view(|window, cx| {
        let view = cx.new(|cx| DiagramView::new(crate::app::results::diagram::view::DiagramKind::Schema, cx));
        gpui_component::Root::new(view, window, cx)
    });
    let view = visual.cx.read(|cx| root.read(cx).view().clone().downcast::<DiagramView>().unwrap());
    visual.simulate_resize(gpui::size(gpui::px(1400.0), gpui::px(850.0)));
    view.update(visual, |view, cx| {
        view.panel = Some(Panel::Inspector);
        view.set_snapshot(Some(snapshot), cx);
    });
    settle(visual);
    let bounds = visual.debug_bounds("diagram-entity-0").expect("the table is drawn");
    visual.simulate_click(bounds.origin + gpui::point(gpui::px(30.0), gpui::px(8.0)), Default::default());
    settle(visual);
    assert_eq!(
        view.read_with(visual, |view, _| view.snapshot.as_ref().unwrap().graph.entities[0].columns.iter().find(|c| c.name == "label").and_then(|c| c.comment.clone())).as_deref(),
        Some("Shown to the customer"),
        "the live driver reports the comment"
    );
    let label = view.read_with(visual, |view, _| view.snapshot.as_ref().unwrap().graph.entities[0].columns.iter().position(|c| c.name == "label").unwrap());
    assert!(visual.debug_bounds(d3_sel(format!("inspector-comment-{label}"))).is_some(), "the Inspector shows the comment of {kind:?}");
}

#[gpui::test]
#[ignore = "needs TABLEX_TEST_PG_URI"]
fn live_postgres_comments_reach_the_inspector(cx: &mut TestAppContext) {
    cx.dispatcher.allow_parking();
    let Ok(uri) = std::env::var("TABLEX_TEST_PG_URI") else { return };
    let dylib = crate::sql_ddl::live::dylib("dbengine-postgres", "dbengine_postgres", &["abi-v2"]);
    d3_live_comment_in_the_inspector(cx, &engine_driver_conformance::fixtures::PostgresFixture, &uri, dylib, ConnectionKind::Postgres, "bigint", "text");
}

#[gpui::test]
#[ignore = "needs TABLEX_TEST_MYSQL_URI"]
fn live_mysql_comments_reach_the_inspector(cx: &mut TestAppContext) {
    cx.dispatcher.allow_parking();
    let Ok(uri) = std::env::var("TABLEX_TEST_MYSQL_URI") else { return };
    let dylib = crate::sql_ddl::live::dylib("dbengine-mysql", "dbengine_mysql", &["abi-v2"]);
    d3_live_comment_in_the_inspector(cx, &engine_driver_conformance::fixtures::MysqlFixture::MYSQL, &uri, dylib, ConnectionKind::Mysql, "bigint", "text");
}

#[gpui::test]
#[ignore = "needs TABLEX_TEST_MARIADB_URI"]
fn live_mariadb_comments_reach_the_inspector(cx: &mut TestAppContext) {
    cx.dispatcher.allow_parking();
    let Ok(uri) = std::env::var("TABLEX_TEST_MARIADB_URI") else { return };
    let dylib = crate::sql_ddl::live::dylib("dbengine-mariadb", "dbengine_mariadb", &["abi-v2"]);
    d3_live_comment_in_the_inspector(cx, &engine_driver_conformance::fixtures::MysqlFixture::MARIADB, &uri, dylib, ConnectionKind::Mariadb, "bigint", "text");
}

#[gpui::test]
#[ignore = "needs TABLEX_TEST_CLICKHOUSE_URI"]
fn live_clickhouse_comments_reach_the_inspector(cx: &mut TestAppContext) {
    cx.dispatcher.allow_parking();
    let Ok(uri) = std::env::var("TABLEX_TEST_CLICKHOUSE_URI") else { return };
    let dylib = crate::sql_ddl::live::dylib("dbengine-clickhouse", "dbengine_clickhouse", &[]);
    d3_live_comment_in_the_inspector(cx, &engine_driver_conformance::fixtures::ClickHouseFixture, &uri, dylib, ConnectionKind::Clickhouse, "Int64", "String");
}

// ---- stale drafts, re-checks, interrupted applies, name collisions ---------------------------------

/// `d2_window` over a catalog the test changed first.
fn d3_changed_snapshot(change: impl FnOnce(&mut crate::app::results::diagram::model::DiagramGraph)) -> DiagramSnapshot {
    let mut snapshot = d2_snapshot();
    snapshot.generation = 2;
    change(std::sync::Arc::make_mut(&mut snapshot.graph));
    snapshot
}

fn d3_note_type(graph: &mut crate::app::results::diagram::model::DiagramGraph, data_type: &str) {
    let orders = graph.entities.iter().position(|e| e.table.table == "orders").unwrap();
    let note = graph.entities[orders].columns.iter().position(|c| c.name == "note").unwrap();
    graph.entities[orders].columns[note].data_type = data_type.into();
}

#[gpui::test]
fn a_draft_made_against_an_older_catalog_is_marked_stale_and_cannot_be_applied(cx: &mut TestAppContext) {
    let (view, visual, applied) = d3_window_with(cx, d3_context(None, false));
    d3_select(&view, visual, "orders");
    diagram_click(visual, "inspector-column-3"); // note: text
    d3_type(visual, "inspector-field-type", "varchar(20)");
    assert_eq!(d3_titles(&view, visual), ["Alter column public.orders.note"]);
    assert!(view.read_with(visual, |view, _| view.session.stale.iter().all(Option::is_none)), "the catalog is what the draft started from");

    // Someone widened the column outside the app: the next catalog says varchar(255).
    view.update(visual, |view, cx| view.set_snapshot(Some(d3_changed_snapshot(|g| d3_note_type(g, "varchar(255)"))), cx));
    settle(visual);
    let reason = view.read_with(visual, |view, _| view.session.stale[0].clone()).expect("the alteration is stale");
    assert!(reason.contains("refresh the diagram") && reason.contains("varchar(255)") && reason.contains("text"), "{reason}");
    assert!(visual.debug_bounds("inspector-stale").is_some(), "the Inspector says so");
    open_review(visual, "diagram-changes");
    assert!(visual.debug_bounds("changes-stale-0").is_some(), "the dialog marks the change");
    diagram_click(visual, "changes-apply");
    assert!(applied.borrow().is_empty(), "a stale change is never sent");
    // Even a direct request is refused by the view itself.
    let (changes, script) = view.read_with(visual, |view, _| {
        let edit = view.edit.as_ref().unwrap();
        let changes = view.session.draft.changes.clone();
        (changes.clone(), crate::sql_ddl::plan(&*edit.dialect(), &changes).script())
    });
    view.update(visual, |view, cx| view.request_apply(changes, script, cx));
    assert!(applied.borrow().is_empty());
    assert!(view.read_with(visual, |view, _| view.review_report()).is_some_and(|(text, failed)| failed && text.contains("stale")));
    // The change is not rebased: discarding it is the way out.
    diagram_click(visual, "changes-discard-0");
    assert!(view.read_with(visual, |view, _| view.session.draft.is_empty() && view.session.stale.is_empty()));
}

#[gpui::test]
fn apply_checks_again_what_was_reviewed_acknowledged_and_allowed(cx: &mut TestAppContext) {
    let (view, visual, applied) = d3_window_with(cx, d3_context(None, true));
    let orders = d3_select(&view, visual, "orders");
    view.update(visual, |view, cx| view.drop_column(orders, 3, cx));
    let model = |view: &gpui::Entity<DiagramView>, visual: &mut gpui::VisualTestContext| {
        view.read_with(visual, |view, _| {
            let changes = view.session.draft.changes.clone();
            let script = crate::sql_ddl::plan(&*view.edit.as_ref().unwrap().dialect(), &changes).script();
            (changes, script)
        })
    };
    let report = |view: &gpui::Entity<DiagramView>, visual: &mut gpui::VisualTestContext| view.read_with(visual, |view, _| view.review_report().map(|r| r.0));
    // Destructive and a named confirmation: both missing.
    let (changes, script) = model(&view, visual);
    view.update(visual, |view, cx| view.request_apply(changes.clone(), script.clone(), cx));
    assert!(report(&view, visual).unwrap().contains("data will be lost"));
    assert!(applied.borrow().is_empty());
    // A change added after the review is not part of what is applied.
    view.update(visual, |view, cx| {
        view.edit_draft(cx, |draft| draft.push(Change::DropIndex { table: crate::sql_ddl::TableId::new(Some("public"), "orders"), name: "late".into() }));
    });
    view.update(visual, |view, cx| view.request_apply(changes, script, cx));
    assert!(report(&view, visual).unwrap().contains("not the ones that were reviewed"));
    assert!(applied.borrow().is_empty());
    // Read-only after the dialog opened: refused.
    view.update(visual, |view, cx| {
        let mut context = d3_context(Some("Structure editing is blocked: read-only."), true);
        context.scope = view.edit.as_ref().unwrap().scope.clone();
        view.set_edit_context(Some(context), cx)
    });
    let (changes, script) = model(&view, visual);
    view.update(visual, |view, cx| view.request_apply(changes, script, cx));
    assert!(report(&view, visual).unwrap().contains("read-only"));
    assert!(applied.borrow().is_empty());
}

fn d3_stored_window_over<'a>(
    cx: &'a mut TestAppContext,
    dir: &std::path::Path,
    snapshot: Option<DiagramSnapshot>,
) -> (gpui::Entity<DiagramView>, &'a mut gpui::VisualTestContext) {
    let (view, visual) = d2_window(cx);
    view.update(visual, |view, cx| {
        view.set_edit_context(Some(d3_context(None, false)), cx);
        if let Some(snapshot) = snapshot {
            view.set_snapshot(Some(snapshot), cx);
        }
    });
    let store = std::sync::Arc::new(LayoutStore::new(dir.to_path_buf()));
    view.update(visual, |view, cx| view.set_persistence(Some((store, d2_key())), cx));
    settle(visual);
    (view, visual)
}

#[gpui::test]
fn the_changes_of_an_interrupted_apply_that_are_in_the_catalog_leave_the_draft_on_load(cx: &mut TestAppContext) {
    cx.dispatcher.allow_parking();
    let dir = std::env::temp_dir().join(format!("tablex-d3-marker-{}", uuid::Uuid::new_v4()));
    let (view, visual) = d3_stored_window_over(cx, &dir, None);
    wait_until(visual, "the (empty) document to be read", |visual| view.read_with(visual, |view, _| view.persisting()));
    let table = crate::sql_ddl::TableId::new(Some("public"), "orders");
    view.update(visual, |view, cx| {
        view.edit_draft(cx, |draft| {
            draft.push(Change::AddColumn {
                table: table.clone(),
                column: crate::sql_ddl::ColumnSpec { name: "fresh".into(), data_type: "text".into(), nullable: true, ..Default::default() },
            });
            draft.push(Change::AddColumn {
                table: table.clone(),
                column: crate::sql_ddl::ColumnSpec { name: "later".into(), data_type: "text".into(), nullable: true, ..Default::default() },
            });
        });
    });
    // Apply starts: the marker is on disk before anything runs. Then the app dies (the view is dropped).
    let (changes, script) = view.read_with(visual, |view, _| {
        let changes = view.session.draft.changes.clone();
        (changes.clone(), crate::sql_ddl::plan(&*view.edit.as_ref().unwrap().dialect(), &changes).script())
    });
    view.update(visual, |view, cx| view.request_apply(changes, script, cx));
    assert_eq!(view.read_with(visual, |view, _| view.applying_marker_len()), 2);
    let files = std::fs::read_dir(&dir).map(|entries| entries.count()).unwrap_or(0);
    assert_eq!(files, 1, "the marker was written synchronously");

    // The next start: the catalog already has `fresh` (the first statement ran), not `later`.
    let changed = d3_changed_snapshot(|graph| {
        let orders = graph.entities.iter().position(|e| e.table.table == "orders").unwrap();
        let mut column = graph.entities[orders].columns[0].clone();
        column.name = "fresh".into();
        column.primary_key = false;
        graph.entities[orders].columns.push(column);
    });
    let (fresh, visual) = d3_stored_window_over(cx, &dir, Some(changed));
    wait_until(visual, "the draft to load and reconcile", |visual| fresh.read_with(visual, |view, _| view.persisting() && view.applying_marker_len() == 0));
    settle(visual);
    let titles = d3_titles(&fresh, visual);
    assert_eq!(titles, ["Add column public.orders.later"], "what already ran is not applied twice");
    std::fs::remove_dir_all(dir).ok();
}

#[gpui::test]
fn a_table_name_the_diagram_does_not_show_is_still_taken(cx: &mut TestAppContext) {
    let mut context = d3_context(None, false);
    context.catalog = Some(std::sync::Arc::new(vec!["audit_log".into(), "orders".into(), "new_table".into(), "z_hidden".into()]));
    let (view, visual, _) = d3_window_with(cx, context);
    d3_select(&view, visual, "orders");
    d3_type(visual, "inspector-table-name", "audit_log");
    assert!(view.read_with(visual, |view, _| view.session.draft.is_empty()), "audit_log exists in the schema");
    assert!(view.read_with(visual, |view, _| view.editor_error()).is_some_and(|e| e.contains("audit_log")));
    d3_type(visual, "inspector-table-name", "fresh_name");
    assert_eq!(d3_titles(&view, visual), ["Rename table public.orders to fresh_name"]);
    // New table skips the name of a table the diagram does not show.
    let empty = d2_empty_point(&view, visual);
    d2_right_click(visual, empty);
    visual.simulate_keystrokes("down down enter");
    settle(visual);
    assert!(d3_titles_all(&view, visual).iter().any(|t| t == "Create table public.new_table_2"), "{:?}", d3_titles_all(&view, visual));
}

fn d3_titles_all(view: &gpui::Entity<DiagramView>, visual: &mut gpui::VisualTestContext) -> Vec<String> {
    d3_titles(view, visual)
}

#[gpui::test]
fn one_apply_runs_per_connection_and_cancel_rolls_it_back(cx: &mut TestAppContext) {
    cx.dispatcher.allow_parking();
    let (app, visual, view) = d1_scenario(cx);
    let tab_id = app.read_with(visual, |this, _| this.workspace.active_tab().unwrap().id);
    let ran = std::sync::Arc::new(std::sync::Mutex::new(Vec::<String>::new()));
    let log = ran.clone();
    // Statements that change the structure wait until the apply is cancelled or released.
    let release = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
    let released = release.clone();
    app.update(visual, |this, _| {
        this.diagram.injected_exec = Some(std::sync::Arc::new(move |sql: &str, control: &crate::app::structure_edit::apply::ApplyControl| {
            log.lock().unwrap().push(sql.to_owned());
            while sql.starts_with("ALTER") && !released.load(std::sync::atomic::Ordering::SeqCst) {
                if control.is_cancelled() {
                    return Err("cancelled by the user".into());
                }
                std::thread::sleep(std::time::Duration::from_millis(5));
            }
            Ok(())
        }));
    });
    settle(visual);
    view.update(visual, |view, cx| view.toggle_panel(Panel::Inspector, cx));
    let track = d2_entity(&view, visual, "Track");
    let bounds = visual.debug_bounds(d3_sel(format!("diagram-entity-{track}"))).unwrap();
    visual.simulate_click(bounds.origin + gpui::point(gpui::px(30.0), gpui::px(8.0)), Default::default());
    settle(visual);
    diagram_click(visual, "inspector-add-column");
    d3_type(visual, "inspector-field-name", "BitRate");
    d3_type(visual, "inspector-field-type", "INTEGER");

    open_review(visual, "diagram-changes");
    assert!(visual.debug_bounds("changes-cancel").is_none(), "nothing runs yet");
    diagram_click(visual, "changes-apply");
    wait_until(visual, "the apply to start", |_| ran.lock().unwrap().iter().any(|sql| sql.starts_with("ALTER TABLE")));
    settle(visual);
    assert!(app.read_with(visual, |this, _| this.diagram.applies.len()) == 1);
    assert!(visual.debug_bounds("changes-progress").is_some(), "progress is shown");
    assert!(visual.debug_bounds("changes-cancel").is_some(), "with a Cancel button");

    // A second apply on the same connection is refused, whoever asks.
    let request = view.read_with(visual, |view, _| {
        let changes = view.session.draft.changes.clone();
        crate::app::structure_edit::ApplyRequest { script: crate::sql_ddl::plan(&*view.edit.as_ref().unwrap().dialect(), &changes).script(), changes, confirmed: true }
    });
    app.update(visual, |this, cx| {
        this.apply_structure_changes(
            crate::app::structure_edit::ApplyTarget::Diagram { tab_id, kind: crate::app::results::diagram::view::DiagramKind::Schema },
            request,
            cx,
        )
    });
    settle(visual);
    assert_eq!(app.read_with(visual, |this, _| this.diagram.applies.len()), 1, "still the first one");
    let refused = app.read_with(visual, |this, _| {
        (0..this.error_log.log.len())
            .filter_map(|i| this.error_log.log.get(i))
            .any(|e| e.area == crate::error_log::Area::Diagram && e.message.contains("already running"))
    });
    assert!(refused, "the second apply is refused with a clear message");

    // Cancel: the running statement stops, SQLite rolls the transaction back, nothing is applied.
    diagram_click(visual, "changes-cancel");
    wait_until(visual, "the cancelled apply to end", |visual| view.read_with(visual, |view, _| !view.review_applying()));
    settle(visual);
    let report = view.read_with(visual, |view, _| view.review_report()).expect("a report");
    assert!(report.1 && report.0.starts_with("Cancelled.") && report.0.contains("rolled back"), "{report:?}");
    assert_eq!(ran.lock().unwrap().last().map(String::as_str), Some("ROLLBACK"));
    assert_eq!(view.read_with(visual, |view, _| view.session.draft.len()), 1, "the draft is kept");
    assert_eq!(app.read_with(visual, |this, _| this.diagram.applies.len()), 0, "the connection is free again");
    assert_eq!(view.read_with(visual, |view, _| view.applying_marker_len()), 0, "the marker is cleared");

    // Free again: the same apply runs to the end when it is not cancelled.
    release.store(true, std::sync::atomic::Ordering::SeqCst);
    diagram_click(visual, "changes-apply");
    wait_until(visual, "the apply to finish", |visual| view.read_with(visual, |view, _| view.session.draft.is_empty()));
    assert_eq!(ran.lock().unwrap().last().map(String::as_str), Some("COMMIT"));
}

// ---- hardening: the checks around an apply ---------------------------------------------------------

mod apply_checks {
    use super::*;
    use crate::app::structure_edit::apply::{run_apply, ApplyControl, ApplyJob, ApplySession, Cancelled};
    use crate::sql_ddl::apply::StatementRunner;
    use crate::sql_ddl::schema::LiveTable;
    use crate::sql_ddl::{dialect_for, plan, Change, TableId};
    use std::collections::HashMap;
    use std::sync::{Arc, Mutex};

    /// A session that logs what it is asked and answers `describe` from a table.
    struct Scripted {
        log: Arc<Mutex<Vec<String>>>,
        mode: Option<String>,
        control: Arc<ApplyControl>,
        live: HashMap<String, LiveTable>,
        cancel_on_describe: bool,
    }

    impl StatementRunner for Scripted {
        fn run(&mut self, sql: &str) -> Result<(), String> {
            if self.control.is_cancelled() {
                return Err("cancelled by the user".into());
            }
            self.log.lock().unwrap().push(sql.to_owned());
            Ok(())
        }

        fn finish(&mut self, sql: &str) -> Result<(), String> {
            self.log.lock().unwrap().push(sql.to_owned());
            Ok(())
        }

        fn cancelled(&self) -> bool {
            self.control.is_cancelled()
        }
    }

    impl ApplySession for Scripted {
        fn sql_mode(&mut self) -> Result<Option<String>, String> {
            Ok(self.mode.clone())
        }

        fn describe(&mut self, table: &TableId, _: &EditScope) -> Result<Option<LiveTable>, String> {
            self.log.lock().unwrap().push(format!("describe {}", table.name));
            if self.cancel_on_describe {
                self.control.cancel();
            }
            Ok(self.live.get(&table.name).cloned())
        }
    }

    fn orders(name: &str) -> TableId {
        TableId { schema: Some("shop".into()), name: name.into() }
    }

    fn indexed(index: &str) -> LiveTable {
        LiveTable { indexes: vec![index.to_owned()], ..LiveTable::default() }
    }

    fn job(kind: ConnectionKind, changes: Vec<Change>) -> ApplyJob {
        let reviewed_script = plan(dialect_for(kind).unwrap(), &changes).script();
        ApplyJob {
            kind,
            scope: EditScope { kind, database: "shop".into(), schema: Some("shop".into()) },
            changes,
            reviewed_script,
            lock_timeout_secs: 10,
        }
    }

    fn session(mode: Option<&str>, live: &[(&str, LiveTable)]) -> (Scripted, Arc<Mutex<Vec<String>>>) {
        let log = Arc::new(Mutex::new(Vec::new()));
        let session = Scripted {
            log: Arc::clone(&log),
            mode: mode.map(str::to_owned),
            control: Arc::new(ApplyControl::default()),
            live: live.iter().map(|(name, table)| ((*name).to_owned(), table.clone())).collect(),
            cancel_on_describe: false,
        };
        (session, log)
    }

    fn drop_index(table: &str) -> Change {
        Change::DropIndex { table: orders(table), name: "idx".into() }
    }

    fn position(log: &[String], prefix: &str) -> usize {
        log.iter().position(|line| line.starts_with(prefix)).unwrap_or_else(|| panic!("{prefix} in {log:?}"))
    }

    /// A column renamed after it was altered carries its new name in the alteration; a table renamed in
    /// the same set is named by its new name in every later change. The apply (the diagram's and the
    /// Properties', one worker) plans without a catalog: neither name may look like a clash.
    #[test]
    fn a_column_altered_and_renamed_in_one_set_applies_and_so_does_its_renamed_table() {
        use crate::sql_ddl::draft::Draft;
        use crate::sql_ddl::{ColumnSpec, IndexSpec};
        let spec = |name: &str, nullable: bool| ColumnSpec { name: name.into(), data_type: "text".into(), nullable, ..ColumnSpec::default() };
        let t = orders("orders");
        let mut draft = Draft::default();
        draft.push(Change::AlterColumn { table: t.clone(), from: spec("note", true), to: spec("note", false) });
        draft.push(Change::RenameColumn { table: t.clone(), from: "note".into(), to: "memo".into() });
        draft.push(Change::DropIndex { table: t.clone(), name: "idx_note".into() });
        draft.push(Change::CreateIndex { table: t.clone(), index: IndexSpec { name: "idx_memo".into(), columns: vec!["memo".into()], unique: false } });
        draft.push(Change::RenameTable { table: t, to: "purchases".into() });
        let live = LiveTable { columns: vec![spec("id", false), spec("note", true)], indexes: vec!["idx_note".into()], ..LiveTable::default() };
        let (mut ok, log) = session(None, &[("orders", live)]);
        let run = run_apply(&job(ConnectionKind::Postgres, draft.changes.clone()), &mut ok).ok().expect("the apply is not refused");
        assert!(run.outcome.succeeded(), "{}", run.outcome.report(&run.planned.steps()));
        let log = log.lock().unwrap().clone();
        for expected in ["ALTER TABLE \"shop\".\"orders\" RENAME TO \"purchases\"", "RENAME COLUMN \"note\" TO \"memo\"", "ALTER COLUMN \"memo\" SET NOT NULL", "CREATE INDEX \"idx_memo\""] {
            assert!(log.iter().any(|sql| sql.contains(expected)), "{expected} in {log:?}");
        }
    }

    #[test]
    fn on_postgres_the_catalog_is_checked_inside_the_transaction_under_a_lock() {
        let (mut ok, log) = session(None, &[("orders", indexed("idx"))]);
        let run = run_apply(&job(ConnectionKind::Postgres, vec![drop_index("orders")]), &mut ok).ok().expect("runs");
        assert!(run.outcome.succeeded());
        let log = log.lock().unwrap().clone();
        let (begin, timeout, lock, describe, drop, commit) = (
            position(&log, "BEGIN"),
            position(&log, "SET LOCAL lock_timeout"),
            position(&log, "LOCK TABLE \"shop\".\"orders\" IN SHARE UPDATE EXCLUSIVE MODE"),
            position(&log, "describe orders"),
            position(&log, "DROP INDEX"),
            position(&log, "COMMIT"),
        );
        assert!(begin < timeout && timeout < lock && lock < describe && describe < drop && drop < commit, "{log:?}");

        // Stale: the check fails after BEGIN, so the transaction is rolled back and no statement ran.
        let (mut moved, log) = session(None, &[("orders", LiveTable::default())]);
        let refusal = run_apply(&job(ConnectionKind::Postgres, vec![drop_index("orders")]), &mut moved).err().expect("refused");
        assert!(refusal.message.contains("refresh the diagram") && refusal.message.contains("idx"), "{}", refusal.message);
        let log = log.lock().unwrap().clone();
        assert!(!log.iter().any(|line| line.starts_with("DROP")), "{log:?}");
        assert!(position(&log, "BEGIN") < position(&log, "describe orders") && log.last().map(String::as_str) == Some("ROLLBACK"), "{log:?}");
    }

    #[test]
    fn without_transactional_ddl_the_check_follows_the_bounded_lock_wait() {
        let (mut ok, log) = session(Some("STRICT_TRANS_TABLES"), &[("orders", indexed("idx"))]);
        let run = run_apply(&job(ConnectionKind::Mysql, vec![drop_index("orders")]), &mut ok).ok().expect("runs");
        assert!(run.outcome.succeeded());
        let log = log.lock().unwrap().clone();
        assert!(!log.iter().any(|line| line == "BEGIN" || line.starts_with("LOCK")), "{log:?}");
        assert!(position(&log, "SET SESSION lock_wait_timeout") < position(&log, "describe orders"), "{log:?}");
        assert!(position(&log, "describe orders") < position(&log, "DROP INDEX"), "{log:?}");
    }

    #[test]
    fn cancel_is_seen_between_the_describes_and_nothing_runs() {
        for kind in [ConnectionKind::Postgres, ConnectionKind::Mysql] {
            let (mut cancelling, log) = session(Some(""), &[("a", indexed("idx")), ("b", indexed("idx"))]);
            cancelling.cancel_on_describe = true;
            let refusal = run_apply(&job(kind, vec![drop_index("a"), drop_index("b")]), &mut cancelling).err().expect("cancelled");
            assert!(refusal.message.starts_with("Cancelled"), "{}", refusal.message);
            let log = log.lock().unwrap().clone();
            assert_eq!(log.iter().filter(|line| line.starts_with("describe")).count(), 1, "the second table is not described: {log:?}");
            assert!(!log.iter().any(|line| line.starts_with("DROP")), "{log:?}");
            if kind == ConnectionKind::Postgres {
                assert_eq!(log.last().map(String::as_str), Some("ROLLBACK"), "{log:?}");
            }
        }
    }

    #[test]
    fn a_cancel_that_comes_before_the_statement_is_registered_still_stops_it() {
        let hits = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let canceller = || {
            let hits = Arc::clone(&hits);
            Arc::new(move || {
                hits.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            }) as Arc<dyn Fn() + Send + Sync>
        };
        // Cancel first (nothing registered yet): registering notices it and the statement does not start.
        let control = ApplyControl::default();
        control.cancel();
        assert_eq!(control.register(canceller()), Err(Cancelled));
        assert_eq!(hits.load(std::sync::atomic::Ordering::SeqCst), 0, "nothing was running to cancel");
        // Register first: a later cancel reaches the statement.
        let control = ApplyControl::default();
        assert_eq!(control.register(canceller()), Ok(()));
        control.cancel();
        assert_eq!(hits.load(std::sync::atomic::Ordering::SeqCst), 1);
        // Once the statement is over nothing is left to cancel.
        control.clear_running();
        control.cancel();
        assert_eq!(hits.load(std::sync::atomic::Ordering::SeqCst), 1);
    }

    #[test]
    fn the_connection_is_free_again_when_the_job_ends_however_it_ends() {
        use crate::app::results::diagram::view::DiagramKind;
        use crate::app::structure_edit::ApplyTarget;
        let (seven, eight) = (ApplyTarget::Diagram { tab_id: 7, kind: DiagramKind::Schema }, ApplyTarget::Diagram { tab_id: 8, kind: DiagramKind::Schema });
        let applies = crate::app::structure_edit::apply::Applies::default();
        let control = Arc::new(ApplyControl::default());
        {
            let guard = applies.begin("conn", seven, &control).expect("free");
            assert_eq!(applies.running("conn"), Some(seven));
            assert_eq!(applies.begin("conn", eight, &control).err(), Some(seven), "a second one is refused, with the target that holds it");
            drop(guard);
        }
        assert_eq!(applies.len(), 0);
        // A panic unwinding through the job frees it as well.
        let held = applies.clone();
        let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(move || {
            let _guard = held.begin("conn", seven, &control).expect("free");
            panic!("forced failure inside the job");
        }));
        assert!(result.is_err());
        assert_eq!(applies.len(), 0, "the entry went with the guard");
    }

    #[test]
    fn one_apply_per_connection_across_a_diagram_and_a_properties_tab() {
        use crate::app::results::diagram::view::DiagramKind;
        use crate::app::structure_edit::ApplyTarget;
        let applies = crate::app::structure_edit::apply::Applies::default();
        let control = Arc::new(ApplyControl::default());
        let diagram = ApplyTarget::Diagram { tab_id: 3, kind: DiagramKind::Table };
        let properties = ApplyTarget::Properties { tab_id: 4, edit: 77 };
        let guard = applies.begin("conn", diagram, &control).expect("free");
        assert_eq!(applies.begin("conn", properties, &control).err(), Some(diagram), "a Properties apply waits for the diagram's");
        // Another connection is not held.
        let other = applies.begin("other", properties, &control).expect("another connection is free");
        drop(guard);
        let guard = applies.begin("conn", properties, &control).expect("free again");
        assert_eq!(applies.begin("conn", diagram, &control).err(), Some(properties), "and the other way round");
        drop((guard, other));
        assert_eq!(applies.len(), 0);
    }
}

#[gpui::test]
fn a_job_that_panics_frees_the_connection_and_tells_the_dialog(cx: &mut TestAppContext) {
    cx.dispatcher.allow_parking();
    let (app, visual, view) = d1_scenario(cx);
    let blow_up = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(true));
    let armed = blow_up.clone();
    app.update(visual, |this, _| {
        this.diagram.injected_exec = Some(std::sync::Arc::new(move |sql: &str, _: &crate::app::structure_edit::apply::ApplyControl| {
            if sql.starts_with("ALTER") && armed.load(std::sync::atomic::Ordering::SeqCst) {
                panic!("forced failure inside the apply job");
            }
            Ok(())
        }));
    });
    settle(visual);
    view.update(visual, |view, cx| view.toggle_panel(Panel::Inspector, cx));
    let track = d2_entity(&view, visual, "Track");
    let bounds = visual.debug_bounds(d3_sel(format!("diagram-entity-{track}"))).unwrap();
    visual.simulate_click(bounds.origin + gpui::point(gpui::px(30.0), gpui::px(8.0)), Default::default());
    settle(visual);
    diagram_click(visual, "inspector-add-column");
    d3_type(visual, "inspector-field-name", "BitRate");
    d3_type(visual, "inspector-field-type", "INTEGER");
    open_review(visual, "diagram-changes");
    diagram_click(visual, "changes-apply");
    wait_until(visual, "the apply to end", |visual| view.read_with(visual, |view, _| !view.review_applying()));
    settle(visual);
    let report = view.read_with(visual, |view, _| view.review_report()).expect("a report");
    assert!(report.1 && report.0.contains("stopped unexpectedly"), "{report:?}");
    assert_eq!(app.read_with(visual, |this, _| this.diagram.applies.len()), 0, "the connection is free again");
    assert_eq!(view.read_with(visual, |view, _| view.session.draft.len()), 1, "the draft is kept");
    // Free again: the same apply runs.
    blow_up.store(false, std::sync::atomic::Ordering::SeqCst);
    diagram_click(visual, "changes-apply");
    wait_until(visual, "the apply to finish", |visual| view.read_with(visual, |view, _| view.session.draft.is_empty()));
}

#[gpui::test]
fn an_apply_opens_its_session_with_the_credentials_of_the_live_one_and_never_logs_them(cx: &mut TestAppContext) {
    cx.dispatcher.allow_parking();
    let (app, visual, _view) = d1_scenario(cx);
    app.update(visual, |this, _| {
        let tab = this.workspace.active_tab().unwrap();
        let connection = tab.connection_id.clone();
        let mut saved = this.saved_connections.iter().find(|saved| saved.matches_id(&connection)).cloned().expect("saved connection");
        // A connection whose password is not in the credential store (the user typed it at connect time).
        saved.save_password = true;
        saved.id = uuid::Uuid::new_v4();
        let key = saved.id.to_string();
        assert!(this.apply_session_uri(&saved, &key).is_err(), "nothing saved, nothing in memory: no credentials");
        let secret = "s3cr3t-typed-password";
        let uri = format!("postgres://app:{secret}@db.example.test/shop");
        this.driver_runtime.session_uris.insert(key.clone(), engine_connect::records::RuntimeUri::new(uri.clone()));
        assert_eq!(this.apply_session_uri(&saved, &key).unwrap().expose_for_test(), uri, "the live session's own URI");
        assert!(!format!("{:?}", this.driver_runtime.session_uris).contains(secret), "never printed");
        this.driver_runtime.session_uris.remove(&key);
        assert!(this.apply_session_uri(&saved, &key).is_err(), "gone with the session");
    });
}

#[gpui::test]
fn an_in_memory_sqlite_database_cannot_be_structure_edited(cx: &mut TestAppContext) {
    cx.dispatcher.allow_parking();
    let (app, visual, _view) = d1_scenario(cx);
    for uri in ["sqlite::memory:", "sqlite://:memory:", "file::memory:?cache=shared", "sqlite:///tmp/x.db?mode=memory"] {
        assert!(crate::app::structure_edit::apply::sqlite_is_in_memory(uri), "{uri}");
    }
    assert!(!crate::app::structure_edit::apply::sqlite_is_in_memory("sqlite:///Users/dev/app.db"));
    app.update(visual, |this, _| {
        let tab = this.workspace.active_tab().unwrap();
        let (connection, database, schema) = (tab.connection_id.clone(), tab.database.clone(), tab.schema.clone().filter(|s| !s.is_empty()));
        let saved = this.saved_connections.iter_mut().find(|saved| saved.matches_id(&connection)).expect("saved connection");
        assert_eq!(saved.kind, ConnectionKind::Sqlite, "the fixture connection of the diagram");
        saved.uri = "sqlite::memory:".into();
        let edit = this.structure_edit_context(&connection, &database, schema.as_deref()).expect("a SQL connection");
        let reason = edit.disabled.expect("editing is off");
        assert!(reason.contains("in-memory SQLite") && reason.contains("different, empty database"), "{reason}");
    });
}

// ---- live: the dedicated apply session (ignored without TABLEX_TEST_*_URI) --------------------------

mod live_apply {
    use super::*;
    use crate::app::structure_edit::apply::{run_apply, ApplyControl, ApplyJob, ApplyRun, ApplySession, DriverApplySession, Refusal};
    use crate::sql_ddl::{plan, ColumnSpec, DEFAULT_LOCK_TIMEOUT_SECS};
    use engine_driver_conformance::harness::{Conn, Ctx};
    use engine_driver_conformance::SqlFixture;

    /// A scratch namespace on a live server and the session of "the user".
    pub struct Live<'a> {
        pub ctx: &'a Ctx<'a>,
        pub kind: ConnectionKind,
        pub scope: EditScope,
        pub namespace: String,
    }

    impl Live<'_> {
        pub fn table(&self, name: &str) -> crate::sql_ddl::TableId {
            crate::sql_ddl::TableId { schema: Some(self.namespace.clone()), name: name.into() }
        }

        pub fn qualified(&self, name: &str) -> String {
            let quote = self.kind_quote();
            format!("{quote}{}{quote}.{quote}{name}{quote}", self.namespace)
        }

        fn kind_quote(&self) -> char {
            if matches!(self.kind, ConnectionKind::Postgres) {
                '"'
            } else {
                '`'
            }
        }

        /// The dedicated session an apply of this connection uses.
        pub fn session(&self, control: &std::sync::Arc<ApplyControl>) -> DriverApplySession {
            let database = (!matches!(self.kind, ConnectionKind::Postgres)).then(|| self.namespace.clone());
            DriverApplySession::connect_for_test(self.ctx.handle.clone(), &self.ctx.uri, database, self.kind, control.clone(), None).expect("a second session")
        }

        pub fn apply(&self, changes: Vec<Change>, lock_timeout_secs: u32) -> Result<ApplyRun, Refusal> {
            let dialect = crate::sql_ddl::dialect_for(self.kind).unwrap();
            let job = ApplyJob {
                kind: self.kind,
                scope: self.scope.clone(),
                reviewed_script: plan(dialect, &changes).script(),
                changes,
                lock_timeout_secs,
            };
            let control = std::sync::Arc::new(ApplyControl::default());
            let mut session = self.session(&control);
            run_apply(&job, &mut session)
        }

        /// The live spec of a column, exactly as the diagram would draft from it.
        pub fn column(&self, table: &str, column: &str) -> ColumnSpec {
            let control = std::sync::Arc::new(ApplyControl::default());
            let mut session = self.session(&control);
            let described = session.describe(&self.table(table), &self.scope).unwrap().expect("the table exists");
            described.columns.into_iter().find(|c| c.name == column).unwrap_or_else(|| panic!("column {column}"))
        }
    }

    fn live<'a>(ctx: &'a Ctx<'a>, kind: ConnectionKind, scratch: &engine_driver_conformance::Scratch) -> Live<'a> {
        let postgres = matches!(kind, ConnectionKind::Postgres);
        Live {
            ctx,
            kind,
            scope: EditScope {
                kind,
                database: if postgres { scratch.database.clone() } else { scratch.name.clone() },
                schema: postgres.then(|| scratch.name.clone()),
            },
            namespace: scratch.name.clone(),
        }
    }

    fn exec(conn: &mut Conn<'_>, sql: &str) {
        conn.exec(sql).unwrap_or_else(|failure| panic!("{sql}: {failure:?}"));
    }

    fn count(conn: &mut Conn<'_>, table: &str) -> i64 {
        conn.count(table).unwrap()
    }

    /// A user transaction open on the interactive session of the connection is neither committed
    /// nor rolled back by an apply, whether the apply succeeds or fails.
    fn user_transaction_untouched(
        fixture: &dyn SqlFixture,
        uri: &str,
        dylib: std::path::PathBuf,
        kind: ConnectionKind,
        begin: &str,
    ) {
        let ctx = Ctx::load(&dylib, uri, fixture).expect("load the driver");
        let scratch = ctx.scratch(false).expect("scratch namespace");
        let live = live(&ctx, kind, &scratch.scratch);
        let unique = ctx.unique();
        let mut user = ctx.connect().expect("the user's session");
        let mut peek = ctx.connect().expect("another reader");
        let (held, other, added) = (format!("held_{unique}"), format!("other_{unique}"), format!("added_{unique}"));
        let engine = if matches!(kind, ConnectionKind::Postgres) { "" } else { " ENGINE=InnoDB" };
        exec(&mut user, &format!("CREATE TABLE {} (id INT PRIMARY KEY){engine}", live.qualified(&held)));
        exec(&mut user, &format!("CREATE TABLE {} (id INT PRIMARY KEY){engine}", live.qualified(&added)));
        // The user's transaction: one uncommitted row.
        exec(&mut user, begin);
        exec(&mut user, &format!("INSERT INTO {} VALUES (1)", live.qualified(&held)));
        assert_eq!(count(&mut user, &live.qualified(&held)), 1);
        assert_eq!(count(&mut peek, &live.qualified(&held)), 0, "not committed yet");

        // A successful apply on its own session (a DDL statement implicitly commits on MySQL).
        let ok = live
            .apply(
                vec![
                    Change::CreateTable {
                        table: live.table(&other),
                        columns: vec![ColumnSpec { name: "id".into(), data_type: "int".into(), nullable: false, ..Default::default() }],
                        primary_key: vec!["id".into()],
                        comment: None,
                    },
                    Change::AddColumn {
                        table: live.table(&added),
                        column: ColumnSpec { name: "note".into(), data_type: "int".into(), nullable: true, ..Default::default() },
                    },
                ],
                DEFAULT_LOCK_TIMEOUT_SECS,
            )
            .ok()
            .expect("the apply runs");
        assert!(ok.outcome.succeeded(), "{}", ok.outcome.report(&ok.planned.steps()));
        assert_eq!(count(&mut peek, &live.qualified(&other)), 0, "the applied table exists");
        assert_eq!(count(&mut user, &live.qualified(&held)), 1, "the user's row is still there, in the user's transaction");
        assert_eq!(count(&mut peek, &live.qualified(&held)), 0, "and still uncommitted: the apply did not commit it");

        // A failing apply (the index names a column that is not there; on PostgreSQL the whole batch rolls back).
        let failed = live
            .apply(
                vec![
                    Change::AddColumn {
                        table: live.table(&added),
                        column: ColumnSpec { name: "second".into(), data_type: "int".into(), nullable: true, ..Default::default() },
                    },
                    Change::CreateIndex {
                        table: live.table(&added),
                        index: crate::sql_ddl::IndexSpec { name: format!("bad_{unique}"), columns: vec!["no_such_column".into()], unique: false },
                    },
                ],
                DEFAULT_LOCK_TIMEOUT_SECS,
            )
            .ok()
            .expect("the apply starts and fails");
        assert!(!failed.outcome.succeeded(), "the statement is refused by the server");
        assert_eq!(count(&mut user, &live.qualified(&held)), 1, "the failure did not roll the user's transaction back");
        assert_eq!(count(&mut peek, &live.qualified(&held)), 0, "nor commit it");
        // The user ends their own transaction as they please: the row goes.
        exec(&mut user, "ROLLBACK");
        assert_eq!(count(&mut user, &live.qualified(&held)), 0);
        assert_eq!(count(&mut peek, &live.qualified(&added)), 0, "tables intact");
    }

    #[test]
    #[ignore = "needs TABLEX_TEST_PG_URI"]
    fn postgres_user_transaction_is_untouched_by_an_apply() {
        let Ok(uri) = std::env::var("TABLEX_TEST_PG_URI") else { return };
        let dylib = crate::sql_ddl::live::dylib("dbengine-postgres", "dbengine_postgres", &["abi-v2"]);
        user_transaction_untouched(&engine_driver_conformance::fixtures::PostgresFixture, &uri, dylib, ConnectionKind::Postgres, "BEGIN");
    }

    #[test]
    #[ignore = "needs TABLEX_TEST_MYSQL_URI"]
    fn mysql_user_transaction_is_untouched_by_an_apply() {
        let Ok(uri) = std::env::var("TABLEX_TEST_MYSQL_URI") else { return };
        let dylib = crate::sql_ddl::live::dylib("dbengine-mysql", "dbengine_mysql", &["abi-v2"]);
        user_transaction_untouched(&engine_driver_conformance::fixtures::MysqlFixture::MYSQL, &uri, dylib, ConnectionKind::Mysql, "START TRANSACTION");
    }

    #[test]
    #[ignore = "needs TABLEX_TEST_MARIADB_URI"]
    fn mariadb_user_transaction_is_untouched_by_an_apply() {
        let Ok(uri) = std::env::var("TABLEX_TEST_MARIADB_URI") else { return };
        let dylib = crate::sql_ddl::live::dylib("dbengine-mariadb", "dbengine_mariadb", &["abi-v2"]);
        user_transaction_untouched(&engine_driver_conformance::fixtures::MysqlFixture::MARIADB, &uri, dylib, ConnectionKind::Mariadb, "START TRANSACTION");
    }

    /// Another session holds a lock: the apply fails with a clear lock timeout and rolls back.
    /// (1) on a table the change-set alters: the check's `LOCK TABLE` times out, nothing ran;
    /// (2) on a table a statement only references (a foreign key's parent): a statement times out
    /// inside the batch and what ran before it is rolled back.
    #[test]
    #[ignore = "needs TABLEX_TEST_PG_URI"]
    fn postgres_lock_timeout_fails_clearly_and_rolls_back() {
        let Ok(uri) = std::env::var("TABLEX_TEST_PG_URI") else { return };
        let dylib = crate::sql_ddl::live::dylib("dbengine-postgres", "dbengine_postgres", &["abi-v2"]);
        let ctx = Ctx::load(&dylib, &uri, &engine_driver_conformance::fixtures::PostgresFixture).expect("load the driver");
        let scratch = ctx.scratch(false).expect("scratch namespace");
        let live = live(&ctx, ConnectionKind::Postgres, &scratch.scratch);
        let unique = ctx.unique();
        let (locked, child, fresh) = (format!("locked_{unique}"), format!("child_{unique}"), format!("fresh_{unique}"));
        let mut holder = ctx.connect().unwrap();
        exec(&mut holder, &format!("CREATE TABLE {} (id INT PRIMARY KEY)", live.qualified(&locked)));
        exec(&mut holder, &format!("CREATE TABLE {} (id INT PRIMARY KEY, parent_id INT)", live.qualified(&child)));
        let new_table = || Change::CreateTable {
            table: live.table(&fresh),
            columns: vec![ColumnSpec { name: "id".into(), data_type: "int".into(), nullable: false, ..Default::default() }],
            primary_key: vec![],
            comment: None,
        };
        let fresh_exists = |ctx: &Ctx<'_>| {
            ctx.connect()
                .unwrap()
                .scalar(&format!("SELECT count(*) FROM information_schema.tables WHERE table_schema = '{}' AND table_name = '{fresh}'", live.namespace))
                .unwrap()
        };
        exec(&mut holder, "BEGIN");
        exec(&mut holder, &format!("LOCK TABLE {} IN ACCESS EXCLUSIVE MODE", live.qualified(&locked)));

        // (1) The alter's table is locked: the check's lock times out, nothing ran.
        let started = std::time::Instant::now();
        let refusal = live
            .apply(
                vec![
                    new_table(),
                    Change::AddColumn {
                        table: live.table(&locked),
                        column: ColumnSpec { name: "extra".into(), data_type: "int".into(), nullable: true, ..Default::default() },
                    },
                ],
                1,
            )
            .err()
            .expect("the check cannot lock the table");
        assert!(started.elapsed() < std::time::Duration::from_secs(8), "it did not wait for the lock for ever");
        assert!(refusal.message.contains("lock timeout") && refusal.message.contains("Nothing was changed"), "{}", refusal.message);
        assert_eq!(fresh_exists(&ctx), "0");

        // (2) A foreign key to the locked table: its ALTER waits for the parent's lock and times out in the batch.
        let started = std::time::Instant::now();
        let run = live
            .apply(
                vec![
                    new_table(),
                    Change::AddForeignKey {
                        table: live.table(&child),
                        foreign_key: crate::sql_ddl::ForeignKeySpec {
                            name: format!("fk_{unique}"),
                            columns: vec!["parent_id".into()],
                            ref_table: live.table(&locked),
                            ref_columns: vec!["id".into()],
                            on_delete: None,
                            on_update: None,
                        },
                    },
                ],
                1,
            )
            .unwrap_or_else(|refusal| panic!("the apply starts: {}", refusal.message));
        assert!(started.elapsed() < std::time::Duration::from_secs(8), "it did not wait for the lock for ever");
        assert!(!run.outcome.succeeded());
        let report = run.outcome.report(&run.planned.steps());
        assert!(report.contains("lock timeout"), "a clear timeout error: {report}");
        assert!(run.outcome.rolled_back && run.outcome.applied.is_empty(), "{report}");
        assert!(report.contains("rolled back"), "{report}");
        exec(&mut holder, "ROLLBACK");
        assert_eq!(fresh_exists(&ctx), "0", "the table created before the timeout was rolled back with the rest");
    }

    /// MySQL / MariaDB: `MODIFY COLUMN` keeps ON UPDATE, the character set and collation; a draft
    /// that is stale (the column was widened outside the app) is refused.
    fn restatement(fixture: &dyn SqlFixture, uri: &str, dylib: std::path::PathBuf, kind: ConnectionKind) {
        let ctx = Ctx::load(&dylib, uri, fixture).expect("load the driver");
        let scratch = ctx.scratch(false).expect("scratch namespace");
        let live = live(&ctx, kind, &scratch.scratch);
        let mut user = ctx.connect().unwrap();
        let table = format!("restated_{}", ctx.unique());
        exec(
            &mut user,
            &format!(
                "CREATE TABLE {} (id INT PRIMARY KEY, \
                 updated_at TIMESTAMP(3) NOT NULL DEFAULT CURRENT_TIMESTAMP(3) ON UPDATE CURRENT_TIMESTAMP(3), \
                 note VARCHAR(10) CHARACTER SET utf8mb4 COLLATE utf8mb4_bin NULL DEFAULT 'it''s', \
                 hidden INT NULL) ENGINE=InnoDB DEFAULT CHARSET=latin1 COLLATE=latin1_swedish_ci",
                live.qualified(&table)
            ),
        );
        let stamp = live.column(&table, "updated_at");
        assert!(stamp.attributes.as_ref().is_some_and(|a| a.extra.to_ascii_lowercase().contains("on update")), "{stamp:?}");
        // A comment-only edit of the ON UPDATE column keeps ON UPDATE (and the default).
        let commented = ColumnSpec { comment: Some("last write".into()), ..stamp.clone() };
        let run = live.apply(vec![Change::AlterColumn { table: live.table(&table), from: stamp.clone(), to: commented }], 10).ok().expect("runs");
        assert!(run.outcome.succeeded(), "{}", run.outcome.report(&run.planned.steps()));
        let after = live.column(&table, "updated_at");
        assert_eq!(after.comment.as_deref(), Some("last write"));
        assert_eq!(after.attributes.as_ref().map(|a| a.extra.to_ascii_lowercase()), stamp.attributes.as_ref().map(|a| a.extra.to_ascii_lowercase()));
        assert_eq!(after.default.as_deref().map(str::to_ascii_lowercase), stamp.default.as_deref().map(str::to_ascii_lowercase), "the default is untouched");
        let shown = user.rows(&format!("SHOW CREATE TABLE {}", live.qualified(&table))).unwrap();
        assert!(shown[0][1].to_ascii_lowercase().contains("on update current_timestamp(3)"), "{}", shown[0][1]);

        // A utf8mb4 / utf8mb4_bin column of a latin1 table keeps both when its type is widened.
        let note = live.column(&table, "note");
        let wider = ColumnSpec { data_type: "varchar(30)".into(), ..note.clone() };
        let run = live.apply(vec![Change::AlterColumn { table: live.table(&table), from: note.clone(), to: wider }], 10).ok().expect("runs");
        assert!(run.outcome.succeeded(), "{}", run.outcome.report(&run.planned.steps()));
        let kept = live.column(&table, "note");
        let attributes = kept.attributes.clone().unwrap();
        assert_eq!((attributes.character_set.as_deref(), attributes.collation.as_deref()), (Some("utf8mb4"), Some("utf8mb4_bin")), "{kept:?}");
        assert_eq!(kept.default.as_deref(), note.default.as_deref(), "the literal default is restated as printed");

        // The column is widened outside the app: the draft that started from varchar(30) is stale.
        exec(&mut user, &format!("ALTER TABLE {} MODIFY COLUMN note VARCHAR(50) CHARACTER SET utf8mb4 COLLATE utf8mb4_bin NULL DEFAULT 'it''s'", live.qualified(&table)));
        let refused = live
            .apply(
                vec![Change::AlterColumn {
                    table: live.table(&table),
                    from: kept.clone(),
                    to: ColumnSpec { comment: Some("x".into()), ..kept },
                }],
                10,
            )
            .err()
            .expect("a stale draft is refused");
        assert!(refused.message.contains("refresh the diagram") && refused.message.contains("varchar(50)"), "{}", refused.message);
        let now = live.column(&table, "note");
        assert_eq!(now.comment, None, "nothing ran");
    }

    #[test]
    #[ignore = "needs TABLEX_TEST_MYSQL_URI"]
    fn mysql_modify_keeps_on_update_charset_and_collation_and_refuses_a_stale_draft() {
        let Ok(uri) = std::env::var("TABLEX_TEST_MYSQL_URI") else { return };
        let dylib = crate::sql_ddl::live::dylib("dbengine-mysql", "dbengine_mysql", &["abi-v2"]);
        restatement(&engine_driver_conformance::fixtures::MysqlFixture::MYSQL, &uri, dylib, ConnectionKind::Mysql);
    }

    #[test]
    #[ignore = "needs TABLEX_TEST_MARIADB_URI"]
    fn mariadb_modify_keeps_on_update_charset_and_collation_and_refuses_a_stale_draft() {
        let Ok(uri) = std::env::var("TABLEX_TEST_MARIADB_URI") else { return };
        let dylib = crate::sql_ddl::live::dylib("dbengine-mariadb", "dbengine_mariadb", &["abi-v2"]);
        restatement(&engine_driver_conformance::fixtures::MysqlFixture::MARIADB, &uri, dylib, ConnectionKind::Mariadb);
    }

    /// A comment is a string literal, and with `standard_conforming_strings = off` a backslash in it
    /// escapes the next quote: the apply session says `on` for its own transaction, so a hostile
    /// comment stays a comment on a server (here: this database) that has it off.
    #[test]
    #[ignore = "needs TABLEX_TEST_PG_URI"]
    fn postgres_comments_stay_comments_on_a_server_with_standard_conforming_strings_off() {
        let Ok(uri) = std::env::var("TABLEX_TEST_PG_URI") else { return };
        let dylib = crate::sql_ddl::live::dylib("dbengine-postgres", "dbengine_postgres", &["abi-v2"]);
        let ctx = Ctx::load(&dylib, &uri, &engine_driver_conformance::fixtures::PostgresFixture).expect("load the driver");
        let scratch = ctx.scratch(false).expect("scratch namespace");
        let live = live(&ctx, ConnectionKind::Postgres, &scratch.scratch);
        let unique = ctx.unique();
        let (victim, target) = (format!("victim_{unique}"), format!("commented_{unique}"));
        let mut admin = ctx.connect().unwrap();
        exec(&mut admin, &format!("CREATE TABLE {} (id INT PRIMARY KEY)", live.qualified(&victim)));
        let database = scratch.scratch.database.clone();
        exec(&mut admin, &format!("ALTER DATABASE \"{database}\" SET standard_conforming_strings = off"));
        let hostile = format!("\\'; DROP TABLE {}; --", live.qualified(&victim));
        let outcome = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            // Sessions opened from here on have the setting off (the apply opens its own).
            let mut probe = ctx.connect().unwrap();
            assert_eq!(probe.scalar("SHOW standard_conforming_strings").unwrap(), "off", "the server default is off for new sessions");
            let run = live
                .apply(
                    vec![Change::CreateTable {
                        table: live.table(&target),
                        columns: vec![ColumnSpec {
                            name: "id".into(),
                            data_type: "int".into(),
                            nullable: false,
                            comment: Some(hostile.clone()),
                            ..Default::default()
                        }],
                        primary_key: vec!["id".into()],
                        comment: Some(hostile.clone()),
                    }],
                    10,
                )
                .ok()
                .expect("the apply runs");
            assert!(run.outcome.succeeded(), "{}", run.outcome.report(&run.planned.steps()));
            let mut exists = |name: &str| {
                probe
                    .scalar(&format!("SELECT count(*) FROM information_schema.tables WHERE table_schema = '{}' AND table_name = '{name}'", live.namespace))
                    .unwrap()
            };
            assert_eq!(exists(&victim), "1", "the hostile comment dropped nothing");
            assert_eq!(exists(&target), "1");
            let stored = probe.scalar(&format!("SELECT obj_description('{}'::regclass)", live.qualified(&target))).unwrap();
            assert_eq!(stored, hostile, "the comment is stored as typed");
        }));
        exec(&mut admin, &format!("ALTER DATABASE \"{database}\" RESET standard_conforming_strings"));
        if let Err(panic) = outcome {
            std::panic::resume_unwind(panic);
        }
    }

    /// PostgreSQL defaults the catalog prints survive a type change verbatim.
    #[test]
    #[ignore = "needs TABLEX_TEST_PG_URI"]
    fn postgres_defaults_survive_a_type_change() {
        let Ok(uri) = std::env::var("TABLEX_TEST_PG_URI") else { return };
        let dylib = crate::sql_ddl::live::dylib("dbengine-postgres", "dbengine_postgres", &["abi-v2"]);
        let ctx = Ctx::load(&dylib, &uri, &engine_driver_conformance::fixtures::PostgresFixture).expect("load the driver");
        let scratch = ctx.scratch(false).expect("scratch namespace");
        let live = live(&ctx, ConnectionKind::Postgres, &scratch.scratch);
        let mut user = ctx.connect().unwrap();
        let table = format!("defaults_{}", ctx.unique());
        exec(&mut user, &format!("CREATE SEQUENCE {}.seq_{table}", live.namespace));
        exec(
            &mut user,
            &format!(
                "CREATE TABLE {} (a VARCHAR(10) DEFAULT NULL::character varying, b VARCHAR(40) DEFAULT CURRENT_USER, \
                 c INT DEFAULT nextval('{}.seq_{table}'::regclass))",
                live.qualified(&table),
                live.namespace
            ),
        );
        let before: Vec<ColumnSpec> = ["a", "b", "c"].iter().map(|c| live.column(&table, c)).collect();
        let changes = vec![
            Change::AlterColumn { table: live.table(&table), from: before[0].clone(), to: ColumnSpec { data_type: "text".into(), ..before[0].clone() } },
            Change::AlterColumn { table: live.table(&table), from: before[1].clone(), to: ColumnSpec { data_type: "text".into(), ..before[1].clone() } },
            Change::AlterColumn { table: live.table(&table), from: before[2].clone(), to: ColumnSpec { data_type: "bigint".into(), ..before[2].clone() } },
        ];
        let run = live.apply(changes, 10).ok().expect("runs");
        assert!(run.outcome.succeeded(), "{}", run.outcome.report(&run.planned.steps()));
        for (index, name) in ["a", "b", "c"].iter().enumerate() {
            let after = live.column(&table, name);
            assert_eq!(after.default, before[index].default, "{name}: the default is what it was");
        }
    }
}

#[gpui::test]
fn undo_and_redo_of_the_draft_restore_the_exact_changes_after_edits_that_share_them(cx: &mut TestAppContext) {
    use crate::sql_ddl::{ColumnSpec, TableId};
    let (view, visual) = d3_window(cx);
    let table = TableId { schema: Some("public".into()), name: "orders".into() };
    let column = |name: &str| ColumnSpec { name: name.into(), data_type: "integer".into(), nullable: true, ..ColumnSpec::default() };
    let draft = |view: &gpui::Entity<DiagramView>, visual: &mut gpui::VisualTestContext| view.read_with(visual, |view, _| view.session.draft.changes.clone());
    let mut seen = vec![draft(&view, visual)];
    let steps = vec![
        Change::AddColumn { table: table.clone(), column: column("a") },
        Change::AddColumn { table: table.clone(), column: column("b") },
        Change::DropColumn { table: table.clone(), column: "note".into() },
        Change::RenameTable { table: table.clone(), to: "purchases".into() },
    ];
    for change in steps {
        view.update(visual, |view, cx| assert!(view.edit_draft(cx, |draft| draft.push(change))));
        seen.push(draft(&view, visual));
    }
    for expected in seen.iter().rev().skip(1) {
        view.update(visual, |view, cx| view.undo(cx));
        assert_eq!(draft(&view, visual), *expected);
    }
    for expected in seen.iter().skip(1) {
        view.update(visual, |view, cx| view.redo(cx));
        assert_eq!(draft(&view, visual), *expected);
    }
    // An apply took the first change for good: the steps before it are gone, the later ones are exact.
    let applied = vec![seen[4][0].clone()];
    view.update(visual, |view, _| view.session.forget_applied(&applied));
    assert!(!view.read_with(visual, |view, _| view.session.history.can_undo()));
    let rest = draft(&view, visual);
    view.update(visual, |view, cx| assert!(view.edit_draft(cx, |draft| draft.push(Change::AddColumn { table: table.clone(), column: column("late") }))));
    let with_late = draft(&view, visual);
    view.update(visual, |view, cx| view.undo(cx));
    assert_eq!(draft(&view, visual), rest);
    view.update(visual, |view, cx| view.redo(cx));
    assert_eq!(draft(&view, visual), with_late);
}
