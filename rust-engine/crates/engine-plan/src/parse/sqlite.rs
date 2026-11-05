//! SQLite `EXPLAIN QUERY PLAN`: result rows `id`, `parent`, `notused`, `detail`.
//! `parent = 0` marks a top-level step. There are no costs or row counts.

use std::collections::HashMap;

use crate::plan::{Parsed, PlanNode, PlanRows, Tree};
use crate::ExplainError;

struct Row {
    id: i64,
    parent: i64,
    detail: String,
}

pub(crate) fn parse(rows: &PlanRows) -> Result<(String, Parsed), ExplainError> {
    if rows.rows.is_empty() {
        return Err(ExplainError::EmptyOutput);
    }
    if rows.rows.len() > crate::MAX_NODES {
        return Err(ExplainError::TooManyNodes { limit: crate::MAX_NODES });
    }
    let col = |name: &'static str, fallback: usize| -> Result<usize, ExplainError> {
        let named = rows.columns.iter().position(|c| c.eq_ignore_ascii_case(name));
        match named {
            Some(i) => Ok(i),
            // Unnamed columns: the documented order.
            None if rows.columns.len() <= 4 => Ok(fallback),
            None => Err(ExplainError::MissingColumn(name)),
        }
    };
    let (id_col, parent_col, detail_col) = (col("id", 0)?, col("parent", 1)?, col("detail", 3)?);

    let mut parsed_rows = Vec::with_capacity(rows.rows.len());
    for (n, row) in rows.rows.iter().enumerate() {
        let cell = |c: usize| row.get(c).and_then(|v| v.as_deref());
        let int = |c: usize, reason: &'static str| {
            cell(c)
                .and_then(|v| v.trim().parse::<i64>().ok())
                .ok_or(ExplainError::InvalidRow { row: n, reason })
        };
        parsed_rows.push(Row {
            id: int(id_col, "`id` is not an integer")?,
            parent: int(parent_col, "`parent` is not an integer")?,
            detail: cell(detail_col).unwrap_or_default().to_string(),
        });
    }

    let mut by_id: HashMap<i64, usize> = HashMap::with_capacity(parsed_rows.len());
    for (n, row) in parsed_rows.iter().enumerate() {
        if by_id.insert(row.id, n).is_some() {
            return Err(ExplainError::InvalidRow { row: n, reason: "duplicate `id`" });
        }
    }
    let mut children: Vec<Vec<usize>> = vec![Vec::new(); parsed_rows.len()];
    let mut roots = Vec::new();
    for (n, row) in parsed_rows.iter().enumerate() {
        if row.parent == 0 {
            roots.push(n);
        } else if let Some(&p) = by_id.get(&row.parent) {
            children[p].push(n);
        } else {
            return Err(ExplainError::InvalidRow { row: n, reason: "`parent` refers to no row" });
        }
    }

    // Pre-order walk; rows never reached sit on a parent cycle.
    let mut tree = Tree::default();
    let mut stack: Vec<(usize, Option<usize>)> = roots.iter().rev().map(|&r| (r, None)).collect();
    while let Some((row, parent)) = stack.pop() {
        let idx = tree.push(parent, node(&parsed_rows[row].detail))?;
        stack.extend(children[row].iter().rev().map(|&c| (c, Some(idx))));
    }
    if tree.nodes.len() != parsed_rows.len() {
        return Err(ExplainError::CyclicPlan);
    }

    let raw = rows
        .rows
        .iter()
        .map(|r| r.iter().map(|c| c.as_deref().unwrap_or("")).collect::<Vec<_>>().join("\t"))
        .collect::<Vec<_>>()
        .join("\n");
    Ok((raw, Parsed { nodes: tree.nodes, ..Parsed::default() }))
}

/// `SEARCH t USING INDEX i (a=?)` -> (SEARCH, t, USING INDEX i (a=?)); other
/// steps (`USE TEMP B-TREE FOR ORDER BY`) are all operation.
fn node(detail: &str) -> PlanNode {
    let detail = detail.trim();
    for verb in ["SCAN", "SEARCH"] {
        let Some(rest) = detail.strip_prefix(verb).and_then(|r| r.strip_prefix(' ')) else { continue };
        let rest = rest.strip_prefix("TABLE ").unwrap_or(rest);
        let cut = [" USING ", " VIA ", " ("].iter().filter_map(|m| rest.find(m)).min();
        let (object, tail) = match cut {
            Some(p) => (&rest[..p], rest[p..].trim()),
            None => (rest, ""),
        };
        return PlanNode {
            operation: verb.to_string(),
            object: (!object.is_empty()).then(|| object.to_string()),
            detail: (!tail.is_empty()).then(|| tail.to_string()),
            ..PlanNode::default()
        };
    }
    PlanNode { operation: detail.to_string(), ..PlanNode::default() }
}
