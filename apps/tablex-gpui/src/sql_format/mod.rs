//! Format SQL: the formatter behind the editor's Format SQL command.
//!
//! [`SqlFormatter`] is the seam, with three variants:
//!
//! * [`BuiltIn`](SqlFormatter::BuiltIn): `sqlformat`, token based, in process, well under a
//!   millisecond a statement. The default.
//! * [`Tool`](SqlFormatter::Tool): `tablex-sqruff`, the tool the Driver Manager installs. sqruff
//!   (a real SQL parser) runs in a separate process: linking it into the app costs 16 MB and about a
//!   second per 200 statements.
//! * [`External`](SqlFormatter::External): a command the user configured; see [`external`].
//!
//! [`SqlFormatter::format`] may block on a process; the caller runs it on the background executor
//! and applies the result only if the buffer did not change meanwhile (`AppRoot::format_sql`).
//! Whatever a formatter returns goes through [`guard::verify`] before it is offered. The built-in
//! formatter is held to it strictly (a refusal leaves the text alone). The tool and the command are
//! the user's choice, so when the guard disagrees with them the user decides
//! ([`FormatError::Disagreement`]: apply anyway, or cancel).

pub mod external;
pub use engine_driver_kit::sql_guard as guard;
use crate::settings::KeywordCase;
use engine_driver_api::sql_format::{
    Dialect, FormatRequest, KeywordCase as WireKeywordCase, PROTOCOL_VERSION,
};
use engine_driver_host::{
    run_filter, FormatCancel, FormatHostError, SqlFormatTool, ToolStore, SQRUFF_TOOL_ID,
};
use engine_driver_kit::sql::SqlLexOptions;
use engine_plan::ExplainDialect;
use guard::{verify, GuardViolation, Verified};
use sqlformat::{Dialect as SqlFormatDialect, FormatOptions, Indent, QueryParams};
use std::ops::Range;
use std::sync::Arc;
use std::time::Duration;

/// How the built-in formatter lays text out.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct BuiltInOptions {
    pub keyword_case: KeywordCase,
    /// Spaces per indent level.
    pub indent: u8,
    /// Blank lines after a statement's `;`.
    pub lines_between_statements: u8,
}

/// What only the open tab knows, for the formatters that run a process.
#[derive(Debug, Clone)]
pub struct FormatContext {
    /// The dialect of the tab's connection.
    pub dialect: Dialect,
    /// Where tools are installed; `None` when the platform has no data directory.
    pub tools: Option<Arc<ToolStore>>,
    /// Stops the process when the editor closes or a newer format starts.
    pub cancel: FormatCancel,
}

/// The `tablex-sqruff` tool.
#[derive(Debug, Clone)]
pub struct ToolOptions {
    pub context: FormatContext,
    pub keyword_case: KeywordCase,
    pub indent: u8,
    pub timeout: Duration,
}

/// The user's command, as the settings hold it.
#[derive(Debug, Clone)]
pub struct ExternalOptions {
    pub context: FormatContext,
    pub program: String,
    /// The arguments text (see [`external::split_arguments`]).
    pub args: String,
    /// The exit codes that are a success (see [`external::parse_exit_codes`]).
    pub exit_codes: String,
    pub timeout: Duration,
}

/// A formatter for SQL text.
#[derive(Debug, Clone)]
pub enum SqlFormatter {
    BuiltIn(BuiltInOptions),
    Tool(ToolOptions),
    External(ExternalOptions),
}

/// A tool's or a command's output that the guard disagrees with, offered to the user to apply or not.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Disagreement {
    /// Who produced the output: "SQL Formatter (sqruff)" or "The external command".
    pub source: &'static str,
    /// The output, trimmed.
    pub formatted: String,
    pub violation: GuardViolation,
    /// The first pair of tokens that differ (the input's, the output's; `None`: it ran out), when
    /// the violation is about tokens.
    pub differing: Option<(Option<String>, Option<String>)>,
}

impl Disagreement {
    /// "The first difference: a became b." for the dialog; `None` when the guard's complaint is
    /// not about a pair of tokens.
    pub fn first_difference(&self) -> Option<String> {
        Some(match self.differing.as_ref()? {
            (Some(before), Some(after)) => format!("The first difference: {before} became {after}."),
            (Some(before), None) => format!("The first difference: {before} was removed."),
            (None, Some(after)) => format!("The first difference: {after} was added."),
            (None, None) => return None,
        })
    }
}

/// Why a format was not applied.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FormatError {
    /// Nothing but blanks.
    Empty,
    /// The built-in formatter's output changed what the statement says (see [`guard`]).
    Refused(GuardViolation),
    /// The tool's or command's output differs from the statement in a way the guard cannot accept;
    /// the user may still apply it.
    Disagreement(Box<Disagreement>),
    /// SQL Formatter (sqruff) is not installed.
    ToolNotInstalled,
    /// The external command is not set.
    NotConfigured,
    /// The process could not run or failed; the text is for the user (stderr included).
    Failed(String),
    /// Stopped by the editor closing or a newer format.
    Cancelled,
}

impl std::fmt::Display for FormatError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Empty => f.write_str("There is no SQL to format"),
            Self::Refused(violation) => write!(f, "{violation}"),
            Self::Disagreement(disagreement) => write!(f, "{}", disagreement.violation),
            Self::ToolNotInstalled => {
                f.write_str("SQL Formatter (sqruff) is not installed. Install it from the Driver Manager")
            }
            Self::NotConfigured => f.write_str("No external formatter command is set. Set one in Settings \u{203A} Editor"),
            Self::Failed(message) => f.write_str(message),
            Self::Cancelled => f.write_str("The formatting was stopped"),
        }
    }
}

impl std::error::Error for FormatError {}

impl From<GuardViolation> for FormatError {
    fn from(violation: GuardViolation) -> Self {
        Self::Refused(violation)
    }
}

/// The wire dialect of a connection's explain dialect.
pub fn wire_dialect(dialect: ExplainDialect) -> Dialect {
    match dialect {
        ExplainDialect::Postgres => Dialect::Postgres,
        ExplainDialect::MySql => Dialect::Mysql,
        ExplainDialect::MariaDb => Dialect::Mariadb,
        ExplainDialect::Sqlite => Dialect::Sqlite,
        ExplainDialect::ClickHouse => Dialect::Clickhouse,
    }
}

impl SqlFormatter {
    /// Formats `sql` (read as `lex` reads it) and checks the result. May block: call it off the
    /// UI thread.
    pub fn format(&self, sql: &str, lex: &SqlLexOptions) -> Result<Verified, FormatError> {
        if sql.trim().is_empty() {
            return Err(FormatError::Empty);
        }
        let (formatted, source) = match self {
            Self::BuiltIn(options) => return Ok(verify(sql, built_in(sql, lex, options).trim(), lex)?),
            Self::Tool(options) => (options.run(sql)?, "SQL Formatter (sqruff)"),
            Self::External(options) => (options.run(sql)?, "The external command"),
        };
        let formatted = formatted.trim();
        if formatted.is_empty() {
            return Err(FormatError::Failed("The formatter returned no text".into()));
        }
        verify(sql, formatted, lex).map_err(|violation| {
            FormatError::Disagreement(Box::new(Disagreement {
                source,
                differing: violation.differing_tokens(sql, formatted, lex),
                formatted: formatted.to_owned(),
                violation,
            }))
        })
    }
}

fn process_failure(error: FormatHostError) -> FormatError {
    match error {
        FormatHostError::Cancelled => FormatError::Cancelled,
        other => FormatError::Failed(other.to_string()),
    }
}

impl ToolOptions {
    fn run(&self, sql: &str) -> Result<String, FormatError> {
        let store = self.context.tools.as_ref().ok_or(FormatError::ToolNotInstalled)?;
        // The size and digest of the executable are checked (and remembered): a replaced or
        // truncated file is "not installed", never run.
        match store.installed(SQRUFF_TOOL_ID) {
            Ok(Some(_)) => {}
            Ok(None) => return Err(FormatError::ToolNotInstalled),
            Err(error) => return Err(FormatError::Failed(error.to_string())),
        }
        let executable = store
            .executable_path(SQRUFF_TOOL_ID)
            .map_err(|error| FormatError::Failed(error.to_string()))?;
        let request = FormatRequest {
            protocol: PROTOCOL_VERSION,
            sql: sql.to_owned(),
            dialect: self.context.dialect,
            keyword_case: match self.keyword_case {
                KeywordCase::Upper => WireKeywordCase::Upper,
                KeywordCase::Lower => WireKeywordCase::Lower,
                KeywordCase::Keep => WireKeywordCase::Keep,
            },
            indent: self.indent,
        };
        SqlFormatTool::new(executable)
            .format(request, self.timeout, &self.context.cancel)
            .map_err(|error| match error {
                FormatHostError::NotInstalled { .. } => FormatError::ToolNotInstalled,
                FormatHostError::ProtocolMismatch { found, expected } => FormatError::Failed(format!(
                    "SQL Formatter (sqruff) speaks protocol {found} and this TableX speaks {expected}. Update SQL Formatter (sqruff) in the Driver Manager"
                )),
                other => process_failure(other),
            })
    }
}

impl ExternalOptions {
    fn run(&self, sql: &str) -> Result<String, FormatError> {
        if self.program.is_empty() {
            return Err(FormatError::NotConfigured);
        }
        let arguments = external::split_arguments(&self.args)
            .map_err(|error| FormatError::Failed(format!("The command's arguments: {error}")))?;
        let accepted = external::parse_exit_codes(&self.exit_codes)
            .map_err(|error| FormatError::Failed(format!("The accepted exit codes: {error}")))?;
        // Looking for the program may ask the login shell (macOS): that is part of the time limit.
        let started = std::time::Instant::now();
        let path = external::search_path(self.timeout / 2);
        let program = external::resolve_program(&self.program, &path, &external::system_pathext()).ok_or_else(|| {
            FormatError::Failed(format!(
                "{} was not found on your PATH (a program that is not on it needs its full path)",
                self.program
            ))
        })?;
        let mut command = std::process::Command::new(program);
        command
            .args(arguments.iter().map(|argument| external::expand_placeholders(argument, self.context.dialect)))
            .env("PATH", &path);
        let remaining = self.timeout.saturating_sub(started.elapsed());
        run_filter(command, sql, remaining, &self.context.cancel, &accepted).map_err(process_failure)
    }
}

fn built_in(sql: &str, lex: &SqlLexOptions, options: &BuiltInOptions) -> String {
    let dialect = if lex.dollar_quotes {
        SqlFormatDialect::PostgreSql
    } else if lex.bracket_identifiers {
        SqlFormatDialect::SQLServer
    } else {
        SqlFormatDialect::Generic
    };
    sqlformat::format(
        sql,
        &QueryParams::None,
        &FormatOptions {
            indent: Indent::Spaces(options.indent),
            uppercase: match options.keyword_case {
                KeywordCase::Upper => Some(true),
                KeywordCase::Lower => Some(false),
                KeywordCase::Keep => None,
            },
            // `sqlformat` counts the line breaks after the `;`: one is no blank line.
            lines_between_queries: options.lines_between_statements.saturating_add(1),
            dialect,
            ..FormatOptions::default()
        },
    )
}

/// The part of `text` a format works on: the statements `selection` touches (a statement is
/// whole even when the selection covers a piece of it), else, with no selection, all the text
/// without its outer blanks. `None` when that is nothing to format.
pub fn target_range(text: &str, selection: Range<usize>, lex: &SqlLexOptions) -> Option<Range<usize>> {
    if selection.is_empty() {
        let lead = text.len() - text.trim_start().len();
        let end = text.trim_end().len();
        return (lead < end).then_some(lead..end);
    }
    let statements = crate::sql_script::statement_ranges(text, lex);
    let mut touched = statements
        .iter()
        .filter(|range| range.start < selection.end && selection.start < range.end);
    let first = touched.next()?;
    let last = touched.next_back().unwrap_or(first);
    Some(first.start..last.end)
}

#[cfg(test)]
mod tests;
