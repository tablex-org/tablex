//! Named parameters of a SQL text: `:name` and `${name}`.
//!
//! They are found with the driver kit's lexer for the connection's dialect, so a
//! `:name` inside a string, a quoted identifier, a comment or a `$$` body is not
//! one, and neither are PostgreSQL's `::` cast, `:=`, an array slice (`a[1:n]`) or
//! a JSON-style `a:b`. A parameter is bound by writing its value into the text as
//! a literal quoted for the dialect: no driver accepts bound values in
//! `QueryRequest.payload` (it carries `sql` and the run's flags only), so the
//! quoting here is the one thing between a typed value and the statement.

use engine_driver_kit::sql::{tokens, SqlLexOptions, SqlToken, SqlTokenKind};
use std::collections::HashMap;
use std::fmt;
use std::ops::Range;

/// One parameter of a text: its name, and every place it is written (the whole
/// `:name` / `${name}`), in text order.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Parameter {
    pub name: String,
    pub occurrences: Vec<Range<usize>>,
}

/// How a typed value becomes SQL.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum ValueKind {
    /// A string literal, quotes and escapes added.
    #[default]
    Text,
    /// A number, checked, written as typed.
    Number,
    /// `NULL`.
    Null,
    /// SQL written as typed: the user's explicit choice.
    Raw,
}

impl ValueKind {
    pub const ALL: [Self; 4] = [Self::Text, Self::Number, Self::Null, Self::Raw];

    pub fn label(self) -> &'static str {
        match self {
            Self::Text => "Text",
            Self::Number => "Number",
            Self::Null => "NULL",
            Self::Raw => "Raw SQL",
        }
    }
}

/// What the user typed for one parameter.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Value {
    pub kind: ValueKind,
    pub text: String,
}

/// Why a value cannot be written into the statement. Nothing is run then.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum BindError {
    /// A parameter of the text has no value.
    Missing(String),
    /// A "Number" value that is not a plain number.
    NotANumber(String),
    /// A text value with a NUL character, which no server takes in a string.
    NulInText(String),
    /// A "Raw SQL" value with nothing in it.
    EmptyRaw(String),
}

impl fmt::Display for BindError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Missing(name) => write!(f, "No value for :{name}"),
            Self::NotANumber(name) => write!(f, ":{name} is not a number"),
            Self::NulInText(name) => write!(f, ":{name} contains a NUL character"),
            Self::EmptyRaw(name) => write!(f, ":{name} has no SQL in it"),
        }
    }
}

impl std::error::Error for BindError {}

fn is_name(text: &str) -> bool {
    let mut chars = text.chars();
    chars
        .next()
        .is_some_and(|first| first.is_ascii_alphabetic() || first == '_')
        && chars.all(|c| c.is_ascii_alphanumeric() || c == '_')
}

/// The parameters of `sql`, each name once, in the order they first appear.
pub fn find(sql: &str, lex: &SqlLexOptions) -> Vec<Parameter> {
    let found = tokens(sql, lex).tokens;
    let text = |token: &SqlToken| &sql[token.range.clone()];
    let touching = |left: &SqlToken, right: &SqlToken| left.range.end == right.range.start;
    let mut parameters: Vec<Parameter> = Vec::new();
    let mut add = |name: &str, occurrence: Range<usize>| {
        match parameters.iter_mut().find(|parameter| parameter.name == name) {
            Some(parameter) => parameter.occurrences.push(occurrence),
            None => parameters.push(Parameter { name: name.to_owned(), occurrences: vec![occurrence] }),
        }
    };
    let mut at = 0;
    while at < found.len() {
        let token = &found[at];
        // `${name}`: `$`, `{`, a word, `}` with nothing between them. (`$` can also start a
        // dollar quote or a word, depending on the dialect: the shape of the text decides.)
        if let Some(end) = brace_parameter(sql, &found, at) {
            add(&sql[token.range.start + 2..found[end - 1].range.start], token.range.start..found[end - 1].range.end);
            at = end;
            continue;
        }
        if token.kind == SqlTokenKind::Other && text(token) == ":" {
            if let Some(word) = found.get(at + 1).filter(|word| {
                word.kind == SqlTokenKind::Word && touching(token, word) && is_name(text(word))
            }) {
                let previous = at.checked_sub(1).map(|index| &found[index]);
                let glued = previous.is_some_and(|previous| {
                    touching(previous, token)
                        && (matches!(
                            previous.kind,
                            SqlTokenKind::Word | SqlTokenKind::Number | SqlTokenKind::Quoted | SqlTokenKind::Close
                        ) || text(previous) == ":")
                });
                // `arr[:n]` is a slice; `ARRAY[:a, :b]` is a list of parameters.
                let slice = previous.is_some_and(|previous| text(previous) == "[" && touching(previous, token))
                    && !at
                        .checked_sub(2)
                        .is_some_and(|index| text(&found[index]).eq_ignore_ascii_case("array"));
                if !glued && !slice {
                    add(text(word), token.range.start..word.range.end);
                    at += 2;
                    continue;
                }
            }
        }
        at += 1;
    }
    parameters
}

/// `${name}` starting at token `at`: the index after its last token.
fn brace_parameter(sql: &str, found: &[SqlToken], at: usize) -> Option<usize> {
    let token = &found[at];
    let rest = &sql[token.range.start..];
    let name_length = rest.strip_prefix("${")?.find('}')?;
    let name = &rest[2..2 + name_length];
    if !is_name(name) {
        return None;
    }
    let end = token.range.start + 2 + name_length + 1;
    // The tokens covering the span: everything inside is part of the parameter, whatever the
    // dialect's lexer made of it, but it must not run into a token that goes beyond it.
    let last = found[at..].iter().position(|candidate| candidate.range.end >= end)? + at;
    (found[last].range.end == end).then_some(last + 1)
}

/// `sql` with every parameter of `parameters` replaced by its value, quoted for
/// the dialect `lex` describes.
pub fn bind(
    sql: &str,
    parameters: &[Parameter],
    values: &HashMap<String, Value>,
    lex: &SqlLexOptions,
) -> Result<String, BindError> {
    let mut replacements: Vec<(&Range<usize>, String)> = Vec::new();
    for parameter in parameters {
        let value = values
            .get(&parameter.name)
            .ok_or_else(|| BindError::Missing(parameter.name.clone()))?;
        let literal = literal(&parameter.name, value, lex)?;
        replacements.extend(parameter.occurrences.iter().map(|range| (range, literal.clone())));
    }
    replacements.sort_by_key(|(range, _)| range.start);
    let mut bound = String::with_capacity(sql.len());
    let mut copied = 0;
    for (range, literal) in replacements {
        bound.push_str(&sql[copied..range.start]);
        bound.push_str(&literal);
        copied = range.end;
    }
    bound.push_str(&sql[copied..]);
    Ok(bound)
}

fn literal(name: &str, value: &Value, lex: &SqlLexOptions) -> Result<String, BindError> {
    match value.kind {
        ValueKind::Null => Ok("NULL".into()),
        ValueKind::Number => {
            let text = value.text.trim();
            is_number(text)
                .then(|| text.to_owned())
                .ok_or_else(|| BindError::NotANumber(name.to_owned()))
        }
        ValueKind::Raw => {
            let text = value.text.trim();
            (!text.is_empty())
                .then(|| text.to_owned())
                .ok_or_else(|| BindError::EmptyRaw(name.to_owned()))
        }
        ValueKind::Text => {
            if value.text.contains('\0') {
                return Err(BindError::NulInText(name.to_owned()));
            }
            let mut quoted = String::with_capacity(value.text.len() + 2);
            quoted.push('\'');
            for c in value.text.chars() {
                match c {
                    '\'' => quoted.push_str("''"),
                    // Where a backslash escapes inside a string it must be escaped itself, or it
                    // would eat the quote that ends the literal.
                    '\\' if lex.backslash_escapes => quoted.push_str("\\\\"),
                    c => quoted.push(c),
                }
            }
            quoted.push('\'');
            Ok(quoted)
        }
    }
}

/// `-12`, `+3.5`, `.5`, `1e-3`: digits with an optional sign, point and exponent. Nothing else
/// (no `Infinity`, hex, separators or SQL) so a number field cannot carry a statement.
fn is_number(text: &str) -> bool {
    let digits = text.strip_prefix(['+', '-']).unwrap_or(text);
    let (mantissa, exponent) = match digits.split_once(['e', 'E']) {
        Some((mantissa, exponent)) => (mantissa, Some(exponent)),
        None => (digits, None),
    };
    let (whole, fraction) = mantissa.split_once('.').unwrap_or((mantissa, ""));
    let only_digits = |part: &str| part.bytes().all(|b| b.is_ascii_digit());
    only_digits(whole)
        && only_digits(fraction)
        && !(whole.is_empty() && fraction.is_empty())
        && exponent.is_none_or(|exponent| {
            let exponent = exponent.strip_prefix(['+', '-']).unwrap_or(exponent);
            !exponent.is_empty() && only_digits(exponent)
        })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn names(sql: &str, lex: &SqlLexOptions) -> Vec<String> {
        find(sql, lex).into_iter().map(|parameter| parameter.name).collect()
    }

    fn pg(sql: &str) -> Vec<String> {
        names(sql, &SqlLexOptions::postgres())
    }

    fn value(kind: ValueKind, text: &str) -> Value {
        Value { kind, text: text.into() }
    }

    fn bound(sql: &str, lex: &SqlLexOptions, values: &[(&str, Value)]) -> Result<String, BindError> {
        let values = values.iter().map(|(name, v)| ((*name).to_owned(), v.clone())).collect();
        bind(sql, &find(sql, lex), &values, lex)
    }

    #[test]
    fn finds_both_syntaxes_once_each_in_order() {
        assert_eq!(pg("select * from t where a = :id and b = ${name} and c = :id"), ["id", "name"]);
        let parameters = find("a = :x or b = :x", &SqlLexOptions::postgres());
        assert_eq!(parameters[0].occurrences, vec![4..6, 14..16]);
        assert_eq!(pg("select :a,:b, (:c)"), ["a", "b", "c"]);
        assert_eq!(pg("limit ${page_size} offset ${offset_1}"), ["page_size", "offset_1"]);
    }

    #[test]
    fn strings_comments_and_quoted_names_hide_their_contents() {
        assert!(pg("select ':a', '${b}', 'it''s :c'").is_empty());
        assert!(pg("select \":a\" from \"t:b\"").is_empty());
        assert!(pg("select 1 -- :a\n, 2 /* ${b} :c */").is_empty());
        assert!(pg("select $$ :a ${b} $$, $tag$ :c $tag$").is_empty());
        assert_eq!(pg("select 1 /* :a */, :b -- :c"), ["b"]);
    }

    #[test]
    fn casts_assignments_and_slices_are_not_parameters() {
        assert!(pg("select a::int, b::text, c::\"My Type\"").is_empty());
        assert!(pg("select 'x'::jsonb, (1+2)::int").is_empty());
        assert!(pg("select f(a := 1, b => 2)").is_empty());
        assert!(pg("set @v := 1").is_empty());
        assert!(pg("select arr[1:n], arr[lo:hi], arr[:n] from t").is_empty());
        assert!(pg("select a : b, a: b, ':'").is_empty());
        assert!(pg("select doc:field from t").is_empty());
        // The cast does not hide a real parameter next to it.
        assert_eq!(pg("select :v::int, :w::text"), ["v", "w"]);
        assert_eq!(pg("select arr[:lo] from t where x = :x"), ["x"]);
        assert_eq!(pg("select ANY(ARRAY[:a, :b])"), ["a", "b"]);
    }

    #[test]
    fn only_plain_names_count() {
        assert!(pg("select :1, :$x, : x, ${}, ${a b}, ${1a}, ${a-b}").is_empty());
        assert_eq!(pg("select :_x1, ${A_b}"), ["_x1", "A_b"]);
    }

    #[test]
    fn dialects_read_quotes_their_own_way() {
        let mysql = SqlLexOptions::mysql();
        // MySQL: "…" is a string, `…` a name; neither holds a parameter.
        assert!(names("select \"a :b\", `c :d`", &mysql).is_empty());
        assert_eq!(names("select \"a\", :p", &mysql), ["p"]);
        // ANSI_QUOTES: "…" is a name.
        assert!(names("select \"a :b\"", &mysql.with_ansi_quotes()).is_empty());
        // Backslash escapes keep a string open past `\'`.
        assert!(names("select 'it\\' :a'", &mysql).is_empty());
        assert_eq!(names("select 'it\\' :a' , :b", &mysql), ["b"]);
        // `#` and `-- ` comments.
        assert!(names("select 1 # :a\n", &mysql).is_empty());
        assert!(names("select 1 -- :a\n", &mysql).is_empty());
        // `$` is part of a name in MySQL, not the start of a dollar quote.
        assert_eq!(names("select ${p}", &mysql), ["p"]);
        let sqlite = SqlLexOptions::sqlite();
        assert!(names("select [a :b], \"c :d\", `e :f`", &sqlite).is_empty());
        assert_eq!(names("select * from t where id = :id", &sqlite), ["id"]);
    }

    #[test]
    fn binds_text_number_null_and_raw() {
        let lex = SqlLexOptions::postgres();
        let sql = "select :t, :n, :z, ${r}, :t";
        let out = bound(
            sql,
            &lex,
            &[
                ("t", value(ValueKind::Text, "it's")),
                ("n", value(ValueKind::Number, " -12.5e3 ")),
                ("z", value(ValueKind::Null, "ignored")),
                ("r", value(ValueKind::Raw, " now() ")),
            ],
        )
        .unwrap();
        assert_eq!(out, "select 'it''s', -12.5e3, NULL, now(), 'it''s'");
    }

    #[test]
    fn text_stays_a_literal_whatever_it_says() {
        let attack = "x'; DROP TABLE t; --";
        let pg = bound("select * from t where a = :a", &SqlLexOptions::postgres(), &[("a", value(ValueKind::Text, attack))])
            .unwrap();
        assert_eq!(pg, "select * from t where a = 'x''; DROP TABLE t; --'");
        // The result is still one statement with one literal, read by the same lexer.
        for lex in [SqlLexOptions::postgres(), SqlLexOptions::mysql(), SqlLexOptions::sqlite()] {
            for text in [attack, "x\\'; DROP TABLE t; --", "\\", "a\\\\", "'", "''", "$$ ; $$", "/* x"] {
                let out = bound("select :a; select 2", &lex, &[("a", value(ValueKind::Text, text))]).unwrap();
                let statements = crate::sql_script::statement_ranges(&out, &lex);
                assert_eq!(statements.len(), 2, "{text:?} on {out:?}");
                let first = tokens(&out[statements[0].clone()], &lex);
                assert!(!first.unterminated, "{text:?} left the literal open: {out:?}");
                let quoted: Vec<_> = first.tokens.iter().filter(|t| t.kind == SqlTokenKind::Quoted).collect();
                assert_eq!(quoted.len(), 1, "{text:?} on {out:?}");
            }
        }
    }

    #[test]
    fn mysql_text_escapes_the_backslash() {
        let out = bound("select :a", &SqlLexOptions::mysql(), &[("a", value(ValueKind::Text, "x\\'"))]).unwrap();
        assert_eq!(out, "select 'x\\\\'''");
        let ansi = SqlLexOptions::mysql().without_backslash_escapes();
        let out = bound("select :a", &ansi, &[("a", value(ValueKind::Text, "x\\'"))]).unwrap();
        assert_eq!(out, "select 'x\\'''");
    }

    #[test]
    fn a_number_field_cannot_carry_sql() {
        let lex = SqlLexOptions::postgres();
        for ok in ["0", "42", "-1", "+1", "1.5", ".5", "5.", "1e9", "1E-9", "-0.0"] {
            assert_eq!(bound(":n", &lex, &[("n", value(ValueKind::Number, ok))]).unwrap(), ok, "{ok}");
        }
        for bad in ["", " ", "1 or 1=1", "1;", "0x1f", "NaN", "Infinity", "1,5", "1_000", "--1", "e5", ".", "1e", "1e+", "١٢"] {
            assert_eq!(
                bound(":n", &lex, &[("n", value(ValueKind::Number, bad))]),
                Err(BindError::NotANumber("n".into())),
                "{bad:?}"
            );
        }
    }

    #[test]
    fn missing_nul_and_empty_raw_are_refused() {
        let lex = SqlLexOptions::postgres();
        assert_eq!(bound("select :a", &lex, &[]), Err(BindError::Missing("a".into())));
        assert_eq!(
            bound("select :a", &lex, &[("a", value(ValueKind::Text, "a\0b"))]),
            Err(BindError::NulInText("a".into()))
        );
        assert_eq!(
            bound("select :a", &lex, &[("a", value(ValueKind::Raw, "  "))]),
            Err(BindError::EmptyRaw("a".into()))
        );
    }

    #[test]
    fn text_without_parameters_is_returned_unchanged() {
        let lex = SqlLexOptions::postgres();
        let sql = "select 'a:b', x::int from \"t:1\" -- :c";
        assert!(find(sql, &lex).is_empty());
        assert_eq!(bound(sql, &lex, &[]).unwrap(), sql);
    }
}
