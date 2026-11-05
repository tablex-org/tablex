use serde::Serialize;
use tree_sitter::{Language, Node, Parser};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SyntaxProfile {
    Json,
    MongoExpression,
    JavaScript,
    Sql,
}

#[derive(Debug, Serialize, PartialEq, Eq)]
pub struct HighlightToken {
    pub start: usize,
    pub end: usize,
    pub kind: &'static str,
}

fn parse(input: &str, language: Language) -> Option<tree_sitter::Tree> {
    let mut parser = Parser::new();
    parser.set_language(&language).ok()?;
    parser.parse(input, None)
}

pub fn highlight_syntax(
    input: &str,
    language: Language,
    profile: SyntaxProfile,
) -> Vec<HighlightToken> {
    let Some(tree) = parse(input, language) else {
        return Vec::new();
    };
    let mut tokens = Vec::with_capacity(64);
    let root = tree.root_node();
    walk(root, input.as_bytes(), &mut tokens, profile);
    tokens.sort_by_key(|t| t.start);
    tokens
}

fn walk(node: Node, source: &[u8], out: &mut Vec<HighlightToken>, profile: SyntaxProfile) {
    let kind = node.kind();
    if matches!(profile, SyntaxProfile::Json) && kind == "pair" {
        let mut cursor = node.walk();
        for child in node.children(&mut cursor) {
            walk(child, source, out, profile);
        }
        return;
    }
    if let Some(tok_kind) = classify(node, kind, source, profile) {
        out.push(HighlightToken {
            start: node.byte_range().start,
            end: node.byte_range().end,
            kind: tok_kind,
        });
        return;
    }
    let mut cursor = node.walk();
    for child in node.children(&mut cursor) {
        walk(child, source, out, profile);
    }
}

fn classify(node: Node, kind: &str, source: &[u8], profile: SyntaxProfile) -> Option<&'static str> {
    match profile {
        SyntaxProfile::Json => classify_json(node, kind, source),
        SyntaxProfile::MongoExpression | SyntaxProfile::JavaScript => {
            classify_mongo(node, kind, source)
        }
        SyntaxProfile::Sql => classify_sql(node, kind, source),
    }
}

fn classify_json(node: Node, kind: &str, _source: &[u8]) -> Option<&'static str> {
    if node.is_error() || kind == "ERROR" {
        return Some("error");
    }
    match kind {
        "string" => {
            let is_pair_key = node.parent().is_some_and(|p| {
                p.kind() == "pair" && p.child(0).is_some_and(|first| first.id() == node.id())
            });
            if is_pair_key {
                Some("property")
            } else {
                Some("string")
            }
        }
        "number" => Some("number"),
        "true" | "false" => Some("boolean"),
        "null" => Some("null"),
        _ => None,
    }
}

fn classify_mongo(node: Node, kind: &str, source: &[u8]) -> Option<&'static str> {
    if node.is_error() || kind == "ERROR" {
        return Some("error");
    }
    match kind {
        "comment" => Some("comment"),
        "string" => classify_quoted_mongo_key(node, source),
        "string_fragment" => Some("string"),
        "number" => Some("number"),
        "true" | "false" => Some("boolean"),
        "null" => Some("null"),
        "property_identifier" => {
            let text = node_text(node, source);
            if text.starts_with('$') {
                Some("operator")
            } else {
                Some("property")
            }
        }
        "identifier" => {
            let in_call_expr = node.parent().is_some_and(|p| {
                p.kind() == "call_expression"
                    && p.child(0).is_some_and(|first| first.id() == node.id())
            });
            if in_call_expr {
                return Some("function");
            }
            let text = node_text(node, source);
            if text.starts_with('$') {
                Some("operator")
            } else {
                None
            }
        }
        "shorthand_property_identifier_pattern" => {
            let text = node_text(node, source);
            if text.starts_with('$') {
                Some("operator")
            } else {
                Some("property")
            }
        }
        _ => None,
    }
}

fn classify_quoted_mongo_key(node: Node, source: &[u8]) -> Option<&'static str> {
    let parent = node.parent();
    let is_pair_key = parent.is_some_and(|p| {
        p.kind() == "pair" && p.child(0).is_some_and(|first| first.id() == node.id())
    });
    if !is_pair_key {
        return Some("string");
    }
    let raw = node_text(node, source);
    let trimmed = raw
        .strip_prefix('"')
        .and_then(|s| s.strip_suffix('"'))
        .unwrap_or(raw);
    if trimmed.starts_with('$') {
        Some("operator")
    } else {
        Some("property")
    }
}

fn classify_sql(node: Node, kind: &str, source: &[u8]) -> Option<&'static str> {
    if node.is_error() || kind == "ERROR" {
        return Some("error");
    }

    match kind {
        "comment" | "line_comment" | "block_comment" => Some("comment"),
        "string" | "string_literal" | "quoted_literal" | "escape_string" | "keyword_string" => {
            Some("string")
        }
        "number" | "integer" | "float" | "numeric" | "numeric_literal" => Some("number"),
        "literal" => classify_sql_literal(node, source),
        "keyword_function" | "window_function" | "function_call" => Some("function"),
        "operator" | ">" | "<" | "=" | ">=" | "<=" | "<>" | "!=" | "+" | "-" | "*" | "/" => {
            Some("operator")
        }
        "identifier" | "object_reference" | "field" | "table_reference" => {
            let text = node_text(node, source);
            if is_sql_keyword(text) {
                Some("keyword")
            } else if is_sql_function(node, source) {
                Some("function")
            } else {
                Some("identifier")
            }
        }
        _ => {
            let text = node_text(node, source);
            if is_sql_keyword(text) {
                Some("keyword")
            } else if is_sql_function(node, source) {
                Some("function")
            } else if text.len() == 1 && matches!(text, ">" | "<" | "=" | "+" | "-" | "*" | "/") {
                Some("operator")
            } else {
                None
            }
        }
    }
}

fn is_sql_keyword(text: &str) -> bool {
    matches!(
        text.to_ascii_uppercase().as_str(),
        "SELECT"
            | "FROM"
            | "WHERE"
            | "AND"
            | "OR"
            | "NOT"
            | "NULL"
            | "IS"
            | "IN"
            | "LIKE"
            | "ILIKE"
            | "BETWEEN"
            | "JOIN"
            | "INNER"
            | "LEFT"
            | "RIGHT"
            | "FULL"
            | "OUTER"
            | "ON"
            | "GROUP"
            | "BY"
            | "ORDER"
            | "LIMIT"
            | "OFFSET"
            | "HAVING"
            | "DISTINCT"
            | "AS"
            | "CASE"
            | "WHEN"
            | "THEN"
            | "ELSE"
            | "END"
            | "WITH"
            | "UNION"
            | "ALL"
            | "TRUE"
            | "FALSE"
            | "ASC"
            | "DESC"
    )
}

fn classify_sql_literal(node: Node, source: &[u8]) -> Option<&'static str> {
    let text = node_text(node, source).trim();
    if text.starts_with('\'') || text.starts_with('"') {
        Some("string")
    } else if text.parse::<f64>().is_ok() {
        Some("number")
    } else if matches!(text.to_ascii_uppercase().as_str(), "TRUE" | "FALSE") {
        Some("boolean")
    } else if text.eq_ignore_ascii_case("NULL") {
        Some("null")
    } else {
        None
    }
}

fn is_sql_function(node: Node, source: &[u8]) -> bool {
    let text = node_text(node, source);
    if text.is_empty() || is_sql_keyword(text) {
        return false;
    }
    node.next_named_sibling()
        .is_some_and(|sibling| sibling.kind() == "(" || node_text(sibling, source) == "(")
        || node
            .next_sibling()
            .is_some_and(|sibling| node_text(sibling, source) == "(")
}

fn node_text<'a>(node: Node, source: &'a [u8]) -> &'a str {
    let range = node.byte_range();
    std::str::from_utf8(&source[range.start..range.end]).unwrap_or("")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn json_language() -> Language {
        tree_sitter_json::LANGUAGE.into()
    }

    #[test]
    fn highlights_json_properties_and_values() {
        let input = r#"{"name":"A","count":1,"ok":true,"x":null}"#;
        let tokens = highlight_syntax(input, json_language(), SyntaxProfile::Json);
        let kinds: Vec<&str> = tokens.iter().map(|t| t.kind).collect();
        for t in &tokens {
            let s = &input[t.start..t.end];
            eprintln!("{}..{} {:?} {:?}", t.start, t.end, t.kind, s);
        }
        assert!(kinds.contains(&"property"));
        assert!(kinds.contains(&"string"));
        assert!(kinds.contains(&"number"));
        assert!(kinds.contains(&"boolean"));
        assert!(kinds.contains(&"null"));
    }

    #[test]
    fn highlight_token_offsets_are_utf8_bytes_even_after_emoji() {
        let input = r#"{"emoji":"😀","name":"Alice"}"#;
        let name_offset = input.find("\"name\"").expect("name key present");
        let tokens = highlight_syntax(input, json_language(), SyntaxProfile::Json);

        let name_token = tokens
            .iter()
            .find(|token| token.start == name_offset)
            .expect("property token anchored at utf8 byte offset of \"name\"");

        assert_eq!(name_token.kind, "property");
        assert_eq!(&input[name_token.start..name_token.end], "\"name\"");
    }

    #[test]
    fn walk_recurses_into_object_pair() {
        let tokens = highlight_syntax(r#"{"a": 1, "b": 2}"#, json_language(), SyntaxProfile::Json);
        let kinds: Vec<&str> = tokens.iter().map(|t| t.kind).collect();
        assert!(
            kinds.contains(&"property"),
            "expected property token, got {:?}",
            kinds
        );
    }
}
