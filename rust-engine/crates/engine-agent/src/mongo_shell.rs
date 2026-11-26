//! mongosh-style commands for `execute_query` on a MongoDB connection: `db.orders.find({...})`,
//! `db.getCollection('orders').getIndexes()`, `show collections`, `db.getSiblingDB('x').orders.count()`.
//!
//! The text is read here, once, into a [`Command`] that says what to do and how risky it is; no
//! JavaScript runs. The values inside the arguments (`ObjectId("...")`, `ISODate("...")`,
//! `/regex/i`, bare keys, single quotes) are read by the shell value parser the app uses.
//! Everything the parser does not know is refused with the list of what it does, rather than
//! guessed at: a command nobody classified is never sent.
//!
//! A [`Command`] holds the server commands a write sends, already built (`insert`, `update`,
//! `delete`, `findAndModify`, `createIndexes`, `dropIndexes`, `drop`), so the policy, the approval
//! card and the run all see the very same document. Reads are built by the layer, which knows the
//! row limit.

use crate::mongo::MAX_INSERT_DOCUMENTS;
use crate::shell_value::{parse_value, NumberTyping};
use bson::{doc, Bson, Document};

/// The most operations one `bulkWrite` takes.
const MAX_BULK_OPERATIONS: usize = MAX_INSERT_DOCUMENTS;


/// What the parser understands, for the error of a command it does not.
const SUPPORTED: &str = "show dbs, show collections, use <db>, db.version(), db.stats(), \
db.getCollectionNames(), db.getSiblingDB(name) and, on db.<collection> or db.getCollection(name): \
find, findOne, count, countDocuments, estimatedDocumentCount, aggregate, distinct, getIndexes, \
stats, dataSize, storageSize, totalIndexSize, insertOne, insertMany, insert, updateOne, updateMany, \
update, replaceOne, deleteOne, deleteMany, bulkWrite, findOneAndUpdate, findOneAndReplace, \
findOneAndDelete, createIndex, dropIndex, dropIndexes, drop, renameCollection";

/// A `find` as written: the cursor methods are folded in.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct FindSpec {
    pub collection: String,
    pub filter: Document,
    pub projection: Option<Document>,
    pub sort: Option<Document>,
    pub collation: Option<Document>,
    pub skip: u64,
    /// `limit(n)`; `None` when not given (or 0, which the shell reads as no limit).
    pub limit: Option<u64>,
}

#[derive(Debug, Clone, PartialEq)]
pub(crate) enum Command {
    /// `use <db>`: answered without a round trip, see the layer.
    Use(String),
    Version,
    ShowDatabases,
    ShowCollections,
    DbStats,
    Find {
        spec: Box<FindSpec>,
        /// `findOne`: one document.
        one: bool,
        /// `.explain(verbosity)` at the end of the chain.
        explain: Option<String>,
    },
    Count {
        collection: String,
        /// `None`: all documents (`estimatedDocumentCount`).
        filter: Option<Document>,
    },
    Aggregate {
        collection: String,
        pipeline: Vec<Document>,
        options: Document,
    },
    Distinct {
        collection: String,
        field: String,
        filter: Option<Document>,
    },
    GetIndexes {
        collection: String,
    },
    CollectionStats {
        collection: String,
        /// The reply field the shell method returns; `None` for the whole `stats()` reply.
        metric: Option<&'static str>,
        scale: Option<i64>,
    },
    /// Server commands that change data, in the order they are sent.
    Write {
        collection: String,
        method: &'static str,
        steps: Vec<Document>,
        /// `bulkWrite` / `insertMany` `ordered`: stop at the first command with write errors.
        ordered: bool,
        dangerous: Option<String>,
    },
    /// `renameCollection` needs the database name, which the layer knows.
    Rename {
        collection: String,
        new_name: String,
    },
    /// Never run through MCP.
    Refused(&'static str),
}

/// What a command needs before it runs.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Class<'a> {
    Read,
    /// A change; `Some` is why it is dangerous.
    Write(Option<&'a str>),
    Refused(&'a str),
}

impl Command {
    pub(crate) fn class(&self) -> Class<'_> {
        match self {
            Self::Refused(name) => Class::Refused(name),
            Self::Write { dangerous, .. } => Class::Write(dangerous.as_deref()),
            Self::Rename { .. } => Class::Write(Some(
                "renaming a collection changes every query and index that names it",
            )),
            Self::Aggregate { pipeline, .. } if pipeline_writes(pipeline) => Class::Write(Some(
                "a `$out` or `$merge` stage writes (and may replace) a collection",
            )),
            _ => Class::Read,
        }
    }

    /// The method or command name for messages.
    pub(crate) fn name(&self) -> &str {
        match self {
            Self::Use(_) => "use",
            Self::Version => "version",
            Self::ShowDatabases => "show dbs",
            Self::ShowCollections => "show collections",
            Self::DbStats => "stats",
            Self::Find { one: true, .. } => "findOne",
            Self::Find { .. } => "find",
            Self::Count { .. } => "count",
            Self::Aggregate { .. } => "aggregate",
            Self::Distinct { .. } => "distinct",
            Self::GetIndexes { .. } => "getIndexes",
            Self::CollectionStats { .. } => "stats",
            Self::Write { method, .. } => method,
            Self::Rename { .. } => "renameCollection",
            Self::Refused(name) => name,
        }
    }

    /// The collection a write changes (for the approval card).
    pub(crate) fn collection(&self) -> Option<&str> {
        match self {
            Self::Find { spec, .. } => Some(&spec.collection),
            Self::Count { collection, .. }
            | Self::Aggregate { collection, .. }
            | Self::Distinct { collection, .. }
            | Self::GetIndexes { collection }
            | Self::CollectionStats { collection, .. }
            | Self::Write { collection, .. }
            | Self::Rename { collection, .. } => Some(collection),
            _ => None,
        }
    }
}

/// A command and the database it names itself (`db.getSiblingDB`).
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct Parsed {
    pub database: Option<String>,
    pub command: Command,
}

/// Whether any stage, at any depth (`$lookup`, `$unionWith` and `$facet` sub-pipelines), writes.
pub(crate) fn pipeline_writes(pipeline: &[Document]) -> bool {
    fn writes(value: &Bson) -> bool {
        match value {
            Bson::Document(document) => document_writes(document),
            Bson::Array(items) => items.iter().any(writes),
            _ => false,
        }
    }
    fn document_writes(document: &Document) -> bool {
        document
            .iter()
            .any(|(key, inner)| key == "$out" || key == "$merge" || writes(inner))
    }
    pipeline.iter().any(document_writes)
}

/// The databases a pipeline names for a stage that writes or reads outside the command's own
/// database, at any depth: `$out: {db, coll}`, `$merge: {into: {db, coll}}`, `$lookup` and
/// `$graphLookup` `from: {db, coll}`, `$unionWith: {coll: {db, coll}}`. A `db` that is not a plain
/// string is reported as an empty name, which no allow-list holds.
pub(crate) fn pipeline_databases(pipeline: &[Document]) -> Vec<String> {
    const STAGES: &[&str] = &["$out", "$merge", "$lookup", "$unionWith", "$graphLookup"];
    fn named(value: &Bson, out: &mut Vec<String>) {
        if let Bson::Document(document) = value {
            if let Some(database) = document.get("db") {
                out.push(match database {
                    Bson::String(name) => name.clone(),
                    _ => String::new(),
                });
            }
        }
    }
    fn any_in(value: &Bson, out: &mut Vec<String>) {
        match value {
            Bson::Document(document) => document_in(document, out),
            Bson::Array(items) => items.iter().for_each(|item| any_in(item, out)),
            _ => {}
        }
    }
    fn document_in(document: &Document, out: &mut Vec<String>) {
        for (key, inner) in document {
            if STAGES.contains(&key.as_str()) {
                named(inner, out);
                if let Bson::Document(stage) = inner {
                    for field in ["into", "from", "coll"] {
                        if let Some(target) = stage.get(field) {
                            named(target, out);
                        }
                    }
                }
            }
            any_in(inner, out);
        }
    }
    let mut out = Vec::new();
    pipeline.iter().for_each(|stage| document_in(stage, &mut out));
    out
}

// ---------------------------------------------------------------- scanning

fn skip_string(bytes: &[u8], start: usize) -> Option<usize> {
    let quote = bytes[start];
    let mut i = start + 1;
    while i < bytes.len() {
        match bytes[i] {
            b'\\' => i += 2,
            byte if byte == quote => return Some(i + 1),
            _ => i += 1,
        }
    }
    None
}

fn skip_regex(bytes: &[u8], start: usize) -> Option<usize> {
    let mut i = start + 1;
    let mut class = false;
    while i < bytes.len() {
        match bytes[i] {
            b'\\' => i += 2,
            b'[' => {
                class = true;
                i += 1;
            }
            b']' => {
                class = false;
                i += 1;
            }
            b'/' if !class => {
                i += 1;
                while i < bytes.len() && bytes[i].is_ascii_alphabetic() {
                    i += 1;
                }
                return Some(i);
            }
            b'\n' => return None,
            _ => i += 1,
        }
    }
    None
}

/// The end of a comment starting at `i`, or `None` when none starts there.
fn skip_comment(bytes: &[u8], i: usize) -> Option<usize> {
    match (bytes.get(i)?, bytes.get(i + 1)?) {
        (b'/', b'/') => Some(bytes[i..].iter().position(|b| *b == b'\n').map_or(bytes.len(), |n| i + n)),
        (b'/', b'*') => bytes[i + 2..]
            .windows(2)
            .position(|pair| pair == b"*/")
            .map(|n| i + 2 + n + 2),
        _ => None,
    }
}

/// Skips whitespace and comments.
fn skip_trivia(bytes: &[u8], mut i: usize) -> usize {
    loop {
        while i < bytes.len() && bytes[i].is_ascii_whitespace() {
            i += 1;
        }
        match skip_comment(bytes, i) {
            Some(end) => i = end,
            None => return i,
        }
    }
}

/// Walks `bytes[from..to]` at the top level of a bracket group, calling `on` with the index of
/// every byte that is outside strings, comments and regular expressions, and the depth before it.
fn walk(bytes: &[u8], from: usize, to: usize, mut on: impl FnMut(usize, usize)) -> Result<(), String> {
    let mut i = from;
    let mut depth = 0usize;
    let mut previous: Option<u8> = None;
    while i < to {
        let byte = bytes[i];
        if let Some(end) = skip_comment(bytes, i) {
            i = end;
            continue;
        }
        match byte {
            b'"' | b'\'' | b'`' => {
                i = skip_string(bytes, i).ok_or("a string is not closed")?;
                previous = Some(b'"');
                continue;
            }
            b'/' if previous.is_none_or(|p| b"(,:[{=!&|?;".contains(&p)) => {
                i = skip_regex(bytes, i).ok_or("a regular expression is not closed")?;
                previous = Some(b'"');
                continue;
            }
            b'(' | b'[' | b'{' => {
                on(i, depth);
                depth += 1;
            }
            b')' | b']' | b'}' => {
                depth = depth.checked_sub(1).ok_or("brackets do not match")?;
                on(i, depth);
            }
            _ => on(i, depth),
        }
        if !byte.is_ascii_whitespace() {
            previous = Some(byte);
        }
        i += 1;
    }
    if depth != 0 {
        return Err("brackets do not match".into());
    }
    Ok(())
}

/// The index of the bracket closing the one at `open`.
fn matching(text: &str, open: usize) -> Result<usize, String> {
    let bytes = text.as_bytes();
    let mut depth = 0usize;
    let mut found = None;
    // Walk from the bracket to the end of the text; stop recording at the match.
    let end = bytes.len();
    let mut i = open;
    let mut previous: Option<u8> = None;
    while i < end {
        let byte = bytes[i];
        if let Some(next) = skip_comment(bytes, i) {
            i = next;
            continue;
        }
        match byte {
            b'"' | b'\'' | b'`' => {
                i = skip_string(bytes, i).ok_or("a string is not closed")?;
                previous = Some(b'"');
                continue;
            }
            b'/' if previous.is_none_or(|p| b"(,:[{=!&|?;".contains(&p)) => {
                i = skip_regex(bytes, i).ok_or("a regular expression is not closed")?;
                previous = Some(b'"');
                continue;
            }
            b'(' | b'[' | b'{' => depth += 1,
            b')' | b']' | b'}' => {
                depth -= 1;
                if depth == 0 {
                    found = Some(i);
                    break;
                }
            }
            _ => {}
        }
        if !byte.is_ascii_whitespace() {
            previous = Some(byte);
        }
        i += 1;
    }
    found.ok_or_else(|| "brackets do not match".to_string())
}

/// The arguments between the brackets of a call, split at the commas at the top level.
fn split_arguments(inner: &str) -> Result<Vec<String>, String> {
    let bytes = inner.as_bytes();
    let mut pieces = Vec::new();
    let mut start = 0;
    let mut commas = Vec::new();
    walk(bytes, 0, bytes.len(), |i, depth| {
        if depth == 0 && bytes[i] == b',' {
            commas.push(i);
        }
    })?;
    for comma in commas {
        pieces.push(inner[start..comma].trim().to_owned());
        start = comma + 1;
    }
    pieces.push(inner[start..].trim().to_owned());
    // `f()` has no arguments; `f(a, b,)` has a trailing comma; any other empty piece is a mistake.
    if pieces.len() == 1 && pieces[0].is_empty() {
        return Ok(Vec::new());
    }
    if pieces.len() > 1 && pieces.last().is_some_and(String::is_empty) {
        pieces.pop();
    }
    if pieces.iter().any(String::is_empty) {
        return Err("an argument is empty".into());
    }
    Ok(pieces)
}

struct Seg {
    name: String,
    args: Option<Vec<String>>,
}

fn is_ident_start(ch: char) -> bool {
    ch.is_alphabetic() || ch == '_' || ch == '$'
}

fn is_ident(ch: char) -> bool {
    ch.is_alphanumeric() || ch == '_' || ch == '$'
}

/// `db.a.b(...).c[...](...)` as the names and the arguments of the calls.
fn segments(text: &str) -> Result<Vec<Seg>, String> {
    let bytes = text.as_bytes();
    let mut i = 2;
    let mut segs = Vec::new();
    loop {
        i = skip_trivia(bytes, i);
        if i >= bytes.len() {
            return Ok(segs);
        }
        let name = match bytes[i] {
            b'.' => {
                i = skip_trivia(bytes, i + 1);
                let rest = &text[i..];
                let mut chars = rest.char_indices();
                match chars.next() {
                    Some((_, ch)) if is_ident_start(ch) => {}
                    _ => return Err("a name is expected after `.`".into()),
                }
                let end = rest
                    .char_indices()
                    .find(|(_, ch)| !is_ident(*ch))
                    .map_or(rest.len(), |(at, _)| at);
                i += end;
                rest[..end].to_owned()
            }
            b'[' => {
                let close = matching(text, i)?;
                let name = string_value(text[i + 1..close].trim())
                    .map_err(|_| "`[...]` takes a quoted collection name, e.g. db[\"orders\"]".to_string())?;
                i = close + 1;
                name
            }
            b'-' => {
                return Err("a name with `-` cannot follow a dot; write db.getCollection(\"the-name\")".into())
            }
            _ => {
                let shown: String = text[i..].chars().take(20).collect();
                return Err(format!("unexpected text `{shown}`; give exactly one command"));
            }
        };
        let after = skip_trivia(bytes, i);
        if bytes.get(after) == Some(&b'(') {
            let close = matching(text, after)?;
            segs.push(Seg {
                name,
                args: Some(split_arguments(&text[after + 1..close])?),
            });
            i = close + 1;
        } else {
            segs.push(Seg { name, args: None });
        }
    }
}

// ------------------------------------------------------------------ values

fn value(text: &str, typing: NumberTyping) -> Result<Bson, String> {
    parse_value(text.trim(), typing).map_err(|error| error.to_string())
}

fn document(text: &str, typing: NumberTyping) -> Result<Document, String> {
    match value(text, typing)? {
        Bson::Document(document) => Ok(document),
        _ => Err(format!("expected a document {{ ... }}, found `{}`", clip(text))),
    }
}

fn clip(text: &str) -> String {
    text.chars().take(40).collect()
}

fn string_value(text: &str) -> Result<String, String> {
    match value(text, NumberTyping::Mongosh)? {
        Bson::String(text) => Ok(text),
        _ => Err(format!("expected a quoted string, found `{}`", clip(text))),
    }
}

fn integer(text: &str, what: &str) -> Result<i64, String> {
    match value(text, NumberTyping::Filter)? {
        Bson::Int32(n) => Ok(i64::from(n)),
        Bson::Int64(n) => Ok(n),
        Bson::Double(n) if n.fract() == 0.0 && n.abs() < 9e15 => Ok(n as i64),
        _ => Err(format!("{what} takes an integer")),
    }
}

/// A filter (and other query documents): an integer beyond Int32 is an Int64.
fn filter(text: &str) -> Result<Document, String> {
    document(text, NumberTyping::Filter)
}

/// A document that is stored or applied (insert, update): typed like the shell types it.
fn stored(text: &str) -> Result<Document, String> {
    document(text, NumberTyping::Mongosh)
}

fn arity(method: &str, args: &[String], min: usize, max: usize) -> Result<(), String> {
    if args.len() < min || args.len() > max {
        return Err(if min == max {
            format!("{method}() takes {min} argument(s), not {}", args.len())
        } else {
            format!("{method}() takes {min} to {max} arguments, not {}", args.len())
        });
    }
    Ok(())
}

fn no_chain(method: &str, chain: &[Seg]) -> Result<(), String> {
    match chain.first() {
        None => Ok(()),
        Some(seg) => Err(format!("{method}() cannot be followed by .{}()", seg.name)),
    }
}

fn optional_filter(args: &[String], at: usize) -> Result<Document, String> {
    args.get(at).map_or_else(|| Ok(Document::new()), |text| filter(text))
}

/// The options document of a call, only the keys in `allowed`.
fn options(method: &str, text: Option<&String>, allowed: &[&str]) -> Result<Document, String> {
    let Some(text) = text else {
        return Ok(Document::new());
    };
    let options = document(text, NumberTyping::Filter)?;
    if let Some(key) = options.keys().find(|key| !allowed.contains(&key.as_str())) {
        return Err(format!(
            "{method}() does not take the option `{key}` here; supported: {}",
            allowed.join(", ")
        ));
    }
    Ok(options)
}

fn boolean_option(options: &Document, key: &str) -> Result<Option<bool>, String> {
    match options.get(key) {
        None => Ok(None),
        Some(Bson::Boolean(value)) => Ok(Some(*value)),
        Some(_) => Err(format!("the option `{key}` must be true or false")),
    }
}

// --------------------------------------------------------------- the parse

/// Reads one command. `Err` is the message for the model.
pub(crate) fn parse(input: &str) -> Result<Parsed, String> {
    let text = input.trim();
    let text = text.strip_suffix(';').unwrap_or(text).trim_end();
    if text.is_empty() {
        return Err("the command is empty".into());
    }
    let words: Vec<&str> = text.split_whitespace().collect();
    if words.first() == Some(&"show") {
        let command = match words.as_slice() {
            ["show", "dbs" | "databases"] => Command::ShowDatabases,
            ["show", "collections" | "tables"] => Command::ShowCollections,
            _ => return Err(format!("only `show dbs` and `show collections` are supported; supported commands: {SUPPORTED}")),
        };
        return Ok(Parsed { database: None, command });
    }
    if words.first() == Some(&"use") {
        let name = match words.as_slice() {
            ["use", name] => name.trim_matches(['"', '\'']),
            _ => return Err("`use` takes one database name".into()),
        };
        if name.is_empty() {
            return Err("`use` takes one database name".into());
        }
        return Ok(Parsed {
            database: None,
            command: Command::Use(name.to_owned()),
        });
    }
    let starts_with_db = text.strip_prefix("db").is_some_and(|rest| {
        rest.chars().next().is_some_and(|ch| ch == '.' || ch == '[' || ch.is_whitespace())
    });
    if !starts_with_db {
        return Err(format!(
            "a MongoDB command starts with `db.` (for example db.orders.find({{}})); supported commands: {SUPPORTED}"
        ));
    }
    interpret(segments(text)?)
}

fn string_arg(args: &[String], method: &str) -> Result<String, String> {
    arity(method, args, 1, 1)?;
    string_value(&args[0])
}

fn interpret(mut segs: Vec<Seg>) -> Result<Parsed, String> {
    let mut database = None;
    if segs.first().is_some_and(|seg| seg.name == "getSiblingDB" && seg.args.is_some()) {
        let seg = segs.remove(0);
        let name = string_arg(seg.args.as_deref().unwrap_or_default(), "getSiblingDB")?;
        if name.is_empty() {
            return Err("getSiblingDB() takes a database name".into());
        }
        database = Some(name);
    }
    let Some(first) = segs.first() else {
        return Err(if database.is_some() {
            "getSiblingDB() must be followed by a collection or database method".into()
        } else {
            "write db.<collection>.<method>(...) or db.<method>()".into()
        });
    };
    let (collection, rest) = match (&first.args, first.name.as_str()) {
        (Some(args), "getCollection") => (string_arg(args, "getCollection")?, &segs[1..]),
        (None, _) => (first.name.clone(), &segs[1..]),
        (Some(args), name) => {
            no_chain(name, &segs[1..])?;
            let command = database_method(name, args)?;
            return Ok(Parsed { database, command });
        }
    };
    if collection.is_empty() {
        return Err("the collection name is empty".into());
    }
    let (Some(method), Some(args)) = (rest.first(), rest.first().and_then(|seg| seg.args.as_ref())) else {
        return Err("a collection method is required, e.g. db.<collection>.find({})".into());
    };
    let command = collection_method(&collection, &method.name, args, &rest[1..])?;
    Ok(Parsed { database, command })
}

fn database_method(name: &str, args: &[String]) -> Result<Command, String> {
    let none = |command: Command| arity(name, args, 0, 0).map(|()| command);
    match name {
        "version" => none(Command::Version),
        "getCollectionNames" => none(Command::ShowCollections),
        "stats" => arity(name, args, 0, 1).map(|()| Command::DbStats),
        "runCommand" => Ok(Command::Refused("runCommand")),
        "adminCommand" => Ok(Command::Refused("adminCommand")),
        "createUser" => Ok(Command::Refused("createUser")),
        "dropDatabase" => Ok(Command::Refused("dropDatabase")),
        _ => Err(format!("db.{name}() is not supported; supported commands: {SUPPORTED}")),
    }
}

fn find_spec(collection: &str, args: &[String]) -> Result<FindSpec, String> {
    Ok(FindSpec {
        collection: collection.to_owned(),
        filter: optional_filter(args, 0)?,
        projection: args.get(1).map(|text| filter(text)).transpose()?,
        sort: None,
        collation: None,
        skip: 0,
        limit: None,
    })
}

fn explain_verbosity(args: &[String]) -> Result<String, String> {
    arity("explain", args, 0, 1)?;
    let verbosity = args.first().map_or_else(|| Ok("queryPlanner".to_owned()), |text| string_value(text))?;
    match verbosity.as_str() {
        "queryPlanner" | "executionStats" | "allPlansExecution" => Ok(verbosity),
        _ => Err("explain() takes queryPlanner, executionStats or allPlansExecution".into()),
    }
}

fn find_chain(mut spec: FindSpec, chain: &[Seg]) -> Result<Command, String> {
    for (index, seg) in chain.iter().enumerate() {
        let args = seg
            .args
            .as_deref()
            .ok_or_else(|| format!("`.{}` after find() must be a call", seg.name))?;
        match seg.name.as_str() {
            "sort" => {
                arity("sort", args, 1, 1)?;
                spec.sort = Some(filter(&args[0])?);
            }
            "project" => {
                arity("project", args, 1, 1)?;
                spec.projection = Some(filter(&args[0])?);
            }
            "collation" => {
                arity("collation", args, 1, 1)?;
                spec.collation = Some(filter(&args[0])?);
            }
            "skip" => {
                arity("skip", args, 1, 1)?;
                spec.skip = u64::try_from(integer(&args[0], "skip()")?)
                    .map_err(|_| "skip() takes a non-negative integer".to_string())?;
            }
            "limit" => {
                arity("limit", args, 1, 1)?;
                // A negative limit is a limit with one batch in the shell.
                let limit = integer(&args[0], "limit()")?.unsigned_abs();
                spec.limit = (limit > 0).then_some(limit);
            }
            "count" if args.is_empty() => {
                return Ok(Command::Count {
                    collection: spec.collection,
                    filter: Some(spec.filter),
                });
            }
            "explain" => {
                if index + 1 != chain.len() {
                    return Err("explain() must be the last call of the chain".into());
                }
                let verbosity = explain_verbosity(args)?;
                return Ok(Command::Find {
                    spec: Box::new(spec),
                    one: false,
                    explain: Some(verbosity),
                });
            }
            // Results are always collected: these change nothing.
            "toArray" | "pretty" if args.is_empty() => {}
            "batchSize" => arity("batchSize", args, 1, 1)?,
            other => return Err(format!("find().{other}() is not supported; use sort, skip, limit, project, collation, count, explain or toArray")),
        }
    }
    Ok(Command::Find {
        spec: Box::new(spec),
        one: false,
        explain: None,
    })
}

fn collection_method(collection: &str, method: &str, args: &[String], chain: &[Seg]) -> Result<Command, String> {
    let collection_name = collection.to_owned();
    if method == "find" {
        arity(method, args, 0, 2)?;
        return find_chain(find_spec(collection, args)?, chain);
    }
    if method == "aggregate" {
        arity(method, args, 1, 2)?;
        for seg in chain {
            if !(matches!(seg.name.as_str(), "toArray" | "pretty") && seg.args.as_ref().is_some_and(Vec::is_empty)) {
                return Err(format!(
                    "aggregate().{}() is not supported; put $sort and $limit in the pipeline",
                    seg.name
                ));
            }
        }
        let Bson::Array(stages) = value(&args[0], NumberTyping::Filter)? else {
            return Err("aggregate() takes a pipeline array".into());
        };
        let pipeline = stages
            .into_iter()
            .map(|stage| match stage {
                Bson::Document(stage) => Ok(stage),
                _ => Err("every stage of the pipeline is a document".to_string()),
            })
            .collect::<Result<Vec<_>, _>>()?;
        let options = options(method, args.get(1), &["allowDiskUse", "collation", "hint", "comment"])?;
        return Ok(Command::Aggregate {
            collection: collection_name,
            pipeline,
            options,
        });
    }
    no_chain(method, chain)?;
    match method {
        "findOne" => {
            arity(method, args, 0, 3)?;
            let mut spec = find_spec(collection, args)?;
            let extra = options(method, args.get(2), &["sort", "skip", "collation"])?;
            spec.sort = extra.get_document("sort").ok().cloned();
            spec.collation = extra.get_document("collation").ok().cloned();
            spec.skip = match extra.get("skip") {
                None => 0,
                Some(Bson::Int32(n)) if *n >= 0 => *n as u64,
                Some(Bson::Int64(n)) if *n >= 0 => *n as u64,
                Some(_) => return Err("the option `skip` takes a non-negative integer".into()),
            };
            spec.limit = Some(1);
            Ok(Command::Find {
                spec: Box::new(spec),
                one: true,
                explain: None,
            })
        }
        "countDocuments" | "count" => {
            arity(method, args, 0, 2)?;
            options(method, args.get(1), &[])?;
            Ok(Command::Count {
                collection: collection_name,
                filter: Some(optional_filter(args, 0)?),
            })
        }
        "estimatedDocumentCount" => {
            arity(method, args, 0, 1)?;
            options(method, args.first(), &[])?;
            Ok(Command::Count {
                collection: collection_name,
                filter: None,
            })
        }
        "distinct" => {
            arity(method, args, 1, 2)?;
            Ok(Command::Distinct {
                collection: collection_name,
                field: string_value(&args[0])?,
                filter: args.get(1).map(|text| filter(text)).transpose()?,
            })
        }
        "getIndexes" => {
            arity(method, args, 0, 0)?;
            Ok(Command::GetIndexes {
                collection: collection_name,
            })
        }
        "stats" | "dataSize" | "storageSize" | "totalIndexSize" => {
            arity(method, args, 0, 1)?;
            // `stats(1024)` or `stats({ scale: 1024 })`.
            let scale = match args.first().map(|text| value(text, NumberTyping::Filter)).transpose()? {
                None => None,
                Some(Bson::Document(options)) => options
                    .get("scale")
                    .map(|scale| integer(&scale.to_string(), "scale"))
                    .transpose()?,
                Some(other) => Some(integer(&other.to_string(), "scale")?),
            };
            if scale.is_some_and(|scale| scale < 1) {
                return Err("the scale must be at least 1".into());
            }
            let metric = match method {
                "dataSize" => Some("size"),
                "storageSize" => Some("storageSize"),
                "totalIndexSize" => Some("totalIndexSize"),
                _ => None,
            };
            Ok(Command::CollectionStats {
                collection: collection_name,
                metric,
                scale,
            })
        }
        "insertOne" | "insertMany" | "insert" => insert(collection, method, args),
        "updateOne" | "updateMany" | "update" => update(collection, method, args),
        "replaceOne" => {
            arity(method, args, 2, 3)?;
            let replacement = stored(&args[1])?;
            let opts = options(method, args.get(2), &["upsert", "collation", "hint"])?;
            let spec = update_spec(filter(&args[0])?, replacement_value(replacement)?, false, &opts)?;
            Ok(single_update(collection, method, spec))
        }
        "deleteOne" | "deleteMany" => {
            arity(method, args, 1, 2)?;
            let many = method == "deleteMany";
            let opts = options(method, args.get(1), &["collation", "hint"])?;
            let spec = delete_spec(filter(&args[0])?, many, &opts);
            Ok(single_delete(collection, method, spec))
        }
        "bulkWrite" => bulk_write(collection, args),
        "findOneAndUpdate" | "findOneAndReplace" => {
            arity(method, args, 2, 3)?;
            let change = if method == "findOneAndUpdate" {
                update_value(stored_value(&args[1])?)?
            } else {
                replacement_value(stored(&args[1])?)?
            };
            find_and_modify(collection, method, filter(&args[0])?, Some(change), args.get(2))
        }
        "findOneAndDelete" => {
            arity(method, args, 1, 2)?;
            find_and_modify(collection, method, filter(&args[0])?, None, args.get(1))
        }
        "createIndex" => {
            arity(method, args, 1, 2)?;
            create_index(collection, &args[0], args.get(1))
        }
        "dropIndex" | "dropIndexes" => {
            arity(method, args, usize::from(method == "dropIndex"), 1)?;
            let target = match args.first() {
                None => Bson::String("*".into()),
                Some(text) => match value(text, NumberTyping::Mongosh)? {
                    target @ (Bson::String(_) | Bson::Document(_)) => target,
                    Bson::Array(names) if method == "dropIndexes" && names.iter().all(|n| matches!(n, Bson::String(_))) => Bson::Array(names),
                    _ => return Err(format!("{method}() takes an index name or its key document")),
                },
            };
            Ok(Command::Write {
                collection: collection_name.clone(),
                method: if method == "dropIndex" { "dropIndex" } else { "dropIndexes" },
                steps: vec![doc! { "dropIndexes": collection_name, "index": target }],
                ordered: true,
                dangerous: Some("dropping an index makes the queries that use it slow, and it can take a long time to build again".into()),
            })
        }
        "drop" => {
            arity(method, args, 0, 0)?;
            Ok(Command::Write {
                collection: collection_name.clone(),
                method: "drop",
                steps: vec![doc! { "drop": collection_name }],
                ordered: true,
                dangerous: Some("dropping a collection deletes all its documents and indexes".into()),
            })
        }
        "renameCollection" => {
            if args.len() > 1 {
                return Err("renameCollection() does not take dropTarget; drop the target collection first".into());
            }
            let new_name = string_arg(args, method)?;
            if new_name.is_empty() || new_name.contains('$') || new_name == collection {
                return Err("renameCollection() takes a new name that is valid and differs from the current one".into());
            }
            Ok(Command::Rename {
                collection: collection_name,
                new_name,
            })
        }
        other => Err(format!(
            "{other}() is not supported on a collection; supported commands: {SUPPORTED}"
        )),
    }
}

fn stored_value(text: &str) -> Result<Bson, String> {
    value(text, NumberTyping::Mongosh)
}

/// An update: operators (`$set`, ...) or a pipeline.
fn update_value(update: Bson) -> Result<Bson, String> {
    match update {
        Bson::Document(operators) => {
            if operators.is_empty() || operators.keys().any(|key| !key.starts_with('$')) {
                return Err("the update must use operators such as {$set: {...}}; to replace a whole document use replaceOne".into());
            }
            Ok(Bson::Document(operators))
        }
        Bson::Array(stages) if !stages.is_empty() && stages.iter().all(|stage| matches!(stage, Bson::Document(_))) => {
            Ok(Bson::Array(stages))
        }
        _ => Err("the update is an operator document {$set: {...}} or a pipeline array".into()),
    }
}

fn replacement_value(replacement: Document) -> Result<Bson, String> {
    if let Some(key) = replacement.keys().find(|key| key.starts_with('$')) {
        return Err(format!(
            "a replacement document cannot hold the operator {key}; use updateOne to change fields"
        ));
    }
    Ok(Bson::Document(replacement))
}

fn insert(collection: &str, method: &str, args: &[String]) -> Result<Command, String> {
    arity(method, args, 1, 2)?;
    let given = value(&args[0], NumberTyping::Mongosh)?;
    let documents: Vec<Bson> = match (method, given) {
        ("insertOne", document @ Bson::Document(_)) | ("insert", document @ Bson::Document(_)) => vec![document],
        ("insertMany" | "insert", Bson::Array(items)) => items,
        ("insertOne", _) => return Err("insertOne() takes a document".into()),
        ("insertMany", _) => return Err("insertMany() takes an array of documents".into()),
        _ => return Err("insert() takes a document or an array of documents".into()),
    };
    if documents.is_empty() || documents.len() > MAX_INSERT_DOCUMENTS {
        return Err(format!(
            "an insert takes between 1 and {MAX_INSERT_DOCUMENTS} documents, not {}",
            documents.len()
        ));
    }
    if documents.iter().any(|document| !matches!(document, Bson::Document(_))) {
        return Err("every document to insert is a document {...}".into());
    }
    let opts = options(method, args.get(1), &["ordered"])?;
    let ordered = boolean_option(&opts, "ordered")?.unwrap_or(true);
    Ok(Command::Write {
        collection: collection.to_owned(),
        method: static_name(method),
        steps: vec![doc! { "insert": collection, "documents": documents, "ordered": ordered }],
        ordered,
        dangerous: None,
    })
}

fn static_name(method: &str) -> &'static str {
    [
        "insertOne", "insertMany", "insert", "updateOne", "updateMany", "update", "replaceOne",
        "deleteOne", "deleteMany", "bulkWrite", "findOneAndUpdate", "findOneAndReplace",
        "findOneAndDelete", "createIndex",
    ]
    .into_iter()
    .find(|name| *name == method)
    .unwrap_or("write")
}

/// One entry of the `updates` array of an `update` command.
fn update_spec(filter: Document, update: Bson, many: bool, opts: &Document) -> Result<Document, String> {
    let mut spec = doc! { "q": filter, "u": update, "multi": many };
    spec.insert("upsert", boolean_option(opts, "upsert")?.unwrap_or(false));
    if let Some(filters) = opts.get("arrayFilters") {
        if !matches!(filters, Bson::Array(_)) {
            return Err("the option `arrayFilters` takes an array".into());
        }
        spec.insert("arrayFilters", filters.clone());
    }
    if let Some(collation) = opts.get("collation") {
        if !matches!(collation, Bson::Document(_)) {
            return Err("the option `collation` takes a document".into());
        }
        spec.insert("collation", collation.clone());
    }
    if let Some(hint) = opts.get("hint") {
        if !matches!(hint, Bson::Document(_) | Bson::String(_)) {
            return Err("the option `hint` takes an index name or key document".into());
        }
        spec.insert("hint", hint.clone());
    }
    Ok(spec)
}

fn delete_spec(filter: Document, many: bool, opts: &Document) -> Document {
    let mut spec = doc! { "q": filter, "limit": i32::from(!many) };
    for key in ["collation", "hint"] {
        if let Some(value) = opts.get(key) {
            spec.insert(key, value.clone());
        }
    }
    spec
}

/// Why a change with `many` set (every match, not the first) on this `q` is dangerous.
fn spec_danger(spec: &Document, many_key: &str) -> Option<String> {
    let many = match spec.get(many_key) {
        Some(Bson::Boolean(many)) => *many,
        Some(Bson::Int32(limit)) => *limit == 0,
        _ => false,
    };
    if !many {
        return None;
    }
    spec.get_document("q").ok().and_then(crate::mongo_filter::danger)
}

fn single_update(collection: &str, method: &str, spec: Document) -> Command {
    let dangerous = spec_danger(&spec, "multi");
    Command::Write {
        collection: collection.to_owned(),
        method: static_name(method),
        steps: vec![doc! { "update": collection, "updates": [spec], "ordered": true }],
        ordered: true,
        dangerous,
    }
}

fn single_delete(collection: &str, method: &str, spec: Document) -> Command {
    let dangerous = spec_danger(&spec, "limit");
    Command::Write {
        collection: collection.to_owned(),
        method: static_name(method),
        steps: vec![doc! { "delete": collection, "deletes": [spec], "ordered": true }],
        ordered: true,
        dangerous,
    }
}

fn update(collection: &str, method: &str, args: &[String]) -> Result<Command, String> {
    arity(method, args, 2, 3)?;
    let update = update_value(stored_value(&args[1])?)?;
    // The legacy `update()` takes `multi` in its options.
    let allowed: &[&str] = if method == "update" {
        &["upsert", "multi", "arrayFilters", "collation", "hint"]
    } else {
        &["upsert", "arrayFilters", "collation", "hint"]
    };
    let opts = options(method, args.get(2), allowed)?;
    let many = method == "updateMany" || boolean_option(&opts, "multi")?.unwrap_or(false);
    let spec = update_spec(filter(&args[0])?, update, many, &opts)?;
    Ok(single_update(collection, method, spec))
}

fn bulk_write(collection: &str, args: &[String]) -> Result<Command, String> {
    arity("bulkWrite", args, 1, 2)?;
    let Bson::Array(operations) = value(&args[0], NumberTyping::Mongosh)? else {
        return Err("bulkWrite() takes an array of operations".into());
    };
    if operations.is_empty() || operations.len() > MAX_BULK_OPERATIONS {
        return Err(format!(
            "bulkWrite() takes between 1 and {MAX_BULK_OPERATIONS} operations, not {}",
            operations.len()
        ));
    }
    let opts = options("bulkWrite", args.get(1), &["ordered"])?;
    let ordered = boolean_option(&opts, "ordered")?.unwrap_or(true);
    // Consecutive operations of one kind go in one command; the order of the commands is the
    // order of the operations.
    let mut groups: Vec<(&'static str, Vec<Bson>)> = Vec::new();
    let mut dangerous = None;
    for (index, operation) in operations.into_iter().enumerate() {
        let position = index + 1;
        let fail = |why: &str| format!("bulkWrite() operation {position}: {why}");
        let Bson::Document(operation) = operation else {
            return Err(fail("expected a document such as { insertOne: { document: {...} } }"));
        };
        if operation.len() != 1 {
            return Err(fail("expected exactly one operation key"));
        }
        let Some((kind, spec)) = operation.into_iter().next() else {
            return Err(fail("expected exactly one operation key"));
        };
        let Bson::Document(mut spec) = spec else {
            return Err(fail(&format!("`{kind}` takes a document")));
        };
        let mut take = |key: &str| spec.remove(key);
        let (group, entry): (&'static str, Bson) = match kind.as_str() {
            "insertOne" => {
                let Some(document @ Bson::Document(_)) = take("document") else {
                    return Err(fail("insertOne needs a `document`"));
                };
                ("insert", document)
            }
            "updateOne" | "updateMany" | "replaceOne" => {
                let Some(Bson::Document(filter)) = take("filter") else {
                    return Err(fail("a `filter` document is required"));
                };
                let change = if kind == "replaceOne" {
                    let Some(Bson::Document(replacement)) = take("replacement") else {
                        return Err(fail("replaceOne needs a `replacement` document"));
                    };
                    replacement_value(replacement).map_err(|why| fail(&why))?
                } else {
                    let Some(update) = take("update") else {
                        return Err(fail("an `update` is required"));
                    };
                    update_value(update).map_err(|why| fail(&why))?
                };
                let allowed: &[&str] = if kind == "replaceOne" {
                    &["upsert", "collation", "hint"]
                } else {
                    &["upsert", "arrayFilters", "collation", "hint"]
                };
                if let Some(key) = spec.keys().find(|key| !allowed.contains(&key.as_str())) {
                    return Err(fail(&format!("`{kind}` has no field `{key}`")));
                }
                let spec = update_spec(filter, change, kind == "updateMany", &spec).map_err(|why| fail(&why))?;
                dangerous = dangerous.or_else(|| spec_danger(&spec, "multi"));
                ("update", Bson::Document(spec))
            }
            "deleteOne" | "deleteMany" => {
                let Some(Bson::Document(filter)) = take("filter") else {
                    return Err(fail("a `filter` document is required"));
                };
                if let Some(key) = spec.keys().find(|key| !["collation", "hint"].contains(&key.as_str())) {
                    return Err(fail(&format!("`{kind}` has no field `{key}`")));
                }
                let spec = delete_spec(filter, kind == "deleteMany", &spec);
                dangerous = dangerous.or_else(|| spec_danger(&spec, "limit"));
                ("delete", Bson::Document(spec))
            }
            other => {
                return Err(fail(&format!(
                    "`{other}` is not supported; use insertOne, updateOne, updateMany, replaceOne, deleteOne or deleteMany"
                )))
            }
        };
        if kind == "insertOne" && !spec.is_empty() {
            return Err(fail("insertOne takes only `document`"));
        }
        match groups.last_mut() {
            Some((last, entries)) if *last == group => entries.push(entry),
            _ => groups.push((group, vec![entry])),
        }
    }
    let steps = groups
        .into_iter()
        .map(|(group, entries)| match group {
            "insert" => doc! { "insert": collection, "documents": entries, "ordered": ordered },
            "update" => doc! { "update": collection, "updates": entries, "ordered": ordered },
            _ => doc! { "delete": collection, "deletes": entries, "ordered": ordered },
        })
        .collect();
    Ok(Command::Write {
        collection: collection.to_owned(),
        method: "bulkWrite",
        steps,
        ordered,
        dangerous,
    })
}

fn find_and_modify(
    collection: &str,
    method: &str,
    query: Document,
    change: Option<Bson>,
    options_text: Option<&String>,
) -> Result<Command, String> {
    let mut allowed = vec!["sort", "projection", "collation", "hint"];
    if change.is_some() {
        allowed.extend(["upsert", "returnDocument", "arrayFilters"]);
    }
    let opts = options(method, options_text, &allowed)?;
    let mut command = doc! { "findAndModify": collection, "query": query };
    match change {
        Some(change) => {
            command.insert("update", change);
            let after = match opts.get("returnDocument") {
                None => false,
                Some(Bson::String(which)) if which == "after" => true,
                Some(Bson::String(which)) if which == "before" => false,
                Some(_) => return Err("the option `returnDocument` is \"before\" or \"after\"".into()),
            };
            command.insert("new", after);
            if let Some(upsert) = boolean_option(&opts, "upsert")? {
                command.insert("upsert", upsert);
            }
        }
        None => {
            command.insert("remove", true);
        }
    }
    for (from, to) in [
        ("sort", "sort"),
        ("projection", "fields"),
        ("collation", "collation"),
        ("hint", "hint"),
        ("arrayFilters", "arrayFilters"),
    ] {
        if let Some(value) = opts.get(from) {
            command.insert(to, value.clone());
        }
    }
    Ok(Command::Write {
        collection: collection.to_owned(),
        method: static_name(method),
        steps: vec![command],
        ordered: true,
        dangerous: None,
    })
}

fn create_index(collection: &str, keys: &str, options_text: Option<&String>) -> Result<Command, String> {
    let keys = document(keys, NumberTyping::Mongosh)?;
    if keys.is_empty() {
        return Err("createIndex() takes a key document such as {field: 1}".into());
    }
    let mut spec = Document::new();
    spec.insert("key", keys.clone());
    let mut opts = match options_text {
        Some(text) => document(text, NumberTyping::Filter)?,
        None => Document::new(),
    };
    if opts.contains_key("key") {
        return Err("the index options cannot hold `key`".into());
    }
    let name = match opts.remove("name") {
        Some(Bson::String(name)) if !name.is_empty() => name,
        Some(_) => return Err("the option `name` takes a non-empty string".into()),
        None => index_name(&keys)?,
    };
    spec.insert("name", name);
    spec.extend(opts);
    Ok(Command::Write {
        collection: collection.to_owned(),
        method: "createIndex",
        steps: vec![doc! { "createIndexes": collection, "indexes": [spec] }],
        ordered: true,
        dangerous: None,
    })
}

/// The name the shell gives an index: `a_1_b_-1`.
fn index_name(keys: &Document) -> Result<String, String> {
    let parts = keys
        .iter()
        .map(|(field, direction)| match direction {
            Bson::Int32(n) => Ok(format!("{field}_{n}")),
            Bson::Int64(n) => Ok(format!("{field}_{n}")),
            Bson::Double(n) if n.fract() == 0.0 => Ok(format!("{field}_{}", *n as i64)),
            Bson::String(kind) => Ok(format!("{field}_{kind}")),
            _ => Err(format!("the key `{field}` takes 1, -1 or an index type such as \"text\"")),
        })
        .collect::<Result<Vec<_>, _>>()?;
    Ok(parts.join("_"))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn command(text: &str) -> Command {
        parse(text).unwrap_or_else(|error| panic!("{text}: {error}")).command
    }

    fn fails(text: &str) -> String {
        parse(text).expect_err(text)
    }

    fn write_of(text: &str) -> (String, Vec<Document>, Option<String>) {
        match command(text) {
            Command::Write {
                method,
                steps,
                dangerous,
                ..
            } => (method.to_owned(), steps, dangerous),
            other => panic!("{text}: {other:?}"),
        }
    }

    #[test]
    fn the_shell_commands_of_the_session() {
        assert_eq!(command("show dbs"), Command::ShowDatabases);
        assert_eq!(command("show databases;"), Command::ShowDatabases);
        assert_eq!(command("show collections"), Command::ShowCollections);
        assert_eq!(command("db.getCollectionNames()"), Command::ShowCollections);
        assert_eq!(command("use shop"), Command::Use("shop".into()));
        assert_eq!(command("use 'shop'"), Command::Use("shop".into()));
        assert_eq!(command("db.version()"), Command::Version);
        assert_eq!(command("db.stats()"), Command::DbStats);
        assert!(fails("show users").contains("show dbs"));
        assert!(fails("use").contains("database name"));
    }

    #[test]
    fn find_reads_filter_projection_and_the_cursor_chain() {
        let Command::Find { spec, one, explain } = command(
            "db.orders.find({status: 'paid', total: {$gte: 10}}, {total: 1}).sort({created: -1}).skip(5).limit(20).toArray()",
        ) else {
            panic!("not a find")
        };
        assert!(!one && explain.is_none());
        assert_eq!(spec.collection, "orders");
        assert_eq!(spec.filter, doc! {"status": "paid", "total": {"$gte": 10}});
        assert_eq!(spec.projection, Some(doc! {"total": 1}));
        assert_eq!(spec.sort, Some(doc! {"created": -1}));
        assert_eq!((spec.skip, spec.limit), (5, Some(20)));
        // No arguments, a getCollection accessor, a bracket accessor and whitespace between calls.
        for text in [
            "db.orders.find()",
            "db.getCollection('orders').find()",
            "db['orders'].find()",
            "db . orders\n  .find( )\n  .pretty()",
            "db.orders.find({}) ;",
        ] {
            let Command::Find { spec, .. } = command(text) else { panic!("{text}") };
            assert_eq!(spec.collection, "orders", "{text}");
            assert!(spec.filter.is_empty(), "{text}");
        }
        // limit(0) is no limit; a negative limit is its size.
        let Command::Find { spec, .. } = command("db.c.find().limit(0)") else { panic!() };
        assert_eq!(spec.limit, None);
        let Command::Find { spec, .. } = command("db.c.find().limit(-3)") else { panic!() };
        assert_eq!(spec.limit, Some(3));
    }

    #[test]
    fn typed_values_regexes_and_comments_are_read_by_the_shell_value_parser() {
        let Command::Find { spec, .. } = command(
            r#"db.c.find({_id: ObjectId("507f1f77bcf86cd799439011"), at: {$gt: ISODate("2024-01-02T03:04:05Z")}, name: /^a,b/i, n: NumberLong(5), "q": 'x' /* why */})"#,
        ) else {
            panic!()
        };
        assert!(matches!(spec.filter.get("_id"), Some(Bson::ObjectId(_))));
        assert!(matches!(spec.filter.get_document("at").unwrap().get("$gt"), Some(Bson::DateTime(_))));
        assert!(matches!(spec.filter.get("name"), Some(Bson::RegularExpression(_))));
        assert_eq!(spec.filter.get("n"), Some(&Bson::Int64(5)));
        assert_eq!(spec.filter.get_str("q"), Ok("x"));
    }

    #[test]
    fn find_explain_count_and_findone() {
        let Command::Find { explain, spec, .. } = command("db.c.find({a: 1}).sort({a: 1}).explain('executionStats')") else { panic!() };
        assert_eq!(explain.as_deref(), Some("executionStats"));
        assert_eq!(spec.sort, Some(doc! {"a": 1}));
        let Command::Find { explain, .. } = command("db.c.find().explain()") else { panic!() };
        assert_eq!(explain.as_deref(), Some("queryPlanner"));
        assert!(fails("db.c.find().explain().limit(1)").contains("last call"));
        assert!(fails("db.c.find().explain('nope')").contains("queryPlanner"));
        assert_eq!(
            command("db.c.find({a: 1}).count()"),
            Command::Count { collection: "c".into(), filter: Some(doc! {"a": 1}) }
        );
        let Command::Find { spec, one, .. } = command("db.c.findOne({a: 1}, {b: 1}, {sort: {b: -1}})") else { panic!() };
        assert!(one);
        assert_eq!((spec.limit, spec.projection, spec.sort), (Some(1), Some(doc! {"b": 1}), Some(doc! {"b": -1})));
        assert!(fails("db.c.findOne({}).limit(1)").contains("cannot be followed"));
    }

    #[test]
    fn counts() {
        for (text, filter) in [
            ("db.c.countDocuments({a: 1})", Some(doc! {"a": 1})),
            ("db.c.count()", Some(doc! {})),
            ("db.c.estimatedDocumentCount()", None),
        ] {
            assert_eq!(command(text), Command::Count { collection: "c".into(), filter }, "{text}");
        }
        assert!(fails("db.c.countDocuments({}, {limit: 5})").contains("option"));
    }

    #[test]
    fn aggregate_distinct_indexes_and_stats() {
        let Command::Aggregate { pipeline, options, .. } =
            command("db.c.aggregate([{$match: {a: 1}}, {$group: {_id: '$b', n: {$sum: 1}}}], {allowDiskUse: true}).toArray()") else { panic!() };
        assert_eq!(pipeline.len(), 2);
        assert_eq!(options, doc! {"allowDiskUse": true});
        assert!(fails("db.c.aggregate([{$match: {}}]).limit(1)").contains("pipeline"));
        assert!(fails("db.c.aggregate({$match: {}})").contains("pipeline array"));
        assert!(fails("db.c.aggregate([1])").contains("document"));
        assert_eq!(
            command("db.c.distinct('a', {b: 1})"),
            Command::Distinct { collection: "c".into(), field: "a".into(), filter: Some(doc! {"b": 1}) }
        );
        assert_eq!(command("db.getCollection('c').getIndexes()"), Command::GetIndexes { collection: "c".into() });
        assert!(fails("db.c.getIndexes(1)").contains("argument"));
        assert_eq!(
            command("db.c.stats()"),
            Command::CollectionStats { collection: "c".into(), metric: None, scale: None }
        );
        assert_eq!(
            command("db.c.dataSize()"),
            Command::CollectionStats { collection: "c".into(), metric: Some("size"), scale: None }
        );
        assert_eq!(
            command("db.c.stats({scale: 1024})"),
            Command::CollectionStats { collection: "c".into(), metric: None, scale: Some(1024) }
        );
        assert_eq!(
            command("db.c.totalIndexSize(1024)"),
            Command::CollectionStats { collection: "c".into(), metric: Some("totalIndexSize"), scale: Some(1024) }
        );
    }

    #[test]
    fn a_sibling_database_wraps_any_command() {
        let parsed = parse("db.getSiblingDB('other').orders.find({})").unwrap();
        assert_eq!(parsed.database.as_deref(), Some("other"));
        assert!(matches!(parsed.command, Command::Find { .. }));
        let parsed = parse("db.getSiblingDB(\"other\").getCollection('x').getIndexes()").unwrap();
        assert_eq!(parsed.database.as_deref(), Some("other"));
        assert_eq!(parsed.command, Command::GetIndexes { collection: "x".into() });
        assert_eq!(parse("db.getSiblingDB('other').getCollectionNames()").unwrap().command, Command::ShowCollections);
        assert!(fails("db.getSiblingDB('other')").contains("followed"));
        assert!(fails("db.getSiblingDB().x.find()").contains("argument"));
    }

    #[test]
    fn inserts() {
        let (method, steps, danger) = write_of("db.c.insertOne({a: 1, d: ISODate('2024-01-01T00:00:00Z')})");
        assert_eq!((method.as_str(), danger), ("insertOne", None));
        assert_eq!(steps[0].get_str("insert"), Ok("c"));
        assert_eq!(steps[0].get_array("documents").unwrap().len(), 1);
        let (_, steps, _) = write_of("db.c.insertMany([{a: 1}, {a: 2}], {ordered: false})");
        assert_eq!(steps[0].get_array("documents").unwrap().len(), 2);
        assert_eq!(steps[0].get_bool("ordered"), Ok(false));
        let (_, steps, _) = write_of("db.c.insert([{a: 1}])");
        assert_eq!(steps[0].get_array("documents").unwrap().len(), 1);
        assert!(fails("db.c.insertOne([{a: 1}])").contains("takes a document"));
        assert!(fails("db.c.insertMany({a: 1})").contains("array"));
        assert!(fails("db.c.insertMany([])").contains("between 1 and"));
        assert!(fails("db.c.insertMany([1])").contains("every document"));
    }

    #[test]
    fn updates_and_replaces() {
        let (method, steps, danger) = write_of("db.c.updateOne({a: 1}, {$set: {b: 2}}, {upsert: true})");
        assert_eq!((method.as_str(), danger), ("updateOne", None));
        let spec = steps[0].get_array("updates").unwrap()[0].as_document().unwrap().clone();
        assert_eq!(spec.get_bool("multi"), Ok(false));
        assert_eq!(spec.get_bool("upsert"), Ok(true));
        let (_, steps, danger) = write_of("db.c.updateMany({a: 1}, [{$set: {b: '$a'}}])");
        assert_eq!(danger, None);
        assert!(steps[0].get_array("updates").unwrap()[0].as_document().unwrap().get_array("u").is_ok());
        let (_, steps, _) = write_of("db.c.update({a: 1}, {$set: {b: 2}}, {multi: true})");
        assert_eq!(
            steps[0].get_array("updates").unwrap()[0].as_document().unwrap().get_bool("multi"),
            Ok(true)
        );
        let (_, steps, danger) = write_of("db.c.replaceOne({a: 1}, {a: 1, b: 3})");
        assert_eq!(danger, None);
        assert_eq!(
            steps[0].get_array("updates").unwrap()[0].as_document().unwrap().get_document("u"),
            Ok(&doc! {"a": 1, "b": 3})
        );
        assert!(fails("db.c.updateOne({a: 1}, {b: 2})").contains("operators"));
        assert!(fails("db.c.updateOne({a: 1}, {})").contains("operators"));
        assert!(fails("db.c.replaceOne({a: 1}, {$set: {b: 2}})").contains("operator"));
        assert!(fails("db.c.updateOne({a: 1}, {$set: {b: 2}}, {wat: 1})").contains("option"));
        assert!(fails("db.c.updateOne({a: 1}, {$set: {b: 2}}, {upsert: 'yes'})").contains("true or false"));
        assert!(fails("db.c.updateOne({a: 1})").contains("argument"));
    }

    #[test]
    fn deletes() {
        let (_, steps, danger) = write_of("db.c.deleteOne({a: 1})");
        assert_eq!(danger, None);
        let spec = steps[0].get_array("deletes").unwrap()[0].as_document().unwrap().clone();
        assert_eq!(spec.get_i32("limit"), Ok(1));
        let (_, steps, danger) = write_of("db.c.deleteMany({a: 1})");
        assert_eq!(danger, None);
        assert_eq!(steps[0].get_array("deletes").unwrap()[0].as_document().unwrap().get_i32("limit"), Ok(0));
    }

    #[test]
    fn a_many_change_with_an_empty_filter_is_dangerous_and_a_single_one_is_not() {
        for text in ["db.c.deleteMany({})", "db.c.updateMany({}, {$set: {a: 1}})", "db.c.update({}, {$set: {a: 1}}, {multi: true})", "db.c.bulkWrite([{deleteMany: {filter: {}}}])", "db.c.bulkWrite([{insertOne: {document: {a: 1}}}, {updateMany: {filter: {}, update: {$set: {a: 1}}}}])"] {
            assert!(write_of(text).2.is_some(), "{text}");
        }
        for text in ["db.c.deleteOne({})", "db.c.updateOne({}, {$set: {a: 1}})", "db.c.replaceOne({}, {a: 1})", "db.c.deleteMany({a: 1})", "db.c.findOneAndDelete({})"] {
            assert!(write_of(text).2.is_none(), "{text}");
        }
    }

    #[test]
    fn a_many_change_on_a_filter_that_matches_everything_or_cannot_be_checked_is_dangerous() {
        for text in [
            "db.c.deleteMany({$where: 'true'})",
            "db.c.updateMany({$expr: {$gt: ['$a', 0]}}, {$set: {a: 1}})",
            "db.c.deleteMany({_id: {$exists: true}})",
            "db.c.updateMany({$or: [{a: 1}, {a: {$ne: 1}}]}, {$set: {b: 1}})",
            "db.c.update({_id: {$gte: MinKey()}}, {$set: {b: 1}}, {multi: true})",
            "db.c.bulkWrite([{insertOne: {document: {a: 1}}}, {deleteMany: {filter: {$nor: [{a: 1}]}}}])",
        ] {
            let (_, _, danger) = write_of(text);
            assert!(danger.is_some(), "{text}");
        }
        // One document, or a filter that names them: not dangerous.
        for text in [
            "db.c.deleteOne({_id: {$exists: true}})",
            "db.c.updateOne({$where: 'true'}, {$set: {b: 1}})",
            "db.c.deleteMany({_id: {$exists: false}})",
            "db.c.deleteMany({_id: {$in: [1, 2]}})",
            "db.c.findOneAndDelete({_id: {$exists: true}})",
        ] {
            assert!(write_of(text).2.is_none(), "{text}");
        }
        assert!(write_of("db.c.deleteMany({$where: 'true'})").2.unwrap().contains("$where"));
    }

    #[test]
    fn bulk_write_groups_consecutive_operations_in_order() {
        let (method, steps, danger) = write_of(
            "db.c.bulkWrite([{insertOne: {document: {a: 1}}}, {insertOne: {document: {a: 2}}}, {updateOne: {filter: {a: 1}, update: {$set: {b: 1}}, upsert: true}}, {replaceOne: {filter: {a: 2}, replacement: {a: 3}}}, {deleteOne: {filter: {a: 9}}}, {insertOne: {document: {a: 4}}}], {ordered: false})",
        );
        assert_eq!((method.as_str(), danger), ("bulkWrite", None));
        let names: Vec<&str> = steps.iter().map(|step| step.keys().next().unwrap().as_str()).collect();
        assert_eq!(names, ["insert", "update", "delete", "insert"]);
        assert_eq!(steps[0].get_array("documents").unwrap().len(), 2);
        assert_eq!(steps[1].get_array("updates").unwrap().len(), 2);
        assert_eq!(steps[1].get_bool("ordered"), Ok(false));
        assert!(fails("db.c.bulkWrite([])").contains("between 1 and"));
        assert!(fails("db.c.bulkWrite([{wat: {}}])").contains("operation 1"));
        assert!(fails("db.c.bulkWrite([{insertOne: {document: {}, x: 1}}])").contains("only `document`"));
        assert!(fails("db.c.bulkWrite([{updateOne: {filter: {}, update: {a: 1}}}])").contains("operators"));
        assert!(fails("db.c.bulkWrite([{deleteOne: {filter: {}, wat: 1}}])").contains("no field"));
        assert!(fails("db.c.bulkWrite([{a: 1, b: 2}])").contains("exactly one"));
    }

    #[test]
    fn find_and_modify_commands() {
        let (method, steps, _) = write_of(
            "db.c.findOneAndUpdate({a: 1}, {$inc: {n: 1}}, {returnDocument: 'after', upsert: true, sort: {a: 1}, projection: {n: 1}})",
        );
        assert_eq!(method, "findOneAndUpdate");
        let command = &steps[0];
        assert_eq!(command.get_str("findAndModify"), Ok("c"));
        assert_eq!(command.get_bool("new"), Ok(true));
        assert_eq!(command.get_bool("upsert"), Ok(true));
        assert_eq!(command.get_document("fields"), Ok(&doc! {"n": 1}));
        let (_, steps, _) = write_of("db.c.findOneAndReplace({a: 1}, {a: 2})");
        assert_eq!(steps[0].get_bool("new"), Ok(false));
        assert_eq!(steps[0].get_document("update"), Ok(&doc! {"a": 2}));
        let (_, steps, _) = write_of("db.c.findOneAndDelete({a: 1}, {sort: {a: -1}})");
        assert_eq!(steps[0].get_bool("remove"), Ok(true));
        assert!(fails("db.c.findOneAndDelete({a: 1}, {upsert: true})").contains("option"));
        assert!(fails("db.c.findOneAndUpdate({a: 1}, {$set: {a: 1}}, {returnDocument: 'x'})").contains("before"));
    }

    #[test]
    fn index_commands() {
        let (_, steps, danger) = write_of("db.c.createIndex({a: 1, b: -1}, {unique: true, expireAfterSeconds: 60})");
        assert_eq!(danger, None);
        let spec = steps[0].get_array("indexes").unwrap()[0].as_document().unwrap().clone();
        assert_eq!(spec.get_str("name"), Ok("a_1_b_-1"));
        assert_eq!(spec.get_bool("unique"), Ok(true));
        assert_eq!(spec.get_document("key"), Ok(&doc! {"a": 1, "b": -1}));
        let (_, steps, _) = write_of("db.c.createIndex({t: 'text'}, {name: 'search'})");
        assert_eq!(steps[0].get_array("indexes").unwrap()[0].as_document().unwrap().get_str("name"), Ok("search"));
        assert!(fails("db.c.createIndex({})").contains("key document"));
        assert!(fails("db.c.createIndex({a: true})").contains("takes 1, -1"));
        for (text, index) in [
            ("db.c.dropIndex('a_1')", Bson::String("a_1".into())),
            ("db.c.dropIndex({a: 1})", Bson::Document(doc! {"a": 1})),
            ("db.c.dropIndexes()", Bson::String("*".into())),
            ("db.c.dropIndexes('a_1')", Bson::String("a_1".into())),
            ("db.c.dropIndexes(['a_1', 'b_1'])", Bson::Array(vec!["a_1".into(), "b_1".into()])),
        ] {
            let (_, steps, danger) = write_of(text);
            assert_eq!(steps[0].get("index"), Some(&index), "{text}");
            assert!(danger.is_some(), "{text}");
        }
        assert!(fails("db.c.dropIndex()").contains("argument"));
        assert!(fails("db.c.dropIndexes(5)").contains("index name"));
    }

    #[test]
    fn drop_and_rename_are_dangerous() {
        let (method, steps, danger) = write_of("db.c.drop()");
        assert_eq!((method.as_str(), steps[0].get_str("drop")), ("drop", Ok("c")));
        assert!(danger.is_some());
        let parsed = command("db.c.renameCollection('d')");
        assert_eq!(parsed, Command::Rename { collection: "c".into(), new_name: "d".into() });
        assert!(matches!(parsed.class(), Class::Write(Some(_))));
        assert!(fails("db.c.renameCollection('d', true)").contains("dropTarget"));
        assert!(fails("db.c.renameCollection('c')").contains("differs"));
        assert!(fails("db.c.renameCollection('a$b')").contains("valid"));
    }

    #[test]
    fn the_refused_commands() {
        for (text, name) in [
            ("db.runCommand({ping: 1})", "runCommand"),
            ("db.adminCommand({listDatabases: 1})", "adminCommand"),
            ("db.createUser({user: 'u', pwd: 'p', roles: []})", "createUser"),
            ("db.dropDatabase()", "dropDatabase"),
            ("db.getSiblingDB('x').runCommand({ping: 1})", "runCommand"),
        ] {
            assert_eq!(command(text), Command::Refused(static_name_of(name)), "{text}");
            assert!(matches!(command(text).class(), Class::Refused(_)), "{text}");
        }
    }

    fn static_name_of(name: &str) -> &'static str {
        ["runCommand", "adminCommand", "createUser", "dropDatabase"]
            .into_iter()
            .find(|candidate| *candidate == name)
            .unwrap()
    }

    #[test]
    fn an_aggregate_that_writes_is_dangerous_at_any_depth() {
        for text in [
            "db.c.aggregate([{$match: {}}, {$out: 'x'}])",
            "db.c.aggregate([{$merge: {into: 'x'}}])",
            "db.c.aggregate([{$lookup: {from: 'o', pipeline: [{$out: 'x'}], as: 'a'}}])",
            "db.c.aggregate([{$facet: {a: [{$match: {}}], b: [{$merge: 'x'}]}}])",
            "db.c.aggregate([{$unionWith: {coll: 'o', pipeline: [{$out: 'x'}]}}])",
        ] {
            assert!(matches!(command(text).class(), Class::Write(Some(_))), "{text}");
        }
        assert_eq!(command("db.c.aggregate([{$match: {a: 1}}, {$lookup: {from: 'o', pipeline: [{$match: {}}], as: 'a'}}])").class(), Class::Read);
    }

    #[test]
    fn classes() {
        for text in [
            "show dbs", "show collections", "use x", "db.version()", "db.c.find()", "db.c.findOne()", "db.c.count()",
            "db.c.distinct('a')", "db.c.getIndexes()", "db.c.stats()", "db.c.aggregate([{$match: {}}])",
            "db.c.find().explain()", "db.getSiblingDB('x').c.find()",
        ] {
            assert_eq!(command(text).class(), Class::Read, "{text}");
        }
        for text in [
            "db.c.insertOne({})", "db.c.updateOne({a: 1}, {$set: {a: 2}})", "db.c.deleteOne({a: 1})", "db.c.createIndex({a: 1})",
            "db.c.findOneAndUpdate({a: 1}, {$set: {a: 2}})", "db.c.bulkWrite([{insertOne: {document: {}}}])",
        ] {
            assert_eq!(command(text).class(), Class::Write(None), "{text}");
        }
    }

    #[test]
    fn what_is_not_one_command_is_refused() {
        for text in [
            "",
            "   ;  ",
            "db",
            "db.c",
            "db.c.find({}); db.c.drop()",
            "db.c.find({})\ndb.c.drop()",
            "x = db.c.find()",
            "db.c.find({a: })",
            "db.c.find({a: 1}",
            "db.c.find('{a: 1}')",
            "db.c.nope()",
            "db.nope()",
            "db.c.find().nope()",
            "db.c.find({}).forEach(printjson)",
            "db.sales-orders.find()",
            "db.c.find({a: 1}).sort",
            "db.c.find({$where: 'sleep(10)'}).skip(-1)",
            "sh.status()",
            "load('x.js')",
        ] {
            let error = parse(text).expect_err(text);
            assert!(!error.is_empty(), "{text}");
        }
        assert!(fails("db.sales-orders.find()").contains("getCollection"));
        assert!(fails("db.c.nope()").contains("supported commands"));
    }

    #[test]
    fn strings_and_regexes_with_brackets_and_commas_do_not_confuse_the_scanner() {
        let Command::Find { spec, .. } = command(r#"db.c.find({a: "x), db.c.drop(", b: /[)]\//, c: 'it\'s'}).limit(1)"#) else { panic!() };
        assert_eq!(spec.filter.get_str("a"), Ok("x), db.c.drop("));
        assert_eq!(spec.limit, Some(1));
        let (_, steps, _) = write_of("db.c.insertOne({a: 'x;y'}) ;");
        assert_eq!(steps[0].get_array("documents").unwrap()[0].as_document().unwrap().get_str("a"), Ok("x;y"));
    }

    #[test]
    fn pipeline_databases_are_found_in_every_stage_that_can_name_one() {
        assert_eq!(pipeline_databases(&[doc! {"$out": {"db": "other", "coll": "x"}}]), ["other"]);
        assert_eq!(pipeline_databases(&[doc! {"$merge": {"into": {"db": "other", "coll": "x"}}}]), ["other"]);
        assert_eq!(
            pipeline_databases(&[doc! {"$lookup": {"from": {"db": "a", "coll": "x"}, "as": "y"}}, doc! {"$unionWith": {"coll": {"db": "b", "coll": "x"}}}]),
            ["a", "b"]
        );
        assert_eq!(
            pipeline_databases(&[doc! {"$facet": {"f": [{"$lookup": {"from": {"db": "deep", "coll": "x"}}}]}}]),
            ["deep"]
        );
        assert_eq!(pipeline_databases(&[doc! {"$out": {"db": {"$literal": "x"}, "coll": "x"}}]), [""]);
        // Same-database forms and a `db` field that is only data name no other database.
        assert!(pipeline_databases(&[
            doc! {"$out": "x"},
            doc! {"$merge": {"into": "x"}},
            doc! {"$lookup": {"from": "x"}},
            doc! {"$match": {"db": "other"}},
        ])
        .is_empty());
    }

    #[test]
    fn pipeline_writes_is_found_in_every_corner() {
        assert!(pipeline_writes(&[doc! {"$match": {}}, doc! {"$out": "x"}]));
        assert!(pipeline_writes(&[doc! {"$lookup": {"pipeline": [{"$merge": "x"}]}}]));
        assert!(!pipeline_writes(&[doc! {"$match": {"a": 1}}]));
    }
}
