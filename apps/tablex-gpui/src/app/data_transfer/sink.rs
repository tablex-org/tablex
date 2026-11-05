//! The file side of a transfer: CSV, JSON, SQL INSERT and XLSX written one row at a time, so a table of any size
//! costs the memory of a buffer, never of its rows.
//!
//! Cells arrive as the text the result grid shows (`None` is a SQL NULL) next to the type of their column, and the
//! type decides how the text is written: a JSON number is a number only when the text is one, a text that looks
//! like a number stays a string, a NULL is `null` / the NULL text / an empty XLSX cell, never the word. The formats
//! reuse what the app already has: the CSV quoting and NULL rules of the copy dialog (`DsvOptions`), the SQL literal
//! rules of "Copy as SQL INSERT" (`sql_literal`, `SqlTarget`) and the XLSX writer of `engine-export`.
//!
//! A text format is written to a part file of its own (`.<name>.<random>.part` beside it, never a name the user may have) and
//! renamed when the table is done, so a file that exists is complete, and an export that fails, is cancelled or is dropped
//! leaves nothing behind. An XLSX workbook is written when it is finished.

use std::fs::File;
use std::io::{BufWriter, Write};
use std::path::{Path, PathBuf};

use engine_export::{ExportCell, ExportDialect, ExportFormat, TableExporter};

use super::super::results::column_type::{value_kind, ValueKind};
use super::super::results::copy::{sql_literal, unique_column_names, DsvOptions, SqlTarget};

/// A text format writes in chunks this big.
const BUFFER_BYTES: usize = 256 * 1024;

/// Rows of one `INSERT` statement of a SQL file.
pub(in crate::app) const SQL_ROWS_PER_STATEMENT: usize = 500;

/// An XLSX sheet holds this many rows, the header included.
pub(in crate::app) const XLSX_MAX_ROWS: u64 = 1_048_576;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(in crate::app) enum FileFormat {
    Csv,
    Json,
    SqlInsert,
    Xlsx,
}

impl FileFormat {
    pub(in crate::app) const ALL: [Self; 4] = [Self::Csv, Self::Json, Self::SqlInsert, Self::Xlsx];

    pub(in crate::app) fn label(self) -> &'static str {
        match self {
            Self::Csv => "CSV",
            Self::Json => "JSON",
            Self::SqlInsert => "SQL INSERT",
            Self::Xlsx => "XLSX",
        }
    }

    pub(in crate::app) fn extension(self) -> &'static str {
        match self {
            Self::Csv => "csv",
            Self::Json => "json",
            Self::SqlInsert => "sql",
            Self::Xlsx => "xlsx",
        }
    }
}

/// A column of the file.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(in crate::app) struct SinkColumn {
    pub(in crate::app) name: String,
    /// The type the source reports: how a JSON value, an XLSX cell or a SQL literal is made of the text.
    pub(in crate::app) data_type: String,
}

/// What a SQL file needs: the dialect it is written for and the table its statements insert into.
#[derive(Debug, Clone, Copy)]
pub(in crate::app) struct SqlStyle<'a> {
    pub(in crate::app) target: SqlTarget,
    pub(in crate::app) schema: Option<&'a str>,
    pub(in crate::app) table: &'a str,
}

/// Why a file cannot be written.
#[derive(Debug)]
pub(in crate::app) enum SinkError {
    /// The file exists and the export was asked not to replace it.
    Exists(PathBuf),
    Io { path: PathBuf, source: std::io::Error },
    /// A name the dialect cannot quote, or a CSV setting it cannot use.
    Refused(String),
    Xlsx(String),
}

impl std::fmt::Display for SinkError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Exists(path) => write!(f, "{} exists already (tick \"Replace existing files\" to overwrite it)", path.display()),
            Self::Io { path, source } => write!(f, "Could not write {}: {source}", path.display()),
            Self::Refused(message) => f.write_str(message),
            Self::Xlsx(message) => write!(f, "Could not write the workbook: {message}"),
        }
    }
}

impl std::error::Error for SinkError {}

/// How a cell of a column is written.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Kind {
    /// A whole or floating point number: bare when the text is one.
    Number,
    /// Exact digits: a string in JSON (a reader's double would round them), a number in a sheet.
    Decimal,
    Boolean,
    Json,
    Text,
}

impl Kind {
    fn of(data_type: &str) -> Self {
        match value_kind(data_type) {
            ValueKind::Integer | ValueKind::Float => Self::Number,
            ValueKind::Decimal => Self::Decimal,
            ValueKind::Boolean => Self::Boolean,
            ValueKind::Json => Self::Json,
            ValueKind::Temporal | ValueKind::Binary | ValueKind::Text => Self::Text,
        }
    }
}

/// `-?(0|[1-9][0-9]*)(.[0-9]+)?([eE][+-]?[0-9]+)?`: what JSON takes as a number.
fn is_json_number(text: &str) -> bool {
    let bytes = text.as_bytes();
    let digits = |from: usize| bytes[from..].iter().take_while(|byte| byte.is_ascii_digit()).count();
    let mut at = usize::from(bytes.first() == Some(&b'-'));
    let whole = digits(at);
    if whole == 0 || (whole > 1 && bytes[at] == b'0') {
        return false;
    }
    at += whole;
    if bytes.get(at) == Some(&b'.') {
        let fraction = digits(at + 1);
        if fraction == 0 {
            return false;
        }
        at += 1 + fraction;
    }
    if matches!(bytes.get(at), Some(b'e' | b'E')) {
        at += 1;
        if matches!(bytes.get(at), Some(b'+' | b'-')) {
            at += 1;
        }
        let exponent = digits(at);
        if exponent == 0 {
            return false;
        }
        at += exponent;
    }
    at == bytes.len()
}

fn flag(text: &str) -> Option<bool> {
    match text {
        "true" | "t" | "TRUE" | "T" | "1" => Some(true),
        "false" | "f" | "FALSE" | "F" | "0" => Some(false),
        _ => None,
    }
}

enum Body {
    Csv { out: BufWriter<File>, dsv: DsvOptions },
    Json { out: BufWriter<File>, keys: Vec<String> },
    Sql { out: BufWriter<File>, target: SqlTarget, head: String, in_statement: usize },
    Xlsx { exporter: Box<TableExporter> },
}

/// One cell of a row of a file.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(in crate::app) enum Cell<'a> {
    Null,
    Text(&'a str),
    /// `0x` and hex digits of bytes a column that is not binary sent (text that is not valid UTF-8): a SQL file writes them as
    /// bytes, the other formats as the hex text.
    Bytes(&'a str),
}

/// What a file ended up as.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(in crate::app) struct FileSummary {
    pub(in crate::app) rows: u64,
    pub(in crate::app) size: u64,
    /// Cells cut to the longest an XLSX cell holds.
    pub(in crate::app) truncated: u64,
}

/// An XLSX cell holds this many characters.
pub(in crate::app) const XLSX_MAX_CELL_CHARS: usize = 32_767;
/// An XLSX sheet holds this many columns.
pub(in crate::app) const XLSX_MAX_COLUMNS: usize = 16_384;

/// The part file of a sink: removed when it is dropped, unless `finish` renamed it away.
struct Part(Option<PathBuf>);

impl Part {
    fn path(&self) -> Option<&Path> {
        self.0.as_deref()
    }

    fn renamed(&mut self) {
        self.0 = None;
    }
}

impl Drop for Part {
    fn drop(&mut self) {
        if let Some(path) = self.0.take() {
            let _ = std::fs::remove_file(path);
        }
    }
}

/// A part file's path beside `path`: a name nobody else has.
fn part_path(path: &Path) -> PathBuf {
    let name = path.file_name().map(|name| name.to_string_lossy().into_owned()).unwrap_or_default();
    path.with_file_name(format!(".{name}.{}.part", uuid::Uuid::new_v4().to_simple()))
}

/// One file being written.
pub(in crate::app) struct FileSink {
    /// Where the file ends up; a text format is written to its part file until `finish`.
    path: PathBuf,
    part: Part,
    overwrite: bool,
    body: Body,
    columns: Vec<SinkColumn>,
    kinds: Vec<Kind>,
    rows: u64,
    truncated: u64,
}

impl FileSink {
    /// Opens `path`. `Err` when it exists and `overwrite` is off, or when a name or setting cannot be written.
    pub(in crate::app) fn create(
        path: &Path,
        format: FileFormat,
        columns: &[SinkColumn],
        dsv: &DsvOptions,
        sql: Option<SqlStyle<'_>>,
        overwrite: bool,
    ) -> Result<Self, SinkError> {
        if !overwrite && path.exists() {
            return Err(SinkError::Exists(path.to_owned()));
        }
        let io = |source| SinkError::Io { path: path.to_owned(), source };
        let kinds: Vec<Kind> = columns.iter().map(|column| Kind::of(&column.data_type)).collect();
        let names: Vec<&str> = columns.iter().map(|column| column.name.as_str()).collect();
        let part_file = part_path(path);
        let mut guard = Part(None);
        let mut open = || -> Result<BufWriter<File>, SinkError> {
            // `create_new`: a file that is there, whatever its name, is never truncated.
            let file = std::fs::OpenOptions::new().write(true).create_new(true).open(&part_file).map_err(io)?;
            guard.0 = Some(part_file.clone());
            Ok(BufWriter::with_capacity(BUFFER_BYTES, file))
        };
        let body = match format {
            FileFormat::Csv => {
                dsv.validate().map_err(SinkError::Refused)?;
                let mut out = open()?;
                if dsv.include_headers {
                    let header: Vec<_> = names.iter().map(|name| dsv.field(name)).collect();
                    out.write_all(header.join(dsv.column_separator.as_str()).as_bytes()).and_then(|()| out.write_all(dsv.row_separator.as_bytes())).map_err(io)?;
                }
                Body::Csv { out, dsv: dsv.clone() }
            }
            FileFormat::Json => {
                // Repeated names are made unique in the order they come (`id`, `id_2`), like the JSON of a copy.
                let (unique, _) = unique_column_names(names.iter().copied());
                let keys = unique.iter().map(|name| format!("{}:", serde_json::to_string(name).unwrap_or_default())).collect();
                let mut out = open()?;
                out.write_all(b"[").map_err(io)?;
                Body::Json { out, keys }
            }
            FileFormat::SqlInsert => {
                let style = sql.ok_or_else(|| SinkError::Refused("A SQL file needs the dialect it is written for.".to_owned()))?;
                let quote = |name: &str| style.target.quote(name).map_err(SinkError::Refused);
                let table = match style.schema.filter(|schema| !schema.is_empty()) {
                    Some(schema) => format!("{}.{}", quote(schema)?, quote(style.table)?),
                    None => quote(style.table)?,
                };
                let list = names.iter().map(|name| quote(name)).collect::<Result<Vec<_>, _>>()?.join(", ");
                Body::Sql { out: open()?, target: style.target, head: format!("INSERT INTO {table} ({list}) VALUES\n"), in_statement: 0 }
            }
            FileFormat::Xlsx => {
                if columns.len() > XLSX_MAX_COLUMNS {
                    return Err(SinkError::Refused(format!("An XLSX sheet holds at most {XLSX_MAX_COLUMNS} columns.")));
                }
                // Saved to the part file when it is finished, and renamed like the text formats.
                let exporter = TableExporter::create(&part_file, ExportFormat::Xlsx, ExportDialect::Postgres, String::new(), columns.iter().map(|column| column.name.clone()).collect())
                    .map_err(SinkError::Xlsx)?;
                Body::Xlsx { exporter: Box::new(exporter) }
            }
        };
        if format == FileFormat::Xlsx {
            guard.0 = Some(part_file);
        }
        Ok(Self { path: path.to_owned(), part: guard, overwrite, body, columns: columns.to_vec(), kinds, rows: 0, truncated: 0 })
    }

    /// Writes one row. `cells` has a cell per column.
    pub(in crate::app) fn write_row(&mut self, cells: &[Cell<'_>]) -> Result<(), SinkError> {
        let path = &self.path;
        let io = |source| SinkError::Io { path: path.clone(), source };
        match &mut self.body {
            Body::Csv { out, dsv } => {
                for (index, cell) in cells.iter().enumerate() {
                    if index > 0 {
                        out.write_all(dsv.column_separator.as_bytes()).map_err(io)?;
                    }
                    match cell {
                        Cell::Null => out.write_all(dsv.null_text.as_bytes()),
                        Cell::Text(text) | Cell::Bytes(text) => out.write_all(dsv.field(text).as_bytes()),
                    }
                    .map_err(io)?;
                }
                out.write_all(dsv.row_separator.as_bytes()).map_err(io)?;
            }
            Body::Json { out, keys } => {
                out.write_all(if self.rows == 0 { b"\n  {" } else { b",\n  {" }).map_err(io)?;
                for (index, cell) in cells.iter().enumerate() {
                    if index > 0 {
                        out.write_all(b", ").map_err(io)?;
                    }
                    out.write_all(keys[index].as_bytes()).map_err(io)?;
                    write_json_value(out, *cell, self.kinds[index]).map_err(io)?;
                }
                out.write_all(b"}").map_err(io)?;
            }
            Body::Sql { out, target, head, in_statement } => {
                if *in_statement == 0 {
                    out.write_all(head.as_bytes()).map_err(io)?;
                } else {
                    out.write_all(b",\n").map_err(io)?;
                }
                out.write_all(b"(").map_err(io)?;
                for (index, cell) in cells.iter().enumerate() {
                    if index > 0 {
                        out.write_all(b", ").map_err(io)?;
                    }
                    let literal = match cell {
                        Cell::Null => sql_literal(None, "", *target),
                        Cell::Text(text) => sql_literal(Some(text), &self.columns[index].data_type, *target),
                        // Bytes are written as bytes, whatever the column is called.
                        Cell::Bytes(hex) => sql_literal(Some(hex), "blob", *target),
                    }
                    .map_err(SinkError::Refused)?;
                    out.write_all(literal.as_bytes()).map_err(io)?;
                }
                out.write_all(b")").map_err(io)?;
                *in_statement += 1;
                if *in_statement == SQL_ROWS_PER_STATEMENT {
                    out.write_all(b";\n").map_err(io)?;
                    *in_statement = 0;
                }
            }
            Body::Xlsx { exporter } => {
                if self.rows + 1 >= XLSX_MAX_ROWS {
                    return Err(SinkError::Xlsx(format!("a sheet holds {} rows at most; export to CSV instead", XLSX_MAX_ROWS - 1)));
                }
                // A cell holds 32,767 characters: more is cut, and counted, never a reason to lose the workbook.
                let mut cut = 0;
                let row: Vec<ExportCell<'_>> = cells
                    .iter()
                    .zip(&self.kinds)
                    .map(|(cell, kind)| match (cell, kind) {
                        (Cell::Null, _) => ExportCell::Null,
                        (Cell::Text(text), Kind::Number | Kind::Decimal) if is_json_number(text.trim()) => ExportCell::Number(text.trim()),
                        (Cell::Text(text), Kind::Boolean) => flag(text).map_or(ExportCell::Text(text), ExportCell::Bool),
                        (Cell::Text(text) | Cell::Bytes(text), _) => match text.char_indices().nth(XLSX_MAX_CELL_CHARS) {
                            Some((end, _)) => {
                                cut += 1;
                                ExportCell::Text(&text[..end])
                            }
                            None => ExportCell::Text(text),
                        },
                    })
                    .collect();
                self.truncated += cut;
                exporter.write_cells(&row).map_err(SinkError::Xlsx)?;
            }
        }
        self.rows += 1;
        Ok(())
    }

    /// Ends the file: the closing text, the flush, the rename. Returns the rows and the size of the file.
    pub(in crate::app) fn finish(self) -> Result<FileSummary, SinkError> {
        // Whatever fails below, `part` is dropped with the part file still there and removes it.
        let Self { path, mut part, overwrite, body, rows, truncated, .. } = self;
        let io = |source| SinkError::Io { path: path.clone(), source };
        match body {
            Body::Csv { mut out, .. } => out.flush().map_err(io)?,
            Body::Json { mut out, .. } => {
                out.write_all(if rows == 0 { b"]\n" } else { b"\n]\n" }).and_then(|()| out.flush()).map_err(io)?;
            }
            Body::Sql { mut out, in_statement, .. } => {
                if in_statement > 0 {
                    out.write_all(b";\n").map_err(io)?;
                }
                out.flush().map_err(io)?;
            }
            Body::Xlsx { exporter } => {
                exporter.finish().map_err(SinkError::Xlsx)?;
            }
        }
        if let Some(temp) = part.path() {
            // A file that appeared while the export ran is not replaced unless replacing was asked for; a rename puts the new
            // file over an old one in one step.
            if !overwrite && path.exists() {
                return Err(SinkError::Exists(path));
            }
            std::fs::rename(temp, &path).map_err(io)?;
            part.renamed();
        }
        let size = std::fs::metadata(&path).map(|meta| meta.len()).unwrap_or(0);
        Ok(FileSummary { rows, size, truncated })
    }

    /// The export stopped: what was written is removed.
    pub(in crate::app) fn abandon(self) {
        drop(self);
    }
}

fn write_json_value(out: &mut impl Write, cell: Cell<'_>, kind: Kind) -> std::io::Result<()> {
    let text = match cell {
        Cell::Null => return out.write_all(b"null"),
        Cell::Bytes(hex) => return serde_json::to_writer(out, hex).map_err(Into::into),
        Cell::Text(text) => text,
    };
    match kind {
        Kind::Number if is_json_number(text.trim()) => out.write_all(text.trim().as_bytes()),
        Kind::Boolean => match flag(text) {
            Some(value) => out.write_all(if value { b"true" } else { b"false" }),
            None => serde_json::to_writer(out, text).map_err(Into::into),
        },
        Kind::Json if serde_json::from_str::<serde::de::IgnoredAny>(text).is_ok() => out.write_all(text.as_bytes()),
        _ => serde_json::to_writer(out, text).map_err(Into::into),
    }
}

/// A file name for an object: its own name where that is a safe name, never a path that leaves the folder, never
/// empty, never a name Windows reserves.
pub(in crate::app) fn safe_file_stem(name: &str) -> String {
    let mut stem: String = name
        .chars()
        .map(|character| if character.is_control() || matches!(character, '/' | '\\' | ':' | '*' | '?' | '"' | '<' | '>' | '|') { '_' } else { character })
        .collect();
    stem = stem.trim().trim_matches('.').to_owned();
    if stem.is_empty() {
        stem = "_".to_owned();
    }
    let upper = stem.to_ascii_uppercase();
    let head = upper.split('.').next().unwrap_or_default();
    let reserved = matches!(head, "CON" | "PRN" | "AUX" | "NUL")
        || (head.len() == 4 && (head.starts_with("COM") || head.starts_with("LPT")) && head.as_bytes()[3].is_ascii_digit());
    if reserved {
        stem.insert(0, '_');
    }
    // One component of a path stays within what file systems take.
    if stem.len() > 120 {
        let mut end = 120;
        while !stem.is_char_boundary(end) {
            end -= 1;
        }
        stem.truncate(end);
    }
    stem
}

/// What a file system takes for one name: macOS and Windows tell `é` written as one character from `e` and an accent only by
/// their bytes, and `A` from `a` not at all.
fn folded(name: &str) -> String {
    icu_normalizer::ComposingNormalizer::new_nfc().normalize(name).to_lowercase()
}

/// The file names of `names` in `format`, each unique within the list (two names that read the same once made safe, or
/// that differ by case or by Unicode normalization on a file system that does not tell, get a number).
pub(in crate::app) fn file_names(names: &[String], format: FileFormat) -> Vec<String> {
    let mut taken = std::collections::HashSet::new();
    names
        .iter()
        .map(|name| {
            let stem = safe_file_stem(name);
            let mut candidate = format!("{stem}.{}", format.extension());
            let mut number = 1;
            while !taken.insert(folded(&candidate)) {
                number += 1;
                candidate = format!("{stem}_{number}.{}", format.extension());
            }
            candidate
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cells<'a>(row: &[Option<&'a str>]) -> Vec<Cell<'a>> {
        row.iter().map(|cell| cell.map_or(Cell::Null, Cell::Text)).collect()
    }

    fn columns() -> Vec<SinkColumn> {
        [("id", "bigint"), ("name", "text"), ("price", "numeric(10,2)"), ("ok", "boolean"), ("doc", "jsonb"), ("raw", "bytea")]
            .iter()
            .map(|(name, data_type)| SinkColumn { name: (*name).to_owned(), data_type: (*data_type).to_owned() })
            .collect()
    }

    /// The part files in `dir`.
    fn parts_in(dir: &Path) -> Vec<PathBuf> {
        std::fs::read_dir(dir).unwrap().filter_map(|entry| entry.ok().map(|entry| entry.path())).filter(|path| path.extension().is_some_and(|extension| extension == "part")).collect()
    }

    #[test]
    fn a_file_the_user_has_named_like_a_part_file_is_never_touched_and_a_dropped_sink_leaves_nothing() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("x.csv");
        let theirs = dir.path().join("x.csv.part");
        std::fs::write(&theirs, "theirs").unwrap();
        let mut sink = FileSink::create(&path, FileFormat::Csv, &columns(), &DsvOptions::default(), None, false).unwrap();
        sink.write_row(&cells(&[Some("1"), None, None, None, None, None])).unwrap();
        assert_eq!(parts_in(dir.path()).len(), 2, "a part file of its own beside theirs");
        drop(sink);
        assert_eq!(std::fs::read_to_string(&theirs).unwrap(), "theirs");
        assert_eq!(parts_in(dir.path()), std::slice::from_ref(&theirs), "the sink's own part went with it");
        let sink = FileSink::create(&path, FileFormat::Csv, &columns(), &DsvOptions::default(), None, false).unwrap();
        sink.finish().unwrap();
        assert_eq!(std::fs::read_to_string(&theirs).unwrap(), "theirs");
        assert!(path.exists());
    }

    fn rows() -> Vec<[Option<&'static str>; 6]> {
        vec![
            [Some("1"), Some("Ann, \"A\""), Some("12.50"), Some("true"), Some("{\"a\": [1, 2]}"), Some("0x00ff")],
            [Some("2"), None, None, Some("f"), Some("not json"), None],
            [Some("3"), Some(""), Some("NaN"), Some("maybe"), Some("[]"), Some("0x")],
            [Some("4"), Some("null"), Some("1e3"), None, None, Some("0xDEAD")],
        ]
    }

    fn write(format: FileFormat, dsv: &DsvOptions, sql: Option<SqlStyle<'_>>) -> String {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join(format!("t.{}", format.extension()));
        let mut sink = FileSink::create(&path, format, &columns(), dsv, sql, false).unwrap();
        for row in rows() {
            sink.write_row(&cells(&row)).unwrap();
        }
        let FileSummary { rows: count, size, .. } = sink.finish().unwrap();
        assert_eq!(count, 4);
        assert_eq!(size, std::fs::metadata(&path).unwrap().len());
        assert!(parts_in(dir.path()).is_empty(), "the partial file is renamed away");
        std::fs::read_to_string(&path).unwrap()
    }

    #[test]
    fn csv_quotes_like_the_copy_dialog_and_tells_null_from_an_empty_text() {
        let dsv = DsvOptions { include_headers: true, ..Default::default() };
        let text = write(FileFormat::Csv, &dsv, None);
        assert_eq!(
            text,
            "id,name,price,ok,doc,raw\n\
             1,\"Ann, \"\"A\"\"\",12.50,true,\"{\"\"a\"\": [1, 2]}\",0x00ff\n\
             2,,,f,not json,\n\
             3,\"\",NaN,maybe,[],0x\n\
             4,null,1e3,,,0xDEAD\n"
        );
        // A NULL text of its own, and a value equal to it is quoted so it cannot be mistaken for a NULL.
        let dsv = DsvOptions { include_headers: false, null_text: "null".into(), column_separator: ";".into(), ..Default::default() };
        let text = write(FileFormat::Csv, &dsv, None);
        assert!(text.contains("2;null;null;f;not json;null\n") && text.contains("4;\"null\";1e3;null;null;0xDEAD\n"), "{text}");
    }

    #[test]
    fn json_keeps_numbers_numbers_text_text_and_null_null() {
        let text = write(FileFormat::Json, &DsvOptions::default(), None);
        let value: serde_json::Value = serde_json::from_str(&text).unwrap();
        let rows = value.as_array().unwrap();
        assert_eq!(rows.len(), 4);
        assert_eq!(rows[0], serde_json::json!({"id": 1, "name": "Ann, \"A\"", "price": "12.50", "ok": true, "doc": {"a": [1, 2]}, "raw": "0x00ff"}));
        assert_eq!(rows[1], serde_json::json!({"id": 2, "name": null, "price": null, "ok": false, "doc": "not json", "raw": null}));
        assert_eq!(rows[2], serde_json::json!({"id": 3, "name": "", "price": "NaN", "ok": "maybe", "doc": [], "raw": "0x"}));
        assert_eq!(rows[3]["name"], "null", "the text null is not a NULL");
    }

    #[test]
    fn an_empty_json_file_is_an_empty_array() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("e.json");
        let sink = FileSink::create(&path, FileFormat::Json, &columns(), &DsvOptions::default(), None, false).unwrap();
        assert_eq!(sink.finish().unwrap().rows, 0);
        assert_eq!(serde_json::from_str::<serde_json::Value>(&std::fs::read_to_string(&path).unwrap()).unwrap(), serde_json::json!([]));
    }

    #[test]
    fn repeated_column_names_are_made_unique_in_json() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("d.json");
        let columns: Vec<SinkColumn> = ["id", "id"].iter().map(|name| SinkColumn { name: (*name).into(), data_type: "int".into() }).collect();
        let mut sink = FileSink::create(&path, FileFormat::Json, &columns, &DsvOptions::default(), None, false).unwrap();
        sink.write_row(&cells(&[Some("1"), Some("2")])).unwrap();
        sink.finish().unwrap();
        assert_eq!(serde_json::from_str::<serde_json::Value>(&std::fs::read_to_string(&path).unwrap()).unwrap(), serde_json::json!([{"id": 1, "id_2": 2}]));
    }

    #[test]
    fn sql_inserts_use_the_quoting_and_literals_of_the_target_dialect() {
        use crate::sql_table::{BinaryLiteral, SqlDialect};
        let postgres = SqlStyle { target: SqlTarget { dialect: SqlDialect::POSTGRES, binary: BinaryLiteral::Postgres }, schema: Some("public"), table: "we\"ird" };
        let text = write(FileFormat::SqlInsert, &DsvOptions::default(), Some(postgres));
        assert!(text.starts_with("INSERT INTO \"public\".\"we\"\"ird\" (\"id\", \"name\", \"price\", \"ok\", \"doc\", \"raw\") VALUES\n"), "{text}");
        assert!(text.contains("(1, 'Ann, \"A\"', 12.50, TRUE, '{\"a\": [1, 2]}', decode('00ff', 'hex')),\n"), "{text}");
        assert!(text.contains("(2, NULL, NULL, FALSE, 'not json', NULL),\n"), "{text}");
        assert!(text.contains("(3, '', 'NaN', 'maybe', '[]', decode('', 'hex')),\n"), "{text}");
        assert!(text.ends_with("(4, 'null', 1e3, NULL, NULL, decode('DEAD', 'hex'));\n"), "{text}");

        let mysql = SqlStyle { target: SqlTarget { dialect: SqlDialect::MYSQL, binary: BinaryLiteral::HexString }, schema: Some("shop"), table: "o`rders" };
        let text = write(FileFormat::SqlInsert, &DsvOptions::default(), Some(mysql));
        assert!(text.starts_with("INSERT INTO `shop`.`o``rders` (`id`, `name`"), "{text}");
        assert!(text.contains("X'00ff'"), "{text}");
    }

    #[test]
    fn a_name_the_dialect_cannot_quote_is_refused_before_a_file_is_made() {
        use crate::sql_table::{BinaryLiteral, SqlDialect};
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("x.sql");
        let clickhouse = SqlStyle { target: SqlTarget { dialect: SqlDialect::CLICKHOUSE, binary: BinaryLiteral::ClickHouse }, schema: None, table: "back\\slash" };
        let error = FileSink::create(&path, FileFormat::SqlInsert, &columns(), &DsvOptions::default(), Some(clickhouse), false).err().expect("refused");
        assert!(error.to_string().contains("backslash"), "{error}");
        assert!(!path.exists() && parts_in(dir.path()).is_empty(), "nothing was created");
        let missing = FileSink::create(&path, FileFormat::SqlInsert, &columns(), &DsvOptions::default(), None, false).err().expect("refused");
        assert!(missing.to_string().contains("dialect"));
    }

    #[test]
    fn statements_are_cut_every_so_many_rows() {
        use crate::sql_table::{BinaryLiteral, SqlDialect};
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("n.sql");
        let style = SqlStyle { target: SqlTarget { dialect: SqlDialect::SQLITE, binary: BinaryLiteral::HexString }, schema: None, table: "n" };
        let columns = vec![SinkColumn { name: "v".into(), data_type: "integer".into() }];
        let mut sink = FileSink::create(&path, FileFormat::SqlInsert, &columns, &DsvOptions::default(), Some(style), false).unwrap();
        for value in 0..SQL_ROWS_PER_STATEMENT + 1 {
            sink.write_row(&cells(&[Some(&value.to_string())])).unwrap();
        }
        sink.finish().unwrap();
        let text = std::fs::read_to_string(&path).unwrap();
        assert_eq!(text.matches("INSERT INTO").count(), 2);
        assert!(text.ends_with("(500);\n") && text.contains("(499);\nINSERT INTO"), "{text}");
    }

    #[test]
    fn xlsx_files_are_written_and_the_row_limit_is_reported() {
        let text = {
            let dir = tempfile::tempdir().unwrap();
            let path = dir.path().join("t.xlsx");
            let mut sink = FileSink::create(&path, FileFormat::Xlsx, &columns(), &DsvOptions::default(), None, false).unwrap();
            for row in rows() {
                sink.write_row(&cells(&row)).unwrap();
            }
            let FileSummary { rows: count, size, .. } = sink.finish().unwrap();
            assert_eq!(count, 4);
            size
        };
        assert!(text > 500, "a workbook of some size: {text}");
    }

    #[test]
    fn an_existing_file_is_kept_unless_replacing_is_asked_for_and_a_failed_export_leaves_nothing() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("t.csv");
        std::fs::write(&path, "keep me").unwrap();
        let dsv = DsvOptions::default();
        assert!(matches!(FileSink::create(&path, FileFormat::Csv, &columns(), &dsv, None, false), Err(SinkError::Exists(_))));
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "keep me");
        // Replacing: the old file stays until the new one is complete.
        let mut sink = FileSink::create(&path, FileFormat::Csv, &columns(), &dsv, None, true).unwrap();
        sink.write_row(&cells(&[Some("1"), None, None, None, None, None])).unwrap();
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "keep me");
        sink.abandon();
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "keep me");
        assert!(parts_in(dir.path()).is_empty());
        let mut sink = FileSink::create(&path, FileFormat::Csv, &columns(), &dsv, None, true).unwrap();
        sink.write_row(&cells(&[Some("1"), None, None, None, None, None])).unwrap();
        sink.finish().unwrap();
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "1,,,,,\n");
    }

    #[test]
    fn json_numbers_follow_the_json_grammar() {
        for good in ["0", "-1", "12.5", "1e3", "-0.5E-2", "10"] {
            assert!(is_json_number(good), "{good}");
        }
        for bad in ["", "-", "01", "1.", ".5", "NaN", "Infinity", "1e", "+1", "0x10", "1,5", " 1"] {
            assert!(!is_json_number(bad), "{bad:?}");
        }
    }

    #[test]
    fn a_name_becomes_a_file_name_that_stays_in_its_folder() {
        for (name, expected) in [
            ("orders", "orders"),
            ("../../etc/passwd", "_.._etc_passwd"),
            ("a/b\\c:d", "a_b_c_d"),
            ("", "_"),
            ("..", "_"),
            ("CON", "_CON"),
            ("com1.txt", "_com1.txt"),
            ("tab\tname\n", "tab_name_"),
            (" spaced ", "spaced"),
        ] {
            let stem = safe_file_stem(name);
            assert_eq!(stem, expected, "{name:?}");
            assert!(!stem.contains(['/', '\\']) && stem != "." && stem != "..");
        }
        assert!(safe_file_stem(&"é".repeat(200)).len() <= 120);
        assert_eq!(file_names(&["a/b".to_owned(), "a_b".to_owned(), "A_B".to_owned(), "c".to_owned()], FileFormat::Csv), ["a_b.csv", "a_b_2.csv", "A_B_3.csv", "c.csv"]);
    }

    #[test]
    fn bytes_in_a_text_column_are_bytes_in_a_sql_file_and_hex_text_elsewhere() {
        use crate::sql_table::{BinaryLiteral, SqlDialect};
        let dir = tempfile::tempdir().unwrap();
        let columns = vec![SinkColumn { name: "t".into(), data_type: "text".into() }];
        let postgres = SqlStyle { target: SqlTarget { dialect: SqlDialect::POSTGRES, binary: BinaryLiteral::Postgres }, schema: None, table: "x" };
        let path = dir.path().join("b.sql");
        let mut sink = FileSink::create(&path, FileFormat::SqlInsert, &columns, &DsvOptions::default(), Some(postgres), false).unwrap();
        sink.write_row(&[Cell::Bytes("0xff00")]).unwrap();
        sink.write_row(&[Cell::Text("0xff00")]).unwrap();
        sink.finish().unwrap();
        assert!(std::fs::read_to_string(&path).unwrap().ends_with("(decode('ff00', 'hex')),\n('0xff00');\n"));
        let path = dir.path().join("b.json");
        let mut sink = FileSink::create(&path, FileFormat::Json, &columns, &DsvOptions::default(), None, false).unwrap();
        sink.write_row(&[Cell::Bytes("0xff00")]).unwrap();
        sink.finish().unwrap();
        assert_eq!(serde_json::from_str::<serde_json::Value>(&std::fs::read_to_string(&path).unwrap()).unwrap(), serde_json::json!([{"t": "0xff00"}]));
    }

    #[test]
    fn an_xlsx_cell_is_cut_at_its_limit_and_the_workbook_goes_on_and_a_wide_sheet_is_refused_at_the_start() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("w.xlsx");
        let columns = vec![SinkColumn { name: "t".into(), data_type: "text".into() }, SinkColumn { name: "n".into(), data_type: "int".into() }];
        let mut sink = FileSink::create(&path, FileFormat::Xlsx, &columns, &DsvOptions::default(), None, false).unwrap();
        let long = "é".repeat(XLSX_MAX_CELL_CHARS + 5);
        sink.write_row(&[Cell::Text(&long), Cell::Text("1")]).unwrap();
        sink.write_row(&[Cell::Text(&"x".repeat(XLSX_MAX_CELL_CHARS)), Cell::Null]).unwrap();
        let summary = sink.finish().unwrap();
        assert_eq!((summary.rows, summary.truncated), (2, 1), "only the cell past the limit is cut");
        assert!(path.exists() && parts_in(dir.path()).is_empty(), "written to a part file and renamed");
        // Exactly the columns a sheet holds are fine, one more is refused before anything is made.
        let wide = |count: usize| (0..count).map(|index| SinkColumn { name: format!("c{index}"), data_type: "text".into() }).collect::<Vec<_>>();
        let path = dir.path().join("x.xlsx");
        assert!(FileSink::create(&path, FileFormat::Xlsx, &wide(XLSX_MAX_COLUMNS), &DsvOptions::default(), None, false).is_ok());
        assert!(matches!(FileSink::create(&path, FileFormat::Xlsx, &wide(XLSX_MAX_COLUMNS + 1), &DsvOptions::default(), None, false), Err(SinkError::Refused(message)) if message.contains("16384")));
    }

    #[test]
    fn a_file_that_appeared_during_the_export_is_not_replaced_and_no_part_is_left() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("race.csv");
        let sink = FileSink::create(&path, FileFormat::Csv, &columns(), &DsvOptions::default(), None, false).unwrap();
        std::fs::write(&path, "someone else's").unwrap();
        assert!(matches!(sink.finish(), Err(SinkError::Exists(_))));
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "someone else's");
        assert!(parts_in(dir.path()).is_empty());
        // With replacing asked for, the rename puts the new file over the old one.
        let sink = FileSink::create(&path, FileFormat::Csv, &columns(), &DsvOptions::default(), None, true).unwrap();
        sink.finish().unwrap();
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "");
    }

    #[test]
    fn names_a_file_system_cannot_tell_apart_get_a_number() {
        // `é` as one character and as `e` and a combining accent, and a case difference.
        let names = vec!["caf\u{e9}".to_owned(), "cafe\u{301}".to_owned(), "CAF\u{c9}".to_owned(), "other".to_owned()];
        assert_eq!(file_names(&names, FileFormat::Csv), ["caf\u{e9}.csv", "cafe\u{301}_2.csv", "CAF\u{c9}_3.csv", "other.csv"]);
    }
}

