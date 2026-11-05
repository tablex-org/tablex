use thiserror::Error;

use crate::ExplainDialect;

/// Everything `explain_sql` and `Plan::parse` can refuse or fail with.
#[derive(Debug, Clone, PartialEq, Eq, Error)]
pub enum ExplainError {
    /// The statement holds nothing but whitespace, comments and `;`.
    #[error("the statement is empty")]
    EmptyStatement,
    /// More than one statement was given (a `;` followed by more SQL).
    #[error("only a single statement can be explained")]
    MultipleStatements,
    /// The statement already starts with `EXPLAIN` / `ANALYZE` / `DESCRIBE` / `DESC`.
    #[error("the statement already starts with {0}")]
    AlreadyExplain(String),
    /// `ExplainMode::Analyze` was requested for a dialect that cannot do it.
    #[error("{} has no EXPLAIN ANALYZE", .0.name())]
    AnalyzeUnsupported(ExplainDialect),
    /// `ExplainMode::Analyze` executes the statement, so it needs a read-only one.
    #[error("EXPLAIN ANALYZE executes the statement; only read-only statements are allowed")]
    AnalyzeRequiresReadOnly,
    /// The result set carried no plan text at all.
    #[error("the explain output is empty")]
    EmptyOutput,
    /// The result set is larger than `MAX_RAW_BYTES`.
    #[error("the explain output is {bytes} bytes, the limit is {limit}")]
    OutputTooLarge { bytes: usize, limit: usize },
    /// The plan has more than `MAX_NODES` nodes.
    #[error("the plan has more than {limit} nodes")]
    TooManyNodes { limit: usize },
    /// The output should be JSON but is not (also: JSON nested deeper than 128 levels).
    #[error("the explain output is not valid JSON: {0}")]
    InvalidJson(String),
    /// Valid text, but not the structure this dialect's EXPLAIN produces.
    #[error("unexpected explain output: {0}")]
    UnexpectedShape(&'static str),
    /// A column the parser needs is missing from the result set.
    #[error("the explain output has no `{0}` column")]
    MissingColumn(&'static str),
    /// One result row of a row-based plan (SQLite) is unusable. `row` is 0-based.
    #[error("explain row {row}: {reason}")]
    InvalidRow { row: usize, reason: &'static str },
    /// Parent ids of a row-based plan form a cycle.
    #[error("the plan rows reference each other in a cycle")]
    CyclicPlan,
}
