//! Running the scenarios and reporting which rules a driver breaks.

use crate::fixture::SqlFixture;
use crate::harness::{Ctx, Failure};
use crate::scenarios::{ScenarioFn, SCENARIOS};
use std::fmt::Write;
use std::panic::{catch_unwind, AssertUnwindSafe};
use std::path::Path;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Outcome {
    Passed,
    Failed(String),
    Skipped(String),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ScenarioResult {
    pub name: &'static str,
    /// The rule(s) the scenario checks, for the report.
    pub rule: &'static str,
    pub outcome: Outcome,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ConformanceReport {
    pub driver: String,
    pub results: Vec<ScenarioResult>,
}

impl ConformanceReport {
    pub fn failed(&self) -> Vec<&ScenarioResult> {
        self.results
            .iter()
            .filter(|result| matches!(result.outcome, Outcome::Failed(_)))
            .collect()
    }

    pub fn passed(&self) -> usize {
        self.results
            .iter()
            .filter(|result| result.outcome == Outcome::Passed)
            .count()
    }

    pub fn is_ok(&self) -> bool {
        self.failed().is_empty()
    }

    /// A readable summary: one line per scenario, failures with their reason.
    pub fn summary(&self) -> String {
        let mut text = format!(
            "SQL conformance for {}: {} passed, {} failed, {} skipped\n",
            self.driver,
            self.passed(),
            self.failed().len(),
            self.results
                .iter()
                .filter(|r| matches!(r.outcome, Outcome::Skipped(_)))
                .count()
        );
        for result in &self.results {
            let _ = match &result.outcome {
                Outcome::Passed => writeln!(text, "  PASS  {}", result.name),
                Outcome::Skipped(why) => writeln!(text, "  SKIP  {}: {why}", result.name),
                Outcome::Failed(why) => {
                    writeln!(
                        text,
                        "  FAIL  {} [{}]\n        {why}",
                        result.name, result.rule
                    )
                }
            };
        }
        text
    }
}

fn run_one(
    ctx: &Ctx,
    name: &'static str,
    rule: &'static str,
    scenario: ScenarioFn,
) -> ScenarioResult {
    let outcome = match catch_unwind(AssertUnwindSafe(|| scenario(ctx))) {
        Ok(Ok(())) => Outcome::Passed,
        Ok(Err(Failure::Fail(message))) => Outcome::Failed(message),
        Ok(Err(Failure::Skip(message))) => Outcome::Skipped(message),
        Err(panic) => {
            let message = panic
                .downcast_ref::<&str>()
                .map(|m| (*m).to_owned())
                .or_else(|| panic.downcast_ref::<String>().cloned())
                .unwrap_or_else(|| "panicked".into());
            Outcome::Failed(format!("scenario panicked: {message}"))
        }
    };
    ScenarioResult {
        name,
        rule,
        outcome,
    }
}

/// Runs scenarios over an already loaded driver. `only` restricts the run to
/// the named scenarios (all when empty).
pub fn run_scenarios(ctx: &Ctx, only: &[&str]) -> ConformanceReport {
    let results = SCENARIOS
        .iter()
        .filter(|(name, _, _)| only.is_empty() || only.contains(name))
        .map(|(name, rule, scenario)| {
            eprintln!("conformance: {name} ...");
            run_one(ctx, name, rule, *scenario)
        })
        .collect();
    ConformanceReport {
        driver: ctx.fixture.label().to_owned(),
        results,
    }
}

/// Loads `dylib`, runs every scenario against `uri` and returns the report
/// without panicking. `TABLEX_CONFORMANCE_ONLY=a,b` limits the run.
pub fn run_sql_conformance_report(
    dylib: &Path,
    uri: &str,
    fixture: &dyn SqlFixture,
) -> ConformanceReport {
    let only = std::env::var("TABLEX_CONFORMANCE_ONLY").unwrap_or_default();
    let only: Vec<&str> = only
        .split(',')
        .map(str::trim)
        .filter(|name| !name.is_empty())
        .collect();
    match Ctx::load(dylib, uri, fixture) {
        Ok(ctx) => run_scenarios(&ctx, &only),
        Err(message) => ConformanceReport {
            driver: fixture.label().to_owned(),
            results: vec![ScenarioResult {
                name: "load",
                rule: "the driver dylib loads and connects",
                outcome: Outcome::Failed(message),
            }],
        },
    }
}

/// Runs the whole suite and **panics with the full report** if any scenario
/// fails. Use it from a driver's live test:
///
/// ```ignore
/// engine_driver_conformance::run_sql_conformance(&dylib, &uri, &PostgresFixture);
/// ```
pub fn run_sql_conformance(dylib: &Path, uri: &str, fixture: &dyn SqlFixture) {
    let report = run_sql_conformance_report(dylib, uri, fixture);
    eprintln!("{}", report.summary());
    assert!(report.is_ok(), "{}", report.summary());
}

#[cfg(test)]
mod tests {
    use super::*;

    fn result(name: &'static str, outcome: Outcome) -> ScenarioResult {
        ScenarioResult {
            name,
            rule: "rule",
            outcome,
        }
    }

    #[test]
    fn summary_lists_failures_with_reasons_and_counts() {
        let report = ConformanceReport {
            driver: "demo".into(),
            results: vec![
                result("a", Outcome::Passed),
                result("b", Outcome::Failed("broken".into())),
                result("c", Outcome::Skipped("n/a".into())),
            ],
        };
        assert!(!report.is_ok());
        assert_eq!(report.passed(), 1);
        assert_eq!(report.failed().len(), 1);
        let text = report.summary();
        assert!(text.contains("1 passed, 1 failed, 1 skipped"), "{text}");
        assert!(
            text.contains("FAIL  b [rule]") && text.contains("broken"),
            "{text}"
        );
        assert!(text.contains("SKIP  c: n/a"), "{text}");
    }

    #[test]
    fn scenario_names_are_unique() {
        let mut names: Vec<_> = SCENARIOS.iter().map(|(name, _, _)| *name).collect();
        let total = names.len();
        names.sort_unstable();
        names.dedup();
        assert_eq!(names.len(), total);
    }
}
