//! The guard every formatter's output passes before it may replace the user's text.
//!
//! A formatter that works on tokens of its own (the built-in one does) can misread a dialect: it
//! splits `ON DELETE SET NULL` over lines, upper-cases a column called `action`, re-lays the body
//! of a `$$ ... $$` string. The guard lexes the input and the output with the driver kit's
//! dialect-aware lexer (the one that cuts statements and spots writes) and accepts the output only
//! when it says the same thing:
//!
//! - the token sequences have the same length and the same kinds, token by token;
//! - every token is byte-identical, except a reserved word may change case (to all upper or all
//!   lower case): any other word that only differs in case is *restored* to the author's spelling,
//!   so identifiers keep their case whatever the formatter did;
//! - the comments between the tokens are the same, blanks inside them aside;
//! - whitespace the dialect reads as part of a token pair is kept: a space cannot appear in `<>`,
//!   `--`, `@x`, `a.b` or `N'x'`, and cannot be added between a function name and its `(`.
//!
//! Text with a `/*! ... */` executable comment or an unterminated string is never formatted: the
//! lexer cannot say what the server will read there.
//!
//! The guard knows nothing about who produced the output, so the formatter tool and an external
//! command go through the same check. It lives in the driver kit, next to the lexer, so the tool's
//! own tests can run it over the tool's output.

use crate::sql::{tokens, SqlLexOptions, SqlToken, SqlTokenKind};
use std::ops::Range;

/// Why an output was refused.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum GuardViolation {
    /// A string, quoted name or block comment is not closed (in the input or the output).
    Unterminated,
    /// A `/*! ... */` comment the server runs as code: its delimiters are not tokens.
    ExecutableComment,
    /// The output has a different number of tokens.
    TokenCount { before: usize, after: usize },
    /// Token number `index` (0-based, counting from the start of the formatted text) differs.
    TokenChanged { index: usize },
    /// Whitespace was added or removed where the dialect reads it as meaning (token `index`
    /// and the one after it).
    Spacing { index: usize },
    /// A comment was changed, moved or lost (the gap before token `index`).
    CommentChanged { index: usize },
}

impl std::fmt::Display for GuardViolation {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Unterminated => f.write_str("a string, quoted name or comment is not closed"),
            Self::ExecutableComment => {
                f.write_str("a /*! ... */ comment is code to the server and cannot be re-laid safely")
            }
            Self::TokenCount { before, after } => {
                write!(f, "the formatter changed the number of tokens ({before} became {after})")
            }
            Self::TokenChanged { index } => {
                write!(f, "the formatter changed token {} of the statement", index + 1)
            }
            Self::Spacing { index } => {
                write!(f, "the formatter changed spacing that matters next to token {}", index + 1)
            }
            Self::CommentChanged { index } => {
                write!(f, "the formatter changed or moved a comment before token {}", index + 1)
            }
        }
    }
}

impl std::error::Error for GuardViolation {}

impl GuardViolation {
    /// The first pair of tokens (the input's, the output's; `None` when one side has run out)
    /// that this violation is about, as text: what to show a user who may apply the output
    /// anyway. `None` for a violation that is not about a token pair (spacing, a comment, an
    /// unclosed string).
    pub fn differing_tokens(
        &self,
        original: &str,
        formatted: &str,
        lex: &SqlLexOptions,
    ) -> Option<(Option<String>, Option<String>)> {
        let (before, after) = (tokens(original, lex).tokens, tokens(formatted, lex).tokens);
        let text = |source: &str, list: &[SqlToken], index: usize| {
            list.get(index).map(|token| source[token.range.clone()].to_owned())
        };
        let index = match self {
            Self::TokenChanged { index } => *index,
            Self::TokenCount { .. } => (0..before.len().max(after.len())).find(|&index| {
                match (before.get(index), after.get(index)) {
                    (Some(old), Some(new)) => {
                        let (old_text, new_text) = (&original[old.range.clone()], &formatted[new.range.clone()]);
                        old.kind != new.kind
                            || !(old_text == new_text
                                || (old.kind == SqlTokenKind::Word && old_text.eq_ignore_ascii_case(new_text)))
                    }
                    _ => true,
                }
            })?,
            _ => return None,
        };
        Some((text(original, &before, index), text(formatted, &after, index)))
    }
}

/// The accepted output: its text (identifier case restored) and where each token went, to carry the
/// cursor across the re-layout.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Verified {
    pub text: String,
    /// `(range in the input, range in `text`)` of every token, in order.
    anchors: Vec<(Range<usize>, Range<usize>)>,
}

impl Verified {
    /// `text` taken without the checks: the user chose to apply an output the guard refused.
    /// No token positions are known, so [`Self::map_offset`] answers the end of the text.
    pub fn unchecked(text: String) -> Self {
        Self {
            text,
            anchors: Vec::new(),
        }
    }

    /// Where the input offset `offset` is in the output: the same place inside (or at the end of)
    /// the same token, or the start of the next token when it sat in whitespace.
    pub fn map_offset(&self, offset: usize) -> usize {
        let at = self.anchors.partition_point(|(from, _)| from.end < offset);
        match self.anchors.get(at) {
            Some((from, to)) if offset >= from.start => (to.start + (offset - from.start)).min(to.end),
            Some((_, to)) => to.start,
            None => self.text.len(),
        }
    }
}

/// Checks `formatted` against `original` for the dialect `lex`.
pub fn verify(original: &str, formatted: &str, lex: &SqlLexOptions) -> Result<Verified, GuardViolation> {
    let before = tokens(original, lex);
    let after = tokens(formatted, lex);
    if before.unterminated || after.unterminated {
        return Err(GuardViolation::Unterminated);
    }
    if before.executable_comment || after.executable_comment {
        return Err(GuardViolation::ExecutableComment);
    }
    if before.tokens.len() != after.tokens.len() {
        return Err(GuardViolation::TokenCount {
            before: before.tokens.len(),
            after: after.tokens.len(),
        });
    }
    let (before, after) = (before.tokens, after.tokens);

    let mut text = String::with_capacity(formatted.len());
    let mut copied = 0;
    for (index, (old, new)) in before.iter().zip(&after).enumerate() {
        let (old_text, new_text) = (&original[old.range.clone()], &formatted[new.range.clone()]);
        if old.kind != new.kind {
            return Err(GuardViolation::TokenChanged { index });
        }
        if old_text == new_text {
            continue;
        }
        let only_case = old.kind == SqlTokenKind::Word && old_text.eq_ignore_ascii_case(new_text);
        if !only_case {
            return Err(GuardViolation::TokenChanged { index });
        }
        let previous = index.checked_sub(1).map(|at| &original[before[at].range.clone()]);
        if is_reserved(old_text) || previous.is_some_and(|previous| is_phrase(previous, old_text)) {
            // A keyword's case is the formatter's to set: all upper or all lower, nothing mixed.
            let uniform = new_text.bytes().all(|b| !b.is_ascii_lowercase())
                || new_text.bytes().all(|b| !b.is_ascii_uppercase());
            if !uniform {
                return Err(GuardViolation::TokenChanged { index });
            }
        } else {
            // An identifier (or a word this guard cannot call a keyword in every dialect): the
            // author's spelling stays. Same bytes long, so the other ranges are unaffected.
            text.push_str(&formatted[copied..new.range.start]);
            text.push_str(old_text);
            copied = new.range.end;
        }
    }
    text.push_str(&formatted[copied..]);

    for index in 0..before.len().saturating_sub(1) {
        if !spacing_kept(original, formatted, &before[index..=index + 1], &after[index..=index + 1]) {
            return Err(GuardViolation::Spacing { index });
        }
    }
    if let Some(index) = first_comment_difference(original, formatted, &before, &after, lex) {
        return Err(GuardViolation::CommentChanged { index });
    }

    let anchors = before
        .iter()
        .zip(&after)
        .map(|(old, new)| (old.range.clone(), new.range.clone()))
        .collect();
    Ok(Verified { text, anchors })
}

/// Bytes of an operator that reads the same with or without blanks around it.
fn is_free_operator(token: &SqlToken, source: &str) -> bool {
    token.kind == SqlTokenKind::Other
        && matches!(
            source.as_bytes()[token.range.start],
            b'=' | b'+' | b'-' | b'*' | b'/' | b'<' | b'>' | b'%' | b'|' | b'&' | b'^' | b'!' | b'~'
        )
}

/// Whether the pair `(a, b)` of the input is spaced the way the pair of the output is, where the
/// dialect makes that matter.
fn spacing_kept(original: &str, formatted: &str, old: &[SqlToken], new: &[SqlToken]) -> bool {
    use SqlTokenKind::{Close, Comma, Open, Other, Semicolon, Word};
    let (a, b) = (old[0].kind, old[1].kind);
    // Blanks beside brackets, commas and `;` are the formatter's to place, except that a function
    // name must not gain a space before its `(` (MySQL reads `count (x)` differently).
    if matches!(a, Close | Comma | Semicolon) || matches!(b, Close | Comma | Semicolon) {
        return true;
    }
    let joined_before = old[0].range.end == old[1].range.start;
    let joined_after = new[0].range.end == new[1].range.start;
    if b == Open {
        return !(a == Word && joined_before && !joined_after);
    }
    if a == Open {
        return true;
    }
    if joined_before == joined_after {
        return true;
    }
    // An operator beside an operand may gain or lose blanks; two operators, a prefix character
    // (`@x`, `$1`, `:id`, `a.b`, `[`) or a prefixed string (`N'x'`) may not.
    let free_beside_operand = |operator: &SqlToken, operand: &SqlToken, source: &str| {
        is_free_operator(operator, source) && operand.kind != Other
    };
    (free_beside_operand(&old[0], &old[1], original) && free_beside_operand(&new[0], &new[1], formatted))
        || (free_beside_operand(&old[1], &old[0], original) && free_beside_operand(&new[1], &new[0], formatted))
}

/// Index of the first token whose preceding comments differ, if any (`tokens.len()` for the text
/// after the last token).
fn first_comment_difference(
    original: &str,
    formatted: &str,
    before: &[SqlToken],
    after: &[SqlToken],
    lex: &SqlLexOptions,
) -> Option<usize> {
    let gap = |source: &str, list: &[SqlToken], index: usize| -> Range<usize> {
        let start = if index == 0 { 0 } else { list[index - 1].range.end };
        let end = list.get(index).map_or(source.len(), |token| token.range.start);
        start..end
    };
    (0..=before.len()).find(|&index| {
        let old = &original[gap(original, before, index)];
        let new = &formatted[gap(formatted, after, index)];
        if old.trim().is_empty() && new.trim().is_empty() {
            return false;
        }
        !comments_equal(old, new, lex)
    })
}

fn comments_equal(old: &str, new: &str, lex: &SqlLexOptions) -> bool {
    let (mut old, mut new) = (Comments { rest: old, lex }, Comments { rest: new, lex });
    loop {
        match (old.next(), new.next()) {
            (None, None) => return true,
            (Some(a), Some(b)) => {
                // Blanks inside a comment may be re-indented; its words may not change.
                if !a.split_whitespace().eq(b.split_whitespace()) {
                    return false;
                }
            }
            _ => return false,
        }
    }
}

/// The comments of a stretch of text that holds only blanks and comments.
struct Comments<'a> {
    rest: &'a str,
    lex: &'a SqlLexOptions,
}

impl<'a> Iterator for Comments<'a> {
    type Item = &'a str;

    fn next(&mut self) -> Option<&'a str> {
        let rest = self.rest.trim_start();
        if rest.is_empty() {
            self.rest = rest;
            return None;
        }
        let length = if rest.starts_with("/*") {
            block_comment_len(rest, self.lex.nested_block_comments)
        } else {
            let ends_at = |b: u8| b == b'\n' || (self.lex.line_comment_ends_at_cr && b == b'\r');
            rest.bytes().position(ends_at).unwrap_or(rest.len())
        };
        let (comment, rest) = rest.split_at(length);
        self.rest = rest;
        Some(comment)
    }
}

fn block_comment_len(text: &str, nested: bool) -> usize {
    let bytes = text.as_bytes();
    let (mut depth, mut at) = (0usize, 0usize);
    while at < bytes.len() {
        match bytes.get(at..at + 2) {
            Some(b"/*") if at == 0 || nested => {
                depth += 1;
                at += 2;
            }
            Some(b"*/") => {
                depth -= 1;
                at += 2;
                if depth == 0 || !nested {
                    return at;
                }
            }
            _ => at += 1,
        }
    }
    bytes.len()
}

/// Words that are keywords in PostgreSQL, MySQL / MariaDB and SQLite alike, so a keyword-case
/// setting may change their case. Any other word keeps the author's case: a column named `action`,
/// `key` or `name` is a keyword to a formatter's word list, not to every server.
const RESERVED: &[&str] = &[
    "ALL", "ALTER", "AND", "AS", "ASC", "BETWEEN", "BY", "CASE", "CHECK", "COLUMN", "CONSTRAINT",
    "CREATE", "CROSS", "DEFAULT", "DELETE", "DESC", "DISTINCT", "DROP", "ELSE", "END", "EXCEPT",
    "EXISTS", "FALSE", "FOR", "FOREIGN", "FROM", "FULL", "GROUP", "HAVING", "IN", "INDEX", "INNER",
    "INSERT", "INTERSECT", "INTO", "IS", "JOIN", "LEFT", "LIKE", "LIMIT", "NOT", "NULL", "OFFSET",
    "ON", "OR", "ORDER", "OUTER", "PRIMARY", "REFERENCES", "RETURNING", "RIGHT", "SELECT", "SET",
    "TABLE", "THEN", "TO", "TRUE", "UNION", "UNIQUE", "UPDATE", "USING", "VALUES", "WHEN", "WHERE",
    "WITH",
];

/// Second words of a keyword phrase that are only keywords after the first (`PRIMARY KEY`,
/// `ON DELETE CASCADE`, `NO ACTION`): alone they are ordinary names, so they follow their first
/// word's case instead of getting a place in [`RESERVED`].
const PHRASES: &[(&str, &str)] = &[
    ("DELETE", "CASCADE"),
    ("DELETE", "RESTRICT"),
    ("FOREIGN", "KEY"),
    ("NO", "ACTION"),
    ("PRIMARY", "KEY"),
    ("UPDATE", "CASCADE"),
    ("UPDATE", "RESTRICT"),
];

fn is_phrase(first: &str, second: &str) -> bool {
    PHRASES
        .iter()
        .any(|(a, b)| first.eq_ignore_ascii_case(a) && second.eq_ignore_ascii_case(b))
}

fn is_reserved(word: &str) -> bool {
    RESERVED
        .binary_search_by(|reserved| {
            reserved
                .bytes()
                .cmp(word.bytes().map(|byte| byte.to_ascii_uppercase()))
        })
        .is_ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn pg() -> SqlLexOptions {
        SqlLexOptions::postgres()
    }

    #[test]
    fn the_reserved_list_is_sorted_for_the_binary_search() {
        assert!(RESERVED.windows(2).all(|pair| pair[0] < pair[1]));
        assert!(is_reserved("select") && is_reserved("Where") && !is_reserved("action"));
    }

    #[test]
    fn layout_and_keyword_case_may_change() {
        let ok = verify("select a,b from t where x=1", "SELECT\n  a,\n  b\nFROM\n  t\nWHERE\n  x = 1", &pg()).unwrap();
        assert!(ok.text.starts_with("SELECT"));
    }

    #[test]
    fn a_word_that_only_changes_case_and_is_not_a_keyword_gets_its_spelling_back() {
        let ok = verify("select action, Name from t", "SELECT\n  ACTION,\n  NAME\nFROM\n  t", &pg()).unwrap();
        assert_eq!(ok.text, "SELECT\n  action,\n  Name\nFROM\n  t");
    }

    #[test]
    fn a_changed_token_is_refused() {
        for (before, after) in [
            ("select 'a'", "select 'A'"),
            ("select \"Col\"", "select \"COL\""),
            ("select 1", "select 2"),
            ("select a", "select b"),
            ("select a", "select\na\nlimit 1"),
            ("select 1 from t", "select 1 from t where"),
        ] {
            assert!(verify(before, after, &pg()).is_err(), "{before} -> {after}");
        }
        assert_eq!(
            verify("select a", "select b", &pg()),
            Err(GuardViolation::TokenChanged { index: 1 })
        );
    }

    #[test]
    fn mixed_case_keywords_are_refused() {
        assert!(verify("select 1", "SeLeCt 1", &pg()).is_err());
    }

    #[test]
    fn spacing_that_changes_meaning_is_refused() {
        for (before, after) in [
            ("select a<>b", "select a < > b"),
            ("select a--b\n", "select a - - b\n"),
            ("select @x", "select @ x"),
            ("select t.a", "select t . a"),
            ("select N'x'", "select N 'x'"),
            ("select count(*)", "select count (*)"),
            ("select a - -1", "select a--1"),
        ] {
            assert!(
                matches!(verify(before, after, &SqlLexOptions::mysql()), Err(GuardViolation::Spacing { .. } | GuardViolation::TokenCount { .. })),
                "{before} -> {after}"
            );
        }
        // Blanks around plain operators, commas and brackets are free.
        assert!(verify("select a=1,b", "select a = 1,\n b", &pg()).is_ok());
        assert!(verify("select f (a )", "select f(a)", &pg()).is_ok());
    }

    #[test]
    fn comments_must_stay_where_they_are_but_may_be_re_indented() {
        assert!(verify("select 1 -- c\nfrom t", "select 1 -- c\nfrom t", &pg()).is_ok());
        assert!(verify("select /* a\n b */ 1", "select /* a\n    b */ 1", &pg()).is_ok());
        assert_eq!(
            verify("select 1 -- c\nfrom t", "select 1\nfrom t", &pg()),
            Err(GuardViolation::CommentChanged { index: 2 })
        );
        assert!(verify("select 1 -- c\nfrom t", "-- c\nselect 1\nfrom t", &pg()).is_err());
        assert!(verify("select 1 /* c */", "select 1 /* d */", &pg()).is_err());
    }

    #[test]
    fn executable_and_unterminated_text_is_never_formatted() {
        let mysql = SqlLexOptions::mysql();
        assert_eq!(
            verify("select /*!50000 1 */", "select /*!50000 1 */", &mysql),
            Err(GuardViolation::ExecutableComment)
        );
        assert_eq!(verify("select 'a", "select 'a", &pg()), Err(GuardViolation::Unterminated));
    }

    #[test]
    fn dialect_quoting_is_read_by_the_dialect_lexer() {
        // `"x"` is a string on MySQL (its case must stay), an identifier with ANSI_QUOTES.
        assert!(verify("select \"X\"", "select \"x\"", &SqlLexOptions::mysql()).is_err());
        assert!(verify("select `A b`", "select `a b`", &SqlLexOptions::mysql()).is_err());
        assert!(verify("select \"A\"", "select \"A\"", &SqlLexOptions::mysql().with_ansi_quotes()).is_ok());
        // A `$$` body is one token: re-laying it out is refused, keeping it is fine.
        let body = "create function f() returns int as $$ begin return 1; end $$ language sql";
        assert!(verify(body, &body.replace("begin return 1;", "begin\n return 1;"), &pg()).is_err());
        assert!(verify(body, &body.replace(" as ", "\nas\n"), &pg()).is_ok());
    }

    #[test]
    fn the_first_differing_pair_of_tokens_is_named() {
        let lex = pg();
        let pair = |before: &str, after: &str| {
            let violation = verify(before, after, &lex).unwrap_err();
            violation.differing_tokens(before, after, &lex)
        };
        assert_eq!(pair("select a from t", "select b from t"), Some((Some("a".into()), Some("b".into()))));
        // A longer output: the first token that has no partner, and then the extra ones.
        assert_eq!(
            pair("select 1 from t", "select 1 from t where"),
            Some((None, Some("where".into())))
        );
        assert_eq!(pair("select a, b", "select a b"), Some((Some(",".into()), Some("b".into()))));
        // Spacing and comments are not about a pair of tokens.
        assert_eq!(pair("select count(*)", "select count (*)"), None);
        assert_eq!(pair("select 1 -- c\nfrom t", "select 1\nfrom t"), None);
    }

    #[test]
    fn unchecked_text_is_taken_as_is() {
        let applied = Verified::unchecked("SELECT 2".into());
        assert_eq!(applied.text, "SELECT 2");
        assert_eq!(applied.map_offset(3), 8);
    }

    #[test]
    fn offsets_follow_their_token() {
        let ok = verify("select  a ,b", "SELECT\n  a,\n  b", &pg()).unwrap();
        assert_eq!(ok.map_offset(0), 0);
        // Inside `a`, and right after it.
        assert_eq!(ok.map_offset(8), 9);
        assert_eq!(ok.map_offset(9), 10);
        // In the blanks between `select` and `a`: the start of `a`.
        assert_eq!(ok.map_offset(7), 9);
        assert_eq!(ok.map_offset(100), ok.text.len());
    }
}
