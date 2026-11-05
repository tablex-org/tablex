//! Pure SQL text helpers: identifier quoting, a top-level keyword scan for
//! server-side paging and the mutation statement builders. Nothing here touches
//! the network.
//!
//! Which statements are pageable, dangerous or change the default database is
//! decided by `engine_driver_kit::sql::classify`, not here. This module only
//! answers the narrower question "can this *single pure read* take a trailing
//! `LIMIT n OFFSET m`?". It reads the SQL with the same `SqlLexOptions` the
//! classifier used, so both agree about quotes and comments in the session's
//! `sql_mode`.

use engine_driver_kit::sql::SqlLexOptions;
use std::fmt::Write;

/// Appends `name` as a backtick-quoted identifier (embedded backticks doubled).
pub(super) fn push_ident(out: &mut String, name: &str) {
    out.push('`');
    for ch in name.chars() {
        if ch == '`' {
            out.push('`');
        }
        out.push(ch);
    }
    out.push('`');
}

/// Appends `` `database`.`table` `` (or just `` `table` `` without a database).
pub(super) fn push_qualified(out: &mut String, database: Option<&str>, table: &str) {
    if let Some(database) = database.filter(|database| !database.is_empty()) {
        push_ident(out, database);
        out.push('.');
    }
    push_ident(out, table);
}

/// Top-level (outside quotes, comments and parentheses) keywords of interest.
pub(super) mod word {
    pub const LIMIT: u16 = 1;
    pub const INTO: u16 = 1 << 1;
    pub const FOR: u16 = 1 << 2;
    pub const LOCK: u16 = 1 << 3;
    pub const PROCEDURE: u16 = 1 << 4;
}

/// Result of [`scan`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) struct Scan {
    /// Bit set of [`word`] flags.
    pub words: u16,
    /// A `/*! ... */` (or MariaDB `/*M! ... */`) executable comment: its content
    /// is code the scan does not read, so no rewriting is safe.
    pub executable_comment: bool,
}

impl Scan {
    /// A trailing `LIMIT n OFFSET m` can be appended unchanged.
    pub(super) fn accepts_limit(&self) -> bool {
        !self.executable_comment
            && self.words & (word::LIMIT | word::INTO | word::FOR | word::LOCK | word::PROCEDURE)
                == 0
    }
}

fn is_word(byte: u8) -> bool {
    byte.is_ascii_alphanumeric() || byte == b'_' || byte == b'$' || byte >= 0x80
}

/// Skips a quoted run starting at `start`. A backslash escapes the next byte
/// inside a string literal when the mode allows it; never inside a quoted
/// identifier (backticks, and `"` under `ANSI_QUOTES`).
fn skip_quoted(bytes: &[u8], start: usize, options: &SqlLexOptions) -> usize {
    let quote = bytes[start];
    let escapes = options.backslash_escapes
        && match quote {
            b'\'' => true,
            b'"' => !options.double_quote_is_identifier,
            _ => false,
        };
    let mut index = start + 1;
    while index < bytes.len() {
        match bytes[index] {
            b'\\' if escapes => index += 2,
            byte if byte == quote => return index + 1,
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

fn skip_block(bytes: &[u8], start: usize) -> usize {
    let mut index = start + 2;
    while index + 1 < bytes.len() {
        if bytes[index] == b'*' && bytes[index + 1] == b'/' {
            return index + 2;
        }
        index += 1;
    }
    bytes.len()
}

fn is_dash_comment(bytes: &[u8], index: usize, options: &SqlLexOptions) -> bool {
    bytes.get(index + 1) == Some(&b'-')
        && (!options.dash_comment_needs_space
            || bytes
                .get(index + 2)
                .is_none_or(|byte| byte.is_ascii_whitespace() || byte.is_ascii_control()))
}

fn keyword_flag(word: &[u8]) -> u16 {
    let eq = |keyword: &str| word.eq_ignore_ascii_case(keyword.as_bytes());
    if eq("limit") {
        word::LIMIT
    } else if eq("into") {
        word::INTO
    } else if eq("for") {
        word::FOR
    } else if eq("lock") {
        word::LOCK
    } else if eq("procedure") {
        word::PROCEDURE
    } else {
        0
    }
}

/// Scans one statement (no allocation), reading quotes and comments as
/// `options` (the session's `sql_mode`) say.
pub(super) fn scan(statement: &str, options: &SqlLexOptions) -> Scan {
    let bytes = statement.as_bytes();
    let mut result = Scan {
        words: 0,
        executable_comment: false,
    };
    let mut depth = 0_usize;
    let mut index = 0;
    while index < bytes.len() {
        match bytes[index] {
            b'\'' | b'"' | b'`' => index = skip_quoted(bytes, index, options),
            b'#' if options.hash_comments => index = skip_line(bytes, index),
            b'-' if is_dash_comment(bytes, index, options) => index = skip_line(bytes, index),
            b'/' if bytes.get(index + 1) == Some(&b'*') => {
                let after = (bytes.get(index + 2), bytes.get(index + 3));
                if options.executable_comments
                    && (after.0 == Some(&b'!')
                        || (options.mariadb_comments
                            && after.0 == Some(&b'M')
                            && after.1 == Some(&b'!')))
                {
                    result.executable_comment = true;
                }
                index = skip_block(bytes, index);
            }
            b'(' => {
                depth += 1;
                index += 1;
            }
            b')' => {
                depth = depth.saturating_sub(1);
                index += 1;
            }
            b';' => break,
            byte if is_word(byte) => {
                let start = index;
                while index < bytes.len() && is_word(bytes[index]) {
                    index += 1;
                }
                if depth == 0 {
                    result.words |= keyword_flag(&bytes[start..index]);
                }
            }
            _ => index += 1,
        }
    }
    result
}

/// `statement` (one pure read, as delimited by `classify`) with
/// `LIMIT`/`OFFSET` appended server-side, fetching one row more than `limit` so
/// the caller can tell whether the result continues. `None` when the statement
/// cannot take a trailing `LIMIT` (the caller pages client-side).
pub(super) fn with_limit(statement: &str, scan: &Scan, offset: u64, limit: u64) -> Option<String> {
    if !scan.accepts_limit() {
        return None;
    }
    let mut out = String::with_capacity(statement.len() + 48);
    out.push_str(statement);
    // A newline first: the statement may end in a `-- comment`.
    // Saturating: `limit + 1` must not wrap for absurd page sizes.
    let _ = write!(out, "\nLIMIT {}", limit.saturating_add(1));
    if offset > 0 {
        let _ = write!(out, " OFFSET {offset}");
    }
    Some(out)
}

pub(super) fn insert_sql(database: Option<&str>, table: &str, columns: &[&str]) -> String {
    let mut out = String::from("INSERT INTO ");
    push_qualified(&mut out, database, table);
    if columns.is_empty() {
        out.push_str(" () VALUES ()");
        return out;
    }
    out.push_str(" (");
    for (index, column) in columns.iter().enumerate() {
        if index > 0 {
            out.push_str(", ");
        }
        push_ident(&mut out, column);
    }
    out.push_str(") VALUES (");
    for index in 0..columns.len() {
        out.push_str(if index == 0 { "?" } else { ", ?" });
    }
    out.push(')');
    out
}

fn push_where(out: &mut String, key_columns: &[&str]) {
    out.push_str(" WHERE ");
    for (index, column) in key_columns.iter().enumerate() {
        if index > 0 {
            out.push_str(" AND ");
        }
        push_ident(out, column);
        // Null-safe: a NULL key value matches the row whose column IS NULL.
        out.push_str(" <=> ?");
    }
}

pub(super) fn update_sql(
    database: Option<&str>,
    table: &str,
    set_columns: &[&str],
    key_columns: &[&str],
) -> String {
    let mut out = String::from("UPDATE ");
    push_qualified(&mut out, database, table);
    out.push_str(" SET ");
    for (index, column) in set_columns.iter().enumerate() {
        if index > 0 {
            out.push_str(", ");
        }
        push_ident(&mut out, column);
        out.push_str(" = ?");
    }
    push_where(&mut out, key_columns);
    out
}

pub(super) fn delete_sql(database: Option<&str>, table: &str, key_columns: &[&str]) -> String {
    let mut out = String::from("DELETE FROM ");
    push_qualified(&mut out, database, table);
    push_where(&mut out, key_columns);
    out
}

/// How many rows (at most 2) the key matches: the guard of a change on a table
/// without transactions, which cannot be rolled back.
pub(super) fn count_sql(database: Option<&str>, table: &str, key_columns: &[&str]) -> String {
    let mut out = String::from("SELECT COUNT(*) FROM (SELECT 1 FROM ");
    push_qualified(&mut out, database, table);
    push_where(&mut out, key_columns);
    out.push_str(" LIMIT 2) AS tablex_guard");
    out
}

/// `CREATE DATABASE` for `name` (backtick-quoted).
pub(super) fn create_database_sql(name: &str) -> String {
    let mut out = String::from("CREATE DATABASE ");
    push_ident(&mut out, name);
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn paged_with(
        statement: &str,
        options: &SqlLexOptions,
        offset: u64,
        limit: u64,
    ) -> Option<String> {
        with_limit(statement, &scan(statement, options), offset, limit)
    }

    fn paged(statement: &str, offset: u64, limit: u64) -> Option<String> {
        paged_with(statement, &SqlLexOptions::mariadb(), offset, limit)
    }

    #[test]
    fn quotes_identifiers_by_doubling_backticks() {
        let mut out = String::new();
        push_qualified(&mut out, Some("a`b"), "t");
        assert_eq!(out, "`a``b`.`t`");
        out.clear();
        push_qualified(&mut out, Some(""), "t");
        assert_eq!(out, "`t`");
    }

    #[test]
    fn appends_limit_and_offset_with_one_lookahead_row() {
        assert_eq!(
            paged("SELECT * FROM t", 0, 200).unwrap(),
            "SELECT * FROM t\nLIMIT 201"
        );
        assert_eq!(
            paged("select 1", 40, 20).unwrap(),
            "select 1\nLIMIT 21 OFFSET 40"
        );
        assert!(paged("SELECT 1", 0, u64::MAX)
            .unwrap()
            .contains("LIMIT 18446744073709551615"));
    }

    #[test]
    fn a_trailing_line_comment_does_not_swallow_the_limit() {
        let sql = "SELECT * FROM t -- latest rows";
        assert_eq!(
            paged(sql, 0, 10).unwrap(),
            "SELECT * FROM t -- latest rows\nLIMIT 11"
        );
        let sql = "SELECT * FROM t # latest rows";
        assert_eq!(
            paged(sql, 0, 10).unwrap(),
            "SELECT * FROM t # latest rows\nLIMIT 11"
        );
    }

    #[test]
    fn existing_limit_is_found_only_at_top_level() {
        assert!(paged("SELECT * FROM t LIMIT 5", 0, 10).is_none());
        assert!(paged("select * from t limit 5, 10", 0, 10).is_none());
        // `limit` inside a string, a quoted identifier, a comment or a
        // sub-select does not count.
        assert!(paged("SELECT 'limit' AS a FROM t", 0, 10).is_some());
        assert!(paged("SELECT `limit` FROM t", 0, 10).is_some());
        assert!(paged("SELECT 1 /* limit */", 0, 10).is_some());
        assert!(paged("SELECT 1 # limit", 0, 10).is_some());
        assert!(paged("SELECT * FROM (SELECT 1 LIMIT 1) x", 0, 10).is_some());
        assert!(paged("SELECT 'it''s a \\' limit'", 0, 10).is_some());
    }

    #[test]
    fn backslash_handling_follows_the_sql_mode() {
        // `'a\' ... LIMIT 3`: with backslash escapes the `\'` does not close the
        // string, so the LIMIT is inside it (not a real clause); without them
        // (NO_BACKSLASH_ESCAPES) the string closes and the LIMIT is real.
        let sql = "SELECT 'a\\' FROM t LIMIT 3";
        let escapes = SqlLexOptions::mysql();
        assert!(paged_with(sql, &escapes, 0, 10).is_some());
        assert!(paged_with(sql, &escapes.without_backslash_escapes(), 0, 10).is_none());
    }

    #[test]
    fn ansi_quotes_make_double_quotes_identifiers_without_escapes() {
        // `"a\"` is the identifier a\ under ANSI_QUOTES, so the LIMIT after it is
        // real and no second LIMIT may be appended; as a string (default mode)
        // the `\"` is an escape and the rest is still inside the string.
        let sql = "SELECT 1 AS \"a\\\" LIMIT 5";
        let ansi = SqlLexOptions::mysql().with_ansi_quotes();
        assert!(paged_with(sql, &ansi, 0, 10).is_none());
        assert!(paged_with(sql, &SqlLexOptions::mysql(), 0, 10).is_some());
    }

    #[test]
    fn comment_forms_follow_the_dialect() {
        // `--x` is not a comment in MySQL (needs a space) but a LIMIT-hiding
        // comment elsewhere; `/*M!` is code only for MariaDB.
        assert!(paged_with("SELECT 1--x LIMIT 5", &SqlLexOptions::mysql(), 0, 10).is_none());
        assert!(paged_with("SELECT 1 --x\n", &SqlLexOptions::postgres(), 0, 10).is_some());
        assert!(paged_with("/*M!100000 SELECT 1 */", &SqlLexOptions::mariadb(), 0, 10).is_none());
        assert!(paged_with("SELECT 1 /*M!100000 x */", &SqlLexOptions::mysql(), 0, 10).is_some());
    }

    #[test]
    fn clauses_that_must_follow_the_limit_are_left_alone() {
        for sql in [
            "SELECT * FROM t FOR UPDATE",
            "SELECT * FROM t LOCK IN SHARE MODE",
            "SELECT a INTO @x FROM t",
            "SELECT * FROM t PROCEDURE ANALYSE()",
            "/*!40101 SET x=1 */ SELECT 1",
            "/*M!100000 SELECT 1 */",
        ] {
            assert!(paged(sql, 0, 10).is_none(), "{sql}");
        }
    }

    #[test]
    fn builds_mutation_statements_with_placeholders() {
        assert_eq!(
            insert_sql(Some("d"), "t", &["a", "b`c"]),
            "INSERT INTO `d`.`t` (`a`, `b``c`) VALUES (?, ?)"
        );
        assert_eq!(insert_sql(None, "t", &[]), "INSERT INTO `t` () VALUES ()");
        assert_eq!(
            update_sql(Some("d"), "t", &["a", "b"], &["id", "tenant"]),
            "UPDATE `d`.`t` SET `a` = ?, `b` = ? WHERE `id` <=> ? AND `tenant` <=> ?"
        );
        assert_eq!(
            delete_sql(Some("d"), "t", &["id"]),
            "DELETE FROM `d`.`t` WHERE `id` <=> ?"
        );
    }

    #[test]
    fn the_key_guard_counts_at_most_two_rows() {
        assert_eq!(
            count_sql(Some("d"), "t", &["id", "tenant"]),
            "SELECT COUNT(*) FROM (SELECT 1 FROM `d`.`t` WHERE `id` <=> ? AND `tenant` <=> ? \
             LIMIT 2) AS tablex_guard"
        );
    }

    #[test]
    fn create_database_quotes_the_name() {
        assert_eq!(create_database_sql("new`db"), "CREATE DATABASE `new``db`");
    }
}
