//! Hand-written outputs: server quirks the fixtures do not show, and hostile input.

mod common;
use common::*;
use engine_plan::{ExplainDialect as D, ExplainError as E, ExplainMode as M, Plan, PlanRows, MAX_NODES, MAX_RAW_BYTES};

fn parse(d: D, m: M, text: &str) -> Result<Plan, E> {
    Plan::parse(d, m, &cell(text))
}

fn pg(plan_json: &str) -> String {
    format!(r#"[{{"Plan": {plan_json}}}]"#)
}

fn sqlite(rows: &[(&str, &str, &str)]) -> PlanRows {
    PlanRows {
        columns: ["id", "parent", "notused", "detail"].map(String::from).to_vec(),
        rows: rows.iter().map(|(i, p, d)| vec![Some(i.to_string()), Some(p.to_string()), Some("0".into()), Some(d.to_string())]).collect(),
    }
}

// ---- PostgreSQL --------------------------------------------------------------------------------

#[test]
fn pg_minimal_and_object_top_level() {
    let p = parse(D::Postgres, M::Estimate, &pg(r#"{"Node Type": "Result", "Total Cost": 0.01, "Plan Rows": 1}"#)).unwrap();
    assert_eq!(p.nodes.len(), 1);
    assert_eq!(p.nodes[0].operation, "Result");
    assert_eq!(p.total_cost, Some(0.01));
    let p = parse(D::Postgres, M::Estimate, r#"{"Plan": {"Node Type": "Result"}}"#).unwrap();
    assert_eq!(p.nodes.len(), 1);
}

#[test]
fn pg_never_executed_node_has_zero_loops_and_no_warning() {
    let json = pg(r#"{"Node Type": "Nested Loop", "Total Cost": 10, "Plan Rows": 5, "Actual Rows": 0, "Actual Loops": 1, "Actual Total Time": 0.1,
        "Plans": [{"Node Type": "Seq Scan", "Relation Name": "t", "Total Cost": 5, "Plan Rows": 5000, "Actual Rows": 0, "Actual Loops": 0, "Actual Total Time": 0}]}"#);
    let p = parse(D::Postgres, M::Analyze, &json).unwrap();
    assert_eq!(p.nodes[1].loops, Some(0));
    assert_eq!(p.nodes[1].actual_time_ms, Some(0.0));
    assert!(p.warnings.is_empty(), "{:?}", p.warnings);
}

#[test]
fn pg_misestimate_threshold_is_more_than_10x_both_ways() {
    let node = |est: u32, act: u32| {
        pg(&format!(r#"{{"Node Type": "Seq Scan", "Relation Name": "t", "Plan Rows": {est}, "Actual Rows": {act}, "Actual Loops": 1}}"#))
    };
    let warn = |est, act| parse(D::Postgres, M::Analyze, &node(est, act)).unwrap().warnings;
    assert!(warn(10, 100).is_empty());
    assert!(warn(100, 10).is_empty());
    assert_eq!(warn(10, 12000), ["Seq Scan on t: rows estimated 10, actual 12,000"]);
    assert_eq!(warn(12000, 10), ["Seq Scan on t: rows estimated 12,000, actual 10"]);
    // Tiny numbers are floored at 1: 0 vs 10 is not a misestimate, 0 vs 11 is.
    assert!(warn(0, 10).is_empty());
    assert_eq!(warn(0, 11).len(), 1);
}

#[test]
fn pg_misestimate_mentions_loops_and_is_capped() {
    let child = r#"{"Node Type": "Index Scan", "Relation Name": "t", "Plan Rows": 1, "Actual Rows": 500, "Actual Loops": 3}"#;
    let kids = [child; 8].join(",");
    let json = pg(&format!(r#"{{"Node Type": "Append", "Plans": [{kids}]}}"#));
    let p = parse(D::Postgres, M::Analyze, &json).unwrap();
    assert_eq!(p.warnings.len(), 6);
    assert_eq!(p.warnings[0], "Index Scan on t: rows estimated 1, actual 500 per loop");
    assert_eq!(p.warnings[5], "and 3 more nodes with row estimates off by more than 10x");
}

#[test]
fn pg_spill_warnings() {
    let json = pg(r#"{"Node Type": "Sort", "Actual Rows": 1, "Sort Method": "external merge", "Sort Space Used": 9000,
        "Plans": [{"Node Type": "Aggregate", "Strategy": "Hashed", "Disk Usage": 128}]}"#);
    let p = parse(D::Postgres, M::Analyze, &json).unwrap();
    assert_eq!(p.warnings, ["Sort: sort spilled to disk", "HashAggregate: hash spilled to disk"]);
}

#[test]
fn pg_aggregate_strategies_and_object_names() {
    let json = pg(r#"{"Node Type": "Aggregate", "Strategy": "Sorted", "Plans": [
        {"Node Type": "Seq Scan", "Schema": "public", "Relation Name": "t", "Alias": "x"},
        {"Node Type": "CTE Scan", "CTE Name": "c", "Alias": "c"},
        {"Node Type": "Function Scan", "Function Name": "generate_series", "Alias": "g"}]}"#);
    let p = parse(D::Postgres, M::Estimate, &json).unwrap();
    assert_eq!(p.nodes[0].operation, "GroupAggregate");
    assert_eq!(p.nodes[1].object.as_deref(), Some("public.t AS x"));
    assert_eq!(p.nodes[2].object.as_deref(), Some("c"));
    assert_eq!(p.nodes[3].object.as_deref(), Some("generate_series"));
}

#[test]
fn pg_top_level_extras_land_in_root_props() {
    let json = r#"[{"Plan": {"Node Type": "Result"}, "Triggers": [{"Trigger Name": "tg", "Calls": 1}], "JIT": {"Functions": 3}, "Planning Time": 1.5, "Execution Time": 2.5}]"#;
    let p = parse(D::Postgres, M::Analyze, json).unwrap();
    assert!(p.nodes[0].props.iter().any(|(k, v)| k == "Triggers" && v.contains("tg")));
    assert!(p.nodes[0].props.iter().any(|(k, v)| k == "JIT" && v == r#"{"Functions":3}"#));
    assert_eq!(p.total_time_ms, Some(4.0));
}

#[test]
fn pg_second_plan_in_the_array_is_noted() {
    let json = r#"[{"Plan": {"Node Type": "A"}}, {"Plan": {"Node Type": "B"}}]"#;
    let p = parse(D::Postgres, M::Estimate, json).unwrap();
    assert_eq!(p.nodes.len(), 1);
    assert_eq!(p.warnings, ["the output holds 2 plans, only the first is shown"]);
}

#[test]
fn pg_wrong_types_never_panic() {
    for json in [
        r#"[{"Plan": 5}]"#,
        r#"[{"Plan": []}]"#,
        r#"[5]"#,
        r#"[]"#,
        r#"5"#,
        r#""text""#,
        r#"null"#,
        r#"[{"Plan": {"Node Type": 7, "Plans": "x", "Total Cost": "oops", "Plan Rows": {"a": 1}}}]"#,
        r#"[{"Plan": {"Node Type": "A", "Plans": [1, null, "x", []]}}]"#,
    ] {
        let _ = parse(D::Postgres, M::Analyze, json);
    }
    assert!(matches!(parse(D::Postgres, M::Estimate, r#"[{"Plan": 5}]"#), Err(E::UnexpectedShape(_))));
    assert!(matches!(parse(D::Postgres, M::Estimate, "[]"), Err(E::UnexpectedShape(_))));
    // Odd node fields degrade to missing values.
    let p = parse(D::Postgres, M::Estimate, r#"[{"Plan": {"Node Type": 7, "Total Cost": "oops", "Plans": "x"}}]"#).unwrap();
    assert_eq!(p.nodes[0].operation, "Unknown");
    assert_eq!(p.nodes[0].total_cost, None);
}

#[test]
fn pg_huge_and_odd_numbers() {
    // 1e999 does not fit an f64: the document is rejected, not turned into infinity.
    assert!(matches!(parse(D::Postgres, M::Estimate, r#"[{"Plan": {"Node Type": "A", "Total Cost": 1e999}}]"#), Err(E::InvalidJson(_))));
    let json = pg(r#"{"Node Type": "A", "Total Cost": 1.7e308, "Plan Rows": 1e300, "Actual Rows": 1e300, "Actual Loops": 1e30, "Actual Total Time": 1e300}"#);
    let p = parse(D::Postgres, M::Analyze, &json).unwrap();
    assert_eq!(p.nodes[0].loops, Some(u64::MAX));
    // time x loops overflows to infinity: kept as is, no panic and no NaN.
    assert!(p.nodes[0].actual_time_ms.is_some());
    let json = pg(r#"{"Node Type": "A", "Actual Loops": -4, "Plan Rows": 18446744073709551615, "Actual Rows": 3}"#);
    let p = parse(D::Postgres, M::Analyze, &json).unwrap();
    assert_eq!(p.nodes[0].loops, None);
    assert_eq!(p.warnings.len(), 1);
}

#[test]
fn pg_truncated_json() {
    let text = fixture_payload("postgres/join_analyze.json");
    for cut in [0, 1, 10, text.len() / 2, text.len() - 5] {
        let r = parse(D::Postgres, M::Analyze, &text[..cut]);
        assert!(r.is_err(), "cut {cut}");
    }
    assert!(matches!(parse(D::Postgres, M::Analyze, "[{\"Plan\": {\"Node Type\": \"A\""), Err(E::InvalidJson(_))));
}

#[test]
fn self_cost_is_total_minus_children_floored_at_zero() {
    let json = pg(r#"{"Node Type": "A", "Total Cost": 100, "Plans": [
        {"Node Type": "B", "Total Cost": 30, "Plans": [{"Node Type": "D", "Total Cost": 12.5}]},
        {"Node Type": "C", "Total Cost": 20}]}"#);
    let p = parse(D::Postgres, M::Estimate, &json).unwrap();
    let selfs: Vec<_> = p.nodes.iter().map(|n| n.self_cost).collect();
    assert_eq!(selfs, [Some(50.0), Some(17.5), Some(12.5), Some(20.0)]);
    // Children costing more than the parent (nested-loop inner side): floored.
    let json = pg(r#"{"Node Type": "NL", "Total Cost": 10, "Plans": [{"Node Type": "S", "Total Cost": 8}, {"Node Type": "I", "Total Cost": 9}]}"#);
    let p = parse(D::Postgres, M::Estimate, &json).unwrap();
    assert_eq!(p.nodes[0].self_cost, Some(0.0));
    // A node without cost has no self cost; its children are still fine.
    let json = pg(r#"{"Node Type": "NL", "Plans": [{"Node Type": "S", "Total Cost": 8}]}"#);
    let p = parse(D::Postgres, M::Estimate, &json).unwrap();
    assert_eq!(p.nodes[0].self_cost, None);
    assert_eq!(p.nodes[1].self_cost, Some(8.0));
}

#[test]
fn analyze_without_actuals_is_flagged() {
    let p = parse(D::Postgres, M::Analyze, &pg(r#"{"Node Type": "Result"}"#)).unwrap();
    assert_eq!(p.warnings, ["analyze was requested but the output has no actual statistics"]);
    let p = parse(D::Postgres, M::Estimate, &pg(r#"{"Node Type": "Result"}"#)).unwrap();
    assert!(p.warnings.is_empty());
}

// ---- MySQL tree text -----------------------------------------------------------------------------

#[test]
fn mysql_tree_never_executed_and_single_time() {
    let text = "-> Nested loop inner join  (cost=10 rows=5) (actual time=0.5 rows=0 loops=1)\n    -> Table scan on t  (cost=1 rows=1000) (never executed)\n";
    let p = parse(D::MySql, M::Analyze, text).unwrap();
    assert_eq!(p.nodes[0].actual_time_ms, Some(0.5));
    assert_eq!(p.nodes[1].loops, Some(0));
    assert_eq!(p.nodes[1].actual_rows, None);
    assert_eq!(p.nodes[1].object.as_deref(), Some("t"));
    assert!(p.warnings.is_empty());
}

#[test]
fn mysql_tree_irregular_indent_wrapped_lines_and_estimate_text() {
    let text = "-> Sort: a, b  (cost=3 rows=2)\n  -> Filter: (x > 1)  (cost=2 rows=2)\n        -> Table scan on t  (cost=1 rows=2)\n  -> Index lookup on u using k (a=t.a)\n        and more text\n";
    let p = parse(D::MySql, M::Estimate, text).unwrap();
    let shape: Vec<_> = p.nodes.iter().map(|n| (n.operation.as_str(), n.depth)).collect();
    assert_eq!(shape, [("Sort", 0), ("Filter", 1), ("Table scan", 2), ("Index lookup", 1)]);
    assert_eq!(p.nodes[3].detail.as_deref(), Some("using k (a=t.a) and more text"));
    assert_eq!(p.nodes[3].parent, Some(0));
}

#[test]
fn mysql_tree_label_with_parentheses_and_cost_like_text() {
    let text = "-> Filter: (t.a = 'x (cost=9 rows=9)')  (cost=2 rows=2) (actual time=0.1..0.2 rows=2 loops=4)\n";
    let p = parse(D::MySql, M::Analyze, text).unwrap();
    assert_eq!(p.nodes[0].detail.as_deref(), Some("(t.a = 'x (cost=9 rows=9)')"));
    assert_eq!(p.nodes[0].total_cost, Some(2.0));
    assert_eq!(p.nodes[0].loops, Some(4));
    assert!((p.nodes[0].actual_time_ms.unwrap() - 0.8).abs() < 1e-9);
}

#[test]
fn mysql_tree_garbage_is_an_error_not_a_panic() {
    assert!(matches!(parse(D::MySql, M::Analyze, "this is not a plan"), Err(E::UnexpectedShape(_))));
    assert!(matches!(parse(D::MySql, M::Analyze, "   \n  "), Err(E::EmptyOutput)));
    let p = parse(D::MySql, M::Analyze, "-> A  (cost=nan rows=inf) (actual time=x..y rows=z loops=-1)").unwrap();
    assert_eq!(p.nodes.len(), 1);
    assert_eq!(p.nodes[0].total_cost, None);
    assert_eq!(p.nodes[0].est_rows, None);
    assert_eq!(p.nodes[0].operation, "A");
}

#[test]
fn mysql_json_robustness() {
    assert!(matches!(parse(D::MySql, M::Estimate, "{\"query_block\": 5}"), Err(E::UnexpectedShape(_))));
    assert!(matches!(parse(D::MySql, M::Estimate, "{\"other\": {}}"), Err(E::UnexpectedShape(_))));
    assert!(matches!(parse(D::MySql, M::Estimate, "{\"query_block\": {"), Err(E::InvalidJson(_))));
    assert!(matches!(parse(D::MariaDb, M::Estimate, "-> not json"), Err(E::InvalidJson(_))));
    assert!(matches!(parse(D::MariaDb, M::Estimate, "[1,2]"), Err(E::UnexpectedShape(_))));
    let json = r#"{"query_block": {"select_id": "x", "cost_info": {"query_cost": [1]}, "nested_loop": [1, null, {"table": 5}, {"table": {"table_name": 3, "cost_info": "bad", "rows_examined_per_scan": "many"}}]}}"#;
    let p = parse(D::MySql, M::Estimate, json).unwrap();
    assert_eq!(p.nodes.len(), 3);
    assert_eq!(p.nodes[2].operation, "Table access");
    assert_eq!(p.nodes[2].object, None);
}

#[test]
fn mysql_json_no_tables() {
    let p = parse(D::MySql, M::Estimate, r#"{"query_block": {"select_id": 1, "message": "No tables used"}}"#).unwrap();
    assert_eq!(p.nodes.len(), 1);
    assert_eq!(p.nodes[0].detail.as_deref(), Some("select #1; No tables used"));
}

#[test]
fn mariadb_extra_shapes() {
    let json = r#"{"query_block": {"select_id": 1, "cost": 5, "nested_loop": [
        {"block-nl-join": {"table": {"table_name": "a", "access_type": "ALL", "rows": 10, "cost": 2}, "buffer_type": "flat", "r_filtered": 100}},
        {"table": {"table_name": "b", "access_type": "ALL", "rows": 20, "cost": 3}}],
        "expression_cache": {"state": "uninitialized", "query_block": {"select_id": 2}}}}"#;
    let p = parse(D::MariaDb, M::Estimate, json).unwrap();
    let shape: Vec<_> = p.nodes.iter().map(|n| (n.operation.as_str(), n.depth)).collect();
    assert_eq!(
        shape,
        [("Query block", 0), ("Nested loop", 1), ("Block nested loop join", 2), ("Full table scan", 3), ("Full table scan", 2), ("Expression cache", 1), ("Query block", 2)]
    );
    assert!(p.nodes[2].props.iter().any(|(k, v)| k == "buffer_type" && v == "flat"));
}

// ---- SQLite --------------------------------------------------------------------------------------

#[test]
fn sqlite_tree_from_rows_in_any_order() {
    let rows = sqlite(&[("3", "2", "SCAN b"), ("2", "0", "MERGE (UNION)"), ("5", "2", "SEARCH c USING INDEX i (x=?)"), ("9", "0", "USE TEMP B-TREE FOR ORDER BY")]);
    let p = Plan::parse(D::Sqlite, M::Estimate, &rows).unwrap();
    let shape: Vec<_> = p.nodes.iter().map(|n| (n.operation.as_str(), n.depth, n.parent)).collect();
    assert_eq!(
        shape,
        [("MERGE (UNION)", 0, None), ("SCAN", 1, Some(0)), ("SEARCH", 1, Some(0)), ("USE TEMP B-TREE FOR ORDER BY", 0, None)]
    );
    assert_eq!(p.nodes[2].object.as_deref(), Some("c"));
    assert_eq!(p.nodes[2].detail.as_deref(), Some("USING INDEX i (x=?)"));
}

#[test]
fn sqlite_detail_shapes() {
    let rows = sqlite(&[("1", "0", "SCAN t1 AS x"), ("2", "0", "SCAN CONSTANT ROW"), ("3", "0", "SCAN TABLE old"), ("4", "0", "SEARCH t USING COVERING INDEX i (a=? AND b>?)"), ("5", "0", "")]);
    let p = Plan::parse(D::Sqlite, M::Estimate, &rows).unwrap();
    assert_eq!(p.nodes[0].object.as_deref(), Some("t1 AS x"));
    assert_eq!(p.nodes[1].object.as_deref(), Some("CONSTANT ROW"));
    assert_eq!(p.nodes[2].object.as_deref(), Some("old"));
    assert_eq!(p.nodes[3].detail.as_deref(), Some("USING COVERING INDEX i (a=? AND b>?)"));
    assert_eq!(p.nodes[4].operation, "");
}

#[test]
fn sqlite_cycles_and_bad_rows_are_errors() {
    let cyc = |rows: &[(&str, &str, &str)]| Plan::parse(D::Sqlite, M::Estimate, &sqlite(rows));
    assert_eq!(cyc(&[("1", "2", "A"), ("2", "1", "B")]).unwrap_err(), E::CyclicPlan);
    assert_eq!(cyc(&[("1", "1", "A")]).unwrap_err(), E::CyclicPlan);
    assert_eq!(cyc(&[("1", "0", "A"), ("2", "3", "B"), ("3", "2", "C")]).unwrap_err(), E::CyclicPlan);
    assert!(matches!(cyc(&[("1", "7", "A")]), Err(E::InvalidRow { row: 0, .. })));
    assert!(matches!(cyc(&[("1", "0", "A"), ("1", "0", "B")]), Err(E::InvalidRow { row: 1, .. })));
    assert!(matches!(cyc(&[("x", "0", "A")]), Err(E::InvalidRow { row: 0, .. })));
    assert!(matches!(cyc(&[("1", "", "A")]), Err(E::InvalidRow { .. })));
    assert!(matches!(cyc(&[("99999999999999999999", "0", "A")]), Err(E::InvalidRow { .. })));
    assert_eq!(Plan::parse(D::Sqlite, M::Estimate, &PlanRows::default()).unwrap_err(), E::EmptyOutput);
}

#[test]
fn sqlite_columns_by_name_or_position() {
    let swapped = PlanRows {
        columns: ["detail", "notused", "parent", "id"].map(String::from).to_vec(),
        rows: vec![vec![Some("SCAN t".into()), Some("0".into()), Some("0".into()), Some("4".into())]],
    };
    assert_eq!(Plan::parse(D::Sqlite, M::Estimate, &swapped).unwrap().nodes[0].object.as_deref(), Some("t"));
    let unnamed = PlanRows { columns: vec![], rows: vec![vec![Some("4".into()), Some("0".into()), Some("0".into()), Some("SCAN t".into())]] };
    assert_eq!(Plan::parse(D::Sqlite, M::Estimate, &unnamed).unwrap().nodes.len(), 1);
    let odd = PlanRows { columns: ["a", "b", "c", "d", "e"].map(String::from).to_vec(), rows: vec![vec![Some("1".into()); 5]] };
    assert!(matches!(Plan::parse(D::Sqlite, M::Estimate, &odd), Err(E::MissingColumn("id"))));
    // Short rows and NULL cells.
    let short = PlanRows { columns: vec![], rows: vec![vec![Some("4".into())]] };
    assert!(matches!(Plan::parse(D::Sqlite, M::Estimate, &short), Err(E::InvalidRow { .. })));
    let null_detail = PlanRows { columns: vec![], rows: vec![vec![Some("4".into()), Some("0".into()), None, None]] };
    assert_eq!(Plan::parse(D::Sqlite, M::Estimate, &null_detail).unwrap().nodes[0].operation, "");
}

#[test]
fn analyze_is_refused_when_parsing_for_sqlite_and_clickhouse() {
    assert_eq!(Plan::parse(D::Sqlite, M::Analyze, &sqlite(&[("1", "0", "SCAN t")])).unwrap_err(), E::AnalyzeUnsupported(D::Sqlite));
    assert_eq!(parse(D::ClickHouse, M::Analyze, "[]").unwrap_err(), E::AnalyzeUnsupported(D::ClickHouse));
}

// ---- ClickHouse ----------------------------------------------------------------------------------

#[test]
fn clickhouse_shapes() {
    let json = r#"[{"Plan": {"Node Type": "Union", "Plans": [{"Node Type": "ReadFromMergeTree", "Description": "db.t",
        "Indexes": [{"Type": "MinMax", "Condition": "true", "Initial Parts": 2, "Selected Parts": 2},
                    {"Type": "Skip", "Name": "idx", "Keys": ["a", "b"], "Initial Granules": 10, "Selected Granules": 3}, 5]}]}}]"#;
    let p = parse(D::ClickHouse, M::Estimate, json).unwrap();
    assert_eq!(p.nodes[1].object.as_deref(), Some("db.t"));
    let idx = &p.nodes[1].props.iter().find(|(k, _)| k == "Indexes").unwrap().1;
    assert_eq!(idx, "MinMax [true]: parts 2/2; Skip idx(a, b): granules 3/10");
    // An object (not array) form, and a bare plan object.
    assert!(parse(D::ClickHouse, M::Estimate, r#"{"Plan": {"Node Type": "A"}}"#).is_ok());
    assert!(parse(D::ClickHouse, M::Estimate, r#"{"Node Type": "A"}"#).is_ok());
    assert!(matches!(parse(D::ClickHouse, M::Estimate, "[{}]"), Err(E::UnexpectedShape(_))));
    assert!(matches!(parse(D::ClickHouse, M::Estimate, "Expression (Project names)"), Err(E::InvalidJson(_))));
}

#[test]
fn long_property_values_are_cut_on_a_char_boundary() {
    let long = "é".repeat(10_000);
    let json = format!(r#"[{{"Plan": {{"Node Type": "A", "Filter": "{long}"}}}}]"#);
    let p = parse(D::Postgres, M::Estimate, &json).unwrap();
    let v = &p.nodes[0].props.iter().find(|(k, _)| k == "Filter").unwrap().1;
    assert_eq!(v.chars().count(), 4097);
    assert!(v.ends_with('…'));
}

// ---- limits and hostile input ----------------------------------------------------------------------

#[test]
fn empty_outputs() {
    for d in [D::Postgres, D::MySql, D::MariaDb, D::ClickHouse] {
        assert_eq!(Plan::parse(d, M::Estimate, &PlanRows::default()).unwrap_err(), E::EmptyOutput);
        assert_eq!(parse(d, M::Estimate, "  \n ").unwrap_err(), E::EmptyOutput);
        let nulls = PlanRows { columns: vec!["x".into()], rows: vec![vec![None], vec![]] };
        assert_eq!(Plan::parse(d, M::Estimate, &nulls).unwrap_err(), E::EmptyOutput);
    }
}

#[test]
fn json_split_over_several_rows_is_joined() {
    let text = fixture_payload("postgres/join_estimate.json");
    let rows = PlanRows { columns: vec!["QUERY PLAN".into()], rows: text.lines().map(|l| vec![Some(l.to_string())]).collect() };
    let p = Plan::parse(D::Postgres, M::Estimate, &rows).unwrap();
    assert_eq!(p.nodes.len(), 6);
}

#[test]
fn raw_size_cap() {
    let big = "x".repeat(MAX_RAW_BYTES + 1);
    for d in [D::Postgres, D::MySql, D::ClickHouse, D::Sqlite] {
        let rows = PlanRows { columns: vec![], rows: vec![vec![Some(big.clone())]] };
        assert_eq!(Plan::parse(d, M::Estimate, &rows).unwrap_err(), E::OutputTooLarge { bytes: MAX_RAW_BYTES + 1, limit: MAX_RAW_BYTES });
    }
    // Exactly at the cap is fine as far as the size check goes (then it fails as non-JSON).
    let at = "x".repeat(MAX_RAW_BYTES);
    assert!(matches!(parse(D::Postgres, M::Estimate, &at), Err(E::InvalidJson(_))));
}

#[test]
fn node_cap_postgres_wide_and_mysql_tree_and_sqlite() {
    let leaf = r#"{"Node Type": "L"}"#;
    let ok = vec![leaf; MAX_NODES - 1].join(",");
    let p = parse(D::Postgres, M::Estimate, &pg(&format!(r#"{{"Node Type": "A", "Plans": [{ok}]}}"#))).unwrap();
    assert_eq!(p.nodes.len(), MAX_NODES);
    let too_many = vec![leaf; MAX_NODES].join(",");
    let err = parse(D::Postgres, M::Estimate, &pg(&format!(r#"{{"Node Type": "A", "Plans": [{too_many}]}}"#))).unwrap_err();
    assert_eq!(err, E::TooManyNodes { limit: MAX_NODES });

    let tree = "-> A\n".repeat(MAX_NODES + 1);
    assert_eq!(parse(D::MySql, M::Analyze, &tree).unwrap_err(), E::TooManyNodes { limit: MAX_NODES });

    let strs: Vec<String> = (1..=MAX_NODES + 1).map(|i| i.to_string()).collect();
    let rows = PlanRows {
        columns: vec![],
        rows: strs.iter().map(|s| vec![Some(s.clone()), Some("0".into()), Some("0".into()), Some("SCAN t".into())]).collect(),
    };
    assert_eq!(Plan::parse(D::Sqlite, M::Estimate, &rows).unwrap_err(), E::TooManyNodes { limit: MAX_NODES });
}

#[test]
fn deeply_nested_json_is_an_error_not_a_stack_overflow() {
    for open in ["[", "{\"Plans\":[", "{\"query_block\":"] {
        let text = open.repeat(100_000);
        for d in [D::Postgres, D::MySql, D::MariaDb, D::ClickHouse] {
            match parse(d, M::Estimate, &text) {
                Err(E::InvalidJson(_)) => {}
                // MySQL treats text that does not start with `{` as tree text.
                Err(E::UnexpectedShape(_)) if d == D::MySql => {}
                other => panic!("{open} {d:?}: {other:?}"),
            }
        }
    }
    // Nested as deep as the JSON parser allows: parses, no overflow.
    let mut text = String::from(r#"{"Node Type": "L"}"#);
    for _ in 0..60 {
        text = format!(r#"{{"Node Type": "N", "Plans": [{text}]}}"#);
    }
    let p = parse(D::Postgres, M::Estimate, &pg(&text)).unwrap();
    assert_eq!(p.nodes.len(), 61);
    assert_eq!(p.nodes[60].depth, 60);
}

#[test]
fn very_deep_mysql_tree_text_and_sqlite_chains() {
    // 3,000 levels of indentation (a 40k-level chain would need 800 MB of spaces).
    let levels = 3_000;
    let chain: String = (0..levels).map(|i| format!("{}-> Filter: (a)  (cost={} rows=1)\n", " ".repeat(i * 2), levels - i)).collect();
    let p = parse(D::MySql, M::Estimate, &chain).unwrap();
    assert_eq!(p.nodes.len(), levels);
    assert_eq!(p.nodes[levels - 1].depth as usize, levels - 1);
    assert_eq!(p.nodes[0].self_cost, Some(1.0));

    let rows: Vec<(String, String)> = (1..=MAX_NODES).map(|i| (i.to_string(), (i - 1).to_string())).collect();
    let rows = PlanRows {
        columns: vec![],
        rows: rows.iter().map(|(i, p)| vec![Some(i.clone()), Some(p.clone()), Some("0".into()), Some("SCAN t".into())]).collect(),
    };
    let p = Plan::parse(D::Sqlite, M::Estimate, &rows).unwrap();
    assert_eq!(p.nodes.len(), MAX_NODES);
    assert_eq!(p.nodes[MAX_NODES - 1].depth, (MAX_NODES - 1).min(u16::MAX as usize) as u16);
}

#[test]
fn depth_saturates_instead_of_overflowing() {
    // 70k-deep chain from the tree text parser would exceed the node cap; the u16
    // depth is exercised through the cap-sized chain above. Here: depth stays within u16.
    let levels = 1_000;
    let chain: String = (0..levels).map(|i| format!("{}-> A\n", " ".repeat(i))).collect();
    let p = parse(D::MySql, M::Estimate, &chain).unwrap();
    assert_eq!(p.nodes.last().unwrap().depth, 999);
}

#[test]
fn truncating_and_corrupting_fixtures_never_panics() {
    let sources = [
        (D::Postgres, M::Analyze, "postgres/cte_analyze.json"),
        (D::MySql, M::Estimate, "mysql/cte_estimate.json"),
        (D::MySql, M::Analyze, "mysql/cte_analyze.txt"),
        (D::MariaDb, M::Analyze, "mariadb/cte_analyze.json"),
        (D::ClickHouse, M::Estimate, "clickhouse/union_estimate.json"),
    ];
    for (d, m, path) in sources {
        let text = fixture_payload(path);
        let step = (text.len() / 97).max(1);
        for cut in (0..text.len()).step_by(step) {
            let mut end = cut;
            while !text.is_char_boundary(end) {
                end -= 1;
            }
            let _ = parse(d, m, &text[..end]);
        }
        // Flip structure characters.
        for (i, ch) in text.char_indices().filter(|(_, c)| "{}[]\":,()".contains(*c)).step_by(37) {
            let mut s = text.clone();
            s.replace_range(i..i + ch.len_utf8(), "#");
            let _ = parse(d, m, &s);
        }
    }
}

#[test]
fn plan_is_clone_and_eq() {
    let p = parse(D::Postgres, M::Estimate, &pg(r#"{"Node Type": "A"}"#)).unwrap();
    assert_eq!(p.clone(), p);
}

// ---- misestimate flags and Limit ----------------------------------------------------------------

#[test]
fn misestimate_flag_matches_the_warning() {
    let json = pg(r#"{"Node Type": "Seq Scan", "Relation Name": "t", "Plan Rows": 10, "Actual Rows": 12000, "Actual Loops": 1}"#);
    let p = parse(D::Postgres, M::Analyze, &json).unwrap();
    let m = p.nodes[0].misestimate.unwrap();
    assert!(m.under);
    assert_eq!(m.factor, 1200.0);
    let json = pg(r#"{"Node Type": "Seq Scan", "Relation Name": "t", "Plan Rows": 5000, "Actual Rows": 50, "Actual Loops": 1}"#);
    let m = parse(D::Postgres, M::Analyze, &json).unwrap().nodes[0].misestimate.unwrap();
    assert!(!m.under);
    assert_eq!(m.factor, 100.0);
    let fine = pg(r#"{"Node Type": "Seq Scan", "Plan Rows": 50, "Actual Rows": 100, "Actual Loops": 1}"#);
    assert_eq!(parse(D::Postgres, M::Analyze, &fine).unwrap().nodes[0].misestimate, None);
}

#[test]
fn limit_ancestor_hides_only_overestimates() {
    let json = pg(r#"{"Node Type": "Limit", "Plan Rows": 10, "Actual Rows": 10, "Actual Loops": 1, "Plans": [
        {"Node Type": "Sort", "Plan Rows": 5000, "Actual Rows": 10, "Actual Loops": 1, "Plans": [
            {"Node Type": "Seq Scan", "Relation Name": "t", "Plan Rows": 5000, "Actual Rows": 5000, "Actual Loops": 1},
            {"Node Type": "Index Scan", "Relation Name": "u", "Plan Rows": 1, "Actual Rows": 900, "Actual Loops": 1}]}]}"#);
    let p = parse(D::Postgres, M::Analyze, &json).unwrap();
    // Sort: over-estimate under a Limit, silent. Index Scan: under-estimate, still reported.
    assert_eq!(p.warnings, ["Index Scan on u: rows estimated 1, actual 900"]);
    assert!(p.nodes[1].misestimate.is_none());
    assert!(p.nodes[3].misestimate.unwrap().under);
    // The same over-estimate without a Limit above it is reported.
    let json = pg(r#"{"Node Type": "Sort", "Plan Rows": 5000, "Actual Rows": 10, "Actual Loops": 1}"#);
    assert_eq!(parse(D::Postgres, M::Analyze, &json).unwrap().warnings.len(), 1);
}

#[test]
fn limit_in_mysql_tree_text() {
    let text = "-> Limit: 5 row(s)  (cost=9 rows=5) (actual time=1..1 rows=5 loops=1)\n    -> Table scan on t  (cost=9 rows=900) (actual time=1..1 rows=5 loops=1)\n-> Sort: a  (cost=9 rows=900) (actual time=1..1 rows=5 loops=1)\n";
    let p = parse(D::MySql, M::Analyze, text).unwrap();
    // The second root is not below the Limit.
    assert_eq!(p.warnings, ["Sort: rows estimated 900, actual 5"]);
}

// ---- caps on display text ------------------------------------------------------------------------

#[test]
fn operation_object_and_detail_are_capped() {
    let long = "x".repeat(100_000);
    let json = pg(&format!(r#"{{"Node Type": "{long}", "Relation Name": "{long}", "Filter": "{long}", "Index Name": "i"}}"#));
    let p = parse(D::Postgres, M::Estimate, &json).unwrap();
    let n = &p.nodes[0];
    assert_eq!(n.operation.chars().count(), 513);
    assert_eq!(n.object.as_ref().unwrap().chars().count(), 513);
    assert_eq!(n.detail.as_ref().unwrap().chars().count(), 2049);
    assert!(n.detail.as_ref().unwrap().ends_with('…'));
    // The (separately capped) full text stays in props.
    assert!(n.props.iter().any(|(k, v)| k == "Filter" && v.chars().count() == 4097));

    let text = format!("-> Filter: ({long})  (cost=1 rows=1)\n");
    let p = parse(D::MySql, M::Estimate, &text).unwrap();
    assert_eq!(p.nodes[0].detail.as_ref().unwrap().chars().count(), 2049);
    assert!(p.raw.contains(&long));
}

#[test]
fn many_continuation_lines_stay_linear() {
    let mut text = String::from("-> Filter: (a)  (cost=1 rows=1)\n");
    for _ in 0..200_000 {
        text.push_str("        and more condition text\n");
    }
    let start = std::time::Instant::now();
    let p = parse(D::MySql, M::Estimate, &text).unwrap();
    assert!(start.elapsed() < std::time::Duration::from_secs(1), "{:?}", start.elapsed());
    assert_eq!(p.nodes.len(), 1);
    assert_eq!(p.nodes[0].detail.as_ref().unwrap().chars().count(), 2049);
}

#[test]
fn many_short_nodes_and_long_lines_parse_quickly() {
    let start = std::time::Instant::now();
    let text = "-> Table scan on t  (cost=1 rows=1)\n".repeat(MAX_NODES);
    let p = parse(D::MySql, M::Estimate, &text).unwrap();
    assert_eq!(p.nodes.len(), MAX_NODES);
    assert!(start.elapsed() < std::time::Duration::from_secs(1), "{:?}", start.elapsed());
}

// ---- PostgreSQL parallel plans -------------------------------------------------------------------

#[test]
fn parallel_nodes_report_elapsed_not_summed_time() {
    let json = pg(r#"{"Node Type": "Gather", "Workers Planned": 2, "Workers Launched": 2, "Actual Rows": 300, "Actual Loops": 1, "Actual Total Time": 30.0,
        "Plans": [{"Node Type": "Seq Scan", "Parallel Aware": true, "Relation Name": "t", "Actual Rows": 100, "Actual Loops": 3, "Actual Total Time": 27.0,
                   "Plans": [{"Node Type": "Hash", "Actual Rows": 1, "Actual Loops": 3, "Actual Total Time": 3.0}]}]}"#);
    let p = parse(D::Postgres, M::Analyze, &json).unwrap();
    // Gather itself: leader view, not divided.
    assert_eq!(p.nodes[0].actual_time_ms, Some(30.0));
    assert!(!p.nodes[0].props.iter().any(|(k, _)| k == "Time Summed Over Processes"));
    // 27 ms per loop x 3 loops = 81 ms of work, over 3 processes: 27 ms elapsed.
    assert_eq!(p.nodes[1].actual_time_ms, Some(27.0));
    assert!(p.nodes[1].props.iter().any(|(k, v)| k == "Time Summed Over Processes" && v == "81 ms"));
    assert_eq!(p.nodes[2].actual_time_ms, Some(3.0));
    assert!(p.nodes[1].actual_time_ms.unwrap() <= p.nodes[0].actual_time_ms.unwrap());
}

#[test]
fn parallel_edge_cases() {
    // Fewer loops than participants (a worker did not take part), loops 1, and a Single Copy gather.
    let json = pg(r#"{"Node Type": "Gather Merge", "Workers Launched": 3, "Actual Loops": 1, "Actual Total Time": 10,
        "Plans": [{"Node Type": "Sort", "Actual Loops": 2, "Actual Total Time": 8},
                  {"Node Type": "Result", "Actual Loops": 1, "Actual Total Time": 4}]}"#);
    let p = parse(D::Postgres, M::Analyze, &json).unwrap();
    assert_eq!(p.nodes[1].actual_time_ms, Some(8.0));
    assert_eq!(p.nodes[2].actual_time_ms, Some(4.0));
    let json = pg(r#"{"Node Type": "Gather", "Single Copy": true, "Workers Launched": 1, "Actual Loops": 1,
        "Plans": [{"Node Type": "Seq Scan", "Actual Loops": 1, "Actual Total Time": 5}]}"#);
    let p = parse(D::Postgres, M::Analyze, &json).unwrap();
    assert_eq!(p.nodes[1].actual_time_ms, Some(5.0));
    // Estimate output (no Workers Launched): no division.
    let json = pg(r#"{"Node Type": "Gather", "Plans": [{"Node Type": "Seq Scan", "Actual Loops": 4, "Actual Total Time": 5}]}"#);
    assert_eq!(parse(D::Postgres, M::Analyze, &json).unwrap().nodes[1].actual_time_ms, Some(20.0));
}
