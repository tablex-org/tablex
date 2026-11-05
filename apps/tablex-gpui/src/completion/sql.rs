//! SQL completion: find the clause at the cursor, the tables in scope and
//! the qualifier being typed, then rank candidates from the static vocabulary
//! and the cached catalog.

use std::borrow::Cow;
use std::ops::Range;

use super::rank::{Candidate, Ranker};
use super::sql_data_clickhouse;
use super::sql_data::{self, is_reserved, Place, SNIPPETS};
use super::sql_lex::{lex, statement_bounds, Tok, Token};
use super::{
    render, Catalog, Column, Completions, Kind, PropertyStyle, Relation, SqlEditor, SqlFlavor,
    Style, MAX_ITEMS,
};

const RANK_CAP: usize = MAX_ITEMS + 10;

const BOOST_TABLE: u16 = 500;
const BOOST_TABLE_OTHER_SCHEMA: u16 = 400;
const BOOST_CTE: u16 = 450;
const BOOST_COLUMN: u16 = 450;
const BOOST_ALIAS: u16 = 420;
const BOOST_NEXT_KEYWORD: u16 = 320;
const BOOST_SNIPPET: u16 = 300;
const BOOST_SCHEMA: u16 = 300;
const BOOST_KEYWORD: u16 = 200;
const BOOST_FUNCTION: u16 = 150;

/// Table reference found in a `FROM` / `JOIN` / `UPDATE` / `INTO` list.
#[derive(Clone, Copy, Debug)]
struct TableRef<'a> {
    namespace: Option<&'a str>,
    name: &'a str,
    alias: Option<&'a str>,
    /// Subquery alias: columns unknown.
    derived: bool,
}

impl<'a> TableRef<'a> {
    fn exposed_name(&self) -> &'a str {
        self.alias.unwrap_or(self.name)
    }

    fn answers_to(&self, qualifier: &str) -> bool {
        self.exposed_name().eq_ignore_ascii_case(qualifier)
    }
}

/// What kind of position the cursor is in, from the nearest clause keyword.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Clause {
    Select,
    From,
    Into,
    Update,
    /// `WHERE`, `ON`, `GROUP BY`, `SET`, ... : an expression.
    Expr,
    Values,
    Ddl,
    /// `UNION`, `WITH`, `EXPLAIN`: a new query starts here.
    NewQuery,
    Other,
}

fn clause_of(word: &str) -> Option<Clause> {
    const TABLE: &[(&str, Clause)] = &[
        ("SELECT", Clause::Select),
        ("FROM", Clause::From),
        ("JOIN", Clause::From),
        ("INTO", Clause::Into),
        ("UPDATE", Clause::Update),
        ("ON", Clause::Expr),
        ("USING", Clause::Expr),
        ("WHERE", Clause::Expr),
        ("HAVING", Clause::Expr),
        ("PREWHERE", Clause::Expr),
        ("GROUP", Clause::Expr),
        ("ORDER", Clause::Expr),
        ("LIMIT", Clause::Expr),
        ("OFFSET", Clause::Expr),
        ("SET", Clause::Expr),
        ("RETURNING", Clause::Expr),
        ("VALUES", Clause::Values),
        ("CREATE", Clause::Ddl),
        ("ALTER", Clause::Ddl),
        ("DROP", Clause::Ddl),
        ("WITH", Clause::NewQuery),
        ("UNION", Clause::NewQuery),
        ("INTERSECT", Clause::NewQuery),
        ("EXCEPT", Clause::NewQuery),
        ("EXPLAIN", Clause::NewQuery),
    ];
    TABLE
        .iter()
        .find(|(keyword, _)| keyword.eq_ignore_ascii_case(word))
        .map(|(_, clause)| *clause)
}

struct Source<'a, 't> {
    text: &'a str,
    tokens: &'t [Token],
}

impl<'a> Source<'a, '_> {
    fn kind(&self, index: usize) -> Option<Tok> {
        self.tokens.get(index).map(|token| token.kind)
    }

    fn word(&self, index: usize) -> Option<&'a str> {
        let token = self.tokens.get(index)?;
        (token.kind == Tok::Word).then(|| token.text(self.text))
    }

    fn is_keyword(&self, index: usize, keyword: &str) -> bool {
        self.word(index)
            .is_some_and(|word| word.eq_ignore_ascii_case(keyword))
    }

    /// Word or quoted identifier, unquoted.
    fn ident(&self, index: usize) -> Option<&'a str> {
        let token = self.tokens.get(index)?;
        match token.kind {
            Tok::Word => Some(token.text(self.text)),
            Tok::Quoted => {
                let closing = usize::from(token.terminated);
                let end = token.end.saturating_sub(closing).max(token.start + 1);
                Some(&self.text[token.start + 1..end])
            }
            _ => None,
        }
    }

    /// Index of the `)` closing the `(` at `open`, or the token count.
    fn matching_close(&self, open: usize) -> usize {
        let mut depth = 0usize;
        for (index, token) in self.tokens.iter().enumerate().skip(open) {
            match token.kind {
                Tok::Open => depth += 1,
                Tok::Close => {
                    depth -= 1;
                    if depth == 0 {
                        return index;
                    }
                }
                _ => {}
            }
        }
        self.tokens.len()
    }

    fn starts_subquery(&self, open: usize) -> bool {
        self.is_keyword(open + 1, "SELECT") || self.is_keyword(open + 1, "WITH")
    }

    fn alias_at(&self, index: usize, typing: Option<usize>) -> Option<&'a str> {
        if Some(index) == typing {
            return None;
        }
        match self.kind(index)? {
            Tok::Quoted => self.ident(index),
            Tok::Word => self.word(index).filter(|word| !is_reserved(word)),
            _ => None,
        }
    }

    /// `[schema.]name [AS] [alias]` at `index`; returns the reference (None
    /// for table functions) and the index after it.
    fn table_ref(&self, index: usize, typing: Option<usize>) -> (Option<TableRef<'a>>, usize) {
        let mut parts = [None; 3];
        let mut count = 0;
        let mut next = index;
        while count < parts.len() {
            let Some(part) = self.ident(next) else { break };
            parts[count] = Some(part);
            count += 1;
            next += 1;
            if self.kind(next) == Some(Tok::Dot)
                && self.ident(next + 1).is_some()
                && Some(next + 1) != typing
            {
                next += 1;
            } else {
                break;
            }
        }
        // A table function (`f(`) or a qualifier still being typed (`schema.`).
        if count == 0 || matches!(self.kind(next), Some(Tok::Open | Tok::Dot)) {
            return (None, next.max(index + 1));
        }
        let mut cursor = next;
        if self.is_keyword(cursor, "AS") {
            cursor += 1;
        }
        let alias = self.alias_at(cursor, typing);
        if alias.is_some() {
            cursor += 1;
        } else {
            cursor = next;
        }
        let reference = TableRef {
            namespace: count.checked_sub(2).and_then(|i| parts[i]),
            name: parts[count - 1].unwrap_or_default(),
            alias,
            derived: false,
        };
        (Some(reference), cursor)
    }

    /// Tables of one query block (`range` of token indices): the depth-0
    /// operands of `FROM`, `JOIN`, `UPDATE` and `INTO`.
    fn collect_refs(
        &self,
        range: Range<usize>,
        typing: Option<usize>,
        out: &mut Vec<TableRef<'a>>,
    ) {
        let (mut depth, mut expecting, mut in_from) = (0usize, false, false);
        let mut index = range.start;
        while index < range.end {
            match self.kind(index) {
                Some(Tok::Open) => {
                    if depth == 0 && expecting && self.starts_subquery(index) {
                        let close = self.matching_close(index);
                        let mut next = close + 1;
                        if self.is_keyword(next, "AS") {
                            next += 1;
                        }
                        if let Some(alias) = self.alias_at(next, typing) {
                            out.push(TableRef {
                                namespace: None,
                                name: alias,
                                alias: None,
                                derived: true,
                            });
                            next += 1;
                        }
                        expecting = in_from;
                        index = next;
                        continue;
                    }
                    depth += 1;
                }
                Some(Tok::Close) => depth = depth.saturating_sub(1),
                Some(Tok::Comma) if depth == 0 && in_from => expecting = true,
                Some(Tok::Word | Tok::Quoted) if depth == 0 => {
                    let word = self.word(index).unwrap_or_default();
                    if word.eq_ignore_ascii_case("FROM") || word.eq_ignore_ascii_case("JOIN") {
                        (expecting, in_from) = (true, true);
                    } else if word.eq_ignore_ascii_case("UPDATE")
                        || word.eq_ignore_ascii_case("INTO")
                    {
                        (expecting, in_from) = (true, false);
                    } else if expecting {
                        if word.eq_ignore_ascii_case("ONLY") || word.eq_ignore_ascii_case("LATERAL")
                        {
                            index += 1;
                            continue;
                        }
                        if Some(index) == typing || is_reserved(word) {
                            expecting = false;
                        } else {
                            let (found, next) = self.table_ref(index, typing);
                            out.extend(found);
                            expecting = false;
                            index = next;
                            continue;
                        }
                    } else if in_from
                        && clause_of(word).is_some_and(|clause| clause != Clause::From)
                    {
                        in_from = false;
                    }
                }
                _ => {}
            }
            index += 1;
        }
    }

    /// Names defined by a leading `WITH name AS (...)` list.
    fn collect_ctes(&self, out: &mut Vec<&'a str>) {
        if !self.is_keyword(0, "WITH") {
            return;
        }
        let mut index = 1;
        if self.is_keyword(index, "RECURSIVE") {
            index += 1;
        }
        while let Some(name) = self.ident(index) {
            index += 1;
            if self.kind(index) == Some(Tok::Open) {
                index = self.matching_close(index) + 1;
            }
            if !self.is_keyword(index, "AS") {
                break;
            }
            index += 1;
            for modifier in ["NOT", "MATERIALIZED"] {
                if self.is_keyword(index, modifier) {
                    index += 1;
                }
            }
            if self.kind(index) != Some(Tok::Open) {
                break;
            }
            out.push(name);
            index = self.matching_close(index) + 1;
            if self.kind(index) != Some(Tok::Comma) {
                break;
            }
            index += 1;
        }
    }
}

/// The word under the cursor and where it sits in the token list.
struct Typing<'a> {
    prefix: &'a str,
    replace: Range<usize>,
    in_quote: bool,
    /// Token being typed, if any.
    token: Option<usize>,
    /// Tokens `..before` precede the cursor word.
    before: usize,
}

fn locate<'a>(text: &'a str, tokens: &[Token], cursor: usize) -> Option<Typing<'a>> {
    let index = tokens.partition_point(|token| token.end < cursor);
    let Some(token) = tokens.get(index).filter(|token| token.start < cursor) else {
        return Some(Typing {
            prefix: "",
            replace: cursor..cursor,
            in_quote: false,
            token: None,
            before: index,
        });
    };
    let previous = Typing {
        prefix: "",
        replace: cursor..cursor,
        in_quote: false,
        token: None,
        before: index + 1,
    };
    match token.kind {
        Tok::Comment | Tok::Str if token.contains_cursor(cursor) => None,
        Tok::Num => None,
        Tok::Word => Some(Typing {
            prefix: &text[token.start..cursor],
            replace: token.start..cursor,
            in_quote: false,
            token: Some(index),
            before: index,
        }),
        Tok::Quoted if token.contains_cursor(cursor) => Some(Typing {
            prefix: &text[token.start + 1..cursor],
            replace: token.start + 1..cursor,
            in_quote: true,
            token: Some(index),
            before: index,
        }),
        _ => Some(previous),
    }
}

/// `a.` / `a.b.` before the word: the qualifier parts, right-aligned in the
/// array, and how many there are.
fn qualifier<'a>(source: &Source<'a, '_>, before: usize) -> Option<([&'a str; 2], usize)> {
    if before == 0 || source.kind(before - 1) != Some(Tok::Dot) {
        return None;
    }
    let mut parts = [""; 2];
    let mut len = 0;
    let mut dot = before - 1;
    loop {
        let name_index = dot.checked_sub(1)?;
        len += 1;
        parts[2 - len] = source.ident(name_index)?;
        if len == 2 {
            break;
        }
        match name_index.checked_sub(1) {
            Some(previous) if source.kind(previous) == Some(Tok::Dot) => dot = previous,
            _ => break,
        }
    }
    Some((parts, len))
}

struct Query<'a, 'c> {
    editor: &'c SqlEditor<'a>,
    catalog: &'a dyn Catalog,
    source: Source<'a, 'c>,
    typing: Typing<'a>,
    refs: Vec<TableRef<'a>>,
    ctes: Vec<&'a str>,
}

pub(super) fn complete<'a>(
    editor: &SqlEditor<'a>,
    text: &'a str,
    cursor: usize,
    catalog: &'a dyn Catalog,
) -> Completions<'a> {
    let flavor = editor.flavor;
    let bounds = if editor.fragment_of.is_some() {
        0..text.len()
    } else {
        statement_bounds(text, cursor, flavor)
    };
    let tokens = lex(text, bounds.clone(), flavor);
    let Some(typing) = locate(text, &tokens, cursor) else {
        return Completions::none(cursor);
    };
    let source = Source {
        text,
        tokens: &tokens,
    };
    let mut query = Query {
        editor,
        catalog,
        source,
        typing,
        refs: Vec::new(),
        ctes: Vec::new(),
    };
    let replace = query.typing.replace.clone();
    let items = query.items();
    Completions { replace, items }
}

impl<'a> Query<'a, '_> {
    fn items(&mut self) -> Vec<super::Item<'a>> {
        let prefix = self.typing.prefix;
        let before = self.typing.before;
        let qualified = qualifier(&self.source, before);
        let mut ranker = Ranker::new(prefix, RANK_CAP);

        // Block structure: innermost query block, tables in scope.
        let mut open = Vec::new();
        for (index, token) in self.source.tokens[..before].iter().enumerate() {
            match token.kind {
                Tok::Open => open.push(index),
                Tok::Close => {
                    open.pop();
                }
                _ => {}
            }
        }
        let block_start = open
            .iter()
            .rev()
            .find(|&&index| self.source.starts_subquery(index))
            .map_or(0, |&index| index + 1);
        let head = open
            .last()
            .copied()
            .filter(|&index| index + 1 != block_start);
        self.scope(&open);

        if let Some((parts, len)) = qualified {
            self.qualified_items(&mut ranker, &parts[2 - len..]);
        } else {
            let prev = before.checked_sub(1).filter(|&index| index >= block_start);
            let clause = self.clause_before(block_start, before);
            let prev_word = prev.and_then(|index| self.source.word(index));
            let prev_is =
                |keyword: &str| prev_word.is_some_and(|word| word.eq_ignore_ascii_case(keyword));
            let expects_table = [
                "FROM", "JOIN", "INTO", "UPDATE", "TRUNCATE", "ONLY", "DESCRIBE",
            ]
            .iter()
            .any(|keyword| prev_is(keyword))
                || (prev_is("TABLE") && !self.source.is_keyword(0, "CREATE"))
                || (prev_is("EXISTS")
                    && clause == Clause::Ddl
                    && !self.source.is_keyword(0, "CREATE"))
                || (prev.is_some_and(|index| self.source.kind(index) == Some(Tok::Comma))
                    && clause == Clause::From
                    && head.is_none());
            // `ARRAY JOIN` takes array columns, not tables.
            let expects_table = expects_table
                && !(prev_is("JOIN")
                    && before
                        .checked_sub(2)
                        .is_some_and(|index| self.source.is_keyword(index, "ARRAY")));
            if self.editor.flavor == SqlFlavor::Clickhouse && self.expects_engine(before) {
                self.engine_items(&mut ranker);
            } else if expects_table {
                self.relation_items(&mut ranker, None);
                self.cte_items(&mut ranker);
            } else if let Some(table) = self.insert_target(head, clause) {
                self.column_items(&mut ranker, std::slice::from_ref(&table));
            } else if self.editor.fragment_of.is_none() && prev.is_none() && block_start == 0 {
                if !prefix.is_empty() {
                    self.statement_start_items(&mut ranker);
                }
            } else if prefix.is_empty() {
                let columns_expected = prev.is_some_and(|index| {
                    self.source.kind(index) == Some(Tok::Comma)
                        || [
                            "SELECT", "WHERE", "PREWHERE", "AND", "OR", "ON", "SET", "BY", "HAVING",
                            "NOT",
                        ]
                        .iter()
                        .any(|keyword| self.source.is_keyword(index, keyword))
                });
                if columns_expected && matches!(clause, Clause::Select | Clause::Expr) {
                    self.column_items(&mut ranker, &self.refs);
                    self.alias_items(&mut ranker);
                }
            } else {
                self.expression_items(&mut ranker, clause);
            }
        }

        let style = Style {
            quote: Some(self.editor.identifier_quote),
            folds_to_lower: self.editor.flavor == SqlFlavor::Postgres,
            inside_quote: self.typing.in_quote,
            lowercase_words: !prefix.is_empty() && !prefix.chars().any(char::is_uppercase),
            query_chars: prefix.chars().count(),
            property: PropertyStyle::Bare,
        };
        render(ranker.finish(), &style)
    }

    /// Fills `refs` / `ctes`: the innermost query block plus every enclosing
    /// block (correlated subqueries see the outer aliases).
    fn scope(&mut self, open: &[usize]) {
        let tokens = self.source.tokens.len();
        let typing = self.typing.token;
        let mut ranges: Vec<Range<usize>> = open
            .iter()
            .rev()
            .filter(|&&index| self.source.starts_subquery(index))
            .map(|&index| index + 1..self.source.matching_close(index).min(tokens))
            .collect();
        ranges.push(0..tokens);
        let mut refs = Vec::new();
        for range in ranges {
            self.source.collect_refs(range, typing, &mut refs);
        }
        if let Some(table) = self.editor.fragment_of {
            refs.push(TableRef {
                namespace: table.namespace,
                name: table.name,
                alias: None,
                derived: false,
            });
        }
        self.refs = refs;
        let mut ctes = Vec::new();
        self.source.collect_ctes(&mut ctes);
        self.ctes = ctes;
    }

    /// Nearest clause keyword before the cursor at the same nesting depth.
    fn clause_before(&self, block_start: usize, before: usize) -> Clause {
        if self.editor.fragment_of.is_some() {
            return Clause::Expr;
        }
        let mut depth = 0usize;
        for index in (block_start..before).rev() {
            match self.source.kind(index) {
                Some(Tok::Close) => depth += 1,
                Some(Tok::Open) => depth = depth.saturating_sub(1),
                Some(Tok::Word) if depth == 0 => {
                    if let Some(clause) = self.source.word(index).and_then(clause_of) {
                        return clause;
                    }
                }
                _ => {}
            }
        }
        Clause::Other
    }

    /// `INSERT INTO table (|`: the table whose columns are being listed.
    fn insert_target(&self, head: Option<usize>, clause: Clause) -> Option<TableRef<'a>> {
        let head = head.filter(|_| clause == Clause::Into)?;
        let name_at = head.checked_sub(1)?;
        let name = self.source.ident(name_at)?;
        let namespace = (name_at >= 2 && self.source.kind(name_at - 1) == Some(Tok::Dot))
            .then(|| self.source.ident(name_at - 2))
            .flatten();
        Some(TableRef {
            namespace,
            name,
            alias: None,
            derived: false,
        })
    }

    fn is_default_namespace(&self, namespace: Option<&str>) -> bool {
        match (namespace, self.catalog.default_namespace()) {
            (None, _) => true,
            (Some(namespace), Some(default)) => namespace.eq_ignore_ascii_case(default),
            (Some(_), None) => false,
        }
    }

    /// `qualifier.` / `schema.table.`: columns of that table or the tables of
    /// that schema.
    fn qualified_items(&self, ranker: &mut Ranker<'a, '_>, parts: &[&'a str]) {
        match parts {
            [schema, table] => {
                let reference = TableRef {
                    namespace: Some(schema),
                    name: table,
                    alias: None,
                    derived: false,
                };
                self.push_columns(ranker, std::slice::from_ref(&reference), false);
            }
            [qualifier] => {
                let matching: Vec<TableRef<'a>> = self
                    .refs
                    .iter()
                    .filter(|r| r.answers_to(qualifier))
                    .copied()
                    .collect();
                if !matching.is_empty() {
                    self.push_columns(ranker, &matching, false);
                    return;
                }
                let mut is_namespace = false;
                self.catalog.namespaces(&mut |namespace| {
                    is_namespace |= namespace.eq_ignore_ascii_case(qualifier);
                });
                if is_namespace {
                    self.relation_items(ranker, Some(qualifier));
                } else {
                    let reference = TableRef {
                        namespace: None,
                        name: qualifier,
                        alias: None,
                        derived: false,
                    };
                    self.push_columns(ranker, std::slice::from_ref(&reference), false);
                }
            }
            _ => {}
        }
    }

    /// Tables and views. With `only_namespace`, just that schema's, unqualified.
    fn relation_items(&self, ranker: &mut Ranker<'a, '_>, only_namespace: Option<&str>) {
        self.catalog.relations(&mut |relation: Relation<'a>| {
            let default = self.is_default_namespace(relation.namespace);
            let kind = if relation.view {
                Kind::View
            } else {
                Kind::Table
            };
            let detail = if relation.view { "view" } else { "table" };
            let candidate = Candidate::new(relation.name, kind, detail);
            match only_namespace {
                Some(namespace) => {
                    let same = relation.namespace.map_or(
                        self.catalog
                            .default_namespace()
                            .is_some_and(|d| d.eq_ignore_ascii_case(namespace)),
                        |own| own.eq_ignore_ascii_case(namespace),
                    );
                    if same {
                        ranker.offer(candidate, BOOST_TABLE);
                    }
                }
                None if default => ranker.offer(candidate, BOOST_TABLE),
                None => ranker.offer(
                    candidate.in_namespace(relation.namespace),
                    BOOST_TABLE_OTHER_SCHEMA,
                ),
            }
        });
        if only_namespace.is_none() {
            self.catalog.namespaces(&mut |namespace| {
                if !self.is_default_namespace(Some(namespace)) {
                    ranker.offer(
                        Candidate::new(namespace, Kind::Schema, "schema"),
                        BOOST_SCHEMA,
                    );
                }
            });
        }
    }

    /// `ENGINE`, optionally followed by `=`, right before the cursor word.
    fn expects_engine(&self, before: usize) -> bool {
        let Some(last) = before.checked_sub(1) else {
            return false;
        };
        let is_equals = self.source.kind(last) == Some(Tok::Op)
            && self.source.tokens[last].text(self.source.text) == "=";
        let word = if is_equals { last.checked_sub(1) } else { Some(last) };
        word.is_some_and(|index| self.source.is_keyword(index, "ENGINE"))
    }

    fn engine_items(&self, ranker: &mut Ranker<'a, '_>) {
        for &(name, description) in sql_data_clickhouse::ENGINES {
            ranker.offer(
                Candidate::word(name, Kind::Keyword, description, ""),
                BOOST_TABLE,
            );
        }
    }

    fn cte_items(&self, ranker: &mut Ranker<'a, '_>) {
        for cte in &self.ctes {
            ranker.offer(Candidate::new(cte, Kind::Table, "cte"), BOOST_CTE);
        }
    }

    fn column_items(&self, ranker: &mut Ranker<'a, '_>, refs: &[TableRef<'a>]) {
        self.push_columns(ranker, refs, refs.len() > 1);
    }

    /// Columns of `refs`; a name present in several tables is offered
    /// qualified by its alias when `disambiguate`.
    fn push_columns(&self, ranker: &mut Ranker<'a, '_>, refs: &[TableRef<'a>], disambiguate: bool) {
        let mut columns: Vec<(Column<'a>, usize)> = Vec::new();
        for (index, reference) in refs.iter().enumerate().filter(|(_, r)| !r.derived) {
            self.catalog
                .columns(reference.namespace, reference.name, &mut |column| {
                    columns.push((column, index))
                });
        }
        for &(column, index) in &columns {
            let duplicated = disambiguate
                && columns
                    .iter()
                    .filter(|(other, _)| other.name.eq_ignore_ascii_case(column.name))
                    .count()
                    > 1;
            let detail = Cow::Borrowed(column.data_type.unwrap_or("column"));
            let candidate = Candidate::new(column.name, Kind::Column, detail);
            if duplicated {
                let owner = refs[index].exposed_name();
                ranker.offer(candidate.in_namespace(Some(owner)), BOOST_COLUMN);
            } else {
                ranker.offer(candidate, BOOST_COLUMN);
            }
        }
    }

    fn alias_items(&self, ranker: &mut Ranker<'a, '_>) {
        for reference in &self.refs {
            let detail = if reference.alias.is_some() {
                "alias"
            } else {
                "table in scope"
            };
            ranker.offer(
                Candidate::new(reference.exposed_name(), Kind::Alias, detail),
                BOOST_ALIAS,
            );
        }
    }

    fn function_items(&self, ranker: &mut Ranker<'a, '_>) {
        for function in sql_data::functions_for(self.editor.flavor) {
            let suffix = if function.no_args() { "()" } else { "(" };
            ranker.offer(
                Candidate::word(function.name, Kind::Function, function.desc, suffix),
                BOOST_FUNCTION,
            );
        }
    }

    fn keyword_items(&self, ranker: &mut Ranker<'a, '_>, place: Place, boost: u16) {
        for keyword in sql_data::keywords_for(self.editor.flavor, place) {
            ranker.offer(
                Candidate::word(keyword.text, Kind::Keyword, keyword.desc, ""),
                boost,
            );
        }
    }

    fn snippet_items(&self, ranker: &mut Ranker<'a, '_>) {
        for snippet in SNIPPETS {
            let mut candidate =
                Candidate::text(snippet.label, Kind::Snippet, snippet.summary, snippet.body);
            candidate.alias = Some(snippet.trigger);
            ranker.offer(candidate, BOOST_SNIPPET);
        }
    }

    fn statement_start_items(&self, ranker: &mut Ranker<'a, '_>) {
        self.snippet_items(ranker);
        self.keyword_items(ranker, Place::Statement, BOOST_KEYWORD);
        self.function_items(ranker);
    }

    fn expression_items(&self, ranker: &mut Ranker<'a, '_>, clause: Clause) {
        match clause {
            Clause::Select | Clause::Expr | Clause::Values => {
                self.column_items(ranker, &self.refs);
                self.alias_items(ranker);
                self.function_items(ranker);
                self.keyword_items(ranker, Place::Expr, BOOST_KEYWORD);
                self.keyword_items(ranker, Place::Clause, BOOST_KEYWORD - 20);
            }
            Clause::From | Clause::Into | Clause::Update => {
                // After a table reference: what may follow it.
                self.keyword_items(ranker, Place::Clause, BOOST_NEXT_KEYWORD);
                self.keyword_items(ranker, Place::Expr, BOOST_KEYWORD);
            }
            Clause::Ddl => {
                self.keyword_items(ranker, Place::Ddl, BOOST_NEXT_KEYWORD);
                self.keyword_items(ranker, Place::Statement, BOOST_KEYWORD);
            }
            Clause::NewQuery => {
                self.keyword_items(ranker, Place::Statement, BOOST_NEXT_KEYWORD);
                self.keyword_items(ranker, Place::Clause, BOOST_KEYWORD);
            }
            Clause::Other => {
                self.keyword_items(ranker, Place::Statement, BOOST_KEYWORD);
                self.keyword_items(ranker, Place::Clause, BOOST_KEYWORD);
                self.keyword_items(ranker, Place::Expr, BOOST_KEYWORD);
                self.function_items(ranker);
            }
        }
    }
}
