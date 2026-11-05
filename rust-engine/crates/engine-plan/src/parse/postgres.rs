//! PostgreSQL `EXPLAIN (FORMAT JSON)` / `EXPLAIN (ANALYZE, BUFFERS, FORMAT JSON)`.
//!
//! `Plan Rows` / `Actual Rows` are per loop; `Actual Total Time` is per loop too
//! and is multiplied by `Actual Loops` for `actual_time_ms`.
//!
//! Below a `Gather` / `Gather Merge` the loops are the processes that ran the
//! node concurrently (workers launched + the leader), so per-loop time x loops is
//! CPU time summed over processes and would exceed the Gather node and the
//! execution time. For those nodes `actual_time_ms` is the elapsed estimate
//! `per-loop time x loops / min(loops, participants)`; the summed figure is kept
//! in the `Time Summed Over Processes` prop.

use serde_json::{Map, Value};

use super::json;
use crate::plan::{Parsed, PlanNode, Tree};
use crate::value::{field_count, field_number, field_text, prop_string};
use crate::ExplainError;

/// Fields mapped to struct fields (or to `object` / `detail`) and so not repeated in `props`.
const CONSUMED: [&str; 14] = [
    "Plans",
    "Node Type",
    "Startup Cost",
    "Total Cost",
    "Plan Rows",
    "Actual Rows",
    "Actual Loops",
    "Actual Total Time",
    "Relation Name",
    "Schema",
    "Alias",
    "CTE Name",
    "Function Name",
    "Index Name",
];

/// Condition fields, the first present one becomes the node's `detail`.
const CONDITIONS: [&str; 7] =
    ["Index Cond", "Hash Cond", "Merge Cond", "Join Filter", "Recheck Cond", "Filter", "One-Time Filter"];

pub(crate) fn parse(raw: &str) -> Result<Parsed, ExplainError> {
    let doc = json(raw)?;
    let mut warnings = Vec::new();
    let top = match &doc {
        Value::Array(items) => {
            if items.len() > 1 {
                warnings.push(format!("the output holds {} plans, only the first is shown", items.len()));
            }
            items.first().and_then(Value::as_object)
        }
        Value::Object(o) => Some(o),
        _ => None,
    }
    .ok_or(ExplainError::UnexpectedShape("expected a JSON array holding an object"))?;
    let root = top
        .get("Plan")
        .and_then(Value::as_object)
        .ok_or(ExplainError::UnexpectedShape("no \"Plan\" object in the PostgreSQL output"))?;

    let mut tree = Tree::default();
    let mut stack: Vec<(&Map<String, Value>, Option<usize>, u64)> = vec![(root, None, 1)];
    while let Some((obj, parent, participants)) = stack.pop() {
        let (node, spill) = node(obj, participants);
        let idx = tree.push(parent, node)?;
        if let Some(spill) = spill {
            let n = &tree.nodes[idx];
            warnings.push(format!("{}: {spill}", n.operation));
        }
        if let Some(Value::Array(children)) = obj.get("Plans") {
            let below = participants_below(obj, participants);
            stack.extend(children.iter().rev().filter_map(Value::as_object).map(|c| (c, Some(idx), below)));
        }
    }

    let mut nodes = tree.nodes;
    for (key, value) in top {
        match key.as_str() {
            "Plan" | "Planning Time" | "Execution Time" => {}
            "Planning" => {
                if let Some(o) = value.as_object() {
                    nodes[0].props.extend(nonzero_buffers(o).map(|(k, v)| (format!("Planning {k}"), v)));
                }
            }
            _ => {
                if let Some(v) = prop_string(value) {
                    nodes[0].props.push((key.clone(), v));
                }
            }
        }
    }

    Ok(Parsed {
        nodes,
        planning_ms: field_number(top, "Planning Time"),
        execution_ms: field_number(top, "Execution Time"),
        warnings,
        cumulative_cost: true,
    })
}

fn is_counter(key: &str) -> bool {
    key.ends_with(" Blocks") || key.ends_with(" Time")
}

fn nonzero_buffers(o: &Map<String, Value>) -> impl Iterator<Item = (String, String)> + '_ {
    o.iter()
        .filter(|(k, v)| !(is_counter(k) && field_number_is_zero(v)))
        .filter_map(|(k, v)| Some((k.clone(), prop_string(v)?)))
}

fn field_number_is_zero(v: &Value) -> bool {
    v.as_f64() == Some(0.0)
}

/// The node plus a note when it spilled to disk.
fn node(o: &Map<String, Value>, participants: u64) -> (PlanNode, Option<String>) {
    let mut operation = field_text(o, "Node Type").unwrap_or("Unknown").to_string();
    if operation == "Aggregate" {
        operation = match field_text(o, "Strategy") {
            Some("Hashed") => "HashAggregate",
            Some("Sorted") => "GroupAggregate",
            Some("Mixed") => "MixedAggregate",
            _ => "Aggregate",
        }
        .to_string();
    }

    let relation = field_text(o, "Relation Name").map(|r| match field_text(o, "Schema") {
        Some(s) => format!("{s}.{r}"),
        None => r.to_string(),
    });
    let object = match (relation, field_text(o, "Alias")) {
        (Some(r), Some(a)) if !r.ends_with(a) => Some(format!("{r} AS {a}")),
        (Some(r), _) => Some(r),
        (None, _) => field_text(o, "CTE Name").or_else(|| field_text(o, "Function Name")).map(str::to_string),
    };
    let detail = crate::value::join_nonempty(
        field_text(o, "Index Name")
            .map(|i| format!("using {i}"))
            .into_iter()
            .chain(CONDITIONS.iter().find_map(|k| field_text(o, k)).map(str::to_string)),
        "; ",
    );

    let loops = field_count(o, "Actual Loops");
    let summed_ms = field_number(o, "Actual Total Time").map(|t| t * loops.unwrap_or(1) as f64);
    let divisor = loops.unwrap_or(1).min(participants).max(1);
    let actual_time_ms = summed_ms.map(|t| t / divisor as f64);

    let mut props: Vec<(String, String)> = o
        .iter()
        .filter(|(k, _)| !CONSUMED.contains(&k.as_str()))
        .filter(|(k, v)| !(is_counter(k) && field_number_is_zero(v)))
        .filter_map(|(k, v)| Some((k.clone(), prop_string(v)?)))
        .collect();

    if let (Some(sum), true) = (summed_ms, divisor > 1) {
        props.push(("Time Summed Over Processes".to_string(), format!("{sum} ms")));
    }

    let spill = if field_text(o, "Sort Method").is_some_and(|m| m.starts_with("external")) {
        Some("sort spilled to disk".to_string())
    } else if field_number(o, "Disk Usage").is_some_and(|d| d > 0.0) {
        Some("hash spilled to disk".to_string())
    } else {
        None
    };

    let node = PlanNode {
        operation,
        object,
        detail,
        startup_cost: field_number(o, "Startup Cost"),
        total_cost: field_number(o, "Total Cost"),
        est_rows: field_number(o, "Plan Rows"),
        actual_rows: field_number(o, "Actual Rows"),
        actual_time_ms,
        loops,
        props,
        ..PlanNode::default()
    };
    (node, spill)
}

/// Processes that run the subtree below `o`: a Gather launches workers and (unless
/// `Single Copy`) the leader joins them; other nodes pass the count on.
fn participants_below(o: &Map<String, Value>, inherited: u64) -> u64 {
    if !matches!(field_text(o, "Node Type"), Some("Gather" | "Gather Merge")) {
        return inherited;
    }
    if o.get("Single Copy").and_then(Value::as_bool) == Some(true) {
        return 1;
    }
    field_count(o, "Workers Launched").map_or(1, |w| w.saturating_add(1))
}
