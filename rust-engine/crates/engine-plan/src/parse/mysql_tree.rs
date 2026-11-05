//! MySQL `EXPLAIN ANALYZE` (and `EXPLAIN FORMAT=TREE`) text:
//!
//! ```text
//! -> Nested loop inner join  (cost=2795 rows=6547) (actual time=2.74..8.9 rows=10000 loops=1)
//!     -> Table scan on t1  (cost=504 rows=5000) (actual time=1.6..2.1 rows=5000 loops=1)
//! ```
//!
//! Nesting follows the column of each `->`. `rows` and the `actual time` are per
//! loop; `actual_time_ms` is the end time times `loops`. `(never executed)`
//! gives `loops = 0` and no actuals.

use crate::plan::{Parsed, PlanNode, Tree, DETAIL_CAP};
use crate::ExplainError;

pub(crate) fn parse(raw: &str) -> Result<Parsed, ExplainError> {
    let mut tree = Tree::default();
    // (column of the `->`, node index) of the currently open ancestors.
    let mut open: Vec<(usize, usize)> = Vec::new();

    for line in raw.lines() {
        let body = line.trim_start();
        if body.trim_end().is_empty() {
            continue;
        }
        let column = line.len() - body.len();
        if let Some(rest) = body.strip_prefix("->") {
            while open.last().is_some_and(|&(c, _)| c >= column) {
                open.pop();
            }
            let parent = open.last().map(|&(_, i)| i);
            let idx = tree.push(parent, parse_line(rest.trim()))?;
            open.push((column, idx));
        } else if let Some(&(_, idx)) = open.last() {
            // A wrapped continuation of the previous node's text. Appended in
            // place; once the detail is past its display cap further lines add
            // nothing (the full text stays in `raw`), so this stays linear.
            let node = &mut tree.nodes[idx];
            let more = body.trim_end();
            let detail = node.detail.get_or_insert_with(String::new);
            if detail.len() < DETAIL_CAP * 4 {
                if !detail.is_empty() {
                    detail.push(' ');
                }
                detail.push_str(more);
            }
        }
    }
    for node in &mut tree.nodes {
        node.cap_text();
    }
    if tree.nodes.is_empty() {
        return Err(ExplainError::UnexpectedShape("no `->` plan lines in the output"));
    }

    let execution_ms = tree.nodes[0].actual_time_ms;
    Ok(Parsed { nodes: tree.nodes, planning_ms: None, execution_ms, warnings: Vec::new(), cumulative_cost: true })
}

/// `key=value` words of a `(...)` group such as `(cost=1..2 rows=3)`.
fn group_words<'a>(group: &'a str, lead: &str) -> Option<Vec<(&'a str, &'a str)>> {
    let inner = group.strip_prefix('(')?.strip_suffix(')')?;
    let inner = inner.strip_prefix(lead).unwrap_or(inner);
    inner.split_whitespace().map(|w| w.split_once('=')).collect()
}

fn num(s: &str) -> Option<f64> {
    s.parse::<f64>().ok().filter(|n| n.is_finite())
}

/// `a..b` -> (Some(a), b); a single value -> (None, value).
fn range(s: &str) -> Option<(Option<f64>, f64)> {
    match s.split_once("..") {
        Some((a, b)) => Some((Some(num(a)?), num(b)?)),
        None => Some((None, num(s)?)),
    }
}

fn parse_line(text: &str) -> PlanNode {
    let mut node = PlanNode::default();
    let mut label = text;

    if let Some(rest) = label.strip_suffix("(never executed)") {
        node.loops = Some(0);
        label = rest.trim_end();
    } else if label.ends_with(')') {
        if let Some(p) = label.rfind("(actual ") {
            if let Some(words) = group_words(&label[p..], "actual ") {
                let get = |k: &str| words.iter().find(|(w, _)| *w == k).map(|(_, v)| *v);
                node.loops = get("loops").and_then(|l| l.parse::<u64>().ok());
                node.actual_rows = get("rows").and_then(num);
                node.actual_time_ms = get("time")
                    .and_then(range)
                    .map(|(_, end)| end * node.loops.unwrap_or(1) as f64);
                label = label[..p].trim_end();
            }
        }
    }
    if label.ends_with(')') {
        if let Some(p) = label.rfind("(cost=") {
            if let Some(words) = group_words(&label[p..], "") {
                let get = |k: &str| words.iter().find(|(w, _)| *w == k).map(|(_, v)| *v);
                if let Some((startup, total)) = get("cost").and_then(range) {
                    node.startup_cost = startup;
                    node.total_cost = Some(total);
                }
                node.est_rows = get("rows").and_then(num);
                label = label[..p].trim_end();
            }
        }
    }

    let (operation, object, detail) = split_label(label);
    node.operation = operation;
    node.object = object;
    node.detail = detail;
    node
}

/// `Filter: cond` -> (Filter, -, cond); `Index lookup on t using i (c)` ->
/// (Index lookup, t, using i (c)); anything else is all operation.
fn split_label(label: &str) -> (String, Option<String>, Option<String>) {
    let nonempty = |s: &str| (!s.is_empty()).then(|| s.to_string());
    let colon = label.find(": ");
    let on = label.find(" on ");
    match (colon, on) {
        (Some(c), o) if o.is_none_or(|o| c < o) => {
            (label[..c].to_string(), None, nonempty(label[c + 2..].trim()))
        }
        (_, Some(o)) => {
            let rest = &label[o + 4..];
            let cut = [" using ", " over ", " ("].iter().filter_map(|m| rest.find(m)).min();
            let (object, tail) = match cut {
                Some(p) => (&rest[..p], rest[p..].trim()),
                None => (rest, ""),
            };
            (label[..o].to_string(), nonempty(object.trim()), nonempty(tail))
        }
        _ => (label.to_string(), None, None),
    }
}
