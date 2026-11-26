//! DuckDB `EXPLAIN (FORMAT JSON)` and `EXPLAIN ANALYZE (FORMAT JSON)`.
//!
//! The estimate is a list of operator trees, `[{"name": "TOP_N", "children": [...], "extra_info":
//! {"Estimated Cardinality": "5", ...}}]`. The analyze output is one object holding the same trees
//! under `"operator"` (and the timings of the planner and the optimizer beside it), each operator
//! with its own `"timing"` in seconds and the rows it produced, `"intermediate_rows"`.
//!
//! DuckDB reports no costs. `extra_info` values (a string, or a list of strings) are kept in `props`
//! as they are; the table of a scan is the node's object.

use serde_json::{Map, Value};

use super::json;
use crate::plan::{Parsed, PlanNode, Tree};
use crate::value::{field_number, field_text, join_nonempty, prop_string};
use crate::ExplainError;

pub(crate) fn parse(raw: &str) -> Result<Parsed, ExplainError> {
    let doc = json(raw)?;
    let roots: Vec<&Map<String, Value>> = match &doc {
        Value::Array(items) => items.iter().filter_map(Value::as_object).collect(),
        Value::Object(top) => match top.get("operator") {
            Some(Value::Array(items)) => items.iter().filter_map(Value::as_object).collect(),
            _ if top.contains_key("name") => vec![top],
            _ => Vec::new(),
        },
        _ => Vec::new(),
    };
    if roots.is_empty() {
        return Err(ExplainError::UnexpectedShape("no operator tree in the DuckDB output"));
    }

    let mut tree = Tree::default();
    let mut stack: Vec<(&Map<String, Value>, Option<usize>)> =
        roots.into_iter().rev().map(|root| (root, None)).collect();
    while let Some((object, parent)) = stack.pop() {
        let index = tree.push(parent, node(object))?;
        if let Some(Value::Array(children)) = object.get("children") {
            stack.extend(children.iter().rev().filter_map(Value::as_object).map(|child| (child, Some(index))));
        }
    }
    Ok(Parsed { nodes: tree.nodes, ..Parsed::default() })
}

/// `HASH_JOIN` as `Hash Join`.
fn operation(name: &str) -> String {
    name.split('_')
        .map(|word| {
            let mut chars = word.chars();
            chars.next().map_or_else(String::new, |first| {
                first.to_uppercase().chain(chars.flat_map(char::to_lowercase)).collect()
            })
        })
        .collect::<Vec<_>>()
        .join(" ")
}

fn node(o: &Map<String, Value>) -> PlanNode {
    let name = field_text(o, "name").or_else(|| field_text(o, "type")).unwrap_or("Unknown");
    let extra = o.get("extra_info").and_then(Value::as_object);
    let info = |key: &str| extra.and_then(|extra| extra.get(key)).and_then(prop_string);

    let object = info("Table");
    let detail = join_nonempty(
        ["Conditions", "Filters", "Groups", "Order By", "Projections"].into_iter().filter_map(info),
        "; ",
    );
    // A string: `"500"`, or `"1,000"` in some versions.
    let est_rows = extra
        .and_then(|extra| extra.get("Estimated Cardinality"))
        .and_then(Value::as_str)
        .and_then(|text| text.replace([',', '~'], "").trim().parse::<f64>().ok());

    let mut props: Vec<(String, String)> = extra
        .into_iter()
        .flatten()
        .filter(|(key, _)| !matches!(key.as_str(), "Table" | "Estimated Cardinality"))
        .filter_map(|(key, value)| Some((key.clone(), prop_string(value)?)))
        .collect();
    for key in ["rows_scanned", "row_groups_scanned", "total_row_groups_to_scan", "intermediate_size_bytes"] {
        if let Some(value) = o.get(key).and_then(prop_string) {
            props.push((key.to_owned(), value));
        }
    }
    PlanNode {
        operation: operation(name),
        object,
        detail,
        est_rows,
        actual_rows: field_number(o, "intermediate_rows"),
        actual_time_ms: field_number(o, "timing").map(|seconds| seconds * 1000.0),
        props,
        ..PlanNode::default()
    }
}
