//! Plans captured from the real test containers (see the header of each fixture).

mod common;
use common::*;
use engine_plan::{ExplainDialect as D, ExplainMode as M, Plan};

fn approx(a: Option<f64>, b: f64) {
    let a = a.expect("value present");
    assert!((a - b).abs() < 1e-6 * b.abs().max(1.0), "{a} != {b}");
}

fn find<'a>(plan: &'a Plan, op: &str, object: Option<&str>) -> &'a engine_plan::PlanNode {
    plan.nodes
        .iter()
        .find(|n| n.operation == op && (object.is_none() || n.object.as_deref() == object))
        .unwrap_or_else(|| panic!("no {op} {object:?} in {}", dump(plan)))
}

/// (dialect, directory, mode inferred from the file name) -> every fixture file.
fn all_fixtures() -> Vec<(D, M, String)> {
    let mut out = Vec::new();
    for (d, dir) in [
        (D::Postgres, "postgres"),
        (D::MySql, "mysql"),
        (D::MariaDb, "mariadb"),
        (D::Sqlite, "sqlite"),
        (D::ClickHouse, "clickhouse"),
    ] {
        let path = format!("{}/tests/fixtures/{dir}", env!("CARGO_MANIFEST_DIR"));
        for entry in std::fs::read_dir(path).unwrap() {
            let name = entry.unwrap().file_name().into_string().unwrap();
            let mode = if name.contains("analyze") { M::Analyze } else { M::Estimate };
            out.push((d, mode, format!("{dir}/{name}")));
        }
    }
    out.sort_by(|a, b| a.2.cmp(&b.2));
    out
}

#[test]
fn every_fixture_has_a_header_and_parses() {
    let fixtures = all_fixtures();
    assert_eq!(fixtures.len(), 38);
    for (d, m, path) in fixtures {
        let full = format!("{}/tests/fixtures/{path}", env!("CARGO_MANIFEST_DIR"));
        let text = std::fs::read_to_string(full).unwrap();
        assert!(text.starts_with("-- server: "), "{path}: header");
        assert!(text.contains("\n-- statement: "), "{path}: statement header");
        let plan = parse_fixture(d, m, &path);
        assert!(!plan.nodes.is_empty(), "{path}");
        assert_eq!(plan.nodes[0].parent, None);
        assert_eq!(plan.nodes[0].depth, 0);
        // Pre-order: a parent precedes its children and sits one level up.
        for (i, n) in plan.nodes.iter().enumerate().skip(1) {
            match n.parent {
                Some(p) => {
                    assert!(p < i, "{path}");
                    assert_eq!(plan.nodes[p].depth + 1, n.depth, "{path}");
                }
                // Only SQLite plans have several top-level steps.
                None => assert!(d == D::Sqlite && n.depth == 0, "{path}: node {i}"),
            }
        }
        if m == M::Analyze {
            assert!(plan.nodes.iter().any(|n| n.actual_rows.is_some()), "{path}: actuals");
        }
    }
}

#[test]
fn node_counts_and_roots() {
    let expected = [
        ("postgres/join_estimate.json", M::Estimate, D::Postgres, 6, "Sort"),
        ("postgres/join_analyze.json", M::Analyze, D::Postgres, 6, "Sort"),
        ("postgres/cte_analyze.json", M::Analyze, D::Postgres, 10, "Limit"),
        ("postgres/union_analyze.json", M::Analyze, D::Postgres, 4, "HashAggregate"),
        ("postgres/index_analyze.json", M::Analyze, D::Postgres, 2, "Bitmap Heap Scan"),
        ("postgres/nested_analyze.json", M::Analyze, D::Postgres, 4, "Nested Loop"),
        ("mysql/join_estimate.json", M::Estimate, D::MySql, 6, "Query block"),
        ("mysql/cte_estimate.json", M::Estimate, D::MySql, 11, "Query block"),
        ("mysql/union_estimate.json", M::Estimate, D::MySql, 6, "Query block"),
        ("mysql/join_analyze.txt", M::Analyze, D::MySql, 7, "Sort"),
        ("mysql/cte_analyze.txt", M::Analyze, D::MySql, 12, "Limit"),
        ("mysql/union_analyze.txt", M::Analyze, D::MySql, 6, "Table scan"),
        ("mysql/index_analyze.txt", M::Analyze, D::MySql, 1, "Index lookup"),
        ("mariadb/join_estimate.json", M::Estimate, D::MariaDb, 6, "Query block"),
        ("mariadb/join_analyze.json", M::Analyze, D::MariaDb, 6, "Query block"),
        ("mariadb/cte_analyze.json", M::Analyze, D::MariaDb, 15, "Query block"),
        ("mariadb/union_analyze.json", M::Analyze, D::MariaDb, 8, "Query block"),
        ("sqlite/join_estimate.tsv", M::Estimate, D::Sqlite, 4, "SCAN"),
        ("sqlite/cte_estimate.tsv", M::Estimate, D::Sqlite, 7, "CO-ROUTINE big"),
        ("sqlite/union_estimate.tsv", M::Estimate, D::Sqlite, 5, "COMPOUND QUERY"),
        ("sqlite/index_estimate.tsv", M::Estimate, D::Sqlite, 1, "SEARCH"),
        ("clickhouse/join_estimate.json", M::Estimate, D::ClickHouse, 13, "Expression"),
        ("clickhouse/union_estimate.json", M::Estimate, D::ClickHouse, 8, "Distinct"),
        ("clickhouse/index_estimate.json", M::Estimate, D::ClickHouse, 3, "Expression"),
    ];
    for (path, mode, dialect, count, root) in expected {
        let plan = parse_fixture(dialect, mode, path);
        assert_eq!(plan.nodes.len(), count, "{path}");
        assert_eq!(plan.nodes[0].operation, root, "{path}");
    }
}

#[test]
fn postgres_analyze_values() {
    let p = parse_fixture(D::Postgres, M::Analyze, "postgres/join_analyze.json");
    approx(p.total_cost, 564.46);
    approx(p.planning_ms, 0.724);
    approx(p.execution_ms, 6.068);
    approx(p.total_time_ms, 6.792);
    let scan = find(&p, "Seq Scan", Some("plan_t2"));
    approx(scan.total_cost, 309.0);
    approx(scan.est_rows, 20000.0);
    approx(scan.actual_rows, 20000.0);
    approx(scan.actual_time_ms, 1.563);
    assert_eq!(scan.loops, Some(1));
    assert_eq!(scan.depth, 3);
    // Self cost: total minus children, floored at zero.
    approx(p.nodes[0].self_cost, 564.46 - 562.92);
    approx(find(&p, "Hash Join", None).self_cost, 487.29 - 309.0 - 94.5);
    let agg = find(&p, "HashAggregate", None);
    assert!(agg.props.iter().any(|(k, v)| k == "Group Key" && v == "t1.grp"));
    // Zero buffer counters are dropped, non-zero kept.
    assert!(!p.nodes[0].props.iter().any(|(k, _)| k == "Shared Read Blocks"));
    assert!(p.nodes[0].props.iter().any(|(k, v)| k == "Shared Hit Blocks" && v == "144"));
    assert!(p.nodes[0].props.iter().any(|(k, v)| k == "Planning Shared Hit Blocks" && v == "144"));
    assert!(p.warnings.is_empty(), "{:?}", p.warnings);
}

#[test]
fn postgres_loops_semantics() {
    let p = parse_fixture(D::Postgres, M::Analyze, "postgres/nested_analyze.json");
    let heap = find(&p, "Bitmap Heap Scan", Some("plan_t2"));
    assert_eq!(heap.loops, Some(19));
    // Rows are per loop, time is total over loops (0.007 ms x 19 loops).
    approx(heap.actual_rows, 4.0);
    approx(heap.actual_time_ms, 0.007 * 19.0);
    assert_eq!(heap.depth, 1);
    let index = find(&p, "Bitmap Index Scan", None);
    assert_eq!(index.detail.as_deref(), Some("using plan_t2_t1_idx; (t1_id = t1.id)"));
}

#[test]
fn postgres_estimate_has_no_actuals() {
    let p = parse_fixture(D::Postgres, M::Estimate, "postgres/join_estimate.json");
    assert!(p.nodes.iter().all(|n| n.actual_rows.is_none() && n.actual_time_ms.is_none() && n.loops.is_none()));
    assert_eq!(p.total_time_ms, None);
    assert!(p.warnings.is_empty());
    approx(p.total_cost, 564.46);
}

#[test]
fn postgres_limit_suppresses_the_overestimate_below_it() {
    // Sort estimated 1,667 rows but only 20 were pulled: that is the LIMIT, not a bad estimate.
    let p = parse_fixture(D::Postgres, M::Analyze, "postgres/cte_analyze.json");
    assert!(p.warnings.is_empty(), "{:?}", p.warnings);
    assert!(p.nodes.iter().all(|n| n.misestimate.is_none()));
    let sort = find(&p, "Sort", None);
    assert_eq!((sort.est_rows, sort.actual_rows), (Some(1667.0), Some(20.0)));
}

#[test]
fn postgres_cte_and_subplans() {
    let p = parse_fixture(D::Postgres, M::Estimate, "postgres/cte_estimate.json");
    let sub = find(&p, "Subquery Scan", None);
    assert_eq!(p.nodes[sub.parent.unwrap()].operation, "Hash");
    // The InitPlan hangs under the Limit next to the Sort.
    let init = p.nodes.iter().find(|n| n.props.iter().any(|(k, v)| k == "Subplan Name" && v.starts_with("InitPlan"))).unwrap();
    assert_eq!(p.nodes[init.parent.unwrap()].operation, "Limit");
}

#[test]
fn mysql_json_estimate() {
    let p = parse_fixture(D::MySql, M::Estimate, "mysql/join_estimate.json");
    approx(p.total_cost, 2795.2);
    assert_eq!(p.nodes[0].operation, "Query block");
    assert_eq!(p.nodes[0].detail.as_deref(), Some("select #1"));
    let t2 = find(&p, "Index lookup", Some("t2"));
    assert_eq!(t2.detail.as_deref(), Some("using plan_t2_t1_idx"));
    // Table cost is read_cost + eval_cost, and is its own cost.
    assert_eq!(t2.self_cost, t2.total_cost);
    assert!(t2.props.iter().any(|(k, _)| k == "cost_info.prefix_cost"));
    assert!(t2.props.iter().any(|(k, v)| k == "possible_keys" && v == "plan_t2_t1_idx"));
    let scan = find(&p, "Full table scan", Some("t1"));
    approx(scan.est_rows, 5000.0);
    assert!(scan.actual_rows.is_none());
}

#[test]
fn mysql_json_subqueries_and_materialization() {
    let p = parse_fixture(D::MySql, M::Estimate, "mysql/cte_estimate.json");
    let blocks: Vec<_> = p.nodes.iter().filter(|n| n.operation == "Query block").collect();
    assert_eq!(blocks.len(), 3);
    let mat = find(&p, "Materialize", None);
    assert_eq!(p.nodes[mat.parent.unwrap()].object.as_deref(), Some("big"));
    let sub = p.nodes.iter().find(|n| n.detail.as_deref() == Some("select #3")).unwrap();
    assert!(sub.props.iter().any(|(k, v)| k == "dependent" && v == "false"));
    approx(sub.total_cost, 1988.55);
    assert_eq!(find(&p, "Order by", None).props, [("using_filesort".to_string(), "true".to_string())]);
}

#[test]
fn mysql_json_union() {
    let p = parse_fixture(D::MySql, M::Estimate, "mysql/union_estimate.json");
    let union = find(&p, "Union", None);
    assert_eq!(union.object.as_deref(), Some("<union1,2>"));
    let children: Vec<_> = p.nodes.iter().filter(|n| n.parent == Some(1)).collect();
    assert_eq!(children.len(), 2);
}

#[test]
fn mysql_tree_values_and_loops() {
    let p = parse_fixture(D::MySql, M::Analyze, "mysql/join_analyze.txt");
    assert_eq!(p.nodes[0].operation, "Sort");
    assert_eq!(p.nodes[0].detail.as_deref(), Some("total DESC"));
    approx(p.nodes[0].actual_time_ms, 14.0);
    approx(p.execution_ms, 14.0);
    let lookup = find(&p, "Index lookup", Some("t2"));
    assert_eq!(lookup.detail.as_deref(), Some("using plan_t2_t1_idx (t1_id=t1.id)"));
    approx(lookup.total_cost, 0.982);
    approx(lookup.est_rows, 3.93);
    approx(lookup.actual_rows, 4.0);
    assert_eq!(lookup.loops, Some(2500));
    // 0.00205 ms per loop x 2500 loops.
    approx(lookup.actual_time_ms, 0.00205 * 2500.0);
    assert_eq!(lookup.depth, 4);
    let join = find(&p, "Nested loop inner join", None);
    approx(join.startup_cost.or(Some(0.0)), 0.0);
    approx(join.total_cost, 2795.0);
    // 2795 - 504 (filter) - 0.982 (lookup)
    approx(join.self_cost, 2795.0 - 504.0 - 0.982);
}

#[test]
fn mysql_tree_limit_suppresses_the_overestimates_below_it() {
    let p = parse_fixture(D::MySql, M::Analyze, "mysql/cte_analyze.txt");
    assert!(p.warnings.is_empty(), "{:?}", p.warnings);
    assert!(p.nodes.iter().all(|n| n.misestimate.is_none()));
}

#[test]
fn mysql_tree_cost_ranges() {
    let p = parse_fixture(D::MySql, M::Analyze, "mysql/cte_analyze.txt");
    let sort = find(&p, "Sort", None);
    approx(sort.startup_cost, 11662.0);
    approx(sort.total_cost, 11662.0);
    let filter = find(&p, "Filter", None);
    approx(filter.startup_cost, 4453.0);
    approx(filter.total_cost, 5018.0);
    let sub = find(&p, "Select #3 (subquery in condition; run only once)", None);
    assert!(sub.total_cost.is_none());
    assert_eq!(p.nodes[sub.parent.unwrap()].operation, "Filter");
}

#[test]
fn mysql_tree_misestimates_are_reported_once_each() {
    let p = parse_fixture(D::MySql, M::Analyze, "mysql/union_analyze.txt");
    assert_eq!(p.warnings.len(), 3);
    assert!(p.warnings[0].contains("rows estimated 2,166, actual 150"));
    // The flag on the node matches the warning: estimated more than got, by 14.4x.
    let m = p.nodes[0].misestimate.unwrap();
    assert!(!m.under);
    assert!((m.factor - 2166.0 / 150.0).abs() < 1e-9);
    assert_eq!(p.nodes.iter().filter(|n| n.misestimate.is_some()).count(), 3);
}

#[test]
fn mariadb_json_analyze() {
    let p = parse_fixture(D::MariaDb, M::Analyze, "mariadb/join_analyze.json");
    approx(p.total_cost, 12.929134);
    assert!(p.planning_ms.is_some());
    assert_eq!(p.execution_ms, p.nodes[0].actual_time_ms);
    let t2 = find(&p, "Index lookup", Some("t2"));
    assert_eq!(t2.loops, Some(2500));
    assert_eq!(t2.est_rows, Some(4.0));
    assert!(t2.actual_rows.is_some());
    // r_table_time_ms + r_other_time_ms (already totals).
    assert!(t2.actual_time_ms.unwrap() > 0.0);
    assert_eq!(t2.self_cost, t2.total_cost);
    assert!(find(&p, "Filesort", None).detail.as_deref().unwrap().contains("sum(t2.amount)"));
    assert!(t2.props.iter().any(|(k, _)| k == "r_filtered"));
}

#[test]
fn mariadb_subqueries_and_derived_tables() {
    let p = parse_fixture(D::MariaDb, M::Analyze, "mariadb/cte_analyze.json");
    let derived = find(&p, "Full table scan", Some("<derived2>"));
    assert!(p.nodes.iter().any(|n| n.parent == Some(p.nodes.iter().position(|x| std::ptr::eq(x, derived)).unwrap()) && n.operation == "Materialize"));
    let blocks: Vec<_> = p.nodes.iter().filter(|n| n.operation == "Query block").collect();
    assert_eq!(blocks.len(), 3);
    assert_eq!(find(&p, "Read sorted file", None).actual_rows, Some(20.0));
}

#[test]
fn mariadb_estimate_has_no_actuals() {
    let p = parse_fixture(D::MariaDb, M::Estimate, "mariadb/join_estimate.json");
    assert!(p.nodes.iter().all(|n| n.actual_rows.is_none()));
    assert_eq!(p.total_time_ms, None);
    approx(p.total_cost, 12.929134);
}

#[test]
fn sqlite_rows() {
    let p = parse_fixture(D::Sqlite, M::Estimate, "sqlite/cte_estimate.tsv");
    let scan = &p.nodes[1];
    assert_eq!((scan.operation.as_str(), scan.object.as_deref()), ("SCAN", Some("plan_t2")));
    assert_eq!(scan.detail.as_deref(), Some("USING INDEX plan_t2_t1_idx"));
    assert_eq!(scan.parent, Some(0));
    assert_eq!(scan.depth, 1);
    let sub = find(&p, "SCALAR SUBQUERY 2", None);
    assert_eq!(sub.parent, None);
    assert_eq!(p.nodes.iter().filter(|n| n.parent.is_none()).count(), 5);
    assert!(p.raw.starts_with("2\t0\t0\tCO-ROUTINE big\n"));
    assert!(p.nodes.iter().all(|n| n.total_cost.is_none()));
}

#[test]
fn clickhouse_plan() {
    let p = parse_fixture(D::ClickHouse, M::Estimate, "clickhouse/index_estimate.json");
    let read = &p.nodes[2];
    assert_eq!(read.operation, "ReadFromMergeTree");
    assert_eq!(read.object.as_deref(), Some("tablex.plan_t2"));
    assert_eq!(read.depth, 2);
    let idx = read.props.iter().find(|(k, _)| k == "Indexes").unwrap();
    assert_eq!(idx.1, "PrimaryKey(t1_id) [(t1_id in [42, 42])]: parts 1/1, granules 1/2");
    assert!(read.props.iter().any(|(k, v)| k == "Parts" && v == "1"));
    assert_eq!(p.nodes[0].detail.as_deref(), Some("(Project names + Projection)"));
    assert!(p.nodes[0].props.iter().any(|(k, _)| k == "Expression"));
    // No costs from ClickHouse.
    assert!(p.nodes.iter().all(|n| n.total_cost.is_none() && n.self_cost.is_none()));
}

#[test]
fn clickhouse_join_has_two_reads() {
    let p = parse_fixture(D::ClickHouse, M::Estimate, "clickhouse/join_estimate.json");
    let reads: Vec<_> = p.nodes.iter().filter(|n| n.operation == "ReadFromMergeTree").map(|n| n.object.clone().unwrap()).collect();
    assert_eq!(reads, ["tablex.plan_t2", "tablex.plan_t1"]);
    let join = find(&p, "Join", None);
    assert!(join.props.iter().any(|(k, v)| k == "Type" && v == "INNER"));
}

#[test]
fn clickhouse_props_are_capped() {
    for (d, m, path) in all_fixtures().into_iter().filter(|f| f.0 == D::ClickHouse) {
        for n in parse_fixture(d, m, &path).nodes {
            for (k, v) in n.props {
                assert!(v.chars().count() <= 4097, "{path}: {k}");
            }
        }
    }
}

#[test]
fn raw_is_the_original_text() {
    let payload = fixture_payload("postgres/join_estimate.json");
    let p = parse_fixture(D::Postgres, M::Estimate, "postgres/join_estimate.json");
    assert_eq!(p.raw, payload);
}
