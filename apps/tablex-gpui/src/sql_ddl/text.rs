//! The free-text fields of a column, a type and a default expression, are
//! spliced into a statement as typed, so they are lexed first: one typed field
//! must stay one expression, whatever the server would make of it.
//!
//! The lexer follows the server's own quoting rules: a backslash escapes the
//! next character inside a string where the dialect says so (MySQL and
//! MariaDB unless `NO_BACKSLASH_ESCAPES`, ClickHouse), and not elsewhere, so
//! `'\'; DROP TABLE t; -- '` is one literal under one `sql_mode` and a literal,
//! a `;` and a comment under the other. Where `"…"` is a name (`ANSI_QUOTES`) a
//! backslash inside a quoted name is refused, as ClickHouse would read it as an escape.

use crate::sql_table::SqlDialect;

/// Words that start another action of the same `ALTER TABLE` (or add an
/// attribute the editor sets elsewhere): never part of a type.
const TYPE_KEYWORDS: &[&str] = &[
    "ADD", "DROP", "ALTER", "MODIFY", "CHANGE", "RENAME", "REMOVE", "COMMENT", "DEFAULT", "NOT", "NULL", "USING",
    "REFERENCES", "PRIMARY", "UNIQUE", "CHECK", "CONSTRAINT", "GENERATED", "AFTER", "FIRST",
];

/// The same for a default expression (which may legitimately say `NOT` and `NULL`).
const DEFAULT_KEYWORDS: &[&str] =
    &["ADD", "DROP", "ALTER", "MODIFY", "CHANGE", "RENAME", "REMOVE", "COMMENT", "REFERENCES", "CONSTRAINT", "PRIMARY", "UNIQUE", "CHECK"];

/// What the lexer found at the top level (outside quotes and parentheses).
struct Top {
    words: Vec<String>,
}

fn scan(quoting: SqlDialect, label: &str, text: &str) -> Result<Top, String> {
    let mut chars = text.chars().peekable();
    let mut quote: Option<char> = None;
    let mut depth: i32 = 0;
    let mut words = Vec::new();
    let mut word = String::new();
    let flush = |word: &mut String, depth: i32, words: &mut Vec<String>| {
        if depth == 0 && !word.is_empty() {
            words.push(word.to_ascii_uppercase());
        }
        word.clear();
    };
    while let Some(ch) = chars.next() {
        if let Some(open) = quote {
            if ch == '\\' && matches!(open, '"' | '`') && quoting.backslash_in_name_is_ambiguous() {
                return Err(format!("{label}: a backslash inside {open}…{open} is not allowed (servers read it differently)"));
            }
            if ch == '\\' && quoting.backslash_escapes && open != '`' && open != ']' {
                chars.next();
            } else if ch == open {
                if chars.peek() == Some(&open) {
                    chars.next();
                } else {
                    quote = None;
                }
            }
            continue;
        }
        if ch.is_alphanumeric() || ch == '_' {
            word.push(ch);
            continue;
        }
        flush(&mut word, depth, &mut words);
        match ch {
            '\'' | '"' | '`' => quote = Some(ch),
            // `[name]` (MariaDB `MSSQL`): `]]` escapes `]`, a backslash is plain.
            '[' if quoting.bracket_identifiers => quote = Some(']'),
            ';' => return Err(format!("{label}: ';' is not allowed (a single statement is run)")),
            '#' if quoting.backslash_escapes => return Err(format!("{label}: comments are not allowed")),
            '-' if chars.peek() == Some(&'-') => return Err(format!("{label}: comments are not allowed")),
            '/' if chars.peek() == Some(&'*') => return Err(format!("{label}: comments are not allowed")),
            '(' => depth += 1,
            ')' => {
                depth -= 1;
                if depth < 0 {
                    return Err(format!("{label}: unbalanced ')'"));
                }
            }
            ',' if depth == 0 => return Err(format!("{label}: a ',' outside parentheses would end the column definition")),
            _ => {}
        }
    }
    flush(&mut word, depth, &mut words);
    if quote.is_some() {
        return Err(format!("{label}: unterminated quoted text"));
    }
    if depth != 0 {
        return Err(format!("{label}: unbalanced '('"));
    }
    Ok(Top { words })
}

/// Checks a typed column type.
pub(super) fn check_type(quoting: SqlDialect, text: &str) -> Result<(), String> {
    let top = scan(quoting, "Type", text)?;
    match top.words.iter().find(|word| TYPE_KEYWORDS.contains(&word.as_str())) {
        Some(word) => Err(format!("Type: {word} cannot be part of a type")),
        None => Ok(()),
    }
}

/// Checks a typed default expression.
pub(super) fn check_default(quoting: SqlDialect, text: &str) -> Result<(), String> {
    let top = scan(quoting, "Default", text)?;
    match top.words.iter().find(|word| DEFAULT_KEYWORDS.contains(&word.as_str())) {
        Some(word) => Err(format!("Default: {word} cannot be part of a default expression")),
        None => Ok(()),
    }
}
