//! SQL on MongoDB in a script tab: a `SELECT` statement is translated into the shell statement that
//! reads the same documents (`find`, or `aggregate` for GROUP BY, DISTINCT, aggregates and aliases)
//! before the script runs, as other database tools let you query MongoDB with SQL.
//!
//! The supported subset:
//!
//! ```sql
//! SELECT [DISTINCT] * | field [AS alias], COUNT(*), SUM(field), AVG(..), MIN(..), MAX(..)
//! FROM "database"."collection" | collection
//! [WHERE condition]   -- = != <> < <= > >=, [NOT] IN (..), [NOT] LIKE / ILIKE, IS [NOT] NULL,
//!                     -- [NOT] BETWEEN .. AND .., AND, OR, NOT, parentheses
//! [GROUP BY field, ..] [ORDER BY field [ASC|DESC], ..] [LIMIT n [OFFSET m]]
//! ```
//!
//! A field is a name (`status`), a path (`info.ref`) or a quoted name (`"info.ref"`); a value is a
//! string (`'a'`, or `"a"` on the right of a comparison), a number, `TRUE`, `FALSE` or `NULL`. An
//! `_id` compared with 24 hex digits is an `ObjectId`. Comments are `--`, `//` and `/* */`.
//!
//! The translation is one line followed by as many line breaks as the statement had, so a shell
//! error still names the line the user wrote.

use std::fmt::Write as _;

/// A statement that is not SQL this module reads, with where (a byte offset in the statement).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SqlError {
    pub at: usize,
    pub message: String,
}

/// [`rewrite_script`]'s answer: the script to run, and the database a single statement named.
#[derive(Debug, PartialEq, Eq)]
pub struct Rewritten {
    pub script: String,
    /// `FROM "database"."collection"` of a script that is that one statement: the run's database
    /// (the tab moves to it, as `use` does), so it stays on the fast path.
    pub database: Option<String>,
}

/// Translates the SQL statements of `script` (see the module docs); other statements are left
/// alone, and a line comment written `-- ` becomes `// `. `Err` names the line and column.
pub fn rewrite_script(script: &str) -> Result<Rewritten, String> {
    let ranges = crate::mongo_script::statement_ranges(script);
    let sql: Vec<(std::ops::Range<usize>, usize)> = ranges
        .iter()
        .filter_map(|range| sql_start(&script[range.clone()]).map(|start| (range.clone(), range.start + start)))
        .collect();
    if sql.is_empty() {
        return Ok(Rewritten { script: dash_comments_to_js(script, false), database: None });
    }
    let single = ranges.len() == 1;
    let mut out = String::with_capacity(script.len());
    let mut database = None;
    let mut copied = 0;
    for (range, start) in sql {
        out.push_str(&dash_comments_to_js(&script[copied..start], true));
        let statement = &script[start..range.end];
        let select = parse(statement).map_err(|error| place(script, start + error.at, &error.message))?;
        let shell = if single {
            database = select.database.clone();
            select.to_shell(false)
        } else {
            select.to_shell(true)
        };
        out.push_str(&shell);
        copied = range.end;
        // Its `;` (a single statement needs none), then the lines the statement took, so what
        // follows keeps its line numbers.
        if script[copied..].starts_with(';') {
            copied += 1;
            if !single {
                out.push(';');
            }
        }
        out.extend(std::iter::repeat_n('\n', statement.matches('\n').count()));
    }
    out.push_str(&dash_comments_to_js(&script[copied..], true));
    Ok(Rewritten { script: out, database })
}

/// "SQL: message (line L, column C)" for byte `at` of `script`.
fn place(script: &str, at: usize, message: &str) -> String {
    let at = at.min(script.len());
    let before = &script[..at];
    let line = before.matches('\n').count() + 1;
    let column = before.rsplit('\n').next().map_or(0, |text| text.chars().count()) + 1;
    format!("SQL: {message} (line {line}, column {column})")
}

/// Where the `SELECT` of a statement starts, after its leading comments; `None` when it is not one.
fn sql_start(statement: &str) -> Option<usize> {
    let mut lexer = Lexer::new(statement);
    let token = lexer.next().ok()??;
    matches!(&token.kind, Tok::Word(word) if word.eq_ignore_ascii_case("select")).then_some(token.at)
}

/// Lines that start with `--` followed by a space, a dash or nothing (an SQL comment, a rule
/// `-----`) become `//` comments, which the shell reads; in a script with SQL (`with_sql`) any
/// line that starts with `--` does (`--where …`, a clause commented out). Otherwise `--x` (a
/// decrement) is left alone.
fn dash_comments_to_js(text: &str, with_sql: bool) -> String {
    let mut out = String::with_capacity(text.len());
    for (index, line) in text.split('\n').enumerate() {
        if index > 0 {
            out.push('\n');
        }
        let trimmed = line.trim_start();
        let is_comment = trimmed
            .strip_prefix("--")
            .is_some_and(|rest| with_sql || rest.is_empty() || rest.starts_with([' ', '\t', '-', '\r']));
        if is_comment {
            out.push_str(&line[..line.len() - trimmed.len()]);
            out.push_str("//");
            out.push_str(&trimmed[2..]);
        } else {
            out.push_str(line);
        }
    }
    out
}

// ---- tokens ------------------------------------------------------------------------------------

#[derive(Debug, Clone, PartialEq)]
enum Tok {
    /// A bare word: a keyword or a name.
    Word(String),
    /// `"…"` or `` `…` ``: a name, or a string on the right of a comparison.
    Quoted(String),
    /// `'…'`.
    Str(String),
    Num(String),
    Sym(&'static str),
}

#[derive(Debug, Clone)]
struct Token {
    kind: Tok,
    at: usize,
}

struct Lexer<'a> {
    text: &'a str,
    at: usize,
}

impl<'a> Lexer<'a> {
    fn new(text: &'a str) -> Self {
        Self { text, at: 0 }
    }

    fn error(&self, at: usize, message: impl Into<String>) -> SqlError {
        SqlError { at, message: message.into() }
    }

    fn next(&mut self) -> Result<Option<Token>, SqlError> {
        let bytes = self.text.as_bytes();
        loop {
            while self.at < bytes.len() && bytes[self.at].is_ascii_whitespace() {
                self.at += 1;
            }
            let rest = &self.text[self.at..];
            if rest.starts_with("--") || rest.starts_with("//") {
                self.at += rest.find('\n').unwrap_or(rest.len());
                continue;
            }
            if rest.starts_with("/*") {
                self.at += rest.find("*/").map_or(rest.len(), |end| end + 2);
                continue;
            }
            break;
        }
        let start = self.at;
        let Some(&first) = bytes.get(start) else {
            return Ok(None);
        };
        let token = |kind, at| Ok(Some(Token { kind, at }));
        match first {
            b'\'' | b'"' | b'`' => {
                let mut value = String::new();
                let mut index = start + 1;
                loop {
                    let Some(c) = self.text[index..].chars().next() else {
                        return Err(self.error(start, "unterminated quote"));
                    };
                    index += c.len_utf8();
                    if c == first as char {
                        // A doubled quote is the quote itself.
                        if self.text[index..].starts_with(first as char) {
                            value.push(c);
                            index += 1;
                            continue;
                        }
                        break;
                    }
                    if c == '\\' && first != b'`' {
                        if let Some(escaped) = self.text[index..].chars().next() {
                            index += escaped.len_utf8();
                            value.push(escaped);
                            continue;
                        }
                    }
                    value.push(c);
                }
                self.at = index;
                let kind = if first == b'\'' { Tok::Str(value) } else { Tok::Quoted(value) };
                token(kind, start)
            }
            b'0'..=b'9' => {
                let mut end = start;
                while end < bytes.len() && (bytes[end].is_ascii_digit() || bytes[end] == b'.') {
                    end += 1;
                }
                if end < bytes.len() && matches!(bytes[end], b'e' | b'E') {
                    end += 1;
                    if end < bytes.len() && matches!(bytes[end], b'+' | b'-') {
                        end += 1;
                    }
                    while end < bytes.len() && bytes[end].is_ascii_digit() {
                        end += 1;
                    }
                }
                self.at = end;
                let number = &self.text[start..end];
                if number.parse::<f64>().is_err() {
                    return Err(self.error(start, format!("not a number: {number}")));
                }
                token(Tok::Num(number.to_owned()), start)
            }
            c if c.is_ascii_alphabetic() || c == b'_' || c == b'$' || c >= 0x80 => {
                let mut end = start;
                while let Some(c) = self.text[end..].chars().next() {
                    if c.is_alphanumeric() || c == '_' || c == '$' {
                        end += c.len_utf8();
                    } else {
                        break;
                    }
                }
                self.at = end;
                token(Tok::Word(self.text[start..end].to_owned()), start)
            }
            _ => {
                const SYMBOLS: [&str; 15] = ["<=", ">=", "<>", "!=", "==", "=", "<", ">", "(", ")", ",", ".", "*", ";", "-"];
                let Some(symbol) = SYMBOLS.iter().find(|symbol| rest_starts(self.text, start, symbol)) else {
                    let c = self.text[start..].chars().next().unwrap_or(' ');
                    return Err(self.error(start, format!("unexpected '{c}'")));
                };
                self.at = start + symbol.len();
                token(Tok::Sym(symbol), start)
            }
        }
    }
}

fn rest_starts(text: &str, at: usize, prefix: &str) -> bool {
    text[at..].starts_with(prefix)
}

// ---- the statement -----------------------------------------------------------------------------

#[derive(Debug, Clone, Copy, PartialEq)]
enum Aggregate {
    Count,
    Sum,
    Avg,
    Min,
    Max,
}

#[derive(Debug, Clone, PartialEq)]
enum Item {
    Field { path: String, alias: Option<String> },
    /// `COUNT(*)` has no field.
    Aggregate { function: Aggregate, field: Option<String>, alias: Option<String> },
}

#[derive(Debug)]
struct Select {
    database: Option<String>,
    collection: String,
    distinct: bool,
    /// Empty: `*`.
    items: Vec<Item>,
    filter: Option<J>,
    group: Vec<String>,
    order: Vec<(String, bool)>,
    limit: Option<u64>,
    offset: Option<u64>,
}

/// A value of the shell statement.
#[derive(Debug, Clone, PartialEq)]
enum J {
    Str(String),
    Num(String),
    Bool(bool),
    Null,
    ObjectId(String),
    /// A shell expression written as the value (`new Date()`, `ISODate('…')`), as written.
    Raw(String),
    Arr(Vec<J>),
    Obj(Vec<(String, J)>),
}

impl J {
    fn obj(pairs: impl IntoIterator<Item = (String, J)>) -> Self {
        Self::Obj(pairs.into_iter().collect())
    }

    fn one(key: &str, value: J) -> Self {
        Self::Obj(vec![(key.to_owned(), value)])
    }

    fn write(&self, out: &mut String) {
        match self {
            Self::Str(text) => out.push_str(&json(text)),
            Self::Num(number) => out.push_str(number),
            Self::Bool(value) => out.push_str(if *value { "true" } else { "false" }),
            Self::Null => out.push_str("null"),
            Self::ObjectId(hex) => {
                let _ = write!(out, "ObjectId({})", json(hex));
            }
            Self::Raw(text) => out.push_str(text),
            Self::Arr(items) => {
                out.push('[');
                for (index, item) in items.iter().enumerate() {
                    if index > 0 {
                        out.push_str(", ");
                    }
                    item.write(out);
                }
                out.push(']');
            }
            Self::Obj(pairs) => {
                out.push('{');
                for (index, (key, value)) in pairs.iter().enumerate() {
                    if index > 0 {
                        out.push_str(", ");
                    }
                    out.push_str(&json(key));
                    out.push_str(": ");
                    value.write(out);
                }
                out.push('}');
            }
        }
    }

    fn text(&self) -> String {
        let mut out = String::new();
        self.write(&mut out);
        out
    }
}

fn json(text: &str) -> String {
    serde_json::Value::String(text.to_owned()).to_string()
}

struct Parser<'a> {
    tokens: Vec<Token>,
    index: usize,
    end: usize,
    text: &'a str,
}

fn parse(statement: &str) -> Result<Select, SqlError> {
    let mut lexer = Lexer::new(statement);
    let mut tokens = Vec::new();
    while let Some(token) = lexer.next()? {
        tokens.push(token);
    }
    // A trailing `;` is the statement's end.
    while tokens.last().is_some_and(|token| token.kind == Tok::Sym(";")) {
        tokens.pop();
    }
    let mut parser = Parser { tokens, index: 0, end: statement.trim_end().len(), text: statement };
    let select = parser.select()?;
    if let Some(token) = parser.peek() {
        return Err(parser.unexpected(token));
    }
    Ok(select)
}

impl Parser<'_> {
    fn peek(&self) -> Option<&Token> {
        self.tokens.get(self.index)
    }

    fn at(&self) -> usize {
        self.peek().map_or(self.end, |token| token.at)
    }

    fn bump(&mut self) -> Option<Token> {
        let token = self.tokens.get(self.index).cloned();
        self.index += usize::from(token.is_some());
        token
    }

    fn unexpected(&self, token: &Token) -> SqlError {
        let what = match &token.kind {
            Tok::Word(word) => word.clone(),
            Tok::Quoted(text) => format!("\"{text}\""),
            Tok::Str(text) => format!("'{text}'"),
            Tok::Num(number) => number.clone(),
            Tok::Sym(symbol) => (*symbol).to_owned(),
        };
        SqlError { at: token.at, message: format!("unexpected {what}") }
    }

    fn error(&self, message: impl Into<String>) -> SqlError {
        SqlError { at: self.at(), message: message.into() }
    }

    fn is_word(&self, word: &str) -> bool {
        matches!(self.peek(), Some(Token { kind: Tok::Word(found), .. }) if found.eq_ignore_ascii_case(word))
    }

    fn is_sym(&self, symbol: &str) -> bool {
        matches!(self.peek(), Some(Token { kind: Tok::Sym(found), .. }) if *found == symbol)
    }

    fn eat_word(&mut self, word: &str) -> bool {
        let found = self.is_word(word);
        self.index += usize::from(found);
        found
    }

    fn eat_sym(&mut self, symbol: &str) -> bool {
        let found = self.is_sym(symbol);
        self.index += usize::from(found);
        found
    }

    fn expect_word(&mut self, word: &str) -> Result<(), SqlError> {
        if self.eat_word(word) {
            Ok(())
        } else {
            Err(self.error(format!("expected {}", word.to_uppercase())))
        }
    }

    fn expect_sym(&mut self, symbol: &str) -> Result<(), SqlError> {
        if self.eat_sym(symbol) {
            Ok(())
        } else {
            Err(self.error(format!("expected '{symbol}'")))
        }
    }

    fn select(&mut self) -> Result<Select, SqlError> {
        self.expect_word("select")?;
        let distinct = self.eat_word("distinct");
        let items = if self.eat_sym("*") {
            Vec::new()
        } else {
            let mut items = vec![self.item()?];
            while self.eat_sym(",") {
                items.push(self.item()?);
            }
            items
        };
        self.expect_word("from")?;
        let (database, collection) = self.source()?;
        let filter = if self.eat_word("where") { Some(self.or()?) } else { None };
        let mut group = Vec::new();
        if self.eat_word("group") {
            self.expect_word("by")?;
            group.push(self.field()?);
            while self.eat_sym(",") {
                group.push(self.field()?);
            }
        }
        let mut order = Vec::new();
        if self.eat_word("order") {
            self.expect_word("by")?;
            loop {
                let field = self.field()?;
                let descending = if self.eat_word("desc") {
                    true
                } else {
                    self.eat_word("asc");
                    false
                };
                order.push((field, descending));
                if !self.eat_sym(",") {
                    break;
                }
            }
        }
        let (mut limit, mut offset) = (None, None);
        if self.eat_word("limit") {
            limit = Some(self.count()?);
            if self.eat_word("offset") {
                offset = Some(self.count()?);
            }
        }
        if offset.is_none() && self.eat_word("offset") {
            offset = Some(self.count()?);
        }
        Ok(Select { database, collection, distinct, items, filter, group, order, limit, offset })
    }

    fn count(&mut self) -> Result<u64, SqlError> {
        match self.bump() {
            Some(Token { kind: Tok::Num(number), .. }) if number.parse::<u64>().is_ok() => Ok(number.parse().unwrap_or(0)),
            Some(token) => Err(SqlError { at: token.at, message: "expected a whole number".into() }),
            None => Err(self.error("expected a whole number")),
        }
    }

    fn item(&mut self) -> Result<Item, SqlError> {
        let function = match self.peek() {
            Some(Token { kind: Tok::Word(word), .. })
                if self.tokens.get(self.index + 1).is_some_and(|next| next.kind == Tok::Sym("(")) =>
            {
                match word.to_ascii_lowercase().as_str() {
                    "count" => Some(Aggregate::Count),
                    "sum" => Some(Aggregate::Sum),
                    "avg" => Some(Aggregate::Avg),
                    "min" => Some(Aggregate::Min),
                    "max" => Some(Aggregate::Max),
                    other => return Err(self.error(format!("the function {other} is not supported"))),
                }
            }
            _ => None,
        };
        let item = match function {
            Some(function) => {
                self.index += 2;
                let field = if function == Aggregate::Count && self.eat_sym("*") { None } else { Some(self.field()?) };
                if field.is_none() && function != Aggregate::Count {
                    return Err(self.error("expected a field"));
                }
                self.expect_sym(")")?;
                Item::Aggregate { function, field, alias: self.alias()? }
            }
            None => {
                let path = self.field()?;
                Item::Field { path, alias: self.alias()? }
            }
        };
        Ok(item)
    }

    fn alias(&mut self) -> Result<Option<String>, SqlError> {
        if self.eat_word("as") {
            return self.name().map(Some);
        }
        // A bare name right after an item is its alias, unless it is the next clause.
        match self.peek() {
            Some(Token { kind: Tok::Word(word), .. }) if !is_keyword(word) => self.name().map(Some),
            Some(Token { kind: Tok::Quoted(_), .. }) => self.name().map(Some),
            _ => Ok(None),
        }
    }

    fn name(&mut self) -> Result<String, SqlError> {
        match self.bump() {
            Some(Token { kind: Tok::Word(word) | Tok::Quoted(word), .. }) => Ok(word),
            Some(token) => Err(self.unexpected(&token)),
            None => Err(self.error("expected a name")),
        }
    }

    /// `name`, `a.b.c`, `"a.b"`.
    fn field(&mut self) -> Result<String, SqlError> {
        let mut path = self.name()?;
        while self.is_sym(".") {
            self.index += 1;
            path.push('.');
            path.push_str(&self.name()?);
        }
        Ok(path)
    }

    /// `collection`, `database.collection`, `"database"."collection"`.
    fn source(&mut self) -> Result<(Option<String>, String), SqlError> {
        let first = self.name()?;
        if self.eat_sym(".") {
            let collection = self.name()?;
            // A third part belongs to the collection name (`db.system.users`).
            let mut collection = collection;
            while self.eat_sym(".") {
                collection.push('.');
                collection.push_str(&self.name()?);
            }
            return Ok((Some(first), collection));
        }
        Ok((None, first))
    }

    fn or(&mut self) -> Result<J, SqlError> {
        let mut parts = vec![self.and()?];
        while self.eat_word("or") {
            parts.push(self.and()?);
        }
        Ok(if parts.len() == 1 { parts.remove(0) } else { J::one("$or", J::Arr(parts)) })
    }

    fn and(&mut self) -> Result<J, SqlError> {
        let mut parts = vec![self.not()?];
        while self.eat_word("and") {
            parts.push(self.not()?);
        }
        Ok(merge_and(parts))
    }

    fn not(&mut self) -> Result<J, SqlError> {
        if self.eat_word("not") {
            let inner = self.not()?;
            return Ok(J::one("$nor", J::Arr(vec![inner])));
        }
        if self.eat_sym("(") {
            let inner = self.or()?;
            self.expect_sym(")")?;
            return Ok(inner);
        }
        self.predicate()
    }

    fn predicate(&mut self) -> Result<J, SqlError> {
        let field = self.field()?;
        let id = field == "_id";
        if self.eat_word("is") {
            let negated = self.eat_word("not");
            self.expect_word("null")?;
            return Ok(if negated { J::one(&field, J::one("$ne", J::Null)) } else { J::one(&field, J::Null) });
        }
        let negated = self.eat_word("not");
        if self.eat_word("in") {
            self.expect_sym("(")?;
            let mut values = vec![self.value(id)?];
            while self.eat_sym(",") {
                values.push(self.value(id)?);
            }
            self.expect_sym(")")?;
            let operator = if negated { "$nin" } else { "$in" };
            return Ok(J::one(&field, J::one(operator, J::Arr(values))));
        }
        let like = self.is_word("like");
        if like || self.is_word("ilike") {
            self.index += 1;
            let pattern = match self.bump() {
                Some(Token { kind: Tok::Str(text) | Tok::Quoted(text), .. }) => text,
                Some(token) => return Err(SqlError { at: token.at, message: "expected a pattern string".into() }),
                None => return Err(self.error("expected a pattern string")),
            };
            let mut regex = vec![("$regex".to_owned(), J::Str(like_to_regex(&pattern)))];
            if !like {
                regex.push(("$options".to_owned(), J::Str("i".into())));
            }
            let regex = J::Obj(regex);
            return Ok(J::one(&field, if negated { J::one("$not", regex) } else { regex }));
        }
        if self.eat_word("between") {
            let low = self.value(id)?;
            self.expect_word("and")?;
            let high = self.value(id)?;
            return Ok(if negated {
                J::one(
                    "$or",
                    J::Arr(vec![J::one(&field, J::one("$lt", low)), J::one(&field, J::one("$gt", high))]),
                )
            } else {
                J::one(&field, J::obj([("$gte".to_owned(), low), ("$lte".to_owned(), high)]))
            });
        }
        if negated {
            return Err(self.error("expected IN, LIKE or BETWEEN after NOT"));
        }
        let operator = match self.bump() {
            Some(Token { kind: Tok::Sym(symbol), at }) => match symbol {
                "=" | "==" => None,
                "!=" | "<>" => Some("$ne"),
                "<" => Some("$lt"),
                "<=" => Some("$lte"),
                ">" => Some("$gt"),
                ">=" => Some("$gte"),
                _ => return Err(SqlError { at, message: format!("unexpected {symbol}") }),
            },
            Some(token) => return Err(self.unexpected(&token)),
            None => return Err(self.error("expected a comparison")),
        };
        let value = self.value(id)?;
        Ok(match operator {
            None => J::one(&field, value),
            Some(operator) => J::one(&field, J::one(operator, value)),
        })
    }

    /// A literal; `"…"` is a string here (some tools quote strings that way). An `_id` value of
    /// 24 hex digits is an ObjectId.
    fn value(&mut self, id: bool) -> Result<J, SqlError> {
        if let Some(raw) = self.shell_expression()? {
            return Ok(J::Raw(raw));
        }
        let negative = self.eat_sym("-");
        match self.bump() {
            Some(Token { kind: Tok::Str(text) | Tok::Quoted(text), .. }) if !negative => {
                Ok(if id && text.len() == 24 && text.bytes().all(|b| b.is_ascii_hexdigit()) {
                    J::ObjectId(text)
                } else {
                    J::Str(text)
                })
            }
            Some(Token { kind: Tok::Num(number), .. }) => {
                Ok(J::Num(if negative { format!("-{number}") } else { number }))
            }
            Some(Token { kind: Tok::Word(word), at }) if !negative => match word.to_ascii_lowercase().as_str() {
                "true" => Ok(J::Bool(true)),
                "false" => Ok(J::Bool(false)),
                "null" => Ok(J::Null),
                _ => Err(SqlError {
                    at,
                    message: format!("expected a value, found {word} (quote strings: '{word}')"),
                }),
            },
            Some(token) => Err(self.unexpected(&token)),
            None => Err(self.error("expected a value")),
        }
    }
}

impl Parser<'_> {
    /// `new Name(…)` or `Name(…)` (`ISODate('…')`, `ObjectId('…')`), with dotted names inside
    /// (`Date.now()`): the shell expression's text, up to its closing parenthesis.
    fn shell_expression(&mut self) -> Result<Option<String>, SqlError> {
        let start_index = self.index;
        let Some(Token { kind: Tok::Word(word), at: start }) = self.peek().cloned() else {
            return Ok(None);
        };
        let constructor = word.eq_ignore_ascii_case("new");
        let call = self.tokens.get(self.index + 1).is_some_and(|next| next.kind == Tok::Sym("("));
        if !constructor && !call {
            return Ok(None);
        }
        self.index += 1;
        if constructor {
            self.field()?;
        }
        self.expect_sym("(")?;
        let mut depth = 1;
        while depth > 0 {
            match self.bump() {
                Some(Token { kind: Tok::Sym("("), .. }) => depth += 1,
                Some(Token { kind: Tok::Sym(")"), at }) => {
                    depth -= 1;
                    if depth == 0 {
                        return Ok(Some(self.text[start..=at].to_owned()));
                    }
                }
                Some(_) => {}
                None => {
                    self.index = start_index;
                    return Err(self.error("expected ')'"));
                }
            }
        }
        Ok(None)
    }
}

fn is_keyword(word: &str) -> bool {
    matches!(
        word.to_ascii_lowercase().as_str(),
        "from" | "where" | "group" | "order" | "limit" | "offset" | "and" | "or" | "not" | "as"
    )
}

/// AND of conditions: one document when their keys differ and none is an operator, else `$and`.
fn merge_and(mut parts: Vec<J>) -> J {
    if parts.len() == 1 {
        return parts.remove(0);
    }
    let mut keys = std::collections::HashSet::new();
    let mergeable = parts.iter().all(|part| match part {
        J::Obj(pairs) => pairs.iter().all(|(key, _)| !key.starts_with('$') && keys.insert(key.clone())),
        _ => false,
    });
    if mergeable {
        J::Obj(
            parts
                .into_iter()
                .flat_map(|part| match part {
                    J::Obj(pairs) => pairs,
                    _ => Vec::new(),
                })
                .collect(),
        )
    } else {
        J::one("$and", J::Arr(parts))
    }
}

/// `%` any run, `_` one character; everything else literal. Anchored.
fn like_to_regex(pattern: &str) -> String {
    let mut regex = String::from("^");
    for c in pattern.chars() {
        match c {
            '%' => regex.push_str(".*"),
            '_' => regex.push('.'),
            c if "\\^$.|?*+()[]{}/".contains(c) => {
                regex.push('\\');
                regex.push(c);
            }
            c => regex.push(c),
        }
    }
    regex.push('$');
    regex
}

impl Select {
    /// The shell statement: `db["c"].find(…)` (the fast path reads it), or with `sibling` the
    /// collection of the named database, for a statement among others.
    fn to_shell(&self, sibling: bool) -> String {
        let collection = match (&self.database, sibling) {
            (Some(database), true) => format!("db.getSiblingDB({}).getCollection({})", json(database), json(&self.collection)),
            _ => format!("db[{}]", json(&self.collection)),
        };
        let aggregated = self.distinct
            || !self.group.is_empty()
            || self.items.iter().any(|item| match item {
                Item::Aggregate { .. } => true,
                Item::Field { alias, .. } => alias.is_some(),
            });
        if aggregated {
            return format!("{collection}.aggregate({})", J::Arr(self.pipeline()).text());
        }
        let filter = self.filter.clone().unwrap_or(J::Obj(Vec::new()));
        let mut out = format!("{collection}.find({}", filter.text());
        if !self.items.is_empty() {
            let mut projection: Vec<(String, J)> = self
                .items
                .iter()
                .filter_map(|item| match item {
                    Item::Field { path, .. } => Some((path.clone(), J::Num("1".into()))),
                    Item::Aggregate { .. } => None,
                })
                .collect();
            if !projection.iter().any(|(path, _)| path == "_id") {
                projection.push(("_id".into(), J::Num("0".into())));
            }
            out.push_str(", ");
            out.push_str(&J::Obj(projection).text());
        }
        out.push(')');
        if !self.order.is_empty() {
            let _ = write!(out, ".sort({})", self.sort(|path| path.to_owned()).text());
        }
        if let Some(offset) = self.offset {
            let _ = write!(out, ".skip({offset})");
        }
        if let Some(limit) = self.limit {
            let _ = write!(out, ".limit({limit})");
        }
        out
    }

    fn sort(&self, name: impl Fn(&str) -> String) -> J {
        J::Obj(
            self.order
                .iter()
                .map(|(path, descending)| (name(path), J::Num(if *descending { "-1" } else { "1" }.into())))
                .collect(),
        )
    }

    fn pipeline(&self) -> Vec<J> {
        let mut stages = Vec::new();
        if let Some(filter) = &self.filter {
            stages.push(J::one("$match", filter.clone()));
        }
        let grouping = self.distinct || !self.group.is_empty() || self.items.iter().any(|item| matches!(item, Item::Aggregate { .. }));
        if !grouping {
            // Only aliases: sort on the source fields, then rename.
            if !self.order.is_empty() {
                let source = |name: &str| {
                    self.items
                        .iter()
                        .find_map(|item| match item {
                            Item::Field { path, alias: Some(alias) } if alias == name => Some(path.clone()),
                            _ => None,
                        })
                        .unwrap_or_else(|| name.to_owned())
                };
                stages.push(J::one("$sort", self.sort(source)));
            }
            self.page(&mut stages);
            let mut project = vec![("_id".to_owned(), J::Num("0".into()))];
            for item in &self.items {
                if let Item::Field { path, alias } = item {
                    let name = alias.clone().unwrap_or_else(|| path.clone());
                    if name == "_id" {
                        project.retain(|(key, _)| key != "_id");
                    }
                    project.push((name, J::Str(format!("${path}"))));
                }
            }
            stages.push(J::one("$project", J::Obj(project)));
            return stages;
        }
        // DISTINCT a, b is GROUP BY a, b.
        let keys: Vec<String> = if self.group.is_empty() && self.distinct {
            self.items
                .iter()
                .filter_map(|item| match item {
                    Item::Field { path, .. } => Some(path.clone()),
                    Item::Aggregate { .. } => None,
                })
                .collect()
        } else {
            self.group.clone()
        };
        let id = match keys.as_slice() {
            [] => J::Null,
            [only] => J::Str(format!("${only}")),
            many => J::Obj(many.iter().enumerate().map(|(index, path)| (format!("k{index}"), J::Str(format!("${path}")))).collect()),
        };
        let key_ref = |path: &str| -> Option<String> {
            let index = keys.iter().position(|key| key == path)?;
            Some(if keys.len() == 1 { "$_id".to_owned() } else { format!("$_id.k{index}") })
        };
        let mut group = vec![("_id".to_owned(), id)];
        let mut project = vec![("_id".to_owned(), J::Num("0".into()))];
        let items: Vec<Item> = if self.items.is_empty() {
            keys.iter().map(|path| Item::Field { path: path.clone(), alias: None }).collect()
        } else {
            self.items.clone()
        };
        for (index, item) in items.iter().enumerate() {
            match item {
                Item::Field { path, alias } => {
                    let name = alias.clone().unwrap_or_else(|| path.clone());
                    // A field that is not a key: its first value in the group.
                    let value = key_ref(path).unwrap_or_else(|| {
                        let slot = format!("f{index}");
                        group.push((slot.clone(), J::one("$first", J::Str(format!("${path}")))));
                        format!("${slot}")
                    });
                    project.push((name, J::Str(value)));
                }
                Item::Aggregate { function, field, alias } => {
                    let slot = format!("a{index}");
                    let operand = field.as_ref().map(|path| J::Str(format!("${path}")));
                    let accumulator = match (function, operand) {
                        (Aggregate::Count, None) => J::one("$sum", J::Num("1".into())),
                        (Aggregate::Count, Some(operand)) => J::one(
                            "$sum",
                            J::one(
                                "$cond",
                                J::Arr(vec![
                                    J::one("$eq", J::Arr(vec![J::one("$ifNull", J::Arr(vec![operand, J::Null])), J::Null])),
                                    J::Num("0".into()),
                                    J::Num("1".into()),
                                ]),
                            ),
                        ),
                        (Aggregate::Sum, Some(operand)) => J::one("$sum", operand),
                        (Aggregate::Avg, Some(operand)) => J::one("$avg", operand),
                        (Aggregate::Min, Some(operand)) => J::one("$min", operand),
                        (Aggregate::Max, Some(operand)) => J::one("$max", operand),
                        (_, None) => J::one("$sum", J::Num("1".into())),
                    };
                    group.push((slot.clone(), accumulator));
                    let name = alias.clone().unwrap_or_else(|| default_name(*function, field.as_deref()));
                    project.push((name, J::Str(format!("${slot}"))));
                }
            }
        }
        stages.push(J::one("$group", J::Obj(group)));
        stages.push(J::one("$project", J::Obj(project)));
        if !self.order.is_empty() {
            stages.push(J::one("$sort", self.sort(|name| name.to_owned())));
        }
        self.page(&mut stages);
        stages
    }

    fn page(&self, stages: &mut Vec<J>) {
        if let Some(offset) = self.offset {
            stages.push(J::one("$skip", J::Num(offset.to_string())));
        }
        if let Some(limit) = self.limit {
            stages.push(J::one("$limit", J::Num(limit.to_string())));
        }
    }
}

fn default_name(function: Aggregate, field: Option<&str>) -> String {
    let name = match function {
        Aggregate::Count => "count",
        Aggregate::Sum => "sum",
        Aggregate::Avg => "avg",
        Aggregate::Min => "min",
        Aggregate::Max => "max",
    };
    match field {
        Some(field) => format!("{name}_{}", field.replace('.', "_")),
        None => name.to_owned(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn one(sql: &str) -> String {
        rewrite_script(sql).unwrap().script.trim_end().to_owned()
    }

    #[test]
    fn a_select_with_where_order_and_limit_is_a_find() {
        assert_eq!(
            one("select * from \"app\".\"orders\"\n  where ref = 'A1' or parent = 'B2'\n  order by updatedAt desc limit 5;"),
            r#"db["orders"].find({"$or": [{"ref": "A1"}, {"parent": "B2"}]}).sort({"updatedAt": -1}).limit(5)"#
        );
        let rewritten = rewrite_script("select * from \"app\".\"orders\" where ref IN ('a', 'b', '');").unwrap();
        assert_eq!(rewritten.database.as_deref(), Some("app"), "the tab moves to the named database");
        assert_eq!(rewritten.script, r#"db["orders"].find({"ref": {"$in": ["a", "b", ""]}})"#);
    }

    #[test]
    fn conditions_translate_to_query_operators() {
        assert_eq!(
            one("select * from c where (src = \"x\" or src = 'y') and status != 'DONE'"),
            r#"db["c"].find({"$and": [{"$or": [{"src": "x"}, {"src": "y"}]}, {"status": {"$ne": "DONE"}}]})"#
        );
        assert_eq!(
            one("select * from c where a = 1 and b >= -2.5 and c is not null and d is null"),
            r#"db["c"].find({"a": 1, "b": {"$gte": -2.5}, "c": {"$ne": null}, "d": null})"#
        );
        assert_eq!(
            one("select * from c where name like 'Ab_%.x' and code not in (1, 2) and n between 1 and 9"),
            r#"db["c"].find({"name": {"$regex": "^Ab..*\\.x$"}, "code": {"$nin": [1, 2]}, "n": {"$gte": 1, "$lte": 9}})"#
        );
        assert_eq!(
            one("select * from c where not (a = true) and \"info.ref\" ilike '%q%'"),
            r#"db["c"].find({"$and": [{"$nor": [{"a": true}]}, {"info.ref": {"$regex": "^.*q.*$", "$options": "i"}}]})"#
        );
        assert_eq!(
            one("select * from c where _id = '507f1f77bcf86cd799439011'"),
            r#"db["c"].find({"_id": ObjectId("507f1f77bcf86cd799439011")})"#
        );
        assert_eq!(
            one("select * from c where at >= new Date(Date.now() - 1) and d < ISODate('2026-01-01')"),
            r#"db["c"].find({"at": {"$gte": new Date(Date.now() - 1)}, "d": {"$lt": ISODate('2026-01-01')}})"#
        );
    }

    #[test]
    fn selected_fields_are_a_projection_and_paging_is_skip_and_limit() {
        assert_eq!(
            one("SELECT ref, info.amount FROM c ORDER BY ref, n DESC LIMIT 10 OFFSET 20"),
            r#"db["c"].find({}, {"ref": 1, "info.amount": 1, "_id": 0}).sort({"ref": 1, "n": -1}).skip(20).limit(10)"#
        );
    }

    #[test]
    fn group_by_distinct_and_aggregates_are_a_pipeline() {
        assert_eq!(
            one("select type from c group by type;"),
            r#"db["c"].aggregate([{"$group": {"_id": "$type"}}, {"$project": {"_id": 0, "type": "$_id"}}])"#
        );
        assert_eq!(
            one("select state, count(*) as n from c where a = 1 group by state order by n desc"),
            r#"db["c"].aggregate([{"$match": {"a": 1}}, {"$group": {"_id": "$state", "a1": {"$sum": 1}}}, {"$project": {"_id": 0, "state": "$_id", "n": "$a1"}}, {"$sort": {"n": -1}}])"#
        );
        assert_eq!(
            one("select count(*) from c where a = 1"),
            r#"db["c"].aggregate([{"$match": {"a": 1}}, {"$group": {"_id": null, "a0": {"$sum": 1}}}, {"$project": {"_id": 0, "count": "$a0"}}])"#
        );
        assert_eq!(
            one("select distinct a, b from c"),
            r#"db["c"].aggregate([{"$group": {"_id": {"k0": "$a", "k1": "$b"}}}, {"$project": {"_id": 0, "a": "$_id.k0", "b": "$_id.k1"}}])"#
        );
        assert_eq!(
            one("select ref as r from c order by r limit 2"),
            r#"db["c"].aggregate([{"$sort": {"ref": 1}}, {"$limit": 2}, {"$project": {"_id": 0, "r": "$ref"}}])"#
        );
    }

    #[test]
    fn a_mixed_script_keeps_its_line_numbers_and_shell_statements() {
        let script = "//note\n--where a = 'x'\nselect * from \"d\".\"c\"\n   where a = 'x';\n\n----------\ndb.c.find({\n  b: 1\n});\n";
        let rewritten = rewrite_script(script).unwrap();
        assert_eq!(rewritten.database, None, "several statements: each names its database");
        assert_eq!(rewritten.script.matches('\n').count(), script.matches('\n').count());
        let lines: Vec<&str> = rewritten.script.split('\n').collect();
        assert_eq!(lines[1], "//where a = 'x'");
        assert_eq!(lines[2], r#"db.getSiblingDB("d").getCollection("c").find({"a": "x"});"#);
        assert_eq!(lines[5], "//--------");
        assert_eq!(lines[6], "db.c.find({");
    }

    #[test]
    fn a_script_without_sql_is_unchanged_but_for_dash_comments() {
        let script = "var i = 3;\ni--;\n-- note\ndb.c.find({})";
        assert_eq!(rewrite_script(script).unwrap().script, "var i = 3;\ni--;\n// note\ndb.c.find({})");
    }

    #[test]
    fn what_is_not_supported_names_the_line_and_column() {
        let script = "db.c.find({});\n\nselect * from c\n  where a = b;";
        assert_eq!(
            rewrite_script(script).unwrap_err(),
            "SQL: expected a value, found b (quote strings: 'b') (line 4, column 13)"
        );
        assert!(rewrite_script("select a from c join d").unwrap_err().contains("unexpected join"));
        assert!(rewrite_script("select upper(a) from c").unwrap_err().contains("upper is not supported"));
        assert!(rewrite_script("select * from c where a = 'x").unwrap_err().contains("unterminated quote"));
    }
}
