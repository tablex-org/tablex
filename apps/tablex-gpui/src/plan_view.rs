//! Model of the Explain plan viewer: a parsed [`Plan`] (nodes in pre-order, each with its depth)
//! plus the view state around it, which is only a set of collapsed node indices, the selected
//! node and a list of the node indices on screen. Nothing here is per node, so a 10 000 node plan
//! costs one `Vec<u32>` and the renderer touches only the rows in the viewport.

use engine_plan::{ExplainDialect, ExplainMode, Plan, PlanNode};
use std::collections::HashSet;
use std::sync::atomic::{AtomicU64, Ordering};

/// Which optional columns the plan has data for, decided once per plan.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PlanColumns {
    pub cost: bool,
    pub actual_rows: bool,
    pub time: bool,
    pub loops: bool,
}

#[derive(Debug)]
pub struct PlanView {
    /// Distinguishes one plan from the next (the Raw editor refills when it changes).
    pub id: u64,
    pub dialect: ExplainDialect,
    pub mode: ExplainMode,
    /// The EXPLAIN statement that produced the plan, as sent to the server.
    pub sql: String,
    pub plan: Plan,
    pub selected: Option<usize>,
    /// Show `Plan::raw` (read-only) instead of the node list.
    pub show_raw: bool,
    pub columns: PlanColumns,
    /// Cost the bars are relative to (the plan's total, else the largest node total).
    cost_base: f64,
    collapsed: HashSet<usize>,
    /// Indices of the nodes on screen, ascending: every node except those under a collapsed one.
    visible: Vec<u32>,
}

impl PlanView {
    pub fn new(plan: Plan, dialect: ExplainDialect, mode: ExplainMode, sql: String) -> Self {
        static NEXT_ID: AtomicU64 = AtomicU64::new(1);
        let nodes = &plan.nodes;
        let largest = nodes
            .iter()
            .filter_map(|node| node.total_cost)
            .fold(0.0f64, f64::max);
        let cost_base = plan.total_cost.filter(|total| *total > 0.0).unwrap_or(largest);
        let columns = PlanColumns {
            cost: cost_base > 0.0
                && nodes
                    .iter()
                    .any(|node| node.total_cost.is_some() || node.self_cost.is_some()),
            actual_rows: nodes.iter().any(|node| node.actual_rows.is_some()),
            time: nodes.iter().any(|node| node.actual_time_ms.is_some()),
            loops: nodes.iter().any(|node| node.loops.is_some()),
        };
        let mut view = Self {
            id: NEXT_ID.fetch_add(1, Ordering::Relaxed),
            dialect,
            mode,
            sql,
            plan,
            selected: None,
            show_raw: false,
            columns,
            cost_base,
            collapsed: HashSet::new(),
            visible: Vec::new(),
        };
        view.rebuild_visible();
        view
    }

    pub fn node(&self, index: usize) -> Option<&PlanNode> {
        self.plan.nodes.get(index)
    }

    /// Number of rows on screen.
    pub fn visible_len(&self) -> usize {
        self.visible.len()
    }

    /// The node shown at `position` (a row of the list).
    pub fn visible_node(&self, position: usize) -> Option<usize> {
        self.visible.get(position).map(|index| *index as usize)
    }

    /// The row a node is shown at, `None` while it is hidden under a collapsed node.
    pub fn position_of(&self, node: usize) -> Option<usize> {
        self.visible.binary_search(&(node as u32)).ok()
    }

    /// Nodes are in pre-order, so a node has children exactly when the next one is deeper.
    pub fn has_children(&self, node: usize) -> bool {
        match (self.plan.nodes.get(node), self.plan.nodes.get(node + 1)) {
            (Some(this), Some(next)) => next.depth > this.depth,
            _ => false,
        }
    }

    pub fn is_collapsed(&self, node: usize) -> bool {
        self.collapsed.contains(&node)
    }

    #[cfg(test)]
    pub fn collapsed_count(&self) -> usize {
        self.collapsed.len()
    }

    /// Collapses or expands the subtree of `node`. `false` for a leaf. Collapsing a node that
    /// hides the selection moves the selection to the node.
    pub fn toggle(&mut self, node: usize) -> bool {
        if !self.has_children(node) {
            return false;
        }
        if !self.collapsed.remove(&node) {
            self.collapsed.insert(node);
            if self.selected.is_some_and(|selected| selected > node && self.is_under(selected, node)) {
                self.selected = Some(node);
            }
        }
        self.rebuild_visible();
        true
    }

    /// Whether `node` lies in the subtree of `ancestor` (`ancestor < node`).
    fn is_under(&self, node: usize, ancestor: usize) -> bool {
        let depth = self.plan.nodes[ancestor].depth;
        self.plan.nodes[ancestor + 1..=node]
            .iter()
            .all(|between| between.depth > depth)
    }

    pub fn select(&mut self, node: usize) {
        if node < self.plan.nodes.len() && self.position_of(node).is_some() {
            self.selected = Some(node);
        }
    }

    /// The node `delta` rows from the selection (the first row when nothing is selected).
    pub fn step_target(&self, delta: isize) -> Option<usize> {
        if self.visible.is_empty() {
            return None;
        }
        let from = self
            .selected
            .and_then(|node| self.position_of(node))
            .map(|position| position as isize);
        let target = match from {
            Some(from) => (from + delta).clamp(0, self.visible.len() as isize - 1),
            None => 0,
        };
        self.visible_node(target as usize)
    }

    fn rebuild_visible(&mut self) {
        let nodes = &self.plan.nodes;
        self.visible.clear();
        self.visible.reserve(nodes.len());
        let mut index = 0;
        while index < nodes.len() {
            self.visible.push(index as u32);
            let depth = nodes[index].depth;
            index += 1;
            if self.collapsed.contains(&(index - 1)) {
                while index < nodes.len() && nodes[index].depth > depth {
                    index += 1;
                }
            }
        }
    }

    /// `(own share, cumulative share)` of the plan's cost, each within 0..=1. `None` when the plan
    /// has no costs.
    pub fn cost_shares(&self, node: &PlanNode) -> Option<(f32, f32)> {
        if !self.columns.cost {
            return None;
        }
        let share = |cost: Option<f64>| (cost.unwrap_or(0.0) / self.cost_base).clamp(0.0, 1.0) as f32;
        let own = share(node.self_cost);
        Some((own, share(node.total_cost).max(own)))
    }

    /// Whether the engine flagged the node's row estimate (off by more than 10x; an over-estimate
    /// under a `Limit` is expected and not flagged). The same nodes the plan's warnings name.
    pub fn is_misestimated(node: &PlanNode) -> bool {
        node.misestimate.is_some()
    }

    /// Every field of a node for the detail pane: the typed fields that are set, then the
    /// server's own `props`.
    pub fn detail_fields(node: &PlanNode) -> Vec<(String, String)> {
        let mut fields = vec![("Operation".to_string(), node.operation.clone())];
        let mut add = |label: &str, value: Option<String>| {
            if let Some(value) = value {
                fields.push((label.to_string(), value));
            }
        };
        add("Object", node.object.clone());
        add("Detail", node.detail.clone());
        add("Startup cost", node.startup_cost.map(fmt_cost));
        add("Total cost", node.total_cost.map(fmt_cost));
        add("Own cost", node.self_cost.map(fmt_cost));
        add("Est. rows", node.est_rows.map(fmt_rows));
        add(
            "Estimate",
            node.misestimate.map(|miss| {
                let direction = if miss.under { "too low" } else { "too high" };
                format!("off by {}x, {direction}", fmt_rows(miss.factor.round()))
            }),
        );
        add("Actual rows", node.actual_rows.map(fmt_rows));
        add("Time (ms)", node.actual_time_ms.map(fmt_ms));
        add("Loops", node.loops.map(|loops| fmt_rows(loops as f64)));
        fields.extend(node.props.iter().cloned());
        fields
    }
}

/// `1234567` -> `1,234,567`; fractions keep up to two digits.
pub fn fmt_rows(value: f64) -> String {
    if !value.is_finite() {
        return "-".to_string();
    }
    let rounded = (value * 100.0).round() / 100.0;
    let text = if rounded.fract() == 0.0 {
        format!("{rounded:.0}")
    } else {
        format!("{rounded:.2}")
    };
    let (integer, fraction) = match text.split_once('.') {
        Some((integer, fraction)) => (integer, Some(fraction)),
        None => (text.as_str(), None),
    };
    let (sign, digits) = integer
        .strip_prefix('-')
        .map_or(("", integer), |digits| ("-", digits));
    let mut grouped = String::with_capacity(text.len() + digits.len() / 3);
    grouped.push_str(sign);
    for (index, digit) in digits.chars().enumerate() {
        if index > 0 && (digits.len() - index) % 3 == 0 {
            grouped.push(',');
        }
        grouped.push(digit);
    }
    if let Some(fraction) = fraction {
        grouped.push('.');
        grouped.push_str(fraction);
    }
    grouped
}

/// Milliseconds with a precision that fits their size.
pub fn fmt_ms(value: f64) -> String {
    if !value.is_finite() {
        "-".to_string()
    } else if value >= 100.0 {
        fmt_rows(value.round())
    } else if value >= 1.0 {
        format!("{value:.1}")
    } else {
        format!("{value:.3}")
    }
}

/// Server cost units: two decimals, grouped.
pub fn fmt_cost(value: f64) -> String {
    fmt_rows(value)
}

#[cfg(test)]
pub(crate) mod fixtures {
    use super::*;
    use engine_plan::PlanRows;

    /// The text of an `engine-plan` fixture without its `-- ` header lines.
    pub(crate) fn payload(path: &str) -> String {
        let full = format!(
            "{}/../../rust-engine/crates/engine-plan/tests/fixtures/{path}",
            env!("CARGO_MANIFEST_DIR")
        );
        let text = std::fs::read_to_string(&full).unwrap_or_else(|error| panic!("{full}: {error}"));
        text.lines()
            .skip_while(|line| line.starts_with("-- "))
            .collect::<Vec<_>>()
            .join("\n")
    }

    /// The result set a driver returns for the fixture's statement.
    pub(crate) fn rows(dialect: ExplainDialect, path: &str) -> PlanRows {
        let payload = payload(path);
        if dialect == ExplainDialect::Sqlite {
            PlanRows {
                columns: ["id", "parent", "notused", "detail"].map(String::from).to_vec(),
                rows: payload
                    .lines()
                    .filter(|line| !line.is_empty())
                    .map(|line| line.split('\t').map(|cell| Some(cell.to_string())).collect())
                    .collect(),
            }
        } else {
            PlanRows {
                columns: vec!["QUERY PLAN".into()],
                rows: vec![vec![Some(payload)]],
            }
        }
    }

    pub(crate) fn plan(dialect: ExplainDialect, mode: ExplainMode, path: &str) -> Plan {
        Plan::parse(dialect, mode, &rows(dialect, path)).unwrap_or_else(|error| panic!("{path}: {error}"))
    }

    pub(crate) fn view(dialect: ExplainDialect, mode: ExplainMode, path: &str) -> PlanView {
        PlanView::new(plan(dialect, mode, path), dialect, mode, String::new())
    }

    /// `count` nodes in pre-order whose depth cycles 0..24 (cheap synthetic input for size
    /// tests, not a real plan shape).
    pub(crate) fn synthetic(count: usize) -> Plan {
        let mut nodes: Vec<PlanNode> = Vec::with_capacity(count);
        for index in 0..count {
            let previous = nodes.last().map_or(0, |node| node.depth);
            let depth = if index == 0 { 0 } else { ((index % 24) as u16).clamp(1, previous + 1) };
            nodes.push(PlanNode {
                depth,
                operation: format!("Seq Scan {index}"),
                object: Some(format!("t{index}")),
                total_cost: Some(1.0 + (index % 97) as f64),
                self_cost: Some((index % 13) as f64),
                est_rows: Some(100.0),
                actual_rows: Some(if index % 7 == 0 { 5000.0 } else { 100.0 }),
                actual_time_ms: Some(0.5),
                loops: Some(1),
                ..PlanNode::default()
            });
        }
        Plan {
            total_cost: Some(100.0),
            nodes,
            ..Plan::default()
        }
    }
}

#[cfg(test)]
mod tests {
    use super::fixtures::*;
    use super::*;

    #[test]
    fn visible_rows_follow_the_collapsed_set() {
        let mut view = view(ExplainDialect::Postgres, ExplainMode::Analyze, "postgres/join_analyze.json");
        let total = view.plan.nodes.len();
        assert!(total > 3, "the fixture has a tree");
        assert_eq!(view.visible_len(), total);
        assert!(view.has_children(0));
        assert!(view.toggle(0));
        assert_eq!(view.visible_len(), 1, "collapsing the root hides every other node");
        assert!(view.is_collapsed(0));
        assert!(view.toggle(0));
        assert_eq!(view.visible_len(), total);
        let leaf = (0..total).find(|node| !view.has_children(*node)).unwrap();
        assert!(!view.toggle(leaf), "a leaf has nothing to collapse");
        assert_eq!(view.collapsed_count(), 0);
    }

    #[test]
    fn collapsing_above_the_selection_selects_the_collapsed_node() {
        let mut view = view(ExplainDialect::Postgres, ExplainMode::Analyze, "postgres/join_analyze.json");
        let leaf = (0..view.plan.nodes.len()).rfind(|node| !view.has_children(*node)).unwrap();
        view.select(leaf);
        assert_eq!(view.selected, Some(leaf));
        view.toggle(0);
        assert_eq!(view.selected, Some(0));
        assert_eq!(view.position_of(leaf), None);
        view.select(leaf);
        assert_eq!(view.selected, Some(0), "a hidden node cannot be selected");
    }

    #[test]
    fn a_sibling_after_a_collapsed_subtree_stays_visible() {
        let mut view = view(ExplainDialect::Postgres, ExplainMode::Estimate, "postgres/cte_estimate.json");
        let nodes = &view.plan.nodes;
        // A node with children that has a later node at its own depth or shallower.
        let (collapsible, after) = (0..nodes.len())
            .filter(|node| view.has_children(*node))
            .find_map(|node| {
                let depth = nodes[node].depth;
                let after = (node + 1..nodes.len()).find(|later| nodes[*later].depth <= depth)?;
                Some((node, after))
            })
            .expect("the cte fixture has a subtree followed by a sibling");
        let before = view.visible_len();
        view.toggle(collapsible);
        assert!(view.position_of(collapsible + 1).is_none());
        assert!(view.position_of(after).is_some());
        assert!(view.visible_len() < before);
    }

    #[test]
    fn stepping_clamps_at_both_ends() {
        let mut view = view(ExplainDialect::Postgres, ExplainMode::Estimate, "postgres/join_estimate.json");
        assert_eq!(view.step_target(1), Some(0), "nothing selected: the first row");
        view.select(0);
        assert_eq!(view.step_target(-1), Some(0));
        assert_eq!(view.step_target(1), Some(1));
        let last = view.plan.nodes.len() - 1;
        view.select(last);
        assert_eq!(view.step_target(1), Some(last));
    }

    #[test]
    fn cost_shares_are_relative_to_the_plan_total() {
        let view = view(ExplainDialect::Postgres, ExplainMode::Analyze, "postgres/join_analyze.json");
        assert!(view.columns.cost && view.columns.actual_rows && view.columns.time && view.columns.loops);
        let (own, cumulative) = view.cost_shares(&view.plan.nodes[0]).unwrap();
        assert!((cumulative - 1.0).abs() < 1e-6, "the root is the whole plan: {cumulative}");
        assert!(own <= cumulative);
        for node in &view.plan.nodes {
            let (own, cumulative) = view.cost_shares(node).unwrap();
            assert!((0.0..=1.0).contains(&own) && (0.0..=1.0).contains(&cumulative));
        }
    }

    #[test]
    fn dialects_without_costs_or_actuals_hide_those_columns() {
        let sqlite = view(ExplainDialect::Sqlite, ExplainMode::Estimate, "sqlite/join_estimate.tsv");
        assert_eq!(
            sqlite.columns,
            PlanColumns { cost: false, actual_rows: false, time: false, loops: false }
        );
        assert!(sqlite.cost_shares(&sqlite.plan.nodes[0]).is_none());
        let mysql = view(ExplainDialect::MySql, ExplainMode::Estimate, "mysql/join_estimate.json");
        assert!(mysql.columns.cost && !mysql.columns.actual_rows && !mysql.columns.time);
        let postgres = view(ExplainDialect::Postgres, ExplainMode::Estimate, "postgres/join_estimate.json");
        assert!(postgres.columns.cost && !postgres.columns.actual_rows);
    }

    #[test]
    fn the_misestimate_flag_is_the_engines() {
        use engine_plan::Misestimate;
        let flagged = PlanNode {
            misestimate: Some(Misestimate { factor: 83.0, under: false }),
            ..PlanNode::default()
        };
        assert!(PlanView::is_misestimated(&flagged));
        assert!(!PlanView::is_misestimated(&PlanNode::default()));
        let fields = PlanView::detail_fields(&flagged);
        assert!(fields.contains(&("Estimate".to_string(), "off by 83x, too high".to_string())), "{fields:?}");
        // The flagged nodes of a real plan are the ones its warnings talk about.
        let view = view(ExplainDialect::Postgres, ExplainMode::Analyze, "postgres/cte_analyze.json");
        let flagged = view.plan.nodes.iter().filter(|node| PlanView::is_misestimated(node)).count();
        assert_eq!(flagged > 0, !view.plan.warnings.is_empty());
    }

    #[test]
    fn detail_fields_list_typed_fields_then_props() {
        let view = view(ExplainDialect::Postgres, ExplainMode::Analyze, "postgres/join_analyze.json");
        let node = &view.plan.nodes[0];
        let fields = PlanView::detail_fields(node);
        assert_eq!(fields[0].0, "Operation");
        assert!(fields.iter().any(|(label, _)| label == "Total cost"));
        assert!(fields.iter().any(|(label, _)| label == "Actual rows"));
        let props = &fields[fields.len() - node.props.len()..];
        assert_eq!(props, node.props.as_slice());
        assert!(!node.props.is_empty());
    }

    #[test]
    fn numbers_are_grouped_and_sized() {
        assert_eq!(fmt_rows(0.0), "0");
        assert_eq!(fmt_rows(1234567.0), "1,234,567");
        assert_eq!(fmt_rows(999.0), "999");
        assert_eq!(fmt_rows(1000.0), "1,000");
        assert_eq!(fmt_rows(12.5), "12.50");
        assert_eq!(fmt_rows(-4321.0), "-4,321");
        assert_eq!(fmt_rows(f64::NAN), "-");
        assert_eq!(fmt_ms(0.0123), "0.012");
        assert_eq!(fmt_ms(12.34), "12.3");
        assert_eq!(fmt_ms(12345.6), "12,346");
    }

    #[test]
    fn a_large_plan_builds_and_toggles_in_linear_time() {
        let started = std::time::Instant::now();
        let mut view = PlanView::new(synthetic(50_000), ExplainDialect::Postgres, ExplainMode::Analyze, String::new());
        assert_eq!(view.visible_len(), 50_000);
        view.toggle(0);
        assert_eq!(view.visible_len(), 1);
        view.toggle(0);
        assert_eq!(view.visible_len(), 50_000);
        assert!(started.elapsed() < std::time::Duration::from_secs(2), "{:?}", started.elapsed());
    }
}
