//! MySQL and MariaDB `EXPLAIN FORMAT=JSON` / `ANALYZE FORMAT=JSON`.
//!
//! Both nest objects under keys such as `query_block`, `nested_loop`, `table`,
//! `ordering_operation`. The keys in [`NODE_KEYS`] become nodes, the arrays in
//! [`TRANSPARENT_ARRAYS`] only contribute their elements' nodes, every other
//! field lands in `props`.
//!
//! Costs here are additive per table (`read_cost + eval_cost` on MySQL, `cost`
//! on MariaDB), so a table's `self_cost` equals its `total_cost`; containers
//! (query blocks) carry the server's overall `query_cost` / `cost`.
//! `est_rows` is the per-scan estimate (`rows_examined_per_scan` / `rows`),
//! `actual_rows` is MariaDB's `r_rows`. MariaDB reports times as totals.

use serde_json::{Map, Value};

use super::{json, mysql_tree};
use crate::plan::{Parsed, PlanNode, Tree};
use crate::value::{field_count, field_number, field_text, join_nonempty, prop_string};
use crate::ExplainError;

const NODE_KEYS: [&str; 17] = [
    "query_block",
    "nested_loop",
    "table",
    "ordering_operation",
    "grouping_operation",
    "duplicates_removal",
    "buffer_result",
    "windowing",
    "window_functions_computation",
    "materialized_from_subquery",
    "materialized",
    "union_result",
    "filesort",
    "temporary_table",
    "read_sorted_file",
    "block-nl-join",
    "expression_cache",
];

const TRANSPARENT_ARRAYS: [&str; 4] =
    ["attached_subqueries", "optimized_away_subqueries", "subqueries", "query_specifications"];

/// Fields already represented by a struct field.
const CONSUMED: [&str; 12] = [
    "cost_info",
    "cost",
    "select_id",
    "table_name",
    "rows_examined_per_scan",
    "rows",
    "r_rows",
    "r_loops",
    "r_total_time_ms",
    "r_table_time_ms",
    "r_other_time_ms",
    "message",
];

enum Source<'a> {
    Object(&'a Map<String, Value>),
    /// The array form of `nested_loop`.
    List(&'a [Value]),
}

struct Work<'a> {
    key: &'a str,
    source: Source<'a>,
    parent: Option<usize>,
    /// Scalar fields of an array element wrapper (`dependent`, `cacheable`, ...).
    extra: Vec<(String, String)>,
}

fn is_node_key(k: &str) -> bool {
    NODE_KEYS.contains(&k)
}

pub(crate) fn parse(raw: &str, is_mysql: bool) -> Result<Parsed, ExplainError> {
    if is_mysql && !raw.trim_start().starts_with('{') {
        return mysql_tree::parse(raw);
    }
    let doc = json(raw)?;
    let top = doc
        .as_object()
        .ok_or(ExplainError::UnexpectedShape("expected a JSON object"))?;
    let root = top
        .get("query_block")
        .and_then(Value::as_object)
        .ok_or(ExplainError::UnexpectedShape("no \"query_block\" object in the output"))?;

    let mut tree = Tree::default();
    let mut stack = vec![Work { key: "query_block", source: Source::Object(root), parent: None, extra: Vec::new() }];
    while let Some(work) = stack.pop() {
        let mut children = Vec::new();
        let node = match &work.source {
            Source::Object(o) => {
                collect_children(o, &mut children);
                make_node(work.key, o, work.extra)
            }
            Source::List(items) => {
                for item in items.iter() {
                    wrapper_children(item, &mut children);
                }
                PlanNode { operation: operation_name(work.key, None).to_string(), ..PlanNode::default() }
            }
        };
        let idx = tree.push(work.parent, node)?;
        for child in &mut children {
            child.parent = Some(idx);
        }
        stack.extend(children.into_iter().rev());
    }

    let nodes = tree.nodes;
    let planning_ms = top
        .get("query_optimization")
        .and_then(Value::as_object)
        .and_then(|o| field_number(o, "r_total_time_ms"));
    let execution_ms = nodes.first().and_then(|n| n.actual_time_ms);
    Ok(Parsed { nodes, planning_ms, execution_ms, warnings: Vec::new(), cumulative_cost: false })
}

/// Child nodes of `o`, in document order.
fn collect_children<'a>(o: &'a Map<String, Value>, out: &mut Vec<Work<'a>>) {
    for (key, value) in o {
        match value {
            Value::Object(child) if is_node_key(key) => out.push(Work {
                key,
                source: Source::Object(child),
                parent: None,
                extra: Vec::new(),
            }),
            Value::Array(items) if is_node_key(key) => {
                out.push(Work { key, source: Source::List(items), parent: None, extra: Vec::new() })
            }
            Value::Array(items) if TRANSPARENT_ARRAYS.contains(&key.as_str()) => {
                for item in items {
                    wrapper_children(item, out);
                }
            }
            _ => {}
        }
    }
}

/// An array element such as `{"table": {...}}` or `{"dependent": false, "query_block": {...}}`.
fn wrapper_children<'a>(item: &'a Value, out: &mut Vec<Work<'a>>) {
    let Some(wrapper) = item.as_object() else { return };
    let mut extra: Vec<(String, String)> = wrapper
        .iter()
        .filter(|(_, v)| !v.is_object() && !v.is_array())
        .filter_map(|(k, v)| Some((k.clone(), prop_string(v)?)))
        .collect();
    for (key, value) in wrapper {
        if let Value::Object(child) = value {
            if is_node_key(key) {
                out.push(Work {
                    key,
                    source: Source::Object(child),
                    parent: None,
                    extra: std::mem::take(&mut extra),
                });
            }
        }
    }
}

fn operation_name(key: &str, access_type: Option<&str>) -> &'static str {
    match key {
        "query_block" => "Query block",
        "nested_loop" => "Nested loop",
        "table" => match access_type {
            Some("ALL") => "Full table scan",
            Some("index") => "Full index scan",
            Some("range") => "Index range scan",
            Some("ref") | Some("ref_or_null") => "Index lookup",
            Some("eq_ref") => "Unique index lookup",
            Some("const") | Some("system") => "Constant lookup",
            Some("fulltext") => "Fulltext search",
            Some("index_merge") => "Index merge",
            _ => "Table access",
        },
        "ordering_operation" => "Order by",
        "grouping_operation" => "Group by",
        "duplicates_removal" => "Distinct",
        "buffer_result" => "Buffer result",
        "windowing" => "Windowing",
        "window_functions_computation" => "Window functions",
        "materialized_from_subquery" | "materialized" => "Materialize",
        "union_result" => "Union",
        "filesort" => "Filesort",
        "temporary_table" => "Temporary table",
        "read_sorted_file" => "Read sorted file",
        "block-nl-join" => "Block nested loop join",
        "expression_cache" => "Expression cache",
        _ => "Operation",
    }
}

fn make_node(key: &str, o: &Map<String, Value>, extra: Vec<(String, String)>) -> PlanNode {
    let is_table = key == "table";
    let operation = operation_name(key, field_text(o, "access_type")).to_string();

    let detail = join_nonempty(
        [
            o.get("select_id").and_then(prop_string).map(|id| format!("select #{id}")),
            field_text(o, "key").map(|k| format!("using {k}")),
            field_text(o, "sort_key").map(str::to_string),
            field_text(o, "attached_condition").map(str::to_string),
            field_text(o, "message").map(str::to_string),
        ]
        .into_iter()
        .flatten(),
        "; ",
    );

    // Costs: MySQL nests them in `cost_info`, MariaDB has a plain `cost`.
    let cost_info = o.get("cost_info").and_then(Value::as_object);
    let info = |k: &str| cost_info.and_then(|c| field_number(c, k));
    let mut self_cost = None;
    let total_cost = if let Some(q) = info("query_cost") {
        Some(q)
    } else if info("read_cost").is_some() || info("eval_cost").is_some() {
        let own = info("read_cost").unwrap_or(0.0) + info("eval_cost").unwrap_or(0.0);
        self_cost = Some(own);
        Some(own)
    } else if let Some(sort) = info("sort_cost") {
        self_cost = Some(sort);
        Some(sort)
    } else if let Some(c) = field_number(o, "cost") {
        self_cost = is_table.then_some(c);
        Some(c)
    } else {
        None
    };

    // MariaDB times are totals; a table splits them in table and other time.
    let table_time = match (field_number(o, "r_table_time_ms"), field_number(o, "r_other_time_ms")) {
        (None, None) => None,
        (a, b) => Some(a.unwrap_or(0.0) + b.unwrap_or(0.0)),
    };

    let mut props = extra;
    if let Some(c) = cost_info {
        props.extend(
            c.iter()
                .filter(|(k, _)| k.as_str() != "query_cost")
                .filter_map(|(k, v)| Some((format!("cost_info.{k}"), prop_string(v)?))),
        );
    }
    props.extend(
        o.iter()
            .filter(|(k, v)| {
                let child = (is_node_key(k) || TRANSPARENT_ARRAYS.contains(&k.as_str()))
                    && (v.is_object() || v.is_array());
                !child && !CONSUMED.contains(&k.as_str())
            })
            .filter_map(|(k, v)| Some((k.clone(), prop_string(v)?))),
    );

    PlanNode {
        operation,
        object: field_text(o, "table_name").map(str::to_string),
        detail,
        total_cost,
        self_cost,
        est_rows: field_number(o, "rows_examined_per_scan").or_else(|| field_number(o, "rows")),
        actual_rows: field_number(o, "r_rows"),
        actual_time_ms: table_time.or_else(|| field_number(o, "r_total_time_ms")),
        loops: field_count(o, "r_loops"),
        props,
        ..PlanNode::default()
    }
}
