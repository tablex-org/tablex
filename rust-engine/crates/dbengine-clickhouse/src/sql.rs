//! Pure SQL text helpers: identifier and literal quoting, query-parameter
//! escaping, the top-level keyword scan behind server-side paging, and the
//! ClickHouse-only dangerous statements. Nothing here touches the network.
//!
//! Which statements are pageable, dangerous or change the default database is
//! decided by `engine_driver_kit::sql::classify`; this module only answers "can
//! this *single pure read* take a trailing `LIMIT n OFFSET m`?" and adds the
//! few statements ClickHouse has that the generic classifier does not know.

use engine_driver_kit::sql::SqlLexOptions;
use std::fmt::Write;

/// Appends `name` as a backtick-quoted identifier (`\` and `` ` `` escaped).
pub(crate) fn push_ident(out: &mut String, name: &str) {
    out.push('`');
    for ch in name.chars() {
        if matches!(ch, '`' | '\\') {
            out.push('\\');
        }
        out.push(ch);
    }
    out.push('`');
}

/// `` `database`.`table` `` (or just `` `table` ``).
pub(crate) fn push_qualified(out: &mut String, database: Option<&str>, table: &str) {
    if let Some(database) = database.filter(|database| !database.is_empty()) {
        push_ident(out, database);
        out.push('.');
    }
    push_ident(out, table);
}

pub(crate) fn qualified(database: Option<&str>, table: &str) -> String {
    let mut out = String::new();
    push_qualified(&mut out, database, table);
    out
}

/// A string literal for text the driver itself builds (a query id in `KILL`).
/// Values from the user always travel as query parameters instead.
pub(crate) fn string_literal(value: &str) -> String {
    let mut out = String::with_capacity(value.len() + 2);
    out.push('\'');
    for ch in value.chars() {
        match ch {
            '\\' | '\'' => {
                out.push('\\');
                out.push(ch);
            }
            '\0' => out.push_str("\\0"),
            _ => out.push(ch),
        }
    }
    out.push('\'');
    out
}

/// A query-parameter value. ClickHouse parses `param_*` values in its escaped
/// (TSV) text format, so backslash, tab, newline, carriage return and NUL are
/// escaped, and a literal `\N` is not mistaken for NULL.
pub(crate) fn escape_param(value: &str) -> String {
    let mut out = String::with_capacity(value.len());
    for ch in value.chars() {
        match ch {
            '\\' => out.push_str("\\\\"),
            '\t' => out.push_str("\\t"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\0' => out.push_str("\\0"),
            _ => out.push(ch),
        }
    }
    out
}

/// The NULL value of a query parameter.
pub(crate) const NULL_PARAM: &str = "\\N";

/// Top-level clauses that stop a trailing `LIMIT` from being appended.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub(crate) struct Scan {
    /// `LIMIT`, `OFFSET`, `FETCH` or `SETTINGS` outside parentheses: a `LIMIT`
    /// appended after them would be a syntax error (or change the query).
    pub paging_or_settings: bool,
    /// The statement ends in `FORMAT <name>`.
    pub format_clause: bool,
}

impl Scan {
    pub(crate) fn accepts_limit(&self) -> bool {
        !self.paging_or_settings && !self.format_clause
    }
}

fn is_word(byte: u8) -> bool {
    byte.is_ascii_alphanumeric() || byte == b'_' || byte >= 0x80
}

/// Index just past the quoted run starting at `start` (`\` escapes the next
/// byte when `escapes`; a doubled quote is an escaped quote).
fn skip_quoted(bytes: &[u8], start: usize, escapes: bool) -> usize {
    let quote = bytes[start];
    let mut index = start + 1;
    while index < bytes.len() {
        match bytes[index] {
            b'\\' if escapes => index += 2,
            byte if byte == quote => {
                if bytes.get(index + 1) == Some(&quote) {
                    index += 2;
                } else {
                    return index + 1;
                }
            }
            _ => index += 1,
        }
    }
    bytes.len()
}

fn skip_line(bytes: &[u8], start: usize) -> usize {
    bytes[start..]
        .iter()
        .position(|byte| *byte == b'\n')
        .map_or(bytes.len(), |offset| start + offset + 1)
}

/// `$tag$ ... $tag$` heredoc starting at `start`; `None` when `$` is not one.
fn skip_heredoc(bytes: &[u8], start: usize) -> Option<usize> {
    let tag_len = bytes[start + 1..]
        .iter()
        .take_while(|byte| byte.is_ascii_alphanumeric() || **byte == b'_')
        .count();
    if bytes.get(start + 1 + tag_len) != Some(&b'$') {
        return None;
    }
    let delimiter = &bytes[start..start + tag_len + 2];
    let body = start + tag_len + 2;
    Some(
        bytes[body..]
            .windows(delimiter.len())
            .position(|window| window == delimiter)
            .map_or(bytes.len(), |offset| body + offset + delimiter.len()),
    )
}

/// Scans one statement (no allocation), reading quotes and comments the way
/// `options` (the ClickHouse lexer options) say.
pub(crate) fn scan(statement: &str, options: &SqlLexOptions) -> Scan {
    let bytes = statement.as_bytes();
    let mut result = Scan::default();
    let mut depth = 0_usize;
    let mut index = 0;
    // The previous token was a top-level `FORMAT`.
    let mut after_format = false;
    while index < bytes.len() {
        let byte = bytes[index];
        if byte.is_ascii_whitespace() {
            index += 1;
            continue;
        }
        // Comments are not tokens: they neither end nor continue a `FORMAT name`.
        match byte {
            b'-' if bytes.get(index + 1) == Some(&b'-') => {
                index = skip_line(bytes, index);
                continue;
            }
            b'#' if options.hash_comments => {
                index = skip_line(bytes, index);
                continue;
            }
            b'/' if bytes.get(index + 1) == Some(&b'*') => {
                index = statement[index + 2..]
                    .find("*/")
                    .map_or(bytes.len(), |end| index + 2 + end + 2);
                continue;
            }
            _ => {}
        }
        let was_after_format = std::mem::take(&mut after_format);
        // Any token after `FORMAT <name>` means it was not the final clause.
        result.format_clause = false;
        match byte {
            b'\'' => index = skip_quoted(bytes, index, options.backslash_escapes),
            b'"' | b'`' => index = skip_quoted(bytes, index, options.backslash_in_identifiers),
            b'$' if options.dollar_quotes => match skip_heredoc(bytes, index) {
                Some(end) => index = end,
                None => index += 1,
            },
            b'(' => {
                depth += 1;
                index += 1;
            }
            b')' => {
                depth = depth.saturating_sub(1);
                index += 1;
            }
            byte if is_word(byte) => {
                let start = index;
                while index < bytes.len() && is_word(bytes[index]) {
                    index += 1;
                }
                let word = &bytes[start..index];
                if was_after_format {
                    result.format_clause = true;
                } else if depth == 0 {
                    let is = |keyword: &str| word.eq_ignore_ascii_case(keyword.as_bytes());
                    if is("limit") || is("offset") || is("fetch") || is("settings") {
                        result.paging_or_settings = true;
                    }
                    after_format = is("format");
                }
            }
            _ => index += 1,
        }
    }
    result
}

/// `statement` (one pure `SELECT`/`WITH`) with `LIMIT`/`OFFSET` appended,
/// fetching one row more than `limit` so the caller can tell whether the result
/// continues. Call only when [`Scan::accepts_limit`].
pub(crate) fn with_limit(statement: &str, offset: u64, limit: u64) -> String {
    let mut out = String::with_capacity(statement.len() + 40);
    out.push_str(statement);
    // A newline first: the statement may end in a `-- comment`. Saturating:
    // `limit + 1` must not wrap for absurd page sizes.
    let _ = write!(out, "\nLIMIT {}", limit.saturating_add(1));
    if offset > 0 {
        let _ = write!(out, " OFFSET {offset}");
    }
    out
}

/// Splits `a, f(b, c), 'x,y'` at its top-level commas (a key expression list).
pub(crate) fn split_expressions(list: &str) -> Vec<String> {
    let bytes = list.as_bytes();
    let mut items = Vec::new();
    let (mut depth, mut start, mut index) = (0_usize, 0, 0);
    while index < bytes.len() {
        match bytes[index] {
            b'\'' | b'"' | b'`' => {
                index = skip_quoted(bytes, index, true);
                continue;
            }
            b'(' | b'[' => depth += 1,
            b')' | b']' => depth = depth.saturating_sub(1),
            b',' if depth == 0 => {
                items.push(list[start..index].trim().to_owned());
                start = index + 1;
            }
            _ => {}
        }
        index += 1;
    }
    items.push(list[start..].trim().to_owned());
    items.retain(|item| !item.is_empty());
    items
}

/// Statements ClickHouse has that `engine_driver_kit::sql::classify` does not
/// know to be dangerous. `first` is the statement's first word, `text` the whole
/// statement. Returns the reason shown with `query.confirmation_required`.
pub(crate) fn extra_danger(first: &str, text: &str) -> Option<&'static str> {
    let is = |keyword: &str| first.eq_ignore_ascii_case(keyword);
    if is("optimize") {
        // `OPTIMIZE ... FINAL` rewrites every part of the table.
        let rewrites = text
            .split(|c: char| !c.is_ascii_alphanumeric() && c != '_')
            .any(|word| word.eq_ignore_ascii_case("final"));
        return rewrites.then_some("OPTIMIZE ... FINAL rewrites every data part of the table");
    }
    if is("detach") {
        return Some("DETACH takes a table or database offline");
    }
    if is("rename") || is("exchange") {
        return Some("RENAME/EXCHANGE changes the names of database objects");
    }
    if is("kill") {
        return Some("KILL stops running queries or mutations");
    }
    if is("system") {
        return Some("SYSTEM commands change the state of the server");
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    fn lex() -> SqlLexOptions {
        SqlLexOptions::clickhouse()
    }

    #[test]
    fn identifiers_and_literals_are_escaped() {
        assert_eq!(qualified(Some("my db"), "t`x"), "`my db`.`t\\`x`");
        assert_eq!(qualified(None, "t"), "`t`");
        assert_eq!(qualified(Some(""), "t"), "`t`");
        assert_eq!(string_literal("a'b\\c"), "'a\\'b\\\\c'");
        assert_eq!(string_literal("x\0"), "'x\\0'");
    }

    #[test]
    fn parameters_use_the_tsv_escapes_and_keep_a_literal_backslash_n() {
        assert_eq!(escape_param("a\\b\tc\nd\re"), "a\\\\b\\tc\\nd\\re");
        assert_eq!(escape_param("\\N"), "\\\\N");
        assert_eq!(NULL_PARAM, "\\N");
        assert_eq!(escape_param("it's \"fine\""), "it's \"fine\"");
    }

    #[test]
    fn limit_is_appended_only_to_statements_that_take_it() {
        for sql in [
            "SELECT 1",
            "SELECT a FROM t WHERE b = 'limit' ORDER BY a",
            "SELECT * FROM (SELECT * FROM t LIMIT 5) AS x",
            "WITH x AS (SELECT 1) SELECT * FROM x",
            "SELECT format('{}', 1) AS limit_",
            "SELECT 1 AS format",
            "SELECT \"limit\", `offset` FROM t -- limit 1",
            "SELECT $$ limit $$",
            "SELECT 1 /* settings */",
        ] {
            assert!(scan(sql, &lex()).accepts_limit(), "{sql}");
        }
        for sql in [
            "SELECT * FROM t LIMIT 10",
            "SELECT * FROM t limit 10 offset 5",
            "SELECT * FROM t LIMIT 3 BY a",
            "SELECT * FROM t SETTINGS max_threads = 1",
            "SELECT * FROM t OFFSET 5 ROWS FETCH FIRST 3 ROWS ONLY",
            "SELECT 1 FORMAT JSON",
            "SELECT 1 format TabSeparated -- done",
        ] {
            assert!(!scan(sql, &lex()).accepts_limit(), "{sql}");
        }
        assert!(scan("SELECT 1 FORMAT JSON", &lex()).format_clause);
        assert!(!scan("SELECT 1 SETTINGS a = 1", &lex()).format_clause);
    }

    #[test]
    fn with_limit_adds_a_lookahead_row_and_survives_trailing_comments() {
        assert_eq!(with_limit("SELECT 1", 0, 10), "SELECT 1\nLIMIT 11");
        assert_eq!(
            with_limit("SELECT 1 -- c", 20, 10),
            "SELECT 1 -- c\nLIMIT 11 OFFSET 20"
        );
        assert_eq!(
            with_limit("SELECT 1", 0, u64::MAX),
            format!("SELECT 1\nLIMIT {}", u64::MAX)
        );
    }

    #[test]
    fn key_expressions_split_at_top_level_commas() {
        assert_eq!(split_expressions("tenant, id"), ["tenant", "id"]);
        assert_eq!(
            split_expressions("toYYYYMM(d), cityHash64(a, b), 'x,y'"),
            ["toYYYYMM(d)", "cityHash64(a, b)", "'x,y'"]
        );
        assert!(split_expressions("").is_empty());
    }

    #[test]
    fn clickhouse_only_statements_are_flagged() {
        let danger = |sql: &str| {
            let first = sql.split_whitespace().next().unwrap_or_default();
            extra_danger(first, sql)
        };
        assert!(danger("OPTIMIZE TABLE t FINAL").is_some());
        assert!(danger("optimize table t final deduplicate").is_some());
        assert_eq!(danger("OPTIMIZE TABLE t"), None);
        assert!(danger("DETACH TABLE t").is_some());
        assert!(danger("RENAME TABLE a TO b").is_some());
        assert!(danger("KILL QUERY WHERE 1").is_some());
        assert!(danger("SYSTEM FLUSH LOGS").is_some());
        assert_eq!(danger("SELECT 1"), None);
        assert_eq!(danger("INSERT INTO t VALUES (1)"), None);
    }
}
