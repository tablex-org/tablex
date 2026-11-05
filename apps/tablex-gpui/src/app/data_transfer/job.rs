//! The transfer itself: the jobs of a plan one after the other, each reading its query page by page and writing the
//! rows to a table (the load of R4a, fed by the pages) or to a file. Nothing here knows about a window or a driver:
//! the feed of pages and the writer of batches are traits, so the whole run is tested with tables in memory.
//!
//! Errors, by who is wrong:
//! * a **row or batch** the target refuses follows the row policy of the load (`run::run`): stop, or skip and log;
//! * a **table** that cannot be transferred (the query is refused, the target has no such column, the file exists,
//!   the load stopped) ends the transfer when the policy is to stop, and is listed in the summary when it is to skip;
//! * **cancel**, and a source or target session that is gone, end the transfer whatever the policy.
//!
//! Every table is its own unit on the target: *commit per batch* or *one transaction* is a choice per table, so a
//! table that fails never takes back a table that was done.

use std::path::PathBuf;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Mutex;
use std::time::{Duration, Instant};

use arrow_array::RecordBatch;

use super::super::data_import::convert::Converter;
use super::super::data_import::run::{self, BatchWriter, ColumnPlan, Load, LoadSettings, OnError, Outcome, Progress, Report};
use super::super::data_import::TargetTable;
use super::reader::{holds_bytes, Cursor, PageFeed, Query, QueryRows, ReadError, SnapshotState};
use super::sink::{Cell, FileFormat, FileSink, FileSummary, SinkColumn, SinkError, SqlStyle};
use super::super::results::column_type::{base_type_head, value_kind, ValueKind};
use super::super::results::copy::{DsvOptions, SqlTarget};
use crate::grid_arrow::{ArrowCellReader, BinaryDisplay};
use crate::import::infer::Hints;

/// A column of a table job: the source column by name, the target column and its type.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(in crate::app) struct MappedColumn {
    pub(in crate::app) source: String,
    pub(in crate::app) target: String,
    pub(in crate::app) data_type: String,
}

/// Where a job writes.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(in crate::app) enum JobSink {
    Table { table: TargetTable, columns: Vec<MappedColumn> },
    File {
        path: PathBuf,
        /// The table the SQL file inserts into (its schema and name).
        sql_table: (Option<String>, String),
    },
}

/// Whether a column the server declares as `data_type` holds bytes.
fn declared_binary(data_type: &str) -> bool {
    value_kind(data_type) == ValueKind::Binary || base_type_head(data_type) == "bytea"
}

/// One source query and where its rows go.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(in crate::app) struct JobPlan {
    pub(in crate::app) label: String,
    pub(in crate::app) query: Query,
    /// The types the source reports by column name (the page's own are the fallback).
    pub(in crate::app) types: Vec<(String, String)>,
    pub(in crate::app) sink: JobSink,
}

/// How the files are written.
#[derive(Debug, Clone)]
pub(in crate::app) struct FileSettings {
    pub(in crate::app) format: FileFormat,
    pub(in crate::app) dsv: DsvOptions,
    pub(in crate::app) overwrite: bool,
    /// The dialect a SQL file is written for.
    pub(in crate::app) sql: SqlTarget,
}

#[derive(Debug, Clone)]
pub(in crate::app) struct TransferSettings {
    pub(in crate::app) load: LoadSettings,
    pub(in crate::app) file: FileSettings,
    /// Source and target are servers of one dialect: a date or timestamp the dialect wrote but this reader cannot parse
    /// (`0000-00-00`, `infinity`) goes through as it is.
    pub(in crate::app) same_dialect: bool,
}

/// What the window reads while the transfer runs.
#[derive(Debug)]
pub(in crate::app) struct TransferProgress {
    /// The job running now.
    pub(in crate::app) current: AtomicUsize,
    /// Counters per job.
    pub(in crate::app) jobs: Vec<Progress>,
    /// The report of each job that is over.
    pub(in crate::app) finished: Mutex<Vec<Option<JobResult>>>,
}

impl TransferProgress {
    pub(in crate::app) fn new(jobs: usize) -> Self {
        Self { current: AtomicUsize::new(0), jobs: (0..jobs).map(|_| Progress::default()).collect(), finished: Mutex::new(vec![None; jobs]) }
    }

    /// How each job that is over ended, without the lists of its errors (a window reads this at every report).
    pub(in crate::app) fn finished_lines(&self) -> Vec<Option<FinishedLine>> {
        self.finished
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .iter()
            .map(|result| {
                result.as_ref().map(|result| FinishedLine {
                    outcome: result.report.outcome.clone(),
                    rows_read: result.report.rows_read,
                    written: result.report.inserted,
                    skipped: result.report.errors_total,
                })
            })
            .collect()
    }
}

/// The numbers of a job that is over.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(in crate::app) struct FinishedLine {
    pub(in crate::app) outcome: Outcome,
    pub(in crate::app) rows_read: u64,
    pub(in crate::app) written: u64,
    pub(in crate::app) skipped: u64,
}

/// How a job ended.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(in crate::app) struct JobResult {
    pub(in crate::app) report: Report,
    /// The file a file job made and its size.
    pub(in crate::app) file: Option<(PathBuf, u64)>,
    /// Something about the result the user should know (cells that were cut, a time zone the server ignored).
    pub(in crate::app) note: Option<String>,
    /// Whether the table was read as one snapshot.
    pub(in crate::app) snapshot: SnapshotState,
}

/// The snapshot of a job that did not read.
const NOT_READ: SnapshotState = SnapshotState::Unavailable("nothing was read");

/// How the transfer ended.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(in crate::app) enum TransferOutcome {
    /// Every job ran; the ones that failed (with the policy to skip them) are in the results.
    Completed,
    /// A job failed and the policy is to stop: the rest did not run.
    Stopped(String),
    Cancelled,
    /// A session could not be opened or was lost.
    Failed(String),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(in crate::app) struct TransferReport {
    pub(in crate::app) outcome: TransferOutcome,
    /// One per job, `None` for a job that did not run.
    pub(in crate::app) jobs: Vec<Option<JobResult>>,
    pub(in crate::app) elapsed: Duration,
}

/// A writer that can be aimed at the table of the next job.
pub(in crate::app) trait TableWriter: BatchWriter {
    fn aim(&mut self, table: &TargetTable);
}

impl TableWriter for super::super::data_import::DriverWriter {
    fn aim(&mut self, table: &TargetTable) {
        super::super::data_import::DriverWriter::aim(self, table.clone());
    }
}

pub(in crate::app) struct TransferRun<'a> {
    pub(in crate::app) jobs: &'a [JobPlan],
    pub(in crate::app) settings: &'a TransferSettings,
    pub(in crate::app) progress: &'a TransferProgress,
    pub(in crate::app) cancelled: &'a (dyn Fn() -> bool + Sync),
    pub(in crate::app) notify: &'a (dyn Fn() + Sync),
}

fn failed_report(message: String, started: Instant) -> Report {
    Report {
        outcome: Outcome::Failed(message),
        inserted: 0,
        rolled_back: 0,
        skipped: 0,
        rows_read: 0,
        errors: Vec::new(),
        errors_total: 0,
        uncertain: 0,
        bytes: 0,
        elapsed: started.elapsed(),
    }
}

/// The index of `name` among the columns of a page: exact, else without regard to case.
fn column_index(columns: &[engine_driver_api::PageColumn], name: &str) -> Option<usize> {
    columns.iter().position(|column| column.name == name).or_else(|| columns.iter().position(|column| column.name.eq_ignore_ascii_case(name)))
}

/// One table job: the rows of the query through the load.
fn transfer_table<F: PageFeed, W: TableWriter>(
    run: &TransferRun<'_>,
    index: usize,
    feed: &mut F,
    writer: &mut W,
    table: &TargetTable,
    mapped: &[MappedColumn],
) -> Report {
    let started = Instant::now();
    let job = &run.jobs[index];
    let progress = &run.progress.jobs[index];
    writer.aim(table);
    let mut rows = QueryRows::new(feed, job.query.clone(), progress.bytes.clone());
    // Bytes a column the source calls text sent cannot go to a column that is text: those rows are refused, one by one.
    rows.refuse_bytes_in(
        mapped
            .iter()
            .filter(|column| !declared_binary(&column.data_type))
            .filter(|column| job.types.iter().find(|(name, _)| *name == column.source).is_some_and(|(_, data_type)| !declared_binary(data_type)))
            .map(|column| column.source.clone())
            .collect(),
    );
    let columns = match rows.open() {
        Ok(columns) => columns.to_vec(),
        Err(ReadError::Cancelled) => return Report { outcome: Outcome::Cancelled, ..failed_report(String::new(), started) },
        Err(error) => return failed_report(error.to_string(), started),
    };
    let mut plans = Vec::with_capacity(mapped.len());
    for column in mapped {
        let Some(source) = column_index(&columns, &column.source) else {
            return failed_report(format!("The column \"{}\" is not in the result any more: go back and map the table again.", column.source), started);
        };
        plans.push(ColumnPlan { source, target: column.target.clone(), converter: Converter::new(&column.data_type) });
    }
    // A timestamp with an offset goes to a column without a zone as UTC: the servers differ in what they make of the offset.
    let hints = Hints { offsets_to_utc: true, passthrough_temporal: run.settings.same_dialect, ..Hints::default() };
    let load = Load { columns: &plans, extras: &[], hints: &hints, settings: run.settings.load, progress, cancelled: run.cancelled, notify: run.notify };
    run::run(&load, &mut rows, writer)
}

/// One file job: the rows of the query through the sink, a page at a time.
fn export_table<F: PageFeed>(run: &TransferRun<'_>, index: usize, feed: &mut F, path: &std::path::Path, sql_table: &(Option<String>, String)) -> JobResult {
    let types = &run.jobs[index].types;
    let started = Instant::now();
    let job = &run.jobs[index];
    let progress = &run.progress.jobs[index];
    let failed = |message: String| JobResult { report: failed_report(message, started), file: None, note: None, snapshot: NOT_READ };
    let cancelled = || JobResult { report: Report { outcome: Outcome::Cancelled, ..failed_report(String::new(), started) }, file: None, note: None, snapshot: NOT_READ };
    let mut cursor = Cursor::new(job.query.clone());
    let mut sink: Option<FileSink> = None;
    let (mut rows_written, mut bytes) = (0u64, 0u64);
    let mut buffers: Vec<String> = Vec::new();
    // Whether each column is declared as bytes (bytes in any other are bytes the server calls text).
    let mut binary: Vec<bool> = Vec::new();
    loop {
        if (run.cancelled)() {
            if let Some(sink) = sink {
                sink.abandon();
            }
            return cancelled();
        }
        let page = match cursor.next(feed) {
            Ok(Some(page)) => page,
            Ok(None) => break,
            Err(error) => {
                if let Some(sink) = sink {
                    sink.abandon();
                }
                return if error == ReadError::Cancelled { cancelled() } else { failed(error.to_string()) };
            }
        };
        if sink.is_none() {
            let columns: Vec<SinkColumn> = page
                .columns
                .iter()
                .map(|column| SinkColumn {
                    name: column.name.clone(),
                    data_type: types.iter().find(|(name, _)| *name == column.name).map_or_else(|| column.data_type.clone(), |(_, data_type)| data_type.clone()),
                })
                .collect();
            let style = SqlStyle { target: run.settings.file.sql, schema: sql_table.0.as_deref(), table: &sql_table.1 };
            match FileSink::create(path, run.settings.file.format, &columns, &run.settings.file.dsv, Some(style), run.settings.file.overwrite) {
                Ok(created) => sink = Some(created),
                Err(error) => return failed(error.to_string()),
            }
            buffers = vec![String::new(); columns.len()];
            binary = columns.iter().map(|column| declared_binary(&column.data_type)).collect();
        }
        let Some(open) = sink.as_mut() else { return failed("no file".to_owned()) };
        match write_page(open, &page.batch, &mut buffers, &binary, run.cancelled, progress) {
            Ok(page_bytes) => bytes += page_bytes,
            Err(WriteStop::Cancelled) => {
                if let Some(sink) = sink {
                    sink.abandon();
                }
                return cancelled();
            }
            Err(WriteStop::Sink(error)) => {
                if let Some(sink) = sink {
                    sink.abandon();
                }
                return failed(error.to_string());
            }
        }
        rows_written += page.batch.num_rows() as u64;
        (run.notify)();
    }
    let Some(sink) = sink else { return failed("The query returned no columns.".to_owned()) };
    match sink.finish() {
        Ok(FileSummary { rows, size, truncated }) => JobResult {
            report: Report {
                outcome: Outcome::Completed,
                inserted: rows,
                rolled_back: 0,
                skipped: 0,
                rows_read: rows_written,
                errors: Vec::new(),
                errors_total: 0,
                uncertain: 0,
                bytes,
                elapsed: started.elapsed(),
            },
            file: Some((path.to_owned(), size)),
            snapshot: NOT_READ,
            note: (truncated > 0).then(|| format!("{} cut to the 32,767 characters an XLSX cell holds.", if truncated == 1 { "1 cell was".to_owned() } else { format!("{truncated} cells were") })),
        },
        Err(error) => failed(error.to_string()),
    }
}

enum WriteStop {
    Cancelled,
    Sink(SinkError),
}

/// The rows of `batch` to `sink`. Returns the bytes of text written.
fn write_page(sink: &mut FileSink, batch: &RecordBatch, buffers: &mut [String], binary: &[bool], cancelled: &(dyn Fn() -> bool + Sync), progress: &Progress) -> Result<u64, WriteStop> {
    let readers: Vec<ArrowCellReader<'_>> = batch.columns().iter().map(|array| ArrowCellReader::new(array.as_ref(), BinaryDisplay::Hex)).collect();
    let mut bytes = 0u64;
    for row in 0..batch.num_rows() {
        if row % 256 == 0 && cancelled() {
            return Err(WriteStop::Cancelled);
        }
        let cells: Vec<Cell<'_>> = readers
            .iter()
            .zip(buffers.iter_mut())
            .zip(batch.columns())
            .enumerate()
            .map(|(column, ((reader, buffer), array))| {
                if reader.is_null(row) {
                    Cell::Null
                } else if holds_bytes(array.as_ref()) && !binary.get(column).copied().unwrap_or(true) {
                    Cell::Bytes(reader.text(row, buffer))
                } else {
                    Cell::Text(reader.text(row, buffer))
                }
            })
            .collect();
        bytes += cells.iter().map(|cell| if let Cell::Text(text) | Cell::Bytes(text) = cell { text.len() as u64 } else { 0 }).sum::<u64>();
        sink.write_row(&cells).map_err(WriteStop::Sink)?;
        progress.rows_read.fetch_add(1, Ordering::Relaxed);
        progress.inserted.fetch_add(1, Ordering::Relaxed);
    }
    progress.bytes.fetch_add(bytes, Ordering::Relaxed);
    Ok(bytes)
}

/// Runs every job of the plan. Blocking: run it off the UI thread.
pub(in crate::app) fn run_jobs<F: PageFeed, W: TableWriter>(run: &TransferRun<'_>, feed: &mut F, mut writer: Option<&mut W>) -> TransferReport {
    let started = Instant::now();
    let mut results: Vec<Option<JobResult>> = vec![None; run.jobs.len()];
    let mut outcome = TransferOutcome::Completed;
    for (index, job) in run.jobs.iter().enumerate() {
        if (run.cancelled)() {
            outcome = TransferOutcome::Cancelled;
            break;
        }
        run.progress.current.store(index, Ordering::Relaxed);
        (run.notify)();
        // One snapshot per table: from before its first page to after its last, so the locks and the old row versions the server
        // keeps for it go as soon as the table is read.
        let snapshot = feed.begin_table();
        let mut result = match (&job.sink, writer.as_deref_mut()) {
            (JobSink::Table { table, columns }, Some(writer)) => JobResult { report: transfer_table(run, index, feed, writer, table, columns), file: None, note: None, snapshot: NOT_READ },
            (JobSink::File { path, sql_table }, _) => export_table(run, index, feed, path, sql_table),
            (JobSink::Table { .. }, None) => JobResult { report: failed_report("There is no session to write to.".to_owned(), started), file: None, note: None, snapshot: NOT_READ },
        };
        feed.end_table();
        result.snapshot = snapshot;
        let notes = feed.notes();
        if !notes.is_empty() {
            result.note = Some(result.note.iter().cloned().chain(notes).collect::<Vec<_>>().join(" "));
        }
        let (ended, stop_reason) = match &result.report.outcome {
            Outcome::Completed => (None, None),
            Outcome::Cancelled => (Some(TransferOutcome::Cancelled), None),
            Outcome::Stopped(reason) | Outcome::Failed(reason) => (None, Some(reason.clone())),
        };
        run.progress.finished.lock().unwrap_or_else(|poisoned| poisoned.into_inner())[index] = Some(result.clone());
        results[index] = Some(result);
        (run.notify)();
        if let Some(ended) = ended {
            outcome = ended;
            break;
        }
        if let Some(reason) = stop_reason {
            if run.settings.load.on_error == OnError::Stop {
                outcome = TransferOutcome::Stopped(format!("{}: {reason}", job.label));
                break;
            }
        }
    }
    (run.notify)();
    TransferReport { outcome, jobs: results, elapsed: started.elapsed() }
}

#[cfg(test)]
mod tests {
    use super::super::reader::memory::MemoryFeed;
    use super::super::reader::FIRST_PAGE_ROWS;
    use super::super::super::data_import::run::tests::FakeWriter;
    use super::super::super::data_import::run::TransactionMode;
    use super::*;
    use std::sync::atomic::AtomicBool;

    impl TableWriter for FakeWriter {
        fn aim(&mut self, table: &TargetTable) {
            self.calls.push(format!("aim {}", table.object));
        }
    }

    fn mapped() -> Vec<MappedColumn> {
        vec![
            MappedColumn { source: "id".into(), target: "id".into(), data_type: "bigint".into() },
            MappedColumn { source: "name".into(), target: "label".into(), data_type: "text".into() },
        ]
    }

    fn table_job(label: &str) -> JobPlan {
        JobPlan {
            label: label.into(),
            query: Query::new(None, format!("SELECT * FROM {label}")),
            types: vec![("id".into(), "bigint".into()), ("name".into(), "text".into())],
            sink: JobSink::Table { table: TargetTable { database: "d".into(), schema: "s".into(), object: format!("copy_{label}") }, columns: mapped() },
        }
    }

    fn settings(on_error: OnError, batch: usize) -> TransferSettings {
        TransferSettings {
            load: LoadSettings { batch_size: batch, transaction: TransactionMode::PerBatch, on_error },
            file: FileSettings {
                format: FileFormat::Csv,
                dsv: DsvOptions { include_headers: true, ..Default::default() },
                overwrite: false,
                sql: SqlTarget { dialect: crate::sql_table::SqlDialect::POSTGRES, binary: crate::sql_table::BinaryLiteral::Postgres },
            },
            same_dialect: false,
        }
    }

    fn run_plan(jobs: &[JobPlan], settings: &TransferSettings, feed: &mut MemoryFeed, writer: &mut FakeWriter, cancelled: &(dyn Fn() -> bool + Sync)) -> TransferReport {
        let progress = TransferProgress::new(jobs.len());
        run_jobs(&TransferRun { jobs, settings, progress: &progress, cancelled, notify: &|| {} }, feed, Some(writer))
    }

    const NEVER: &(dyn Fn() -> bool + Sync) = &|| false;

    #[test]
    fn a_table_goes_through_the_load_page_by_page_with_its_columns_renamed() {
        let mut feed = MemoryFeed::new(5_000);
        feed.null_every = 4;
        let mut writer = FakeWriter::default();
        let report = run_plan(&[table_job("t")], &settings(OnError::Stop, 1000), &mut feed, &mut writer, NEVER);
        assert_eq!(report.outcome, TransferOutcome::Completed);
        let result = report.jobs[0].as_ref().unwrap();
        assert_eq!((result.report.outcome.clone(), result.report.inserted, result.report.rows_read), (Outcome::Completed, 5_000, 5_000));
        assert_eq!(writer.calls[0], "aim copy_t");
        assert_eq!(writer.committed.len(), 5_000);
        assert_eq!(writer.committed[0]["label"], serde_json::json!("name 1"), "the source column goes to the target column of the mapping");
        assert_eq!(writer.committed[3]["label"], serde_json::Value::Null);
        assert_eq!(writer.committed[4]["id"], serde_json::json!(5), "numbers are typed by the target column");
        assert!(feed.calls.len() >= 2, "{:?}", feed.calls);
    }

    #[test]
    fn a_failing_table_stops_the_transfer_or_is_skipped_by_policy() {
        let jobs = [table_job("a"), table_job("b"), table_job("c")];
        let failing = |feed: &mut MemoryFeed| feed.fail_at_offset = Some((0, ReadError::Refused("relation \"t\" does not exist".into())));
        // Stop: the first table fails, nothing else runs.
        let mut feed = MemoryFeed::new(10);
        failing(&mut feed);
        let mut writer = FakeWriter::default();
        let report = run_plan(&jobs, &settings(OnError::Stop, 100), &mut feed, &mut writer, NEVER);
        assert!(matches!(&report.outcome, TransferOutcome::Stopped(reason) if reason.contains("a: relation")), "{:?}", report.outcome);
        assert!(report.jobs[0].is_some() && report.jobs[1].is_none() && report.jobs[2].is_none());
        // Skip: all three are tried, each listed with why.
        let mut feed = MemoryFeed::new(10);
        failing(&mut feed);
        let mut writer = FakeWriter::default();
        let report = run_plan(&jobs, &settings(OnError::SkipAndLog, 100), &mut feed, &mut writer, NEVER);
        assert_eq!(report.outcome, TransferOutcome::Completed);
        assert!(report.jobs.iter().all(|job| matches!(&job.as_ref().unwrap().report.outcome, Outcome::Failed(reason) if reason.contains("does not exist"))));
    }

    #[test]
    fn every_table_is_read_in_a_snapshot_of_its_own_and_a_table_that_fails_does_not_poison_the_next() {
        let jobs = [table_job("a"), table_job("b"), table_job("c")];
        let mut feed = MemoryFeed::new(3_000);
        feed.fail_on = Some(("FROM b".into(), ReadError::Refused("permission denied for table b".into())));
        feed.abort = true;
        feed.notes = vec!["a zone was ignored".into()];
        let mut writer = FakeWriter::default();
        let report = run_plan(&jobs, &settings(OnError::SkipAndLog, 500), &mut feed, &mut writer, NEVER);
        assert_eq!(report.outcome, TransferOutcome::Completed);
        let outcomes: Vec<_> = report.jobs.iter().map(|job| job.as_ref().unwrap().report.outcome.clone()).collect();
        assert_eq!(outcomes[0], Outcome::Completed);
        assert!(matches!(&outcomes[1], Outcome::Failed(reason) if reason.contains("permission denied")), "{outcomes:?}");
        assert_eq!(outcomes[2], Outcome::Completed, "the table after the failed one is read in a transaction of its own");
        assert_eq!(report.jobs[2].as_ref().unwrap().report.inserted, 3_000);
        // A snapshot begins before the first page of each table and ends after its last, the failed one's too.
        let marks: Vec<&str> = feed.events.iter().copied().filter(|event| *event != "fetch").collect();
        assert_eq!(marks, ["begin", "end", "begin", "end", "begin", "end"]);
        let position = |nth: usize, mark: &str| feed.events.iter().enumerate().filter(|(_, event)| **event == mark).nth(nth).map(|(index, _)| index).unwrap();
        assert!(position(0, "end") < position(1, "begin") && position(1, "end") < position(2, "begin"), "{:?}", feed.events);
        assert!(feed.events[position(0, "begin")..position(0, "end")].contains(&"fetch"));
        for job in report.jobs.iter().flatten() {
            assert_eq!(job.snapshot, SnapshotState::Held);
            assert_eq!(job.note.as_deref(), Some("a zone was ignored"), "what the session could not set up is said of every table");
        }
        // A database that has no snapshot says so per table.
        let mut feed = MemoryFeed::new(10);
        feed.snapshot = SnapshotState::Unavailable("no snapshot here");
        let report = run_plan(&jobs[..1], &settings(OnError::Stop, 100), &mut feed, &mut writer, NEVER);
        assert_eq!(report.jobs[0].as_ref().unwrap().snapshot, SnapshotState::Unavailable("no snapshot here"));
    }

    #[test]
    fn a_column_the_source_lost_fails_the_table_before_anything_is_written() {
        let mut job = table_job("t");
        if let JobSink::Table { columns, .. } = &mut job.sink {
            columns[1].source = "gone".into();
        }
        let mut feed = MemoryFeed::new(10);
        let mut writer = FakeWriter::default();
        let report = run_plan(&[job], &settings(OnError::Stop, 100), &mut feed, &mut writer, NEVER);
        assert!(matches!(&report.outcome, TransferOutcome::Stopped(reason) if reason.contains("\"gone\" is not in the result")), "{:?}", report.outcome);
        assert!(writer.committed.is_empty());
    }

    #[test]
    fn rows_the_target_refuses_follow_the_row_policy_and_are_listed() {
        let mut feed = MemoryFeed::new(10);
        let mut writer = FakeWriter::refusing(vec![3, 7]);
        let report = run_plan(&[table_job("t")], &settings(OnError::SkipAndLog, 4), &mut feed, &mut writer, NEVER);
        assert_eq!(report.outcome, TransferOutcome::Completed);
        let result = report.jobs[0].as_ref().unwrap();
        assert_eq!((result.report.inserted, result.report.skipped), (8, 2));
        assert_eq!(result.report.errors.iter().map(|error| error.row).collect::<Vec<_>>(), [3, 7]);
        assert_eq!(result.report.errors[0].to_string().split(':').next(), Some("Row 3"), "a row of a query has no file line");
    }

    #[test]
    fn cancel_ends_the_transfer_between_rows_and_keeps_what_was_committed() {
        let jobs = [table_job("a"), table_job("b")];
        let asked = AtomicBool::new(false);
        let calls = std::sync::atomic::AtomicUsize::new(0);
        let cancelled = || {
            if calls.fetch_add(1, Ordering::SeqCst) >= 30 {
                asked.store(true, Ordering::SeqCst);
            }
            asked.load(Ordering::SeqCst)
        };
        let mut feed = MemoryFeed::new(1_000);
        let mut writer = FakeWriter::default();
        let report = run_plan(&jobs, &settings(OnError::SkipAndLog, 10), &mut feed, &mut writer, &cancelled);
        assert_eq!(report.outcome, TransferOutcome::Cancelled);
        assert!(report.jobs[1].is_none(), "the second table never started");
        let first = report.jobs[0].as_ref().unwrap();
        assert_eq!(first.report.outcome, Outcome::Cancelled);
        assert!(first.report.inserted > 0 && first.report.inserted < 1_000);
        assert_eq!(writer.committed.len() as u64, first.report.inserted);
    }

    fn file_job(dir: &std::path::Path, name: &str) -> JobPlan {
        JobPlan {
            label: name.into(),
            query: Query::new(None, format!("SELECT * FROM {name}")),
            types: vec![("id".into(), "bigint".into())],
            sink: JobSink::File { path: dir.join(format!("{name}.csv")), sql_table: (Some("public".into()), name.into()) },
        }
    }

    #[test]
    fn a_table_streams_to_a_csv_file_page_by_page() {
        let dir = tempfile::tempdir().unwrap();
        let mut feed = MemoryFeed::new(6_000);
        feed.null_every = 2;
        let mut writer = FakeWriter::default();
        let report = run_plan(&[file_job(dir.path(), "t")], &settings(OnError::Stop, 100), &mut feed, &mut writer, NEVER);
        assert_eq!(report.outcome, TransferOutcome::Completed);
        let result = report.jobs[0].as_ref().unwrap();
        assert_eq!((result.report.outcome.clone(), result.report.inserted), (Outcome::Completed, 6_000));
        let (path, size) = result.file.clone().unwrap();
        let text = std::fs::read_to_string(&path).unwrap();
        assert_eq!(size, text.len() as u64);
        let lines: Vec<&str> = text.lines().collect();
        assert_eq!((lines.len(), lines[0], lines[1], lines[2]), (6_001, "id,name", "1,name 1", "2,"));
        assert!(feed.calls.len() >= 2, "{:?}", feed.calls);
        assert!(writer.calls.is_empty(), "a file job does not touch the writer");
    }

    #[test]
    fn a_file_that_exists_fails_that_table_and_the_others_still_go_by_policy() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("a.csv"), "keep").unwrap();
        let jobs = [file_job(dir.path(), "a"), file_job(dir.path(), "b")];
        let mut feed = MemoryFeed::new(3);
        let mut writer = FakeWriter::default();
        let report = run_plan(&jobs, &settings(OnError::SkipAndLog, 100), &mut feed, &mut writer, NEVER);
        assert!(matches!(&report.jobs[0].as_ref().unwrap().report.outcome, Outcome::Failed(reason) if reason.contains("exists already")));
        assert_eq!(report.jobs[1].as_ref().unwrap().report.inserted, 3);
        assert_eq!(std::fs::read_to_string(dir.path().join("a.csv")).unwrap(), "keep");
    }

    #[test]
    fn a_cancelled_or_failed_export_leaves_no_file() {
        let dir = tempfile::tempdir().unwrap();
        // Fails on the second page.
        let mut feed = MemoryFeed::new(10_000);
        feed.fail_at_offset = Some((FIRST_PAGE_ROWS, ReadError::Lost("connection reset".into())));
        let mut writer = FakeWriter::default();
        let report = run_plan(&[file_job(dir.path(), "t")], &settings(OnError::Stop, 100), &mut feed, &mut writer, NEVER);
        assert!(matches!(&report.outcome, TransferOutcome::Stopped(reason) if reason.contains("connection reset")), "{:?}", report.outcome);
        assert_eq!(std::fs::read_dir(dir.path()).unwrap().count(), 0, "neither the file nor its .part");
        // Cancelled mid-page.
        let asked = AtomicBool::new(false);
        let calls = std::sync::atomic::AtomicUsize::new(0);
        let cancelled = || {
            if calls.fetch_add(1, Ordering::SeqCst) >= 3 {
                asked.store(true, Ordering::SeqCst);
            }
            asked.load(Ordering::SeqCst)
        };
        let mut feed = MemoryFeed::new(10_000);
        let report = run_plan(&[file_job(dir.path(), "t")], &settings(OnError::Stop, 100), &mut feed, &mut writer, &cancelled);
        assert_eq!(report.outcome, TransferOutcome::Cancelled);
        assert_eq!(std::fs::read_dir(dir.path()).unwrap().count(), 0);
    }
}
