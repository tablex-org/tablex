//! The rows of an import file, however it is laid out: [`SourceReader`] opens a file with the user's
//! options and hands out its rows one at a time, every cell already a `String` (or NULL).
//!
//! A row that cannot be used (a field that is not UTF-8, a record of the wrong width, a JSON value
//! that is not an object) comes out as a [`RowProblem`] and the next row still reads. What breaks
//! the file itself (an I/O error, a quote that never closes, JSON syntax) is an [`ImportError`].

use std::collections::HashMap;
use std::fs::File;
use std::io::{self, BufRead};
use std::path::{Path, PathBuf};
use std::sync::atomic::AtomicU64;
use std::sync::Arc;

use super::delimited::{DelimitedFormat, DelimitedReader, FormatError, Next, ParseError};
use super::encoding::{CountingReader, Encoding};
use super::json::{JsonError, JsonNext, JsonReader};

/// How the rows are laid out.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Layout {
    Delimited {
        format: DelimitedFormat,
        /// The first record (after the skipped lines) names the columns.
        header: bool,
    },
    /// An array of objects or one object per line.
    Json,
}

/// What the user chose about reading the file.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SourceOptions {
    pub encoding: Encoding,
    pub layout: Layout,
    /// Lines dropped from the top of a delimited file (before the header).
    pub skip_lines: u64,
    /// A cell that reads exactly this is NULL (`None`: no such text).
    pub null_text: Option<String>,
    /// An empty cell is NULL.
    pub empty_as_null: bool,
    /// White space around a cell is dropped.
    pub trim: bool,
}

impl SourceOptions {
    pub fn delimited(format: DelimitedFormat) -> Self {
        Self {
            encoding: Encoding::Auto,
            layout: Layout::Delimited { format, header: true },
            skip_lines: 0,
            null_text: None,
            empty_as_null: false,
            trim: false,
        }
    }

    pub fn json() -> Self {
        Self { layout: Layout::Json, ..Self::delimited(DelimitedFormat::default()) }
    }

    /// The cell a raw text becomes: trimmed, then NULL if it is the NULL text or (when asked) empty.
    pub fn cell(&self, raw: String) -> Option<String> {
        let raw = if self.trim && raw.trim().len() != raw.len() { raw.trim().to_owned() } else { raw };
        if self.null_text.as_deref() == Some(raw.as_str()) || (self.empty_as_null && raw.is_empty()) {
            return None;
        }
        Some(raw)
    }
}

/// One row of the file.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Row {
    /// 1-based among the data rows of the file (rows that could not be read count).
    pub number: u64,
    /// Where it starts in the file: the line of a delimited file, the position among the values of a JSON one.
    pub line: u64,
    pub cells: Vec<Option<String>>,
    /// JSON values whose key is not one of the columns the file was opened with (keys that first appear after the
    /// sample), already through the cell options. Always empty for a delimited file.
    pub extra: Vec<(String, Option<String>)>,
}

/// Why a row of the file cannot be imported.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RowProblem {
    FieldCount { found: usize, expected: usize },
    InvalidUtf8,
    NotAnObject(&'static str),
    /// A column the server calls text sent bytes that are not valid text (a data transfer read it).
    BytesInText(String),
}

impl std::fmt::Display for RowProblem {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::FieldCount { found, expected } => write!(f, "has {found} values, the header has {expected}"),
            Self::InvalidUtf8 => f.write_str("is not valid text in this encoding"),
            Self::NotAnObject(found) => write!(f, "is {found}, not an object"),
            Self::BytesInText(column) => write!(f, "has bytes that are not valid text in \"{column}\", which cannot be written to a text column"),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BadRow {
    pub number: u64,
    pub line: u64,
    pub problem: RowProblem,
}

#[derive(Debug)]
pub enum Read {
    Row(Row),
    Bad(BadRow),
    End,
}

/// What stops the whole import of a file.
#[derive(Debug)]
pub enum ImportError {
    Open { path: PathBuf, source: io::Error },
    Format(FormatError),
    Parse(ParseError),
    Json(JsonError),
    /// A delimited file with no record to take the columns from.
    NoColumns,
}

impl std::fmt::Display for ImportError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Open { path, source } => write!(f, "Could not open {}: {source}", path.display()),
            Self::Format(error) => error.fmt(f),
            Self::Parse(error) => error.fmt(f),
            Self::Json(error) => error.fmt(f),
            Self::NoColumns => f.write_str("The file has no rows to take the columns from"),
        }
    }
}

impl std::error::Error for ImportError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Open { source, .. } => Some(source),
            Self::Format(error) => Some(error),
            Self::Parse(error) => Some(error),
            Self::Json(error) => Some(error),
            Self::NoColumns => None,
        }
    }
}

impl From<FormatError> for ImportError {
    fn from(error: FormatError) -> Self {
        Self::Format(error)
    }
}

impl From<ParseError> for ImportError {
    fn from(error: ParseError) -> Self {
        Self::Parse(error)
    }
}

impl From<JsonError> for ImportError {
    fn from(error: JsonError) -> Self {
        Self::Json(error)
    }
}

enum Inner {
    Delimited { reader: DelimitedReader<Box<dyn BufRead>> },
    Json { reader: JsonReader<Box<dyn BufRead>>, position: HashMap<String, usize> },
}

pub struct SourceReader {
    inner: Inner,
    options: SourceOptions,
    columns: Vec<String>,
    /// The first record of a delimited file without a header: it is a row, read ahead to count the columns.
    ahead: Option<Row>,
    /// Records before the first readable one that could not be decoded (a file without a header): they are rows
    /// too, and come out first.
    early_bad: std::collections::VecDeque<BadRow>,
    number: u64,
}

/// The file's bytes read so far, shared with whoever shows the progress.
pub type ByteCount = Arc<AtomicU64>;

impl SourceReader {
    /// Opens `path`. `columns` are the names to read a JSON file by (the keys of its rows; any other key is
    /// left out and counted); a delimited file takes its columns from its header, or `column1…` without
    /// one, whatever `columns` says.
    pub fn open(path: &Path, options: &SourceOptions, columns: &[String], bytes: ByteCount) -> Result<Self, ImportError> {
        let file = File::open(path).map_err(|source| ImportError::Open { path: path.to_owned(), source })?;
        let text = options
            .encoding
            .decode(CountingReader::new(file, bytes))
            .map_err(|source| ImportError::Open { path: path.to_owned(), source })?;
        let io_error = |error: io::Error| ImportError::Parse(ParseError::Io(error));
        match &options.layout {
            Layout::Delimited { format, header } => {
                let mut reader = DelimitedReader::new(text, format.checked()?);
                reader.skip_lines(options.skip_lines).map_err(io_error)?;
                let mut source = Self { inner: Inner::Delimited { reader }, options: options.clone(), columns: Vec::new(), ahead: None, early_bad: Default::default(), number: 0 };
                source.columns = source.delimited_columns(*header)?;
                Ok(source)
            }
            Layout::Json => {
                let reader = JsonReader::new(text).map_err(io_error)?;
                let position = columns.iter().enumerate().map(|(index, name)| (name.clone(), index)).collect();
                Ok(Self { inner: Inner::Json { reader, position }, options: options.clone(), columns: columns.to_vec(), ahead: None, early_bad: Default::default(), number: 0 })
            }
        }
    }

    /// The column names: the header's, or `column1…` counted from the first record.
    fn delimited_columns(&mut self, header: bool) -> Result<Vec<String>, ImportError> {
        loop {
            let Inner::Delimited { reader } = &mut self.inner else { unreachable!("only a delimited file has a header") };
            match reader.next_record()? {
                Next::End => return Err(ImportError::NoColumns),
                // A header that cannot be read names no column.
                Next::Bad(_) if header => return Err(ImportError::NoColumns),
                // Without a header it is a row: it comes out as a bad one, and the width comes from the next.
                Next::Bad(bad) => {
                    self.number += 1;
                    self.early_bad.push_back(BadRow { number: self.number, line: bad.line, problem: RowProblem::InvalidUtf8 });
                }
                Next::Record(record) if header => {
                    return Ok(unique_names(record.fields.iter().map(|field| field.trim().to_owned())));
                }
                Next::Record(record) => {
                    let width = record.fields.len();
                    self.ahead = Some(self.row_of(record.line, record.fields));
                    return Ok((1..=width).map(|index| format!("column{index}")).collect());
                }
            }
        }
    }

    fn row_of(&mut self, line: u64, fields: Vec<String>) -> Row {
        self.number += 1;
        Row { number: self.number, line, cells: fields.into_iter().map(|field| self.options.cell(field)).collect(), extra: Vec::new() }
    }

    pub fn columns(&self) -> &[String] {
        &self.columns
    }

    pub fn next_row(&mut self) -> Result<Read, ImportError> {
        if let Some(bad) = self.early_bad.pop_front() {
            return Ok(Read::Bad(bad));
        }
        if let Some(row) = self.ahead.take() {
            return Ok(Read::Row(row));
        }
        let width = self.columns.len();
        match &mut self.inner {
            Inner::Delimited { reader } => match reader.next_record()? {
                Next::End => Ok(Read::End),
                Next::Bad(bad) => {
                    self.number += 1;
                    Ok(Read::Bad(BadRow { number: self.number, line: bad.line, problem: RowProblem::InvalidUtf8 }))
                }
                Next::Record(record) if record.fields.len() != width => {
                    self.number += 1;
                    Ok(Read::Bad(BadRow {
                        number: self.number,
                        line: record.line,
                        problem: RowProblem::FieldCount { found: record.fields.len(), expected: width },
                    }))
                }
                Next::Record(record) => Ok(Read::Row(self.row_of(record.line, record.fields))),
            },
            Inner::Json { reader, position } => match reader.next_row()? {
                JsonNext::End => Ok(Read::End),
                JsonNext::Bad(bad) => {
                    self.number += 1;
                    Ok(Read::Bad(BadRow { number: self.number, line: bad.index, problem: RowProblem::NotAnObject(bad.found) }))
                }
                JsonNext::Row(row) => {
                    self.number += 1;
                    let mut cells = vec![None; width];
                    let mut extra = Vec::new();
                    for (key, value) in row.fields {
                        let value = value.and_then(|text| self.options.cell(text));
                        match position.get(&key) {
                            Some(&index) => cells[index] = value,
                            None => extra.push((key, value)),
                        }
                    }
                    Ok(Read::Row(Row { number: self.number, line: row.index, cells, extra }))
                }
            },
        }
    }
}

/// `names` with blanks and repeats made distinct: `column3` for a blank third, `name_2` for a second `name`.
fn unique_names(names: impl Iterator<Item = String>) -> Vec<String> {
    let mut seen: HashMap<String, usize> = HashMap::new();
    names
        .enumerate()
        .map(|(index, name)| {
            let name = if name.is_empty() { format!("column{}", index + 1) } else { name };
            let count = seen.entry(name.to_lowercase()).or_insert(0);
            *count += 1;
            if *count == 1 {
                name
            } else {
                format!("{name}_{count}")
            }
        })
        .collect()
}

/// The first rows of a file, to show and to infer types from.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Sample {
    pub columns: Vec<String>,
    pub rows: Vec<Row>,
    /// Rows of the sample that could not be read.
    pub bad: Vec<BadRow>,
    /// The file has more rows than the sample.
    pub more: bool,
    pub file_size: u64,
}

impl Sample {
    /// The first `limit` data rows of `path`. A JSON file's columns are the keys of those rows in the order
    /// they first appear.
    pub fn read(path: &Path, options: &SourceOptions, limit: usize) -> Result<Self, ImportError> {
        let file_size = std::fs::metadata(path).map_err(|source| ImportError::Open { path: path.to_owned(), source })?.len();
        let bytes = ByteCount::default();
        let json = matches!(options.layout, Layout::Json);
        // A JSON file is read twice: once for the names of its columns, once by those names.
        let mut names: Vec<String> = Vec::new();
        if json {
            let mut keys = JsonReader::new(
                options
                    .encoding
                    .decode(File::open(path).map_err(|source| ImportError::Open { path: path.to_owned(), source })?)
                    .map_err(|source| ImportError::Open { path: path.to_owned(), source })?,
            )
            .map_err(|source| ImportError::Parse(ParseError::Io(source)))?;
            let mut seen = 0;
            while seen < limit {
                match keys.next_row()? {
                    JsonNext::End => break,
                    JsonNext::Bad(_) => {}
                    JsonNext::Row(row) => {
                        for (key, _) in row.fields {
                            if !names.contains(&key) {
                                names.push(key);
                            }
                        }
                    }
                }
                seen += 1;
            }
        }
        let mut reader = SourceReader::open(path, options, &names, bytes)?;
        let mut rows = Vec::new();
        let mut bad = Vec::new();
        let mut more = false;
        loop {
            match reader.next_row()? {
                Read::End => break,
                _ if rows.len() + bad.len() >= limit => {
                    more = true;
                    break;
                }
                Read::Row(row) => rows.push(row),
                Read::Bad(row) => bad.push(row),
            }
        }
        Ok(Self { columns: reader.columns().to_vec(), rows, bad, more, file_size })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn file(name: &str, bytes: &[u8]) -> (tempfile::TempDir, PathBuf) {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join(name);
        std::fs::write(&path, bytes).unwrap();
        (dir, path)
    }

    fn cells(row: &Row) -> Vec<Option<&str>> {
        row.cells.iter().map(|cell| cell.as_deref()).collect()
    }

    #[test]
    fn a_header_names_the_columns_and_the_rows_follow() {
        let (_dir, path) = file("a.csv", b"id,name\n1,Ann\n2,\n");
        let sample = Sample::read(&path, &SourceOptions::delimited(DelimitedFormat::default()), 100).unwrap();
        assert_eq!(sample.columns, ["id", "name"]);
        assert_eq!(sample.rows.iter().map(cells).collect::<Vec<_>>(), [[Some("1"), Some("Ann")], [Some("2"), Some("")]]);
        assert_eq!(sample.rows[1].line, 3);
        assert!(!sample.more);
    }

    #[test]
    fn without_a_header_the_first_record_is_a_row_and_columns_are_numbered() {
        let (_dir, path) = file("a.csv", b"1,Ann\n2,Bob\n");
        let mut options = SourceOptions::delimited(DelimitedFormat::default());
        options.layout = Layout::Delimited { format: DelimitedFormat::default(), header: false };
        let sample = Sample::read(&path, &options, 100).unwrap();
        assert_eq!(sample.columns, ["column1", "column2"]);
        assert_eq!(sample.rows.len(), 2);
        assert_eq!(cells(&sample.rows[0]), [Some("1"), Some("Ann")]);
    }

    #[test]
    fn blank_and_repeated_header_names_are_made_distinct() {
        let (_dir, path) = file("a.csv", b"name, ,Name,id\n1,2,3,4\n");
        let sample = Sample::read(&path, &SourceOptions::delimited(DelimitedFormat::default()), 10).unwrap();
        assert_eq!(sample.columns, ["name", "column2", "Name_2", "id"]);
    }

    #[test]
    fn skipped_lines_come_before_the_header() {
        let (_dir, path) = file("a.csv", b"Report of 2024\n\nid;name\n1;x\n");
        let mut options = SourceOptions::delimited(DelimitedFormat { delimiter: b';', ..Default::default() });
        options.skip_lines = 2;
        let sample = Sample::read(&path, &options, 10).unwrap();
        assert_eq!(sample.columns, ["id", "name"]);
        assert_eq!(sample.rows.len(), 1);
    }

    #[test]
    fn null_text_empty_as_null_and_trim_make_cells() {
        let (_dir, path) = file("a.csv", b"a,b,c,d\n NULL ,\\N,,  x  \n");
        let mut options = SourceOptions::delimited(DelimitedFormat::default());
        options.null_text = Some("\\N".into());
        options.empty_as_null = true;
        options.trim = true;
        let sample = Sample::read(&path, &options, 10).unwrap();
        assert_eq!(cells(&sample.rows[0]), [Some("NULL"), None, None, Some("x")]);
        // Without trimming, the NULL text must match the whole cell.
        options.trim = false;
        options.null_text = Some("NULL".into());
        let untrimmed = Sample::read(&path, &options, 10).unwrap();
        assert_eq!(cells(&untrimmed.rows[0])[0], Some(" NULL "));
    }

    #[test]
    fn ragged_rows_are_bad_rows_and_the_others_still_read() {
        let (_dir, path) = file("a.csv", b"a,b\n1,2\n3\n4,5,6\n7,8\n");
        let sample = Sample::read(&path, &SourceOptions::delimited(DelimitedFormat::default()), 10).unwrap();
        assert_eq!(sample.rows.len(), 2);
        assert_eq!(
            sample.bad,
            [
                BadRow { number: 2, line: 3, problem: RowProblem::FieldCount { found: 1, expected: 2 } },
                BadRow { number: 3, line: 4, problem: RowProblem::FieldCount { found: 3, expected: 2 } },
            ]
        );
        assert_eq!(sample.rows[1].number, 4);
    }

    #[test]
    fn the_sample_stops_at_its_limit_and_says_there_is_more() {
        let (_dir, path) = file("a.csv", b"a\n1\n2\n3\n4\n");
        let sample = Sample::read(&path, &SourceOptions::delimited(DelimitedFormat::default()), 3).unwrap();
        assert_eq!((sample.rows.len(), sample.more), (3, true));
        let all = Sample::read(&path, &SourceOptions::delimited(DelimitedFormat::default()), 4).unwrap();
        assert_eq!((all.rows.len(), all.more), (4, false));
    }

    #[test]
    fn latin1_and_utf16_files_read_as_text() {
        let (_dir, latin) = file("l.csv", &[b'n', b'a', b'm', b'e', b'\n', b'c', b'a', b'f', 0xE9, b'\n']);
        let mut options = SourceOptions::delimited(DelimitedFormat::default());
        options.encoding = Encoding::Latin1;
        assert_eq!(cells(&Sample::read(&latin, &options, 10).unwrap().rows[0]), [Some("café")]);

        let mut bytes = vec![0xFF, 0xFE];
        bytes.extend("a,b\nä,😀\n".encode_utf16().flat_map(u16::to_le_bytes));
        let (_dir16, utf16) = file("u.csv", &bytes);
        let sample = Sample::read(&utf16, &SourceOptions::delimited(DelimitedFormat::default()), 10).unwrap();
        assert_eq!(sample.columns, ["a", "b"]);
        assert_eq!(cells(&sample.rows[0]), [Some("ä"), Some("😀")]);
    }

    #[test]
    fn invalid_utf8_is_one_bad_row() {
        let (_dir, path) = file("a.csv", b"a\nok\nb\xFFd\nfine\n");
        let sample = Sample::read(&path, &SourceOptions::delimited(DelimitedFormat::default()), 10).unwrap();
        assert_eq!(sample.rows.len(), 2);
        assert_eq!(sample.bad[0].problem, RowProblem::InvalidUtf8);
    }

    #[test]
    fn json_columns_are_the_keys_in_order_of_appearance_and_missing_keys_are_null() {
        let (_dir, path) = file("a.json", br#"[{"id":1,"name":"Ann"},{"name":"Bob","extra":true,"id":2},{"id":3}]"#);
        let sample = Sample::read(&path, &SourceOptions::json(), 100).unwrap();
        assert_eq!(sample.columns, ["id", "name", "extra"]);
        assert_eq!(
            sample.rows.iter().map(cells).collect::<Vec<_>>(),
            [[Some("1"), Some("Ann"), None], [Some("2"), Some("Bob"), Some("true")], [Some("3"), None, None]]
        );
    }

    #[test]
    fn ndjson_reads_like_an_array_and_a_key_outside_the_columns_comes_with_its_row() {
        let (_dir, path) = file("a.ndjson", b"{\"a\":1}\n{\"a\":2,\"late\":\" 5 \"}\n");
        let mut options = SourceOptions::json();
        options.trim = true;
        let mut reader = SourceReader::open(&path, &options, &["a".to_owned()], ByteCount::default()).unwrap();
        let mut rows = Vec::new();
        while let Read::Row(row) = reader.next_row().unwrap() {
            rows.push(row);
        }
        assert_eq!(rows.len(), 2);
        assert!(rows[0].extra.is_empty());
        assert_eq!(rows[1].extra, [("late".to_owned(), Some("5".to_owned()))]);
    }

    #[test]
    fn an_undecodable_first_record_of_a_file_without_a_header_is_a_bad_row_and_the_numbering_holds() {
        let (_dir, path) = file("a.csv", b"b\xFFd,x\n1,2\n3,4\n");
        let mut options = SourceOptions::delimited(DelimitedFormat::default());
        options.layout = Layout::Delimited { format: DelimitedFormat::default(), header: false };
        let sample = Sample::read(&path, &options, 10).unwrap();
        assert_eq!(sample.columns, ["column1", "column2"]);
        assert_eq!(sample.bad, [BadRow { number: 1, line: 1, problem: RowProblem::InvalidUtf8 }]);
        assert_eq!(sample.rows.iter().map(|row| (row.number, row.line)).collect::<Vec<_>>(), [(2, 2), (3, 3)]);
    }

    #[test]
    fn a_json_element_that_is_not_an_object_is_a_bad_row() {
        let (_dir, path) = file("a.json", b"[{\"a\":1}, 7, {\"a\":2}]");
        let sample = Sample::read(&path, &SourceOptions::json(), 10).unwrap();
        assert_eq!(sample.rows.len(), 2);
        assert_eq!(sample.bad[0].problem, RowProblem::NotAnObject("a number"));
    }

    #[test]
    fn an_empty_delimited_file_has_no_columns_and_a_missing_one_cannot_open() {
        let (_dir, path) = file("a.csv", b"");
        assert!(matches!(Sample::read(&path, &SourceOptions::delimited(DelimitedFormat::default()), 10), Err(ImportError::NoColumns)));
        let missing = path.with_file_name("missing.csv");
        assert!(matches!(Sample::read(&missing, &SourceOptions::delimited(DelimitedFormat::default()), 10), Err(ImportError::Open { .. })));
    }

    #[test]
    fn bytes_read_are_counted_for_the_progress() {
        let (_dir, path) = file("a.csv", b"a\n1\n2\n");
        let bytes = ByteCount::default();
        let mut reader = SourceReader::open(&path, &SourceOptions::delimited(DelimitedFormat::default()), &[], bytes.clone()).unwrap();
        while !matches!(reader.next_row().unwrap(), Read::End) {}
        assert_eq!(bytes.load(std::sync::atomic::Ordering::Relaxed), 6);
    }
}
