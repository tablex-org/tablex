//! Export Data of an aggregation: one aggregate cursor, spooled to disk, then
//! written through the existing `export::TableExporter`.
//!
//! The columns of a Mongo result are the union of every document's keys, and
//! `TableExporter` needs them up front. The previous export took the columns
//! of the first batch (later keys were silently dropped) and swallowed cursor
//! errors. Here the single cursor is read to the end first, each document
//! going to a BSON spool file next to the destination while the column union
//! is collected; only then the spool is replayed into the exporter. Memory
//! stays bounded (a spool read is one document at a time), no document is
//! dropped, and any error - cursor, disk, format - removes the partial
//! output and is returned.
use crate::export::{ExportDialect, ExportFormat, TableExporter};
use bson::Document;
use std::fs::File;
use std::io::{BufReader, BufWriter, Write};
use std::path::{Path, PathBuf};

/// Rows per `TableExporter::write_rows` call while replaying the spool.
const REPLAY_CHUNK: usize = 1000;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct ExportSummary {
    pub rows: u64,
    pub bytes: u64,
}

/// Formats `dbengine_export` accepts for a Mongo result.
pub(crate) fn parse_format(text: &str) -> Result<ExportFormat, String> {
    match ExportFormat::parse(text)? {
        ExportFormat::SqlInsert => Err("SQL INSERT export is only available for SQL databases".into()),
        format => Ok(format),
    }
}

/// A hidden sibling of `destination` (same directory, so a rename is atomic).
fn sibling_path(destination: &Path, kind: &str) -> PathBuf {
    static COUNTER: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    let name = destination
        .file_name()
        .map(|name| name.to_string_lossy().into_owned())
        .unwrap_or_else(|| "export".into());
    destination.with_file_name(format!(
        ".{name}.{kind}-{}-{}",
        std::process::id(),
        COUNTER.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
    ))
}

struct Spool {
    path: PathBuf,
    writer: Option<BufWriter<File>>,
    columns: Vec<String>,
    rows: u64,
}

impl Spool {
    fn create(path: PathBuf) -> Result<Self, String> {
        let file = File::create(&path).map_err(|error| format!("cannot create export file: {error}"))?;
        Ok(Self {
            path,
            writer: Some(BufWriter::new(file)),
            columns: Vec::new(),
            rows: 0,
        })
    }

    fn push(&mut self, document: &Document) -> Result<(), String> {
        for key in document.keys() {
            if !self.columns.iter().any(|column| column == key) {
                self.columns.push(key.clone());
            }
        }
        let bytes = bson::to_vec(document).map_err(|error| error.to_string())?;
        let writer = self.writer.as_mut().ok_or("export spool is closed")?;
        writer
            .write_all(&bytes)
            .map_err(|error| format!("cannot write export data: {error}"))?;
        self.rows += 1;
        Ok(())
    }

    fn finish_writing(&mut self) -> Result<(), String> {
        if let Some(mut writer) = self.writer.take() {
            writer
                .flush()
                .map_err(|error| format!("cannot write export data: {error}"))?;
        }
        Ok(())
    }
}

impl Drop for Spool {
    fn drop(&mut self) {
        self.writer = None;
        let _ = std::fs::remove_file(&self.path);
    }
}

/// Runs `feed` once (it must push every document into the sink) and writes
/// the result to `destination` as `format`. The output is built in a temporary
/// file next to `destination` and renamed over it only on success, so a
/// failure leaves nothing partial behind **and never touches a file that was
/// already at `destination`**.
pub(crate) fn export_documents(
    format: ExportFormat,
    destination: &Path,
    table: &str,
    feed: impl FnOnce(&mut (dyn FnMut(Document) -> Result<(), String> + Send)) -> Result<(), String>,
) -> Result<ExportSummary, String> {
    if let Some(parent) = destination.parent().filter(|parent| !parent.as_os_str().is_empty()) {
        std::fs::create_dir_all(parent)
            .map_err(|error| format!("cannot create export folder: {error}"))?;
    }
    let partial = sibling_path(destination, "partial");
    let result = export_inner(format, &partial, table, feed).and_then(|summary| {
        std::fs::rename(&partial, destination)
            .map_err(|error| format!("cannot write {}: {error}", destination.display()))?;
        Ok(summary)
    });
    if result.is_err() {
        let _ = std::fs::remove_file(&partial);
    }
    result
}

fn export_inner(
    format: ExportFormat,
    destination: &Path,
    table: &str,
    feed: impl FnOnce(&mut (dyn FnMut(Document) -> Result<(), String> + Send)) -> Result<(), String>,
) -> Result<ExportSummary, String> {
    let mut spool = Spool::create(sibling_path(destination, "spool"))?;
    feed(&mut |document| spool.push(&document))?;
    spool.finish_writing()?;
    let columns = spool.columns.clone();
    let mut exporter =
        TableExporter::create(destination, format, ExportDialect::Mongo, table.to_string(), columns.clone())?;
    let mut reader = BufReader::new(File::open(&spool.path).map_err(|error| error.to_string())?);
    let mut chunk: Vec<Vec<String>> = Vec::with_capacity(REPLAY_CHUNK);
    for _ in 0..spool.rows {
        let document = Document::from_reader(&mut reader).map_err(|error| error.to_string())?;
        chunk.push(crate::mongo_export_row(&document, &columns));
        if chunk.len() == REPLAY_CHUNK {
            exporter.write_rows(&chunk)?;
            chunk.clear();
        }
    }
    if !chunk.is_empty() {
        exporter.write_rows(&chunk)?;
    }
    let rows = exporter.finish()? as u64;
    let bytes = std::fs::metadata(destination).map(|meta| meta.len()).unwrap_or(0);
    Ok(ExportSummary { rows, bytes })
}

#[cfg(test)]
mod tests {
    use super::*;
    use bson::doc;

    fn temp_dir(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("tablex-agg-export-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn feed(documents: Vec<Document>) -> impl FnOnce(&mut (dyn FnMut(Document) -> Result<(), String> + Send)) -> Result<(), String> {
        move |sink| {
            for document in documents {
                sink(document)?;
            }
            Ok(())
        }
    }

    #[test]
    fn csv_columns_are_the_union_over_every_document_not_just_the_first_batch() {
        let dir = temp_dir("csv");
        let path = dir.join("out.csv");
        let summary = export_documents(
            ExportFormat::Csv,
            &path,
            "orders",
            feed(vec![
                doc! { "_id": 1, "a": "x" },
                doc! { "_id": 2, "a": "y", "late": "kept" },
                doc! { "_id": 3, "nested": { "k": [1, 2] } },
            ]),
        )
        .unwrap();
        assert_eq!(summary.rows, 3);
        assert!(summary.bytes > 0);
        let text = std::fs::read_to_string(&path).unwrap();
        let mut lines = text.lines();
        assert_eq!(lines.next().unwrap(), "_id,a,late,nested");
        assert_eq!(lines.next().unwrap(), "1,x,,");
        assert!(text.contains("2,y,kept,"), "{text}");
        assert!(text.contains("\"k\""), "nested values are exported as JSON: {text}");
        let leftovers: Vec<_> = std::fs::read_dir(&dir).unwrap().flatten().map(|entry| entry.file_name()).collect();
        assert_eq!(leftovers.len(), 1, "the spool file is removed: {leftovers:?}");
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn json_and_xlsx_are_written() {
        let dir = temp_dir("fmt");
        let json = dir.join("out.json");
        export_documents(ExportFormat::Json, &json, "c", feed(vec![doc! { "_id": 1, "n": 2 }])).unwrap();
        let value: serde_json::Value = serde_json::from_str(&std::fs::read_to_string(&json).unwrap()).unwrap();
        assert_eq!(value.as_array().unwrap().len(), 1);
        let xlsx = dir.join("out.xlsx");
        let summary = export_documents(ExportFormat::Xlsx, &xlsx, "c", feed(vec![doc! { "_id": 1 }])).unwrap();
        assert_eq!(summary.rows, 1);
        assert!(std::fs::metadata(&xlsx).unwrap().len() > 0);
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn an_empty_result_still_produces_a_valid_file() {
        let dir = temp_dir("empty");
        let path = dir.join("out.csv");
        let summary = export_documents(ExportFormat::Csv, &path, "c", feed(vec![])).unwrap();
        assert_eq!(summary.rows, 0);
        assert!(path.exists());
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn a_cursor_error_mid_stream_fails_the_export_and_leaves_no_files() {
        let dir = temp_dir("fail");
        let path = dir.join("out.csv");
        let error = export_documents(ExportFormat::Csv, &path, "c", |sink| {
            sink(doc! { "_id": 1 })?;
            sink(doc! { "_id": 2 })?;
            Err("cursor error: connection reset".to_string())
        })
        .unwrap_err();
        assert!(error.contains("connection reset"), "{error}");
        assert!(!path.exists(), "no partial output");
        assert_eq!(std::fs::read_dir(&dir).unwrap().count(), 0, "no spool left behind");
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn a_failed_export_never_touches_an_existing_destination_and_success_replaces_it() {
        let dir = temp_dir("existing");
        let path = dir.join("out.csv");
        std::fs::write(&path, "precious,data\n1,2\n").unwrap();
        let error = export_documents(ExportFormat::Csv, &path, "c", |sink| {
            sink(doc! { "_id": 1 })?;
            Err("cursor error: gone".to_string())
        })
        .unwrap_err();
        assert!(error.contains("gone"));
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "precious,data\n1,2\n", "untouched on failure");
        assert_eq!(std::fs::read_dir(&dir).unwrap().count(), 1, "no partial / spool files");
        export_documents(ExportFormat::Csv, &path, "c", feed(vec![doc! { "_id": 7 }])).unwrap();
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "_id\n7\n");
        assert_eq!(std::fs::read_dir(&dir).unwrap().count(), 1);
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn replay_chunks_do_not_lose_rows() {
        let dir = temp_dir("chunk");
        let path = dir.join("out.csv");
        let documents: Vec<Document> = (0..(REPLAY_CHUNK as i32 * 2 + 7)).map(|id| doc! { "_id": id }).collect();
        let summary = export_documents(ExportFormat::Csv, &path, "c", feed(documents)).unwrap();
        assert_eq!(summary.rows, REPLAY_CHUNK as u64 * 2 + 7);
        assert_eq!(std::fs::read_to_string(&path).unwrap().lines().count(), REPLAY_CHUNK * 2 + 8);
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn unwritable_destinations_and_sql_format_are_refused() {
        // A destination below a regular file cannot be created on any OS (`/dev/null/...` is a
        // relative path that Windows happily creates).
        let dir = temp_dir("unwritable");
        let file = dir.join("not-a-folder");
        std::fs::write(&file, "x").unwrap();
        let error = export_documents(
            ExportFormat::Csv,
            &file.join("cannot").join("out.csv"),
            "c",
            feed(vec![]),
        )
        .unwrap_err();
        assert!(error.contains("cannot create"), "{error}");
        std::fs::remove_dir_all(dir).unwrap();
        assert!(parse_format("sql_insert").is_err());
        assert!(parse_format("csv").is_ok());
        assert!(parse_format("nope").is_err());
    }
}
