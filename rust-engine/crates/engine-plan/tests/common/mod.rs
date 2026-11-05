#![allow(dead_code)]

use engine_plan::{ExplainDialect, ExplainMode, Plan, PlanRows};

/// Fixture payload: the file without its leading `-- ` header lines.
pub fn fixture_payload(path: &str) -> String {
    let full = format!("{}/tests/fixtures/{path}", env!("CARGO_MANIFEST_DIR"));
    let text = std::fs::read_to_string(&full).unwrap_or_else(|e| panic!("{full}: {e}"));
    let mut rest = text.as_str();
    while let Some(line_end) = rest.find('\n').filter(|_| rest.starts_with("-- ")) {
        rest = &rest[line_end + 1..];
    }
    rest.to_string()
}

/// One-cell result set, as a driver returns EXPLAIN text.
pub fn cell(text: &str) -> PlanRows {
    PlanRows { columns: vec!["QUERY PLAN".into()], rows: vec![vec![Some(text.to_string())]] }
}

/// Rows of a SQLite fixture (`id\tparent\tnotused\tdetail`).
pub fn sqlite_rows(payload: &str) -> PlanRows {
    PlanRows {
        columns: ["id", "parent", "notused", "detail"].map(String::from).to_vec(),
        rows: payload
            .lines()
            .filter(|l| !l.is_empty())
            .map(|l| l.split('\t').map(|c| Some(c.to_string())).collect())
            .collect(),
    }
}

pub fn parse_fixture(dialect: ExplainDialect, mode: ExplainMode, path: &str) -> Plan {
    let payload = fixture_payload(path);
    let rows = if dialect == ExplainDialect::Sqlite { sqlite_rows(&payload) } else { cell(&payload) };
    Plan::parse(dialect, mode, &rows).unwrap_or_else(|e| panic!("{path}: {e}"))
}

/// `depth operation [object] [detail] total_cost self_cost est actual time loops` lines for eyeballing.
pub fn dump(plan: &Plan) -> String {
    let mut out = String::new();
    for n in &plan.nodes {
        out.push_str(&format!(
            "{}{} | {:?} | {:?} | cost {:?}/{:?} | est {:?} act {:?} t {:?} loops {:?} | props {}\n",
            "  ".repeat(n.depth as usize),
            n.operation,
            n.object,
            n.detail,
            n.total_cost,
            n.self_cost,
            n.est_rows,
            n.actual_rows,
            n.actual_time_ms,
            n.loops,
            n.props.len()
        ));
    }
    out.push_str(&format!(
        "total_cost {:?} time {:?} planning {:?} exec {:?}\nwarnings {:#?}\n",
        plan.total_cost, plan.total_time_ms, plan.planning_ms, plan.execution_ms, plan.warnings
    ));
    out
}
