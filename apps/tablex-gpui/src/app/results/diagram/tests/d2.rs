//! D2 unit tests: relationship inference, annotations, undo history, the
//! store (including a D1-format file) and the exports of what the user added.

use std::sync::Arc;

use super::d1::{fixture, request};
use crate::sql_ddl::draft::Draft;
use super::*;
use crate::app::results::diagram::annotations::{
    cardinality_label, group_frame, Annotations, AnnotationError, GroupIndex, IgnoredLink, RelationEnd, Rgb,
    TableKey, SWATCHES,
};
use crate::app::results::diagram::export::{ExportFormat, ExportRequest};
use crate::app::results::diagram::history::{Edit, History, MoveStep, HISTORY_LIMIT};
use crate::app::results::diagram::join_sql;
use crate::app::results::diagram::infer::{suggest, Level, NameMatch, TypeMatch, MAX_SUGGESTIONS};
use crate::app::results::diagram::model::RelationOrigin;
use crate::app::results::diagram::store::{DiagramKey, LayoutStore, SavedDiagram};

// ---- inference ---------------------------------------------------------------------------

fn column(name: &str, ty: &str, pk: bool) -> CatalogColumn {
    CatalogColumn { name: name.into(), data_type: ty.into(), nullable: !pk, primary_key: pk, ..Default::default() }
}

fn properties(columns: Vec<CatalogColumn>) -> TableProperties {
    TableProperties { columns, ..Default::default() }
}

/// Every table `Ready`, in the whole-schema graph.
fn schema(tables: Vec<(&str, TableProperties)>) -> DiagramGraph {
    let entries: Vec<_> = tables.into_iter().map(|(name, p)| (table(name), ready(p))).collect();
    DiagramGraph::build_schema(&entries)
}

fn pair(graph: &DiagramGraph, s: &crate::app::results::diagram::infer::Suggestion) -> String {
    let (child, parent) = (&graph.entities[s.child.0], &graph.entities[s.parent.0]);
    format!(
        "{}.{} -> {}.{}",
        child.table.table, child.columns[s.child_column].name, parent.table.table, parent.columns[s.parent_column].name
    )
}

fn pairs(graph: &DiagramGraph, annotations: &Annotations) -> Vec<String> {
    suggest(graph, annotations).iter().map(|s| pair(graph, s)).collect()
}

#[test]
fn inference_follows_the_naming_conventions() {
    let graph = schema(vec![
        // snake_case, camelCase, PascalCase and UPPERCASE key columns.
        ("users", properties(vec![column("id", "integer", true), column("name", "text", false)])),
        ("categories", properties(vec![column("category_id", "integer", true)])),
        ("Media_Type", properties(vec![column("MediaTypeId", "integer", true)])),
        (
            "orders",
            properties(vec![
                column("id", "integer", true),
                column("user_id", "integer", false),
                column("categoryId", "integer", false),
                column("MediaTypeID", "integer", false),
                column("note", "text", false),
            ]),
        ),
    ]);
    let found = pairs(&graph, &Annotations::default());
    assert!(found.contains(&"orders.user_id -> users.id".to_string()), "{found:?}");
    assert!(found.contains(&"orders.categoryId -> categories.category_id".to_string()), "singular/plural: {found:?}");
    assert!(found.contains(&"orders.MediaTypeID -> Media_Type.MediaTypeId".to_string()), "case and underscores fold: {found:?}");
    assert_eq!(found.len(), 3, "plain columns and primary keys are not candidates: {found:?}");
}

#[test]
fn inference_matches_singular_plural_and_ignores_unrelated_names() {
    let graph = schema(vec![
        ("person", properties(vec![column("id", "uuid", true)])),
        ("boxes", properties(vec![column("id", "uuid", true)])),
        ("people_group", properties(vec![column("id", "uuid", true)])),
        (
            "x",
            properties(vec![
                column("id", "uuid", true),
                column("person_id", "uuid", false),
                column("boxes_id", "uuid", false),
                column("box_id", "uuid", false),
                column("valid", "uuid", false),
                column("grid", "uuid", false),
                column("id2", "uuid", false),
                column("paid", "uuid", false),
            ]),
        ),
    ]);
    let found = pairs(&graph, &Annotations::default());
    for expected in ["x.person_id -> person.id", "x.boxes_id -> boxes.id", "x.box_id -> boxes.id"] {
        assert!(found.contains(&expected.to_string()), "{expected} in {found:?}");
    }
    assert_eq!(found.len(), 3, "valid / grid / id2 / paid name no table: {found:?}");
}

#[test]
fn inference_needs_compatible_types_and_ranks_by_confidence() {
    let graph = schema(vec![
        ("customer", properties(vec![column("id", "integer", true)])),
        ("region", properties(vec![column("id", "integer", true)])),
        ("tag", properties(vec![column("id", "uuid", true)])),
        (
            "invoice",
            properties(vec![
                column("id", "integer", true),
                column("customer_id", "integer", false),
                column("region_id", "bigint", false),
                column("tag_id", "integer", false),
            ]),
        ),
    ]);
    let all = suggest(&graph, &Annotations::default());
    let found: Vec<String> = all.iter().map(|s| pair(&graph, s)).collect();
    assert_eq!(found, ["invoice.customer_id -> customer.id", "invoice.region_id -> region.id"], "integer vs uuid is no match");
    assert_eq!((all[0].types, all[1].types), (TypeMatch::Same, TypeMatch::Compatible));
    assert!(all[0].confidence > all[1].confidence, "the same type outranks a compatible one");
    assert_eq!(all[0].name, NameMatch::Table);
    assert!(all.iter().all(|s| (0.0..=1.0).contains(&s.confidence)));
    assert_eq!(all[0].level(), Level::High);
    assert!(all[1].level() != Level::Low, "{:?}", all[1]);
}

#[test]
fn inference_skips_existing_relations_ignored_pairs_and_unloaded_tables() {
    // `fixture()`: order_items.order_id -> orders.id and orders.customer_id
    // -> customers.id are catalog foreign keys already.
    let graph = fixture();
    assert_eq!(pairs(&graph, &Annotations::default()), Vec::<String>::new(), "declared foreign keys are not suggested");

    // A column that no foreign key covers is.
    let graph = schema(vec![
        ("customers", properties(vec![column("id", "integer", true)])),
        ("orders", properties(vec![column("id", "integer", true), column("customers_id", "integer", false)])),
    ]);
    let mut annotations = Annotations::default();
    let found = suggest(&graph, &annotations);
    assert_eq!(found.len(), 1);
    // Ignoring it removes it; a logical relationship for it does too.
    annotations.ignore(found[0].link(&graph));
    assert!(suggest(&graph, &annotations).is_empty());
    let mut declared = Annotations::default();
    declared.add_relation(
        RelationEnd { schema: Some("public".into()), table: "orders".into(), columns: vec!["customers_id".into()] },
        RelationEnd { schema: Some("public".into()), table: "customers".into(), columns: vec!["id".into()] },
        true,
        false,
    )
    .unwrap();
    let effective = graph.with_logical(&declared.relations);
    assert!(suggest(&effective, &declared).is_empty(), "a logical relationship covers the column");

    // Loading and failed tables have no known columns: no guesses.
    let entries = vec![
        (table("customers"), RelatedProperties::Loading),
        (table("orders"), ready(properties(vec![column("id", "integer", true), column("customers_id", "integer", false)]))),
    ];
    assert!(suggest(&DiagramGraph::build_schema(&entries), &Annotations::default()).is_empty());
}

#[test]
fn inference_prefers_the_parents_only_primary_key_and_is_bounded() {
    // A composite key is no target: the same-named column is.
    let graph = schema(vec![
        ("line", properties(vec![column("order_id", "integer", true), column("n", "integer", true)])),
        ("parcel", properties(vec![column("id", "integer", true), column("line_id", "integer", false)])),
    ]);
    let found = pairs(&graph, &Annotations::default());
    assert!(found.is_empty(), "no column of `line` named line_id or id: {found:?}");

    // Bounded.
    let parents: Vec<_> = (0..40).map(|i| (format!("p{i}"), properties(vec![column("id", "integer", true)]))).collect();
    let mut children: Vec<_> = (0..40)
        .map(|c| {
            let mut columns = vec![column("id", "integer", true)];
            columns.extend((0..40).map(|i| column(&format!("p{i}_id"), "integer", false)));
            (format!("c{c}"), properties(columns))
        })
        .collect();
    children.extend(parents);
    let entries: Vec<_> = children.into_iter().map(|(name, p)| (table(&name), ready(p))).collect();
    let big = DiagramGraph::build_schema(&entries);
    assert_eq!(suggest(&big, &Annotations::default()).len(), MAX_SUGGESTIONS);
}

// ---- annotations --------------------------------------------------------------------------

fn end(table: &str, column: &str) -> RelationEnd {
    RelationEnd { schema: Some("public".into()), table: table.into(), columns: vec![column.into()] }
}

fn key(table: &str) -> TableKey {
    TableKey { schema: Some("public".into()), table: table.into() }
}

#[test]
fn logical_relationships_are_unique_editable_and_removable() {
    let mut a = Annotations::default();
    assert!(a.is_empty());
    let id = a.add_relation(end("orders", "customer_id"), end("customers", "id"), true, false).unwrap();
    assert_eq!(a.add_relation(end("orders", "customer_id"), end("customers", "id"), false, true), Err(AnnotationError::DuplicateRelation));
    assert_eq!(a.add_relation(end("orders", "x"), RelationEnd::default(), false, false), Err(AnnotationError::NoColumns));
    let other = a.add_relation(end("orders", "region_id"), end("regions", "id"), false, true).unwrap();
    assert_ne!(id, other);
    assert!(a.set_cardinality(id, false, true));
    assert_eq!((a.relation(id).unwrap().optional, a.relation(id).unwrap().unique), (false, true));
    assert_eq!(a.relation(id).unwrap().cardinality(), "One to one");
    assert!(!a.set_cardinality(999, true, true));
    assert!(a.remove_relation(id) && !a.remove_relation(id));
    assert_eq!(a.relations.len(), 1);
    assert_eq!(cardinality_label(true, false), "Many to one (optional)");
    assert_eq!(cardinality_label(true, true), "One to one (optional)");
}

#[test]
fn notes_colors_and_groups_are_edited_through_the_annotations() {
    let mut a = Annotations::default();
    let note = a.add_note(10.0, 20.0);
    a.note_mut(note).unwrap().text = "hello".into();
    assert_eq!(a.note(note).unwrap().rect().width, 200.0);
    a.note_mut(note).unwrap().width = 10.0;
    assert_eq!(a.note(note).unwrap().rect().width, 80.0, "a size below the minimum is clamped");
    a.note_mut(note).unwrap().width = f32::NAN;
    assert_eq!(a.note(note).unwrap().rect().width, 200.0, "a hand-edited NaN falls back to the default");
    assert!(a.remove_note(note) && !a.remove_note(note));

    a.set_color(key("orders"), Some(SWATCHES[3].1));
    assert_eq!(a.colors[&key("orders")], Rgb(0xef4444));
    a.set_color(key("orders"), None);
    assert!(a.colors.is_empty(), "reset removes the color");

    let g1 = a.add_group("Sales".into(), SWATCHES[0].1, vec![key("orders"), key("customers")]);
    let g2 = a.add_group("Moved".into(), SWATCHES[1].1, vec![key("orders")]);
    assert_eq!(a.group(g1).unwrap().members, [key("customers")], "a table belongs to one group");
    assert_eq!(a.group_of(&key("orders")).unwrap().id, g2);
    a.leave_group(&key("customers"));
    assert!(a.group(g1).is_none(), "an emptied group goes away");
    assert!(a.remove_group(g2));
    assert!(a.groups.is_empty());

    // Ids are unique across kinds.
    let (n, r) = (a.add_note(0.0, 0.0), a.add_relation(end("a", "x"), end("b", "y"), false, false).unwrap());
    assert_ne!(n, r);
    assert_eq!(Rgb::parse("#3B82F6"), Some(Rgb(0x3b82f6)));
    assert_eq!(Rgb::parse("3b82f6"), Some(Rgb(0x3b82f6)));
    for bad in ["red", "#12345", "#1234567", "#12345g", ""] {
        assert_eq!(Rgb::parse(bad), None, "{bad:?}");
    }
    assert_eq!(Rgb(0x3b82f6).hex(), "#3b82f6");
    assert_eq!(Rgb(0x000000).over_white(0.5), "#808080");
}

#[test]
fn logical_relationships_join_the_graph_when_both_ends_exist() {
    let graph = fixture(); // orders (focus), customers, order_items
    let mut a = Annotations::default();
    a.add_relation(end("customers", "name"), end("orders", "note"), true, false).unwrap();
    // Stale: unknown table / unknown column.
    a.add_relation(end("gone", "x"), end("orders", "id"), true, false).unwrap();
    a.add_relation(end("orders", "nope"), end("customers", "id"), true, false).unwrap();
    // The catalog declares this one already.
    a.add_relation(end("orders", "customer_id"), end("customers", "id"), false, false).unwrap();
    let effective = graph.with_logical(&a.relations);
    assert_eq!(graph.relations.len(), 2);
    assert_eq!(effective.relations.len(), 3, "one logical relationship fits");
    let logical = effective.relations.last().unwrap();
    assert_eq!(logical.origin, RelationOrigin::Logical(a.relations[0].id));
    assert_eq!((logical.optional, logical.unique), (true, false));
    assert_eq!(
        (effective.entities[logical.child.0].table.table.as_str(), effective.entities[logical.parent.0].table.table.as_str()),
        ("customers", "orders")
    );
    assert!(effective.relations[..2].iter().all(|r| r.origin == RelationOrigin::Declared));
    // Everything downstream iterates the relations: JOIN SQL included.
    let sql = join_sql::relation_join(&effective, 2, crate::sql_table::SqlDialect::POSTGRES).unwrap();
    assert!(sql.contains("LEFT JOIN") && sql.contains("t1.\"name\" = t2.\"note\""), "{sql}");
}

#[test]
fn groups_hide_their_entities_and_frame_them() {
    let graph = fixture();
    let mut a = Annotations::default();
    let id = a.add_group("G".into(), SWATCHES[0].1, vec![key("orders"), key("customers"), key("missing")]);
    let index = GroupIndex::build(&graph, &a.groups);
    assert_eq!(index.members, [vec![0, 1]], "a table that is not in the diagram is skipped");
    assert!(index.hidden.iter().all(|h| !h));
    a.group_mut(id).unwrap().collapsed = true;
    assert_eq!(GroupIndex::build(&graph, &a.groups).hidden, [true, true, false], "collapsed hides its entities");
    a.group_mut(id).unwrap().collapsed = false;
    a.group_mut(id).unwrap().hidden = true;
    let hidden = GroupIndex::build(&graph, &a.groups);
    assert!(hidden.hidden[0] && hidden.hidden[1] && !hidden.shown(0) && hidden.shown(2));

    let union = DiagramRect { x: 100.0, y: 50.0, width: 300.0, height: 200.0 };
    let frame = group_frame(union, false);
    assert!(frame.x < union.x && frame.y < union.y && frame.right() > union.right() && frame.bottom() > union.bottom());
    let chip = group_frame(union, true);
    assert_eq!((chip.x, chip.y), (frame.x, frame.y), "the chip sits at the frame's corner");
    assert!(chip.width < frame.width && chip.height < frame.height);
}

// ---- layout additions -------------------------------------------------------------------------

#[test]
fn row_at_is_the_inverse_of_the_route_anchors() {
    let graph = fixture();
    let m = metrics();
    let layout = DiagramLayout::auto(&graph, AttributeMode::All, &m);
    for mode in AttributeMode::ALL {
        let layout = DiagramLayout::auto(&graph, mode, &m);
        for (index, entity) in graph.entities.iter().enumerate() {
            let rect = layout.rects[index];
            for (column, c) in entity.columns.iter().enumerate() {
                let anchor = layout.row_anchor(&graph, EntityId(index), column, mode, &m);
                assert_eq!(anchor.is_some(), mode.shows(c), "{mode:?} {index}.{column}");
                if let Some(anchor) = anchor {
                    assert_eq!(anchor.x, rect.x + rect.width);
                    let inside = DiagramPoint { x: rect.x + 5.0, y: anchor.y };
                    assert_eq!(layout.row_at(&graph, EntityId(index), inside, mode, &m), Some(column), "{mode:?} {index}.{column}");
                }
            }
            // The header belongs to no row.
            let header = DiagramPoint { x: rect.x + 5.0, y: rect.y + m.header_height / 2.0 };
            assert_eq!(layout.row_at(&graph, EntityId(index), header, mode, &m), None);
        }
    }
    // A point outside the box is no row.
    let outside = DiagramPoint { x: layout.rects[0].x - 5.0, y: layout.rects[0].y + 40.0 };
    assert_eq!(layout.row_at(&graph, EntityId(0), outside, AttributeMode::All, &m), None);
    // Hidden entities are not hit.
    let inside = DiagramPoint { x: layout.rects[0].x + 4.0, y: layout.rects[0].y + 4.0 };
    assert_eq!(layout.hit_test(inside), Some(EntityId(0)));
    assert_eq!(layout.hit_test_where(inside, |i| i != 0), None);
    assert_eq!(layout.union_of(&[0, 1]), Some(layout.rects[0].union(&layout.rects[1])));
}

// ---- history ---------------------------------------------------------------------------------

fn step(n: usize) -> Edit {
    Edit::Move(vec![MoveStep { table: key("t"), from: DiagramPoint { x: n as f32, y: 0.0 }, to: DiagramPoint { x: n as f32 + 1.0, y: 0.0 } }])
}

#[test]
fn history_undoes_redoes_and_forgets_the_oldest_beyond_the_limit() {
    let mut h = History::default();
    assert!(!h.can_undo() && !h.can_redo() && h.take_undo().is_none() && h.take_redo().is_none());
    for n in 0..HISTORY_LIMIT + 20 {
        h.push(step(n));
    }
    assert_eq!(h.undo_len(), HISTORY_LIMIT, "bounded");
    // The newest is undone first; the oldest 20 are gone.
    let last = h.take_undo().unwrap();
    assert_eq!(last, step(HISTORY_LIMIT + 19));
    h.keep_undone(last.clone());
    assert!(h.can_redo());
    let again = h.take_redo().unwrap();
    assert_eq!(again, last);
    h.keep_done(again);
    for _ in 0..HISTORY_LIMIT {
        let edit = h.take_undo().unwrap();
        h.keep_undone(edit);
    }
    assert!(h.take_undo().is_none(), "nothing before the limit");
    assert_eq!(h.take_redo().unwrap(), step(20), "the oldest kept step");

    // A new edit drops what could be redone.
    let mut h = History::default();
    h.push(step(1));
    let undone = h.take_undo().unwrap();
    h.keep_undone(undone);
    h.push(step(2));
    assert!(!h.can_redo());
    h.clear();
    assert!(!h.can_undo());

    // Annotation edits keep before and after.
    let before = Arc::new(Annotations::default());
    let mut next = Annotations::default();
    next.add_note(1.0, 2.0);
    let after = Arc::new(next);
    h.push(Edit::Annotations { before: before.clone(), after: after.clone() });
    match h.take_undo().unwrap() {
        Edit::Annotations { before: b, after: a } => assert_eq!((b, a), (before, after)),
        other => panic!("{other:?}"),
    }
}

// ---- store ----------------------------------------------------------------------------------

fn temp_store() -> (LayoutStore, std::path::PathBuf) {
    let dir = std::env::temp_dir().join(format!("tablex-diagram-d2-{}", uuid::Uuid::new_v4()));
    (LayoutStore::new(dir.clone()), dir)
}

fn diagram_key() -> DiagramKey {
    DiagramKey { connection: "c1".into(), database: "db".into(), schema: Some("public".into()), table: Some("orders".into()) }
}

fn annotated() -> Annotations {
    let mut a = Annotations::default();
    a.add_relation(end("customers", "name"), end("orders", "note"), true, false).unwrap();
    let note = a.add_note(40.0, 50.0);
    a.note_mut(note).unwrap().text = "Check this\nsecond line".into();
    a.set_color(key("orders"), Some(SWATCHES[2].1));
    a.add_group("Core".into(), SWATCHES[1].1, vec![key("orders"), key("customers")]);
    a.ignore(IgnoredLink { child: end("order_items", "sku"), parent: end("orders", "id") });
    a
}

#[test]
fn annotations_round_trip_through_the_store() {
    let (store, dir) = temp_store();
    let graph = fixture();
    let layout = DiagramLayout::auto(&graph, AttributeMode::All, &metrics());
    let annotations = annotated();
    let key = diagram_key();
    let pinned = [false, true, false];
    let saved = SavedDiagram { key: key.clone(), ..Default::default() }.with_document(&graph, &layout, &pinned, &annotations, &Draft::default());
    assert_eq!(saved.entities.len(), 1, "only the entities the user placed are written");
    assert_eq!(saved.entities[0].table, "customers");
    store.save(&key, &saved).unwrap();

    let loaded = store.load(&key).unwrap().unwrap();
    assert_eq!(loaded.annotations(), annotations);
    assert_eq!(loaded, saved);
    let text: serde_json::Value = serde_json::from_slice(&std::fs::read(store.path(&key)).unwrap()).unwrap();
    assert_eq!(text["version"], 1, "additive fields need no version bump");
    assert_eq!(text["relations"][0]["child"]["table"], "customers");
    assert_eq!(text["notes"][0]["text"], "Check this\nsecond line");
    assert_eq!(text["colors"][0]["color"], "#f59e0b");
    assert_eq!(text["groups"][0]["name"], "Core");
    assert_eq!(text["ignored"][0]["child"]["columns"][0], "sku");

    // Re-layout: positions go, the annotations stay; an emptied document is blank.
    let relaid = loaded.clone().without_positions();
    assert!(relaid.entities.is_empty() && !relaid.is_blank());
    assert!(SavedDiagram::default().is_blank());
    assert!(relaid.with_annotations(&Annotations::default()).is_blank());
    std::fs::remove_dir_all(dir).ok();
}

#[test]
fn a_d1_file_loads_with_empty_annotations_and_keeps_its_positions() {
    let (store, dir) = temp_store();
    std::fs::create_dir_all(&dir).unwrap();
    let key = diagram_key();
    let d1 = serde_json::json!({
        "version": 1,
        "key": {"connection": "c1", "database": "db", "schema": "public", "table": "orders"},
        "entities": [
            {"schema": "public", "table": "orders", "x": 5.0, "y": 6.0},
            {"schema": "public", "table": "customers", "x": 7.0, "y": 8.0},
        ],
    });
    std::fs::write(store.path(&key), d1.to_string()).unwrap();
    let loaded = store.load(&key).unwrap().unwrap();
    assert!(loaded.annotations().is_empty());
    assert_eq!(loaded.entities.len(), 2);
    let graph = fixture();
    let mut layout = DiagramLayout::auto(&graph, AttributeMode::All, &metrics());
    assert_eq!(loaded.apply_entities(&graph, &mut layout), vec![0, 1]);
    assert_eq!((layout.rects[1].x, layout.rects[1].y), (7.0, 8.0));

    // Saving annotations over it keeps the old entries it is told to keep.
    let rewritten = loaded.with_document(&graph, &layout, &[true, true, false], &annotated(), &Draft::default());
    assert_eq!(rewritten.entities.len(), 2);
    assert_eq!(rewritten.annotations(), annotated());
    std::fs::remove_dir_all(dir).ok();
}

#[test]
fn unknown_fields_of_annotations_and_bad_colors_are_tolerated() {
    let (store, dir) = temp_store();
    std::fs::create_dir_all(&dir).unwrap();
    let key = diagram_key();
    let future = serde_json::json!({
        "version": 1,
        "key": {"connection": "c1", "database": "db", "schema": "public", "table": "orders"},
        "relations": [{"id": 4, "child": {"table": "a", "columns": ["x"]}, "parent": {"table": "b", "columns": ["y"]}, "style": "bold"}],
        "notes": [{"id": 5, "text": "n", "pinned": true}],
        "groups": [{"id": 6, "name": "g", "members": [{"table": "a"}], "icon": "star"}],
        "colors": [{"table": "a", "color": "not-a-color"}, {"table": "b", "color": "#112233"}],
        "layers": [1, 2],
    });
    std::fs::write(store.path(&key), future.to_string()).unwrap();
    let loaded = store.load(&key).unwrap().unwrap();
    let annotations = loaded.annotations();
    assert_eq!(annotations.colors.len(), 1, "the unreadable color is dropped");
    assert_eq!(annotations.colors[&TableKey { schema: None, table: "b".into() }], Rgb(0x112233));
    assert_eq!(annotations.relations[0].extra["style"], "bold");
    // A sizeless note falls back to the default size.
    assert_eq!(annotations.notes[0].rect().width, 200.0);
    // Re-saving keeps what this build does not know.
    let graph = fixture();
    let layout = DiagramLayout::auto(&graph, AttributeMode::All, &metrics());
    store.save(&key, &loaded.clone().with_document(&graph, &layout, &[], &annotations, &Draft::default())).unwrap();
    let text: serde_json::Value = serde_json::from_slice(&std::fs::read(store.path(&key)).unwrap()).unwrap();
    assert_eq!(text["relations"][0]["style"], "bold");
    assert_eq!(text["notes"][0]["pinned"], true);
    assert_eq!(text["groups"][0]["icon"], "star");
    assert_eq!(text["layers"], serde_json::json!([1, 2]));
    std::fs::remove_dir_all(dir).ok();
}

// ---- exports --------------------------------------------------------------------------------

/// `fixed_request` of D1 plus logical relationship, note, color and group.
fn annotated_request() -> ExportRequest {
    let mut request = request(fixture());
    request.layout.rects = vec![
        DiagramRect { x: 300.0, y: 40.0, width: 160.0, height: 100.0 },
        DiagramRect { x: 600.0, y: 40.0, width: 160.0, height: 78.0 },
        DiagramRect { x: 0.0, y: 40.0, width: 160.0, height: 100.0 },
    ];
    let annotations = Arc::new(annotated());
    request.graph = Arc::new(request.graph.with_logical(&annotations.relations));
    request.annotations = annotations;
    request
}

#[test]
fn exports_include_logical_relationships_notes_colors_and_groups() {
    let request = annotated_request();
    assert_eq!(request.graph.relations.len(), 3);

    // SVG: a dashed relation, the note with its lines, the tinted header, the group frame.
    let svg = request.svg();
    let tree = resvg::usvg::Tree::from_str(&svg, &resvg::usvg::Options::default()).expect("a valid SVG");
    assert!(tree.size().width() > 400.0);
    assert_eq!(svg.matches("class=\"relation").count(), 3);
    assert_eq!(svg.matches("class=\"relation logical\"").count(), 1, "{svg}");
    assert_eq!(svg.matches("stroke-dasharray=\"7 4\"").count(), 1, "only the logical one is dashed");
    assert!(svg.contains("class=\"note\"") && svg.contains(">Check this</text>") && svg.contains(">second line</text>"), "{svg}");
    assert!(svg.contains("class=\"group\"") && svg.contains("Core (2)"));
    assert!(svg.contains(&Rgb(0xf59e0b).over_white(0.38)), "the colored header");
    assert_eq!(svg.matches("class=\"entity\"").count(), 3);
    // PNG is that SVG.
    let png = request.render(ExportFormat::Png).unwrap();
    assert!(image::load_from_memory_with_format(&png, image::ImageFormat::Png).is_ok());

    // Mermaid: a dashed (non-identifying) link and comments.
    let mermaid = request.mermaid();
    assert!(mermaid.contains("orders |o..o{ customers : \"name -> note (logical)\""), "{mermaid}");
    assert!(mermaid.contains("%% note ") && mermaid.contains("Check this") && mermaid.contains("%% color orders: #f59e0b"), "{mermaid}");
    assert!(mermaid.contains("%% group Core: orders, customers"));

    // DBML: a Ref with a marker, the header color, the group and the note.
    let dbml = request.dbml();
    assert!(dbml.contains("Ref: public.customers.name > public.orders.note // logical relationship"), "{dbml}");
    assert!(dbml.contains("Table public.orders [headercolor: #f59e0b] {"), "{dbml}");
    assert!(dbml.contains("TableGroup Core {") && dbml.contains("  public.customers"), "{dbml}");
    assert!(dbml.contains("Note note_") && dbml.contains("'''Check this\nsecond line'''"), "{dbml}");

    // JSON.
    let value: serde_json::Value = serde_json::from_str(&request.json().unwrap()).unwrap();
    let relations = value["relations"].as_array().unwrap();
    assert_eq!(relations.iter().filter(|r| r["logical"] == true).count(), 1);
    assert_eq!(relations.iter().filter(|r| r.get("logical").is_none()).count(), 2, "declared ones carry no flag");
    assert_eq!(value["tables"][0]["color"], "#f59e0b");
    assert!(value["tables"][1].get("color").is_none());
    assert_eq!(value["notes"][0]["text"], "Check this\nsecond line");
    assert_eq!(value["groups"][0]["name"], "Core");
    assert_eq!(value["groups"][0]["tables"].as_array().unwrap().len(), 2);
}

#[test]
fn a_hidden_group_leaves_the_images_but_not_the_data() {
    let mut request = annotated_request();
    let mut annotations = (*request.annotations).clone();
    annotations.groups[0].hidden = true;
    request.annotations = Arc::new(annotations);
    let svg = request.svg();
    assert_eq!(svg.matches("class=\"entity\"").count(), 1, "orders and customers are hidden");
    assert_eq!(svg.matches("class=\"group\"").count(), 0);
    assert_eq!(svg.matches("class=\"relation").count(), 0, "lines touching hidden tables go too");
    assert!(request.mermaid().contains("orders") && request.dbml().contains("Table public.customers"), "data formats list everything");
    let value: serde_json::Value = serde_json::from_str(&request.json().unwrap()).unwrap();
    assert_eq!(value["tables"].as_array().unwrap().len(), 3);
}

#[test]
fn the_inspector_lists_every_column_with_its_key_nullability_and_default() {
    use crate::app::results::diagram::view::{inspector_columns, KeyKind};
    let mut graph = fixture();
    graph.entities[0].columns[2].default = Some("'n/a'".into());
    let rows = inspector_columns(&graph.entities[0]);
    assert_eq!(rows.len(), graph.entities[0].columns.len());
    assert_eq!((rows[0].name, rows[0].key, rows[0].nullable), ("id", KeyKind::Primary, false));
    let fk = rows.iter().find(|r| r.name == "customer_id").unwrap();
    assert_eq!((fk.key, fk.data_type), (KeyKind::Foreign, "integer"));
    assert_eq!(rows.iter().find(|r| r.name == "note").unwrap().default, Some("'n/a'"));
}
