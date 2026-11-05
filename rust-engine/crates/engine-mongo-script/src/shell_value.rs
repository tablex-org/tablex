//! MongoDB shell value syntax: the one text form for typed BSON values.
//!
//! The parser reads what a user types in a filter, a document editor or a cell
//! editor: JSON5-ish text (single or double quoted strings, bare keys, trailing
//! commas, comments), the shell constructors (`ObjectId("…")`, `ISODate("…")`,
//! `NumberLong(5)`, `Decimal128("…")`, `UUID("…")`, `BinData(0, "…")`,
//! `Timestamp({ t: 1, i: 2 })`, `MinKey()`, `Code("…")`, `/regex/flags`,
//! `undefined`, `NaN`, `Infinity`) and MongoDB Extended JSON v2, canonical and
//! relaxed (`{"$oid": …}`, `{"$numberLong": "5"}`, `{"$date": …}` …), in any mix.
//! The printer writes the same syntax, so a value printed here parses back to
//! the identical BSON type and value.
//!
//! Plain numbers are typed like mongosh types them: an integer inside the Int32
//! range is an Int32, any other number is a Double ([`NumberTyping::Mongosh`]).
//! An integer too large for a Double to hold exactly is refused rather than
//! rounded. Filters use [`NumberTyping::Filter`]: an integer beyond Int32 is an
//! Int64, which the server compares numerically with every other number type.
//! A Double is always written with a decimal point or an exponent, so it cannot
//! be taken for an integer.

use bson::{Binary, Bson, DateTime, Decimal128, Document};
use serde_json::{Map, Number, Value};
use std::fmt::{self, Write};
use std::sync::LazyLock;

/// How a plain (unwrapped) number literal is typed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NumberTyping {
    /// mongosh: Int32 when it is an integer in range, otherwise Double.
    Mongosh,
    /// Query filters: Int32 when in range, otherwise Int64 (exact, never rounded).
    Filter,
}

/// A syntax or value error with the line and column it was found at.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ShellError {
    message: String,
    line: usize,
    column: usize,
}

impl ShellError {
    fn at(source: &str, offset: usize, message: impl Into<String>) -> Self {
        let before = &source[..offset.min(source.len())];
        let line = before.matches('\n').count() + 1;
        let column = before
            .rsplit('\n')
            .next()
            .map_or(0, |tail| tail.chars().count())
            + 1;
        Self {
            message: message.into(),
            line,
            column,
        }
    }

    fn plain(message: impl Into<String>) -> Self {
        Self {
            message: message.into(),
            line: 0,
            column: 0,
        }
    }

    pub fn message(&self) -> &str {
        &self.message
    }
}

impl fmt::Display for ShellError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        if self.line == 0 {
            formatter.write_str(&self.message)
        } else {
            write!(
                formatter,
                "{} (line {}, column {})",
                self.message, self.line, self.column
            )
        }
    }
}

impl std::error::Error for ShellError {}

const MAX_DEPTH: usize = 128;
/// Largest integer a Double holds exactly.
const MAX_EXACT_DOUBLE_INTEGER: i64 = 1 << 53;

/// Keys that make a JSON object an Extended JSON v2 wrapper (the set `bson`'s
/// own Extended JSON reader recognises).
const WRAPPER_KEYS: [&str; 16] = [
    "$oid",
    "$symbol",
    "$regularExpression",
    "$numberInt",
    "$numberLong",
    "$numberDouble",
    "$numberDecimal",
    "$binary",
    "$uuid",
    "$code",
    "$timestamp",
    "$date",
    "$minKey",
    "$maxKey",
    "$dbPointer",
    "$undefined",
];

/// Parses one value.
pub fn parse_value(text: &str, typing: NumberTyping) -> Result<Bson, ShellError> {
    let mut parser = Parser {
        source: text,
        pos: 0,
        depth: 0,
    };
    let value = parser.value()?;
    parser.skip_trivia()?;
    if parser.pos < text.len() {
        return Err(parser.error("unexpected text after the value"));
    }
    to_bson(value, typing)
}

/// Parses one document (`{ … }`).
pub fn parse_document(text: &str, typing: NumberTyping) -> Result<Document, ShellError> {
    match parse_value(text, typing)? {
        Bson::Document(document) => Ok(document),
        _ => Err(ShellError::plain("expected a document { … }")),
    }
}

/// An ISO 8601 / RFC 3339 instant; a missing time is midnight and a missing
/// zone is UTC (`2024-01-02`, `2024-01-02T03:04:05`, `2024-01-02 03:04:05.5+01:00`).
pub fn parse_iso_date(text: &str) -> Option<DateTime> {
    static SHAPE: LazyLock<regex::Regex> = LazyLock::new(|| {
        regex::Regex::new(
            r"^(\d{4}-\d{2}-\d{2})(?:[T ](\d{2}:\d{2})(:\d{2}(?:\.\d+)?)?)?(Z|[+-]\d{2}:?\d{2})?$",
        )
        .expect("static date pattern")
    });
    let text = text.trim();
    if let Ok(date) = DateTime::parse_rfc3339_str(text) {
        return Some(date);
    }
    let captures = SHAPE.captures(text)?;
    let zone = match captures.get(4).map(|zone| zone.as_str()) {
        None | Some("Z") => "Z".to_string(),
        Some(offset) if offset.contains(':') => offset.to_string(),
        Some(offset) => format!("{}:{}", &offset[..3], &offset[3..]),
    };
    let normalized = format!(
        "{}T{}{}{}",
        &captures[1],
        captures.get(2).map_or("00:00", |time| time.as_str()),
        captures.get(3).map_or(":00", |seconds| seconds.as_str()),
        zone
    );
    DateTime::parse_rfc3339_str(normalized).ok()
}

struct Parser<'a> {
    source: &'a str,
    pos: usize,
    depth: usize,
}

fn is_identifier_byte(byte: u8) -> bool {
    byte.is_ascii_alphanumeric() || byte == b'_' || byte == b'$' || byte >= 0x80
}

impl<'a> Parser<'a> {
    fn error(&self, message: impl Into<String>) -> ShellError {
        ShellError::at(self.source, self.pos, message)
    }

    fn peek(&self) -> Option<u8> {
        self.source.as_bytes().get(self.pos).copied()
    }

    fn rest(&self) -> &'a str {
        &self.source[self.pos..]
    }

    fn skip_trivia(&mut self) -> Result<(), ShellError> {
        loop {
            let rest = self.rest();
            let trimmed = rest.trim_start();
            self.pos += rest.len() - trimmed.len();
            if trimmed.starts_with("//") {
                self.pos += trimmed.find('\n').unwrap_or(trimmed.len());
            } else if let Some(comment) = trimmed.strip_prefix("/*") {
                match comment.find("*/") {
                    Some(end) => self.pos += end + 4,
                    None => return Err(self.error("unterminated comment")),
                }
            } else {
                return Ok(());
            }
        }
    }

    fn expect(&mut self, byte: u8, what: &str) -> Result<(), ShellError> {
        self.skip_trivia()?;
        if self.peek() == Some(byte) {
            self.pos += 1;
            Ok(())
        } else {
            Err(self.error(format!("expected {what}")))
        }
    }

    fn value(&mut self) -> Result<Value, ShellError> {
        self.skip_trivia()?;
        self.depth += 1;
        if self.depth > MAX_DEPTH {
            return Err(self.error("value is nested too deeply"));
        }
        let value = match self.peek() {
            None => return Err(self.error("a value is expected")),
            Some(b'{') => self.object()?,
            Some(b'[') => self.array()?,
            Some(b'"' | b'\'') => Value::String(self.string()?),
            Some(b'/') => self.regex()?,
            Some(b'-' | b'+' | b'.' | b'0'..=b'9') => self.number()?,
            Some(byte) if is_identifier_byte(byte) => self.identifier()?,
            Some(_) => return Err(self.error("a value is expected")),
        };
        self.depth -= 1;
        Ok(value)
    }

    fn object(&mut self) -> Result<Value, ShellError> {
        self.pos += 1;
        let mut map = Map::new();
        loop {
            self.skip_trivia()?;
            match self.peek() {
                Some(b'}') => {
                    self.pos += 1;
                    return Ok(Value::Object(map));
                }
                None => return Err(self.error("unterminated object")),
                _ => {}
            }
            let key_at = self.pos;
            let key = self.key()?;
            self.expect(b':', "':' after the field name")?;
            let value = self.value()?;
            if map.insert(key.clone(), value).is_some() {
                self.pos = key_at;
                return Err(self.error(format!("duplicate field \"{key}\"")));
            }
            self.skip_trivia()?;
            match self.peek() {
                Some(b',') => self.pos += 1,
                Some(b'}') => {}
                _ => return Err(self.error("expected ',' or '}'")),
            }
        }
    }

    fn key(&mut self) -> Result<String, ShellError> {
        match self.peek() {
            Some(b'"' | b'\'') => self.string(),
            Some(byte) if is_identifier_byte(byte) => {
                let start = self.pos;
                while self.peek().is_some_and(is_identifier_byte) {
                    self.pos += 1;
                }
                Ok(self.source[start..self.pos].to_string())
            }
            _ => Err(self.error("a field name is expected")),
        }
    }

    fn array(&mut self) -> Result<Value, ShellError> {
        self.pos += 1;
        let mut items = Vec::new();
        loop {
            self.skip_trivia()?;
            match self.peek() {
                Some(b']') => {
                    self.pos += 1;
                    return Ok(Value::Array(items));
                }
                None => return Err(self.error("unterminated array")),
                _ => {}
            }
            items.push(self.value()?);
            self.skip_trivia()?;
            match self.peek() {
                Some(b',') => self.pos += 1,
                Some(b']') => {}
                _ => return Err(self.error("expected ',' or ']'")),
            }
        }
    }

    fn string(&mut self) -> Result<String, ShellError> {
        let quote = self.source.as_bytes()[self.pos];
        self.pos += 1;
        let mut out = String::new();
        loop {
            let Some(character) = self.rest().chars().next() else {
                return Err(self.error("unterminated string"));
            };
            self.pos += character.len_utf8();
            match character {
                c if c as u32 == u32::from(quote) => return Ok(out),
                '\\' => self.escape(&mut out)?,
                c => out.push(c),
            }
        }
    }

    fn escape(&mut self, out: &mut String) -> Result<(), ShellError> {
        let Some(character) = self.rest().chars().next() else {
            return Err(self.error("unterminated string"));
        };
        self.pos += character.len_utf8();
        match character {
            'n' => out.push('\n'),
            't' => out.push('\t'),
            'r' => out.push('\r'),
            'b' => out.push('\u{8}'),
            'f' => out.push('\u{c}'),
            'v' => out.push('\u{b}'),
            '0' => out.push('\0'),
            '\n' => {}
            'x' => out.push(self.hex_char(2)?),
            'u' => {
                let high = self.hex_code(4)?;
                if (0xD800..0xDC00).contains(&high) && self.rest().starts_with("\\u") {
                    self.pos += 2;
                    let low = self.hex_code(4)?;
                    if !(0xDC00..0xE000).contains(&low) {
                        return Err(self.error("invalid surrogate pair"));
                    }
                    let code = 0x10000 + ((high - 0xD800) << 10) + (low - 0xDC00);
                    out.push(char::from_u32(code).ok_or_else(|| self.error("invalid escape"))?);
                } else {
                    out.push(
                        char::from_u32(high).ok_or_else(|| self.error("invalid unicode escape"))?,
                    );
                }
            }
            other => out.push(other),
        }
        Ok(())
    }

    fn hex_code(&mut self, digits: usize) -> Result<u32, ShellError> {
        let text = self
            .rest()
            .get(..digits)
            .ok_or_else(|| self.error("invalid escape"))?;
        let code = u32::from_str_radix(text, 16).map_err(|_| self.error("invalid escape"))?;
        self.pos += digits;
        Ok(code)
    }

    fn hex_char(&mut self, digits: usize) -> Result<char, ShellError> {
        let code = self.hex_code(digits)?;
        char::from_u32(code).ok_or_else(|| self.error("invalid escape"))
    }

    fn regex(&mut self) -> Result<Value, ShellError> {
        self.pos += 1;
        let start = self.pos;
        let mut in_class = false;
        loop {
            let Some(character) = self.rest().chars().next() else {
                return Err(self.error("unterminated regular expression"));
            };
            match character {
                '\n' | '\r' => return Err(self.error("unterminated regular expression")),
                '\\' => {
                    self.pos += 1;
                    if let Some(escaped) = self.rest().chars().next() {
                        self.pos += escaped.len_utf8();
                    }
                    continue;
                }
                '[' => in_class = true,
                ']' => in_class = false,
                '/' if !in_class => break,
                _ => {}
            }
            self.pos += character.len_utf8();
        }
        let pattern = &self.source[start..self.pos];
        self.pos += 1;
        let flags_start = self.pos;
        while self.peek().is_some_and(|byte| byte.is_ascii_alphabetic()) {
            self.pos += 1;
        }
        Ok(wrapper(
            "$regularExpression",
            serde_json::json!({ "pattern": pattern, "options": &self.source[flags_start..self.pos] }),
        ))
    }

    fn number(&mut self) -> Result<Value, ShellError> {
        let start = self.pos;
        let negative = self.peek() == Some(b'-');
        if matches!(self.peek(), Some(b'-' | b'+')) {
            self.pos += 1;
        }
        let rest = self.rest();
        if rest.starts_with("Infinity") {
            self.pos += "Infinity".len();
            return Ok(double_wrapper(if negative {
                "-Infinity"
            } else {
                "Infinity"
            }));
        }
        if rest.starts_with("NaN") {
            self.pos += "NaN".len();
            return Ok(double_wrapper("NaN"));
        }
        if rest.starts_with("0x") || rest.starts_with("0X") {
            self.pos += 2;
            let digits_start = self.pos;
            while self.peek().is_some_and(|byte| byte.is_ascii_hexdigit()) {
                self.pos += 1;
            }
            let magnitude = i64::from_str_radix(&self.source[digits_start..self.pos], 16)
                .map_err(|_| self.error("invalid hexadecimal number"))?;
            return Ok(Value::from(if negative { -magnitude } else { magnitude }));
        }
        let mut is_float = false;
        let mut digits = 0;
        while let Some(byte) = self.peek() {
            match byte {
                b'0'..=b'9' => digits += 1,
                b'.' if !is_float => is_float = true,
                _ => break,
            }
            self.pos += 1;
        }
        if digits == 0 {
            return Err(self.error("a number is expected"));
        }
        if matches!(self.peek(), Some(b'e' | b'E')) {
            let exponent_at = self.pos;
            self.pos += 1;
            if matches!(self.peek(), Some(b'+' | b'-')) {
                self.pos += 1;
            }
            let exponent_digits = self.pos;
            while self.peek().is_some_and(|byte| byte.is_ascii_digit()) {
                self.pos += 1;
            }
            if self.pos == exponent_digits {
                self.pos = exponent_at;
                return Err(self.error("invalid exponent"));
            }
            is_float = true;
        }
        let text = &self.source[start..self.pos];
        let text = text.strip_prefix('+').unwrap_or(text);
        if is_float {
            let float: f64 = text.parse().map_err(|_| self.error("invalid number"))?;
            return Number::from_f64(float)
                .map(Value::Number)
                .ok_or_else(|| self.error("number is out of range"));
        }
        if let Ok(integer) = text.parse::<i64>() {
            return Ok(Value::from(integer));
        }
        text.parse::<u64>()
            .map(Value::from)
            .map_err(|_| self.error("integer is out of range; use NumberDecimal(\"…\") for it"))
    }

    fn identifier(&mut self) -> Result<Value, ShellError> {
        let start = self.pos;
        while self.peek().is_some_and(is_identifier_byte) {
            self.pos += 1;
        }
        let name = &self.source[start..self.pos];
        match name {
            "true" => return Ok(Value::Bool(true)),
            "false" => return Ok(Value::Bool(false)),
            "null" => return Ok(Value::Null),
            "undefined" => return Ok(wrapper("$undefined", Value::Bool(true))),
            "NaN" => return Ok(double_wrapper("NaN")),
            "Infinity" => return Ok(double_wrapper("Infinity")),
            _ => {}
        }
        let name = if name == "new" {
            self.skip_trivia()?;
            let constructor_at = self.pos;
            while self.peek().is_some_and(is_identifier_byte) {
                self.pos += 1;
            }
            &self.source[constructor_at..self.pos]
        } else {
            name
        };
        self.skip_trivia()?;
        if self.peek() != Some(b'(') {
            self.pos = start;
            return Err(self.error(format!("unknown name \"{name}\"; strings need quotes")));
        }
        let call_at = start;
        let arguments = self.arguments()?;
        let after = self.pos;
        self.pos = call_at;
        let built = self.construct(name, arguments);
        self.pos = after;
        built
    }

    fn arguments(&mut self) -> Result<Vec<Value>, ShellError> {
        self.pos += 1;
        let mut arguments = Vec::new();
        loop {
            self.skip_trivia()?;
            if self.peek() == Some(b')') {
                self.pos += 1;
                return Ok(arguments);
            }
            arguments.push(self.value()?);
            self.skip_trivia()?;
            match self.peek() {
                Some(b',') => self.pos += 1,
                Some(b')') => {}
                _ => return Err(self.error("expected ',' or ')'")),
            }
        }
    }

    fn construct(&self, name: &str, arguments: Vec<Value>) -> Result<Value, ShellError> {
        let bad = |detail: &str| self.error(format!("{name}(): {detail}"));
        let text_argument = |index: usize| match arguments.get(index) {
            Some(Value::String(text)) => Ok(text.as_str()),
            _ => Err(bad("a quoted string argument is expected")),
        };
        let count = |expected: std::ops::RangeInclusive<usize>| {
            if expected.contains(&arguments.len()) {
                Ok(())
            } else {
                Err(bad("wrong number of arguments"))
            }
        };
        match name {
            "ObjectId" => {
                count(1..=1)?;
                let hex = text_argument(0)?;
                bson::oid::ObjectId::parse_str(hex)
                    .map_err(|_| bad("an ObjectId is 24 hexadecimal digits"))?;
                Ok(wrapper("$oid", Value::String(hex.to_ascii_lowercase())))
            }
            "ISODate" | "Date" => {
                count(1..=1)?;
                let millis = match &arguments[0] {
                    Value::String(text) => parse_iso_date(text)
                        .ok_or_else(|| bad("not a date; use 2024-01-02T03:04:05.000Z"))?
                        .timestamp_millis(),
                    Value::Number(number) => number
                        .as_i64()
                        .ok_or_else(|| bad("milliseconds must be an integer"))?,
                    _ => return Err(bad("a date string or milliseconds is expected")),
                };
                Ok(wrapper(
                    "$date",
                    wrapper("$numberLong", Value::String(millis.to_string())),
                ))
            }
            "NumberInt" | "Int32" => {
                count(1..=1)?;
                let int: i32 = integer_argument(&arguments[0])
                    .and_then(|int| i32::try_from(int).ok())
                    .ok_or_else(|| bad("expected an integer in the 32-bit range"))?;
                Ok(wrapper("$numberInt", Value::String(int.to_string())))
            }
            "NumberLong" | "Long" => {
                count(1..=1)?;
                let int = integer_argument(&arguments[0])
                    .ok_or_else(|| bad("expected an integer in the 64-bit range"))?;
                Ok(wrapper("$numberLong", Value::String(int.to_string())))
            }
            "Double" => {
                count(1..=1)?;
                match &arguments[0] {
                    Value::Number(number) => {
                        let float = number.as_f64().ok_or_else(|| bad("expected a number"))?;
                        Ok(double_wrapper(&double_text(float)))
                    }
                    Value::String(text) => {
                        let float: f64 = text.parse().map_err(|_| bad("expected a number"))?;
                        Ok(double_wrapper(&double_text(float)))
                    }
                    other => match other.pointer("/$numberDouble") {
                        Some(Value::String(text)) => Ok(double_wrapper(text)),
                        _ => Err(bad("expected a number")),
                    },
                }
            }
            "NumberDecimal" | "Decimal128" => {
                count(1..=1)?;
                let text = match &arguments[0] {
                    Value::String(text) => text.clone(),
                    Value::Number(number) => number.to_string(),
                    _ => return Err(bad("a decimal string is expected")),
                };
                text.parse::<Decimal128>()
                    .map_err(|_| bad("not a valid Decimal128"))?;
                Ok(wrapper("$numberDecimal", Value::String(text)))
            }
            "UUID" => {
                count(1..=2)?;
                let subtype = match arguments.get(1) {
                    None => 4,
                    Some(Value::Number(number)) if matches!(number.as_u64(), Some(3 | 4)) => {
                        number.as_u64().unwrap_or(4) as u8
                    }
                    Some(_) => return Err(bad("the subtype is 3 (legacy) or 4")),
                };
                let hex: String = text_argument(0)?.chars().filter(|c| *c != '-').collect();
                let bytes = decode_hex(&hex)
                    .filter(|bytes| bytes.len() == 16)
                    .ok_or_else(|| bad("a UUID is 32 hexadecimal digits"))?;
                Ok(binary_wrapper(subtype, &bytes))
            }
            "BinData" | "HexData" => {
                count(2..=2)?;
                let subtype = match &arguments[0] {
                    Value::Number(number) => number
                        .as_u64()
                        .and_then(|subtype| u8::try_from(subtype).ok())
                        .ok_or_else(|| bad("the subtype is 0 to 255"))?,
                    _ => return Err(bad("the subtype is 0 to 255")),
                };
                let data = text_argument(1)?;
                let bytes = if name == "HexData" {
                    decode_hex(data).ok_or_else(|| bad("hexadecimal digits are expected"))?
                } else {
                    base64_decode(data).ok_or_else(|| bad("base64 data is expected"))?
                };
                Ok(binary_wrapper(subtype, &bytes))
            }
            "Timestamp" => {
                let (seconds, increment) = match arguments.as_slice() {
                    [Value::Object(fields)] => (fields.get("t"), fields.get("i")),
                    [seconds, increment] => (Some(seconds), Some(increment)),
                    _ => return Err(bad("use Timestamp({ t: seconds, i: increment })")),
                };
                let part = |value: Option<&Value>| {
                    value
                        .and_then(Value::as_u64)
                        .and_then(|value| u32::try_from(value).ok())
                        .ok_or_else(|| bad("t and i are integers from 0 to 4294967295"))
                };
                Ok(wrapper(
                    "$timestamp",
                    serde_json::json!({ "t": part(seconds)?, "i": part(increment)? }),
                ))
            }
            "MinKey" => {
                count(0..=0)?;
                Ok(wrapper("$minKey", Value::from(1)))
            }
            "MaxKey" => {
                count(0..=0)?;
                Ok(wrapper("$maxKey", Value::from(1)))
            }
            "Symbol" | "BSONSymbol" => {
                count(1..=1)?;
                Ok(wrapper(
                    "$symbol",
                    Value::String(text_argument(0)?.to_string()),
                ))
            }
            "Code" => {
                count(1..=2)?;
                let mut fields = Map::new();
                fields.insert("$code".into(), Value::String(text_argument(0)?.to_string()));
                if let Some(scope) = arguments.get(1) {
                    if !scope.is_object() {
                        return Err(bad("the scope is a document"));
                    }
                    fields.insert("$scope".into(), scope.clone());
                }
                Ok(Value::Object(fields))
            }
            _ => Err(self.error(format!("unknown constructor {name}()"))),
        }
    }
}

fn wrapper(key: &str, inner: Value) -> Value {
    let mut fields = Map::new();
    fields.insert(key.into(), inner);
    Value::Object(fields)
}

fn double_wrapper(text: &str) -> Value {
    wrapper("$numberDouble", Value::String(text.to_string()))
}

fn binary_wrapper(subtype: u8, bytes: &[u8]) -> Value {
    wrapper(
        "$binary",
        serde_json::json!({ "base64": base64_encode(bytes), "subType": format!("{subtype:02x}") }),
    )
}

fn integer_argument(value: &Value) -> Option<i64> {
    match value {
        Value::Number(number) => number.as_i64(),
        Value::String(text) => text.trim().parse().ok(),
        Value::Object(fields) if fields.len() == 1 => fields
            .get("$numberLong")
            .or_else(|| fields.get("$numberInt"))
            .and_then(Value::as_str)
            .and_then(|text| text.parse().ok()),
        _ => None,
    }
}

/// `f64` text that reads back as the same Double (see [`write_double`]).
fn double_text(value: f64) -> String {
    let mut text = String::new();
    let _ = write_double(&mut text, value);
    text
}

fn base64_encode(bytes: &[u8]) -> String {
    use base64::Engine;
    base64::engine::general_purpose::STANDARD.encode(bytes)
}

fn base64_decode(text: &str) -> Option<Vec<u8>> {
    use base64::Engine;
    base64::engine::general_purpose::STANDARD
        .decode(text.trim())
        .ok()
}

fn decode_hex(text: &str) -> Option<Vec<u8>> {
    if !text.len().is_multiple_of(2) || !text.is_ascii() {
        return None;
    }
    (0..text.len())
        .step_by(2)
        .map(|index| u8::from_str_radix(&text[index..index + 2], 16).ok())
        .collect()
}

fn to_bson(value: Value, typing: NumberTyping) -> Result<Bson, ShellError> {
    match value {
        Value::Null => Ok(Bson::Null),
        Value::Bool(value) => Ok(Bson::Boolean(value)),
        Value::String(value) => Ok(Bson::String(value)),
        Value::Number(number) => plain_number(&number, typing),
        Value::Array(items) => items
            .into_iter()
            .map(|item| to_bson(item, typing))
            .collect::<Result<Vec<_>, _>>()
            .map(Bson::Array),
        Value::Object(fields)
            if fields
                .keys()
                .any(|key| WRAPPER_KEYS.contains(&key.as_str())) =>
        {
            Bson::try_from(fields).map_err(|error| ShellError::plain(error.to_string()))
        }
        Value::Object(fields) => fields
            .into_iter()
            .map(|(key, value)| Ok((key, to_bson(value, typing)?)))
            .collect::<Result<Document, ShellError>>()
            .map(Bson::Document),
    }
}

fn plain_number(number: &Number, typing: NumberTyping) -> Result<Bson, ShellError> {
    if let Some(integer) = number.as_i64() {
        if let Ok(small) = i32::try_from(integer) {
            return Ok(Bson::Int32(small));
        }
        return match typing {
            NumberTyping::Filter => Ok(Bson::Int64(integer)),
            NumberTyping::Mongosh if integer.abs() <= MAX_EXACT_DOUBLE_INTEGER => {
                Ok(Bson::Double(integer as f64))
            }
            NumberTyping::Mongosh => Err(ShellError::plain(format!(
                "{integer} is too large for a Double to hold exactly; write NumberLong(\"{integer}\") for an Int64"
            ))),
        };
    }
    if number.is_u64() {
        return Err(ShellError::plain(format!(
            "{number} does not fit an Int64; write NumberDecimal(\"{number}\") for a Decimal128"
        )));
    }
    number
        .as_f64()
        .map(Bson::Double)
        .ok_or_else(|| ShellError::plain("number is out of range"))
}

// ---------------------------------------------------------------------------
// Printing
// ---------------------------------------------------------------------------

/// Writes a Double the way mongosh would read it back as a Double: shortest
/// round-trip digits and always a decimal point or an exponent (`5.0`,
/// `1.5e300`), `NaN`, `Infinity`, `-Infinity`, `-0.0`.
pub fn write_double(out: &mut dyn Write, value: f64) -> fmt::Result {
    if value.is_nan() {
        out.write_str("NaN")
    } else if value.is_infinite() {
        out.write_str(if value > 0.0 { "Infinity" } else { "-Infinity" })
    } else {
        write!(out, "{value:?}")
    }
}

/// `2024-01-02T03:04:05.123Z` (UTC, always milliseconds); `None` outside years
/// 0000-9999.
pub fn iso_date_text(millis: i64) -> Option<String> {
    const DAY_MILLIS: i64 = 86_400_000;
    let (year, month, day) = civil_from_days(millis.div_euclid(DAY_MILLIS));
    if !(0..=9999).contains(&year) {
        return None;
    }
    let of_day = millis.rem_euclid(DAY_MILLIS);
    Some(format!(
        "{year:04}-{month:02}-{day:02}T{:02}:{:02}:{:02}.{:03}Z",
        of_day / 3_600_000,
        of_day / 60_000 % 60,
        of_day / 1000 % 60,
        of_day % 1000
    ))
}

/// `ISODate('2024-01-02T03:04:05.123Z')` (UTC, always milliseconds); an instant
/// outside years 0000-9999 is written `Date(<milliseconds>)`.
pub fn write_date(out: &mut dyn Write, millis: i64) -> fmt::Result {
    match iso_date_text(millis) {
        Some(text) => write!(out, "ISODate('{text}')"),
        None => write!(out, "Date({millis})"),
    }
}

/// Days since 1970-01-01 to a proleptic Gregorian (year, month, day).
fn civil_from_days(days: i64) -> (i64, i64, i64) {
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let day_of_era = z.rem_euclid(146_097);
    let year_of_era =
        (day_of_era - day_of_era / 1460 + day_of_era / 36_524 - day_of_era / 146_096) / 365;
    let day_of_year = day_of_era - (365 * year_of_era + year_of_era / 4 - year_of_era / 100);
    let month_index = (5 * day_of_year + 2) / 153;
    let day = day_of_year - (153 * month_index + 2) / 5 + 1;
    let month = if month_index < 10 {
        month_index + 3
    } else {
        month_index - 9
    };
    let year = year_of_era + era * 400 + i64::from(month <= 2);
    (year, month, day)
}

/// `UUID('…')` for subtype 4, `UUID('…', 3)` for legacy subtype 3 (16 bytes),
/// otherwise `BinData(<subtype>, '<base64>')`. With `inline_limit`, a longer
/// payload is shown as `BinData(<subtype>, <N bytes>)` (display only).
pub fn write_binary(
    out: &mut dyn Write,
    subtype: u8,
    bytes: &[u8],
    inline_limit: Option<usize>,
) -> fmt::Result {
    if bytes.len() == 16 && matches!(subtype, 3 | 4) {
        out.write_str("UUID('")?;
        for (index, byte) in bytes.iter().enumerate() {
            if matches!(index, 4 | 6 | 8 | 10) {
                out.write_char('-')?;
            }
            write!(out, "{byte:02x}")?;
        }
        return if subtype == 3 {
            out.write_str("', 3)")
        } else {
            out.write_str("')")
        };
    }
    if inline_limit.is_some_and(|limit| bytes.len() > limit) {
        return write!(out, "BinData({subtype}, <{} bytes>)", bytes.len());
    }
    write!(out, "BinData({subtype}, '{}')", base64_encode(bytes))
}

/// `/pattern/flags`; a pattern a regex literal cannot carry (a `/`, a line
/// break, or empty) is written as Extended JSON, which parses back identically.
pub fn write_regex(out: &mut dyn Write, pattern: &str, options: &str) -> fmt::Result {
    if pattern.is_empty() || pattern.contains(['/', '\n', '\r']) {
        let pattern = serde_json::to_string(pattern).unwrap_or_default();
        let options = serde_json::to_string(options).unwrap_or_default();
        return write!(
            out,
            "{{ \"$regularExpression\": {{ \"pattern\": {pattern}, \"options\": {options} }} }}"
        );
    }
    write!(out, "/{pattern}/{options}")
}

/// `Timestamp({ t: 1, i: 2 })`.
pub fn write_timestamp(out: &mut dyn Write, time: u32, increment: u32) -> fmt::Result {
    write!(out, "Timestamp({{ t: {time}, i: {increment} }})")
}

/// The value in shell syntax; `pretty` lays documents and arrays out one member
/// per line.
pub fn to_shell(value: &Bson, pretty: bool) -> String {
    let mut out = String::new();
    write_bson(&mut out, value, pretty, 0);
    out
}

pub fn document_to_shell(document: &Document, pretty: bool) -> String {
    let mut out = String::new();
    write_members(&mut out, document, pretty, 0);
    out
}

fn quoted(text: &str) -> String {
    serde_json::to_string(text).unwrap_or_else(|_| "\"\"".into())
}

fn newline(out: &mut String, depth: usize) {
    out.push('\n');
    for _ in 0..depth {
        out.push_str("  ");
    }
}

fn write_members(out: &mut String, members: &Document, pretty: bool, depth: usize) {
    if members.is_empty() {
        out.push_str("{}");
        return;
    }
    out.push('{');
    for (index, (key, value)) in members.iter().enumerate() {
        if index > 0 {
            out.push(',');
        }
        if pretty {
            newline(out, depth + 1);
        } else {
            out.push(' ');
        }
        out.push_str(&quoted(key));
        out.push_str(": ");
        write_bson(out, value, pretty, depth + 1);
    }
    if pretty {
        newline(out, depth);
    } else {
        out.push(' ');
    }
    out.push('}');
}

fn write_bson(out: &mut String, value: &Bson, pretty: bool, depth: usize) {
    // Writing into a `String` cannot fail.
    match value {
        Bson::Document(document) => write_members(out, document, pretty, depth),
        Bson::Array(items) => {
            if items.is_empty() {
                out.push_str("[]");
                return;
            }
            out.push('[');
            for (index, item) in items.iter().enumerate() {
                if index > 0 {
                    out.push(',');
                }
                if pretty {
                    newline(out, depth + 1);
                } else {
                    out.push(' ');
                }
                write_bson(out, item, pretty, depth + 1);
            }
            if pretty {
                newline(out, depth);
            } else {
                out.push(' ');
            }
            out.push(']');
        }
        Bson::String(text) => out.push_str(&quoted(text)),
        Bson::Boolean(value) => out.push_str(if *value { "true" } else { "false" }),
        Bson::Null => out.push_str("null"),
        Bson::Undefined => out.push_str("undefined"),
        Bson::Int32(value) => {
            let _ = write!(out, "{value}");
        }
        Bson::Int64(value) => {
            let _ = write!(out, "Long('{value}')");
        }
        Bson::Double(value) => {
            let _ = write_double(out, *value);
        }
        Bson::Decimal128(value) => {
            let _ = write!(out, "Decimal128('{value}')");
        }
        Bson::ObjectId(value) => {
            let _ = write!(out, "ObjectId('{value}')");
        }
        Bson::DateTime(value) => {
            let _ = write_date(out, value.timestamp_millis());
        }
        Bson::Binary(Binary { subtype, bytes }) => {
            let _ = write_binary(out, u8::from(*subtype), bytes, None);
        }
        Bson::RegularExpression(regex) => {
            let _ = write_regex(out, &regex.pattern, &regex.options);
        }
        Bson::Timestamp(value) => {
            let _ = write_timestamp(out, value.time, value.increment);
        }
        Bson::MinKey => out.push_str("MinKey()"),
        Bson::MaxKey => out.push_str("MaxKey()"),
        Bson::Symbol(text) => {
            let _ = write!(out, "Symbol({})", quoted(text));
        }
        Bson::JavaScriptCode(code) => {
            let _ = write!(out, "Code({})", quoted(code));
        }
        Bson::JavaScriptCodeWithScope(code) => {
            let _ = write!(out, "Code({}, ", quoted(&code.code));
            write_members(out, &code.scope, pretty, depth);
            out.push(')');
        }
        Bson::DbPointer(_) => out.push_str(&value.clone().into_canonical_extjson().to_string()),
    }
}

/// Which JSON object keys read as BSON wrappers; exposed for callers that
/// decide whether an object is a value or a document.
pub fn is_wrapper_key(key: &str) -> bool {
    WRAPPER_KEYS.contains(&key)
}

#[cfg(test)]
mod tests {
    use super::*;
    use bson::oid::ObjectId;
    use bson::spec::BinarySubtype;
    use bson::{doc, Timestamp};

    fn parse(text: &str) -> Bson {
        parse_value(text, NumberTyping::Mongosh).unwrap_or_else(|error| panic!("{text}: {error}"))
    }

    fn one_of_each() -> Document {
        let binary = |subtype, bytes: Vec<u8>| Bson::Binary(Binary { subtype, bytes });
        let regex = |pattern: &str| {
            Bson::RegularExpression(bson::Regex {
                pattern: pattern.into(),
                options: "i".into(),
            })
        };
        let members: Vec<(&str, Bson)> = vec![
            ("int32", Bson::Int32(5)),
            ("int32_neg", Bson::Int32(-7)),
            ("int32_min", Bson::Int32(i32::MIN)),
            ("int64_small", Bson::Int64(5)),
            ("int64_neg", Bson::Int64(-5)),
            ("int64_big", Bson::Int64(9_007_199_254_740_993)),
            ("double_whole", Bson::Double(5.0)),
            ("double", Bson::Double(1.5)),
            ("double_exp", Bson::Double(1.5e300)),
            ("double_tiny", Bson::Double(5e-324)),
            ("double_neg_zero", Bson::Double(-0.0)),
            ("nan", Bson::Double(f64::NAN)),
            ("inf", Bson::Double(f64::INFINITY)),
            ("ninf", Bson::Double(f64::NEG_INFINITY)),
            ("decimal", Bson::Decimal128("1.50".parse().unwrap())),
            ("string", Bson::String("h\u{e9} \"llo\"\n".into())),
            ("empty", Bson::String(String::new())),
            ("bool", Bson::Boolean(true)),
            ("null", Bson::Null),
            ("undefined", Bson::Undefined),
            (
                "oid",
                Bson::ObjectId(ObjectId::parse_str("66d4f84d71c18f22d433b003").unwrap()),
            ),
            (
                "date",
                Bson::DateTime(DateTime::from_millis(1_700_000_000_123)),
            ),
            (
                "date_old",
                Bson::DateTime(DateTime::from_millis(-62_198_755_200_001)),
            ),
            ("date_epoch", Bson::DateTime(DateTime::from_millis(0))),
            ("bin", binary(BinarySubtype::Generic, vec![0, 1, 255])),
            (
                "bin_user",
                binary(BinarySubtype::UserDefined(0x90), vec![9]),
            ),
            ("uuid", binary(BinarySubtype::Uuid, (0..16).collect())),
            (
                "uuid_legacy",
                binary(BinarySubtype::UuidOld, (0..16).collect()),
            ),
            ("regex", regex("^a.b$")),
            ("regex_slash", regex("a/b")),
            (
                "ts",
                Bson::Timestamp(Timestamp {
                    time: 1,
                    increment: 2,
                }),
            ),
            ("min", Bson::MinKey),
            ("max", Bson::MaxKey),
            ("sym", Bson::Symbol("s".into())),
            ("js", Bson::JavaScriptCode("x => 1".into())),
            (
                "js_scope",
                Bson::JavaScriptCodeWithScope(bson::JavaScriptCodeWithScope {
                    code: "f".into(),
                    scope: doc! { "a": 1_i64 },
                }),
            ),
            (
                "doc",
                Bson::Document(doc! { "z": 1_i64, "a": { "n": [1, 2.0, Bson::Int64(3)] } }),
            ),
            (
                "arr",
                Bson::Array(vec![Bson::Document(doc! { "k": "v" }), Bson::Array(vec![])]),
            ),
            ("$weird.key", Bson::Int32(1)),
            ("a.b", Bson::Int32(2)),
        ];
        members
            .into_iter()
            .map(|(k, v)| (k.to_string(), v))
            .collect()
    }

    #[test]
    fn every_bson_type_prints_and_parses_back_identically() {
        let document = one_of_each();
        for pretty in [true, false] {
            let text = document_to_shell(&document, pretty);
            let parsed = parse_document(&text, NumberTyping::Mongosh)
                .unwrap_or_else(|error| panic!("{error}\n{text}"));
            // NaN != NaN: compare the canonical text, which is also exact for the rest.
            assert_eq!(
                Bson::Document(parsed.clone())
                    .into_canonical_extjson()
                    .to_string(),
                Bson::Document(document.clone())
                    .into_canonical_extjson()
                    .to_string(),
                "{text}"
            );
            assert_eq!(
                parsed.iter().map(|(k, _)| k.clone()).collect::<Vec<_>>(),
                document.iter().map(|(k, _)| k.clone()).collect::<Vec<_>>(),
                "key order"
            );
        }
    }

    #[test]
    fn canonical_extended_json_round_trips_through_the_shell_syntax() {
        let document = one_of_each();
        let canonical = Bson::Document(document.clone()).into_canonical_extjson();
        let back = Bson::try_from(canonical.clone()).unwrap();
        let text = to_shell(&back, true);
        assert_eq!(
            Bson::try_from(parse(&text).into_canonical_extjson())
                .unwrap()
                .into_canonical_extjson(),
            canonical
        );
    }

    #[test]
    fn plain_numbers_are_typed_like_mongosh() {
        assert_eq!(parse("5"), Bson::Int32(5));
        assert_eq!(parse("-2147483648"), Bson::Int32(i32::MIN));
        assert_eq!(parse("5.0"), Bson::Double(5.0));
        assert_eq!(parse("1e3"), Bson::Double(1000.0));
        assert_eq!(parse("2147483648"), Bson::Double(2_147_483_648.0));
        assert_eq!(parse("1.5"), Bson::Double(1.5));
        assert_eq!(parse("0x10"), Bson::Int32(16));
        assert!(parse_value("9007199254740993", NumberTyping::Mongosh).is_err());
        assert_eq!(
            parse_value("9007199254740993", NumberTyping::Filter).unwrap(),
            Bson::Int64(9_007_199_254_740_993)
        );
        assert_eq!(
            parse_value("2147483648", NumberTyping::Filter).unwrap(),
            Bson::Int64(2_147_483_648)
        );
        assert!(parse_value("18446744073709551615", NumberTyping::Filter).is_err());
    }

    #[test]
    fn shell_constructors_and_extended_json_give_the_same_value() {
        let pairs = [
            (
                r#"ObjectId("66d4f84d71c18f22d433b003")"#,
                r#"{"$oid":"66d4f84d71c18f22d433b003"}"#,
            ),
            (r#"NumberLong(5)"#, r#"{"$numberLong":"5"}"#),
            (r#"NumberLong("5")"#, r#"{"$numberLong":"5"}"#),
            (
                r#"Long("-9223372036854775808")"#,
                r#"{"$numberLong":"-9223372036854775808"}"#,
            ),
            (r#"NumberInt("7")"#, r#"{"$numberInt":"7"}"#),
            (r#"NumberDecimal("1.50")"#, r#"{"$numberDecimal":"1.50"}"#),
            (r#"Decimal128("NaN")"#, r#"{"$numberDecimal":"NaN"}"#),
            (
                r#"ISODate("2024-09-01T00:00:00Z")"#,
                r#"{"$date":"2024-09-01T00:00:00Z"}"#,
            ),
            (
                r#"ISODate("2024-09-01")"#,
                r#"{"$date":{"$numberLong":"1725148800000"}}"#,
            ),
            (r#"new Date(1725148800000)"#, r#"{"$date":1725148800000}"#),
            (r#"NaN"#, r#"{"$numberDouble":"NaN"}"#),
            (r#"-Infinity"#, r#"{"$numberDouble":"-Infinity"}"#),
            (r#"MinKey()"#, r#"{"$minKey":1}"#),
            (r#"undefined"#, r#"{"$undefined":true}"#),
            (
                r#"/a\/b/i"#,
                r#"{"$regularExpression":{"pattern":"a\\/b","options":"i"}}"#,
            ),
            (
                r#"Timestamp({ t: 1, i: 2 })"#,
                r#"{"$timestamp":{"t":1,"i":2}}"#,
            ),
            (r#"Timestamp(1, 2)"#, r#"{"$timestamp":{"t":1,"i":2}}"#),
            (
                r#"BinData(0, "AAH/")"#,
                r#"{"$binary":{"base64":"AAH/","subType":"00"}}"#,
            ),
            (
                r#"UUID("00010203-0405-0607-0809-0a0b0c0d0e0f")"#,
                r#"{"$binary":{"base64":"AAECAwQFBgcICQoLDA0ODw==","subType":"04"}}"#,
            ),
            (
                r#"{ $date: "2024-09-01T00:00:00Z" }"#,
                r#"{"$date":"2024-09-01T00:00:00Z"}"#,
            ),
        ];
        for (shell, extended) in pairs {
            let from_extended: Value = serde_json::from_str(extended).unwrap();
            assert_eq!(
                parse(shell).into_canonical_extjson().to_string(),
                Bson::try_from(from_extended)
                    .unwrap()
                    .into_canonical_extjson()
                    .to_string(),
                "{shell} vs {extended}"
            );
        }
    }

    #[test]
    fn json5_conveniences_still_parse() {
        let document = parse_document(
            "// filter\n{ name: 'Ada', \"age\": {$gt: 25, $lt: 50,}, tags: ['a', \"b\",], /* c */ n: .5, $or: [], }",
            NumberTyping::Filter,
        )
        .unwrap();
        assert_eq!(document.get_str("name").unwrap(), "Ada");
        assert_eq!(
            document
                .get_document("age")
                .unwrap()
                .get_i32("$gt")
                .unwrap(),
            25
        );
        assert_eq!(document.get_f64("n").unwrap(), 0.5);
    }

    #[test]
    fn strings_that_look_like_constructors_are_left_alone() {
        let document = parse_document(
            r#"{ "a": "ObjectId(\"66d4f84d71c18f22d433b003\")", "b": 'ISODate("x")' }"#,
            NumberTyping::Filter,
        )
        .unwrap();
        assert_eq!(
            document.get_str("a").unwrap(),
            r#"ObjectId("66d4f84d71c18f22d433b003")"#
        );
        assert_eq!(document.get_str("b").unwrap(), r#"ISODate("x")"#);
    }

    #[test]
    fn operator_documents_are_not_mistaken_for_wrappers() {
        let document = parse_document(
            r#"{ n: { $regex: "^a", $options: "i" }, d: { $exists: true } }"#,
            NumberTyping::Filter,
        )
        .unwrap();
        assert_eq!(
            document
                .get_document("n")
                .unwrap()
                .get_str("$regex")
                .unwrap(),
            "^a"
        );
    }

    #[test]
    fn mistakes_are_reported_with_a_position() {
        for bad in [
            "{ a: }",
            "{ a: 1",
            "[1, 2",
            "{ a: foo }",
            "ObjectId(\"zz\")",
            "NumberLong(1.5)",
            "{ a: 1 } x",
            "'open",
            "{ a: 1, a: 2 }",
            "Nope(1)",
            "{ $numberLong: 5 }",
            "NumberInt(2147483648)",
            "/open",
        ] {
            let error = parse_value(bad, NumberTyping::Mongosh).unwrap_err();
            assert!(!error.to_string().is_empty(), "{bad}");
        }
        let error = parse_value("{\n  a: foo\n}", NumberTyping::Mongosh).unwrap_err();
        assert!(error.to_string().contains("line 2"), "{error}");
    }

    #[test]
    fn dates_print_in_utc_with_milliseconds() {
        let text = |millis| {
            let mut out = String::new();
            write_date(&mut out, millis).unwrap();
            out
        };
        assert_eq!(text(0), "ISODate('1970-01-01T00:00:00.000Z')");
        assert_eq!(
            text(1_577_934_245_123),
            "ISODate('2020-01-02T03:04:05.123Z')"
        );
        assert_eq!(text(-1), "ISODate('1969-12-31T23:59:59.999Z')");
        assert_eq!(text(253_402_300_800_000), "Date(253402300800000)");
        assert_eq!(
            parse(&text(253_402_300_800_000)),
            Bson::DateTime(DateTime::from_millis(253_402_300_800_000))
        );
    }

    #[test]
    fn doubles_always_carry_a_point_or_an_exponent() {
        let text = |value| {
            let mut out = String::new();
            write_double(&mut out, value).unwrap();
            out
        };
        assert_eq!(text(5.0), "5.0");
        assert_eq!(text(0.1 + 0.2), "0.30000000000000004");
        assert_eq!(text(1e300), "1e300");
        assert_eq!(text(-0.0), "-0.0");
        assert_eq!(text(f64::NAN), "NaN");
        assert_eq!(text(f64::NEG_INFINITY), "-Infinity");
    }

    #[test]
    fn iso_dates_accept_the_shorthands_mongosh_accepts() {
        let millis = |text| parse_iso_date(text).map(|date| date.timestamp_millis());
        assert_eq!(millis("2024-09-01"), Some(1_725_148_800_000));
        assert_eq!(millis("2024-09-01T00:00:00"), Some(1_725_148_800_000));
        assert_eq!(millis("2024-09-01 01:00:00+0100"), Some(1_725_148_800_000));
        assert_eq!(millis("2024-09-01T00:00:00.5Z"), Some(1_725_148_800_500));
        assert_eq!(millis("yesterday"), None);
    }
}
