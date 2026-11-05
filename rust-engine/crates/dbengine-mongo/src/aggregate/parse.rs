//! mongosh-style relaxed literal parser for aggregation pipelines.
//!
//! Replaces the old `json5::from_str::<Vec<Document>>` path, which could not
//! read the shell syntax users paste from mongosh / Compass (`ObjectId("…")`,
//! `ISODate("…")`, `/re/i`, single-quoted strings, …) and whose find-path
//! cousin (`query_parser`) rewrote text with regexes and parsed through a
//! `serde_json::Value` (key order is not guaranteed there).
//!
//! This is a small recursive-descent parser that builds [`bson::Bson`]
//! directly, so **key order is preserved exactly as written** (it matters for
//! `$sort`, `$group` and compound index hints) and every error carries the
//! 1-based line / column of the offending character.
//!
//! Accepted syntax: JSON plus unquoted / single-quoted keys, single-quoted
//! strings, trailing commas, `//` and `/* */` comments, hex integers,
//! `Infinity` / `NaN` / `undefined`, regex literals, `new X(...)` and the
//! shell constructors (see [`parse_constructor`]), and Extended JSON wrapper
//! objects (`{"$oid": …}`, `{"$date": …}`, `{"$numberLong": …}`, …).
//!
//! Numbers: an integer literal that fits `i32` becomes `Int32`, one that
//! fits `i64` becomes `Int64`, anything with a fraction / exponent (or too
//! large) becomes `Double`.
//!
//! The module depends only on `bson`, `chrono` and `serde`, so the GPUI app
//! can include the same file to validate pipeline text while the user types.
use bson::{Bson, Document};
use serde::{Deserialize, Serialize};

const MAX_DEPTH: usize = 100;

// Test hook: how many parses ran **on this thread** (tests run in parallel,
// each on its own thread). The GPUI tests use it to prove that rendering
// never parses a pipeline.
#[cfg(test)]
thread_local! {
    static PARSER_RUNS: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
}

#[cfg(test)]
pub fn parser_runs() -> usize {
    PARSER_RUNS.with(std::cell::Cell::get)
}

/// A structured parse / validation failure. `line` / `col` are 1-based
/// character positions in the pipeline text (`0` = unknown, e.g. the
/// pipeline did not come from text); `stage` is the 1-based pipeline stage
/// the problem belongs to when known.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PipelineError {
    pub stage: Option<usize>,
    pub line: usize,
    pub col: usize,
    pub message: String,
}

impl std::fmt::Display for PipelineError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        if let Some(stage) = self.stage {
            write!(f, "Stage {stage}")?;
            if self.line > 0 {
                write!(f, " (line {}, col {})", self.line, self.col)?;
            }
            write!(f, ": {}", self.message)
        } else if self.line > 0 {
            write!(f, "Line {}, col {}: {}", self.line, self.col, self.message)
        } else {
            f.write_str(&self.message)
        }
    }
}

impl std::error::Error for PipelineError {}

/// A parsed pipeline: its stages plus where each one starts in the source.
#[derive(Debug, Clone, PartialEq)]
pub struct ParsedPipeline {
    pub stages: Vec<Document>,
    /// `(line, col)` of each stage's opening brace, 1-based.
    pub positions: Vec<(usize, usize)>,
}

struct Fail {
    pos: usize,
    message: String,
}

type R<T> = Result<T, Fail>;

fn fail<T>(pos: usize, message: impl Into<String>) -> R<T> {
    Err(Fail {
        pos,
        message: message.into(),
    })
}

/// 1-based `(line, col)` of byte offset `pos` (columns count characters).
pub fn line_col(text: &str, pos: usize) -> (usize, usize) {
    let pos = pos.min(text.len());
    let mut line = 1;
    let mut col = 1;
    for (index, ch) in text.char_indices() {
        if index >= pos {
            break;
        }
        if ch == '\n' {
            line += 1;
            col = 1;
        } else {
            col += 1;
        }
    }
    (line, col)
}

fn to_error(text: &str, failure: Fail, stage: Option<usize>) -> PipelineError {
    let (line, col) = line_col(text, failure.pos);
    PipelineError {
        stage,
        line,
        col,
        message: failure.message,
    }
}

/// Parses a whole pipeline `[ {stage}, … ]`. Empty / whitespace-only text is
/// an error (a pipeline is an array); `[]` is a valid empty pipeline.
pub fn parse_pipeline(text: &str) -> Result<ParsedPipeline, PipelineError> {
    let mut parser = Parser::new(text);
    parser.skip_ws();
    if parser.at_end() {
        return Err(PipelineError {
            stage: None,
            line: 1,
            col: 1,
            message: "Pipeline is empty. Start with [ and add stage objects.".into(),
        });
    }
    let mut stages = Vec::new();
    let mut positions = Vec::new();
    let outcome = parser.parse_pipeline_array(&mut stages, &mut positions);
    match outcome {
        Ok(()) => Ok(ParsedPipeline { stages, positions }),
        Err(failure) => {
            // Errors raised while reading stage N (0-based `stages.len()`)
            // belong to that stage once its opening brace was seen.
            let stage = parser.current_stage.map(|index| index + 1);
            Err(to_error(text, failure, stage))
        }
    }
}

/// Parses one value (a stage editor's text). Trailing content is an error.
pub fn parse_value(text: &str) -> Result<Bson, PipelineError> {
    let mut parser = Parser::new(text);
    parser.skip_ws();
    if parser.at_end() {
        return Err(PipelineError {
            stage: None,
            line: 1,
            col: 1,
            message: "Expected a value.".into(),
        });
    }
    let value = parser
        .parse_value(0)
        .and_then(|value| parser.finish().map(|()| value));
    value.map_err(|failure| to_error(text, failure, None))
}

/// Parses one stage's text: a single `{ $op: … }` object.
pub fn parse_stage(text: &str) -> Result<Document, PipelineError> {
    match parse_value(text)? {
        Bson::Document(document) => Ok(document),
        _ => Err(PipelineError {
            stage: None,
            line: 1,
            col: 1,
            message: "A stage must be an object like { $match: { … } }.".into(),
        }),
    }
}

struct Parser<'a> {
    src: &'a str,
    bytes: &'a [u8],
    pos: usize,
    /// 0-based index of the top-level element being parsed.
    current_stage: Option<usize>,
}

impl<'a> Parser<'a> {
    fn new(src: &'a str) -> Self {
        #[cfg(test)]
        PARSER_RUNS.with(|runs| runs.set(runs.get() + 1));
        Self {
            src,
            bytes: src.as_bytes(),
            pos: 0,
            current_stage: None,
        }
    }

    fn at_end(&self) -> bool {
        self.pos >= self.bytes.len()
    }

    fn peek(&self) -> Option<u8> {
        self.bytes.get(self.pos).copied()
    }

    fn peek_char(&self) -> Option<char> {
        self.src[self.pos..].chars().next()
    }

    /// Skips whitespace and `//` / `/* */` comments.
    fn skip_ws(&mut self) {
        loop {
            match self.peek() {
                Some(b' ' | b'\t' | b'\n' | b'\r') => self.pos += 1,
                Some(b'/') => match self.bytes.get(self.pos + 1) {
                    Some(b'/') => {
                        while let Some(byte) = self.peek() {
                            if byte == b'\n' {
                                break;
                            }
                            self.pos += 1;
                        }
                    }
                    Some(b'*') => {
                        let start = self.pos;
                        self.pos += 2;
                        loop {
                            match self.peek() {
                                None => {
                                    // Unterminated: stop here, the caller reports
                                    // the unexpected end at the comment start.
                                    self.pos = start;
                                    return;
                                }
                                Some(b'*') if self.bytes.get(self.pos + 1) == Some(&b'/') => {
                                    self.pos += 2;
                                    break;
                                }
                                Some(_) => self.pos += 1,
                            }
                        }
                    }
                    _ => return,
                },
                // Non-breaking space and BOM are common in pasted text.
                Some(0xC2) if self.bytes.get(self.pos + 1) == Some(&0xA0) => self.pos += 2,
                Some(0xEF)
                    if self.bytes.get(self.pos + 1) == Some(&0xBB)
                        && self.bytes.get(self.pos + 2) == Some(&0xBF) =>
                {
                    self.pos += 3
                }
                _ => return,
            }
        }
    }

    fn finish(&mut self) -> R<()> {
        self.skip_ws();
        if self.at_end() {
            Ok(())
        } else {
            fail(self.pos, "Unexpected content after the end of the value.")
        }
    }

    fn describe_here(&self) -> String {
        match self.peek_char() {
            None => "end of input".into(),
            Some(ch) => format!("'{ch}'"),
        }
    }

    fn expect(&mut self, byte: u8) -> R<()> {
        self.skip_ws();
        if self.peek() == Some(byte) {
            self.pos += 1;
            Ok(())
        } else {
            fail(
                self.pos,
                format!("Expected '{}' but found {}.", byte as char, self.describe_here()),
            )
        }
    }

    fn parse_pipeline_array(
        &mut self,
        stages: &mut Vec<Document>,
        positions: &mut Vec<(usize, usize)>,
    ) -> R<()> {
        if self.peek() != Some(b'[') {
            return fail(
                self.pos,
                format!(
                    "A pipeline must be an array: expected '[' but found {}.",
                    self.describe_here()
                ),
            );
        }
        self.pos += 1;
        loop {
            self.skip_ws();
            match self.peek() {
                Some(b']') => {
                    self.pos += 1;
                    break;
                }
                None => return fail(self.pos, "Unterminated pipeline: expected ']'."),
                _ => {}
            }
            let index = stages.len();
            self.current_stage = Some(index);
            let start = self.pos;
            if self.peek() != Some(b'{') {
                return fail(
                    start,
                    format!(
                        "Stage {} must be an object like {{ $match: {{ … }} }}, found {}.",
                        index + 1,
                        self.describe_here()
                    ),
                );
            }
            let value = self.parse_value(0)?;
            let Bson::Document(document) = value else {
                return fail(
                    start,
                    format!("Stage {} must be an object.", index + 1),
                );
            };
            positions.push(line_col(self.src, start));
            stages.push(document);
            self.current_stage = None;
            self.skip_ws();
            match self.peek() {
                Some(b',') => self.pos += 1,
                Some(b']') => {}
                None => return fail(self.pos, "Unterminated pipeline: expected ']'."),
                Some(_) => {
                    // Point at the end of the finished stage; blame belongs to
                    // the stage that is missing a comma after it.
                    self.current_stage = Some(index);
                    return fail(
                        self.pos,
                        format!(
                            "Expected ',' or ']' after stage {} but found {}.",
                            index + 1,
                            self.describe_here()
                        ),
                    );
                }
            }
        }
        self.finish()
    }

    fn parse_value(&mut self, depth: usize) -> R<Bson> {
        if depth > MAX_DEPTH {
            return fail(self.pos, "Nesting is too deep.");
        }
        self.skip_ws();
        let start = self.pos;
        match self.peek() {
            None => fail(self.pos, "Unexpected end of input: expected a value."),
            Some(b'{') => self.parse_object(depth),
            Some(b'[') => self.parse_array(depth),
            Some(b'"') | Some(b'\'') => self.parse_string().map(Bson::String),
            Some(b'/') => self.parse_regex(),
            Some(b'-') | Some(b'+') | Some(b'.') | Some(b'0'..=b'9') => self.parse_number(),
            Some(byte) if is_ident_start(byte) || byte >= 0x80 => self.parse_word(depth, start),
            Some(_) => fail(
                self.pos,
                format!("Unexpected {}.", self.describe_here()),
            ),
        }
    }

    fn parse_array(&mut self, depth: usize) -> R<Bson> {
        self.pos += 1; // [
        let mut items = Vec::new();
        loop {
            self.skip_ws();
            match self.peek() {
                Some(b']') => {
                    self.pos += 1;
                    return Ok(Bson::Array(items));
                }
                None => return fail(self.pos, "Unterminated array: expected ']'."),
                _ => {}
            }
            items.push(self.parse_value(depth + 1)?);
            self.skip_ws();
            match self.peek() {
                Some(b',') => self.pos += 1,
                Some(b']') => {}
                None => return fail(self.pos, "Unterminated array: expected ']'."),
                _ => {
                    return fail(
                        self.pos,
                        format!("Expected ',' or ']' but found {}.", self.describe_here()),
                    )
                }
            }
        }
    }

    fn parse_object(&mut self, depth: usize) -> R<Bson> {
        let open = self.pos;
        self.pos += 1; // {
        let mut document = Document::new();
        loop {
            self.skip_ws();
            match self.peek() {
                Some(b'}') => {
                    self.pos += 1;
                    break;
                }
                None => return fail(self.pos, "Unterminated object: expected '}'."),
                _ => {}
            }
            let key = self.parse_key()?;
            self.expect(b':')?;
            let value = self.parse_value(depth + 1)?;
            document.insert(key, value);
            self.skip_ws();
            match self.peek() {
                Some(b',') => self.pos += 1,
                Some(b'}') => {}
                None => return fail(self.pos, "Unterminated object: expected '}'."),
                _ => {
                    return fail(
                        self.pos,
                        format!("Expected ',' or '}}' but found {}.", self.describe_here()),
                    )
                }
            }
        }
        match extended_json(&document) {
            Some(Ok(value)) => Ok(value),
            Some(Err(message)) => fail(open, message),
            None => Ok(Bson::Document(document)),
        }
    }

    fn parse_key(&mut self) -> R<String> {
        self.skip_ws();
        match self.peek() {
            Some(b'"') | Some(b'\'') => self.parse_string(),
            Some(byte) if is_ident_start(byte) || byte >= 0x80 => Ok(self.parse_ident()),
            Some(b'0'..=b'9') => {
                let start = self.pos;
                while self.peek().is_some_and(|byte| byte.is_ascii_digit()) {
                    self.pos += 1;
                }
                Ok(self.src[start..self.pos].to_string())
            }
            None => fail(self.pos, "Unterminated object: expected a key or '}'."),
            Some(_) => fail(
                self.pos,
                format!("Expected a key but found {}.", self.describe_here()),
            ),
        }
    }

    fn parse_ident(&mut self) -> String {
        let start = self.pos;
        while let Some(ch) = self.peek_char() {
            if ch.is_alphanumeric() || ch == '_' || ch == '$' {
                self.pos += ch.len_utf8();
            } else {
                break;
            }
        }
        self.src[start..self.pos].to_string()
    }

    fn parse_string(&mut self) -> R<String> {
        let start = self.pos;
        let quote = self.bytes[self.pos];
        self.pos += 1;
        let mut out = String::new();
        loop {
            let Some(ch) = self.peek_char() else {
                return fail(start, "Unterminated string.");
            };
            self.pos += ch.len_utf8();
            if ch as u32 == quote as u32 {
                return Ok(out);
            }
            match ch {
                '\n' | '\r' => return fail(start, "Unterminated string (line break inside quotes)."),
                '\\' => {
                    let esc_pos = self.pos - 1;
                    let Some(esc) = self.peek_char() else {
                        return fail(start, "Unterminated string.");
                    };
                    self.pos += esc.len_utf8();
                    match esc {
                        'n' => out.push('\n'),
                        't' => out.push('\t'),
                        'r' => out.push('\r'),
                        'b' => out.push('\u{8}'),
                        'f' => out.push('\u{c}'),
                        'v' => out.push('\u{b}'),
                        '0' => out.push('\0'),
                        '\\' | '"' | '\'' | '/' => out.push(esc),
                        '\n' => {}
                        'x' => {
                            let hex = self.take_hex(2, esc_pos)?;
                            out.push(char::from_u32(hex).unwrap_or('\u{fffd}'));
                        }
                        'u' => {
                            let unit = self.take_hex(4, esc_pos)?;
                            if (0xD800..0xDC00).contains(&unit) {
                                // High surrogate: needs a following \uDC00..DFFF.
                                if self.src[self.pos..].starts_with("\\u") {
                                    let save = self.pos;
                                    self.pos += 2;
                                    let low = self.take_hex(4, esc_pos)?;
                                    if (0xDC00..0xE000).contains(&low) {
                                        let code = 0x10000 + ((unit - 0xD800) << 10) + (low - 0xDC00);
                                        out.push(char::from_u32(code).unwrap_or('\u{fffd}'));
                                        continue;
                                    }
                                    self.pos = save;
                                }
                                out.push('\u{fffd}');
                            } else {
                                out.push(char::from_u32(unit).unwrap_or('\u{fffd}'));
                            }
                        }
                        other => out.push(other),
                    }
                }
                other => out.push(other),
            }
        }
    }

    fn take_hex(&mut self, count: usize, esc_pos: usize) -> R<u32> {
        let end = self.pos + count;
        let Some(digits) = self.src.get(self.pos..end) else {
            return fail(esc_pos, "Invalid escape sequence.");
        };
        match u32::from_str_radix(digits, 16) {
            Ok(value) if digits.bytes().all(|byte| byte.is_ascii_hexdigit()) => {
                self.pos = end;
                Ok(value)
            }
            _ => fail(esc_pos, "Invalid escape sequence."),
        }
    }

    fn parse_number(&mut self) -> R<Bson> {
        let start = self.pos;
        if matches!(self.peek(), Some(b'-') | Some(b'+')) {
            self.pos += 1;
        }
        // -Infinity / +Infinity
        if self.src[self.pos..].starts_with("Infinity") {
            self.pos += "Infinity".len();
            let negative = self.bytes[start] == b'-';
            return Ok(Bson::Double(if negative {
                f64::NEG_INFINITY
            } else {
                f64::INFINITY
            }));
        }
        let digits_start = self.pos;
        // Hexadecimal integers.
        if self.src[self.pos..].starts_with("0x") || self.src[self.pos..].starts_with("0X") {
            self.pos += 2;
            let hex_start = self.pos;
            while self.peek().is_some_and(|byte| byte.is_ascii_hexdigit()) {
                self.pos += 1;
            }
            let Ok(value) = i64::from_str_radix(&self.src[hex_start..self.pos], 16) else {
                return fail(start, "Invalid hexadecimal number.");
            };
            let value = if self.bytes[start] == b'-' { -value } else { value };
            return Ok(int_bson(value));
        }
        let mut is_float = false;
        while self.peek().is_some_and(|byte| byte.is_ascii_digit()) {
            self.pos += 1;
        }
        if self.peek() == Some(b'.') {
            is_float = true;
            self.pos += 1;
            while self.peek().is_some_and(|byte| byte.is_ascii_digit()) {
                self.pos += 1;
            }
        }
        if self.pos == digits_start || &self.src[digits_start..self.pos] == "." {
            return fail(start, "Invalid number.");
        }
        if matches!(self.peek(), Some(b'e') | Some(b'E')) {
            is_float = true;
            self.pos += 1;
            if matches!(self.peek(), Some(b'-') | Some(b'+')) {
                self.pos += 1;
            }
            let exp_start = self.pos;
            while self.peek().is_some_and(|byte| byte.is_ascii_digit()) {
                self.pos += 1;
            }
            if self.pos == exp_start {
                return fail(start, "Invalid number: missing exponent digits.");
            }
        }
        if self
            .peek_char()
            .is_some_and(|ch| ch.is_alphanumeric() || ch == '_' || ch == '$')
        {
            return fail(start, "Invalid number.");
        }
        let text = &self.src[start..self.pos];
        let text = text.strip_prefix('+').unwrap_or(text);
        if !is_float {
            if let Ok(value) = text.parse::<i64>() {
                return Ok(int_bson(value));
            }
        }
        match text.parse::<f64>() {
            Ok(value) => Ok(Bson::Double(value)),
            Err(_) => fail(start, "Invalid number."),
        }
    }

    fn parse_regex(&mut self) -> R<Bson> {
        let start = self.pos;
        self.pos += 1; // opening '/'
        let mut pattern = String::new();
        let mut in_class = false;
        loop {
            let Some(ch) = self.peek_char() else {
                return fail(start, "Unterminated regular expression.");
            };
            self.pos += ch.len_utf8();
            match ch {
                '\n' | '\r' => return fail(start, "Unterminated regular expression."),
                '\\' => {
                    match self.peek_char() {
                        // `\/` only exists to delimit the literal; PCRE reads `/`.
                        Some('/') => {
                            pattern.push('/');
                            self.pos += 1;
                        }
                        Some(next) if next != '\n' && next != '\r' => {
                            pattern.push('\\');
                            pattern.push(next);
                            self.pos += next.len_utf8();
                        }
                        _ => return fail(start, "Unterminated regular expression."),
                    }
                }
                '[' => {
                    in_class = true;
                    pattern.push(ch);
                }
                ']' => {
                    in_class = false;
                    pattern.push(ch);
                }
                '/' if !in_class => break,
                other => pattern.push(other),
            }
        }
        let flags_start = self.pos;
        while self.peek().is_some_and(|byte| byte.is_ascii_alphabetic()) {
            self.pos += 1;
        }
        let flags = &self.src[flags_start..self.pos];
        regex_bson(pattern, flags).or_else(|message| fail(start, message))
    }

    /// A bare word in value position: literal, number keyword or constructor.
    fn parse_word(&mut self, depth: usize, start: usize) -> R<Bson> {
        let word = self.parse_ident();
        if word.is_empty() {
            return fail(start, format!("Unexpected {}.", self.describe_here()));
        }
        match word.as_str() {
            "true" => return Ok(Bson::Boolean(true)),
            "false" => return Ok(Bson::Boolean(false)),
            "null" | "undefined" => return Ok(Bson::Null),
            "Infinity" => return Ok(Bson::Double(f64::INFINITY)),
            "NaN" => return Ok(Bson::Double(f64::NAN)),
            _ => {}
        }
        let mut name = word;
        let mut is_new = false;
        if name == "new" {
            self.skip_ws();
            let ctor_start = self.pos;
            if !self.peek().is_some_and(is_ident_start) {
                return fail(ctor_start, "Expected a constructor name after 'new'.");
            }
            name = self.parse_ident();
            is_new = true;
        }
        self.skip_ws();
        if self.peek() != Some(b'(') {
            return if is_new {
                // `new Date` without parentheses is valid JS.
                parse_constructor(&name, Vec::new(), start)
            } else {
                fail(start, format!("Unknown identifier '{name}'."))
            };
        }
        self.pos += 1;
        let mut args: Vec<(Bson, usize)> = Vec::new();
        loop {
            self.skip_ws();
            match self.peek() {
                Some(b')') => {
                    self.pos += 1;
                    break;
                }
                None => return fail(self.pos, format!("Unterminated call to {name}(…): expected ')'.")),
                _ => {}
            }
            let arg_start = self.pos;
            let value = self.parse_value(depth + 1)?;
            args.push((value, arg_start));
            self.skip_ws();
            match self.peek() {
                Some(b',') => self.pos += 1,
                Some(b')') => {}
                _ => {
                    return fail(
                        self.pos,
                        format!("Expected ',' or ')' but found {}.", self.describe_here()),
                    )
                }
            }
        }
        parse_constructor(&name, args, start)
    }
}

fn is_ident_start(byte: u8) -> bool {
    byte.is_ascii_alphabetic() || byte == b'_' || byte == b'$'
}

fn int_bson(value: i64) -> Bson {
    match i32::try_from(value) {
        Ok(small) => Bson::Int32(small),
        Err(_) => Bson::Int64(value),
    }
}

fn regex_bson(pattern: String, flags: &str) -> Result<Bson, String> {
    let mut normalized: Vec<char> = Vec::new();
    for flag in flags.chars() {
        match flag {
            // BSON regex options: i m s x l u (JS-only g / y / d are dropped).
            'i' | 'm' | 's' | 'x' | 'l' | 'u' => {
                if !normalized.contains(&flag) {
                    normalized.push(flag);
                }
            }
            'g' | 'y' | 'd' => {}
            other => return Err(format!("Invalid regular expression flag '{other}'.")),
        }
    }
    normalized.sort_unstable();
    Ok(Bson::RegularExpression(bson::Regex {
        pattern,
        options: normalized.into_iter().collect(),
    }))
}

/// Shell constructors. `start` is the byte offset of the whole expression
/// (for error positions); arguments carry their own offsets.
fn parse_constructor(name: &str, args: Vec<(Bson, usize)>, start: usize) -> R<Bson> {
    let arity = |min: usize, max: usize| -> R<()> {
        if args.len() < min || args.len() > max {
            let expected = if min == max {
                format!("{min}")
            } else {
                format!("{min} to {max}")
            };
            fail(
                start,
                format!("{name}() takes {expected} argument(s), got {}.", args.len()),
            )
        } else {
            Ok(())
        }
    };
    let string_arg = |index: usize| -> R<&str> {
        match &args[index] {
            (Bson::String(text), _) => Ok(text.as_str()),
            (_, pos) => fail(*pos, format!("{name}() expects a string argument.")),
        }
    };
    let integer_arg = |index: usize| -> R<i64> {
        match &args[index] {
            (Bson::Int32(value), _) => Ok(i64::from(*value)),
            (Bson::Int64(value), _) => Ok(*value),
            (Bson::Double(value), pos) => {
                if value.is_finite() && value.fract() == 0.0 && value.abs() < 9.0e18 {
                    Ok(*value as i64)
                } else {
                    fail(*pos, format!("{name}() expects an integer."))
                }
            }
            (Bson::String(text), pos) => text
                .trim()
                .parse::<i64>()
                .or_else(|_| fail(*pos, format!("{name}() expects an integer, got \"{text}\"."))),
            (_, pos) => fail(*pos, format!("{name}() expects a number.")),
        }
    };
    match name {
        "ObjectId" => {
            arity(0, 1)?;
            if args.is_empty() {
                return Ok(Bson::ObjectId(bson::oid::ObjectId::new()));
            }
            let hex = string_arg(0)?;
            bson::oid::ObjectId::parse_str(hex)
                .map(Bson::ObjectId)
                .or_else(|_| {
                    fail(
                        args[0].1,
                        format!("Invalid ObjectId \"{hex}\": expected 24 hexadecimal characters."),
                    )
                })
        }
        "ISODate" | "Date" => {
            if args.is_empty() {
                return Ok(Bson::DateTime(bson::DateTime::now()));
            }
            if args.len() == 1 {
                return match &args[0].0 {
                    Bson::String(text) => match parse_date_string(text) {
                        Some(date) => Ok(Bson::DateTime(date)),
                        None => fail(args[0].1, format!("Invalid date \"{text}\".")),
                    },
                    Bson::DateTime(date) => Ok(Bson::DateTime(*date)),
                    _ => integer_arg(0).map(|ms| Bson::DateTime(bson::DateTime::from_millis(ms))),
                };
            }
            arity(2, 7)?;
            let mut parts = [0i64, 0, 1, 0, 0, 0, 0];
            for (index, part) in parts.iter_mut().enumerate().take(args.len()) {
                *part = integer_arg(index)?;
            }
            let date = chrono::NaiveDate::from_ymd_opt(
                i32::try_from(parts[0]).unwrap_or(i32::MAX),
                parts[1]
                    .checked_add(1)
                    .and_then(|month| u32::try_from(month).ok())
                    .unwrap_or(0),
                u32::try_from(parts[2]).unwrap_or(0),
            )
            .and_then(|date| {
                date.and_hms_milli_opt(
                    u32::try_from(parts[3]).ok()?,
                    u32::try_from(parts[4]).ok()?,
                    u32::try_from(parts[5]).ok()?,
                    u32::try_from(parts[6]).ok()?,
                )
            });
            match date {
                Some(naive) => Ok(Bson::DateTime(bson::DateTime::from_millis(
                    naive.and_utc().timestamp_millis(),
                ))),
                None => fail(start, "Invalid date components."),
            }
        }
        "NumberLong" | "Long" => {
            arity(1, 1)?;
            integer_arg(0).map(Bson::Int64)
        }
        "NumberInt" | "Int32" => {
            arity(1, 1)?;
            let value = integer_arg(0)?;
            i32::try_from(value)
                .map(Bson::Int32)
                .or_else(|_| fail(args[0].1, format!("{name}() value {value} is out of range.")))
        }
        "Double" | "NumberDouble" => {
            arity(1, 1)?;
            match &args[0].0 {
                Bson::Int32(value) => Ok(Bson::Double(f64::from(*value))),
                Bson::Int64(value) => Ok(Bson::Double(*value as f64)),
                Bson::Double(value) => Ok(Bson::Double(*value)),
                Bson::String(text) => text
                    .trim()
                    .parse::<f64>()
                    .map(Bson::Double)
                    .or_else(|_| fail(args[0].1, format!("{name}() expects a number."))),
                _ => fail(args[0].1, format!("{name}() expects a number.")),
            }
        }
        "NumberDecimal" | "Decimal128" => {
            arity(1, 1)?;
            let text = match &args[0].0 {
                Bson::String(text) => text.clone(),
                Bson::Int32(value) => value.to_string(),
                Bson::Int64(value) => value.to_string(),
                Bson::Double(value) => value.to_string(),
                _ => return fail(args[0].1, format!("{name}() expects a string.")),
            };
            text.parse::<bson::Decimal128>()
                .map(Bson::Decimal128)
                .or_else(|_| fail(args[0].1, format!("Invalid decimal \"{text}\".")))
        }
        "Timestamp" => {
            arity(2, 2)?;
            let time = integer_arg(0)?;
            let increment = integer_arg(1)?;
            match (u32::try_from(time), u32::try_from(increment)) {
                (Ok(time), Ok(increment)) => Ok(Bson::Timestamp(bson::Timestamp { time, increment })),
                _ => fail(start, "Timestamp() values must fit in an unsigned 32-bit integer."),
            }
        }
        "BinData" => {
            arity(2, 2)?;
            let subtype = integer_arg(0)?;
            let subtype = u8::try_from(subtype)
                .or_else(|_| fail(args[0].1, "BinData() subtype must be between 0 and 255."))?;
            let data = string_arg(1)?;
            bson::Binary::from_base64(data, bson::spec::BinarySubtype::from(subtype))
                .map(Bson::Binary)
                .or_else(|_| fail(args[1].1, "BinData() expects base64 data."))
        }
        "UUID" => {
            arity(1, 1)?;
            let text = string_arg(0)?;
            let hex: String = text.chars().filter(|ch| *ch != '-').collect();
            if hex.len() != 32 || !hex.bytes().all(|byte| byte.is_ascii_hexdigit()) {
                return fail(args[0].1, format!("Invalid UUID \"{text}\"."));
            }
            let mut bytes = Vec::with_capacity(16);
            for index in 0..16 {
                bytes.push(u8::from_str_radix(&hex[index * 2..index * 2 + 2], 16).unwrap_or(0));
            }
            Ok(Bson::Binary(bson::Binary {
                subtype: bson::spec::BinarySubtype::Uuid,
                bytes,
            }))
        }
        "MinKey" => {
            arity(0, 0)?;
            Ok(Bson::MinKey)
        }
        "MaxKey" => {
            arity(0, 0)?;
            Ok(Bson::MaxKey)
        }
        "RegExp" => {
            arity(1, 2)?;
            let pattern = string_arg(0)?.to_string();
            let flags = if args.len() > 1 { string_arg(1)?.to_string() } else { String::new() };
            regex_bson(pattern, &flags).or_else(|message| fail(start, message))
        }
        other => fail(start, format!("Unknown function or constructor '{other}'.")),
    }
}

fn parse_date_string(text: &str) -> Option<bson::DateTime> {
    let text = text.trim();
    if let Ok(date) = bson::DateTime::parse_rfc3339_str(text) {
        return Some(date);
    }
    // Shell forms without a zone are UTC (ISODate semantics).
    for format in [
        "%Y-%m-%dT%H:%M:%S%.f",
        "%Y-%m-%d %H:%M:%S%.f",
        "%Y-%m-%dT%H:%M",
        "%Y-%m-%d %H:%M",
    ] {
        if let Ok(naive) = chrono::NaiveDateTime::parse_from_str(text, format) {
            return Some(bson::DateTime::from_millis(naive.and_utc().timestamp_millis()));
        }
    }
    chrono::NaiveDate::parse_from_str(text, "%Y-%m-%d")
        .ok()
        .and_then(|date| date.and_hms_opt(0, 0, 0))
        .map(|naive| bson::DateTime::from_millis(naive.and_utc().timestamp_millis()))
}

/// Converts Extended JSON wrapper objects. `None` = not a wrapper.
fn extended_json(document: &Document) -> Option<Result<Bson, String>> {
    let (key, value) = document.iter().next()?;
    let single = document.len() == 1;
    match (key.as_str(), value) {
        ("$oid", Bson::String(hex)) if single => Some(
            bson::oid::ObjectId::parse_str(hex)
                .map(Bson::ObjectId)
                .map_err(|_| format!("Invalid $oid \"{hex}\".")),
        ),
        ("$date", value) if single => Some(match value {
            Bson::String(text) => parse_date_string(text)
                .map(Bson::DateTime)
                .ok_or_else(|| format!("Invalid $date \"{text}\".")),
            Bson::Int32(ms) => Ok(Bson::DateTime(bson::DateTime::from_millis(i64::from(*ms)))),
            Bson::Int64(ms) => Ok(Bson::DateTime(bson::DateTime::from_millis(*ms))),
            Bson::Document(inner) => match inner.get("$numberLong") {
                Some(Bson::String(ms)) => ms
                    .parse::<i64>()
                    .map(|ms| Bson::DateTime(bson::DateTime::from_millis(ms)))
                    .map_err(|_| format!("Invalid $date \"{ms}\".")),
                _ => Err("Invalid $date value.".to_string()),
            },
            _ => Err("Invalid $date value.".to_string()),
        }),
        ("$numberLong", Bson::String(text)) if single => Some(
            text.parse::<i64>()
                .map(Bson::Int64)
                .map_err(|_| format!("Invalid $numberLong \"{text}\".")),
        ),
        ("$numberInt", Bson::String(text)) if single => Some(
            text.parse::<i32>()
                .map(Bson::Int32)
                .map_err(|_| format!("Invalid $numberInt \"{text}\".")),
        ),
        ("$numberDouble", Bson::String(text)) if single => Some(match text.as_str() {
            "NaN" => Ok(Bson::Double(f64::NAN)),
            "Infinity" => Ok(Bson::Double(f64::INFINITY)),
            "-Infinity" => Ok(Bson::Double(f64::NEG_INFINITY)),
            other => other
                .parse::<f64>()
                .map(Bson::Double)
                .map_err(|_| format!("Invalid $numberDouble \"{other}\".")),
        }),
        ("$numberDecimal", Bson::String(text)) if single => Some(
            text.parse::<bson::Decimal128>()
                .map(Bson::Decimal128)
                .map_err(|_| format!("Invalid $numberDecimal \"{text}\".")),
        ),
        ("$minKey", _) if single => Some(Ok(Bson::MinKey)),
        ("$maxKey", _) if single => Some(Ok(Bson::MaxKey)),
        ("$timestamp", Bson::Document(inner)) if single => {
            let read = |name: &str| match inner.get(name) {
                Some(Bson::Int32(value)) => u32::try_from(*value).ok(),
                Some(Bson::Int64(value)) => u32::try_from(*value).ok(),
                _ => None,
            };
            Some(match (read("t"), read("i")) {
                (Some(time), Some(increment)) => {
                    Ok(Bson::Timestamp(bson::Timestamp { time, increment }))
                }
                _ => Err("Invalid $timestamp: expected { t, i } unsigned integers.".to_string()),
            })
        }
        ("$regularExpression", Bson::Document(inner)) if single => {
            let pattern = inner.get_str("pattern").ok();
            let options = inner.get_str("options").unwrap_or("");
            Some(match pattern {
                Some(pattern) => regex_bson(pattern.to_string(), options),
                None => Err("Invalid $regularExpression: expected { pattern, options }.".to_string()),
            })
        }
        ("$uuid", Bson::String(text)) if single => {
            let hex: String = text.chars().filter(|ch| *ch != '-').collect();
            Some(if hex.len() == 32 && hex.bytes().all(|byte| byte.is_ascii_hexdigit()) {
                let bytes = (0..16)
                    .map(|index| u8::from_str_radix(&hex[index * 2..index * 2 + 2], 16).unwrap_or(0))
                    .collect();
                Ok(Bson::Binary(bson::Binary {
                    subtype: bson::spec::BinarySubtype::Uuid,
                    bytes,
                }))
            } else {
                Err(format!("Invalid $uuid \"{text}\"."))
            })
        }
        ("$binary", Bson::Document(inner)) if single => {
            let data = inner.get_str("base64").ok();
            let subtype = inner
                .get_str("subType")
                .ok()
                .and_then(|text| u8::from_str_radix(text, 16).ok());
            Some(match (data, subtype) {
                (Some(data), Some(subtype)) => {
                    bson::Binary::from_base64(data, bson::spec::BinarySubtype::from(subtype))
                        .map(Bson::Binary)
                        .map_err(|_| "Invalid $binary base64 data.".to_string())
                }
                _ => Err("Invalid $binary: expected { base64, subType }.".to_string()),
            })
        }
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use bson::doc;

    fn stage(text: &str) -> Document {
        parse_stage(text).unwrap_or_else(|error| panic!("{text}: {error}"))
    }

    #[test]
    fn parses_plain_json_pipeline_preserving_key_order() {
        let parsed = parse_pipeline(
            r#"[{"$sort": {"zeta": 1, "alpha": -1, "mid": 1}}, {"$group": {"_id": "$k", "n": {"$sum": 1}}}]"#,
        )
        .unwrap();
        assert_eq!(parsed.stages.len(), 2);
        let sort = parsed.stages[0].get_document("$sort").unwrap();
        assert_eq!(
            sort.keys().map(String::as_str).collect::<Vec<_>>(),
            ["zeta", "alpha", "mid"],
            "key order must be exactly as written"
        );
    }

    #[test]
    fn parses_shell_syntax_constructors() {
        let doc = stage(
            r#"{ $match: {
                _id: ObjectId("507f191e810c19729de860ea"),
                at: ISODate("2024-01-15T10:30:00Z"),
                d2: new Date("2024-02-01"),
                d3: new Date(1700000000000),
                big: NumberLong("9007199254740993"),
                small: NumberInt(5),
                price: NumberDecimal("12.50"),
                name: /^ab.c$/i,
                ts: Timestamp(10, 2),
                u: UUID("123e4567-e89b-12d3-a456-426614174000"),
                lo: MinKey(), hi: MaxKey(),
            } }"#,
        );
        let m = doc.get_document("$match").unwrap();
        assert!(matches!(m.get("_id"), Some(Bson::ObjectId(_))));
        assert_eq!(
            m.get_datetime("at").unwrap().try_to_rfc3339_string().unwrap(),
            "2024-01-15T10:30:00Z"
        );
        assert_eq!(
            m.get_datetime("d2").unwrap().timestamp_millis(),
            1_706_745_600_000
        );
        assert_eq!(m.get_datetime("d3").unwrap().timestamp_millis(), 1_700_000_000_000);
        assert_eq!(m.get("big"), Some(&Bson::Int64(9007199254740993)));
        assert_eq!(m.get("small"), Some(&Bson::Int32(5)));
        assert!(matches!(m.get("price"), Some(Bson::Decimal128(_))));
        match m.get("name") {
            Some(Bson::RegularExpression(re)) => {
                assert_eq!(re.pattern, "^ab.c$");
                assert_eq!(re.options, "i");
            }
            other => panic!("regex expected, got {other:?}"),
        }
        assert_eq!(m.get("ts"), Some(&Bson::Timestamp(bson::Timestamp { time: 10, increment: 2 })));
        assert!(matches!(m.get("u"), Some(Bson::Binary(b)) if b.subtype == bson::spec::BinarySubtype::Uuid));
        assert_eq!(m.get("lo"), Some(&Bson::MinKey));
        assert_eq!(m.get("hi"), Some(&Bson::MaxKey));
    }

    #[test]
    fn single_quotes_unquoted_keys_comments_and_trailing_commas() {
        let doc = stage(
            "{ // line comment\n  $match: { 'status': 'a\\'b', \"x\": 1, /* inline */ y: [1, 2,], }, }",
        );
        let m = doc.get_document("$match").unwrap();
        assert_eq!(m.get_str("status").unwrap(), "a'b");
        assert_eq!(m.get_array("y").unwrap().len(), 2);
    }

    #[test]
    fn integers_map_to_int32_when_they_fit_else_int64_and_doubles() {
        let doc = stage("{ $limit: 5, a: 2147483647, b: 2147483648, c: -2147483648, d: -2147483649, e: 1.5, f: 2.0, g: 1e3, h: 0x10 }");
        assert_eq!(doc.get("$limit"), Some(&Bson::Int32(5)));
        assert_eq!(doc.get("a"), Some(&Bson::Int32(i32::MAX)));
        assert_eq!(doc.get("b"), Some(&Bson::Int64(2147483648)));
        assert_eq!(doc.get("c"), Some(&Bson::Int32(i32::MIN)));
        assert_eq!(doc.get("d"), Some(&Bson::Int64(-2147483649)));
        assert_eq!(doc.get("e"), Some(&Bson::Double(1.5)));
        assert_eq!(doc.get("f"), Some(&Bson::Double(2.0)));
        assert_eq!(doc.get("g"), Some(&Bson::Double(1000.0)));
        assert_eq!(doc.get("h"), Some(&Bson::Int32(16)));
    }

    #[test]
    fn extended_json_wrappers_convert() {
        let doc = stage(
            r#"{ a: {"$oid": "507f191e810c19729de860ea"}, b: {"$date": "2024-01-15T10:30:00Z"}, c: {"$numberLong": "7"}, d: {"$numberDecimal": "1.5"}, e: {"$date": {"$numberLong": "1700000000000"}} }"#,
        );
        assert!(matches!(doc.get("a"), Some(Bson::ObjectId(_))));
        assert!(matches!(doc.get("b"), Some(Bson::DateTime(_))));
        assert_eq!(doc.get("c"), Some(&Bson::Int64(7)));
        assert!(matches!(doc.get("d"), Some(Bson::Decimal128(_))));
        assert_eq!(doc.get_datetime("e").unwrap().timestamp_millis(), 1_700_000_000_000);
    }

    #[test]
    fn operators_that_look_like_wrappers_stay_documents() {
        // $regex/$options and unknown $-keys are ordinary query operators.
        let doc = stage(r#"{ $match: { n: { $regex: "^a", $options: "i" }, d: { $date: 5, other: 1 } } }"#);
        let m = doc.get_document("$match").unwrap();
        assert!(m.get_document("n").is_ok());
        assert!(m.get_document("d").is_ok(), "two-key $date object is not a wrapper");
    }

    #[test]
    fn errors_carry_line_col_and_stage() {
        let text = "[\n  { $match: { a: 1 } },\n  { $project: { b: } }\n]";
        let error = parse_pipeline(text).unwrap_err();
        assert_eq!(error.stage, Some(2));
        assert_eq!(error.line, 3);
        assert_eq!(error.col, 20, "{error}");
        assert!(error.message.contains("Unexpected"), "{error}");
        assert!(error.to_string().starts_with("Stage 2 (line 3, col 20)"));
    }

    #[test]
    fn error_cases_are_reported_not_swallowed() {
        for (text, needle) in [
            ("", "empty"),
            ("   \n ", "empty"),
            ("{ $match: {} }", "must be an array"),
            ("[{ $match: {} }", "Unterminated pipeline"),
            ("[1]", "Stage 1 must be an object"),
            ("[{ $match: { a: 1 } } { $limit: 1 }]", "Expected ',' or ']'"),
            ("[{ $match: { a: ObjectId('zz') } }]", "Invalid ObjectId"),
            ("[{ $match: { a: NumberInt(\"x\") } }]", "expects an integer"),
            ("[{ $match: { a: foo } }]", "Unknown identifier 'foo'"),
            ("[{ $match: { a: Foo(1) } }]", "Unknown function"),
            ("[{ $match: { a: 'abc } }]", "Unterminated string"),
            ("[{ $match: { a: /abc } }]", "Unterminated regular"),
            ("[{ $match: { a: 12abc } }]", "Invalid number"),
            ("[{ $match: {} }] extra", "after the end"),
            ("[{ a: ISODate('nope') }]", "Invalid date"),
            ("[{ a: 1 b: 2 }]", "Expected ',' or '}'"),
        ] {
            let error = parse_pipeline(text).expect_err(text);
            assert!(
                error.message.contains(needle),
                "{text:?}: expected {needle:?} in {:?}",
                error.message
            );
            assert!(error.line >= 1 && error.col >= 1, "{text:?}: {error:?}");
        }
    }

    #[test]
    fn date_components_at_the_integer_limits_are_errors_not_overflows() {
        for text in [
            "[{ a: new Date(2024, 9223372036854775807, 1) }]",
            "[{ a: new Date(2024, -9223372036854775807, 1) }]",
            "[{ a: new Date(99999999999999, 1, 1) }]",
            "[{ a: new Date(2024, 0, 1, -1) }]",
        ] {
            let error = parse_pipeline(text).expect_err(text);
            assert!(error.message.contains("Invalid date"), "{text}: {error}");
        }
    }

    #[test]
    fn deep_nesting_is_rejected_without_overflowing_the_stack() {
        let text = format!("[{}{}]", "{a:".repeat(500), "}".repeat(500));
        let error = parse_pipeline(&text).unwrap_err();
        assert!(error.message.contains("too deep"));
    }

    #[test]
    fn unicode_columns_count_characters() {
        let text = "[{ $match: { \"héllo\": ¿ } }]";
        let error = parse_pipeline(text).unwrap_err();
        assert_eq!(error.line, 1);
        assert_eq!(error.col, 23);
    }

    #[test]
    fn positions_point_at_each_stage() {
        let parsed = parse_pipeline("[\n  {$match: {}},\n  {$limit: 1}\n]").unwrap();
        assert_eq!(parsed.positions, vec![(2, 3), (3, 3)]);
    }

    #[test]
    fn empty_pipeline_array_is_valid() {
        let parsed = parse_pipeline(" [ ] ").unwrap();
        assert!(parsed.stages.is_empty());
    }

    #[test]
    fn string_escapes_decode() {
        let doc = stage(r#"{ a: "x\ny\té😀\x41" }"#);
        assert_eq!(doc.get_str("a").unwrap(), "x\ny\té😀A");
    }

    #[test]
    fn matches_the_legacy_document_shape() {
        let doc = stage("{ $match: { a: { $gt: 1 } } }");
        assert_eq!(doc, doc! { "$match": { "a": { "$gt": 1 } } });
    }
}
