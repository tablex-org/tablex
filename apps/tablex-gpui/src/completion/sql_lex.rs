//! A small, forgiving SQL lexer for completion. Editors hold half-typed text,
//! so unterminated strings, comments and quoted identifiers are tokens (flagged
//! `terminated: false`) instead of errors. Nothing here allocates except the
//! token vector of the single statement under the cursor.

use std::ops::Range;

use super::SqlFlavor;

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(super) enum Tok {
    Word,
    /// `"ident"` / `` `ident` ``, quotes included.
    Quoted,
    /// `'text'` (and `"text"` where double quotes are strings), `$$text$$`.
    Str,
    Num,
    Dot,
    Comma,
    Open,
    Close,
    Semi,
    Op,
    Comment,
}

#[derive(Clone, Copy, Debug)]
pub(super) struct Token {
    pub kind: Tok,
    pub start: usize,
    pub end: usize,
    /// Closing delimiter present. Line comments never terminate.
    pub terminated: bool,
}

impl Token {
    pub(super) fn text<'a>(&self, source: &'a str) -> &'a str {
        &source[self.start..self.end]
    }

    /// Cursor strictly inside, or at the open end of, a literal / comment.
    pub(super) fn contains_cursor(&self, cursor: usize) -> bool {
        self.start < cursor && (cursor < self.end || (cursor == self.end && !self.terminated))
    }
}

fn is_word_start(c: char) -> bool {
    c == '_' || c.is_alphabetic()
}

fn is_word_char(c: char) -> bool {
    c == '_' || c == '$' || c.is_alphanumeric()
}

struct Scanner<'a> {
    source: &'a str,
    pos: usize,
    flavor: SqlFlavor,
}

impl<'a> Scanner<'a> {
    fn rest(&self) -> &'a str {
        &self.source[self.pos..]
    }

    fn peek(&self) -> Option<char> {
        self.rest().chars().next()
    }

    fn mysql(&self) -> bool {
        matches!(self.flavor, SqlFlavor::Mysql | SqlFlavor::Mariadb)
    }

    /// `\` escapes inside `'…'`, and `#` starts a line comment.
    fn backslash_strings(&self) -> bool {
        self.mysql() || self.flavor == SqlFlavor::Clickhouse
    }

    /// Consumes up to and including `close`; a doubled `close` (or a
    /// backslash on MySQL strings) escapes it.
    fn quoted(&mut self, close: char, backslash: bool) -> bool {
        self.pos += close.len_utf8();
        while let Some(c) = self.peek() {
            self.pos += c.len_utf8();
            if backslash && c == '\\' {
                self.pos += self.peek().map_or(0, char::len_utf8);
            } else if c == close {
                if self.peek() == Some(close) {
                    self.pos += close.len_utf8();
                } else {
                    return true;
                }
            }
        }
        false
    }

    fn block_comment(&mut self) -> bool {
        self.pos += 2;
        match self.rest().find("*/") {
            Some(end) => {
                self.pos += end + 2;
                true
            }
            None => {
                self.pos = self.source.len();
                false
            }
        }
    }

    fn line_comment(&mut self) {
        self.pos += self.rest().find('\n').unwrap_or(self.rest().len());
    }

    /// `$tag$ ... $tag$` (PostgreSQL); the caller checked the opening tag.
    fn dollar_quoted(&mut self) -> bool {
        let rest = self.rest();
        let tag_len = rest[1..].find('$').map_or(1, |close| close + 2);
        let tag = &rest[..tag_len];
        self.pos += tag_len;
        match self.rest().find(tag) {
            Some(end) => {
                self.pos += end + tag_len;
                true
            }
            None => {
                self.pos = self.source.len();
                false
            }
        }
    }

    fn next(&mut self) -> Option<Token> {
        while self.peek().is_some_and(char::is_whitespace) {
            self.pos += self.peek().map_or(0, char::len_utf8);
        }
        let start = self.pos;
        let c = self.peek()?;
        let rest = self.rest();
        let mut terminated = true;
        let kind = match c {
            '-' if rest.starts_with("--") => {
                self.line_comment();
                terminated = false;
                Tok::Comment
            }
            '#' if self.backslash_strings() => {
                self.line_comment();
                terminated = false;
                Tok::Comment
            }
            '/' if rest.starts_with("/*") => {
                terminated = self.block_comment();
                Tok::Comment
            }
            '\'' => {
                terminated = self.quoted('\'', self.backslash_strings());
                Tok::Str
            }
            '"' if self.mysql() => {
                terminated = self.quoted('"', true);
                Tok::Str
            }
            '"' => {
                terminated = self.quoted('"', false);
                Tok::Quoted
            }
            '`' => {
                terminated = self.quoted('`', false);
                Tok::Quoted
            }
            '$' if self.flavor == SqlFlavor::Postgres && self.dollar_quoted_ahead() => {
                terminated = self.dollar_quoted();
                Tok::Str
            }
            '.' if !rest[1..].starts_with(|c: char| c.is_ascii_digit()) => {
                self.pos += 1;
                Tok::Dot
            }
            ',' => {
                self.pos += 1;
                Tok::Comma
            }
            '(' => {
                self.pos += 1;
                Tok::Open
            }
            ')' => {
                self.pos += 1;
                Tok::Close
            }
            ';' => {
                self.pos += 1;
                Tok::Semi
            }
            c if c.is_ascii_digit() || c == '.' => {
                self.pos += rest
                    .find(|c: char| !(c.is_ascii_alphanumeric() || c == '.' || c == '_'))
                    .unwrap_or(rest.len());
                Tok::Num
            }
            c if is_word_start(c) => {
                self.pos += rest.find(|c: char| !is_word_char(c)).unwrap_or(rest.len());
                Tok::Word
            }
            c => {
                self.pos += c.len_utf8();
                Tok::Op
            }
        };
        Some(Token {
            kind,
            start,
            end: self.pos,
            terminated,
        })
    }

    fn dollar_quoted_ahead(&self) -> bool {
        let rest = self.rest();
        rest[1..].find('$').is_some_and(|close| {
            let tag = &rest[1..1 + close];
            !tag.starts_with(|c: char| c.is_ascii_digit()) && tag.chars().all(is_word_char)
        })
    }
}

fn scan(source: &str, range: Range<usize>, flavor: SqlFlavor) -> impl Iterator<Item = Token> + '_ {
    let mut scanner = Scanner {
        source: &source[..range.end],
        pos: range.start,
        flavor,
    };
    std::iter::from_fn(move || scanner.next())
}

/// Byte range of the statement containing `cursor` (delimited by `;` outside
/// strings and comments). Does not allocate.
pub(super) fn statement_bounds(source: &str, cursor: usize, flavor: SqlFlavor) -> Range<usize> {
    let mut start = 0;
    for token in scan(source, 0..source.len(), flavor) {
        if token.kind != Tok::Semi {
            continue;
        }
        if token.end <= cursor {
            start = token.end;
        } else {
            return start..token.start;
        }
    }
    start..source.len()
}

pub(super) fn lex(source: &str, range: Range<usize>, flavor: SqlFlavor) -> Vec<Token> {
    scan(source, range, flavor).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn kinds(sql: &str) -> Vec<Tok> {
        lex(sql, 0..sql.len(), SqlFlavor::Postgres)
            .iter()
            .map(|t| t.kind)
            .collect()
    }

    #[test]
    fn lexes_words_dots_and_quoted_identifiers() {
        assert_eq!(
            kinds(r#"select u."Name", 1.5 from t"#),
            [
                Tok::Word,
                Tok::Word,
                Tok::Dot,
                Tok::Quoted,
                Tok::Comma,
                Tok::Num,
                Tok::Word,
                Tok::Word
            ]
        );
    }

    #[test]
    fn unterminated_literals_are_tokens_not_errors() {
        let sql = "select 'abc";
        let tokens = lex(sql, 0..sql.len(), SqlFlavor::Postgres);
        assert_eq!(tokens[1].kind, Tok::Str);
        assert!(!tokens[1].terminated);
        assert!(tokens[1].contains_cursor(sql.len()));
    }

    #[test]
    fn statement_bounds_skip_semicolons_in_strings_and_comments() {
        let sql = "select ';' -- ;\n from a; select 2";
        let first = statement_bounds(sql, 8, SqlFlavor::Postgres);
        assert_eq!(&sql[first], "select ';' -- ;\n from a");
        let second = statement_bounds(sql, sql.len(), SqlFlavor::Postgres);
        assert_eq!(&sql[second], " select 2");
    }

    #[test]
    fn mysql_double_quotes_are_strings_and_hash_is_a_comment() {
        let sql = "select \"a\" # c";
        let tokens = lex(sql, 0..sql.len(), SqlFlavor::Mysql);
        assert_eq!(tokens[1].kind, Tok::Str);
        assert_eq!(tokens[2].kind, Tok::Comment);
    }

    #[test]
    fn postgres_dollar_quoting_hides_semicolons() {
        let sql = "do $$ a; b $$; select 1";
        assert_eq!(
            &sql[statement_bounds(sql, 3, SqlFlavor::Postgres)],
            "do $$ a; b $$"
        );
    }
}
