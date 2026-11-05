use super::super::*;
use super::snowflake::SnowflakeStore;
use super::column_type::{value_kind, ValueKind};
use crate::sql_table::SqlDialect;
use engine_driver_kit::sql::{tokens, SqlLexOptions, SqlToken, SqlTokenKind};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(in crate::app) enum GenerateKind {
    EmptyString,
    Null,
    CurrentDateTime,
    CurrentDate,
    Uuid,
    Increment(i64),
}

pub(in crate::app) fn generated_values(
    kind: GenerateKind,
    count: usize,
) -> Result<Vec<Option<String>>, String> {
    let now = chrono::Utc::now();
    let mut values = Vec::with_capacity(count);
    for index in 0..count {
        let value = match kind {
            GenerateKind::EmptyString => Some(String::new()),
            GenerateKind::Null => None,
            GenerateKind::CurrentDateTime => Some(now.to_rfc3339()),
            GenerateKind::CurrentDate => Some(now.date_naive().to_string()),
            GenerateKind::Uuid => Some(uuid::Uuid::new_v4().to_string()),
            GenerateKind::Increment(start) => Some(
                start
                    .checked_add(i64::try_from(index).map_err(|_| "Increment overflow")?)
                    .ok_or("Increment overflow")?
                    .to_string(),
            ),
        };
        values.push(value);
    }
    Ok(values)
}

/// Why a statement cannot be re-ordered by [`sorted_sql_query`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(in crate::app) enum SortError {
    /// The column or table name is empty or holds a NUL byte.
    InvalidTarget,
    /// The column name holds a backslash that a backslash-escaping engine would read as
    /// an escape inside the quoted name.
    ColumnNotWritable,
    /// The connection quotes identifiers with something other than `"` or `` ` ``.
    UnsupportedQuoting,
    /// The text cannot be read with certainty (`reason` says why).
    Unreadable(&'static str),
    /// Empty, or more than one statement.
    NotOneStatement,
    NotSelect,
    /// `WITH`, `OFFSET`, `FETCH`, a lock, a non-numeric `LIMIT`, an engine-specific
    /// clause: re-ordering would change what the statement means.
    Unsafe,
    SetQuery,
    /// A projection other than `*`, `DISTINCT`, `GROUP BY`, `HAVING`, a window, `INTO`,
    /// or a `WHERE` that is not a plain comparison of columns and literals.
    NotSimple,
    /// `FROM a, b`.
    NotOneTable,
    Joined,
    /// A subquery, an alias, a table function or a table hint instead of a table name.
    NotPlainTable,
    /// The statement reads another table than the tab's.
    OtherTable,
}

impl std::fmt::Display for SortError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::InvalidTarget => f.write_str("Invalid sort column or table"),
            Self::ColumnNotWritable => {
                f.write_str("This column name can't be written safely for this connection")
            }
            Self::UnsupportedQuoting => {
                f.write_str("This connection's identifier quoting is not supported here")
            }
            Self::Unreadable(reason) => {
                write!(f, "The statement could not be parsed: {reason}")
            }
            Self::NotOneStatement => f.write_str("Only one SELECT statement can be sorted"),
            Self::NotSelect => f.write_str("Only a SELECT statement can be sorted"),
            Self::Unsafe => f.write_str("This SELECT cannot be safely sorted"),
            Self::SetQuery => f.write_str("Set queries cannot be sorted here"),
            Self::NotSimple => f.write_str("Only a simple table SELECT can be sorted"),
            Self::NotOneTable => f.write_str("SELECT must read one table"),
            Self::Joined => f.write_str("Joined SELECT cannot be sorted here"),
            Self::NotPlainTable => f.write_str("SELECT must read one unaliased table"),
            Self::OtherTable => f.write_str("SELECT does not target the current table"),
        }
    }
}

impl std::error::Error for SortError {}

/// Punctuation a sortable statement may contain; any other character (a placeholder
/// `$1`, `?`, `[`, a stray backtick) is not read here.
const PUNCTUATION: &[u8] = b".+-*/%:=<>!|&^~";

/// Words that are not a plain column name in an expression: operators, special
/// forms and values that sqlparser read as something other than an identifier, and
/// every clause keyword (so one can never be mistaken for a column).
const NOT_A_COLUMN: &[&str] = &[
    "ALL", "AND", "ANY", "ARRAY", "AS", "ASC", "AT", "BETWEEN", "BY", "CASE", "CAST", "COLLATE",
    "CROSS", "CURRENT_CATALOG", "CURRENT_DATE", "CURRENT_ROLE", "CURRENT_SCHEMA",
    "CURRENT_TIME", "CURRENT_TIMESTAMP", "CURRENT_USER", "DEFAULT", "DESC", "DISTINCT", "DIV",
    "ELSE", "END", "ESCAPE", "EXCEPT", "EXISTS", "FETCH", "FOR", "FORMAT", "FROM", "FULL",
    "GLOB", "GROUP", "HAVING", "ILIKE", "IN", "INNER", "INTERSECT", "INTERVAL", "INTO", "IS",
    "JOIN", "LEFT", "LIKE", "LIMIT", "LOCALTIME", "LOCALTIMESTAMP", "LOCK", "MATCH", "MINUS",
    "MOD", "NATURAL", "NOT", "OFFSET", "ON", "OR", "ORDER", "OUTER", "OVERLAPS", "QUALIFY",
    "REGEXP", "RIGHT", "RLIKE", "ROW", "SELECT", "SESSION_USER", "SETTINGS", "SIMILAR", "SOME",
    "STRAIGHT_JOIN", "SYSDATE", "SYSTEM_USER", "TABLE", "THEN", "UNION", "USER", "USING",
    "UTC_DATE", "UTC_TIME", "UTC_TIMESTAMP", "VALUES", "WHEN", "WHERE", "WINDOW", "WITH", "XOR",
];

/// Functions whose arguments have their own syntax (`EXTRACT(x FROM y)`,
/// `TRIM(BOTH ' ' FROM x)`, `CAST(x AS t)`, `GROUP_CONCAT(x SEPARATOR ',')`): never a
/// plain column, and a call to one is never read as a plain `f(a, b)`.
const SPECIAL_CALLS: &[&str] = &[
    "ARRAY_AGG", "CAST", "CEIL", "CEILING", "CONVERT", "EXTRACT", "FLOOR", "GROUP_CONCAT",
    "JSON_ARRAYAGG", "JSON_OBJECT", "JSON_OBJECTAGG", "LISTAGG", "MATCH", "OVERLAY", "POSITION",
    "SAFE_CAST", "STRING_AGG", "SUBSTR", "SUBSTRING", "TRIM", "TRY_CAST",
];

/// The clause keywords an existing `ORDER BY` list may never contain: it is replaced
/// as a whole, so anything that is more than an ordering would be lost with it.
const CLAUSE_WORDS: &[&str] = &[
    "EXCEPT", "FETCH", "FOR", "FORMAT", "FROM", "GROUP", "HAVING", "INTERSECT", "INTO", "LIMIT",
    "LOCK", "MINUS", "OFFSET", "ORDER", "QUALIFY", "SELECT", "SETTINGS", "UNION", "VALUES",
    "WHERE", "WINDOW", "WITH",
];

/// A statement lexed for the session's dialect, ready to be edited in place.
struct SortScan<'a> {
    sql: &'a str,
    dialect: SqlDialect,
    tokens: Vec<SqlToken>,
}

impl<'a> SortScan<'a> {
    /// The lexer reading `dialect`'s statements: the MySQL family (backtick
    /// identifiers or backslash escapes; MariaDB and ClickHouse, which read the same simple
    /// `SELECT * FROM … WHERE …`) with `"` an identifier when the session quotes with
    /// it or the driver reports it (`ANSI_QUOTES`); PostgreSQL where `LIKE` needs a cast;
    /// SQLite for the other double-quoting dialects. Backslash escapes are the session's own;
    /// `[name]` identifiers are added when the driver reports them (MariaDB `MSSQL`).
    fn lex_options(dialect: SqlDialect) -> SqlLexOptions {
        let mut options = if dialect.identifier_quote == '`' || dialect.backslash_escapes {
            // `mariadb()`: MariaDB shares this dialect and runs `/*M! … */` as code.
            let mysql = SqlLexOptions::mariadb();
            if dialect.identifier_quote == '"' {
                // Quoted with `"` yet escaping backslashes: the engine is not known well
                // enough to read its `#` comments, so a statement with one is refused
                // rather than guessed at.
                let mut ansi = mysql.with_ansi_quotes();
                ansi.hash_comments = false;
                ansi
            } else if dialect.double_quote_identifiers {
                // `ANSI_QUOTES` as the driver reports it: still MySQL / MariaDB (or
                // ClickHouse), whose `#` comments the lexer reads as they are.
                mysql.with_ansi_quotes()
            } else {
                mysql
            }
        } else if dialect.like_requires_text_cast {
            SqlLexOptions::postgres()
        } else {
            SqlLexOptions::sqlite()
        };
        options.backslash_escapes = dialect.backslash_escapes;
        // Only ever added: a driver that predates the flag reports `false` for SQLite.
        options.bracket_identifiers |= dialect.bracket_identifiers;
        options
    }

    /// Lexes `sql` and refuses text whose structure cannot be known for sure: an
    /// unterminated literal or comment, an executable `/*! … */` comment, unbalanced
    /// parentheses, a character no sortable statement holds, a quoted token with a
    /// backslash. A backslash escapes or not depending on the engine and, on PostgreSQL,
    /// on a session setting (`standard_conforming_strings`) this scan cannot see.
    fn new(dialect: SqlDialect, sql: &'a str) -> Result<Self, SortError> {
        let options = Self::lex_options(dialect);
        let scan = tokens(sql, &options);
        // Engines whose block comments do not nest still refuse text where nesting
        // would read it differently (`/* a /* b */ c */`): the reading is not obvious.
        if !options.nested_block_comments {
            let nested = tokens(
                sql,
                &SqlLexOptions {
                    nested_block_comments: true,
                    ..options
                },
            );
            if nested != scan {
                return Err(SortError::Unreadable("it has a nested /* comment"));
            }
        }
        if scan.unterminated {
            return Err(SortError::Unreadable("a string, identifier or comment is not closed"));
        }
        if scan.executable_comment {
            return Err(SortError::Unreadable("it has an executable /*! … */ comment"));
        }
        let this = Self {
            sql,
            dialect,
            tokens: scan.tokens,
        };
        let mut depth = 0_usize;
        for token in &this.tokens {
            match token.kind {
                SqlTokenKind::Open => depth += 1,
                SqlTokenKind::Close => {
                    depth = depth
                        .checked_sub(1)
                        .ok_or(SortError::Unreadable("the parentheses do not match"))?;
                }
                // `$` inside a name (`a$1`) is legal on some engines and not on others.
                SqlTokenKind::Word if this.text(token).contains('$') => {
                    return Err(SortError::Unreadable("it has an unexpected character"));
                }
                SqlTokenKind::Other if !PUNCTUATION.contains(&this.sql.as_bytes()[token.range.start]) => {
                    return Err(SortError::Unreadable("it has an unexpected character"));
                }
                SqlTokenKind::Quoted if this.text(token).contains('\\') => {
                    return Err(SortError::NotSimple);
                }
                _ => {}
            }
        }
        if depth != 0 {
            return Err(SortError::Unreadable("the parentheses do not match"));
        }
        Ok(this)
    }

    fn text(&self, token: &SqlToken) -> &'a str {
        &self.sql[token.range.clone()]
    }

    fn is_word(&self, token: &SqlToken, word: &str) -> bool {
        token.kind == SqlTokenKind::Word && self.text(token).eq_ignore_ascii_case(word)
    }

    fn is_punct(&self, token: &SqlToken, byte: u8) -> bool {
        token.kind == SqlTokenKind::Other && self.sql.as_bytes()[token.range.start] == byte
    }

    /// A word that is a keyword or a special form in an expression, never a column.
    fn is_keyword(&self, token: &SqlToken) -> bool {
        self.word_in(token, NOT_A_COLUMN) || self.word_in(token, SPECIAL_CALLS)
    }

    fn word_in(&self, token: &SqlToken, words: &[&str]) -> bool {
        token.kind == SqlTokenKind::Word
            && words.iter().any(|word| self.text(token).eq_ignore_ascii_case(word))
    }

    /// Index of the first word of `words` at parenthesis depth 0.
    fn top_level_word(&self, tokens: &[SqlToken], words: &[&str]) -> Option<usize> {
        let mut depth = 0_usize;
        tokens.iter().position(|token| match token.kind {
            SqlTokenKind::Open => {
                depth += 1;
                false
            }
            SqlTokenKind::Close => {
                depth = depth.saturating_sub(1);
                false
            }
            _ => depth == 0 && self.word_in(token, words),
        })
    }

    /// The name an identifier token spells: an unquoted word (not a clause keyword) or a
    /// quoted identifier (see [`Self::quote_of`]), a doubled quote read as one.
    fn identifier(&self, token: &SqlToken) -> Option<String> {
        let text = self.text(token);
        match token.kind {
            SqlTokenKind::Word => (!self.word_in(token, CLAUSE_WORDS)).then(|| text.to_string()),
            SqlTokenKind::Quoted => {
                let quote = self.quote_of(token)?;
                let inner = text.strip_prefix(quote)?.strip_suffix(quote)?;
                let doubled: String = [quote, quote].iter().collect();
                Some(inner.replace(&doubled, &quote.to_string()))
            }
            _ => None,
        }
    }

    /// The error for a keyword that follows the table (or sits in the `WHERE`):
    /// what kind of statement this is.
    fn refusal_at(&self, token: &SqlToken) -> SortError {
        const SET: &[&str] = &["UNION", "EXCEPT", "INTERSECT", "MINUS"];
        const JOIN: &[&str] =
            &["JOIN", "INNER", "LEFT", "RIGHT", "FULL", "CROSS", "NATURAL", "STRAIGHT_JOIN"];
        const UNSAFE: &[&str] = &["WITH", "OFFSET", "FETCH", "FOR", "LOCK", "SETTINGS", "FORMAT"];
        const SIMPLE: &[&str] = &["GROUP", "HAVING", "WINDOW", "QUALIFY", "INTO", "DISTINCT"];
        match token.kind {
            SqlTokenKind::Comma => SortError::NotOneTable,
            _ if self.word_in(token, SET) => SortError::SetQuery,
            _ if self.word_in(token, JOIN) => SortError::Joined,
            _ if self.word_in(token, UNSAFE) => SortError::Unsafe,
            _ if self.word_in(token, SIMPLE) => SortError::NotSimple,
            _ => SortError::NotPlainTable,
        }
    }

    /// `body` is `SELECT * FROM <table>`: the table's name parts and what follows them.
    fn table_name(&self, body: &'a [SqlToken]) -> Result<(Vec<String>, &'a [SqlToken]), SortError> {
        let Some(first) = body.first() else {
            return Err(SortError::NotPlainTable);
        };
        let mut parts = vec![self.identifier(first).ok_or(SortError::NotPlainTable)?];
        let mut rest = &body[1..];
        while let [dot, name, tail @ ..] = rest {
            if !self.is_punct(dot, b'.') {
                break;
            }
            parts.push(self.identifier(name).ok_or(SortError::NotPlainTable)?);
            rest = tail;
        }
        if rest.first().is_some_and(|token| self.is_punct(token, b'.')) {
            return Err(SortError::NotPlainTable);
        }
        Ok((parts, rest))
    }

    /// A `WHERE` condition that only compares columns and literals: operands are
    /// plain column names, `NULL`/`TRUE`/`FALSE`, plain numbers and strings (or
    /// identifiers in the dialect's quote), joined by `AND`/`OR` and `= <> != < <= > >=`
    /// with parentheses to group. The text is kept as typed, so everything else
    /// (functions, `IN`, `LIKE`, `IS`, arithmetic, subqueries, casts) is refused.
    fn check_filter(&self, tokens: &[SqlToken]) -> Result<(), SortError> {
        const OPERATORS: &[&str] = &["=", "<>", "!=", "<", "<=", ">", ">="];
        let mut want_operand = true;
        let mut depth = 0_usize;
        let mut index = 0;
        while let Some(token) = tokens.get(index) {
            index += 1;
            match (want_operand, token.kind) {
                (true, SqlTokenKind::Open) => depth += 1,
                (false, SqlTokenKind::Close) if depth > 0 => depth -= 1,
                (true, SqlTokenKind::Word) => {
                    let is_call = tokens
                        .get(index)
                        .is_some_and(|next| next.kind == SqlTokenKind::Open);
                    if is_call || self.is_keyword(token) {
                        return Err(match self.refusal_at(token) {
                            SortError::NotPlainTable => SortError::NotSimple,
                            error => error,
                        });
                    }
                    want_operand = false;
                }
                (true, SqlTokenKind::Number) => {
                    let text = self.text(token);
                    let digits = |part: &str| !part.is_empty() && part.bytes().all(|b| b.is_ascii_digit());
                    let plain = match text.split_once('.') {
                        Some((whole, fraction)) => digits(whole) && digits(fraction),
                        None => digits(text),
                    };
                    if !plain {
                        return Err(SortError::NotSimple);
                    }
                    want_operand = false;
                }
                (true, SqlTokenKind::Quoted) => {
                    let first = self.text(token).chars().next().unwrap_or_default();
                    if !matches!(first, '\'' | '"') && first != self.dialect.identifier_quote {
                        return Err(SortError::NotSimple);
                    }
                    want_operand = false;
                }
                (false, SqlTokenKind::Word) if self.word_in(token, &["AND", "OR"]) => {
                    want_operand = true;
                }
                (false, SqlTokenKind::Other) => {
                    // A comparison operator is adjacent punctuation (`<` `=`), never
                    // spaced out.
                    let start = token.range.start;
                    let mut end = token.range.end;
                    while let Some(next) = tokens.get(index) {
                        if next.kind != SqlTokenKind::Other || next.range.start != end {
                            break;
                        }
                        end = next.range.end;
                        index += 1;
                    }
                    if !OPERATORS.contains(&&self.sql[start..end]) {
                        return Err(SortError::NotSimple);
                    }
                    want_operand = true;
                }
                (_, SqlTokenKind::Word) => {
                    return Err(match self.refusal_at(token) {
                        SortError::NotPlainTable => SortError::NotSimple,
                        error => error,
                    });
                }
                _ => return Err(SortError::NotSimple),
            }
        }
        if want_operand || depth != 0 {
            return Err(SortError::NotSimple);
        }
        Ok(())
    }

    /// Whether `token` can be a column or function name: an unquoted word that is not
    /// a keyword or a literal word, or an identifier in the dialect's quote.
    fn is_name(&self, token: &SqlToken) -> bool {
        match token.kind {
            SqlTokenKind::Word => {
                !self.is_keyword(token) && !self.word_in(token, &["NULL", "TRUE", "FALSE"])
            }
            SqlTokenKind::Quoted => self.text(token).starts_with(self.dialect.identifier_quote),
            _ => false,
        }
    }

    /// The quote a quoted token names an identifier with: the dialect's own, or `"` on
    /// a backtick dialect (ClickHouse reads both; on MySQL it is a string that no
    /// statement on screen can have used as a table).
    fn quote_of(&self, token: &SqlToken) -> Option<char> {
        let first = self.text(token).chars().next()?;
        let quote = self.dialect.identifier_quote;
        (first == quote || (quote == '`' && first == '"')).then_some(first)
    }

    /// An `ORDER BY` operand at `tokens[*at]`: a (dotted) column, a plain number or
    /// string, a function call `f(expr, …)` or a parenthesised expression. Advances
    /// `at` past it; `false` for anything else (including unary operators).
    fn order_operand(&self, tokens: &[SqlToken], at: &mut usize) -> bool {
        let Some(token) = tokens.get(*at) else {
            return false;
        };
        *at += 1;
        match token.kind {
            SqlTokenKind::Open => {
                if !self.order_expression(tokens, at) {
                    return false;
                }
                let closed = tokens.get(*at).is_some_and(|next| next.kind == SqlTokenKind::Close);
                *at += usize::from(closed);
                closed
            }
            SqlTokenKind::Number => {
                let text = self.text(token);
                !text.is_empty() && text.bytes().all(|byte| byte.is_ascii_digit())
            }
            SqlTokenKind::Quoted | SqlTokenKind::Word if self.is_name(token) => {
                if tokens.get(*at).is_some_and(|next| next.kind == SqlTokenKind::Open) {
                    return token.kind == SqlTokenKind::Word && self.order_arguments(tokens, at);
                }
                // `schema.table.column`
                while let [dot, name, ..] = &tokens[*at..] {
                    if !self.is_punct(dot, b'.') || !self.is_name(name) {
                        break;
                    }
                    *at += 2;
                }
                true
            }
            SqlTokenKind::Quoted => {
                matches!(self.text(token).as_bytes().first(), Some(b'\'' | b'"'))
            }
            _ => false,
        }
    }

    /// At `(` of a call: `(expr, …)`, possibly empty.
    fn order_arguments(&self, tokens: &[SqlToken], at: &mut usize) -> bool {
        *at += 1;
        if tokens.get(*at).is_some_and(|next| next.kind == SqlTokenKind::Close) {
            *at += 1;
            return true;
        }
        loop {
            if !self.order_expression(tokens, at) {
                return false;
            }
            match tokens.get(*at).map(|next| next.kind) {
                Some(SqlTokenKind::Comma) => *at += 1,
                Some(SqlTokenKind::Close) => {
                    *at += 1;
                    return true;
                }
                _ => return false,
            }
        }
    }

    /// `operand ((+ | - | * | / | %) operand)*`.
    fn order_expression(&self, tokens: &[SqlToken], at: &mut usize) -> bool {
        loop {
            if !self.order_operand(tokens, at) {
                return false;
            }
            match tokens.get(*at) {
                Some(next) if next.kind == SqlTokenKind::Other
                    && b"+-*/%".contains(&self.sql.as_bytes()[next.range.start]) =>
                {
                    *at += 1;
                }
                _ => return true,
            }
        }
    }

    /// An existing `ORDER BY` list (replaced as a whole) is `expression [ASC | DESC]
    /// [NULLS FIRST | LAST]`, comma separated: anything else (a clause keyword, `WITH
    /// FILL`, a collation, a cast) could mean more than an ordering.
    fn check_order_list(&self, tokens: &[SqlToken]) -> Result<(), SortError> {
        if let Some(token) = tokens.iter().find(|token| self.word_in(token, CLAUSE_WORDS)) {
            return Err(match self.refusal_at(token) {
                SortError::NotPlainTable => SortError::NotSimple,
                error => error,
            });
        }
        let mut at = 0;
        loop {
            if !self.order_expression(tokens, &mut at) {
                return Err(SortError::NotSimple);
            }
            if tokens.get(at).is_some_and(|token| self.word_in(token, &["ASC", "DESC"])) {
                at += 1;
            }
            if tokens.get(at).is_some_and(|token| self.is_word(token, "NULLS")) {
                if !tokens.get(at + 1).is_some_and(|token| self.word_in(token, &["FIRST", "LAST"])) {
                    return Err(SortError::NotSimple);
                }
                at += 2;
            }
            match tokens.get(at) {
                None => return Ok(()),
                Some(token) if token.kind == SqlTokenKind::Comma => at += 1,
                Some(_) => return Err(SortError::NotSimple),
            }
        }
    }

    /// `sql` with its `ORDER BY` set to `order`: the statement's own text, edited in
    /// place after checking it is `SELECT * FROM <this table> [WHERE <comparisons>]
    /// [ORDER BY …] [LIMIT <n>]` and nothing else.
    fn sorted(&self, schema: &str, table: &str, order: &str) -> Result<String, SortError> {
        let end = self
            .tokens
            .iter()
            .rposition(|token| token.kind != SqlTokenKind::Semicolon)
            .map_or(0, |last| last + 1);
        let body = &self.tokens[..end];
        if body.is_empty() || body.iter().any(|token| token.kind == SqlTokenKind::Semicolon) {
            return Err(SortError::NotOneStatement);
        }
        let [select, rest @ ..] = body else {
            return Err(SortError::NotOneStatement);
        };
        if !self.is_word(select, "SELECT") {
            return Err(match select.kind {
                SqlTokenKind::Open => SortError::SetQuery,
                _ if self.is_word(select, "WITH") => SortError::Unsafe,
                _ => SortError::NotSelect,
            });
        }
        let [star, from, rest @ ..] = rest else {
            return Err(SortError::NotSimple);
        };
        if !self.is_punct(star, b'*') || !self.is_word(from, "FROM") {
            return Err(SortError::NotSimple);
        }
        let (parts, rest) = self.table_name(rest)?;
        // What follows the table: `[WHERE …]`, then `[ORDER BY …]`, then `[LIMIT n]`.
        let clauses = self.top_level_word(rest, &["ORDER", "LIMIT"]).unwrap_or(rest.len());
        let (filter, tail) = rest.split_at(clauses);
        match filter {
            [] => {}
            [first, condition @ ..] if self.is_word(first, "WHERE") => self.check_filter(condition)?,
            [first, ..] => return Err(self.refusal_at(first)),
        }
        let (order_clause, limit) = match tail {
            [keyword, by, list @ ..] if self.is_word(keyword, "ORDER") && self.is_word(by, "BY") => {
                let split = self.top_level_word(list, &["LIMIT"]).unwrap_or(list.len());
                let (list, limit) = list.split_at(split);
                self.check_order_list(list)?;
                (Some((keyword, list)), limit)
            }
            [keyword, ..] if self.is_word(keyword, "ORDER") => return Err(SortError::NotSimple),
            _ => (None, tail),
        };
        match limit {
            [] => {}
            [keyword, count]
                if self.is_word(keyword, "LIMIT")
                    && count.kind == SqlTokenKind::Number
                    && self.text(count).parse::<u64>().is_ok() => {}
            _ => return Err(SortError::Unsafe),
        }
        let target_matches = match parts.as_slice() {
            [actual] => actual == table,
            [actual_schema, actual_table] => actual_schema == schema && actual_table == table,
            _ => false,
        };
        if !target_matches {
            return Err(SortError::OtherTable);
        }
        let (range, mut text) = match order_clause {
            Some((keyword, list)) => (
                keyword.range.start..list.last().map_or(keyword.range.end, |last| last.range.end),
                order.to_string(),
            ),
            None => {
                // Right after the last token before `LIMIT`, so a comment that ends
                // the line cannot swallow the clause.
                let at = body[..body.len() - limit.len()]
                    .last()
                    .map_or(0, |token| token.range.end);
                (at..at, format!(" {order}"))
            }
        };
        // Whatever follows (`LIMIT`, a string, a comment) must not be glued to the clause.
        if self.sql[range.end..]
            .bytes()
            .next()
            .is_some_and(|next| !next.is_ascii_whitespace() && next != b';')
        {
            text.push(' ');
        }
        let mut sorted = String::with_capacity(self.sql.len() + text.len());
        sorted.push_str(&self.sql[..range.start]);
        sorted.push_str(&text);
        sorted.push_str(&self.sql[range.end..]);
        Ok(sorted)
    }
}

/// `sql` (a plain single-table `SELECT * FROM … [WHERE …] [ORDER BY …] [LIMIT n]`)
/// with its `ORDER BY` set to `column`, quoted in `dialect`'s own quoting. The
/// statement's text is kept as typed, only the `ORDER BY` is replaced or added
/// (before `LIMIT`). `Err` says why a statement cannot be re-ordered.
pub(in crate::app) fn sorted_sql_query(
    dialect: SqlDialect,
    sql: &str,
    schema: &str,
    table: &str,
    column: &str,
    descending: bool,
) -> Result<String, SortError> {
    if column.is_empty() || column.contains('\0') || table.is_empty() {
        return Err(SortError::InvalidTarget);
    }
    if dialect.backslash_escapes && column.contains('\\') {
        return Err(SortError::ColumnNotWritable);
    }
    if !matches!(dialect.identifier_quote, '"' | '`') {
        return Err(SortError::UnsupportedQuoting);
    }
    let order = format!(
        "ORDER BY {} {}",
        dialect.quote(column),
        if descending { "DESC" } else { "ASC" }
    );
    SortScan::new(dialect, sql)?.sorted(schema, table, &order)
}

pub(in crate::app) struct ContextEdit {
    detail: CellDetail,
    payload: InlineEditPayload,
    mongo_update: Option<PendingMongoMutation>,
}

struct SnowflakeTarget {
    tab_id: u64,
    query_generation: u64,
    page: crate::grid::GridState<crate::grid_source::GridContentSource>,
    selection: crate::grid::GridSelection,
    context_cell: Option<CellPosition>,
    visible_columns: Vec<usize>,
    request: u64,
    pending_edits: Vec<crate::workspace::PendingCellEdit>,
    pending_rows: Vec<crate::workspace::PendingSqlRow>,
    pending_mongo: crate::mongo_result::MongoMutationHistory,
}

impl SnowflakeTarget {
    fn capture(app: &AppRoot, tab_id: u64) -> Result<Self, String> {
        let tab = app.workspace.tab(tab_id).ok_or("No active result tab")?;
        let view = app.view(tab_id);
        let selection = view.grid_ui.selection.borrow().clone();
        if selection.range.is_none() {
            return Err("Select cells before generating Snowflake IDs".into());
        }
        let runtime = app
            .workspace
            .runtime(tab.id)
            .ok_or("Result tab unavailable")?;
        Ok(Self {
            tab_id: tab.id,
            query_generation: runtime.query.generation(),
            page: app.grid(tab_id).clone(),
            selection,
            context_cell: view.grid_ui.context_cell,
            visible_columns: view.grid_ui.visible_columns.clone(),
            request: app.grid_prefs.snowflake_request,
            pending_edits: runtime.pending_edits.clone(),
            pending_rows: runtime.pending_sql_rows.clone(),
            pending_mongo: runtime.mongo.mutations.clone(),
        })
    }

    fn matches(&self, app: &AppRoot) -> bool {
        let view = app.view(self.tab_id);
        // The dialog belongs to the tab the user was working in: switching
        // away invalidates it.
        app.workspace
            .active_tab()
            .is_some_and(|tab| tab.id == self.tab_id)
            && app.grid(self.tab_id).same_page(&self.page)
            && *view.grid_ui.selection.borrow() == self.selection
            && view.grid_ui.context_cell == self.context_cell
            && view.grid_ui.visible_columns == self.visible_columns
            && app.grid_prefs.snowflake_request == self.request
            && app.workspace.runtime(self.tab_id).is_some_and(|runtime| {
                runtime.query.generation() == self.query_generation
                    && runtime.pending_edits == self.pending_edits
                    && runtime.pending_sql_rows == self.pending_rows
                    && runtime.mongo.mutations == self.pending_mongo
            })
    }
}

impl AppRoot {
    fn snowflake_directory(&self) -> Result<std::path::PathBuf, String> {
        match &self.grid_prefs.snowflake_dir {
            Some(path) => Ok(path.clone()),
            None => SnowflakeStore::default_directory(),
        }
    }

    pub(in crate::app) fn open_context_snowflake_dialog(
        &mut self,
        tab_id: u64,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.open_context_snowflake_dialog_with_read(tab_id, SnowflakeStore::read_worker, window, cx);
    }

    pub(in crate::app) fn open_context_snowflake_dialog_with_read(
        &mut self,
        tab_id: u64,
        read: impl FnOnce(&std::path::Path) -> Result<Option<u16>, String> + Send + 'static,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let target = match SnowflakeTarget::capture(self, tab_id) {
            Ok(target) => target,
            Err(error) => {
                self.view_mut(tab_id).detail_ui.error = Some(error);
                self.notify_tab(tab_id, cx);
                return;
            }
        };
        let directory = match self.snowflake_directory() {
            Ok(path) => path,
            Err(error) => {
                self.view_mut(tab_id).detail_ui.error = Some(error);
                self.notify_tab(tab_id, cx);
                return;
            }
        };
        let handle = window.window_handle();
        let root = cx.entity().downgrade();
        cx.spawn(async move |this, cx| {
            let current = cx
                .background_executor()
                .spawn(async move { read(&directory) })
                .await;
            match current {
                Ok(current) => {
                    let _ = cx.update_window(handle, move |_, window, cx| {
                        let _ = root.update(cx, |this, cx| {
                            if target.matches(this) {
                                this.show_context_snowflake_dialog(current, target, window, cx);
                            } else {
                                this.view_mut(tab_id).detail_ui.error = Some(
                                    "Snowflake target changed while loading worker configuration"
                                        .into(),
                                );
                                this.notify_tab(tab_id, cx);
                            }
                        });
                    });
                }
                Err(error) => {
                    let _ = this.update(cx, |this, cx| {
                        this.report_error(Area::Mutation, None, &error);
                        this.view_mut(tab_id).detail_ui.error = Some(error);
                        this.notify_tab(tab_id, cx);
                    });
                }
            }
        })
        .detach();
    }

    fn show_context_snowflake_dialog(
        &mut self,
        current: Option<u16>,
        target: SnowflakeTarget,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let current = current.map(|worker| worker.to_string()).unwrap_or_default();
        let input = cx.new(|cx| InputState::new(window, cx).default_value(current));
        let root = cx.entity().downgrade();
        let target = std::rc::Rc::new(target);
        let error = std::rc::Rc::new(std::cell::RefCell::new(None::<String>));
        window.open_alert_dialog(cx, move |dialog, _, _| {
            let input = input.clone();
            let root = root.clone();
            let target = target.clone();
            let error = error.clone();
            let message = error.borrow().clone();
            dialog.title("Snowflake Worker ID")
                .child("Enter 0–1023. Use a unique worker ID across machines and users that do not share this state folder. Processes sharing this folder coordinate through a lock. Keep the state file; deleting or restoring it can reuse IDs.")
                .child(div().debug_selector(|| "snowflake-worker-input".into()).child(Input::new(&input)))
                .when_some(message, |dialog, message| dialog.child(div().text_color(theme::danger()).child(message)))
                .button_props(gpui_component::dialog::DialogButtonProps::default().ok_text("Stage Snowflake IDs"))
                .confirm()
                .on_ok(move |_, window, cx| {
                    let text = input.read(cx).value().to_string();
                    let result = text.parse::<u16>().map_err(|_| "Enter a worker ID from 0 to 1023".to_string())
                        .and_then(|worker| root.update(cx, |this, cx| {
                            if !target.matches(this) {
                                return Err("Snowflake target changed before confirmation; reopen the menu".into());
                            }
                            this.start_snowflake_generation(target.tab_id, worker, cx)
                        })
                            .map_err(|_| "Result tab was closed".to_string())?);
                    match result {
                        Ok(()) => true,
                        Err(message) => { *error.borrow_mut() = Some(message); window.refresh(); false }
                    }
                })
        });
    }

    fn check_snowflake_cell_types(
        &self,
        tab: &Tab,
        positions: &[CellPosition],
    ) -> Result<(), String> {
        let sqlite = tab.query_mode.is_sqlite_table()
            || self.saved_connections.iter().any(|saved| {
                saved.matches_id(&tab.connection_id) && saved.kind == ConnectionKind::Sqlite
            });
        for position in positions {
            let detail = self
                .build_cell_detail(tab, *position)
                .ok_or("Cell details unavailable")?;
            let kind = detail.type_name.to_ascii_lowercase();
            let exact = kind.contains("bigint")
                || kind.contains("int64")
                || kind == "int8"
                || (sqlite && kind == "integer");
            let text = kind.contains("text") || kind.contains("char") || kind.contains("string");
            if !exact && !text {
                return Err(format!(
                    "{} needs a BIGINT/int64 or text column to preserve Snowflake precision",
                    detail.column_name
                ));
            }
        }
        Ok(())
    }

    pub(in crate::app) fn start_snowflake_generation(
        &mut self,
        tab_id: u64,
        worker: u16,
        cx: &mut Context<Self>,
    ) -> Result<(), String> {
        if worker > 1023 {
            return Err("Worker ID must be between 0 and 1023".into());
        }
        let dir = self.snowflake_directory()?;
        let (tab, _, eligible, skipped) = self.eligible_generation_positions(tab_id)?;
        self.check_snowflake_cell_types(&tab, &eligible)?;
        let mut target = SnowflakeTarget::capture(self, tab_id)?;
        self.grid_prefs.snowflake_request = self.grid_prefs.snowflake_request.wrapping_add(1);
        target.request = self.grid_prefs.snowflake_request;
        let count = eligible.len();
        cx.spawn(async move |this, cx| {
            let result = cx.background_executor().spawn(async move {
                let now = std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH).map_err(|_| "Clock is before Unix epoch".to_string())?;
                let now = u64::try_from(now.as_millis()).map_err(|_| "Clock overflow".to_string())?;
                SnowflakeStore::configure_worker(&dir, worker)?;
                SnowflakeStore::reserve(&dir, worker, count, now)
            }).await;
            let _ = this.update(cx, |this, cx| {
                match result {
                    Err(error) => { this.report_error(Area::Mutation, None, &error); this.view_mut(tab_id).detail_ui.error = Some(error); this.notify_tab(tab_id, cx); }
                    Ok(ids) => {
                        if !target.matches(this) {
                            this.view_mut(tab_id).detail_ui.error = Some("Selection or staged changes changed while reserving Snowflake IDs; the reserved IDs were not used".into());
                            this.notify_tab(tab_id, cx);
                            return;
                        }
                        let result = this.eligible_generation_positions(tab_id).and_then(|(current_tab, _, positions, current_skipped)| {
                            if positions != eligible || current_skipped != skipped { return Err("Eligible cells changed before Snowflake staging".into()); }
                            this.check_snowflake_cell_types(&current_tab, &positions)?;
                            this.stage_generated_values(current_tab, positions, skipped, ids.into_iter().map(|value| Some(value.to_string())).collect(), cx)
                        });
                        if let Err(error) = result { this.view_mut(tab_id).detail_ui.error = Some(error); this.notify_tab(tab_id, cx); }
                    }
                }
            });
        }).detach();
        Ok(())
    }
    pub(in crate::app) fn refresh_tab_results(
        &mut self,
        tab_id: u64,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.refuse_while_editing_mongo_document(tab_id, cx) {
            return;
        }
        let Some(runtime) = self.workspace.runtime(tab_id) else {
            return;
        };
        // Refresh would run the script again (and a script may write): only Run does.
        if self.shell_view(tab_id).is_some() {
            return;
        }
        if runtime.sql_apply_running
            || matches!(
                runtime.query.status(),
                crate::query::QueryStatus::Running { .. }
            )
        {
            window.open_alert_dialog(cx, |dialog, _, _| {
                dialog
                    .title("Refresh unavailable")
                    .child("Wait for the current operation to finish.")
            });
            return;
        }
        let view = self.view(tab_id);
        let has_pending = runtime.has_staged_changes()
            || view.grid_ui.inline_edit.is_some()
            || view.detail_ui.edit.is_some();
        if !has_pending {
            self.rerun_shown_result(tab_id, false, cx);
            return;
        }
        let root = cx.entity().downgrade();
        window.open_alert_dialog(cx, move |dialog, _, _| {
            let root = root.clone();
            dialog
                .title("Discard staged changes and refresh?")
                .child("Cancel preserves all edits, inserts and deletions in this tab.")
                .button_props(
                    gpui_component::dialog::DialogButtonProps::default()
                        .ok_text("Discard and Refresh"),
                )
                .confirm()
                .on_ok(move |_, _, cx| {
                    let _ = root.update(cx, |this, cx| {
                        if this.workspace.tab(tab_id).is_none() {
                            return;
                        }
                        let view = this.view_mut(tab_id);
                        view.grid_ui.inline_edit = None;
                        view.detail_ui.edit = None;
                        if let Some(runtime) = this.workspace.runtime_mut(tab_id) {
                            runtime.pending_edits.clear();
                            runtime.pending_sql_rows.clear();
                            runtime.pending_deleted_rows.clear();
                            runtime.clear_pending_cell_texts();
                            runtime.mongo.mutations.rollback();
                            rebuild_mongo_pending_overlay(runtime);
                        }
                        this.rerun_shown_result(tab_id, false, cx);
                    });
                    true
                })
        });
    }
    pub(in crate::app) fn sql_sort_available(&self, tab_id: u64) -> bool {
        let Some(tab) = self.workspace.tab(tab_id) else {
            return false;
        };
        if !tab.query_mode.is_sql_table() {
            return false;
        }
        if self.scenario_ui.service.is_some() {
            return true;
        }
        let kind = self
            .saved_connections
            .iter()
            .find(|saved| saved.matches_id(&tab.connection_id))
            .map(|saved| saved.kind);
        kind.is_some_and(crate::navigator::sql_driver::is_sql_kind)
            && self.sessions.is_connected(&tab.connection_id)
    }
    pub(in crate::app) fn prepare_context_edits(
        &self,
        tab_id: u64,
        text: Option<&str>,
    ) -> Result<Vec<ContextEdit>, String> {
        let range = self
            .view(tab_id)
            .grid_ui
            .selection
            .borrow()
            .range
            .ok_or("Select one or more cells first")?;
        self.prepare_context_edits_range(tab_id, range, text)
    }

    fn prepare_context_edits_range(
        &self,
        tab_id: u64,
        range: crate::grid::CellRange,
        text: Option<&str>,
    ) -> Result<Vec<ContextEdit>, String> {
        let tab = self.workspace.tab(tab_id).ok_or("No active result tab")?;
        let view = self.view(tab_id);
        if view.grid_ui.inline_edit.is_some() || view.detail_ui.edit.is_some() {
            return Err("Finish or cancel the active cell editor first.".into());
        }
        if self
            .workspace
            .runtime(tab.id)
            .is_some_and(|runtime| runtime.sql_apply_running)
        {
            return Err("Wait for the SQL Apply operation to finish.".into());
        }
        let range = range.normalized();
        if range.end.row >= self.grid(tab_id).row_count()
            || range.end.column >= self.display_column_count(tab_id)
        {
            return Err("Selection is outside the current page".into());
        }
        if self.workspace.runtime(tab.id).is_some_and(|runtime| {
            (range.start.row..=range.end.row).any(|row| runtime.pending_deleted_rows.contains(&row))
        }) {
            return Err("Selection contains a row staged for deletion".into());
        }
        let mut edits = Vec::new();
        for row in range.start.row..=range.end.row {
            let inspected_row = (tab.query_mode.is_mongo_find())
                .then(|| self.mongo_row_fields(tab.id, row))
                .transpose()?;
            for column in range.start.column..=range.end.column {
                let position = CellPosition { row, column };
                let detail = self
                    .build_cell_detail_with_row(tab, position, inspected_row.as_deref())
                    .ok_or_else(|| {
                        format!(
                            "Cannot read cell at row {}, column {}",
                            position.row + 1,
                            position.column + 1
                        )
                    })?;
                if !detail.editable
                    || detail.column_name == "_id"
                    || self.workspace.runtime(tab.id).is_some_and(|runtime| {
                        runtime
                            .structure_columns
                            .iter()
                            .any(|column| column.name == detail.column_name && column.primary_key)
                    })
                {
                    return Err(format!("{} cannot be edited", detail.column_name));
                }
                if !detail.nullable && text.is_none() {
                    return Err(format!("{} cannot be NULL", detail.column_name));
                }
                let payload = self
                    .cell_detail_payload_with_row(tab, &detail, inspected_row.as_deref())
                    .ok_or("Row identity is unavailable")?;
                if matches!(payload, InlineEditPayload::ReadOnly) {
                    return Err(format!("{} is read-only", detail.column_name));
                }
                let mongo_update = if let Some(text) = text {
                    match &payload {
                        InlineEditPayload::Sql { data_type, .. } => {
                            let value = mutation_value_from_input(text, data_type)?;
                            if matches!(value, MutationValue::Null) && !detail.nullable {
                                return Err(format!("{} cannot be NULL", detail.column_name));
                            }
                            None
                        }
                        InlineEditPayload::Mongo { .. } => {
                            Some(build_inline_mongo_update(&payload, position.row, text)?)
                        }
                        InlineEditPayload::ReadOnly => unreachable!(),
                    }
                } else {
                    None
                };
                edits.push(ContextEdit {
                    detail,
                    payload,
                    mongo_update,
                });
            }
        }
        Ok(edits)
    }

    pub(in crate::app) fn stage_context_edits(
        &mut self,
        tab_id: u64,
        edits: Vec<ContextEdit>,
        text: Option<&str>,
        cx: &mut Context<Self>,
    ) -> Result<usize, String> {
        if let Some(message) = self.read_only_edit_message(tab_id) {
            return Err(message);
        }
        let runtime = self
            .workspace
            .runtime(tab_id)
            .ok_or("Result tab unavailable")?;
        if runtime.sql_apply_running
            || edits.iter().any(|edit| {
                runtime
                    .pending_deleted_rows
                    .contains(&edit.detail.position.row)
            })
        {
            return Err("Selection includes a deleted row or SQL Apply is in progress".into());
        }
        let saved_edits = runtime.pending_edits.clone();
        let saved_mongo = runtime.mongo.mutations.clone();
        let mut count = 0;
        for edit in edits {
            match self.stage_prepared_context_edit(edit, text, tab_id, cx) {
                Ok(staged) => count += usize::from(staged),
                Err(error) => {
                    self.restore_context_staging(tab_id, saved_edits, saved_mongo);
                    return Err(error);
                }
            }
        }
        self.refresh_cell_detail(tab_id);
        self.notify_tab(tab_id, cx);
        Ok(count)
    }

    fn stage_prepared_context_edit(
        &mut self,
        edit: ContextEdit,
        text: Option<&str>,
        tab_id: u64,
        cx: &mut Context<Self>,
    ) -> Result<bool, String> {
        let ContextEdit {
            detail,
            payload,
            mongo_update,
        } = edit;
        if let Some(text) = text {
            match payload {
                payload @ InlineEditPayload::Sql { .. } => {
                    if self.workspace.runtime(tab_id).is_some() {
                        let staged = self.stage_sql_edit(
                            tab_id,
                            detail.position,
                            &payload,
                            &detail.value,
                            text,
                        );
                        if !staged {
                            if text == detail.value.as_ref() {
                                return Ok(false);
                            }
                            return Err(
                                "SQL cell could not be staged; the queue was preserved".into()
                            );
                        }
                    } else {
                        return Err("Result tab was closed".into());
                    }
                }
                InlineEditPayload::Mongo { .. } => {
                    if let Some(update) = mongo_update {
                        self.stage_mongo_mutation(tab_id, update, cx);
                    } else {
                        return Err("Mongo update was not prepared".into());
                    }
                }
                InlineEditPayload::ReadOnly => return Err("Cell is read-only".into()),
            }
        } else {
            self.stage_cell_detail_null(tab_id, detail, payload, cx);
        }
        Ok(true)
    }

    fn restore_context_staging(
        &mut self,
        tab_id: u64,
        saved_edits: Vec<crate::workspace::PendingCellEdit>,
        saved_mongo: crate::mongo_result::MongoMutationHistory,
    ) {
        if let Some(runtime) = self.workspace.runtime_mut(tab_id) {
            runtime.pending_edits = saved_edits;
            runtime.clear_pending_cell_texts();
            for edit in runtime.pending_edits.clone() {
                runtime.set_pending_cell_text(
                    CellPosition {
                        row: edit.row,
                        column: edit.column,
                    },
                    edit.display_text,
                );
            }
            runtime.mongo.mutations = saved_mongo;
            if matches!(
                runtime.grid.source(),
                crate::grid_source::GridContentSource::Bson(_)
            ) {
                rebuild_mongo_pending_overlay(runtime);
            }
        }
    }

    pub(in crate::app) fn generate_context_selection(
        &mut self,
        tab_id: u64,
        kind: GenerateKind,
        cx: &mut Context<Self>,
    ) -> Result<(usize, usize), String> {
        let (tab, _, eligible, skipped) = self.eligible_generation_positions(tab_id)?;
        let values = generated_values(kind, eligible.len())?;
        self.stage_generated_values(tab, eligible, skipped, values, cx)
    }

    fn eligible_generation_positions(
        &self,
        tab_id: u64,
    ) -> Result<(Tab, crate::grid::CellRange, Vec<CellPosition>, usize), String> {
        let tab = self
            .workspace
            .tab(tab_id)
            .ok_or("No active result tab")?
            .clone();
        let range = self
            .view(tab_id)
            .grid_ui
            .selection
            .borrow()
            .range
            .ok_or("Select cells first")?
            .normalized();
        if range.end.row >= self.grid(tab_id).row_count()
            || range.end.column >= self.display_column_count(tab_id)
        {
            return Err("Selection is outside the current page".into());
        }
        if self.workspace.runtime(tab.id).is_some_and(|runtime| {
            (range.start.row..=range.end.row).any(|row| runtime.pending_deleted_rows.contains(&row))
        }) {
            return Err("Selection contains a row staged for deletion".into());
        }
        let mut eligible = Vec::new();
        let mut skipped = 0;
        for position in range.cells() {
            let detail = self
                .build_cell_detail(&tab, position)
                .ok_or("Cell details unavailable")?;
            let primary_key = self.workspace.runtime(tab.id).is_some_and(|runtime| {
                runtime
                    .structure_columns
                    .iter()
                    .any(|column| column.name == detail.column_name && column.primary_key)
            });
            if !detail.editable || detail.column_name == "_id" || primary_key {
                skipped += 1;
            } else {
                eligible.push(position);
            }
        }
        Ok((tab, range, eligible, skipped))
    }

    fn stage_generated_values(
        &mut self,
        tab: Tab,
        eligible: Vec<CellPosition>,
        skipped: usize,
        values: Vec<Option<String>>,
        cx: &mut Context<Self>,
    ) -> Result<(usize, usize), String> {
        if eligible.len() != values.len() {
            return Err("Generated value count changed before staging".into());
        }
        let mut prepared = Vec::with_capacity(eligible.len());
        for (position, value) in eligible.into_iter().zip(values) {
            let single = crate::grid::CellRange {
                start: position,
                end: position,
            };
            let mut edit = self.prepare_context_edits_range(tab.id, single, value.as_deref())?;
            prepared.push((edit.pop().ok_or("Cell cannot be staged")?, value));
        }
        let saved_edits = self
            .workspace
            .runtime(tab.id)
            .ok_or("Result tab unavailable")?
            .pending_edits
            .clone();
        let saved_mongo = self
            .workspace
            .runtime(tab.id)
            .ok_or("Result tab unavailable")?
            .mongo
            .mutations
            .clone();
        let mut count = 0;
        for (edit, value) in prepared {
            match self.stage_prepared_context_edit(edit, value.as_deref(), tab.id, cx) {
                Ok(staged) => count += usize::from(staged),
                Err(error) => {
                    self.restore_context_staging(tab.id, saved_edits, saved_mongo);
                    return Err(error);
                }
            }
        }
        self.refresh_cell_detail(tab.id);
        self.view_mut(tab.id).detail_ui.error = Some(format!(
            "Staged {count} values; skipped {skipped} read-only cells."
        ));
        cx.notify();
        Ok((count, skipped))
    }

    pub(in crate::app) fn open_context_increment_dialog(
        &mut self,
        tab_id: u64,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let input = cx.new(|cx| InputState::new(window, cx).default_value("1"));
        let root = cx.entity().downgrade();
        let error = std::rc::Rc::new(std::cell::RefCell::new(None::<String>));
        window.open_alert_dialog(cx, move |dialog, _, _| {
            let input = input.clone();
            let root = root.clone();
            let error = error.clone();
            let message = error.borrow().clone();
            dialog
                .title("Increment from")
                .child(Input::new(&input))
                .when_some(message, |dialog, message| {
                    dialog.child(div().text_color(theme::danger()).child(message))
                })
                .button_props(
                    gpui_component::dialog::DialogButtonProps::default().ok_text("Stage values"),
                )
                .confirm()
                .on_ok(move |_, window, cx| {
                    let value = input.read(cx).value().to_string();
                    let result = value
                        .parse::<i64>()
                        .map_err(|_| "Enter a valid signed integer".to_string())
                        .and_then(|start| {
                            root.update(cx, |this, cx| {
                                this.generate_context_selection(tab_id, GenerateKind::Increment(start), cx)
                            })
                            .map_err(|_| "Result tab was closed".to_string())?
                        });
                    match result {
                        Ok(_) => true,
                        Err(message) => {
                            *error.borrow_mut() = Some(message);
                            window.refresh();
                            false
                        }
                    }
                })
        });
    }

    pub(in crate::app) fn set_context_selection_null(
        &mut self,
        tab_id: u64,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        match self.prepare_context_edits(tab_id, None) {
            Ok(edits) => {
                if let Err(message) = self.stage_context_edits(tab_id, edits, None, cx) {
                    window.open_alert_dialog(cx, move |dialog, _, _| {
                        dialog.title("Cannot set NULL").child(message.clone())
                    });
                }
            }
            Err(message) => {
                window.open_alert_dialog(cx, move |dialog, _, _| {
                    dialog.title("Cannot set NULL").child(message.clone())
                });
            }
        }
    }

    pub(in crate::app) fn open_context_bulk_edit(
        &mut self,
        tab_id: u64,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let input = cx.new(|cx| InputState::new(window, cx).default_value(""));
        let root = cx.entity().downgrade();
        let error = std::rc::Rc::new(std::cell::RefCell::new(None::<String>));
        window.open_alert_dialog(cx, move |dialog, _, _| {
            let input = input.clone();
            let root = root.clone();
            let error = error.clone();
            let message = error.borrow().clone();
            dialog
                .title("Bulk edit selected cells")
                .child(Input::new(&input))
                .when_some(message, |dialog, message| {
                    dialog.child(div().text_color(theme::danger()).child(message))
                })
                .button_props(
                    gpui_component::dialog::DialogButtonProps::default().ok_text("Stage changes"),
                )
                .confirm()
                .on_ok(move |_, window, cx| {
                    let text = input.read(cx).value().to_string();
                    let result = root.update(cx, |this, cx| {
                        let value = (!text.is_empty()).then_some(text.as_str());
                        let edits = this.prepare_context_edits(tab_id, value)?;
                        this.stage_context_edits(tab_id, edits, value, cx)?;
                        Ok::<_, String>(())
                    });
                    match result {
                        Ok(Ok(())) => true,
                        Ok(Err(message)) => {
                            *error.borrow_mut() = Some(message);
                            window.refresh();
                            false
                        }
                        Err(_) => false,
                    }
                })
        });
    }

    pub(in crate::app) fn sort_context_page(
        &mut self,
        tab_id: u64,
        descending: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(target) = self.view(tab_id).grid_ui.context_cell else {
            return;
        };
        let Some(column) = self.source_column_for_display(tab_id, target.column) else {
            return;
        };
        if self.context_sort_blocked(tab_id) {
            window.open_alert_dialog(cx, |dialog, _, _| {
                dialog.title("Sort unavailable").child(
                    "Wait for the query and finish pending changes before reordering this page.",
                )
            });
            return;
        }
        // The column's resolved type (catalog, then the engine's own type
        // name), not its Arrow label: `number` hides decimals and u64 alike,
        // and a PostgreSQL `numeric` arrives as text.
        let kind = match self
            .resolved_column_type(tab_id, column)
            .map_or(ValueKind::Text, |type_name| value_kind(&type_name))
        {
            ValueKind::Integer => crate::grid::SortKind::Integer,
            ValueKind::Decimal | ValueKind::Float => crate::grid::SortKind::Number,
            ValueKind::Boolean => crate::grid::SortKind::Boolean,
            ValueKind::Temporal => crate::grid::SortKind::Temporal,
            ValueKind::Binary | ValueKind::Json | ValueKind::Text => crate::grid::SortKind::Text,
        };
        let mut grid = self.grid(tab_id).clone();
        if let Err(message) = grid.sort_page(column, kind, descending) {
            window.open_alert_dialog(cx, move |dialog, _, _| {
                dialog.title("Sort failed").child(message.clone())
            });
            return;
        }
        if let Some(runtime) = self.workspace.runtime_mut(tab_id) {
            runtime.grid = grid;
            runtime.cell_detail.close();
        }
        let grid_ui = &mut self.view_mut(tab_id).grid_ui;
        grid_ui.selection.borrow_mut().clear();
        grid_ui.context_cell = None;
        self.notify_tab(tab_id, cx);
    }

    pub(in crate::app) fn sort_context_database(
        &mut self,
        tab_id: u64,
        descending: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(tab) = self.workspace.tab(tab_id).cloned() else {
            return;
        };
        let sql_table = tab.query_mode.is_sql_table();
        if !tab.query_mode.is_mongo_find() && !sql_table {
            return;
        }
        if sql_table && !self.sql_sort_available(tab_id) {
            window.open_alert_dialog(cx, |dialog, _, _| {
                dialog
                    .title("Database sort unavailable")
                    .child("This SQL connection cannot run a table sort.")
            });
            return;
        }
        let Some(target) = self.view(tab_id).grid_ui.context_cell else {
            return;
        };
        let Some(column) = self.source_column_for_display(tab_id, target.column) else {
            return;
        };
        let Some(name) = self.grid(tab_id).column_name(column) else {
            return;
        };
        if self.context_sort_blocked(tab.id) {
            window.open_alert_dialog(cx, |dialog, _, _| {
                dialog.title("Sort unavailable").child(
                    "Wait for the query and finish pending changes before sorting the database.",
                )
            });
            return;
        }
        if sql_table && tab.query_mode.is_table_data() {
            let term = crate::sql_table::sort_term(
                self.sql_table_dialect(&tab),
                name,
                descending,
            );
            self.set_sql_table_order_by(tab.id, &term, window, cx);
            return;
        }
        if sql_table {
            // Sort the statement whose rows are on screen, never the editor
            // text (it may hold anything typed since the last run).
            // Scenario fixtures seed rows without executing anything; their
            // tab SQL is the statement on screen.
            let scenario = self.scenario_ui.service.is_some();
            let Some(base) = (if scenario {
                Some(tab.sql.clone())
            } else {
                self.shown_sql_base(tab.id)
            }) else {
                return;
            };
            let result = sorted_sql_query(
                self.sql_table_dialect(&tab),
                &base,
                tab.schema.as_deref().unwrap_or("public"),
                &tab.collection,
                name,
                descending,
            );
            match result {
                Ok(sorted) => {
                    if let Some(runtime) = self.workspace.runtime_mut(tab.id) {
                        runtime.context_sql_sort = Some((base, sorted.clone()));
                        runtime.pagination.reset();
                    }
                    if scenario {
                        self.run_tab_query(tab_id, cx);
                    } else if !self.refuse_run_over_staged_changes(tab_id, cx) {
                        self.run_sql(tab.id, sorted, Some(0), cx);
                    }
                }
                Err(error) => {
                    let error = error.to_string();
                    window.open_alert_dialog(cx, move |dialog, _, _| {
                        dialog
                            .title("Database sort unavailable")
                            .child(error.clone())
                    });
                }
            }
            return;
        }
        let sort = serde_json::json!({name: if descending { -1 } else { 1 }}).to_string();
        if let Some(runtime) = self.workspace.runtime_mut(tab.id) {
            runtime.mongo.sort = sort.clone();
            runtime.pagination.reset();
        }
        if let Some(editor) = self.mongo_ui.sort_editors.get(&tab.id).cloned() {
            editor.update(cx, |state, cx| state.set_value(sort.clone(), window, cx));
        }
        if let Some(editor) = self.mongo_ui.expanded_sort_editors.get(&tab.id).cloned() {
            editor.update(cx, |state, cx| state.set_value(sort, window, cx));
        }
        self.run_tab_query(tab_id, cx);
    }

    pub(in crate::app) fn clear_context_page_sort(
        &mut self,
        tab_id: u64,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.context_sort_blocked(tab_id) {
            window.open_alert_dialog(cx, |dialog, _, _| {
                dialog.title("Sort unavailable").child(
                    "Wait for the query and finish pending changes before restoring page order.",
                )
            });
            return;
        }
        if let Some(runtime) = self.workspace.runtime_mut(tab_id) {
            runtime.grid.clear_page_sort();
            runtime.cell_detail.close();
        }
        let grid_ui = &mut self.view_mut(tab_id).grid_ui;
        grid_ui.selection.borrow_mut().clear();
        grid_ui.context_cell = None;
        self.notify_tab(tab_id, cx);
    }

    pub(in crate::app) fn clear_context_sort(
        &mut self,
        tab_id: u64,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(tab) = self.workspace.tab(tab_id).cloned() else {
            return;
        };
        if tab.query_mode.is_table_data() {
            if self.context_sort_blocked(tab.id) {
                self.clear_context_page_sort(tab_id, window, cx);
                return;
            }
            self.clear_context_page_sort(tab_id, window, cx);
            if !tab.applied_order_by.trim().is_empty() {
                self.clear_sql_table_field(tab.id, results::sql_table::SqlField::OrderBy, window, cx);
            }
            return;
        }
        if !tab.query_mode.is_mongo_find() {
            if self.context_sort_blocked(tab.id) {
                self.clear_context_page_sort(tab_id, window, cx);
                return;
            }
            // Clearing a database sort re-runs its unsorted base statement,
            // never the editor text.
            let base = self
                .workspace
                .runtime_mut(tab.id)
                .and_then(|runtime| runtime.context_sql_sort.take())
                .map(|(base, _)| base);
            if base.is_some() {
                if let Some(runtime) = self.workspace.runtime_mut(tab.id) {
                    runtime.pagination.reset();
                }
            }
            self.clear_context_page_sort(tab_id, window, cx);
            if let Some(base) = base {
                if self.scenario_ui.service.is_some() {
                    self.run_tab_query(tab_id, cx);
                } else if !self.refuse_run_over_staged_changes(tab_id, cx) {
                    self.run_sql(tab.id, base, Some(0), cx);
                }
            }
            return;
        }
        if self.context_sort_blocked(tab.id) {
            window.open_alert_dialog(cx, |dialog, _, _| {
                dialog
                    .title("Sort unavailable")
                    .child("Wait for the query and finish pending changes before clearing sort.")
            });
            return;
        }
        if let Some(runtime) = self.workspace.runtime_mut(tab.id) {
            runtime.grid.clear_page_sort();
            runtime.pagination.reset();
            runtime.cell_detail.close();
        }
        let grid_ui = &mut self.view_mut(tab_id).grid_ui;
        grid_ui.selection.borrow_mut().clear();
        grid_ui.context_cell = None;
        self.clear_mongo_sort(tab.id, window, cx);
        self.run_tab_query(tab_id, cx);
    }

    pub(in crate::app) fn open_context_cell_details(&mut self, tab_id: u64, cx: &mut Context<Self>) {
        let Some(target) = self.view(tab_id).grid_ui.context_cell else {
            return;
        };
        self.select_grid_cell(tab_id, target);
        self.open_cell_detail_tab(tab_id, CellDetailTab::Details, cx);
    }

    pub(in crate::app) fn open_context_row_details(&mut self, tab_id: u64, cx: &mut Context<Self>) {
        let Some(target) = self.view(tab_id).grid_ui.context_cell else {
            return;
        };
        self.select_grid_cell(tab_id, target);
        self.open_cell_detail_tab(tab_id, CellDetailTab::Row, cx);
    }

    pub(in crate::app) fn open_context_column_details(&mut self, tab_id: u64, cx: &mut Context<Self>) {
        let Some(target) = self.view(tab_id).grid_ui.context_cell else {
            return;
        };
        self.select_grid_cell(tab_id, target);
        self.open_cell_detail_tab(tab_id, CellDetailTab::Column, cx);
    }

    pub(in crate::app) fn clone_context_sql_row(
        &mut self,
        tab_id: u64,
        row: usize,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        match self.prepare_sql_clone(tab_id, row) {
            Ok(super::mutations::CloneSqlRow::Ready(request)) => {
                self.stage_sql_insert(request, tab_id, cx)
            }
            Ok(super::mutations::CloneSqlRow::NeedsValues { request, missing }) => {
                self.open_sql_clone_draft_editor(tab_id, request, missing, window, cx);
            }
            Err(error) => {
                window.open_alert_dialog(cx, move |dialog, _, _| {
                    dialog.title("Cannot clone SQL row").child(error.clone())
                });
            }
        }
    }

    pub(in crate::app) fn context_sort_blocked(&self, tab_id: u64) -> bool {
        let view = self.view(tab_id);
        view.grid_ui.inline_edit.is_some()
            || view.detail_ui.edit.is_some()
            || self.workspace.runtime(tab_id).is_some_and(|runtime| {
                runtime.has_staged_changes()
                    || matches!(
                        runtime.query.status(),
                        crate::query::QueryStatus::Running { .. }
                    )
            })
    }
}

#[cfg(test)]
mod sql_sort_tests {
    use super::*;

    const CLICKHOUSE: SqlDialect = SqlDialect {
        identifier_quote: '`',
        string_quote: '\'',
        backslash_escapes: true,
        like_requires_text_cast: false,
        double_quote_identifiers: false,
        bracket_identifiers: false,
    };
    /// MySQL with `sql_mode = ANSI_QUOTES`: `"` quotes identifiers, `\` still escapes.
    const MYSQL_ANSI: SqlDialect = SqlDialect {
        identifier_quote: '"',
        string_quote: '\'',
        backslash_escapes: true,
        like_requires_text_cast: false,
        double_quote_identifiers: false,
        bracket_identifiers: false,
    };
    /// MySQL with `sql_mode = NO_BACKSLASH_ESCAPES`.
    const MYSQL_NO_BACKSLASH: SqlDialect = SqlDialect {
        backslash_escapes: false,
        ..SqlDialect::MYSQL
    };

    fn schema_of(dialect: SqlDialect) -> &'static str {
        if dialect.identifier_quote == '`' {
            "shop"
        } else {
            "public"
        }
    }

    /// `sql` sorted by `name` ascending on the `orders` table of `dialect`.
    fn sort(dialect: SqlDialect, sql: &str) -> Result<String, SortError> {
        sorted_sql_query(dialect, sql, schema_of(dialect), "orders", "name", false)
    }

    #[test]
    fn parsed_table_select_orders_quoted_column_without_executing_any_other_statement() {
        let sql = sorted_sql_query(
            SqlDialect::POSTGRES,
            "SELECT * FROM public.orders WHERE status = 'open' LIMIT 50",
            "public",
            "orders",
            "na\"me",
            true,
        )
        .unwrap();
        assert!(sql.contains("ORDER BY \"na\"\"me\" DESC"), "{sql}");
        assert!(
            sql.contains("WHERE status = 'open' LIMIT 50")
                || sql.contains("WHERE status = 'open' ORDER BY")
        );
        assert!(sorted_sql_query(SqlDialect::POSTGRES, "DELETE FROM orders", "public", "orders", "name", false).is_err());
        assert!(sorted_sql_query(
            SqlDialect::POSTGRES,
            "SELECT * FROM orders; DELETE FROM orders",
            "public",
            "orders",
            "name",
            false
        )
        .is_err());
        assert!(sorted_sql_query(
            SqlDialect::POSTGRES,
            "WITH x AS (DELETE FROM orders RETURNING *) SELECT * FROM x",
            "public",
            "orders",
            "name",
            false
        )
        .is_err());
        assert!(sorted_sql_query(
            SqlDialect::POSTGRES,
            "SELECT * FROM orders JOIN other ON TRUE",
            "public",
            "orders",
            "name",
            false
        )
        .is_err());
        assert!(
            sorted_sql_query(SqlDialect::POSTGRES, "SELECT * FROM other", "public", "orders", "name", false).is_err()
        );
    }

    #[test]
    fn each_engines_own_quoting_is_parsed_and_printed() {
        let cases = [
            (
                SqlDialect::MYSQL,
                "SELECT * FROM `shop`.`orders` WHERE `qty` > 0 LIMIT 50",
                "ORDER BY `na``me` DESC",
            ),
            (
                CLICKHOUSE,
                "SELECT * FROM `shop`.`orders`",
                "ORDER BY `na``me` DESC",
            ),
            (
                SqlDialect::SQLITE,
                "SELECT * FROM \"main\".\"orders\" WHERE \"qty\" > 0",
                "ORDER BY \"na`me\" DESC",
            ),
            (
                SqlDialect::POSTGRES,
                "SELECT * FROM \"public\".\"orders\" WHERE \"qty\" > 0",
                "ORDER BY \"na`me\" DESC",
            ),
        ];
        for (dialect, sql, order) in cases {
            let schema = match dialect.identifier_quote {
                '`' => "shop",
                _ if sql.contains("main") => "main",
                _ => "public",
            };
            let sorted = sorted_sql_query(dialect, sql, schema, "orders", "na`me", true)
                .unwrap_or_else(|error| panic!("{sql}: {error}"));
            assert!(sorted.contains(order), "{sorted}");
            assert!(
                sorted.contains(&dialect.quote("orders")),
                "the table keeps its own quoting: {sorted}"
            );
        }
        // The old behaviour: a backtick statement was refused as "cannot be sorted".
        let error = sorted_sql_query(
            SqlDialect::POSTGRES,
            "SELECT * FROM `shop`.`orders`",
            "shop",
            "orders",
            "id",
            false,
        )
        .unwrap_err();
        assert!(error.to_string().contains("could not be parsed"), "{error}");
    }

    #[test]
    fn a_literal_with_a_backslash_is_refused_on_every_dialect() {
        let sql = "SELECT * FROM `shop`.`orders` WHERE `name` = 'a\\\\b'";
        let refused =
            sorted_sql_query(SqlDialect::MYSQL, sql, "shop", "orders", "id", false).unwrap_err();
        assert!(refused.to_string().contains("simple table SELECT"), "{refused}");
        // PostgreSQL reads it as an ordinary character only while
        // `standard_conforming_strings` is on, a session setting the scan cannot see
        // (with it off `'x\' LIMIT 5 -- '` is one string): refused there too.
        let pg = "SELECT * FROM \"public\".\"orders\" WHERE \"name\" = 'a\\\\b'";
        assert_eq!(
            sorted_sql_query(SqlDialect::POSTGRES, pg, "public", "orders", "id", false),
            Err(SortError::NotSimple)
        );
        for dialect in [SqlDialect::POSTGRES, SqlDialect::SQLITE, MYSQL_NO_BACKSLASH] {
            for sql in [
                r"SELECT * FROM orders WHERE a = 'x\' LIMIT 5 -- '",
                r"SELECT * FROM orders WHERE a = 'a\b'",
                r"SELECT * FROM orders WHERE a = E'a\n'",
                r#"SELECT * FROM "or\ders""#,
            ] {
                assert_eq!(sort(dialect, sql), Err(SortError::NotSimple), "{dialect:?} {sql}");
            }
        }
    }

    #[test]
    fn the_session_decides_whether_a_backslash_escapes() {
        let backslash = r"SELECT * FROM `orders` WHERE `name` = 'a\' OR x = 'b'";
        // Escaping: the quote after the backslash is part of the string, which then
        // runs on past the next quote and never closes.
        assert!(matches!(sort(SqlDialect::MYSQL, backslash), Err(SortError::Unreadable(_))));
        assert!(matches!(sort(CLICKHOUSE, backslash), Err(SortError::Unreadable(_))));
        // A closed literal holding a backslash is refused, as it always was.
        assert_eq!(
            sort(SqlDialect::MYSQL, r"SELECT * FROM `orders` WHERE `name` = 'it\'s'"),
            Err(SortError::NotSimple)
        );
        // `NO_BACKSLASH_ESCAPES`: `'a\'` is a complete string, still refused for
        // its backslash.
        assert_eq!(sort(MYSQL_NO_BACKSLASH, backslash), Err(SortError::NotSimple));
        // A backslash in an identifier is refused where it escapes (ClickHouse reads
        // it so), and so is a column holding one.
        assert_eq!(
            sort(CLICKHOUSE, r"SELECT * FROM `or\ders`"),
            Err(SortError::NotSimple)
        );
        let column_error =
            sorted_sql_query(CLICKHOUSE, "SELECT * FROM `orders`", "shop", "orders", "a\\", false);
        assert_eq!(column_error, Err(SortError::ColumnNotWritable));
        assert_eq!(
            column_error.unwrap_err().to_string(),
            "This column name can't be written safely for this connection"
        );
        assert!(sorted_sql_query(SqlDialect::POSTGRES, "SELECT * FROM orders", "public", "orders", "a\\", false).is_ok());
    }

    #[test]
    fn ansi_quotes_make_double_quotes_identifiers() {
        assert_eq!(
            sorted_sql_query(
                MYSQL_ANSI,
                "SELECT * FROM \"shop\".\"orders\" WHERE \"qty\" > 0",
                "shop",
                "orders",
                "name",
                false
            )
            .unwrap(),
            "SELECT * FROM \"shop\".\"orders\" WHERE \"qty\" > 0 ORDER BY \"name\" ASC"
        );
        // Without ANSI quotes `"x"` is a string (an operand, never a table).
        assert_eq!(
            sort(SqlDialect::MYSQL, "SELECT * FROM `orders` WHERE name = \"x y\"").unwrap(),
            "SELECT * FROM `orders` WHERE name = \"x y\" ORDER BY `name` ASC"
        );
        // As a table name `"x"` is read as an identifier on the backtick dialects too
        // (ClickHouse accepts both quotes), and compared by what it spells.
        assert_eq!(
            sort(CLICKHOUSE, "SELECT * FROM \"shop\".\"orders\" WHERE \"a\" = 1").unwrap(),
            "SELECT * FROM \"shop\".\"orders\" WHERE \"a\" = 1 ORDER BY `name` ASC"
        );
        assert_eq!(
            sort(CLICKHOUSE, "SELECT * FROM \"other\""),
            Err(SortError::OtherTable)
        );
    }

    #[test]
    fn a_dialect_the_driver_reports_with_ansi_quotes_reads_double_quotes_as_identifiers() {
        use engine_driver_api::SqlDialectInfo;
        let info = |double_quote_identifiers, bracket_identifiers| SqlDialectInfo {
            identifier_quote: "`".into(),
            string_literal_quote: "'".into(),
            supports_schemas: false,
            database_qualified_names: true,
            like_requires_text_cast: false,
            string_backslash_escapes: true,
            double_quote_identifiers,
            bracket_identifiers,
        };
        let ansi = SqlDialect::from_capabilities(&info(true, false));
        assert_eq!(ansi.identifier_quote, '`', "the sort still quotes with backticks");
        let options = SortScan::lex_options(ansi);
        assert!(options.double_quote_is_identifier && !options.bracket_identifiers);
        assert!(options.hash_comments, "still MySQL / MariaDB: `#` starts a comment");
        assert_eq!(
            sort(ansi, "SELECT * FROM \"shop\".\"orders\" WHERE \"qty\" > 0").unwrap(),
            "SELECT * FROM \"shop\".\"orders\" WHERE \"qty\" > 0 ORDER BY `name` ASC"
        );
        // Not reported: `"x"` is a string, as on a default session.
        let default = SqlDialect::from_capabilities(&info(false, false));
        assert!(!SortScan::lex_options(default).double_quote_is_identifier);
        // MariaDB `MSSQL`: `[name]` is an identifier too; the default session keeps `[` out.
        let mssql = SqlDialect::from_capabilities(&info(true, true));
        assert!(SortScan::lex_options(mssql).bracket_identifiers);
        assert!(!SortScan::lex_options(default).bracket_identifiers);
        // SQLite keeps its brackets when an older driver does not report them.
        assert!(SortScan::lex_options(SqlDialect { bracket_identifiers: false, ..SqlDialect::SQLITE }).bracket_identifiers);
    }

    #[test]
    fn the_where_clause_is_kept_byte_for_byte() {
        let sql = "SELECT  *\n FROM public.orders\n WHERE ( status = 'open'   AND qty >= 2 )\tOR note <> 'x''y'  LIMIT 50";
        assert_eq!(
            sorted_sql_query(SqlDialect::POSTGRES, sql, "public", "orders", "name", true).unwrap(),
            "SELECT  *\n FROM public.orders\n WHERE ( status = 'open'   AND qty >= 2 )\tOR note <> 'x''y' ORDER BY \"name\" DESC  LIMIT 50"
        );
        for filter in [
            "a = 1",
            "a <> 1 AND b != 'x' OR c < 2.5 AND d <= 3 AND e > 4 AND f >= 5",
            "((a = 1) OR (b = 2)) AND c = NULL",
            "a = TRUE OR b = false",
            "a",
            "a AND (b OR c = 1)",
            "a = \"b\"",
            "\"a\" = 'x' AND 1 = 1",
        ] {
            let sql = format!("SELECT * FROM orders WHERE {filter}");
            assert_eq!(
                sort(SqlDialect::POSTGRES, &sql).unwrap(),
                format!("{sql} ORDER BY \"name\" ASC"),
                "{filter}"
            );
        }
    }

    #[test]
    fn a_plain_select_is_ordered_in_every_shape() {
        let pg = SqlDialect::POSTGRES;
        let cases = [
            ("SELECT * FROM orders", "SELECT * FROM orders ORDER BY \"name\" ASC"),
            ("select * from orders", "select * from orders ORDER BY \"name\" ASC"),
            ("SELECT * FROM orders;", "SELECT * FROM orders ORDER BY \"name\" ASC;"),
            ("SELECT * FROM orders ;;", "SELECT * FROM orders ORDER BY \"name\" ASC ;;"),
            ("SELECT * FROM orders LIMIT 5", "SELECT * FROM orders ORDER BY \"name\" ASC LIMIT 5"),
            (
                "select * from orders where a = 1 order by id desc, b limit 10;",
                "select * from orders where a = 1 ORDER BY \"name\" ASC limit 10;",
            ),
            (
                "SELECT * FROM orders ORDER BY lower(id), (b) NULLS LAST",
                "SELECT * FROM orders ORDER BY \"name\" ASC",
            ),
            (
                "SELECT * FROM orders WHERE a = 1 ORDER BY id",
                "SELECT * FROM orders WHERE a = 1 ORDER BY \"name\" ASC",
            ),
            (
                "SELECT * FROM public.orders ORDER BY id LIMIT 2",
                "SELECT * FROM public.orders ORDER BY \"name\" ASC LIMIT 2",
            ),
            (
                "SELECT * FROM \"public\".\"orders\"\nWHERE a = 1\nLIMIT 7",
                "SELECT * FROM \"public\".\"orders\"\nWHERE a = 1 ORDER BY \"name\" ASC\nLIMIT 7",
            ),
        ];
        for (sql, expected) in cases {
            assert_eq!(sort(pg, sql).as_deref(), Ok(expected), "{sql}");
        }
    }

    #[test]
    fn limit_stays_after_the_order_by() {
        let sorted = sorted_sql_query(
            SqlDialect::MYSQL,
            "SELECT * FROM shop.orders WHERE qty > 0 ORDER BY id LIMIT 25",
            "shop",
            "orders",
            "name",
            true,
        )
        .unwrap();
        assert_eq!(
            sorted,
            "SELECT * FROM shop.orders WHERE qty > 0 ORDER BY `name` DESC LIMIT 25"
        );
        assert!(sorted.find("ORDER BY").unwrap() < sorted.find("LIMIT").unwrap());
    }

    #[test]
    fn comments_between_clauses_are_kept_and_never_swallow_the_new_clause() {
        let pg = SqlDialect::POSTGRES;
        let cases = [
            (
                "SELECT * FROM orders -- the table\n WHERE a = 1 /* c */ -- trailing\n LIMIT 5",
                "SELECT * FROM orders -- the table\n WHERE a = 1 ORDER BY \"name\" ASC /* c */ -- trailing\n LIMIT 5",
            ),
            (
                "SELECT * FROM orders WHERE a = 1 -- note",
                "SELECT * FROM orders WHERE a = 1 ORDER BY \"name\" ASC -- note",
            ),
            (
                "SELECT /* x */ * /* y */ FROM /* z */ orders /* w */ WHERE /* v */ a /* u */ = /* t */ 1",
                "SELECT /* x */ * /* y */ FROM /* z */ orders /* w */ WHERE /* v */ a /* u */ = /* t */ 1 ORDER BY \"name\" ASC",
            ),
            (
                "SELECT * FROM orders ORDER /* a */ BY /* b */ id /* c */ LIMIT 5",
                "SELECT * FROM orders ORDER BY \"name\" ASC /* c */ LIMIT 5",
            ),
            (
                "SELECT * FROM orders /* ; ORDER BY q LIMIT x */",
                "SELECT * FROM orders ORDER BY \"name\" ASC /* ; ORDER BY q LIMIT x */",
            ),
            (
                "SELECT * FROM orders /* a /* nested */ b */ LIMIT 1",
                "SELECT * FROM orders ORDER BY \"name\" ASC /* a /* nested */ b */ LIMIT 1",
            ),
        ];
        for (sql, expected) in cases {
            assert_eq!(sort(pg, sql).as_deref(), Ok(expected), "{sql}");
        }
        // MySQL's `#` comment, and a `--` that is not one without a space after it.
        assert_eq!(
            sort(SqlDialect::MYSQL, "SELECT * FROM orders # it's\n WHERE a = 1").unwrap(),
            "SELECT * FROM orders # it's\n WHERE a = 1 ORDER BY `name` ASC"
        );
        assert_eq!(
            sort(SqlDialect::MYSQL, "SELECT * FROM orders WHERE a = 1 --x"),
            Err(SortError::NotSimple)
        );
        // Block comments nest on PostgreSQL only; where the reading would differ the
        // statement is refused, not guessed at.
        for dialect in [pg, SqlDialect::MYSQL, SqlDialect::SQLITE] {
            assert!(
                matches!(
                    sort(dialect, "SELECT * FROM orders /* a /* b */ LIMIT 1"),
                    Err(SortError::Unreadable(_))
                ),
                "{dialect:?}"
            );
        }
        assert!(sort(SqlDialect::MYSQL, "SELECT * FROM orders /* a * b / c */ LIMIT 1").is_ok());
    }

    #[test]
    fn keywords_inside_strings_identifiers_and_comments_do_not_confuse_the_scanner() {
        let pg = SqlDialect::POSTGRES;
        let cases = [
            (
                "SELECT * FROM orders WHERE a = ' ORDER BY z LIMIT 1; DROP TABLE orders'",
                "SELECT * FROM orders WHERE a = ' ORDER BY z LIMIT 1; DROP TABLE orders' ORDER BY \"name\" ASC",
            ),
            (
                "SELECT * FROM orders WHERE \"order by\" = 'x' AND \"limit\" = 'y' AND \"where\" = 1",
                "SELECT * FROM orders WHERE \"order by\" = 'x' AND \"limit\" = 'y' AND \"where\" = 1 ORDER BY \"name\" ASC",
            ),
            (
                "SELECT * FROM orders WHERE a = ')' AND b = '('",
                "SELECT * FROM orders WHERE a = ')' AND b = '(' ORDER BY \"name\" ASC",
            ),
            (
                "SELECT * FROM orders WHERE note = $$ ORDER BY x; $$",
                "",
            ),
            (
                "SELECT * FROM orders WHERE a = 'x' ORDER BY \"limit\" LIMIT 3",
                "SELECT * FROM orders WHERE a = 'x' ORDER BY \"name\" ASC LIMIT 3",
            ),
            (
                "SELECT * FROM \"order by\".\"limit\"",
                "",
            ),
        ];
        for (sql, expected) in cases {
            let result = sort(pg, sql);
            if expected.is_empty() {
                // `$$ … $$` is an operand the scanner cannot judge: refused, not misread.
                assert!(result.is_err(), "{sql}");
            } else {
                assert_eq!(result.as_deref(), Ok(expected), "{sql}");
            }
        }
        // The same keywords as table and schema names, quoted.
        assert_eq!(
            sorted_sql_query(
                pg,
                "SELECT * FROM \"order by\".\"limit\" LIMIT 1",
                "order by",
                "limit",
                "name",
                false
            )
            .unwrap(),
            "SELECT * FROM \"order by\".\"limit\" ORDER BY \"name\" ASC LIMIT 1"
        );
    }

    #[test]
    fn quoted_names_are_matched_by_what_they_spell() {
        let pg = SqlDialect::POSTGRES;
        let sorted = |sql: &str, schema: &str, table: &str| {
            sorted_sql_query(pg, sql, schema, table, "id", false)
        };
        assert!(sorted("SELECT * FROM \"we\"\"ird\"", "public", "we\"ird").is_ok());
        assert!(sorted("SELECT * FROM \"a.b\"", "public", "a.b").is_ok());
        assert!(sorted("SELECT * FROM \"my schema\".\"my table\"", "my schema", "my table").is_ok());
        assert_eq!(
            sorted("SELECT * FROM \"we\"\"ird\"", "public", "we\"ird2"),
            Err(SortError::OtherTable)
        );
        // A name matches exactly (no case folding), in the right schema only.
        assert_eq!(sorted("SELECT * FROM Orders", "public", "orders"), Err(SortError::OtherTable));
        assert_eq!(sorted("SELECT * FROM x.orders", "public", "orders"), Err(SortError::OtherTable));
        assert_eq!(
            sorted("SELECT * FROM db.public.orders", "public", "orders"),
            Err(SortError::OtherTable)
        );
        assert!(sorted("SELECT * FROM public.orders", "public", "orders").is_ok());
        // A bare name matches whatever the schema is.
        assert!(sorted("SELECT * FROM orders", "other", "orders").is_ok());
        assert_eq!(
            sorted("SELECT * FROM public.", "public", "orders"),
            Err(SortError::NotPlainTable)
        );
    }

    #[test]
    fn column_quoting_follows_each_dialect() {
        let cases = [
            (SqlDialect::POSTGRES, "SELECT * FROM orders", "name", "\"name\""),
            (SqlDialect::POSTGRES, "SELECT * FROM orders", "Order Date", "\"Order Date\""),
            (SqlDialect::POSTGRES, "SELECT * FROM orders", "order", "\"order\""),
            (SqlDialect::POSTGRES, "SELECT * FROM orders", "a\"b\"\"c", "\"a\"\"b\"\"\"\"c\""),
            (SqlDialect::POSTGRES, "SELECT * FROM orders", "na`me", "\"na`me\""),
            (SqlDialect::POSTGRES, "SELECT * FROM orders", "ünï;--", "\"ünï;--\""),
            (SqlDialect::SQLITE, "SELECT * FROM orders", "a\"b", "\"a\"\"b\""),
            (SqlDialect::SQLITE, "SELECT * FROM orders", "a b", "\"a b\""),
            (SqlDialect::MYSQL, "SELECT * FROM orders", "a b", "`a b`"),
            (SqlDialect::MYSQL, "SELECT * FROM orders", "a`b", "`a``b`"),
            (SqlDialect::MYSQL, "SELECT * FROM orders", "a\"b", "`a\"b`"),
            (CLICKHOUSE, "SELECT * FROM orders", "a`b`", "`a``b```"),
            (MYSQL_ANSI, "SELECT * FROM orders", "a\"b", "\"a\"\"b\""),
        ];
        for (dialect, sql, column, quoted) in cases {
            for (descending, direction) in [(false, "ASC"), (true, "DESC")] {
                let sorted =
                    sorted_sql_query(dialect, sql, "public", "orders", column, descending).unwrap();
                assert_eq!(sorted, format!("{sql} ORDER BY {quoted} {direction}"), "{column}");
            }
        }
    }

    #[test]
    fn invalid_targets_and_quotings_are_refused() {
        let pg = SqlDialect::POSTGRES;
        let sql = "SELECT * FROM orders";
        assert_eq!(sorted_sql_query(pg, sql, "public", "orders", "", false), Err(SortError::InvalidTarget));
        assert_eq!(sorted_sql_query(pg, sql, "public", "orders", "a\0b", false), Err(SortError::InvalidTarget));
        assert_eq!(sorted_sql_query(pg, sql, "public", "", "id", false), Err(SortError::InvalidTarget));
        let bracket = SqlDialect {
            identifier_quote: '[',
            ..SqlDialect::POSTGRES
        };
        assert_eq!(
            sorted_sql_query(bracket, sql, "public", "orders", "id", false),
            Err(SortError::UnsupportedQuoting)
        );
    }

    #[test]
    fn statements_that_are_not_one_select_are_refused() {
        let pg = SqlDialect::POSTGRES;
        for sql in [
            "",
            "   ",
            ";",
            "-- nothing",
            "SELECT * FROM orders; SELECT * FROM orders",
            "SELECT * FROM orders;; SELECT * FROM orders",
            "SELECT * FROM orders; DELETE FROM orders",
            "; SELECT * FROM orders",
            "SELECT * FROM orders; -- c\n DROP TABLE orders",
        ] {
            assert_eq!(sort(pg, sql), Err(SortError::NotOneStatement), "{sql:?}");
        }
        for sql in [
            "DELETE FROM orders",
            "UPDATE orders SET a = 1",
            "INSERT INTO orders VALUES (1)",
            "DROP TABLE orders",
            "EXPLAIN SELECT * FROM orders",
            "VALUES (1)",
            "TABLE orders",
            "SHOW TABLES",
            "42",
            "'SELECT * FROM orders'",
        ] {
            assert_eq!(sort(pg, sql), Err(SortError::NotSelect), "{sql}");
        }
    }

    #[test]
    fn with_set_operations_and_row_limiting_clauses_are_refused() {
        let pg = SqlDialect::POSTGRES;
        for sql in [
            "WITH x AS (SELECT * FROM orders) SELECT * FROM x",
            "WITH x AS (DELETE FROM orders RETURNING *) SELECT * FROM x",
            "SELECT * FROM orders OFFSET 5",
            "SELECT * FROM orders LIMIT 5 OFFSET 2",
            "SELECT * FROM orders ORDER BY id OFFSET 2 ROWS FETCH NEXT 3 ROWS ONLY",
            "SELECT * FROM orders FETCH FIRST 3 ROWS ONLY",
            "SELECT * FROM orders FOR UPDATE",
            "SELECT * FROM orders WHERE a = 1 FOR SHARE",
            "SELECT * FROM orders ORDER BY id FOR UPDATE",
            "SELECT * FROM orders LIMIT ALL",
            "SELECT * FROM orders LIMIT x",
            "SELECT * FROM orders LIMIT 1 + 1",
            "SELECT * FROM orders LIMIT -1",
            "SELECT * FROM orders LIMIT 1.5",
            "SELECT * FROM orders LIMIT 99999999999999999999999",
            "SELECT * FROM orders LIMIT",
            "SELECT * FROM orders LIMIT 5 ORDER BY id",
            "SELECT * FROM orders ORDER BY id LIMIT 5 LIMIT 6",
            "SELECT * FROM orders ORDER BY id LIMIT 5, 10",
            "SELECT * FROM orders LIMIT 5 BY a",
            "SELECT * FROM orders ORDER BY id WITH FILL",
            "SELECT * FROM orders SETTINGS max_threads = 1",
            "SELECT * FROM orders FORMAT JSON",
        ] {
            assert_eq!(sort(pg, sql), Err(SortError::Unsafe), "{sql}");
        }
        for sql in [
            "(SELECT * FROM orders)",
            "SELECT * FROM orders UNION SELECT * FROM orders",
            "SELECT * FROM orders UNION ALL SELECT * FROM orders",
            "SELECT * FROM orders WHERE a = 1 UNION SELECT * FROM orders",
            "SELECT * FROM orders EXCEPT SELECT * FROM orders",
            "SELECT * FROM orders INTERSECT SELECT * FROM orders",
            "SELECT * FROM orders WHERE a = 1 EXCEPT SELECT * FROM orders",
        ] {
            assert_eq!(sort(pg, sql), Err(SortError::SetQuery), "{sql}");
        }
    }

    #[test]
    fn projections_grouping_and_windows_are_refused() {
        let pg = SqlDialect::POSTGRES;
        for sql in [
            "SELECT id FROM orders",
            "SELECT *, id FROM orders",
            "SELECT id, * FROM orders",
            "SELECT o.* FROM orders",
            "SELECT DISTINCT * FROM orders",
            "SELECT DISTINCT ON (id) * FROM orders",
            "SELECT TOP 5 * FROM orders",
            "SELECT * INTO copy FROM orders",
            "SELECT * FROM orders GROUP BY id",
            "SELECT * FROM orders WHERE a = 1 GROUP BY id",
            "SELECT * FROM orders HAVING a = 1",
            "SELECT * FROM orders WINDOW w AS (ORDER BY id)",
            "SELECT * FROM orders QUALIFY a = 1",
            "SELECT * FROM orders INTO OUTFILE '/tmp/x'",
            "SELECT * FROM orders ORDER BY id GROUP BY id",
            "SELECT * FROM orders ORDER BY (SELECT 1)",
            "SELECT * FROM orders ORDER BY id UNION SELECT * FROM orders",
            "SELECT * FROM orders ORDER BY",
            "SELECT * FROM orders ORDER BY LIMIT 5",
            "SELECT * FROM orders ORDER id",
            "SELECT * FROM orders ORDER BY a ORDER BY b",
            "SELECT * FROM orders WHERE",
            "SELECT * FROM orders WHERE ORDER BY id",
            "SELECT *",
            "SELECT * FROM",
            "SELECT",
        ] {
            let result = sort(pg, sql);
            let expected = if sql.contains("UNION") {
                SortError::SetQuery
            } else if sql == "SELECT * FROM" {
                SortError::NotPlainTable
            } else {
                SortError::NotSimple
            };
            assert_eq!(result, Err(expected), "{sql}");
        }
    }

    #[test]
    fn joins_aliases_and_derived_tables_are_refused() {
        let pg = SqlDialect::POSTGRES;
        for sql in [
            "SELECT * FROM orders JOIN other ON TRUE",
            "SELECT * FROM orders INNER JOIN other ON a = b",
            "SELECT * FROM orders LEFT JOIN other ON a = b",
            "SELECT * FROM orders CROSS JOIN other",
            "SELECT * FROM orders NATURAL JOIN other",
            "SELECT * FROM orders FULL OUTER JOIN other ON a = b",
        ] {
            assert_eq!(sort(pg, sql), Err(SortError::Joined), "{sql}");
        }
        for sql in ["SELECT * FROM orders, other", "SELECT * FROM orders o, other"] {
            let expected = if sql.contains("o,") {
                SortError::NotPlainTable
            } else {
                SortError::NotOneTable
            };
            assert_eq!(sort(pg, sql), Err(expected), "{sql}");
        }
        for sql in [
            "SELECT * FROM orders o",
            "SELECT * FROM orders AS o",
            "SELECT * FROM public.orders o WHERE o.a = 1",
            "SELECT * FROM (SELECT * FROM orders) x",
            "SELECT * FROM (SELECT * FROM orders)",
            "SELECT * FROM generate_series(1, 2)",
            "SELECT * FROM orders TABLESAMPLE SYSTEM (10)",
            "SELECT * FROM orders PARTITION (p0)",
            "SELECT * FROM orders USE INDEX (i)",
            "SELECT * FROM orders ONLY",
            "SELECT * FROM public.orders.x.",
            "SELECT * FROM .orders",
            "SELECT * FROM 'orders'",
            "SELECT * FROM WHERE a = 1",
            "SELECT * FROM LIMIT 5",
        ] {
            assert_eq!(sort(pg, sql), Err(SortError::NotPlainTable), "{sql}");
        }
        assert_eq!(sort(SqlDialect::SQLITE, "SELECT * FROM [orders]"), Err(SortError::NotPlainTable));
        assert_eq!(sort(pg, "SELECT * FROM other"), Err(SortError::OtherTable));
        assert_eq!(
            sorted_sql_query(pg, "SELECT * FROM public.orders", "main", "orders", "id", false),
            Err(SortError::OtherTable)
        );
    }

    #[test]
    fn only_plain_comparisons_are_accepted_in_a_where_clause() {
        let pg = SqlDialect::POSTGRES;
        for filter in [
            "lower(a) = 'x'",
            "a = lower('X')",
            "now() > a",
            "a IN (1, 2)",
            "a NOT IN (1)",
            "a LIKE 'x%'",
            "a ILIKE 'x%'",
            "a IS NULL",
            "a IS NOT NULL",
            "NOT a = 1",
            "NOT (a = 1)",
            "a = 1 AND NOT b = 2",
            "a = (SELECT 1)",
            "(SELECT 1) = 1",
            "EXISTS (SELECT 1)",
            "a + 1 = 2",
            "a = 1 + 1",
            "a = -1",
            "a = +1",
            "a * 2 > 1",
            "a || 'x' = 'y'",
            "t.a = 1",
            "a = t.b",
            "orders.a = 1",
            "a BETWEEN 1 AND 2",
            "CASE WHEN a THEN 1 END = 1",
            "a = CASE WHEN b THEN 1 ELSE 2 END",
            "a = CURRENT_DATE",
            "a = CURRENT_TIMESTAMP",
            "a = current_user",
            "a = 'x'::text",
            "a = CAST('1' AS int)",
            "a = E'x'",
            "a = N'x'",
            "a = X'ab'",
            "a = DATE '2020-01-01'",
            "a = INTERVAL '1 day'",
            "a = 1e3",
            "a = 0x10",
            "a = 1.",
            "a = .5",
            "a = 1_000",
            "a == 1",
            "a <=> 1",
            "a < = 1",
            "a = = 1",
            "a ! = 1",
            "a =< 1",
            "a >< 1",
            "a b",
            "a = 1 b",
            "a 1",
            "1 2",
            "a = 'x' 'y'",
            "a = 1 AND",
            "a = 1 OR OR b = 2",
            "AND a = 1",
            "a =",
            "= 1",
            "()",
            "(a = 1) (b = 2)",
            "(a = 1) b",
            "a = 1 COLLATE x",
            "a = b, c = d",
            "a = ALL (SELECT 1)",
            "a SIMILAR TO 'x'",
            "a MOD 2 = 0",
            "a = DEFAULT",
        ] {
            assert_eq!(
                sort(pg, &format!("SELECT * FROM orders WHERE {filter}")),
                Err(SortError::NotSimple),
                "{filter}"
            );
        }
        // A backtick or bracket identifier is not the quote of these dialects.
        assert_eq!(
            sort(SqlDialect::SQLITE, "SELECT * FROM orders WHERE [a] = 1"),
            Err(SortError::NotSimple)
        );
        assert_eq!(
            sort(SqlDialect::SQLITE, "SELECT * FROM orders WHERE `a` = 1"),
            Err(SortError::NotSimple)
        );
        // `<=>` and `div` are MySQL operators.
        assert_eq!(
            sort(SqlDialect::MYSQL, "SELECT * FROM orders WHERE a <=> 1"),
            Err(SortError::NotSimple)
        );
        assert_eq!(
            sort(SqlDialect::MYSQL, "SELECT * FROM orders WHERE a = 1 XOR b = 2"),
            Err(SortError::NotSimple)
        );
    }

    #[test]
    fn text_that_cannot_be_read_with_certainty_is_refused() {
        let pg = SqlDialect::POSTGRES;
        for sql in [
            "SELECT * FROM orders WHERE a = 'x",
            "SELECT * FROM orders WHERE \"a = 1",
            "SELECT * FROM \"orders",
            "SELECT * FROM orders /* open",
            "SELECT * FROM orders WHERE a = 'x''",
            "SELECT * FROM orders WHERE a = $t$ x",
            "SELECT * FROM orders WHERE (a = 1",
            "SELECT * FROM orders WHERE a = 1)",
            "SELECT * FROM orders WHERE ((a = 1)",
            ") SELECT * FROM orders",
            "SELECT * FROM orders WHERE a = $1",
            "SELECT * FROM orders WHERE a = ?",
            "SELECT * FROM orders WHERE a = @x",
            "SELECT * FROM orders WHERE a = `b`",
            "SELECT * FROM orders WHERE a # b",
            "SELECT * FROM `shop`.`orders`",
            "SELECT * FROM orders WHERE a = ANY (ARRAY[1])",
            "SELECT * FROM orders WHERE a = [1]",
            "SELECT * FROM orders WHERE a = {1}",
            "SELECT * FROM orders ORDER BY a[1]",
            "SELECT * FROM orders ORDER BY a \\ b",
        ] {
            assert!(matches!(sort(pg, sql), Err(SortError::Unreadable(_))), "{sql}");
        }
        // MySQL's executable comments are code the scanner cannot place.
        for sql in [
            "SELECT * FROM orders /*!50000 WHERE a = 1 */",
            "SELECT * FROM orders WHERE a = 1 /*!99999 ORDER BY id */",
            "SELECT * /*! FROM orders */",
        ] {
            let result = sort(SqlDialect::MYSQL, sql);
            assert!(matches!(result, Err(SortError::Unreadable(_))), "{sql}: {result:?}");
        }
        // PostgreSQL has no such comments: the text is an ordinary comment there.
        assert!(sort(pg, "SELECT * FROM orders /*!50000 WHERE a = 1 */").is_ok());
        let error = sort(pg, "SELECT * FROM orders /* open").unwrap_err();
        assert!(error.to_string().starts_with("The statement could not be parsed"), "{error}");
    }

    #[test]
    fn the_new_clause_is_never_glued_to_what_follows() {
        // Insert path: no space between the last token and `LIMIT`.
        let inserts = [
            ("SELECT * FROM \"orders\"LIMIT 5", "SELECT * FROM \"orders\" ORDER BY \"name\" ASC LIMIT 5"),
            ("SELECT * FROM orders WHERE a = 'x'LIMIT 5", "SELECT * FROM orders WHERE a = 'x' ORDER BY \"name\" ASC LIMIT 5"),
            ("SELECT * FROM orders WHERE (a = 1)LIMIT 5", "SELECT * FROM orders WHERE (a = 1) ORDER BY \"name\" ASC LIMIT 5"),
            ("SELECT * FROM orders WHERE a = 1LIMIT 5", ""),
            ("SELECT * FROM orders/* c */LIMIT 5", "SELECT * FROM orders ORDER BY \"name\" ASC /* c */LIMIT 5"),
            ("SELECT * FROM orders;", "SELECT * FROM orders ORDER BY \"name\" ASC;"),
        ];
        for (sql, expected) in inserts {
            let result = sort(SqlDialect::POSTGRES, sql);
            if expected.is_empty() {
                // `1LIMIT` is one token (a number with letters), not a number and `LIMIT`.
                assert!(result.is_err(), "{sql}");
            } else {
                assert_eq!(result.as_deref(), Ok(expected), "{sql}");
            }
        }
        // Replace path: the old list ends right at `LIMIT`.
        let replaces = [
            ("SELECT * FROM orders ORDER BY \"id\"LIMIT 5", "SELECT * FROM orders ORDER BY \"name\" ASC LIMIT 5"),
            ("SELECT * FROM orders ORDER BY (id)LIMIT 5", "SELECT * FROM orders ORDER BY \"name\" ASC LIMIT 5"),
            ("SELECT * FROM orders ORDER BY f(id)LIMIT 5", "SELECT * FROM orders ORDER BY \"name\" ASC LIMIT 5"),
            ("SELECT * FROM orders ORDER BY 'x'LIMIT 5", "SELECT * FROM orders ORDER BY \"name\" ASC LIMIT 5"),
            ("SELECT * FROM orders ORDER BY id/* c */LIMIT 5", "SELECT * FROM orders ORDER BY \"name\" ASC /* c */LIMIT 5"),
            ("SELECT * FROM orders ORDER BY id;", "SELECT * FROM orders ORDER BY \"name\" ASC;"),
        ];
        for (sql, expected) in replaces {
            assert_eq!(sort(SqlDialect::POSTGRES, sql).as_deref(), Ok(expected), "{sql}");
        }
        // Every dialect, in its own quoting.
        for dialect in [SqlDialect::POSTGRES, SqlDialect::SQLITE, SqlDialect::MYSQL, CLICKHOUSE, MYSQL_ANSI] {
            let quoted = |name: &str| dialect.quote(name);
            let orders = quoted("orders");
            let column = quoted("name");
            let insert = format!("SELECT * FROM {orders}LIMIT 5");
            assert_eq!(
                sort(dialect, &insert).unwrap(),
                format!("SELECT * FROM {orders} ORDER BY {column} ASC LIMIT 5"),
                "{dialect:?}"
            );
            let replace = format!("SELECT * FROM {orders} ORDER BY {orders}LIMIT 5");
            assert_eq!(
                sort(dialect, &replace).unwrap(),
                format!("SELECT * FROM {orders} ORDER BY {column} ASC LIMIT 5"),
                "{dialect:?}"
            );
            let after_string = format!("SELECT * FROM {orders} WHERE a = 'x'LIMIT 5");
            assert_eq!(
                sort(dialect, &after_string).unwrap(),
                format!("SELECT * FROM {orders} WHERE a = 'x' ORDER BY {column} ASC LIMIT 5"),
                "{dialect:?}"
            );
            let after_paren = format!("SELECT * FROM {orders} ORDER BY f(a)LIMIT 5");
            assert_eq!(
                sort(dialect, &after_paren).unwrap(),
                format!("SELECT * FROM {orders} ORDER BY {column} ASC LIMIT 5"),
                "{dialect:?}"
            );
        }
    }

    #[test]
    fn marias_executable_comments_are_refused_like_mysqls() {
        // MariaDB shares the MySQL dialect and runs `/*M! … */` as code: it would read
        // `ORDER BY c ASC, other` or keep a second statement.
        for sql in [
            "SELECT * FROM orders /*M! , other */",
            "SELECT * FROM orders /*M!; DELETE FROM orders */",
            "SELECT * FROM orders /*M!100000 LIMIT 5 */",
            "SELECT * FROM orders /*M!50000 WHERE a = 1 */ LIMIT 5",
            "SELECT * FROM orders ORDER BY id /*M! DESC */",
            "SELECT * FROM orders /*!50000 WHERE a = 1 */",
        ] {
            for dialect in [SqlDialect::MYSQL, CLICKHOUSE, MYSQL_NO_BACKSLASH, MYSQL_ANSI] {
                let result = sort(dialect, sql);
                assert!(matches!(result, Err(SortError::Unreadable(_))), "{dialect:?} {sql}: {result:?}");
            }
        }
        // Plain comments that merely look similar stay fine.
        assert!(sort(SqlDialect::MYSQL, "SELECT * FROM orders /* M! , other */").is_ok());
        assert!(sort(SqlDialect::MYSQL, "SELECT * FROM orders /*M x*/").is_ok());
        // PostgreSQL and SQLite have no executable comments.
        assert!(sort(SqlDialect::POSTGRES, "SELECT * FROM orders /*M! , other */").is_ok());
        assert!(sort(SqlDialect::SQLITE, "SELECT * FROM orders /*M!; x */").is_ok());
    }

    #[test]
    fn a_carriage_return_ends_a_line_comment_on_postgres_only() {
        // `--x\r` then a clause: PostgreSQL reads the clause as code, the others as comment.
        let sql = "SELECT * FROM orders --x\r LIMIT 5";
        assert_eq!(
            sort(SqlDialect::POSTGRES, sql).unwrap(),
            "SELECT * FROM orders ORDER BY \"name\" ASC --x\r LIMIT 5"
        );
        assert_eq!(
            sort(SqlDialect::SQLITE, sql).unwrap(),
            "SELECT * FROM orders ORDER BY \"name\" ASC --x\r LIMIT 5"
        );
        // The same text would hide a statement behind the comment for PostgreSQL's scanner.
        let hidden = "SELECT * FROM orders WHERE a = 1 --x\r; DELETE FROM orders";
        assert_eq!(sort(SqlDialect::POSTGRES, hidden), Err(SortError::NotOneStatement));
        assert_eq!(
            sort(SqlDialect::SQLITE, hidden).unwrap(),
            "SELECT * FROM orders WHERE a = 1 ORDER BY \"name\" ASC --x\r; DELETE FROM orders"
        );
    }

    #[test]
    fn error_messages_say_why() {
        let pg = SqlDialect::POSTGRES;
        for (sql, message) in [
            ("DELETE FROM orders", "Only a SELECT statement can be sorted"),
            ("SELECT * FROM orders; SELECT 1", "Only one SELECT statement can be sorted"),
            ("WITH x AS (SELECT 1) SELECT * FROM x", "This SELECT cannot be safely sorted"),
            ("SELECT * FROM orders UNION SELECT 1", "Set queries cannot be sorted here"),
            ("SELECT DISTINCT * FROM orders", "Only a simple table SELECT can be sorted"),
            ("SELECT * FROM orders, x", "SELECT must read one table"),
            ("SELECT * FROM orders JOIN x ON TRUE", "Joined SELECT cannot be sorted here"),
            ("SELECT * FROM orders o", "SELECT must read one unaliased table"),
            ("SELECT * FROM other", "SELECT does not target the current table"),
        ] {
            assert_eq!(sort(pg, sql).unwrap_err().to_string(), message, "{sql}");
        }
        assert_eq!(
            sorted_sql_query(pg, "SELECT * FROM orders", "public", "orders", "", false)
                .unwrap_err()
                .to_string(),
            "Invalid sort column or table"
        );
    }

    /// The `name` of the first row `sql` returns from a fresh in-memory SQLite
    /// database holding the `orders` fixture (the `sqlite3` CLI, as the
    /// driver-route tests use).
    fn first_name(sql: &str) -> String {
        let script = format!(
            "CREATE TABLE orders (id INTEGER PRIMARY KEY, name TEXT); \
             INSERT INTO orders VALUES (1, 'alpha'), (2, 'zulu'); {sql};"
        );
        let output = std::process::Command::new("sqlite3")
            .arg(":memory:")
            .arg(script)
            .output()
            .expect("the sqlite3 CLI");
        assert!(output.status.success(), "{}", String::from_utf8_lossy(&output.stderr));
        let stdout = String::from_utf8(output.stdout).unwrap();
        stdout.lines().next().unwrap().split('|').nth(1).unwrap().to_string()
    }

    #[test]
    fn sqlite_sort_then_original_query_restores_database_row_order() {
        let original = "SELECT * FROM main.orders ORDER BY id LIMIT 2";
        let sorted =
            sorted_sql_query(SqlDialect::SQLITE, original, "main", "orders", "name", true).unwrap();
        for (sql, expected) in [(sorted.as_str(), "zulu"), (original, "alpha")] {
            assert_eq!(first_name(sql), expected);
        }
    }
}

#[cfg(test)]
mod generator_tests {
    use super::*;

    #[test]
    fn generators_preflight_increment() {
        assert!(generated_values(GenerateKind::Increment(i64::MAX), 2).is_err());
        assert_eq!(
            generated_values(GenerateKind::Increment(1), 3).unwrap(),
            vec![Some("1".into()), Some("2".into()), Some("3".into())]
        );
        let uuid = generated_values(GenerateKind::Uuid, 2).unwrap();
        assert_ne!(uuid[0], uuid[1]);
        assert_eq!(
            generated_values(GenerateKind::EmptyString, 1).unwrap(),
            vec![Some(String::new())]
        );
        assert_eq!(generated_values(GenerateKind::Null, 1).unwrap(), vec![None]);
    }
}
