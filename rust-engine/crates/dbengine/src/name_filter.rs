//! Server-side name filtering for catalog listings (sidebar search).
//!
//! User text is never interpolated into SQL: it is escaped here and bound as a
//! query parameter. Matching is a case-insensitive substring match.

/// Default cap for filtered catalog listings (callers show a "more results"
/// notice when a listing reaches it).
pub const DEFAULT_FILTER_LIMIT: usize = 500;

/// Escapes `\`, `%` and `_` so the text matches literally inside a `LIKE`
/// pattern written with `ESCAPE '\'`.
pub fn escape_like(input: &str) -> String {
    let mut escaped = String::with_capacity(input.len());
    for character in input.chars() {
        if matches!(character, '\\' | '%' | '_') {
            escaped.push('\\');
        }
        escaped.push(character);
    }
    escaped
}

/// `%escaped%`: the bound parameter for a substring `LIKE`/`ILIKE ... ESCAPE '\'`.
pub fn like_contains_pattern(filter: &str) -> String {
    format!("%{}%", escape_like(filter))
}

/// Escapes regex metacharacters so the text matches literally inside a regex
/// (MongoDB `$regex`, PCRE).
pub fn escape_regex(input: &str) -> String {
    let mut escaped = String::with_capacity(input.len());
    for character in input.chars() {
        if matches!(
            character,
            '\\' | '.' | '+' | '*' | '?' | '(' | ')' | '|' | '[' | ']' | '{' | '}' | '^' | '$' | '#' | '&' | '-' | '~'
        ) {
            escaped.push('\\');
        }
        escaped.push(character);
    }
    escaped
}

/// MongoDB `listCollections` / `listDatabases` name filter:
/// `{name: {$regex: <escaped>, $options: "i"}}`.
pub fn mongo_name_filter(filter: &str) -> mongodb::bson::Document {
    mongodb::bson::doc! {
        "name": { "$regex": escape_regex(filter), "$options": "i" }
    }
}

/// Clamps a caller-provided limit into a valid SQL `LIMIT` (at least 1).
pub fn sql_limit(limit: usize) -> i64 {
    i64::try_from(limit.max(1)).unwrap_or(i64::MAX)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn like_wildcards_and_backslashes_are_escaped() {
        assert_eq!(escape_like("plain"), "plain");
        assert_eq!(escape_like("50%_off\\x"), "50\\%\\_off\\\\x");
        assert_eq!(like_contains_pattern("a_b"), "%a\\_b%");
        assert_eq!(like_contains_pattern(""), "%%");
    }

    #[test]
    fn regex_metacharacters_are_escaped() {
        assert_eq!(escape_regex("a.b*c"), "a\\.b\\*c");
        assert_eq!(escape_regex("(x|y)[z]"), "\\(x\\|y\\)\\[z\\]");
        assert_eq!(escape_regex("^$\\"), "\\^\\$\\\\");
        assert_eq!(escape_regex("plain_name"), "plain_name");
    }

    #[test]
    fn mongo_filter_is_a_case_insensitive_escaped_regex() {
        let filter = mongo_name_filter("a.b");
        let name = filter.get_document("name").unwrap();
        assert_eq!(name.get_str("$regex").unwrap(), "a\\.b");
        assert_eq!(name.get_str("$options").unwrap(), "i");
    }

    #[test]
    fn sql_limit_is_at_least_one() {
        assert_eq!(sql_limit(0), 1);
        assert_eq!(sql_limit(500), 500);
        assert_eq!(sql_limit(usize::MAX), i64::MAX);
    }
}
