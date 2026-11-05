//! Cleaning of database-supplied text before it reaches the model.
//!
//! Names, types, defaults and comments come from the database, so they are
//! untrusted: a table called `"x\nTABLE admin\n  password text"` must not be
//! able to forge lines of the schema digest. Every such string goes through
//! [`clean`] (one line, no control or invisible characters, length-capped);
//! names that are not plain identifiers are additionally shown JSON-quoted
//! by [`name_form`], so no name can read as digest syntax.

use std::fmt::Write;

/// Longest identifier, type or default shown.
pub const MAX_NAME_CHARS: usize = 256;
/// Longest comment shown.
pub const MAX_COMMENT_CHARS: usize = 200;

/// Bidirectional-formatting and zero-width characters (text can be reordered
/// or hidden by them). This is the set [`clean`] has always dropped from text (cell values,
/// comments, types, error texts); names are not cleaned with it (see [`clean_name`]).
fn is_invisible(ch: char) -> bool {
    matches!(
        ch,
        '\u{00AD}'
            | '\u{061C}'
            | '\u{180E}'
            | '\u{200B}'..='\u{200F}'
            | '\u{202A}'..='\u{202E}'
            | '\u{2060}'..='\u{2064}'
            | '\u{2066}'..='\u{206F}'
            | '\u{FEFF}'
            | '\u{FFF9}'..='\u{FFFB}'
    )
}

/// Characters that have no shape of their own, or change how the text around them is laid out,
/// or draw as a blank: the set [`clean`] drops, the control characters, the Unicode line and
/// paragraph separators, the variation selectors, the tag block (which can spell text only a
/// machine reads), U+2065 and the blank glyphs that are valid inside identifiers (Hangul fillers,
/// the Braille blank, the combining grapheme joiner, Khmer inherent vowels, Mongolian selectors,
/// the object replacement character) and the format controls of the supplementary planes
/// (Egyptian hieroglyph, shorthand and musical-notation controls: zero width). A front end that shows a statement or a name to a person
/// spells these out instead of drawing them; one set for the model's view of names and the user's.
pub fn is_shapeless(ch: char) -> bool {
    is_invisible(ch)
        || ch.is_control()
        || matches!(
            ch,
            '\u{2028}'
                | '\u{2029}'
                | '\u{2065}'
                | '\u{034F}'
                | '\u{115F}'
                | '\u{1160}'
                | '\u{17B4}'
                | '\u{17B5}'
                | '\u{180B}'..='\u{180D}'
                | '\u{180F}'
                | '\u{2800}'
                | '\u{3164}'
                | '\u{FE00}'..='\u{FE0F}'
                | '\u{FFA0}'
                | '\u{FFFC}'
                | '\u{13430}'..='\u{1343F}'
                | '\u{1BCA0}'..='\u{1BCA3}'
                | '\u{1D173}'..='\u{1D17A}'
                | '\u{E0000}'..='\u{E007F}'
                | '\u{E0100}'..='\u{E01EF}'
        )
}

/// A character of a right-to-left script (Hebrew, Arabic and relatives). It needs no control
/// character to reorder the punctuation and quotes next to it.
pub fn is_right_to_left(ch: char) -> bool {
    matches!(
        ch,
        '\u{0590}'..='\u{08FF}'
            | '\u{FB1D}'..='\u{FDFF}'
            | '\u{FE70}'..='\u{FEFE}'
            | '\u{10800}'..='\u{10FFF}'
            | '\u{1E800}'..='\u{1EFFF}'
    )
}

/// `text` with every character without a shape of its own ([`is_shapeless`]) spelled out as
/// `<U+XXXX>`, and how many there were: what a front end draws when it shows a statement or a name
/// to a person who approves it (the app's card, the MCP server's question), so a reordering or
/// hidden character is a visible marker. For a statement (`multiline`) line breaks and tabs are
/// text, not hidden, and stay, and so does a `\r` that is the first half of `\r\n`; a bare `\r`
/// (it ends a line for some databases and not for an editor) is spelled. A name is one line: its
/// breaks are spelled too. The text that runs is never this one, always the original.
pub fn spell_out(text: &str, multiline: bool) -> (String, usize) {
    let mut spelled = 0;
    let mut out = String::with_capacity(text.len());
    let mut chars = text.chars().peekable();
    while let Some(c) = chars.next() {
        let plain = match c {
            '\n' | '\t' => multiline,
            '\r' => multiline && chars.peek() == Some(&'\n'),
            c => !is_shapeless(c),
        };
        if plain {
            out.push(c);
        } else {
            spelled += 1;
            let _ = write!(out, "<U+{:04X}>", u32::from(c));
        }
    }
    (out, spelled)
}

/// A name (a table, column, schema, database, connection) as the model reads it: like [`clean`],
/// but a name is never altered without saying so. A character without a shape of its own (a
/// zero-width non-joiner in a Persian name, a variation selector, a tag character) is kept as
/// `\u{XXXX}` instead of dropped, which also makes the name something other than a plain
/// identifier, so the digest shows it in its quoted form. The model sees what the object is
/// called; it cannot pass the escape back to select that object (said in the plan's known limits).
pub fn clean_name(text: &str, max_chars: usize) -> String {
    let mut out = String::with_capacity(text.len().min(max_chars * 2));
    for (count, ch) in text.chars().enumerate() {
        if count >= max_chars {
            out.push('…');
            break;
        }
        match ch {
            // The escapes below start with a backslash; a literal one is written twice, so a
            // table named `a\u{200C}b` and a table `a<ZWNJ>b` do not read the same.
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            ch if ch.is_control() => {
                let _ = write!(out, "\\x{:02X}", ch as u32);
            }
            ch if is_shapeless(ch) => {
                let _ = write!(out, "\\u{{{:04X}}}", ch as u32);
            }
            ch => out.push(ch),
        }
    }
    out
}

/// One line of printable text: invisible characters removed, `\n`, `\r`, `\t`
/// written as those two characters, other control characters as `\xNN`, cut
/// at `max_chars` (with a `…`).
pub fn clean(text: &str, max_chars: usize) -> String {
    let mut out = String::with_capacity(text.len().min(max_chars * 2));
    for (count, ch) in text.chars().filter(|ch| !is_invisible(*ch)).enumerate() {
        if count >= max_chars {
            out.push('…');
            break;
        }
        match ch {
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            '\u{2028}' | '\u{2029}' => out.push_str("\\n"),
            ch if ch.is_control() => {
                let _ = write!(out, "\\x{:02X}", ch as u32);
            }
            ch => out.push(ch),
        }
    }
    out
}

/// Multi-line text (DDL): invisible characters and control characters other
/// than newline and tab are dropped; capped like [`clean`].
pub fn clean_multiline(text: &str) -> String {
    text.chars()
        .filter(|ch| !is_invisible(*ch) && (!ch.is_control() || matches!(ch, '\n' | '\t')))
        .collect()
}

fn is_plain_identifier(text: &str) -> bool {
    let mut chars = text.chars();
    chars
        .next()
        .is_some_and(|first| first.is_ascii_alphabetic() || first == '_')
        && chars.all(|ch| ch.is_ascii_alphanumeric() || ch == '_' || ch == '$')
}

/// An already-[`clean`]ed name as it appears in the digest: as is when it is
/// a plain identifier, else a JSON string literal.
pub fn name_form(cleaned: &str) -> String {
    if is_plain_identifier(cleaned) {
        cleaned.to_owned()
    } else {
        serde_json::to_string(cleaned).unwrap_or_else(|_| "\"?\"".to_owned())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_shapeless_set_covers_controls_separators_tags_and_selectors() {
        for ch in [
            '\u{00AD}',
            '\u{180E}',
            '\u{200B}',
            '\u{202E}',
            '\u{2065}',
            '\u{2069}',
            '\u{206F}',
            '\u{2028}',
            '\u{2029}',
            '\u{FE0F}',
            '\u{FFF9}',
            '\u{E0041}',
            '\u{E0100}',
            '\u{7}',
            '\u{1B}',
            '\r',
        ] {
            assert!(is_shapeless(ch), "U+{:04X}", ch as u32);
        }
        for ch in ['a', ' ', '\'', '-', '\u{00E9}', '\u{4E2D}', '\u{1F600}'] {
            assert!(!is_shapeless(ch), "U+{:04X}", ch as u32);
        }
        assert!(is_right_to_left('\u{05D0}') && is_right_to_left('\u{0627}'));
        assert!(!is_right_to_left('a') && !is_right_to_left('\u{FEFF}'));
        for ch in [
            '\u{034F}', '\u{115F}', '\u{1160}', '\u{17B4}', '\u{17B5}', '\u{180B}', '\u{180D}',
            '\u{180F}', '\u{2800}', '\u{3164}', '\u{FFA0}', '\u{FFFC}',
        ] {
            assert!(is_shapeless(ch), "blank glyph U+{:04X}", ch as u32);
        }
    }

    #[test]
    fn invisible_characters_are_spelled_out_and_nothing_else_changes() {
        let (shown, spelled) = spell_out("SELECT 1;\n\t-- \u{202E}drop\u{200B}\r\n", true);
        assert_eq!(shown, "SELECT 1;\n\t-- <U+202E>drop<U+200B>\r\n");
        assert_eq!(spelled, 2);
        let (plain, none) = spell_out("UPDATE t SET a = '*bold* ![x](y) <b>' WHERE id = 1", true);
        assert_eq!(plain, "UPDATE t SET a = '*bold* ![x](y) <b>' WHERE id = 1");
        assert_eq!(none, 0);
        // A control character other than the line breaks is spelled too.
        assert_eq!(spell_out("a\u{1B}[2Jb", true).0, "a<U+001B>[2Jb");
        // The set is the engine's: soft hyphen, the separators, the tag block, selectors.
        let (all, count) = spell_out("a\u{AD}\u{2028}\u{E0041}\u{FE0F}\u{180E}\u{2065}b", true);
        assert_eq!(count, 6, "{all}");
        // A name is one line.
        assert_eq!(
            spell_out("a\nb\tc", false),
            ("a<U+000A>b<U+0009>c".to_owned(), 2)
        );
    }

    #[test]
    fn a_bare_carriage_return_is_spelled_and_a_crlf_is_not() {
        // The server ends a `--` comment at the `\r`; an editor would draw the rest as comment.
        let (shown, spelled) = spell_out("DELETE FROM users WHERE id = 1 -- note\r OR true", true);
        assert_eq!(
            shown,
            "DELETE FROM users WHERE id = 1 -- note<U+000D> OR true"
        );
        assert_eq!(spelled, 1);
        let (crlf, none) = spell_out("SELECT 1\r\nFROM t\r\n", true);
        assert_eq!(crlf, "SELECT 1\r\nFROM t\r\n");
        assert_eq!(none, 0);
        let (end, one) = spell_out("SELECT 1\r", true);
        assert_eq!((end.as_str(), one), ("SELECT 1<U+000D>", 1));
    }

    #[test]
    fn text_is_cleaned_as_it_always_was_and_a_name_is_never_altered_without_saying_so() {
        // Cell values, comments, types: the old set is dropped, nothing else is touched.
        assert_eq!(
            clean("a\u{200C}b\u{FE0F}c\u{E0041}", 20),
            "ab\u{FE0F}c\u{E0041}"
        );
        // A Persian name with a zero-width non-joiner keeps it, spelled.
        let persian = "\u{645}\u{6CC}\u{200C}\u{62E}\u{648}\u{627}\u{647}\u{645}";
        assert_eq!(
            clean_name(persian, 50),
            "\u{645}\u{6CC}\\u{200C}\u{62E}\u{648}\u{627}\u{647}\u{645}"
        );
        // An emoji with its variation selector, a tag character, a control character.
        assert_eq!(
            clean_name("heart\u{2764}\u{FE0F}", 50),
            "heart\u{2764}\\u{FE0F}"
        );
        assert_eq!(clean_name("t\u{E0041}", 50), "t\\u{E0041}");
        assert_eq!(clean_name("a\nb\u{7}", 50), "a\\nb\\x07");
        // A plain name is untouched, and the digest quotes the escaped one.
        assert_eq!(clean_name("orders_2024", 50), "orders_2024");
        assert_eq!(name_form(&clean_name("orders_2024", 50)), "orders_2024");
        assert_eq!(
            name_form(&clean_name(persian, 50)),
            "\"\u{645}\u{6CC}\\\\u{200C}\u{62E}\u{648}\u{627}\u{647}\u{645}\""
        );
        // A literal backslash is written twice: a table named `a\u{200C}b` (backslash, u, ...)
        // and a table `a<ZWNJ>b` do not read the same.
        assert_eq!(clean_name("a\\u{200C}b", 50), "a\\\\u{200C}b");
        assert_eq!(clean_name("a\u{200C}b", 50), "a\\u{200C}b");
        assert_ne!(clean_name("a\\u{200C}b", 50), clean_name("a\u{200C}b", 50));
        assert_eq!(clean_name("C:\\data", 50), "C:\\\\data");
        // The cap still applies.
        assert_eq!(clean_name("abcdef", 3), "abc\u{2026}");
    }

    #[test]
    fn newlines_and_controls_are_escaped_not_emitted() {
        assert_eq!(clean("a\nTABLE x", 100), "a\\nTABLE x");
        assert_eq!(clean("a\r\n\tb", 100), "a\\r\\n\\tb");
        assert_eq!(clean("a\u{0007}b\u{0085}c", 100), "a\\x07b\\x85c");
        assert_eq!(clean("a\u{2028}b", 100), "a\\nb");
    }

    #[test]
    fn bidi_and_zero_width_characters_are_stripped() {
        assert_eq!(clean("ad\u{202E}min\u{200B}\u{FEFF}", 100), "admin");
        assert_eq!(clean("x\u{2066}y\u{2069}", 100), "xy");
    }

    #[test]
    fn long_text_is_capped() {
        let cleaned = clean(&"é".repeat(300), MAX_NAME_CHARS);
        assert_eq!(cleaned.chars().count(), MAX_NAME_CHARS + 1);
        assert!(cleaned.ends_with('…'));
    }

    #[test]
    fn names_that_are_not_plain_identifiers_are_json_quoted() {
        assert_eq!(name_form("orders"), "orders");
        assert_eq!(name_form("_a$1"), "_a$1");
        assert_eq!(name_form("two words"), "\"two words\"");
        assert_eq!(name_form("a\\nTABLE x"), "\"a\\\\nTABLE x\"");
        assert_eq!(name_form("1abc"), "\"1abc\"");
        assert_eq!(name_form(""), "\"\"");
        assert_eq!(name_form("quo\"te"), "\"quo\\\"te\"");
    }

    #[test]
    fn multiline_text_keeps_its_lines() {
        assert_eq!(
            clean_multiline("CREATE TABLE t (\r\n  a int\u{200B}\n)\u{0000}"),
            "CREATE TABLE t (\n  a int\n)"
        );
    }
}
