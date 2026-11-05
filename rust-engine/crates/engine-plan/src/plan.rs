use crate::parse;
use crate::value::{fmt_count, join_nonempty};
use crate::{ExplainDialect, ExplainError, ExplainMode, MAX_NODES, MAX_RAW_BYTES};

/// The text of a result set, exactly as the driver returned the cells.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct PlanRows {
    pub columns: Vec<String>,
    pub rows: Vec<Vec<Option<String>>>,
}

/// One operation of a plan.
///
/// `est_rows` / `actual_rows` are per loop, `actual_time_ms` is the total over
/// all loops, see the crate docs.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct PlanNode {
    /// Index of the parent in [`Plan::nodes`]; `None` for roots.
    pub parent: Option<usize>,
    /// Nesting level (0 for roots, saturates at `u16::MAX`).
    pub depth: u16,
    pub operation: String,
    /// What the operation works on (table, CTE, function, ...).
    pub object: Option<String>,
    /// Short human readable remainder (index used, condition, ...).
    pub detail: Option<String>,
    pub startup_cost: Option<f64>,
    pub total_cost: Option<f64>,
    /// Cost of this node alone. Cumulative-cost dialects (PostgreSQL, MySQL
    /// `EXPLAIN ANALYZE`): total minus the children's totals, at least 0.
    /// MySQL / MariaDB JSON: the table's own cost. `None` when unknown.
    pub self_cost: Option<f64>,
    pub est_rows: Option<f64>,
    pub actual_rows: Option<f64>,
    pub actual_time_ms: Option<f64>,
    pub loops: Option<u64>,
    /// Everything else the server said about the node, stringified, in
    /// document order.
    pub props: Vec<(String, String)>,
    /// Set when `est_rows` and `actual_rows` differ by more than 10x; exactly the
    /// nodes the `warnings` talk about (except the "and N more" overflow line,
    /// which counts nodes flagged here too). Not set for an over-estimate below a
    /// `Limit` node, where stopping early is expected.
    pub misestimate: Option<Misestimate>,
}

/// How far a node's row estimate was off.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Misestimate {
    /// Ratio between the larger and the smaller of estimate and actual, both
    /// floored at 1 row; always above 10.
    pub factor: f64,
    /// `true` when the planner expected fewer rows than it got (`actual > est`).
    pub under: bool,
}

/// A parsed plan: nodes in pre-order.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Plan {
    pub nodes: Vec<PlanNode>,
    /// Cost of the root node (server units).
    pub total_cost: Option<f64>,
    /// Planning + execution time when the server reports them.
    pub total_time_ms: Option<f64>,
    pub planning_ms: Option<f64>,
    pub execution_ms: Option<f64>,
    /// The output as received (concatenated cells; SQLite: tab separated rows).
    pub raw: String,
    /// Short factual notes, e.g. a row estimate off by more than 10x.
    pub warnings: Vec<String>,
}

/// What a dialect parser hands back before the common post-processing.
#[derive(Debug, Default)]
pub(crate) struct Parsed {
    pub nodes: Vec<PlanNode>,
    pub planning_ms: Option<f64>,
    pub execution_ms: Option<f64>,
    pub warnings: Vec<String>,
    /// Costs are cumulative (a node's total includes its children): derive
    /// `self_cost` as total minus children.
    pub cumulative_cost: bool,
}

/// Longest `operation` / `object`, in characters; longer text is cut with `…`.
pub(crate) const NAME_CAP: usize = 512;
/// Longest `detail`, in characters. The full text stays in `props` / `raw`.
pub(crate) const DETAIL_CAP: usize = 2048;

impl PlanNode {
    /// Enforce the display-text caps (`props` have their own cap).
    pub(crate) fn cap_text(&mut self) {
        let operation = std::mem::take(&mut self.operation);
        self.operation = cap_chars(operation, NAME_CAP);
        self.object = self.object.take().map(|o| cap_chars(o, NAME_CAP));
        self.detail = self.detail.take().map(|d| cap_chars(d, DETAIL_CAP));
    }
}

/// Truncate to `max` characters (on a char boundary) and add `…` when cut.
pub(crate) fn cap_chars(mut s: String, max: usize) -> String {
    if let Some((idx, _)) = s.char_indices().nth(max) {
        s.truncate(idx);
        s.push('…');
    }
    s
}

/// Pre-order node collector with the node cap built in.
#[derive(Debug, Default)]
pub(crate) struct Tree {
    pub nodes: Vec<PlanNode>,
}

impl Tree {
    pub fn push(&mut self, parent: Option<usize>, mut node: PlanNode) -> Result<usize, ExplainError> {
        if self.nodes.len() >= MAX_NODES {
            return Err(ExplainError::TooManyNodes { limit: MAX_NODES });
        }
        node.cap_text();
        node.parent = parent;
        node.depth = parent.map_or(0, |p| self.nodes[p].depth.saturating_add(1));
        self.nodes.push(node);
        Ok(self.nodes.len() - 1)
    }
}

impl PlanRows {
    /// Bytes of all cells, for the size cap.
    fn byte_len(&self) -> usize {
        self.rows.iter().flatten().flatten().map(String::len).sum()
    }

    /// First column of every row joined by `\n` (JSON / tree text; some drivers
    /// split long text over several rows).
    fn first_column_text(&self) -> String {
        let cells: Vec<&str> = self.rows.iter().filter_map(|r| r.first()?.as_deref()).collect();
        cells.join("\n")
    }
}

impl Plan {
    /// Parse the result of the statement [`crate::explain_sql`] built for `dialect`
    /// / `mode`.
    ///
    /// Never panics. Fails with a typed error on empty, oversized
    /// ([`MAX_RAW_BYTES`]), malformed or too large ([`MAX_NODES`]) output.
    pub fn parse(dialect: ExplainDialect, mode: ExplainMode, rows: &PlanRows) -> Result<Plan, ExplainError> {
        if mode == ExplainMode::Analyze && !dialect.supports_analyze() {
            return Err(ExplainError::AnalyzeUnsupported(dialect));
        }
        let bytes = rows.byte_len();
        if bytes > MAX_RAW_BYTES {
            return Err(ExplainError::OutputTooLarge { bytes, limit: MAX_RAW_BYTES });
        }

        let (raw, parsed) = if dialect == ExplainDialect::Sqlite {
            parse::sqlite::parse(rows)?
        } else {
            let raw = rows.first_column_text();
            if raw.trim().is_empty() {
                return Err(ExplainError::EmptyOutput);
            }
            let parsed = match dialect {
                ExplainDialect::Postgres => parse::postgres::parse(&raw)?,
                ExplainDialect::MySql => parse::mysql::parse(&raw, true)?,
                ExplainDialect::MariaDb => parse::mysql::parse(&raw, false)?,
                ExplainDialect::ClickHouse => parse::clickhouse::parse(&raw)?,
                ExplainDialect::Sqlite => unreachable!("handled above"),
            };
            (raw, parsed)
        };
        Ok(Self::finish(mode, raw, parsed))
    }

    fn finish(mode: ExplainMode, raw: String, parsed: Parsed) -> Plan {
        let Parsed { mut nodes, planning_ms, execution_ms, mut warnings, cumulative_cost } = parsed;
        if cumulative_cost {
            derive_self_costs(&mut nodes);
        }
        if mode == ExplainMode::Analyze
            && !nodes.is_empty()
            && nodes.iter().all(|n| n.actual_rows.is_none() && n.actual_time_ms.is_none())
        {
            warnings.push("analyze was requested but the output has no actual statistics".to_string());
        }
        warnings.extend(flag_misestimates(&mut nodes));
        let total_time_ms = match (planning_ms, execution_ms) {
            (None, None) => None,
            (p, e) => Some(p.unwrap_or(0.0) + e.unwrap_or(0.0)),
        };
        Plan {
            total_cost: nodes.first().and_then(|n| n.total_cost),
            nodes,
            total_time_ms,
            planning_ms,
            execution_ms,
            raw,
            warnings,
        }
    }
}

/// `self = total - sum(children totals)`, floored at 0. Children always come
/// after their parent in pre-order, so one reverse pass is enough (no recursion).
fn derive_self_costs(nodes: &mut [PlanNode]) {
    let mut children = vec![0.0f64; nodes.len()];
    for i in (0..nodes.len()).rev() {
        let total = nodes[i].total_cost;
        nodes[i].self_cost = total.map(|t| (t - children[i]).max(0.0));
        if let (Some(p), Some(t)) = (nodes[i].parent, total) {
            children[p] += t;
        }
    }
}

/// Flag nodes whose row estimate is off by more than 10x and describe the first
/// few (at most `SHOWN` lines, then a count). An over-estimate below a `Limit`
/// node is skipped: the limit stops the scan early by design.
fn flag_misestimates(nodes: &mut [PlanNode]) -> Vec<String> {
    const SHOWN: usize = 5;
    // Parents precede children, so one forward pass finds every Limit ancestor.
    let mut below_limit = vec![false; nodes.len()];
    let mut lines = Vec::new();
    let mut flagged = 0usize;
    for i in 0..nodes.len() {
        if let Some(p) = nodes[i].parent {
            below_limit[i] = below_limit[p] || nodes[p].operation.starts_with("Limit");
        }
        let n = &mut nodes[i];
        let (Some(est), Some(act)) = (n.est_rows, n.actual_rows) else { continue };
        if n.loops == Some(0) {
            continue;
        }
        let (e, a) = (est.max(1.0), act.max(1.0));
        let (under, factor) = if a >= e { (true, a / e) } else { (false, e / a) };
        if factor <= 10.0 || (!under && below_limit[i]) {
            continue;
        }
        n.misestimate = Some(Misestimate { factor, under });
        flagged += 1;
        if lines.len() < SHOWN {
            let label = join_nonempty([n.operation.clone(), n.object.clone().unwrap_or_default()], " on ")
                .unwrap_or_default();
            let per_loop = if n.loops.is_some_and(|l| l > 1) { " per loop" } else { "" };
            lines.push(format!("{label}: rows estimated {}, actual {}{per_loop}", fmt_count(est), fmt_count(act)));
        }
    }
    if flagged > SHOWN {
        lines.push(format!("and {} more nodes with row estimates off by more than 10x", flagged - SHOWN));
    }
    lines
}
