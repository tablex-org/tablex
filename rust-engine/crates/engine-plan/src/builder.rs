use engine_driver_kit::sql::{classify, Keyword, SqlLexOptions};

use crate::{ExplainDialect, ExplainError, ExplainMode, ExplainStatement, PlanFormat};

/// Wrap `statement` in the dialect's EXPLAIN syntax.
///
/// The text is lexed with the driver kit's classifier using `lex`
/// ([`ExplainDialect::default_lex_options`], adjusted for the session's `sql_mode`),
/// so a `;` inside a literal, comment, dollar quote or MySQL executable comment
/// (`/*! ... */`) is seen exactly as the server sees it; anything but a single
/// statement is refused. Surrounding whitespace and a trailing `;` are dropped;
/// comments inside the statement's span are kept.
///
/// `read_only` is the caller's verdict on the statement: `Analyze` executes it, so
/// it is refused unless the caller says it is read-only.
///
/// | dialect | `Estimate` | `Analyze` |
/// |---|---|---|
/// | PostgreSQL | `EXPLAIN (FORMAT JSON) s` | `EXPLAIN (ANALYZE, BUFFERS, FORMAT JSON) s` |
/// | MySQL | `EXPLAIN FORMAT=JSON s` | `EXPLAIN ANALYZE s` (tree text, 8.0.18+) |
/// | MariaDB | `EXPLAIN FORMAT=JSON s` | `ANALYZE FORMAT=JSON s` |
/// | SQLite | `EXPLAIN QUERY PLAN s` | refused |
/// | ClickHouse | `EXPLAIN json = 1, actions = 1, indexes = 1 s` | refused |
pub fn explain_sql(
    dialect: ExplainDialect,
    mode: ExplainMode,
    statement: &str,
    read_only: bool,
    lex: &SqlLexOptions,
) -> Result<ExplainStatement, ExplainError> {
    let classified = classify(statement, lex);
    if classified.is_empty() {
        return Err(ExplainError::EmptyStatement);
    }
    let single = classified.single().ok_or(ExplainError::MultipleStatements)?;
    if matches!(single.keyword, Some(Keyword::Explain | Keyword::Describe | Keyword::Analyze)) {
        let word = single.keyword_range.as_ref().map_or("", |r| &statement[r.clone()]);
        return Err(ExplainError::AlreadyExplain(word.to_ascii_uppercase()));
    }
    if mode == ExplainMode::Analyze {
        if !dialect.supports_analyze() {
            return Err(ExplainError::AnalyzeUnsupported(dialect));
        }
        if !read_only {
            return Err(ExplainError::AnalyzeRequiresReadOnly);
        }
    }

    let body = statement[single.range.clone()].trim();
    let (prefix, format) = match (dialect, mode) {
        (ExplainDialect::Postgres, ExplainMode::Estimate) => ("EXPLAIN (FORMAT JSON)", PlanFormat::Json),
        (ExplainDialect::Postgres, ExplainMode::Analyze) => {
            ("EXPLAIN (ANALYZE, BUFFERS, FORMAT JSON)", PlanFormat::Json)
        }
        (ExplainDialect::MySql, ExplainMode::Estimate) | (ExplainDialect::MariaDb, ExplainMode::Estimate) => {
            ("EXPLAIN FORMAT=JSON", PlanFormat::Json)
        }
        (ExplainDialect::MySql, ExplainMode::Analyze) => ("EXPLAIN ANALYZE", PlanFormat::Tree),
        (ExplainDialect::MariaDb, ExplainMode::Analyze) => ("ANALYZE FORMAT=JSON", PlanFormat::Json),
        (ExplainDialect::Sqlite, _) => ("EXPLAIN QUERY PLAN", PlanFormat::Rows),
        (ExplainDialect::ClickHouse, _) => ("EXPLAIN json = 1, actions = 1, indexes = 1", PlanFormat::Json),
    };
    Ok(ExplainStatement { sql: format!("{prefix} {body}"), format })
}
