//! The order a source is read in. Pages are separate statements, so every page must come out of the same total order:
//! with ties (rows equal on every ordered column) a row can be read twice and another not at all, silently.
//!
//! * a table with a unique key is read in the order of its key (by key, see `reader`: no offset at all);
//! * a table without one is ordered by every column the server can order, so only rows identical in all of them can tie
//!   (and identical rows are interchangeable);
//! * a ClickHouse table has a sorting key, which is not unique: it comes first, then the other columns;
//! * a query typed by the user is wrapped, `SELECT * FROM (<query>) AS transfer_page ORDER BY "a", "b", …` by the names of its
//!   output columns (SQLite: by their positions, as it reads an unknown name as a string); one with an `ORDER BY` of its own
//!   keeps it as the leading key (its terms that name an output column or a position) and the columns follow as tie-breakers;
//!   one whose `ORDER BY` names something else, or that has a `LIMIT` and no order, is read as written and said to be unsure;
//!   one with an order *and* a `LIMIT` is wrapped too, and said to pick its rows by an order that may tie.
//!
//! Text compares the way the column's collation says, and a collation can call two different strings equal (`a`, `A`, `á`,
//! `a ` under MySQL's `_ci`/PAD SPACE, SQLite's `NOCASE`, PostgreSQL's `citext` or a non-deterministic ICU collation): the
//! order of a keyless table or a query compares text *by its bytes* (`CAST(col AS BINARY)`, `col COLLATE BINARY`,
//! `(col)::text COLLATE "C"`), so two rows tie only when they are identical.
//!
//! Columns the server cannot order (JSON, arrays, user types, the unbounded text of MySQL, whose sort looks at a prefix) are
//! left out, and what is left is said: rows that tie on every ordered column may be read twice or missed.

use engine_driver_kit::sql::{tokens, SqlLexOptions, SqlToken, SqlTokenKind};

use super::super::results::column_type::base_type_head;
use super::typemap::{family_of, Family};
use crate::connections::ConnectionKind;

/// How well the pages of a read follow each other.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(in crate::app) enum RowOrder {
    /// By key: each page starts after the last key of the one before.
    Key,
    /// By offset, in the order of a unique key.
    UniqueKey,
    /// By offset, in the order of every column: only identical rows tie.
    Total,
    /// By offset, in the order of every column the server can order; these cannot be ordered.
    Partial(Vec<String>),
    /// Both: the query has an `ORDER BY` and a `LIMIT` or `OFFSET` *and* columns the server cannot order.
    PartialLimited(Vec<String>),
    /// The statement is read in the order it is written in (a statement made by the program).
    Own,
    /// The query has an `ORDER BY` of its own that the transfer cannot extend to a total order.
    OwnUnmapped,
    /// The query has an `ORDER BY` and a `LIMIT`, `OFFSET` or `DISTINCT ON`: the pages are in a total order, but which rows the query returns
    /// is chosen by its own order, which may tie.
    OwnLimited,
    /// By offset, with no order the transfer can make: why.
    Unordered(&'static str),
}

impl RowOrder {
    /// What the user should be told, when the pages may not follow each other.
    pub(in crate::app) fn warning(&self, label: &str) -> Option<String> {
        let unorderable = |columns: &[String]| {
            format!(
                "{label} is read in pages ordered by every column except {}, which the server cannot order: rows equal in all the others may be read twice or missed.",
                columns.iter().map(|column| format!("\"{column}\"")).collect::<Vec<_>>().join(", ")
            )
        };
        const LIMITED: &str = "the rows its LIMIT or DISTINCT ON picks are chosen by an order that may tie; rows at the boundary may differ between pages.";
        match self {
            Self::Partial(columns) => Some(unorderable(columns)),
            Self::PartialLimited(columns) => Some(format!("{} Also, {LIMITED}", unorderable(columns))),
            Self::OwnUnmapped => Some(format!("{label} is read in the order of its own ORDER BY, which does not name only columns of its result: rows equal on it may be read twice or missed.")),
            Self::OwnLimited => Some(format!("{label} is read in pages, and {LIMITED}")),
            Self::Unordered(reason) => Some(format!("{label} is read in pages with no total order ({reason}): rows may be read twice or missed.")),
            Self::Key | Self::UniqueKey | Self::Total | Self::Own => None,
        }
    }
}

/// A column of a table, in table order, with whether it belongs to the key.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(in crate::app) struct OrderColumn {
    pub(in crate::app) name: String,
    pub(in crate::app) data_type: String,
}

/// The lexer of a dialect: the one the server reads its statements with (backtick names holding a `(`, `#` comments, …).
pub(in crate::app) fn lex_options(kind: ConnectionKind) -> SqlLexOptions {
    match kind {
        ConnectionKind::Mysql => SqlLexOptions::mysql(),
        ConnectionKind::Mariadb => SqlLexOptions::mariadb(),
        ConnectionKind::Sqlite => SqlLexOptions::sqlite(),
        ConnectionKind::Clickhouse => SqlLexOptions::clickhouse(),
        _ => SqlLexOptions::postgres(),
    }
}

/// The lexer of a connection whose driver reported `dialect` (see [`SqlDialect`]): the preset of its kind moved by what the
/// session set (`NO_BACKSLASH_ESCAPES`, `ANSI_QUOTES`, MariaDB's `MSSQL` brackets) and, for MySQL and MariaDB, by the server's
/// `server_version` (a versioned `/*!N … */` comment is code or comment by it), the same moves as [`session_lex_options`] makes
/// from the driver's reply.
pub(in crate::app) fn dialect_lex_options(kind: ConnectionKind, dialect: crate::sql_table::SqlDialect, server_version: Option<u32>) -> SqlLexOptions {
    let mut options = lex_options(kind);
    options.backslash_escapes &= dialect.backslash_escapes;
    options.double_quote_is_identifier |= dialect.double_quote_identifiers;
    options.bracket_identifiers |= dialect.bracket_identifiers;
    read_by_version(kind, options, server_version)
}

/// The lexer of a session: the dialect's preset moved by what the driver reported for it (`sql_mode`: `NO_BACKSLASH_ESCAPES`,
/// `ANSI_QUOTES`, MariaDB's `MSSQL` brackets) and, for MySQL and MariaDB, by the server's version (a versioned `/*!N … */`
/// comment is code or comment by it). The same as `explain` reads a statement with.
pub(in crate::app) fn session_lex_options(kind: ConnectionKind, info: Option<&engine_driver_api::SqlDialectInfo>, server_version: Option<u32>) -> SqlLexOptions {
    let options = info.map_or_else(|| lex_options(kind), |info| lex_options(kind).with_session_dialect(info));
    read_by_version(kind, options, server_version)
}

fn read_by_version(kind: ConnectionKind, options: SqlLexOptions, server_version: Option<u32>) -> SqlLexOptions {
    match (kind, server_version) {
        (ConnectionKind::Mysql | ConnectionKind::Mariadb, Some(version)) => options.with_server_version(version),
        _ => options,
    }
}

/// Whether the statement has a locking clause (`FOR UPDATE`, `FOR SHARE`, `FOR NO KEY UPDATE`, `FOR KEY SHARE`, `LOCK IN SHARE
/// MODE`): read for the length of a transfer it would hold row locks on the source. A word pair in the token stream, so a
/// string or a comment holding the words does not count.
pub(in crate::app) fn has_locking_clause(sql: &str, options: &SqlLexOptions) -> bool {
    let found = tokens(sql, options);
    let words: Vec<String> = found.tokens.iter().map(|token| word_of(sql, token).unwrap_or_default()).collect();
    let at = |index: usize, word: &str| words.get(index).is_some_and(|spelled| spelled == word);
    (0..words.len()).any(|i| {
        at(i, "FOR") && (at(i + 1, "UPDATE") || at(i + 1, "SHARE") || at(i + 1, "NO") && at(i + 2, "KEY") || at(i + 1, "KEY") && at(i + 2, "SHARE")) || at(i, "LOCK") && at(i + 1, "IN") && at(i + 2, "SHARE")
    })
}

/// The most bytes of a value MySQL and MariaDB sort by (`max_sort_length`, 1024 unless the server was set otherwise): a longer
/// value that differs from another only after them ties with it.
const MAX_SORT_LENGTH: u32 = 1024;

/// Whether the server `kind` orders values of `data_type` completely.
pub(in crate::app) fn orderable(kind: ConnectionKind, data_type: &str) -> bool {
    // SQLite orders every value, of any storage class, against every other.
    if kind == ConnectionKind::Sqlite {
        return true;
    }
    let mysql = matches!(kind, ConnectionKind::Mysql | ConnectionKind::Mariadb);
    let head = base_type_head(data_type);
    match family_of(kind, data_type) {
        Family::Integer { .. } | Family::Decimal { .. } | Family::Float { .. } | Family::Boolean | Family::Date | Family::Time | Family::Timestamp { .. } | Family::Uuid => true,
        // MySQL sorts the first `max_sort_length` bytes of a value: a TEXT or a blob, and a varchar that can hold more (up to 4
        // bytes a character) sort by a prefix (a query's column says `varchar` with no length, and is taken to sort whole).
        Family::Text { length, .. } => !mysql || (!matches!(head.as_str(), "text" | "tinytext" | "mediumtext" | "longtext") && length.is_none_or(|length| length <= MAX_SORT_LENGTH / 4)),
        Family::Binary => !mysql || matches!(head.as_str(), "binary" | "varbinary"),
        // An enum sorts by its position and a set by its bits: both are total orders.
        Family::Other => mysql && matches!(head.as_str(), "enum" | "set"),
        Family::Json => false,
    }
}

/// `quoted` (a column, or its position) as a term of an `ORDER BY` that ties only identical values: text by its bytes where
/// the column's collation could call two different strings equal.
fn term(kind: ConnectionKind, quoted: &str, data_type: &str) -> String {
    let text = matches!(family_of(kind, data_type), Family::Text { .. });
    match kind {
        ConnectionKind::Mysql | ConnectionKind::Mariadb if text => format!("CAST({quoted} AS BINARY)"),
        // `citext` and a non-deterministic ICU collation call different strings equal: the `text` of the value, by byte order.
        ConnectionKind::Postgres if text => format!("({quoted})::text COLLATE \"C\""),
        ConnectionKind::Sqlite => format!("{quoted} COLLATE BINARY"),
        _ => quoted.to_owned(),
    }
}

/// The `ORDER BY` list of a table read by offset, and how good that order is. `key` names the key columns in key order;
/// `unique_key`: no two rows share a key. `quote` writes an identifier (`None`: it cannot be written safely).
pub(in crate::app) fn table_order(
    kind: ConnectionKind,
    columns: &[OrderColumn],
    key: &[String],
    unique_key: bool,
    quote: impl Fn(&str) -> Option<String>,
) -> Option<(String, RowOrder)> {
    let find = |name: &str| columns.iter().find(|column| column.name == name);
    let key_columns: Vec<&OrderColumn> = key.iter().filter_map(|name| find(name)).collect();
    if !key_columns.is_empty() && unique_key {
        let list = key_columns.iter().map(|column| quote(&column.name)).collect::<Option<Vec<_>>>()?;
        return Some((list.join(", "), RowOrder::UniqueKey));
    }
    // The key first (ClickHouse reads its table in that order), then the rest in table order.
    let ordered: Vec<&OrderColumn> = key_columns.iter().copied().chain(columns.iter().filter(|column| !key.contains(&column.name))).collect();
    let (usable, left_out): (Vec<&OrderColumn>, Vec<&OrderColumn>) = ordered.into_iter().partition(|column| orderable(kind, &column.data_type));
    if usable.is_empty() {
        return None;
    }
    let list = usable.iter().map(|column| quote(&column.name).map(|quoted| term(kind, &quoted, &column.data_type))).collect::<Option<Vec<_>>>()?;
    let order = if left_out.is_empty() { RowOrder::Total } else { RowOrder::Partial(left_out.iter().map(|column| column.name.clone()).collect()) };
    Some((list.join(", "), order))
}

/// The statement without the `;`s and comments after its last token (a line comment there would swallow what a wrapper adds
/// after it). The text as it is when it cannot be cut safely (an unterminated string, a versioned comment).
pub(in crate::app) fn strip_tail<'a>(sql: &'a str, options: &SqlLexOptions) -> &'a str {
    let found = tokens(sql, options);
    if found.unterminated || found.executable_comment {
        return sql.trim();
    }
    match found.tokens.iter().rev().find(|token| token.kind != SqlTokenKind::Semicolon) {
        Some(last) => sql[..last.range.end].trim_start(),
        None => "",
    }
}

/// The top-level part of a statement: its tokens outside every parenthesis, and the parentheses that open at that level.
fn top_level(found: &[SqlToken]) -> impl Iterator<Item = (usize, &SqlToken)> {
    let mut depth = 0i32;
    found.iter().enumerate().filter(move |(_, token)| match token.kind {
        // The `(` that opens a call or a group stays (a term `max(qty)` is not the name `max`); what is inside does not.
        SqlTokenKind::Open => {
            depth += 1;
            depth == 1
        }
        SqlTokenKind::Close => {
            depth -= 1;
            false
        }
        _ => depth == 0,
    })
}

fn word_of(sql: &str, token: &SqlToken) -> Option<String> {
    (token.kind == SqlTokenKind::Word).then(|| sql[token.range.clone()].to_ascii_uppercase())
}

/// Whether the top-level word at `at` starts a clause that limits the rows or skips some (`LIMIT`, `OFFSET n`, `FETCH FIRST|NEXT`,
/// SQL Server's `SELECT TOP n`), and is not a column that is named like one (`SELECT offset FROM t`, `t.offset`, `ORDER BY top`,
/// where servers that do not reserve the word let it name a column). `TOP` is followed by what it takes: a number, a parenthesis
/// or a parameter.
fn limiting_clause(sql: &str, top: &[(usize, &SqlToken)], at: usize) -> bool {
    let Some(word) = word_of(sql, top[at].1) else { return false };
    let text = |token: &SqlToken| &sql[token.range.clone()];
    let dotted = at.checked_sub(1).is_some_and(|before| top[before].1.kind == SqlTokenKind::Other && text(top[before].1) == ".");
    let after = top.get(at + 1).map(|(_, token)| *token);
    let takes_value = after.is_some_and(|token| match token.kind {
        SqlTokenKind::Number | SqlTokenKind::Open => true,
        SqlTokenKind::Other => matches!(text(token), "?" | ":" | "@" | "$"),
        SqlTokenKind::Quoted => text(token).starts_with('$'),
        _ => false,
    });
    if dotted {
        return false;
    }
    // `OFFSET` takes any expression (`OFFSET coalesce($1, 0)`): it is a column only where what follows ends or continues a name.
    // A wrong guess either way costs a warning or a page read twice, so an unknown follower is taken as the clause.
    let names_a_column = after.is_none_or(|token| match token.kind {
        SqlTokenKind::Semicolon | SqlTokenKind::Comma | SqlTokenKind::Close => true,
        SqlTokenKind::Other => !matches!(text(token), "?" | ":" | "@" | "$" | "-" | "+"),
        SqlTokenKind::Word => word_of(sql, token).is_some_and(|next| {
            matches!(
                next.as_str(),
                "FROM" | "AS" | "ASC" | "DESC" | "NULLS" | "AND" | "OR" | "IS" | "IN" | "NOT" | "LIKE" | "ILIKE" | "BETWEEN" | "WHERE" | "GROUP" | "ORDER" | "HAVING" | "LIMIT" | "UNION" | "EXCEPT" | "INTERSECT" | "COLLATE"
            )
        }),
        _ => false,
    });
    match word.as_str() {
        "LIMIT" => true,
        "OFFSET" => !names_a_column,
        "FETCH" => after.and_then(|token| word_of(sql, token)).is_some_and(|next| matches!(next.as_str(), "FIRST" | "NEXT")),
        "TOP" => takes_value && at.checked_sub(1).and_then(|before| word_of(sql, top[before].1)).is_some_and(|before| matches!(before.as_str(), "SELECT" | "DISTINCT" | "ALL")),
        _ => false,
    }
}

/// The terms of the statement's own `ORDER BY` (the tokens of each, split at the commas outside parentheses), when it has one.
fn own_order_terms<'a>(sql: &str, found: &'a [SqlToken]) -> Option<Vec<Vec<&'a SqlToken>>> {
    let top: Vec<(usize, &SqlToken)> = top_level(found).collect();
    let by = top.windows(2).position(|pair| word_of(sql, pair[0].1).is_some_and(|word| word == "ORDER") && word_of(sql, pair[1].1).is_some_and(|word| word == "BY"))? + 1;
    let mut terms: Vec<Vec<&SqlToken>> = vec![Vec::new()];
    for at in by + 1..top.len() {
        let token = top[at].1;
        match token.kind {
            SqlTokenKind::Semicolon => break,
            SqlTokenKind::Comma => terms.push(Vec::new()),
            SqlTokenKind::Word if limiting_clause(sql, &top, at) => break,
            SqlTokenKind::Word if word_of(sql, token).is_some_and(|word| matches!(word.as_str(), "FOR" | "SETTINGS" | "FORMAT" | "INTO" | "WINDOW" | "UNION" | "EXCEPT" | "INTERSECT")) => break,
            _ => terms.last_mut()?.push(token),
        }
    }
    Some(terms)
}

/// Whether the statement limits its rows or skips some (outside every parenthesis).
fn has_limit(sql: &str, found: &[SqlToken]) -> bool {
    let top: Vec<(usize, &SqlToken)> = top_level(found).collect();
    (0..top.len()).any(|at| limiting_clause(sql, &top, at))
}

/// Whether the statement is a `SELECT DISTINCT ON (…)` (PostgreSQL): of each group it keeps the first row of its order, so
/// without an order that decides it the row kept can differ from one page's execution to the next.
fn has_distinct_on(sql: &str, found: &[SqlToken]) -> bool {
    let top: Vec<&SqlToken> = top_level(found).map(|(_, token)| token).collect();
    top.windows(2).any(|pair| word_of(sql, pair[0]).is_some_and(|word| word == "DISTINCT") && word_of(sql, pair[1]).is_some_and(|word| word == "ON"))
}

/// The name a quoted or bare identifier token spells.
fn identifier_of(sql: &str, token: &SqlToken) -> Option<String> {
    let text = &sql[token.range.clone()];
    match token.kind {
        SqlTokenKind::Word => Some(text.to_owned()),
        SqlTokenKind::Quoted => {
            let (open, close) = match text.as_bytes().first()? {
                b'"' => ('"', '"'),
                b'`' => ('`', '`'),
                b'[' => ('[', ']'),
                _ => return None,
            };
            let inner = text.strip_prefix(open)?.strip_suffix(close)?;
            Some(if open == close { inner.replace(&format!("{open}{open}"), &open.to_string()) } else { inner.replace("]]", "]") })
        }
        _ => None,
    }
}

/// An `ORDER BY` term as the column of the result it orders by and the words after it (`DESC`, `NULLS LAST`): a position, or a
/// name of the result's columns. `None` when it orders by anything else (an expression, a column the query does not output).
fn map_term(sql: &str, term: &[&SqlToken], columns: &[OrderColumn]) -> Option<(usize, String)> {
    let (first, rest) = term.split_first()?;
    let index = if first.kind == SqlTokenKind::Number {
        sql[first.range.clone()].parse::<usize>().ok().filter(|position| (1..=columns.len()).contains(position))? - 1
    } else {
        let name = identifier_of(sql, first)?;
        columns.iter().position(|column| column.name == name).or_else(|| columns.iter().position(|column| column.name.eq_ignore_ascii_case(&name)))?
    };
    let mut suffix = Vec::new();
    for token in rest {
        let word = word_of(sql, token)?;
        if !matches!(word.as_str(), "ASC" | "DESC" | "NULLS" | "FIRST" | "LAST") {
            return None;
        }
        suffix.push(word);
    }
    Some((index, suffix.join(" ")))
}

/// Whether `name` is a word or a quoted name of the statement (case does not matter): a query that reads a table has its name.
pub(in crate::app) fn query_mentions(sql: &str, name: &str) -> bool {
    // Read the ways every dialect reads it: either way the name is in it.
    [SqlLexOptions::postgres(), SqlLexOptions::mysql(), SqlLexOptions::sqlite(), SqlLexOptions::clickhouse()].iter().any(|options| {
        tokens(sql, options).tokens.iter().any(|token| {
            let text = &sql[token.range.clone()];
            match token.kind {
                SqlTokenKind::Word => text.eq_ignore_ascii_case(name),
                SqlTokenKind::Quoted => identifier_of(sql, token).is_some_and(|spelled| spelled.eq_ignore_ascii_case(name)),
                _ => false,
            }
        })
    })
}

/// The first word of the statement, upper case.
fn first_word(sql: &str, options: &SqlLexOptions) -> String {
    let found = tokens(sql, options);
    found.tokens.iter().find(|token| token.kind == SqlTokenKind::Word).map(|token| sql[token.range.clone()].to_ascii_uppercase()).unwrap_or_default()
}

/// The statement a query is read by, paged by offset, and its order. `options` is the lexer of the source connection (its dialect
/// as the session reports it, see [`dialect_lex_options`]); `columns` are the query's output columns (from its first page);
/// `quote` writes an identifier (`None`: it cannot be written safely).
pub(in crate::app) fn query_statement(kind: ConnectionKind, options: &SqlLexOptions, sql: &str, columns: &[OrderColumn], quote: impl Fn(&str) -> Option<String>) -> (String, RowOrder) {
    let body = strip_tail(sql, options);
    let found = tokens(body, options);
    if found.unterminated || found.executable_comment {
        return (body.to_owned(), RowOrder::Unordered("the statement has a versioned comment or an unfinished string"));
    }
    if !matches!(first_word(body, options).as_str(), "SELECT" | "WITH" | "VALUES") {
        return (body.to_owned(), RowOrder::Unordered("the statement cannot be ordered from outside"));
    }
    // A derived table with two columns of one name is refused by MySQL, and ambiguous elsewhere.
    let mut names: Vec<String> = columns.iter().map(|column| column.name.to_lowercase()).collect();
    names.sort_unstable();
    if names.windows(2).any(|pair| pair[0] == pair[1]) {
        return (body.to_owned(), RowOrder::Unordered("two columns have the same name"));
    }
    let Some(quoted) = columns.iter().map(|column| quote(&column.name)).collect::<Option<Vec<String>>>() else {
        return (body.to_owned(), RowOrder::Unordered("a column name cannot be quoted"));
    };
    // SQLite reads a name it does not know as a string: the wrapper orders by the position of each column.
    let quoted: Vec<String> = if kind == ConnectionKind::Sqlite { (1..=columns.len()).map(|position| position.to_string()).collect() } else { quoted };
    let own = own_order_terms(body, &found.tokens);
    let limited = has_limit(body, &found.tokens);
    if own.is_none() && limited {
        return (body.to_owned(), RowOrder::Unordered("it has a LIMIT and no ORDER BY, so which rows it returns is not defined"));
    }
    // `DISTINCT ON` keeps the first row of each group by the statement's order: with none, the row kept is any of the group.
    let distinct_on = has_distinct_on(body, &found.tokens);
    if own.is_none() && distinct_on {
        return (body.to_owned(), RowOrder::Unordered("DISTINCT ON has no ORDER BY, so the row it keeps of each group is not defined"));
    }
    let limited = limited || distinct_on;
    // The user's order leads, when every term of it names a column of the result.
    let leading: Vec<String> = match &own {
        None => Vec::new(),
        Some(terms) => {
            let mapped: Option<Vec<(usize, String)>> = terms.iter().map(|term| map_term(body, term, columns)).collect();
            match mapped {
                Some(mapped) if !mapped.is_empty() => mapped.into_iter().map(|(index, suffix)| if suffix.is_empty() { quoted[index].clone() } else { format!("{} {suffix}", quoted[index]) }).collect(),
                _ => return (body.to_owned(), RowOrder::OwnUnmapped),
            }
        }
    };
    let (usable, left_out): (Vec<_>, Vec<_>) = columns.iter().enumerate().partition(|(_, column)| orderable(kind, &column.data_type));
    if usable.is_empty() && leading.is_empty() {
        return (body.to_owned(), RowOrder::Unordered("no column can be ordered"));
    }
    let tie_breakers = usable.iter().map(|(index, column)| term(kind, &quoted[*index], &column.data_type));
    let list = leading.into_iter().chain(tie_breakers).collect::<Vec<_>>().join(", ");
    let order = match (left_out.is_empty(), own.is_some() && limited) {
        (false, true) => RowOrder::PartialLimited(left_out.iter().map(|(_, column)| column.name.clone()).collect()),
        (false, false) => RowOrder::Partial(left_out.iter().map(|(_, column)| column.name.clone()).collect()),
        (true, true) => RowOrder::OwnLimited,
        (true, false) => RowOrder::Total,
    };
    (format!("SELECT * FROM (\n{body}\n) AS transfer_page ORDER BY {list}"), order)
}

#[cfg(test)]
mod tests {
    use super::*;
    use ConnectionKind::{Clickhouse, Mysql, Postgres, Sqlite};

    /// The statement of a query lexed by the preset of its kind (the tests that need no session setting).
    fn query_statement(kind: ConnectionKind, sql: &str, columns: &[OrderColumn], quote: impl Fn(&str) -> Option<String>) -> (String, RowOrder) {
        super::query_statement(kind, &lex_options(kind), sql, columns, quote)
    }

    fn strip_tail(sql: &str, kind: ConnectionKind) -> &str {
        super::strip_tail(sql, &lex_options(kind))
    }

    fn columns(list: &[(&str, &str)]) -> Vec<OrderColumn> {
        list.iter().map(|(name, data_type)| OrderColumn { name: (*name).to_owned(), data_type: (*data_type).to_owned() }).collect()
    }

    fn quote(name: &str) -> Option<String> {
        Some(format!("\"{}\"", name.replace('"', "\"\"")))
    }

    #[test]
    fn a_table_without_a_key_is_ordered_by_every_column_the_server_can_order() {
        let table = columns(&[("id", "integer"), ("name", "text"), ("doc", "jsonb"), ("seen", "timestamp with time zone"), ("tags", "text[]")]);
        let (list, order) = table_order(Postgres, &table, &[], false, quote).unwrap();
        assert_eq!(list, "\"id\", (\"name\")::text COLLATE \"C\", \"seen\"");
        assert_eq!(order, RowOrder::Partial(vec!["doc".into(), "tags".into()]));
        let (list, order) = table_order(Postgres, &columns(&[("a", "integer"), ("b", "text")]), &[], false, quote).unwrap();
        assert_eq!((list.as_str(), order), ("\"a\", (\"b\")::text COLLATE \"C\"", RowOrder::Total));
        assert!(table_order(Postgres, &columns(&[("doc", "jsonb")]), &[], false, quote).is_none(), "nothing to order by");
    }

    #[test]
    fn a_unique_key_is_the_order_and_a_sorting_key_is_only_the_first_of_it() {
        let table = columns(&[("a", "integer"), ("b", "text"), ("c", "double precision")]);
        let key = vec!["b".to_owned(), "a".to_owned()];
        assert_eq!(table_order(Postgres, &table, &key, true, quote), Some(("\"b\", \"a\"".to_owned(), RowOrder::UniqueKey)));
        // ClickHouse: the sorting key does not make rows unique, so the other columns follow it.
        let (list, order) = table_order(Clickhouse, &columns(&[("a", "Int64"), ("b", "String"), ("c", "Float64"), ("d", "Array(Int32)")]), &["b".to_owned()], false, quote).unwrap();
        assert_eq!(list, "\"b\", \"a\", \"c\"");
        assert_eq!(order, RowOrder::Partial(vec!["d".into()]));
    }

    #[test]
    fn mysql_does_not_order_by_text_it_sorts_a_prefix_of_and_sqlite_orders_everything() {
        let table = columns(&[("id", "int"), ("body", "longtext"), ("name", "varchar(20)"), ("raw", "blob")]);
        let (list, order) = table_order(Mysql, &table, &[], false, quote).unwrap();
        assert_eq!(list, "\"id\", CAST(\"name\" AS BINARY)");
        assert_eq!(order, RowOrder::Partial(vec!["body".into(), "raw".into()]));
        let (list, order) = table_order(Sqlite, &columns(&[("a", ""), ("b", "BLOB"), ("c", "JSON")]), &[], false, quote).unwrap();
        assert_eq!((list.as_str(), order), ("\"a\" COLLATE BINARY, \"b\" COLLATE BINARY, \"c\" COLLATE BINARY", RowOrder::Total));
    }

    #[test]
    fn a_name_that_cannot_be_quoted_is_not_ordered_by() {
        assert!(table_order(Postgres, &columns(&[("a", "int")]), &[], false, |_| None).is_none());
    }

    fn out(list: &[(&str, &str)]) -> Vec<OrderColumn> {
        columns(list)
    }

    fn backticks(name: &str) -> Option<String> {
        Some(format!("`{}`", name.replace('`', "``")))
    }

    #[test]
    fn a_query_is_wrapped_and_ordered_by_the_names_of_its_orderable_output_columns() {
        let (sql, order) = query_statement(Postgres, "SELECT id, doc, name FROM t WHERE x = 1", &out(&[("id", "int8"), ("doc", "jsonb"), ("name", "text")]), quote);
        assert_eq!(sql, "SELECT * FROM (\nSELECT id, doc, name FROM t WHERE x = 1\n) AS transfer_page ORDER BY \"id\", (\"name\")::text COLLATE \"C\"");
        assert_eq!(order, RowOrder::Partial(vec!["doc".into()]));
        let (sql, order) = query_statement(Sqlite, "WITH c AS (SELECT 1 x) SELECT x FROM c", &out(&[("x", "int64")]), quote);
        assert_eq!((sql.as_str(), order), ("SELECT * FROM (\nWITH c AS (SELECT 1 x) SELECT x FROM c\n) AS transfer_page ORDER BY 1 COLLATE BINARY", RowOrder::Total));
        // Positions would be read as plain numbers where a setting says otherwise (ClickHouse `enable_positional_arguments`): never used.
        let (sql, _) = query_statement(Clickhouse, "SELECT a FROM t", &out(&[("a", "UInt8")]), backticks);
        assert!(sql.ends_with("ORDER BY `a`"), "{sql}");
    }

    #[test]
    fn trailing_semicolons_and_comments_never_swallow_the_closing_parenthesis() {
        for (sql, inner) in [("SELECT a FROM t -- note", "SELECT a FROM t"), ("SELECT 1; -- x", "SELECT 1"), ("SELECT 1 /* x */ ;;\n", "SELECT 1"), ("# top\nSELECT a FROM t # note", "# top\nSELECT a FROM t")] {
            let (wrapped, order) = query_statement(Mysql, sql, &out(&[("a", "int")]), backticks);
            assert_eq!(wrapped, format!("SELECT * FROM (\n{inner}\n) AS transfer_page ORDER BY `a`"), "{sql}");
            assert_eq!(order, RowOrder::Total);
        }
        assert_eq!(strip_tail("SELECT 1; -- x", Postgres), "SELECT 1");
        assert_eq!(strip_tail("-- only a comment\n;", Postgres), "");
        // A backtick name holding a parenthesis and a semicolon is one name, in the dialects that have backticks.
        assert_eq!(strip_tail("SELECT `a(;` FROM t;", Mysql), "SELECT `a(;` FROM t");
        assert_eq!(strip_tail("SELECT `a(;` FROM t;", Clickhouse), "SELECT `a(;` FROM t");
        assert_eq!(strip_tail("SELECT `a(;` FROM t;", Sqlite), "SELECT `a(;` FROM t");
    }

    #[test]
    fn the_order_of_a_query_leads_and_its_columns_break_the_ties() {
        let columns = out(&[("a", "int4"), ("b", "text")]);
        let (sql, order) = query_statement(Postgres, "SELECT * FROM t ORDER BY b DESC NULLS LAST, 1", &columns, quote);
        assert_eq!(sql, "SELECT * FROM (\nSELECT * FROM t ORDER BY b DESC NULLS LAST, 1\n) AS transfer_page ORDER BY \"b\" DESC NULLS LAST, \"a\", \"a\", (\"b\")::text COLLATE \"C\"");
        assert_eq!(order, RowOrder::Total);
        // Comments inside the clause, a quoted name, a keyword cased any way.
        for sql in ["select a from t order /* c */ by \"a\"", "select a from t ORDER\n BY A desc", "SELECT a FROM t UNION SELECT b FROM u ORDER BY 1"] {
            let (wrapped, order) = query_statement(Postgres, sql, &out(&[("a", "int4")]), quote);
            assert!(wrapped.starts_with("SELECT * FROM (\n") && wrapped.contains("AS transfer_page ORDER BY \"a\""), "{wrapped}");
            assert_eq!(order, RowOrder::Total, "{sql}");
        }
        // An order that is not by a column of the result cannot be extended: read as written, and said to be unsure.
        for sql in ["SELECT a FROM t ORDER BY b", "SELECT a FROM t ORDER BY a + 1", "SELECT a FROM t ORDER BY t.a", "SELECT a FROM t ORDER BY a COLLATE \"C\"", "SELECT a FROM t ORDER BY 2"] {
            assert_eq!(query_statement(Postgres, sql, &out(&[("a", "int4")]), quote), (sql.to_owned(), RowOrder::OwnUnmapped), "{sql}");
        }
        // An ORDER BY inside a subquery, a string or a comment is not the query's own.
        let sql = "SELECT a FROM (SELECT a FROM t ORDER BY a LIMIT 5) s WHERE note = 'ORDER BY x' -- order by y";
        let (wrapped, order) = query_statement(Postgres, sql, &out(&[("a", "int4")]), quote);
        assert!(wrapped.starts_with("SELECT * FROM (") && wrapped.ends_with("ORDER BY \"a\"") && order == RowOrder::Total, "{wrapped}");
    }

    #[test]
    fn a_limit_without_an_order_and_what_cannot_be_wrapped_say_so() {
        let one = out(&[("a", "int4")]);
        assert!(matches!(query_statement(Postgres, "SELECT a FROM t LIMIT 10", &one, quote), (_, RowOrder::Unordered(reason)) if reason.contains("LIMIT")));
        assert!(matches!(query_statement(Postgres, "SELECT a FROM t FETCH FIRST 3 ROWS ONLY", &one, quote).1, RowOrder::Unordered(_)));
        // A LIMIT in a subquery is not the query's.
        assert_eq!(query_statement(Postgres, "SELECT a FROM (SELECT a FROM t LIMIT 3) s", &one, quote).1, RowOrder::Total);
        assert!(matches!(query_statement(Postgres, "SHOW TABLES", &out(&[("a", "text")]), quote).1, RowOrder::Unordered(_)));
        assert!(matches!(query_statement(Mysql, "SELECT a, a FROM t", &out(&[("a", "int"), ("A", "int")]), backticks).1, RowOrder::Unordered(reason) if reason.contains("same name")));
        assert!(matches!(query_statement(Postgres, "SELECT doc FROM t", &out(&[("doc", "jsonb")]), quote).1, RowOrder::Unordered(_)));
        assert!(matches!(query_statement(Mysql, "SELECT 1 /*!50000 , 2 */", &one, backticks).1, RowOrder::Unordered(_)));
    }

    #[test]
    fn text_is_ordered_by_its_bytes_where_a_collation_could_call_different_strings_equal() {
        let table = columns(&[("id", "int"), ("name", "varchar(20)"), ("note", "varchar")]);
        let (list, order) = table_order(Mysql, &table, &[], false, backticks).unwrap();
        assert_eq!((list.as_str(), order), ("`id`, CAST(`name` AS BINARY), CAST(`note` AS BINARY)", RowOrder::Total));
        let (list, _) = table_order(Sqlite, &columns(&[("a", "TEXT NOCASE"), ("b", "INTEGER")]), &[], false, quote).unwrap();
        assert_eq!(list, "\"a\" COLLATE BINARY, \"b\" COLLATE BINARY");
        let (sql, _) = query_statement(Mysql, "SELECT code, name FROM t", &out(&[("code", "varchar"), ("name", "char(3)")]), backticks);
        assert!(sql.ends_with("ORDER BY CAST(`code` AS BINARY), CAST(`name` AS BINARY)"), "{sql}");
    }

    #[test]
    fn mysql_text_types_that_sort_a_prefix_are_the_only_ones_left_out() {
        // A result column says `varchar` and `char` with no length: they sort whole.
        for data_type in ["varchar", "char", "varchar(20)", "char(3)", "binary(4)", "varbinary(10)", "enum('a','b')", "set('x','y')", "int", "datetime(6)"] {
            assert!(orderable(Mysql, data_type), "{data_type}");
        }
        for data_type in ["text", "tinytext", "mediumtext", "longtext", "blob", "longblob", "json"] {
            assert!(!orderable(Mysql, data_type), "{data_type}");
        }
        // `max_sort_length` is 1024 bytes and a character is up to 4: a varchar that can hold more sorts by a prefix.
        assert!(orderable(Mysql, "varchar(256)") && orderable(ConnectionKind::Mariadb, "char(255)"));
        assert!(!orderable(Mysql, "varchar(257)") && !orderable(ConnectionKind::Mariadb, "varchar(1000)"));
        assert!(orderable(Postgres, "varchar(1000)"), "only MySQL and MariaDB sort by a prefix");
        let (_, order) = table_order(Mysql, &columns(&[("id", "int"), ("long", "varchar(500)")]), &[], false, quote).unwrap();
        assert_eq!(order, RowOrder::Partial(vec!["long".into()]));
        let (_, order) = query_statement(Mysql, "SELECT code, name FROM t", &out(&[("code", "varchar"), ("name", "varchar")]), backticks);
        assert_eq!(order, RowOrder::Total, "no warning for a query whose columns are plain varchars");
    }

    #[test]
    fn postgresql_text_is_ordered_by_its_bytes_so_citext_and_non_deterministic_collations_cannot_tie() {
        let table = columns(&[("id", "integer"), ("name", "citext"), ("note", "character varying(20)"), ("code", "character(3)")]);
        let (list, order) = table_order(Postgres, &table, &[], false, quote).unwrap();
        assert_eq!((list.as_str(), order), ("\"id\", (\"name\")::text COLLATE \"C\", (\"note\")::text COLLATE \"C\", (\"code\")::text COLLATE \"C\"", RowOrder::Total));
        // The tie-breakers of a query, after its own order; a unique key is read as it is.
        let (sql, _) = query_statement(Postgres, "SELECT n, id FROM t ORDER BY n", &out(&[("n", "citext"), ("id", "int8")]), quote);
        assert!(sql.ends_with("ORDER BY \"n\", (\"n\")::text COLLATE \"C\", \"id\""), "{sql}");
        let (list, order) = table_order(Postgres, &columns(&[("k", "text"), ("v", "text")]), &["k".to_owned()], true, quote).unwrap();
        assert_eq!((list.as_str(), order), ("\"k\"", RowOrder::UniqueKey));
    }

    #[test]
    fn an_order_of_its_own_with_a_limit_or_an_offset_picks_rows_by_an_order_that_may_tie() {
        let one = out(&[("a", "int4"), ("b", "text")]);
        for sql in ["SELECT a, b FROM t ORDER BY a LIMIT 10", "SELECT a, b FROM t ORDER BY a OFFSET 5", "SELECT a, b FROM t ORDER BY a FETCH FIRST 3 ROWS ONLY", "SELECT a, b FROM t ORDER BY a LIMIT 3 OFFSET 2"] {
            let (wrapped, order) = query_statement(Postgres, sql, &one, quote);
            assert!(wrapped.starts_with("SELECT * FROM (\n") && wrapped.contains("AS transfer_page ORDER BY \"a\", \"a\", (\"b\")::text COLLATE \"C\""), "{wrapped}");
            assert_eq!(order, RowOrder::OwnLimited, "{sql}");
        }
        let text = RowOrder::OwnLimited.warning("Query").unwrap();
        assert!(text.contains("the rows its LIMIT or DISTINCT ON picks are chosen by an order that may tie; rows at the boundary may differ between pages"), "{text}");
        // A subquery's LIMIT is not the query's, and an offset with no order of its own is as unsure as a limit with none.
        assert_eq!(query_statement(Postgres, "SELECT a FROM (SELECT a FROM t ORDER BY a LIMIT 3) s ORDER BY a", &out(&[("a", "int4")]), quote).1, RowOrder::Total);
        assert!(matches!(query_statement(Postgres, "SELECT a FROM t OFFSET 5", &out(&[("a", "int4")]), quote).1, RowOrder::Unordered(_)));
        // What cannot be ordered at all is the stronger warning.
        // Columns the server cannot order *and* a limit: both are said.
        let (_, order) = query_statement(Postgres, "SELECT a, doc FROM t ORDER BY a LIMIT 3", &out(&[("a", "int4"), ("doc", "jsonb")]), quote);
        assert_eq!(order, RowOrder::PartialLimited(vec!["doc".into()]));
        let text = order.warning("Query").unwrap();
        assert!(text.contains("\"doc\"") && text.contains("may be read twice or missed") && text.contains("its LIMIT or DISTINCT ON picks are chosen by an order that may tie"), "{text}");
        // Without a limit of its own, the limit is not mentioned.
        let (_, order) = query_statement(Postgres, "SELECT a, doc FROM t ORDER BY a", &out(&[("a", "int4"), ("doc", "jsonb")]), quote);
        assert_eq!(order, RowOrder::Partial(vec!["doc".into()]));
        assert!(!order.warning("Query").unwrap().contains("LIMIT"));
    }

    #[test]
    fn a_limit_is_found_by_the_clause_it_is_and_not_by_a_column_named_like_one() {
        let one = out(&[("a", "int4")]);
        // SQL Server's TOP, and OFFSET … FETCH, are limits.
        for sql in ["SELECT TOP 10 a FROM t", "SELECT TOP (5) a FROM t", "SELECT DISTINCT top 3 a FROM t", "SELECT a FROM t OFFSET 5 ROWS", "SELECT a FROM t OFFSET $1", "SELECT a FROM t OFFSET ? ROWS FETCH NEXT ? ROWS ONLY", "SELECT a FROM t FETCH FIRST 3 ROWS ONLY", "SELECT a FROM t OFFSET coalesce($1, 0)", "SELECT a FROM t OFFSET CAST(? AS int)", "SELECT a FROM t OFFSET '5'"] {
            assert!(matches!(query_statement(Postgres, sql, &one, quote).1, RowOrder::Unordered(reason) if reason.contains("LIMIT")), "{sql}");
        }
        for sql in ["SELECT a FROM t ORDER BY a OFFSET 5 ROWS FETCH NEXT 3 ROWS ONLY", "SELECT TOP 3 a FROM t ORDER BY a", "SELECT a FROM t ORDER BY a FETCH FIRST ROW ONLY"] {
            let (wrapped, order) = query_statement(Postgres, sql, &one, quote);
            assert_eq!(order, RowOrder::OwnLimited, "{sql}");
            assert!(wrapped.starts_with("SELECT * FROM (\n") && wrapped.ends_with("ORDER BY \"a\", \"a\""), "{wrapped}");
        }
        // A column that is named offset, top, fetch or limit (quoted where the server reserves the word) is not a clause.
        let named = out(&[("offset", "int4"), ("top", "int4"), ("fetch", "int4"), ("limit", "int4")]);
        for sql in ["SELECT offset, top, fetch, \"limit\" FROM t", "SELECT t.offset, t.top, t.fetch, t.\"limit\" FROM t", "SELECT offset, top, fetch, \"limit\" FROM t WHERE offset > 3 AND top + 1 < fetch"] {
            let (wrapped, order) = query_statement(Mysql, sql, &named, backticks);
            assert_eq!(order, RowOrder::Total, "{sql}");
            assert!(wrapped.starts_with("SELECT * FROM (\n") && wrapped.ends_with("ORDER BY `offset`, `top`, `fetch`, `limit`"), "{wrapped}");
        }
        // … nor in the ORDER BY: the word is the column it orders by, and what follows still counts.
        let (wrapped, order) = query_statement(Mysql, "SELECT offset FROM t ORDER BY offset DESC, 1", &out(&[("offset", "int")]), backticks);
        assert_eq!(order, RowOrder::Total);
        assert!(wrapped.ends_with("ORDER BY `offset` DESC, `offset`, `offset`"), "{wrapped}");
        let (_, order) = query_statement(Mysql, "SELECT offset FROM t ORDER BY offset LIMIT 5 OFFSET 2", &out(&[("offset", "int")]), backticks);
        assert_eq!(order, RowOrder::OwnLimited);
        // A limit in a string, a comment or a subquery is not the query's.
        for sql in ["SELECT 'LIMIT 5' AS a FROM t -- OFFSET 1", "SELECT a FROM (SELECT TOP 3 a FROM t) s"] {
            assert_eq!(query_statement(Postgres, sql, &one, quote).1, RowOrder::Total, "{sql}");
        }
    }

    #[test]
    fn a_distinct_on_query_is_wrapped_as_a_subquery_and_said_to_pick_rows_by_its_order() {
        let columns = out(&[("g", "int4"), ("v", "text")]);
        // The wrapper is a derived table: `DISTINCT ON` runs inside it, with the query's own ORDER BY, and the pages follow the
        // order of every column of what it returned.
        let sql = "SELECT DISTINCT ON (g) g, v FROM t ORDER BY g, v DESC";
        let (wrapped, order) = query_statement(Postgres, sql, &columns, quote);
        assert_eq!(wrapped, format!("SELECT * FROM (\n{sql}\n) AS transfer_page ORDER BY \"g\", \"v\" DESC, \"g\", (\"v\")::text COLLATE \"C\""));
        assert_eq!(order, RowOrder::OwnLimited);
        assert!(order.warning("Query").is_some_and(|text| text.contains("DISTINCT ON")));
        // Without an order the row kept of each group is any of them: read as written, and said so.
        let (same, order) = query_statement(Postgres, "SELECT DISTINCT ON (g) g, v FROM t", &columns, quote);
        assert_eq!(same, "SELECT DISTINCT ON (g) g, v FROM t");
        assert!(matches!(order, RowOrder::Unordered(reason) if reason.contains("DISTINCT ON")), "{order:?}");
        // A plain DISTINCT is a set of whole rows: nothing to say, and `ON` of a join is not `DISTINCT ON`.
        assert_eq!(query_statement(Postgres, "SELECT DISTINCT g, v FROM t", &columns, quote).1, RowOrder::Total);
        assert_eq!(query_statement(Postgres, "SELECT DISTINCT g, v FROM t JOIN u ON (t.id = u.id)", &columns, quote).1, RowOrder::Total);
    }

    #[test]
    fn a_query_is_lexed_as_the_source_session_reads_it_not_as_the_preset_of_its_kind() {
        use crate::sql_table::SqlDialect;
        let one = out(&[("a", "int")]);
        // NO_BACKSLASH_ESCAPES: the backslash ends the string, so this is a complete statement with a `;` after it. As the preset
        // reads it (an escape) the string never ends and the statement cannot be cut.
        let sql = "SELECT 'a\\' AS a FROM t;";
        let preset = lex_options(Mysql);
        assert_eq!(super::strip_tail(sql, &preset), sql.trim(), "the preset sees an unfinished string");
        assert!(matches!(super::query_statement(Mysql, &preset, sql, &one, backticks).1, RowOrder::Unordered(_)));
        let session = dialect_lex_options(Mysql, SqlDialect { backslash_escapes: false, ..SqlDialect::MYSQL }, None);
        assert_eq!(super::strip_tail(sql, &session), "SELECT 'a\\' AS a FROM t");
        let (wrapped, order) = super::query_statement(Mysql, &session, sql, &one, backticks);
        assert_eq!(order, RowOrder::Total);
        assert!(wrapped.starts_with("SELECT * FROM (\nSELECT 'a\\' AS a FROM t\n) AS transfer_page"), "{wrapped}");
        // MariaDB `sql_mode=MSSQL`: `[a']` is a name, so the `'` opens nothing.
        let sql = "SELECT [a'] FROM t ORDER BY 1;";
        let mssql = dialect_lex_options(ConnectionKind::Mariadb, SqlDialect { bracket_identifiers: true, ..SqlDialect::MYSQL }, None);
        assert_eq!(super::strip_tail(sql, &mssql), "SELECT [a'] FROM t ORDER BY 1");
        assert!(matches!(super::query_statement(ConnectionKind::Mariadb, &lex_options(ConnectionKind::Mariadb), sql, &one, backticks).1, RowOrder::Unordered(_)));
        // ANSI_QUOTES: `"ORDER BY"` is a name, not the clause. The preset reads a string; the session an identifier.
        let sql = "SELECT 1 AS \"a\" FROM t ORDER BY \"a\"";
        let ansi = dialect_lex_options(Mysql, SqlDialect { double_quote_identifiers: true, ..SqlDialect::MYSQL }, None);
        assert!(ansi.double_quote_is_identifier && !lex_options(Mysql).double_quote_is_identifier);
        assert_eq!(super::query_statement(Mysql, &ansi, sql, &one, backticks).1, RowOrder::Total);
        // The preset of PostgreSQL and SQLite is what a driver that reports nothing gets.
        assert_eq!(dialect_lex_options(Postgres, SqlDialect::POSTGRES, Some(160_000)), SqlLexOptions::postgres());
    }

    #[test]
    fn a_term_with_a_call_in_it_is_not_the_column_named_like_the_function() {
        let columns = out(&[("max", "int8"), ("qty", "int4")]);
        for sql in ["SELECT max(q) AS max, qty FROM t GROUP BY qty ORDER BY max(q)", "SELECT max(q) AS max, qty FROM t GROUP BY qty ORDER BY max(q) DESC", "SELECT 1 max, 2 qty ORDER BY (max)", "SELECT 1 max, 2 qty ORDER BY qty, qty(1)"] {
            assert_eq!(query_statement(Postgres, sql, &columns, quote), (sql.to_owned(), RowOrder::OwnUnmapped), "{sql}");
        }
        let (_, order) = query_statement(Postgres, "SELECT max(q) AS max, qty FROM t GROUP BY qty ORDER BY max DESC, 2", &columns, quote);
        assert_eq!(order, RowOrder::Total);
    }

    #[test]
    fn sqlite_wraps_a_query_ordered_by_position_because_it_reads_an_unknown_name_as_a_string() {
        let (sql, order) = query_statement(Sqlite, "SELECT b, a FROM t ORDER BY a DESC", &out(&[("b", "TEXT"), ("a", "INTEGER")]), quote);
        assert_eq!(sql, "SELECT * FROM (\nSELECT b, a FROM t ORDER BY a DESC\n) AS transfer_page ORDER BY 2 DESC, 1 COLLATE BINARY, 2 COLLATE BINARY");
        assert_eq!(order, RowOrder::Total);
    }

    #[test]
    fn locking_clauses_are_found_by_their_words_not_by_their_letters() {
        let mysql = SqlLexOptions::mysql();
        let pg = SqlLexOptions::postgres();
        for sql in ["SELECT * FROM t FOR UPDATE", "select * from t for  update nowait", "SELECT * FROM t FOR SHARE", "SELECT * FROM t FOR NO KEY UPDATE", "SELECT * FROM t FOR KEY SHARE", "SELECT * FROM t LOCK IN SHARE MODE", "SELECT * FROM t FOR /* c */ UPDATE", "SELECT * FROM (SELECT * FROM t FOR UPDATE) s"] {
            assert!(has_locking_clause(sql, &mysql) && has_locking_clause(sql, &pg), "{sql}");
        }
        for sql in ["SELECT 'FOR UPDATE' FROM t", "SELECT * FROM t -- FOR UPDATE", "SELECT \"for\" update FROM t", "SELECT substring(a FROM 1 FOR 2) FROM t", "SELECT lock, in_share FROM t", "SELECT * FROM t ORDER BY a"] {
            assert!(!has_locking_clause(sql, &mysql), "{sql}");
        }
    }

    #[test]
    fn a_session_lexer_reads_what_the_server_reads_mariadb_brackets_and_the_version_of_a_versioned_comment() {
        use engine_driver_kit::sql::classify;
        let info = |bracket: bool| engine_driver_api::SqlDialectInfo { identifier_quote: "`".into(), string_literal_quote: "'".into(), supports_schemas: false, database_qualified_names: true, like_requires_text_cast: false, string_backslash_escapes: true, double_quote_identifiers: false, bracket_identifiers: bracket };
        // With `sql_mode=MSSQL` the `'` in `[a']` is inside a name: what follows is three statements, the last one a delete.
        let script = "SELECT [a'], 1; COMMIT; DELETE FROM t WHERE id>0; -- ']";
        let plain = session_lex_options(ConnectionKind::Mariadb, Some(&info(false)), Some(110_400));
        let mssql = session_lex_options(ConnectionKind::Mariadb, Some(&info(true)), Some(110_400));
        assert!(classify(script, &plain).single().is_some(), "without the mode the quote opens a string that hides the rest");
        assert!(classify(script, &mssql).single().is_none(), "with it the script is seen");
        assert!(super::super::reader::read_refusal(script, &mssql).is_some());
        // `/*!80000 … */` is code or a comment by the version of the server; unknown, the text is ambiguous and refused.
        let versioned = "SELECT 1 /*!80000 + 1 */";
        let unknown = session_lex_options(ConnectionKind::Mysql, Some(&info(false)), None);
        let known = session_lex_options(ConnectionKind::Mysql, Some(&info(false)), Some(80_036));
        assert!(super::super::reader::read_refusal(versioned, &unknown).is_some());
        assert_eq!(super::super::reader::read_refusal(versioned, &known), None);
        // PostgreSQL does not take a version, and no reply of the driver is the preset.
        assert_eq!(session_lex_options(Postgres, None, Some(160_000)), SqlLexOptions::postgres());
    }

    #[test]
    fn a_query_that_reads_a_table_has_its_name() {
        assert!(query_mentions("SELECT * FROM public.Orders o JOIN x ON 1=1", "orders"));
        assert!(query_mentions("SELECT * FROM \"my \"\"t\"\"\"", "my \"t\""));
        assert!(query_mentions("SELECT * FROM `a b`", "A B"));
        assert!(!query_mentions("SELECT 'orders' FROM other -- orders", "orders"));
        assert!(!query_mentions("SELECT * FROM orders_archive", "orders"));
    }

    #[test]
    fn what_may_go_wrong_is_said_only_when_it_may() {
        assert!(RowOrder::Partial(vec!["doc".into()]).warning("t").is_some_and(|text| text.contains("\"doc\"") && text.contains("twice")));
        assert!(RowOrder::Unordered("two columns have the same name").warning("q").is_some());
        assert!(RowOrder::OwnUnmapped.warning("q").is_some() && RowOrder::OwnLimited.warning("q").is_some());
        for order in [RowOrder::Key, RowOrder::UniqueKey, RowOrder::Total, RowOrder::Own] {
            assert!(order.warning("t").is_none(), "{order:?}");
        }
    }
}
