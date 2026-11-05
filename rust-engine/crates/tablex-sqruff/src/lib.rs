//! SQL formatting with sqruff, limited to rules that cannot change what a statement says.
//!
//! The configuration is the contract of this tool: the `layout` rule group (whitespace, line
//! breaks and indentation only) and, unless the keyword case is kept as typed, the capitalisation
//! of keywords and of the `NULL` / `TRUE` / `FALSE` literals. Everything that rewrites tokens
//! (aliasing, convention such as `!=` to `<>` or `COALESCE`, references, structure) is not selected,
//! and identifiers, functions and types keep their spelling. The app still checks every answer
//! with its guard before it touches the user's text.

use engine_driver_api::sql_format::{Dialect, Failure, FailureKind, FormatRequest, KeywordCase};
use sqruff_lib::core::config::FluffConfig;
use sqruff_lib::core::linter::core::Linter;

/// Largest text formatted in one request. sqruff parses the whole text at once; beyond this the
/// time grows past what a format command should take.
pub const MAX_SQL_BYTES: usize = 8 * 1024 * 1024;

/// The sqruff configuration (INI text) for `request`.
pub fn config(dialect: Dialect, keyword_case: KeywordCase, indent: u8) -> String {
    let indent = indent.clamp(1, 16);
    let mut text = format!(
        "[sqruff]\n\
         dialect = {dialect}\n\
         rules = {rules}\n\
         large_file_skip_byte_limit = 0\n\
         large_file_skip_char_limit = 0\n\
         [sqruff:indentation]\n\
         indent_unit = space\n\
         tab_space_size = {indent}\n",
        dialect = dialect.as_str(),
        rules = match keyword_case {
            KeywordCase::Keep => "layout",
            KeywordCase::Upper | KeywordCase::Lower => "layout,capitalisation.keywords,capitalisation.literals",
        },
    );
    if let KeywordCase::Upper | KeywordCase::Lower = keyword_case {
        let policy = if keyword_case == KeywordCase::Upper { "upper" } else { "lower" };
        for rule in ["capitalisation.keywords", "capitalisation.literals"] {
            text.push_str(&format!("[sqruff:rules:{rule}]\ncapitalisation_policy = {policy}\n"));
        }
    }
    text
}

/// Formats `request.sql`.
pub fn format(request: &FormatRequest) -> Result<String, Failure> {
    let failure = |kind, message: String| Failure { kind, message };
    if request.sql.len() > MAX_SQL_BYTES {
        return Err(failure(
            FailureKind::TooLarge,
            format!("the SQL is larger than {} MiB", MAX_SQL_BYTES / (1024 * 1024)),
        ));
    }
    let source = config(request.dialect, request.keyword_case, request.indent);
    let config = FluffConfig::try_from_source(&source, None)
        .map_err(|error| failure(FailureKind::Internal, format!("sqruff configuration: {}", error.value)))?;
    let linter = Linter::new(config, None, None, false)
        .map_err(|error| failure(FailureKind::Internal, format!("sqruff: {error}")))?;
    let linted = linter
        .lint_string(&request.sql, None, true)
        .map_err(|error| failure(FailureKind::BadRequest, format!("sqruff: {}", error.value)))?;
    if linted.has_parse_or_templating_errors() {
        // sqruff does not fix text it cannot parse: say where, rather than hand the text back.
        let message = linted
            .violations()
            .iter()
            .find(|violation| violation.rule.is_none())
            .map_or_else(
                || "the SQL could not be parsed".to_owned(),
                |violation| format!("line {}: {}", violation.line_no, violation.description),
            );
        return Err(failure(FailureKind::Parse, message));
    }
    Ok(linted.fix_string())
}
