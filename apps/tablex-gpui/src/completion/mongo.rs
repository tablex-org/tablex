//! MongoDB completion for the shell script editor, the aggregation editors
//! and the JSON filter / sort / projection editors.
//!
//! A single forward scan up to the cursor keeps a stack of open `{ [ (`
//! frames. Each frame knows what its contents mean (a filter document, an
//! update document, a pipeline stage, ...) from its parent frame and the key
//! or call argument it is the value of, so `{ $match: { age: { $` resolves to
//! query operators while `{ $group: { total: { $` resolves to accumulators.

use std::borrow::Cow;

use super::mongo_data::{
    is_option_stage, stage_options, Method, Spec, ACCUMULATORS, COLLECTION_METHODS, CURSOR_METHODS,
    DATABASE_METHODS, EXPRESSION_OPERATORS, FILTER_KEYS, JSON_VALUES, PIPELINE_STAGES,
    PUSH_MODIFIERS, QUERY_OPERATORS, ROOT_SNIPPETS, UPDATE_OPERATORS, VALUE_SNIPPETS,
};
use super::rank::{Candidate, Insert, Ranker};
use super::{
    render, Catalog, Completions, Item, Kind, MongoEditor, MongoRole, PropertyStyle, Style,
    MAX_ITEMS,
};

const RANK_CAP: usize = MAX_ITEMS + 10;

const BOOST_FIELD: u16 = 350;
const BOOST_COLLECTION: u16 = 400;
const BOOST_SNIPPET: u16 = 420;
const BOOST_METHOD: u16 = 400;
const BOOST_OPERATOR: u16 = 300;
const BOOST_HOT: u16 = 30;
const BOOST_VALUE: u16 = 200;

/// What the keys of an object mean.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Obj<'a> {
    /// `find` filter: field names and logical operators.
    Filter,
    /// Conditions on one field: `{ $gt: 1 }`.
    Comparison,
    /// Update document: `{ $set: {...} }`.
    Update,
    /// Fields under an update operator.
    UpdateFields {
        push: bool,
    },
    /// `$each`, `$slice`, ... under `$push`.
    Modifiers,
    Projection,
    Sort,
    /// Plain document: field names only.
    Document,
    /// Pipeline stage: `{ $match: ... }`.
    Stage,
    /// `$project` / `$addFields` body.
    ProjectExpr,
    Group,
    Accumulators,
    Expr,
    /// Option names of a stage such as `$lookup`.
    Options(&'a str),
    /// `$facet`: user-named pipelines.
    Facet,
}

/// What the next value in a frame is expected to be.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Shape<'a> {
    /// An object (or an array of objects) with this meaning.
    Obj(Obj<'a>),
    Pipeline,
    Any,
}

fn child_shape<'a>(parent: Obj<'a>, key: &'a str) -> Shape<'a> {
    use Obj::*;
    match parent {
        Filter => match key {
            "$expr" => Shape::Obj(Expr),
            "$and" | "$or" | "$nor" => Shape::Obj(Filter),
            _ if key.starts_with('$') => Shape::Any,
            _ => Shape::Obj(Comparison),
        },
        Comparison => match key {
            "$not" => Shape::Obj(Comparison),
            "$elemMatch" => Shape::Obj(Filter),
            _ => Shape::Any,
        },
        Update => Shape::Obj(UpdateFields {
            push: matches!(key, "$push" | "$addToSet"),
        }),
        UpdateFields { push: true } => Shape::Obj(Modifiers),
        Modifiers if key == "$sort" => Shape::Obj(Sort),
        Stage => match key {
            "$match" => Shape::Obj(Filter),
            "$project" | "$addFields" | "$set" => Shape::Obj(ProjectExpr),
            "$group" => Shape::Obj(Group),
            "$sort" => Shape::Obj(Sort),
            "$facet" => Shape::Obj(Facet),
            _ if is_option_stage(key) => Shape::Obj(Options(key)),
            _ => Shape::Obj(Expr),
        },
        Group if key == "_id" => Shape::Obj(Expr),
        Group => Shape::Obj(Accumulators),
        Accumulators | ProjectExpr | Expr => Shape::Obj(Expr),
        Options(_) if key == "pipeline" => Shape::Pipeline,
        Options(_) if key == "let" => Shape::Obj(Document),
        Facet => Shape::Pipeline,
        _ => Shape::Any,
    }
}

fn call_shape<'a>(method: &str, arg: usize) -> Shape<'a> {
    use Obj::*;
    match (method, arg) {
        (
            "find" | "findOne" | "count" | "countDocuments" | "deleteOne" | "deleteMany"
            | "findOneAndDelete" | "updateOne" | "updateMany" | "findOneAndUpdate"
            | "findOneAndReplace" | "replaceOne" | "remove" | "update",
            0,
        ) => Shape::Obj(Filter),
        ("find" | "findOne", 1) => Shape::Obj(Projection),
        ("project" | "projection", 0) => Shape::Obj(Projection),
        ("hint" | "min" | "max", 0) => Shape::Obj(Sort),
        ("updateOne" | "updateMany" | "findOneAndUpdate" | "update", 1) => Shape::Obj(Update),
        ("findOneAndReplace" | "replaceOne", 1) => Shape::Obj(Document),
        ("insertOne" | "insertMany" | "insert", 0) => Shape::Obj(Document),
        ("aggregate", 0) => Shape::Pipeline,
        ("sort" | "createIndex", 0) => Shape::Obj(Sort),
        ("distinct", 1) => Shape::Obj(Filter),
        _ => Shape::Any,
    }
}

fn root_shape<'a>(role: MongoRole<'a>) -> Shape<'a> {
    match role {
        MongoRole::Script => Shape::Any,
        MongoRole::Filter => Shape::Obj(Obj::Filter),
        MongoRole::Sort => Shape::Obj(Obj::Sort),
        MongoRole::Pipeline => Shape::Pipeline,
        MongoRole::Stage(operator) => child_shape(Obj::Stage, operator),
    }
}

#[derive(Clone, Copy, Debug)]
enum Content<'a> {
    Object(Obj<'a>),
    Array(Shape<'a>),
    Call(&'a str),
    Plain,
}

#[derive(Clone, Copy, Debug)]
struct Frame<'a> {
    content: Content<'a>,
    /// Key whose value is being written (objects).
    key: Option<&'a str>,
    /// At a key position (after `{` or `,`) rather than after `:`.
    expect_key: bool,
    /// Argument index (calls).
    arg: usize,
}

impl<'a> Frame<'a> {
    fn open(delimiter: char, parent: Shape<'a>, callee: Option<&'a str>) -> Self {
        let content = match (delimiter, parent) {
            ('{', Shape::Obj(obj)) => Content::Object(obj),
            ('[', Shape::Obj(obj)) => Content::Array(Shape::Obj(obj)),
            ('[', Shape::Pipeline) => Content::Array(Shape::Obj(Obj::Stage)),
            ('(', _) => Content::Call(callee.unwrap_or_default()),
            _ => Content::Plain,
        };
        Self {
            content,
            key: None,
            expect_key: delimiter == '{',
            arg: 0,
        }
    }

    /// Shape of the value that comes next inside this frame.
    fn value_shape(&self) -> Shape<'a> {
        match self.content {
            Content::Object(obj) => self.key.map_or(Shape::Any, |key| child_shape(obj, key)),
            Content::Array(element) => element,
            Content::Call(method) => call_shape(method, self.arg),
            Content::Plain => Shape::Any,
        }
    }
}

struct StringState {
    /// Offset of the first character after the opening quote.
    start: usize,
}

struct Scan<'a> {
    stack: Vec<Frame<'a>>,
    string: Option<StringState>,
    in_comment: bool,
}

fn is_ident_start(c: char) -> bool {
    c == '_' || c == '$' || c.is_alphabetic()
}

fn is_ident_char(c: char) -> bool {
    c == '_' || c == '$' || c.is_alphanumeric()
}

fn scan<'a>(text: &'a str, end: usize, root: Shape<'a>) -> Scan<'a> {
    let mut state = Scan {
        stack: Vec::new(),
        string: None,
        in_comment: false,
    };
    let mut pending: Option<&'a str> = None;
    let mut index = 0;
    while index < end {
        let Some(c) = text[index..end].chars().next() else {
            break;
        };
        let rest = &text[index..end];
        match c {
            c if c.is_whitespace() => index += c.len_utf8(),
            '/' if rest.starts_with("//") => match rest.find('\n') {
                Some(newline) => index += newline,
                None => {
                    state.in_comment = true;
                    index = end;
                }
            },
            '/' if rest.starts_with("/*") => match rest[2..].find("*/") {
                Some(close) => index += close + 4,
                None => {
                    state.in_comment = true;
                    index = end;
                }
            },
            '"' | '\'' | '`' => {
                let start = index + 1;
                let mut cursor = start;
                let mut closed = None;
                while let Some(next) = text[cursor..end].chars().next() {
                    if next == '\\' {
                        cursor += 1 + text[cursor + 1..end]
                            .chars()
                            .next()
                            .map_or(0, char::len_utf8);
                    } else if next == c {
                        closed = Some(cursor);
                        break;
                    } else {
                        cursor += next.len_utf8();
                    }
                }
                match closed {
                    Some(close) => {
                        pending = Some(&text[start..close]);
                        index = close + 1;
                    }
                    None => {
                        state.string = Some(StringState { start });
                        index = end;
                    }
                }
            }
            c if is_ident_start(c) => {
                let length = rest.find(|c: char| !is_ident_char(c)).unwrap_or(rest.len());
                pending = Some(&rest[..length]);
                index += length;
            }
            ':' => {
                if let Some(top) = state
                    .stack
                    .last_mut()
                    .filter(|f| matches!(f.content, Content::Object(_)))
                {
                    top.key = pending.take();
                    top.expect_key = false;
                }
                pending = None;
                index += 1;
            }
            ',' => {
                if let Some(top) = state.stack.last_mut() {
                    top.key = None;
                    top.expect_key = true;
                    top.arg += 1;
                }
                pending = None;
                index += 1;
            }
            '{' | '[' | '(' => {
                let parent = state.stack.last().map_or(root, Frame::value_shape);
                state.stack.push(Frame::open(c, parent, pending.take()));
                index += 1;
            }
            '}' | ']' | ')' => {
                state.stack.pop();
                pending = None;
                index += 1;
            }
            '.' => index += 1,
            c => {
                pending = None;
                index += c.len_utf8();
            }
        }
    }
    state
}

/// What to offer, decided from the scan and the text around the cursor.
enum Plan<'a> {
    None,
    Keys(Obj<'a>),
    Values,
    /// Collection names inside a string (`getCollection("`, `$lookup.from`).
    CollectionNames,
    /// `"$field"` references inside an aggregation string.
    FieldRefs,
    /// Start of a script: statement templates and bare shell helpers.
    Root,
    /// After `db.` (`collection: None`) or `db.<collection>.`.
    DbMembers {
        collection: Option<&'a str>,
    },
    /// After `).`: methods of the cursor / collection the call returned.
    Chain {
        methods: &'static [Method],
        allow_count: bool,
    },
}

struct Request<'a, 'c> {
    editor: &'c MongoEditor<'a>,
    catalog: &'a dyn Catalog,
    text: &'a str,
    cursor: usize,
}

pub(super) fn complete<'a>(
    editor: &MongoEditor<'a>,
    text: &'a str,
    cursor: usize,
    catalog: &'a dyn Catalog,
) -> Completions<'a> {
    let state = scan(text, cursor, root_shape(editor.role));
    if state.in_comment {
        return Completions::none(cursor);
    }
    let request = Request {
        editor,
        catalog,
        text,
        cursor,
    };
    let (plan, start) = request.plan(&state);
    let items = request.items(plan, start, state.string.is_some());
    Completions {
        replace: start..cursor,
        items,
    }
}

impl<'a> Request<'a, '_> {
    /// Start of the identifier run ending at the cursor; `extra` also admits `.`.
    fn run_start(&self, extra: bool) -> usize {
        self.text[..self.cursor]
            .char_indices()
            .rev()
            .take_while(|&(_, c)| is_ident_char(c) || (extra && c == '.'))
            .last()
            .map_or(self.cursor, |(index, _)| index)
    }

    fn plan(&self, state: &Scan<'a>) -> (Plan<'a>, usize) {
        if let Some(string) = &state.string {
            return (self.string_plan(state, string.start), string.start);
        }
        if self.editor.role == MongoRole::Script {
            let chain_start = self.run_start(true);
            let chain = &self.text[chain_start..self.cursor];
            if let Some(rest) = chain.strip_prefix("db.") {
                let (collection, prefix_len) = match rest.rfind('.') {
                    Some(dot) => (Some(&rest[..dot]), rest.len() - dot - 1),
                    None => (None, rest.len()),
                };
                return (Plan::DbMembers { collection }, self.cursor - prefix_len);
            }
            if let Some(dot) = chain.rfind('.').filter(|_| chain.starts_with('.')) {
                return (self.chain_plan(chain_start), chain_start + dot + 1);
            }
            if state.stack.is_empty() && !chain.is_empty() && !chain.contains('.') {
                return (Plan::Root, chain_start);
            }
        }
        let word = self.run_start(false);
        let plan = match state.stack.last() {
            Some(Frame {
                content: Content::Object(obj),
                expect_key: true,
                ..
            }) => Plan::Keys(*obj),
            Some(Frame {
                content: Content::Object(_),
                expect_key: false,
                ..
            }) => Plan::Values,
            _ => Plan::None,
        };
        (plan, word)
    }

    fn string_plan(&self, state: &Scan<'a>, start: usize) -> Plan<'a> {
        match state.stack.last() {
            Some(Frame {
                content: Content::Object(obj),
                expect_key: true,
                ..
            }) => Plan::Keys(*obj),
            Some(Frame {
                content: Content::Object(obj),
                key,
                ..
            }) => {
                let is_reference = matches!(
                    obj,
                    Obj::Group | Obj::ProjectExpr | Obj::Expr | Obj::Accumulators | Obj::Options(_)
                );
                if matches!(obj, Obj::Options(_)) && matches!(key, Some("from" | "coll" | "into")) {
                    Plan::CollectionNames
                } else if is_reference && self.text[start..self.cursor].starts_with('$') {
                    Plan::FieldRefs
                } else {
                    Plan::None
                }
            }
            Some(Frame {
                content: Content::Call("getCollection"),
                ..
            }) => Plan::CollectionNames,
            _ => Plan::None,
        }
    }

    /// Methods available after `<call>(...).`.
    fn chain_plan(&self, dot: usize) -> Plan<'a> {
        let before = self.text[..dot].trim_end();
        let Some(inner) = before.strip_suffix(')') else {
            return Plan::None;
        };
        let mut depth = 1usize;
        let open = inner.char_indices().rev().find_map(|(index, c)| {
            match c {
                ')' => depth += 1,
                '(' => depth -= 1,
                _ => {}
            }
            (depth == 0).then_some(index)
        });
        let head = &inner[..open.unwrap_or(0)];
        let name_start = head
            .char_indices()
            .rev()
            .take_while(|&(_, c)| is_ident_char(c))
            .last()
            .map_or(head.len(), |(index, _)| index);
        match &head[name_start..] {
            "getCollection" => Plan::Chain {
                methods: COLLECTION_METHODS,
                allow_count: true,
            },
            "find" => Plan::Chain {
                methods: CURSOR_METHODS,
                allow_count: true,
            },
            "sort" | "limit" | "skip" | "project" | "projection" | "batchSize" | "hint"
            | "collation" | "comment" | "maxTimeMS" | "min" | "max" | "allowDiskUse"
            | "noCursorTimeout" | "returnKey" | "showRecordId" | "aggregate"
            | "listCollections" => Plan::Chain {
                methods: CURSOR_METHODS,
                allow_count: false,
            },
            "getSiblingDB" => Plan::Chain {
                methods: DATABASE_METHODS,
                allow_count: true,
            },
            _ => Plan::None,
        }
    }

    fn items(&self, plan: Plan<'a>, start: usize, in_string: bool) -> Vec<Item<'a>> {
        let prefix = &self.text[start..self.cursor];
        // `"$fi` matches field `fi`: the sigil is part of the reference, not the name.
        let query = if matches!(plan, Plan::FieldRefs) {
            prefix.trim_start_matches('$')
        } else {
            prefix
        };
        let mut ranker = Ranker::new(query, RANK_CAP);
        match plan {
            Plan::None => {}
            Plan::Keys(obj) => self.key_items(&mut ranker, obj),
            Plan::Values => {
                if !prefix.is_empty() {
                    let values = if self.editor.json {
                        JSON_VALUES
                    } else {
                        VALUE_SNIPPETS
                    };
                    for value in values {
                        ranker.offer(
                            Candidate::text(value.label, Kind::Value, value.desc, value.insert),
                            BOOST_VALUE,
                        );
                    }
                }
            }
            Plan::CollectionNames => self.catalog.collections(&mut |name| {
                ranker.offer(
                    Candidate::new(name, Kind::Collection, "collection"),
                    BOOST_COLLECTION,
                );
            }),
            Plan::FieldRefs => self.catalog.fields(&mut |field| {
                let insert = Cow::Owned(format!("${field}"));
                ranker.offer(
                    Candidate::text(field, Kind::Field, "field", insert),
                    BOOST_FIELD,
                );
            }),
            Plan::Root => self.root_items(&mut ranker),
            Plan::DbMembers { collection: None } => {
                self.catalog.collections(&mut |name| {
                    let candidate = if needs_get_collection(name) {
                        let insert = format!("getCollection(\"{}\")", name.replace('"', "\\\""));
                        Candidate::text(name, Kind::Collection, "collection", insert)
                    } else {
                        Candidate::new(name, Kind::Collection, "collection")
                    };
                    ranker.offer(candidate, BOOST_COLLECTION);
                });
                offer_methods(
                    &mut ranker,
                    DATABASE_METHODS,
                    Kind::Method,
                    BOOST_METHOD - 50,
                    true,
                );
            }
            Plan::DbMembers {
                collection: Some(_),
            } => {
                offer_methods(
                    &mut ranker,
                    COLLECTION_METHODS,
                    Kind::Method,
                    BOOST_METHOD,
                    true,
                );
            }
            Plan::Chain {
                methods,
                allow_count,
            } => {
                offer_methods(
                    &mut ranker,
                    methods,
                    Kind::Method,
                    BOOST_METHOD,
                    allow_count,
                );
            }
        }
        let property = if in_string {
            PropertyStyle::InsideQuotes
        } else if self.editor.json {
            PropertyStyle::Quoted
        } else {
            PropertyStyle::Bare
        };
        let style = Style {
            quote: None,
            folds_to_lower: false,
            inside_quote: in_string,
            lowercase_words: false,
            query_chars: query.chars().count(),
            property,
        };
        render(ranker.finish(), &style)
    }

    fn key_items(&self, ranker: &mut Ranker<'a, '_>, obj: Obj<'a>) {
        let fields = |ranker: &mut Ranker<'a, '_>| {
            self.catalog.fields(&mut |field| {
                ranker.offer(property(field, Kind::Field, "field"), BOOST_FIELD);
            });
        };
        match obj {
            Obj::Filter => {
                fields(ranker);
                offer_specs(ranker, FILTER_KEYS);
            }
            Obj::Comparison => offer_specs(ranker, QUERY_OPERATORS),
            Obj::Update => offer_specs(ranker, UPDATE_OPERATORS),
            Obj::Modifiers => offer_specs(ranker, PUSH_MODIFIERS),
            Obj::Stage => offer_specs(ranker, PIPELINE_STAGES),
            Obj::Accumulators => offer_specs(ranker, ACCUMULATORS),
            Obj::Expr => offer_specs(ranker, EXPRESSION_OPERATORS),
            Obj::Options(stage) => offer_specs(ranker, stage_options(stage)),
            Obj::Group => {
                ranker.offer(property("_id", Kind::Field, "group key"), BOOST_FIELD + 20);
                fields(ranker);
            }
            Obj::UpdateFields { .. }
            | Obj::Projection
            | Obj::Sort
            | Obj::Document
            | Obj::ProjectExpr => fields(ranker),
            Obj::Facet => {}
        }
    }

    /// Start of a script: statement templates, then bare shell helpers.
    fn root_items(&self, ranker: &mut Ranker<'a, '_>) {
        let collection = self
            .editor
            .collection
            .filter(|name| !name.is_empty())
            .unwrap_or("collection");
        for (index, template) in ROOT_SNIPPETS.iter().enumerate() {
            let insert = if template.insert.contains("{collection}") {
                Cow::Owned(template.insert.replace("{collection}", collection))
            } else {
                Cow::Borrowed(template.insert)
            };
            let boost = BOOST_SNIPPET - index as u16;
            ranker.offer(
                Candidate::text(template.label, Kind::Snippet, template.desc, insert),
                boost,
            );
        }
        offer_methods(ranker, COLLECTION_METHODS, Kind::Method, BOOST_VALUE, true);
    }
}

fn property<'a>(name: &'a str, kind: Kind, detail: &'a str) -> Candidate<'a> {
    Candidate {
        insert: Insert::Property,
        ..Candidate::new(name, kind, detail)
    }
}

fn offer_specs<'a>(ranker: &mut Ranker<'a, '_>, specs: &'static [Spec]) {
    for spec in specs {
        let boost = BOOST_OPERATOR + if spec.hot { BOOST_HOT } else { 0 };
        ranker.offer(property(spec.label, Kind::Operator, spec.desc), boost);
    }
}

/// Methods in table order: popularity breaks ties through the boost.
fn offer_methods<'a>(
    ranker: &mut Ranker<'a, '_>,
    methods: &'static [Method],
    kind: Kind,
    base: u16,
    allow_count: bool,
) {
    for (index, method) in methods.iter().enumerate() {
        if !allow_count && method.label == "count" {
            continue;
        }
        let boost = base.saturating_sub(index as u16);
        ranker.offer(
            Candidate::text(method.label, kind, method.desc, method.insert),
            boost,
        );
    }
}

/// Names that cannot be written as `db.<name>`.
fn needs_get_collection(name: &str) -> bool {
    let mut chars = name.chars();
    let identifier = chars.next().is_some_and(is_ident_start) && chars.all(is_ident_char);
    !identifier || DATABASE_METHODS.iter().any(|method| method.label == name)
}
