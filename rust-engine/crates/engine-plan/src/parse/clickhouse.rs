//! ClickHouse `EXPLAIN json = 1, actions = 1, indexes = 1`:
//! `[{"Plan": {"Node Type": ..., "Plans": [...], ...}}]`.
//!
//! ClickHouse reports no costs, row estimates or timings here. The bulky
//! `actions` payloads (`Expression`, `Prewhere info`, ...) are kept in `props`
//! as compact JSON, cut at 4096 characters; `Indexes` is summarised.

use serde_json::{Map, Value};

use super::json;
use crate::plan::{Parsed, PlanNode, Tree};
use crate::value::{field_text, join_nonempty, prop_string};
use crate::ExplainError;

pub(crate) fn parse(raw: &str) -> Result<Parsed, ExplainError> {
    let doc = json(raw)?;
    let top = match &doc {
        Value::Array(items) => items.first().and_then(Value::as_object),
        Value::Object(o) => Some(o),
        _ => None,
    };
    let root = top
        .and_then(|t| t.get("Plan").and_then(Value::as_object).or(Some(t).filter(|t| t.contains_key("Node Type"))))
        .ok_or(ExplainError::UnexpectedShape("no \"Plan\" object in the ClickHouse output"))?;

    let mut tree = Tree::default();
    let mut stack: Vec<(&Map<String, Value>, Option<usize>)> = vec![(root, None)];
    while let Some((obj, parent)) = stack.pop() {
        let idx = tree.push(parent, node(obj))?;
        if let Some(Value::Array(children)) = obj.get("Plans") {
            stack.extend(children.iter().rev().filter_map(Value::as_object).map(|c| (c, Some(idx))));
        }
    }
    Ok(Parsed { nodes: tree.nodes, ..Parsed::default() })
}

fn node(o: &Map<String, Value>) -> PlanNode {
    let operation = field_text(o, "Node Type").unwrap_or("Unknown").to_string();
    let description = field_text(o, "Description").map(str::to_string);
    // `ReadFromMergeTree` describes the table it reads; other steps describe themselves.
    let reads_table = operation.starts_with("ReadFrom");
    let (object, detail) = if reads_table { (description, None) } else { (None, description) };

    let props = o
        .iter()
        .filter(|(k, _)| !matches!(k.as_str(), "Plans" | "Node Type" | "Description"))
        .filter_map(|(k, v)| {
            let text = if k == "Indexes" { indexes(v) } else { prop_string(v) };
            Some((k.clone(), text?))
        })
        .collect();

    PlanNode { operation, object, detail, props, ..PlanNode::default() }
}

/// `PrimaryKey(t1_id) [t1_id in [42, 42]]: parts 1/1, granules 1/2; ...`
fn indexes(v: &Value) -> Option<String> {
    let Value::Array(items) = v else { return prop_string(v) };
    let lines = items.iter().filter_map(Value::as_object).map(|i| {
        let ratio = |sel: &str, init: &str| match (i.get(sel), i.get(init)) {
            (Some(s), Some(t)) => Some(format!("{}/{}", prop_string(s)?, prop_string(t)?)),
            _ => None,
        };
        let head = join_nonempty(
            [
                field_text(i, "Type").unwrap_or("Index").to_string(),
                field_text(i, "Name").map(|n| format!(" {n}")).unwrap_or_default(),
                i.get("Keys").and_then(prop_string).map(|k| format!("({k})")).unwrap_or_default(),
                field_text(i, "Condition").map(|c| format!(" [{c}]")).unwrap_or_default(),
            ],
            "",
        )
        .unwrap_or_default();
        let counts: Vec<String> = [
            ratio("Selected Parts", "Initial Parts").map(|r| format!("parts {r}")),
            ratio("Selected Granules", "Initial Granules").map(|r| format!("granules {r}")),
        ]
        .into_iter()
        .flatten()
        .collect();
        if counts.is_empty() { head } else { format!("{head}: {}", counts.join(", ")) }
    });
    join_nonempty(lines, "; ")
}
