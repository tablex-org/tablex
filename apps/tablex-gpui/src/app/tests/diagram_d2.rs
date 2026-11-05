// D2 (annotate) interactions of the ER diagram, driven through the real
// events: drag a column onto another table's column, the relation editor,
// context menus, the suggestions / layers / inspector panels, notes, colors,
// groups with multi-select, undo / redo by button and by shortcut, and the
// saved document read back by a fresh view.

use crate::app::results::diagram::annotations::{Rgb, SWATCHES};
use crate::app::results::diagram::layout::DiagramPoint;
use crate::app::results::diagram::model::{DiagramGraph, RelationOrigin, RelatedProperties};
use crate::app::results::diagram::store::{DiagramKey, LayoutStore};
use crate::app::results::diagram::view::Panel;

/// `(name, type, primary key)`.
type D2Column = (&'static str, &'static str, bool);

fn d2_properties(columns: &[D2Column], fks: &[(&str, &str)]) -> crate::catalog::TableProperties {
    use crate::catalog::{CatalogColumn, CatalogForeignKey, TableProperties};
    TableProperties {
        columns: columns
            .iter()
            .map(|&(name, ty, pk)| CatalogColumn {
                name: name.into(),
                data_type: ty.into(),
                nullable: !pk,
                primary_key: pk,
                ..Default::default()
            })
            .collect(),
        foreign_keys: Some(
            fks.iter()
                .map(|&(column, target)| CatalogForeignKey {
                    name: format!("fk_{column}"),
                    table: String::new(),
                    columns: vec![column.into()],
                    referenced_table: format!("public.{target}"),
                    referenced_columns: vec!["id".into()],
                    on_update: None,
                    on_delete: None,
                    definition: None,
                })
                .collect(),
        ),
        ..Default::default()
    }
}

/// Five tables: one declared foreign key (`order_items.product_id`) and three
/// columns named after a table without one (`orders.customers_id`,
/// `order_items.orders_id`, `customers.region_id`).
fn d2_snapshot() -> DiagramSnapshot {
    let table = |name: &str| TableRef { database: "db".into(), schema: Some("public".into()), table: name.into() };
    let ready = |p| RelatedProperties::Ready(std::sync::Arc::new(p));
    let id: D2Column = ("id", "integer", true);
    let entries = vec![
        (
            table("customers"),
            ready(d2_properties(&[id, ("name", "text", false), ("region_id", "integer", false)], &[])),
        ),
        (table("regions"), ready(d2_properties(&[id, ("name", "text", false)], &[]))),
        (
            table("orders"),
            ready(d2_properties(
                &[id, ("customers_id", "integer", false), ("shipped_by", "integer", false), ("note", "text", false)],
                &[],
            )),
        ),
        (
            table("order_items"),
            ready(d2_properties(
                &[id, ("orders_id", "integer", false), ("product_id", "integer", false), ("sku", "text", false)],
                &[("product_id", "products")],
            )),
        ),
        (table("products"), ready(d2_properties(&[id, ("name", "text", false)], &[]))),
    ];
    DiagramSnapshot {
        graph: std::sync::Arc::new(DiagramGraph::build_schema(&entries)),
        generation: 1,
        error: None,
        progress: None,
        truncated: None,
    }
}

fn d2_key() -> DiagramKey {
    DiagramKey { connection: "c".into(), database: "db".into(), schema: Some("public".into()), table: None }
}

/// A schema `DiagramView` over [`d2_snapshot`] with the app's key bindings.
fn d2_window(cx: &mut TestAppContext) -> (gpui::Entity<DiagramView>, &mut gpui::VisualTestContext) {
    cx.update(|app| {
        gpui_component::init(app);
        theme::init(app);
        actions::bind_keys(app);
        // Dialogs (Changes, confirmations) slide in on the wall clock; with motion reduced they settle on
        // their first frame, so a click never aims at a control that is still moving (a missed click on
        // Copy SQL under load was the flaky failure here).
        app.set_reduce_motion(true);
    });
    let (root, visual) = cx.add_window_view(|window, cx| {
        let view = cx.new(|cx| DiagramView::new(crate::app::results::diagram::view::DiagramKind::Schema, cx));
        gpui_component::Root::new(view, window, cx)
    });
    let view = visual.cx.read(|cx| root.read(cx).view().clone().downcast::<DiagramView>().unwrap());
    visual.simulate_resize(gpui::size(gpui::px(1400.0), gpui::px(850.0)));
    visual.update(|window, _| window.activate_window());
    view.update(visual, |view, cx| view.set_snapshot(Some(d2_snapshot()), cx));
    settle(visual);
    (view, visual)
}

/// Like [`d2_window`], saving to `dir`; the document is read first.
fn d2_stored_window<'a>(
    cx: &'a mut TestAppContext,
    dir: &std::path::Path,
) -> (gpui::Entity<DiagramView>, &'a mut gpui::VisualTestContext) {
    let (view, visual) = d2_window(cx);
    let store = std::sync::Arc::new(LayoutStore::new(dir.to_path_buf()));
    view.update(visual, |view, cx| view.set_persistence(Some((store, d2_key())), cx));
    settle(visual);
    (view, visual)
}

fn d2_entity(view: &gpui::Entity<DiagramView>, visual: &mut gpui::VisualTestContext, name: &str) -> usize {
    view.read_with(visual, |view, _| {
        view.snapshot.as_ref().unwrap().graph.entities.iter().position(|e| e.table.table == name).unwrap()
    })
}

fn d2_column(view: &gpui::Entity<DiagramView>, visual: &mut gpui::VisualTestContext, entity: &str, column: &str) -> (usize, usize) {
    let e = d2_entity(view, visual, entity);
    let c = view.read_with(visual, |view, _| {
        view.snapshot.as_ref().unwrap().graph.entities[e].columns.iter().position(|c| c.name == column).unwrap()
    });
    (e, c)
}

fn d2_row(visual: &mut gpui::VisualTestContext, (entity, column): (usize, usize)) -> gpui::Bounds<gpui::Pixels> {
    let selector: &'static str = Box::leak(format!("diagram-row-{entity}-{column}").into_boxed_str());
    visual.debug_bounds(selector).unwrap_or_else(|| panic!("row {entity}.{column} is drawn"))
}

/// Drags from the link band at the right edge of one row to the middle of another.
fn d2_link(visual: &mut gpui::VisualTestContext, from: (usize, usize), to: (usize, usize)) {
    let start = d2_row(visual, from);
    let end = d2_row(visual, to);
    let at = gpui::point(start.right() - gpui::px(6.0), start.center().y);
    drag(visual, at, end.center() - at);
}

fn d2_relations(view: &gpui::Entity<DiagramView>, visual: &mut gpui::VisualTestContext) -> usize {
    view.read_with(visual, |view, _| view.snapshot.as_ref().unwrap().graph.relations.len())
}

fn d2_logical(view: &gpui::Entity<DiagramView>, visual: &mut gpui::VisualTestContext) -> Vec<(String, String, bool, bool)> {
    view.read_with(visual, |view, _| {
        view.annotations
            .relations
            .iter()
            .map(|r| (r.child.columns.join(","), r.parent.table.clone(), r.optional, r.unique))
            .collect()
    })
}

/// Clicks the middle of an entity's box.
fn d2_click_entity(visual: &mut gpui::VisualTestContext, entity: usize, modifiers: gpui::Modifiers) {
    let bounds = entity_bounds(visual, entity);
    visual.simulate_click(bounds.center(), modifiers);
}

/// Drags a link between two columns by name.
fn d2_link_named(
    view: &gpui::Entity<DiagramView>,
    visual: &mut gpui::VisualTestContext,
    (from_table, from_column): (&str, &str),
    (to_table, to_column): (&str, &str),
) {
    let from = d2_column(view, visual, from_table, from_column);
    let to = d2_column(view, visual, to_table, to_column);
    d2_link(visual, from, to);
}

/// Whether the element with this debug selector is drawn.
fn d2_has(visual: &mut gpui::VisualTestContext, selector: String) -> bool {
    visual.debug_bounds(Box::leak(selector.into_boxed_str())).is_some()
}

fn d2_right_click(visual: &mut gpui::VisualTestContext, at: gpui::Point<gpui::Pixels>) {
    visual.simulate_mouse_move(at, None, Default::default());
    visual.simulate_mouse_down(at, gpui::MouseButton::Right, Default::default());
    visual.simulate_mouse_up(at, gpui::MouseButton::Right, Default::default());
    visual.run_until_parked();
}

/// A point of the canvas that no entity, note, group or line covers: near
/// the top-left corner of the area (the diagram is fitted and centered).
fn d2_empty_point(view: &gpui::Entity<DiagramView>, visual: &mut gpui::VisualTestContext) -> gpui::Point<gpui::Pixels> {
    let area = visual.debug_bounds("diagram-area").unwrap();
    let at = area.origin + gpui::point(gpui::px(24.0), gpui::px(24.0));
    let taken = view.read_with(visual, |view, _| {
        let local = DiagramPoint { x: 24.0, y: 24.0 };
        view.entity_at_for_test(local)
    });
    assert!(!taken, "the corner of the area is empty");
    at
}

#[gpui::test]
fn dragging_a_column_onto_another_creates_a_logical_relationship_you_can_edit_delete_undo_and_redo(cx: &mut TestAppContext) {
    let (view, visual) = d2_window(cx);
    let declared = d2_relations(&view, visual);
    assert_eq!(declared, 1);
    let from = d2_column(&view, visual, "orders", "shipped_by");
    let to = d2_column(&view, visual, "customers", "id");

    // The link handle shows on the hovered row (and only there).
    assert!(!d2_has(visual, format!("diagram-link-{}-{}", from.0, from.1)));
    let row = d2_row(visual, from);
    visual.simulate_mouse_move(row.center(), None, Default::default());
    settle(visual);
    assert!(d2_has(visual, format!("diagram-link-{}-{}", from.0, from.1)), "the handle of the hovered row");
    assert!(!d2_has(visual, format!("diagram-link-{}-{}", to.0, to.1)));

    d2_link(visual, from, to);
    assert_eq!(d2_relations(&view, visual), declared + 1);
    view.read_with(visual, |view, _| {
        let graph = &view.snapshot.as_ref().unwrap().graph;
        let relation = graph.relations.last().unwrap();
        assert!(matches!(relation.origin, RelationOrigin::Logical(_)));
        assert_eq!((relation.child.0, relation.child_columns.clone()), (from.0, vec![from.1]));
        assert_eq!((relation.parent.0, relation.parent_columns.clone()), (to.0, vec![to.1]));
        assert_eq!(view.selected_relation, Some(graph.relations.len() - 1), "the new relationship is selected");
        assert_eq!(view.routes.len(), graph.relations.len(), "it is routed (and drawn dashed)");
        assert_eq!(view.session.history.undo_len(), 1);
    });
    assert_eq!(d2_logical(&view, visual), [("shipped_by".to_string(), "customers".to_string(), true, false)]);
    assert!(visual.debug_bounds("diagram-relation-bar").is_some(), "the editor is next to it");

    // Dragging the same pair again is refused with a message.
    d2_link(visual, from, to);
    assert_eq!(d2_relations(&view, visual), declared + 1);
    assert!(view.read_with(visual, |view, _| view.notice.as_ref().is_some_and(|(text, error)| *error && text.contains("already exists"))));

    // Cardinality: one-to-one, then mandatory.
    diagram_click(visual, "relation-one");
    assert!(d2_logical(&view, visual)[0].3, "one to one");
    diagram_click(visual, "relation-optional");
    assert_eq!(d2_logical(&view, visual), [("shipped_by".to_string(), "customers".to_string(), false, true)]);
    view.read_with(visual, |view, _| {
        let relation = view.snapshot.as_ref().unwrap().graph.relations.last().unwrap();
        assert_eq!((relation.optional, relation.unique), (false, true), "the drawing follows");
    });

    // Undo by button: mandatory <- one-to-one <- created <- nothing.
    diagram_click(visual, "diagram-undo");
    assert!(d2_logical(&view, visual)[0].2);
    diagram_click(visual, "diagram-undo");
    assert!(!d2_logical(&view, visual)[0].3);
    diagram_click(visual, "diagram-undo");
    assert!(d2_logical(&view, visual).is_empty());
    assert_eq!(d2_relations(&view, visual), declared);
    assert!(visual.debug_bounds("diagram-relation-bar").is_none());
    // Redo restores them in order.
    diagram_click(visual, "diagram-redo");
    assert_eq!(d2_relations(&view, visual), declared + 1);
    diagram_click(visual, "diagram-redo");
    diagram_click(visual, "diagram-redo");
    assert_eq!(d2_logical(&view, visual), [("shipped_by".to_string(), "customers".to_string(), false, true)]);

    // Delete from the editor, then bring it back with undo.
    let bar_was_open = visual.debug_bounds("diagram-relation-bar").is_some();
    if !bar_was_open {
        // Select the relation by clicking its line.
        let at = view.read_with(visual, |view, _| {
            let route = view.routes.last().unwrap();
            DiagramPoint { x: route.points[1].x, y: (route.points[1].y + route.points[2].y) / 2.0 }
        });
        let point = d1_window_point(visual, &view, at);
        visual.simulate_click(point, Default::default());
        settle(visual);
    }
    diagram_click(visual, "relation-delete");
    assert!(d2_logical(&view, visual).is_empty());
    assert_eq!(d2_relations(&view, visual), declared);
    diagram_click(visual, "diagram-undo");
    assert_eq!(d2_logical(&view, visual).len(), 1, "undo brings the deleted relationship back");
}

#[gpui::test]
fn the_context_menu_edits_and_deletes_a_logical_relationship_and_leaves_catalog_ones_alone(cx: &mut TestAppContext) {
    let (view, visual) = d2_window(cx);
    d2_link_named(&view, visual, ("orders", "shipped_by"), ("customers", "id"));
    let line_point = |view: &gpui::Entity<DiagramView>, visual: &mut gpui::VisualTestContext, last: bool| {
        let at = view.read_with(visual, |view, _| {
            let route = if last { view.routes.last().unwrap() } else { view.routes.first().unwrap() };
            DiagramPoint { x: route.points[1].x, y: (route.points[1].y + route.points[2].y) / 2.0 }
        });
        d1_window_point(visual, view, at)
    };

    // A catalog foreign key: only Copy JOIN SQL.
    let declared_at = line_point(&view, visual, false);
    let declared_index = view.read_with(visual, |view, _| view.routes.first().unwrap().relation);
    assert!(view.read_with(visual, |view, _| view.snapshot.as_ref().unwrap().graph.relations[declared_index].origin == RelationOrigin::Declared));
    d2_right_click(visual, declared_at);
    visual.simulate_keystrokes("down");
    visual.simulate_keystrokes("enter");
    settle(visual);
    let copied = visual.cx.read_from_clipboard().and_then(|item| item.text()).expect("the first item copies the JOIN");
    assert!(copied.contains("JOIN"), "{copied}");

    // The logical one: cardinality submenu, then delete.
    let at = line_point(&view, visual, true);
    d2_right_click(visual, at);
    // Copy JOIN SQL, (separator), Cardinality > ..., Delete.
    visual.simulate_keystrokes("down down right");
    // Items of the submenu (the first is selected on opening): Many to one,
    // Many to one (optional), One to one, One to one (optional).
    visual.simulate_keystrokes("down down enter");
    settle(visual);
    assert_eq!(d2_logical(&view, visual), [("shipped_by".to_string(), "customers".to_string(), false, true)], "One to one");
    d2_right_click(visual, at);
    visual.simulate_keystrokes("down down down enter");
    settle(visual);
    assert!(d2_logical(&view, visual).is_empty(), "Delete logical relationship");
    visual.simulate_keystrokes(&primary("z"));
    settle(visual);
    assert_eq!(d2_logical(&view, visual).len(), 1, "the shortcut undoes the delete");
}

#[gpui::test]
fn the_suggestions_panel_confirms_ignores_and_restores_inferred_relationships(cx: &mut TestAppContext) {
    let (view, visual) = d2_window(cx);
    let names = |view: &gpui::Entity<DiagramView>, visual: &mut gpui::VisualTestContext| {
        view.read_with(visual, |view, _| {
            let graph = &view.snapshot.as_ref().unwrap().graph;
            view.suggestions
                .iter()
                .map(|s| {
                    format!(
                        "{}.{}->{}",
                        graph.entities[s.child.0].table.table,
                        graph.entities[s.child.0].columns[s.child_column].name,
                        graph.entities[s.parent.0].table.table
                    )
                })
                .collect::<Vec<_>>()
        })
    };
    let mut found = names(&view, visual);
    found.sort();
    assert_eq!(found, ["customers.region_id->regions", "order_items.orders_id->orders", "orders.customers_id->customers"]);
    assert!(visual.debug_bounds("diagram-dock").is_none(), "closed by default");
    diagram_click(visual, "diagram-suggestions");
    assert_eq!(view.read_with(visual, |view, _| view.panel), Some(Panel::Suggestions));
    for index in 0..3 {
        let selector: &'static str = Box::leak(format!("suggestion-{index}").into_boxed_str());
        assert!(visual.debug_bounds(selector).is_some(), "row {index}");
    }
    let first = names(&view, visual)[0].clone();

    // Confirm the first: it becomes a logical relationship and leaves the list.
    diagram_click(visual, "suggestion-confirm-0");
    assert_eq!(names(&view, visual).len(), 2);
    assert!(!names(&view, visual).contains(&first));
    assert_eq!(d2_relations(&view, visual), 2);
    assert_eq!(d2_logical(&view, visual).len(), 1);

    // Ignore the next: gone, and "Restore ignored" appears.
    let ignored = names(&view, visual)[0].clone();
    diagram_click(visual, "suggestion-ignore-0");
    assert!(!names(&view, visual).contains(&ignored));
    assert_eq!(d2_relations(&view, visual), 2, "ignoring adds nothing");
    assert_eq!(view.read_with(visual, |view, _| view.annotations.ignored.len()), 1);
    assert!(visual.debug_bounds("suggestions-restore").is_some());
    diagram_click(visual, "suggestions-restore");
    assert!(names(&view, visual).contains(&ignored), "restored");
    assert!(visual.debug_bounds("suggestions-restore").is_none());

    // Ignore all, undo, Confirm all (one undo step each).
    diagram_click(visual, "suggestions-ignore-all");
    assert!(names(&view, visual).is_empty());
    assert!(visual.debug_bounds("suggestions-empty").is_some());
    diagram_click(visual, "diagram-undo");
    assert_eq!(names(&view, visual).len(), 2);
    diagram_click(visual, "suggestions-confirm-all");
    assert!(names(&view, visual).is_empty());
    assert_eq!(d2_relations(&view, visual), 4, "1 catalog + 3 logical");
    diagram_click(visual, "diagram-undo");
    assert_eq!(d2_relations(&view, visual), 2, "Confirm all is one step");
    assert_eq!(names(&view, visual).len(), 2);
}

#[gpui::test]
fn notes_are_added_edited_moved_resized_deleted_and_undone(cx: &mut TestAppContext) {
    let (view, visual) = d2_window(cx);
    let empty = d2_empty_point(&view, visual);
    d2_right_click(visual, empty);
    visual.simulate_keystrokes("down enter"); // Add note
    settle(visual);
    let id = view.read_with(visual, |view, _| {
        assert_eq!(view.annotations.notes.len(), 1);
        view.annotations.notes[0].id
    });
    let selector: &'static str = Box::leak(format!("diagram-note-{id}").into_boxed_str());
    assert!(visual.debug_bounds(selector).is_some(), "the note is on the canvas");

    // The editor has the focus: type, then click the canvas to finish.
    visual.simulate_input("Ship by Friday");
    settle(visual);
    let area = visual.debug_bounds("diagram-area").unwrap();
    visual.simulate_click(area.origin + gpui::point(gpui::px(6.0), gpui::px(6.0)), Default::default());
    settle(visual);
    assert_eq!(view.read_with(visual, |view, _| view.annotations.notes[0].text.clone()), "Ship by Friday");

    // Move it by its body.
    let note = visual.debug_bounds(selector).unwrap();
    let before = view.read_with(visual, |view, _| (view.annotations.notes[0].x, view.annotations.notes[0].y));
    drag(visual, note.origin + gpui::point(gpui::px(30.0), gpui::px(30.0)), gpui::point(gpui::px(40.0), gpui::px(25.0)));
    let after = view.read_with(visual, |view, _| (view.annotations.notes[0].x, view.annotations.notes[0].y));
    let zoom = zoom_of(&view, visual);
    assert!(((after.0 - before.0) * zoom - 40.0).abs() < 0.5 && ((after.1 - before.1) * zoom - 25.0).abs() < 0.5, "{before:?} -> {after:?}");

    // Resize by the corner.
    let note = visual.debug_bounds(selector).unwrap();
    let size_before = view.read_with(visual, |view, _| (view.annotations.notes[0].width, view.annotations.notes[0].height));
    drag(visual, note.bottom_right() - gpui::point(gpui::px(4.0), gpui::px(4.0)), gpui::point(gpui::px(60.0), gpui::px(30.0)));
    let size_after = view.read_with(visual, |view, _| (view.annotations.notes[0].width, view.annotations.notes[0].height));
    assert!(size_after.0 > size_before.0 + 50.0 / zoom - 1.0 && size_after.1 > size_before.1 + 25.0 / zoom - 1.0, "{size_before:?} -> {size_after:?}");

    // Double-click edits the text again.
    let note = visual.debug_bounds(selector).unwrap();
    diagram_double_click(visual, note.center());
    assert!(view.read_with(visual, |view, _| view.note_edit_open()));
    visual.simulate_keystrokes("secondary-a");
    visual.simulate_input("Rewritten");
    assert_eq!(view.read_with(visual, |view, cx| view.note_edit_text(cx)).as_deref(), Some("Rewritten"));
    visual.simulate_click(area.origin + gpui::point(gpui::px(6.0), gpui::px(6.0)), Default::default());
    settle(visual);
    assert_eq!(view.read_with(visual, |view, _| view.annotations.notes[0].text.clone()), "Rewritten");

    // Undo walks back: text, resize, move, text, creation.
    let texts = |view: &gpui::Entity<DiagramView>, visual: &mut gpui::VisualTestContext| view.read_with(visual, |view, _| view.annotations.notes.first().map(|n| n.text.clone()));
    diagram_click(visual, "diagram-undo");
    assert_eq!(texts(&view, visual).as_deref(), Some("Ship by Friday"));
    diagram_click(visual, "diagram-undo");
    assert_eq!(view.read_with(visual, |view, _| view.annotations.notes[0].width), size_before.0, "the resize is one step");
    diagram_click(visual, "diagram-undo");
    assert_eq!(view.read_with(visual, |view, _| (view.annotations.notes[0].x, view.annotations.notes[0].y)), before, "the move is one step");
    diagram_click(visual, "diagram-undo");
    assert_eq!(texts(&view, visual).as_deref(), Some(""));
    diagram_click(visual, "diagram-undo");
    assert_eq!(texts(&view, visual), None, "the note is gone");
    for _ in 0..5 {
        diagram_click(visual, "diagram-redo");
    }
    assert_eq!(texts(&view, visual).as_deref(), Some("Rewritten"));

    // Delete from the context menu.
    let note = visual.debug_bounds(selector).unwrap();
    d2_right_click(visual, note.origin + gpui::point(gpui::px(20.0), gpui::px(20.0)));
    visual.simulate_keystrokes("down down enter"); // Edit note, Delete note
    settle(visual);
    assert_eq!(texts(&view, visual), None);
    assert!(visual.debug_bounds(selector).is_none());
}

#[gpui::test]
fn an_entity_color_is_set_from_the_context_menu_reset_and_undone(cx: &mut TestAppContext) {
    let (view, visual) = d2_window(cx);
    let regions = d2_entity(&view, visual, "regions");
    let color = |view: &gpui::Entity<DiagramView>, visual: &mut gpui::VisualTestContext| {
        view.read_with(visual, |view, _| view.entity_color(crate::app::results::diagram::model::EntityId(regions)))
    };
    assert_eq!(color(&view, visual), None);
    let bounds = entity_bounds(visual, regions);
    // regions has no relations, so the menu starts with Color.
    d2_right_click(visual, bounds.origin + gpui::point(gpui::px(30.0), gpui::px(10.0)));
    visual.simulate_keystrokes("down right");
    // Blue is selected on opening; Green, Amber, Red: pick the 4th.
    visual.simulate_keystrokes("down down down enter");
    settle(visual);
    assert_eq!(color(&view, visual), Some(SWATCHES[3].1));
    assert_eq!(view.read_with(visual, |view, _| view.annotations.colors.len()), 1);

    // Reset (after the eight swatches and the separator).
    d2_right_click(visual, bounds.origin + gpui::point(gpui::px(30.0), gpui::px(10.0)));
    visual.simulate_keystrokes("down right");
    visual.simulate_keystrokes("down down down down down down down down enter");
    settle(visual);
    assert_eq!(color(&view, visual), None);
    diagram_click(visual, "diagram-undo");
    assert_eq!(color(&view, visual), Some(SWATCHES[3].1), "undo brings the color back");
    diagram_click(visual, "diagram-undo");
    assert_eq!(color(&view, visual), None);
}

#[gpui::test]
fn groups_form_from_a_multi_selection_hide_collapse_and_move_as_a_whole(cx: &mut TestAppContext) {
    let (view, visual) = d2_window(cx);
    let (customers, regions, orders) =
        (d2_entity(&view, visual, "customers"), d2_entity(&view, visual, "regions"), d2_entity(&view, visual, "orders"));

    // Shift-click adds to the selection and takes out again.
    let shift = gpui::Modifiers { shift: true, ..Default::default() };
    d2_click_entity(visual, customers, Default::default());
    d2_click_entity(visual, regions, shift);
    assert_eq!(view.read_with(visual, |view, _| view.selection.iter().map(|id| id.0).collect::<Vec<_>>()), {
        let mut ids = vec![customers, regions];
        ids.sort_unstable();
        ids
    });
    d2_click_entity(visual, orders, shift);
    assert_eq!(view.read_with(visual, |view, _| view.selection.len()), 3);
    d2_click_entity(visual, orders, shift);
    assert_eq!(view.read_with(visual, |view, _| view.selection.len()), 2, "shift-click again deselects");

    // A shift-drag on empty canvas selects what the band touches.
    let empty = d2_empty_point(&view, visual);
    visual.simulate_click(empty, Default::default());
    assert!(view.read_with(visual, |view, _| view.selection.is_empty()));
    let area = visual.debug_bounds("diagram-area").unwrap();
    let (a, b) = (entity_bounds(visual, customers), entity_bounds(visual, regions));
    let top_left = area.origin + gpui::point(gpui::px(2.0), gpui::px(2.0));
    let corner = gpui::point(a.right().max(b.right()) + gpui::px(4.0), a.bottom().max(b.bottom()) + gpui::px(4.0));
    visual.simulate_mouse_down(top_left, gpui::MouseButton::Left, shift);
    visual.simulate_mouse_move(top_left + (corner - top_left) * 0.5, gpui::MouseButton::Left, shift);
    visual.simulate_mouse_move(corner, gpui::MouseButton::Left, shift);
    assert!(visual.debug_bounds("diagram-marquee").is_some(), "the rubber band is drawn");
    visual.simulate_mouse_up(corner, gpui::MouseButton::Left, shift);
    settle(visual);
    let selected = view.read_with(visual, |view, _| view.selection.iter().map(|id| id.0).collect::<Vec<_>>());
    assert!(selected.contains(&customers) && selected.contains(&regions), "{selected:?}");

    // Dragging one of several selected entities moves them all.
    let (customers_before, regions_before) = (entity_bounds(visual, customers), entity_bounds(visual, regions));
    let outside_other = view.read_with(visual, |view, _| view.selection.len());
    assert!(outside_other >= 2);
    drag(visual, customers_before.center(), gpui::point(gpui::px(0.0), gpui::px(60.0)));
    assert_eq!(entity_bounds(visual, customers).origin.y - customers_before.origin.y, gpui::px(60.0));
    assert_eq!(entity_bounds(visual, regions).origin.y - regions_before.origin.y, gpui::px(60.0), "the whole selection moved");
    diagram_click(visual, "diagram-undo");
    assert_eq!(entity_bounds(visual, customers).origin, customers_before.origin, "one undo step for the whole move");
    assert_eq!(entity_bounds(visual, regions).origin, regions_before.origin);

    // Group the selection from the Layers panel.
    diagram_click(visual, "diagram-layers");
    assert!(visual.debug_bounds("layers-empty").is_some());
    diagram_click(visual, "layers-new");
    let group = view.read_with(visual, |view, _| {
        assert_eq!(view.annotations.groups.len(), 1);
        assert_eq!(view.annotations.groups[0].members.len(), outside_other);
        view.annotations.groups[0].id
    });
    let row: &'static str = Box::leak(format!("layer-row-{group}").into_boxed_str());
    assert!(visual.debug_bounds(row).is_some());
    let frame_selector: &'static str = Box::leak(format!("diagram-group-{group}").into_boxed_str());
    let frame = visual.debug_bounds(frame_selector).expect("the group frame is drawn behind its tables");
    let (customers_box, regions_box) = (entity_bounds(visual, customers), entity_bounds(visual, regions));
    assert!(frame.contains(&customers_box.origin) && frame.contains(&regions_box.bottom_right()), "the frame surrounds its tables");

    // Moving the group by its title bar moves every member.
    drag(visual, frame.origin + gpui::point(gpui::px(80.0), gpui::px(8.0)), gpui::point(gpui::px(20.0), gpui::px(30.0)));
    assert_eq!(entity_bounds(visual, customers).origin - customers_box.origin, gpui::point(gpui::px(20.0), gpui::px(30.0)));
    assert_eq!(entity_bounds(visual, regions).origin - regions_box.origin, gpui::point(gpui::px(20.0), gpui::px(30.0)));
    let orders_box = entity_bounds(visual, orders);
    assert_eq!(orders_box, entity_bounds(visual, orders), "entities outside the group stay");

    // Rename in the panel.
    let row_bounds = visual.debug_bounds(row).unwrap();
    diagram_double_click(visual, row_bounds.origin + gpui::point(gpui::px(60.0), gpui::px(10.0)));
    visual.simulate_keystrokes("secondary-a");
    visual.simulate_input("Reference data");
    visual.simulate_keystrokes("enter");
    settle(visual);
    assert_eq!(view.read_with(visual, |view, _| view.annotations.groups[0].name.clone()), "Reference data");

    // Hide: its tables and their lines are not drawn.
    let visible: &'static str = Box::leak(format!("layer-visible-{group}").into_boxed_str());
    diagram_click(visual, visible);
    assert!(!d2_has(visual, format!("diagram-entity-{customers}")), "hidden");
    assert!(visual.debug_bounds(frame_selector).is_none());
    assert_eq!(view.read_with(visual, |view, _| view.groups.hidden.iter().filter(|h| **h).count()), outside_other);
    diagram_click(visual, visible);
    assert!(visual.debug_bounds(frame_selector).is_some(), "shown again");

    // Collapse: one chip instead of the tables.
    let collapse: &'static str = Box::leak(format!("layer-collapse-{group}").into_boxed_str());
    diagram_click(visual, collapse);
    let chip = visual.debug_bounds(frame_selector).expect("the chip");
    assert!(chip.size.width < frame.size.width + gpui::px(1.0) && chip.size.height < gpui::px(50.0), "{chip:?}");
    let selector: &'static str = Box::leak(format!("diagram-entity-{regions}").into_boxed_str());
    assert!(visual.debug_bounds(selector).is_none());
    diagram_click(visual, collapse);
    assert!(visual.debug_bounds(selector).is_some());

    // Undo the collapse and the rename, then delete (ungroup) the group.
    diagram_click(visual, "diagram-undo");
    diagram_click(visual, "diagram-undo");
    diagram_click(visual, "diagram-undo");
    diagram_click(visual, "diagram-undo");
    diagram_click(visual, "diagram-undo");
    assert_eq!(view.read_with(visual, |view, _| view.annotations.groups[0].name.clone()), "Group 1", "rename undone");
    let delete: &'static str = Box::leak(format!("layer-delete-{group}").into_boxed_str());
    diagram_click(visual, delete);
    assert!(view.read_with(visual, |view, _| view.annotations.groups.is_empty()));
    assert!(visual.debug_bounds(frame_selector).is_none());
}

#[gpui::test]
fn the_inspector_shows_the_selected_tables_columns_and_indexes(cx: &mut TestAppContext) {
    let (view, visual) = d2_window(cx);
    diagram_click(visual, "diagram-inspector");
    assert!(visual.debug_bounds("inspector-empty").is_some(), "nothing selected yet");
    let orders = d2_entity(&view, visual, "orders");
    d2_click_entity(visual, orders, Default::default());
    settle(visual);
    assert!(visual.debug_bounds("inspector-title").is_some());
    let columns = view.read_with(visual, |view, _| view.snapshot.as_ref().unwrap().graph.entities[orders].columns.len());
    for index in 0..columns {
        let selector: &'static str = Box::leak(format!("inspector-column-{index}").into_boxed_str());
        assert!(visual.debug_bounds(selector).is_some(), "column {index}");
    }
    let customers = d2_entity(&view, visual, "customers");
    d2_click_entity(visual, customers, Default::default());
    settle(visual);
    let customers_columns = view.read_with(visual, |view, _| view.snapshot.as_ref().unwrap().graph.entities[customers].columns.len());
    assert!(customers_columns < columns);
    assert!(d2_has(visual, format!("inspector-column-{}", customers_columns - 1)));
    assert!(!d2_has(visual, format!("inspector-column-{customers_columns}")), "the list follows the selection");
    diagram_click(visual, "diagram-inspector");
    assert!(visual.debug_bounds("diagram-dock").is_none(), "the toggle closes it");
}

#[gpui::test]
fn undo_and_redo_shortcuts_work_on_the_canvas_but_not_inside_a_text_field(cx: &mut TestAppContext) {
    let (view, visual) = d2_window(cx);
    let customers = d2_entity(&view, visual, "customers");
    let before = d1_rects(&view, visual)[customers];
    let bounds = entity_bounds(visual, customers);
    drag(visual, bounds.center(), gpui::point(gpui::px(50.0), gpui::px(20.0)));
    let moved = d1_rects(&view, visual)[customers];
    assert_ne!(moved, before);
    let at = |view: &gpui::Entity<DiagramView>, visual: &mut gpui::VisualTestContext| d1_rects(view, visual)[customers];

    // Typing in the search field: its own undo runs, the diagram's does not.
    diagram_click(visual, "diagram-search");
    visual.simulate_input("abc");
    settle(visual);
    assert_eq!(view.read_with(visual, |view, _| view.search_text_len()), 3);
    visual.simulate_keystrokes(&primary("z"));
    settle(visual);
    assert_eq!(at(&view, visual), moved, "the text field kept the chord");
    assert_eq!(view.read_with(visual, |view, _| view.session.history.undo_len()), 1);
    assert_eq!(view.read_with(visual, |view, _| view.search_text_len()), 0, "and undid its own text");

    // On the canvas the chord undoes the move, Shift redoes it; so does Ctrl+Y off macOS.
    let area = visual.debug_bounds("diagram-area").unwrap();
    visual.simulate_click(area.origin + gpui::point(gpui::px(6.0), gpui::px(6.0)), Default::default());
    visual.simulate_keystrokes(&primary("z"));
    settle(visual);
    assert_eq!(at(&view, visual), before, "undone");
    visual.simulate_keystrokes(&primary("shift-z"));
    settle(visual);
    assert_eq!(at(&view, visual), moved, "redone");
    if !cfg!(target_os = "macos") {
        visual.simulate_keystrokes(&primary("z"));
        visual.simulate_keystrokes(&primary("y"));
        settle(visual);
        assert_eq!(at(&view, visual), moved);
    }

    // Re-layout is an undoable step, too.
    let dragged = d1_rects(&view, visual);
    diagram_click(visual, "diagram-relayout");
    assert_ne!(d1_rects(&view, visual)[customers], dragged[customers]);
    diagram_click(visual, "diagram-undo");
    assert_eq!(d1_rects(&view, visual), dragged, "undo brings the dragged layout back");
    diagram_click(visual, "diagram-redo");
    assert_ne!(d1_rects(&view, visual)[customers], dragged[customers]);
}

#[gpui::test]
fn every_annotation_is_saved_and_a_fresh_view_loads_it(cx: &mut TestAppContext) {
    cx.dispatcher.allow_parking();
    let dir = d1_temp_dir("d2");
    let (view, visual) = d2_stored_window(cx, &dir);

    // A logical relationship, a note, a color and a group; one ignored suggestion.
    d2_link_named(&view, visual, ("orders", "shipped_by"), ("customers", "id"));
    let customers = d2_entity(&view, visual, "customers");
    view.update(visual, |view, cx| {
        view.set_entity_color(&[crate::app::results::diagram::model::EntityId(customers)], Some(Rgb(0x10b981)), cx);
        view.group_entities(&[crate::app::results::diagram::model::EntityId(customers)], cx);
        view.ignore_suggestion(0, cx);
    });
    let empty = d2_empty_point(&view, visual);
    d2_right_click(visual, empty);
    visual.simulate_keystrokes("down enter");
    settle(visual);
    visual.simulate_input("Persist me");
    let area = visual.debug_bounds("diagram-area").unwrap();
    visual.simulate_click(area.origin + gpui::point(gpui::px(6.0), gpui::px(6.0)), Default::default());
    settle(visual);
    let expected = view.read_with(visual, |view, _| (*view.annotations).clone());
    assert_eq!(expected.relations.len(), 1);
    assert_eq!((expected.notes.len(), expected.colors.len(), expected.groups.len(), expected.ignored.len()), (1, 1, 1, 1));

    // Debounced: nothing yet, then one file.
    let files = |dir: &std::path::Path| std::fs::read_dir(dir).map(|entries| entries.count()).unwrap_or(0);
    assert_eq!(files(&dir), 0);
    visual.cx.executor().advance_clock(crate::app::results::diagram::view::SAVE_DEBOUNCE + std::time::Duration::from_millis(10));
    wait_until(visual, "the saved document", |_| files(&dir) == 1);
    settle(visual);

    // A new view of the same diagram reads it all back, drawn.
    let (fresh, visual) = d2_stored_window(cx, &dir);
    wait_until(visual, "the annotations to load", |visual| fresh.read_with(visual, |view, _| !view.annotations.is_empty()));
    settle(visual);
    assert_eq!(fresh.read_with(visual, |view, _| (*view.annotations).clone()), expected);
    fresh.read_with(visual, |view, _| {
        let graph = &view.snapshot.as_ref().unwrap().graph;
        assert_eq!(graph.relations.len(), 2, "the logical relationship is back in the graph");
        assert!(matches!(graph.relations.last().unwrap().origin, RelationOrigin::Logical(_)));
        assert!(view.entity_color(crate::app::results::diagram::model::EntityId(customers)).is_some());
        assert_eq!(view.suggestions.len(), 2, "the ignored suggestion stays ignored");
        assert_eq!(view.session.history.undo_len(), 0, "history is not persisted");
    });
    let note_id = fresh.read_with(visual, |view, _| view.annotations.notes[0].id);
    let selector: &'static str = Box::leak(format!("diagram-note-{note_id}").into_boxed_str());
    assert!(visual.debug_bounds(selector).is_some(), "the note is drawn");
    let group_id = fresh.read_with(visual, |view, _| view.annotations.groups[0].id);
    let selector: &'static str = Box::leak(format!("diagram-group-{group_id}").into_boxed_str());
    assert!(visual.debug_bounds(selector).is_some(), "the group frame is drawn");

    // Deleting everything leaves nothing worth a file: it is removed.
    fresh.update(visual, |view, cx| {
        view.edit_annotations(cx, |a| *a = Default::default());
    });
    visual.cx.executor().advance_clock(crate::app::results::diagram::view::SAVE_DEBOUNCE + std::time::Duration::from_millis(10));
    wait_until(visual, "the file to go", |_| files(&dir) == 0);
    std::fs::remove_dir_all(dir).ok();
}

#[gpui::test]
fn a_table_diagram_reopens_with_its_annotations(cx: &mut TestAppContext) {
    cx.dispatcher.allow_parking();
    let dir = d1_temp_dir("d2-app");
    // The Chinook `table-diagram` scenario: Track with its neighbours.
    let (app, visual, view) = d1_scenario(cx);
    let tab_id = app.read_with(visual, |this, _| this.workspace.active_tab().unwrap().id);
    d1_use_store(&app, visual, &dir);
    settle(visual);
    wait_until(visual, "the (empty) document to be read", |visual| view.read_with(visual, |view, _| view.persisting()));
    let declared = d2_relations(&view, visual);

    // A logical relationship, a note by the context menu and a color.
    d2_link_named(&view, visual, ("InvoiceLine", "Quantity"), ("MediaType", "MediaTypeId"));
    assert_eq!(d2_relations(&view, visual), declared + 1);
    let empty = d2_empty_point(&view, visual);
    d2_right_click(visual, empty);
    visual.simulate_keystrokes("down enter");
    settle(visual);
    visual.simulate_input("remember");
    let area = visual.debug_bounds("diagram-area").unwrap();
    visual.simulate_click(area.origin + gpui::point(gpui::px(6.0), gpui::px(6.0)), Default::default());
    settle(visual);
    let album = d2_entity(&view, visual, "Album");
    view.update(visual, |view, cx| {
        view.set_entity_color(&[crate::app::results::diagram::model::EntityId(album)], Some(SWATCHES[1].1), cx)
    });
    let expected = view.read_with(visual, |view, _| (*view.annotations).clone());
    assert_eq!((expected.relations.len(), expected.notes.len(), expected.colors.len()), (1, 1, 1));
    let files = |dir: &std::path::Path| std::fs::read_dir(dir).map(|entries| entries.count()).unwrap_or(0);
    visual.cx.executor().advance_clock(crate::app::results::diagram::view::SAVE_DEBOUNCE + std::time::Duration::from_millis(10));
    wait_until(visual, "the saved document", |_| files(&dir) == 1);

    // The tab's view goes away (as when the app restarts); a new one reads the file.
    app.update(visual, |this, cx| {
        this.diagram_views.remove(&tab_id);
        cx.notify();
    });
    settle(visual);
    let reopened = diagram_view(&app, visual);
    wait_until(visual, "the annotations to load", |visual| reopened.read_with(visual, |view, _| !view.annotations.is_empty()));
    settle(visual);
    assert_eq!(reopened.read_with(visual, |view, _| (*view.annotations).clone()), expected);
    assert_eq!(d2_relations(&reopened, visual), declared + 1, "the logical relationship is drawn again");
    let note_id = reopened.read_with(visual, |view, _| view.annotations.notes[0].id);
    assert!(d2_has(visual, format!("diagram-note-{note_id}")), "and the note");
    assert!(reopened.read_with(visual, |view, _| view.entity_color(crate::app::results::diagram::model::EntityId(album))).is_some());
    // The suggestions panel works on the real catalog too: nothing undeclared in Chinook.
    assert!(reopened.read_with(visual, |view, _| view.suggestions.is_empty()));

    // Re-layout keeps the annotations and only forgets positions.
    diagram_click(visual, "diagram-relayout");
    assert_eq!(reopened.read_with(visual, |view, _| view.annotations.relations.len()), 1);
    visual.cx.executor().advance_clock(std::time::Duration::from_millis(50));
    settle(visual);
    assert_eq!(files(&dir), 1, "the file stays: the annotations are still in it");
    std::fs::remove_dir_all(dir).ok();
}

#[gpui::test]
fn only_the_row_edge_or_alt_starts_a_link_and_a_plain_drag_still_moves_the_table(cx: &mut TestAppContext) {
    let (view, visual) = d2_window(cx);
    let declared = d2_relations(&view, visual);
    let orders = d2_column(&view, visual, "orders", "shipped_by");
    let customers = d2_column(&view, visual, "customers", "id");

    // A plain drag from the middle of a row moves the table, no link.
    let row = d2_row(visual, orders);
    let before = entity_bounds(visual, orders.0);
    drag(visual, row.center(), gpui::point(gpui::px(0.0), gpui::px(40.0)));
    assert_eq!(entity_bounds(visual, orders.0).origin.y - before.origin.y, gpui::px(40.0));
    assert_eq!(d2_relations(&view, visual), declared);
    assert!(view.read_with(visual, |view, _| view.annotations.relations.is_empty()));

    // Alt starts a link from anywhere on the row, and the table stays.
    let alt = gpui::Modifiers { alt: true, ..Default::default() };
    let (from, to) = (d2_row(visual, orders), d2_row(visual, customers));
    let table = entity_bounds(visual, orders.0);
    let start = from.origin + gpui::point(gpui::px(30.0), from.size.height / 2.0);
    visual.simulate_mouse_down(start, gpui::MouseButton::Left, alt);
    visual.simulate_mouse_move(to.center(), gpui::MouseButton::Left, alt);
    assert!(view.read_with(visual, |view, _| matches!(view.drag, Some(crate::app::results::diagram::view::Drag::Link { .. }))));
    visual.simulate_mouse_up(to.center(), gpui::MouseButton::Left, alt);
    settle(visual);
    assert_eq!(d2_relations(&view, visual), declared + 1);
    assert_eq!(entity_bounds(visual, orders.0), table, "the table did not move");

    // Dropping a link on empty canvas (or on its own row) adds nothing.
    let empty = d2_empty_point(&view, visual);
    let note_column = d2_column(&view, visual, "orders", "note");
    let from = d2_row(visual, note_column);
    let at = gpui::point(from.right() - gpui::px(6.0), from.center().y);
    drag(visual, at, empty - at);
    assert_eq!(view.read_with(visual, |view, _| view.annotations.relations.len()), 1);
    drag(visual, at, gpui::point(gpui::px(-3.0), gpui::px(0.0)));
    assert_eq!(view.read_with(visual, |view, _| view.annotations.relations.len()), 1);
}

#[gpui::test]
fn escape_in_the_note_editor_keeps_the_text_and_returns_the_focus_to_the_diagram(cx: &mut TestAppContext) {
    let (view, visual) = d2_window(cx);
    let empty = d2_empty_point(&view, visual);
    d2_right_click(visual, empty);
    visual.simulate_keystrokes("down enter");
    settle(visual);
    visual.simulate_input("keep me");
    visual.simulate_keystrokes("escape");
    settle(visual);
    assert!(!view.read_with(visual, |view, _| view.note_edit_open()), "the editor closed");
    assert_eq!(view.read_with(visual, |view, _| view.annotations.notes[0].text.clone()), "keep me");
    // The diagram has the focus: its undo shortcut works now.
    visual.simulate_keystrokes(&primary("z"));
    settle(visual);
    assert_eq!(view.read_with(visual, |view, _| view.annotations.notes[0].text.clone()), "", "the text edit is undone");
}

#[gpui::test]
fn the_group_context_menu_recolors_hides_collapses_and_ungroups(cx: &mut TestAppContext) {
    use crate::app::results::diagram::model::EntityId;
    let (view, visual) = d2_window(cx);
    let (customers, regions) = (d2_entity(&view, visual, "customers"), d2_entity(&view, visual, "regions"));
    let group = view
        .update(visual, |view, cx| view.group_entities(&[EntityId(customers), EntityId(regions)], cx))
        .expect("a group");
    settle(visual);
    let frame_selector: &'static str = Box::leak(format!("diagram-group-{group}").into_boxed_str());
    let title_point = |visual: &mut gpui::VisualTestContext| {
        let frame = visual.debug_bounds(frame_selector).expect("the group frame");
        frame.origin + gpui::point(gpui::px(60.0), gpui::px(8.0))
    };
    let color = |view: &gpui::Entity<DiagramView>, visual: &mut gpui::VisualTestContext| {
        view.read_with(visual, |view, _| view.group(group).map(|g| g.color.clone()))
    };
    assert_eq!(color(&view, visual).as_deref(), Some(SWATCHES[0].1.hex().as_str()));

    // Color > Amber (Rename, Color; the submenu opens on Blue).
    let at = title_point(visual);
    d2_right_click(visual, at);
    visual.simulate_keystrokes("down down right down down enter");
    settle(visual);
    assert_eq!(color(&view, visual).as_deref(), Some(SWATCHES[2].1.hex().as_str()));

    // Collapse / expand, hide, ungroup.
    let at = title_point(visual);
    d2_right_click(visual, at);
    visual.simulate_keystrokes("down down down down enter");
    settle(visual);
    assert!(view.read_with(visual, |view, _| view.group(group).unwrap().collapsed));
    assert!(!d2_has(visual, format!("diagram-entity-{regions}")), "the entities are replaced by the chip");
    let at = title_point(visual);
    d2_right_click(visual, at);
    visual.simulate_keystrokes("down down down down enter");
    settle(visual);
    assert!(d2_has(visual, format!("diagram-entity-{regions}")), "expanded again");
    let at = title_point(visual);
    d2_right_click(visual, at);
    visual.simulate_keystrokes("down down down enter");
    settle(visual);
    assert!(view.read_with(visual, |view, _| view.group(group).unwrap().hidden));
    assert!(!d2_has(visual, frame_selector.to_string()) && !d2_has(visual, format!("diagram-entity-{customers}")));
    diagram_click(visual, "diagram-undo");
    let at = title_point(visual);
    d2_right_click(visual, at);
    visual.simulate_keystrokes("down down down down down enter");
    settle(visual);
    assert!(view.read_with(visual, |view, _| view.group(group).is_none()), "Ungroup");
    assert!(d2_has(visual, format!("diagram-entity-{customers}")), "the tables stay");
}

#[gpui::test]
fn only_the_notes_and_groups_in_view_are_built_as_elements(cx: &mut TestAppContext) {
    use crate::app::results::diagram::annotations::{TableKey, SWATCHES};
    let (view, visual) = d2_window(cx);
    let origin = view.read_with(visual, |view, _| view.viewport.origin);
    view.update(visual, |view, cx| {
        view.edit_annotations(cx, |a| {
            // 50 notes: 10 x 5 in view, and one far away.
            for n in 0..50 {
                a.add_note(origin.x + 10.0 + (n % 10) as f32 * 130.0, origin.y + 10.0 + (n / 10) as f32 * 120.0);
            }
            let far = a.add_note(origin.x + 100_000.0, origin.y);
            a.note_mut(far).unwrap().text = "far away".into();
            a.add_group("Near".into(), SWATCHES[0].1, vec![TableKey { schema: Some("public".into()), table: "customers".into() }]);
        });
    });
    settle(visual);
    let (ids, far) = view.read_with(visual, |view, _| {
        let ids: Vec<u64> = view.annotations.notes.iter().map(|n| n.id).collect();
        (ids.clone(), *ids.last().unwrap())
    });
    let drawn = ids.iter().filter(|id| d2_has(visual, format!("diagram-note-{id}"))).count();
    assert!(drawn >= 45, "the notes in view are drawn ({drawn} of 50)");
    assert!(!d2_has(visual, format!("diagram-note-{far}")), "a note far outside the view is not built");
    let group = view.read_with(visual, |view, _| view.annotations.groups[0].id);
    assert!(d2_has(visual, format!("diagram-group-{group}")));
}

#[gpui::test]
fn escape_in_the_group_name_field_keeps_the_old_name(cx: &mut TestAppContext) {
    use crate::app::results::diagram::model::EntityId;
    let (view, visual) = d2_window(cx);
    let customers = d2_entity(&view, visual, "customers");
    let group = view.update(visual, |view, cx| view.group_entities(&[EntityId(customers)], cx)).unwrap();
    diagram_click(visual, "diagram-layers");
    let row = visual.debug_bounds(Box::leak(format!("layer-row-{group}").into_boxed_str())).unwrap();
    diagram_double_click(visual, row.origin + gpui::point(gpui::px(60.0), gpui::px(10.0)));
    visual.simulate_keystrokes("secondary-a");
    visual.simulate_input("Discarded");
    visual.simulate_keystrokes("escape");
    settle(visual);
    assert_eq!(view.read_with(visual, |view, _| view.annotations.groups[0].name.clone()), "Group 1");
    assert_eq!(view.read_with(visual, |view, _| view.session.history.undo_len()), 1, "only the grouping was recorded");
}

const TOOLBAR_CONTROLS: [&str; 17] = [
    "diagram-zoom-out",
    "diagram-zoom-label",
    "diagram-zoom-presets",
    "diagram-zoom-in",
    "diagram-fit",
    "diagram-relayout",
    "diagram-refresh",
    "diagram-mode",
    "diagram-notation",
    "diagram-search",
    "diagram-export",
    "diagram-undo",
    "diagram-redo",
    "diagram-suggestions",
    "diagram-layers",
    "diagram-inspector",
    "diagram-overflow",
];

/// Resizes the window to `width` and checks the toolbar: the zoom field and
/// the search field keep their widths, nothing drawn leaves the toolbar.
/// Returns the controls that are drawn.
fn d2_check_toolbar(visual: &mut gpui::VisualTestContext, width: f32) -> Vec<&'static str> {
    visual.simulate_resize(gpui::size(gpui::px(width), gpui::px(850.0)));
    settle(visual);
    let toolbar = visual.debug_bounds("diagram-toolbar").expect("the toolbar");
    let mut drawn = Vec::new();
    for selector in TOOLBAR_CONTROLS {
        let Some(bounds) = visual.debug_bounds(selector) else {
            continue;
        };
        drawn.push(selector);
        assert!(
            bounds.left() >= toolbar.left() - gpui::px(0.5) && bounds.right() <= toolbar.right() + gpui::px(0.5),
            "{selector} at {width}: {bounds:?} leaves the toolbar {toolbar:?}"
        );
    }
    let zoom = visual.debug_bounds("diagram-zoom-label").expect("zoom field");
    let search = visual.debug_bounds("diagram-search").expect("search field");
    assert!(zoom.size.width >= gpui::px(58.0), "zoom field at {width}: {zoom:?}");
    assert!(search.size.width >= gpui::px(140.0), "search field at {width}: {search:?}");
    for always in ["diagram-zoom-out", "diagram-zoom-in", "diagram-fit", "diagram-export"] {
        assert!(drawn.contains(&always), "{always} stays at {width}");
    }
    drawn
}

#[gpui::test]
fn the_toolbar_collapses_lower_priority_controls_into_an_overflow_menu_instead_of_squeezing(cx: &mut TestAppContext) {
    let (view, visual) = d2_window(cx);
    let level = |view: &gpui::Entity<DiagramView>, visual: &mut gpui::VisualTestContext| view.read_with(visual, |view, _| view.toolbar_level());

    // Wide: everything is there and nothing is in the overflow menu.
    let drawn = d2_check_toolbar(visual, 1400.0);
    assert_eq!(level(&view, visual), 0);
    for control in TOOLBAR_CONTROLS.iter().filter(|c| **c != "diagram-overflow") {
        assert!(drawn.contains(control), "{control} at 1400");
    }
    assert!(!drawn.contains(&"diagram-overflow"));

    // A result pane (~900): the least important controls (Crow's foot, then
    // the visible-columns dropdown) moved into the menu.
    let drawn = d2_check_toolbar(visual, 900.0);
    assert!(level(&view, visual) >= 1, "900 does not fit everything");
    assert!(drawn.contains(&"diagram-overflow") && !drawn.contains(&"diagram-notation"));
    assert!(drawn.contains(&"diagram-search") && drawn.contains(&"diagram-zoom-label"));

    // Narrow (~640): only the fixed controls and the menu are left.
    let drawn = d2_check_toolbar(visual, 640.0);
    assert_eq!(level(&view, visual), 4);
    for gone in ["diagram-mode", "diagram-notation", "diagram-undo", "diagram-redo", "diagram-suggestions", "diagram-layers", "diagram-inspector"] {
        assert!(!drawn.contains(&gone), "{gone} collapsed at 640");
    }
    assert!(drawn.contains(&"diagram-overflow"));

    // Everything collapsed is reachable and works through the menu.
    let customers = d2_entity(&view, visual, "customers");
    view.update(visual, |view, cx| {
        view.group_entities(&[crate::app::results::diagram::model::EntityId(customers)], cx);
    });
    settle(visual);
    let open_overflow = |visual: &mut gpui::VisualTestContext| {
        diagram_click(visual, "diagram-overflow");
    };
    // Undo.
    open_overflow(visual);
    diagram_click(visual, "diagram-undo");
    assert!(view.read_with(visual, |view, _| view.annotations.groups.is_empty()), "undo through the menu");
    open_overflow(visual);
    diagram_click(visual, "diagram-redo");
    assert_eq!(view.read_with(visual, |view, _| view.annotations.groups.len()), 1, "redo through the menu");
    // Notation and visible columns.
    let notation = view.read_with(visual, |view, _| view.notation);
    open_overflow(visual);
    diagram_click(visual, "diagram-notation");
    assert_ne!(view.read_with(visual, |view, _| view.notation), notation);
    open_overflow(visual);
    diagram_click(visual, "diagram-mode-1");
    assert_eq!(view.read_with(visual, |view, _| view.mode), DiagramMode::KeysOnly);
    // Panels.
    for (selector, panel) in [("diagram-suggestions", Panel::Suggestions), ("diagram-layers", Panel::Layers), ("diagram-inspector", Panel::Inspector)] {
        open_overflow(visual);
        diagram_click(visual, selector);
        assert_eq!(view.read_with(visual, |view, _| view.panel), Some(panel), "{selector} through the menu");
    }
    open_overflow(visual);
    diagram_click(visual, "diagram-inspector");
    assert_eq!(view.read_with(visual, |view, _| view.panel), None, "the entry toggles the panel off");

    // Growing the window brings the controls back.
    let drawn = d2_check_toolbar(visual, 1400.0);
    assert!(drawn.contains(&"diagram-mode") && drawn.contains(&"diagram-undo") && !drawn.contains(&"diagram-overflow"));
}

#[gpui::test]
fn the_toolbar_fits_a_table_diagram_in_a_result_pane_and_a_full_width_tab(cx: &mut TestAppContext) {
    // The Chinook table diagram in a result pane, as in the window of the capture.
    let (_app, visual, _view) = d1_scenario(cx);
    for width in [1400.0, 1100.0, 900.0] {
        d2_check_toolbar(visual, width);
    }
}
