//! A streaming reader of delimited text (CSV, TSV, any one-character delimiter).
//!
//! It reads one record at a time from a buffered UTF-8 reader and keeps nothing else, so a file of any
//! size costs one record of memory. The dialect:
//!
//! * a field starts with the quote character to be quoted; inside a quoted field the delimiter and
//!   line breaks are text, a doubled quote is one quote, and (when an escape character other than the
//!   quote is set) the escape character followed by the quote or by itself is that character;
//! * a quote in the middle of an unquoted field is just text;
//! * records end at `\n`, `\r\n` or a lone `\r` (old Mac files); a line with nothing on it is not a record;
//! * a field longer than [`MAX_FIELD_BYTES`], or a record longer than [`MAX_RECORD_BYTES`], stops the read: it is
//!   a quote that never closes (or a file that is not delimited text), and would otherwise fill the memory.

use std::io::{self, BufRead};

/// The longest field read. A quoted field that never closes would otherwise swallow the rest of the file.
pub const MAX_FIELD_BYTES: usize = 16 * 1024 * 1024;

/// The most one record holds: its values, plus a fixed cost per value (a file of nothing but delimiters would
/// otherwise make an empty `String` of each).
pub const MAX_RECORD_BYTES: usize = 64 * 1024 * 1024;

/// What a value costs besides its bytes, in the record budget.
const FIELD_COST: usize = 32;

/// The characters that shape the records. All three are single ASCII characters.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DelimitedFormat {
    pub delimiter: u8,
    /// `None`: no quoting.
    pub quote: Option<u8>,
    /// `None`: the quote escapes itself (a doubled quote).
    pub escape: Option<u8>,
}

impl Default for DelimitedFormat {
    fn default() -> Self {
        Self { delimiter: b',', quote: Some(b'"'), escape: None }
    }
}

/// Why a format cannot be read with.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FormatError {
    /// The delimiter, quote or escape is a line break, not ASCII, or the same as another.
    Unusable(&'static str),
}

impl std::fmt::Display for FormatError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Unusable(reason) => f.write_str(reason),
        }
    }
}

impl std::error::Error for FormatError {}

impl DelimitedFormat {
    pub fn checked(self) -> Result<Self, FormatError> {
        let all = [Some(self.delimiter), self.quote, self.escape];
        if all.iter().flatten().any(|byte| !byte.is_ascii() || matches!(byte, b'\n' | b'\r' | 0)) {
            return Err(FormatError::Unusable("The delimiter, quote and escape must be single ASCII characters, not line breaks"));
        }
        if Some(self.delimiter) == self.quote || Some(self.delimiter) == self.escape {
            return Err(FormatError::Unusable("The delimiter cannot also be the quote or the escape character"));
        }
        Ok(self)
    }

    fn escape_char(&self) -> Option<u8> {
        self.escape.filter(|escape| Some(*escape) != self.quote)
    }
}

/// One record: where it starts (the line, counted from 1 across the whole file) and its fields.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Record {
    pub line: u64,
    pub fields: Vec<String>,
}

/// A record that could be split but not read.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BadRecord {
    pub line: u64,
}

#[derive(Debug, PartialEq, Eq)]
pub enum Next {
    Record(Record),
    /// A field that is not valid UTF-8: the next record is still readable.
    Bad(BadRecord),
    End,
}

/// What stops the read of the rest of the file.
#[derive(Debug)]
pub enum ParseError {
    Io(io::Error),
    /// The file ends inside a quoted field that started on `line`.
    UnterminatedQuote { line: u64 },
    /// A field of the record on `line` is longer than [`MAX_FIELD_BYTES`].
    FieldTooLarge { line: u64 },
    /// The record on `line` is longer than [`MAX_RECORD_BYTES`].
    RecordTooLarge { line: u64 },
}

impl std::fmt::Display for ParseError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Io(error) => write!(f, "The file could not be read: {error}"),
            Self::UnterminatedQuote { line } => write!(f, "A quoted value that starts on line {line} never ends: check the quote character"),
            Self::FieldTooLarge { line } => {
                write!(f, "A value on line {line} is larger than {} MiB: check the quote character", MAX_FIELD_BYTES / (1024 * 1024))
            }
            Self::RecordTooLarge { line } => {
                write!(f, "The record on line {line} is larger than {} MiB: check the delimiter and the quote character", MAX_RECORD_BYTES / (1024 * 1024))
            }
        }
    }
}

impl std::error::Error for ParseError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Io(error) => Some(error),
            _ => None,
        }
    }
}

impl From<io::Error> for ParseError {
    fn from(error: io::Error) -> Self {
        Self::Io(error)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum State {
    FieldStart,
    Unquoted,
    Quoted,
    /// After the escape character inside a quoted field.
    Escaped,
    /// After a quote inside a quoted field: a second one is a quote, anything else closes the field.
    QuoteSeen,
}

pub struct DelimitedReader<R> {
    reader: R,
    format: DelimitedFormat,
    /// The line the next byte is on (1-based).
    line: u64,
    field: Vec<u8>,
    /// The last record ended at a `\r`: a `\n` right after it is the same line break.
    skip_lf: bool,
}

impl<R: BufRead> DelimitedReader<R> {
    pub fn new(reader: R, format: DelimitedFormat) -> Self {
        Self { reader, format, line: 1, field: Vec::with_capacity(256), skip_lf: false }
    }

    /// Consumes the `\n` of a `\r\n` whose `\r` ended the last line.
    fn swallow_lf(&mut self) -> io::Result<()> {
        if std::mem::take(&mut self.skip_lf) && self.reader.fill_buf()?.first() == Some(&b'\n') {
            self.reader.consume(1);
        }
        Ok(())
    }

    /// Drops one line unread; `false` at the end of the input.
    fn skip_line(&mut self) -> io::Result<bool> {
        self.swallow_lf()?;
        let mut any = false;
        loop {
            let buffer = self.reader.fill_buf()?;
            if buffer.is_empty() {
                return Ok(any);
            }
            if let Some(at) = buffer.iter().position(|byte| matches!(byte, b'\n' | b'\r')) {
                self.skip_lf = buffer[at] == b'\r';
                self.reader.consume(at + 1);
                self.line += 1;
                return Ok(true);
            }
            any = true;
            let length = buffer.len();
            self.reader.consume(length);
        }
    }

    /// Drops the next `count` lines unread (a preamble before the header).
    pub fn skip_lines(&mut self, count: u64) -> io::Result<()> {
        for _ in 0..count {
            if !self.skip_line()? {
                break;
            }
        }
        Ok(())
    }

    pub fn next_record(&mut self) -> Result<Next, ParseError> {
        let DelimitedFormat { delimiter, quote, .. } = self.format;
        let escape = self.format.escape_char();
        loop {
            self.swallow_lf()?;
            let start_line = self.line;
            let mut fields: Vec<String> = Vec::new();
            let mut record_bytes = 0usize;
            let mut valid = true;
            let mut state = State::FieldStart;
            let mut touched = false;
            let mut quoted_any = false;
            self.field.clear();
            let mut ended = false;
            while !ended {
                let buffer = self.reader.fill_buf()?;
                if buffer.is_empty() {
                    if !touched {
                        return Ok(Next::End);
                    }
                    if matches!(state, State::Quoted | State::Escaped) {
                        return Err(ParseError::UnterminatedQuote { line: start_line });
                    }
                    Self::finish_field(&mut self.field, &mut fields, &mut valid);
                    break;
                }
                let mut used = 0;
                for &byte in buffer {
                    used += 1;
                    touched = true;
                    match state {
                        State::FieldStart | State::Unquoted => match byte {
                            b if b == delimiter => {
                                record_bytes += self.field.len() + FIELD_COST;
                                Self::finish_field(&mut self.field, &mut fields, &mut valid);
                                state = State::FieldStart;
                            }
                            b @ (b'\n' | b'\r') => {
                                self.line += 1;
                                self.skip_lf = b == b'\r';
                                record_bytes += self.field.len() + FIELD_COST;
                                Self::finish_field(&mut self.field, &mut fields, &mut valid);
                                ended = true;
                                break;
                            }
                            b if state == State::FieldStart && Some(b) == quote => {
                                state = State::Quoted;
                                quoted_any = true;
                            }
                            b => {
                                state = State::Unquoted;
                                self.field.push(b);
                            }
                        },
                        State::Quoted => match byte {
                            b if Some(b) == escape => state = State::Escaped,
                            b if Some(b) == quote => state = State::QuoteSeen,
                            b => {
                                if b == b'\n' {
                                    self.line += 1;
                                }
                                self.field.push(b);
                            }
                        },
                        State::Escaped => {
                            // An escape means the quote or the escape itself; any other pair is kept as written.
                            if Some(byte) != quote && Some(byte) != escape {
                                self.field.extend(escape);
                            }
                            if byte == b'\n' {
                                self.line += 1;
                            }
                            self.field.push(byte);
                            state = State::Quoted;
                        }
                        State::QuoteSeen => match byte {
                            b if Some(b) == quote => {
                                self.field.push(b);
                                state = State::Quoted;
                            }
                            b if b == delimiter => {
                                record_bytes += self.field.len() + FIELD_COST;
                                Self::finish_field(&mut self.field, &mut fields, &mut valid);
                                state = State::FieldStart;
                            }
                            b @ (b'\n' | b'\r') => {
                                self.line += 1;
                                self.skip_lf = b == b'\r';
                                record_bytes += self.field.len() + FIELD_COST;
                                Self::finish_field(&mut self.field, &mut fields, &mut valid);
                                ended = true;
                                break;
                            }
                            b => {
                                self.field.push(b);
                                state = State::Unquoted;
                            }
                        },
                    }
                    if self.field.len() > MAX_FIELD_BYTES {
                        return Err(ParseError::FieldTooLarge { line: start_line });
                    }
                    if record_bytes + self.field.len() > MAX_RECORD_BYTES {
                        return Err(ParseError::RecordTooLarge { line: start_line });
                    }
                }
                self.reader.consume(used);
            }
            // A line with nothing on it is not a record.
            if fields.len() == 1 && fields[0].is_empty() && !quoted_any && valid {
                continue;
            }
            return Ok(if valid { Next::Record(Record { line: start_line, fields }) } else { Next::Bad(BadRecord { line: start_line }) });
        }
    }

    fn finish_field(field: &mut Vec<u8>, fields: &mut Vec<String>, valid: &mut bool) {
        match std::str::from_utf8(field) {
            Ok(text) => fields.push(text.to_owned()),
            Err(_) => {
                *valid = false;
                fields.push(String::new());
            }
        }
        field.clear();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Cursor;

    fn reader(text: &str, format: DelimitedFormat) -> DelimitedReader<Cursor<Vec<u8>>> {
        DelimitedReader::new(Cursor::new(text.as_bytes().to_vec()), format)
    }

    fn all(text: &str, format: DelimitedFormat) -> Vec<Vec<String>> {
        let mut reader = reader(text, format);
        let mut rows = Vec::new();
        loop {
            match reader.next_record().unwrap() {
                Next::Record(record) => rows.push(record.fields),
                Next::Bad(bad) => panic!("bad record at line {}", bad.line),
                Next::End => return rows,
            }
        }
    }

    fn csv(text: &str) -> Vec<Vec<String>> {
        all(text, DelimitedFormat::default())
    }

    fn row(fields: &[&str]) -> Vec<String> {
        fields.iter().map(|field| (*field).to_owned()).collect()
    }

    #[test]
    fn plain_records_with_lf_and_crlf_and_no_final_newline() {
        assert_eq!(csv("a,b,c\n1,2,3\r\n4,5,6"), [row(&["a", "b", "c"]), row(&["1", "2", "3"]), row(&["4", "5", "6"])]);
    }

    #[test]
    fn quotes_hold_delimiters_line_breaks_and_doubled_quotes() {
        let rows = csv("id,note\n1,\"a, b\"\n2,\"line one\nline two\"\n3,\"she said \"\"hi\"\"\"\n4,\"\"\n");
        assert_eq!(rows[1], row(&["1", "a, b"]));
        assert_eq!(rows[2], row(&["2", "line one\nline two"]));
        assert_eq!(rows[3], row(&["3", "she said \"hi\""]));
        assert_eq!(rows[4], row(&["4", ""]));
    }

    #[test]
    fn crlf_after_a_closing_quote_is_not_part_of_the_value() {
        assert_eq!(csv("\"a\",\"b\"\r\n\"c\",\"d\"\r\n"), [row(&["a", "b"]), row(&["c", "d"])]);
    }

    #[test]
    fn a_quote_inside_an_unquoted_field_is_text_and_text_after_a_closing_quote_is_kept() {
        assert_eq!(csv("5\" pipe,x\n\"ab\"cd,y\n"), [row(&["5\" pipe", "x"]), row(&["abcd", "y"])]);
    }

    #[test]
    fn empty_fields_and_a_trailing_delimiter_are_fields() {
        assert_eq!(csv("a,,c\n,,\nx,\n"), [row(&["a", "", "c"]), row(&["", "", ""]), row(&["x", ""])]);
    }

    #[test]
    fn blank_lines_are_not_records_but_an_empty_quoted_value_is() {
        assert_eq!(csv("a\n\n\r\nb\n\"\"\n"), [row(&["a"]), row(&["b"]), row(&[""])]);
    }

    #[test]
    fn a_lone_carriage_return_ends_a_record_and_crlf_is_one_line_break() {
        assert_eq!(csv("a,b\r1,2\r\n3,4\r\r\n5,6\r"), [row(&["a", "b"]), row(&["1", "2"]), row(&["3", "4"]), row(&["5", "6"])]);
        // Inside quotes a carriage return is text.
        assert_eq!(csv("\"x\ry\",z\r1,2\r"), [row(&["x\ry", "z"]), row(&["1", "2"])]);
        // Lines are counted once for CRLF and once for a lone CR.
        let mut reader = reader("h\r\n1\r2\n3\r\n", DelimitedFormat::default());
        let mut lines = Vec::new();
        while let Next::Record(record) = reader.next_record().unwrap() {
            lines.push(record.line);
        }
        assert_eq!(lines, [1, 2, 3, 4]);
    }

    #[test]
    fn skipping_lines_counts_every_kind_of_line_break() {
        let mut reader = reader("one\rtwo\r\nthree\nid\n1\n", DelimitedFormat::default());
        reader.skip_lines(3).unwrap();
        let Next::Record(header) = reader.next_record().unwrap() else { panic!("a header") };
        assert_eq!((header.line, header.fields), (4, row(&["id"])));
    }

    #[test]
    fn a_record_of_endless_empty_values_is_stopped_by_the_record_cap() {
        let commas = ",".repeat(MAX_RECORD_BYTES / FIELD_COST + 10);
        let mut reader = reader(&commas, DelimitedFormat::default());
        assert!(matches!(reader.next_record(), Err(ParseError::RecordTooLarge { line: 1 })));
        // Many large values add up too, though each is under the field cap.
        let value = "v".repeat(MAX_FIELD_BYTES - 1);
        let record = std::iter::repeat_n(value.as_str(), 5).collect::<Vec<_>>().join(",");
        let mut reader = self::reader(&record, DelimitedFormat::default());
        assert!(matches!(reader.next_record(), Err(ParseError::RecordTooLarge { .. })));
    }

    #[test]
    fn tab_and_other_delimiters() {
        let tsv = DelimitedFormat { delimiter: b'\t', ..Default::default() };
        assert_eq!(all("a\tb,c\n1\t\"x\ty\"\n", tsv), [row(&["a", "b,c"]), row(&["1", "x\ty"])]);
        let pipe = DelimitedFormat { delimiter: b'|', quote: Some(b'\''), escape: None };
        assert_eq!(all("a|'it''s'|\"q\"\n", pipe), [row(&["a", "it's", "\"q\""])]);
        let unquoted = DelimitedFormat { delimiter: b';', quote: None, escape: None };
        assert_eq!(all("\"a\";b\n", unquoted), [row(&["\"a\"", "b"])]);
    }

    #[test]
    fn a_backslash_escape_makes_a_quote_or_itself_and_keeps_other_pairs() {
        let backslash = DelimitedFormat { escape: Some(b'\\'), ..Default::default() };
        assert_eq!(all("\"say \\\"hi\\\"\",\"c:\\\\dir\",\"a\\nb\"\n", backslash), [row(&["say \"hi\"", "c:\\dir", "a\\nb"])]);
        // The doubled quote still works next to an escape.
        assert_eq!(all("\"x\"\"y\"\n", backslash), [row(&["x\"y"])]);
    }

    #[test]
    fn ragged_rows_come_out_as_they_are() {
        assert_eq!(csv("a,b,c\n1,2\n1,2,3,4\n"), [row(&["a", "b", "c"]), row(&["1", "2"]), row(&["1", "2", "3", "4"])]);
    }

    #[test]
    fn records_know_the_line_they_start_on() {
        let mut reader = reader("h\n\"a\nb\",1\n\nz\n", DelimitedFormat::default());
        let mut lines = Vec::new();
        while let Next::Record(record) = reader.next_record().unwrap() {
            lines.push(record.line);
        }
        assert_eq!(lines, [1, 2, 5]);
    }

    #[test]
    fn skipping_lines_comes_before_the_first_record() {
        let mut reader = reader("preamble one\npreamble, two\nid,name\n1,a\n", DelimitedFormat::default());
        reader.skip_lines(2).unwrap();
        let Next::Record(header) = reader.next_record().unwrap() else { panic!("a header") };
        assert_eq!((header.line, header.fields), (3, row(&["id", "name"])));
        // Skipping more lines than the file has is not an error.
        let mut short = self::reader("one\n", DelimitedFormat::default());
        short.skip_lines(5).unwrap();
        assert_eq!(short.next_record().unwrap(), Next::End);
    }

    #[test]
    fn invalid_utf8_marks_one_record_bad_and_the_next_still_reads() {
        let mut bytes = b"a,b\n".to_vec();
        bytes.extend([b'x', 0xFF, b',', b'y', b'\n']);
        bytes.extend(b"c,d\n");
        let mut reader = DelimitedReader::new(Cursor::new(bytes), DelimitedFormat::default());
        assert!(matches!(reader.next_record().unwrap(), Next::Record(_)));
        assert_eq!(reader.next_record().unwrap(), Next::Bad(BadRecord { line: 2 }));
        assert_eq!(reader.next_record().unwrap(), Next::Record(Record { line: 3, fields: row(&["c", "d"]) }));
    }

    #[test]
    fn a_quote_that_never_closes_is_an_error_at_the_end_of_the_file() {
        let mut reader = reader("a,b\n1,\"open\nstill open\n", DelimitedFormat::default());
        assert!(matches!(reader.next_record().unwrap(), Next::Record(_)));
        assert!(matches!(reader.next_record(), Err(ParseError::UnterminatedQuote { line: 2 })));
    }

    #[test]
    fn a_huge_field_reads_and_one_past_the_limit_stops() {
        let big = "x".repeat(3 * 1024 * 1024);
        assert_eq!(csv(&format!("a,\"{big}\"\n"))[0][1].len(), big.len());
        let too_big = "y".repeat(MAX_FIELD_BYTES + 1);
        let mut reader = reader(&format!("\"{too_big}\n"), DelimitedFormat::default());
        assert!(matches!(reader.next_record(), Err(ParseError::FieldTooLarge { line: 1 })));
    }

    #[test]
    fn unicode_and_a_final_record_without_a_newline() {
        assert_eq!(csv("名前,café\nZoë,😀"), [row(&["名前", "café"]), row(&["Zoë", "😀"])]);
    }

    #[test]
    fn the_format_refuses_what_cannot_be_read() {
        assert!(DelimitedFormat { delimiter: b'\n', ..Default::default() }.checked().is_err());
        assert!(DelimitedFormat { delimiter: b'"', ..Default::default() }.checked().is_err());
        assert!(DelimitedFormat { delimiter: 0xC3, ..Default::default() }.checked().is_err());
        assert!(DelimitedFormat { delimiter: b',', quote: Some(b'"'), escape: Some(b',') }.checked().is_err());
        assert!(DelimitedFormat::default().checked().is_ok());
    }
}
