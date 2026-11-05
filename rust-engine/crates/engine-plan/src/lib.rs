//! Per-dialect `EXPLAIN` support for TableX: build the statement to run
//! ([`explain_sql`]) and turn the result set into a flat plan tree
//! ([`Plan::parse`]). Pure computation: no I/O, no async, nothing here talks to a
//! server.
//!
//! ```
//! use engine_plan::{explain_sql, ExplainDialect, ExplainMode};
//!
//! let lex = ExplainDialect::Postgres.default_lex_options();
//! let stmt = explain_sql(ExplainDialect::Postgres, ExplainMode::Estimate, "select 1;", true, &lex).unwrap();
//! assert_eq!(stmt.sql, "EXPLAIN (FORMAT JSON) select 1");
//! ```
//!
//! # Plan shape
//!
//! A [`Plan`] is a pre-order list of [`PlanNode`]s; `parent` and `depth` let a UI
//! draw an indented flat list without building tree objects. Numbers keep the
//! meaning the server gives them, with these conventions:
//!
//! * `est_rows` and `actual_rows` are **per loop** (as PostgreSQL, MySQL and
//!   MariaDB report them), so they compare directly.
//! * `actual_time_ms` is the **total** time spent in the node over all loops
//!   (per-loop time x `loops`; MariaDB already reports totals).
//! * costs are in the server's own unit and are not comparable across dialects.
//!
//! Nothing here panics on odd input; oversized or malformed output yields an
//! [`ExplainError`]. Limits: [`MAX_RAW_BYTES`], [`MAX_NODES`].

mod builder;
mod error;
mod parse;
mod plan;
mod value;

pub use builder::explain_sql;
pub use engine_driver_kit::sql::SqlLexOptions;
pub use error::ExplainError;
pub use plan::{Misestimate, Plan, PlanNode, PlanRows};

/// Largest accepted explain output (all cells together), in bytes.
pub const MAX_RAW_BYTES: usize = 16 * 1024 * 1024;
/// Largest accepted plan, in nodes.
pub const MAX_NODES: usize = 50_000;

/// Database dialects that have an explain implementation.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum ExplainDialect {
    Postgres,
    MySql,
    MariaDb,
    Sqlite,
    ClickHouse,
}

impl ExplainDialect {
    /// Human readable name.
    pub fn name(self) -> &'static str {
        match self {
            Self::Postgres => "PostgreSQL",
            Self::MySql => "MySQL",
            Self::MariaDb => "MariaDB",
            Self::Sqlite => "SQLite",
            Self::ClickHouse => "ClickHouse",
        }
    }

    /// The lexer preset of this dialect (the session's `sql_mode` /
    /// `standard_conforming_strings` may need `without_backslash_escapes()` /
    /// `with_ansi_quotes()` on top).
    pub fn default_lex_options(self) -> SqlLexOptions {
        match self {
            Self::Postgres => SqlLexOptions::postgres(),
            Self::MySql => SqlLexOptions::mysql(),
            Self::MariaDb => SqlLexOptions::mariadb(),
            Self::Sqlite => SqlLexOptions::sqlite(),
            Self::ClickHouse => SqlLexOptions::clickhouse(),
        }
    }

    /// Whether `ExplainMode::Analyze` (execute and measure) exists for this dialect.
    pub fn supports_analyze(self) -> bool {
        matches!(self, Self::Postgres | Self::MySql | Self::MariaDb)
    }
}

/// Estimate only (the statement is not executed) or Analyze (it is executed).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum ExplainMode {
    Estimate,
    Analyze,
}

/// How the server returns the plan, which tells the caller how to fill [`PlanRows`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum PlanFormat {
    /// One JSON document in the first column (rows are concatenated with `\n`).
    Json,
    /// Indented `-> ...` text (MySQL `EXPLAIN ANALYZE`) in the first column.
    Tree,
    /// One result row per plan step (SQLite: `id`, `parent`, `notused`, `detail`).
    Rows,
}

/// The statement to send to the server and the format of what comes back.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ExplainStatement {
    pub sql: String,
    pub format: PlanFormat,
}
