//! A streaming reader of JSON rows: a top-level array of objects, or one object per line (NDJSON, or
//! any run of objects separated by whitespace).
//!
//! The array form is turned into the second by a small byte filter (the brackets and commas that
//! belong to the outer array become blanks), so both are read by `serde_json`'s stream deserializer
//! one value at a time and the file is never held in memory. An object keeps the order of its keys, and a number
//! keeps the digits it was written with (it is read as raw text, never through a double). One value larger than
//! [`MAX_ELEMENT_BYTES`] stops the read.

use std::io::{self, BufRead, Read};

use serde::de::{self, IgnoredAny, MapAccess, SeqAccess, Visitor};
use serde_json::de::IoRead;
use serde_json::value::RawValue;
use serde_json::{Deserializer, StreamDeserializer};

/// The most one JSON value of the file may take: a value that is not closed would otherwise be read to the end.
pub const MAX_ELEMENT_BYTES: usize = 64 * 1024 * 1024;

/// One row: its 1-based position among the values of the file and its cells in key order. A cell is
/// the value's text (`None` for JSON `null`); an array or object value is its JSON text.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct JsonRow {
    pub index: u64,
    pub fields: Vec<(String, Option<String>)>,
}

/// A value of the file that is not an object.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NotAnObject {
    pub index: u64,
    pub found: &'static str,
}

#[derive(Debug, PartialEq, Eq)]
pub enum JsonNext {
    Row(JsonRow),
    /// The value is valid JSON but not an object: the next one is still readable.
    Bad(NotAnObject),
    End,
}

/// What stops the read of the rest of the file.
#[derive(Debug)]
pub enum JsonError {
    Io(io::Error),
    /// A syntax error: the stream cannot be read past it.
    Syntax { line: usize, column: usize, message: String },
    /// The whole file is one object whose only value is an array of objects (`{"data": [...]}`).
    Wrapped { key: String },
}

impl std::fmt::Display for JsonError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Io(error) => write!(f, "The file could not be read: {error}"),
            Self::Syntax { line, column, message } => write!(f, "Invalid JSON at line {line}, column {column}: {message}"),
            Self::Wrapped { key } => write!(
                f,
                "The file is one object that holds its rows in \"{key}\". Import needs an array of objects, or one object per line: take the array out of the file first"
            ),
        }
    }
}

impl std::error::Error for JsonError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Io(error) => Some(error),
            Self::Syntax { .. } | Self::Wrapped { .. } => None,
        }
    }
}

/// A JSON value as a row sees it: an object with its keys in file order, or the name of what else it is.
enum Element {
    Object(Vec<(String, Option<String>)>),
    Other(&'static str),
}

/// The text a JSON value is imported as: `None` for `null`, a string unescaped, anything else (a number, a boolean,
/// an array or an object) exactly as it was written.
fn cell_text(raw: &RawValue) -> Option<String> {
    let text = raw.get().trim();
    match text.as_bytes().first() {
        Some(b'n') => None,
        Some(b'"') => serde_json::from_str::<String>(text).ok(),
        _ => Some(text.to_owned()),
    }
}

struct ElementVisitor;

impl<'de> Visitor<'de> for ElementVisitor {
    type Value = Element;

    fn expecting(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("a JSON value")
    }

    fn visit_map<A: MapAccess<'de>>(self, mut map: A) -> Result<Element, A::Error> {
        let mut fields: Vec<(String, Option<String>)> = Vec::new();
        while let Some(key) = map.next_key::<String>()? {
            let value = cell_text(&map.next_value::<Box<RawValue>>()?);
            match fields.iter_mut().find(|(existing, _)| *existing == key) {
                // The last of a repeated key wins, in the place of the first.
                Some(slot) => slot.1 = value,
                None => fields.push((key, value)),
            }
        }
        Ok(Element::Object(fields))
    }

    fn visit_seq<A: SeqAccess<'de>>(self, mut seq: A) -> Result<Element, A::Error> {
        while seq.next_element::<IgnoredAny>()?.is_some() {}
        Ok(Element::Other("an array"))
    }

    fn visit_str<E: de::Error>(self, _: &str) -> Result<Element, E> {
        Ok(Element::Other("a string"))
    }

    fn visit_bool<E: de::Error>(self, _: bool) -> Result<Element, E> {
        Ok(Element::Other("a boolean"))
    }

    fn visit_i64<E: de::Error>(self, _: i64) -> Result<Element, E> {
        Ok(Element::Other("a number"))
    }

    fn visit_u64<E: de::Error>(self, _: u64) -> Result<Element, E> {
        Ok(Element::Other("a number"))
    }

    fn visit_f64<E: de::Error>(self, _: f64) -> Result<Element, E> {
        Ok(Element::Other("a number"))
    }

    fn visit_unit<E: de::Error>(self) -> Result<Element, E> {
        Ok(Element::Other("null"))
    }
}

impl<'de> serde::Deserialize<'de> for Element {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        deserializer.deserialize_any(ElementVisitor)
    }
}

/// Passes the bytes of the file through, turning the brackets and commas of an outer array into blanks when the
/// file is one (so its elements read as a run of values), and counting how long the value being read has been.
struct Flatten<R> {
    inner: R,
    array: bool,
    depth: u32,
    in_string: bool,
    escaped: bool,
    /// Bytes since the value being read began.
    since: usize,
    limit: usize,
}

impl<R: Read> Read for Flatten<R> {
    fn read(&mut self, target: &mut [u8]) -> io::Result<usize> {
        let read = self.inner.read(target)?;
        let base = u32::from(self.array);
        for byte in &mut target[..read] {
            if self.in_string {
                match *byte {
                    _ if self.escaped => self.escaped = false,
                    b'\\' => self.escaped = true,
                    b'"' => self.in_string = false,
                    _ => {}
                }
                self.since += 1;
            } else {
                match *byte {
                    b'"' => self.in_string = true,
                    b'[' | b'{' => {
                        self.depth += 1;
                        if self.array && self.depth == 1 {
                            *byte = b' ';
                        }
                    }
                    b']' | b'}' => {
                        if self.array && self.depth == 1 {
                            *byte = b' ';
                        }
                        self.depth = self.depth.saturating_sub(1);
                    }
                    b',' if self.array && self.depth == 1 => *byte = b' ',
                    _ => {}
                }
                // A value ends where the depth is back at the file's own level and a separator follows.
                self.since = if self.depth <= base && (byte.is_ascii_whitespace() || matches!(*byte, b'}' | b']' | b',')) { 0 } else { self.since + 1 };
            }
            if self.since > self.limit {
                return Err(io::Error::new(io::ErrorKind::InvalidData, format!("a JSON value is larger than {} MiB", self.limit / (1024 * 1024))));
            }
        }
        Ok(read)
    }
}

pub struct JsonReader<R: Read> {
    stream: StreamDeserializer<'static, IoRead<Flatten<R>>, Element>,
    index: u64,
    failed: bool,
    /// The second value, read ahead to see that the first was the only one.
    ahead: Option<Result<Element, serde_json::Error>>,
}

impl<R: BufRead> JsonReader<R> {
    pub fn new(reader: R) -> io::Result<Self> {
        Self::with_limit(reader, MAX_ELEMENT_BYTES)
    }

    pub fn with_limit(mut reader: R, limit: usize) -> io::Result<Self> {
        // Whitespace before the first value is dropped to see whether the file is an array.
        let array = loop {
            let buffer = reader.fill_buf()?;
            let Some(&first) = buffer.first() else { break false };
            if first.is_ascii_whitespace() {
                reader.consume(1);
            } else {
                break first == b'[';
            }
        };
        let flatten = Flatten { inner: reader, array, depth: 0, in_string: false, escaped: false, since: 0, limit };
        Ok(Self { stream: Deserializer::from_reader(flatten).into_iter::<Element>(), index: 0, failed: false, ahead: None })
    }

    pub fn next_row(&mut self) -> Result<JsonNext, JsonError> {
        if self.failed {
            return Ok(JsonNext::End);
        }
        let next = self.ahead.take().or_else(|| self.stream.next());
        match next {
            None => Ok(JsonNext::End),
            Some(Ok(element)) => {
                self.index += 1;
                Ok(match element {
                    Element::Object(fields) => {
                        if self.index == 1 {
                            self.refuse_a_wrapper(&fields)?;
                        }
                        JsonNext::Row(JsonRow { index: self.index, fields })
                    }
                    Element::Other(found) => JsonNext::Bad(NotAnObject { index: self.index, found }),
                })
            }
            Some(Err(error)) => {
                self.failed = true;
                Err(if error.is_io() {
                    JsonError::Io(error.into())
                } else {
                    JsonError::Syntax { line: error.line(), column: error.column(), message: error.to_string() }
                })
            }
        }
    }

    /// A file that is one object with one array of objects in it (`{"data": [{...}]}`) is not rows: say so.
    fn refuse_a_wrapper(&mut self, fields: &[(String, Option<String>)]) -> Result<(), JsonError> {
        let [(key, Some(text))] = fields else { return Ok(()) };
        let mut inner = text.trim_start().strip_prefix('[').unwrap_or("").trim_start().chars();
        if inner.next() != Some('{') {
            return Ok(());
        }
        self.ahead = Some(match self.stream.next() {
            None => return Err(self.wrapped(key)),
            Some(other) => other,
        });
        Ok(())
    }

    fn wrapped(&mut self, key: &str) -> JsonError {
        self.failed = true;
        JsonError::Wrapped { key: key.to_owned() }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Cursor;

    fn rows_of(text: &str) -> Vec<Result<JsonNext, String>> {
        rows(text)
    }

    fn rows(text: &str) -> Vec<Result<JsonNext, String>> {
        let mut reader = JsonReader::new(Cursor::new(text.as_bytes().to_vec())).unwrap();
        let mut out = Vec::new();
        loop {
            match reader.next_row() {
                Ok(JsonNext::End) => return out,
                Ok(next) => out.push(Ok(next)),
                Err(error) => {
                    out.push(Err(error.to_string()));
                    return out;
                }
            }
        }
    }

    fn cells(next: &Result<JsonNext, String>) -> Vec<(String, Option<String>)> {
        match next {
            Ok(JsonNext::Row(row)) => row.fields.clone(),
            other => panic!("a row, got {other:?}"),
        }
    }

    fn pair(key: &str, value: Option<&str>) -> (String, Option<String>) {
        (key.to_owned(), value.map(str::to_owned))
    }

    #[test]
    fn an_array_of_objects_streams_row_by_row_in_key_order() {
        let rows = rows(" \n[ {\"z\": 1, \"a\": \"x\"},\n {\"a\": null, \"z\": 2.5, \"m\": true} ]\n");
        assert_eq!(rows.len(), 2);
        assert_eq!(cells(&rows[0]), [pair("z", Some("1")), pair("a", Some("x"))]);
        assert_eq!(cells(&rows[1]), [pair("a", None), pair("z", Some("2.5")), pair("m", Some("true"))]);
    }

    #[test]
    fn ndjson_and_whitespace_separated_objects_read_the_same() {
        for text in ["{\"a\":1}\n{\"a\":2}\n{\"a\":3}", "{\"a\":1} {\"a\":2}\r\n\r\n{\"a\":3}\n"] {
            let rows = rows(text);
            assert_eq!(rows.iter().map(|row| cells(row)[0].1.clone().unwrap()).collect::<Vec<_>>(), ["1", "2", "3"]);
        }
    }

    #[test]
    fn brackets_commas_and_quotes_inside_strings_and_nested_values_are_data() {
        let rows = rows("[{\"s\": \"a, [b] \\\"c\\\" }\", \"n\": {\"k\": [1, 2, {\"x\": \"]\"}]}, \"e\": []}]");
        assert_eq!(
            cells(&rows[0]),
            [pair("s", Some("a, [b] \"c\" }")), pair("n", Some("{\"k\": [1, 2, {\"x\": \"]\"}]}")), pair("e", Some("[]"))]
        );
    }

    #[test]
    fn a_value_that_is_not_an_object_is_reported_and_the_rest_still_reads() {
        let rows = rows("[{\"a\":1}, 5, [1,2], \"s\", null, {\"a\":2}]");
        assert_eq!(rows.len(), 6);
        assert!(matches!(&rows[1], Ok(JsonNext::Bad(NotAnObject { index: 2, found: "a number" }))));
        assert!(matches!(&rows[2], Ok(JsonNext::Bad(NotAnObject { found: "an array", .. }))));
        assert!(matches!(&rows[3], Ok(JsonNext::Bad(NotAnObject { found: "a string", .. }))));
        assert!(matches!(&rows[4], Ok(JsonNext::Bad(NotAnObject { found: "null", .. }))));
        assert_eq!(cells(&rows[5]), [pair("a", Some("2"))]);
    }

    #[test]
    fn a_repeated_key_keeps_the_last_value_in_the_first_place() {
        let rows = rows("[{\"a\":1,\"b\":2,\"a\":3}]");
        assert_eq!(cells(&rows[0]), [pair("a", Some("3")), pair("b", Some("2"))]);
    }

    #[test]
    fn a_syntax_error_ends_the_stream_with_its_position() {
        let rows = rows("[{\"a\":1},\n{\"a\": }]");
        assert_eq!(rows.len(), 2);
        let Err(message) = &rows[1] else { panic!("an error") };
        assert!(message.contains("line 2"), "{message}");
    }

    #[test]
    fn an_empty_file_or_an_empty_array_has_no_rows() {
        assert!(rows("").is_empty());
        assert!(rows("  [ ]  ").is_empty());
    }

    #[test]
    fn numbers_keep_every_digit_they_were_written_with() {
        let rows = rows("[{\"big\": 12345678901234567890123, \"exact\": 0.10000000000000000001, \"exp\": 1E+400, \"neg\": -0.0, \"i\": 7}]");
        assert_eq!(
            cells(&rows[0]),
            [
                pair("big", Some("12345678901234567890123")),
                pair("exact", Some("0.10000000000000000001")),
                pair("exp", Some("1E+400")),
                pair("neg", Some("-0.0")),
                pair("i", Some("7")),
            ]
        );
    }

    #[test]
    fn one_object_wrapping_the_array_is_refused_but_a_single_row_with_an_array_cell_is_a_row() {
        let rows = rows("{\"data\": [{\"id\": 1}, {\"id\": 2}]}");
        let Err(message) = &rows[0] else { panic!("a refusal, got {rows:?}") };
        assert!(message.contains("\"data\"") && message.contains("array of objects"), "{message}");
        // Several rows of one array-of-objects column are rows.
        let many = rows_of("{\"data\": [{\"id\": 1}]}\n{\"data\": [{\"id\": 2}]}");
        assert_eq!(many.len(), 2);
        assert_eq!(cells(&many[0]), [pair("data", Some("[{\"id\": 1}]"))]);
        // Arrays of plain values are cells.
        assert_eq!(cells(&rows_of("{\"tags\": [\"a\", \"b\"]}")[0]), [pair("tags", Some("[\"a\", \"b\"]"))]);
    }

    #[test]
    fn a_value_past_the_cap_stops_the_read_in_either_layout() {
        for text in [format!("[{{\"v\": \"{}\"}}]", "x".repeat(5000)), format!("{{\"v\": \"{}\"}}\n{{\"v\": 1}}", "x".repeat(5000))] {
            let mut reader = JsonReader::with_limit(Cursor::new(text.into_bytes()), 1024).unwrap();
            let Err(error) = reader.next_row() else { panic!("a refusal") };
            assert!(error.to_string().contains("larger than"), "{error}");
        }
        // Many small values add up to more than the cap without any one of them being large.
        let many = (0..2000).map(|id| format!("{{\"id\": {id}}}")).collect::<Vec<_>>().join(",\n");
        let mut reader = JsonReader::with_limit(Cursor::new(format!("[{many}]").into_bytes()), 1024).unwrap();
        let mut count = 0;
        while let JsonNext::Row(_) = reader.next_row().unwrap() {
            count += 1;
        }
        assert_eq!(count, 2000);
    }

    #[test]
    fn a_huge_string_value_reads() {
        let big = "z".repeat(2 * 1024 * 1024);
        let rows = rows(&format!("[{{\"v\":\"{big}\"}}]"));
        assert_eq!(cells(&rows[0])[0].1.as_ref().unwrap().len(), big.len());
    }

    #[test]
    fn unicode_escapes_and_non_ascii_text() {
        let rows = rows("{\"名前\":\"caf\\u00e9 \\ud83d\\ude00\"}");
        assert_eq!(cells(&rows[0]), [pair("名前", Some("café 😀"))]);
    }
}
