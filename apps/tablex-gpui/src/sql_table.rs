//! Table-data tabs for SQL objects opened from the sidebar.
//!
//! A `sqlTable` tab has no SQL editor: it holds `connection/db/schema/table`
//! plus two user-editable fragments (`WHERE`, `ORDER BY`). Each run builds
//!
//! ```text
//! SELECT * FROM "schema"."table"
//! WHERE <where>
//! ORDER BY <order by>
//! ```
//!
//! with per-dialect identifier quoting. The engine wraps the statement as a
//! sub-select and pages it with `LIMIT/OFFSET` (`execute_sql_page`), so a user
//! `ORDER BY` is preserved inside the sub-select and paging stays offset based
//! (keyset paging would need a unique sort key; offset is correct for any
//! ordering and is what the existing Prev/Next pagination already drives).
//!
//! Fragments are user text spliced into the statement, so they are validated
//! first: no `;` outside quotes (one statement only), no comments (a line
//! comment would swallow the engine's closing wrapper), balanced quotes and
//! parentheses. The engine additionally accepts read-only SELECTs only.

use crate::connections::ConnectionKind;

/// SQL syntax of a connection, as behavior flags rather than a product name.
///
/// A connected driver advertises its syntax in `DriverCapabilities::sql_dialect`
/// ([`Self::from_capabilities`]); [`Self::for_kind`] is only the fallback for
/// connections whose driver advertises nothing (older drivers). The `POSTGRES` / `SQLITE` / `MYSQL` presets are what those
/// fallbacks (and tests) use.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SqlDialect {
    /// Quote around identifiers; an embedded quote is escaped by doubling.
    pub identifier_quote: char,
    /// Quote around string literals; an embedded quote is doubled.
    pub string_quote: char,
    /// A backslash inside a string literal is an escape and must be doubled
    /// (the MySQL family; `SqlDialectInfo::string_backslash_escapes`).
    pub backslash_escapes: bool,
    /// `LIKE` only accepts text, so non-text columns are cast first
    /// (`SqlDialectInfo::like_requires_text_cast`).
    pub like_requires_text_cast: bool,
    /// `"…"` quotes an identifier rather than a string (PostgreSQL, SQLite; MySQL and
    /// MariaDB under `ANSI_QUOTES`; `SqlDialectInfo::double_quote_identifiers`).
    pub double_quote_identifiers: bool,
    /// `[name]` quotes an identifier (SQLite; MariaDB under `MSSQL`;
    /// `SqlDialectInfo::bracket_identifiers`).
    pub bracket_identifiers: bool,
}

impl SqlDialect {
    pub const POSTGRES: Self = Self {
        identifier_quote: '"',
        string_quote: '\'',
        backslash_escapes: false,
        like_requires_text_cast: true,
        double_quote_identifiers: true,
        bracket_identifiers: false,
    };
    pub const SQLITE: Self = Self {
        identifier_quote: '"',
        string_quote: '\'',
        backslash_escapes: false,
        like_requires_text_cast: false,
        double_quote_identifiers: true,
        bracket_identifiers: true,
    };
    pub const MYSQL: Self = Self {
        identifier_quote: '`',
        string_quote: '\'',
        backslash_escapes: true,
        like_requires_text_cast: false,
        double_quote_identifiers: false,
        bracket_identifiers: false,
    };
    /// What the ClickHouse driver advertises: backticks, `"…"` a name too, a backslash an escape.
    pub const CLICKHOUSE: Self = Self { double_quote_identifiers: true, ..Self::MYSQL };

    /// Per-kind fallback for connections that advertise no dialect.
    pub fn for_kind(kind: ConnectionKind) -> Self {
        match kind {
            ConnectionKind::Sqlite => Self::SQLITE,
            ConnectionKind::Mysql | ConnectionKind::Mariadb => Self::MYSQL,
            ConnectionKind::Clickhouse => Self::CLICKHOUSE,
            _ => Self::POSTGRES,
        }
    }

    /// Dialect a SQL driver advertises (`DriverCapabilities::sql_dialect`).
    /// Every flag comes from the descriptor; nothing is guessed from the
    /// shape of the dialect. A quote the descriptor leaves empty (or longer
    /// than one character) keeps the standard-SQL default.
    pub fn from_capabilities(info: &engine_driver_api::SqlDialectInfo) -> Self {
        let single = |quote: &str, default: char| {
            let mut chars = quote.chars();
            match (chars.next(), chars.next()) {
                (Some(quote), None) => quote,
                _ => default,
            }
        };
        Self {
            identifier_quote: single(&info.identifier_quote, '"'),
            string_quote: single(&info.string_literal_quote, '\''),
            backslash_escapes: info.string_backslash_escapes,
            like_requires_text_cast: info.like_requires_text_cast,
            double_quote_identifiers: info.double_quote_identifiers,
            bracket_identifiers: info.bracket_identifiers,
        }
    }

    /// A backslash inside a quoted name is read differently by servers this dialect
    /// cannot tell apart: MySQL / MariaDB under `ANSI_QUOTES` take it as a plain
    /// character, ClickHouse (same flags) as an escape. Text with one is refused.
    pub const fn backslash_in_name_is_ambiguous(self) -> bool {
        self.double_quote_identifiers && self.backslash_escapes
    }

    /// [`Self::quote`], refusing a name whose backslash this dialect cannot quote unambiguously
    /// ([`Self::backslash_in_name_is_ambiguous`]).
    pub fn checked_quote(self, name: &str) -> Result<String, AmbiguousName> {
        if self.backslash_in_name_is_ambiguous() && name.contains('\\') {
            return Err(AmbiguousName { name: name.to_owned() });
        }
        Ok(self.quote(name))
    }

    pub fn quote(self, name: &str) -> String {
        let quote = self.identifier_quote;
        let mut out = String::with_capacity(name.len() + 2);
        out.push(quote);
        for ch in name.chars() {
            if ch == quote {
                out.push(quote);
            }
            out.push(ch);
        }
        out.push(quote);
        out
    }
}

/// A schema or table name that cannot be put in a statement safely: it holds a backslash, and the
/// server this dialect stands for may read that as an escape inside the quotes (ClickHouse) or as
/// a plain character (MySQL / MariaDB under `ANSI_QUOTES`). Either quoting would be wrong for the
/// other, and a wrong one lets the name end the identifier early.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AmbiguousName {
    pub name: String,
}

impl std::fmt::Display for AmbiguousName {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "The name \"{}\" has a backslash, which this server may read as an escape inside quotes: it cannot be used in a statement safely.",
            self.name.escape_debug()
        )
    }
}

impl std::error::Error for AmbiguousName {}

/// How an engine spells a byte string in SQL text. Which one applies is a
/// property of the product, so it comes from the connection's kind (the
/// advertised dialect flags do not tell PostgreSQL's apart from SQLite's).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum BinaryLiteral {
    /// `decode('DEADBEEF', 'hex')` (a hex literal): it
    /// does not depend on `standard_conforming_strings` the way `'\xDEADBEEF'`
    /// does.
    #[default]
    Postgres,
    /// `X'DEADBEEF'`: MySQL, MariaDB, SQLite.
    HexString,
    /// `unhex('DEADBEEF')`.
    ClickHouse,
}

impl BinaryLiteral {
    pub fn for_kind(kind: ConnectionKind) -> Self {
        match kind {
            ConnectionKind::Sqlite | ConnectionKind::Mysql | ConnectionKind::Mariadb => {
                Self::HexString
            }
            ConnectionKind::Clickhouse => Self::ClickHouse,
            _ => Self::Postgres,
        }
    }

    /// The literal for `hex` (an even number of hex digits, no `0x`).
    pub fn literal(self, hex: &str) -> String {
        match self {
            Self::Postgres => format!("decode('{hex}', 'hex')"),
            Self::HexString => format!("X'{hex}'"),
            Self::ClickHouse => format!("unhex('{hex}')"),
        }
    }
}

/// `"schema"."table"` (or just the table when no schema is known); a name with a backslash the
/// dialect cannot quote unambiguously is refused.
pub fn qualified_table(dialect: SqlDialect, schema: Option<&str>, table: &str) -> Result<String, AmbiguousName> {
    let table = dialect.checked_quote(table)?;
    Ok(match schema.filter(|schema| !schema.is_empty()) {
        Some(schema) => format!("{}.{table}", dialect.checked_quote(schema)?),
        None => table,
    })
}

/// `SELECT * FROM <table>` without WHERE / ORDER BY. Kept in `Tab::sql` so
/// code that inspects the tab's statement (row identity, clone) still sees a
/// plain single-table SELECT.
pub fn base_select(dialect: SqlDialect, schema: Option<&str>, table: &str) -> Result<String, AmbiguousName> {
    Ok(format!("SELECT * FROM {}", qualified_table(dialect, schema, table)?))
}

/// Validates a WHERE / ORDER BY fragment. Quotes are `'…'`, `"…"` and `` `…` ``;
/// a doubled quote is an escaped quote. Where the dialect escapes backslashes
/// (`backslash_escapes`: MySQL family, ClickHouse) a backslash also escapes the
/// next character inside `'…'` and `"…"`, except that where `"…"` is an identifier
/// (`double_quote_identifiers`) a backslash inside `"…"` or `` `…` `` is refused
/// (`SqlDialect::backslash_in_name_is_ambiguous`: `ANSI_QUOTES` reads it as plain,
/// ClickHouse as an escape). `[…]` quotes a name where the dialect has
/// `bracket_identifiers` (SQLite, MariaDB `MSSQL`). The MySQL family (backtick
/// identifiers, ClickHouse included) treats `#` as a line comment in every
/// mode. PostgreSQL is lexed as `standard_conforming_strings = on` (the default
/// since 9.1): a backslash in a plain string is an ordinary character, except
/// in an `E'…'` string, and `$$…$$` / `$tag$…$tag$` are strings too.
///
/// The fragment must stay ONE boolean expression spliced into the tab's own
/// `SELECT * FROM <table>`: a statement separator, a comment (it would swallow
/// the engine's closing wrapper), an unterminated quote or parenthesis, and a
/// set operation outside parentheses (`WHERE 1 = 0 UNION SELECT * FROM other`
/// would show another table's rows as rows of this table) are all refused.
pub fn validate_fragment(dialect: SqlDialect, label: &str, text: &str) -> Result<(), String> {
    // PostgreSQL is the dialect that casts for LIKE (the same reading as
    // `parser_dialect`); SQLite has no E'' or dollar-quoted strings.
    let postgres = dialect.like_requires_text_cast && !dialect.backslash_escapes;
    let mysql_family = dialect.identifier_quote == '`' || dialect.backslash_escapes;
    let backslash_reading_unknown = dialect.backslash_in_name_is_ambiguous();
    let mut chars = text.char_indices().peekable();
    // The character that closes the quoted text being read (`]` for `[name]`).
    let mut quote: Option<char> = None;
    // A backslash escapes inside the string being read (`E'…'` on PostgreSQL).
    let mut escapes = false;
    let mut depth: i32 = 0;
    let mut word_start: Option<usize> = None;
    while let Some((at, ch)) = chars.next() {
        if let Some(open) = quote {
            if ch == '\\' && matches!(open, '"' | '`') && backslash_reading_unknown {
                return Err(format!(
                    "{label}: a backslash inside {open}…{open} is not allowed (servers read it differently)"
                ));
            }
            if ch == '\\' && escapes && open != '`' {
                chars.next(); // the escaped character, including a quote
            } else if ch == open {
                if chars.peek().map(|(_, next)| *next) == Some(open) {
                    chars.next(); // doubled quote is an escaped quote
                } else {
                    quote = None;
                }
            }
            continue;
        }
        if postgres && ch == '$' && word_start.is_none() {
            if let Some(end) = dollar_quote_end(text, at) {
                let end = end.ok_or_else(|| format!("{label}: unterminated quoted text"))?;
                while chars.peek().is_some_and(|(next, _)| *next < end) {
                    chars.next();
                }
                continue;
            }
        }
        if ch.is_alphanumeric() || ch == '_' || ch == '$' {
            word_start.get_or_insert(at);
            continue;
        }
        let word = word_start.take();
        if let Some(start) = word {
            check_word(label, depth, text, start..at)?;
        }
        match ch {
            '\'' | '"' | '`' => {
                quote = Some(ch);
                // `E'…'`: the word right in front is exactly the letter E.
                let escape_string = postgres
                    && ch == '\''
                    && word.is_some_and(|start| text[start..at].eq_ignore_ascii_case("e"));
                escapes = escape_string || (dialect.backslash_escapes && ch != '`');
            }
            // `[name]` (SQLite, MariaDB `MSSQL`): `]]` escapes `]`, a backslash is plain.
            '[' if dialect.bracket_identifiers => {
                quote = Some(']');
                escapes = false;
            }
            ';' => {
                return Err(format!(
                    "{label}: ';' is not allowed (a single statement is run)"
                ))
            }
            '#' if mysql_family => {
                return Err(format!("{label}: comments are not allowed"))
            }
            '-' if chars.peek().map(|(_, next)| *next) == Some('-') => {
                return Err(format!("{label}: comments are not allowed"))
            }
            '/' if chars.peek().map(|(_, next)| *next) == Some('*') => {
                return Err(format!("{label}: comments are not allowed"))
            }
            '(' => depth += 1,
            ')' => {
                depth -= 1;
                if depth < 0 {
                    return Err(format!("{label}: unbalanced ')'"));
                }
            }
            _ => {}
        }
    }
    if quote.is_some() {
        return Err(format!("{label}: unterminated quoted text"));
    }
    if let Some(start) = word_start {
        check_word(label, depth, text, start..text.len())?;
    }
    if depth != 0 {
        return Err(format!("{label}: unbalanced '('"));
    }
    Ok(())
}

/// Where the PostgreSQL dollar-quoted string opening at byte `at` (`$tag$`,
/// the tag empty or an identifier not starting with a digit) ends: `None` when
/// `$` here does not open one (`$1`), `Some(None)` when it is never closed.
fn dollar_quote_end(text: &str, at: usize) -> Option<Option<usize>> {
    let rest = &text[at + 1..];
    let tag_len = rest
        .char_indices()
        .find(|(_, ch)| !(ch.is_alphanumeric() || *ch == '_'))
        .map_or(rest.len(), |(index, _)| index);
    let tag = &rest[..tag_len];
    if !rest[tag_len..].starts_with('$') || tag.chars().next().is_some_and(|ch| ch.is_ascii_digit())
    {
        return None;
    }
    let delimiter = &text[at..at + tag_len + 2];
    let body = at + tag_len + 2;
    Some(text[body..].find(delimiter).map(|found| body + found + delimiter.len()))
}

/// A set operation outside parentheses turns the statement into a different
/// query (a word after a `.` is a qualified column name, not the keyword).
fn check_word(label: &str, depth: i32, text: &str, word: std::ops::Range<usize>) -> Result<(), String> {
    let name = &text[word.clone()];
    let set_operation = ["UNION", "INTERSECT", "EXCEPT"]
        .iter()
        .any(|keyword| name.eq_ignore_ascii_case(keyword));
    if depth == 0 && set_operation && !text[..word.start].ends_with('.') {
        return Err(format!(
            "{label}: {} is not allowed (a single condition is expected)",
            name.to_ascii_uppercase()
        ));
    }
    Ok(())
}

/// Removes a leading keyword the user may have typed (`WHERE id > 1`).
fn strip_keyword<'a>(text: &'a str, keyword: &str) -> &'a str {
    let trimmed = text.trim();
    match trimmed.get(..keyword.len()) {
        Some(head)
            if head.eq_ignore_ascii_case(keyword)
                && trimmed[keyword.len()..]
                    .chars()
                    .next()
                    .is_none_or(char::is_whitespace) =>
        {
            trimmed[keyword.len()..].trim()
        }
        _ => trimmed,
    }
}

/// Removes a leading `ORDER BY` (any whitespace between the words).
fn strip_order_by(text: &str) -> &str {
    let trimmed = text.trim();
    let rest = strip_keyword(trimmed, "ORDER");
    if rest.len() == trimmed.len() {
        return trimmed;
    }
    let after_by = strip_keyword(rest, "BY");
    if after_by.len() == rest.len() {
        // `ORDER` without `BY` is a column named order, not the keyword.
        return trimmed;
    }
    after_by
}

/// WHERE / ORDER BY fragments with a typed leading keyword removed.
pub fn normalized_fragments(where_clause: &str, order_by: &str) -> (String, String) {
    (
        strip_keyword(where_clause, "WHERE").to_string(),
        strip_order_by(order_by).to_string(),
    )
}

/// Builds the table-data statement. Empty fragments are omitted.
pub fn build_table_sql(
    dialect: SqlDialect,
    schema: Option<&str>,
    table: &str,
    where_clause: &str,
    order_by: &str,
) -> Result<String, String> {
    if table.is_empty() {
        return Err("No table selected".into());
    }
    let mut sql = base_select(dialect, schema, table).map_err(|error| error.to_string())?;
    let (where_clause, order_by) = normalized_fragments(where_clause, order_by);
    let (where_clause, order_by) = (where_clause.as_str(), order_by.as_str());
    validate_fragment(dialect, "WHERE", where_clause)?;
    validate_fragment(dialect, "ORDER BY", order_by)?;
    if !where_clause.is_empty() {
        sql.push_str("\nWHERE ");
        sql.push_str(where_clause);
    }
    if !order_by.is_empty() {
        sql.push_str("\nORDER BY ");
        sql.push_str(order_by);
    }
    Ok(sql)
}

/// First sort term of an ORDER BY fragment: `(column, descending)`.
pub fn primary_sort(order_by: &str) -> Option<(String, bool)> {
    let text = strip_order_by(order_by);
    let mut term = String::new();
    let mut quote: Option<char> = None;
    let mut depth = 0;
    for ch in text.chars() {
        match quote {
            Some(open) => {
                term.push(ch);
                if ch == open {
                    quote = None;
                }
            }
            None => match ch {
                '\'' | '"' | '`' => {
                    quote = Some(ch);
                    term.push(ch);
                }
                '(' => {
                    depth += 1;
                    term.push(ch);
                }
                ')' => {
                    depth -= 1;
                    term.push(ch);
                }
                ',' if depth == 0 => break,
                _ => term.push(ch),
            },
        }
    }
    let term = term.trim();
    if term.is_empty() {
        return None;
    }
    let (column, rest) = if let Some(open) = term.chars().next().filter(|c| matches!(c, '"' | '`'))
    {
        // Quoted identifier: up to the next unescaped closing quote.
        let mut name = String::new();
        let mut chars = term[1..].char_indices().peekable();
        let mut end = term.len();
        while let Some((index, ch)) = chars.next() {
            if ch == open {
                if matches!(chars.peek(), Some((_, next)) if *next == open) {
                    name.push(open);
                    chars.next();
                } else {
                    end = index + 2; // opening quote + closing quote
                    break;
                }
            } else {
                name.push(ch);
            }
        }
        (name, term.get(end..).unwrap_or("").trim())
    } else {
        match term.split_once(char::is_whitespace) {
            Some((name, rest)) => (name.to_string(), rest.trim()),
            None => (term.to_string(), ""),
        }
    };
    if column.contains(['(', ')', ',']) && !term.starts_with(['"', '`']) {
        return None; // expression, not a plain column
    }
    let column = column.rsplit('.').next().unwrap_or(&column).to_string();
    let rest = rest.to_ascii_uppercase();
    let descending = rest == "DESC" || rest.starts_with("DESC ");
    let plain = rest.is_empty()
        || rest == "ASC"
        || rest == "DESC"
        || rest.starts_with("ASC ")
        || rest.starts_with("DESC ");
    plain.then_some((column, descending))
}

/// Header-click cycle for `column`: none/other -> ASC -> DESC -> cleared.
/// Returns the new ORDER BY fragment (empty = default order).
pub fn next_sort(dialect: SqlDialect, current: &str, column: &str) -> String {
    match primary_sort(current) {
        Some((name, false)) if name == column => {
            format!("{} DESC", dialect.quote(column))
        }
        Some((name, true)) if name == column => String::new(),
        _ => format!("{} ASC", dialect.quote(column)),
    }
}

/// Explicit direction (context menu): `"col" ASC|DESC`.
pub fn sort_term(dialect: SqlDialect, column: &str, descending: bool) -> String {
    format!(
        "{} {}",
        dialect.quote(column),
        if descending { "DESC" } else { "ASC" }
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn builds_quoted_statement_per_dialect() {
        let pg = build_table_sql(SqlDialect::POSTGRES, Some("public"), "orders", "", "").unwrap();
        assert_eq!(pg, "SELECT * FROM \"public\".\"orders\"");
        let lite = build_table_sql(SqlDialect::SQLITE, Some("main"), "we\"ird", "", "").unwrap();
        assert_eq!(lite, "SELECT * FROM \"main\".\"we\"\"ird\"");
        let my = build_table_sql(SqlDialect::MYSQL, Some("shop"), "or`ders", "", "").unwrap();
        assert_eq!(my, "SELECT * FROM `shop`.`or``ders`");
        let bare = build_table_sql(SqlDialect::POSTGRES, None, "t", "", "").unwrap();
        assert_eq!(bare, "SELECT * FROM \"t\"");
    }

    /// A backslash inside backticks escapes on ClickHouse and not on MySQL under `ANSI_QUOTES`
    /// (the same flags): a table named ``a\` UNION ALL SELECT ... -- `` would end the identifier on
    /// one of them. The tab's statement is refused for such a name, and still quotes every other.
    #[test]
    fn a_backslash_in_a_table_or_schema_name_is_refused_where_the_dialect_cannot_quote_it() {
        let hostile = "a\\` UNION ALL SELECT secret FROM vault -- ";
        let ansi_mysql = SqlDialect { double_quote_identifiers: true, ..SqlDialect::MYSQL };
        assert_eq!(ansi_mysql, SqlDialect::CLICKHOUSE);
        for dialect in [SqlDialect::CLICKHOUSE, SqlDialect::for_kind(ConnectionKind::Clickhouse), ansi_mysql] {
            for (schema, table) in [(Some("shop"), hostile), (Some(hostile), "t"), (None, hostile), (Some("d\\b"), "t")] {
                let error = qualified_table(dialect, schema, table).unwrap_err();
                assert!(error.name.contains('\\'), "{error:?}");
                assert_eq!(base_select(dialect, schema, table), Err(error.clone()));
                let shown = build_table_sql(dialect, schema, table, "", "").unwrap_err();
                assert_eq!(shown, error.to_string());
                assert!(shown.contains("backslash"), "{shown}");
            }
            // Names without one are quoted as before, a doubled quote included.
            assert_eq!(qualified_table(dialect, Some("shop"), "or`ders").unwrap(), "`shop`.`or``ders`");
        }
        // MySQL / MariaDB in their default mode read a backslash as plain text in backticks, and
        // PostgreSQL and SQLite never escape in a name: a backslash is an ordinary character there.
        assert_eq!(qualified_table(SqlDialect::MYSQL, Some("d\\b"), "t\\").unwrap(), "`d\\b`.`t\\`");
        assert_eq!(qualified_table(SqlDialect::POSTGRES, None, "a\\b").unwrap(), "\"a\\b\"");
        assert_eq!(qualified_table(SqlDialect::SQLITE, None, "a\\b").unwrap(), "\"a\\b\"");
    }

    #[test]
    fn appends_where_and_order_by_and_strips_typed_keywords() {
        let sql = build_table_sql(
            SqlDialect::POSTGRES,
            Some("public"),
            "orders",
            "id > 10 AND status = 'active'",
            "created DESC",
        )
        .unwrap();
        assert_eq!(
            sql,
            "SELECT * FROM \"public\".\"orders\"\nWHERE id > 10 AND status = 'active'\nORDER BY created DESC"
        );
        let typed = build_table_sql(
            SqlDialect::POSTGRES,
            Some("public"),
            "orders",
            "where id = 1",
            "Order By id",
        )
        .unwrap();
        assert!(typed.ends_with("WHERE id = 1\nORDER BY id"), "{typed}");
        // A column merely starting with the keyword is not stripped.
        let column = build_table_sql(SqlDialect::POSTGRES, None, "t", "whereabouts = 1", "").unwrap();
        assert!(column.contains("WHERE whereabouts = 1"));
    }

    /// A fragment is spliced into `SELECT * FROM <table> WHERE <fragment>`: a set
    /// operation outside parentheses would make the statement return ANOTHER
    /// table's rows, which the grid then treats as rows of this table (editable
    /// by the key columns of this table).
    #[test]
    fn a_set_operation_outside_parentheses_is_refused() {
        let strict = SqlDialect {
            backslash_escapes: false,
            ..SqlDialect::MYSQL
        };
        for dialect in [SqlDialect::POSTGRES, SqlDialect::SQLITE, SqlDialect::MYSQL, strict] {
            for fragment in [
                "1 = 0 UNION SELECT * FROM other",
                "1=0 union all select * from other",
                "(a = 1)UNION(SELECT * FROM other)",
                "a = 1\nINTERSECT SELECT * FROM other",
                "a = 1 EXCEPT SELECT * FROM other",
                "a IN (1) OR b = 2 UNION SELECT 1",
            ] {
                let error = validate_fragment(dialect, "WHERE", fragment).unwrap_err();
                assert!(
                    error.contains("UNION") || error.contains("INTERSECT") || error.contains("EXCEPT"),
                    "{fragment}: {error}"
                );
                assert!(build_table_sql(dialect, Some("public"), "orders", fragment, "").is_err());
            }
            assert!(validate_fragment(dialect, "ORDER BY", "id UNION SELECT 1").is_err());
            // Inside parentheses, inside quotes, qualified, or part of a longer word.
            for fragment in [
                "id IN (SELECT id FROM a UNION SELECT id FROM b)",
                "note = 'a UNION b'",
                "\"union\" = 1",
                "t.union = 1",
                "reunion = 1 AND unions = 2 AND union_all = 3",
            ] {
                assert!(
                    validate_fragment(dialect, "WHERE", fragment).is_ok(),
                    "{dialect:?} {fragment}"
                );
            }
        }
    }

    /// Where a backslash is an escape the lexer must agree with the server, or
    /// text the server reads as outside a string hides inside one.
    #[test]
    fn the_sessions_backslash_mode_decides_what_is_inside_a_string() {
        let escaped_quote = r"a = 'it\' UNION SELECT 1 -- '";
        // Backslash escapes on: `\'` does not end the string, the whole tail is text.
        assert!(validate_fragment(SqlDialect::MYSQL, "WHERE", escaped_quote).is_ok());
        // NO_BACKSLASH_ESCAPES: the string ends at `\'`; UNION is then outside it.
        let strict = SqlDialect {
            backslash_escapes: false,
            ..SqlDialect::MYSQL
        };
        let error = validate_fragment(strict, "WHERE", escaped_quote).unwrap_err();
        assert!(error.contains("UNION"), "{error}");
        // Separators, comments and unterminated quotes keep their messages.
        for fragment in ["a = 1; DROP TABLE x", "a = 1 -- tail", "a = 1 /* c", "a = 'x"] {
            assert!(validate_fragment(SqlDialect::POSTGRES, "WHERE", fragment).is_err(), "{fragment}");
        }
    }

    /// `ANSI_QUOTES` and MariaDB `MSSQL` as the driver reports them: what the server
    /// reads as an identifier must not hide a `UNION` from the check.
    #[test]
    fn the_sessions_identifier_quoting_decides_what_is_inside_a_name() {
        let ansi = SqlDialect {
            double_quote_identifiers: true,
            ..SqlDialect::MYSQL
        };
        // To an ANSI_QUOTES server `"a\"` is the name a\ and UNION is code; ClickHouse
        // (same flags) reads one name. Neither reading is guessed.
        let hidden = r#""a\" = 1 UNION SELECT 1 FROM other -- " = 1"#;
        assert!(validate_fragment(SqlDialect::MYSQL, "WHERE", hidden).is_ok());
        let error = validate_fragment(ansi, "WHERE", hidden).unwrap_err();
        assert!(error.contains("backslash"), "{error}");
        assert!(validate_fragment(ansi, "WHERE", "\"qty\" > 0 AND note = 'it\\'s'").is_ok());
        // `[x']` is one name under MSSQL, so the UNION after it is outside any quote.
        let mssql = SqlDialect {
            bracket_identifiers: true,
            ..ansi
        };
        let bracketed = "[x'] = 1 UNION SELECT 1 FROM other -- '] = 1";
        let error = validate_fragment(mssql, "WHERE", bracketed).unwrap_err();
        assert!(error.contains("UNION"), "{error}");
        assert!(validate_fragment(mssql, "WHERE", "[a]]b] = 1").is_ok());
        assert!(validate_fragment(mssql, "WHERE", "[a = 1").is_err());
        // SQLite: `;` inside a bracketed name is part of the name.
        assert!(validate_fragment(SqlDialect::SQLITE, "WHERE", "[a;b] = 1").is_ok());
    }

    /// The three strings of the review: each hides `UNION SELECT` inside what a
    /// lexer that does not know the server's quoting reads as a string or a
    /// comment.
    #[test]
    fn postgresql_e_strings_and_dollar_quotes_and_mysql_hash_comments_cannot_hide_a_union() {
        let strict_mysql = SqlDialect {
            backslash_escapes: false,
            ..SqlDialect::MYSQL
        };
        let bypasses = [
            (SqlDialect::POSTGRES, r"a = E'\'' UNION SELECT * FROM other --'"),
            (SqlDialect::POSTGRES, "a = $$'$$ UNION SELECT * FROM other --'"),
            (SqlDialect::POSTGRES, "a = $t$'$t$ UNION SELECT * FROM other --'"),
            (strict_mysql, "a = 1 #'\nUNION SELECT * FROM other -- '"),
            (SqlDialect::MYSQL, "a = 1 #'\nUNION SELECT * FROM other -- '"),
        ];
        for (dialect, fragment) in bypasses {
            assert!(
                validate_fragment(dialect, "WHERE", fragment).is_err(),
                "{dialect:?}: {fragment}"
            );
        }
        // Look-alikes that are plain conditions.
        for fragment in [
            "a = E'x'",
            r"a = E'it\'s'",
            r"a = E'a\\b' AND b = 1",
            "a = $$x$$",
            "a = $$it's; -- not a comment$$",
            "a = $tag$ UNION $tag$ AND b = $1",
            "price$ = 1 AND $1 = 2",
            "name = 'a # b'",
        ] {
            assert!(
                validate_fragment(SqlDialect::POSTGRES, "WHERE", fragment).is_ok(),
                "{fragment}"
            );
        }
        // `E` is only the prefix when it stands alone (here the server reads
        // the same plain strings the lexer does); SQLite has neither feature.
        assert!(validate_fragment(SqlDialect::POSTGRES, "WHERE", r"nameE'\'' UNION SELECT 1 --'").is_ok());
        assert!(validate_fragment(SqlDialect::SQLITE, "WHERE", "a = E'x'").is_ok());
        assert!(validate_fragment(SqlDialect::SQLITE, "WHERE", "a = $$x$$ UNION SELECT 1").is_err());
        // A `#` inside a string, or in PostgreSQL / SQLite, is not a comment.
        for dialect in [SqlDialect::MYSQL, strict_mysql] {
            assert!(validate_fragment(dialect, "WHERE", "a = '#x' AND b = \"#y\"").is_ok());
            assert!(validate_fragment(dialect, "WHERE", "a = 1 # tail").is_err());
        }
        assert!(validate_fragment(SqlDialect::POSTGRES, "WHERE", "a # b = 1").is_ok());
        // An unterminated dollar quote is refused.
        assert!(validate_fragment(SqlDialect::POSTGRES, "WHERE", "a = $$x").is_err());
    }

    #[test]
    fn rejects_semicolons_and_comments_outside_quotes_only() {
        assert!(validate_fragment(SqlDialect::POSTGRES, "WHERE", "a = 1; DROP TABLE x").is_err());
        assert!(validate_fragment(SqlDialect::POSTGRES, "WHERE", "a = 1 -- tail").is_err());
        assert!(validate_fragment(SqlDialect::POSTGRES, "WHERE", "a = 1 /* c */").is_err());
        assert!(validate_fragment(SqlDialect::POSTGRES, "WHERE", "(a = 1").is_err());
        assert!(validate_fragment(SqlDialect::POSTGRES, "WHERE", "a = 1)").is_err());
        assert!(validate_fragment(SqlDialect::POSTGRES, "WHERE", "a = 'unterminated").is_err());
        assert!(validate_fragment(SqlDialect::POSTGRES, "WHERE", "note = 'a;b -- not a comment'").is_ok());
        assert!(validate_fragment(SqlDialect::POSTGRES, "WHERE", "note = 'it''s; fine'").is_ok());
        assert!(validate_fragment(SqlDialect::POSTGRES, "WHERE", "\"we;ird\" = 1").is_ok());
        let error = build_table_sql(SqlDialect::POSTGRES, None, "t", "a = 1;", "").unwrap_err();
        assert!(error.contains("';'"), "{error}");
    }

    #[test]
    fn backslash_escapes_are_lexed_per_dialect_so_no_second_statement_slips_through() {
        let clickhouse = SqlDialect::from_capabilities(&driver_info("clickhouse"));
        // MySQL / ClickHouse read `\'` as an escaped quote: this is a complete
        // literal followed by `; DROP TABLE`. A lexer that only knows doubled
        // quotes saw one long literal and let it through.
        let attack = r"x = '\'' ; DROP TABLE t; SELECT '\''";
        for (name, dialect) in [("mysql", SqlDialect::MYSQL), ("clickhouse", clickhouse)] {
            assert!(validate_fragment(dialect, "WHERE", attack).is_err(), "{name}");
            // `\'` stays inside the literal, `;` with it.
            assert!(validate_fragment(dialect, "WHERE", r"x = 'it\'s; fine'").is_ok(), "{name}");
            // `\\` is an escaped backslash: the quote after it closes.
            assert!(validate_fragment(dialect, "WHERE", r"x = 'a\\' OR y = ';'").is_ok(), "{name}");
            assert!(validate_fragment(dialect, "WHERE", r"x = 'a\\' ; DROP TABLE t").is_err(), "{name}");
            // An escaped quote leaves the literal open.
            assert!(validate_fragment(dialect, "WHERE", r"x = 'a\' OR y = 1").is_err(), "{name}");
            // `#` starts a comment outside quotes only; backticks do not escape.
            assert!(validate_fragment(dialect, "WHERE", "x = 1 # tail").is_err(), "{name}");
            assert!(validate_fragment(dialect, "WHERE", "x = '#'").is_ok(), "{name}");
        }
        // MySQL (default session): a backslash in backticks is plain.
        assert!(validate_fragment(SqlDialect::MYSQL, "WHERE", r"`a\` = 1").is_ok());
        // ClickHouse escapes in backticks too: it reads one name, then a live UNION. The
        // flags match MySQL under ANSI_QUOTES, so neither reading is guessed.
        let hidden = r"x = `a\` OR y = '` = 1 UNION SELECT * FROM other -- '";
        assert!(validate_fragment(SqlDialect::MYSQL, "WHERE", hidden).is_ok());
        let error = validate_fragment(clickhouse, "WHERE", hidden).unwrap_err();
        assert!(error.contains("backslash"), "{error}");
        assert!(validate_fragment(clickhouse, "WHERE", "`a` = 1").is_ok());
        // PostgreSQL (standard_conforming_strings = on) and SQLite: a backslash
        // is an ordinary character, only a doubled quote escapes.
        for (name, dialect) in [("postgres", SqlDialect::POSTGRES), ("sqlite", SqlDialect::SQLITE)] {
            assert!(validate_fragment(dialect, "WHERE", attack).is_ok(), "{name}");
            assert!(validate_fragment(dialect, "WHERE", r"x = 'a\' OR y = 1").is_ok(), "{name}");
            assert!(validate_fragment(dialect, "WHERE", r"x = 'a\' ; DROP TABLE t").is_err(), "{name}");
            assert!(validate_fragment(dialect, "WHERE", "x = 1 # tail").is_ok(), "{name}");
        }
    }

    #[test]
    fn primary_sort_parses_quoted_and_plain_terms() {
        assert_eq!(primary_sort("created DESC"), Some(("created".into(), true)));
        assert_eq!(primary_sort("\"my col\" asc, id"), Some(("my col".into(), false)));
        assert_eq!(primary_sort("\"a\"\"b\" DESC"), Some(("a\"b".into(), true)));
        assert_eq!(primary_sort("t.id"), Some(("id".into(), false)));
        assert_eq!(primary_sort("lower(name)"), None);
        assert_eq!(primary_sort(""), None);
    }

    #[test]
    fn header_click_cycles_asc_desc_default() {
        let d = SqlDialect::POSTGRES;
        let first = next_sort(d, "", "status");
        assert_eq!(first, "\"status\" ASC");
        let second = next_sort(d, &first, "status");
        assert_eq!(second, "\"status\" DESC");
        assert_eq!(next_sort(d, &second, "status"), "");
        // Another column restarts at ASC.
        assert_eq!(next_sort(d, &second, "id"), "\"id\" ASC");
    }

    fn info(identifier: &str, string: &str, text_cast: bool) -> engine_driver_api::SqlDialectInfo {
        engine_driver_api::SqlDialectInfo {
            identifier_quote: identifier.into(),
            string_literal_quote: string.into(),
            supports_schemas: true,
            database_qualified_names: true,
            like_requires_text_cast: text_cast,
            string_backslash_escapes: false,
            double_quote_identifiers: false,
            bracket_identifiers: false,
        }
    }

    #[test]
    fn dialect_from_driver_descriptor_matches_per_kind_fallback() {
        let real = |quote: &str, schemas: bool, cast: bool| {
            let mut info = info(quote, "'", cast);
            info.supports_schemas = schemas;
            // The MySQL family is the one that escapes backslashes.
            info.string_backslash_escapes = quote == "`";
            info.double_quote_identifiers = quote == "\"";
            info
        };
        let sqlite = |mut info: engine_driver_api::SqlDialectInfo| {
            info.bracket_identifiers = true;
            info
        };
        assert_eq!(
            SqlDialect::from_capabilities(&real("\"", true, true)),
            SqlDialect::for_kind(ConnectionKind::Postgres)
        );
        assert_eq!(
            SqlDialect::from_capabilities(&sqlite(real("\"", false, false))),
            SqlDialect::for_kind(ConnectionKind::Sqlite)
        );
        assert_eq!(
            SqlDialect::from_capabilities(&real("`", false, false)),
            SqlDialect::for_kind(ConnectionKind::Mysql)
        );
        // ClickHouse also reads `"…"` as a name (what its driver reports).
        let mut clickhouse = real("`", false, false);
        clickhouse.double_quote_identifiers = true;
        assert_eq!(
            SqlDialect::from_capabilities(&clickhouse),
            SqlDialect::for_kind(ConnectionKind::Clickhouse)
        );
    }

    #[test]
    fn every_dialect_flag_comes_from_the_descriptor_not_from_its_shape() {
        // Double-quoted with schemas, like PostgreSQL, but LIKE accepts any
        // operand: the old shape heuristic would have cast and misquoted.
        let no_cast = SqlDialect::from_capabilities(&info("\"", "'", false));
        assert!(!no_cast.like_requires_text_cast);
        assert!(!no_cast.backslash_escapes);
        // The same shape with the flag set casts.
        assert!(SqlDialect::from_capabilities(&info("\"", "'", true)).like_requires_text_cast);
        // A backtick dialect that also needs the cast keeps both behaviors,
        // and does NOT get backslash escaping from its quote.
        let backtick = SqlDialect::from_capabilities(&info("`", "'", true));
        assert_eq!(backtick.quote("a`b"), "`a``b`");
        assert!(backtick.like_requires_text_cast);
        assert!(!backtick.backslash_escapes);
        // The flag alone decides, whatever the identifier quote.
        let mut escaping = info("\"", "'", false);
        escaping.string_backslash_escapes = true;
        assert!(SqlDialect::from_capabilities(&escaping).backslash_escapes);
        // The advertised quotes are used for identifiers and literals.
        let odd = SqlDialect::from_capabilities(&info("\"", "\"", false));
        assert_eq!(odd.string_quote, '"');
        assert_eq!(crate::sql_filter::sql_string_literal(odd, "a\"b"), "\"a\"\"b\"");
        // A missing or multi-character quote keeps the standard-SQL default.
        let blank = SqlDialect::from_capabilities(&info("", "''", false));
        assert_eq!((blank.identifier_quote, blank.string_quote), ('"', '\''));
    }

    /// The descriptor of each SQL driver, as it advertises it.
    fn driver_info(driver: &str) -> engine_driver_api::SqlDialectInfo {
        match driver {
            "mysql" | "mariadb" | "clickhouse" => engine_driver_api::SqlDialectInfo {
                identifier_quote: "`".into(),
                string_literal_quote: "'".into(),
                supports_schemas: false,
                database_qualified_names: true,
                like_requires_text_cast: false,
                string_backslash_escapes: true,
                double_quote_identifiers: driver == "clickhouse",
                bracket_identifiers: false,
            },
            "postgresql" => engine_driver_api::SqlDialectInfo {
                identifier_quote: "\"".into(),
                string_literal_quote: "'".into(),
                supports_schemas: true,
                database_qualified_names: false,
                like_requires_text_cast: true,
                string_backslash_escapes: false,
                double_quote_identifiers: true,
                bracket_identifiers: false,
            },
            _ => engine_driver_api::SqlDialectInfo {
                identifier_quote: "\"".into(),
                string_literal_quote: "'".into(),
                supports_schemas: false,
                database_qualified_names: false,
                like_requires_text_cast: false,
                string_backslash_escapes: false,
                double_quote_identifiers: true,
                bracket_identifiers: true,
            },
        }
    }

    /// The text a SQL parser reads back from a string literal.
    fn parse_literal(literal: &str, backslash_escapes: bool) -> String {
        let mut chars = literal[1..literal.len() - 1].chars().peekable();
        let mut out = String::new();
        while let Some(ch) = chars.next() {
            match ch {
                '\'' => {
                    assert_eq!(chars.next(), Some('\''), "a lone quote ends the literal");
                    out.push('\'');
                }
                '\\' if backslash_escapes => out.push(chars.next().expect("escaped character")),
                _ => out.push(ch),
            }
        }
        out
    }

    #[test]
    fn a_value_with_backslash_and_quote_round_trips_through_where_building_per_dialect() {
        use crate::sql_filter::{
            rule_condition, SqlFilterColumnKind, SqlFilterMode, SqlFilterRule,
        };
        let value = "C:\\dir\\it's\\";
        let rule = SqlFilterRule {
            id: 1,
            column: "path".into(),
            mode: SqlFilterMode::Equals,
            value: value.into(),
            ..Default::default()
        };
        for (driver, expected) in [
            ("mysql", "`path` = 'C:\\\\dir\\\\it''s\\\\'"),
            ("mariadb", "`path` = 'C:\\\\dir\\\\it''s\\\\'"),
            ("clickhouse", "`path` = 'C:\\\\dir\\\\it''s\\\\'"),
            ("postgresql", "\"path\" = 'C:\\dir\\it''s\\'"),
            ("sqlite", "\"path\" = 'C:\\dir\\it''s\\'"),
        ] {
            let info = driver_info(driver);
            let dialect = SqlDialect::from_capabilities(&info);
            assert_eq!(dialect.backslash_escapes, info.string_backslash_escapes, "{driver}");
            let condition =
                rule_condition(dialect, SqlFilterColumnKind::Text, &rule).expect("active rule");
            assert_eq!(condition, expected, "{driver}");
            // The literal parses back to the typed value under that dialect's
            // rules, and the finished statement passes the fragment check.
            let literal = condition.split_once(" = ").expect("equality").1;
            assert_eq!(parse_literal(literal, dialect.backslash_escapes), value, "{driver}");
            let sql = build_table_sql(dialect, None, "files", &condition, "").expect(driver);
            assert!(sql.ends_with(&format!("\nWHERE {condition}")), "{driver}: {sql}");
        }
        // Reading a MySQL literal with PostgreSQL rules (the old inference
        // for a double-quoted MySQL ANSI_QUOTES dialect) would change the value.
        let mysql_literal = crate::sql_filter::sql_string_literal(
            SqlDialect::from_capabilities(&driver_info("mysql")),
            value,
        );
        assert_ne!(parse_literal(&mysql_literal, false), value);
    }

    #[test]
    fn descriptor_flags_change_the_generated_like_filter() {
        use crate::sql_filter::{
            rule_condition, SqlFilterColumnKind, SqlFilterMode, SqlFilterRule,
        };
        let rule = SqlFilterRule {
            id: 1,
            column: "n".into(),
            mode: SqlFilterMode::Like,
            value: "1".into(),
            ..Default::default()
        };
        let sql = |text_cast| {
            rule_condition(
                SqlDialect::from_capabilities(&info("\"", "'", text_cast)),
                SqlFilterColumnKind::Number,
                &rule,
            )
            .unwrap()
        };
        assert!(sql(true).starts_with("\"n\"::text LIKE"), "{}", sql(true));
        assert!(sql(false).starts_with("\"n\" LIKE"), "{}", sql(false));
    }
}
