//! Unit tests for the pure diagram model, layout, routing and viewport.

use crate::catalog::{
    CatalogColumn, CatalogConstraint, CatalogConstraintKind, CatalogForeignKey, TableProperties,
};

use super::layout::{AttributeMode, DiagramLayout, DiagramPoint, DiagramRect, Metrics, Viewport};
use super::model::{DiagramGraph, EntityId, EntityState, RelatedProperties, TableRef, TypeGlyph};

fn metrics() -> Metrics {
    Metrics {
        header_height: 28.0,
        row_height: 22.0,
        separator_height: 6.0,
        min_width: 160.0,
        max_width: 320.0,
        char_width: 7.0,
        column_gap: 80.0,
        row_gap: 24.0,
    }
}

fn table(name: &str) -> TableRef {
    TableRef {
        database: "db".into(),
        schema: Some("public".into()),
        table: name.into(),
    }
}

fn col(name: &str, ty: &str, pk: bool) -> CatalogColumn {
    CatalogColumn {
        name: name.into(),
        data_type: ty.into(),
        nullable: !pk,
        primary_key: pk,
        ..Default::default()
    }
}

fn fk(
    name: &str,
    owner: &str,
    cols: &[&str],
    target: &str,
    target_cols: &[&str],
) -> CatalogForeignKey {
    CatalogForeignKey {
        name: name.into(),
        table: owner.into(),
        columns: cols.iter().map(|c| (*c).into()).collect(),
        referenced_table: target.into(),
        referenced_columns: target_cols.iter().map(|c| (*c).into()).collect(),
        on_update: None,
        on_delete: None,
        definition: None,
    }
}

fn pk_constraint(cols: &[&str]) -> CatalogConstraint {
    CatalogConstraint {
        name: "pk".into(),
        kind: CatalogConstraintKind::PrimaryKey,
        columns: cols.iter().map(|c| (*c).into()).collect(),
        definition: None,
    }
}

/// `id` PK plus `extra` plain columns.
fn simple(extra: &[&str]) -> TableProperties {
    let mut columns = vec![col("id", "integer", true)];
    columns.extend(extra.iter().map(|n| col(n, "text", false)));
    TableProperties {
        columns,
        ..Default::default()
    }
}

fn ready(p: TableProperties) -> RelatedProperties {
    RelatedProperties::Ready(std::sync::Arc::new(p))
}

fn names(graph: &DiagramGraph) -> Vec<&str> {
    graph
        .entities
        .iter()
        .map(|e| e.table.table.as_str())
        .collect()
}

/// track(album_id -> album.id), customer.order(track_id -> track.id)
fn track_properties() -> TableProperties {
    TableProperties {
        columns: vec![
            col("id", "integer", true),
            col("name", "varchar(120)", false),
            col("album_id", "integer", false),
        ],
        keys: Some(vec![pk_constraint(&["id"])]),
        foreign_keys: Some(vec![fk(
            "fk_album",
            "track",
            &["album_id"],
            "public.album",
            &["id"],
        )]),
        references: Some(vec![fk(
            "fk_line",
            "invoice_line",
            &["track_id"],
            "track",
            &["id"],
        )]),
        ..Default::default()
    }
}

fn build_track(related: &dyn Fn(&TableRef) -> RelatedProperties) -> DiagramGraph {
    DiagramGraph::build(&table("track"), &track_properties(), related)
}

#[test]
fn outgoing_and_incoming_relations() {
    let graph = build_track(&|t| match t.table.as_str() {
        "album" => ready(simple(&["title"])),
        _ => RelatedProperties::Loading,
    });
    assert_eq!(names(&graph), ["track", "album", "invoice_line"]);
    assert!(graph.entities[0].focus && !graph.entities[1].focus);
    assert_eq!(graph.relations.len(), 2);
    let outgoing = graph
        .relations
        .iter()
        .find(|r| r.child == EntityId(0))
        .unwrap();
    assert_eq!(outgoing.parent, EntityId(1));
    assert_eq!(
        outgoing.child_columns,
        [graph.entities[0]
            .columns
            .iter()
            .position(|c| c.name == "album_id")
            .unwrap()]
    );
    assert_eq!(outgoing.parent_columns, [0]);
    let incoming = graph
        .relations
        .iter()
        .find(|r| r.parent == EntityId(0))
        .unwrap();
    assert_eq!(incoming.child, EntityId(2));
    assert_eq!(incoming.name, "fk_line");
    // The focus' album_id is flagged FK; the child's FK column is known before load.
    assert!(
        graph.entities[0]
            .columns
            .iter()
            .find(|c| c.name == "album_id")
            .unwrap()
            .foreign_key
    );
    assert!(
        !graph.entities[0]
            .columns
            .iter()
            .find(|c| c.name == "name")
            .unwrap()
            .foreign_key
    );
}

#[test]
fn loading_and_failed_entities_keep_known_columns() {
    let graph = build_track(&|t| match t.table.as_str() {
        "album" => RelatedProperties::Failed("boom".into()),
        _ => RelatedProperties::Loading,
    });
    let album = &graph.entities[1];
    assert_eq!(album.state, EntityState::Failed("boom".into()));
    assert!(album.columns.is_empty());
    let line = &graph.entities[2];
    assert_eq!(line.state, EntityState::Loading);
    assert_eq!(line.columns.len(), 1);
    assert_eq!(
        (
            line.columns[0].name.as_str(),
            line.columns[0].foreign_key,
            line.columns[0].primary_key
        ),
        ("track_id", true, false)
    );
    let outgoing = graph
        .relations
        .iter()
        .find(|r| r.child == EntityId(0))
        .unwrap();
    assert!(outgoing.parent_columns.is_empty(), "parent still unknown");
    let incoming = graph
        .relations
        .iter()
        .find(|r| r.parent == EntityId(0))
        .unwrap();
    assert_eq!(incoming.child_columns, [0]);
}

#[test]
fn ready_child_marks_foreign_keys_from_own_and_referencing_columns() {
    let mut line = simple(&["track_id", "invoice_id"]);
    line.foreign_keys = Some(vec![fk(
        "fk_inv",
        "invoice_line",
        &["invoice_id"],
        "invoice",
        &["id"],
    )]);
    let graph = build_track(&|t| {
        if t.table == "invoice_line" {
            ready(line.clone())
        } else {
            RelatedProperties::Loading
        }
    });
    let line = &graph.entities[2];
    let flags: Vec<_> = line
        .columns
        .iter()
        .map(|c| (c.name.as_str(), c.foreign_key))
        .collect();
    assert_eq!(
        flags,
        [("id", false), ("track_id", true), ("invoice_id", true)]
    );
    let incoming = graph
        .relations
        .iter()
        .find(|r| r.parent == EntityId(0))
        .unwrap();
    assert_eq!(incoming.child_columns, [1]);
}

#[test]
fn composite_foreign_key_maps_every_column() {
    let focus = TableProperties {
        columns: vec![
            col("a", "int", true),
            col("b", "int", true),
            col("x", "int", false),
            col("y", "int", false),
        ],
        foreign_keys: Some(vec![fk("fk", "t", &["y", "x"], "other", &["q", "p"])]),
        ..Default::default()
    };
    let other = TableProperties {
        columns: vec![col("p", "int", true), col("q", "int", true)],
        ..Default::default()
    };
    let graph = DiagramGraph::build(&table("t"), &focus, &|_| ready(other.clone()));
    let r = &graph.relations[0];
    assert_eq!(r.child_columns, [3, 2]);
    assert_eq!(r.parent_columns, [1, 0]);
    let fks: Vec<_> = graph.entities[0]
        .columns
        .iter()
        .map(|c| c.foreign_key)
        .collect();
    assert_eq!(fks, [false, false, true, true]);
}

#[test]
fn self_reference_is_kept_once() {
    let mut p = simple(&["parent_id"]);
    let f = fk("fk_self", "emp", &["parent_id"], "public.emp", &["id"]);
    p.foreign_keys = Some(vec![f.clone()]);
    p.references = Some(vec![f]);
    let graph = DiagramGraph::build(&table("emp"), &p, &|_| RelatedProperties::Loading);
    assert_eq!(graph.entities.len(), 1);
    assert_eq!(graph.relations.len(), 1);
    let r = &graph.relations[0];
    assert_eq!(
        (
            r.child,
            r.parent,
            r.child_columns.clone(),
            r.parent_columns.clone()
        ),
        (EntityId(0), EntityId(0), vec![1], vec![0])
    );
    assert!(DiagramGraph::related_tables(&table("emp"), &p, 100).is_empty());
}

#[test]
fn duplicate_edges_collapse() {
    let mut p = simple(&["a_id"]);
    let f = fk("fk1", "t", &["a_id"], "a", &["id"]);
    p.foreign_keys = Some(vec![
        f.clone(),
        f.clone(),
        fk("fk1_dup", "t", &["a_id"], "public.a", &["id"]),
    ]);
    p.references = Some(vec![
        fk("r", "b", &["t_id"], "t", &["id"]),
        fk("r", "public.b", &["t_id"], "t", &["id"]),
    ]);
    let graph = DiagramGraph::build(&table("t"), &p, &|_| RelatedProperties::Loading);
    assert_eq!(graph.relations.len(), 2);
    assert_eq!(names(&graph), ["t", "a", "b"]);
    assert_eq!(
        graph
            .relations
            .iter()
            .find(|r| r.child == EntityId(0))
            .unwrap()
            .name,
        "fk1"
    );
}

#[test]
fn schema_qualified_versus_bare_names() {
    let mut p = simple(&["a_id", "b_id", "c_id"]);
    p.foreign_keys = Some(vec![
        fk("fa", "t", &["a_id"], "a", &["id"]),
        fk("fb", "t", &["b_id"], "sales.b", &["id"]),
        fk("fc", "t", &["c_id"], "public.c", &["id"]),
    ]);
    let graph = DiagramGraph::build(&table("t"), &p, &|_| RelatedProperties::Loading);
    let found: Vec<_> = graph
        .entities
        .iter()
        .map(|e| (e.label.as_str(), e.table.schema.as_deref()))
        .collect();
    assert_eq!(
        found,
        [
            ("t", Some("public")),
            ("a", Some("public")),
            ("c", Some("public")),
            ("sales.b", Some("sales"))
        ]
    );
}

#[test]
fn schema_less_engines_and_database_qualifier() {
    let focus = TableRef {
        database: "shop".into(),
        schema: None,
        table: "t".into(),
    };
    let mut p = simple(&["a_id", "b_id"]);
    p.foreign_keys = Some(vec![
        fk("fa", "t", &["a_id"], "shop.a", &["id"]),
        fk("fb", "t", &["b_id"], "b", &["id"]),
    ]);
    let graph = DiagramGraph::build(&focus, &p, &|_| RelatedProperties::Loading);
    assert!(graph
        .entities
        .iter()
        .all(|e| e.table.schema.is_none() && e.table.database == "shop"));
    assert_eq!(
        graph
            .entities
            .iter()
            .map(|e| e.label.as_str())
            .collect::<Vec<_>>(),
        ["t", "a", "b"]
    );
}

#[test]
fn quoted_identifiers() {
    let focus = table("t");
    let cases = [
        (r#""Sales"."Order Items""#, Some("Sales"), "Order Items"),
        (r#""we.ird"."x.y""#, Some("we.ird"), "x.y"),
        (r#""a""b""#, Some("public"), r#"a"b"#),
        ("`my`.`tbl`", Some("my"), "tbl"),
        ("[dbo].[Orders]", Some("dbo"), "Orders"),
        (r#"public."Mixed""#, Some("public"), "Mixed"),
        ("plain", Some("public"), "plain"),
    ];
    for (raw, schema, name) in cases {
        let mut p = simple(&["x"]);
        p.foreign_keys = Some(vec![fk("f", "t", &["x"], raw, &["id"])]);
        let graph = DiagramGraph::build(&focus, &p, &|_| RelatedProperties::Loading);
        let e = &graph.entities[1];
        assert_eq!(
            (e.table.schema.as_deref(), e.table.table.as_str()),
            (schema, name),
            "{raw}"
        );
    }
}

#[test]
fn primary_key_ordering() {
    let p = TableProperties {
        columns: vec![
            col("name", "text", false),
            col("a", "int", true),
            col("z", "int", true),
            col("m", "int", false),
        ],
        keys: Some(vec![pk_constraint(&["z", "a"])]),
        ..Default::default()
    };
    let graph = DiagramGraph::build(&table("t"), &p, &|_| RelatedProperties::Loading);
    let order: Vec<_> = graph.entities[0]
        .columns
        .iter()
        .map(|c| (c.name.as_str(), c.primary_key))
        .collect();
    assert_eq!(
        order,
        [("z", true), ("a", true), ("name", false), ("m", false)]
    );

    let flags_only = TableProperties { keys: None, ..p };
    let graph = DiagramGraph::build(&table("t"), &flags_only, &|_| RelatedProperties::Loading);
    let order: Vec<_> = graph.entities[0]
        .columns
        .iter()
        .map(|c| c.name.as_str())
        .collect();
    assert_eq!(order, ["a", "z", "name", "m"]);
}

#[test]
fn related_tables_are_sorted_deduped_and_capped() {
    let mut p = simple(&["x", "y"]);
    p.foreign_keys = Some(vec![
        fk("f1", "t", &["x"], "zeta", &["id"]),
        fk("f2", "t", &["y"], "alpha", &["id"]),
    ]);
    p.references = Some(vec![
        fk("r1", "mid", &["t_id"], "t", &["id"]),
        fk("r2", "alpha", &["t_id"], "t", &["id"]),
        fk("r3", "t", &["x"], "t", &["id"]),
    ]);
    let all = DiagramGraph::related_tables(&table("t"), &p, 10);
    let all: Vec<_> = all.iter().map(|t| t.table.as_str()).collect();
    assert_eq!(all, ["alpha", "mid", "zeta"]);
    assert_eq!(DiagramGraph::related_tables(&table("t"), &p, 2).len(), 2);
    assert!(DiagramGraph::related_tables(&table("t"), &p, 0).is_empty());
    let graph = DiagramGraph::build(&table("t"), &p, &|_| RelatedProperties::Loading);
    assert_eq!(names(&graph)[1..], ["alpha", "mid", "zeta"]);
}

#[test]
fn glyph_mapping_for_all_dialects() {
    use TypeGlyph::*;
    let cases: &[(&str, TypeGlyph)] = &[
        // PostgreSQL
        ("integer", Numeric),
        ("bigint", Numeric),
        ("smallserial", Numeric),
        ("numeric(10,2)", Numeric),
        ("double precision", Numeric),
        ("real", Numeric),
        ("character varying(20)", Text),
        ("text", Text),
        ("uuid", Text),
        ("character(3)", Text),
        ("timestamp with time zone", Temporal),
        ("timestamp without time zone", Temporal),
        ("date", Temporal),
        ("time without time zone", Temporal),
        ("interval", Temporal),
        ("boolean", Boolean),
        ("bytea", Binary),
        ("jsonb", Json),
        ("json", Json),
        ("integer[]", Other),
        ("inet", Other),
        // MySQL / MariaDB
        ("int(11) unsigned", Numeric),
        ("TINYINT(1)", Boolean),
        ("tinyint(4)", Numeric),
        ("bigint(20) unsigned", Numeric),
        ("decimal(10,2)", Numeric),
        ("double", Numeric),
        ("varchar(255)", Text),
        ("longtext", Text),
        ("enum('a','b')", Text),
        ("datetime", Temporal),
        ("timestamp", Temporal),
        ("year", Temporal),
        ("blob", Binary),
        ("longblob", Binary),
        ("varbinary(16)", Binary),
        ("binary(16)", Binary),
        ("bool", Boolean),
        ("geometry", Other),
        // SQLite
        ("INTEGER", Numeric),
        ("REAL", Numeric),
        ("NUMERIC", Numeric),
        ("TEXT", Text),
        ("BLOB", Binary),
        ("VARCHAR(10)", Text),
        ("DATETIME", Temporal),
        ("BOOLEAN", Boolean),
        ("", Other),
        // ClickHouse
        ("UInt8", Numeric),
        ("UInt64", Numeric),
        ("Int32", Numeric),
        ("Float64", Numeric),
        ("Decimal(18, 4)", Numeric),
        ("String", Text),
        ("FixedString(16)", Text),
        ("UUID", Text),
        ("DateTime", Temporal),
        ("DateTime64(3)", Temporal),
        ("Date32", Temporal),
        ("Nullable(Int32)", Numeric),
        ("LowCardinality(String)", Text),
        ("LowCardinality(Nullable(String))", Text),
        ("Nullable(DateTime64(3, 'UTC'))", Temporal),
        ("Enum8('a' = 1)", Text),
        ("Array(String)", Other),
        ("JSON", Json),
    ];
    for (ty, expected) in cases {
        assert_eq!(TypeGlyph::for_type(ty), *expected, "{ty}");
    }
}

fn entity_graph(entity_columns: &[(&str, &str)]) -> DiagramGraph {
    let p = TableProperties {
        columns: std::iter::once(col("id", "int", true))
            .chain(entity_columns.iter().map(|(n, t)| col(n, t, false)))
            .collect(),
        ..Default::default()
    };
    DiagramGraph::build(&table("t"), &p, &|_| RelatedProperties::Loading)
}

#[test]
fn entity_height_all_and_keys_only() {
    let m = metrics();
    let mut p = simple(&["a", "b", "c_id"]);
    p.foreign_keys = Some(vec![fk("f", "t", &["c_id"], "c", &["id"])]);
    let g = DiagramGraph::build(&table("t"), &p, &|_| RelatedProperties::Loading);
    let id = EntityId(0);
    assert_eq!(
        DiagramLayout::entity_height(&g, id, AttributeMode::All, &m),
        28.0 + 4.0 * 22.0 + 6.0
    );
    assert_eq!(
        DiagramLayout::entity_height(&g, id, AttributeMode::KeysOnly, &m),
        28.0 + 2.0 * 22.0 + 6.0
    );
    // No separator when only PK rows (or only non-PK rows) are visible.
    let only_pk = entity_graph(&[]);
    assert_eq!(
        DiagramLayout::entity_height(&only_pk, id, AttributeMode::All, &m),
        28.0 + 22.0
    );
    assert_eq!(
        DiagramLayout::entity_height(&only_pk, id, AttributeMode::KeysOnly, &m),
        28.0 + 22.0
    );
    let placeholder = build_track(&|_| RelatedProperties::Loading);
    assert_eq!(
        DiagramLayout::entity_height(&placeholder, EntityId(2), AttributeMode::All, &m),
        28.0 + 22.0
    );
    assert_eq!(
        DiagramLayout::entity_height(&placeholder, EntityId(1), AttributeMode::All, &m),
        28.0
    );
}

#[test]
fn width_follows_text_and_clamps() {
    let m = metrics();
    let short = entity_graph(&[("a", "int")]);
    let long = entity_graph(&[("a_long_column_name", "varchar(30)")]);
    let wide = entity_graph(&[("x".repeat(100).as_str(), "text")]);
    let w = |g: &DiagramGraph| DiagramLayout::auto(g, AttributeMode::All, &m).rects[0].width;
    assert_eq!(w(&short), 160.0);
    assert_eq!(w(&long), (18 + 2 + 11) as f32 * 7.0);
    assert_eq!(w(&wide), 320.0);
}

/// Deterministic xorshift for generated graphs.
struct Rng(u64);
impl Rng {
    fn next(&mut self, bound: u64) -> u64 {
        self.0 ^= self.0 << 13;
        self.0 ^= self.0 >> 7;
        self.0 ^= self.0 << 17;
        self.0 % bound
    }
}

fn generated(rng: &mut Rng, parents: usize, children: usize, columns: usize) -> DiagramGraph {
    let mut p = TableProperties {
        columns: vec![col("id", "int", true)],
        ..Default::default()
    };
    let mut fks = Vec::new();
    let mut refs = Vec::new();
    for i in 0..parents {
        p.columns.push(col(&format!("p{i}_id"), "int", false));
        fks.push(fk(
            &format!("fp{i}"),
            "t",
            &[&format!("p{i}_id")],
            &format!("parent{i:02}"),
            &["id"],
        ));
    }
    for i in 0..columns {
        p.columns
            .push(col(&format!("column_{i}"), "varchar(40)", false));
    }
    for i in 0..children {
        refs.push(fk(
            &format!("fc{i}"),
            &format!("child{i:02}"),
            &["t_id"],
            "t",
            &["id"],
        ));
    }
    if rng.next(2) == 0 {
        // A table that is both parent and child.
        fks.push(fk("both_p", "t", &["id"], "child00", &["id"]));
    }
    if rng.next(2) == 0 {
        fks.push(fk("self", "t", &["id"], "t", &["id"]));
    }
    p.foreign_keys = Some(fks);
    p.references = Some(refs);
    DiagramGraph::build(&table("t"), &p, &|t| {
        let mut r = Rng(t.table.len() as u64 + 1);
        let n = r.next(12) as usize;
        let mut props = simple(&[]);
        props
            .columns
            .extend((0..n).map(|i| col(&format!("c{i}"), "text", false)));
        props.columns.push(col("t_id", "int", false));
        if t.table.starts_with("child") && n.is_multiple_of(2) {
            RelatedProperties::Loading
        } else {
            ready(props)
        }
    })
}

fn overlaps(a: &DiagramRect, b: &DiagramRect) -> bool {
    a.x < b.x + b.width && b.x < a.x + a.width && a.y < b.y + b.height && b.y < a.y + a.height
}

/// No two boxes overlap unless one of them is in `allowed`.
fn assert_no_overlap_except(layout: &DiagramLayout, allowed: &[usize]) {
    for i in 0..layout.rects.len() {
        for j in (i + 1)..layout.rects.len() {
            if allowed.contains(&i) || allowed.contains(&j) {
                continue;
            }
            assert!(!overlaps(&layout.rects[i], &layout.rects[j]), "{i} overlaps {j}");
        }
    }
}

fn assert_no_overlap(layout: &DiagramLayout) {
    for (i, a) in layout.rects.iter().enumerate() {
        for b in &layout.rects[i + 1..] {
            assert!(!overlaps(a, b), "{a:?} overlaps {b:?}");
        }
    }
}

#[test]
fn auto_layout_has_no_overlaps_and_is_deterministic() {
    let m = metrics();
    let mut rng = Rng(0x9e3779b97f4a7c15);
    for _ in 0..200 {
        let (parents, children, columns) = (
            rng.next(8) as usize,
            rng.next(12) as usize,
            rng.next(30) as usize,
        );
        let graph = generated(&mut rng, parents, children, columns);
        for mode in AttributeMode::ALL {
            let layout = DiagramLayout::auto(&graph, mode, &m);
            assert_eq!(layout.rects.len(), graph.entities.len());
            assert_no_overlap(&layout);
            assert_eq!(layout, DiagramLayout::auto(&graph, mode, &m));
            for (i, r) in layout.rects.iter().enumerate() {
                assert_eq!(
                    r.height,
                    DiagramLayout::entity_height(&graph, EntityId(i), mode, &m)
                );
            }
            let focus = layout.rects[0];
            for rel in graph
                .relations
                .iter()
                .filter(|r| r.child.0 == 0 && r.parent.0 != 0)
            {
                assert!(
                    layout.rects[rel.parent.0].x >= focus.x + focus.width + m.column_gap - 0.001,
                    "parent right of focus"
                );
            }
            for rel in graph.relations.iter().filter(|r| {
                r.parent.0 == 0
                    && r.child.0 != 0
                    && !graph
                        .relations
                        .iter()
                        .any(|o| o.child.0 == 0 && o.parent == r.child)
            }) {
                let c = layout.rects[rel.child.0];
                assert!(
                    c.x + c.width + m.column_gap <= focus.x + 0.001,
                    "child left of focus"
                );
            }
        }
    }
}

#[test]
fn auto_centres_columns_on_the_focus() {
    let m = metrics();
    let mut rng = Rng(7);
    let graph = generated(&mut rng, 3, 0, 4);
    let layout = DiagramLayout::auto(&graph, AttributeMode::All, &m);
    let focus_center = layout.rects[0].y + layout.rects[0].height / 2.0;
    let parents: Vec<_> = layout.rects[1..]
        .iter()
        .filter(|r| r.x > layout.rects[0].x)
        .collect();
    let top = parents.iter().map(|r| r.y).fold(f32::MAX, f32::min);
    let bottom = parents
        .iter()
        .map(|r| r.y + r.height)
        .fold(f32::MIN, f32::max);
    assert!(((top + bottom) / 2.0 - focus_center).abs() < 0.01);
    assert_eq!(
        layout.rects[0].x, 0.0,
        "no children => focus is the left column"
    );
}

#[test]
fn both_parent_and_child_goes_right() {
    let mut p = simple(&["x"]);
    p.foreign_keys = Some(vec![fk("f", "t", &["x"], "other", &["id"])]);
    p.references = Some(vec![fk("r", "other", &["t_id"], "t", &["id"])]);
    let graph = DiagramGraph::build(&table("t"), &p, &|_| RelatedProperties::Loading);
    let layout = DiagramLayout::auto(&graph, AttributeMode::All, &metrics());
    assert!(layout.rects[1].x > layout.rects[0].x);
}

#[test]
fn reflow_pins_the_users_boxes_and_moves_the_others_out_of_the_way() {
    let m = metrics();
    let loading = build_track(&|_| RelatedProperties::Loading);
    let mut layout = DiagramLayout::auto(&loading, AttributeMode::All, &m);
    let loaded = build_track(&|t| {
        if t.table == "album" {
            ready(simple(&["a", "b", "c"]))
        } else {
            ready(simple(&["track_id", "q"]))
        }
    });

    // Nothing pinned: boxes grow into the automatic layout of the loaded
    // graph, whatever the placeholder layout was (the old `refit` kept the
    // placeholder positions and the grown boxes overlapped).
    let before_height = layout.rects[2].height;
    layout.reflow(&loaded, AttributeMode::All, &m, false, &[]);
    assert_eq!(layout, DiagramLayout::auto(&loaded, AttributeMode::All, &m));
    assert_ne!(layout.rects[2].height, before_height);
    assert_no_overlap(&layout);

    // A pinned (dragged) box keeps its place; the others step out of its way.
    layout.move_entity(EntityId(1), 33.0, -17.0);
    let moved = layout.rects[1];
    let mut pinned = vec![false; layout.rects.len()];
    pinned[1] = true;
    layout.reflow(&loaded, AttributeMode::KeysOnly, &m, false, &pinned);
    assert_eq!((layout.rects[1].x, layout.rects[1].y), (moved.x, moved.y));
    assert_eq!(layout.rects[1].height, DiagramLayout::entity_height(&loaded, EntityId(1), AttributeMode::KeysOnly, &m));
    assert_no_overlap(&layout);

    // Dropped right onto another box, the pinned one stays and the other
    // box is pushed below it; two pinned boxes may overlap (the user's call).
    let onto = layout.rects[2];
    layout.rects[1].x = onto.x;
    layout.rects[1].y = onto.y;
    layout.reflow(&loaded, AttributeMode::All, &m, false, &pinned);
    assert_eq!((layout.rects[1].x, layout.rects[1].y), (onto.x, onto.y));
    assert_no_overlap_except(&layout, &[1]);
    pinned[2] = true;
    layout.rects[2].x = layout.rects[1].x;
    layout.rects[2].y = layout.rects[1].y;
    let both = (layout.rects[1], layout.rects[2]);
    layout.reflow(&loaded, AttributeMode::All, &m, false, &pinned);
    assert_eq!((layout.rects[1].x, layout.rects[2].y), (both.0.x, both.1.y), "pinned boxes are never moved");

    // A graph with another number of entities is laid out from scratch.
    let mut p = track_properties();
    p.references.as_mut().unwrap().extend((0..5).map(|i| fk("r", &format!("zz{i}"), &["t_id"], "track", &["id"])));
    let bigger = DiagramGraph::build(&table("track"), &p, &|_| ready(simple(&["t_id"])));
    layout.reflow(&bigger, AttributeMode::All, &m, false, &pinned);
    assert_eq!(layout, DiagramLayout::auto(&bigger, AttributeMode::All, &m));

    // Whole-schema layouts reflow the same way.
    let mut schema = DiagramLayout::auto_schema(&bigger, AttributeMode::All, &m);
    schema.reflow(&bigger, AttributeMode::PrimaryKeyOnly, &m, true, &[]);
    assert_eq!(schema, DiagramLayout::auto_schema(&bigger, AttributeMode::PrimaryKeyOnly, &m));
}

#[test]
fn bounds_and_hit_test() {
    let layout = DiagramLayout {
        rects: vec![
            DiagramRect {
                x: 0.0,
                y: 0.0,
                width: 100.0,
                height: 50.0,
            },
            DiagramRect {
                x: 50.0,
                y: 20.0,
                width: 100.0,
                height: 80.0,
            },
            DiagramRect {
                x: -30.0,
                y: -10.0,
                width: 10.0,
                height: 10.0,
            },
        ],
    };
    assert_eq!(
        layout.bounds(),
        DiagramRect {
            x: -30.0,
            y: -10.0,
            width: 180.0,
            height: 110.0
        }
    );
    assert_eq!(DiagramLayout::default().bounds(), DiagramRect::default());
    let at = |x, y| layout.hit_test(DiagramPoint { x, y });
    assert_eq!(at(10.0, 10.0), Some(EntityId(0)));
    assert_eq!(
        at(75.0, 30.0),
        Some(EntityId(1)),
        "overlap picks the topmost (highest index)"
    );
    assert_eq!(at(-25.0, -5.0), Some(EntityId(2)));
    assert_eq!(at(200.0, 0.0), None);
    assert_eq!(at(100.0, 10.0), None, "right edge is exclusive");
    let mut moved = layout.clone();
    moved.move_entity(EntityId(2), 100.0, 100.0);
    assert_eq!(
        moved.hit_test(DiagramPoint { x: 75.0, y: 95.0 }),
        Some(EntityId(2))
    );
}

fn row_y(
    layout: &DiagramLayout,
    graph: &DiagramGraph,
    entity: usize,
    column: &str,
    mode: AttributeMode,
) -> f32 {
    let m = metrics();
    let cols = &graph.entities[entity].columns;
    let visible: Vec<_> = cols
        .iter()
        .filter(|c| mode == AttributeMode::All || c.primary_key || c.foreign_key)
        .collect();
    let k = visible
        .iter()
        .position(|c| c.name == column)
        .expect("visible column");
    let sep = if !visible[k].primary_key && visible[0].primary_key {
        m.separator_height
    } else {
        0.0
    };
    layout.rects[entity].y + m.header_height + sep + (k as f32 + 0.5) * m.row_height
}

#[test]
fn routes_attach_to_the_fk_and_pk_rows_on_facing_sides() {
    let m = metrics();
    let graph = build_track(&|t| {
        if t.table == "album" {
            ready(simple(&["title"]))
        } else {
            ready(simple(&["x", "track_id"]))
        }
    });
    let layout = DiagramLayout::auto(&graph, AttributeMode::All, &m);
    let routes = layout.route(&graph, AttributeMode::All, &m);
    assert_eq!(routes.len(), graph.relations.len());
    for route in &routes {
        let rel = &graph.relations[route.relation];
        assert_eq!(route.points.len(), 4);
        let (first, last) = (route.points[0], route.points[3]);
        let child_col = &graph.entities[rel.child.0].columns[rel.child_columns[0]].name;
        let parent_col = &graph.entities[rel.parent.0].columns[rel.parent_columns[0]].name;
        assert_eq!(
            first.y,
            row_y(&layout, &graph, rel.child.0, child_col, AttributeMode::All)
        );
        assert_eq!(
            last.y,
            row_y(
                &layout,
                &graph,
                rel.parent.0,
                parent_col,
                AttributeMode::All
            )
        );
        let (c, p) = (layout.rects[rel.child.0], layout.rects[rel.parent.0]);
        if c.x < p.x {
            assert_eq!((first.x, last.x), (c.x + c.width, p.x));
        } else {
            assert_eq!((first.x, last.x), (c.x, p.x + p.width));
        }
        // Horizontal, vertical, horizontal.
        assert_eq!(route.points[0].y, route.points[1].y);
        assert_eq!(route.points[1].x, route.points[2].x);
        assert_eq!(route.points[2].y, route.points[3].y);
    }
    assert_eq!(
        routes,
        layout.route(&graph, AttributeMode::All, &m),
        "deterministic"
    );
}

#[test]
fn routes_use_header_centre_when_rows_are_hidden_or_loading() {
    let m = metrics();
    let graph = build_track(&|t| {
        if t.table == "album" {
            ready(simple(&["title"]))
        } else {
            RelatedProperties::Loading
        }
    });
    let layout = DiagramLayout::auto(&graph, AttributeMode::All, &m);
    let incoming = graph
        .relations
        .iter()
        .position(|r| r.parent == EntityId(0))
        .unwrap();
    let route = &layout.route(&graph, AttributeMode::All, &m)[incoming];
    assert_eq!(
        route.points[0].y,
        layout.rects[2].y + 28.0 + 11.0,
        "loading child shows its FK placeholder row"
    );

    let loading_parent = build_track(&|_| RelatedProperties::Loading);
    let layout = DiagramLayout::auto(&loading_parent, AttributeMode::All, &m);
    let outgoing = loading_parent
        .relations
        .iter()
        .position(|r| r.child == EntityId(0))
        .unwrap();
    let route = &layout.route(&loading_parent, AttributeMode::All, &m)[outgoing];
    assert_eq!(route.points[3].y, layout.rects[1].y + 14.0);

    // KeysOnly hides non-key columns: a parent column that is not a key falls back to the header.
    let mut p = simple(&["x"]);
    p.foreign_keys = Some(vec![fk("f", "t", &["x"], "other", &["code"])]);
    let other = TableProperties {
        columns: vec![col("id", "int", true), col("code", "text", false)],
        ..Default::default()
    };
    let graph = DiagramGraph::build(&table("t"), &p, &|_| ready(other.clone()));
    let layout = DiagramLayout::auto(&graph, AttributeMode::KeysOnly, &m);
    let route = &layout.route(&graph, AttributeMode::KeysOnly, &m)[0];
    assert_eq!(route.points[3].y, layout.rects[1].y + 14.0);
    let all = layout.route(&graph, AttributeMode::All, &m);
    assert_eq!(
        all[0].points[3].y,
        layout.rects[1].y + 28.0 + 6.0 + 22.0 + 11.0,
        "separator offsets non-PK rows"
    );
}

#[test]
fn parallel_relations_get_distinct_vertical_offsets() {
    let m = metrics();
    let mut p = simple(&["a", "b", "c"]);
    p.foreign_keys = Some(vec![
        fk("f1", "t", &["a"], "o", &["id"]),
        fk("f2", "t", &["b"], "o", &["id"]),
        fk("f3", "t", &["c"], "o", &["id"]),
    ]);
    let graph = DiagramGraph::build(&table("t"), &p, &|_| RelatedProperties::Loading);
    let layout = DiagramLayout::auto(&graph, AttributeMode::All, &m);
    let routes = layout.route(&graph, AttributeMode::All, &m);
    let xs: Vec<f32> = routes.iter().map(|r| r.points[1].x).collect();
    assert_eq!(xs.len(), 3);
    for i in 0..3 {
        for j in i + 1..3 {
            assert_ne!(xs[i], xs[j]);
        }
    }
    let gap_start = layout.rects[0].x + layout.rects[0].width;
    assert!(xs.iter().all(|&x| x > gap_start && x < layout.rects[1].x));
}

#[test]
fn self_reference_loops_out_of_the_right_edge() {
    let m = metrics();
    let mut p = simple(&["parent_id"]);
    p.foreign_keys = Some(vec![fk("s", "emp", &["parent_id"], "emp", &["id"])]);
    let graph = DiagramGraph::build(&table("emp"), &p, &|_| RelatedProperties::Loading);
    let layout = DiagramLayout::auto(&graph, AttributeMode::All, &m);
    let route = &layout.route(&graph, AttributeMode::All, &m)[0];
    let right = layout.rects[0].x + layout.rects[0].width;
    let pts = &route.points;
    assert_eq!((pts[0].x, pts[3].x), (right, right));
    assert!(pts[1].x > right && pts[2].x == pts[1].x);
    assert_eq!(
        pts[0].y,
        row_y(&layout, &graph, 0, "parent_id", AttributeMode::All)
    );
    assert_eq!(
        pts[3].y,
        row_y(&layout, &graph, 0, "id", AttributeMode::All)
    );
}

#[test]
fn viewport_round_trip_zoom_anchor_clamp_and_fit() {
    let mut v = Viewport {
        origin: DiagramPoint { x: 40.0, y: -20.0 },
        zoom: 1.0,
        min_zoom: Viewport::MIN_ZOOM,
    };
    let p = DiagramPoint { x: 123.0, y: 45.0 };
    let back = v.to_diagram(v.to_screen(p));
    assert!((back.x - p.x).abs() < 1e-3 && (back.y - p.y).abs() < 1e-3);

    let anchor = DiagramPoint { x: 300.0, y: 200.0 };
    let before = v.to_diagram(anchor);
    v.zoom_at(1.25, anchor);
    let after = v.to_diagram(anchor);
    assert_eq!(v.zoom, 1.25);
    assert!((before.x - after.x).abs() < 1e-3 && (before.y - after.y).abs() < 1e-3);
    v.zoom_at(100.0, anchor);
    assert_eq!(v.zoom, Viewport::MAX_ZOOM);
    let after = v.to_diagram(anchor);
    assert!((before.x - after.x).abs() < 1e-3);
    v.zoom_at(0.0001, anchor);
    assert_eq!(v.zoom, Viewport::MIN_ZOOM);
    v.zoom_at(f32::NAN, anchor);
    assert_eq!(v.zoom, Viewport::MIN_ZOOM);

    let mut v = Viewport {
        origin: DiagramPoint::default(),
        zoom: 2.0,
        min_zoom: Viewport::MIN_ZOOM,
    };
    let diagram_point = v.to_diagram(DiagramPoint { x: 100.0, y: 100.0 });
    v.pan(30.0, -10.0);
    let moved = v.to_screen(diagram_point);
    assert_eq!(
        (moved.x, moved.y),
        (130.0, 90.0),
        "content follows the drag"
    );

    let bounds = DiagramRect {
        x: 100.0,
        y: 50.0,
        width: 800.0,
        height: 400.0,
    };
    let fit = Viewport::fit(bounds, 1000.0, 600.0, 40.0, Viewport::MIN_ZOOM);
    assert!((fit.zoom - 920.0 / 800.0).abs() < 1e-5);
    let centre = fit.to_screen(DiagramPoint { x: 500.0, y: 250.0 });
    assert!((centre.x - 500.0).abs() < 1e-3 && (centre.y - 300.0).abs() < 1e-3);
    let tl = fit.to_screen(DiagramPoint { x: 100.0, y: 50.0 });
    assert!(tl.x >= 40.0 - 1e-3 && tl.y >= 40.0 - 1e-3);
    let huge = Viewport::fit(
        DiagramRect {
            x: 0.0,
            y: 0.0,
            width: 10_000.0,
            height: 10_000.0,
        },
        500.0,
        500.0,
        10.0,
        Viewport::MIN_ZOOM,
    );
    assert_eq!(huge.zoom, Viewport::MIN_ZOOM);
    // Whole-schema diagrams may zoom out further to be overviewed.
    let huge_rect = DiagramRect {
        x: 0.0,
        y: 0.0,
        width: 10_000.0,
        height: 10_000.0,
    };
    let overview = Viewport::fit(huge_rect, 1000.0, 1000.0, 20.0, Viewport::SCHEMA_MIN_ZOOM);
    assert_eq!(overview.zoom, Viewport::SCHEMA_MIN_ZOOM);
    let wide = DiagramRect {
        x: 0.0,
        y: 0.0,
        width: 4_000.0,
        height: 2_000.0,
    };
    let between = Viewport::fit(wide, 1000.0, 1000.0, 20.0, Viewport::SCHEMA_MIN_ZOOM);
    assert!(
        (between.zoom - 0.24).abs() < 1e-4,
        "fits below the table diagram's 50% floor"
    );
    let mut v = overview;
    v.zoom_at(0.0001, DiagramPoint::default());
    assert_eq!(v.zoom, Viewport::SCHEMA_MIN_ZOOM);
    assert_eq!(
        Viewport::fit(
            DiagramRect::default(),
            500.0,
            500.0,
            10.0,
            Viewport::MIN_ZOOM
        )
        .zoom,
        1.0
    );
}

/// `cargo test -p tablex-gpui --release diagram_pipeline_bench -- --ignored --nocapture`
#[test]
#[ignore = "benchmark; run in release with --nocapture"]
fn diagram_pipeline_bench() {
    use std::time::Instant;
    const RELATED: usize = 40;
    const COLUMNS: usize = 30;
    let wide = |prefix: &str| {
        let mut p = simple(&[]);
        p.columns.extend((0..COLUMNS - 1).map(|i| {
            col(
                &format!("{prefix}_column_{i}"),
                "character varying(64)",
                false,
            )
        }));
        p
    };
    let mut focus = wide("focus");
    let (mut fks, mut refs) = (Vec::new(), Vec::new());
    for i in 0..RELATED {
        if i % 2 == 0 {
            focus.columns.push(col(&format!("ref{i}_id"), "int", false));
            fks.push(fk(
                &format!("fp{i}"),
                "t",
                &[&format!("ref{i}_id")],
                &format!("rel{i:02}"),
                &["id"],
            ));
        } else {
            refs.push(fk(
                &format!("fc{i}"),
                &format!("rel{i:02}"),
                &["t_id"],
                "t",
                &["id"],
            ));
        }
    }
    focus.foreign_keys = Some(fks);
    focus.references = Some(refs);
    let related_props = std::sync::Arc::new(wide("rel"));
    let m = metrics();
    let focus_ref = table("t");

    let run = || {
        let graph = DiagramGraph::build(&focus_ref, &focus, &|_| {
            RelatedProperties::Ready(related_props.clone())
        });
        let layout = DiagramLayout::auto(&graph, AttributeMode::All, &m);
        let routes = layout.route(&graph, AttributeMode::All, &m);
        (graph.entities.len(), routes.len(), layout.rects.len())
    };
    let _ = run();
    let iterations = 500;
    let start = Instant::now();
    let mut sink = 0;
    for _ in 0..iterations {
        sink += std::hint::black_box(run()).1;
    }
    let per = start.elapsed() / iterations;
    let graph = DiagramGraph::build(&focus_ref, &focus, &|_| {
        RelatedProperties::Ready(related_props.clone())
    });
    let layout = DiagramLayout::auto(&graph, AttributeMode::All, &m);
    let hit_start = Instant::now();
    let mut hits = 0;
    for i in 0..100_000 {
        hits += layout
            .hit_test(DiagramPoint {
                x: i as f32 % 900.0,
                y: i as f32 % 700.0,
            })
            .is_some() as usize;
    }
    let hit = hit_start.elapsed() / 100_000;
    println!("diagram build+auto+route ({} entities, {COLUMNS} cols): {per:?}/iter; hit_test {hit:?}/call (sink {sink}, hits {hits})", graph.entities.len());
    assert_eq!(graph.entities.len(), RELATED + 1);
}

// ---- Phase 2: whole-schema graph --------------------------------------

/// `n` tables named `t000…`, each with an `id` PK and `extra` text columns;
/// `edges` are `(child, parent)` indices, one FK column `fk_<parent>` each.
fn schema_tables(
    n: usize,
    extra: usize,
    edges: &[(usize, usize)],
) -> Vec<(TableRef, RelatedProperties)> {
    (0..n)
        .map(|i| {
            let mut p = simple(&[]);
            p.columns
                .extend((0..extra).map(|c| col(&format!("c{c}"), "varchar(40)", false)));
            let mut fks = Vec::new();
            for &(child, parent) in edges.iter().filter(|e| e.0 == i) {
                p.columns.push(col(&format!("fk_{parent}"), "int", false));
                let target = if parent % 2 == 0 {
                    format!("t{parent:03}")
                } else {
                    format!("public.t{parent:03}")
                };
                fks.push(fk(
                    &format!("fk_{child}_{parent}"),
                    &format!("t{child:03}"),
                    &[&format!("fk_{parent}")],
                    &target,
                    &["id"],
                ));
            }
            p.foreign_keys = Some(fks);
            (table(&format!("t{i:03}")), ready(p))
        })
        .collect()
}

#[test]
fn build_schema_links_every_table_and_counts_external_fks() {
    let mut tables = schema_tables(4, 1, &[(0, 1), (1, 2), (2, 2), (3, 0)]);
    // t3 also points at a table outside the set.
    let RelatedProperties::Ready(p) = &tables[3].1 else {
        unreachable!()
    };
    let mut p = (**p).clone();
    p.foreign_keys
        .as_mut()
        .unwrap()
        .push(fk("ext", "t003", &["c0"], "other.audit", &["id"]));
    tables[3].1 = ready(p);
    let graph = DiagramGraph::build_schema(&tables);
    assert_eq!(names(&graph), ["t000", "t001", "t002", "t003"]);
    assert!(graph.entities.iter().all(|e| !e.focus));
    assert_eq!(graph.relations.len(), 4);
    assert_eq!(graph.external_relations, 1);
    assert!(
        graph
            .relations
            .iter()
            .any(|r| r.child == r.parent && r.child == EntityId(2)),
        "self-reference kept"
    );
    let r = graph
        .relations
        .iter()
        .find(|r| r.child == EntityId(3))
        .unwrap();
    assert_eq!((r.parent, r.parent_columns.clone()), (EntityId(0), vec![0]));
    assert_eq!(
        DiagramGraph::build(&table("t"), &simple(&[]), &|_| RelatedProperties::Loading)
            .external_relations,
        0
    );
}

#[test]
fn build_schema_is_input_order_independent_and_dedups() {
    let tables = schema_tables(5, 2, &[(0, 1), (2, 1), (3, 4), (4, 0)]);
    let forward = DiagramGraph::build_schema(&tables);
    let mut shuffled = tables.clone();
    shuffled.reverse();
    shuffled.push(tables[1].clone()); // duplicate entry
    assert_eq!(forward, DiagramGraph::build_schema(&shuffled));
    // The parent's `references` entry repeats a child FK: still one relation.
    let mut with_refs = tables.clone();
    let RelatedProperties::Ready(p) = &tables[1].1 else {
        unreachable!()
    };
    let mut p = (**p).clone();
    p.references = Some(vec![fk("fk_0_1", "t000", &["fk_1"], "t001", &["id"])]);
    with_refs[1].1 = ready(p);
    assert_eq!(
        forward.relations.len(),
        DiagramGraph::build_schema(&with_refs).relations.len()
    );
}

#[test]
fn build_schema_uses_parent_references_for_loading_children() {
    let mut tables = schema_tables(2, 0, &[]);
    let RelatedProperties::Ready(p) = &tables[0].1 else {
        unreachable!()
    };
    let mut p = (**p).clone();
    p.references = Some(vec![fk("fk_child", "t001", &["t0_id"], "t000", &["id"])]);
    tables[0].1 = ready(p);
    tables[1].1 = RelatedProperties::Loading;
    let graph = DiagramGraph::build_schema(&tables);
    assert_eq!(graph.entities[1].state, EntityState::Loading);
    assert_eq!(graph.entities[1].columns.len(), 1);
    assert_eq!(
        (graph.relations[0].child, graph.relations[0].parent),
        (EntityId(1), EntityId(0))
    );
    assert_eq!(graph.relations[0].child_columns, [0]);
}

#[test]
fn build_schema_qualifies_labels_only_when_schemas_are_mixed() {
    let same = DiagramGraph::build_schema(&schema_tables(2, 0, &[]));
    assert_eq!(same.entities[0].label, "t000");
    let mut mixed = schema_tables(2, 0, &[]);
    mixed[1].0.schema = Some("sales".into());
    let graph = DiagramGraph::build_schema(&mixed);
    assert_eq!(
        (
            graph.entities[0].label.as_str(),
            graph.entities[1].label.as_str()
        ),
        ("public.t000", "sales.t001")
    );
}

fn schema_layout(
    tables: &[(TableRef, RelatedProperties)],
    mode: AttributeMode,
) -> (DiagramGraph, DiagramLayout) {
    let graph = DiagramGraph::build_schema(tables);
    let layout = DiagramLayout::auto_schema(&graph, mode, &metrics());
    (graph, layout)
}

#[test]
fn auto_schema_ranks_parents_right_of_children() {
    // Diamond + chain: 0->1->3, 0->2->3, 3->4.
    let (graph, layout) = schema_layout(
        &schema_tables(5, 2, &[(0, 1), (0, 2), (1, 3), (2, 3), (3, 4)]),
        AttributeMode::All,
    );
    assert_no_overlap(&layout);
    for r in &graph.relations {
        let (c, p) = (layout.rects[r.child.0], layout.rects[r.parent.0]);
        assert!(c.x + c.width < p.x, "{:?} -> {:?}", c, p);
    }
}

#[test]
fn auto_schema_breaks_cycles() {
    // 0->1->2->0 plus a tail 2->3 and a self reference on 3.
    let (graph, layout) = schema_layout(
        &schema_tables(4, 1, &[(0, 1), (1, 2), (2, 0), (2, 3), (3, 3)]),
        AttributeMode::All,
    );
    assert_no_overlap(&layout);
    let forward = graph
        .relations
        .iter()
        .filter(|r| r.child != r.parent)
        .filter(|r| layout.rects[r.child.0].x < layout.rects[r.parent.0].x)
        .count();
    assert!(
        forward >= 3,
        "all but the broken cycle edge point rightwards"
    );
    assert!(layout.rects[3].x > layout.rects[0].x);
    assert_eq!(
        layout,
        DiagramLayout::auto_schema(&graph, AttributeMode::All, &metrics())
    );
}

#[test]
fn auto_schema_separates_components_and_grids_isolated_tables() {
    // Two components {0,1,2} and {3,4}; 5..=12 isolated.
    let edges = [(0, 1), (1, 2), (3, 4)];
    let (graph, layout) = schema_layout(&schema_tables(13, 3, &edges), AttributeMode::All);
    assert_no_overlap(&layout);
    let bounds_of = |ids: &[usize]| {
        let (x0, y0, x1, y1) = ids.iter().fold(
            (f32::MAX, f32::MAX, f32::MIN, f32::MIN),
            |(a, b, c, d), &i| {
                let r = layout.rects[i];
                (
                    a.min(r.x),
                    b.min(r.y),
                    c.max(r.x + r.width),
                    d.max(r.y + r.height),
                )
            },
        );
        DiagramRect {
            x: x0,
            y: y0,
            width: x1 - x0,
            height: y1 - y0,
        }
    };
    assert!(
        !overlaps(&bounds_of(&[0, 1, 2]), &bounds_of(&[3, 4])),
        "component boxes are disjoint"
    );
    let components_bottom = bounds_of(&[0, 1, 2, 3, 4]).y + bounds_of(&[0, 1, 2, 3, 4]).height;
    let isolated: Vec<_> = (5..13).map(|i| layout.rects[i]).collect();
    assert!(
        isolated.iter().all(|r| r.y >= components_bottom),
        "isolated tables sit below the components"
    );
    // Grid: shared x columns, rows top-aligned.
    let mut xs: Vec<f32> = isolated.iter().map(|r| r.x).collect();
    xs.sort_by(f32::total_cmp);
    xs.dedup();
    assert!(
        xs.len() > 1 && xs.len() < isolated.len(),
        "several columns and rows: {xs:?}"
    );
    assert_eq!(graph.relations.len(), 3);
    // Only isolated tables: the grid starts at the origin.
    let (_, only) = schema_layout(&schema_tables(6, 1, &[]), AttributeMode::All);
    assert_no_overlap(&only);
    assert_eq!((only.rects[0].x, only.rects[0].y), (0.0, 0.0));
    assert!(
        DiagramLayout::auto_schema(&DiagramGraph::default(), AttributeMode::All, &metrics())
            .rects
            .is_empty()
    );
}

#[test]
fn auto_schema_no_overlap_and_deterministic_over_generated_schemas() {
    let mut rng = Rng(0x2545f4914f6cdd1d);
    for _ in 0..100 {
        let n = 1 + rng.next(60) as usize;
        let edge_count = rng.next(n as u64 * 2) as usize;
        let edges: Vec<_> = (0..edge_count)
            .map(|_| (rng.next(n as u64) as usize, rng.next(n as u64) as usize))
            .collect();
        let tables = schema_tables(n, rng.next(20) as usize, &edges);
        for mode in AttributeMode::ALL {
            let (graph, layout) = schema_layout(&tables, mode);
            assert_eq!(layout.rects.len(), n);
            assert_no_overlap(&layout);
            assert_eq!(layout, DiagramLayout::auto_schema(&graph, mode, &metrics()));
            let routes = layout.route(&graph, mode, &metrics());
            assert_eq!(routes.len(), graph.relations.len());
            assert!(routes
                .iter()
                .flat_map(|r| &r.points)
                .all(|p| p.x.is_finite() && p.y.is_finite()));
        }
    }
}

#[test]
fn visible_matches_brute_force_with_margin() {
    let (_, layout) = schema_layout(
        &schema_tables(80, 4, &[(0, 1), (1, 2), (5, 6), (7, 8), (9, 3)]),
        AttributeMode::All,
    );
    let mut rng = Rng(99);
    for _ in 0..200 {
        let zoom = 0.5 + rng.next(31) as f32 / 20.0;
        let b = layout.bounds();
        let viewport = Viewport {
            origin: DiagramPoint {
                x: b.x + rng.next(b.width as u64 + 1) as f32 - 100.0,
                y: b.y + rng.next(b.height as u64 + 1) as f32 - 100.0,
            },
            zoom,
            min_zoom: Viewport::MIN_ZOOM,
        };
        let (w, h) = (200.0 + rng.next(800) as f32, 150.0 + rng.next(600) as f32);
        let got: Vec<_> = layout.visible(&viewport, w, h).collect();
        // Everything actually on screen is reported; nothing far outside is.
        for (i, r) in layout.rects.iter().enumerate() {
            let tl = viewport.to_screen(DiagramPoint { x: r.x, y: r.y });
            let br = viewport.to_screen(DiagramPoint {
                x: r.x + r.width,
                y: r.y + r.height,
            });
            let on_screen = tl.x < w && br.x > 0.0 && tl.y < h && br.y > 0.0;
            let far = tl.x >= w + 33.0 || br.x <= -33.0 || tl.y >= h + 33.0 || br.y <= -33.0;
            if on_screen {
                assert!(got.contains(&EntityId(i)), "entity {i} on screen");
            }
            if far {
                assert!(!got.contains(&EntityId(i)), "entity {i} far outside");
            }
        }
        assert!(got.windows(2).all(|w| w[0] < w[1]), "draw order");
    }
}

/// `cargo test -p tablex-gpui --release diagram_schema_bench -- --ignored --nocapture`
#[test]
#[ignore = "benchmark; run in release with --nocapture"]
fn diagram_schema_bench() {
    use std::time::Instant;
    let mut rng = Rng(0xdeadbeef);
    let edges: Vec<(usize, usize)> = (0..400)
        .map(|_| (rng.next(300) as usize, rng.next(300) as usize))
        .collect();
    let tables = schema_tables(300, 14, &edges);
    let m = metrics();
    let run = || {
        let graph = DiagramGraph::build_schema(&tables);
        let layout = DiagramLayout::auto_schema(&graph, AttributeMode::All, &m);
        let routes = layout.route(&graph, AttributeMode::All, &m);
        (graph.relations.len(), routes.len())
    };
    let _ = run();
    let iterations = 50;
    let start = Instant::now();
    let mut sink = 0;
    for _ in 0..iterations {
        sink += std::hint::black_box(run()).1;
    }
    let total = start.elapsed() / iterations;
    let graph = DiagramGraph::build_schema(&tables);
    let t = Instant::now();
    let layout = DiagramLayout::auto_schema(&graph, AttributeMode::All, &m);
    let layout_time = t.elapsed();
    let t = Instant::now();
    let routes = layout.route(&graph, AttributeMode::All, &m);
    let route_time = t.elapsed();
    let viewport = Viewport {
        origin: DiagramPoint::default(),
        zoom: 1.0,
        min_zoom: Viewport::MIN_ZOOM,
    };
    let t = Instant::now();
    let mut seen = 0;
    for _ in 0..10_000 {
        seen += std::hint::black_box(layout.visible(&viewport, 1400.0, 900.0)).count();
    }
    let visible_time = t.elapsed() / 10_000;
    println!(
        "schema 300 tables x 15 cols, {} relations: build+auto_schema+route {total:?}/iter (auto_schema {layout_time:?}, route {route_time:?} for {} routes); visible {visible_time:?}/call ({} avg shown; sink {sink})",
        graph.relations.len(), routes.len(), seen / 10_000
    );
}

// ---- Route channels -----------------------------------------------------

/// Chinook-like: InvoiceLine -> Invoice, Track; PlaylistTrack -> Track;
/// Track -> Album, Genre, MediaType; Album -> Artist.
fn chinook_edges() -> Vec<(usize, usize)> {
    // 0 InvoiceLine, 1 Invoice, 2 Track, 3 PlaylistTrack, 4 Album, 5 Genre, 6 MediaType, 7 Artist
    vec![(0, 1), (0, 2), (3, 2), (2, 4), (2, 5), (2, 6), (4, 7)]
}

fn assert_distinct_channels(
    graph: &DiagramGraph,
    layout: &DiagramLayout,
    routes: &[super::layout::RoutedRelation],
) {
    // Loops and overlapping-box routes run beside the boxes, not in a gap.
    let in_gap = |r: &&super::layout::RoutedRelation| {
        let rel = &graph.relations[r.relation];
        let parent = layout.rects[rel.parent.0];
        rel.child != rel.parent && r.points[3].x != parent.x + parent.width
    };
    let gapped: Vec<_> = routes.iter().filter(in_gap).collect();
    for (i, a) in gapped.iter().enumerate() {
        for b in &gapped[i + 1..] {
            if (a.points[1].x - b.points[1].x).abs() >= 1e-3 {
                continue;
            }
            // A shared vertical x is only allowed from the same child row.
            let same_row = graph.relations[a.relation].child == graph.relations[b.relation].child
                && a.points[0] == b.points[0];
            assert!(
                same_row,
                "relations {} and {} share x {}",
                a.relation, b.relation, a.points[1].x
            );
        }
    }
}

#[test]
fn route_channels_are_distinct_per_gap_in_both_layouts() {
    let m = metrics();
    let tables = schema_tables(8, 6, &chinook_edges());
    let (graph, layout) = schema_layout(&tables, AttributeMode::All);
    let routes = layout.route(&graph, AttributeMode::All, &m);
    assert_eq!(routes.len(), 7);
    assert_distinct_channels(&graph, &layout, &routes);
    // Channels stay between the child's and the parent's facing edges.
    for r in &routes {
        let rel = &graph.relations[r.relation];
        let (c, p) = (layout.rects[rel.child.0], layout.rects[rel.parent.0]);
        let x = r.points[1].x;
        assert!(
            x > c.x.min(p.x) && x < (c.x + c.width).max(p.x + p.width),
            "{x} outside {c:?} / {p:?}"
        );
    }
    assert_eq!(routes, layout.route(&graph, AttributeMode::All, &m));

    // Focus layout: three FKs of the focus into three parents + children.
    let mut p = simple(&["a", "b", "c"]);
    p.foreign_keys = Some(vec![
        fk("f1", "t", &["a"], "pa", &["id"]),
        fk("f2", "t", &["b"], "pb", &["id"]),
        fk("f3", "t", &["c"], "pc", &["id"]),
    ]);
    p.references = Some(vec![
        fk("r1", "k1", &["t_id"], "t", &["id"]),
        fk("r2", "k2", &["t_id"], "t", &["id"]),
    ]);
    let graph = DiagramGraph::build(&table("t"), &p, &|_| RelatedProperties::Loading);
    let layout = DiagramLayout::auto(&graph, AttributeMode::All, &m);
    let routes = layout.route(&graph, AttributeMode::All, &m);
    assert_eq!(routes.len(), 5);
    assert_distinct_channels(&graph, &layout, &routes);
}

#[test]
fn route_channels_stay_distinct_after_drags_and_on_random_schemas() {
    let m = metrics();
    let mut rng = Rng(0x51ed270b);
    for round in 0..60 {
        let n = 2 + rng.next(30) as usize;
        let edges: Vec<_> = (0..rng.next(n as u64 * 2) as usize)
            .map(|_| (rng.next(n as u64) as usize, rng.next(n as u64) as usize))
            .collect();
        let tables = schema_tables(n, 3, &edges);
        let (graph, mut layout) = schema_layout(&tables, AttributeMode::All);
        for mode in AttributeMode::ALL {
            let routes = layout.route(&graph, mode, &m);
            assert_distinct_channels(&graph, &layout, &routes);
            assert_eq!(routes, layout.route(&graph, mode, &m));
        }
        if round % 2 == 0 {
            let e = rng.next(n as u64) as usize;
            layout.move_entity(EntityId(e), 37.0, -21.0);
            let routes = layout.route(&graph, AttributeMode::All, &m);
            assert_distinct_channels(&graph, &layout, &routes);
            for r in &routes {
                assert_eq!(r.points.len(), 4);
                assert!(r.points.iter().all(|p| p.x.is_finite() && p.y.is_finite()));
            }
        }
    }
}

#[test]
fn route_channels_order_by_source_row_to_limit_crossings() {
    // Two children stacked left of one parent; both run downwards to the same
    // parent row. The lower child must take the channel nearer to itself.
    let m = metrics();
    let layout = DiagramLayout {
        rects: vec![
            DiagramRect {
                x: 400.0,
                y: 300.0,
                width: 160.0,
                height: 100.0,
            },
            DiagramRect {
                x: 0.0,
                y: 0.0,
                width: 160.0,
                height: 100.0,
            },
            DiagramRect {
                x: 0.0,
                y: 150.0,
                width: 160.0,
                height: 100.0,
            },
        ],
    };
    let mut parent = simple(&[]);
    parent.references = Some(vec![
        fk("a", "upper", &["pid"], "p", &["id"]),
        fk("b", "lower", &["pid"], "p", &["id"]),
    ]);
    let graph = DiagramGraph::build(&table("p"), &parent, &|_| RelatedProperties::Loading);
    assert_eq!(names(&graph), ["p", "lower", "upper"]);
    let layout = DiagramLayout {
        rects: vec![layout.rects[0], layout.rects[2], layout.rects[1]],
    };
    let routes = layout.route(&graph, AttributeMode::All, &m);
    let x_of = |name: &str| {
        let e = graph
            .entities
            .iter()
            .position(|e| e.table.table == name)
            .unwrap();
        let r = graph.relations.iter().position(|r| r.child.0 == e).unwrap();
        routes.iter().find(|x| x.relation == r).unwrap().points[1].x
    };
    assert!(
        x_of("lower") < x_of("upper"),
        "{} {}",
        x_of("lower"),
        x_of("upper")
    );
}

// ---- D1: export, notation, search, minimap, saved positions, JOIN SQL --------------------

mod d1 {
    use super::*;
    use crate::app::results::diagram::export::{
        png_scale, rasterize, ExportFormat, ExportRequest, PNG_MAX_PIXELS, PNG_MAX_SIDE,
    };
    use crate::app::results::diagram::join_sql;
    use crate::app::results::diagram::layout::{hit_route, Minimap};
    use crate::app::results::diagram::notation::{route_directions, EndMark, RelationNotation};
    use crate::app::results::diagram::store::{DiagramKey, LayoutStore, SavedDiagram, StoreError};
    use crate::sql_table::SqlDialect;
    use crate::catalog::CatalogIndex;
    use std::sync::Arc;

    fn column(name: &str, ty: &str, pk: bool, nullable: bool) -> CatalogColumn {
        CatalogColumn { name: name.into(), data_type: ty.into(), nullable, primary_key: pk, ..Default::default() }
    }

    fn unique_constraint(cols: &[&str]) -> CatalogConstraint {
        CatalogConstraint {
            name: "uq".into(),
            kind: CatalogConstraintKind::Unique,
            columns: cols.iter().map(|c| (*c).into()).collect(),
            definition: None,
        }
    }

    /// `orders` (focus) -> `customers` (mandatory FK, many orders per
    /// customer); `order_items` -> `orders` (nullable + unique FK).
    pub(super) fn fixture() -> DiagramGraph {
        let orders = TableProperties {
            columns: vec![
                column("id", "integer", true, false),
                column("customer_id", "integer", false, false),
                column("note", "text", false, true),
            ],
            keys: Some(vec![pk_constraint(&["id"])]),
            foreign_keys: Some(vec![fk("fk_orders_customer", "orders", &["customer_id"], "public.customers", &["id"])]),
            references: Some(vec![fk("fk_items_order", "public.order_items", &["order_id"], "orders", &["id"])]),
            ..Default::default()
        };
        let customers = TableProperties {
            columns: vec![column("id", "integer", true, false), column("name", "varchar(80)", false, false)],
            keys: Some(vec![pk_constraint(&["id"])]),
            ..Default::default()
        };
        let items = TableProperties {
            columns: vec![
                column("id", "integer", true, false),
                column("order_id", "integer", false, true),
                column("sku", "character varying", false, false),
            ],
            keys: Some(vec![pk_constraint(&["id"]), unique_constraint(&["order_id"])]),
            foreign_keys: Some(vec![fk("fk_items_order", "order_items", &["order_id"], "orders", &["id"])]),
            ..Default::default()
        };
        DiagramGraph::build(&table("orders"), &orders, &|t| match t.table.as_str() {
            "customers" => ready(customers.clone()),
            "order_items" => ready(items.clone()),
            _ => RelatedProperties::Loading,
        })
    }

    pub(super) fn request(graph: DiagramGraph) -> ExportRequest {
        let m = metrics();
        let layout = DiagramLayout::auto(&graph, AttributeMode::All, &m);
        ExportRequest {
            graph: Arc::new(graph),
            layout,
            mode: AttributeMode::All,
            notation: RelationNotation::CrowsFoot,
            metrics: m,
            annotations: Arc::default(),
        }
    }

    fn fixed_request() -> ExportRequest {
        let mut request = request(fixture());
        request.layout.rects = vec![
            DiagramRect { x: 300.0, y: 40.0, width: 160.0, height: 100.0 },
            DiagramRect { x: 600.0, y: 40.0, width: 160.0, height: 78.0 },
            DiagramRect { x: 0.0, y: 40.0, width: 160.0, height: 100.0 },
        ];
        request
    }

    #[test]
    fn relations_know_their_nullability_and_uniqueness() {
        let graph = fixture();
        assert_eq!(names(&graph), ["orders", "customers", "order_items"]);
        // Sorted by (child, parent): order_items -> orders, orders -> customers.
        let items = &graph.relations[0];
        assert_eq!((graph.entities[items.child.0].table.table.as_str(), items.optional, items.unique), ("order_items", true, true));
        let customer = &graph.relations[1];
        assert_eq!((graph.entities[customer.child.0].table.table.as_str(), customer.optional, customer.unique), ("orders", false, false));
    }

    #[test]
    fn a_unique_key_must_lie_inside_the_foreign_key_columns() {
        // PK (order_id, sku) with FK (order_id) is not one-to-one; a unique
        // INDEX on the FK column is.
        let child = TableProperties {
            columns: vec![column("order_id", "integer", true, false), column("sku", "text", true, false)],
            keys: Some(vec![pk_constraint(&["order_id", "sku"])]),
            foreign_keys: Some(vec![fk("f", "line", &["order_id"], "orders", &["id"])]),
            ..Default::default()
        };
        let graph = DiagramGraph::build(&table("line"), &child, &|_| ready(simple(&[])));
        assert!(!graph.relations[0].unique);
        let mut indexed = child.clone();
        indexed.indexes = Some(vec![CatalogIndex {
            name: "u".into(),
            columns: vec!["order_id".into()],
            unique: true,
            primary: false,
            method: None,
            definition: None,
        }]);
        let graph = DiagramGraph::build(&table("line"), &indexed, &|_| ready(simple(&[])));
        assert!(graph.relations[0].unique);
        // While the child loads nothing is known: optional, not unique.
        let loading = DiagramGraph::build(&table("track"), &track_properties(), &|_| RelatedProperties::Loading);
        let incoming = loading.relations.iter().find(|r| loading.entities[r.child.0].table.table == "invoice_line").unwrap();
        assert!(incoming.optional && !incoming.unique);
    }

    #[test]
    fn crows_foot_geometry_is_a_foot_bars_and_a_ring() {
        let tip = DiagramPoint { x: 100.0, y: 50.0 };
        let p = |x: f32, y: f32| DiagramPoint { x, y };
        let many = EndMark::OneOrMany.shape(tip, 1.0, 1.0);
        assert_eq!(
            many.lines(),
            [(p(100.0, 45.0), p(110.0, 50.0)), (p(110.0, 50.0), p(100.0, 55.0)), (p(110.0, 45.0), p(110.0, 55.0))]
        );
        assert_eq!(many.ring, None);
        let one = EndMark::One.shape(tip, -1.0, 2.0);
        assert_eq!(one.lines(), [(p(80.0, 40.0), p(80.0, 60.0)), (p(70.0, 40.0), p(70.0, 60.0))]);
        let optional = EndMark::ZeroOrOne.shape(tip, 1.0, 1.0);
        assert_eq!(optional.lines(), [(p(110.0, 45.0), p(110.0, 55.0))]);
        assert_eq!(optional.ring, Some((p(115.0, 50.0), 4.0)));
        // The mark chosen from the relation.
        let graph = fixture();
        assert_eq!(EndMark::child(&graph.relations[1]), EndMark::OneOrMany);
        assert_eq!(EndMark::parent(&graph.relations[1]), EndMark::One);
        assert_eq!(EndMark::child(&graph.relations[0]), EndMark::One);
        assert_eq!(EndMark::parent(&graph.relations[0]), EndMark::ZeroOrOne);
    }

    #[test]
    fn route_ends_point_away_from_their_entities() {
        let request = fixed_request();
        let routes = request.layout.route(&request.graph, AttributeMode::All, &request.metrics);
        // order_items (left) -> orders: the child end leaves rightwards, the
        // parent end looks back leftwards.
        let (child_dir, parent_dir) = route_directions(&routes[0].points).unwrap();
        assert_eq!((child_dir, parent_dir), (1.0, -1.0));
        assert_eq!(route_directions(&[]), None);
    }

    #[test]
    fn mermaid_output_is_exact() {
        let expected = "\
erDiagram
  orders {
    integer id PK
    integer customer_id FK
    text note
  }
  customers {
    integer id PK
    varchar80 name
  }
  order_items {
    integer id PK
    integer order_id FK
    character_varying sku
  }
  orders |o--o| order_items : \"order_id -> id\"
  customers ||--o{ orders : \"customer_id -> id\"
";
        assert_eq!(fixed_request().mermaid(), expected);
    }

    #[test]
    fn dbml_output_is_exact() {
        let expected = "\
// Generated by TableX

Table public.orders {
  id integer [pk, not null]
  customer_id integer [not null]
  note text
}

Table public.customers {
  id integer [pk, not null]
  name varchar(80) [not null]
}

Table public.order_items {
  id integer [pk, not null]
  order_id integer
  sku \"character varying\" [not null]
}

Ref: public.order_items.order_id - public.orders.id
Ref: public.orders.customer_id > public.customers.id
";
        assert_eq!(fixed_request().dbml(), expected);
    }

    #[test]
    fn json_snapshot_is_exact() {
        let json = fixed_request().json().unwrap();
        let value: serde_json::Value = serde_json::from_str(&json).unwrap();
        let expected = serde_json::json!({
            "format": "tablex-diagram",
            "version": 1,
            "kind": "table",
            "database": "db",
            "schema": "public",
            "tables": [
                {"name": "orders", "schema": "public", "focus": true,
                 "position": {"x": 300.0, "y": 40.0, "width": 160.0, "height": 100.0},
                 "columns": [
                    {"name": "id", "dataType": "integer", "nullable": false, "primaryKey": true, "foreignKey": false},
                    {"name": "customer_id", "dataType": "integer", "nullable": false, "primaryKey": false, "foreignKey": true},
                    {"name": "note", "dataType": "text", "nullable": true, "primaryKey": false, "foreignKey": false}]},
                {"name": "customers", "schema": "public", "focus": false,
                 "position": {"x": 600.0, "y": 40.0, "width": 160.0, "height": 78.0},
                 "columns": [
                    {"name": "id", "dataType": "integer", "nullable": false, "primaryKey": true, "foreignKey": false},
                    {"name": "name", "dataType": "varchar(80)", "nullable": false, "primaryKey": false, "foreignKey": false}]},
                {"name": "order_items", "schema": "public", "focus": false,
                 "position": {"x": 0.0, "y": 40.0, "width": 160.0, "height": 100.0},
                 "columns": [
                    {"name": "id", "dataType": "integer", "nullable": false, "primaryKey": true, "foreignKey": false},
                    {"name": "order_id", "dataType": "integer", "nullable": true, "primaryKey": false, "foreignKey": true},
                    {"name": "sku", "dataType": "character varying", "nullable": false, "primaryKey": false, "foreignKey": false}]}
            ],
            "relations": [
                {"name": "fk_items_order",
                 "child": {"schema": "public", "table": "order_items", "columns": ["order_id"]},
                 "parent": {"schema": "public", "table": "orders", "columns": ["id"]},
                 "optional": true, "unique": true},
                {"name": "fk_orders_customer",
                 "child": {"schema": "public", "table": "orders", "columns": ["customer_id"]},
                 "parent": {"schema": "public", "table": "customers", "columns": ["id"]},
                 "optional": false, "unique": false}
            ]
        });
        assert_eq!(value, expected);
        assert!(json.ends_with("}\n"));
    }

    fn parse_svg(svg: &str) -> resvg::usvg::Tree {
        resvg::usvg::Tree::from_str(svg, &resvg::usvg::Options::default()).expect("the SVG parses")
    }

    #[test]
    fn svg_is_well_formed_and_draws_every_table_and_relation() {
        for notation in [RelationNotation::CrowsFoot, RelationNotation::Plain] {
            let mut request = fixed_request();
            request.notation = notation;
            let svg = request.svg();
            let tree = parse_svg(&svg);
            assert!(tree.size().width() > 0.0 && tree.size().height() > 0.0);
            // Extent: boxes 0..760 x 40..140 plus 24 padding on each side.
            assert_eq!((tree.size().width(), tree.size().height()), (808.0, 148.0));
            assert_eq!(svg.matches("class=\"entity\"").count(), 3);
            assert_eq!(svg.matches("class=\"relation\"").count(), 2);
            for label in ["orders", "customers", "order_items"] {
                assert!(svg.contains(&format!(">{label}</text>")), "{label} is drawn");
            }
            for column in ["customer_id", "order_id", "varchar(80)", "note"] {
                assert!(svg.contains(&format!(">{column}</text>")), "row {column}");
            }
            assert!(svg.contains("order_items(order_id) -&gt; orders(id)"), "{svg}");
            assert!(svg.contains("<title>orders(customer_id) -&gt; customers(id)</title>") || svg.contains("orders(customer_id) -&gt; customers(id)"));
            // The tree keeps one group per entity.
            for id in ["entity-0", "entity-1", "entity-2"] {
                assert!(tree.node_by_id(id).is_some(), "{id}");
            }
            // Crow's foot draws rings / bars; plain draws the FK dot + arrow.
            assert!(svg.contains("<circle"));
        }
    }

    #[test]
    fn svg_escapes_markup_in_names_and_follows_the_attribute_mode() {
        let mut request = fixed_request();
        let mut graph = (*request.graph).clone();
        graph.entities[0].label = "a<b&\"c\"".into();
        request.graph = Arc::new(graph);
        request.mode = AttributeMode::None;
        request.layout = DiagramLayout::auto(&request.graph, AttributeMode::None, &request.metrics);
        let svg = request.svg();
        parse_svg(&svg);
        assert!(svg.contains("a&lt;b&amp;&quot;c&quot;"));
        assert!(!svg.contains(">note</text>"), "no rows in the None mode");
    }

    fn decode(png: &[u8]) -> (u32, u32) {
        let image = image::load_from_memory_with_format(png, image::ImageFormat::Png).expect("a decodable PNG");
        (image.width(), image.height())
    }

    #[test]
    fn png_is_the_svg_rasterized_at_twice_the_size() {
        let request = fixed_request();
        let bytes = request.render(ExportFormat::Png).unwrap();
        assert_eq!(&bytes[..8], b"\x89PNG\r\n\x1a\n");
        // 808 x 148 at 2x.
        assert_eq!(decode(&bytes), (1616, 296));
        assert_eq!(rasterize(&request.svg()).unwrap(), bytes, "deterministic");
        // `DIAGRAM_EXPORT_DIR=… cargo test … png_is_the_svg` leaves the samples
        // behind for a visual check.
        if let Some(dir) = std::env::var_os("DIAGRAM_EXPORT_DIR") {
            for format in ExportFormat::ALL {
                request.write(format, &std::path::Path::new(&dir).join(format.file_name("sample"))).unwrap();
            }
        }
    }

    #[test]
    fn png_scale_is_capped() {
        assert_eq!(png_scale(808.0, 148.0), 2.0);
        // Longest side: never beyond 16384 px.
        let scale = png_scale(20000.0, 100.0);
        assert!((20000.0 * scale).ceil() <= PNG_MAX_SIDE + 1.0 && scale < 1.0);
        // Pixel budget.
        let scale = png_scale(12000.0, 12000.0);
        assert!(12000.0 * scale * 12000.0 * scale <= PNG_MAX_PIXELS * 1.001);
        assert_eq!(png_scale(0.0, 10.0), 2.0);
    }

    #[test]
    fn file_names_keep_to_safe_characters() {
        assert_eq!(ExportFormat::Svg.file_name("orders-diagram"), "orders-diagram.svg");
        assert_eq!(ExportFormat::Mermaid.file_name("my table/x"), "my-table-x.mmd");
        assert_eq!(ExportFormat::Dbml.file_name("///"), "diagram.dbml");
        assert_eq!(ExportFormat::Json.extension(), "json");
    }

    #[test]
    fn search_matches_table_and_column_names_case_insensitively() {
        let graph = fixture();
        let ids = |query: &str| graph.search(query).into_iter().map(|id| id.0).collect::<Vec<_>>();
        assert_eq!(ids("CUST"), [0, 1], "orders has customer_id, customers is the table");
        assert_eq!(ids("  note "), [0]);
        assert_eq!(ids("sku"), [2]);
        assert_eq!(ids("order_items"), [2]);
        assert!(ids("   ").is_empty() && ids("zzz").is_empty());
        let mut graph = fixture();
        graph.entities[1].label = "Ünïcode".into();
        assert_eq!(graph.search("ünï").into_iter().map(|id| id.0).collect::<Vec<_>>(), [1]);
    }

    #[test]
    fn attribute_modes_hide_the_expected_columns() {
        let graph = fixture();
        let m = metrics();
        let height = |mode| DiagramLayout::entity_height(&graph, EntityId(0), mode, &m);
        // orders: id (PK), customer_id (FK), note.
        assert_eq!(height(AttributeMode::All), 28.0 + 3.0 * 22.0 + 6.0);
        assert_eq!(height(AttributeMode::KeysOnly), 28.0 + 2.0 * 22.0 + 6.0);
        assert_eq!(height(AttributeMode::PrimaryKeyOnly), 28.0 + 22.0);
        assert_eq!(height(AttributeMode::None), 28.0);
        assert_eq!(AttributeMode::ALL.len(), 4);
        // Relations anchor on the header when their rows are hidden.
        let layout = DiagramLayout::auto(&graph, AttributeMode::PrimaryKeyOnly, &m);
        let routes = layout.route(&graph, AttributeMode::PrimaryKeyOnly, &m);
        let customer = routes.iter().find(|r| graph.relations[r.relation].parent == EntityId(1)).unwrap();
        assert_eq!(customer.points[0].y, layout.rects[0].y + m.header_height / 2.0);
    }

    #[test]
    fn minimap_maps_content_and_viewport_both_ways() {
        let content = DiagramRect { x: 0.0, y: 0.0, width: 2000.0, height: 1000.0 };
        let viewport = DiagramRect { x: 1500.0, y: 800.0, width: 800.0, height: 600.0 };
        let map = Minimap::new(content, viewport, 180.0, 120.0, 6.0);
        for p in [DiagramPoint { x: 0.0, y: 0.0 }, DiagramPoint { x: 2300.0, y: 1400.0 }, DiagramPoint { x: 1000.0, y: 500.0 }] {
            let m = map.to_map(p);
            assert!(m.x >= 5.9 && m.x <= 174.1 && m.y >= 5.9 && m.y <= 114.1, "{m:?} inside the padded overview");
            let back = map.to_diagram(m);
            assert!((back.x - p.x).abs() < 0.01 && (back.y - p.y).abs() < 0.01);
        }
        // The union of content and viewport is what fits (width-bound here).
        assert!((map.scale() - 168.0 / 2300.0).abs() < 1e-6);
        // Viewport helpers.
        let mut v = Viewport { origin: DiagramPoint::default(), zoom: 2.0, min_zoom: 0.5 };
        v.center_on(DiagramPoint { x: 500.0, y: 300.0 }, 800.0, 400.0);
        assert_eq!(v.visible_rect(800.0, 400.0), DiagramRect { x: 300.0, y: 200.0, width: 400.0, height: 200.0 });
        assert_eq!(v.visible_rect(800.0, 400.0).center(), DiagramPoint { x: 500.0, y: 300.0 });
    }

    #[test]
    fn relation_picking_finds_the_nearest_route_within_tolerance() {
        let request = fixed_request();
        let routes = request.layout.route(&request.graph, AttributeMode::All, &request.metrics);
        let on_line = routes[1].points[1];
        assert_eq!(hit_route(&routes, on_line, 3.0), Some(routes[1].relation));
        let off = DiagramPoint { x: on_line.x + 40.0, y: on_line.y - 300.0 };
        assert_eq!(hit_route(&routes, off, 3.0), None);
    }

    #[test]
    fn join_sql_quotes_per_dialect_and_picks_the_join_kind_from_nullability() {
        let graph = fixture();
        // order_items.order_id is nullable: LEFT JOIN, child is the root.
        assert_eq!(
            join_sql::relation_join(&graph, 0, SqlDialect::POSTGRES).unwrap(),
            "SELECT *\nFROM \"public\".\"order_items\" AS t1\nLEFT JOIN \"public\".\"orders\" AS t2\n  ON t1.\"order_id\" = t2.\"id\""
        );
        assert_eq!(
            join_sql::relation_join(&graph, 1, SqlDialect::MYSQL).unwrap(),
            "SELECT *\nFROM `public`.`orders` AS t1\nINNER JOIN `public`.`customers` AS t2\n  ON t1.`customer_id` = t2.`id`"
        );
        // The focus: its parent (INNER) and its child (LEFT), one alias each.
        assert_eq!(
            join_sql::entity_join(&graph, EntityId(0), SqlDialect::SQLITE).unwrap(),
            "SELECT *\nFROM \"public\".\"orders\" AS t1\n\
             LEFT JOIN \"public\".\"order_items\" AS t2\n  ON t1.\"id\" = t2.\"order_id\"\n\
             INNER JOIN \"public\".\"customers\" AS t3\n  ON t1.\"customer_id\" = t3.\"id\""
        );
        assert_eq!(join_sql::relation_join(&graph, 9, SqlDialect::POSTGRES), None);
    }

    #[test]
    fn join_sql_handles_composite_self_and_unknown_columns() {
        let child = TableProperties {
            columns: vec![
                column("a", "int", true, false),
                column("b", "int", true, false),
                column("parent_id", "int", false, true),
            ],
            foreign_keys: Some(vec![
                fk("composite", "node", &["a", "b"], "other", &["x", "y"]),
                fk("self", "node", &["parent_id"], "node", &["a"]),
            ]),
            ..Default::default()
        };
        let other = TableProperties {
            columns: vec![column("x", "int", true, false), column("y", "int", true, false)],
            ..Default::default()
        };
        let graph = DiagramGraph::build(&table("node"), &child, &|_| ready(other.clone()));
        let sql = join_sql::entity_join(&graph, EntityId(0), SqlDialect::POSTGRES).unwrap();
        assert!(sql.contains("ON t1.\"a\" = t3.\"x\" AND t1.\"b\" = t3.\"y\""), "composite keys join on every column: {sql}");
        assert!(sql.contains("\"public\".\"node\" AS t2\n  ON t1.\"parent_id\" = t2.\"a\""), "self reference gets its own alias: {sql}");
        // A parent that is still loading has no known columns.
        let loading = DiagramGraph::build(&table("node"), &child, &|_| RelatedProperties::Loading);
        assert_eq!(join_sql::relation_join(&loading, 1, SqlDialect::POSTGRES), None);
    }

    fn temp_store() -> (LayoutStore, std::path::PathBuf) {
        let dir = std::env::temp_dir().join(format!("tablex-diagram-store-{}", uuid::Uuid::new_v4()));
        (LayoutStore::new(dir.clone()), dir)
    }

    fn key(table: Option<&str>) -> DiagramKey {
        DiagramKey { connection: "c1".into(), database: "db".into(), schema: Some("public".into()), table: table.map(Into::into) }
    }

    #[test]
    fn saved_positions_round_trip_and_ignore_stale_entries() {
        let (store, dir) = temp_store();
        let graph = fixture();
        let mut layout = DiagramLayout::auto(&graph, AttributeMode::All, &metrics());
        let auto = layout.clone();
        layout.move_entity(EntityId(1), 123.0, -45.0);
        let key = key(Some("orders"));
        assert_eq!(store.load(&key).unwrap(), None, "nothing saved yet");
        store.save(&key, &SavedDiagram::capture(&key, &graph, &layout)).unwrap();

        let saved = store.load(&key).unwrap().expect("saved");
        let mut fresh = auto.clone();
        assert_eq!(saved.apply(&graph, &mut fresh), 3);
        assert_eq!(fresh.rects[1].x, layout.rects[1].x);
        assert_eq!(fresh.rects[1].y, layout.rects[1].y);

        // A table that vanished is ignored; a new one keeps its auto position.
        let mut smaller = DiagramGraph::build(&table("orders"), &TableProperties { columns: vec![column("id", "integer", true, false)], ..Default::default() }, &|_| ready(simple(&[])));
        smaller.entities.truncate(1);
        let mut small_layout = DiagramLayout::auto(&smaller, AttributeMode::All, &metrics());
        assert_eq!(saved.apply(&smaller, &mut small_layout), 1);
        let mut other_table = saved.clone();
        other_table.entities.retain(|e| e.table == "customers");
        let mut untouched = auto.clone();
        assert_eq!(other_table.apply(&graph, &mut untouched), 1);
        assert_eq!(untouched.rects[0], auto.rects[0]);

        // Non-finite coordinates from a hand edit are ignored.
        let mut broken = saved.clone();
        broken.entities[0].x = f32::NAN;
        let mut layout = auto.clone();
        assert_eq!(broken.apply(&graph, &mut layout), 2);
        assert_eq!(layout.rects[0], auto.rects[0]);

        // Re-layout forgets; forgetting twice is fine.
        store.forget(&key).unwrap();
        store.forget(&key).unwrap();
        assert_eq!(store.load(&key).unwrap(), None);
        std::fs::remove_dir_all(dir).ok();
    }

    #[test]
    fn each_diagram_has_its_own_file_and_unknown_fields_survive_a_rewrite() {
        let (store, dir) = temp_store();
        let graph = fixture();
        let layout = DiagramLayout::auto(&graph, AttributeMode::All, &metrics());
        let (table_key, schema_key) = (key(Some("orders")), key(None));
        assert_ne!(store.path(&table_key), store.path(&schema_key));
        let mut other_connection = table_key.clone();
        other_connection.connection = "c2".into();
        assert_ne!(store.path(&table_key), store.path(&other_connection));

        // A later build wrote extra fields (top level and per entity).
        std::fs::create_dir_all(&dir).unwrap();
        let future = serde_json::json!({
            "version": 1,
            "key": {"connection": "c1", "database": "db", "schema": "public", "table": "orders"},
            "entities": [{"schema": "public", "table": "orders", "x": 5.0, "y": 6.0, "shape": "round"}],
            "tags": [{"text": "hello"}],
        });
        std::fs::write(store.path(&table_key), future.to_string()).unwrap();
        let loaded = store.load(&table_key).unwrap().unwrap();
        assert_eq!(loaded.entities[0].extra["shape"], "round");
        let rewritten = loaded.clone().with_positions(&graph, &layout);
        store.save(&table_key, &rewritten).unwrap();
        let text: serde_json::Value = serde_json::from_slice(&std::fs::read(store.path(&table_key)).unwrap()).unwrap();
        assert_eq!(text["tags"][0]["text"], "hello", "top-level extras are kept");
        assert_eq!(text["entities"][0]["shape"], "round", "per-entity extras are kept");
        assert_eq!(text["entities"].as_array().unwrap().len(), 3);
        // Fields missing from an old file default.
        std::fs::write(store.path(&schema_key), r#"{"key": {"connection":"c1","database":"db","schema":"public"}}"#).unwrap();
        assert_eq!(store.load(&schema_key).unwrap().unwrap().entities, vec![]);
        std::fs::remove_dir_all(dir).ok();
    }

    #[test]
    fn newer_corrupt_and_foreign_files_are_never_applied_or_clobbered() {
        let (store, dir) = temp_store();
        let key = key(Some("orders"));
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(store.path(&key), r#"{"version": 99, "key": {}, "entities": []}"#).unwrap();
        assert!(matches!(store.load(&key), Err(StoreError::Newer(99))));
        let graph = fixture();
        let layout = DiagramLayout::auto(&graph, AttributeMode::All, &metrics());
        assert!(matches!(
            store.save(&key, &SavedDiagram::capture(&key, &graph, &layout)),
            Err(StoreError::Newer(99))
        ));
        assert!(std::fs::read_to_string(store.path(&key)).unwrap().contains("99"), "left untouched");

        std::fs::write(store.path(&key), "{not json").unwrap();
        assert!(matches!(store.load(&key), Err(StoreError::Corrupt(_))));
        // A corrupt file is replaced by the next save.
        store.save(&key, &SavedDiagram::capture(&key, &graph, &layout)).unwrap();
        assert!(store.load(&key).unwrap().is_some());

        // A file whose stored key differs (hash collision, copied file).
        let mut foreign = SavedDiagram::capture(&key, &graph, &layout);
        foreign.key.connection = "someone-else".into();
        std::fs::write(store.path(&key), serde_json::to_vec(&foreign).unwrap()).unwrap();
        assert_eq!(store.load(&key).unwrap(), None);
        std::fs::remove_dir_all(dir).ok();
    }
}

mod d2;
