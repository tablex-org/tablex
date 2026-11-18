//! SQL on MongoDB in a script tab: a `SELECT` statement is translated into the shell statement that
//! reads the same documents (`find`, or `aggregate` for GROUP BY, DISTINCT, aggregates and aliases)
//! before the script runs, as other database tools let you query MongoDB with SQL.
//!
//! The supported subset:
//!
//! ```sql
//! SELECT [DISTINCT] * | field [AS alias], COUNT(*), SUM(field), AVG(..), MIN(..), MAX(..)
//! FROM "database"."collection" | collection [alias]
//! [[INNER | LEFT [OUTER]] JOIN collection [alias] ON a.field = b.field] ...
//! [WHERE condition]   -- = != <> < <= > >=, [NOT] IN (..), [NOT] LIKE / ILIKE, IS [NOT] NULL,
//!                     -- [NOT] BETWEEN .. AND .., AND, OR, NOT, parentheses
//! [GROUP BY field, ..] [HAVING condition] [ORDER BY field [ASC|DESC], ..] [LIMIT n [OFFSET m]]
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
        }
        .map_err(|message| place(script, start, &message))?;
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
pub(crate) fn sql_start(statement: &str) -> Option<usize> {
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
    joins: Vec<Join>,
    distinct: bool,
    /// Empty: `*`.
    items: Vec<Item>,
    filter: Option<J>,
    group: Vec<String>,
    /// Field names are output names, group keys, or `@agg:<function>:<field or *>`.
    having: Option<J>,
    order: Vec<(String, bool)>,
    limit: Option<u64>,
    offset: Option<u64>,
}

/// `[INNER | LEFT [OUTER]] JOIN collection [alias] ON a = b`: a `$lookup` of the documents whose
/// `foreign` equals the joined-so-far document's `local`, put under `alias` (one per output
/// document; LEFT keeps a document that has none).
#[derive(Debug)]
struct Join {
    left: bool,
    collection: String,
    alias: String,
    local: String,
    foreign: String,
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
    /// In HAVING: a condition may compare an aggregate (`COUNT(*) > 1`).
    having: bool,
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
    let mut parser = Parser { tokens, index: 0, end: statement.trim_end().len(), text: statement, having: false };
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
        let alias = self.alias()?.unwrap_or_else(|| collection.clone());
        let mut joins = Vec::new();
        let mut raw_joins = Vec::new();
        loop {
            let at = self.at();
            let left = self.eat_word("left");
            if left {
                self.eat_word("outer");
            }
            let inner = !left && self.eat_word("inner");
            if !self.eat_word("join") {
                if left || inner {
                    return Err(self.error("expected JOIN"));
                }
                for word in ["right", "full", "cross"] {
                    if self.is_word(word) {
                        return Err(self.error(format!("{} JOIN is not supported: use INNER or LEFT JOIN", word.to_uppercase())));
                    }
                }
                break;
            }
            let (join_database, join_collection) = self.source()?;
            if join_database.as_ref().is_some_and(|name| Some(name) != database.as_ref()) {
                return Err(SqlError { at, message: "a JOIN reads a collection of the same database".into() });
            }
            let join_alias = self.alias()?.unwrap_or_else(|| join_collection.clone());
            self.expect_word("on")?;
            let on_at = self.at();
            let first = self.field()?;
            if !(self.eat_sym("=") || self.eat_sym("==")) {
                return Err(self.error("a JOIN's ON compares two fields with ="));
            }
            let second = self.field()?;
            if self.is_word("and") || self.is_word("or") {
                return Err(self.error("a JOIN's ON takes one condition"));
            }
            raw_joins.push((left, join_collection, join_alias, first, second, on_at));
        }
        let filter = if self.eat_word("where") { Some(self.or()?) } else { None };
        let mut group = Vec::new();
        if self.eat_word("group") {
            self.expect_word("by")?;
            group.push(self.field()?);
            while self.eat_sym(",") {
                group.push(self.field()?);
            }
        }
        let having = if self.eat_word("having") {
            self.having = true;
            let condition = self.or();
            self.having = false;
            Some(condition?)
        } else {
            None
        };
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
        // Field references are resolved now that every name the statement uses is known.
        let names = std::iter::once(alias.clone()).chain(raw_joins.iter().map(|join| join.2.clone())).collect::<Vec<_>>();
        if let Some(duplicate) = names.iter().enumerate().find(|(index, name)| names[..*index].contains(name)) {
            return Err(SqlError { at: 0, message: format!("{} names two collections: give one an alias", duplicate.1) });
        }
        let resolver = Resolver { base: alias.clone(), joined: names[1..].to_vec() };
        for (left, join_collection, join_alias, first, second, on_at) in raw_joins {
            let of_join = |path: &str| path.split_once('.').is_some_and(|(head, _)| head == join_alias);
            let (local, foreign) = match (of_join(&first), of_join(&second)) {
                (false, true) => (first, second),
                (true, false) => (second, first),
                _ => {
                    return Err(SqlError {
                        at: on_at,
                        message: format!("ON must compare a field of {join_alias} with a field joined before it"),
                    })
                }
            };
            let foreign = foreign.split_once('.').map(|(_, rest)| rest.to_owned()).unwrap_or(foreign);
            // Earlier joins only: a later alias is not there yet.
            let local = resolver.resolve(&local);
            if resolver.joined.iter().skip_while(|name| **name != join_alias).any(|name| local.starts_with(&format!("{name}."))) {
                return Err(SqlError { at: on_at, message: format!("ON must compare a field of {join_alias} with a field joined before it") });
            }
            joins.push(Join { left, collection: join_collection, alias: join_alias, local, foreign });
        }
        let items = items
            .into_iter()
            .map(|item| match item {
                Item::Field { path, alias } => Item::Field { path: resolver.resolve(&path), alias },
                Item::Aggregate { function, field, alias } => {
                    Item::Aggregate { function, field: field.map(|path| resolver.resolve(&path)), alias }
                }
            })
            .collect::<Vec<_>>();
        let filter = filter.map(|filter| rename_fields(&filter, &mut |path| Ok(resolver.resolve(path)))).transpose().map_err(|message| SqlError { at: 0, message })?;
        let group: Vec<String> = group.iter().map(|path| resolver.resolve(path)).collect();
        let output_names: Vec<String> = items.iter().filter_map(|item| match item {
            Item::Field { alias: Some(alias), .. } | Item::Aggregate { alias: Some(alias), .. } => Some(alias.clone()),
            _ => None,
        }).collect();
        let order = order
            .into_iter()
            .map(|(path, descending)| (if output_names.contains(&path) { path } else { resolver.resolve(&path) }, descending))
            .collect();
        let grouping = distinct || !group.is_empty() || items.iter().any(|item| matches!(item, Item::Aggregate { .. }));
        if having.is_some() && !grouping {
            return Err(SqlError { at: 0, message: "HAVING needs GROUP BY or an aggregate".into() });
        }
        Ok(Select { database, collection, joins, distinct, items, filter, group, having, order, limit, offset })
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
        let field = match self.having_aggregate()? {
            Some(reference) => reference,
            None => self.field()?,
        };
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

impl Parser<'_> {
    /// In HAVING, `COUNT(*)` / `SUM(field)` …: the reference `@agg:<function>:<field or *>`.
    fn having_aggregate(&mut self) -> Result<Option<String>, SqlError> {
        if !self.having {
            return Ok(None);
        }
        let Some(Token { kind: Tok::Word(word), .. }) = self.peek().cloned() else {
            return Ok(None);
        };
        if !self.tokens.get(self.index + 1).is_some_and(|next| next.kind == Tok::Sym("(")) {
            return Ok(None);
        }
        let Item::Aggregate { function, field, .. } = self.item()? else {
            return Err(self.error(format!("expected an aggregate, found {word}")));
        };
        Ok(Some(aggregate_reference(function, field.as_deref())))
    }
}

fn aggregate_reference(function: Aggregate, field: Option<&str>) -> String {
    format!("@agg:{function:?}:{}", field.unwrap_or("*"))
}

/// What the names of a statement with aliases or joins point at: `o.ref` (the FROM alias) is
/// the document's `ref`; `c.name` (a joined alias) stays `c.name`, the field of the document the
/// join put under `c`; anything else is a path of the document.
struct Resolver {
    base: String,
    joined: Vec<String>,
}

impl Resolver {
    fn resolve(&self, path: &str) -> String {
        match path.split_once('.') {
            Some((head, rest)) if head == self.base && !self.joined.iter().any(|name| name == head) => rest.to_owned(),
            _ => path.to_owned(),
        }
    }
}

/// `filter` with each field name replaced by `rename` (operators and values are left alone).
fn rename_fields(filter: &J, rename: &mut dyn FnMut(&str) -> Result<String, String>) -> Result<J, String> {
    let J::Obj(pairs) = filter else {
        return Ok(filter.clone());
    };
    let mut out = Vec::with_capacity(pairs.len());
    for (key, value) in pairs {
        if key.starts_with('$') {
            let value = match value {
                J::Arr(items) => J::Arr(items.iter().map(|item| rename_fields(item, rename)).collect::<Result<_, _>>()?),
                other => other.clone(),
            };
            out.push((key.clone(), value));
        } else {
            out.push((rename(key)?, value.clone()));
        }
    }
    Ok(J::Obj(out))
}

fn is_keyword(word: &str) -> bool {
    matches!(
        word.to_ascii_lowercase().as_str(),
        "from" | "where" | "group" | "order" | "limit" | "offset" | "and" | "or" | "not" | "as" | "join" | "inner"
            | "left" | "right" | "full" | "cross" | "outer" | "on" | "having"
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
    fn to_shell(&self, sibling: bool) -> Result<String, String> {
        let collection = match (&self.database, sibling) {
            (Some(database), true) => format!("db.getSiblingDB({}).getCollection({})", json(database), json(&self.collection)),
            _ => format!("db[{}]", json(&self.collection)),
        };
        let aggregated = self.distinct
            || !self.joins.is_empty()
            || !self.group.is_empty()
            || self.items.iter().any(|item| match item {
                Item::Aggregate { .. } => true,
                Item::Field { alias, .. } => alias.is_some(),
            });
        if aggregated {
            return Ok(format!("{collection}.aggregate({})", J::Arr(self.pipeline()?).text()));
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
        Ok(out)
    }

    fn sort(&self, name: impl Fn(&str) -> String) -> J {
        J::Obj(
            self.order
                .iter()
                .map(|(path, descending)| (name(path), J::Num(if *descending { "-1" } else { "1" }.into())))
                .collect(),
        )
    }

    fn pipeline(&self) -> Result<Vec<J>, String> {
        let mut stages = Vec::new();
        for join in &self.joins {
            stages.push(J::one(
                "$lookup",
                J::obj([
                    ("from".to_owned(), J::Str(join.collection.clone())),
                    ("localField".to_owned(), J::Str(join.local.clone())),
                    ("foreignField".to_owned(), J::Str(join.foreign.clone())),
                    ("as".to_owned(), J::Str(join.alias.clone())),
                ]),
            ));
            stages.push(J::one(
                "$unwind",
                J::obj([
                    ("path".to_owned(), J::Str(format!("${}", join.alias))),
                    ("preserveNullAndEmptyArrays".to_owned(), J::Bool(join.left)),
                ]),
            ));
        }
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
            if self.items.is_empty() {
                // SELECT * over joins: each document with the joined ones under their aliases.
                return Ok(stages);
            }
            let mut project = vec![("_id".to_owned(), J::Num("0".into()))];
            for (index, item) in self.items.iter().enumerate() {
                if let Item::Field { path, alias } = item {
                    let name = alias.clone().unwrap_or_else(|| self.output_name(index, path));
                    if name == "_id" {
                        project.retain(|(key, _)| key != "_id");
                    }
                    project.push((name, J::Str(format!("${path}"))));
                }
            }
            stages.push(J::one("$project", J::Obj(project)));
            return Ok(stages);
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
                    let name = alias.clone().unwrap_or_else(|| self.output_name(index, path));
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
        // HAVING reads the groups: an output name, a group key or an aggregate (one not selected
        // is computed in the group too, and not shown).
        let having = self.having.as_ref().map(|having| {
            rename_fields(having, &mut |name| {
                if let Some(rest) = name.strip_prefix("@agg:") {
                    let found = items.iter().enumerate().find_map(|(index, item)| match item {
                        Item::Aggregate { function, field, .. } if aggregate_reference(*function, field.as_deref()) == name => Some(format!("a{index}")),
                        _ => None,
                    });
                    return Ok(found.unwrap_or_else(|| {
                        let slot = format!("h{}", group.len());
                        let (function, field) = rest.split_once(':').unwrap_or((rest, "*"));
                        let operand = J::Str(format!("${field}"));
                        let accumulator = match function {
                            "Sum" => J::one("$sum", operand),
                            "Avg" => J::one("$avg", operand),
                            "Min" => J::one("$min", operand),
                            "Max" => J::one("$max", operand),
                            _ if field == "*" => J::one("$sum", J::Num("1".into())),
                            _ => J::one("$sum", J::one("$cond", J::Arr(vec![J::one("$eq", J::Arr(vec![J::one("$ifNull", J::Arr(vec![operand, J::Null])), J::Null])), J::Num("0".into()), J::Num("1".into())]))),
                        };
                        group.push((slot.clone(), accumulator));
                        slot
                    }));
                }
                // An output name (an alias, a selected field or aggregate) or a group key.
                if let Some((_, J::Str(value))) = project.iter().find(|(output, _)| output == name) {
                    return Ok(value.trim_start_matches('$').to_owned());
                }
                if let Some(key) = key_ref(name) {
                    return Ok(key.trim_start_matches('$').to_owned());
                }
                Err(format!("HAVING names {name}, which is neither selected, grouped nor aggregated"))
            })
        });
        let having = having.transpose()?;
        stages.push(J::one("$group", J::Obj(group)));
        if let Some(having) = having {
            stages.push(J::one("$match", having));
        }
        stages.push(J::one("$project", J::Obj(project)));
        if !self.order.is_empty() {
            stages.push(J::one("$sort", self.sort(|name| name.to_owned())));
        }
        self.page(&mut stages);
        Ok(stages)
    }

    /// The column of a selected field: as written for the collection's own fields; a joined one
    /// is its last name (`c.name`: `name`), or `c_name` when another column has that name.
    fn output_name(&self, index: usize, path: &str) -> String {
        let Some((alias, rest)) = path.split_once('.').filter(|(head, _)| self.joins.iter().any(|join| join.alias == *head)) else {
            return path.to_owned();
        };
        let last = rest.rsplit('.').next().unwrap_or(rest);
        let taken = self.items.iter().enumerate().any(|(other, item)| {
            other != index
                && match item {
                    Item::Field { path, alias } => alias.as_deref().unwrap_or(path.rsplit('.').next().unwrap_or(path)) == last,
                    Item::Aggregate { alias, .. } => alias.as_deref() == Some(last),
                }
        });
        if taken {
            format!("{alias}_{}", rest.replace('.', "_"))
        } else {
            last.to_owned()
        }
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
    fn joins_are_lookups_under_their_aliases() {
        assert_eq!(
            one("select o.ref, c.name, o.total from orders o join customers c on o.customerId = c._id where c.country = 'VN' order by o.total desc limit 5"),
            concat!(
                r#"db["orders"].aggregate([{"$lookup": {"from": "customers", "localField": "customerId", "foreignField": "_id", "as": "c"}}, "#,
                r#"{"$unwind": {"path": "$c", "preserveNullAndEmptyArrays": false}}, {"$match": {"c.country": "VN"}}, "#,
                r#"{"$sort": {"total": -1}}, {"$limit": 5}, {"$project": {"_id": 0, "ref": "$ref", "name": "$c.name", "total": "$total"}}])"#
            )
        );
        // LEFT keeps the documents without a match; two joins, the second on the first; SELECT *.
        assert_eq!(
            one("SELECT * FROM \"shop\".\"orders\" LEFT OUTER JOIN customers AS c ON c._id = orders.customerId INNER JOIN regions r ON r.code = c.region"),
            concat!(
                r#"db["orders"].aggregate([{"$lookup": {"from": "customers", "localField": "customerId", "foreignField": "_id", "as": "c"}}, "#,
                r#"{"$unwind": {"path": "$c", "preserveNullAndEmptyArrays": true}}, "#,
                r#"{"$lookup": {"from": "regions", "localField": "c.region", "foreignField": "code", "as": "r"}}, "#,
                r#"{"$unwind": {"path": "$r", "preserveNullAndEmptyArrays": false}}])"#
            )
        );
        // The same last name twice: the joined one is prefixed with its alias.
        assert_eq!(
            one("select o.name, c.name from orders o join customers c on o.cid = c.id"),
            concat!(
                r#"db["orders"].aggregate([{"$lookup": {"from": "customers", "localField": "cid", "foreignField": "id", "as": "c"}}, "#,
                r#"{"$unwind": {"path": "$c", "preserveNullAndEmptyArrays": false}}, {"$project": {"_id": 0, "name": "$name", "c_name": "$c.name"}}])"#
            )
        );
    }

    #[test]
    fn having_filters_the_groups() {
        assert_eq!(
            one("select status, count(*) as n from orders group by status having n > 1 and status <> 'done'"),
            concat!(
                r#"db["orders"].aggregate([{"$group": {"_id": "$status", "a1": {"$sum": 1}}}, {"$match": {"a1": {"$gt": 1}, "_id": {"$ne": "done"}}}, "#,
                r#"{"$project": {"_id": 0, "status": "$_id", "n": "$a1"}}])"#
            )
        );
        // An aggregate written in HAVING: the selected one, or one computed for it alone.
        assert_eq!(
            one("select status, count(*) from orders group by status having count(*) >= 2 and sum(total) > 100"),
            concat!(
                r#"db["orders"].aggregate([{"$group": {"_id": "$status", "a1": {"$sum": 1}, "h2": {"$sum": "$total"}}}, "#,
                r#"{"$match": {"a1": {"$gte": 2}, "h2": {"$gt": 100}}}, {"$project": {"_id": 0, "status": "$_id", "count": "$a1"}}])"#
            )
        );
    }

    #[test]
    fn what_joins_and_having_cannot_do_is_said() {
        let error = |sql: &str| rewrite_script(sql).unwrap_err();
        assert!(error("select * from a right join b on a.x = b.y").contains("RIGHT JOIN is not supported"));
        assert!(error("select * from a join b on a.x = b.y and a.z = b.w").contains("one condition"));
        assert!(error("select * from a join b on a.x > b.y").contains("compares two fields with ="));
        assert!(error("select * from a join b on a.x = a.y").contains("ON must compare a field of b"));
        assert!(error("select * from \"d1\".\"a\" join \"d2\".\"b\" on a.x = b.y").contains("same database"));
        assert!(error("select * from a having x > 1").contains("HAVING needs GROUP BY"));
        assert!(error("select s, count(*) from a group by s having other > 1").contains("HAVING names other"));
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
        assert!(rewrite_script("select a from c join d").unwrap_err().contains("expected ON"));
        assert!(rewrite_script("select upper(a) from c").unwrap_err().contains("upper is not supported"));
        assert!(rewrite_script("select * from c where a = 'x").unwrap_err().contains("unterminated quote"));
    }
}
