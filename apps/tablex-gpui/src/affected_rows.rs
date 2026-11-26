//! Which rows an `UPDATE` or `DELETE` of the SQL editor would change, found without running it:
//! the statement's table and `WHERE` clause are cut out of its text and rebuilt as the two reads
//! a confirmation shows (`SELECT count(*) ...` and `SELECT * ... LIMIT n`).
//!
//! Read as a stream of the driver kit's lexer tokens for the connection's dialect (comments,
//! quotes and `$$` bodies are not mistaken for words): a statement that is not an `UPDATE` or
//! `DELETE` is skipped to its `;` without keeping a token, so a multi-megabyte `INSERT` costs the
//! lexer's pass and no allocation, and no token list is ever built.
//!
//! Only a statement of a single table is previewed. Anything the reader is not sure of (a join,
//! `USING`, a CTE, `WHERE CURRENT OF`, a multi-table MySQL statement, a clause the dialect does
//! not have) is [`Shape::Unsupported`]: the confirmation says so and never blocks the run.
//!
//! The same goes for a `WHERE` / `ORDER BY` that calls a function with effects a rollback does not
//! undo ([`has_side_effects`]): it is [`Shape::SideEffects`], never copied into a read.
//!
//! A new dialect (SQL Server, which caps rows with `TOP n` instead of `LIMIT n`) adds an arm to
//! [`Syntax::of`], which the compiler demands for every [`ExplainDialect`].

use engine_driver_kit::sql::{token_iter, SqlLexOptions, SqlToken, SqlTokenKind, TokenIter};
use engine_plan::ExplainDialect;
use std::iter::Peekable;
use std::ops::Range;

/// Statements of a script that are previewed (the rest are counted, not read).
pub const MAX_STATEMENTS: usize = 5;

/// Characters of a statement its heading keeps.
const HEADING_CHARS: usize = 70;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Kind {
    Update,
    Delete,
}

/// A statement the rows of which can be read beforehand.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Preview {
    pub kind: Kind,
    /// The table as the statement spells it (`"app"."items"`).
    pub table: String,
    /// No `WHERE` (and no `LIMIT`): the statement changes every row.
    pub whole_table: bool,
    /// SQLite `UPDATE OR REPLACE`: rows that conflict on a unique key are deleted as well, and the
    /// count does not include them.
    pub replaces: bool,
    /// `SELECT count(*) ...`
    pub count_sql: String,
    /// `SELECT * ... LIMIT n`
    pub rows_sql: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Shape {
    Preview(Preview),
    /// An `UPDATE` / `DELETE` whose rows cannot be read beforehand.
    Unsupported,
    /// Its `WHERE` / `ORDER BY` calls a function whose effect outlives a read-only read (an advisory
    /// lock, a sequence, a `dblink` call, a cancel of a backend): copying the text into a preview
    /// would run it before the user said yes, and a rollback does not undo it.
    SideEffects,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Found {
    /// The start of the statement on one line (a section's heading), not the whole text.
    pub heading: String,
    pub shape: Shape,
}

#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Scan {
    /// The first [`MAX_STATEMENTS`] data-changing statements, in order.
    pub found: Vec<Found>,
    /// Data-changing statements after them.
    pub more: usize,
}

/// What of a dialect's `UPDATE` / `DELETE` syntax the reader accepts.
struct Syntax {
    /// Words between `UPDATE` and the table.
    update_modifiers: &'static [&'static str],
    /// Words between `DELETE` and `FROM` (a MySQL `DELETE t1 FROM ...` is multi-table).
    delete_modifiers: &'static [&'static str],
    /// `UPDATE ONLY t` (PostgreSQL).
    only: bool,
    /// `UPDATE OR ROLLBACK t` (SQLite).
    update_or: bool,
    /// `ORDER BY` and `LIMIT` after the `WHERE` of a single-table statement (MySQL, MariaDB).
    order_limit: bool,
    /// `ALTER TABLE t UPDATE ... WHERE` / `DELETE WHERE` (ClickHouse).
    alter_mutation: bool,
    /// `/*! ... */` runs as code: the text between its delimiters cannot be moved.
    executable_comments: bool,
    /// `... RETURNING ...` after the clauses (PostgreSQL, SQLite, MariaDB).
    returning: bool,
    /// `... SETTINGS name = value` after the filter (ClickHouse).
    settings_clause: bool,
}

impl Syntax {
    fn of(dialect: ExplainDialect) -> Self {
        let base = Self {
            update_modifiers: &[],
            delete_modifiers: &[],
            only: false,
            update_or: false,
            order_limit: false,
            alter_mutation: false,
            executable_comments: false,
            returning: false,
            settings_clause: false,
        };
        match dialect {
            ExplainDialect::Postgres => Self { only: true, returning: true, ..base },
            ExplainDialect::MySql | ExplainDialect::MariaDb => Self {
                update_modifiers: &["LOW_PRIORITY", "IGNORE"],
                delete_modifiers: &["LOW_PRIORITY", "QUICK", "IGNORE"],
                order_limit: true,
                executable_comments: true,
                // MariaDB's `DELETE ... RETURNING`; MySQL has none (a column may be named so).
                returning: dialect == ExplainDialect::MariaDb,
                ..base
            },
            ExplainDialect::Sqlite => Self { update_or: true, returning: true, ..base },
            ExplainDialect::ClickHouse => Self { alter_mutation: true, settings_clause: true, ..base },
            // PostgreSQL's UPDATE and DELETE, without `ONLY`; `RETURNING` is read the same way.
            ExplainDialect::DuckDb => Self { returning: true, ..base },
        }
    }
}

/// Words that mean the table reference is more than one table (or more than a table).
const NOT_A_TABLE: &[&str] = &[
    "JOIN", "INNER", "LEFT", "RIGHT", "FULL", "CROSS", "NATURAL", "STRAIGHT_JOIN", "ON", "USING", "FROM", "PARTITION",
    "FORCE", "USE", "IGNORE", "WHERE", "ORDER", "LIMIT", "RETURNING", "SET", "VALUES", "SELECT",
];

/// The data-changing statements of `sql` (at most [`MAX_STATEMENTS`] read in full), for the
/// connection's dialect. `lex` is how the server reads the text.
pub fn scan(sql: &str, lex: &SqlLexOptions, dialect: ExplainDialect) -> Scan {
    let mut scan = Scan::default();
    scan_into(&mut scan, sql, lex, dialect);
    scan
}

/// [`scan`] of statements already cut apart (a script's), without joining them into one text.
pub fn scan_statements<'a>(
    statements: impl Iterator<Item = &'a str>,
    lex: &SqlLexOptions,
    dialect: ExplainDialect,
) -> Scan {
    let mut scan = Scan::default();
    for statement in statements {
        scan_into(&mut scan, statement, lex, dialect);
    }
    scan
}

/// Adds the data-changing statements of `sql` to `scan`: past [`MAX_STATEMENTS`] they are counted
/// and nothing is built for them.
fn scan_into(scan: &mut Scan, sql: &str, lex: &SqlLexOptions, dialect: ExplainDialect) {
    let syntax = Syntax::of(dialect);
    let mut reader = Reader { sql, tokens: token_iter(sql, lex).peekable(), end: 0, ended: false };
    loop {
        reader.ended = false;
        let Some(first) = reader.next() else {
            if reader.ended {
                continue; // an empty statement
            }
            break;
        };
        let outcome = if reader.is_word(&first, "UPDATE") {
            reader.update(&syntax)
        } else if reader.is_word(&first, "DELETE") {
            reader.delete(&syntax)
        } else if syntax.alter_mutation && reader.is_word(&first, "ALTER") {
            reader.alter(&syntax)
        } else if reader.is_word(&first, "WITH") {
            reader.with()
        } else {
            Outcome::Skip
        };
        reader.skip_statement();
        if matches!(outcome, Outcome::Skip) {
            continue;
        }
        if scan.found.len() >= MAX_STATEMENTS {
            scan.more += 1;
            continue;
        }
        let statement = first.range.start..reader.end;
        let shape = match outcome {
            Outcome::Skip => continue,
            // A versioned comment runs as code, and its delimiters are not tokens.
            Outcome::Change(_) if syntax.executable_comments && has_executable_comment(&sql[statement.clone()]) => {
                Shape::Unsupported
            }
            Outcome::Change(change) => Shape::Preview(change.preview(sql)),
            Outcome::Unsupported => Shape::Unsupported,
            Outcome::SideEffects => Shape::SideEffects,
        };
        scan.found.push(Found { heading: crate::script_run::one_line(&sql[statement], HEADING_CHARS), shape });
    }
}

/// Functions that act on the session or the server beyond the rows a read returns, and that a
/// rollback does not undo: advisory and named locks, sequences, other databases, other backends.
fn has_side_effects(word: &str) -> bool {
    const NAMES: &[&str] = &[
        "get_lock",
        "release_lock",
        "release_all_locks",
        "pg_terminate_backend",
        "pg_cancel_backend",
        "nextval",
        "setval",
        "currval",
        "lastval",
        "sleep",
        "benchmark",
        "pg_logical_emit_message",
    ];
    const PREFIXES: &[&str] = &["pg_advisory_", "pg_try_advisory_", "dblink", "pg_stat_reset"];
    NAMES.iter().any(|name| word.eq_ignore_ascii_case(name))
        || PREFIXES.iter().any(|prefix| word.as_bytes().get(..prefix.len()).is_some_and(|head| head.eq_ignore_ascii_case(prefix.as_bytes())))
}

fn has_executable_comment(statement: &str) -> bool {
    statement.contains("/*!") || statement.contains("/*M!")
}

/// What the reader made of one statement.
enum Outcome {
    /// Not a statement that changes data.
    Skip,
    Unsupported,
    /// See [`Shape::SideEffects`].
    SideEffects,
    Change(Change),
}

/// The pieces of an `UPDATE` / `DELETE` the two reads are built from, as ranges of the text.
struct Change {
    kind: Kind,
    /// The table's name.
    table: Range<usize>,
    /// The name and what follows it up to the clause that comes next (an alias).
    source: Range<usize>,
    filter: Option<Range<usize>>,
    order_by: Option<Range<usize>>,
    limit: Option<u64>,
    replaces: bool,
}

impl Change {
    fn preview(self, sql: &str) -> Preview {
        let mut from = format!("FROM {}", &sql[self.source]);
        if let Some(filter) = self.filter.clone() {
            from.push_str(" WHERE ");
            from.push_str(&sql[filter]);
        }
        if let Some(order_by) = self.order_by {
            from.push_str(" ORDER BY ");
            from.push_str(&sql[order_by]);
        }
        let (count_sql, rows_sql) = match self.limit {
            Some(limit) => (
                format!("SELECT count(*) FROM (SELECT 1 {from} LIMIT {limit}) AS tablex_preview"),
                format!("SELECT * {from} LIMIT {}", limit.min(PREVIEW_ROWS as u64)),
            ),
            None => (format!("SELECT count(*) {from}"), format!("SELECT * {from} LIMIT {PREVIEW_ROWS}")),
        };
        Preview {
            kind: self.kind,
            table: sql[self.table].to_owned(),
            whole_table: self.filter.is_none() && self.limit.is_none(),
            replaces: self.replaces,
            count_sql,
            rows_sql,
        }
    }
}

/// Rows of a statement the confirmation shows.
pub const PREVIEW_ROWS: usize = 20;

/// The clause the reader is in.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Part {
    /// `SET a = 1, ...` (or ClickHouse's `UPDATE a = 1`): nothing of it is kept.
    Assign,
    Filter,
    Order,
    Limit,
    /// `RETURNING ...`: nothing of it changes which rows are touched.
    Rest,
}

/// A table reference read: where it is, and the word that ended it.
struct TableRef {
    table: Range<usize>,
    source: Range<usize>,
    stop: Option<&'static str>,
}

struct Reader<'a> {
    sql: &'a str,
    tokens: Peekable<TokenIter<'a>>,
    /// End of the last token of the statement read so far.
    end: usize,
    /// The statement's `;` was read.
    ended: bool,
}

impl<'a> Reader<'a> {
    /// The next token of the statement; `None` at its `;` (read, and `ended` set) or the end of the text.
    fn next(&mut self) -> Option<SqlToken> {
        if self.ended {
            return None;
        }
        let token = self.tokens.next()?;
        if token.kind == SqlTokenKind::Semicolon {
            self.ended = true;
            return None;
        }
        self.end = token.range.end;
        Some(token)
    }

    fn peek(&mut self) -> Option<SqlToken> {
        if self.ended {
            return None;
        }
        self.tokens.peek().filter(|token| token.kind != SqlTokenKind::Semicolon).cloned()
    }

    fn spelled(&self, token: &SqlToken) -> &'a str {
        &self.sql[token.range.clone()]
    }

    fn is_word(&self, token: &SqlToken, word: &str) -> bool {
        token.kind == SqlTokenKind::Word && self.spelled(token).eq_ignore_ascii_case(word)
    }

    /// Whether the token is a call or an assignment whose effects stay after a rollback (see
    /// [`has_side_effects`]): a name, a quoted name followed by `(`, `LAST_INSERT_ID(expr)` (it sets
    /// the value; without an argument it only reads it) and the `:=` of a user variable.
    fn has_side_effects_at(&self, token: &SqlToken) -> bool {
        let (text, rest) = (self.spelled(token), &self.sql[token.range.end..]);
        match token.kind {
            SqlTokenKind::Word => {
                has_side_effects(text)
                    || (text.eq_ignore_ascii_case("last_insert_id")
                        && rest.trim_start().strip_prefix('(').is_some_and(|args| !args.trim_start().starts_with(')')))
            }
            SqlTokenKind::Quoted if self.is_name(token) => {
                rest.trim_start().starts_with('(') && has_side_effects(text.get(1..text.len() - 1).unwrap_or(""))
            }
            SqlTokenKind::Other => text == ":" && rest.starts_with('='),
            _ => false,
        }
    }

    fn is_any_word(&self, token: &SqlToken, words: &[&str]) -> bool {
        words.iter().any(|word| self.is_word(token, word))
    }

    /// Reads the next token when it is one of `words`.
    fn next_if_word(&mut self, words: &[&str]) -> bool {
        let found = self.peek().is_some_and(|token| self.is_any_word(&token, words));
        if found {
            self.next();
        }
        found
    }

    fn skip_statement(&mut self) {
        while self.next().is_some() {}
    }

    /// `UPDATE [modifiers] table [alias] SET ... [WHERE ...] [ORDER BY ...] [LIMIT n]`.
    fn update(&mut self, syntax: &Syntax) -> Outcome {
        while self.next_if_word(syntax.update_modifiers) {}
        let mut replaces = false;
        if syntax.update_or && self.next_if_word(&["OR"]) {
            // `OR REPLACE` deletes the rows that conflict; the other conflict words delete none.
            replaces = self.peek().is_some_and(|token| self.is_word(&token, "REPLACE"));
            if self.next().is_none() {
                return Outcome::Unsupported;
            }
        }
        let only = self.only(syntax);
        match self.table_ref(&["SET"]) {
            Some(TableRef { table, source, stop: Some(_) }) => {
                let source = only.unwrap_or(source.start)..source.end;
                self.finish(syntax, Kind::Update, table, source, Part::Assign, replaces)
            }
            _ => Outcome::Unsupported,
        }
    }

    /// `DELETE [modifiers] FROM table [alias] [WHERE ...] [ORDER BY ...] [LIMIT n]`.
    fn delete(&mut self, syntax: &Syntax) -> Outcome {
        while self.next_if_word(syntax.delete_modifiers) {}
        if !self.next_if_word(&["FROM"]) {
            return Outcome::Unsupported;
        }
        let only = self.only(syntax);
        let Some(TableRef { table, source, stop }) = self.table_ref(&["WHERE", "ORDER", "LIMIT", "RETURNING", "USING"])
        else {
            return Outcome::Unsupported;
        };
        let source = only.unwrap_or(source.start)..source.end;
        let part = match stop {
            None => Part::Rest,
            Some(word) => match self.enter(syntax, word) {
                Some(part) => part,
                None => return Outcome::Unsupported,
            },
        };
        self.finish(syntax, Kind::Delete, table, source, part, false)
    }

    /// PostgreSQL's `ONLY` (the table without its children): kept in the previewed text, where it
    /// decides which rows are read. Returns where it starts.
    fn only(&mut self, syntax: &Syntax) -> Option<usize> {
        if !syntax.only {
            return None;
        }
        let start = self.peek().filter(|token| self.is_word(token, "ONLY"))?.range.start;
        self.next();
        Some(start)
    }

    /// ClickHouse: `ALTER TABLE t UPDATE a = 1 WHERE ...`, `ALTER TABLE t DELETE WHERE ...`. Any other
    /// `ALTER` is not a change of data; one with `ON CLUSTER` is a change this reader does not follow.
    fn alter(&mut self, syntax: &Syntax) -> Outcome {
        if !self.next_if_word(&["TABLE"]) {
            return Outcome::Skip;
        }
        match self.table_ref(&["UPDATE", "DELETE"]) {
            Some(TableRef { table, source, stop: Some(word) }) => {
                let kind = if word == "UPDATE" { Kind::Update } else { Kind::Delete };
                self.finish(syntax, kind, table, source, Part::Assign, false)
            }
            // `ON CLUSTER name UPDATE|DELETE`: the table reference stopped at `ON`.
            None if self.next_if_word(&["ON"]) => {
                let mutation = self.next_if_word(&["CLUSTER"])
                    && self.next().is_some()
                    && self.next_if_word(&["UPDATE", "DELETE"]);
                if mutation {
                    Outcome::Unsupported
                } else {
                    Outcome::Skip
                }
            }
            // `ALTER TABLE t MODIFY COLUMN ..., UPDATE ... WHERE ...`: a data change after another command.
            None if self.update_after_comma() => Outcome::Unsupported,
            _ => Outcome::Skip,
        }
    }

    /// Whether the rest of the statement has a command `UPDATE` / `DELETE` after a top-level `,`.
    fn update_after_comma(&mut self) -> bool {
        let (mut depth, mut after_comma) = (0usize, false);
        while let Some(token) = self.next() {
            match token.kind {
                SqlTokenKind::Open => depth += 1,
                SqlTokenKind::Close => depth = depth.saturating_sub(1),
                SqlTokenKind::Word if depth == 0 && after_comma && self.is_any_word(&token, &["UPDATE", "DELETE"]) => {
                    return true;
                }
                _ => {}
            }
            after_comma = depth == 0 && token.kind == SqlTokenKind::Comma;
        }
        false
    }

    /// `WITH ... UPDATE|DELETE` (or a data-modifying CTE, `WITH d AS (DELETE ...)`) is a data change
    /// this reader does not follow; `WITH ... SELECT` is none.
    fn with(&mut self) -> Outcome {
        let mut depth = 0usize;
        let mut after_open = false;
        while let Some(token) = self.next() {
            let opens = after_open;
            after_open = token.kind == SqlTokenKind::Open;
            match token.kind {
                SqlTokenKind::Open => depth += 1,
                SqlTokenKind::Close => depth = depth.saturating_sub(1),
                SqlTokenKind::Word if depth == 0 => {
                    if self.is_any_word(&token, &["UPDATE", "DELETE"]) {
                        return Outcome::Unsupported;
                    }
                    if self.is_any_word(&token, &["SELECT", "VALUES", "TABLE", "INSERT", "MERGE"]) {
                        return Outcome::Skip;
                    }
                }
                SqlTokenKind::Word if opens && self.is_any_word(&token, &["UPDATE", "DELETE"]) => {
                    return Outcome::Unsupported;
                }
                _ => {}
            }
        }
        Outcome::Skip
    }

    /// `[db.][schema.]table [alias]`, up to (and with) a word of `stops`, or the end of the statement.
    /// `None`: not a plain single table.
    fn table_ref(&mut self, stops: &[&'static str]) -> Option<TableRef> {
        let first = self.next().filter(|token| self.is_name(token))?;
        let start = first.range.start;
        let mut table_end = first.range.end;
        while self.peek().is_some_and(|token| token.kind == SqlTokenKind::Other && self.spelled(&token) == ".") {
            self.next();
            table_end = self.next().filter(|token| self.is_name(token))?.range.end;
        }
        let mut end = table_end;
        loop {
            let Some(token) = self.peek() else {
                return Some(TableRef { table: start..table_end, source: start..end, stop: None });
            };
            if token.kind == SqlTokenKind::Word {
                if let Some(stop) = stops.iter().copied().find(|stop| self.is_word(&token, stop)) {
                    self.next();
                    return Some(TableRef { table: start..table_end, source: start..end, stop: Some(stop) });
                }
                if self.is_any_word(&token, NOT_A_TABLE) {
                    return None;
                }
            }
            match token.kind {
                SqlTokenKind::Word => {}
                SqlTokenKind::Quoted if self.is_name(&token) => {}
                // `UPDATE ONLY t *`
                SqlTokenKind::Other if self.spelled(&token) == "*" => {}
                _ => return None,
            }
            self.next();
            end = token.range.end;
        }
    }

    /// A word, or a quoted identifier (not a string).
    fn is_name(&self, token: &SqlToken) -> bool {
        match token.kind {
            SqlTokenKind::Word => true,
            SqlTokenKind::Quoted => matches!(self.sql.as_bytes()[token.range.start], b'"' | b'`' | b'['),
            _ => false,
        }
    }

    /// The clause a keyword at the top level of the statement starts; `None`: the statement is
    /// not one this reader follows (`USING`, `WHERE CURRENT OF`, `ORDER BY` / `LIMIT` where the
    /// dialect has none).
    fn enter(&mut self, syntax: &Syntax, word: &str) -> Option<Part> {
        let is = |name: &str| word.eq_ignore_ascii_case(name);
        if is("WHERE") {
            let cursor = self.peek().is_some_and(|token| self.is_word(&token, "CURRENT"));
            (!cursor).then_some(Part::Filter)
        } else if is("ORDER") {
            (syntax.order_limit && self.next_if_word(&["BY"])).then_some(Part::Order)
        } else if is("LIMIT") {
            syntax.order_limit.then_some(Part::Limit)
        } else if (is("RETURNING") && syntax.returning) || (is("SETTINGS") && syntax.settings_clause) {
            Some(Part::Rest)
        } else {
            None
        }
    }

    /// Reads the clauses after the table, from `part` on, and builds the outcome.
    fn finish(
        &mut self,
        syntax: &Syntax,
        kind: Kind,
        table: Range<usize>,
        source: Range<usize>,
        part: Part,
        replaces: bool,
    ) -> Outcome {
        let mut part = part;
        let (mut filter, mut order_by): (Option<Range<usize>>, Option<Range<usize>>) = (None, None);
        let mut limit: Option<u64> = None;
        let mut depth = 0usize;
        let mut after_distinct = false;
        let (mut saw_filter, mut saw_order, mut saw_limit) = (part == Part::Filter, part == Part::Order, part == Part::Limit);
        let span = |range: &mut Option<Range<usize>>, token: &SqlToken| {
            let start = range.as_ref().map_or(token.range.start, |range| range.start);
            *range = Some(start..token.range.end);
        };
        while let Some(token) = self.next() {
            let top = depth == 0;
            match token.kind {
                SqlTokenKind::Open => depth += 1,
                SqlTokenKind::Close => match depth.checked_sub(1) {
                    Some(inner) => depth = inner,
                    None => return Outcome::Unsupported,
                },
                _ => {}
            }
            let distinct = after_distinct;
            after_distinct = top && self.is_word(&token, "DISTINCT");
            // ClickHouse `ALTER TABLE t UPDATE ... WHERE ..., DELETE WHERE ...`: more than one command.
            if top
                && syntax.alter_mutation
                && ((token.kind == SqlTokenKind::Comma && part == Part::Filter) || self.is_any_word(&token, &["UPDATE", "DELETE"]))
            {
                return Outcome::Unsupported;
            }
            if top && token.kind == SqlTokenKind::Word && part != Part::Rest {
                let word = self.spelled(&token);
                let next = if word.eq_ignore_ascii_case("WHERE") && part == Part::Assign {
                    Some("WHERE")
                } else if word.eq_ignore_ascii_case("ORDER") && matches!(part, Part::Assign | Part::Filter) {
                    Some("ORDER")
                } else if word.eq_ignore_ascii_case("LIMIT") && matches!(part, Part::Assign | Part::Filter | Part::Order) {
                    Some("LIMIT")
                } else if word.eq_ignore_ascii_case("RETURNING") && syntax.returning && part != Part::Assign {
                    // In the assignments it is a name (the rows are the same either way: the text
                    // after SET is never read).
                    Some("RETURNING")
                } else if word.eq_ignore_ascii_case("SETTINGS") && syntax.settings_clause && part == Part::Filter {
                    Some("SETTINGS")
                } else if word.eq_ignore_ascii_case("FROM") && part == Part::Assign && !distinct {
                    // `UPDATE ... SET ... FROM other` joins.
                    return Outcome::Unsupported;
                } else if word.eq_ignore_ascii_case("IN")
                    && syntax.alter_mutation
                    && self.peek().is_some_and(|token| self.is_word(&token, "PARTITION"))
                {
                    return Outcome::Unsupported;
                } else {
                    None
                };
                if let Some(word) = next {
                    let Some(entered) = self.enter(syntax, word) else {
                        return Outcome::Unsupported;
                    };
                    part = entered;
                    saw_filter |= part == Part::Filter;
                    saw_order |= part == Part::Order;
                    saw_limit |= part == Part::Limit;
                    continue;
                }
            }
            // The text of these two clauses is what the previews copy.
            if matches!(part, Part::Filter | Part::Order) && self.has_side_effects_at(&token) {
                return Outcome::SideEffects;
            }
            match part {
                Part::Filter => span(&mut filter, &token),
                Part::Order => span(&mut order_by, &token),
                Part::Limit => {
                    if limit.is_some() || token.kind != SqlTokenKind::Number {
                        return Outcome::Unsupported;
                    }
                    match self.spelled(&token).parse() {
                        Ok(value) => limit = Some(value),
                        Err(_) => return Outcome::Unsupported,
                    }
                }
                Part::Assign | Part::Rest => {}
            }
        }
        // An open parenthesis, or a clause without its text (`WHERE ;`), is a statement not understood.
        if depth != 0 || (saw_filter && filter.is_none()) || (saw_order && order_by.is_none()) || (saw_limit && limit.is_none()) {
            return Outcome::Unsupported;
        }
        Outcome::Change(Change { kind, table, source, filter, order_by, limit, replaces })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn lex(dialect: ExplainDialect) -> SqlLexOptions {
        match dialect {
            ExplainDialect::Postgres => SqlLexOptions::postgres(),
            ExplainDialect::MySql => SqlLexOptions::mysql(),
            ExplainDialect::MariaDb => SqlLexOptions::mariadb(),
            ExplainDialect::Sqlite => SqlLexOptions::sqlite(),
            ExplainDialect::ClickHouse => SqlLexOptions::clickhouse(),
            ExplainDialect::DuckDb => SqlLexOptions::postgres(),
        }
    }

    fn scan_on(dialect: ExplainDialect, sql: &str) -> Scan {
        scan(sql, &lex(dialect), dialect)
    }

    /// The preview of the one statement of `sql`.
    fn preview_on(dialect: ExplainDialect, sql: &str) -> Preview {
        let scan = scan_on(dialect, sql);
        assert_eq!(scan.found.len(), 1, "{sql}: {scan:?}");
        match scan.found.into_iter().next().unwrap().shape {
            Shape::Preview(preview) => preview,
            Shape::Unsupported | Shape::SideEffects => panic!("{sql}: not previewed"),
        }
    }

    fn unsupported_on(dialect: ExplainDialect, sql: &str) {
        let scan = scan_on(dialect, sql);
        assert_eq!(scan.found.len(), 1, "{sql}: {scan:?}");
        assert_eq!(scan.found[0].shape, Shape::Unsupported, "{sql}");
    }

    use ExplainDialect::{ClickHouse, DuckDb, MariaDb, MySql, Postgres, Sqlite};

    #[test]
    fn a_delete_becomes_a_count_and_the_first_rows() {
        let preview = preview_on(Postgres, "DELETE FROM items WHERE id < 3");
        assert_eq!(preview.kind, Kind::Delete);
        assert_eq!(preview.table, "items");
        assert!(!preview.whole_table);
        assert_eq!(preview.count_sql, "SELECT count(*) FROM items WHERE id < 3");
        assert_eq!(preview.rows_sql, "SELECT * FROM items WHERE id < 3 LIMIT 20");
    }

    #[test]
    fn an_update_keeps_its_alias_and_leaves_its_assignments_out() {
        let preview = preview_on(Postgres, "UPDATE ONLY app.items AS i SET name = 'x WHERE y', n = (SELECT 1) WHERE i.id = 4 RETURNING *");
        assert_eq!(preview.kind, Kind::Update);
        assert_eq!(preview.table, "app.items");
        // `ONLY` (the table without its children) decides which rows are read: it stays.
        assert_eq!(preview.count_sql, "SELECT count(*) FROM ONLY app.items AS i WHERE i.id = 4");
        let preview = preview_on(Postgres, "DELETE FROM ONLY items * WHERE id = 1");
        assert_eq!(preview.count_sql, "SELECT count(*) FROM ONLY items * WHERE id = 1");
    }

    #[test]
    fn a_statement_without_where_changes_the_whole_table() {
        for (dialect, sql) in [
            (Postgres, "DELETE FROM items"),
            (Postgres, "update items set a = 1;"),
            (MySql, "DELETE FROM `items`"),
            (Sqlite, "UPDATE items SET a = 1"),
        ] {
            let preview = preview_on(dialect, sql);
            assert!(preview.whole_table, "{sql}");
            assert_eq!(preview.rows_sql.matches("WHERE").count(), 0, "{sql}");
        }
        assert!(!preview_on(Postgres, "DELETE FROM items WHERE true").whole_table);
    }

    #[test]
    fn quoted_and_schema_qualified_names_are_kept_as_written() {
        let preview = preview_on(Postgres, "DELETE FROM \"My App\".\"Order Items\" WHERE \"id\" = 1");
        assert_eq!(preview.table, "\"My App\".\"Order Items\"");
        assert_eq!(preview.count_sql, "SELECT count(*) FROM \"My App\".\"Order Items\" WHERE \"id\" = 1");
        let preview = preview_on(MySql, "UPDATE `shop`.`items` SET `a` = 1 WHERE `b` = 2");
        assert_eq!(preview.table, "`shop`.`items`");
        assert_eq!(preview.count_sql, "SELECT count(*) FROM `shop`.`items` WHERE `b` = 2");
        let preview = preview_on(Sqlite, "DELETE FROM main.\"t\" WHERE [x] = 1");
        assert_eq!(preview.table, "main.\"t\"");
        let preview = preview_on(Postgres, "DELETE FROM db.app.items WHERE id = 1");
        assert_eq!(preview.table, "db.app.items");
    }

    #[test]
    fn comments_and_strings_do_not_end_or_start_a_clause() {
        let preview = preview_on(
            Postgres,
            "/* lead */ DELETE /* a WHERE b */ FROM items -- WHERE x\n WHERE note = 'WHERE; x' -- tail\n AND id = 1; -- done",
        );
        assert_eq!(preview.count_sql, "SELECT count(*) FROM items WHERE note = 'WHERE; x' -- tail\n AND id = 1");
        // A comment in the clause does not eat the SQL appended after it.
        assert!(preview.rows_sql.ends_with("AND id = 1 LIMIT 20"), "{}", preview.rows_sql);
        // The only WHERE is inside a string: the statement has none.
        let preview = preview_on(Postgres, "UPDATE items SET note = 'WHERE id = 1'");
        assert!(preview.whole_table);
        let preview = preview_on(Postgres, "DELETE FROM items WHERE note = $$ WHERE ; $$");
        assert_eq!(preview.count_sql, "SELECT count(*) FROM items WHERE note = $$ WHERE ; $$");
    }

    #[test]
    fn a_subquery_in_where_is_part_of_the_filter() {
        let preview = preview_on(
            Postgres,
            "DELETE FROM items WHERE id IN (SELECT item FROM sold WHERE n > 1 ORDER BY n LIMIT 3) AND NOT EXISTS (SELECT 1 FROM x)",
        );
        assert_eq!(
            preview.count_sql,
            "SELECT count(*) FROM items WHERE id IN (SELECT item FROM sold WHERE n > 1 ORDER BY n LIMIT 3) AND NOT EXISTS (SELECT 1 FROM x)"
        );
        // A subquery with its own FROM in SET is no join.
        let preview = preview_on(Postgres, "UPDATE t SET a = (SELECT max(x) FROM u WHERE u.k = t.k), b = EXTRACT(year FROM d) WHERE c");
        assert_eq!(preview.count_sql, "SELECT count(*) FROM t WHERE c");
        // `IS DISTINCT FROM` in SET is not a join either.
        preview_on(Postgres, "UPDATE t SET a = b IS DISTINCT FROM c WHERE d");
    }

    #[test]
    fn mysql_delete_and_update_keep_order_by_and_limit() {
        for dialect in [MySql, MariaDb] {
            let preview = preview_on(dialect, "DELETE LOW_PRIORITY QUICK FROM `log` WHERE ts < 5 ORDER BY ts DESC, id LIMIT 100");
            assert_eq!(preview.count_sql, "SELECT count(*) FROM (SELECT 1 FROM `log` WHERE ts < 5 ORDER BY ts DESC, id LIMIT 100) AS tablex_preview");
            assert_eq!(preview.rows_sql, "SELECT * FROM `log` WHERE ts < 5 ORDER BY ts DESC, id LIMIT 20");
            assert!(!preview.whole_table);
            let preview = preview_on(dialect, "UPDATE IGNORE t SET a = 1 WHERE b = 2 LIMIT 5");
            assert_eq!(preview.rows_sql, "SELECT * FROM t WHERE b = 2 LIMIT 5");
            // A limit alone is not "every row".
            assert!(!preview_on(dialect, "DELETE FROM t LIMIT 5").whole_table);
            assert_eq!(preview_on(dialect, "DELETE FROM t ORDER BY id").count_sql, "SELECT count(*) FROM t ORDER BY id");
        }
        // Other dialects have no such clause: not guessed at.
        unsupported_on(Postgres, "DELETE FROM t WHERE a LIMIT 5");
        unsupported_on(Sqlite, "DELETE FROM t WHERE a ORDER BY id LIMIT 5");
        unsupported_on(MySql, "DELETE FROM t WHERE a LIMIT 5 OFFSET 1");
        unsupported_on(MySql, "DELETE FROM t WHERE a LIMIT ?");
    }

    #[test]
    fn sqlite_update_or_and_indexed_by() {
        let preview = preview_on(Sqlite, "UPDATE OR REPLACE items SET a = 1 WHERE id = 2");
        assert_eq!(preview.count_sql, "SELECT count(*) FROM items WHERE id = 2");
        let preview = preview_on(Sqlite, "DELETE FROM items INDEXED BY ix WHERE id = 2 RETURNING id");
        assert_eq!(preview.count_sql, "SELECT count(*) FROM items INDEXED BY ix WHERE id = 2");
    }

    #[test]
    fn clickhouse_mutations_and_lightweight_deletes() {
        let preview = preview_on(ClickHouse, "ALTER TABLE logs.events UPDATE level = 'warn' WHERE ts < now() - 1");
        assert_eq!((preview.kind, preview.table.as_str()), (Kind::Update, "logs.events"));
        assert_eq!(preview.count_sql, "SELECT count(*) FROM logs.events WHERE ts < now() - 1");
        let preview = preview_on(ClickHouse, "ALTER TABLE events DELETE WHERE id IN (1, 2)");
        assert_eq!(preview.kind, Kind::Delete);
        assert_eq!(preview.rows_sql, "SELECT * FROM events WHERE id IN (1, 2) LIMIT 20");
        let preview = preview_on(ClickHouse, "DELETE FROM events WHERE id = 1 SETTINGS mutations_sync = 1");
        assert_eq!(preview.count_sql, "SELECT count(*) FROM events WHERE id = 1");
        unsupported_on(ClickHouse, "DELETE FROM events ON CLUSTER c WHERE id = 1");
        unsupported_on(ClickHouse, "ALTER TABLE events UPDATE a = 1 IN PARTITION '2024' WHERE b");
        unsupported_on(ClickHouse, "ALTER TABLE events ON CLUSTER c DELETE WHERE id = 1");
        unsupported_on(ClickHouse, "ALTER TABLE events UPDATE a = 1 WHERE b = 2, DELETE WHERE c = 3");
        unsupported_on(ClickHouse, "ALTER TABLE events DELETE WHERE b = 2, UPDATE a = 1 WHERE c = 3");
        unsupported_on(ClickHouse, "ALTER TABLE events MODIFY COLUMN x Int32, UPDATE a = 1 WHERE c = 3");
        unsupported_on(ClickHouse, "ALTER TABLE events ADD COLUMN y Int8, DELETE WHERE c = 3");
        // Other `ALTER`s are no data change; on the other dialects `ALTER TABLE ... DELETE` is nothing either.
        assert!(scan_on(ClickHouse, "ALTER TABLE events ADD COLUMN x Int32; ALTER TABLE events DROP COLUMN y").found.is_empty());
        assert!(scan_on(Postgres, "ALTER TABLE events DELETE WHERE id = 1").found.is_empty());
    }

    #[test]
    fn shapes_the_reader_is_unsure_of_are_unsupported() {
        for (dialect, sql) in [
            (Postgres, "UPDATE t SET a = u.a FROM u WHERE u.id = t.id"),
            (Postgres, "DELETE FROM t USING u WHERE u.id = t.id"),
            (Postgres, "WITH d AS (SELECT 1) DELETE FROM t WHERE id IN (SELECT * FROM d)"),
            (Postgres, "WITH u AS (UPDATE t SET a = 1 RETURNING *) UPDATE x SET b = 1"),
            (Postgres, "DELETE FROM t WHERE CURRENT OF cur"),
            (Postgres, "DELETE FROM t WHERE (a = 1"),
            (Postgres, "DELETE FROM t WHERE"),
            (Postgres, "DELETE FROM t, u WHERE a"),
            (Postgres, "DELETE t"),
            (Postgres, "UPDATE t"),
            (Postgres, "UPDATE 'str' SET a = 1"),
            (Postgres, "DELETE FROM (SELECT 1) WHERE a"),
            (MySql, "UPDATE t1, t2 SET t1.a = t2.a WHERE t1.id = t2.id"),
            (MySql, "UPDATE t1 JOIN t2 ON t1.id = t2.id SET t1.a = 1"),
            (MySql, "UPDATE t1 INNER JOIN t2 USING (id) SET t1.a = 1"),
            (MySql, "DELETE t1 FROM t1 JOIN t2 ON t1.id = t2.id"),
            (MySql, "DELETE FROM t1, t2 USING t1 JOIN t2 WHERE t1.id = t2.id"),
            (MariaDb, "DELETE FROM t PARTITION (p0) WHERE a"),
            (MySql, "DELETE FROM t WHERE a /*!50000 AND b = 1 */"),
            (Sqlite, "UPDATE t SET a = u.a FROM u WHERE u.id = t.id"),
        ] {
            unsupported_on(dialect, sql);
        }
    }

    #[test]
    fn a_filter_that_calls_a_function_with_lasting_effects_is_not_previewed() {
        let side_effects = |dialect, sql: &str| {
            let scan = scan_on(dialect, sql);
            assert_eq!(scan.found.len(), 1, "{sql}: {scan:?}");
            assert_eq!(scan.found[0].shape, Shape::SideEffects, "{sql}");
        };
        for (dialect, sql) in [
            (Postgres, "DELETE FROM t WHERE pg_try_advisory_lock(id)"),
            (Postgres, "DELETE FROM t WHERE PG_ADVISORY_XACT_LOCK(1) IS NOT NULL"),
            (Postgres, "UPDATE t SET a = 1 WHERE id IN (SELECT pg_terminate_backend(pid) FROM pg_stat_activity)"),
            (Postgres, "DELETE FROM t WHERE dblink_exec('x', 'y') = 'z'"),
            (Postgres, "UPDATE t SET a = 1 WHERE id = nextval('s')"),
            (MySql, "DELETE FROM t WHERE id = 1 ORDER BY setval('s', 1)"),
            (MySql, "DELETE FROM t WHERE GET_LOCK('k', 10) = 1"),
            (MySql, "UPDATE t SET a = 1 WHERE RELEASE_LOCK('k')"),
            (DuckDb, "DELETE FROM t WHERE id = nextval('item_ids')"),
            (Postgres, "UPDATE t SET a = 1 WHERE pg_catalog.pg_advisory_lock(1)"),
            // A quoted name is the same function.
            (Postgres, "DELETE FROM t WHERE \"nextval\" ('s') > 0"),
            (Postgres, "DELETE FROM t WHERE \"pg_catalog\".\"pg_advisory_lock\"(1)"),
            (MySql, "DELETE FROM t WHERE `get_lock`('k', 1) = 1"),
            (Postgres, "DELETE FROM t WHERE pg_logical_emit_message(true, 'p', 'm') IS NOT NULL"),
            (Postgres, "DELETE FROM t WHERE pg_stat_reset() IS NOT NULL"),
            (Postgres, "DELETE FROM t WHERE pg_stat_reset_shared('bgwriter') IS NOT NULL"),
            (MySql, "DELETE FROM t WHERE LAST_INSERT_ID(id) > 0"),
            (MySql, "DELETE FROM t WHERE id = last_insert_id ( id + 1 )"),
            (MySql, "DELETE FROM t WHERE (@n := @n + 1) > 0"),
            (MySql, "UPDATE t SET a = 1 WHERE SLEEP(5) = 0"),
            (MariaDb, "DELETE FROM t WHERE BENCHMARK(1000000, MD5('x')) = 0"),
        ] {
            side_effects(dialect, sql);
        }
        // The assignments are never copied, and names that merely look alike are not the functions.
        for (dialect, sql) in [
            (Postgres, "UPDATE t SET a = nextval('s') WHERE id = 1"),
            (Postgres, "DELETE FROM t WHERE \"nextval\" = 1 AND note = 'get_lock(1)'"),
            (Postgres, "DELETE FROM t WHERE advisory_count > 1"),
            // LAST_INSERT_ID() without an argument only reads the value.
            (MySql, "DELETE FROM t WHERE id = LAST_INSERT_ID()"),
            (MySql, "DELETE FROM t WHERE id = last_insert_id( )"),
            (MySql, "DELETE FROM t WHERE \"sleep\" = 1 AND note = '@n := 1'"),
        ] {
            preview_on(dialect, sql);
        }
    }

    #[test]
    fn other_statements_are_not_data_changes() {
        for sql in [
            "SELECT * FROM t WHERE a",
            "WITH c AS (SELECT 1) SELECT * FROM c",
            "INSERT INTO t VALUES ('DELETE FROM t')",
            "SELECT 'UPDATE t SET a = 1'",
            "-- DELETE FROM t\nSELECT 1",
            "CREATE TABLE delete_log (id int)",
            "TRUNCATE t",
            ";;",
            "",
        ] {
            assert_eq!(scan_on(Postgres, sql), Scan::default(), "{sql}");
        }
    }

    #[test]
    fn a_script_reads_the_first_five_and_counts_the_rest() {
        let mut script = String::from("SELECT 1;\n");
        for n in 0..8 {
            script.push_str(&format!("DELETE FROM t{n} WHERE id = {n};\nINSERT INTO log VALUES ({n});\n"));
        }
        let scan = scan_on(Postgres, &script);
        assert_eq!(scan.found.len(), MAX_STATEMENTS);
        assert_eq!(scan.more, 3);
        assert_eq!(scan.found[0].heading, "DELETE FROM t0 WHERE id = 0");
        let tables: Vec<_> = scan
            .found
            .iter()
            .map(|found| match &found.shape {
                Shape::Preview(preview) => preview.table.clone(),
                Shape::Unsupported | Shape::SideEffects => String::new(),
            })
            .collect();
        assert_eq!(tables, ["t0", "t1", "t2", "t3", "t4"]);
        // A statement that is not understood still takes its place in the list.
        let scan = scan_on(Postgres, "DELETE FROM a WHERE x; WITH c AS (SELECT 1) DELETE FROM b; UPDATE c SET z = 1");
        assert_eq!(scan.found.len(), 3);
        assert_eq!(scan.found[1].shape, Shape::Unsupported);
        assert!(matches!(&scan.found[2].shape, Shape::Preview(preview) if preview.whole_table));
    }

    #[test]
    fn a_column_named_like_a_clause_is_not_the_clause() {
        // `settings` is a clause of ClickHouse only, and only after the filter.
        for dialect in [Postgres, MySql, MariaDb, Sqlite] {
            let preview = preview_on(dialect, "UPDATE t SET settings = 1 WHERE id = 2");
            assert_eq!(preview.count_sql, "SELECT count(*) FROM t WHERE id = 2", "{dialect:?}");
            assert!(!preview.whole_table);
            let preview = preview_on(dialect, "DELETE FROM t WHERE settings = 1 AND id = 2");
            assert_eq!(preview.count_sql, "SELECT count(*) FROM t WHERE settings = 1 AND id = 2", "{dialect:?}");
        }
        let preview = preview_on(ClickHouse, "ALTER TABLE t UPDATE settings = 1 WHERE id = 2");
        assert_eq!(preview.count_sql, "SELECT count(*) FROM t WHERE id = 2");
        // `returning` is a clause where the dialect has one, and never inside the assignments.
        let preview = preview_on(Postgres, "UPDATE t SET returning = 1 WHERE id = 2");
        assert_eq!(preview.count_sql, "SELECT count(*) FROM t WHERE id = 2");
        let preview = preview_on(MySql, "UPDATE t SET a = 1 WHERE returning = 2");
        assert_eq!(preview.count_sql, "SELECT count(*) FROM t WHERE returning = 2");
        // Straight after SET it changes every row either way.
        assert!(preview_on(Postgres, "UPDATE t SET a = 1 RETURNING *").whole_table);
        assert_eq!(preview_on(Sqlite, "UPDATE t SET a = 1 WHERE b RETURNING a").count_sql, "SELECT count(*) FROM t WHERE b");
    }

    #[test]
    fn duckdb_reads_returning() {
        assert_eq!(preview_on(DuckDb, "UPDATE t SET a = 1 WHERE b RETURNING a").count_sql, "SELECT count(*) FROM t WHERE b");
        assert_eq!(preview_on(DuckDb, "DELETE FROM main.t WHERE b = 1").count_sql, "SELECT count(*) FROM main.t WHERE b = 1");
    }

    #[test]
    fn sqlite_update_or_replace_is_flagged() {
        assert!(preview_on(Sqlite, "UPDATE OR REPLACE t SET a = 1 WHERE b").replaces);
        assert!(preview_on(Sqlite, "update or replace t set a = 1").replaces);
        assert!(!preview_on(Sqlite, "UPDATE OR IGNORE t SET a = 1 WHERE b").replaces);
        assert!(!preview_on(Sqlite, "UPDATE t SET a = 1 WHERE b").replaces);
        assert!(!preview_on(Sqlite, "DELETE FROM t WHERE b").replaces);
    }

    #[test]
    fn a_data_modifying_cte_is_not_previewed() {
        unsupported_on(Postgres, "WITH d AS (DELETE FROM t WHERE a RETURNING *) SELECT * FROM d");
        unsupported_on(Postgres, "WITH RECURSIVE x AS (SELECT 1), u AS (UPDATE t SET a = 1 RETURNING a) SELECT * FROM u");
        assert!(scan_on(Postgres, "WITH c AS (SELECT 1) SELECT * FROM c FOR UPDATE").found.is_empty());
    }

    /// A script's statements are read one by one: a `--` comment at the end of one cannot swallow the next.
    #[test]
    fn statements_are_scanned_without_being_joined() {
        let statements = ["DELETE FROM a WHERE x -- why", "DELETE FROM b", "SELECT 1", "UPDATE c SET z = 1 WHERE y"];
        let scan = scan_statements(statements.into_iter(), &lex(Postgres), Postgres);
        let wholes: Vec<_> = scan
            .found
            .iter()
            .map(|found| match &found.shape {
                Shape::Preview(preview) => (preview.table.as_str(), preview.whole_table),
                Shape::Unsupported | Shape::SideEffects => ("", false),
            })
            .collect();
        assert_eq!(wholes, [("a", false), ("b", true), ("c", false)]);
        assert_eq!(scan.more, 0);
        // The cap holds across statements, and the rest is only counted.
        let many: Vec<String> = (0..9).map(|n| format!("DELETE FROM t{n} WHERE id = {n}")).collect();
        let scan = scan_statements(many.iter().map(String::as_str), &lex(Postgres), Postgres);
        assert_eq!((scan.found.len(), scan.more), (MAX_STATEMENTS, 4));
    }

    #[test]
    fn a_heading_is_one_short_line() {
        let long = format!("DELETE FROM t WHERE note = '{}'\n  AND id = 1", "x".repeat(300));
        let scan = scan_on(Postgres, &long);
        assert!(scan.found[0].heading.chars().count() <= 71 && !scan.found[0].heading.contains('\n'));
    }

    /// A multi-megabyte statement that is not a data change is read without a token list, and an
    /// `UPDATE` after it is found.
    #[test]
    fn a_large_insert_is_skipped() {
        let mut insert = String::from("INSERT INTO t VALUES ");
        while insert.len() < 1 << 20 {
            insert.push_str("(1, 'DELETE FROM x WHERE 1', 'a;b'),");
        }
        insert.push_str("(2, 'z');");
        assert_eq!(scan_on(Postgres, &insert), Scan::default());
        insert.push_str(" UPDATE after SET a = 1 WHERE b = 2");
        let scan = scan_on(Postgres, &insert);
        assert_eq!(scan.found.len(), 1);
        assert_eq!(scan.found[0].heading, "UPDATE after SET a = 1 WHERE b = 2");
    }
}
