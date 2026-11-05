pub use engine_syntax::HighlightToken;
use engine_syntax::SyntaxProfile;
use tree_sitter::Language;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SyntaxLanguage {
    Json,
    MongoExpression,
    JavaScript,
    Sql,
}

fn language_for(lang: SyntaxLanguage) -> Language {
    match lang {
        SyntaxLanguage::Json => tree_sitter_json::LANGUAGE.into(),
        SyntaxLanguage::MongoExpression | SyntaxLanguage::JavaScript => {
            tree_sitter_javascript::LANGUAGE.into()
        }
        SyntaxLanguage::Sql => tree_sitter_sequel::LANGUAGE.into(),
    }
}

fn profile_for(lang: SyntaxLanguage) -> SyntaxProfile {
    match lang {
        SyntaxLanguage::Json => SyntaxProfile::Json,
        SyntaxLanguage::MongoExpression => SyntaxProfile::MongoExpression,
        SyntaxLanguage::JavaScript => SyntaxProfile::JavaScript,
        SyntaxLanguage::Sql => SyntaxProfile::Sql,
    }
}

pub fn highlight_syntax(input: &str, language: SyntaxLanguage) -> Vec<HighlightToken> {
    engine_syntax::highlight_syntax(input, language_for(language), profile_for(language))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn highlights_json_properties_and_values() {
        let input = r#"{"name":"A","count":1,"ok":true,"x":null}"#;
        let tokens = highlight_syntax(input, SyntaxLanguage::Json);
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
    fn highlights_mongo_operators_and_functions() {
        let input = r#"[{ $match: { _id: ObjectId("6982"), merchantCode: "SAGEVB" } }]"#;
        let tokens = highlight_syntax(input, SyntaxLanguage::MongoExpression);
        let kinds: Vec<&str> = tokens.iter().map(|t| t.kind).collect();
        assert!(kinds.contains(&"operator"));
        assert!(kinds.contains(&"function"));
        assert!(kinds.contains(&"property"));
        assert!(kinds.contains(&"string"));
    }

    #[test]
    fn walk_recurses_into_object_pair() {
        let tokens = highlight_syntax(r#"{"a": 1, "b": 2}"#, SyntaxLanguage::Json);
        let kinds: Vec<&str> = tokens.iter().map(|t| t.kind).collect();
        assert!(
            kinds.contains(&"property"),
            "expected property token, got {:?}",
            kinds
        );
    }

    #[test]
    fn highlights_quoted_mongo_operator_keys() {
        let src = r#"[{"$match": {"x": 1}}, {"$sort": {"y": -1}}]"#;
        let tokens = highlight_syntax(src, SyntaxLanguage::MongoExpression);
        let kinds: Vec<&str> = tokens.iter().map(|t| t.kind).collect();
        assert!(
            kinds.contains(&"operator"),
            "expected operator token for $match/$sort, got {:?}",
            kinds
        );
    }

    #[test]
    fn highlights_quoted_mongo_property_keys() {
        let src = r#"[{"merchantCode": 1, "total": 2.5}]"#;
        let tokens = highlight_syntax(src, SyntaxLanguage::MongoExpression);
        let kinds: Vec<&str> = tokens.iter().map(|t| t.kind).collect();
        assert!(
            kinds.contains(&"property"),
            "expected property token for non-$ keys, got {:?}",
            kinds
        );
        assert!(
            !kinds.iter().any(|k| *k == "operator"),
            "did not expect operator for non-$ keys, got {:?}",
            kinds
        );
    }

    #[test]
    fn highlights_realistic_pipeline() {
        let src = r#"[{"$lookup": {"from": "orders", "localField": "id", "foreignField": "uid", "as": "orders"}}, {"$match": {"status": "active"}}]"#;
        let tokens = highlight_syntax(src, SyntaxLanguage::MongoExpression);
        let kinds: Vec<&str> = tokens.iter().map(|t| t.kind).collect();
        assert!(
            kinds.contains(&"operator"),
            "expected operator, got {:?}",
            kinds
        );
        assert!(
            kinds.contains(&"property"),
            "expected property, got {:?}",
            kinds
        );
        assert!(
            kinds.contains(&"string"),
            "expected string, got {:?}",
            kinds
        );
    }

    #[test]
    fn dollar_only_identifier_is_operator() {
        let tokens = highlight_syntax("{ $eq: 1 }", SyntaxLanguage::MongoExpression);
        assert!(tokens
            .iter()
            .any(|t| t.kind == "operator" && t.kind == "operator"));
        assert!(tokens.iter().any(|t| t.kind == "number"));
    }

    #[test]
    fn highlights_sql_keywords_and_literals() {
        let src = "SELECT product_name, price FROM products WHERE price > 100 AND category = 'books' LIMIT 10";
        let tokens = highlight_syntax(src, SyntaxLanguage::Sql);
        let kinds: Vec<&str> = tokens.iter().map(|t| t.kind).collect();
        assert!(
            kinds.contains(&"keyword"),
            "expected keyword, got {:?}",
            kinds
        );
        assert!(
            kinds.contains(&"identifier"),
            "expected identifier, got {:?}",
            kinds
        );
        assert!(
            !kinds.contains(&"property"),
            "did not expect SQL identifiers to use JSON/Mongo property color, got {:?}",
            kinds
        );
        assert!(
            kinds.contains(&"number"),
            "expected number, got {:?}",
            kinds
        );
        assert!(
            kinds.contains(&"string"),
            "expected string, got {:?}",
            kinds
        );
        assert!(
            kinds.contains(&"operator"),
            "expected operator, got {:?}",
            kinds
        );
    }

    #[test]
    fn highlights_sql_functions() {
        let src = "SELECT count(*) FROM products";
        let tokens = highlight_syntax(src, SyntaxLanguage::Sql);
        assert!(
            tokens.iter().any(|t| t.kind == "function"),
            "expected function token, got {:?}",
            tokens
        );
    }
}
