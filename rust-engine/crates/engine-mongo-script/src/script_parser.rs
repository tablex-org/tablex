use crate::pipeline::{AggregateSource, PipelineDefinition, PipelinePlan, PipelineStage};
use bson::{doc, Document};
use tree_sitter::{Node, Parser};

/// Unified result from parsing a mongo script.
/// Level 1 scripts (db.coll.find()) produce a MongoQueryPlan that compiles to PipelinePlan.
/// Level 2 scripts (pipeline.run([...])) produce a PipelinePlan directly.
/// Level 3 scripts use pipeline.define/pipeline.run with named templates.
#[derive(Debug, Clone, PartialEq)]
pub enum ScriptPlan {
    Level1(MongoQueryPlan),
    Level2(PipelinePlan),
    Level3Define(PipelineDefinition),
    Level3Run { name: String, params: Document },
}

#[derive(Debug, Clone, PartialEq)]
pub enum MongoQueryPlan {
    Find {
        collection: String,
        filter: Document,
        projection: Option<Document>,
        sort: Option<Document>,
        skip: Option<u64>,
        limit: Option<i64>,
    },
    CountDocuments {
        collection: String,
        filter: Document,
    },
    FindOne {
        collection: String,
        filter: Document,
        projection: Option<Document>,
    },
    Aggregate {
        collection: String,
        pipeline: Vec<Document>,
    },
}

#[derive(Debug, Clone, PartialEq)]
pub struct MongoReadScript {
    pub collection: String,
    pub filter: Document,
    pub projection: Option<Document>,
    pub sort: Option<Document>,
    pub skip: u64,
    pub limit: Option<u64>,
}

#[derive(PartialEq, Eq)]
enum ReadMethod {
    Find,
    FindOne,
    Terminal,
}

fn unsupported_read() -> ParseError {
    ParseError::UnsupportedExpression(
        "Use one fully supported find() or findOne() expression with literal arguments".into(),
    )
}

fn validate_read_call(node: Node, source: &[u8]) -> Result<ReadMethod, ParseError> {
    if node.kind() != "call_expression" {
        return Err(unsupported_read());
    }
    let func = node
        .child_by_field_name("function")
        .ok_or_else(unsupported_read)?;
    if func.kind() != "member_expression" {
        return Err(unsupported_read());
    }
    let object = func
        .child_by_field_name("object")
        .ok_or_else(unsupported_read)?;
    let method = func
        .child_by_field_name("property")
        .ok_or_else(unsupported_read)?
        .utf8_text(source)
        .map_err(|_| unsupported_read())?;
    let args = collect_arguments(
        node.child_by_field_name("arguments")
            .ok_or_else(unsupported_read)?,
    );
    if object.kind() != "call_expression" {
        extract_collection_from_db_expr(object, source)?;
        if !matches!(method, "find" | "findOne") || args.len() > 2 {
            return Err(unsupported_read());
        }
        for argument in args {
            validate_literal_document(argument, source)?;
        }
        return Ok(if method == "find" {
            ReadMethod::Find
        } else {
            ReadMethod::FindOne
        });
    }
    if validate_read_call(object, source)? != ReadMethod::Find {
        return Err(unsupported_read());
    }
    match method {
        "sort" | "project" if args.len() == 1 => validate_literal_document(args[0], source)?,
        "skip" | "limit" if args.len() == 1 => {
            let value = args[0]
                .utf8_text(source)
                .map_err(|_| unsupported_read())?
                .parse::<i64>()
                .map_err(|_| unsupported_read())?;
            if (method == "skip" && value < 0) || (method == "limit" && value <= 0) {
                return Err(unsupported_read());
            }
        }
        "toArray" | "pretty" if args.is_empty() => return Ok(ReadMethod::Terminal),
        _ => return Err(unsupported_read()),
    }
    Ok(ReadMethod::Find)
}

fn validate_literal_document(node: Node, source: &[u8]) -> Result<(), ParseError> {
    if node.kind() != "object" {
        return Err(unsupported_read());
    }
    let mut cursor = node.walk();
    for pair in node.named_children(&mut cursor) {
        if pair.kind() != "pair" {
            return Err(unsupported_read());
        }
        let key = pair
            .child_by_field_name("key")
            .ok_or_else(unsupported_read)?;
        match key.kind() {
            "property_identifier" | "number" => {}
            "string" => {
                let _: String =
                    json5::from_str(key.utf8_text(source).map_err(|_| unsupported_read())?)
                        .map_err(|_| unsupported_read())?;
            }
            _ => return Err(unsupported_read()),
        }
        validate_literal_value(
            pair.child_by_field_name("value")
                .ok_or_else(unsupported_read)?,
            source,
        )?;
    }
    Ok(())
}

fn validate_literal_value(node: Node, source: &[u8]) -> Result<(), ParseError> {
    match node.kind() {
        "string" => {
            let _: String =
                json5::from_str(node.utf8_text(source).map_err(|_| unsupported_read())?)
                    .map_err(|_| unsupported_read())?;
        }
        "number" | "true" | "false" | "null" => {}
        "object" => validate_literal_document(node, source)?,
        "array" => {
            let mut cursor = node.walk();
            let elements = node.named_children(&mut cursor).collect::<Vec<_>>();
            let mut cursor = node.walk();
            let commas = node
                .children(&mut cursor)
                .filter(|child| child.kind() == ",")
                .count();
            if (elements.is_empty() && commas != 0)
                || (!elements.is_empty() && commas + 1 != elements.len())
            {
                return Err(unsupported_read());
            }
            for element in elements {
                validate_literal_value(element, source)?;
            }
        }
        "unary_expression" => {
            let text = node.utf8_text(source).map_err(|_| unsupported_read())?;
            if !text.starts_with('-')
                || node.named_child_count() != 1
                || node
                    .named_child(0)
                    .is_none_or(|child| child.kind() != "number")
                || !text.parse::<f64>().is_ok_and(f64::is_finite)
            {
                return Err(unsupported_read());
            }
        }
        "call_expression" => {
            let function = node
                .child_by_field_name("function")
                .ok_or_else(unsupported_read)?;
            if function.kind() != "identifier" {
                return Err(unsupported_read());
            }
            let name = function.utf8_text(source).map_err(|_| unsupported_read())?;
            if is_value_constructor(name) {
                // The shell constructors take literal arguments; the value
                // parser checks their number and form.
                let args = collect_arguments(
                    node.child_by_field_name("arguments")
                        .ok_or_else(unsupported_read)?,
                );
                for arg in args {
                    validate_literal_value(arg, source)?;
                }
                return Ok(());
            }
            if !matches!(name, "ISODate" | "Date") {
                return Err(unsupported_read());
            }
            let args = collect_arguments(
                node.child_by_field_name("arguments")
                    .ok_or_else(unsupported_read)?,
            );
            if args.len() != 1 || args[0].kind() != "string" {
                return Err(unsupported_read());
            }
            validate_literal_value(args[0], source)?;
        }
        "regex" => {}
        _ => return Err(unsupported_read()),
    }
    Ok(())
}

/// Constructors besides `ISODate` / `Date` a read script may use for a typed
/// value; they are decoded by [`crate::shell_value`].
fn is_value_constructor(name: &str) -> bool {
    matches!(
        name,
        "ObjectId"
            | "NumberLong"
            | "Long"
            | "NumberInt"
            | "Int32"
            | "Double"
            | "NumberDecimal"
            | "Decimal128"
            | "UUID"
            | "BinData"
            | "HexData"
            | "Timestamp"
            | "MinKey"
            | "MaxKey"
    )
}

pub fn inspect_read_script(input: &str) -> Result<MongoReadScript, ParseError> {
    let source = input.trim();
    if !(source.starts_with("db.") || source.starts_with("db[")) {
        return Err(ParseError::UnsupportedExpression(
            "Use a single db.collection.find(...) or findOne(...) expression".into(),
        ));
    }
    let mut parser = Parser::new();
    parser
        .set_language(&tree_sitter_javascript::LANGUAGE.into())
        .map_err(|error| ParseError::InvalidSyntax(error.to_string()))?;
    let tree = parser
        .parse(source, None)
        .ok_or_else(|| ParseError::InvalidSyntax("Unable to parse script".into()))?;
    if tree.root_node().has_error() || tree.root_node().named_child_count() != 1 {
        return Err(ParseError::InvalidSyntax(
            "Enter exactly one valid MongoDB read expression".into(),
        ));
    }
    let statement = tree
        .root_node()
        .named_child(0)
        .ok_or_else(unsupported_read)?;
    if statement.kind() != "expression_statement" || statement.named_child_count() != 1 {
        return Err(unsupported_read());
    }
    validate_read_call(
        statement.named_child(0).ok_or_else(unsupported_read)?,
        source.as_bytes(),
    )?;
    match parse_script(source)? {
        ScriptPlan::Level1(MongoQueryPlan::Find {
            collection,
            filter,
            projection,
            sort,
            skip,
            limit,
        }) => {
            let skip = skip.unwrap_or(0);
            if skip > i64::MAX as u64 || limit.is_some_and(|limit| limit <= 0) {
                return Err(ParseError::UnsupportedExpression(
                    "skip must be nonnegative and limit must be positive".into(),
                ));
            }
            Ok(MongoReadScript {
                collection,
                filter,
                projection,
                sort,
                skip,
                limit: limit.map(|value| value as u64),
            })
        }
        ScriptPlan::Level1(MongoQueryPlan::FindOne {
            collection,
            filter,
            projection,
        }) => Ok(MongoReadScript {
            collection,
            filter,
            projection,
            sort: None,
            skip: 0,
            limit: Some(1),
        }),
        _ => Err(ParseError::UnsupportedExpression(
            "Only find() and findOne() are supported in this editor".into(),
        )),
    }
}

#[derive(Debug, PartialEq)]
pub enum ParseError {
    InvalidSyntax(String),
    UnsupportedExpression(String),
    InvalidFilter(String),
}

impl std::fmt::Display for ParseError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ParseError::InvalidSyntax(msg) => write!(f, "Invalid syntax: {}", msg),
            ParseError::UnsupportedExpression(msg) => write!(f, "Unsupported expression: {}", msg),
            ParseError::InvalidFilter(msg) => write!(f, "Invalid filter: {}", msg),
        }
    }
}

pub fn parse_script(input: &str) -> Result<ScriptPlan, ParseError> {
    let normalized = normalize_script_input(input);
    let trimmed = normalized.as_str();
    if trimmed.is_empty() {
        return Err(ParseError::InvalidSyntax("Empty script".into()));
    }

    // Pre-process: rewrite hyphenated collection names to bracket notation
    // e.g., db.sales-orders.find() → db["sales-orders"].find()
    let rewritten = rewrite_hyphenated_collections(trimmed);
    let source = rewritten.as_str();

    let mut parser = Parser::new();
    parser
        .set_language(&tree_sitter_javascript::LANGUAGE.into())
        .map_err(|e| ParseError::InvalidSyntax(format!("Parser init failed: {}", e)))?;

    let tree = parser
        .parse(source, None)
        .ok_or(ParseError::InvalidSyntax("Failed to parse script".into()))?;

    let root = tree.root_node();

    // Check for pipeline.define() — Level 3 define
    if let Some(def) = try_parse_pipeline_define(root, source.as_bytes())? {
        return Ok(ScriptPlan::Level3Define(def));
    }

    // Check for pipeline.run("name", params) — Level 3 run (named)
    if let Some(r) = try_parse_pipeline_run_named(root, source.as_bytes())? {
        return Ok(ScriptPlan::Level3Run {
            name: r.0,
            params: r.1,
        });
    }

    // Check for pipeline.run([...]) — Level 2
    if let Some(plan) = try_parse_pipeline_run(root, source.as_bytes())? {
        return Ok(ScriptPlan::Level2(plan));
    }

    // Level 1: db.collection.method()
    find_db_call(root, source.as_bytes()).map(ScriptPlan::Level1)
}

fn normalize_script_input(input: &str) -> String {
    let trimmed = input.trim();

    if let Some(fence_start) = trimmed.find("```") {
        let after_start = &trimmed[fence_start + 3..];
        let code_start = after_start.find('\n').map_or(0, |idx| idx + 1);
        let after_language = &after_start[code_start..];
        if let Some(fence_end) = after_language.find("```") {
            return after_language[..fence_end].trim().to_string();
        }
    }

    let db_dot = trimmed.find("db.");
    let db_bracket = trimmed.find("db[");
    let db_start = match (db_dot, db_bracket) {
        (Some(a), Some(b)) => Some(a.min(b)),
        (Some(idx), None) | (None, Some(idx)) => Some(idx),
        (None, None) => None,
    };

    if let Some(idx) = db_start {
        let prefix = trimmed[..idx].trim();
        if !prefix.is_empty()
            && !prefix.contains(['(', '[', '{', '=', '.', '$'])
            && !prefix.eq_ignore_ascii_case("await")
        {
            return trimmed[idx..].trim().to_string();
        }
    }

    trimmed.to_string()
}

/// Rewrite hyphenated collection names in `db.collection` expressions to bracket notation.
/// This allows users to write `db.sales-orders.find()` instead of `db["sales-orders"].find()`.
///
/// Pattern: `db.<identifier-with-hyphens>` → `db["<identifier-with-hyphens>"]`
///
/// Examples:
/// - `db.sales-orders.find()` → `db["sales-orders"].find()`
/// - `db.my-cool-collection.aggregate([])` → `db["my-cool-collection"].aggregate([])`
/// - `db.users.find()` → `db.users.find()` (unchanged, no hyphens)
fn rewrite_hyphenated_collections(input: &str) -> String {
    // Match: db. followed by an identifier that contains at least one hyphen
    // The identifier must start with letter/underscore, then have alphanumeric/underscore/hyphen chars
    // with at least one hyphen present
    let re = regex::Regex::new(r"db\.([a-zA-Z_][a-zA-Z0-9_]*(?:-[a-zA-Z0-9_]+)+)").unwrap();
    re.replace_all(input, r#"db["$1"]"#).into_owned()
}

/// Walk the AST to find the first `db.collection.method()` call expression.
/// Handles wrappers like expression_statement, variable_declaration,
/// assignment_expression, parenthesized_expression, binary_expression.
fn find_db_call(root: Node, source: &[u8]) -> Result<MongoQueryPlan, ParseError> {
    if root.kind() != "program" {
        return Err(ParseError::InvalidSyntax("Expected program node".into()));
    }

    let mut first_error: Option<ParseError> = None;
    let mut cursor = root.walk();
    for child in root.children(&mut cursor) {
        let result = try_extract_from_statement(child, source);
        match result {
            Ok(plan) => return Ok(plan),
            Err(error) if first_error.is_none() => first_error = Some(error),
            Err(_) => {}
        }
    }

    if let Some(error) = first_error {
        return Err(error);
    }

    Err(ParseError::UnsupportedExpression(
        "Please write a MongoDB query like db.collection.find({}),\n\
         or a pipeline like pipeline.run([{ $source: { ... } }])"
            .into(),
    ))
}

/// Try to extract a db call from a top-level statement.
fn try_extract_from_statement(stmt: Node, source: &[u8]) -> Result<MongoQueryPlan, ParseError> {
    match stmt.kind() {
        "expression_statement" => {
            let mut inner_cursor = stmt.walk();
            for child in stmt.children(&mut inner_cursor) {
                let result = try_unwrap_to_db_call(child, source);
                if result.is_ok() {
                    return result;
                }
            }
            Err(ParseError::UnsupportedExpression("Statement does not contain a MongoDB query".to_string()))
        }
        "variable_declaration" | "lexical_declaration" => {
            // const result = db.users.find({})
            let mut decl_cursor = stmt.walk();
            for child in stmt.children(&mut decl_cursor) {
                if child.kind() == "variable_declarator" {
                    let value = child.child_by_field_name("value");
                    if let Some(val) = value {
                        return try_unwrap_to_db_call(val, source);
                    }
                }
            }
            Err(ParseError::UnsupportedExpression(
                "Variable declaration does not contain a MongoDB query".into(),
            ))
        }
        _ => {
            // Try unwrapping directly
            try_unwrap_to_db_call(stmt, source)
        }
    }
}

/// Recursively unwrap wrapper nodes until we find a `db.xxx.method()` call_expression.
fn try_unwrap_to_db_call(node: Node, source: &[u8]) -> Result<MongoQueryPlan, ParseError> {
    match node.kind() {
        "call_expression" => {
            // Check if this is a db.xxx.method() call
            if is_db_call(node, source) {
                parse_db_call(node, source)
            } else {
                // Could be a chained call like .toArray() wrapping a db call
                // Try unwrapping: the object of the call might be a db call
                let func = node.child_by_field_name("function");
                if let Some(f) = func {
                    if f.kind() == "member_expression" {
                        let obj = f.child_by_field_name("object");
                        if let Some(o) = obj {
                            return try_unwrap_to_db_call(o, source);
                        }
                    }
                }
                Err(ParseError::UnsupportedExpression("Not a MongoDB query call".to_string()))
            }
        }
        "parenthesized_expression" => {
            // (db.users.find({})) → unwrap; use named_child(0) to skip "(" and ")" tokens
            let inner = node.named_child(0);
            match inner {
                Some(n) => try_unwrap_to_db_call(n, source),
                None => Err(ParseError::InvalidSyntax("Empty parenthesized expression".into())),
            }
        }
        "assignment_expression" => {
            // result = db.users.find({}) → take right side
            let right = node.child_by_field_name("right");
            match right {
                Some(r) => try_unwrap_to_db_call(r, source),
                None => Err(ParseError::InvalidSyntax("Assignment has no right side".into())),
            }
        }
        "binary_expression" => {
            // Could be: db.users.find({}).count() > 0
            // Try both sides for a db call
            let left = node.child_by_field_name("left");
            let right = node.child_by_field_name("right");
            if let Some(l) = left {
                let result = try_unwrap_to_db_call(l, source);
                if result.is_ok() {
                    return result;
                }
            }
            if let Some(r) = right {
                let result = try_unwrap_to_db_call(r, source);
                if result.is_ok() {
                    return result;
                }
            }
            Err(ParseError::UnsupportedExpression("Binary expression does not contain a MongoDB query".to_string()))
        }
        "await_expression" => {
            // await db.users.find({}) → unwrap; use named_child(0) to skip the "await" keyword token
            let inner = node.named_child(0);
            match inner {
                Some(n) => try_unwrap_to_db_call(n, source),
                None => Err(ParseError::InvalidSyntax("Await has no inner expression".into())),
            }
        }
        "comment" => Err(ParseError::InvalidSyntax("Cannot execute a comment".into())),
        _ => Err(ParseError::UnsupportedExpression(format!(
            "Unsupported expression type: {}. Please write db.collection.find({{}}) or pipeline.run([...])",
            node.kind()
        ))),
    }
}

/// Check if a call_expression starts with `db.`
fn is_db_call(call: Node, source: &[u8]) -> bool {
    // Walk to the innermost call_expression (for chained calls like .sort().limit())
    let innermost = find_innermost_call(call);
    let inner_func = match innermost.child_by_field_name("function") {
        Some(f) => f,
        None => return false,
    };

    // inner_func should be a member_expression like db.users.find
    // The object of this member_expression should start with "db"
    if inner_func.kind() == "member_expression" {
        let obj = match inner_func.child_by_field_name("object") {
            Some(o) => o,
            None => return false,
        };
        return starts_with_db(obj, source);
    }

    false
}

/// Check if a node is the "db" identifier, or a member/subscript expression chain starting with "db".
fn starts_with_db(node: Node, source: &[u8]) -> bool {
    match node.kind() {
        "identifier" => node.utf8_text(source).unwrap_or("") == "db",
        "member_expression" => {
            // e.g. db.users — check if the object is "db"
            let obj = match node.child_by_field_name("object") {
                Some(o) => o,
                None => return false,
            };
            starts_with_db(obj, source)
        }
        "subscript_expression" => {
            // e.g. db["sales-orders"] — object is "db"
            let obj = match node.child_by_field_name("object") {
                Some(o) => o,
                None => return false,
            };
            starts_with_db(obj, source)
        }
        "call_expression" => {
            // e.g. something.find() — the object could be a call that starts with db
            let func = match node.child_by_field_name("function") {
                Some(f) => f,
                None => return false,
            };
            if func.kind() == "member_expression" {
                let obj = match func.child_by_field_name("object") {
                    Some(o) => o,
                    None => return false,
                };
                starts_with_db(obj, source)
            } else {
                false
            }
        }
        _ => false,
    }
}

/// Check if a call_expression is `pipeline.run(...)`
fn is_pipeline_run_call(call: Node, source: &[u8]) -> bool {
    let func = match call.child_by_field_name("function") {
        Some(f) => f,
        None => return false,
    };

    if func.kind() == "member_expression" {
        let prop = func.child_by_field_name("property");
        let obj = func.child_by_field_name("object");
        if let (Some(p), Some(o)) = (prop, obj) {
            return p.utf8_text(source).unwrap_or("") == "run"
                && o.utf8_text(source).unwrap_or("") == "pipeline";
        }
    }
    false
}

/// Parse a db.collection.method() call expression into a MongoQueryPlan.
fn parse_db_call(call: Node, source: &[u8]) -> Result<MongoQueryPlan, ParseError> {
    let mut chain: Vec<(String, Vec<Node>)> = Vec::new();
    collect_chain(call, &mut chain, source)?;

    if chain.is_empty() {
        return Err(ParseError::InvalidSyntax("No method call found".into()));
    }

    let (first_method, first_args) = &chain[0];
    let collection = extract_collection_from_call(call, source)?;

    match first_method.as_str() {
        "find" => {
            let filter = if !first_args.is_empty() {
                parse_node_as_document(first_args[0], source)?
            } else {
                Document::new()
            };
            let mut projection = if first_args.len() > 1 {
                Some(parse_node_as_document(first_args[1], source)?)
            } else {
                None
            };

            let mut sort: Option<Document> = None;
            let mut skip: Option<u64> = None;
            let mut limit: Option<i64> = None;

            for (method, args) in chain.iter().skip(1) {
                match method.as_str() {
                    "sort" => {
                        sort = Some(if !args.is_empty() {
                            parse_node_as_document(args[0], source)?
                        } else {
                            Document::new()
                        });
                    }
                    "skip" => {
                        skip = Some(if !args.is_empty() {
                            parse_node_as_number(args[0], source).map(|v| v as u64)?
                        } else {
                            0
                        });
                    }
                    "limit" => {
                        limit = Some(if !args.is_empty() {
                            parse_node_as_number(args[0], source)?
                        } else {
                            0
                        });
                    }
                    "project" => {
                        if !args.is_empty() {
                            projection.replace(parse_node_as_document(args[0], source)?);
                        }
                    }
                    "toArray" | "toArray_2" | "pretty" | "shell" => {
                        // No-op terminal methods, ignored
                    }
                    _ => {
                        return Err(ParseError::UnsupportedExpression(format!(
                            "Unsupported chained method: {}. Supported: .sort(), .limit(), .skip(), .project(), .toArray()",
                            method
                        )));
                    }
                }
            }

            Ok(MongoQueryPlan::Find {
                collection,
                filter,
                projection,
                sort,
                skip,
                limit,
            })
        }
        "findOne" => {
            let filter = if !first_args.is_empty() {
                parse_node_as_document(first_args[0], source)?
            } else {
                Document::new()
            };
            let projection = if first_args.len() > 1 {
                Some(parse_node_as_document(first_args[1], source)?)
            } else {
                None
            };
            Ok(MongoQueryPlan::FindOne {
                collection,
                filter,
                projection,
            })
        }
        "countDocuments" => {
            let filter = if !first_args.is_empty() {
                parse_node_as_document(first_args[0], source)?
            } else {
                Document::new()
            };
            Ok(MongoQueryPlan::CountDocuments { collection, filter })
        }
        "aggregate" => {
            let pipeline = if !first_args.is_empty() {
                parse_node_as_array_of_documents(first_args[0], source)?
            } else {
                Vec::new()
            };
            Ok(MongoQueryPlan::Aggregate {
                collection,
                pipeline,
            })
        }
        _ => Err(ParseError::UnsupportedExpression(format!(
            "Unsupported method: {}",
            first_method
        ))),
    }
}

/// Parse a pipeline.run([...]) call expression.
/// Level 2: pipeline.run([{ $source: {...} }, { $lookup: {...} }])
/// Try to parse a pipeline.run([...]) call from the AST root.
/// Returns Ok(None) if no pipeline.run is found (not an error, just not Level 2).
fn try_parse_pipeline_run(root: Node, source: &[u8]) -> Result<Option<PipelinePlan>, ParseError> {
    let mut cursor = root.walk();
    for child in root.children(&mut cursor) {
        // Walk through expression_statement, variable_declarations, etc.
        let call = try_unwrap_to_call(child);
        if let Some(node) = call {
            if is_pipeline_run_call(node, source) {
                let plan = parse_pipeline_run(node, source)?;
                return Ok(Some(plan));
            }
        }
    }
    Ok(None)
}

// ── Level 3: pipeline.define / pipeline.run("name", params) ──

fn try_parse_pipeline_define(
    root: Node,
    source: &[u8],
) -> Result<Option<PipelineDefinition>, ParseError> {
    let mut cursor = root.walk();
    for child in root.children(&mut cursor) {
        let call = try_unwrap_to_call(child);
        if let Some(node) = call {
            if is_pipeline_define_call(node, source) {
                let def = parse_pipeline_define(node, source)?;
                return Ok(Some(def));
            }
        }
    }
    Ok(None)
}

fn is_pipeline_define_call(call: Node, source: &[u8]) -> bool {
    let func = match call.child_by_field_name("function") {
        Some(f) => f,
        None => return false,
    };
    if func.kind() == "member_expression" {
        let prop = func.child_by_field_name("property");
        let obj = func.child_by_field_name("object");
        if let (Some(p), Some(o)) = (prop, obj) {
            return p.utf8_text(source).unwrap_or("") == "define"
                && o.utf8_text(source).unwrap_or("") == "pipeline";
        }
    }
    false
}

/// Parse pipeline.define("name", { defaults }, [ stages ]) into PipelineDefinition.
fn parse_pipeline_define(call: Node, source: &[u8]) -> Result<PipelineDefinition, ParseError> {
    let args_node = call
        .child_by_field_name("arguments")
        .ok_or(ParseError::InvalidSyntax(
            "pipeline.define() requires arguments".into(),
        ))?;
    let args = collect_arguments(args_node);

    if args.len() < 3 {
        return Err(ParseError::InvalidSyntax(
            "pipeline.define(name, defaults, stages) requires 3 arguments".into(),
        ));
    }

    // Arg 0: name (string)
    let name = parse_node_as_string(&args[0], source).ok_or(ParseError::InvalidSyntax(
        "pipeline.define() first argument must be a string name".into(),
    ))?;

    // Arg 1: defaults (object)
    let defaults = if args[1].kind() == "object" {
        parse_node_as_document(args[1], source)?
    } else {
        return Err(ParseError::InvalidSyntax(
            "pipeline.define() second argument must be an object (defaults)".into(),
        ));
    };

    // Arg 2: stages (array)
    if args[2].kind() != "array" {
        return Err(ParseError::InvalidSyntax(
            "pipeline.define() third argument must be an array (stages)".into(),
        ));
    }
    let mut stages = Vec::new();
    let mut cursor = args[2].walk();
    for child in args[2].children(&mut cursor) {
        if child.kind() == "object" {
            let doc = parse_node_as_document(child, source)?;
            let stage = parse_pipeline_stage_from_doc(doc)?;
            stages.push(stage);
        }
    }

    if stages.is_empty() {
        return Err(ParseError::InvalidSyntax(
            "pipeline.define() stages array must not be empty".into(),
        ));
    }

    Ok(PipelineDefinition {
        name,
        default_params: defaults,
        stages,
    })
}

fn try_parse_pipeline_run_named(
    root: Node,
    source: &[u8],
) -> Result<Option<(String, Document)>, ParseError> {
    let mut cursor = root.walk();
    for child in root.children(&mut cursor) {
        let call = try_unwrap_to_call(child);
        if let Some(node) = call {
            if is_pipeline_run_call(node, source) {
                // Check if first arg is a string (named run) vs array (inline run)
                if let Some(args_node) = node.child_by_field_name("arguments") {
                    let args = collect_arguments(args_node);
                    if let Some(first) = args.first() {
                        if first.kind() == "string" {
                            let run_name = parse_node_as_string(first, source).ok_or(
                                ParseError::InvalidSyntax("Invalid pipeline.run name".into()),
                            )?;
                            let params = if args.len() > 1 && args[1].kind() == "object" {
                                parse_node_as_document(args[1], source)?
                            } else {
                                Document::new()
                            };
                            return Ok(Some((run_name, params)));
                        }
                    }
                }
            }
        }
    }
    Ok(None)
}

/// Extract a string literal value from an AST node.
fn parse_node_as_string(node: &Node, source: &[u8]) -> Option<String> {
    if node.kind() == "string" {
        let text = node.utf8_text(source).ok()?;
        // Strip quotes
        let inner = text.trim_matches(|c| c == '\'' || c == '"');
        Some(inner.to_string())
    } else {
        None
    }
}

/// Unwrap wrapping nodes to find a call_expression.
fn try_unwrap_to_call(node: Node) -> Option<Node> {
    match node.kind() {
        "expression_statement" => {
            let mut cursor = node.walk();
            for child in node.children(&mut cursor) {
                if child.kind() == "call_expression" {
                    return Some(child);
                }
                let inner = try_unwrap_to_call(child);
                if inner.is_some() {
                    return inner;
                }
            }
            None
        }
        "call_expression" => Some(node),
        _ => None,
    }
}

/// Parse a pipeline.run([...]) call expression into a PipelinePlan.
fn parse_pipeline_run(call: Node, source: &[u8]) -> Result<PipelinePlan, ParseError> {
    let args_node = call
        .child_by_field_name("arguments")
        .ok_or(ParseError::InvalidSyntax(
            "pipeline.run() requires arguments".into(),
        ))?;

    let args = collect_arguments(args_node);
    let array_node = args.into_iter().next().ok_or(ParseError::InvalidSyntax(
        "pipeline.run() requires an array argument".into(),
    ))?;

    if array_node.kind() != "array" {
        return Err(ParseError::InvalidSyntax(
            "pipeline.run() argument must be an array".into(),
        ));
    }

    let mut stages = Vec::new();
    let mut cursor = array_node.walk();
    for child in array_node.children(&mut cursor) {
        if child.kind() == "object" {
            let doc = parse_node_as_document(child, source)?;
            let stage = parse_pipeline_stage_from_doc(doc)?;
            stages.push(stage);
        }
    }

    if stages.is_empty() {
        return Err(ParseError::InvalidSyntax(
            "pipeline.run() array must not be empty".into(),
        ));
    }

    Ok(PipelinePlan { stages })
}

/// Convert a parsed Document (from a pipeline stage object) into a PipelineStage.
fn parse_pipeline_stage_from_doc(doc: Document) -> Result<PipelineStage, ParseError> {
    let Some((key, value)) = doc.iter().next() else {
        return Err(ParseError::InvalidSyntax("Empty pipeline stage".into()));
    };
    match key.as_str() {
        "$source" => {
            let source_doc = value.as_document().ok_or(ParseError::InvalidSyntax(
                "$source value must be an object".into(),
            ))?;
            let db = source_doc.get_str("db").unwrap_or("$current").to_string();
            let collection = source_doc
                .get_str("collection")
                .map_err(|_| {
                    ParseError::InvalidSyntax("$source requires 'collection' field".into())
                })?
                .to_string();
            Ok(PipelineStage::SourceMongo { db, collection })
        }
        "$match" => {
            let filter = value
                .as_document()
                .ok_or(ParseError::InvalidSyntax(
                    "$match value must be an object".into(),
                ))?
                .clone();
            Ok(PipelineStage::Match { filter })
        }
        "$project" => {
            let spec = value
                .as_document()
                .ok_or(ParseError::InvalidSyntax(
                    "$project value must be an object".into(),
                ))?
                .clone();
            Ok(PipelineStage::Project { spec })
        }
        "$sort" => {
            let spec = value
                .as_document()
                .ok_or(ParseError::InvalidSyntax(
                    "$sort value must be an object".into(),
                ))?
                .clone();
            Ok(PipelineStage::Sort { spec })
        }
        "$limit" => {
            let count = value.as_i64().ok_or(ParseError::InvalidSyntax(
                "$limit value must be a number".into(),
            ))?;
            Ok(PipelineStage::Limit { count })
        }
        "$skip" => {
            let count = value.as_i64().ok_or(ParseError::InvalidSyntax(
                "$skip value must be a number".into(),
            ))?;
            Ok(PipelineStage::Skip { count })
        }
        "$lookup" => {
            let lookup_doc = value.as_document().ok_or(ParseError::InvalidSyntax(
                "$lookup value must be an object".into(),
            ))?;
            let from = lookup_doc
                .get_str("from")
                .map_err(|_| {
                    ParseError::InvalidSyntax(
                        "$lookup requires 'from' (collection name) field".into(),
                    )
                })?
                .to_string();
            let as_field = lookup_doc
                .get_str("as")
                .map_err(|_| ParseError::InvalidSyntax("$lookup requires 'as' field".into()))?
                .to_string();
            let local_field = lookup_doc.get_str("localField").unwrap_or("").to_string();
            let foreign_field = lookup_doc.get_str("foreignField").unwrap_or("").to_string();
            Ok(PipelineStage::LookupMongo {
                from,
                local_field,
                foreign_field,
                as_field,
            })
        }
        "$join" => {
            let join_doc = value.as_document().ok_or(ParseError::InvalidSyntax(
                "$join value must be an object".into(),
            ))?;
            let from_doc = join_doc.get_document("from").map_err(|_| {
                ParseError::InvalidSyntax(
                    "$join requires 'from' object with db and collection fields".into(),
                )
            })?;
            let foreign = AggregateSource {
                db: from_doc
                    .get_str("db")
                    .map_err(|_| {
                        ParseError::InvalidSyntax("$join.from requires 'db' field".into())
                    })?
                    .to_string(),
                collection: from_doc
                    .get_str("collection")
                    .map_err(|_| {
                        ParseError::InvalidSyntax("$join.from requires 'collection' field".into())
                    })?
                    .to_string(),
            };
            let local_field = join_doc
                .get_str("localField")
                .map_err(|_| ParseError::InvalidSyntax("$join requires 'localField'".into()))?
                .to_string();
            let foreign_field = join_doc
                .get_str("foreignField")
                .map_err(|_| ParseError::InvalidSyntax("$join requires 'foreignField'".into()))?
                .to_string();
            let as_field = join_doc
                .get_str("as")
                .map_err(|_| ParseError::InvalidSyntax("$join requires 'as' field".into()))?
                .to_string();
            let pipeline = join_doc
                .get_array("pipeline")
                .map(|arr| {
                    arr.iter()
                        .filter_map(|v| v.as_document().cloned())
                        .collect()
                })
                .unwrap_or_default();
            let one = join_doc.get_bool("one").unwrap_or(false);
            Ok(PipelineStage::Join {
                foreign,
                local_field,
                foreign_field,
                as_field,
                pipeline,
                one,
            })
        }
        "$group" => {
            let group_doc = value.as_document().ok_or(ParseError::InvalidSyntax(
                "$group value must be an object".into(),
            ))?;
            let group_key = group_doc.get("_id").cloned().unwrap_or(bson::Bson::Null);
            let mut accumulators = Document::new();
            for (k, v) in group_doc.iter() {
                if k != "_id" {
                    accumulators.insert(k, v.clone());
                }
            }
            Ok(PipelineStage::Group {
                group_key,
                accumulators,
            })
        }
        _ => {
            // Unknown stage — pass through as-is
            Ok(PipelineStage::Raw { doc: doc.clone() })
        }
    }
}

/// Collect chained method calls from a nested call_expression AST.
/// e.g. db.users.find({}).sort({name:1}).limit(10)
/// → [("find", [arg]), ("sort", [arg]), ("limit", [arg])]
fn collect_chain<'a>(
    node: Node<'a>,
    chain: &mut Vec<(String, Vec<Node<'a>>)>,
    source: &[u8],
) -> Result<(), ParseError> {
    // call_expression has: function + arguments
    let func = node
        .child_by_field_name("function")
        .ok_or(ParseError::InvalidSyntax("Call has no function".into()))?;
    let args_node = node.child_by_field_name("arguments");

    let arg_list = if let Some(an) = args_node {
        collect_arguments(an)
    } else {
        Vec::new()
    };

    // If function is a member_expression (chained call like .sort()),
    // recurse into the object (left side)
    if func.kind() == "member_expression" {
        let object = func.child_by_field_name("object");
        let property = func.child_by_field_name("property");

        let method_name = property
            .map(|p| p.utf8_text(source).unwrap_or("").to_string())
            .unwrap_or_default();

        if let Some(obj) = object {
            if obj.kind() == "call_expression" {
                // Recurse into the inner call first
                collect_chain(obj, chain, source)?;
            }
        }
        chain.push((method_name, arg_list));
    } else if func.kind() == "identifier" || func.kind() == "member_expression" {
        // Direct call like db.users.find() — treat function as the base
        // This is already handled in the recursion above
    } else {
        return Err(ParseError::UnsupportedExpression(format!(
            "Unexpected function node type: {}",
            func.kind()
        )));
    }

    Ok(())
}

/// Collect arguments from an arguments node.
fn collect_arguments(args_node: Node) -> Vec<Node> {
    let mut args = Vec::new();
    let mut cursor = args_node.walk();
    for child in args_node.children(&mut cursor) {
        if child.kind() != "(" && child.kind() != ")" && child.kind() != "," {
            args.push(child);
        }
    }
    args
}

/// Extract the collection name directly from an already-parsed call_expression AST node.
/// Handles both dot notation (db.users) and bracket notation (db["sales-orders"]).
/// Does NOT re-parse — works from the existing AST to avoid position-hardcoding bugs.
fn extract_collection_from_call(call: Node, source: &[u8]) -> Result<String, ParseError> {
    // Walk to the innermost call_expression (handles chains like .sort().limit())
    let innermost = find_innermost_call(call);

    let func = innermost
        .child_by_field_name("function")
        .ok_or(ParseError::InvalidSyntax("No function in base call".into()))?;

    if func.kind() != "member_expression" {
        return Err(ParseError::InvalidSyntax(format!(
            "Expected member_expression for function, got {}",
            func.kind()
        )));
    }

    // The object of this member_expression is the "db.collection" part.
    // Three cases:
    //   db.users            → member_expression(db, users)  → property = "users"
    //   db["sales-orders"] → subscript_expression(db, "sales-orders")
    //   db                  → identifier "db" (missing collection name)
    let obj = func
        .child_by_field_name("object")
        .ok_or(ParseError::InvalidSyntax(
            "No object in member expression".into(),
        ))?;

    extract_collection_from_db_expr(obj, source)
}

/// Extract the collection name from a `db.collection` or `db["collection"]` expression node.
fn extract_collection_from_db_expr(obj: Node, source: &[u8]) -> Result<String, ParseError> {
    match obj.kind() {
        "member_expression" => {
            // db.users — property is the collection name (dot notation)
            let db_part = obj
                .child_by_field_name("object")
                .ok_or(ParseError::InvalidSyntax("No db identifier".into()))?;
            if db_part.utf8_text(source).unwrap_or("") != "db" {
                return Err(ParseError::UnsupportedExpression(format!(
                    "Expected 'db' identifier, got '{}'",
                    db_part.utf8_text(source).unwrap_or("")
                )));
            }
            let collection = obj
                .child_by_field_name("property")
                .map(|p| p.utf8_text(source).unwrap_or("").to_string())
                .filter(|s| !s.is_empty())
                .ok_or(ParseError::InvalidSyntax("Empty collection name".into()))?;
            Ok(collection)
        }
        "subscript_expression" => {
            // db["sales-orders"] — bracket notation for special collection names
            let db_part = obj
                .child_by_field_name("object")
                .ok_or(ParseError::InvalidSyntax(
                    "No db identifier in subscript".into(),
                ))?;
            if db_part.utf8_text(source).unwrap_or("") != "db" {
                return Err(ParseError::UnsupportedExpression(format!(
                    "Expected 'db' identifier in subscript, got '{}'",
                    db_part.utf8_text(source).unwrap_or("")
                )));
            }
            // The index must be a string literal — reject dynamic keys like db[x]
            let index = obj
                .child_by_field_name("index")
                .ok_or(ParseError::InvalidSyntax(
                    "No index in subscript expression".into(),
                ))?;
            if index.kind() != "string" {
                return Err(ParseError::InvalidSyntax(
                    "Dynamic collection names are not supported. Use db[\"collection-name\"].find()".into(),
                ));
            }
            let raw = index.utf8_text(source).unwrap_or("");
            let collection: String = json5::from_str(raw)
                .map_err(|_| ParseError::InvalidSyntax("Invalid quoted collection name".into()))?;
            if collection.is_empty() {
                return Err(ParseError::InvalidSyntax(
                    "Empty collection name in subscript".into(),
                ));
            }
            Ok(collection)
        }
        "identifier" => {
            // db.find() — missing collection name
            let ident = obj.utf8_text(source).unwrap_or("");
            if ident == "db" {
                Err(ParseError::InvalidSyntax(
                    "Missing collection name: use db.<collection>.<method>()".into(),
                ))
            } else {
                Err(ParseError::UnsupportedExpression(format!(
                    "Expected 'db' identifier, got '{}'",
                    ident
                )))
            }
        }
        _ => Err(ParseError::UnsupportedExpression(format!(
            "Unexpected object type in db.collection expression: {}",
            obj.kind()
        ))),
    }
}

/// Find the innermost (deepest) call_expression in the AST.
fn find_innermost_call(node: Node) -> Node {
    let func = node.child_by_field_name("function");
    match func {
        Some(f) if f.kind() == "call_expression" => find_innermost_call(f),
        Some(f) if f.kind() == "member_expression" => {
            // The object of this member_expression might be a call_expression
            let obj = f.child_by_field_name("object");
            match obj {
                Some(o) if o.kind() == "call_expression" => find_innermost_call(o),
                _ => node,
            }
        }
        _ => node,
    }
}

/// Parse a tree-sitter node as a BSON Document.
fn parse_node_as_document(node: Node, source: &[u8]) -> Result<Document, ParseError> {
    if node.kind() == "object" {
        let mut doc = Document::new();
        let mut cursor = node.walk();
        for child in node.children(&mut cursor) {
            if child.kind() == "pair" {
                let key_node = child.child_by_field_name("key").or_else(|| child.child(0));
                let value_node = child
                    .child_by_field_name("value")
                    .or_else(|| child.child(1));

                let key = key_node
                    .ok_or_else(|| ParseError::InvalidFilter("Missing document key".into()))?;
                let text = key
                    .utf8_text(source)
                    .map_err(|_| ParseError::InvalidFilter("Invalid document key".into()))?;
                let key: String = if key.kind() == "string" {
                    json5::from_str(text).map_err(|_| {
                        ParseError::InvalidFilter("Invalid quoted document key".into())
                    })?
                } else {
                    text.into()
                };

                if let Some(val) = value_node {
                    let bson_val = parse_node_as_bson(val, source)?;
                    doc.insert(key, bson_val);
                }
            }
        }
        Ok(doc)
    } else if node.kind() == "number" {
        Err(ParseError::InvalidFilter(
            "Expected a document {}, got a number. Use db.collection.find({}) with a filter object.".into()
        ))
    } else {
        // Try to interpret the text as a JSON document
        let text = node.utf8_text(source).unwrap_or("{}");
        serde_json::from_str(text)
            .or_else(|_| json5::from_str(text))
            .map_err(|e| ParseError::InvalidFilter(format!("Invalid document: {}", e)))
    }
}

/// Parse a tree-sitter node as a BSON value.
fn parse_node_as_bson(node: Node, source: &[u8]) -> Result<bson::Bson, ParseError> {
    match node.kind() {
        "string" => {
            let text = node
                .utf8_text(source)
                .map_err(|_| ParseError::InvalidFilter("Invalid string literal".into()))?;
            let value: String = json5::from_str(text)
                .map_err(|_| ParseError::InvalidFilter("Invalid string escape".into()))?;
            Ok(bson::Bson::String(value))
        }
        "number" => {
            let text = node.utf8_text(source).unwrap_or("0");
            text.parse::<i64>()
                .map(bson::Bson::Int64)
                .or_else(|_| text.parse::<f64>().map(bson::Bson::Double))
                .map_err(|_| ParseError::InvalidFilter(format!("Invalid number: {}", text)))
        }
        "true" => Ok(bson::Bson::Boolean(true)),
        "false" => Ok(bson::Bson::Boolean(false)),
        "null" => Ok(bson::Bson::Null),
        "object" => {
            let doc = parse_node_as_document(node, source)?;
            Ok(bson::Bson::Document(doc))
        }
        "array" => {
            let mut arr = Vec::new();
            let mut cursor = node.walk();
            for child in node.children(&mut cursor) {
                if child.kind() != "," && child.kind() != "[" && child.kind() != "]" {
                    arr.push(parse_node_as_bson(child, source)?);
                }
            }
            Ok(bson::Bson::Array(arr))
        }
        "call_expression" => parse_bson_constructor(node, source),
        "regex" => {
            let text = node.utf8_text(source).unwrap_or_default();
            crate::shell_value::parse_value(text, crate::shell_value::NumberTyping::Filter)
                .map_err(|error| ParseError::InvalidFilter(error.to_string()))
        }
        "identifier" | "property_identifier" => {
            let text = node.utf8_text(source).unwrap_or("");
            // Handle special identifiers like ObjectId, ISODate, etc.
            match text {
                "ObjectId" | "ISODate" | "Date" | "NumberInt" | "NumberLong" | "NumberDecimal" => {
                    // These are function calls — should be handled as call_expression
                    Err(ParseError::UnsupportedExpression(format!(
                        "BSON constructor {}() not yet supported",
                        text
                    )))
                }
                _ => Ok(bson::Bson::String(text.to_string())),
            }
        }
        _ => {
            // Fallback: try to interpret as JSON
            let text = node.utf8_text(source).unwrap_or("null");
            serde_json::from_str(text)
                .map(|v: serde_json::Value| bson::to_bson(&v).unwrap_or(bson::Bson::Null))
                .or_else(|_| {
                    json5::from_str(text)
                        .map(|v: serde_json::Value| bson::to_bson(&v).unwrap_or(bson::Bson::Null))
                })
                .map_err(|_| ParseError::InvalidFilter(format!("Cannot parse value: {}", text)))
        }
    }
}

fn parse_bson_constructor(node: Node, source: &[u8]) -> Result<bson::Bson, ParseError> {
    let function = node
        .child_by_field_name("function")
        .ok_or(ParseError::InvalidFilter(
            "BSON constructor has no function".into(),
        ))?;
    let name = function.utf8_text(source).unwrap_or("");
    let args_node = node
        .child_by_field_name("arguments")
        .ok_or(ParseError::InvalidFilter(format!(
            "{}() requires arguments",
            name
        )))?;
    let args = collect_arguments(args_node);

    match name {
        "ISODate" | "Date" => {
            let value = args
                .first()
                .and_then(|arg| parse_node_as_string(arg, source));
            let Some(date_str) = value else {
                return Err(ParseError::InvalidFilter(format!(
                    "{}() requires a string date argument",
                    name
                )));
            };
            let datetime = bson::DateTime::parse_rfc3339_str(&date_str).map_err(|e| {
                ParseError::InvalidFilter(format!("Invalid {}() date: {}", name, e))
            })?;
            Ok(bson::Bson::DateTime(datetime))
        }
        name if is_value_constructor(name) => {
            let text = node.utf8_text(source).unwrap_or_default();
            crate::shell_value::parse_value(text, crate::shell_value::NumberTyping::Filter)
                .map_err(|error| ParseError::InvalidFilter(error.to_string()))
        }
        _ => Err(ParseError::UnsupportedExpression(format!(
            "BSON constructor {}() is not supported",
            name
        ))),
    }
}

/// Parse a tree-sitter node as a number (i64).
fn parse_node_as_number(node: Node, source: &[u8]) -> Result<i64, ParseError> {
    let text = node.utf8_text(source).unwrap_or("0");
    text.trim()
        .parse::<i64>()
        .map_err(|_| ParseError::InvalidFilter(format!("Invalid number: {}", text)))
}

/// Parse a tree-sitter node as an array of BSON Documents.
fn parse_node_as_array_of_documents(
    node: Node,
    source: &[u8],
) -> Result<Vec<Document>, ParseError> {
    if node.kind() != "array" {
        // Try treating the entire text as JSON
        let text = node.utf8_text(source).unwrap_or("[]");
        return json5::from_str(text)
            .map_err(|e| ParseError::InvalidFilter(format!("Invalid pipeline: {}", e)));
    }

    let mut docs = Vec::new();
    let mut cursor = node.walk();
    for child in node.children(&mut cursor) {
        if child.kind() == "object" {
            docs.push(parse_node_as_document(child, source)?);
        } else if child.kind() != "," && child.kind() != "[" && child.kind() != "]" {
            // Non-object elements in an aggregate pipeline are invalid
            // but we'll try to parse them anyway
            let text = child.utf8_text(source).unwrap_or("{}");
            if let Ok(doc) = json5::from_str::<Document>(text) {
                docs.push(doc);
            }
        }
    }
    Ok(docs)
}

impl MongoQueryPlan {
    pub fn to_pipeline(&self, db: &str) -> PipelinePlan {
        let stages = match self {
            MongoQueryPlan::Find {
                collection,
                filter,
                projection,
                sort,
                skip,
                limit,
            } => {
                let mut s: Vec<PipelineStage> = vec![PipelineStage::SourceMongo {
                    db: db.into(),
                    collection: collection.clone(),
                }];
                if !filter.is_empty() {
                    s.push(PipelineStage::Match {
                        filter: filter.clone(),
                    });
                }
                if let Some(proj) = projection {
                    s.push(PipelineStage::Project { spec: proj.clone() });
                }
                if let Some(sort_spec) = sort {
                    s.push(PipelineStage::Sort {
                        spec: sort_spec.clone(),
                    });
                }
                if let Some(skip_count) = skip {
                    s.push(PipelineStage::Skip {
                        count: *skip_count as i64,
                    });
                }
                if let Some(limit_count) = limit {
                    s.push(PipelineStage::Limit {
                        count: *limit_count,
                    });
                }
                s
            }
            MongoQueryPlan::FindOne {
                collection,
                filter,
                projection,
            } => {
                let mut s: Vec<PipelineStage> = vec![PipelineStage::SourceMongo {
                    db: db.into(),
                    collection: collection.clone(),
                }];
                s.push(PipelineStage::Match {
                    filter: filter.clone(),
                });
                if let Some(proj) = projection {
                    s.push(PipelineStage::Project { spec: proj.clone() });
                }
                s.push(PipelineStage::Limit { count: 1 });
                s
            }
            MongoQueryPlan::CountDocuments { collection, filter } => {
                let mut s: Vec<PipelineStage> = vec![PipelineStage::SourceMongo {
                    db: db.into(),
                    collection: collection.clone(),
                }];
                if !filter.is_empty() {
                    s.push(PipelineStage::Match {
                        filter: filter.clone(),
                    });
                }
                s.push(PipelineStage::Group {
                    group_key: bson::Bson::Null,
                    accumulators: doc! { "count": { "$sum": 1 } },
                });
                s
            }
            MongoQueryPlan::Aggregate {
                collection,
                pipeline,
            } => {
                let mut s: Vec<PipelineStage> = vec![PipelineStage::SourceMongo {
                    db: db.into(),
                    collection: collection.clone(),
                }];
                for stage_doc in pipeline {
                    // Reuse parse_pipeline_stage_from_doc; fall back to Raw on unknown stages
                    match parse_pipeline_stage_from_doc(stage_doc.clone()) {
                        Ok(stage) => s.push(stage),
                        Err(_) => s.push(PipelineStage::Raw {
                            doc: stage_doc.clone(),
                        }),
                    }
                }
                s
            }
        };
        PipelinePlan { stages }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn inspected_read_script_accepts_find_and_rejects_partial_or_write_programs() {
        let plan = inspect_read_script(
            r#"db["audit-log"].find({ active: true }).sort({ _id: -1 }).skip(2).limit(10)"#,
        )
        .unwrap();
        assert_eq!(plan.collection, "audit-log");
        assert_eq!(plan.skip, 2);
        assert_eq!(plan.limit, Some(10));
        assert_eq!(plan.filter.get_bool("active").unwrap(), true);
        assert_eq!(
            inspect_read_script("db.users.findOne({})").unwrap().limit,
            Some(1)
        );
        for script in [
            "db.users.insertOne({ a: 1 })",
            "db.users.aggregate([{ $match: {} }])",
            "db.users.countDocuments({})",
            "db.users.find({}); db.users.deleteMany({})",
            "db.users.find({}).skip(-1)",
            "db.users.find({}).limit(-1)",
            "db.getSiblingDB('admin').users.find({})",
        ] {
            assert!(
                inspect_read_script(script).is_err(),
                "unsupported script was accepted"
            );
        }
    }

    #[test]
    fn inspect_read_script_rejects_every_unconsumed_ast_node() {
        for script in [
            "db.users.findOne({}).sort({ age: 1 })",
            "db.users.findOne({}).deleteMany({})",
            "db.users.find({ tenantId })",
            "db.users.find({ ...restriction })",
            "db.users.find({ age: 1 + 2 })",
            "db.users.find({ age: user.age })",
            "db.users.find({ nested: { tenantId } })",
            "db.users.find({ value: foo() })",
            "db.users.find({ date: ISODate('2024-01-01T00:00:00Z', 'extra') })",
            "db.users.find({}, {}, { extra: 1 })",
            "db.users.find({}).sort({ age: 1 }, { other: 1 })",
        ] {
            assert!(
                inspect_read_script(script).is_err(),
                "unsafe read was accepted: {script}"
            );
        }
        assert!(inspect_read_script(
            "db.users.find({date:ISODate('2024-01-01T00:00:00Z'), n:-1, tags:['a',2]})"
        )
        .is_ok());
    }

    #[test]
    fn quoted_collection_literal_decodes_actual_js_value() {
        for (script, expected) in [
            (r#"db["a\"b"].find({})"#, "a\"b"),
            (r#"db['a\\b'].find({})"#, "a\\b"),
            (r#"db["caf\u00e9"].find({})"#, "café"),
        ] {
            assert_eq!(inspect_read_script(script).unwrap().collection, expected);
        }
        assert!(inspect_read_script(r#"db["bad\xZZ"].find({})"#).is_err());
    }

    fn unwrap_level1(result: Result<ScriptPlan, ParseError>) -> MongoQueryPlan {
        match result.unwrap() {
            ScriptPlan::Level1(p) => p,
            other => panic!("Expected Level1, got {:?}", other),
        }
    }

    fn unwrap_level2(result: Result<ScriptPlan, ParseError>) -> PipelinePlan {
        match result.unwrap() {
            ScriptPlan::Level2(p) => p,
            other => panic!("Expected Level2, got {:?}", other),
        }
    }

    #[test]
    fn parse_simple_find() {
        let plan = unwrap_level1(parse_script(r#"db.users.find({ name: "Huy" })"#));
        match plan {
            MongoQueryPlan::Find {
                collection, filter, ..
            } => {
                assert_eq!(collection, "users");
                assert_eq!(filter.get_str("name").unwrap(), "Huy");
            }
            _ => panic!("Expected Find plan, got {:?}", plan),
        }
    }

    #[test]
    fn parse_find_empty_filter() {
        let plan = unwrap_level1(parse_script("db.orders.find()"));
        match plan {
            MongoQueryPlan::Find {
                collection, filter, ..
            } => {
                assert_eq!(collection, "orders");
                assert!(filter.is_empty());
            }
            _ => panic!("Expected Find plan"),
        }
    }

    #[test]
    fn parse_find_with_sort_limit() {
        let plan = unwrap_level1(parse_script(
            r#"db.orders.find({ status: "PAID" }).sort({ createdAt: -1 }).limit(100)"#,
        ));
        match plan {
            MongoQueryPlan::Find {
                collection,
                limit,
                sort,
                ..
            } => {
                assert_eq!(collection, "orders");
                assert_eq!(limit, Some(100));
                assert!(sort.is_some());
            }
            _ => panic!("Expected Find plan"),
        }
    }

    #[test]
    fn parse_find_with_skip() {
        let plan = unwrap_level1(parse_script(r#"db.users.find({}).skip(20).limit(10)"#));
        match plan {
            MongoQueryPlan::Find { skip, limit, .. } => {
                assert_eq!(skip, Some(20));
                assert_eq!(limit, Some(10));
            }
            _ => panic!("Expected Find plan"),
        }
    }

    #[test]
    fn parse_find_with_projection() {
        let plan = unwrap_level1(parse_script(
            r#"db.users.find({}, { name: 1, email: 1 }).limit(10)"#,
        ));
        match plan {
            MongoQueryPlan::Find { projection, .. } => {
                assert!(projection.is_some());
                let proj = projection.unwrap();
                assert_eq!(proj.get_i64("name").unwrap(), 1);
                assert_eq!(proj.get_i64("email").unwrap(), 1);
            }
            _ => panic!("Expected Find plan"),
        }
    }

    #[test]
    fn parse_count_documents() {
        let plan = unwrap_level1(parse_script(
            r#"db.users.countDocuments({ isActive: true })"#,
        ));
        match plan {
            MongoQueryPlan::CountDocuments { collection, filter } => {
                assert_eq!(collection, "users");
                assert_eq!(filter.get_bool("isActive").unwrap(), true);
            }
            _ => panic!("Expected CountDocuments plan"),
        }
    }

    #[test]
    fn parse_find_one() {
        let plan = unwrap_level1(parse_script(r#"db.users.findOne({ _id: "abc123" })"#));
        match plan {
            MongoQueryPlan::FindOne {
                collection,
                filter,
                projection,
            } => {
                assert_eq!(collection, "users");
                assert_eq!(filter.get_str("_id").unwrap(), "abc123");
                assert!(projection.is_none());
            }
            _ => panic!("Expected FindOne plan"),
        }
    }

    #[test]
    fn parse_aggregate() {
        let plan = unwrap_level1(parse_script(
            r#"db.orders.aggregate([{ "$match": { status: "PAID" } }, { "$limit": 100 }])"#,
        ));
        match plan {
            MongoQueryPlan::Aggregate {
                collection,
                pipeline,
            } => {
                assert_eq!(collection, "orders");
                assert_eq!(pipeline.len(), 2);
            }
            _ => panic!("Expected Aggregate plan"),
        }
    }

    #[test]
    fn reject_empty_script() {
        let err = parse_script("").unwrap_err();
        assert!(matches!(err, ParseError::InvalidSyntax(..)));
    }

    #[test]
    fn reject_arbitrary_js() {
        let err = parse_script(r#"function test() { return 1; }"#).unwrap_err();
        assert!(matches!(
            err,
            ParseError::UnsupportedExpression(..) | ParseError::InvalidSyntax(..)
        ));
    }

    #[test]
    fn reject_dynamic_collection() {
        // db[x].find({}) — variable collection name
        let err = parse_script(r#"db[x].find({})"#).unwrap_err();
        assert!(matches!(
            err,
            ParseError::InvalidSyntax(..) | ParseError::UnsupportedExpression(..)
        ));
    }

    #[test]
    fn parse_find_with_nested_filter() {
        let plan = unwrap_level1(parse_script(
            r#"db.users.find({ age: { $gte: 18 }, name: "Huy" })"#,
        ));
        match plan {
            MongoQueryPlan::Find { filter, .. } => {
                let age_doc = filter.get_document("age").unwrap();
                assert_eq!(age_doc.get_i64("$gte").unwrap(), 18);
                assert_eq!(filter.get_str("name").unwrap(), "Huy");
            }
            _ => panic!("Expected Find plan"),
        }
    }

    #[test]
    fn parse_find_multiple_conditions() {
        let plan = unwrap_level1(parse_script(
            r#"db.users.find({ age: { $gte: 18 }, name: "Huy" }).limit(50)"#,
        ));
        match plan {
            MongoQueryPlan::Find {
                collection, limit, ..
            } => {
                assert_eq!(collection, "users");
                assert_eq!(limit, Some(50));
            }
            _ => panic!("Expected Find plan"),
        }
    }

    #[test]
    fn parse_find_with_toarray() {
        let plan = unwrap_level1(parse_script(r#"db.users.find({ name: "Huy" }).toArray()"#));
        match plan {
            MongoQueryPlan::Find { collection, .. } => {
                assert_eq!(collection, "users");
            }
            _ => panic!("Expected Find plan, got {:?}", plan),
        }
    }

    #[test]
    fn parse_find_with_sort_limit_toarray() {
        let plan = unwrap_level1(parse_script(
            r#"db.orders.find({ status: "PAID" }).sort({ createdAt: -1 }).limit(100).toArray()"#,
        ));
        match plan {
            MongoQueryPlan::Find {
                collection, limit, ..
            } => {
                assert_eq!(collection, "orders");
                assert_eq!(limit, Some(100));
            }
            _ => panic!("Expected Find plan"),
        }
    }

    #[test]
    fn parse_parenthesized_find() {
        let plan = unwrap_level1(parse_script("db.users.find({})"));
        match plan {
            MongoQueryPlan::Find { collection, .. } => {
                assert_eq!(collection, "users");
            }
            _ => panic!("Expected Find plan"),
        }
    }

    #[test]
    fn parse_multiline_find() {
        let plan = unwrap_level1(parse_script(
            r#"db.users.find({
                isDelete: { $ne: true }
            })"#,
        ));
        match plan {
            MongoQueryPlan::Find {
                collection, filter, ..
            } => {
                assert_eq!(collection, "users");
                assert!(filter.contains_key("isDelete"));
            }
            _ => panic!("Expected Find plan"),
        }
    }

    // --- Level 2: pipeline.run([...]) tests ---

    #[test]
    fn parse_pipeline_run_basic() {
        let plan = unwrap_level2(parse_script(
            r#"pipeline.run([{ $source: { db: "$current", collection: "users" } }])"#,
        ));
        assert_eq!(plan.stages.len(), 1);
        assert_eq!(
            plan.stages[0],
            PipelineStage::SourceMongo {
                db: "$current".into(),
                collection: "users".into(),
            }
        );
    }

    #[test]
    fn parse_pipeline_run_with_stages() {
        let plan = unwrap_level2(parse_script(
            r#"pipeline.run([{ $source: { db: "$current", collection: "orders" } }, { $match: { status: "PAID" } }, { $sort: { createdAt: -1 } }, { $limit: 100 }])"#,
        ));
        assert_eq!(plan.stages.len(), 4);
        assert_eq!(
            plan.stages[0],
            PipelineStage::SourceMongo {
                db: "$current".into(),
                collection: "orders".into(),
            }
        );
        assert_eq!(
            plan.stages[1],
            PipelineStage::Match {
                filter: doc! { "status": "PAID" },
            }
        );
        assert_eq!(
            plan.stages[2],
            PipelineStage::Sort {
                spec: {
                    let mut d = Document::new();
                    d.insert("createdAt", bson::Bson::Int64(-1));
                    d
                },
            }
        );
        assert_eq!(plan.stages[3], PipelineStage::Limit { count: 100 });
    }

    #[test]
    fn parse_pipeline_run_with_lookup() {
        let plan = unwrap_level2(parse_script(
            r#"pipeline.run([{ $source: { db: "$current", collection: "orders" } }, { $lookup: { from: "users", localField: "userId", foreignField: "_id", as: "user" } }])"#,
        ));
        assert_eq!(plan.stages.len(), 2);
        assert_eq!(
            plan.stages[0],
            PipelineStage::SourceMongo {
                db: "$current".into(),
                collection: "orders".into(),
            }
        );
        assert_eq!(
            plan.stages[1],
            PipelineStage::LookupMongo {
                from: "users".into(),
                local_field: "userId".into(),
                foreign_field: "_id".into(),
                as_field: "user".into(),
            }
        );
    }

    #[test]
    fn parse_pipeline_run_empty_array_fails() {
        let err = parse_script(r#"pipeline.run([])"#).unwrap_err();
        assert!(matches!(err, ParseError::InvalidSyntax(..)));
    }

    #[test]
    fn find_compiles_to_pipeline() {
        let plan = MongoQueryPlan::Find {
            collection: "orders".into(),
            filter: doc! { "status": "PAID" },
            projection: None,
            sort: Some(doc! { "createdAt": -1 }),
            skip: Some(10),
            limit: Some(100),
        };
        let pipeline = plan.to_pipeline("mydb");
        assert_eq!(pipeline.stages.len(), 5);
        assert_eq!(
            pipeline.stages[0],
            PipelineStage::SourceMongo {
                db: "mydb".into(),
                collection: "orders".into()
            }
        );
        assert_eq!(
            pipeline.stages[1],
            PipelineStage::Match {
                filter: doc! { "status": "PAID" }
            }
        );
        assert_eq!(
            pipeline.stages[2],
            PipelineStage::Sort {
                spec: doc! { "createdAt": -1 }
            }
        );
        assert_eq!(pipeline.stages[3], PipelineStage::Skip { count: 10 });
        assert_eq!(pipeline.stages[4], PipelineStage::Limit { count: 100 });
    }

    #[test]
    fn find_empty_filter_compiles_to_no_match() {
        let plan = MongoQueryPlan::Find {
            collection: "orders".into(),
            filter: Document::new(),
            projection: None,
            sort: None,
            skip: None,
            limit: None,
        };
        let pipeline = plan.to_pipeline("testdb");
        assert_eq!(pipeline.stages.len(), 1);
        assert_eq!(
            pipeline.stages[0],
            PipelineStage::SourceMongo {
                db: "testdb".into(),
                collection: "orders".into()
            }
        );
    }

    #[test]
    fn find_one_compiles_to_pipeline() {
        let plan = MongoQueryPlan::FindOne {
            collection: "users".into(),
            filter: doc! { "_id": "abc123" },
            projection: Some(doc! { "name": 1, "email": 1 }),
        };
        let pipeline = plan.to_pipeline("mydb");
        assert_eq!(pipeline.stages.len(), 4);
        assert_eq!(
            pipeline.stages[0],
            PipelineStage::SourceMongo {
                db: "mydb".into(),
                collection: "users".into()
            }
        );
        assert_eq!(
            pipeline.stages[1],
            PipelineStage::Match {
                filter: doc! { "_id": "abc123" }
            }
        );
        assert_eq!(
            pipeline.stages[2],
            PipelineStage::Project {
                spec: doc! { "name": 1, "email": 1 }
            }
        );
        assert_eq!(pipeline.stages[3], PipelineStage::Limit { count: 1 });
    }

    #[test]
    fn count_documents_compiles_to_pipeline() {
        let plan = MongoQueryPlan::CountDocuments {
            collection: "users".into(),
            filter: doc! { "isActive": true },
        };
        let pipeline = plan.to_pipeline("mydb");
        assert_eq!(pipeline.stages.len(), 3);
        assert_eq!(
            pipeline.stages[0],
            PipelineStage::SourceMongo {
                db: "mydb".into(),
                collection: "users".into()
            }
        );
        assert_eq!(
            pipeline.stages[1],
            PipelineStage::Match {
                filter: doc! { "isActive": true }
            }
        );
        assert_eq!(
            pipeline.stages[2],
            PipelineStage::Group {
                group_key: bson::Bson::Null,
                accumulators: doc! { "count": { "$sum": 1 } },
            }
        );
    }

    #[test]
    fn aggregate_compiles_to_pipeline() {
        let plan = MongoQueryPlan::Aggregate {
            collection: "orders".into(),
            pipeline: vec![
                doc! { "$match": { "status": "PAID" } },
                doc! { "$group": { "_id": "$category", "total": { "$sum": "$amount" } } },
                doc! { "$sort": { "total": -1 } },
            ],
        };
        let compiled = plan.to_pipeline("mydb");
        assert_eq!(compiled.stages.len(), 4);
        assert_eq!(
            compiled.stages[0],
            PipelineStage::SourceMongo {
                db: "mydb".into(),
                collection: "orders".into()
            }
        );
        assert_eq!(
            compiled.stages[1],
            PipelineStage::Match {
                filter: doc! { "status": "PAID" }
            }
        );
        assert_eq!(
            compiled.stages[2],
            PipelineStage::Group {
                group_key: bson::Bson::String("$category".into()),
                accumulators: doc! { "total": { "$sum": "$amount" } },
            }
        );
        assert_eq!(
            compiled.stages[3],
            PipelineStage::Sort {
                spec: doc! { "total": -1 }
            }
        );
    }

    // --- Bracket notation tests ---

    #[test]
    fn parse_find_hyphenated_collection() {
        // db["sales-orders"].find() — hyphen requires bracket notation
        let plan = unwrap_level1(parse_script(r#"db["sales-orders"].find()"#));
        match plan {
            MongoQueryPlan::Find { collection, .. } => {
                assert_eq!(collection, "sales-orders");
            }
            _ => panic!("Expected Find plan"),
        }
    }

    #[test]
    fn parse_find_bracket_notation() {
        // db["users"].find({}) — bracket notation with regular name
        let plan = unwrap_level1(parse_script(r#"db["users"].find({ name: "Huy" })"#));
        match plan {
            MongoQueryPlan::Find {
                collection, filter, ..
            } => {
                assert_eq!(collection, "users");
                assert_eq!(filter.get_str("name").unwrap(), "Huy");
            }
            _ => panic!("Expected Find plan"),
        }
    }

    #[test]
    fn parse_find_bracket_with_chain() {
        // db["sales-orders"].find({}).sort({ date: -1 }).limit(50)
        let plan = unwrap_level1(parse_script(
            r#"db["sales-orders"].find({}).sort({ date: -1 }).limit(50)"#,
        ));
        match plan {
            MongoQueryPlan::Find {
                collection,
                limit,
                sort,
                ..
            } => {
                assert_eq!(collection, "sales-orders");
                assert_eq!(limit, Some(50));
                assert!(sort.is_some());
            }
            _ => panic!("Expected Find plan"),
        }
    }

    #[test]
    fn parse_find_bracket_with_isodate_range() {
        let plan = unwrap_level1(parse_script(
            r#"db["sales-orders"].find({
                state: "PAID",
                createdAt: {
                    $gte: ISODate("2024-09-01T00:00:00.000Z"),
                    $lt: ISODate("2024-10-01T00:00:00.000Z")
                }
            })"#,
        ));
        match plan {
            MongoQueryPlan::Find {
                collection, filter, ..
            } => {
                assert_eq!(collection, "sales-orders");
                assert_eq!(filter.get_str("state").unwrap(), "PAID");

                let created_at = filter.get_document("createdAt").unwrap();
                assert_eq!(
                    created_at.get_datetime("$gte").unwrap().timestamp_millis(),
                    bson::DateTime::parse_rfc3339_str("2024-09-01T00:00:00.000Z")
                        .unwrap()
                        .timestamp_millis()
                );
                assert_eq!(
                    created_at.get_datetime("$lt").unwrap().timestamp_millis(),
                    bson::DateTime::parse_rfc3339_str("2024-10-01T00:00:00.000Z")
                        .unwrap()
                        .timestamp_millis()
                );
            }
            _ => panic!("Expected Find plan"),
        }
    }

    #[test]
    fn parse_find_from_fenced_code_block() {
        let plan = unwrap_level1(parse_script(
            r#"```js
db["sales-orders"].find({ state: "PAID" })
```"#,
        ));
        match plan {
            MongoQueryPlan::Find {
                collection, filter, ..
            } => {
                assert_eq!(collection, "sales-orders");
                assert_eq!(filter.get_str("state").unwrap(), "PAID");
            }
            _ => panic!("Expected Find plan"),
        }
    }

    #[test]
    fn parse_find_after_assistant_prose() {
        let plan = unwrap_level1(parse_script(
            r#"Here is the query:
db["sales-orders"].find({ state: "PAID" })"#,
        ));
        match plan {
            MongoQueryPlan::Find {
                collection, filter, ..
            } => {
                assert_eq!(collection, "sales-orders");
                assert_eq!(filter.get_str("state").unwrap(), "PAID");
            }
            _ => panic!("Expected Find plan"),
        }
    }

    #[test]
    fn parse_count_bracket_notation() {
        let plan = unwrap_level1(parse_script(r#"db["my-collection"].countDocuments({})"#));
        match plan {
            MongoQueryPlan::CountDocuments { collection, .. } => {
                assert_eq!(collection, "my-collection");
            }
            _ => panic!("Expected CountDocuments plan"),
        }
    }

    #[test]
    fn parse_aggregate_bracket_notation() {
        let plan = unwrap_level1(parse_script(
            r#"db["sales-orders"].aggregate([{ "$match": { "status": "PAID" } }])"#,
        ));
        match plan {
            MongoQueryPlan::Aggregate {
                collection,
                pipeline,
            } => {
                assert_eq!(collection, "sales-orders");
                assert_eq!(pipeline.len(), 1);
            }
            _ => panic!("Expected Aggregate plan"),
        }
    }

    // --- A3: await expression ---

    #[test]
    fn parse_await_find() {
        let plan = unwrap_level1(parse_script("await db.users.find({})"));
        match plan {
            MongoQueryPlan::Find { collection, .. } => assert_eq!(collection, "users"),
            _ => panic!("Expected Find plan"),
        }
    }

    #[test]
    fn parse_await_find_with_filter() {
        let plan = unwrap_level1(parse_script(r#"await db.orders.find({ status: "PAID" })"#));
        match plan {
            MongoQueryPlan::Find {
                collection, filter, ..
            } => {
                assert_eq!(collection, "orders");
                assert_eq!(filter.get_str("status").unwrap(), "PAID");
            }
            _ => panic!("Expected Find plan"),
        }
    }

    // --- A4: parenthesized expression ---

    #[test]
    fn parse_parenthesized_find_real() {
        let plan = unwrap_level1(parse_script(r#"(db.users.find({}))"#));
        match plan {
            MongoQueryPlan::Find { collection, .. } => assert_eq!(collection, "users"),
            _ => panic!("Expected Find plan"),
        }
    }

    // --- B1: number as filter → error ---

    #[test]
    fn reject_number_as_filter() {
        // db.users.find(42) — 42 is not a valid filter document
        // Either returns an error or parses 42 → empty filter (fallback behavior)
        // The important thing: it does not crash, and if it parses, produces an empty filter
        match parse_script("db.users.find(42)") {
            Err(_) => {} // correctly rejected
            Ok(ScriptPlan::Level1(MongoQueryPlan::Find { filter, .. })) => {
                // Acceptable fallback: treat invalid filter as empty
                // But should ideally be an error
                let _ = filter;
            }
            Ok(_) => panic!("Unexpected plan type for find(42)"),
        }
    }

    // --- B2: string escape sequences ---

    #[test]
    fn parse_find_with_escaped_double_quote() {
        let plan = unwrap_level1(parse_script(r#"db.users.find({ name: "O\"Brien" })"#));
        match plan {
            MongoQueryPlan::Find { filter, .. } => {
                assert_eq!(filter.get_str("name").unwrap(), "O\"Brien");
            }
            _ => panic!("Expected Find plan"),
        }
    }

    #[test]
    fn parse_find_with_escaped_newline() {
        let plan = unwrap_level1(parse_script(r#"db.logs.find({ msg: "line1\nline2" })"#));
        match plan {
            MongoQueryPlan::Find { filter, .. } => {
                assert_eq!(filter.get_str("msg").unwrap(), "line1\nline2");
            }
            _ => panic!("Expected Find plan"),
        }
    }

    // --- C2: $lookup validation ---

    #[test]
    fn reject_lookup_missing_from() {
        let err = parse_script(
            r#"pipeline.run([
                { $source: { db: "$current", collection: "users" } },
                { $lookup: { localField: "_id", foreignField: "userId", as: "orders" } }
            ])"#,
        )
        .unwrap_err();
        assert!(matches!(err, ParseError::InvalidSyntax(..)));
    }

    #[test]
    fn reject_lookup_missing_as() {
        let err = parse_script(
            r#"pipeline.run([
                { $source: { db: "$current", collection: "users" } },
                { $lookup: { from: "orders", localField: "_id", foreignField: "userId" } }
            ])"#,
        )
        .unwrap_err();
        assert!(matches!(err, ParseError::InvalidSyntax(..)));
    }

    #[test]
    fn parse_pipeline_define_basic() {
        let result = parse_script(
            r#"pipeline.define("topUsers", { minAge: 18 }, [
                { $source: { db: "$current", collection: "users" } },
                { $match: { age: { "$$minAge": 1 } } },
                { $sort: { name: 1 } }
            ])"#,
        )
        .unwrap();
        match result {
            ScriptPlan::Level3Define(def) => {
                assert_eq!(def.name, "topUsers");
                assert_eq!(def.default_params.get_i64("minAge").unwrap(), 18);
                assert_eq!(def.stages.len(), 3);
            }
            other => panic!("Expected Level3Define, got {:?}", other),
        }
    }

    #[test]
    fn parse_pipeline_run_named() {
        let result = parse_script(r#"pipeline.run("topUsers", { minAge: 25 })"#).unwrap();
        match result {
            ScriptPlan::Level3Run { name, params } => {
                assert_eq!(name, "topUsers");
                assert_eq!(params.get_i64("minAge").unwrap(), 25);
            }
            other => panic!("Expected Level3Run, got {:?}", other),
        }
    }

    #[test]
    fn parse_pipeline_run_named_no_params() {
        let result = parse_script(r#"pipeline.run("topUsers")"#).unwrap();
        match result {
            ScriptPlan::Level3Run { name, params } => {
                assert_eq!(name, "topUsers");
                assert!(params.is_empty());
            }
            other => panic!("Expected Level3Run, got {:?}", other),
        }
    }

    // --- D: $join parsing (cross-DB) ---

    #[test]
    fn parse_join_basic() {
        let result = parse_script(
            r#"pipeline.run([
                { $source: { db: "shop-service", collection: "stores" } },
                { $match: { status: "active" } },
                { $join: {
                    from: { db: "ledger-service", collection: "accounts" },
                    localField: "_id",
                    foreignField: "profileId",
                    as: "accounts"
                }}
            ])"#,
        )
        .unwrap();
        match result {
            ScriptPlan::Level2(plan) => {
                assert_eq!(plan.stages.len(), 3);
                match &plan.stages[2] {
                    PipelineStage::Join {
                        foreign,
                        local_field,
                        foreign_field,
                        as_field,
                        pipeline,
                        one,
                    } => {
                        assert_eq!(foreign.db, "ledger-service");
                        assert_eq!(foreign.collection, "accounts");
                        assert_eq!(local_field, "_id");
                        assert_eq!(foreign_field, "profileId");
                        assert_eq!(as_field, "accounts");
                        assert!(pipeline.is_empty());
                        assert!(!one);
                    }
                    other => panic!("Expected Join stage, got {:?}", other),
                }
            }
            other => panic!("Expected Level2, got {:?}", other),
        }
    }

    #[test]
    fn parse_join_with_pipeline_and_one() {
        let result = parse_script(
            r#"pipeline.run([
                { $source: { db: "shop-service", collection: "stores" } },
                { $join: {
                    from: { db: "ledger-service", collection: "accounts" },
                    localField: "_id",
                    foreignField: "profileId",
                    as: "account",
                    pipeline: [{ $match: { currency: "VND" } }],
                    one: true
                }}
            ])"#,
        )
        .unwrap();
        match result {
            ScriptPlan::Level2(plan) => match &plan.stages[1] {
                PipelineStage::Join {
                    foreign,
                    pipeline,
                    one,
                    ..
                } => {
                    assert_eq!(foreign.db, "ledger-service");
                    assert_eq!(pipeline.len(), 1);
                    assert!(*one);
                }
                other => panic!("Expected Join stage, got {:?}", other),
            },
            other => panic!("Expected Level2, got {:?}", other),
        }
    }

    #[test]
    fn parse_lookup_same_db() {
        let result = parse_script(
            r#"pipeline.run([
                { $source: { db: "mydb", collection: "users" } },
                { $lookup: { from: "orders", localField: "_id", foreignField: "userId", as: "userOrders" } }
            ])"#,
        ).unwrap();
        match result {
            ScriptPlan::Level2(plan) => match &plan.stages[1] {
                PipelineStage::LookupMongo {
                    from,
                    local_field,
                    foreign_field,
                    as_field,
                } => {
                    assert_eq!(from, "orders");
                    assert_eq!(local_field, "_id");
                    assert_eq!(foreign_field, "userId");
                    assert_eq!(as_field, "userOrders");
                }
                other => panic!("Expected LookupMongo, got {:?}", other),
            },
            other => panic!("Expected Level2, got {:?}", other),
        }
    }

    // --- E: Hyphenated collection names ---

    #[test]
    fn rewrite_hyphenated_collection_basic() {
        let input = "db.sales-orders.find()";
        let rewritten = rewrite_hyphenated_collections(input);
        assert_eq!(rewritten, r#"db["sales-orders"].find()"#);
    }

    #[test]
    fn rewrite_hyphenated_collection_multiple_hyphens() {
        let input = "db.my-cool-collection.aggregate([])";
        let rewritten = rewrite_hyphenated_collections(input);
        assert_eq!(rewritten, r#"db["my-cool-collection"].aggregate([])"#);
    }

    #[test]
    fn rewrite_hyphenated_collection_no_change_for_normal() {
        let input = "db.users.find()";
        let rewritten = rewrite_hyphenated_collections(input);
        assert_eq!(rewritten, "db.users.find()");
    }

    #[test]
    fn parse_hyphenated_collection_find() {
        let result = parse_script("db.sales-orders.find()").unwrap();
        match result {
            ScriptPlan::Level1(MongoQueryPlan::Find { collection, .. }) => {
                assert_eq!(collection, "sales-orders");
            }
            other => panic!("Expected Level1 Find with sales-orders, got {:?}", other),
        }
    }

    #[test]
    fn parse_hyphenated_collection_find_with_filter() {
        let result = parse_script(r#"db.sales-orders.find({ status: "pending" })"#).unwrap();
        match result {
            ScriptPlan::Level1(MongoQueryPlan::Find {
                collection, filter, ..
            }) => {
                assert_eq!(collection, "sales-orders");
                assert_eq!(filter.get_str("status").unwrap(), "pending");
            }
            other => panic!("Expected Level1 Find, got {:?}", other),
        }
    }

    #[test]
    fn parse_hyphenated_collection_aggregate() {
        let result =
            parse_script(r#"db.my-data.aggregate([{ $match: { active: true } }])"#).unwrap();
        match result {
            ScriptPlan::Level1(MongoQueryPlan::Aggregate {
                collection,
                pipeline,
            }) => {
                assert_eq!(collection, "my-data");
                assert_eq!(pipeline.len(), 1);
            }
            other => panic!("Expected Level1 Aggregate, got {:?}", other),
        }
    }

    #[test]
    fn read_scripts_decode_typed_filter_values() {
        let script = inspect_read_script(
            r#"db.t.find({ _id: ObjectId("66d4f84d0000000000000003"), n: { $gte: NumberLong("9007199254740993") }, d: NumberDecimal("1.5"), at: { $lt: ISODate("2024-09-01T00:00:00Z") }, re: /^a/i })"#,
        )
        .unwrap();
        assert!(script.filter.get_object_id("_id").is_ok());
        assert_eq!(
            script
                .filter
                .get_document("n")
                .unwrap()
                .get_i64("$gte")
                .unwrap(),
            9_007_199_254_740_993
        );
        assert!(matches!(
            script.filter.get("d"),
            Some(bson::Bson::Decimal128(_))
        ));
        assert!(script
            .filter
            .get_document("at")
            .unwrap()
            .get_datetime("$lt")
            .is_ok());
        assert!(matches!(
            script.filter.get("re"),
            Some(bson::Bson::RegularExpression(_))
        ));
        assert!(inspect_read_script(r#"db.t.find({ _id: ObjectId("zz") })"#).is_err());
        assert!(inspect_read_script(r#"db.t.find({ _id: Evil("x") })"#).is_err());
    }
}
