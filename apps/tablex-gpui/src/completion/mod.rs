//! Database-agnostic editor autocomplete, pure Rust with no GPUI.
//!
//! [`complete`] takes the editor text around the cursor, what kind of editor
//! it is (SQL dialect family or Mongo role) and a read-only [`Catalog`] of
//! schema metadata that is already cached. It finds the syntactic context at
//! the cursor with a small forgiving lexer, gathers candidates from `&'static`
//! vocabulary tables plus the catalog, ranks them (prefix > word prefix >
//! subsequence, case-insensitive) and returns at most [`MAX_ITEMS`] items.
//! It never performs I/O; the GPUI adapter lives in `app::completion`.

#![cfg_attr(not(feature = "ui"), allow(dead_code))]

mod catalog;
mod mongo;
mod mongo_data;
mod rank;
mod sql;
mod sql_data;
mod sql_data_clickhouse;
mod sql_lex;
#[cfg(test)]
mod tests;

use std::borrow::Cow;
use std::ops::Range;

pub use catalog::{Catalog, Column, Relation};
#[cfg(test)]
pub use catalog::{EmptyCatalog, StaticCatalog};
use rank::{prefix_bytes, Insert, Ranked, Tier};

/// Upper bound on returned items.
pub const MAX_ITEMS: usize = 50;

/// SQL dialect family. Selects the keyword / function vocabulary and the
/// lexical rules (string and comment syntax).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum SqlFlavor {
    Postgres,
    Sqlite,
    Mysql,
    Mariadb,
    Clickhouse,
}

/// What a candidate is, for the icon / ordering the menu shows.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Kind {
    Snippet,
    Keyword,
    Function,
    Method,
    Table,
    View,
    Column,
    Schema,
    Alias,
    Operator,
    Field,
    Collection,
    Value,
}

pub struct SqlEditor<'a> {
    pub flavor: SqlFlavor,
    /// The driver's identifier quote (`"` or `` ` ``).
    pub identifier_quote: char,
    /// The text is only a `WHERE` / `ORDER BY` fragment of this table (the
    /// table-data bar): no statement structure, one implicit table in scope.
    pub fragment_of: Option<Relation<'a>>,
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum MongoRole<'a> {
    /// A JavaScript shell script (`db.orders.find({})`).
    Script,
    /// A bare filter document.
    Filter,
    /// A bare sort document.
    Sort,
    /// An aggregation pipeline array.
    Pipeline,
    /// The body of one pipeline stage, e.g. `$match`.
    Stage(&'a str),
}

pub struct MongoEditor<'a> {
    pub role: MongoRole<'a>,
    /// Collection the editor targets (tab collection), if any.
    pub collection: Option<&'a str>,
    /// Strict JSON editor: keys are inserted quoted.
    pub json: bool,
}

pub enum Editor<'a> {
    Sql(SqlEditor<'a>),
    Mongo(MongoEditor<'a>),
}

pub struct Item<'a> {
    pub label: Cow<'a, str>,
    /// Text that replaces [`Completions::replace`] when accepted.
    pub insert: Cow<'a, str>,
    /// Short description (or column type) shown next to the label.
    pub detail: Cow<'a, str>,
    pub kind: Kind,
    /// Bytes of `label` to highlight as the matched prefix (0 = none).
    pub matched: usize,
}

pub struct Completions<'a> {
    /// Byte range of `text` the accepted item replaces (the typed word).
    pub replace: Range<usize>,
    pub items: Vec<Item<'a>>,
}

impl Completions<'_> {
    fn none(cursor: usize) -> Self {
        Self {
            replace: cursor..cursor,
            items: Vec::new(),
        }
    }
}

/// Completions at byte offset `cursor` of `text`.
pub fn complete<'a>(
    editor: &Editor<'a>,
    text: &'a str,
    cursor: usize,
    catalog: &'a dyn Catalog,
) -> Completions<'a> {
    let cursor = cursor.min(text.len());
    if !text.is_char_boundary(cursor) {
        return Completions::none(cursor);
    }
    match editor {
        Editor::Sql(sql) => sql::complete(sql, text, cursor, catalog),
        Editor::Mongo(mongo) => mongo::complete(mongo, text, cursor, catalog),
    }
}

/// How ranked candidates are rendered into items.
struct Style {
    /// Identifier quote character, `None` when quoting is never needed.
    pub quote: Option<char>,
    /// Unquoted identifiers fold to lower case (PostgreSQL): anything with an
    /// upper-case letter must be quoted.
    pub folds_to_lower: bool,
    /// The cursor is already inside an opening quote.
    pub inside_quote: bool,
    /// Keywords / functions follow a lower-case query.
    pub lowercase_words: bool,
    /// Characters of the query, for the highlighted prefix.
    pub query_chars: usize,
    pub property: PropertyStyle,
}

/// How a document key is spelled when inserted.
#[derive(Clone, Copy, PartialEq, Eq)]
enum PropertyStyle {
    /// `name: ` (JavaScript object literal).
    Bare,
    /// `"name": ` (strict JSON).
    Quoted,
    /// `name`: the key's quotes are already typed.
    InsideQuotes,
}

impl Style {
    fn needs_quote(&self, name: &str) -> bool {
        let mut chars = name.chars();
        let simple_start = chars.next().is_some_and(|c| c == '_' || c.is_alphabetic());
        let simple = simple_start && chars.all(|c| c == '_' || c == '$' || c.is_alphanumeric());
        !simple
            || (self.folds_to_lower && name.chars().any(char::is_uppercase))
            || sql_data::is_reserved_identifier(name)
    }

    fn identifier(&self, name: &str) -> String {
        match self.quote {
            Some(quote) if !self.inside_quote && self.needs_quote(name) => {
                let escaped = name.replace(quote, &format!("{quote}{quote}"));
                format!("{quote}{escaped}{quote}")
            }
            _ => name.to_owned(),
        }
    }
}

fn render<'a>(ranked: Vec<Ranked<'a>>, style: &Style) -> Vec<Item<'a>> {
    let mut items: Vec<Item<'a>> = Vec::with_capacity(MAX_ITEMS.min(ranked.len()));
    for Ranked {
        tier, candidate, ..
    } in ranked
    {
        let qualified = candidate.namespace.is_some();
        let label: Cow<'a, str> = match candidate.namespace {
            Some(namespace) => Cow::Owned(format!("{namespace}.{}", candidate.name)),
            None => Cow::Borrowed(candidate.name),
        };
        if items.iter().any(|item| {
            item.kind != Kind::Snippet
                && candidate.kind != Kind::Snippet
                && item.label.eq_ignore_ascii_case(&label)
        }) {
            continue;
        }
        let insert = match candidate.insert {
            Insert::Identifier => {
                let name = style.identifier(candidate.name);
                Cow::Owned(match candidate.namespace {
                    Some(namespace) => format!("{}.{name}", style.identifier(namespace)),
                    None => name,
                })
            }
            // Only all-capitals words follow a lower-case query: case-sensitive
            // names such as ClickHouse's `toStartOfInterval` keep their spelling.
            Insert::Word { suffix }
                if style.lowercase_words && !candidate.name.chars().any(char::is_lowercase) =>
            {
                Cow::Owned(format!("{}{suffix}", candidate.name.to_lowercase()))
            }
            Insert::Word { suffix: "" } => Cow::Borrowed(candidate.name),
            Insert::Word { suffix } => Cow::Owned(format!("{}{suffix}", candidate.name)),
            Insert::Property => Cow::Owned(match style.property {
                PropertyStyle::Bare => format!("{}: ", candidate.name),
                PropertyStyle::Quoted => format!("\"{}\": ", candidate.name),
                PropertyStyle::InsideQuotes => candidate.name.to_owned(),
            }),
            Insert::Text(text) => text,
        };
        let highlighted = matches!(tier, Tier::Prefix | Tier::Exact) && !qualified;
        let matched = if highlighted {
            prefix_bytes(&label, style.query_chars)
        } else {
            0
        };
        items.push(Item {
            label,
            insert,
            detail: candidate.detail,
            kind: candidate.kind,
            matched,
        });
        if items.len() == MAX_ITEMS {
            break;
        }
    }
    items
}
