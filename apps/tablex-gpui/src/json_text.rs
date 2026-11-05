//! Token-level JSON text helpers. `serde_json::Value` cannot hold a number it
//! would print differently (`1.10`, `12345678901234567890.5`, `1E2`), so every
//! path that shows or stores a stored JSON text works on the text itself: the
//! tokens (strings and numbers) are copied byte for byte, only the whitespace
//! between them changes. `serde_json` validates first (it also bounds the
//! nesting depth), so the scanners below only ever see well-formed text.

use serde_json::Value;

/// Strict JSON number grammar (`-?(0|[1-9]\d*)(\.\d+)?([eE][+-]?\d+)?`).
pub fn is_json_number(text: &str) -> bool {
    let bytes = text.as_bytes();
    let mut index = usize::from(bytes.first() == Some(&b'-'));
    let digits = |from: usize| {
        bytes[from..]
            .iter()
            .take_while(|byte| byte.is_ascii_digit())
            .count()
    };
    match bytes.get(index) {
        Some(b'0') => index += 1,
        Some(b'1'..=b'9') => index += digits(index),
        _ => return false,
    }
    if bytes.get(index) == Some(&b'.') {
        let fraction = digits(index + 1);
        if fraction == 0 {
            return false;
        }
        index += 1 + fraction;
    }
    if matches!(bytes.get(index), Some(b'e' | b'E')) {
        index += 1;
        if matches!(bytes.get(index), Some(b'+' | b'-')) {
            index += 1;
        }
        let exponent = digits(index);
        if exponent == 0 {
            return false;
        }
        index += exponent;
    }
    index == bytes.len()
}

fn is_valid(text: &str) -> bool {
    serde_json::from_str::<serde::de::IgnoredAny>(text).is_ok()
}

/// `text` re-indented with two spaces, every token unchanged; `None` when it is
/// not JSON.
pub fn pretty(text: &str) -> Option<String> {
    if !is_valid(text) {
        return None;
    }
    let bytes = text.as_bytes();
    let mut out = String::with_capacity(text.len() + text.len() / 4);
    let mut depth = 0usize;
    let mut index = 0;
    let newline = |out: &mut String, depth: usize| {
        out.push('\n');
        out.extend(std::iter::repeat_n("  ", depth));
    };
    while index < bytes.len() {
        match bytes[index] {
            b' ' | b'\t' | b'\n' | b'\r' => index += 1,
            b'"' => {
                let end = string_end(bytes, index);
                out.push_str(&text[index..end]);
                index = end;
            }
            open @ (b'{' | b'[') => {
                out.push(open as char);
                index += 1;
                let next = skip_whitespace(bytes, index);
                if matches!(bytes.get(next), Some(b'}' | b']')) {
                    out.push(bytes[next] as char);
                    index = next + 1;
                } else {
                    depth += 1;
                    newline(&mut out, depth);
                }
            }
            close @ (b'}' | b']') => {
                depth = depth.saturating_sub(1);
                newline(&mut out, depth);
                out.push(close as char);
                index += 1;
            }
            b',' => {
                out.push(',');
                newline(&mut out, depth);
                index += 1;
            }
            b':' => {
                out.push_str(": ");
                index += 1;
            }
            _ => {
                let end = literal_end(bytes, index);
                out.push_str(&text[index..end]);
                index = end;
            }
        }
    }
    Some(out)
}

fn skip_whitespace(bytes: &[u8], mut index: usize) -> usize {
    while matches!(bytes.get(index), Some(b' ' | b'\t' | b'\n' | b'\r')) {
        index += 1;
    }
    index
}

/// End of the string token that starts (with its quote) at `start`.
fn string_end(bytes: &[u8], start: usize) -> usize {
    let mut index = start + 1;
    while index < bytes.len() {
        match bytes[index] {
            b'\\' => index += 2,
            b'"' => return index + 1,
            _ => index += 1,
        }
    }
    bytes.len()
}

/// End of the number / `true` / `false` / `null` token at `start`.
fn literal_end(bytes: &[u8], start: usize) -> usize {
    let length = bytes[start..]
        .iter()
        .take_while(|byte| {
            !matches!(
                byte,
                b' ' | b'\t' | b'\n' | b'\r' | b',' | b']' | b'}' | b':'
            )
        })
        .count();
    start + length.max(1)
}

/// A number token as a value: a JSON number when `serde_json` prints it back
/// unchanged, otherwise the literal as a string (its digits are never rounded).
fn number_value(literal: &str) -> Value {
    literal
        .parse::<serde_json::Number>()
        .ok()
        .filter(|number| number.to_string() == literal)
        .map_or_else(|| Value::String(literal.to_string()), Value::Number)
}

/// `text` as a value whose numbers are exact: one that `serde_json` would print
/// differently stays a string of its literal. `None` when it is not JSON.
pub fn exact_value(text: &str) -> Option<Value> {
    if !is_valid(text) {
        return None;
    }
    let mut position = skip_whitespace(text.as_bytes(), 0);
    exact_value_at(text, &mut position)
}

fn exact_value_at(text: &str, position: &mut usize) -> Option<Value> {
    let bytes = text.as_bytes();
    *position = skip_whitespace(bytes, *position);
    match *bytes.get(*position)? {
        b'{' => {
            *position += 1;
            let mut map = serde_json::Map::new();
            loop {
                *position = skip_whitespace(bytes, *position);
                match bytes.get(*position)? {
                    b'}' => break,
                    b',' => *position += 1,
                    _ => {
                        let key_end = string_end(bytes, *position);
                        let key = serde_json::from_str::<String>(&text[*position..key_end]).ok()?;
                        *position = skip_whitespace(bytes, key_end);
                        *position += 1; // ':'
                        map.insert(key, exact_value_at(text, position)?);
                    }
                }
            }
            *position += 1;
            Some(Value::Object(map))
        }
        b'[' => {
            *position += 1;
            let mut items = Vec::new();
            loop {
                *position = skip_whitespace(bytes, *position);
                match bytes.get(*position)? {
                    b']' => break,
                    b',' => *position += 1,
                    _ => items.push(exact_value_at(text, position)?),
                }
            }
            *position += 1;
            Some(Value::Array(items))
        }
        b'"' => {
            let end = string_end(bytes, *position);
            let value = serde_json::from_str::<String>(&text[*position..end]).ok()?;
            *position = end;
            Some(Value::String(value))
        }
        _ => {
            let end = literal_end(bytes, *position);
            let literal = &text[*position..end];
            *position = end;
            Some(match literal {
                "null" => Value::Null,
                "true" => Value::Bool(true),
                "false" => Value::Bool(false),
                number => number_value(number),
            })
        }
    }
}

/// One member value of a top-level object, kept as written.
#[derive(Debug, Clone, PartialEq)]
pub enum Member<'a> {
    Null,
    Bool(bool),
    /// The number token, byte for byte.
    Number(&'a str),
    String(String),
    /// An object or array, byte for byte.
    Json(&'a str),
}

/// Why a text is not a JSON object.
#[derive(Debug, Clone, PartialEq)]
pub enum ObjectError {
    Invalid(String),
    NotAnObject,
}

impl std::fmt::Display for ObjectError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Invalid(reason) => f.write_str(reason),
            Self::NotAnObject => f.write_str("expected an object"),
        }
    }
}

/// The members of the top-level object `text`, in order (a repeated name keeps
/// its last value), values as written.
pub fn object_members(text: &str) -> Result<Vec<(String, Member<'_>)>, ObjectError> {
    serde_json::from_str::<serde::de::IgnoredAny>(text)
        .map_err(|error| ObjectError::Invalid(error.to_string()))?;
    let bytes = text.as_bytes();
    let mut position = skip_whitespace(bytes, 0);
    if bytes.get(position) != Some(&b'{') {
        return Err(ObjectError::NotAnObject);
    }
    position += 1;
    let mut members: Vec<(String, Member<'_>)> = Vec::new();
    loop {
        position = skip_whitespace(bytes, position);
        match bytes.get(position) {
            Some(b'}') | None => break,
            Some(b',') => position += 1,
            Some(_) => {
                let key_end = string_end(bytes, position);
                let key = serde_json::from_str::<String>(&text[position..key_end])
                    .map_err(|error| ObjectError::Invalid(error.to_string()))?;
                position = skip_whitespace(bytes, key_end) + 1; // ':'
                position = skip_whitespace(bytes, position);
                let start = position;
                let member = match bytes.get(position) {
                    Some(b'{' | b'[') => {
                        position = container_end(bytes, position);
                        Member::Json(&text[start..position])
                    }
                    Some(b'"') => {
                        position = string_end(bytes, position);
                        Member::String(
                            serde_json::from_str(&text[start..position])
                                .map_err(|error| ObjectError::Invalid(error.to_string()))?,
                        )
                    }
                    _ => {
                        position = literal_end(bytes, position);
                        match &text[start..position] {
                            "null" => Member::Null,
                            "true" => Member::Bool(true),
                            "false" => Member::Bool(false),
                            number => Member::Number(number),
                        }
                    }
                };
                members.retain(|(name, _)| *name != key);
                members.push((key, member));
            }
        }
    }
    Ok(members)
}

/// End of the object or array that starts at `start`.
fn container_end(bytes: &[u8], start: usize) -> usize {
    let mut depth = 0usize;
    let mut index = start;
    while index < bytes.len() {
        match bytes[index] {
            b'"' => {
                index = string_end(bytes, index);
                continue;
            }
            b'{' | b'[' => depth += 1,
            b'}' | b']' => {
                depth -= 1;
                if depth == 0 {
                    return index + 1;
                }
            }
            _ => {}
        }
        index += 1;
    }
    bytes.len()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pretty_changes_whitespace_only_and_keeps_every_token() {
        let stored = r#"{"a":1.10,"big":12345678901234567890.123456789,"e":1E2,"s":"x\u00e9\"y","d":1,"d":2,"o":{},"l":[],"n":[1,[2,{"k":null}]]}"#;
        let formatted = pretty(stored).expect("valid JSON");
        assert_eq!(
            formatted,
            "{\n  \"a\": 1.10,\n  \"big\": 12345678901234567890.123456789,\n  \"e\": 1E2,\n  \"s\": \"x\\u00e9\\\"y\",\n  \"d\": 1,\n  \"d\": 2,\n  \"o\": {},\n  \"l\": [],\n  \"n\": [\n    1,\n    [\n      2,\n      {\n        \"k\": null\n      }\n    ]\n  ]\n}"
        );
        // Re-indenting the formatted text is a fixed point, and removing the
        // added whitespace gives the stored text back.
        assert_eq!(pretty(&formatted).as_deref(), Some(formatted.as_str()));
        assert!(pretty("PAY").is_none());
        assert!(pretty("{\"a\":1,}").is_none());
        assert_eq!(pretty("  7.0 ").as_deref(), Some("7.0"));
    }

    #[test]
    fn exact_values_keep_the_digits_serde_would_change() {
        let value =
            exact_value(r#"{"a":1.10,"b":12345678901234567890,"c":2.5,"d":[1,1E2]}"#).unwrap();
        assert_eq!(value["a"], Value::String("1.10".into()));
        assert_eq!(value["b"], serde_json::json!(12345678901234567890u64));
        assert_eq!(value["c"], serde_json::json!(2.5));
        assert_eq!(value["d"][0], serde_json::json!(1));
        assert_eq!(value["d"][1], Value::String("1E2".into()));
        assert!(exact_value("nope").is_none());
    }

    #[test]
    fn object_members_keep_number_text_and_raw_containers() {
        let members = object_members(
            r#" {"amount": 12345678901234567890.123456789, "n": null, "ok": true, "s": "a\"b", "o": {"x": [1, 2]}, "amount": 1.50} "#,
        )
        .unwrap();
        assert_eq!(
            members,
            vec![
                ("n".to_string(), Member::Null),
                ("ok".to_string(), Member::Bool(true)),
                ("s".to_string(), Member::String("a\"b".into())),
                ("o".to_string(), Member::Json(r#"{"x": [1, 2]}"#)),
                ("amount".to_string(), Member::Number("1.50")),
            ]
        );
        let single = object_members(r#"{"amount": 12345678901234567890.123456789}"#).unwrap();
        assert_eq!(
            single[0].1,
            Member::Number("12345678901234567890.123456789")
        );
        assert_eq!(object_members("[1]"), Err(ObjectError::NotAnObject));
        assert!(matches!(object_members("{"), Err(ObjectError::Invalid(_))));
    }

    #[test]
    fn json_number_grammar_is_strict() {
        for number in [
            "0",
            "-0",
            "12",
            "-1.5",
            "1e5",
            "1.5E-3",
            "12345678901234567890.123456789",
        ] {
            assert!(is_json_number(number), "{number}");
        }
        for text in [
            "", "-", "01", "+1", ".5", "1.", "1e", "NaN", "Infinity", "0x1", "1 ",
        ] {
            assert!(!is_json_number(text), "{text}");
        }
    }
}
